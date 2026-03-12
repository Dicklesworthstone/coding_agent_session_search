//! Process discovery — finds running Claude Code instances.
//!
//! Uses `ps` to find claude processes, `lsof` to map PIDs to working directories,
//! then maps working directories to `~/.claude/projects/` JSONL session files.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use crate::monitor::state::{AgentInstance, AgentState, PermissionMode};

/// Raw info parsed from a single `ps` output line.
#[derive(Debug, Clone)]
pub struct ProcessInfo {
    pub pid: u32,
    pub tty: String,
    pub age_secs: u64,
    pub args: Vec<String>,
}

/// Parse a single line from `ps -eo pid,tty,etime,args` output.
///
/// Returns None if the line doesn't represent a Claude Code CLI process
/// (filters out Claude.app, grep, and other non-CLI processes).
pub fn parse_ps_line(line: &str) -> Option<ProcessInfo> {
    let line = line.trim();
    if line.is_empty() || line.starts_with("PID") {
        return None;
    }

    // ps output has variable-width whitespace columns, so split_whitespace
    // to skip empty strings, then rejoin the remainder as args.
    let mut words = line.split_whitespace();
    let pid: u32 = words.next()?.parse().ok()?;
    let tty = words.next()?.to_string();
    let etime = words.next()?;
    let args_str: String = words.collect::<Vec<&str>>().join(" ");
    if args_str.is_empty() {
        return None;
    }
    let args_str = args_str.as_str();

    // Filter: must be the `claude` CLI binary, not Claude.app or grep
    let first_arg = args_str.split_whitespace().next().unwrap_or("");
    if first_arg != "claude" {
        return None;
    }

    let args: Vec<String> = args_str.split_whitespace().map(String::from).collect();
    let age_secs = parse_etime(etime);

    Some(ProcessInfo {
        pid,
        tty,
        age_secs,
        args,
    })
}

/// Parse elapsed time from ps format: `[[DD-]HH:]MM:SS`
pub fn parse_etime(s: &str) -> u64 {
    let s = s.trim();

    // Check for days: "8-17:30:42"
    let (days, rest) = if let Some(idx) = s.find('-') {
        let d: u64 = s[..idx].parse().unwrap_or(0);
        (d, &s[idx + 1..])
    } else {
        (0, s)
    };

    let parts: Vec<u64> = rest.split(':').filter_map(|p| p.parse().ok()).collect();

    let (hours, minutes, seconds) = match parts.len() {
        3 => (parts[0], parts[1], parts[2]),
        2 => (0, parts[0], parts[1]),
        1 => (0, 0, parts[0]),
        _ => (0, 0, 0),
    };

    days * 86400 + hours * 3600 + minutes * 60 + seconds
}

/// Convert a working directory path to the Claude projects directory key.
///
/// `/Users/lee/Projects/foo/bar` → `-Users-lee-Projects-foo-bar`
pub fn cwd_to_project_key(cwd: &str) -> String {
    cwd.replace('/', "-")
}

/// Extract a human-readable project name from the cwd.
///
/// Tries to find the `Projects/` prefix and take the last two path components.
/// Falls back to the last two components of any path.
pub fn extract_project_name(cwd: &str) -> String {
    let path = Path::new(cwd);
    let components: Vec<&str> = path
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();

    // Find "Projects" in the path and take the next two components
    if let Some(idx) = components.iter().position(|&c| c == "Projects") {
        let after: Vec<&str> = components[idx + 1..].to_vec();
        if after.len() >= 2 {
            return format!("{}/{}", after[0], after[1]);
        } else if after.len() == 1 {
            return after[0].to_string();
        }
    }

    // Fallback: last two path components
    let len = components.len();
    if len >= 2 {
        format!("{}/{}", components[len - 2], components[len - 1])
    } else {
        components.last().unwrap_or(&"unknown").to_string()
    }
}

/// Parse lsof -Fn output to extract the cwd path for the target PID.
///
/// Output may contain entries for many PIDs (macOS lsof quirk).
/// Format: `p<pid>\nf<fd>\nn<path>\n` repeated per process.
/// We find our target PID block and take the `n` line from it.
pub fn parse_lsof_cwd(output: &str, target_pid: u32) -> Option<PathBuf> {
    let target_prefix = format!("p{target_pid}");
    let mut found_pid = false;

    for line in output.lines() {
        if line.starts_with('p') {
            // New PID block — check if it's ours
            found_pid = line == target_prefix;
            continue;
        }
        if found_pid {
            if let Some(path) = line.strip_prefix('n') {
                if path.starts_with('/') {
                    return Some(PathBuf::from(path));
                }
            }
        }
    }
    None
}

/// Get the working directory for a process via `lsof -d cwd`.
fn get_process_cwd(pid: u32) -> Option<PathBuf> {
    let output = Command::new("lsof")
        .args(["-d", "cwd", "-p", &pid.to_string(), "-Fn"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_lsof_cwd(&stdout, pid)
}

/// Find the most recently modified JSONL session file for a project.
///
/// Looks in `~/.claude/projects/<project_key>/` for `.jsonl` files,
/// excluding the `subagents/` subdirectory.
pub fn find_latest_session(claude_projects_dir: &Path, project_key: &str) -> Option<PathBuf> {
    let project_dir = claude_projects_dir.join(project_key);
    if !project_dir.is_dir() {
        return None;
    }

    let mut best: Option<(PathBuf, SystemTime)> = None;

    if let Ok(entries) = std::fs::read_dir(&project_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "jsonl") && path.is_file() {
                if let Ok(meta) = path.metadata() {
                    if let Ok(modified) = meta.modified() {
                        if best.as_ref().is_none_or(|(_, t)| modified > *t) {
                            best = Some((path, modified));
                        }
                    }
                }
            }
        }
    }

    best.map(|(p, _)| p)
}

/// Discover all running Claude Code CLI processes and map them to sessions.
///
/// Returns a Vec of partially-populated AgentInstances (state will be
/// set to Starting; caller should update via session::derive_state).
pub fn discover_agents(
    claude_projects_dir: &Path,
    own_pid: u32,
) -> Vec<(AgentInstance, Option<PathBuf>)> {
    let ps_output = match Command::new("ps")
        .args(["-eo", "pid,tty,etime,args"])
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => return vec![],
    };

    let processes: Vec<ProcessInfo> = ps_output
        .lines()
        .filter_map(parse_ps_line)
        .filter(|p| p.pid != own_pid)
        .collect();

    let mut results = Vec::new();

    for proc in processes {
        let cwd = match get_process_cwd(proc.pid) {
            Some(c) => c,
            None => continue,
        };

        let project_name = extract_project_name(cwd.to_str().unwrap_or(""));
        let project_key = cwd_to_project_key(cwd.to_str().unwrap_or(""));
        let session_path = find_latest_session(claude_projects_dir, &project_key);

        let args_refs: Vec<&str> = proc.args.iter().map(String::as_str).collect();
        let permission_mode = PermissionMode::from_args(&args_refs);

        let instance = AgentInstance {
            pid: proc.pid,
            tty: proc.tty,
            cwd,
            project_name,
            state: AgentState::Starting,
            permission_mode,
            age_secs: proc.age_secs,
            last_activity_secs: 0,
            session_context: None,
            is_subagent: false,
            parent_session_id: None,
            team_name: None,
            agent_role: None,
            agent_slug: None,
        };

        results.push((instance, session_path));
    }

    results
}

// ─── Team config discovery ────────────────────────────────────────────────

/// A team member entry from a Claude team config file.
#[derive(Debug, Clone)]
pub struct TeamMember {
    pub name: String,
    pub agent_id: String,
    pub agent_type: String,
    pub cwd: Option<String>,
}

/// A discovered team configuration.
#[derive(Debug, Clone)]
pub struct TeamConfig {
    pub name: String,
    pub members: Vec<TeamMember>,
}

/// Discover all active team configurations from `~/.claude/teams/*/config.json`.
pub fn discover_teams(home: &Path) -> std::collections::HashMap<String, TeamConfig> {
    let teams_dir = home.join(".claude").join("teams");
    let mut teams = std::collections::HashMap::new();

    let entries = match std::fs::read_dir(&teams_dir) {
        Ok(e) => e,
        Err(_) => return teams,
    };

    for entry in entries.flatten() {
        let config_path = entry.path().join("config.json");
        if !config_path.is_file() {
            continue;
        }

        let data = match std::fs::read_to_string(&config_path) {
            Ok(d) => d,
            Err(_) => continue,
        };

        let json: serde_json::Value = match serde_json::from_str(&data) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let team_name = entry
            .file_name()
            .to_string_lossy()
            .to_string();

        let members: Vec<TeamMember> = json
            .get("members")
            .and_then(|m| m.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| {
                        Some(TeamMember {
                            name: m.get("name")?.as_str()?.to_string(),
                            agent_id: m.get("agentId")?.as_str()?.to_string(),
                            agent_type: m
                                .get("agentType")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown")
                                .to_string(),
                            cwd: m.get("cwd").and_then(|v| v.as_str()).map(String::from),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        teams.insert(
            team_name.clone(),
            TeamConfig {
                name: team_name,
                members,
            },
        );
    }

    teams
}

// ─── Subagent session discovery ───────────────────────────────────────────

/// A discovered subagent session file.
#[derive(Debug, Clone)]
pub struct SubagentFile {
    pub agent_id: String,
    pub slug: Option<String>,
    pub cwd: Option<PathBuf>,
    pub session_id: Option<String>,
    pub session_path: PathBuf,
}

/// Discover active subagents for a parent session.
///
/// Scans `~/.claude/projects/<project_key>/<session_id>/subagents/agent-*.jsonl`
/// and reads the first line for metadata. Filters by staleness.
pub fn discover_subagents(
    claude_projects_dir: &Path,
    project_key: &str,
    parent_session_id: &str,
    max_staleness_secs: u64,
) -> Vec<SubagentFile> {
    let subagents_dir = claude_projects_dir
        .join(project_key)
        .join(parent_session_id)
        .join("subagents");

    if !subagents_dir.is_dir() {
        return vec![];
    }

    let entries = match std::fs::read_dir(&subagents_dir) {
        Ok(e) => e,
        Err(_) => return vec![],
    };

    let now = SystemTime::now();
    let mut results = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.extension().is_some_and(|e| e == "jsonl") {
            continue;
        }

        // Check staleness
        if let Ok(meta) = path.metadata() {
            if let Ok(modified) = meta.modified() {
                if let Ok(elapsed) = now.duration_since(modified) {
                    if elapsed.as_secs() > max_staleness_secs {
                        continue;
                    }
                }
            }
        }

        // Read first line for metadata
        let first_line = match std::fs::read_to_string(&path) {
            Ok(content) => content.lines().next().map(String::from),
            Err(_) => continue,
        };

        let (agent_id, slug, cwd, session_id) = if let Some(line) = first_line {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&line) {
                (
                    json.get("agentId")
                        .or_else(|| json.get("agent_id"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    json.get("slug").and_then(|v| v.as_str()).map(String::from),
                    json.get("cwd")
                        .and_then(|v| v.as_str())
                        .map(PathBuf::from),
                    json.get("sessionId")
                        .or_else(|| json.get("session_id"))
                        .and_then(|v| v.as_str())
                        .map(String::from),
                )
            } else {
                continue;
            }
        } else {
            continue;
        };

        if agent_id.is_empty() {
            continue;
        }

        results.push(SubagentFile {
            agent_id,
            slug,
            cwd,
            session_id,
            session_path: path,
        });
    }

    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ps_line_basic() {
        let line = "12345 s005  8-17:30:42 claude";
        let info = parse_ps_line(line).unwrap();
        assert_eq!(info.pid, 12345);
        assert_eq!(info.tty, "s005");
        assert_eq!(info.args, vec!["claude"]);
    }

    #[test]
    fn parse_ps_line_with_flags() {
        let line = "82466 s003     39:18 claude --allow-dangerously-skip-permissions";
        let info = parse_ps_line(line).unwrap();
        assert_eq!(info.pid, 82466);
        assert_eq!(info.tty, "s003");
        assert_eq!(
            info.args,
            vec!["claude", "--allow-dangerously-skip-permissions"]
        );
    }

    #[test]
    fn parse_ps_line_ignores_non_claude() {
        let line = "28769   ??      0:00 /Applications/Claude.app/Contents/MacOS/Claude";
        let info = parse_ps_line(line);
        assert!(info.is_none(), "Should skip Claude.app desktop process");
    }

    #[test]
    fn parse_ps_line_ignores_grep() {
        let line = "99999 s007      0:00 grep claude";
        let info = parse_ps_line(line);
        assert!(info.is_none());
    }

    #[test]
    fn parse_etime_days_hours_mins_secs() {
        assert_eq!(
            parse_etime("8-17:30:42"),
            8 * 86400 + 17 * 3600 + 30 * 60 + 42
        );
    }

    #[test]
    fn parse_etime_hours_mins_secs() {
        assert_eq!(parse_etime("1:48:12"), 1 * 3600 + 48 * 60 + 12);
    }

    #[test]
    fn parse_etime_mins_secs() {
        assert_eq!(parse_etime("39:18"), 39 * 60 + 18);
    }

    #[test]
    fn parse_etime_secs_only() {
        assert_eq!(parse_etime("27"), 27);
    }

    #[test]
    fn path_to_claude_project_dir() {
        let cwd = "/Users/lee/Projects/leegonzales/cass";
        let expected = "-Users-lee-Projects-leegonzales-cass";
        assert_eq!(cwd_to_project_key(cwd), expected);
    }

    #[test]
    fn project_name_from_cwd() {
        assert_eq!(
            extract_project_name("/Users/lee/Projects/leegonzales/cass"),
            "leegonzales/cass"
        );
        assert_eq!(
            extract_project_name("/Users/lee/Projects/Difflab/bizops"),
            "Difflab/bizops"
        );
        assert_eq!(
            extract_project_name("/Users/lee/some/other/path"),
            "other/path"
        );
    }

    #[test]
    fn parse_lsof_cwd_output() {
        let output = "p12345\nfcwd\nn/Users/lee/Projects/leegonzales/cass\n";
        let cwd = parse_lsof_cwd(output, 12345);
        assert_eq!(
            cwd,
            Some(PathBuf::from("/Users/lee/Projects/leegonzales/cass"))
        );
    }

    #[test]
    fn parse_lsof_cwd_skips_other_pids() {
        // macOS lsof may return entries for many PIDs
        let output = "p100\nfcwd\nn/\np12345\nfcwd\nn/Users/lee/Projects/cass\np200\nfcwd\nn/tmp\n";
        let cwd = parse_lsof_cwd(output, 12345);
        assert_eq!(
            cwd,
            Some(PathBuf::from("/Users/lee/Projects/cass"))
        );
    }
}
