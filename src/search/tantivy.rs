use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow};
use arrayvec::ArrayVec;
use tantivy::schema::{
    FAST, Field, INDEXED, IndexRecordOption, STORED, STRING, Schema, TEXT, TextFieldIndexing,
    TextOptions,
};
use tantivy::{Index, IndexReader, IndexWriter, doc};
use tracing::{debug, info, warn};

use crate::connectors::NormalizedConversation;
use crate::sources::provenance::LOCAL_SOURCE_ID;

const SCHEMA_VERSION: &str = "v6";

/// Minimum time (ms) between merge operations
const MERGE_COOLDOWN_MS: i64 = 300_000; // 5 minutes

/// Segment count threshold above which merge is triggered
const MERGE_SEGMENT_THRESHOLD: usize = 4;

/// Global last merge timestamp (ms since epoch)
static LAST_MERGE_TS: AtomicI64 = AtomicI64::new(0);

/// Debug status for segment merge operations
#[derive(Debug, Clone)]
pub struct MergeStatus {
    /// Current number of searchable segments
    pub segment_count: usize,
    /// Timestamp of last merge (ms since epoch), 0 if never
    pub last_merge_ts: i64,
    /// Milliseconds since last merge, -1 if never merged
    pub ms_since_last_merge: i64,
    /// Segment count threshold for auto-merge
    pub merge_threshold: usize,
    /// Cooldown period between merges (ms)
    pub cooldown_ms: i64,
}

impl MergeStatus {
    /// Returns true if merge is recommended based on current status
    pub fn should_merge(&self) -> bool {
        self.segment_count >= self.merge_threshold
            && (self.ms_since_last_merge < 0 || self.ms_since_last_merge >= self.cooldown_ms)
    }
}

// Bump this when schema/tokenizer changes. Used to trigger rebuilds.
pub const SCHEMA_HASH: &str = "tantivy-schema-v6-provenance-indexed";

/// Returns true if the given stored hash matches the current schema hash.
pub fn schema_hash_matches(stored: &str) -> bool {
    stored == SCHEMA_HASH
}

#[derive(Clone, Copy)]
pub struct Fields {
    pub agent: Field,
    pub workspace: Field,
    pub workspace_original: Field,
    pub source_path: Field,
    pub msg_idx: Field,
    pub created_at: Field,
    pub title: Field,
    pub content: Field,
    pub title_prefix: Field,
    pub content_prefix: Field,
    pub preview: Field,
    // Provenance fields (P1.4)
    pub source_id: Field,
    pub origin_kind: Field,
    pub origin_host: Field,
}

pub struct TantivyIndex {
    pub index: Index,
    writer: IndexWriter,
    pub fields: Fields,
}

impl TantivyIndex {
    pub fn open_or_create(path: &Path) -> Result<Self> {
        // Schema we will use if we need to (re)create the index.
        let schema = build_schema();
        std::fs::create_dir_all(path)?;

        let meta_path = path.join("schema_hash.json");
        let mut needs_rebuild = true;
        if meta_path.exists()
            && let Ok(meta) = std::fs::read_to_string(&meta_path)
            && let Ok(json) = serde_json::from_str::<serde_json::Value>(&meta)
            && json.get("schema_hash").and_then(|v| v.as_str()) == Some(SCHEMA_HASH)
        {
            needs_rebuild = false;
        }

        if needs_rebuild {
            // Recreate index directory completely to avoid stale lock files or
            // stale tantivy internals.
            let _ = std::fs::remove_dir_all(path);
            std::fs::create_dir_all(path)?;
        }

        let mut index = if path.join("meta.json").exists() && !needs_rebuild {
            // We believe the schema hash matches; try to open. If this fails
            // (e.g. corrupted meta.json / index), fall back to a clean rebuild.
            match Index::open_in_dir(path) {
                Ok(idx) => idx,
                Err(e) => {
                    warn!(
                        error = %e,
                        "Failed to open existing index; rebuilding from scratch"
                    );
                    let _ = std::fs::remove_dir_all(path);
                    std::fs::create_dir_all(path)?;
                    Index::create_in_dir(path, schema.clone())?
                }
            }
        } else {
            Index::create_in_dir(path, schema.clone())?
        };

        ensure_tokenizer(&mut index);

        // Always write the current schema hash so future runs can detect mismatches.
        std::fs::write(&meta_path, format!("{{\"schema_hash\":\"{SCHEMA_HASH}\"}}"))?;

        // Use the schema actually attached to this index to derive field ids.
        // This avoids subtle field-id mismatches if the on-disk index was created
        // by a slightly different binary.
        let actual_schema = index.schema();
        let writer = index
            .writer(50_000_000)
            .map_err(|e| anyhow!("create index writer: {e:?}"))?;
        let fields = fields_from_schema(&actual_schema)?;
        Ok(Self {
            index,
            writer,
            fields,
        })
    }

    pub fn add_conversation(&mut self, conv: &NormalizedConversation) -> Result<()> {
        self.add_messages(conv, &conv.messages)
    }

    pub fn delete_all(&mut self) -> Result<()> {
        self.writer.delete_all_documents()?;
        Ok(())
    }

    pub fn commit(&mut self) -> Result<()> {
        self.writer.commit()?;
        Ok(())
    }

    pub fn reader(&self) -> Result<IndexReader> {
        Ok(self.index.reader()?)
    }

    /// Get current number of searchable segments
    pub fn segment_count(&self) -> usize {
        self.index
            .searchable_segment_ids()
            .map(|ids| ids.len())
            .unwrap_or(0)
    }

    /// Returns debug info about merge status
    pub fn merge_status(&self) -> MergeStatus {
        let last_merge_ts = LAST_MERGE_TS.load(Ordering::Relaxed);
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let ms_since_last = if last_merge_ts > 0 {
            now_ms - last_merge_ts
        } else {
            -1 // never merged
        };
        MergeStatus {
            segment_count: self.segment_count(),
            last_merge_ts,
            ms_since_last_merge: ms_since_last,
            merge_threshold: MERGE_SEGMENT_THRESHOLD,
            cooldown_ms: MERGE_COOLDOWN_MS,
        }
    }

    /// Attempt to merge segments if idle conditions are met.
    /// Returns Ok(true) if merge was triggered, Ok(false) if skipped.
    /// Merge runs in background thread - this call is non-blocking.
    pub fn optimize_if_idle(&mut self) -> Result<bool> {
        let segment_ids = self.index.searchable_segment_ids()?;
        let segment_count = segment_ids.len();

        // Check if we have enough segments to warrant a merge
        if segment_count < MERGE_SEGMENT_THRESHOLD {
            debug!(
                segments = segment_count,
                threshold = MERGE_SEGMENT_THRESHOLD,
                "Skipping merge: segment count below threshold"
            );
            return Ok(false);
        }

        // Check cooldown period
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let last_merge = LAST_MERGE_TS.load(Ordering::Relaxed);
        if last_merge > 0 && (now_ms - last_merge) < MERGE_COOLDOWN_MS {
            debug!(
                ms_since_last = now_ms - last_merge,
                cooldown = MERGE_COOLDOWN_MS,
                "Skipping merge: cooldown period active"
            );
            return Ok(false);
        }

        // Trigger merge - this runs asynchronously in Tantivy's merge thread pool
        info!(
            segments = segment_count,
            "Starting background segment merge"
        );

        // merge() returns a FutureResult that runs async; we drop it to let it run in background
        // The merge will complete when Tantivy's internal thread pool processes it
        let _merge_future = self.writer.merge(&segment_ids);
        LAST_MERGE_TS.store(now_ms, Ordering::Relaxed);
        info!("Segment merge initiated (running in background)");
        Ok(true)
    }

    /// Force immediate segment merge and wait for completion.
    /// Use sparingly - blocks until merge finishes.
    pub fn force_merge(&mut self) -> Result<()> {
        let segment_ids = self.index.searchable_segment_ids()?;
        if segment_ids.is_empty() {
            return Ok(());
        }
        info!(segments = segment_ids.len(), "Force merging all segments");
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);

        // Start merge and wait for completion
        let merge_future = self.writer.merge(&segment_ids);
        match merge_future.wait() {
            Ok(_) => {
                LAST_MERGE_TS.store(now_ms, Ordering::Relaxed);
                info!("Force merge completed");
                Ok(())
            }
            Err(e) => {
                warn!(error = %e, "Force merge failed");
                Err(anyhow!("merge failed: {e}"))
            }
        }
    }

    pub fn add_messages(
        &mut self,
        conv: &NormalizedConversation,
        messages: &[crate::connectors::NormalizedMessage],
    ) -> Result<()> {
        // Provenance fields (P3.x): default to local, but honor metadata injected by indexer.
        let cass_origin = conv.metadata.get("cass").and_then(|c| c.get("origin"));
        let source_id = cass_origin
            .and_then(|o| o.get("source_id"))
            .and_then(|v| v.as_str())
            .unwrap_or(LOCAL_SOURCE_ID);
        let origin_kind = cass_origin
            .and_then(|o| o.get("kind"))
            .and_then(|v| v.as_str())
            .unwrap_or("local");
        let origin_host = cass_origin
            .and_then(|o| o.get("host"))
            .and_then(|v| v.as_str());

        // Precompute per-conversation fields once (indexing hot path).
        let source_path = conv.source_path.to_string_lossy();
        let workspace = conv.workspace.as_ref().map(|ws| ws.to_string_lossy());
        let workspace_original = conv
            .metadata
            .get("cass")
            .and_then(|c| c.get("workspace_original"))
            .and_then(|v| v.as_str());
        let title = conv.title.as_deref();
        let title_prefix = title.map(generate_edge_ngrams);
        let started_at_fallback = conv.started_at;

        for msg in messages {
            let mut d = doc! {
                self.fields.agent => conv.agent_slug.clone(),
                self.fields.source_path => source_path.as_ref(),
                self.fields.msg_idx => msg.idx as u64,
                self.fields.content => msg.content.clone(),
                self.fields.source_id => source_id,
                self.fields.origin_kind => origin_kind,
            };
            if let Some(host) = origin_host
                && !host.is_empty()
            {
                d.add_text(self.fields.origin_host, host);
            }
            if let Some(ws) = &workspace {
                d.add_text(self.fields.workspace, ws.as_ref());
            }
            // workspace_original from metadata.cass.workspace_original (P6.2)
            if let Some(ws_orig) = workspace_original {
                d.add_text(self.fields.workspace_original, ws_orig);
            }
            if let Some(ts) = msg.created_at.or(started_at_fallback) {
                d.add_i64(self.fields.created_at, ts);
            }
            if let Some(title) = title {
                d.add_text(self.fields.title, title);
                if let Some(title_prefix) = &title_prefix {
                    d.add_text(self.fields.title_prefix, title_prefix);
                }
            }
            d.add_text(
                self.fields.content_prefix,
                generate_edge_ngrams(&msg.content),
            );
            d.add_text(self.fields.preview, build_preview(&msg.content, 400));
            self.writer.add_document(d)?;
        }
        Ok(())
    }
}

/// Maximum number of byte indices needed for edge n-gram generation.
/// We collect up to 22 character byte positions, enabling n-grams from length 2
/// up to 21 characters. For words shorter than 22 chars, the word.len() position
/// is included automatically via the chain; for longer words, we cap at 22 positions.
const MAX_NGRAM_INDICES: usize = 22;

/// Generate edge n-grams from text without heap allocation for index collection.
///
/// Uses `ArrayVec` instead of `Vec` to store byte indices on the stack,
/// reducing allocator pressure during bulk indexing operations.
///
/// # Performance
/// This optimization avoids heap allocation for the indices vector on every
/// word processed. For large indexing jobs with millions of words, this
/// eliminates millions of small allocations and deallocations.
fn generate_edge_ngrams(text: &str) -> String {
    let mut ngrams = String::with_capacity(text.len() * 2);
    // Split by non-alphanumeric characters to identify words
    for word in text.split(|c: char| !c.is_alphanumeric()) {
        // Collect byte indices of characters, plus the total length.
        // Using ArrayVec avoids heap allocation since max size is known (22).
        // We only need up to 21 indices (to support max ngram length of 20)
        let indices: ArrayVec<usize, MAX_NGRAM_INDICES> = word
            .char_indices()
            .map(|(i, _)| i)
            .chain(std::iter::once(word.len()))
            .take(MAX_NGRAM_INDICES)
            .collect();

        // Need at least 2 indices (start, end/char 1) for length 1 ngram
        // indices[0] is always 0.
        // indices[1] is start of char 1 (or len).
        if indices.len() < 2 {
            continue;
        }

        // Generate edge ngrams of length 1..=21 (or word length if shorter)
        for &end_idx in &indices[1..] {
            if !ngrams.is_empty() {
                ngrams.push(' ');
            }
            ngrams.push_str(&word[..end_idx]);
        }
    }
    ngrams
}

pub fn build_schema() -> Schema {
    let mut schema_builder = Schema::builder();
    let text = TextOptions::default()
        .set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("hyphen_normalize")
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        )
        .set_stored();

    let text_not_stored = TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer("hyphen_normalize")
            .set_index_option(IndexRecordOption::WithFreqsAndPositions),
    );

    // Use STRING (not TEXT) so agent slug is stored as a single non-tokenized term.
    // This ensures exact match filtering works correctly with TermQuery.
    schema_builder.add_text_field("agent", STRING | STORED);
    schema_builder.add_text_field("workspace", STRING | STORED);
    // workspace_original stores the pre-rewrite path for audit/display (P6.2)
    schema_builder.add_text_field("workspace_original", STORED);
    schema_builder.add_text_field("source_path", STORED);
    schema_builder.add_u64_field("msg_idx", INDEXED | STORED);
    schema_builder.add_i64_field("created_at", INDEXED | STORED | FAST);
    schema_builder.add_text_field("title", text.clone());
    schema_builder.add_text_field("content", text);
    schema_builder.add_text_field("title_prefix", text_not_stored.clone());
    schema_builder.add_text_field("content_prefix", text_not_stored);
    schema_builder.add_text_field("preview", TEXT | STORED);
    // Provenance fields (P1.4) - STRING for exact match filtering
    schema_builder.add_text_field("source_id", STRING | STORED);
    schema_builder.add_text_field("origin_kind", STRING | STORED);
    schema_builder.add_text_field("origin_host", STRING | STORED);
    schema_builder.build()
}

pub fn fields_from_schema(schema: &Schema) -> Result<Fields> {
    let get = |name: &str| {
        schema
            .get_field(name)
            .map_err(|_| anyhow!("schema missing {name}"))
    };
    Ok(Fields {
        agent: get("agent")?,
        workspace: get("workspace")?,
        workspace_original: get("workspace_original")?,
        source_path: get("source_path")?,
        msg_idx: get("msg_idx")?,
        created_at: get("created_at")?,
        title: get("title")?,
        content: get("content")?,
        title_prefix: get("title_prefix")?,
        content_prefix: get("content_prefix")?,
        preview: get("preview")?,
        source_id: get("source_id")?,
        origin_kind: get("origin_kind")?,
        origin_host: get("origin_host")?,
    })
}

fn build_preview(content: &str, max_chars: usize) -> String {
    let mut out = String::new();
    let mut chars = content.chars();

    // Copy at most max_chars characters into the preview.
    for _ in 0..max_chars {
        if let Some(ch) = chars.next() {
            out.push(ch);
        } else {
            // Content shorter than or equal to max_chars; no ellipsis.
            return out;
        }
    }

    // If there are more characters, append an ellipsis.
    if chars.next().is_some() {
        out.push('…');
    }

    out
}

pub fn index_dir(base: &Path) -> Result<std::path::PathBuf> {
    let dir = base.join("index").join(SCHEMA_VERSION);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub fn ensure_tokenizer(index: &mut Index) {
    use tantivy::tokenizer::{LowerCaser, RemoveLongFilter, SimpleTokenizer, TextAnalyzer};
    let analyzer = TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(LowerCaser)
        .filter(RemoveLongFilter::limit(40))
        .build();
    index.tokenizers().register("hyphen_normalize", analyzer);
}

// =============================================================================
// Index Corruption Handling Tests (tst.idx.corrupt)
// Tests for graceful handling of corrupted or invalid index states
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn open_or_create_handles_missing_schema_hash() {
        let dir = TempDir::new().unwrap();
        let path = dir.path();

        // Create index first
        {
            let _index = TantivyIndex::open_or_create(path).unwrap();
        }

        // Remove schema_hash.json to simulate corruption
        fs::remove_file(path.join("schema_hash.json")).unwrap();

        // Should recreate cleanly without panic
        let result = TantivyIndex::open_or_create(path);
        assert!(
            result.is_ok(),
            "Should handle missing schema_hash.json gracefully"
        );
    }

    #[test]
    fn open_or_create_handles_invalid_schema_hash() {
        let dir = TempDir::new().unwrap();
        let path = dir.path();

        // Create index first
        {
            let _index = TantivyIndex::open_or_create(path).unwrap();
        }

        // Corrupt the schema_hash.json
        fs::write(
            path.join("schema_hash.json"),
            r#"{"schema_hash":"invalid-hash"}"#,
        )
        .unwrap();

        // Should detect mismatch and rebuild
        let result = TantivyIndex::open_or_create(path);
        assert!(
            result.is_ok(),
            "Should handle invalid schema_hash gracefully"
        );
    }

    #[test]
    fn open_or_create_handles_corrupted_schema_hash_json() {
        let dir = TempDir::new().unwrap();
        let path = dir.path();

        // Create index first
        {
            let _index = TantivyIndex::open_or_create(path).unwrap();
        }

        // Write completely invalid JSON
        fs::write(path.join("schema_hash.json"), "{ invalid json {{").unwrap();

        // Should fail to read (non-JSON) but rebuild successfully
        let result = TantivyIndex::open_or_create(path);
        // Reading invalid JSON will fail but rebuild should happen cleanly
        assert!(
            result.is_ok(),
            "Should rebuild index on corrupted schema_hash.json"
        );
    }

    #[test]
    fn open_or_create_handles_empty_directory() {
        let dir = TempDir::new().unwrap();
        let path = dir.path();

        // Open on empty directory - should create new
        let result = TantivyIndex::open_or_create(path);
        assert!(result.is_ok(), "Should create new index in empty directory");
    }

    #[test]
    fn open_or_create_handles_missing_meta_json() {
        let dir = TempDir::new().unwrap();
        let path = dir.path();

        // Create valid schema_hash but no meta.json
        fs::create_dir_all(path).unwrap();
        fs::write(
            path.join("schema_hash.json"),
            format!(r#"{{"schema_hash":"{SCHEMA_HASH}"}}"#),
        )
        .unwrap();

        // Should create new index (meta.json missing triggers create)
        let result = TantivyIndex::open_or_create(path);
        assert!(
            result.is_ok(),
            "Should create new index when meta.json missing"
        );
    }

    #[test]
    fn open_or_create_handles_corrupted_meta_json() {
        let dir = TempDir::new().unwrap();
        let path = dir.path();

        // Create index first
        {
            let _index = TantivyIndex::open_or_create(path).unwrap();
        }

        // Corrupt meta.json (Tantivy's index metadata file)
        let meta_path = path.join("meta.json");
        if meta_path.exists() {
            fs::write(&meta_path, "corrupted meta content").unwrap();
        }

        // Should detect corruption and rebuild (open_in_dir fails)
        let result = TantivyIndex::open_or_create(path);
        assert!(
            result.is_ok(),
            "Should rebuild index on corrupted meta.json without panicking"
        );
    }

    #[test]
    fn open_or_create_handles_truncated_segment_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path();

        // Create index and add some data
        {
            let mut index = TantivyIndex::open_or_create(path).unwrap();
            // Create a simple doc to generate a segment
            let doc = doc! {
                index.fields.agent => "test_agent",
                index.fields.source_path => "/test/path",
                index.fields.msg_idx => 0u64,
                index.fields.content => "test content for segment",
            };
            index.writer.add_document(doc).unwrap();
            index.commit().unwrap();
        }

        // Collect and sort segment files for deterministic behavior
        let mut segment_files: Vec<_> = fs::read_dir(path)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                let name = e.file_name();
                let name_str = name.to_string_lossy();
                name_str.ends_with(".store") || name_str.ends_with(".idx")
            })
            .collect();
        segment_files.sort_by_key(|e| e.path());

        // Ensure we found at least one segment file to truncate
        assert!(
            !segment_files.is_empty(),
            "Test setup error: no .store or .idx files found after commit"
        );

        // Truncate the first segment file (deterministic after sort)
        let file_to_truncate = &segment_files[0];
        let file = fs::OpenOptions::new()
            .write(true)
            .open(file_to_truncate.path())
            .unwrap();
        file.set_len(10).unwrap(); // Leave only 10 bytes (corrupted)
        file.sync_all().unwrap(); // Ensure truncation is persisted to disk

        // Should handle truncated segment gracefully by rebuilding
        let result = TantivyIndex::open_or_create(path);
        assert!(
            result.is_ok(),
            "Should open or rebuild index cleanly after truncated segment file"
        );
    }

    #[test]
    fn open_or_create_roundtrip_add_and_search() {
        let dir = TempDir::new().unwrap();
        let path = dir.path();

        // Create, add, and verify data survives
        {
            let mut index = TantivyIndex::open_or_create(path).unwrap();
            let doc = doc! {
                index.fields.agent => "test_agent",
                index.fields.source_path => "/test/path",
                index.fields.msg_idx => 0u64,
                index.fields.content => "hello world test content",
            };
            index.writer.add_document(doc).unwrap();
            index.commit().unwrap();
        }

        // Reopen and verify
        {
            let index = TantivyIndex::open_or_create(path).unwrap();
            let reader = index.reader().unwrap();
            let searcher = reader.searcher();

            // Should have at least 1 document
            assert!(
                searcher.num_docs() >= 1,
                "Should have at least 1 document after roundtrip"
            );
        }
    }

    #[test]
    fn open_or_create_rebuild_on_schema_mismatch() {
        let dir = TempDir::new().unwrap();
        let path = dir.path();

        // Create index first
        {
            let _index = TantivyIndex::open_or_create(path).unwrap();
        }

        // Write an old/different schema hash
        fs::write(
            path.join("schema_hash.json"),
            r#"{"schema_hash":"old-schema-v1"}"#,
        )
        .unwrap();

        // Should rebuild (delete old and create new)
        let result = TantivyIndex::open_or_create(path);
        assert!(result.is_ok(), "Should rebuild index on schema mismatch");

        // Verify the new schema hash is written
        let hash_content = fs::read_to_string(path.join("schema_hash.json")).unwrap();
        assert!(
            hash_content.contains(SCHEMA_HASH),
            "Should write correct schema hash after rebuild"
        );
    }

    #[test]
    fn build_schema_returns_valid_schema() {
        let schema = build_schema();

        // Verify all required fields exist
        assert!(schema.get_field("agent").is_ok());
        assert!(schema.get_field("workspace").is_ok());
        assert!(schema.get_field("workspace_original").is_ok());
        assert!(schema.get_field("source_path").is_ok());
        assert!(schema.get_field("msg_idx").is_ok());
        assert!(schema.get_field("created_at").is_ok());
        assert!(schema.get_field("title").is_ok());
        assert!(schema.get_field("content").is_ok());
        assert!(schema.get_field("title_prefix").is_ok());
        assert!(schema.get_field("content_prefix").is_ok());
        assert!(schema.get_field("preview").is_ok());
        // Provenance fields (P1.4)
        assert!(schema.get_field("source_id").is_ok());
        assert!(schema.get_field("origin_kind").is_ok());
        assert!(schema.get_field("origin_host").is_ok());
    }

    #[test]
    fn fields_from_schema_extracts_all_fields() {
        let schema = build_schema();
        let fields = fields_from_schema(&schema).unwrap();

        // Verify fields are valid (non-panicking access)
        let _ = fields.agent;
        let _ = fields.workspace;
        let _ = fields.workspace_original;
        let _ = fields.source_path;
        let _ = fields.msg_idx;
        let _ = fields.created_at;
        let _ = fields.title;
        let _ = fields.content;
        let _ = fields.title_prefix;
        let _ = fields.content_prefix;
        let _ = fields.preview;
        // Provenance fields (P1.4)
        let _ = fields.source_id;
        let _ = fields.origin_kind;
        let _ = fields.origin_host;
    }

    #[test]
    fn generate_edge_ngrams_produces_prefixes() {
        let result = generate_edge_ngrams("hello");
        // Should generate ngrams: "he", "hel", "hell", "hello"
        assert!(result.contains("he"));
        assert!(result.contains("hel"));
        assert!(result.contains("hell"));
        assert!(result.contains("hello"));
    }

    #[test]
    fn generate_edge_ngrams_handles_empty_string() {
        let result = generate_edge_ngrams("");
        assert!(result.is_empty());
    }

    #[test]
    fn generate_edge_ngrams_handles_short_string() {
        // Single char words are skipped (len < 2)
        let result = generate_edge_ngrams("a");
        assert!(result.is_empty());

        // Two char word generates just "ab"
        let result = generate_edge_ngrams("ab");
        assert_eq!(result, "ab");
    }

    #[test]
    fn generate_edge_ngrams_handles_multiple_words() {
        let result = generate_edge_ngrams("hello world");
        // Should contain ngrams from both words
        assert!(result.contains("he"));
        assert!(result.contains("wo"));
        assert!(result.contains("world"));
    }

    #[test]
    fn title_prefix_ngrams_are_reused_for_each_message_doc() {
        use crate::connectors::{NormalizedConversation, NormalizedMessage};
        use crate::search::query::{FieldMask, SearchClient, SearchFilters};

        let dir = TempDir::new().unwrap();
        let index_path = dir.path();

        let mut index = TantivyIndex::open_or_create(index_path).unwrap();

        // Title contains "unique..." but message contents do not.
        // Searching for a short prefix ("un") should match via `title_prefix`,
        // and therefore return *every* message document in the conversation.
        let conv = NormalizedConversation {
            agent_slug: "bench-agent".into(),
            external_id: Some("conv-1".into()),
            title: Some("UniqueTitleToken".into()),
            workspace: None,
            source_path: "/tmp/bench/conv-1.jsonl".into(),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_000_000_001),
            metadata: serde_json::json!({}),
            messages: vec![
                NormalizedMessage {
                    idx: 0,
                    role: "user".into(),
                    author: None,
                    created_at: Some(1_700_000_000_000),
                    content: "first message content".into(),
                    extra: serde_json::json!({}),
                    snippets: Vec::new(),
                },
                NormalizedMessage {
                    idx: 1,
                    role: "agent".into(),
                    author: None,
                    created_at: Some(1_700_000_000_001),
                    content: "second message content".into(),
                    extra: serde_json::json!({}),
                    snippets: Vec::new(),
                },
            ],
        };

        index.add_messages(&conv, &conv.messages).unwrap();
        index.commit().unwrap();

        let client = SearchClient::open(index_path, None).unwrap().unwrap();
        let hits = client
            .search("un", SearchFilters::default(), 10, 0, FieldMask::FULL)
            .unwrap();

        assert_eq!(
            hits.len(),
            2,
            "Expected both message docs to match via title_prefix ngrams"
        );
    }

    #[test]
    fn merge_status_should_merge_logic() {
        let status = MergeStatus {
            segment_count: 5,
            last_merge_ts: 0,
            ms_since_last_merge: -1, // never merged
            merge_threshold: 4,
            cooldown_ms: 300_000,
        };
        assert!(
            status.should_merge(),
            "Should merge when never merged and above threshold"
        );

        let status_below_threshold = MergeStatus {
            segment_count: 2,
            last_merge_ts: 0,
            ms_since_last_merge: -1,
            merge_threshold: 4,
            cooldown_ms: 300_000,
        };
        assert!(
            !status_below_threshold.should_merge(),
            "Should not merge when below threshold"
        );

        let status_in_cooldown = MergeStatus {
            segment_count: 5,
            last_merge_ts: 1000,
            ms_since_last_merge: 1000, // Only 1 second since last
            merge_threshold: 4,
            cooldown_ms: 300_000, // 5 minute cooldown
        };
        assert!(
            !status_in_cooldown.should_merge(),
            "Should not merge during cooldown"
        );

        let status_after_cooldown = MergeStatus {
            segment_count: 5,
            last_merge_ts: 1000,
            ms_since_last_merge: 400_000, // 6+ minutes since last
            merge_threshold: 4,
            cooldown_ms: 300_000,
        };
        assert!(
            status_after_cooldown.should_merge(),
            "Should merge after cooldown expires"
        );
    }

    #[test]
    fn index_dir_creates_versioned_path() {
        let dir = TempDir::new().unwrap();
        let result = index_dir(dir.path()).unwrap();

        assert!(result.ends_with(format!("index/{}", SCHEMA_VERSION)));
        assert!(result.exists());
    }

    // =============================================================================
    // Full Index Rebuild Tests (tst.idx.rebuild)
    // Tests for complete index rebuild scenarios
    // =============================================================================

    #[test]
    fn rebuild_from_empty_directory() {
        let dir = TempDir::new().unwrap();
        let path = dir.path();

        // Should create index from scratch in empty directory
        let result = TantivyIndex::open_or_create(path);
        assert!(result.is_ok(), "Should create index from empty directory");

        // Verify schema hash is written
        let hash_path = path.join("schema_hash.json");
        assert!(hash_path.exists(), "Should write schema_hash.json");

        let hash_content = fs::read_to_string(hash_path).unwrap();
        assert!(
            hash_content.contains(SCHEMA_HASH),
            "Should contain current schema hash"
        );
    }

    #[test]
    fn rebuild_creates_meta_json() {
        let dir = TempDir::new().unwrap();
        let path = dir.path();

        let index = TantivyIndex::open_or_create(path).unwrap();
        drop(index);

        // Tantivy should have created meta.json
        let meta_path = path.join("meta.json");
        assert!(meta_path.exists(), "Should create meta.json");
    }

    #[test]
    fn rebuild_doc_count_matches_added_documents() {
        let dir = TempDir::new().unwrap();
        let path = dir.path();

        // Add multiple documents
        {
            let mut index = TantivyIndex::open_or_create(path).unwrap();

            for i in 0..5 {
                let doc = doc! {
                    index.fields.agent => "test_agent",
                    index.fields.source_path => format!("/test/path/{}", i),
                    index.fields.msg_idx => i as u64,
                    index.fields.content => format!("content {}", i),
                };
                index.writer.add_document(doc).unwrap();
            }
            index.commit().unwrap();
        }

        // Verify doc count
        {
            let index = TantivyIndex::open_or_create(path).unwrap();
            let reader = index.reader().unwrap();
            let searcher = reader.searcher();

            assert_eq!(searcher.num_docs(), 5, "Should have exactly 5 documents");
        }
    }

    #[test]
    fn rebuild_delete_all_clears_index() {
        let dir = TempDir::new().unwrap();
        let path = dir.path();

        // Add documents
        {
            let mut index = TantivyIndex::open_or_create(path).unwrap();
            for i in 0..3 {
                let doc = doc! {
                    index.fields.agent => "test_agent",
                    index.fields.source_path => format!("/test/path/{}", i),
                    index.fields.msg_idx => i as u64,
                    index.fields.content => format!("content {}", i),
                };
                index.writer.add_document(doc).unwrap();
            }
            index.commit().unwrap();

            // Delete all and commit
            index.delete_all().unwrap();
            index.commit().unwrap();
        }

        // Wait for lock release (flaky on some FS)
        std::thread::sleep(std::time::Duration::from_millis(100));

        // Verify empty
        {
            let index = TantivyIndex::open_or_create(path).unwrap();
            let reader = index.reader().unwrap();
            let searcher = reader.searcher();

            assert_eq!(
                searcher.num_docs(),
                0,
                "Should have 0 documents after delete_all"
            );
        }
    }

    #[test]
    fn rebuild_force_via_schema_change() {
        let dir = TempDir::new().unwrap();
        let path = dir.path();

        // Create and add documents
        {
            let mut index = TantivyIndex::open_or_create(path).unwrap();
            let doc = doc! {
                index.fields.agent => "test_agent",
                index.fields.source_path => "/test/path",
                index.fields.msg_idx => 0u64,
                index.fields.content => "original content",
            };
            index.writer.add_document(doc).unwrap();
            index.commit().unwrap();
        }

        // Simulate forcing rebuild by changing schema hash
        fs::write(
            path.join("schema_hash.json"),
            r#"{"schema_hash":"force-rebuild-v0"}"#,
        )
        .unwrap();

        // Reopen - should rebuild (losing old data)
        {
            let index = TantivyIndex::open_or_create(path).unwrap();
            let reader = index.reader().unwrap();
            let searcher = reader.searcher();

            // After force rebuild, index is empty
            assert_eq!(
                searcher.num_docs(),
                0,
                "Should have 0 documents after force rebuild"
            );
        }
    }

    #[test]
    fn rebuild_preserves_data_when_schema_matches() {
        let dir = TempDir::new().unwrap();
        let path = dir.path();

        // Create and add documents
        {
            let mut index = TantivyIndex::open_or_create(path).unwrap();
            let doc = doc! {
                index.fields.agent => "preserved_agent",
                index.fields.source_path => "/preserved/path",
                index.fields.msg_idx => 42u64,
                index.fields.content => "preserved content",
            };
            index.writer.add_document(doc).unwrap();
            index.commit().unwrap();
        }

        // Reopen without schema change - should preserve data
        {
            let index = TantivyIndex::open_or_create(path).unwrap();
            let reader = index.reader().unwrap();
            let searcher = reader.searcher();

            assert_eq!(
                searcher.num_docs(),
                1,
                "Should preserve 1 document when schema matches"
            );
        }
    }

    #[test]
    fn rebuild_all_fields_searchable_after_add() {
        let dir = TempDir::new().unwrap();
        let path = dir.path();

        let mut index = TantivyIndex::open_or_create(path).unwrap();

        // Add document with all fields
        let doc = doc! {
            index.fields.agent => "claude_code",
            index.fields.workspace => "/workspace/project",
            index.fields.source_path => "/path/to/session.jsonl",
            index.fields.msg_idx => 0u64,
            index.fields.created_at => 1700000000i64,
            index.fields.title => "Test Session Title",
            index.fields.content => "This is the message content",
            index.fields.title_prefix => generate_edge_ngrams("Test Session Title"),
            index.fields.content_prefix => generate_edge_ngrams("This is the message content"),
            index.fields.preview => "Preview text",
        };
        index.writer.add_document(doc).unwrap();
        index.commit().unwrap();

        let reader = index.reader().unwrap();
        let searcher = reader.searcher();

        // Verify document is indexed
        assert!(
            searcher.num_docs() >= 1,
            "Document should be searchable after add"
        );
    }

    #[test]
    fn rebuild_schema_version_consistency() {
        let dir = TempDir::new().unwrap();
        let path = dir.path();

        // Create index
        {
            let _index = TantivyIndex::open_or_create(path).unwrap();
        }

        // Read and verify schema hash format
        let hash_content = fs::read_to_string(path.join("schema_hash.json")).unwrap();
        let expected = format!(r#"{{"schema_hash":"{}"}}"#, SCHEMA_HASH);
        assert_eq!(hash_content, expected, "Schema hash format should match");
    }

    #[test]
    fn rebuild_commit_creates_searchable_state() {
        let dir = TempDir::new().unwrap();
        let path = dir.path();

        let mut index = TantivyIndex::open_or_create(path).unwrap();

        // Add without commit - not searchable in new reader
        let doc = doc! {
            index.fields.agent => "test",
            index.fields.source_path => "/test",
            index.fields.msg_idx => 0u64,
            index.fields.content => "before commit",
        };
        index.writer.add_document(doc).unwrap();

        // After commit - searchable
        index.commit().unwrap();

        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        assert!(
            searcher.num_docs() >= 1,
            "Document should be searchable after commit"
        );
    }

    #[test]
    fn rebuild_multiple_commits_accumulate() {
        let dir = TempDir::new().unwrap();
        let path = dir.path();

        let mut index = TantivyIndex::open_or_create(path).unwrap();

        // First batch
        let doc1 = doc! {
            index.fields.agent => "agent1",
            index.fields.source_path => "/path1",
            index.fields.msg_idx => 0u64,
            index.fields.content => "first batch",
        };
        index.writer.add_document(doc1).unwrap();
        index.commit().unwrap();

        // Second batch
        let doc2 = doc! {
            index.fields.agent => "agent2",
            index.fields.source_path => "/path2",
            index.fields.msg_idx => 0u64,
            index.fields.content => "second batch",
        };
        index.writer.add_document(doc2).unwrap();
        index.commit().unwrap();

        // Third batch
        let doc3 = doc! {
            index.fields.agent => "agent3",
            index.fields.source_path => "/path3",
            index.fields.msg_idx => 0u64,
            index.fields.content => "third batch",
        };
        index.writer.add_document(doc3).unwrap();
        index.commit().unwrap();

        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        assert_eq!(
            searcher.num_docs(),
            3,
            "Should have 3 documents after 3 commits"
        );
    }

    #[test]
    fn rebuild_empty_index_has_zero_docs() {
        let dir = TempDir::new().unwrap();
        let path = dir.path();

        let index = TantivyIndex::open_or_create(path).unwrap();
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();

        assert_eq!(searcher.num_docs(), 0, "New index should have 0 documents");
    }

    #[test]
    fn rebuild_can_reopen_after_close() {
        let dir = TempDir::new().unwrap();
        let path = dir.path();

        // Create, add, close
        {
            let mut index = TantivyIndex::open_or_create(path).unwrap();
            let doc = doc! {
                index.fields.agent => "test",
                index.fields.source_path => "/test",
                index.fields.msg_idx => 0u64,
                index.fields.content => "content",
            };
            index.writer.add_document(doc).unwrap();
            index.commit().unwrap();
        }

        // Reopen
        let result = TantivyIndex::open_or_create(path);
        assert!(result.is_ok(), "Should be able to reopen after close");

        let index = result.unwrap();
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        assert_eq!(searcher.num_docs(), 1, "Data should persist after reopen");
    }

    #[test]
    fn rebuild_handles_large_batch() {
        let dir = TempDir::new().unwrap();
        let path = dir.path();

        let mut index = TantivyIndex::open_or_create(path).unwrap();

        // Add 100 documents
        for i in 0..100 {
            let doc = doc! {
                index.fields.agent => "batch_agent",
                index.fields.source_path => format!("/batch/path/{}", i),
                index.fields.msg_idx => i as u64,
                index.fields.content => format!("batch content number {}", i),
            };
            index.writer.add_document(doc).unwrap();
        }
        index.commit().unwrap();

        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        assert_eq!(
            searcher.num_docs(),
            100,
            "Should have 100 documents after large batch"
        );
    }
}
