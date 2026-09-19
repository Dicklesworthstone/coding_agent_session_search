mod exclusions;
mod source_budget;

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};

use anyhow::{Context, Result};
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
    fn supports_source_boundaries(&self) -> bool {
        self.inner.supports_source_boundaries()
    }

    fn scan_with_source_boundaries(
        &self,
        ctx: &ScanContext,
        hooks: &mut franken_agent_detection::connectors::SourceScanHooks<'_>,
        on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    ) -> Result<()> {
        source_budget::scan(&self.inner, ctx, hooks, on_conversation, |conversation| {
            augment_modern_codex_messages(conversation, ctx.progress_tick.as_deref())
        })
    }

    fn detect(&self) -> DetectionResult {
        self.inner.detect()
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        let mut conversations = Vec::new();
        self.scan_with_callback(ctx, &mut |conversation| {
            conversations.push(conversation);
            Ok(())
        })?;
        // This legacy all-or-error API must not label a partial vector complete.
        // Streaming/batch collectors retain delivered conversations separately.
        Ok(conversations)
    }

    fn supports_streaming_scan(&self) -> bool {
        self.inner.supports_streaming_scan()
    }

    fn discover_source_files(&self, ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        // GH #486: the published FAD pin does not filter Codex discovery yet.
        // Keep excluded sources out of pre-mirroring as well as parsing.
        let exclusions = exclusions::ScanExclusions::from_env();
        let mut sources = self.inner.discover_source_files(ctx)?;
        sources.retain(|source| !exclusions.excludes(&source.source_path));
        Ok(sources)
    }

    fn scan_with_callback(
        &self,
        ctx: &ScanContext,
        on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    ) -> Result<()> {
        self.scan_with_source_boundaries(
            ctx,
            &mut franken_agent_detection::connectors::SourceScanHooks::default(),
            on_conversation,
        )
    }
}

/// gh373/oeu5a: heartbeat stride for the rollout line loop below. One
/// owning [`ScanContext`] progress tick per this many lines keeps the stall
/// watchdog fed while this second pass re-parses a giant rollout (~40k lines,
/// minutes).
const AUGMENT_HEARTBEAT_LINE_STRIDE: usize = 1024;

/// Bound the enrichment pass independently of the upstream scan. In particular,
/// a growing source must not turn the second pass into an unbounded stream.
const MAX_AUGMENT_ROLLOUT_BYTES: u64 = 100 * 1024 * 1024;

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
) -> Result<()> {
    if conversation
        .source_path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_none_or(|ext| !ext.eq_ignore_ascii_case("jsonl"))
    {
        return Ok(());
    }

    let file = File::open(&conversation.source_path).with_context(|| {
        format!(
            "open Codex enrichment source {}",
            conversation.source_path.display()
        )
    })?;
    let before = file.metadata().context("inspect Codex enrichment source")?;
    if !before.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Codex enrichment source is not a regular file",
        )
        .into());
    }
    if before.len() > MAX_AUGMENT_ROLLOUT_BYTES {
        return Err(source_budget::EnrichmentBudgetExceeded {
            observed_bytes: before.len(),
        }
        .into());
    }

    // The first pass belongs to FAD; this pass owns only the opened prefix.
    // Never follow appends indefinitely. Consumers receive the conversation
    // only after enrichment and these observable-snapshot checks succeed.
    let mut reader = BufReader::new((&file).take(before.len()));
    let bytes_read = augment_modern_codex_reader(conversation, progress_tick, &mut reader)?;
    if bytes_read != before.len() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "Codex enrichment source was truncated while reading; retry this source",
        )
        .into());
    }
    let after = file
        .metadata()
        .context("recheck opened Codex enrichment source")?;
    let named = std::fs::metadata(&conversation.source_path)
        .context("recheck Codex enrichment source path")?;
    if !same_rollout_snapshot(&before, &after)? || !same_rollout_snapshot(&before, &named)? {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Codex enrichment source changed while reading; retry this source",
        )
        .into());
    }
    Ok(())
}

/// Detect observable rewrites/replacements, not malicious timestamp-preserving
/// edits. On Unix, inode/device checks also catch same-size path replacement.
fn same_rollout_snapshot(
    before: &std::fs::Metadata,
    after: &std::fs::Metadata,
) -> io::Result<bool> {
    if !after.is_file() || before.len() != after.len() || before.modified()? != after.modified()? {
        return Ok(false);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if before.dev() != after.dev() || before.ino() != after.ino() {
            return Ok(false);
        }
    }
    Ok(true)
}

/// An error may leave this private in-memory conversation partially enriched.
/// Each public scan route propagates the error before publishing it; do not
/// clone whole rollouts just to implement an in-memory rollback.
fn augment_modern_codex_reader(
    conversation: &mut NormalizedConversation,
    progress_tick: Option<&(dyn Fn() + Send + Sync)>,
    reader: &mut impl BufRead,
) -> Result<u64> {
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
    let mut line = String::new();
    let mut bytes_read = 0_u64;
    let mut line_no = 0_usize;
    loop {
        tick_augment_progress(progress_tick, line_no);
        line.clear();
        // Do not turn I/O errors or invalid UTF-8 into successful EOF.
        let count = reader
            .read_line(&mut line)
            .with_context(|| format!("read Codex enrichment line {}", line_no + 1))?;
        if count == 0 {
            break;
        }
        bytes_read += count as u64;
        line_no += 1;
        let terminated = line.ends_with('\n');
        let line = if line_no == 1 {
            line.trim_start_matches('\u{feff}').trim()
        } else {
            line.trim()
        };
        if line.is_empty() {
            continue;
        }
        let raw = match serde_json::from_str::<Value>(line) {
            Ok(value) => value,
            Err(parse_err) if !terminated && parse_err.is_eof() => {
                // An active writer's unfinished last record is not malformed
                // historical data. Refuse completion so it can be read again.
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!(
                        "incomplete Codex enrichment record at line {line_no}; retry this source"
                    ),
                )
                .into());
            }
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
                modern_codex_message_call_ids(&message)
                    .find_map(|call_id| message_indices_by_call_id.get(&call_id).copied())
            })
            .or_else(|| {
                message_indices_by_signature
                    .get(&message_signature)
                    .copied()
                    .filter(|index| {
                        compatible_tool_call_identity(&conversation.messages[*index], &message)
                    })
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
    Ok(bytes_read)
}

fn tick_augment_progress(progress_tick: Option<&(dyn Fn() + Send + Sync)>, line_no_zero: usize) {
    if line_no_zero.is_multiple_of(AUGMENT_HEARTBEAT_LINE_STRIDE)
        && let Some(tick) = progress_tick
    {
        tick();
    }
}

pub(crate) fn modern_codex_message(raw: &Value) -> Option<NormalizedMessage> {
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
        Some("function_call" | "custom_tool_call") => {
            let freeform = payload.get("type").and_then(Value::as_str) == Some("custom_tool_call");
            let tool_name = payload
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            // Custom tools (notably apply_patch) carry literal `input`, not
            // JSON-encoded `arguments`. Preserve even JSON-looking freeform
            // input as a string; the invocation and searchable body must not
            // silently change its type or whitespace.
            let arguments = if freeform {
                Some(Value::String(payload.get("input")?.as_str()?.to_string()))
            } else {
                payload.get("arguments").cloned()
            };
            let content = if freeform {
                let input = arguments.as_ref()?.as_str()?;
                if input.is_empty() {
                    format!("[Tool: {tool_name}]")
                } else {
                    format!("[Tool: {tool_name}]\n{input}")
                }
            } else {
                tool_call_content(tool_name, arguments.as_ref())
            };
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
                    arguments: if freeform {
                        arguments
                    } else {
                        arguments.and_then(normalize_invocation_arguments)
                    },
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

/// Identical tool text is not identical tool activity. Prefer native call IDs
/// over the text fallback, and never merge distinct native IDs just because
/// their timestamps, tool names, and arguments happen to match.
fn compatible_tool_call_identity(
    existing: &NormalizedMessage,
    candidate: &NormalizedMessage,
) -> bool {
    let existing_has_id = existing
        .invocations
        .iter()
        .any(|call| call.call_id.is_some());
    let candidate_has_id = candidate
        .invocations
        .iter()
        .any(|call| call.call_id.is_some());
    !existing_has_id
        || !candidate_has_id
        || candidate.invocations.iter().any(|candidate_call| {
            candidate_call.call_id.as_ref().is_some_and(|candidate_id| {
                existing
                    .invocations
                    .iter()
                    .any(|call| call.call_id.as_ref() == Some(candidate_id))
            })
        })
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
            let literal_custom_input = matched_call_id
                && candidate
                    .extra
                    .pointer("/payload/type")
                    .and_then(Value::as_str)
                    == Some("custom_tool_call")
                && candidate_invocation
                    .arguments
                    .as_ref()
                    .is_some_and(Value::is_string);
            if (existing_invocation.arguments.is_none() || literal_custom_input)
                && candidate_invocation.arguments.is_some()
                && existing_invocation.arguments != candidate_invocation.arguments
            {
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

    fn response(payload: Value) -> Value {
        serde_json::json!({
            "type": "response_item",
            "timestamp": "2026-06-28T10:00:05.000Z",
            "payload": payload
        })
    }

    fn custom_call(call_id: &str, input: &str) -> Value {
        response(serde_json::json!({
            "type": "custom_tool_call", "name": "apply_patch",
            "call_id": call_id, "input": input
        }))
    }

    fn conversation_at(path: &std::path::Path) -> NormalizedConversation {
        NormalizedConversation {
            agent_slug: "codex".to_string(),
            external_id: Some("freeform-test".to_string()),
            title: Some("freeform patch history".to_string()),
            workspace: None,
            source_path: path.to_path_buf(),
            started_at: None,
            ended_at: None,
            metadata: Value::Null,
            messages: Vec::new(),
        }
    }

    #[test]
    fn custom_tool_input_is_searchable_without_reinterpreting_freeform_bytes() {
        for input in [
            "*** Begin Patch\n*** Add File: 日本語.rs\n+scope_marker\n*** End Patch\n",
            " {\"cmd\":\"not a function argument\"} \n",
            "123",
            "",
        ] {
            let raw = custom_call("patch-1", input);
            let parsed = modern_codex_message(&raw).expect("custom tool message");
            assert_eq!(parsed.role, "assistant");
            assert_eq!(parsed.extra, raw);
            assert_eq!(parsed.invocations.len(), 1);
            assert_eq!(parsed.invocations[0].call_id.as_deref(), Some("patch-1"));
            assert_eq!(parsed.invocations[0].name, "apply_patch");
            assert_eq!(
                parsed.invocations[0].arguments,
                Some(Value::String(input.to_string()))
            );
            let expected = if input.is_empty() {
                "[Tool: apply_patch]".to_string()
            } else {
                format!("[Tool: apply_patch]\n{input}")
            };
            assert_eq!(parsed.content, expected);
        }
    }

    #[test]
    fn malformed_custom_input_is_not_invented_or_read_from_arguments() {
        for input in [
            Value::Null,
            serde_json::json!(123),
            serde_json::json!({"cmd": "x"}),
        ] {
            let raw = response(serde_json::json!({
                "type": "custom_tool_call", "name": "apply_patch",
                "call_id": "patch-1", "input": input, "arguments": "wrong field"
            }));
            assert!(modern_codex_message(&raw).is_none());
        }
        let function = response(serde_json::json!({
            "type": "function_call", "name": "exec_command", "call_id": "shell-1",
            "arguments": "{\"cmd\":\"git status\"}"
        }));
        let parsed = modern_codex_message(&function).expect("function call");
        assert_eq!(
            parsed.invocations[0].arguments,
            Some(serde_json::json!({"cmd": "git status"}))
        );
        assert_eq!(
            parsed.content,
            "[Tool: exec_command]\n{\"cmd\":\"git status\"}"
        );
    }

    #[test]
    fn freeform_enrichment_keeps_distinct_native_calls_and_is_replay_stable() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("rollout-freeform.jsonl");
        let input = "*** Begin Patch\n+same_input_distinct_calls\n*** End Patch\n";
        let first = custom_call("patch-1", input);
        let second = custom_call("patch-2", input);
        let output = response(serde_json::json!({
            "type": "function_call_output", "call_id": "patch-1", "output": "patch applied"
        }));
        std::fs::write(&path, format!("{first}\n{second}\n{output}\n"))?;
        let before = std::fs::read(&path)?;
        let mut conversation = conversation_at(&path);
        augment_modern_codex_messages(&mut conversation, None)?;
        assert_eq!(conversation.messages.len(), 3);
        for (index, expected) in ["patch-1", "patch-2"].into_iter().enumerate() {
            let call = &conversation.messages[index];
            assert_eq!(call.invocations.len(), 1);
            assert_eq!(call.invocations[0].call_id.as_deref(), Some(expected));
            assert!(call.content.contains("same_input_distinct_calls"));
        }
        assert_eq!(conversation.messages[2].role, "tool");
        let snapshot = serde_json::to_value(&conversation)?;
        augment_modern_codex_messages(&mut conversation, None)?;
        assert_eq!(serde_json::to_value(&conversation)?, snapshot);
        assert_eq!(std::fs::read(&path)?, before);
        Ok(())
    }

    #[test]
    fn freeform_enrichment_updates_fad_placeholders_in_place() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("rollout-placeholders.jsonl");
        let first = custom_call("patch-1", "same patch bytes");
        let second = custom_call("patch-2", "same patch bytes");
        std::fs::write(&path, format!("{first}\n{second}\n"))?;
        let mut conversation = conversation_at(&path);
        for (idx, raw) in [&first, &second].into_iter().enumerate() {
            let mut canonical = modern_codex_message(raw).expect("canonical call");
            canonical.idx = idx as i64;
            canonical.content = "[Tool: apply_patch]".to_string();
            // FAD deliberately compacts extra on large rollouts, so raw-record
            // matching cannot be the only way to find this canonical message.
            canonical.extra = serde_json::json!({"compacted": true, "slot": idx});
            conversation.messages.push(canonical);
        }
        let identity = conversation
            .messages
            .iter()
            .map(|message| {
                (
                    message.idx,
                    message.created_at,
                    message.extra.clone(),
                    message.invocations.clone(),
                )
            })
            .collect::<Vec<_>>();
        augment_modern_codex_messages(&mut conversation, None)?;
        assert_eq!(conversation.messages.len(), 2);
        for (message, expected) in conversation.messages.iter().zip(identity) {
            assert_eq!(message.content, "[Tool: apply_patch]\nsame patch bytes");
            assert_eq!(
                (
                    message.idx,
                    message.created_at,
                    message.extra.clone(),
                    message.invocations.clone()
                ),
                expected
            );
        }
        Ok(())
    }

    #[test]
    fn freeform_enrichment_repairs_json_looking_input_without_replacing_identity() {
        let raw = custom_call("patch-1", " 123 \n");
        let candidate = modern_codex_message(&raw).expect("literal custom input");
        let mut canonical = candidate.clone();
        canonical.idx = 12;
        canonical.content = "[Tool: apply_patch]".to_string();
        canonical.extra = serde_json::json!({"compacted": true});
        canonical.invocations[0].arguments = Some(serde_json::json!(123));
        assert!(merge_modern_codex_tool_call(&mut canonical, &candidate));
        assert_eq!(
            canonical.invocations[0].arguments,
            Some(Value::String(" 123 \n".to_string()))
        );
        assert_eq!(canonical.idx, 12);
        assert_eq!(canonical.extra, serde_json::json!({"compacted": true}));
        assert_eq!(canonical.created_at, candidate.created_at);
        assert!(!merge_modern_codex_tool_call(&mut canonical, &candidate));
    }

    #[test]
    fn enrichment_missing_source_fails_instead_of_certifying_incomplete_history() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut conversation = conversation_at(&dir.path().join("rollout-missing.jsonl"));
        let error = augment_modern_codex_messages(&mut conversation, None).unwrap_err();
        assert_eq!(
            error.downcast_ref::<io::Error>().map(io::Error::kind),
            Some(io::ErrorKind::NotFound)
        );
        assert!(conversation.messages.is_empty());
        Ok(())
    }

    #[test]
    fn enrichment_invalid_utf8_is_not_successful_eof() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut conversation = conversation_at(&dir.path().join("rollout-utf8.jsonl"));
        let mut reader = io::Cursor::new(b"\xff\n");
        let error = augment_modern_codex_reader(&mut conversation, None, &mut reader).unwrap_err();
        assert_eq!(
            error.downcast_ref::<io::Error>().map(io::Error::kind),
            Some(io::ErrorKind::InvalidData)
        );
        Ok(())
    }

    #[test]
    fn enrichment_propagates_read_failure_after_valid_records() -> Result<()> {
        struct FailingRead(io::Cursor<Vec<u8>>);
        impl Read for FailingRead {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                if self.0.position() == self.0.get_ref().len() as u64 {
                    Err(io::Error::other("injected source read failure"))
                } else {
                    self.0.read(bytes)
                }
            }
        }
        let dir = tempfile::tempdir()?;
        let raw = custom_call("patch-1", "read_failure_prefix");
        let mut conversation = conversation_at(&dir.path().join("rollout-fault.jsonl"));
        let mut reader = BufReader::new(FailingRead(io::Cursor::new(
            format!("{raw}\n").into_bytes(),
        )));
        let error = augment_modern_codex_reader(&mut conversation, None, &mut reader).unwrap_err();
        assert!(
            error
                .chain()
                .any(|cause| cause.to_string().contains("injected source read failure"))
        );
        assert_eq!(
            conversation.messages.len(),
            1,
            "test actually consumed a prefix before the fault"
        );
        Ok(())
    }

    #[test]
    fn enrichment_accepts_bom_malformed_historical_lines_and_complete_eof_record() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("rollout-bom.jsonl");
        let first = custom_call("patch-1", "first_patch");
        let second = custom_call("patch-2", "last_patch_without_newline");
        let bytes = format!("\u{feff}{first}\nnot-json\n\n{second}");
        std::fs::write(&path, &bytes)?;
        let mut conversation = conversation_at(&path);
        augment_modern_codex_messages(&mut conversation, None)?;
        assert_eq!(conversation.messages.len(), 2);
        assert!(conversation.messages[0].content.contains("first_patch"));
        assert!(
            conversation.messages[1]
                .content
                .contains("last_patch_without_newline")
        );
        assert_eq!(std::fs::read_to_string(&path)?, bytes);
        Ok(())
    }

    #[test]
    fn incomplete_last_record_fails_then_retry_reads_the_completed_append() -> Result<()> {
        use std::io::Write;
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("rollout-active.jsonl");
        let first = custom_call("patch-1", "complete_prefix");
        let second = custom_call("patch-2", "completed_tail").to_string();
        let split = second.len() - 1;
        std::fs::write(&path, format!("{first}\n{}", &second[..split]))?;
        let mut partial = conversation_at(&path);
        let error = augment_modern_codex_messages(&mut partial, None).unwrap_err();
        assert_eq!(
            error.downcast_ref::<io::Error>().map(io::Error::kind),
            Some(io::ErrorKind::UnexpectedEof)
        );
        let mut writer = std::fs::OpenOptions::new().append(true).open(&path)?;
        writer.write_all(&second.as_bytes()[split..])?;
        writer.flush()?;
        let before_retry = std::fs::read(&path)?;
        let mut retry = conversation_at(&path);
        augment_modern_codex_messages(&mut retry, None)?;
        assert_eq!(retry.messages.len(), 2);
        assert!(retry.messages[1].content.contains("completed_tail"));
        assert_eq!(std::fs::read(&path)?, before_retry);
        Ok(())
    }

    #[test]
    fn enrichment_detects_appends_without_chasing_the_growing_file() -> Result<()> {
        use std::io::Write;
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("rollout-growing.jsonl");
        let raw = custom_call("patch-1", "opened_prefix");
        std::fs::write(&path, format!("{raw}\n"))?;
        let ticks = AtomicUsize::new(0);
        let append = || {
            if ticks.fetch_add(1, Ordering::Relaxed) == 0 {
                let mut file = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .unwrap();
                writeln!(file, "{}", custom_call("patch-2", "later_append")).unwrap();
            }
        };
        let mut conversation = conversation_at(&path);
        let error = augment_modern_codex_messages(&mut conversation, Some(&append)).unwrap_err();
        assert!(error.to_string().contains("changed while reading"));
        assert_eq!(
            conversation.messages.len(),
            1,
            "read only the opened prefix"
        );
        let mut retry = conversation_at(&path);
        augment_modern_codex_messages(&mut retry, None)?;
        assert_eq!(retry.messages.len(), 2);
        Ok(())
    }

    #[test]
    fn oversized_enrichment_source_fails_before_parsing_or_allocating_its_body() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("rollout-oversized.jsonl");
        let file = File::create(&path)?;
        file.set_len(MAX_AUGMENT_ROLLOUT_BYTES + 1)?;
        let ticks = AtomicUsize::new(0);
        let tick = || {
            ticks.fetch_add(1, Ordering::Relaxed);
        };
        let mut conversation = conversation_at(&path);
        let error = augment_modern_codex_messages(&mut conversation, Some(&tick)).unwrap_err();
        assert!(error.to_string().contains("100 MiB read budget"));
        assert_eq!(ticks.load(Ordering::Relaxed), 0);
        assert!(conversation.messages.is_empty());
        assert_eq!(
            std::fs::metadata(&path)?.len(),
            MAX_AUGMENT_ROLLOUT_BYTES + 1
        );
        Ok(())
    }

    #[test]
    fn every_connector_route_withholds_an_unfinished_source_then_retries() -> Result<()> {
        use super::super::ScanRoot;
        use franken_agent_detection::connectors::{SourceCompletion, SourceScanHooks};
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("rollout-incomplete.jsonl");
        let first = custom_call("patch-1", "canonical_prefix");
        let second = custom_call("patch-2", "retried_tail").to_string();
        std::fs::write(&path, format!("{first}\n{}", &second[..second.len() - 1]))?;
        let ctx = ScanContext::with_roots(
            dir.path().join("cass-data"),
            vec![ScanRoot::local(path.clone())],
            None,
        );
        let connector = CodexConnector::new();
        assert!(
            connector.scan(&ctx).is_err(),
            "batch scan must not return a partial success"
        );
        let mut emitted = Vec::new();
        assert!(
            connector
                .scan_with_callback(&ctx, &mut |conversation| {
                    emitted.push(conversation);
                    Ok(())
                })
                .is_err()
        );
        assert!(emitted.is_empty());
        let mut completions = 0;
        {
            let mut complete = |_: &SourceCompletion| {
                completions += 1;
                Ok(())
            };
            let mut hooks = SourceScanHooks {
                should_scan_source: None,
                on_source_complete: Some(&mut complete),
            };
            assert!(
                connector
                    .scan_with_source_boundaries(&ctx, &mut hooks, &mut |conversation| {
                        emitted.push(conversation);
                        Ok(())
                    })
                    .is_err()
            );
        }
        assert!(emitted.is_empty());
        assert_eq!(
            completions, 0,
            "failed enrichment must not certify source completion"
        );
        std::fs::write(&path, format!("{first}\n{second}\n"))?;
        let before = std::fs::read(&path)?;
        {
            let mut complete = |_: &SourceCompletion| {
                completions += 1;
                Ok(())
            };
            let mut hooks = SourceScanHooks {
                should_scan_source: None,
                on_source_complete: Some(&mut complete),
            };
            connector.scan_with_source_boundaries(&ctx, &mut hooks, &mut |conversation| {
                emitted.push(conversation);
                Ok(())
            })?;
        }
        assert_eq!(emitted.len(), 1);
        assert_eq!(completions, 1);
        assert_eq!(emitted[0].messages.len(), 2);
        assert!(emitted[0].messages[1].content.contains("retried_tail"));
        assert_eq!(std::fs::read(&path)?, before);
        Ok(())
    }
}
