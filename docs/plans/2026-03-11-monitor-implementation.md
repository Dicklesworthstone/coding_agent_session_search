# `cass monitor` Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add a `cass monitor` subcommand that discovers active Claude Code processes, derives their state from JSONL session files, and presents a live dashboard (ftui TUI or streaming JSON).

**Architecture:** Process discovery via `ps`/`lsof` → JSONL tail parsing → state machine → ftui Elm-architecture TUI or JSON output. New `src/monitor/` module with 5 files. Integration into existing `Commands` enum and `execute_cli` dispatch in `src/lib.rs`.

**Tech Stack:** Rust (edition 2024), ftui 0.2.0 (Elm-architecture TUI), serde/serde_json, std::process::Command (for ps/lsof), std::io::SeekFrom (for efficient tail reading)

**Design doc:** `docs/plans/2026-03-11-monitor-design.md`

---

## Task 1: Agent State Types (`src/monitor/state.rs`)

Pure data types with no dependencies. Start here because everything else uses these.

**Files:**
- Create: `src/monitor/state.rs`
- Test: inline `#[cfg(test)]` module

**Step 1: Write the failing test**

```rust
// src/monitor/state.rs
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_state_display() {
        assert_eq!(AgentState::Working.to_string(), "WORKING");
        assert_eq!(AgentState::WaitingInput.to_string(), "NEEDS INPUT");
        assert_eq!(AgentState::WaitingPermission.to_string(), "PERMISSION");
    }

    #[test]
    fn agent_state_priority_order() {
        assert!(AgentState::WaitingInput.priority() < AgentState::WaitingPermission.priority());
        assert!(AgentState::WaitingPermission.priority() < AgentState::Working.priority());
        assert!(AgentState::Working.priority() < AgentState::Idle.priority());
    }

    #[test]
    fn agent_state_needs_attention() {
        assert!(AgentState::WaitingInput.needs_attention());
        assert!(AgentState::WaitingPermission.needs_attention());
        assert!(!AgentState::Working.needs_attention());
        assert!(!AgentState::Idle.needs_attention());
    }

    #[test]
    fn permission_mode_from_args() {
        assert_eq!(
            PermissionMode::from_args(&["claude", "--dangerously-skip-permissions"]),
            PermissionMode::DangerouslySkip
        );
        assert_eq!(
            PermissionMode::from_args(&["claude", "--allow-dangerously-skip-permissions"]),
            PermissionMode::AllowDangerouslySkip
        );
        assert_eq!(
            PermissionMode::from_args(&["claude"]),
            PermissionMode::Default
        );
    }

    #[test]
    fn agent_instance_display_name() {
        let inst = AgentInstance {
            pid: 12345,
            tty: "s005".into(),
            cwd: PathBuf::from("/Users/lee/Projects/leegonzales/cass"),
            project_name: "leegonzales/cass".into(),
            state: AgentState::Working,
            permission_mode: PermissionMode::Default,
            age_secs: 3600,
            last_activity_secs: 2,
            session_context: None,
        };
        assert_eq!(inst.project_name, "leegonzales/cass");
    }
}
```

**Step 2: Run test to verify it fails**

Run: `cd ~/Projects/leegonzales/cass && cargo test -p coding-agent-search monitor::state --no-run 2>&1 | head -20`
Expected: compile error — module doesn't exist yet

**Step 3: Write the implementation**

```rust
// src/monitor/state.rs
use serde::Serialize;
use std::fmt;
use std::path::PathBuf;

/// State of an active Claude Code agent, derived from JSONL session tail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AgentState {
    /// Last entry is assistant with stop_reason: null (still streaming)
    Working,
    /// Progress entry — hook or tool executing
    ToolRunning,
    /// tool_use emitted, no tool_result yet, stale >2s
    WaitingPermission,
    /// Complete assistant turn, no subsequent user message, stale >5s
    WaitingInput,
    /// queue-operation: enqueue present
    Queued,
    /// Process alive but no JSONL writes >30s
    Idle,
    /// Process running but no JSONL file found yet
    Starting,
}

impl AgentState {
    /// Lower number = higher urgency. Used for sorting the dashboard table.
    pub fn priority(&self) -> u8 {
        match self {
            Self::WaitingInput => 0,
            Self::WaitingPermission => 1,
            Self::Queued => 2,
            Self::Working => 3,
            Self::ToolRunning => 4,
            Self::Starting => 5,
            Self::Idle => 6,
        }
    }

    /// Whether this state requires user attention (flashing in TUI).
    pub fn needs_attention(&self) -> bool {
        matches!(self, Self::WaitingInput | Self::WaitingPermission)
    }
}

impl fmt::Display for AgentState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Working => write!(f, "WORKING"),
            Self::ToolRunning => write!(f, "TOOL RUNNING"),
            Self::WaitingPermission => write!(f, "PERMISSION"),
            Self::WaitingInput => write!(f, "NEEDS INPUT"),
            Self::Queued => write!(f, "QUEUED"),
            Self::Idle => write!(f, "IDLE"),
            Self::Starting => write!(f, "STARTING"),
        }
    }
}

/// How the Claude Code instance was launched re: permissions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    Default,
    DangerouslySkip,
    AllowDangerouslySkip,
}

impl PermissionMode {
    /// Parse from process command-line arguments.
    pub fn from_args(args: &[&str]) -> Self {
        if args.iter().any(|a| *a == "--dangerously-skip-permissions") {
            Self::DangerouslySkip
        } else if args.iter().any(|a| *a == "--allow-dangerously-skip-permissions") {
            Self::AllowDangerouslySkip
        } else {
            Self::Default
        }
    }
}

impl fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Default => write!(f, "default"),
            Self::DangerouslySkip => write!(f, "yolo"),
            Self::AllowDangerouslySkip => write!(f, "allow-yolo"),
        }
    }
}

/// Context extracted from the session JSONL for display in the detail pane.
#[derive(Debug, Clone, Serialize)]
pub struct SessionContext {
    pub model: Option<String>,
    pub git_branch: Option<String>,
    pub last_user_message: Option<String>,
    pub last_assistant_message: Option<String>,
    pub session_id: Option<String>,
    /// Recent activity entries (most recent first), summarized for display.
    pub recent_activity: Vec<ActivityEntry>,
}

/// A single summarized activity entry from the JSONL log.
#[derive(Debug, Clone, Serialize)]
pub struct ActivityEntry {
    pub timestamp: String,
    pub kind: String,     // "user", "assistant", "tool", "progress"
    pub summary: String,  // e.g. "Ran: cargo test (passed)" or "Edited src/main.rs"
}

/// A discovered Claude Code agent instance with derived state.
#[derive(Debug, Clone, Serialize)]
pub struct AgentInstance {
    pub pid: u32,
    pub tty: String,
    pub cwd: PathBuf,
    pub project_name: String,
    pub state: AgentState,
    pub permission_mode: PermissionMode,
    pub age_secs: u64,
    pub last_activity_secs: u64,
    pub session_context: Option<SessionContext>,
}
```

**Step 4: Run test to verify it passes**

Run: `cd ~/Projects/leegonzales/cass && cargo test monitor::state -- --nocapture`
Expected: 4 tests pass

**Step 5: Commit**

```bash
cd ~/Projects/leegonzales/cass
git add src/monitor/state.rs src/monitor/mod.rs
git commit -m "feat(monitor): add agent state types and priority model"
```

---

## Task 2: Module Scaffolding (`src/monitor/mod.rs`)

Register the module so it compiles.

**Files:**
- Create: `src/monitor/mod.rs`
- Modify: `src/lib.rs:1` (add `pub mod monitor;`)

**Step 1: Create the module file**

```rust
// src/monitor/mod.rs
//! Live monitoring of active Claude Code instances.
//!
//! Discovers running `claude` processes via the process table,
//! tails their JSONL session files, derives agent state, and
//! renders a dashboard (ftui TUI or streaming JSON).

pub mod state;
pub mod discovery;
pub mod session;
pub mod tui;
```

**Step 2: Register in lib.rs**

Add after line 18 in `src/lib.rs` (after `pub mod update_check;`):

```rust
pub mod monitor;
```

**Step 3: Create stub files so it compiles**

Create empty stubs for `discovery.rs`, `session.rs`, `tui.rs`:

```rust
// src/monitor/discovery.rs
//! Process discovery — finds running Claude Code instances.

// src/monitor/session.rs
//! JSONL session file tail reader and context extraction.

// src/monitor/tui.rs
//! ftui-based live monitoring dashboard.
```

**Step 4: Verify it compiles**

Run: `cd ~/Projects/leegonzales/cass && cargo check 2>&1 | tail -5`
Expected: compiles with no errors (may have warnings for unused modules)

**Step 5: Commit**

```bash
cd ~/Projects/leegonzales/cass
git add src/monitor/ src/lib.rs
git commit -m "feat(monitor): scaffold monitor module with stubs"
```

---

## Task 3: Process Discovery (`src/monitor/discovery.rs`)

Finds running `claude` processes, maps them to TTYs and working directories.

**Files:**
- Create: `src/monitor/discovery.rs`
- Test: inline `#[cfg(test)]` module

**Step 1: Write the failing tests**

```rust
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
        assert_eq!(info.args, vec!["claude", "--allow-dangerously-skip-permissions"]);
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
        assert_eq!(parse_etime("8-17:30:42"), 8 * 86400 + 17 * 3600 + 30 * 60 + 42);
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
        assert_eq!(cwd, Some(PathBuf::from("/Users/lee/Projects/leegonzales/cass")));
    }
}
```

**Step 2: Run test to verify it fails**

Run: `cd ~/Projects/leegonzales/cass && cargo test monitor::discovery --no-run 2>&1 | head -10`
Expected: compile error — functions don't exist

**Step 3: Write the implementation**

```rust
// src/monitor/discovery.rs
//! Process discovery — finds running Claude Code instances.
//!
//! Uses `ps` to find claude processes, `lsof` to map PIDs to working directories,
//! then maps working directories to `~/.claude/projects/` JSONL session files.

use std::collections::HashMap;
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

    let parts: Vec<&str> = line.splitn(4, char::is_whitespace).collect();
    if parts.len() < 4 {
        return None;
    }

    let pid: u32 = parts[0].trim().parse().ok()?;
    let tty = parts[1].trim().to_string();
    let etime = parts[2].trim();
    let args_str = parts[3].trim();

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
    cwd.replace('/', "-").trim_start_matches('-').to_string()
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
```

**Step 4: Run tests**

Run: `cd ~/Projects/leegonzales/cass && cargo test monitor::discovery -- --nocapture`
Expected: all tests pass

**Step 5: Commit**

```bash
cd ~/Projects/leegonzales/cass
git add src/monitor/discovery.rs
git commit -m "feat(monitor): process discovery via ps and lsof"
```

---

## Task 4: JSONL Session Reader (`src/monitor/session.rs`)

Reads the tail of a JSONL session file, derives agent state and context.

**Files:**
- Create: `src/monitor/session.rs`
- Test: inline `#[cfg(test)]` module with fixture data

**Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn make_jsonl(lines: &[&str]) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        for line in lines {
            writeln!(f, "{}", line).unwrap();
        }
        f.flush().unwrap();
        f
    }

    #[test]
    fn derive_working_state() {
        let f = make_jsonl(&[
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Working on it..."}],"stop_reason":null},"timestamp":"2026-03-11T22:14:08Z"}"#,
        ]);
        let (state, _ctx) = derive_state(f.path()).unwrap();
        assert_eq!(state, AgentState::Working);
    }

    #[test]
    fn derive_waiting_input_state() {
        let f = make_jsonl(&[
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Done! What next?"}],"stop_reason":"end_turn","model":"claude-opus-4-6"},"timestamp":"2026-03-11T22:14:08Z"}"#,
        ]);
        let (state, _ctx) = derive_state(f.path()).unwrap();
        assert_eq!(state, AgentState::WaitingInput);
    }

    #[test]
    fn derive_waiting_permission_state() {
        let f = make_jsonl(&[
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_123","name":"Bash","input":{"command":"ls"}}],"stop_reason":null},"timestamp":"2026-03-11T22:14:08Z"}"#,
        ]);
        let (state, _ctx) = derive_state(f.path()).unwrap();
        assert_eq!(state, AgentState::WaitingPermission);
    }

    #[test]
    fn derive_tool_running_state() {
        let f = make_jsonl(&[
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_123","name":"Bash","input":{"command":"ls"}}],"stop_reason":null},"timestamp":"2026-03-11T22:14:06Z"}"#,
            r#"{"type":"progress","data":{"type":"hook_progress","hookEvent":"PreToolUse","hookName":"PreToolUse:Bash"},"timestamp":"2026-03-11T22:14:08Z"}"#,
        ]);
        let (state, _ctx) = derive_state(f.path()).unwrap();
        assert_eq!(state, AgentState::ToolRunning);
    }

    #[test]
    fn user_message_after_assistant_means_working() {
        // If there's a user message after the assistant's final turn,
        // the agent is processing a new request → Working
        let f = make_jsonl(&[
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Done!"}],"stop_reason":"end_turn"},"timestamp":"2026-03-11T22:14:06Z"}"#,
            r#"{"type":"user","message":{"role":"user","content":"Now do X"},"timestamp":"2026-03-11T22:14:08Z"}"#,
        ]);
        let (state, _ctx) = derive_state(f.path()).unwrap();
        assert_eq!(state, AgentState::Working);
    }

    #[test]
    fn extracts_session_context() {
        let f = make_jsonl(&[
            r#"{"type":"user","message":{"role":"user","content":"Fix the bug"},"timestamp":"2026-03-11T22:14:00Z"}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"I'll fix it now"}],"stop_reason":"end_turn","model":"claude-opus-4-6"},"gitBranch":"feat/fix-bug","timestamp":"2026-03-11T22:14:08Z"}"#,
        ]);
        let (_state, ctx) = derive_state(f.path()).unwrap();
        let ctx = ctx.unwrap();
        assert_eq!(ctx.model.as_deref(), Some("claude-opus-4-6"));
        assert_eq!(ctx.git_branch.as_deref(), Some("feat/fix-bug"));
        assert_eq!(ctx.last_user_message.as_deref(), Some("Fix the bug"));
        assert_eq!(ctx.last_assistant_message.as_deref(), Some("I'll fix it now"));
    }

    #[test]
    fn tail_lines_reads_last_n() {
        let f = make_jsonl(&["line1", "line2", "line3", "line4", "line5"]);
        let lines = tail_lines(f.path(), 3).unwrap();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "line3");
        assert_eq!(lines[2], "line5");
    }
}
```

**Step 2: Run test to verify it fails**

Run: `cd ~/Projects/leegonzales/cass && cargo test monitor::session --no-run 2>&1 | head -10`
Expected: compile error

**Step 3: Write the implementation**

```rust
// src/monitor/session.rs
//! JSONL session file tail reader and context extraction.
//!
//! Reads the last N lines of a Claude Code JSONL session file,
//! parses them to derive agent state and extract display context.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use std::time::SystemTime;

use serde_json::Value;

use crate::monitor::state::{ActivityEntry, AgentState, SessionContext};

/// Read the last `n` lines from a file efficiently.
///
/// Seeks to near the end and reads forward. Falls back to reading
/// the entire file if it's small enough.
pub fn tail_lines(path: &Path, n: usize) -> std::io::Result<Vec<String>> {
    let mut file = File::open(path)?;
    let file_len = file.metadata()?.len();

    // For small files, just read everything
    if file_len < 64 * 1024 {
        let reader = BufReader::new(file);
        let all_lines: Vec<String> = reader.lines().collect::<Result<_, _>>()?;
        let start = all_lines.len().saturating_sub(n);
        return Ok(all_lines[start..].to_vec());
    }

    // For large files, seek to near the end
    // Estimate: JSONL lines average ~2KB, read 2x what we need
    let seek_pos = file_len.saturating_sub((n as u64) * 4096);
    file.seek(SeekFrom::Start(seek_pos))?;

    let reader = BufReader::new(file);
    let mut lines: Vec<String> = Vec::new();

    for line in reader.lines() {
        let line = line?;
        if !line.is_empty() {
            lines.push(line);
        }
    }

    // If we seeked into the middle of a line, the first "line" is partial — skip it
    if seek_pos > 0 && !lines.is_empty() {
        lines.remove(0);
    }

    let start = lines.len().saturating_sub(n);
    Ok(lines[start..].to_vec())
}

/// Derive agent state and context from a JSONL session file.
///
/// Reads the last ~20 lines and walks backwards to determine state.
pub fn derive_state(path: &Path) -> std::io::Result<(AgentState, Option<SessionContext>)> {
    let lines = tail_lines(path, 20)?;

    if lines.is_empty() {
        return Ok((AgentState::Starting, None));
    }

    // Parse all lines into JSON values
    let entries: Vec<Value> = lines
        .iter()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();

    if entries.is_empty() {
        return Ok((AgentState::Starting, None));
    }

    // Extract context from all entries
    let mut context = SessionContext {
        model: None,
        git_branch: None,
        last_user_message: None,
        last_assistant_message: None,
        session_id: None,
        recent_activity: Vec::new(),
    };

    // Walk all entries to collect context
    for entry in &entries {
        // Extract model and branch from any entry
        if let Some(model) = entry.get("message").and_then(|m| m.get("model")).and_then(|v| v.as_str()) {
            context.model = Some(model.to_string());
        }
        if let Some(branch) = entry.get("gitBranch").and_then(|v| v.as_str()) {
            context.git_branch = Some(branch.to_string());
        }
        if let Some(sid) = entry.get("sessionId").and_then(|v| v.as_str()) {
            context.session_id = Some(sid.to_string());
        }

        // Track last user and assistant messages
        let entry_type = entry.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if entry_type == "user" {
            if let Some(msg) = extract_user_text(entry) {
                context.last_user_message = Some(msg);
            }
        } else if entry_type == "assistant" {
            if let Some(msg) = extract_assistant_text(entry) {
                context.last_assistant_message = Some(msg);
            }
        }

        // Build activity log
        if let Some(activity) = entry_to_activity(entry) {
            context.recent_activity.push(activity);
        }
    }

    // Keep only last 5 activities, most recent first
    context.recent_activity.reverse();
    context.recent_activity.truncate(5);

    // Derive state by walking entries backwards
    let state = derive_state_from_entries(&entries);

    Ok((state, Some(context)))
}

/// Determine the agent state from parsed JSONL entries.
fn derive_state_from_entries(entries: &[Value]) -> AgentState {
    // Walk backwards to find the last meaningful entry
    for entry in entries.iter().rev() {
        let entry_type = entry.get("type").and_then(|v| v.as_str()).unwrap_or("");

        match entry_type {
            "user" => {
                // If the last meaningful entry is a user message,
                // the agent is processing it → Working
                let content = entry
                    .get("message")
                    .and_then(|m| m.get("content"));

                // Check if this is a tool_result (permission response)
                if let Some(Value::Array(parts)) = content {
                    if parts.iter().any(|p| p.get("type").and_then(|v| v.as_str()) == Some("tool_result")) {
                        // Tool result received → agent is processing → Working
                        return AgentState::Working;
                    }
                }

                // Regular user message → agent is processing
                return AgentState::Working;
            }
            "assistant" => {
                let message = entry.get("message").unwrap_or(&Value::Null);
                let stop_reason = message.get("stop_reason").and_then(|v| v.as_str());
                let content = message.get("content");

                // Check if there's a tool_use in the content
                let has_tool_use = if let Some(Value::Array(parts)) = content {
                    parts.iter().any(|p| {
                        p.get("type").and_then(|v| v.as_str()) == Some("tool_use")
                    })
                } else {
                    false
                };

                if has_tool_use && stop_reason.is_none() {
                    // Tool use emitted, not yet responded to → WaitingPermission
                    return AgentState::WaitingPermission;
                }

                match stop_reason {
                    None => return AgentState::Working, // Still streaming
                    Some("end_turn") => return AgentState::WaitingInput,
                    Some(_) => return AgentState::WaitingInput,
                }
            }
            "progress" => {
                if let Some(data) = entry.get("data") {
                    if data.get("type").and_then(|v| v.as_str()) == Some("hook_progress") {
                        return AgentState::ToolRunning;
                    }
                }
                // Other progress types — keep looking
                continue;
            }
            "queue-operation" => {
                if entry.get("operation").and_then(|v| v.as_str()) == Some("enqueue") {
                    return AgentState::Queued;
                }
                continue;
            }
            "file-history-snapshot" => continue, // Skip metadata entries
            _ => continue,
        }
    }

    AgentState::Starting
}

/// Extract user message text from a JSONL entry.
fn extract_user_text(entry: &Value) -> Option<String> {
    let content = entry.get("message")?.get("content")?;
    match content {
        Value::String(s) => {
            // Skip system/hook messages
            if s.starts_with("<local-command") || s.starts_with("<system-reminder") {
                return None;
            }
            Some(truncate_str(s, 200))
        }
        Value::Array(parts) => {
            // Look for text parts, skip tool_results
            for part in parts {
                if part.get("type").and_then(|v| v.as_str()) == Some("text") {
                    if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                        if !text.starts_with("[Request interrupted") {
                            return Some(truncate_str(text, 200));
                        }
                    }
                }
            }
            None
        }
        _ => None,
    }
}

/// Extract assistant message text from a JSONL entry.
fn extract_assistant_text(entry: &Value) -> Option<String> {
    let content = entry.get("message")?.get("content")?;
    if let Value::Array(parts) = content {
        for part in parts {
            if part.get("type").and_then(|v| v.as_str()) == Some("text") {
                if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        return Some(truncate_str(trimmed, 200));
                    }
                }
            }
        }
    }
    None
}

/// Convert a JSONL entry to a summarized activity entry.
fn entry_to_activity(entry: &Value) -> Option<ActivityEntry> {
    let entry_type = entry.get("type")?.as_str()?;
    let timestamp = entry
        .get("timestamp")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Extract just the time portion (HH:MM:SS)
    let time = if timestamp.len() >= 19 {
        timestamp[11..19].to_string()
    } else {
        timestamp.clone()
    };

    match entry_type {
        "user" => {
            let text = extract_user_text(entry)?;
            Some(ActivityEntry {
                timestamp: time,
                kind: "user".into(),
                summary: text,
            })
        }
        "assistant" => {
            let message = entry.get("message")?;
            let content = message.get("content")?;

            if let Value::Array(parts) = content {
                // Summarize tool uses
                for part in parts {
                    if part.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                        let tool = part.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                        let input = part.get("input").unwrap_or(&Value::Null);

                        let summary = match tool {
                            "Bash" => {
                                let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("...");
                                format!("Ran: {}", truncate_str(cmd, 60))
                            }
                            "Edit" => {
                                let file = input.get("file_path").and_then(|v| v.as_str()).unwrap_or("...");
                                let short = Path::new(file).file_name()
                                    .and_then(|f| f.to_str())
                                    .unwrap_or(file);
                                format!("Edited {}", short)
                            }
                            "Write" => {
                                let file = input.get("file_path").and_then(|v| v.as_str()).unwrap_or("...");
                                let short = Path::new(file).file_name()
                                    .and_then(|f| f.to_str())
                                    .unwrap_or(file);
                                format!("Wrote {}", short)
                            }
                            "Read" => {
                                let file = input.get("file_path").and_then(|v| v.as_str()).unwrap_or("...");
                                let short = Path::new(file).file_name()
                                    .and_then(|f| f.to_str())
                                    .unwrap_or(file);
                                format!("Read {}", short)
                            }
                            _ => format!("Used {}", tool),
                        };

                        return Some(ActivityEntry {
                            timestamp: time,
                            kind: "tool".into(),
                            summary,
                        });
                    }

                    // Text response
                    if part.get("type").and_then(|v| v.as_str()) == Some("text") {
                        if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                            let trimmed = text.trim();
                            if !trimmed.is_empty() {
                                return Some(ActivityEntry {
                                    timestamp: time,
                                    kind: "assistant".into(),
                                    summary: truncate_str(trimmed, 80),
                                });
                            }
                        }
                    }
                }
            }
            None
        }
        "progress" => {
            let data = entry.get("data")?;
            if data.get("type").and_then(|v| v.as_str()) == Some("hook_progress") {
                let hook = data.get("hookName").and_then(|v| v.as_str()).unwrap_or("hook");
                return Some(ActivityEntry {
                    timestamp: time,
                    kind: "progress".into(),
                    summary: format!("Running {}", hook),
                });
            }
            None
        }
        _ => None,
    }
}

/// Compute seconds since the file was last modified.
pub fn file_staleness_secs(path: &Path) -> u64 {
    path.metadata()
        .and_then(|m| m.modified())
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .map(|d| d.as_secs())
        .unwrap_or(u64::MAX)
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max.saturating_sub(3)])
    }
}
```

**Step 4: Run tests**

Run: `cd ~/Projects/leegonzales/cass && cargo test monitor::session -- --nocapture`
Expected: all 7 tests pass

Note: `tempfile` is needed as a dev dependency. Check if it's already in Cargo.toml; if not, add it:
```bash
cd ~/Projects/leegonzales/cass && grep -q 'tempfile' Cargo.toml && echo "already present" || echo 'tempfile = "*"' >> Cargo.toml
```

**Step 5: Commit**

```bash
cd ~/Projects/leegonzales/cass
git add src/monitor/session.rs
git commit -m "feat(monitor): JSONL tail reader and state derivation"
```

---

## Task 5: CLI Integration (`src/lib.rs`)

Add the `Commands::Monitor` variant and dispatch to a handler function.

**Files:**
- Modify: `src/lib.rs:731` (before closing `}` of Commands enum)
- Modify: `src/lib.rs:2547` (add to tracing match arm)
- Modify: `src/lib.rs:~3261` (add dispatch in inner match)
- Modify: `src/lib.rs:4308` (describe_command)
- Modify: `src/lib.rs:4342` (is_robot_mode)

**Step 1: Add the Commands variant**

Add before line 732 (the `}` closing the Commands enum) in `src/lib.rs`:

```rust
    /// Live monitoring dashboard for active Claude Code instances.
    ///
    /// Discovers running claude processes, tails their JSONL session files,
    /// and shows a real-time status dashboard.
    #[command(name = "monitor", about = "Live dashboard of active Claude Code agents")]
    Monitor {
        /// Output as streaming JSON instead of TUI
        #[arg(long)]
        json: bool,

        /// Refresh interval in seconds
        #[arg(long, default_value = "2")]
        interval: u64,

        /// Override data directory
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
```

**Step 2: Add to tracing match arm**

At line 2547, add `Monitor` to the list of commands that get stderr tracing:

Find: `Commands::Index { .. } | Commands::Search { .. } | Commands::Stats { .. } | Commands::Diag { .. } | Commands::Status { .. } | Commands::View { .. } | Commands::Pages { .. } | Commands::Import(_) | Commands::Analytics(_)`

Add `| Commands::Monitor { .. }` to the end of this match arm.

**Step 3: Add dispatch in inner match**

After line 3261 (`_ => {}` in the inner match), or before it, add:

```rust
Commands::Monitor { json, interval, data_dir } => {
    crate::monitor::run_monitor(*json, *interval, data_dir.clone())?;
}
```

**Step 4: Add to describe_command (line 4308-4337)**

Add a line:
```rust
Commands::Monitor { .. } => "monitor",
```

**Step 5: Add to is_robot_mode (line 4342-4393)**

Add a line:
```rust
Commands::Monitor { json, .. } => *json,
```

**Step 6: Add the handler function**

In `src/monitor/mod.rs`, add:

```rust
use crate::CliResult;
use std::path::PathBuf;

/// Entry point for `cass monitor`.
pub fn run_monitor(json: bool, interval: u64, data_dir: Option<PathBuf>) -> CliResult<()> {
    if json {
        run_json_monitor(interval, data_dir)
    } else {
        run_tui_monitor(interval, data_dir)
    }
}

fn run_json_monitor(interval: u64, _data_dir: Option<PathBuf>) -> CliResult<()> {
    // TODO: implement in Task 6
    eprintln!("JSON monitor mode not yet implemented");
    Ok(())
}

fn run_tui_monitor(interval: u64, _data_dir: Option<PathBuf>) -> CliResult<()> {
    // TODO: implement in Task 7
    eprintln!("TUI monitor not yet implemented");
    Ok(())
}
```

**Step 7: Verify it compiles and runs**

Run: `cd ~/Projects/leegonzales/cass && cargo build 2>&1 | tail -5`
Expected: compiles

Run: `cd ~/Projects/leegonzales/cass && cargo run -- monitor --help`
Expected: shows help text for the monitor subcommand

**Step 8: Commit**

```bash
cd ~/Projects/leegonzales/cass
git add src/lib.rs src/monitor/mod.rs
git commit -m "feat(monitor): wire up CLI subcommand and dispatch"
```

---

## Task 6: JSON Output Mode

Implement `cass monitor --json` streaming output.

**Files:**
- Modify: `src/monitor/mod.rs`
- Test: inline tests

**Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_serializes_to_json() {
        let snapshot = MonitorSnapshot {
            timestamp: "2026-03-11T22:14:08Z".into(),
            agents_active: 2,
            agents_needing_attention: 1,
            agents: vec![],
        };
        let json = serde_json::to_string(&snapshot).unwrap();
        assert!(json.contains("\"agents_active\":2"));
        assert!(json.contains("\"agents_needing_attention\":1"));
    }
}
```

**Step 2: Run test to verify it fails**

Run: `cd ~/Projects/leegonzales/cass && cargo test monitor::tests --no-run 2>&1 | head -10`

**Step 3: Implement MonitorSnapshot and the JSON monitor loop**

Update `src/monitor/mod.rs`:

```rust
// src/monitor/mod.rs
//! Live monitoring of active Claude Code instances.

pub mod discovery;
pub mod session;
pub mod state;
pub mod tui;

use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use serde::Serialize;

use crate::CliResult;
use state::AgentInstance;

/// A point-in-time snapshot of all monitored agents.
#[derive(Debug, Clone, Serialize)]
pub struct MonitorSnapshot {
    pub timestamp: String,
    pub agents_active: usize,
    pub agents_needing_attention: usize,
    pub agents: Vec<AgentInstance>,
}

/// Collect a single snapshot of all active Claude Code agents.
pub fn collect_snapshot() -> MonitorSnapshot {
    let home = dirs::home_dir().unwrap_or_default();
    let claude_projects = home.join(".claude").join("projects");
    let own_pid = std::process::id();

    let mut agents: Vec<AgentInstance> = discovery::discover_agents(&claude_projects, own_pid)
        .into_iter()
        .map(|(mut inst, session_path)| {
            if let Some(ref path) = session_path {
                // Derive state from JSONL
                if let Ok((state, ctx)) = session::derive_state(path) {
                    inst.state = state;
                    inst.session_context = ctx;
                }
                inst.last_activity_secs = session::file_staleness_secs(path);

                // Override to Idle if file is stale and state looks active
                if inst.last_activity_secs > 30
                    && !inst.state.needs_attention()
                    && inst.state != state::AgentState::WaitingInput
                {
                    inst.state = state::AgentState::Idle;
                }
            }
            inst
        })
        .collect();

    // Sort by state priority (most urgent first)
    agents.sort_by_key(|a| a.state.priority());

    let needing_attention = agents.iter().filter(|a| a.state.needs_attention()).count();

    MonitorSnapshot {
        timestamp: chrono::Utc::now().to_rfc3339(),
        agents_active: agents.len(),
        agents_needing_attention: needing_attention,
        agents,
    }
}

/// Entry point for `cass monitor`.
pub fn run_monitor(json: bool, interval: u64, data_dir: Option<PathBuf>) -> CliResult<()> {
    if json {
        run_json_monitor(interval)
    } else {
        tui::run_tui_monitor(interval)
    }
}

fn run_json_monitor(interval: u64) -> CliResult<()> {
    loop {
        let snapshot = collect_snapshot();
        let json = serde_json::to_string(&snapshot).map_err(|e| {
            crate::CliError {
                code: 65,
                kind: "data",
                message: format!("JSON serialization failed: {e}"),
                hint: None,
            }
        })?;
        println!("{json}");

        thread::sleep(Duration::from_secs(interval));
    }
}
```

Note: `dirs` crate is needed. Check if it's in Cargo.toml; if not, add `dirs = "*"`.

**Step 4: Run tests and verify JSON mode works**

Run: `cd ~/Projects/leegonzales/cass && cargo test monitor -- --nocapture`
Expected: all tests pass

Run: `cd ~/Projects/leegonzales/cass && timeout 5 cargo run -- monitor --json 2>&1 | head -3`
Expected: JSON output showing active agents (or empty agents list)

**Step 5: Commit**

```bash
cd ~/Projects/leegonzales/cass
git add src/monitor/mod.rs
git commit -m "feat(monitor): JSON streaming output mode"
```

---

## Task 7: TUI Dashboard (`src/monitor/tui.rs`)

The main event — ftui Elm-architecture dashboard with ASCII art header, instance table, and detail pane.

**Files:**
- Create: `src/monitor/tui.rs`
- Uses: ftui Model trait, Flex layout, Block/Paragraph/VirtualizedList widgets

**Step 1: Write the MonitorApp struct and message enum**

```rust
// src/monitor/tui.rs
//! ftui-based live monitoring dashboard.

use std::time::{Duration, Instant};

use ftui::layout::{Constraint, Flex};
use ftui::text::{Line, Span, Text};
use ftui::widgets::block::{Block, Borders, BorderType};
use ftui::widgets::paragraph::Paragraph;
use ftui::widgets::Widget;
use ftui::core::geometry::Rect;
use ftui::{Cmd, Event, Frame, KeyCode, Model, ProgramConfig};

use crate::CliResult;
use crate::monitor::{collect_snapshot, MonitorSnapshot};
use crate::monitor::state::{AgentInstance, AgentState};

const ASCII_HEADER: &str = r#"
  ▄████▄   ▄▄▄        ██████   ██████
 ▓█    ▀  ▒████▄    ▒██    ▒ ▒██    ▒
 ▒▓█      ▒██  ▀█▄  ░ ▓██▄   ░ ▓██▄     m o n i t o r
 ▒▓▓▄ ▄██▒░██▄▄▄▄██   ▒   ██▒  ▒   ██▒
 ▒ ▓███▀ ░ ▓█   ▓██▒▒██████▒ ▒██████▒
 ░ ▒░▒  ░  ▒▒   ▓▒█░▒ ▒▓▒ ▒ ░▒ ▒▓▒ ▒ ░"#;

/// Message type for the monitor TUI.
#[derive(Debug)]
enum Msg {
    /// Terminal event (key press, resize, etc.)
    Key(KeyCode),
    /// Refresh tick — re-collect snapshot
    Tick,
    /// Snapshot collected from background
    SnapshotReady(MonitorSnapshot),
    /// Quit the app
    Quit,
    /// Ignored event
    Noop,
}

impl From<Event> for Msg {
    fn from(event: Event) -> Self {
        match event {
            Event::Key(key) => match key.code {
                KeyCode::Char('q') | KeyCode::Escape => Msg::Quit,
                KeyCode::Char('c') if key.modifiers.contains(ftui::Modifiers::CONTROL) => Msg::Quit,
                KeyCode::Char('r') => Msg::Tick, // Force refresh
                KeyCode::Up | KeyCode::Char('k') => Msg::Key(KeyCode::Up),
                KeyCode::Down | KeyCode::Char('j') => Msg::Key(KeyCode::Down),
                KeyCode::Enter => Msg::Key(KeyCode::Enter),
                KeyCode::Char(c) if c.is_ascii_digit() => Msg::Key(KeyCode::Char(c)),
                _ => Msg::Noop,
            },
            _ => Msg::Noop,
        }
    }
}

/// Monitor TUI application state.
struct MonitorApp {
    snapshot: MonitorSnapshot,
    selected: usize,
    detail_expanded: bool,
    interval: Duration,
    last_refresh: Instant,
    blink_phase: bool,
}

impl Default for MonitorApp {
    fn default() -> Self {
        Self {
            snapshot: MonitorSnapshot {
                timestamp: String::new(),
                agents_active: 0,
                agents_needing_attention: 0,
                agents: vec![],
            },
            selected: 0,
            detail_expanded: true,
            interval: Duration::from_secs(2),
            last_refresh: Instant::now() - Duration::from_secs(10), // trigger immediate refresh
            blink_phase: false,
        }
    }
}

impl Model for MonitorApp {
    type Message = Msg;

    fn init(&mut self) -> Cmd<Msg> {
        // Collect initial snapshot
        Cmd::task(|| Msg::SnapshotReady(collect_snapshot()))
    }

    fn update(&mut self, msg: Msg) -> Cmd<Msg> {
        match msg {
            Msg::Quit => return Cmd::quit(),

            Msg::Tick => {
                self.blink_phase = !self.blink_phase;

                // Only refresh if interval has elapsed
                if self.last_refresh.elapsed() >= self.interval {
                    self.last_refresh = Instant::now();
                    return Cmd::task(|| Msg::SnapshotReady(collect_snapshot()));
                }
            }

            Msg::SnapshotReady(snapshot) => {
                self.snapshot = snapshot;
                // Clamp selected index
                if !self.snapshot.agents.is_empty() {
                    self.selected = self.selected.min(self.snapshot.agents.len() - 1);
                }
            }

            Msg::Key(KeyCode::Up) => {
                if self.selected > 0 {
                    self.selected -= 1;
                }
            }
            Msg::Key(KeyCode::Down) => {
                if self.selected + 1 < self.snapshot.agents.len() {
                    self.selected += 1;
                }
            }
            Msg::Key(KeyCode::Enter) => {
                self.detail_expanded = !self.detail_expanded;
            }
            Msg::Key(KeyCode::Char(c)) if c.is_ascii_digit() => {
                let idx = c.to_digit(10).unwrap_or(0) as usize;
                if idx > 0 && idx <= self.snapshot.agents.len() {
                    self.selected = idx - 1;
                }
            }
            _ => {}
        }

        Cmd::none()
    }

    fn view(&self, frame: &mut Frame) {
        let area = Rect::from_size(frame.buffer.width(), frame.buffer.height());

        if area.height < 10 || area.width < 40 {
            Paragraph::new("Terminal too small for monitor")
                .render(area, frame);
            return;
        }

        // Layout: header | table | detail | footer
        let detail_height = if self.detail_expanded { 10 } else { 0 };
        let chunks = Flex::vertical()
            .constraints([
                Constraint::Fixed(8),          // Header with ASCII art
                Constraint::Min(4),            // Instance table
                Constraint::Fixed(detail_height), // Detail pane
                Constraint::Fixed(1),          // Footer/keybindings
            ])
            .split(area);

        self.render_header(frame, chunks[0]);
        self.render_table(frame, chunks[1]);
        if self.detail_expanded && !self.snapshot.agents.is_empty() {
            self.render_detail(frame, chunks[2]);
        }
        self.render_footer(frame, chunks[3]);
    }
}

impl MonitorApp {
    fn render_header(&self, frame: &mut Frame, area: Rect) {
        let header_style = ftui::Style::default();
        let block = Block::new()
            .borders(Borders::BOTTOM)
            .border_type(BorderType::Double)
            .style(header_style);
        let inner = block.inner(area);
        block.render(area, frame);

        // Build header text
        let mut lines: Vec<Line> = Vec::new();
        for art_line in ASCII_HEADER.lines().skip(1) { // skip first empty line
            lines.push(Line::from(Span::styled(
                art_line.to_string(),
                ftui::Style::default(),
            )));
        }

        // Stats line
        let attention = self.snapshot.agents_needing_attention;
        let total = self.snapshot.agents_active;
        let stats = format!("  {} agents active", total);
        let attention_str = if attention > 0 {
            format!(" | {} need attention", attention)
        } else {
            String::new()
        };
        lines.push(Line::from(vec![
            Span::styled(stats, ftui::Style::default()),
            Span::styled(attention_str, ftui::Style::default().bold()),
        ]));

        Paragraph::new(Text::from_lines(lines)).render(inner, frame);
    }

    fn render_table(&self, frame: &mut Frame, area: Rect) {
        let block = Block::new()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(" Agents ");
        let inner = block.inner(area);
        block.render(area, frame);

        if self.snapshot.agents.is_empty() {
            Paragraph::new("  No active Claude Code instances found. Waiting...")
                .render(inner, frame);
            return;
        }

        // Header line
        let mut lines: Vec<Line> = Vec::new();
        lines.push(Line::from(format!(
            " {:>2} │ {:<24} │ {:<16} │ {:<5} │ {:>8} │ {:>5}",
            "#", "PROJECT", "STATE", "TTY", "AGE", "LAST"
        )));
        lines.push(Line::from(format!(
            " ───┼──────────────────────────┼──────────────────┼───────┼──────────┼───────"
        )));

        for (i, agent) in self.snapshot.agents.iter().enumerate() {
            let selected = i == self.selected;
            let state_str = format_state(&agent.state, self.blink_phase);
            let age = format_duration(agent.age_secs);
            let last = format_duration(agent.last_activity_secs);

            let prefix = if selected { "▶" } else { " " };
            let line_str = format!(
                "{}{:>2} │ {:<24} │ {:<16} │ {:<5} │ {:>8} │ {:>5}",
                prefix,
                i + 1,
                truncate(&agent.project_name, 24),
                state_str,
                agent.tty,
                age,
                last,
            );

            let style = if selected {
                ftui::Style::default().bold()
            } else {
                match agent.state {
                    AgentState::Idle => ftui::Style::default(),
                    _ => ftui::Style::default(),
                }
            };

            lines.push(Line::from(Span::styled(line_str, style)));
        }

        Paragraph::new(Text::from_lines(lines)).render(inner, frame);
    }

    fn render_detail(&self, frame: &mut Frame, area: Rect) {
        if area.height == 0 {
            return;
        }

        let agent = match self.snapshot.agents.get(self.selected) {
            Some(a) => a,
            None => return,
        };

        let title = format!(
            " [{}] {}  {} ",
            self.selected + 1,
            agent.project_name,
            agent.tty
        );
        let block = Block::new()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(title);
        let inner = block.inner(area);
        block.render(area, frame);

        let mut lines: Vec<Line> = Vec::new();

        // Mode / Model / Branch line
        if let Some(ref ctx) = agent.session_context {
            let model = ctx.model.as_deref().unwrap_or("?");
            let branch = ctx.git_branch.as_deref().unwrap_or("?");
            lines.push(Line::from(format!(
                "  Mode: {} │ Model: {} │ Branch: {}",
                agent.permission_mode, model, branch
            )));

            // Last messages
            if let Some(ref msg) = ctx.last_user_message {
                lines.push(Line::from(format!("  User: {}", truncate(msg, 70))));
            }
            if let Some(ref msg) = ctx.last_assistant_message {
                lines.push(Line::from(format!("  Agent: {}", truncate(msg, 70))));
            }

            // Recent activity
            if !ctx.recent_activity.is_empty() {
                lines.push(Line::from("  ─── Recent ───"));
                for activity in ctx.recent_activity.iter().take(3) {
                    lines.push(Line::from(format!(
                        "    {} {} {}",
                        activity.timestamp,
                        activity.kind,
                        truncate(&activity.summary, 50)
                    )));
                }
            }
        } else {
            lines.push(Line::from("  No session data available"));
        }

        Paragraph::new(Text::from_lines(lines)).render(inner, frame);
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect) {
        let footer = " ↑↓/jk: select │ Enter: toggle detail │ r: refresh │ 1-9: jump │ q: quit";
        Paragraph::new(footer)
            .style(ftui::Style::default())
            .render(area, frame);
    }
}

/// Format agent state with icon for display.
fn format_state(state: &AgentState, blink: bool) -> String {
    match state {
        AgentState::Working => "⚡ WORKING".into(),
        AgentState::ToolRunning => "⚙  TOOL RUN".into(),
        AgentState::WaitingPermission => {
            if blink { "⚠  PERMISSION" } else { "   PERMISSION" }.into()
        }
        AgentState::WaitingInput => {
            if blink { "🔴 NEEDS INPUT" } else { "   NEEDS INPUT" }.into()
        }
        AgentState::Queued => "📋 QUEUED".into(),
        AgentState::Idle => "💤 IDLE".into(),
        AgentState::Starting => "⏳ STARTING".into(),
    }
}

/// Format a duration in seconds to a compact human-readable string.
fn format_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        format!("{}h {}m", h, m)
    } else {
        let d = secs / 86400;
        let h = (secs % 86400) / 3600;
        format!("{}d {}h", d, h)
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max.saturating_sub(1)])
    }
}

/// Launch the monitor TUI.
pub fn run_tui_monitor(interval: u64) -> CliResult<()> {
    let mut app = MonitorApp::default();
    app.interval = Duration::from_secs(interval);

    let config = ProgramConfig::fullscreen();

    let mut program = ftui::Program::with_native_backend(app, config)
        .map_err(|e| crate::CliError {
            code: 71,
            kind: "tui",
            message: format!("Failed to initialize TUI: {e}"),
            hint: None,
        })?;

    program.run().map_err(|e| crate::CliError {
        code: 71,
        kind: "tui",
        message: format!("TUI runtime error: {e}"),
        hint: None,
    })?;

    Ok(())
}
```

**Step 2: Verify it compiles**

Run: `cd ~/Projects/leegonzales/cass && cargo build 2>&1 | tail -10`

Expect: may need adjustments based on exact ftui API. Key things that might differ:
- `Event::Key` structure — check `src/ui/app.rs` `From<Event>` impl for exact pattern
- `Rect::from_size` — might be `Rect::new(0, 0, w, h)` or similar
- `Text::from_lines` — might be `Text::from(lines)` or `Text::raw`
- Widget `render` signature — check if it's `render(area, frame)` or `render(area, &mut frame.buffer)`

The implementing agent should read `src/ui/app.rs` lines ~12600-12700 (the `From<Event>` impl) and ~12900-13000 (the `view` method) to match the exact ftui API.

**Step 3: Test the TUI**

Run: `cd ~/Projects/leegonzales/cass && cargo run -- monitor`
Expected: fullscreen TUI showing active Claude Code instances

**Step 4: Commit**

```bash
cd ~/Projects/leegonzales/cass
git add src/monitor/tui.rs
git commit -m "feat(monitor): ftui live dashboard with ASCII header"
```

---

## Task 8: Tick Timer for Auto-Refresh

The ftui framework needs a tick event source to trigger periodic refreshes.

**Files:**
- Modify: `src/monitor/tui.rs`

**Step 1: Check how CassApp handles ticks**

Read `src/ui/app.rs` and search for "Tick" or "tick" to find how the existing TUI handles periodic events. The ftui `ProgramConfig` may have a `tick_rate` or similar field. Alternatively, `Cmd::task` with a sleep can simulate ticks.

**Step 2: Implement tick-based refresh**

If ftui supports `ProgramConfig::tick_rate()`:
```rust
let mut config = ProgramConfig::fullscreen();
config.tick_rate = Duration::from_millis(500);
```

If not, use `Cmd::task` with sleep in the `init` and after each `Tick`:
```rust
fn init(&mut self) -> Cmd<Msg> {
    Cmd::batch(vec![
        Cmd::task(|| Msg::SnapshotReady(collect_snapshot())),
        schedule_tick(self.interval),
    ])
}

fn update(&mut self, msg: Msg) -> Cmd<Msg> {
    match msg {
        Msg::Tick => {
            self.blink_phase = !self.blink_phase;
            Cmd::batch(vec![
                Cmd::task(|| Msg::SnapshotReady(collect_snapshot())),
                schedule_tick(self.interval),
            ])
        }
        // ...
    }
}

fn schedule_tick(interval: Duration) -> Cmd<Msg> {
    Cmd::task(move || {
        std::thread::sleep(interval);
        Msg::Tick
    })
}
```

**Step 3: Verify auto-refresh works**

Run: `cd ~/Projects/leegonzales/cass && cargo run -- monitor`
Expected: dashboard updates every 2 seconds showing current agent states

**Step 4: Commit**

```bash
cd ~/Projects/leegonzales/cass
git add src/monitor/tui.rs
git commit -m "feat(monitor): auto-refresh tick timer"
```

---

## Task 9: Integration Test

End-to-end test with real session data.

**Files:**
- Create: `tests/monitor_integration.rs` (or inline in mod.rs)

**Step 1: Write the test**

```rust
// In src/monitor/mod.rs, add to tests module:

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_serializes_to_json() {
        let snapshot = MonitorSnapshot {
            timestamp: "2026-03-11T22:14:08Z".into(),
            agents_active: 2,
            agents_needing_attention: 1,
            agents: vec![],
        };
        let json = serde_json::to_string(&snapshot).unwrap();
        assert!(json.contains("\"agents_active\":2"));
        assert!(json.contains("\"agents_needing_attention\":1"));
    }

    #[test]
    fn collect_snapshot_runs_without_panic() {
        // This test verifies the full pipeline runs without errors
        // on the current system (may find 0 agents if none running)
        let snapshot = collect_snapshot();
        // Should at minimum serialize to JSON
        let _json = serde_json::to_string(&snapshot).unwrap();
    }

    #[test]
    fn snapshot_agents_sorted_by_priority() {
        let snapshot = collect_snapshot();
        for window in snapshot.agents.windows(2) {
            assert!(
                window[0].state.priority() <= window[1].state.priority(),
                "Agents should be sorted by state priority"
            );
        }
    }
}
```

**Step 2: Run all monitor tests**

Run: `cd ~/Projects/leegonzales/cass && cargo test monitor -- --nocapture`
Expected: all tests pass

**Step 3: Commit**

```bash
cd ~/Projects/leegonzales/cass
git add src/monitor/
git commit -m "test(monitor): integration tests for snapshot collection"
```

---

## Summary

| Task | Description | Est. Lines | Dependencies |
|------|-------------|-----------|--------------|
| 1 | Agent state types | ~120 | None |
| 2 | Module scaffolding | ~20 | Task 1 |
| 3 | Process discovery | ~200 | Task 1, 2 |
| 4 | JSONL session reader | ~250 | Task 1, 2 |
| 5 | CLI integration | ~30 (edits) | Task 1-4 |
| 6 | JSON output mode | ~80 | Task 3, 4, 5 |
| 7 | TUI dashboard | ~300 | Task 3, 4, 5 |
| 8 | Tick timer | ~20 | Task 7 |
| 9 | Integration tests | ~40 | Task 6 |

**Total estimated:** ~1,060 lines across 5 new files + edits to lib.rs and mod.rs.

**Critical notes for implementing agent:**
- Read `src/ui/app.rs` lines 12600-12700 for exact ftui `Event` → `Msg` conversion pattern
- Read `src/ui/app.rs` lines 12900+ for exact `view()` rendering pattern and widget API
- The `ftui` crate is a local path dependency — if it's not checked out, `cargo build` will fail. Check `ls ../frankentui/` first.
- `dirs` crate may need to be added to Cargo.toml for `home_dir()`
- `tempfile` dev-dependency may need to be added for session.rs tests
- The `CliError` struct fields (`code`, `kind`, `message`, `hint`) — verify exact field names by reading `src/lib.rs` grep for `pub struct CliError`
