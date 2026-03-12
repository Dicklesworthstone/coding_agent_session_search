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

/// Parse lsof -Fn output to extract the cwd path.
///
/// Output format: `p<pid>\nn<path>\n`
pub fn parse_lsof_cwd(output: &str) -> Option<PathBuf> {
    for line in output.lines() {
        if let Some(path) = line.strip_prefix('n') {
            if path.starts_with('/') {
                return Some(PathBuf::from(path));
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
    parse_lsof_cwd(&stdout)
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
        };

        results.push((instance, session_path));
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
        let output = "p12345\nn/Users/lee/Projects/leegonzales/cass\n";
        let cwd = parse_lsof_cwd(output);
        assert_eq!(
            cwd,
            Some(PathBuf::from("/Users/lee/Projects/leegonzales/cass"))
        );
    }
}
