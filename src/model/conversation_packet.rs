//! Versioned normalize-once conversation packet contract.
//!
//! A `ConversationPacket` is the canonical unit that refresh and rebuild code
//! can hand to storage, lexical, analytics, and semantic sinks without asking
//! each sink to re-normalize the same conversation. The contract keeps the
//! owned canonical payload separate from lightweight sink projections so future
//! pipelines can pass indices, counts, and hashes instead of duplicating message
//! text in every derived structure.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{borrow::Cow, ops::Range, path::Path};

use crate::connectors::{NormalizedConversation, NormalizedMessage, NormalizedSnippet};
use crate::model::types::{Conversation, Message, MessageRole, Snippet};

pub const CONVERSATION_PACKET_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConversationPacketBuilder {
    RawConnectorScan,
    CanonicalReplay,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConversationPacketVersionStatus {
    Current,
    Mismatch { expected: u32, observed: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationPacketDiagnostics {
    pub builder: ConversationPacketBuilder,
    pub contract_version: u32,
    pub version_status: ConversationPacketVersionStatus,
    pub warnings: Vec<String>,
}

impl ConversationPacketDiagnostics {
    pub fn current(builder: ConversationPacketBuilder) -> Self {
        Self {
            builder,
            contract_version: CONVERSATION_PACKET_VERSION,
            version_status: ConversationPacketVersionStatus::Current,
            warnings: Vec::new(),
        }
    }

    pub fn version_mismatch(builder: ConversationPacketBuilder, observed: u32) -> Self {
        Self {
            builder,
            contract_version: CONVERSATION_PACKET_VERSION,
            version_status: ConversationPacketVersionStatus::Mismatch {
                expected: CONVERSATION_PACKET_VERSION,
                observed,
            },
            warnings: vec![format!(
                "conversation packet version mismatch: expected {}, observed {}",
                CONVERSATION_PACKET_VERSION, observed
            )],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationPacketProvenance {
    pub source_id: String,
    pub origin_kind: String,
    pub origin_host: Option<String>,
}

impl ConversationPacketProvenance {
    pub fn local() -> Self {
        Self {
            source_id: "local".to_string(),
            origin_kind: "local".to_string(),
            origin_host: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationPacketIdentity {
    pub conversation_id: Option<i64>,
    pub agent_slug: String,
    pub external_id: Option<String>,
    pub workspace: Option<String>,
    pub source_path: String,
    pub title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationPacketTimestamps {
    pub started_at: Option<i64>,
    pub ended_at: Option<i64>,
    pub first_message_at: Option<i64>,
    pub last_message_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationPacketSnippet {
    pub file_path: Option<String>,
    pub start_line: Option<i64>,
    pub end_line: Option<i64>,
    pub language: Option<String>,
    pub snippet_text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConversationPacketMessage {
    pub message_id: Option<i64>,
    pub idx: i64,
    pub role: String,
    pub author: Option<String>,
    pub created_at: Option<i64>,
    pub content: String,
    pub extra_json: Value,
    pub snippets: Vec<ConversationPacketSnippet>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConversationPacketPayload {
    pub identity: ConversationPacketIdentity,
    pub provenance: ConversationPacketProvenance,
    pub timestamps: ConversationPacketTimestamps,
    pub metadata_json: Value,
    pub messages: Vec<ConversationPacketMessage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationPacketHashes {
    /// Versioned BLAKE3 digest of identity, provenance, metadata, timestamps,
    /// normalized message roles, message content, extras, and snippets.
    /// Database row IDs are intentionally excluded so raw scans and canonical
    /// replay can prove semantic equivalence for the same logical conversation.
    pub semantic_hash: String,
    /// BLAKE3 digest of normalized message role/content/timestamp/snippet data.
    pub message_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationPacketLexicalProjection {
    pub message_indices: Vec<usize>,
    pub total_content_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationPacketSemanticProjection {
    pub message_indices: Vec<usize>,
    pub total_content_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationPacketAnalyticsProjection {
    pub user_messages: usize,
    pub assistant_messages: usize,
    pub tool_messages: usize,
    pub system_messages: usize,
    pub other_messages: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationPacketSinkProjections {
    pub lexical: ConversationPacketLexicalProjection,
    pub semantic: ConversationPacketSemanticProjection,
    pub analytics: ConversationPacketAnalyticsProjection,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConversationPacket {
    pub version: u32,
    pub diagnostics: ConversationPacketDiagnostics,
    pub payload: ConversationPacketPayload,
    pub hashes: ConversationPacketHashes,
    pub projections: ConversationPacketSinkProjections,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConversationPacketTextSink {
    Lexical,
    Semantic,
    Fingerprint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversationPacketTextBatchMode {
    Slab,
    OwnedFallback,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationPacketProjectionError {
    pub sink: ConversationPacketTextSink,
    pub message_index: usize,
    pub message_count: usize,
}

impl std::fmt::Display for ConversationPacketProjectionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{:?} packet projection references message index {} but packet has {} messages",
            self.sink, self.message_index, self.message_count
        )
    }
}

impl std::error::Error for ConversationPacketProjectionError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationPacketTextMessage<'a> {
    pub message_index: usize,
    pub message_id: Option<i64>,
    pub idx: i64,
    pub role: Cow<'a, str>,
    pub author: Option<Cow<'a, str>>,
    pub created_at: Option<i64>,
    pub content: Cow<'a, str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationPacketTextBatch<'a> {
    pub sink: ConversationPacketTextSink,
    pub mode: ConversationPacketTextBatchMode,
    pub total_content_bytes: usize,
    messages: Vec<ConversationPacketTextMessage<'a>>,
}

impl<'a> ConversationPacketTextBatch<'a> {
    pub fn messages(&self) -> &[ConversationPacketTextMessage<'a>] {
        &self.messages
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationPacketTextSlab {
    text: String,
    message_ranges: Vec<Range<usize>>,
}

impl ConversationPacketTextSlab {
    pub fn from_packet(packet: &ConversationPacket) -> Self {
        let mut text = String::with_capacity(packet_total_content_bytes(&packet.payload.messages));
        let mut message_ranges = Vec::with_capacity(packet.payload.messages.len());
        for message in &packet.payload.messages {
            let start = text.len();
            text.push_str(&message.content);
            let end = text.len();
            message_ranges.push(start..end);
        }
        Self {
            text,
            message_ranges,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn message_count(&self) -> usize {
        self.message_ranges.len()
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn message_content(&self, message_index: usize) -> Option<&str> {
        self.message_ranges
            .get(message_index)
            .and_then(|range| self.text.get(range.clone()))
    }

    pub fn message_range(&self, message_index: usize) -> Option<Range<usize>> {
        self.message_ranges.get(message_index).cloned()
    }

    pub fn sink_batch<'a>(
        &'a self,
        packet: &'a ConversationPacket,
        sink: ConversationPacketTextSink,
    ) -> Result<ConversationPacketTextBatch<'a>, ConversationPacketProjectionError> {
        let indices = packet.sink_message_indices(sink);
        let mut messages = Vec::with_capacity(indices.len());
        for &message_index in indices.iter() {
            let Some(message) = packet.payload.messages.get(message_index) else {
                return Err(ConversationPacketProjectionError {
                    sink,
                    message_index,
                    message_count: packet.payload.messages.len(),
                });
            };
            let Some(content) = self.message_content(message_index) else {
                return Err(ConversationPacketProjectionError {
                    sink,
                    message_index,
                    message_count: self.message_count(),
                });
            };
            messages.push(ConversationPacketTextMessage {
                message_index,
                message_id: message.message_id,
                idx: message.idx,
                role: Cow::Borrowed(message.role.as_str()),
                author: message.author.as_deref().map(Cow::Borrowed),
                created_at: message.created_at,
                content: Cow::Borrowed(content),
            });
        }
        Ok(ConversationPacketTextBatch {
            sink,
            mode: ConversationPacketTextBatchMode::Slab,
            total_content_bytes: packet.sink_total_content_bytes(sink),
            messages,
        })
    }
}

impl ConversationPacket {
    pub fn from_normalized_conversation(
        conversation: &NormalizedConversation,
        provenance: ConversationPacketProvenance,
    ) -> Self {
        let messages = conversation
            .messages
            .iter()
            .map(packet_message_from_normalized)
            .collect::<Vec<_>>();
        let payload = ConversationPacketPayload {
            identity: ConversationPacketIdentity {
                conversation_id: None,
                agent_slug: conversation.agent_slug.clone(),
                external_id: conversation.external_id.clone(),
                workspace: conversation.workspace.as_deref().map(path_to_packet_string),
                source_path: path_to_packet_string(&conversation.source_path),
                title: conversation.title.clone(),
            },
            provenance,
            timestamps: timestamps_from_parts(
                conversation.started_at,
                conversation.ended_at,
                &messages,
            ),
            metadata_json: conversation.metadata.clone(),
            messages,
        };
        Self::from_payload(payload, ConversationPacketBuilder::RawConnectorScan)
    }

    /// Cap cumulative lexical content at `cap` bytes for the incremental-inline
    /// Tantivy add path (#291 Gap A) — the analogue of the `--full` rebuild cap
    /// (`truncate_lexical_rebuild_conversation_content`). This packet exists
    /// solely to feed `TantivyIndex::add_messages_from_packet`, so it truncates
    /// message content (on a UTF-8 boundary; later messages cleared; message
    /// rows preserved so per-message accounting is unchanged) and re-derives
    /// hashes/projections from the capped payload to stay internally consistent.
    /// The canonical store is unaffected — it is persisted from the conversation
    /// separately, with full content. No-op when content is within the cap.
    #[must_use]
    pub fn capped_for_inline_lexical_index(mut self, cap: usize) -> Self {
        let original_bytes: usize = self
            .payload
            .messages
            .iter()
            .map(|message| message.content.len())
            .sum();
        if original_bytes <= cap {
            return self;
        }

        let mut used = 0usize;
        for message in &mut self.payload.messages {
            if used >= cap {
                message.content.clear();
                continue;
            }
            let remaining = cap - used;
            if message.content.len() <= remaining {
                used += message.content.len();
            } else {
                // Largest byte length <= remaining that ends on a UTF-8 boundary.
                let mut boundary = remaining;
                while boundary > 0 && !message.content.is_char_boundary(boundary) {
                    boundary -= 1;
                }
                message.content.truncate(boundary);
                used += boundary;
            }
        }

        let capped_bytes: usize = self
            .payload
            .messages
            .iter()
            .map(|message| message.content.len())
            .sum();
        tracing::warn!(
            diagnostic = "lexical_content_truncated",
            external_id = self.payload.identity.external_id.as_deref().unwrap_or(""),
            source_path = %self.payload.identity.source_path,
            original_bytes,
            capped_bytes,
            cap,
            "incremental-inline lexical packet content exceeded the per-conversation cap; truncated indexed text to stay within budget instead of OOM-quarantining (#291)"
        );

        // Re-derive hashes + projections from the capped payload so the packet
        // stays internally consistent while preserving which authoritative
        // builder supplied the payload.
        let builder = self.diagnostics.builder;
        Self::from_payload(self.payload, builder)
    }

    pub fn from_canonical_replay(
        conversation: &Conversation,
        provenance: ConversationPacketProvenance,
    ) -> Self {
        let messages = conversation
            .messages
            .iter()
            .map(packet_message_from_canonical)
            .collect::<Vec<_>>();
        let payload = ConversationPacketPayload {
            identity: ConversationPacketIdentity {
                conversation_id: conversation.id,
                agent_slug: conversation.agent_slug.clone(),
                external_id: conversation.external_id.clone(),
                workspace: conversation.workspace.as_deref().map(path_to_packet_string),
                source_path: path_to_packet_string(&conversation.source_path),
                title: conversation.title.clone(),
            },
            provenance,
            timestamps: timestamps_from_parts(
                conversation.started_at,
                conversation.ended_at,
                &messages,
            ),
            metadata_json: conversation.metadata_json.clone(),
            messages,
        };
        Self::from_payload(payload, ConversationPacketBuilder::CanonicalReplay)
    }

    pub fn semantically_equivalent_to(&self, other: &Self) -> bool {
        self.version == other.version
            && self.hashes == other.hashes
            && self.projections == other.projections
    }

    pub fn text_slab(&self) -> ConversationPacketTextSlab {
        ConversationPacketTextSlab::from_packet(self)
    }

    pub fn owned_text_batch_fallback(
        &self,
        sink: ConversationPacketTextSink,
    ) -> ConversationPacketTextBatch<'static> {
        let indices = fallback_sink_message_indices(sink, &self.payload.messages);
        let messages = indices
            .into_iter()
            .filter_map(|message_index| {
                let message = self.payload.messages.get(message_index)?;
                Some(ConversationPacketTextMessage {
                    message_index,
                    message_id: message.message_id,
                    idx: message.idx,
                    role: Cow::Owned(message.role.clone()),
                    author: message.author.clone().map(Cow::Owned),
                    created_at: message.created_at,
                    content: Cow::Owned(message.content.clone()),
                })
            })
            .collect();
        ConversationPacketTextBatch {
            sink,
            mode: ConversationPacketTextBatchMode::OwnedFallback,
            total_content_bytes: packet_total_content_bytes(&self.payload.messages),
            messages,
        }
    }

    fn from_payload(
        payload: ConversationPacketPayload,
        builder: ConversationPacketBuilder,
    ) -> Self {
        let hashes = packet_hashes(&payload);
        let projections = packet_projections(&payload.messages);
        Self {
            version: CONVERSATION_PACKET_VERSION,
            diagnostics: ConversationPacketDiagnostics::current(builder),
            payload,
            hashes,
            projections,
        }
    }

    fn sink_message_indices(&self, sink: ConversationPacketTextSink) -> Cow<'_, [usize]> {
        match sink {
            ConversationPacketTextSink::Lexical => {
                Cow::Borrowed(&self.projections.lexical.message_indices)
            }
            ConversationPacketTextSink::Semantic => {
                Cow::Borrowed(&self.projections.semantic.message_indices)
            }
            ConversationPacketTextSink::Fingerprint => {
                Cow::Owned((0..self.payload.messages.len()).collect())
            }
        }
    }

    fn sink_total_content_bytes(&self, sink: ConversationPacketTextSink) -> usize {
        match sink {
            ConversationPacketTextSink::Lexical => self.projections.lexical.total_content_bytes,
            ConversationPacketTextSink::Semantic => self.projections.semantic.total_content_bytes,
            ConversationPacketTextSink::Fingerprint => {
                packet_total_content_bytes(&self.payload.messages)
            }
        }
    }
}

fn fallback_sink_message_indices(
    sink: ConversationPacketTextSink,
    messages: &[ConversationPacketMessage],
) -> Vec<usize> {
    match sink {
        ConversationPacketTextSink::Lexical | ConversationPacketTextSink::Semantic => messages
            .iter()
            .enumerate()
            .filter(|(_, message)| !message.content.is_empty())
            .map(|(idx, _)| idx)
            .collect(),
        ConversationPacketTextSink::Fingerprint => (0..messages.len()).collect(),
    }
}

fn packet_total_content_bytes(messages: &[ConversationPacketMessage]) -> usize {
    messages
        .iter()
        .map(|message| message.content.len())
        .sum::<usize>()
}

fn path_to_packet_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn normalize_role(role: &str) -> String {
    match role.trim().to_ascii_lowercase().as_str() {
        "agent" | "assistant" => "assistant".to_string(),
        "user" => "user".to_string(),
        "tool" => "tool".to_string(),
        "system" => "system".to_string(),
        other => other.to_string(),
    }
}

fn canonical_role(role: &MessageRole) -> String {
    match role {
        MessageRole::User => "user".to_string(),
        MessageRole::Agent => "assistant".to_string(),
        MessageRole::Tool => "tool".to_string(),
        MessageRole::System => "system".to_string(),
        MessageRole::Other(other) => normalize_role(other),
    }
}

fn packet_message_from_normalized(message: &NormalizedMessage) -> ConversationPacketMessage {
    ConversationPacketMessage {
        message_id: None,
        idx: message.idx,
        role: normalize_role(&message.role),
        author: message.author.clone(),
        created_at: message.created_at,
        content: message.content.clone(),
        extra_json: message.extra.clone(),
        snippets: message
            .snippets
            .iter()
            .map(packet_snippet_from_normalized)
            .collect(),
    }
}

fn packet_message_from_canonical(message: &Message) -> ConversationPacketMessage {
    ConversationPacketMessage {
        message_id: message.id,
        idx: message.idx,
        role: canonical_role(&message.role),
        author: message.author.clone(),
        created_at: message.created_at,
        content: message.content.clone(),
        extra_json: message.extra_json.clone(),
        snippets: message
            .snippets
            .iter()
            .map(packet_snippet_from_canonical)
            .collect(),
    }
}

fn packet_snippet_from_normalized(snippet: &NormalizedSnippet) -> ConversationPacketSnippet {
    ConversationPacketSnippet {
        file_path: snippet.file_path.as_deref().map(path_to_packet_string),
        start_line: snippet.start_line,
        end_line: snippet.end_line,
        language: snippet.language.clone(),
        snippet_text: snippet.snippet_text.clone(),
    }
}

fn packet_snippet_from_canonical(snippet: &Snippet) -> ConversationPacketSnippet {
    ConversationPacketSnippet {
        file_path: snippet.file_path.as_deref().map(path_to_packet_string),
        start_line: snippet.start_line,
        end_line: snippet.end_line,
        language: snippet.language.clone(),
        snippet_text: snippet.snippet_text.clone(),
    }
}

fn timestamps_from_parts(
    started_at: Option<i64>,
    ended_at: Option<i64>,
    messages: &[ConversationPacketMessage],
) -> ConversationPacketTimestamps {
    let first_message_at = messages
        .iter()
        .filter_map(|message| message.created_at)
        .min();
    let last_message_at = messages
        .iter()
        .filter_map(|message| message.created_at)
        .max();
    ConversationPacketTimestamps {
        started_at,
        ended_at,
        first_message_at,
        last_message_at,
    }
}

fn packet_projections(messages: &[ConversationPacketMessage]) -> ConversationPacketSinkProjections {
    let message_indices = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| !message.content.is_empty())
        .map(|(idx, _)| idx)
        .collect::<Vec<_>>();
    let total_content_bytes = messages
        .iter()
        .map(|message| message.content.len())
        .sum::<usize>();
    let mut analytics = ConversationPacketAnalyticsProjection {
        user_messages: 0,
        assistant_messages: 0,
        tool_messages: 0,
        system_messages: 0,
        other_messages: 0,
    };
    for message in messages {
        match message.role.as_str() {
            "user" => analytics.user_messages += 1,
            "assistant" => analytics.assistant_messages += 1,
            "tool" => analytics.tool_messages += 1,
            "system" => analytics.system_messages += 1,
            _ => analytics.other_messages += 1,
        }
    }
    ConversationPacketSinkProjections {
        lexical: ConversationPacketLexicalProjection {
            message_indices: message_indices.clone(),
            total_content_bytes,
        },
        semantic: ConversationPacketSemanticProjection {
            message_indices,
            total_content_bytes,
        },
        analytics,
    }
}

fn packet_hashes(payload: &ConversationPacketPayload) -> ConversationPacketHashes {
    let mut semantic = blake3::Hasher::new();
    update_u32(&mut semantic, "version", CONVERSATION_PACKET_VERSION);
    update_identity_hash(&mut semantic, &payload.identity);
    update_provenance_hash(&mut semantic, &payload.provenance);
    update_timestamps_hash(&mut semantic, &payload.timestamps);
    update_json(&mut semantic, "metadata_json", &payload.metadata_json);
    update_messages_hash(&mut semantic, &payload.messages);

    let mut messages = blake3::Hasher::new();
    update_u32(&mut messages, "version", CONVERSATION_PACKET_VERSION);
    update_messages_hash(&mut messages, &payload.messages);

    ConversationPacketHashes {
        semantic_hash: semantic.finalize().to_hex().to_string(),
        message_hash: messages.finalize().to_hex().to_string(),
    }
}

fn update_identity_hash(hasher: &mut blake3::Hasher, identity: &ConversationPacketIdentity) {
    update_str(hasher, "agent_slug", &identity.agent_slug);
    update_opt_str(hasher, "external_id", identity.external_id.as_deref());
    update_opt_str(hasher, "workspace", identity.workspace.as_deref());
    update_str(hasher, "source_path", &identity.source_path);
    update_opt_str(hasher, "title", identity.title.as_deref());
}

fn update_provenance_hash(hasher: &mut blake3::Hasher, provenance: &ConversationPacketProvenance) {
    update_str(hasher, "source_id", &provenance.source_id);
    update_str(hasher, "origin_kind", &provenance.origin_kind);
    update_opt_str(hasher, "origin_host", provenance.origin_host.as_deref());
}

fn update_timestamps_hash(hasher: &mut blake3::Hasher, timestamps: &ConversationPacketTimestamps) {
    update_opt_i64(hasher, "started_at", timestamps.started_at);
    update_opt_i64(hasher, "ended_at", timestamps.ended_at);
    update_opt_i64(hasher, "first_message_at", timestamps.first_message_at);
    update_opt_i64(hasher, "last_message_at", timestamps.last_message_at);
}

fn update_messages_hash(hasher: &mut blake3::Hasher, messages: &[ConversationPacketMessage]) {
    update_usize(hasher, "message_count", messages.len());
    for message in messages {
        update_i64(hasher, "message_idx", message.idx);
        update_str(hasher, "message_role", &message.role);
        update_opt_str(hasher, "message_author", message.author.as_deref());
        update_opt_i64(hasher, "message_created_at", message.created_at);
        update_str(hasher, "message_content", &message.content);
        update_json(hasher, "message_extra_json", &message.extra_json);
        update_usize(hasher, "snippet_count", message.snippets.len());
        for snippet in &message.snippets {
            update_opt_str(hasher, "snippet_file_path", snippet.file_path.as_deref());
            update_opt_i64(hasher, "snippet_start_line", snippet.start_line);
            update_opt_i64(hasher, "snippet_end_line", snippet.end_line);
            update_opt_str(hasher, "snippet_language", snippet.language.as_deref());
            update_opt_str(hasher, "snippet_text", snippet.snippet_text.as_deref());
        }
    }
}

fn update_label(hasher: &mut blake3::Hasher, label: &str) {
    hasher.update(label.as_bytes());
    hasher.update(&[0]);
}

fn update_str(hasher: &mut blake3::Hasher, label: &str, value: &str) {
    update_label(hasher, label);
    update_usize(hasher, "len", value.len());
    hasher.update(value.as_bytes());
}

fn update_opt_str(hasher: &mut blake3::Hasher, label: &str, value: Option<&str>) {
    match value {
        Some(value) => {
            update_label(hasher, label);
            hasher.update(&[1]);
            update_usize(hasher, "len", value.len());
            hasher.update(value.as_bytes());
        }
        None => {
            update_label(hasher, label);
            hasher.update(&[0]);
        }
    }
}

fn update_json(hasher: &mut blake3::Hasher, label: &str, value: &Value) {
    let stable = serde_json::to_string(value).unwrap_or_else(|_| "null".to_string());
    update_str(hasher, label, &stable);
}

fn update_i64(hasher: &mut blake3::Hasher, label: &str, value: i64) {
    update_label(hasher, label);
    hasher.update(&value.to_le_bytes());
}

fn update_opt_i64(hasher: &mut blake3::Hasher, label: &str, value: Option<i64>) {
    update_label(hasher, label);
    match value {
        Some(value) => {
            hasher.update(&[1]);
            hasher.update(&value.to_le_bytes());
        }
        None => {
            hasher.update(&[0]);
        }
    }
}

fn update_u32(hasher: &mut blake3::Hasher, label: &str, value: u32) {
    update_label(hasher, label);
    hasher.update(&value.to_le_bytes());
}

fn update_usize(hasher: &mut blake3::Hasher, label: &str, value: usize) {
    update_label(hasher, label);
    let value = u64::try_from(value).unwrap_or(u64::MAX);
    hasher.update(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::{NormalizedConversation, NormalizedMessage, NormalizedSnippet};
    use crate::model::types::{Conversation, Message, MessageRole, Snippet};
    use serde_json::json;
    use std::path::PathBuf;

    fn raw_conversation() -> NormalizedConversation {
        NormalizedConversation {
            agent_slug: "codex".to_string(),
            external_id: Some("session-1".to_string()),
            title: Some("Packet contract".to_string()),
            workspace: Some(PathBuf::from("/work/cass")),
            source_path: PathBuf::from("/work/cass/.codex/session.jsonl"),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_000_010_000),
            metadata: json!({"model": "gpt-5", "temperature": 0}),
            messages: vec![
                NormalizedMessage {
                    idx: 0,
                    role: "user".to_string(),
                    author: Some("human".to_string()),
                    created_at: Some(1_700_000_000_000),
                    content: "build the packet".to_string(),
                    extra: json!({"turn": 1}),
                    snippets: vec![NormalizedSnippet {
                        file_path: Some(PathBuf::from("src/main.rs")),
                        start_line: Some(10),
                        end_line: Some(12),
                        language: Some("rust".to_string()),
                        snippet_text: Some("fn main() {}".to_string()),
                    }],
                    invocations: Vec::new(),
                },
                NormalizedMessage {
                    idx: 1,
                    role: "assistant".to_string(),
                    author: None,
                    created_at: Some(1_700_000_001_000),
                    content: "packet built".to_string(),
                    extra: json!({}),
                    snippets: Vec::new(),
                    invocations: Vec::new(),
                },
            ],
        }
    }

    #[test]
    fn capped_for_inline_lexical_index_truncates_and_preserves_rows() {
        // #291 Gap A: two 600-byte messages (1200 total) capped to 1000.
        let mut conv = raw_conversation();
        conv.messages[0].content = "a".repeat(600);
        conv.messages[1].content = "b".repeat(600);
        let packet = ConversationPacket::from_normalized_conversation(
            &conv,
            ConversationPacketProvenance::local(),
        )
        .capped_for_inline_lexical_index(1000);

        // Message rows preserved (structure unchanged; only indexed text dropped).
        assert_eq!(packet.payload.messages.len(), 2);
        // First message kept whole (600), second truncated to the remaining 400.
        assert_eq!(packet.payload.messages[0].content.len(), 600);
        assert_eq!(packet.payload.messages[1].content.len(), 400);
        let total: usize = packet
            .payload
            .messages
            .iter()
            .map(|m| m.content.len())
            .sum();
        assert!(total <= 1000, "cumulative content {total} exceeds cap");
        // Projections were re-derived from the capped payload (internally consistent).
        assert!(packet.projections.lexical.total_content_bytes <= 1000);
        assert_eq!(
            packet.diagnostics.builder,
            ConversationPacketBuilder::RawConnectorScan
        );
    }

    #[test]
    fn capped_for_inline_lexical_index_is_noop_within_budget() {
        let conv = raw_conversation();
        let baseline = ConversationPacket::from_normalized_conversation(
            &conv,
            ConversationPacketProvenance::local(),
        );
        let capped = baseline
            .clone()
            .capped_for_inline_lexical_index(8 * 1024 * 1024);
        assert_eq!(baseline, capped, "cap within budget must be a no-op");
    }

    fn canonical_conversation() -> Conversation {
        Conversation {
            id: Some(42),
            agent_slug: "codex".to_string(),
            workspace: Some(PathBuf::from("/work/cass")),
            external_id: Some("session-1".to_string()),
            title: Some("Packet contract".to_string()),
            source_path: PathBuf::from("/work/cass/.codex/session.jsonl"),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_000_010_000),
            approx_tokens: None,
            metadata_json: json!({"model": "gpt-5", "temperature": 0}),
            source_id: "local".to_string(),
            origin_host: None,
            messages: vec![
                Message {
                    id: Some(100),
                    idx: 0,
                    role: MessageRole::User,
                    author: Some("human".to_string()),
                    created_at: Some(1_700_000_000_000),
                    content: "build the packet".to_string(),
                    extra_json: json!({"turn": 1}),
                    snippets: vec![Snippet {
                        id: Some(7),
                        file_path: Some(PathBuf::from("src/main.rs")),
                        start_line: Some(10),
                        end_line: Some(12),
                        language: Some("rust".to_string()),
                        snippet_text: Some("fn main() {}".to_string()),
                    }],
                },
                Message {
                    id: Some(101),
                    idx: 1,
                    role: MessageRole::Agent,
                    author: None,
                    created_at: Some(1_700_000_001_000),
                    content: "packet built".to_string(),
                    extra_json: json!({}),
                    snippets: Vec::new(),
                },
            ],
        }
    }

    #[test]
    fn capped_canonical_packet_preserves_builder_diagnostics() {
        let mut conv = canonical_conversation();
        conv.messages[0].content = "a".repeat(600);
        conv.messages[1].content = "b".repeat(600);

        let packet =
            ConversationPacket::from_canonical_replay(&conv, ConversationPacketProvenance::local())
                .capped_for_inline_lexical_index(1000);

        assert_eq!(packet.payload.messages[0].content.len(), 600);
        assert_eq!(packet.payload.messages[1].content.len(), 400);
        assert_eq!(
            packet.diagnostics.builder,
            ConversationPacketBuilder::CanonicalReplay
        );
    }

    #[test]
    fn raw_and_canonical_builders_produce_equivalent_packet_semantics() {
        let provenance = ConversationPacketProvenance::local();
        let raw = ConversationPacket::from_normalized_conversation(
            &raw_conversation(),
            provenance.clone(),
        );
        let canonical =
            ConversationPacket::from_canonical_replay(&canonical_conversation(), provenance);

        assert_eq!(raw.version, CONVERSATION_PACKET_VERSION);
        assert!(raw.semantically_equivalent_to(&canonical));
        assert_eq!(raw.payload.messages[1].role, "assistant");
        assert_eq!(canonical.payload.messages[1].role, "assistant");
        assert_eq!(raw.projections.lexical.message_indices, vec![0, 1]);
        assert_eq!(raw.projections.analytics.user_messages, 1);
        assert_eq!(raw.projections.analytics.assistant_messages, 1);
    }

    #[test]
    fn packet_hash_changes_when_normalized_content_changes() {
        let mut changed = raw_conversation();
        changed.messages[1].content = "packet changed".to_string();

        let original = ConversationPacket::from_normalized_conversation(
            &raw_conversation(),
            ConversationPacketProvenance::local(),
        );
        let changed = ConversationPacket::from_normalized_conversation(
            &changed,
            ConversationPacketProvenance::local(),
        );

        assert_ne!(original.hashes.semantic_hash, changed.hashes.semantic_hash);
        assert_ne!(original.hashes.message_hash, changed.hashes.message_hash);
    }

    #[test]
    fn text_slab_reuses_one_utf8_arena_for_packet_sinks() {
        let mut canonical = canonical_conversation();
        canonical.messages[0].content = format!("build {} packet", "\u{2603}");
        canonical.messages.push(Message {
            id: Some(102),
            idx: 2,
            role: MessageRole::System,
            author: None,
            created_at: Some(1_700_000_002_000),
            content: String::new(),
            extra_json: json!({}),
            snippets: Vec::new(),
        });
        let packet = ConversationPacket::from_canonical_replay(
            &canonical,
            ConversationPacketProvenance::local(),
        );
        let slab = packet.text_slab();

        assert_eq!(slab.message_count(), 3);
        let range = slab
            .message_range(0)
            .expect("first message should have a slab range");
        assert!(slab.text().is_char_boundary(range.start));
        assert!(slab.text().is_char_boundary(range.end));
        assert_eq!(
            slab.message_content(0),
            Some(packet.payload.messages[0].content.as_str())
        );

        let lexical = slab
            .sink_batch(&packet, ConversationPacketTextSink::Lexical)
            .expect("lexical projection should borrow from the slab");
        let semantic = slab
            .sink_batch(&packet, ConversationPacketTextSink::Semantic)
            .expect("semantic projection should borrow from the slab");
        let fingerprint = slab
            .sink_batch(&packet, ConversationPacketTextSink::Fingerprint)
            .expect("fingerprint projection should cover all messages");

        assert_eq!(lexical.mode, ConversationPacketTextBatchMode::Slab);
        assert_eq!(lexical.len(), 2, "empty content stays out of lexical");
        assert_eq!(semantic.len(), 2, "empty content stays out of semantic");
        assert_eq!(fingerprint.len(), 3, "fingerprint sees every message");
        assert!(fingerprint.messages()[2].content.is_empty());

        let slab_content = slab
            .message_content(0)
            .expect("first message should be readable from the slab");
        assert!(std::ptr::eq(
            lexical.messages()[0].content.as_ref().as_ptr(),
            slab_content.as_ptr()
        ));
        assert!(std::ptr::eq(
            semantic.messages()[0].content.as_ref().as_ptr(),
            slab_content.as_ptr()
        ));
        assert_eq!(
            lexical.messages()[0].content.as_ref(),
            "build \u{2603} packet"
        );
    }

    #[test]
    fn owned_text_batch_fallback_recovers_from_bad_projection() {
        let mut packet = ConversationPacket::from_canonical_replay(
            &canonical_conversation(),
            ConversationPacketProvenance::local(),
        );
        packet.projections.semantic.message_indices = vec![0, 99];
        let slab = packet.text_slab();
        let err = slab
            .sink_batch(&packet, ConversationPacketTextSink::Semantic)
            .expect_err("bad projection should not build a slab view");

        assert_eq!(err.sink, ConversationPacketTextSink::Semantic);
        assert_eq!(err.message_index, 99);
        assert_eq!(err.message_count, packet.payload.messages.len());

        let fallback = packet.owned_text_batch_fallback(ConversationPacketTextSink::Semantic);
        assert_eq!(
            fallback.mode,
            ConversationPacketTextBatchMode::OwnedFallback
        );
        assert_eq!(fallback.len(), 2);
        assert!(
            matches!(fallback.messages()[0].content, Cow::Owned(_)),
            "fallback should own content instead of borrowing from the slab"
        );
        assert_eq!(fallback.messages()[0].content.as_ref(), "build the packet");
    }

    #[test]
    fn version_mismatch_diagnostic_is_explicit() {
        let diagnostic = ConversationPacketDiagnostics::version_mismatch(
            ConversationPacketBuilder::CanonicalReplay,
            0,
        );

        assert_eq!(
            diagnostic.version_status,
            ConversationPacketVersionStatus::Mismatch {
                expected: CONVERSATION_PACKET_VERSION,
                observed: 0,
            }
        );
        assert!(
            diagnostic.warnings[0].contains("conversation packet version mismatch"),
            "diagnostic should explain packet version mismatch"
        );
    }
}
