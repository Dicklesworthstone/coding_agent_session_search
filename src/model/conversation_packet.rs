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
use std::path::Path;

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
