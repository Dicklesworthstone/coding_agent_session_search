//! Export functionality for search results.
//!
//! Provides conversion of search results to various output formats:
//! - Markdown - formatted with headers, code blocks, and metadata
//! - JSON - structured data for programmatic use
//! - Plain Text - simple, copy-paste friendly format

use chrono::{DateTime, Utc};
use serde_json::{Map, Value};
use std::borrow::{Borrow, Cow};

use crate::model::types::Message;
use crate::search::query::SearchHit;

/// Sharing policy applied before raw or archived conversation data is rendered.
/// Export policy is independent of index-time settings: an archive built with
/// redaction disabled must still be safe to share by default.
pub(crate) struct ExportPrivacy {
    include_secrets: bool,
    redaction_applied: bool,
}

impl ExportPrivacy {
    pub(crate) fn new(include_secrets: bool) -> Self {
        Self {
            include_secrets,
            redaction_applied: false,
        }
    }

    pub(crate) fn redact_messages(&mut self, messages: &mut [Value]) {
        for message in messages {
            self.redact_message(message);
        }
    }

    pub(crate) fn redact_message(&mut self, message: &mut Value) {
        if !self.include_secrets {
            let redacted = crate::indexer::redact_secrets::redact_json(message);
            self.redaction_applied |= redacted != *message;
            *message = redacted;
        }
    }

    pub(crate) fn redact_optional_text(&mut self, text: &mut Option<String>) {
        if let Some(text) = text {
            self.redact_text(text);
        }
    }

    pub(crate) fn redact_text(&mut self, text: &mut String) {
        if !self.include_secrets
            && let Value::String(redacted) =
                crate::indexer::redact_secrets::redact_json(&Value::String(text.clone()))
            && redacted != *text
        {
            // Titles and text metadata can contain JSON-encoded credentials,
            // just like message strings. Apply the same field-aware policy.
            *text = redacted;
            self.redaction_applied = true;
        }
    }

    pub(crate) fn metadata(&self) -> Value {
        serde_json::json!({
            "policy": if self.include_secrets { "include-secrets" } else { "redact-secrets" },
            "redaction_applied": self.redaction_applied,
            "sensitive_output": self.include_secrets,
        })
    }

    pub(crate) fn announce(&self) {
        eprintln!("note: export privacy: {}", self.metadata());
    }
}

const SKILL_DIRECTORY_MARKER: &str = "Base directory for this skill:";
pub(crate) const SAFE_HTML_EXPORT_FILENAME: &str = "conversation-export.html";
pub(crate) const SAFE_MARKDOWN_EXPORT_FILENAME: &str = "conversation-export.md";

#[derive(Clone, Copy, Debug, Default, clap::Args)]
pub struct ExportInjections {
    /// Include the full content of injected skills.
    #[arg(long)]
    pub include_skills: bool,
    /// Include injected system reminders and hook metadata.
    #[arg(long)]
    pub include_system_reminders: bool,
}

impl ExportInjections {
    #[cfg(test)]
    pub(crate) const INCLUDE_ALL: Self = Self {
        include_skills: true,
        include_system_reminders: true,
    };

    fn excludes(self, content: &str) -> bool {
        (!self.include_skills && content.contains(SKILL_DIRECTORY_MARKER))
            || (!self.include_system_reminders
                && (content.contains("<system-reminder>")
                    || content.contains(
                        "The following skills are available for use with the Skill tool:",
                    )
                    || has_hook_match_metadata(content)
                    || content.contains("<!-- skillInjection:")))
    }

    fn excludes_raw(self, message: &Value) -> bool {
        (!self.include_skills || !self.include_system_reminders)
            && self.excludes(&message.to_string())
    }

    fn excludes_archived(self, message: &Message) -> bool {
        self.excludes(&message.content)
            || (!message.extra_json.is_null() && self.excludes_raw(&message.extra_json))
    }

    fn excludes_record(self, raw: &Value, archived: Option<&Message>) -> bool {
        self.excludes_raw(raw) || archived.is_some_and(|message| self.excludes_archived(message))
    }

    pub(crate) fn excludes_title(self, title: &str, raw: &[Value], archived: &[Message]) -> bool {
        if self.include_skills && self.include_system_reminders {
            return false;
        }
        self.excludes(title)
            || derived_title_prefix(title).is_some_and(|prefix| {
                archived.iter().any(|message| {
                    message.content.trim_start().starts_with(prefix)
                        && self.excludes_archived(message)
                }) || raw.iter().any(|message| {
                    crate::extract_text_content(message)
                        .trim_start()
                        .starts_with(prefix)
                        && self.excludes_raw(message)
                })
            })
    }
}

// Handle the textual marker, JSON object keys, and JSON archived as a string.
fn has_hook_match_metadata(content: &str) -> bool {
    content.contains("matchedSkills")
        && content.split("skillInjection").skip(1).any(|tail| {
            tail.trim_start_matches(['\\', '"'])
                .trim_start()
                .starts_with(':')
        })
}

/// Whole-message skill and hook injections omitted from handoffs and previews.
pub(crate) fn is_skill_injection(content: &str) -> bool {
    ExportInjections::default().excludes(content)
}

/// A normalized archive row can retain a marker only in its source envelope.
pub(crate) fn archived_message_is_skill_injection(message: &Message) -> bool {
    ExportInjections::default().excludes_archived(message)
}

/// Generated titles can preserve the start of a skill message after its
/// marker was truncated or was kept only in the archived envelope.
fn derived_title_prefix(title: &str) -> Option<&str> {
    let title = title.trim();
    let prefix = title
        .strip_suffix('…')
        .or_else(|| title.strip_suffix("..."))
        .unwrap_or(title)
        .trim_end();
    (!prefix.is_empty()).then_some(prefix)
}

pub(crate) fn title_is_skill_injection(title: &str, archived: &[Message]) -> bool {
    ExportInjections::default().excludes_title(title, &[], archived)
}

pub(crate) fn raw_title_is_skill_injection(title: &str, messages: &[Value]) -> bool {
    ExportInjections::default().excludes_title(title, messages, &[])
}

/// Pi-family fallback titles can come from the first indexed message's
/// searchable text, even when the corresponding tool is hidden in an export.
pub(crate) fn archived_title_has_hidden_pi_tools(
    title: &str,
    view: &crate::ui::data::ConversationView,
    projected: impl IntoIterator<Item: Borrow<Value>>,
) -> bool {
    if !matches!(view.convo.agent_slug.as_str(), "pi_agent" | "omp") {
        return false;
    }
    for (archived, message) in view.messages.iter().zip(projected) {
        let message = message.borrow();
        if title_matches_hidden_pi_tools(
            title,
            &crate::extract_role(message),
            &archived.content,
            message,
        ) {
            return true;
        }
        // The modal skips empty and injected messages when deriving a title.
        if !archived.content.trim().is_empty() && !archived_message_is_skill_injection(archived) {
            break;
        }
    }
    false
}

pub(crate) fn raw_title_has_hidden_pi_tools(title: &str, agent: &str, messages: &[Value]) -> bool {
    if !matches!(agent, "pi_agent" | "omp") {
        return false;
    }
    for message in messages {
        if message.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        let Some(content) = message.pointer("/message/content") else {
            continue;
        };
        let searchable =
            franken_agent_detection::connectors::pi_wire::flatten_message_content(content);
        if title_matches_hidden_pi_tools(title, &crate::extract_role(message), &searchable, message)
        {
            return true;
        }
        if !searchable.trim().is_empty() && !ExportInjections::default().excludes_raw(message) {
            break;
        }
    }
    false
}

fn title_matches_hidden_pi_tools(
    title: &str,
    role: &str,
    searchable: &str,
    rendered: &Value,
) -> bool {
    let Some(prefix) = derived_title_prefix(title) else {
        return false;
    };
    let searchable = searchable.trim_start();
    if role != "tool" && (role != "assistant" || !searchable.contains("[Tool:")) {
        return false;
    }

    // TUI writers receive a title that has already passed credential redaction.
    // Its fallback title can span lines. Preserve raw matching for CLI titles
    // and compare the visible text using the same form as the matched source.
    let compare_redacted = !searchable.starts_with(prefix);
    let mut privacy = ExportPrivacy::new(false);
    let mut source = Cow::Borrowed(searchable);
    if compare_redacted {
        privacy.redact_text(source.to_mut());
    }
    let source = source.trim_start();
    if !source.starts_with(prefix) {
        return false;
    }
    if role == "tool" {
        return true;
    }

    // A flattened tool block can follow ordinary text. Inspect the matched
    // source lines so a preview that cuts through the marker still overlaps it.
    let mut line_start = 0;
    let mut overlaps_tool_line = false;
    for line in source.split_inclusive('\n') {
        if line_start >= prefix.len() {
            break;
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with("[Tool:") && line_start + (line.len() - trimmed.len()) < prefix.len()
        {
            overlaps_tool_line = true;
            break;
        }
        line_start += line.len();
    }
    if !overlaps_tool_line {
        return false;
    }

    let mut visible = crate::extract_text_content_without_tool_blocks(rendered);
    if compare_redacted {
        privacy.redact_text(&mut visible);
    }
    !std::iter::once(0)
        .chain(visible.match_indices('\n').map(|(index, _)| index + 1))
        .any(|start| {
            visible[start..]
                .trim_start_matches(|ch: char| ch != '\n' && ch.is_whitespace())
                .starts_with(prefix)
        })
}

// Only use a path basename with the syntax of a skill identifier. Never copy
// prose, the absolute path, or source record metadata into the replacement.
fn skill_placeholder(message: &Value) -> Option<Value> {
    if crate::extract_role(message) != "user" {
        return None;
    }
    let content = crate::extract_text_content_without_tool_blocks(message);
    let directory = content
        .lines()
        .find_map(|line| line.trim_start().strip_prefix(SKILL_DIRECTORY_MARKER))?
        .trim()
        .trim_end_matches(['/', '\\']);
    let name = directory.rsplit(['/', '\\']).next().filter(|name| {
        name.len() <= 64
            && name
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    });
    Some(serde_json::json!({
        "role": "system",
        "content": format!("[Skill loaded: {}]", name.unwrap_or("unknown")),
    }))
}

/// Apply the policy before credential redaction can erase an injection marker.
/// Keep source indexes for HTML rendering and indexed record lookup.
pub(crate) fn select_export_records<'a>(
    raw_messages: &'a mut [Value],
    archived_messages: Option<&'a [Message]>,
    injections: ExportInjections,
) -> impl Iterator<Item = (usize, &'a mut Value)> {
    raw_messages
        .iter_mut()
        .enumerate()
        .filter_map(move |(index, message)| {
            let archived = archived_messages.and_then(|archived| archived.get(index));
            if injections.excludes_record(message, archived) {
                let reminders = ExportInjections {
                    include_skills: true,
                    ..injections
                };
                if injections.include_skills || reminders.excludes_record(message, archived) {
                    return None;
                }
                *message = skill_placeholder(message)?;
            }
            Some((index, message))
        })
}

/// Project one conversation turn without retaining its source envelope.
///
/// This is an export-only view: callers keep the source records and canonical
/// rows unchanged. An allowlist is essential here because removing a top-level
/// tool field would leave arguments/results in nested metadata in JSON exports.
/// `source_index` is the original record position, not a renumbered output row;
/// `message_index` is the one-based canonical follow-up ordinal, present only
/// when the archive supplied a valid zero-based message index.
pub(crate) fn messages_only_record(
    source_index: usize,
    raw: &Value,
    archived: Option<&Message>,
) -> Option<Value> {
    project_messages_only_record(source_index, raw, archived, false)
}

/// Project a trusted loader's flattened content when original blocks are lost.
/// This choice belongs to the caller, never to a field in an untrusted record.
pub(crate) fn messages_only_flattened_record(source_index: usize, raw: &Value) -> Option<Value> {
    project_messages_only_record(source_index, raw, None, true)
}

fn project_messages_only_record(
    source_index: usize,
    raw: &Value,
    archived: Option<&Message>,
    flattened_source: bool,
) -> Option<Value> {
    let raw_role = crate::extract_role(raw);
    // Selection can replace an injected skill with a system placeholder.
    // Never reconstruct its omitted original from an archived user-role row.
    if !matches!(raw_role.as_str(), "user" | "assistant") {
        return None;
    }
    let role = archived.map_or_else(
        || raw_role,
        |message| crate::role_to_export_string(&message.role),
    );
    if !matches!(role.as_str(), "user" | "assistant") {
        return None;
    }

    // Searchable archive text may flatten a mixed text/tool message. Recover
    // its preserved content blocks before selecting prose, including legacy
    // raw-line envelopes. A recognized tool-only content value is final: do
    // not fall back to flattened text when it produces no visible message.
    let preserved = archived
        .and_then(|message| {
            crate::storage::sqlite::message_extra_to_source_jsonl_line(&message.extra_json)
        })
        .and_then(|line| serde_json::from_str::<Value>(&line).ok());
    let preserved_source = preserved.as_ref().filter(|event| {
        crate::extract_role(event) == role
            && (messages_only_nonprose_envelope(event) || messages_only_content(event).is_some())
    });
    let is_cline_sdk =
        |extra: &Value| extra.pointer("/cass/cline_sdk").and_then(Value::as_bool) == Some(true);
    if preserved_source.is_none()
        && (preserved.as_ref().is_some_and(is_cline_sdk)
            || archived.is_some_and(|message| is_cline_sdk(&message.extra_json)))
    {
        // Cline SDK's compact archive metadata loses the original block
        // boundaries and does not distinguish prose from unmarked reasoning,
        // file contents or mixed tool results. Even tool_call_count == 0 is
        // insufficient evidence. Keep this unrecoverable turn in the archive
        // but omit it from the strict view instead of presenting tool data as
        // a prompt or reply.
        return None;
    }
    let source = preserved_source.unwrap_or(raw);
    if messages_only_nonprose_envelope(source)
        || messages_only_nonprose_envelope(messages_only_body(source)?)
    {
        // Reasoning and tool-directed messages can carry ordinary text blocks.
        // A preserved nonprose envelope is authoritative even when the archive
        // also has flattened text; never fall back to that text.
        return None;
    }
    let source_content = messages_only_content(source)?;
    let mut content = messages_only_text(source_content);
    let legacy_prefix = (flattened_source || (archived.is_some() && preserved_source.is_none()))
        && source_content.is_string()
        && {
            let prefix = messages_only_legacy_prefix(&content);
            let shortened = prefix.len() != content.len();
            if shortened {
                content = prefix.to_string();
            }
            // Pi's ordinary renderer may have already removed the same
            // flattened suffix. Compare only to label the loss; never copy
            // anything from the original canonical text back into this view.
            shortened
                || archived.is_some_and(|message| {
                    let original_prefix = messages_only_legacy_prefix(&message.content);
                    original_prefix.len() != message.content.len()
                        && original_prefix.starts_with(&content)
                })
        };
    if content.trim().is_empty() {
        return None;
    }

    let mut projected = serde_json::json!({
        "role": role,
        "content": content,
        "source_index": source_index,
    });
    if legacy_prefix {
        projected["content_projection"] = Value::from("legacy-visible-prefix");
    }
    if let Some(message_index) = archived
        .and_then(|message| message.idx.checked_add(1))
        .filter(|index| *index > 0)
    {
        projected["message_index"] = Value::from(message_index);
    }
    if let Some(timestamp) = archived
        .and_then(|message| message.created_at)
        .or_else(|| crate::extract_message_timestamp(source))
        .or_else(|| crate::extract_message_timestamp(raw))
    {
        projected["timestamp"] = Value::from(timestamp);
    }
    if let Some(id) = messages_only_body(source)
        .and_then(|body| body.get("id"))
        .or_else(|| source.get("id"))
        .or_else(|| source.get("uuid"))
        .filter(|id| id.is_string() || id.is_number())
    {
        projected["id"] = id.clone();
    }
    Some(projected)
}

fn messages_only_legacy_prefix(content: &str) -> &str {
    // Some older connectors retained only flattened search text. Once a tool
    // description/argument or reasoning segment begins, its newlines cannot
    // be distinguished from later prose without the original content blocks.
    // Preserve only the known prefix; never guess a boundary that could copy
    // tool data back into the export. Structured/string source envelopes are
    // exempt so literal discussions of these markers remain intact.
    let prefix = crate::without_flattened_tool_suffix(content);
    let mut offset = 0;
    for line in prefix.split_inclusive('\n') {
        let text = line.trim_start();
        if text.starts_with("[Tool:")
            || text.starts_with("[Tool Output]")
            || text.starts_with("[Patch]")
            || text.starts_with("[Thinking]")
            || text.starts_with("[Reasoning]")
        {
            return prefix[..offset].trim_end_matches(['\r', '\n']);
        }
        offset += line.len();
    }
    prefix
}

fn messages_only_content(record: &Value) -> Option<&Value> {
    let body = messages_only_body(record)?;
    if messages_only_nonprose_envelope(record) || messages_only_nonprose_envelope(body) {
        // A role-less tool envelope can sit inside a user event. Keep it as a
        // typed, non-text block so the preserved source is recognized and its
        // empty projection cannot fall back to flattened canonical tool text.
        Some(body)
    } else {
        body.get("content")
    }
}

fn messages_only_nonprose_envelope(body: &Value) -> bool {
    // Some providers use ordinary message/text types for reasoning and tool
    // dispatch, distinguished only by channel or destination metadata. Keep
    // public commentary/final replies and ordinary conversational recipients.
    if matches!(
        body.get("channel").and_then(Value::as_str),
        Some("analysis" | "reasoning" | "thinking")
    ) || body
        .get("recipient")
        .and_then(Value::as_str)
        .is_some_and(|recipient| !matches!(recipient, "all" | "user" | "assistant"))
    {
        return true;
    }
    matches!(
        body.get("type").and_then(Value::as_str),
        Some(
            "tool"
                | "tool_use"
                | "tool_result"
                | "tool_call"
                | "toolCall"
                | "toolResult"
                | "function_call"
                | "function_call_output"
                | "custom_tool_call"
                | "custom_tool_call_output"
                | "thinking"
                | "reasoning"
                | "patch"
        )
    )
}

fn messages_only_body(record: &Value) -> Option<&Value> {
    // Follow the role extractor's envelope precedence. A second envelope must
    // not smuggle its content into a record classified by a different role.
    if let Some(role) = record.get("role").and_then(Value::as_str) {
        if record.get("content").is_none() {
            // Cursor agent transcripts put the role on the outer event but
            // the content on a role-less message object. This is the same
            // turn, unlike an explicitly conflicting nested envelope.
            for key in ["message", "payload"] {
                if let Some(body) = record.get(key)
                    && (body.get("content").is_some() || messages_only_nonprose_envelope(body))
                    && body
                        .get("role")
                        .and_then(Value::as_str)
                        .is_none_or(|nested| {
                            crate::normalized_export_role(nested)
                                == crate::normalized_export_role(role)
                        })
                    && (matches!(
                        body.get("type").and_then(Value::as_str),
                        None | Some("message" | "user" | "assistant")
                    ) || messages_only_nonprose_envelope(body))
                {
                    return Some(body);
                }
            }
        }
        return Some(record);
    }
    for key in ["message", "payload"] {
        if let Some(body) = record.get(key)
            && body.get("role").is_some_and(Value::is_string)
        {
            return Some(body);
        }
    }
    if matches!(
        record.get("type").and_then(Value::as_str),
        Some("user" | "assistant")
    ) {
        if record.get("content").is_some() {
            return Some(record);
        }
        for key in ["message", "payload"] {
            if let Some(body) = record.get(key)
                && (body.get("content").is_some() || messages_only_nonprose_envelope(body))
                && (matches!(
                    body.get("type").and_then(Value::as_str),
                    None | Some("message" | "user" | "assistant")
                ) || messages_only_nonprose_envelope(body))
            {
                return Some(body);
            }
        }
    }
    None
}

fn messages_only_text(content: &Value) -> String {
    // The ordinary no-tools renderer also retains reasoning blocks. This
    // stricter view contains just prompts and replies, so use only text blocks
    // and never recursively flatten tool results, thinking or attachments.
    if let Some(blocks) = content.as_array() {
        blocks
            .iter()
            .filter_map(messages_only_text_block)
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        messages_only_text_block(content)
            .unwrap_or_default()
            .to_string()
    }
}

fn messages_only_text_block(block: &Value) -> Option<&str> {
    if messages_only_nonprose_envelope(block) {
        None
    } else {
        crate::export_text_block(block)
    }
}

/// Convert the selected source records after the caller redacts credentials.
pub(crate) fn html_messages_from_raw<'a>(
    raw_messages: impl IntoIterator<Item = (usize, &'a Value)>,
    include_tools: bool,
) -> Vec<crate::html_export::Message> {
    use crate::html_export::Message;
    use chrono::TimeZone;

    raw_messages
        .into_iter()
        .flat_map(|(i, msg)| {
            let role = crate::extract_role(msg);
            if !include_tools && role == "tool" {
                return Vec::new();
            }
            let timestamp = crate::extract_message_timestamp(msg)
                .and_then(|ts| chrono::Utc.timestamp_millis_opt(ts).single())
                .map(|dt| dt.to_rfc3339());
            let tool_calls = if include_tools {
                crate::extract_tool_calls(msg)
            } else {
                Vec::new()
            };
            let content = if include_tools {
                if !tool_calls.is_empty() {
                    crate::strip_tool_marker(&crate::extract_text_content_without_tool_blocks(msg))
                } else {
                    crate::extract_text_content(msg)
                }
            } else {
                crate::extract_text_content_without_tool_blocks(msg)
            };
            match role.as_str() {
                "user" | "assistant" | "system" | "tool" | "unknown" => {}
                _ => return Vec::new(),
            }
            if content.is_empty() && tool_calls.is_empty() {
                return Vec::new();
            }
            if tool_calls.is_empty() {
                return vec![Message {
                    role,
                    content,
                    timestamp,
                    tool_call: None,
                    index: Some(i),
                    author: None,
                }];
            }
            tool_calls
                .into_iter()
                .enumerate()
                .map(|(tool_index, tool_call)| Message {
                    role: role.clone(),
                    content: if tool_index == 0 {
                        content.clone()
                    } else {
                        String::new()
                    },
                    timestamp: timestamp.clone(),
                    tool_call: Some(tool_call),
                    index: Some(i),
                    author: None,
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Supported export formats
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExportFormat {
    /// Markdown format with headers, code blocks, and rich formatting
    #[default]
    Markdown,
    /// JSON format for programmatic consumption
    Json,
    /// Plain text format for simple copy-paste
    PlainText,
}

impl ExportFormat {
    fn metadata(self) -> (&'static str, &'static str, Self) {
        match self {
            Self::Markdown => ("Markdown", "md", Self::Json),
            Self::Json => ("JSON", "json", Self::PlainText),
            Self::PlainText => ("Plain Text", "txt", Self::Markdown),
        }
    }

    /// Get the display name for this format
    pub fn name(self) -> &'static str {
        self.metadata().0
    }

    /// Get the file extension for this format
    pub fn extension(self) -> &'static str {
        self.metadata().1
    }

    /// Cycle to the next export format
    pub fn next(self) -> Self {
        self.metadata().2
    }

    /// List all available formats
    pub fn all() -> &'static [Self] {
        &[Self::Markdown, Self::Json, Self::PlainText]
    }
}

/// Options for export customization
#[derive(Debug, Clone)]
pub struct ExportOptions {
    /// Include full content (not just snippets)
    pub include_content: bool,
    /// Include score in output
    pub include_score: bool,
    /// Include source path
    pub include_path: bool,
    /// Maximum snippet length (0 = unlimited)
    pub max_snippet_len: usize,
    /// Query string (for header/metadata)
    pub query: Option<String>,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            include_content: false,
            include_score: true,
            include_path: true,
            max_snippet_len: 500,
            query: None,
        }
    }
}

/// Export search results to the specified format
pub fn export_results(hits: &[SearchHit], format: ExportFormat, options: &ExportOptions) -> String {
    match format {
        ExportFormat::Markdown => export_markdown(hits, options),
        ExportFormat::Json => export_json(hits, options),
        ExportFormat::PlainText => export_plain_text(hits, options),
    }
}

/// Escape special Markdown characters to prevent formatting issues or injection.
fn escape_markdown(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('|', "\\|")
        .replace('*', "\\*")
        .replace('_', "\\_")
        .replace('[', "\\[")
        .replace(']', "\\]")
        .replace('<', "\\<")
        .replace('>', "\\>")
        .replace('`', "\\`")
        .replace('\n', " ") // Replace newlines with space to prevent breaking tables
        .replace('\r', "") // Remove carriage returns
}

/// Determine the appropriate code block delimiter (e.g., ``` or ````) based on content.
fn get_code_block_delimiter(content: &str) -> String {
    let mut max_backticks = 0;
    let mut current = 0;
    for c in content.chars() {
        if c == '`' {
            current += 1;
        } else {
            max_backticks = max_backticks.max(current);
            current = 0;
        }
    }
    max_backticks = max_backticks.max(current);

    let needed = (max_backticks + 1).max(3);
    "`".repeat(needed)
}

fn get_code_span_delimiter(content: &str) -> String {
    let mut max_backticks = 0;
    let mut current = 0;
    for c in content.chars() {
        if c == '`' {
            current += 1;
        } else {
            max_backticks = max_backticks.max(current);
            current = 0;
        }
    }
    max_backticks = max_backticks.max(current);

    "`".repeat(max_backticks + 1)
}

fn markdown_code_span(content: &str) -> String {
    let delim = get_code_span_delimiter(content);
    if content.starts_with('`') || content.ends_with('`') {
        format!("{delim} {content} {delim}")
    } else {
        format!("{delim}{content}{delim}")
    }
}

/// Export to Markdown format
fn export_markdown(hits: &[SearchHit], options: &ExportOptions) -> String {
    let mut output = String::new();

    // Header
    output.push_str("# Search Results\n\n");

    if let Some(query) = &options.query {
        output.push_str(&format!("**Query:** {}\n\n", markdown_code_span(query)));
    }

    output.push_str(&format!(
        "**Results:** {} | **Exported:** {}\n\n",
        hits.len(),
        Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
    ));

    output.push_str("---\n\n");

    // Results
    for (i, hit) in hits.iter().enumerate() {
        let safe_title = escape_markdown(&hit.title);
        output.push_str(&format!("## {}. {}\n\n", i + 1, safe_title));

        // Metadata table
        output.push_str("| Field | Value |\n");
        output.push_str("|-------|-------|\n");
        output.push_str(&format!("| Agent | {} |\n", escape_markdown(&hit.agent)));
        output.push_str(&format!(
            "| Workspace | {} |\n",
            escape_markdown(&hit.workspace)
        ));

        if options.include_score {
            output.push_str(&format!("| Score | {:.2} |\n", hit.score));
        }

        if let Some(ts) = hit.created_at
            && let Some(dt) = DateTime::from_timestamp_millis(ts)
        {
            output.push_str(&format!("| Date | {} |\n", dt.format("%Y-%m-%d %H:%M")));
        }

        if options.include_path {
            let source_path_chars = hit.source_path.chars().count();
            let path_display: Cow<'_, str> = if source_path_chars > 60 {
                let skip = source_path_chars - 57;
                Cow::Owned(format!(
                    "...{}",
                    hit.source_path.chars().skip(skip).collect::<String>()
                ))
            } else {
                Cow::Borrowed(hit.source_path.as_str())
            };
            output.push_str(&format!(
                "| Source | {} |\n",
                escape_markdown(path_display.as_ref())
            ));

            if let Some(line) = hit.line_number {
                output.push_str(&format!("| Line | {line} |\n"));
            }
        }

        output.push('\n');

        // Snippet
        output.push_str("### Snippet\n\n");
        let snippet = truncate_text(&hit.snippet, options.max_snippet_len);
        let delim = get_code_block_delimiter(&snippet);
        output.push_str(&format!("{}\n", delim));
        output.push_str(&snippet);
        if !snippet.ends_with('\n') {
            output.push('\n');
        }
        output.push_str(&format!("{}\n\n", delim));

        // Full content (optional)
        if options.include_content && !hit.content.is_empty() {
            output.push_str("<details>\n<summary>Full Content</summary>\n\n");
            let content_delim = get_code_block_delimiter(&hit.content);
            output.push_str(&format!("{}\n", content_delim));
            output.push_str(&hit.content);
            if !hit.content.ends_with('\n') {
                output.push('\n');
            }
            output.push_str(&format!("{}\n\n", content_delim));
            output.push_str("</details>\n\n");
        }

        output.push_str("---\n\n");
    }

    output
}

/// Export to JSON format
fn export_json(hits: &[SearchHit], options: &ExportOptions) -> String {
    let exported_at = Utc::now().to_rfc3339();
    let export_data = export_json_value(hits, options, &exported_at);

    serde_json::to_string_pretty(&export_data).unwrap_or_else(|_| "{}".to_string())
}

fn export_json_value(
    hits: &[SearchHit],
    options: &ExportOptions,
    exported_at: &str,
) -> serde_json::Value {
    serde_json::json!({
        "query": options.query,
        "count": hits.len(),
        "exported_at": exported_at,
        "hits": hits
            .iter()
            .map(|hit| export_hit_json(hit, options))
            .collect::<Vec<_>>()
    })
}

fn export_hit_json(hit: &SearchHit, options: &ExportOptions) -> serde_json::Value {
    let mut obj = export_hit_base_json(hit, options);

    if options.include_score {
        let score = if hit.score.is_finite() {
            hit.score
        } else {
            0.0
        };
        obj.insert("score".to_string(), serde_json::json!(score));
    }

    if options.include_path {
        obj.insert(
            "source_path".to_string(),
            serde_json::json!(hit.source_path),
        );
        if let Some(line) = hit.line_number {
            obj.insert("line_number".to_string(), serde_json::json!(line));
        }
    }

    if let Some(ts) = hit.created_at {
        obj.insert("created_at".to_string(), serde_json::json!(ts));
        if let Some(dt) = DateTime::from_timestamp_millis(ts) {
            obj.insert(
                "created_at_formatted".to_string(),
                serde_json::json!(dt.to_rfc3339()),
            );
        }
    }

    if options.include_content && !hit.content.is_empty() {
        obj.insert("content".to_string(), serde_json::json!(hit.content));
    }

    Value::Object(obj)
}

fn export_hit_base_json(hit: &SearchHit, options: &ExportOptions) -> Map<String, Value> {
    Map::from_iter([
        ("title".to_string(), serde_json::json!(hit.title)),
        ("agent".to_string(), serde_json::json!(hit.agent)),
        ("workspace".to_string(), serde_json::json!(hit.workspace)),
        (
            "snippet".to_string(),
            serde_json::json!(truncate_text(&hit.snippet, options.max_snippet_len)),
        ),
    ])
}

/// Export to plain text format
fn export_plain_text(hits: &[SearchHit], options: &ExportOptions) -> String {
    let mut output = String::new();

    // Header
    output.push_str("SEARCH RESULTS\n");
    output.push_str(&"=".repeat(60));
    output.push('\n');

    if let Some(query) = &options.query {
        output.push_str(&format!("Query: {query}\n"));
    }

    output.push_str(&format!(
        "Results: {} | Exported: {}\n",
        hits.len(),
        Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
    ));

    output.push_str(&"=".repeat(60));
    output.push_str("\n\n");

    // Results
    for (i, hit) in hits.iter().enumerate() {
        output.push_str(&format!("[{}] {}\n", i + 1, hit.title));
        output.push_str(&"-".repeat(60));
        output.push('\n');

        output.push_str(&format!("Agent: {}\n", hit.agent));
        output.push_str(&format!("Workspace: {}\n", hit.workspace));

        if options.include_score {
            output.push_str(&format!("Score: {:.2}\n", hit.score));
        }

        if let Some(ts) = hit.created_at
            && let Some(dt) = DateTime::from_timestamp_millis(ts)
        {
            output.push_str(&format!("Date: {}\n", dt.format("%Y-%m-%d %H:%M")));
        }

        if options.include_path {
            output.push_str(&format!("Source: {}\n", hit.source_path));
            if let Some(line) = hit.line_number {
                output.push_str(&format!("Line: {line}\n"));
            }
        }

        output.push('\n');
        output.push_str("Snippet:\n");
        let snippet = truncate_text(&hit.snippet, options.max_snippet_len);
        for line in snippet.lines() {
            output.push_str(&format!("  {line}\n"));
        }

        if options.include_content && !hit.content.is_empty() {
            output.push_str("\nFull Content:\n");
            for line in hit.content.lines() {
                output.push_str(&format!("  {line}\n"));
            }
        }

        output.push('\n');
    }

    output
}

/// Truncate text to max length (in characters), adding ellipsis if needed.
///
/// When max_len <= 3, truncates without ellipsis to avoid exceeding max_len.
fn truncate_text(text: &str, max_len: usize) -> String {
    if max_len == 0 {
        return text.to_string();
    }

    let mut chars = text.chars();
    let mut preview: String = chars.by_ref().take(max_len).collect();

    if chars.next().is_none() {
        return preview;
    }

    // For very small max_len (≤3), truncate without ellipsis to avoid exceeding limit
    if max_len <= 3 {
        return preview;
    }

    let take = max_len.saturating_sub(3);
    preview.truncate(preview.chars().take(take).map(|c| c.len_utf8()).sum());
    preview.push_str("...");
    preview
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_only_projects_mixed_turns_without_raw_tool_metadata() {
        let raw = serde_json::json!({
            "type": "assistant",
            "uuid": "turn-7",
            "timestamp": "2026-01-01T00:00:01Z",
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "Before café 界"},
                    {"type": "tool_use", "id": "call-7", "name": "Read", "input": {"path": "PRIVATE_ARGUMENT"}},
                    {"type": "thinking", "thinking": "PRIVATE_REASONING"},
                    {"type": "tool_result", "content": "PRIVATE_RESULT"},
                    {"type": "image", "source": {"data": "PRIVATE_ATTACHMENT"}},
                    {"type": "text", "text": "After [Tool: literal prose]"}
                ]
            },
            "metadata": {"tool_result": "PRIVATE_METADATA"}
        });
        let before = raw.clone();
        let projected = messages_only_record(7, &raw, None).expect("ordinary assistant text");
        assert_eq!(
            projected,
            serde_json::json!({
                "role": "assistant",
                "content": "Before café 界\nAfter [Tool: literal prose]",
                "source_index": 7,
                "id": "turn-7",
                "timestamp": 1767225601000_i64
            })
        );
        assert!(!projected.to_string().contains("PRIVATE_"));
        assert_eq!(raw, before, "projection must not rewrite the source record");
    }

    #[test]
    fn messages_only_omits_non_conversation_and_tool_only_records() {
        for raw in [
            serde_json::json!({"role": "system", "content": "internal instructions"}),
            serde_json::json!({"role": "tool", "content": "a file read"}),
            serde_json::json!({"message": {"role": "toolResult", "content": "tool output"}}),
            serde_json::json!({"type": "session_meta", "payload": {"content": "metadata"}}),
            serde_json::json!({"role": "unknown", "content": "unknown content"}),
            serde_json::json!({"role": "assistant", "content": [{"type": "thinking", "thinking": "private"}]}),
            serde_json::json!({"role": "user", "content": {"type": "tool_result", "content": "result"}}),
            serde_json::json!({"role": "user", "type": "tool_result", "content": "result"}),
            serde_json::json!({"type": "user", "message": {"type": "tool_result", "content": "result"}}),
            serde_json::json!({"role": "user", "message": {"type": "tool_result", "content": "result"}}),
            serde_json::json!({"role": "assistant", "content": [{"type": "toolCall", "name": "Read", "arguments": {"text": "argument"}}]}),
            serde_json::json!({"role": "user", "content": " \n\t"}),
        ] {
            assert!(messages_only_record(2, &raw, None).is_none(), "{raw}");
        }
    }

    #[test]
    fn messages_only_omits_reasoning_and_tool_directed_text_envelopes() {
        for body in [
            serde_json::json!({"role": "assistant", "type": "message", "channel": "analysis",
                "content": [{"type": "output_text", "text": "PRIVATE_REASONING"}]}),
            serde_json::json!({"role": "assistant", "channel": "reasoning",
                "content": "PRIVATE_REASONING", "text": "PRIVATE_UNTYPED_FALLBACK"}),
            serde_json::json!({"role": "assistant", "channel": "thinking",
                "content": "PRIVATE_THINKING"}),
            serde_json::json!({"role": "assistant", "type": "message", "channel": "commentary",
                "recipient": "functions.exec", "content": [{"type": "output_text", "text": "PRIVATE_TOOL_INPUT"}]}),
            serde_json::json!({"role": "assistant", "recipient": "python",
                "content": "PRIVATE_TOOL_INPUT", "text": "PRIVATE_UNTYPED_FALLBACK"}),
            serde_json::json!({"role": "assistant", "type": "assistant", "channel": "analysis"}),
        ] {
            for raw in [
                body.clone(),
                serde_json::json!({"type": "response_item", "payload": body.clone()}),
                serde_json::json!({"type": "assistant", "message": body.clone()}),
            ] {
                let before = raw.clone();
                assert!(messages_only_record(5, &raw, None).is_none(), "{raw}");
                assert!(messages_only_flattened_record(5, &raw).is_none(), "{raw}");

                let flattened = serde_json::json!({
                    "role": "assistant", "content": "PRIVATE_FLATTENED_CONTENT"
                });
                for extra_json in [
                    raw.clone(),
                    serde_json::json!({"__cass_historical_raw_json__": raw.to_string()}),
                ] {
                    let archived = Message {
                        id: Some(900),
                        idx: 5,
                        role: crate::model::types::MessageRole::Agent,
                        author: None,
                        created_at: None,
                        content: "PRIVATE_FLATTENED_CONTENT".to_string(),
                        extra_json,
                        snippets: Vec::new(),
                    };
                    assert!(messages_only_record(5, &flattened, Some(&archived)).is_none());
                    assert_eq!(archived.content, "PRIVATE_FLATTENED_CONTENT");
                }
                assert_eq!(raw, before, "projection must leave the source intact");
            }
        }
    }

    #[test]
    fn messages_only_preserved_reasoning_metadata_prevents_flattened_fallback() {
        let raw =
            serde_json::json!({"role": "assistant", "content": "PRIVATE_FLATTENED_REASONING"});
        for extra_json in [
            serde_json::json!({"type": "assistant", "channel": "analysis"}),
            serde_json::json!({"type": "assistant", "recipient": "python"}),
        ] {
            let archived = Message {
                id: None,
                idx: 2,
                role: crate::model::types::MessageRole::Agent,
                author: None,
                created_at: None,
                content: "PRIVATE_FLATTENED_REASONING".to_string(),
                extra_json,
                snippets: Vec::new(),
            };
            assert!(messages_only_record(2, &raw, Some(&archived)).is_none());
        }
    }

    #[test]
    fn messages_only_applies_channel_policy_to_wrappers_and_text_blocks() {
        for raw in [
            serde_json::json!({"role": "assistant", "channel": "analysis",
                "message": {"content": "PRIVATE_WRAPPED_REASONING"}}),
            serde_json::json!({"type": "assistant", "recipient": "functions.exec",
                "message": {"role": "assistant", "content": "PRIVATE_WRAPPED_TOOL_INPUT"}}),
            serde_json::json!({"type": "response_item", "channel": "analysis",
                "payload": {"role": "assistant", "content": "PRIVATE_WRAPPED_REASONING"}}),
            serde_json::json!({"role": "assistant", "content": {
                "type": "text", "channel": "thinking", "text": "PRIVATE_TYPED_REASONING"}}),
        ] {
            assert!(messages_only_record(4, &raw, None).is_none(), "{raw}");
        }
        let raw = serde_json::json!({"role": "assistant", "content": [
            {"type": "text", "text": "Before"},
            {"type": "output_text", "channel": "analysis", "text": "PRIVATE_REASONING"},
            {"type": "text", "recipient": "python", "text": "PRIVATE_TOOL_INPUT"},
            {"channel": "thinking", "text": "PRIVATE_UNTYPED_REASONING"},
            {"type": "output_text", "channel": "final", "recipient": "all", "text": "After"}
        ]});
        let projected = messages_only_record(6, &raw, None).expect("public text blocks");
        assert_eq!(projected["content"], "Before\nAfter");
        assert!(!projected.to_string().contains("PRIVATE_"));
    }

    #[test]
    fn messages_only_preserves_public_channels_and_selected_envelope() {
        for channel in [Value::Null, Value::from("commentary"), Value::from("final")] {
            for recipient in [Value::Null, Value::from("all"), Value::from("user")] {
                let raw = serde_json::json!({"type": "response_item", "payload": {
                    "type": "message", "role": "assistant", "channel": channel,
                    "recipient": recipient, "phase": "final_answer", "id": "public-reply",
                    "content": [{"type": "output_text", "text": "Ordinary reply"}]
                }});
                let projected =
                    messages_only_record(9, &raw, None).expect("public assistant reply");
                assert_eq!(projected["content"], "Ordinary reply");
                assert_eq!(projected["id"], "public-reply");
                assert_eq!(projected["source_index"], 9);
            }
        }

        // Metadata on a conflicting envelope cannot suppress the actual turn.
        let raw = serde_json::json!({
            "role": "user", "recipient": "assistant", "content": "Please explain the analysis channel",
            "message": {"role": "assistant", "channel": "analysis",
                "recipient": "python", "content": "PRIVATE_OTHER_ENVELOPE"}
        });
        let projected = messages_only_record(10, &raw, None).expect("ordinary user prompt");
        assert_eq!(projected["content"], "Please explain the analysis channel");
    }

    #[test]
    fn messages_only_preserves_text_block_shapes_and_original_positions() {
        let records = [
            serde_json::json!({"type": "session"}),
            serde_json::json!({"role": "user", "content": "first prompt"}),
            serde_json::json!({"role": "tool", "content": "omitted"}),
            serde_json::json!({"type": "response_item", "payload": {
                "role": "assistant", "id": "codex-response", "content": [
                    {"type": "output_text", "text": "first reply"},
                    "plain array text",
                    {"text": "untyped text"}
                ]
            }}),
            serde_json::json!({"role": "user", "content": {"type": "input_text", "text": "follow-up"}}),
        ];
        let selected: Vec<_> = records
            .iter()
            .enumerate()
            .filter_map(|(index, raw)| messages_only_record(index, raw, None))
            .collect();
        assert_eq!(selected.len(), 3);
        assert_eq!(selected[0]["source_index"], 1);
        assert_eq!(selected[1]["source_index"], 3);
        assert_eq!(selected[2]["source_index"], 4);
        assert_eq!(selected[1]["id"], "codex-response");
        assert_eq!(
            selected[1]["content"],
            "first reply\nplain array text\nuntyped text"
        );
        assert_eq!(selected[2]["content"], "follow-up");
        assert!(
            selected
                .iter()
                .all(|record| record.get("message_index").is_none())
        );
    }

    #[test]
    fn messages_only_archive_uses_preserved_blocks_and_canonical_identity() {
        let source_event = serde_json::json!({
            "type": "message", "id": "pi-reply", "timestamp": 1767225601000_i64,
            "message": {"role": "assistant", "content": [
                {"type": "text", "text": "Before"},
                {"type": "toolCall", "name": "Read", "arguments": {"path": "PRIVATE_ARGUMENT"}},
                {"type": "thinking", "thinking": "PRIVATE_THINKING"},
                {"type": "text", "text": "After"}
            ]}
        });
        let raw = serde_json::json!({
            "role": "assistant",
            "content": "Before\n[Tool: Read] PRIVATE_ARGUMENT\n[Thinking] PRIVATE_THINKING\nAfter"
        });
        for extra_json in [
            source_event.clone(),
            serde_json::json!({"__cass_historical_raw_json__": source_event.to_string()}),
        ] {
            let archived = Message {
                id: Some(900),
                idx: 42,
                role: crate::model::types::MessageRole::Agent,
                author: None,
                created_at: Some(1767225602000_i64),
                content: raw["content"].as_str().unwrap().to_string(),
                extra_json,
                snippets: Vec::new(),
            };
            let projected = messages_only_record(6, &raw, Some(&archived)).expect("archive prose");
            assert_eq!(
                projected,
                serde_json::json!({
                    "role": "assistant", "content": "Before\nAfter", "source_index": 6,
                    "message_index": 43, "id": "pi-reply", "timestamp": 1767225602000_i64
                })
            );
            assert!(!projected.to_string().contains("PRIVATE_"));
            assert!(archived.content.contains("PRIVATE_ARGUMENT"));
        }
    }

    #[test]
    fn messages_only_archive_cannot_restore_hidden_tools_or_host_context() {
        let raw = serde_json::json!({"role": "user", "content": "flattened tool result"});
        let mut archived = Message {
            id: None,
            idx: 8,
            role: crate::model::types::MessageRole::User,
            author: None,
            created_at: None,
            content: "flattened tool result".to_string(),
            extra_json: serde_json::json!({
                "type": "user", "message": {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call", "content": "flattened tool result"}
                ]}
            }),
            snippets: Vec::new(),
        };
        assert!(messages_only_record(8, &raw, Some(&archived)).is_none());
        for preserved in [
            serde_json::json!({"type": "user", "message": {
                "type": "tool_result", "content": "flattened tool result"
            }}),
            serde_json::json!({"role": "user", "message": {
                "type": "tool_result", "content": "flattened tool result"
            }}),
            serde_json::json!({"role": "user", "type": "tool_result", "content": "flattened tool result"}),
            serde_json::json!({"type": "user", "payload": {
                "type": "function_call_output", "output": "flattened tool result"
            }}),
        ] {
            archived.extra_json = preserved;
            assert!(messages_only_record(8, &raw, Some(&archived)).is_none());
        }
        archived.role = crate::model::types::MessageRole::System;
        archived.extra_json = serde_json::json!({"role": "user", "content": "host context"});
        assert!(messages_only_record(8, &raw, Some(&archived)).is_none());

        archived.role = crate::model::types::MessageRole::User;
        archived.extra_json = serde_json::json!({"role": "user", "content": "PRIVATE_SKILL_BODY"});
        let placeholder =
            serde_json::json!({"role": "system", "content": "[Skill loaded: example]"});
        assert!(messages_only_record(8, &placeholder, Some(&archived)).is_none());
    }

    #[test]
    fn messages_only_content_and_identity_follow_the_role_envelope() {
        let nested = serde_json::json!({
            "content": "PRIVATE_OUTER_METADATA",
            "message": {"role": "user", "id": "actual-user", "content": "ordinary prompt"},
            "payload": {"role": "tool", "content": "PRIVATE_OTHER_ENVELOPE"}
        });
        let result = messages_only_record(3, &nested, None).expect("nested user message");
        assert_eq!(result["content"], "ordinary prompt");
        assert_eq!(result["id"], "actual-user");
        assert!(!result.to_string().contains("PRIVATE_"));

        let outer = serde_json::json!({
            "role": "assistant", "id": "actual-assistant", "content": "ordinary reply",
            "message": {"role": "tool", "id": "PRIVATE_TOOL_ID", "content": "PRIVATE_TOOL_RESULT"}
        });
        let result = messages_only_record(4, &outer, None).expect("outer assistant message");
        assert_eq!(result["content"], "ordinary reply");
        assert_eq!(result["id"], "actual-assistant");
        assert!(!result.to_string().contains("PRIVATE_"));

        let absent = serde_json::json!({
            "role": "assistant", "message": {"role": "tool", "content": "PRIVATE_FALLBACK"}
        });
        assert!(messages_only_record(5, &absent, None).is_none());

        let cursor = serde_json::json!({"role": "assistant", "message": {"content": [
            {"type": "text", "text": "Cursor narrative"},
            {"type": "tool_use", "name": "Read", "input": {"description": "PRIVATE_CURSOR_TOOL"}}
        ]}});
        let result = messages_only_record(6, &cursor, None).expect("Cursor text envelope");
        assert_eq!(result["content"], "Cursor narrative");
        assert!(!result.to_string().contains("PRIVATE_"));
        let typed_tool = serde_json::json!({
            "role": "assistant", "message": {"type": "tool_result", "content": "PRIVATE_TOOL_RESULT"}
        });
        assert!(messages_only_record(7, &typed_tool, None).is_none());
    }

    #[test]
    fn messages_only_selection_and_redaction_do_not_restore_omitted_archive_data() {
        let mut records = [
            serde_json::json!({"role": "user", "content":
                "Base directory for this skill: /private/example\nPRIVATE_SKILL_BODY"}),
            serde_json::json!({"role": "assistant", "content": [
                {"type": "text", "text": "Ordinary reply password=synthetic-projection-secret"},
                {"type": "tool_use", "name": "Read", "input": {"path": "PRIVATE_TOOL_ARGUMENT"}}
            ]}),
        ];
        let archived: Vec<_> = records
            .iter()
            .enumerate()
            .map(|(index, record)| Message {
                id: None,
                idx: index as i64,
                role: if index == 0 {
                    crate::model::types::MessageRole::User
                } else {
                    crate::model::types::MessageRole::Agent
                },
                author: None,
                created_at: None,
                content: crate::extract_text_content(record),
                extra_json: record.clone(),
                snippets: Vec::new(),
            })
            .collect();
        let archive_before = serde_json::to_value(&archived).unwrap();
        let mut messages: Vec<_> =
            select_export_records(&mut records, Some(&archived), ExportInjections::default())
                .filter_map(|(index, raw)| messages_only_record(index, raw, archived.get(index)))
                .collect();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["source_index"], 1);
        assert_eq!(messages[0]["message_index"], 2);
        let mut privacy = ExportPrivacy::new(false);
        privacy.redact_messages(&mut messages);
        assert_eq!(messages[0]["content"], "Ordinary reply [REDACTED]");
        assert_eq!(privacy.metadata()["redaction_applied"], true);
        assert!(
            !serde_json::to_string(&messages)
                .unwrap()
                .contains("PRIVATE_")
        );
        assert_eq!(serde_json::to_value(&archived).unwrap(), archive_before);
    }

    #[test]
    fn messages_only_legacy_archive_retains_only_a_proven_text_prefix() {
        for suffix in [
            "[Tool: Read] PRIVATE_ARGUMENT\nPRIVATE_CONTINUATION\nlater prose cannot be distinguished",
            "[Tool: Read - PRIVATE_ARGUMENT\nPRIVATE_CONTINUATION]\nlater prose cannot be distinguished",
            "[Tool Output]\nPRIVATE_RESULT\nlater prose cannot be distinguished",
            "[Patch]\nPRIVATE_PATCH\nlater prose cannot be distinguished",
            "[Thinking] PRIVATE_REASONING\nlater prose cannot be distinguished",
            "[Reasoning] PRIVATE_REASONING\nlater prose cannot be distinguished",
        ] {
            let text = format!("Ordinary prefix café 界\n{suffix}");
            let raw = serde_json::json!({"role": "assistant", "content": text});
            let mut archived = Message {
                id: None,
                idx: 12,
                role: crate::model::types::MessageRole::Agent,
                author: None,
                created_at: None,
                content: text.clone(),
                extra_json: Value::Null,
                snippets: Vec::new(),
            };
            let projected =
                messages_only_record(12, &raw, Some(&archived)).expect("known prose prefix");
            assert_eq!(projected["content"], "Ordinary prefix café 界");
            assert_eq!(projected["content_projection"], "legacy-visible-prefix");
            assert_eq!(projected["message_index"], 13);
            assert_eq!(archived.content, text);

            let already_rendered =
                serde_json::json!({"role": "assistant", "content": "Ordinary prefix café 界"});
            let projected = messages_only_record(12, &already_rendered, Some(&archived))
                .expect("already rendered prefix");
            assert_eq!(projected["content"], "Ordinary prefix café 界");
            assert_eq!(projected["content_projection"], "legacy-visible-prefix");

            let tool_only = serde_json::json!({"role": "assistant", "content": suffix});
            archived.content = suffix.to_string();
            assert!(messages_only_record(12, &tool_only, Some(&archived)).is_none());

            // Original string envelopes have intact text boundaries, including
            // literal examples of tool markers; they must not be truncated.
            archived.content = text.clone();
            archived.extra_json = raw.clone();
            let exact =
                messages_only_record(12, &raw, Some(&archived)).expect("preserved source string");
            assert_eq!(exact["content"], text);
            assert!(exact.get("content_projection").is_none());
            let direct = messages_only_record(12, &raw, None).expect("direct source string");
            assert_eq!(direct["content"], text);
            assert!(direct.get("content_projection").is_none());

            let flattened = messages_only_flattened_record(12, &raw)
                .expect("explicitly flattened loader prefix");
            assert_eq!(flattened["content"], "Ordinary prefix café 界");
            assert_eq!(flattened["content_projection"], "legacy-visible-prefix");
            assert_eq!(flattened["source_index"], 12);
            assert!(flattened.get("message_index").is_none());
            assert!(messages_only_flattened_record(12, &tool_only).is_none());
        }
    }

    #[test]
    fn messages_only_omits_cline_sdk_archive_rows_without_recoverable_blocks() {
        for role in [
            crate::model::types::MessageRole::User,
            crate::model::types::MessageRole::Agent,
        ] {
            let role_name = crate::role_to_export_string(&role);
            let raw = serde_json::json!({
                "role": role_name, "content": "Ordinary prose\nPRIVATE_UNMARKED_CONTENT"
            });
            let compact = serde_json::json!({"cass": {
                "cline_sdk": true, "role": role_name, "tool_call_count": 0
            }});
            let mut archived = Message {
                id: None,
                idx: 9,
                role,
                author: None,
                created_at: None,
                content: "Ordinary prose\nPRIVATE_UNMARKED_CONTENT".to_string(),
                extra_json: compact.clone(),
                snippets: Vec::new(),
            };
            for extra in [
                compact.clone(),
                serde_json::json!({"__cass_historical_raw_json__": compact.to_string()}),
            ] {
                archived.extra_json = extra;
                assert!(messages_only_record(9, &raw, Some(&archived)).is_none());
            }

            // If an intact original envelope is available, it is authoritative
            // and can recover prose on both sides of otherwise hidden data.
            archived.extra_json = serde_json::json!({
                "role": role_name, "cass": {"cline_sdk": true}, "content": [
                    {"type": "text", "text": "Ordinary prose"},
                    {"type": "tool_result", "content": "PRIVATE_UNMARKED_CONTENT"},
                    {"type": "thinking", "thinking": "PRIVATE_REASONING"},
                    {"type": "text", "text": "Later prose"}
                ]
            });
            let recovered =
                messages_only_record(9, &raw, Some(&archived)).expect("original source blocks");
            assert_eq!(recovered["content"], "Ordinary prose\nLater prose");
            assert_eq!(recovered["message_index"], 10);
            assert!(!recovered.to_string().contains("PRIVATE_"));
            assert_eq!(archived.content, "Ordinary prose\nPRIVATE_UNMARKED_CONTENT");
        }
    }

    #[test]
    fn pi_tool_title_detection_keeps_visible_literal_text() {
        let session = serde_json::json!({"type": "session"});
        let tool = serde_json::json!({
            "type": "toolCall", "name": "Read", "arguments": {"path": "fixture"}
        });
        let title = "[Tool: Read] path=fixture";
        let export_messages = |blocks| {
            [
                session.clone(),
                serde_json::json!({"type": "message", "message": {"role": "user", "content": " "}}),
                serde_json::json!({
                    "type": "message",
                    "message": {"role": "assistant", "content": blocks}
                }),
            ]
        };
        let narrative = serde_json::json!({"type": "text", "text": "Ordinary narrative"});
        let hidden = export_messages([tool.clone(), narrative]);
        assert!(raw_title_has_hidden_pi_tools(title, "omp", &hidden));
        assert!(!raw_title_has_hidden_pi_tools(title, "codex", &hidden));
        assert!(!raw_title_has_hidden_pi_tools(
            "Custom title",
            "omp",
            &hidden
        ));

        let literal = serde_json::json!({"type": "text", "text": title});
        assert!(!raw_title_has_hidden_pi_tools(
            title,
            "pi_agent",
            &export_messages([literal, tool.clone()])
        ));

        let narrative = serde_json::json!({"type": "text", "text": "Ordinary narrative"});
        let mixed = export_messages([narrative.clone(), tool.clone()]);
        assert!(raw_title_has_hidden_pi_tools(
            "Ordinary narrative\n[Tool: Read] path=fixture",
            "pi_agent",
            &mixed
        ));
        assert!(raw_title_has_hidden_pi_tools(
            "Ordinary narrative\n[To...",
            "pi_agent",
            &mixed
        ));
        assert!(!raw_title_has_hidden_pi_tools(
            "Ordinary narrative...",
            "pi_agent",
            &mixed
        ));
        let literal = serde_json::json!({
            "type": "text", "text": "Ordinary narrative\n[Tool: a literal example]"
        });
        assert!(!raw_title_has_hidden_pi_tools(
            "Ordinary narrative\n[Tool: a literal example]",
            "pi_agent",
            &export_messages([literal, tool])
        ));

        let raw_title = "[Tool: Read] path=fixture password=synthetic-tool-title";
        let redacted_title = "[Tool: Read] path=fixture [REDACTED]";
        let tool = serde_json::json!({
            "type": "toolCall", "name": "Read",
            "arguments": {"path": "fixture password=synthetic-tool-title\nprivate continuation"}
        });
        let narrative = serde_json::json!({"type": "text", "text": "Ordinary narrative"});
        let hidden = export_messages([tool.clone(), narrative]);
        assert!(raw_title_has_hidden_pi_tools(
            redacted_title,
            "omp",
            &hidden
        ));
        assert!(raw_title_has_hidden_pi_tools(
            "[Tool: Read] path=fixture [REDACTED]\nprivate cont...",
            "omp",
            &hidden
        ));

        let literal = serde_json::json!({
            "type": "text",
            "text": "[Tool: Read] path=fixture password=synthetic-visible-example"
        });
        let visible = export_messages([tool.clone(), literal]);
        assert!(!raw_title_has_hidden_pi_tools(
            redacted_title,
            "omp",
            &visible
        ));
        assert!(raw_title_has_hidden_pi_tools(raw_title, "omp", &visible));
        assert!(raw_title_has_hidden_pi_tools(
            "[Tool: Read] path=fixture [REDACTED]\nprivate cont...",
            "omp",
            &visible
        ));
        let literal = serde_json::json!({
            "type": "text",
            "text": "\n \n[Tool: Read] path=fixture password=synthetic-visible-example\nprivate continuation"
        });
        assert!(!raw_title_has_hidden_pi_tools(
            "[Tool: Read] path=fixture [REDACTED]\nprivate cont...",
            "omp",
            &export_messages([tool, literal])
        ));
    }

    #[test]
    fn export_privacy_redacts_nested_credentials_and_preserves_message_identity() {
        let original = serde_json::json!({
            "role": "assistant",
            "message_index": 7,
            "content": "Keep café 界 and password=synthetic-secret-84 visible only with consent",
            "tool": {"name": "Read", "input": {"passphrase": "short", "path": "src/lib.rs"}},
            "metadata": {"authorization": "short", "answer": 42}
        });
        let mut messages = [original.clone()];
        let mut title = Some("Title password=synthetic-title-84".to_string());
        let mut privacy = ExportPrivacy::new(false);
        privacy.redact_messages(&mut messages);
        privacy.redact_optional_text(&mut title);

        assert_eq!(messages[0]["message_index"], 7);
        assert_eq!(messages[0]["role"], "assistant");
        assert_eq!(messages[0]["tool"]["name"], "Read");
        assert_eq!(messages[0]["tool"]["input"]["path"], "src/lib.rs");
        assert_eq!(messages[0]["tool"]["input"]["passphrase"], "[REDACTED]");
        assert_eq!(messages[0]["metadata"]["authorization"], "[REDACTED]");
        assert_eq!(messages[0]["metadata"]["answer"], 42);
        assert_eq!(
            messages[0]["content"],
            "Keep café 界 and [REDACTED] visible only with consent"
        );
        assert_eq!(title.as_deref(), Some("Title [REDACTED]"));
        assert_eq!(privacy.metadata()["redaction_applied"], true);
        assert_eq!(privacy.metadata()["sensitive_output"], false);
        assert_eq!(original["tool"]["input"]["passphrase"], "short");
    }

    #[test]
    fn export_privacy_preserves_benign_text_and_marks_explicit_raw_output() {
        let benign = serde_json::json!({"content": "Discuss password rotation; café 界", "idx": 3});
        let mut messages = [benign.clone()];
        let mut privacy = ExportPrivacy::new(false);
        privacy.redact_messages(&mut messages);
        assert_eq!(messages[0], benign);
        assert_eq!(privacy.metadata()["redaction_applied"], false);

        for original in [
            "Discuss password rotation; café 界",
            "  { \"note\": \"café 界\", \"token_count\": 42 }\n",
            "\"Discuss password rotation; café 界\"",
            "\"ordinary quoted prose without its closing quote",
        ] {
            let mut title = Some(original.to_owned());
            privacy.redact_optional_text(&mut title);
            assert_eq!(title.as_deref(), Some(original));
            assert_eq!(privacy.metadata()["redaction_applied"], false);
        }
        let mut absent = None;
        privacy.redact_optional_text(&mut absent);
        assert_eq!(absent, None);
        assert_eq!(privacy.metadata()["redaction_applied"], false);

        let raw = serde_json::json!({"content": "password=synthetic-explicit-84"});
        let mut messages = [raw.clone()];
        let mut title = Some("password=synthetic-title-84".to_string());
        let mut privacy = ExportPrivacy::new(true);
        privacy.redact_messages(&mut messages);
        privacy.redact_optional_text(&mut title);
        assert_eq!(messages[0], raw);
        assert_eq!(title.as_deref(), Some("password=synthetic-title-84"));
        assert_eq!(privacy.metadata()["policy"], "include-secrets");
        assert_eq!(privacy.metadata()["sensitive_output"], true);
        assert_eq!(privacy.metadata()["redaction_applied"], false);
    }

    #[test]
    fn export_privacy_redacts_json_titles_and_text_metadata() -> Result<(), serde_json::Error> {
        for (payload, expected) in [
            (
                serde_json::json!({"registration_token":"tiny-title-551", "note":"title-kept café 界", "token_count":42}),
                serde_json::json!({"registration_token":"[REDACTED]", "note":"title-kept café 界", "token_count":42}),
            ),
            (
                serde_json::json!([{"authorization":"tiny", "enabled":true}]),
                serde_json::json!([{"authorization":"[REDACTED]", "enabled":true}]),
            ),
        ] {
            for wrappers in 0..=2 {
                let mut original = payload.to_string();
                for _ in 0..wrappers {
                    original = serde_json::to_string(&original)?;
                }
                let mut title = Some(original.clone());
                let mut privacy = ExportPrivacy::new(false);
                privacy.redact_optional_text(&mut title);
                let mut decoded = title.expect("title remains text");
                for _ in 0..wrappers {
                    decoded = serde_json::from_str(&decoded)?;
                }
                assert_eq!(serde_json::from_str::<Value>(&decoded)?, expected);
                assert_eq!(privacy.metadata()["redaction_applied"], true);
                assert_eq!(privacy.metadata()["sensitive_output"], false);
                let mut clean_metadata = "metadata-kept café 界".to_owned();
                privacy.redact_text(&mut clean_metadata);
                assert_eq!(clean_metadata, "metadata-kept café 界");
                assert_eq!(privacy.metadata()["redaction_applied"], true);

                let mut raw_title = Some(original.clone());
                let mut raw_privacy = ExportPrivacy::new(true);
                raw_privacy.redact_optional_text(&mut raw_title);
                assert_eq!(raw_title.as_deref(), Some(original.as_str()));
                assert_eq!(raw_privacy.metadata()["redaction_applied"], false);
                assert_eq!(raw_privacy.metadata()["sensitive_output"], true);
            }
        }
        let mut truncated = r#"{"registration_token":"tiny-title-551""#.to_owned();
        let mut privacy = ExportPrivacy::new(false);
        privacy.redact_text(&mut truncated);
        assert_eq!(truncated, "[REDACTED]");
        assert_eq!(privacy.metadata()["redaction_applied"], true);
        Ok(())
    }

    #[test]
    fn skill_injection_policy_preserves_ordinary_discussion() {
        for marker in [
            "Base directory for this skill:",
            "<system-reminder>",
            "The following skills are available for use with the Skill tool:",
            "skillInjection: metadata matchedSkills",
            r#"{"skillInjection":{"matchedSkills":["review"]}}"#,
            r#"{"source_line":"{\"skillInjection\":{\"matchedSkills\":[\"review\"]}}"}"#,
            "<!-- skillInjection:",
        ] {
            assert!(is_skill_injection(&format!(
                "{}\n{marker}",
                "界".repeat(200)
            )));
        }
        for ordinary in [
            "",
            "Implement a new skill and test it.",
            "matchedSkills is the parser field name.",
            "skillInjection: is a label without matched entries.",
            "Discuss skillInjection and matchedSkills as separate field names.",
            "The skill tool should preserve ordinary discussion.",
        ] {
            assert!(!is_skill_injection(ordinary), "{ordinary}");
        }
    }

    fn sample_hit() -> SearchHit {
        SearchHit {
            title: "Test Result".to_string(),
            snippet: "This is a test snippet".to_string(),
            content: "Full content here".to_string(),
            content_hash: crate::search::query::stable_content_hash("Full content here"),
            conversation_id: None,
            score: 8.5,
            source_path: "/path/to/file.jsonl".to_string(),
            agent: "claude_code".to_string(),
            workspace: "/projects/test".to_string(),
            workspace_original: None,
            created_at: Some(1700000000000),
            line_number: Some(42),
            match_type: crate::search::query::MatchType::Exact,
            source_id: "local".to_string(),
            origin_kind: "local".to_string(),
            origin_host: None,
        }
    }

    fn assert_json_field(value: &serde_json::Value, key: &str, expected: serde_json::Value) {
        assert_eq!(
            value.get(key),
            Some(&expected),
            "unexpected JSON field `{key}` in {value}"
        );
    }

    #[test]
    fn test_export_format_cycle() {
        let format = ExportFormat::Markdown;
        assert_eq!(format.next(), ExportFormat::Json);
        assert_eq!(format.next().next(), ExportFormat::PlainText);
        assert_eq!(format.next().next().next(), ExportFormat::Markdown);
    }

    #[test]
    fn test_export_format_extension() {
        assert_eq!(ExportFormat::Markdown.extension(), "md");
        assert_eq!(ExportFormat::Json.extension(), "json");
        assert_eq!(ExportFormat::PlainText.extension(), "txt");
    }

    #[test]
    fn test_truncate_text() {
        assert_eq!(truncate_text("short", 100), "short");
        assert_eq!(truncate_text("this is long text", 10), "this is...");
        assert_eq!(truncate_text("any", 0), "any");

        // Edge case: very small max_len should not exceed limit
        assert_eq!(truncate_text("hello", 3), "hel"); // No ellipsis for max_len <= 3
        assert_eq!(truncate_text("hello", 2), "he");
        assert_eq!(truncate_text("hello", 1), "h");
        assert_eq!(truncate_text("hello", 4), "h..."); // max_len > 3 gets ellipsis
    }

    #[test]
    fn test_export_markdown() {
        let hits = vec![sample_hit()];
        let options = ExportOptions::default();
        let output = export_markdown(&hits, &options);

        assert!(output.contains("# Search Results"));
        assert!(output.contains("Test Result"));
        // underscores are escaped in markdown
        assert!(output.contains("claude\\_code"));
        assert!(output.contains("```"));
    }

    #[test]
    fn test_export_markdown_preserves_backticks_in_query() {
        let options = ExportOptions {
            query: Some("literal `foo` search".to_string()),
            ..ExportOptions::default()
        };
        let output = export_markdown(&[], &options);

        assert!(output.contains("**Query:** ``literal `foo` search``"));
        assert!(
            !output.contains("literal foo search"),
            "query backticks must not be stripped from Markdown export: {output}"
        );
    }

    #[test]
    fn test_export_json() {
        let hits = vec![sample_hit()];
        let options = ExportOptions::default();
        let output = export_json(&hits, &options);

        assert!(output.contains("\"count\": 1"));
        assert!(output.contains("\"agent\": \"claude_code\""));
    }

    #[test]
    fn test_export_json_value_shape() {
        let hits = vec![sample_hit()];
        let options = ExportOptions {
            query: Some("authentication error".to_string()),
            ..ExportOptions::default()
        };

        let projected = export_json_value(&hits, &options, "2026-04-26T17:26:00Z");

        assert_eq!(
            projected,
            serde_json::json!({
                "query": "authentication error",
                "count": 1,
                "exported_at": "2026-04-26T17:26:00Z",
                "hits": [{
                    "title": "Test Result",
                    "agent": "claude_code",
                    "workspace": "/projects/test",
                    "snippet": "This is a test snippet",
                    "score": 8.5,
                    "source_path": "/path/to/file.jsonl",
                    "line_number": 42,
                    "created_at": 1700000000000i64,
                    "created_at_formatted": "2023-11-14T22:13:20+00:00"
                }]
            })
        );
    }

    #[test]
    fn test_export_hit_json_shape() {
        let mut hit = sample_hit();
        hit.score = f32::NAN;
        let options = ExportOptions {
            include_content: true,
            include_score: true,
            include_path: true,
            max_snippet_len: 10,
            query: Some("ignored by hit projection".to_string()),
        };

        let projected = export_hit_json(&hit, &options);

        for (key, expected) in [
            ("title", serde_json::json!("Test Result")),
            ("agent", serde_json::json!("claude_code")),
            ("workspace", serde_json::json!("/projects/test")),
            ("snippet", serde_json::json!("This is...")),
            ("score", serde_json::json!(0.0)),
            ("source_path", serde_json::json!("/path/to/file.jsonl")),
            ("line_number", serde_json::json!(42)),
            ("created_at", serde_json::json!(1700000000000i64)),
            (
                "created_at_formatted",
                serde_json::json!("2023-11-14T22:13:20+00:00"),
            ),
            ("content", serde_json::json!("Full content here")),
        ] {
            assert_json_field(&projected, key, expected);
        }
        assert_eq!(projected.as_object().expect("object").len(), 10);
    }

    #[test]
    fn test_export_plain_text() {
        let hits = vec![sample_hit()];
        let options = ExportOptions::default();
        let output = export_plain_text(&hits, &options);

        assert!(output.contains("SEARCH RESULTS"));
        assert!(output.contains("[1] Test Result"));
        assert!(output.contains("Agent: claude_code"));
    }

    #[test]
    fn test_export_markdown_escapes_special_chars() {
        let mut hit = sample_hit();
        hit.title = "[Link](javascript:alert(1))".to_string();
        hit.agent = "agent|pipe".to_string();
        hit.content = "Contains ``` backticks".to_string();

        let options = ExportOptions {
            include_content: true,
            ..ExportOptions::default()
        };
        let output = export_markdown(&[hit], &options);

        assert!(output.contains("\\[Link\\](javascript:alert(1))"));
        assert!(output.contains("agent\\|pipe"));
        // Should use 4 backticks because content has 3
        assert!(output.contains("````\nContains ``` backticks"));
    }
}
