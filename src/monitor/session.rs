//! JSONL session file tail reader and context extraction.
//!
//! Reads the last N lines of a Claude Code JSONL session file,
//! parses them to derive agent state and extract display context.

use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::Path;
use std::time::SystemTime;

use serde_json::Value;

use std::collections::HashMap;

use crate::monitor::state::{ActivityEntry, AgentState, SessionContext, SessionTelemetry};

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
        telemetry: None,
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

/// Derive telemetry from a full scan of the JSONL session file.
///
/// Extracts usage data from every assistant turn to compute context pressure,
/// burn rate, tool mix, and other metrics.
pub fn derive_telemetry(path: &Path) -> Option<SessionTelemetry> {
    let file = File::open(path).ok()?;
    let reader = BufReader::new(file);

    let mut context_tokens: u64 = 0;
    let mut total_output_tokens: u64 = 0;
    let mut turn_count: u32 = 0;
    let mut context_max: u64 = 200_000;
    let mut session_start: Option<String> = None;
    let mut has_queued_messages = false;
    let mut tool_counts: HashMap<String, u32> = HashMap::new();

    // For burn rate: track (timestamp_secs, context_tokens) of recent assistant turns
    let mut recent_snapshots: Vec<(f64, u64)> = Vec::new();

    for line in reader.lines() {
        let line = match line {
            Ok(l) if !l.is_empty() => l,
            _ => continue,
        };

        // Minimal parse — only look at fields we need
        let entry: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Track first timestamp for session_start
        if session_start.is_none() {
            if let Some(ts) = entry.get("timestamp").and_then(|v| v.as_str()) {
                session_start = Some(ts.to_string());
            }
        }

        let entry_type = entry.get("type").and_then(|v| v.as_str()).unwrap_or("");

        match entry_type {
            "assistant" => {
                let message = match entry.get("message") {
                    Some(m) => m,
                    None => continue,
                };

                // Extract usage data
                if let Some(usage) = message.get("usage") {
                    let input = usage
                        .get("input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let cache_creation = usage
                        .get("cache_creation_input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let cache_read = usage
                        .get("cache_read_input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let output = usage
                        .get("output_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);

                    context_tokens = input + cache_creation + cache_read;
                    total_output_tokens += output;
                    turn_count += 1;

                    // Track timestamp for burn rate
                    if let Some(ts) = entry.get("timestamp").and_then(|v| v.as_str()) {
                        if let Some(secs) = parse_iso_to_epoch_secs(ts) {
                            recent_snapshots.push((secs, context_tokens));
                        }
                    }
                }

                // Extract model for context_max
                if let Some(model) = message.get("model").and_then(|v| v.as_str()) {
                    context_max = model_context_max(model);
                }

                // Count tool_use items in content
                if let Some(Value::Array(parts)) = message.get("content") {
                    for part in parts {
                        if part.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                            if let Some(name) = part.get("name").and_then(|v| v.as_str()) {
                                *tool_counts.entry(name.to_string()).or_insert(0) += 1;
                            }
                        }
                    }
                }
            }
            "queue-operation" => {
                let op = entry.get("operation").and_then(|v| v.as_str());
                match op {
                    Some("enqueue") => has_queued_messages = true,
                    Some("dequeue") => has_queued_messages = false,
                    _ => {}
                }
            }
            _ => {}
        }
    }

    if turn_count == 0 {
        return None;
    }

    // Burn rate: last 5 snapshots
    let burn_rate_per_min = compute_burn_rate(&recent_snapshots);

    // Context pressure
    let context_pressure_pct = if context_max > 0 {
        ((context_tokens as f64 / context_max as f64) * 100.0).min(100.0) as u8
    } else {
        0
    };

    // Tool mix: sort by count descending, keep top entries
    let mut tool_mix: Vec<(String, u32)> = tool_counts.into_iter().collect();
    tool_mix.sort_by(|a, b| b.1.cmp(&a.1));

    Some(SessionTelemetry {
        context_tokens,
        context_pressure_pct,
        context_max,
        total_output_tokens,
        turn_count,
        burn_rate_per_min,
        tool_mix,
        session_start,
        has_queued_messages,
    })
}

/// Compute context burn rate from last 5 usage snapshots (tokens/minute).
fn compute_burn_rate(snapshots: &[(f64, u64)]) -> u64 {
    if snapshots.len() < 2 {
        return 0;
    }

    // Take last 5
    let start = snapshots.len().saturating_sub(5);
    let window = &snapshots[start..];

    let first = window.first().unwrap();
    let last = window.last().unwrap();

    let time_delta_mins = (last.0 - first.0) / 60.0;
    if time_delta_mins < 0.1 {
        return 0;
    }

    let token_delta = last.1.saturating_sub(first.1);
    (token_delta as f64 / time_delta_mins) as u64
}

/// Map model name to max context window tokens.
fn model_context_max(model: &str) -> u64 {
    // All current Claude models use 200K context
    if model.contains("opus")
        || model.contains("sonnet")
        || model.contains("haiku")
    {
        200_000
    } else {
        200_000 // safe default
    }
}

/// Parse an ISO8601 timestamp to seconds since epoch (approximate).
///
/// Handles format: "2026-03-11T22:14:08Z" or "2026-03-11T22:14:08.565Z"
fn parse_iso_to_epoch_secs(ts: &str) -> Option<f64> {
    // Quick parse: extract components from fixed positions
    // Format: YYYY-MM-DDTHH:MM:SS[.sss]Z
    if ts.len() < 19 {
        return None;
    }

    let hours: f64 = ts[11..13].parse().ok()?;
    let minutes: f64 = ts[14..16].parse().ok()?;
    let seconds: f64 = ts[17..19].parse().ok()?;

    // Day-of-year approximation for relative comparisons
    let year: f64 = ts[0..4].parse().ok()?;
    let month: f64 = ts[5..7].parse().ok()?;
    let day: f64 = ts[8..10].parse().ok()?;

    Some(
        year * 365.25 * 86400.0
            + month * 30.44 * 86400.0
            + day * 86400.0
            + hours * 3600.0
            + minutes * 60.0
            + seconds,
    )
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

    #[test]
    fn derive_telemetry_basic() {
        let f = make_jsonl(&[
            r#"{"type":"user","message":{"role":"user","content":"Hello"},"timestamp":"2026-03-11T22:00:00Z"}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Hi!"}],"stop_reason":"end_turn","model":"claude-opus-4-6","usage":{"input_tokens":100,"cache_creation_input_tokens":500,"cache_read_input_tokens":200,"output_tokens":50}},"timestamp":"2026-03-11T22:00:10Z"}"#,
            r#"{"type":"user","message":{"role":"user","content":"Do something"},"timestamp":"2026-03-11T22:01:00Z"}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}},{"type":"text","text":"Running ls"}],"stop_reason":"end_turn","model":"claude-opus-4-6","usage":{"input_tokens":200,"cache_creation_input_tokens":600,"cache_read_input_tokens":400,"output_tokens":80}},"timestamp":"2026-03-11T22:01:10Z"}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t2","name":"Read","input":{"file_path":"/tmp/foo"}},{"type":"tool_use","id":"t3","name":"Bash","input":{"command":"echo hi"}}],"stop_reason":"end_turn","model":"claude-opus-4-6","usage":{"input_tokens":300,"cache_creation_input_tokens":700,"cache_read_input_tokens":500,"output_tokens":120}},"timestamp":"2026-03-11T22:02:10Z"}"#,
        ]);

        let telem = derive_telemetry(f.path()).unwrap();

        // 3 assistant turns
        assert_eq!(telem.turn_count, 3);

        // Context tokens = last assistant's input + cache_creation + cache_read
        assert_eq!(telem.context_tokens, 300 + 700 + 500); // 1500

        // Total output = sum of all output_tokens
        assert_eq!(telem.total_output_tokens, 50 + 80 + 120); // 250

        // Context max for opus
        assert_eq!(telem.context_max, 200_000);

        // Pressure: 1500 / 200_000 * 100 ≈ 0.75 → 0% (rounds to 0 with u8)
        assert!(telem.context_pressure_pct < 2);

        // Session start
        assert_eq!(
            telem.session_start.as_deref(),
            Some("2026-03-11T22:00:00Z")
        );

        // Tool mix: Bash=2, Read=1
        assert_eq!(telem.tool_mix[0], ("Bash".to_string(), 2));
        assert_eq!(telem.tool_mix[1], ("Read".to_string(), 1));

        // Not queued
        assert!(!telem.has_queued_messages);
    }

    #[test]
    fn derive_telemetry_with_queue() {
        let f = make_jsonl(&[
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Hi"}],"stop_reason":"end_turn","model":"claude-sonnet-4-6","usage":{"input_tokens":50,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":10}},"timestamp":"2026-03-11T22:00:00Z"}"#,
            r#"{"type":"queue-operation","operation":"enqueue","timestamp":"2026-03-11T22:00:30Z"}"#,
        ]);

        let telem = derive_telemetry(f.path()).unwrap();
        assert!(telem.has_queued_messages);
        assert_eq!(telem.context_tokens, 50);
        assert_eq!(telem.context_max, 200_000);
    }

    #[test]
    fn derive_telemetry_empty_file() {
        let f = make_jsonl(&[]);
        assert!(derive_telemetry(f.path()).is_none());
    }

    #[test]
    fn derive_telemetry_no_assistant_turns() {
        let f = make_jsonl(&[
            r#"{"type":"user","message":{"role":"user","content":"Hello"},"timestamp":"2026-03-11T22:00:00Z"}"#,
        ]);
        assert!(derive_telemetry(f.path()).is_none());
    }

    #[test]
    fn burn_rate_calculation() {
        // 5 snapshots over 5 minutes, growing by 1000 tokens each
        let snapshots = vec![
            (100.0 * 60.0, 10_000),
            (101.0 * 60.0, 11_000),
            (102.0 * 60.0, 12_000),
            (103.0 * 60.0, 13_000),
            (104.0 * 60.0, 14_000),
        ];
        let rate = compute_burn_rate(&snapshots);
        assert_eq!(rate, 1000); // 4000 tokens / 4 minutes = 1000/min
    }

    #[test]
    fn parse_iso_timestamp() {
        let secs = parse_iso_to_epoch_secs("2026-03-11T22:14:08Z");
        assert!(secs.is_some());

        let secs2 = parse_iso_to_epoch_secs("2026-03-11T22:14:08.565Z");
        assert!(secs2.is_some());

        // Same second should be roughly equal
        let diff = (secs.unwrap() - secs2.unwrap()).abs();
        assert!(diff < 1.0);
    }
}
