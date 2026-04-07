use std::path::Path;

use anyhow::{Error, Result};
use frankensearch::lexical::{
    CASS_SCHEMA_HASH, CassDocument as FsCassDocument, CassFields as FsCassFields,
    CassMergeStatus as FsCassMergeStatus, CassTantivyIndex as FsCassTantivyIndex, Index,
    IndexReader, Schema, cass_build_schema as fs_build_schema,
    cass_ensure_tokenizer as fs_ensure_tokenizer, cass_fields_from_schema as fs_fields_from_schema,
    cass_index_dir as fs_index_dir, cass_schema_hash_matches,
};

use crate::connectors::NormalizedConversation;
use crate::connectors::NormalizedMessage;
use crate::search::canonicalize::is_hard_message_noise;
use crate::sources::provenance::LOCAL_SOURCE_ID;

fn normalized_index_source_id(
    source_id: Option<&str>,
    origin_kind: Option<&str>,
    origin_host: Option<&str>,
) -> String {
    let trimmed_source_id = source_id.unwrap_or_default().trim();
    if !trimmed_source_id.is_empty() {
        if trimmed_source_id.eq_ignore_ascii_case(LOCAL_SOURCE_ID) {
            return LOCAL_SOURCE_ID.to_string();
        }
        return trimmed_source_id.to_string();
    }

    let trimmed_origin_kind = origin_kind.unwrap_or_default().trim();
    if trimmed_origin_kind.eq_ignore_ascii_case("ssh")
        || trimmed_origin_kind.eq_ignore_ascii_case("remote")
    {
        return origin_host
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("remote")
            .to_string();
    }

    LOCAL_SOURCE_ID.to_string()
}

fn normalized_index_origin_kind(source_id: &str, origin_kind: Option<&str>) -> String {
    if let Some(kind) = origin_kind.map(str::trim).filter(|value| !value.is_empty()) {
        if kind.eq_ignore_ascii_case("local") {
            return LOCAL_SOURCE_ID.to_string();
        }
        if kind.eq_ignore_ascii_case("ssh") || kind.eq_ignore_ascii_case("remote") {
            return "remote".to_string();
        }
        return kind.to_ascii_lowercase();
    }

    if source_id == LOCAL_SOURCE_ID {
        LOCAL_SOURCE_ID.to_string()
    } else {
        "remote".to_string()
    }
}

fn normalized_index_origin_host(origin_host: Option<&str>) -> Option<String> {
    origin_host
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

pub const SCHEMA_HASH: &str = CASS_SCHEMA_HASH;
const TANTIVY_ADD_BATCH_MAX_MESSAGES: usize = 512;
const TANTIVY_ADD_BATCH_MAX_CHARS: usize = 1024 * 1024;

fn map_fs_err(err: frankensearch::SearchError) -> Error {
    Error::new(err)
}

/// Returns true if the given stored hash matches the current schema hash.
pub fn schema_hash_matches(stored: &str) -> bool {
    cass_schema_hash_matches(stored)
}

pub type Fields = FsCassFields;
pub type MergeStatus = FsCassMergeStatus;

pub struct TantivyIndex {
    inner: FsCassTantivyIndex,
    pub fields: Fields,
}

impl TantivyIndex {
    pub fn open_or_create(path: &Path) -> Result<Self> {
        let inner = FsCassTantivyIndex::open_or_create(path).map_err(map_fs_err)?;
        let fields = inner.fields();
        Ok(Self { inner, fields })
    }

    pub fn add_conversation(&mut self, conv: &NormalizedConversation) -> Result<()> {
        self.add_messages(conv, &conv.messages)
    }

    pub fn add_conversation_with_id(
        &mut self,
        conv: &NormalizedConversation,
        conversation_id: Option<i64>,
    ) -> Result<()> {
        self.add_messages_with_conversation_id(conv, &conv.messages, conversation_id)
    }

    pub fn delete_all(&mut self) -> Result<()> {
        self.inner.delete_all().map_err(map_fs_err)
    }

    pub fn commit(&mut self) -> Result<()> {
        self.inner.commit().map_err(map_fs_err)
    }

    pub fn reader(&self) -> Result<IndexReader> {
        self.inner.reader().map_err(map_fs_err)
    }

    pub fn segment_count(&self) -> usize {
        self.inner.segment_count()
    }

    pub fn merge_status(&self) -> MergeStatus {
        self.inner.merge_status()
    }

    /// Attempt to merge segments if idle conditions are met.
    /// Returns Ok(true) if merge was triggered, Ok(false) if skipped.
    pub fn optimize_if_idle(&mut self) -> Result<bool> {
        self.inner.optimize_if_idle().map_err(map_fs_err)
    }

    /// Force immediate segment merge and wait for completion.
    /// Use sparingly - blocks until merge finishes.
    pub fn force_merge(&mut self) -> Result<()> {
        self.inner.force_merge().map_err(map_fs_err)
    }

    pub fn add_messages(
        &mut self,
        conv: &NormalizedConversation,
        messages: &[NormalizedMessage],
    ) -> Result<()> {
        self.add_messages_with_conversation_id(conv, messages, None)
    }

    pub fn add_messages_with_conversation_id(
        &mut self,
        conv: &NormalizedConversation,
        messages: &[NormalizedMessage],
        conversation_id: Option<i64>,
    ) -> Result<()> {
        // Provenance fields (P3.x): default to local, but honor metadata injected by indexer.
        let cass_origin = conv.metadata.get("cass").and_then(|c| c.get("origin"));
        let raw_source_id = cass_origin
            .and_then(|o| o.get("source_id"))
            .and_then(|v| v.as_str());
        let raw_origin_kind = cass_origin
            .and_then(|o| o.get("kind"))
            .and_then(|v| v.as_str());
        let origin_host = normalized_index_origin_host(
            cass_origin
                .and_then(|o| o.get("host"))
                .and_then(|v| v.as_str()),
        );
        let source_id =
            normalized_index_source_id(raw_source_id, raw_origin_kind, origin_host.as_deref());
        let origin_kind = normalized_index_origin_kind(&source_id, raw_origin_kind);

        let source_path = conv.source_path.to_string_lossy().to_string();
        let workspace = conv
            .workspace
            .as_ref()
            .map(|ws| ws.to_string_lossy().to_string());
        let workspace_original = conv
            .metadata
            .get("cass")
            .and_then(|c| c.get("workspace_original"))
            .and_then(|v| v.as_str())
            .map(ToOwned::to_owned);
        let title = conv.title.clone();
        let started_at_fallback = conv.started_at;

        let mut docs: Vec<FsCassDocument> = Vec::new();
        let mut pending_chars = 0usize;
        for msg in messages {
            if is_hard_message_noise(Some(msg.role.as_str()), &msg.content) {
                continue;
            }
            docs.push(FsCassDocument {
                agent: conv.agent_slug.clone(),
                workspace: workspace.clone(),
                workspace_original: workspace_original.clone(),
                source_path: source_path.clone(),
                msg_idx: msg.idx.max(0) as u64,
                created_at: msg.created_at.or(started_at_fallback),
                title: title.clone(),
                content: msg.content.clone(),
                conversation_id,
                source_id: source_id.clone(),
                origin_kind: origin_kind.clone(),
                origin_host: origin_host.clone(),
            });
            pending_chars = pending_chars.saturating_add(msg.content.len());

            if docs.len() >= TANTIVY_ADD_BATCH_MAX_MESSAGES
                || pending_chars >= TANTIVY_ADD_BATCH_MAX_CHARS
            {
                self.inner.add_cass_documents(&docs).map_err(map_fs_err)?;
                docs.clear();
                pending_chars = 0;
            }
        }

        if docs.is_empty() {
            Ok(())
        } else {
            self.inner.add_cass_documents(&docs).map_err(map_fs_err)
        }
    }
}

pub fn build_schema() -> Schema {
    fs_build_schema()
}

pub fn fields_from_schema(schema: &Schema) -> Result<Fields> {
    fs_fields_from_schema(schema).map_err(map_fs_err)
}

pub fn index_dir(base: &Path) -> Result<std::path::PathBuf> {
    fs_index_dir(base).map_err(map_fs_err)
}

pub fn ensure_tokenizer(index: &mut Index) {
    fs_ensure_tokenizer(index);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::{NormalizedConversation, NormalizedMessage};
    use serde_json::Value;
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[test]
    fn open_or_create_roundtrip() {
        let dir = TempDir::new().expect("temp dir");
        let idx = TantivyIndex::open_or_create(dir.path()).expect("create index");
        let reader = idx.reader().expect("reader");
        let searcher = reader.searcher();
        assert_eq!(searcher.num_docs(), 0);
    }

    #[test]
    fn schema_hash_matches_current_hash() {
        assert!(schema_hash_matches(SCHEMA_HASH));
        assert!(!schema_hash_matches("invalid"));
    }

    #[test]
    fn generate_edge_ngrams_prefixes() {
        let out = frankensearch::lexical::cass_generate_edge_ngrams("hello world");
        assert!(out.contains("he"));
        assert!(out.contains("world"));
    }

    #[test]
    fn build_preview_truncates_with_ellipsis() {
        let preview = frankensearch::lexical::cass_build_preview("abcdefghijklmnopqrstuvwxyz", 10);
        assert_eq!(preview, "abcdefghij…");
    }

    #[test]
    fn merge_status_api_is_exposed() {
        let dir = TempDir::new().expect("temp dir");
        let index = TantivyIndex::open_or_create(dir.path()).expect("create");
        let status = index.merge_status();
        assert_eq!(status.merge_threshold, 4);
    }

    #[test]
    fn merge_status_should_merge_logic() {
        let status = MergeStatus {
            segment_count: 5,
            last_merge_ts: 0,
            ms_since_last_merge: -1,
            merge_threshold: 4,
            cooldown_ms: 300_000,
        };
        assert!(status.should_merge());
    }

    #[test]
    fn index_dir_creates_versioned_path() {
        let dir = TempDir::new().expect("temp dir");
        let result = index_dir(dir.path()).expect("index dir");
        assert!(result.ends_with("index/v7"));
    }

    #[test]
    fn tokenizer_registration_is_callable() {
        let dir = TempDir::new().expect("temp dir");
        let mut idx = Index::create_in_ram(build_schema());
        ensure_tokenizer(&mut idx);
        let _ = TantivyIndex::open_or_create(dir.path()).expect("open or create");
    }

    #[test]
    fn add_messages_batches_large_payloads_without_dropping_docs() {
        let dir = TempDir::new().expect("temp dir");
        let mut idx = TantivyIndex::open_or_create(dir.path()).expect("create index");
        let content = "x".repeat(4096);
        let messages: Vec<_> = (0..1_200)
            .map(|i| NormalizedMessage {
                idx: i,
                role: "assistant".to_string(),
                author: None,
                created_at: Some(1_700_000_000_000 + i),
                content: format!("{i}-{content}"),
                extra: Value::Null,
                snippets: Vec::new(),
                invocations: Vec::new(),
            })
            .collect();
        let conv = NormalizedConversation {
            agent_slug: "codex".to_string(),
            external_id: Some("large-batch".to_string()),
            title: Some("Large Batch".to_string()),
            workspace: Some(PathBuf::from("/tmp/workspace")),
            source_path: PathBuf::from("/tmp/rollout.jsonl"),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_000_000_999),
            metadata: Value::Null,
            messages,
        };

        idx.add_messages(&conv, &conv.messages)
            .expect("add messages");
        idx.commit().expect("commit");

        let reader = idx.reader().expect("reader");
        reader.reload().expect("reload");
        let searcher = reader.searcher();
        assert_eq!(searcher.num_docs(), conv.messages.len() as u64);
    }
}
