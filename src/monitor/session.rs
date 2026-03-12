//! JSONL session file tail reader and context extraction.
//!
//! Reads the last N lines of a Claude Code JSONL session file,
//! parses them to derive agent state and extract display context.

use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
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

    // If we seeked into the middle of a line, the first "line" is partial -- skip it
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
        if let Some(model) = entry
            .get("message")
            .and_then(|m| m.get("model"))
            .and_then(|v| v.as_str())
        {
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
                // the agent is processing it -> Working
                let content = entry.get("message").and_then(|m| m.get("content"));

                // Check if this is a tool_result (permission response)
                if let Some(Value::Array(parts)) = content {
                    if parts
                        .iter()
                        .any(|p| p.get("type").and_then(|v| v.as_str()) == Some("tool_result"))
                    {
                        // Tool result received -> agent is processing -> Working
                        return AgentState::Working;
                    }
                }

                // Regular user message -> agent is processing
                return AgentState::Working;
            }
            "assistant" => {
                let message = entry.get("message").unwrap_or(&Value::Null);
                let stop_reason = message.get("stop_reason").and_then(|v| v.as_str());
                let content = message.get("content");

                // Check if there's a tool_use in the content
                let has_tool_use = if let Some(Value::Array(parts)) = content {
                    parts
                        .iter()
                        .any(|p| p.get("type").and_then(|v| v.as_str()) == Some("tool_use"))
                } else {
                    false
                };

                if has_tool_use && stop_reason.is_none() {
                    // Tool use emitted, not yet responded to -> WaitingPermission
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
                // Other progress types -- keep looking
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
                                let cmd = input
                                    .get("command")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("...");
                                format!("Ran: {}", truncate_str(cmd, 60))
                            }
                            "Edit" => {
                                let file = input
                                    .get("file_path")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("...");
                                let short = Path::new(file)
                                    .file_name()
                                    .and_then(|f| f.to_str())
                                    .unwrap_or(file);
                                format!("Edited {}", short)
                            }
                            "Write" => {
                                let file = input
                                    .get("file_path")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("...");
                                let short = Path::new(file)
                                    .file_name()
                                    .and_then(|f| f.to_str())
                                    .unwrap_or(file);
                                format!("Wrote {}", short)
                            }
                            "Read" => {
                                let file = input
                                    .get("file_path")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("...");
                                let short = Path::new(file)
                                    .file_name()
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
                let hook = data
                    .get("hookName")
                    .and_then(|v| v.as_str())
                    .unwrap_or("hook");
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
    let modified = match path.metadata().and_then(|m| m.modified()) {
        Ok(t) => t,
        Err(_) => return u64::MAX,
    };
    SystemTime::now()
        .duration_since(modified)
        .map(|d| d.as_secs())
        .unwrap_or(u64::MAX)
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let end = s
            .char_indices()
            .nth(max.saturating_sub(3))
            .map(|(i, _)| i)
            .unwrap_or(s.len());
        format!("{}...", &s[..end])
    }
}

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
        // the agent is processing a new request -> Working
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
        assert_eq!(
            ctx.last_assistant_message.as_deref(),
            Some("I'll fix it now")
        );
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
