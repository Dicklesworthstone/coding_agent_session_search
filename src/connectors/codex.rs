use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};

use anyhow::Result;
use serde_json::Value;
use tracing::warn;

use super::{
    Connector, DetectionResult, DiscoveredSourceFile, NormalizedConversation, NormalizedMessage,
    ScanContext, parse_timestamp, reindex_messages,
};

const MAX_INDEXED_TOOL_OUTPUT_CHARS: usize = 128 * 1024;

pub struct CodexConnector {
    inner: franken_agent_detection::CodexConnector,
}

impl Default for CodexConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl CodexConnector {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: franken_agent_detection::CodexConnector::new(),
        }
    }
}

impl Connector for CodexConnector {
    fn detect(&self) -> DetectionResult {
        self.inner.detect()
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        let mut conversations = self.inner.scan(ctx)?;
        for conversation in &mut conversations {
            augment_modern_codex_messages(conversation, ctx.progress_tick.as_deref());
        }
        Ok(conversations)
    }

    fn supports_streaming_scan(&self) -> bool {
        self.inner.supports_streaming_scan()
    }

    fn discover_source_files(&self, ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        self.inner.discover_source_files(ctx)
    }

    fn scan_with_callback(
        &self,
        ctx: &ScanContext,
        on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    ) -> Result<()> {
        self.inner.scan_with_callback(ctx, &mut |mut conversation| {
            augment_modern_codex_messages(&mut conversation, ctx.progress_tick.as_deref());
            on_conversation(conversation)
        })
    }
}

/// gh373/oeu5a: heartbeat stride for the rollout line loop below. One
/// owning [`ScanContext`] progress tick per this many lines keeps the stall
/// watchdog fed while this second pass re-parses a giant rollout (~40k lines,
/// minutes).
const AUGMENT_HEARTBEAT_LINE_STRIDE: usize = 1024;

// TODO(gh373/oeu5a follow-up): this function is a full second parse of every
// rollout — `franken_agent_detection`'s scan already read and parsed the same
// file to produce `conversation`. Merging this enrichment into FAD's primary
// parse (single-pass) would roughly halve producer CPU/IO on the codex
// corpus, but the parse lives in the pinned external FAD crate
// (Cargo.toml rev pin), so the merge must land there first with cass-side
// plumbing behind a feature/rev bump. Deferred deliberately; do not attempt
// by duplicating FAD parse internals here.
fn augment_modern_codex_messages(
    conversation: &mut NormalizedConversation,
    progress_tick: Option<&(dyn Fn() + Send + Sync)>,
) {
    if conversation
        .source_path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_none_or(|ext| !ext.eq_ignore_ascii_case("jsonl"))
    {
        return;
    }

    let Ok(file) = File::open(&conversation.source_path) else {
        return;
    };

    let mut message_indices_by_signature: HashMap<ModernCodexMessageSignature, usize> =
        conversation
            .messages
            .iter()
            .enumerate()
            .map(|(index, message)| (modern_codex_message_signature(message), index))
            .collect();
    let mut message_indices_by_call_id: HashMap<String, usize> = conversation
        .messages
        .iter()
        .enumerate()
        .flat_map(|(index, message)| {
            modern_codex_message_call_ids(message).map(move |call_id| (call_id, index))
        })
        .collect();
    let mut message_indices_by_raw_entry: HashMap<[u8; 32], usize> = conversation
        .messages
        .iter()
        .enumerate()
        .map(|(index, message)| (modern_codex_raw_signature(&message.extra), index))
        .collect();
    let mut added = false;
    for (line_no_zero, line) in BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .enumerate()
    {
        let line_no = line_no_zero + 1;
        // gh373/oeu5a: connector-internal liveness during the minutes-long
        // re-parse of one giant rollout (see AUGMENT_HEARTBEAT_LINE_STRIDE).
        tick_augment_progress(progress_tick, line_no_zero);
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let raw = match serde_json::from_str::<Value>(line) {
            Ok(value) => value,
            Err(parse_err) => {
                // Per gauntlet finding CONF-cass-003: surface malformed JSONL lines
                // to tracing so operators can correlate `cass diag` reports against
                // unreadable Codex rollout entries. The line is still dropped to
                // preserve resilience; the warning is purely diagnostic.
                warn!(
                    source_path = %conversation.source_path.display(),
                    line_no = line_no,
                    error = %parse_err,
                    "codex rollout JSONL line failed to parse; skipping",
                );
                continue;
            }
        };
        let raw_signature = modern_codex_raw_signature(&raw);
        let Some(message) = modern_codex_message(&raw) else {
            continue;
        };
        let message_signature = modern_codex_message_signature(&message);
        let existing_index = message_indices_by_raw_entry
            .get(&raw_signature)
            .copied()
            .or_else(|| {
                message_indices_by_signature
                    .get(&message_signature)
                    .copied()
            })
            .or_else(|| {
                modern_codex_message_call_ids(&message)
                    .find_map(|call_id| message_indices_by_call_id.get(&call_id).copied())
            });

        if let Some(existing_index) = existing_index {
            let existing = &mut conversation.messages[existing_index];
            if merge_modern_codex_tool_call(existing, &message) {
                message_indices_by_signature
                    .insert(modern_codex_message_signature(existing), existing_index);
                message_indices_by_call_id.extend(
                    modern_codex_message_call_ids(existing)
                        .map(|call_id| (call_id, existing_index)),
                );
            }
            message_indices_by_raw_entry.insert(raw_signature, existing_index);
            continue;
        }

        let message_index = conversation.messages.len();
        conversation.messages.push(message);
        let stored = &conversation.messages[message_index];
        message_indices_by_signature.insert(message_signature, message_index);
        message_indices_by_call_id
            .extend(modern_codex_message_call_ids(stored).map(|call_id| (call_id, message_index)));
        message_indices_by_raw_entry.insert(raw_signature, message_index);
        added = true;
    }

    if added {
        conversation.messages.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.idx.cmp(&right.idx))
        });
        reindex_messages(&mut conversation.messages);
    }
}

fn tick_augment_progress(
    progress_tick: Option<&(dyn Fn() + Send + Sync)>,
    line_no_zero: usize,
) {
    if line_no_zero % AUGMENT_HEARTBEAT_LINE_STRIDE == 0
        && let Some(tick) = progress_tick
    {
        tick();
    }
}

fn modern_codex_message(raw: &Value) -> Option<NormalizedMessage> {
    let entry_type = raw.get("type").and_then(Value::as_str)?;
    let payload = raw.get("payload")?;
    let created_at = raw.get("timestamp").and_then(parse_timestamp);

    match entry_type {
        "response_item" => response_item_message(payload, created_at, raw),
        "event_msg" => event_message(payload, created_at, raw),
        _ => None,
    }
}

fn response_item_message(
    payload: &Value,
    created_at: Option<i64>,
    raw: &Value,
) -> Option<NormalizedMessage> {
    match payload.get("type").and_then(Value::as_str) {
        Some("message") | None => {
            let content = payload.get("content").and_then(flatten_modern_content)?;
            let role = payload
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("agent")
                .to_string();
            Some(normalized_message(
                role,
                None,
                created_at,
                content,
                raw.clone(),
                payload.get("content").map_or_else(
                    Vec::new,
                    franken_agent_detection::extract_invocations_from_content_blocks,
                ),
            ))
        }
        Some("function_call") => {
            let tool_name = payload
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let arguments = payload.get("arguments").cloned();
            let content = tool_call_content(tool_name, arguments.as_ref());
            let call_id = payload
                .get("call_id")
                .or_else(|| payload.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string);
            Some(normalized_message(
                "assistant".to_string(),
                None,
                created_at,
                content,
                raw.clone(),
                vec![franken_agent_detection::NormalizedInvocation {
                    kind: "tool".to_string(),
                    name: tool_name.to_string(),
                    raw_name: None,
                    call_id,
                    arguments: arguments.and_then(normalize_invocation_arguments),
                }],
            ))
        }
        Some("function_call_output") => {
            let output = payload.get("output").and_then(Value::as_str)?;
            let call_id = payload.get("call_id").and_then(Value::as_str);
            Some(normalized_message(
                "tool".to_string(),
                None,
                created_at,
                tool_output_content(call_id, output),
                raw.clone(),
                Vec::new(),
            ))
        }
        _ => None,
    }
}

fn event_message(
    payload: &Value,
    created_at: Option<i64>,
    raw: &Value,
) -> Option<NormalizedMessage> {
    match payload.get("type").and_then(Value::as_str) {
        Some("agent_message") => {
            let content = payload
                .get("message")
                .or_else(|| payload.get("text"))
                .and_then(Value::as_str)?
                .trim()
                .to_string();
            non_empty_message("assistant".to_string(), None, created_at, content, raw)
        }
        Some("tool_result") => {
            let output = payload
                .get("output")
                .or_else(|| payload.get("result"))
                .and_then(Value::as_str)?;
            let call_id = payload
                .get("call_id")
                .or_else(|| payload.get("id"))
                .and_then(Value::as_str);
            Some(normalized_message(
                "tool".to_string(),
                None,
                created_at,
                tool_output_content(call_id, output),
                raw.clone(),
                Vec::new(),
            ))
        }
        _ => None,
    }
}

fn normalized_message(
    role: String,
    author: Option<String>,
    created_at: Option<i64>,
    content: String,
    extra: Value,
    invocations: Vec<franken_agent_detection::NormalizedInvocation>,
) -> NormalizedMessage {
    NormalizedMessage {
        idx: 0,
        role,
        author,
        created_at,
        content,
        extra,
        invocations,
        snippets: Vec::new(),
    }
}

fn non_empty_message(
    role: String,
    author: Option<String>,
    created_at: Option<i64>,
    content: String,
    raw: &Value,
) -> Option<NormalizedMessage> {
    (!content.trim().is_empty())
        .then(|| normalized_message(role, author, created_at, content, raw.clone(), Vec::new()))
}

fn flatten_modern_content(content: &Value) -> Option<String> {
    if let Some(text) = content
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        return Some(text.to_string());
    }

    let mut parts = Vec::new();
    for item in content.as_array()? {
        let text = modern_content_part_text(item);

        let text = text.trim();
        if !text.is_empty() {
            parts.push(text.to_string());
        }
    }

    (!parts.is_empty()).then(|| parts.join("\n"))
}

fn modern_content_part_text(item: &Value) -> String {
    if let Some(text) = item.as_str() {
        return text.to_string();
    }

    let item_type = item.get("type").and_then(Value::as_str);
    if matches!(
        item_type,
        None | Some("text") | Some("input_text") | Some("output_text")
    ) {
        return item
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
    }

    if item_type == Some("tool_use") {
        let tool_name = item
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let detail = item
            .get("input")
            .and_then(|input| {
                input
                    .get("description")
                    .or_else(|| input.get("file_path"))
                    .or_else(|| input.get("path"))
                    .or_else(|| input.get("command"))
            })
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        return if detail.is_empty() {
            format!("[Tool: {tool_name}]")
        } else {
            format!("[Tool: {tool_name} - {detail}]")
        };
    }

    String::new()
}

fn tool_call_content(tool_name: &str, arguments: Option<&Value>) -> String {
    let mut content = format!("[Tool: {tool_name}]");
    if let Some(arguments) = arguments.and_then(argument_text) {
        content.push('\n');
        content.push_str(&arguments);
    }
    content
}

fn tool_output_content(call_id: Option<&str>, output: &str) -> String {
    let label = call_id.map_or_else(
        || "[Tool output]".to_string(),
        |id| format!("[Tool output: {id}]"),
    );
    let output = truncate_tool_output(output.trim());
    if output.is_empty() {
        label
    } else {
        format!("{label}\n{output}")
    }
}

fn argument_text(arguments: &Value) -> Option<String> {
    let text = match arguments {
        Value::String(text) => text.trim().to_string(),
        other => serde_json::to_string(other).ok()?,
    };
    (!text.is_empty()).then_some(text)
}

fn normalize_invocation_arguments(arguments: Value) -> Option<Value> {
    match arguments {
        Value::String(text) => serde_json::from_str(&text)
            .ok()
            .or_else(|| (!text.trim().is_empty()).then_some(Value::String(text))),
        Value::Null => None,
        other => Some(other),
    }
}

fn truncate_tool_output(output: &str) -> String {
    let mut truncated = String::new();
    let mut chars = output.chars();
    for _ in 0..MAX_INDEXED_TOOL_OUTPUT_CHARS {
        let Some(ch) = chars.next() else {
            return output.to_string();
        };
        truncated.push(ch);
    }
    let omitted = chars.count();
    truncated.push_str(&format!(
        "\n[truncated {omitted} additional chars from tool output]"
    ));
    truncated
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ModernCodexMessageSignature {
    role: String,
    author: Option<String>,
    created_at: Option<i64>,
    content_hash: [u8; 32],
}

fn modern_codex_message_signature(message: &NormalizedMessage) -> ModernCodexMessageSignature {
    ModernCodexMessageSignature {
        role: message.role.clone(),
        author: message.author.clone(),
        created_at: message.created_at,
        content_hash: *blake3::hash(message.content.as_bytes()).as_bytes(),
    }
}

fn modern_codex_raw_signature(raw: &Value) -> [u8; 32] {
    let mut bytes = Vec::new();
    if serde_json::to_writer(&mut bytes, raw).is_err() {
        bytes.extend_from_slice(raw.to_string().as_bytes());
    }
    *blake3::hash(&bytes).as_bytes()
}

fn modern_codex_message_call_ids(message: &NormalizedMessage) -> impl Iterator<Item = String> + '_ {
    message
        .invocations
        .iter()
        .filter_map(|invocation| invocation.call_id.clone())
}

fn merge_modern_codex_tool_call(
    existing: &mut NormalizedMessage,
    candidate: &NormalizedMessage,
) -> bool {
    let mut changed = false;
    let mut matched_invocation = false;
    let mut upgraded_unknown_call_id = None;

    for candidate_invocation in &candidate.invocations {
        let existing_invocation = existing.invocations.iter_mut().find(|invocation| {
            match (
                invocation.call_id.as_deref(),
                candidate_invocation.call_id.as_deref(),
            ) {
                (Some(existing_id), Some(candidate_id)) => existing_id == candidate_id,
                (None, None) => {
                    invocation.kind == candidate_invocation.kind
                        && invocation.name == candidate_invocation.name
                }
                _ => false,
            }
        });

        if let Some(existing_invocation) = existing_invocation {
            matched_invocation = true;
            let matched_call_id = existing_invocation.call_id.is_some()
                && existing_invocation.call_id == candidate_invocation.call_id;
            if existing_invocation.arguments.is_none() && candidate_invocation.arguments.is_some() {
                existing_invocation
                    .arguments
                    .clone_from(&candidate_invocation.arguments);
                changed = true;
            }
            if existing_invocation.raw_name.is_none() && candidate_invocation.raw_name.is_some() {
                existing_invocation
                    .raw_name
                    .clone_from(&candidate_invocation.raw_name);
                changed = true;
            }
            if existing_invocation.name == "unknown" && candidate_invocation.name != "unknown" {
                if matched_call_id {
                    upgraded_unknown_call_id.clone_from(&existing_invocation.call_id);
                }
                existing_invocation
                    .name
                    .clone_from(&candidate_invocation.name);
                changed = true;
            }
        } else {
            existing.invocations.push(candidate_invocation.clone());
            matched_invocation = true;
            changed = true;
        }
    }

    let resolves_unknown_placeholder = upgraded_unknown_call_id.as_deref().is_some_and(|call_id| {
        existing.content == "[Tool: unknown]"
            && candidate.invocations.iter().any(|invocation| {
                invocation.call_id.as_deref() == Some(call_id)
                    && invocation.name != "unknown"
                    && tool_call_content_has_name(&candidate.content, &invocation.name)
            })
    });
    if matched_invocation
        && candidate.content.len() > existing.content.len()
        && (candidate.content.starts_with(&existing.content) || resolves_unknown_placeholder)
    {
        existing.content.clone_from(&candidate.content);
        changed = true;
    }

    changed
}

fn tool_call_content_has_name(content: &str, tool_name: &str) -> bool {
    let prefix = format!("[Tool: {tool_name}]");
    content == prefix
        || content
            .strip_prefix(&prefix)
            .is_some_and(|rest| rest.starts_with('\n'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn message(content: &str, call_id: Option<&str>) -> NormalizedMessage {
        NormalizedMessage {
            idx: 0,
            role: "assistant".to_string(),
            author: None,
            created_at: Some(1_700_000_000_000),
            content: content.to_string(),
            extra: Value::Null,
            invocations: call_id
                .map(|call_id| {
                    vec![franken_agent_detection::NormalizedInvocation {
                        kind: "tool".to_string(),
                        name: "shell".to_string(),
                        raw_name: None,
                        call_id: Some(call_id.to_string()),
                        arguments: None,
                    }]
                })
                .unwrap_or_default(),
            snippets: Vec::new(),
        }
    }

    #[test]
    fn modern_codex_tool_call_merge_enriches_content_without_duplication() {
        let mut existing = message("[Tool: shell]", Some("call-1"));
        existing.idx = 7;
        existing.author = Some("codex".to_string());
        existing.invocations[0].arguments = Some(serde_json::json!({"cmd": "git status"}));
        existing.extra = serde_json::json!({"canonical": true});
        let mut candidate = message("[Tool: shell]\n{\"cmd\":\"git status\"}", Some("call-1"));
        candidate.invocations[0].arguments = Some(serde_json::json!({"cmd": "git status"}));
        let stable_identity = (
            existing.idx,
            existing.role.clone(),
            existing.author.clone(),
            existing.created_at,
            existing.extra.clone(),
        );

        assert!(merge_modern_codex_tool_call(&mut existing, &candidate));
        assert_eq!(existing.content, "[Tool: shell]\n{\"cmd\":\"git status\"}");
        assert_eq!(
            (
                existing.idx,
                existing.role.clone(),
                existing.author.clone(),
                existing.created_at,
                existing.extra.clone(),
            ),
            stable_identity,
            "enrichment must not replace the canonical message identity"
        );
        assert_eq!(existing.invocations.len(), 1);
        assert!(!merge_modern_codex_tool_call(&mut existing, &candidate));

        let mut missing_invocation = message("[Tool: shell]", None);
        assert!(merge_modern_codex_tool_call(
            &mut missing_invocation,
            &candidate
        ));
        assert_eq!(missing_invocation.invocations, candidate.invocations);
        assert_eq!(missing_invocation.content, candidate.content);
    }

    #[test]
    fn modern_codex_tool_call_merge_resolves_same_call_unknown_placeholder() {
        let mut existing = message("[Tool: unknown]", Some("call-1"));
        existing.invocations[0].name = "unknown".to_string();
        let mut candidate = message(
            "[Tool: exec_command]\n{\"cmd\":\"git status\"}",
            Some("call-1"),
        );
        candidate.invocations[0].name = "exec_command".to_string();
        candidate.invocations[0].arguments = Some(serde_json::json!({"cmd": "git status"}));

        assert!(merge_modern_codex_tool_call(&mut existing, &candidate));
        assert_eq!(existing.invocations.len(), 1);
        assert_eq!(existing.invocations[0].name, "exec_command");
        assert_eq!(
            existing.invocations[0].arguments,
            candidate.invocations[0].arguments
        );
        assert_eq!(existing.content, candidate.content);
        assert!(!merge_modern_codex_tool_call(&mut existing, &candidate));
    }

    #[test]
    fn modern_codex_tool_call_merge_rejects_unrelated_content_replacement() {
        let mut existing = message("canonical response", Some("call-1"));
        let candidate = message("unrelated replacement", Some("call-1"));

        assert!(!merge_modern_codex_tool_call(&mut existing, &candidate));
        assert_eq!(existing.content, "canonical response");
        assert_eq!(existing.invocations.len(), 1);
    }

    #[test]
    fn augment_progress_ticks_only_the_owning_scan_context_at_stride() {
        let owner_ticks = AtomicUsize::new(0);
        let unrelated_ticks = AtomicUsize::new(0);
        let owner = || {
            owner_ticks.fetch_add(1, Ordering::Relaxed);
        };
        let unrelated = || {
            unrelated_ticks.fetch_add(1, Ordering::Relaxed);
        };

        tick_augment_progress(Some(&owner), 0);
        tick_augment_progress(Some(&owner), AUGMENT_HEARTBEAT_LINE_STRIDE - 1);
        tick_augment_progress(Some(&owner), AUGMENT_HEARTBEAT_LINE_STRIDE);
        tick_augment_progress(None, AUGMENT_HEARTBEAT_LINE_STRIDE * 2);

        assert_eq!(owner_ticks.load(Ordering::Relaxed), 2);
        assert_eq!(unrelated_ticks.load(Ordering::Relaxed), 0);
        let _ = unrelated;
    }
}
