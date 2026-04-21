use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use frankensearch::index::{
    HNSW_DEFAULT_EF_CONSTRUCTION as FS_HNSW_DEFAULT_EF_CONSTRUCTION,
    HNSW_DEFAULT_M as FS_HNSW_DEFAULT_M, HnswConfig as FsHnswConfig, HnswIndex as FsHnswIndex,
    Quantization as FsQuantization, VectorIndex as FsVectorIndex,
    VectorIndexWriter as FsVectorIndexWriter,
};
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use rayon::prelude::*;

use crate::search::canonicalize::{canonicalize_for_embedding, content_hash};
use crate::search::embedder::Embedder;
use crate::search::fastembed_embedder::FastEmbedder;
use crate::search::hash_embedder::HashEmbedder;
use crate::search::vector_index::{ROLE_USER, SemanticDocId, VECTOR_INDEX_DIR, vector_index_path};

/// Default embedder batch size. 128 is a sweet spot for ONNX MiniLM models on
/// modern CPUs: big enough to amortize dispatch overhead and keep the tensor
/// kernels saturated, small enough that one batch fits comfortably in L2 and
/// memory reservation stays bounded for large corpora.
const DEFAULT_SEMANTIC_BATCH_SIZE: usize = 128;

fn resolved_default_batch_size() -> usize {
    dotenvy::var("CASS_SEMANTIC_BATCH_SIZE")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_SEMANTIC_BATCH_SIZE)
}

/// Opt in to the rayon-parallel canonicalize+hash prep step. **Default: OFF.**
///
/// The parallel path is kept because canonicalize+hash CAN dominate the
/// embedding wall-clock on pathological inputs (very long messages, costly
/// Unicode normalization). But criterion baselines captured under
/// `tests/artifacts/perf/2026-04-21-profile-run/baselines.md` showed a
/// 1.2×–2.3× **regression** on the hash embedder across every batch size
/// tested (2 000 messages, mixed markdown/code/unicode): rayon's per-task
/// scheduling overhead is larger than the per-message canonicalize+hash cost
/// when the embedder itself is cheap. For the production ONNX (MiniLM)
/// embedder, per-batch inference already dwarfs prep, so parallel prep never
/// buys meaningful wall-clock — the prep step is ≤ 1% of total embed time.
///
/// Set `CASS_SEMANTIC_PREP_PARALLEL=1` / `true` / `yes` / `on` to opt in.
fn parallel_prep_enabled() -> bool {
    dotenvy::var("CASS_SEMANTIC_PREP_PARALLEL")
        .ok()
        .map(|v| {
            let v = v.trim().to_ascii_lowercase();
            matches!(v.as_str(), "1" | "true" | "yes" | "on")
        })
        .unwrap_or(false)
}

#[derive(Debug, Clone)]
pub struct EmbeddingInput {
    pub message_id: u64,
    pub created_at_ms: i64,
    pub agent_id: u32,
    pub workspace_id: u32,
    pub source_id: u32,
    pub role: u8,
    pub chunk_idx: u8,
    pub content: String,
}

impl EmbeddingInput {
    pub fn new(message_id: u64, content: impl Into<String>) -> Self {
        Self {
            message_id,
            created_at_ms: 0,
            agent_id: 0,
            workspace_id: 0,
            source_id: 0,
            role: ROLE_USER,
            chunk_idx: 0,
            content: content.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct EmbeddedMessage {
    pub message_id: u64,
    pub created_at_ms: i64,
    pub agent_id: u32,
    pub workspace_id: u32,
    pub source_id: u32,
    pub role: u8,
    pub chunk_idx: u8,
    pub content_hash: [u8; 32],
    pub embedding: Vec<f32>,
}

fn hnsw_index_path(data_dir: &Path, embedder_id: &str) -> PathBuf {
    data_dir
        .join(VECTOR_INDEX_DIR)
        .join(format!("hnsw-{embedder_id}.chsw"))
}

struct Prepared<'a> {
    msg: &'a EmbeddingInput,
    canonical: String,
    hash: [u8; 32],
}

/// Canonicalize + hash a window of messages. Default is serial; opt in to
/// the rayon-parallel path via `CASS_SEMANTIC_PREP_PARALLEL=1` (see the
/// `parallel_prep_enabled` docstring for why it is not the default).
/// Parallel results preserve input order via `par_iter().filter_map().collect()`.
/// Messages whose canonical form is empty are filtered out so the embedder
/// batch is never polluted with useless inputs.
fn prepare_window<'a>(window: &'a [EmbeddingInput], serial: bool) -> Vec<Prepared<'a>> {
    let prep = |msg: &'a EmbeddingInput| -> Option<Prepared<'a>> {
        let canonical = canonicalize_for_embedding(&msg.content);
        if canonical.is_empty() {
            return None;
        }
        let hash = content_hash(&canonical);
        Some(Prepared {
            msg,
            canonical,
            hash,
        })
    };

    if serial {
        window.iter().filter_map(prep).collect()
    } else {
        window.par_iter().filter_map(prep).collect()
    }
}

fn flush_prepared_batch(
    batch: &[Prepared<'_>],
    embeddings: &mut Vec<EmbeddedMessage>,
    pb: &ProgressBar,
    embedder: &dyn Embedder,
) -> Result<()> {
    if batch.is_empty() {
        return Ok(());
    }

    let texts: Vec<&str> = batch.iter().map(|p| p.canonical.as_str()).collect();
    let vectors = embedder
        .embed_batch_sync(&texts)
        .map_err(|e| anyhow::anyhow!("embedding failed: {e}"))?;

    if vectors.len() != batch.len() {
        bail!(
            "embedder returned {} embeddings for {} inputs",
            vectors.len(),
            batch.len()
        );
    }

    for (prepared, vector) in batch.iter().zip(vectors) {
        if vector.len() != embedder.dimension() {
            bail!(
                "embedding dimension mismatch: expected {}, got {}",
                embedder.dimension(),
                vector.len()
            );
        }
        embeddings.push(EmbeddedMessage {
            message_id: prepared.msg.message_id,
            created_at_ms: prepared.msg.created_at_ms,
            agent_id: prepared.msg.agent_id,
            workspace_id: prepared.msg.workspace_id,
            source_id: prepared.msg.source_id,
            role: prepared.msg.role,
            chunk_idx: prepared.msg.chunk_idx,
            content_hash: prepared.hash,
            embedding: vector,
        });
    }

    pb.inc(batch.len() as u64);
    Ok(())
}

pub struct SemanticIndexer {
    embedder: Box<dyn Embedder>,
    batch_size: usize,
}

impl SemanticIndexer {
    pub fn new(embedder_type: &str, data_dir: Option<&Path>) -> Result<Self> {
        let embedder: Box<dyn Embedder> = match embedder_type {
            "fastembed" => {
                let dir = data_dir
                    .ok_or_else(|| anyhow::anyhow!("data_dir required for fastembed embedder"))?;
                let model_dir = FastEmbedder::default_model_dir(dir);
                Box::new(
                    FastEmbedder::load_from_dir(&model_dir)
                        .map_err(|e| anyhow::anyhow!("fastembed unavailable: {e}"))?,
                )
            }
            "hash" => Box::new(HashEmbedder::default()),
            other => bail!("unknown embedder: {other}"),
        };

        Ok(Self {
            embedder,
            batch_size: resolved_default_batch_size(),
        })
    }

    pub fn with_batch_size(mut self, batch_size: usize) -> Result<Self> {
        if batch_size == 0 {
            bail!("batch_size must be > 0");
        }
        self.batch_size = batch_size;
        Ok(self)
    }

    pub fn batch_size(&self) -> usize {
        self.batch_size
    }

    pub fn embedder_id(&self) -> &str {
        self.embedder.id()
    }

    pub fn embedder_dimension(&self) -> usize {
        self.embedder.dimension()
    }

    pub fn embed_messages(&self, messages: &[EmbeddingInput]) -> Result<Vec<EmbeddedMessage>> {
        if messages.is_empty() {
            return Ok(Vec::new());
        }

        let show_progress = std::io::stderr().is_terminal();
        let pb = ProgressBar::new(messages.len() as u64);
        if show_progress {
            let style = ProgressStyle::default_bar()
                .template("{spinner:.green} [{bar:40.cyan/blue}] {pos}/{len} messages embedded")
                .unwrap_or_else(|_| ProgressStyle::default_bar());
            pb.set_style(style);
        } else {
            pb.set_draw_target(ProgressDrawTarget::hidden());
        }

        let mut embeddings = Vec::with_capacity(messages.len());

        // Process the corpus in windows of ~4 batches. Within each window,
        // rayon parallelizes the canonicalize + hash prep across cores; the
        // ONNX embedder is then fed serially in `batch_size` chunks so its
        // internal thread pool stays saturated without being starved by the
        // single-threaded prep loop we had before. `with_batch_size` and
        // `resolved_default_batch_size` both guarantee `batch_size >= 1`,
        // so saturating_mul(4) is always >= batch_size — no further clamp.
        let window = self.batch_size.saturating_mul(4);
        for window_slice in messages.chunks(window) {
            let prepared_window = prepare_window(window_slice, !parallel_prep_enabled());
            let skipped_in_window = window_slice.len() - prepared_window.len();
            if skipped_in_window > 0 {
                pb.inc(skipped_in_window as u64);
            }

            for batch in prepared_window.chunks(self.batch_size) {
                flush_prepared_batch(batch, &mut embeddings, &pb, self.embedder.as_ref())?;
            }
        }

        pb.finish_with_message("Embedding complete");
        Ok(embeddings)
    }

    pub fn build_and_save_index<I>(
        &self,
        embedded_messages: I,
        data_dir: &Path,
    ) -> Result<FsVectorIndex>
    where
        I: IntoIterator<Item = EmbeddedMessage>,
    {
        let index_path = vector_index_path(data_dir, self.embedder_id());
        if let Some(parent) = index_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Store as f16 by default (smaller, faster I/O). Embeddings are validated by the writer.
        let mut writer: FsVectorIndexWriter = FsVectorIndex::create_with_revision(
            &index_path,
            self.embedder_id(),
            "1.0",
            self.embedder_dimension(),
            FsQuantization::F16,
        )
        .map_err(|err| anyhow::anyhow!("create fsvi index failed: {err}"))?;

        let write_result: Result<()> = (|| {
            for embedded in embedded_messages {
                if embedded.embedding.len() != self.embedder_dimension() {
                    bail!(
                        "embedding dimension mismatch: expected {}, got {}",
                        self.embedder_dimension(),
                        embedded.embedding.len()
                    );
                }
                let doc_id = SemanticDocId {
                    message_id: embedded.message_id,
                    chunk_idx: embedded.chunk_idx,
                    agent_id: embedded.agent_id,
                    workspace_id: embedded.workspace_id,
                    source_id: embedded.source_id,
                    role: embedded.role,
                    created_at_ms: embedded.created_at_ms,
                    content_hash: Some(embedded.content_hash),
                }
                .to_doc_id_string();
                writer
                    .write_record(&doc_id, &embedded.embedding)
                    .map_err(|err| anyhow::anyhow!("write fsvi record failed: {err}"))?;
            }
            Ok(())
        })();

        if let Err(e) = &write_result {
            // Clean up partial index file to prevent corruption
            tracing::warn!("removing partial vector index after write failure: {e}");
            if let Err(rm_err) = std::fs::remove_file(&index_path) {
                tracing::error!(
                    "failed to remove partial index file {}: {rm_err}",
                    index_path.display()
                );
            }
            return Err(anyhow::anyhow!("{e}"));
        }

        writer
            .finish()
            .map_err(|err| anyhow::anyhow!("finish fsvi index failed: {err}"))?;

        FsVectorIndex::open(&index_path)
            .map_err(|err| anyhow::anyhow!("open fsvi index failed: {err}"))
    }

    /// Append new embeddings to an existing FSVI index via the WAL.
    ///
    /// Used for incremental semantic indexing in watch mode. Opens the
    /// existing index, appends a batch of new embeddings, and compacts if
    /// the WAL has grown large enough.
    ///
    /// Returns the number of entries appended.
    pub fn append_to_index(
        &self,
        embedded_messages: impl IntoIterator<Item = EmbeddedMessage>,
        data_dir: &Path,
    ) -> Result<usize> {
        let index_path = vector_index_path(data_dir, self.embedder_id());
        let mut index = FsVectorIndex::open(&index_path)
            .map_err(|err| anyhow::anyhow!("open fsvi index for append: {err}"))?;

        let entries: Vec<(String, Vec<f32>)> = embedded_messages
            .into_iter()
            .map(|em| {
                let doc_id = SemanticDocId {
                    message_id: em.message_id,
                    chunk_idx: em.chunk_idx,
                    agent_id: em.agent_id,
                    workspace_id: em.workspace_id,
                    source_id: em.source_id,
                    role: em.role,
                    created_at_ms: em.created_at_ms,
                    content_hash: Some(em.content_hash),
                }
                .to_doc_id_string();
                (doc_id, em.embedding)
            })
            .collect();

        let count = entries.len();
        if count == 0 {
            return Ok(0);
        }

        index
            .append_batch(&entries)
            .map_err(|err| anyhow::anyhow!("append_batch: {err}"))?;

        if index.needs_compaction() {
            index
                .compact()
                .map_err(|err| anyhow::anyhow!("compaction: {err}"))?;
        }

        Ok(count)
    }

    /// Build and save an HNSW index for approximate nearest neighbor search.
    ///
    /// This creates an HNSW graph structure from the existing VectorIndex,
    /// enabling O(log n) approximate search with the `--approximate` flag.
    ///
    /// # Arguments
    /// * `vector_index` - The VectorIndex to build HNSW from
    /// * `data_dir` - Directory to save the HNSW index
    /// * `m` - Max connections per node (default: 16)
    /// * `ef_construction` - Search width during build (default: 200)
    ///
    /// # Returns
    /// Path to the saved HNSW index file
    pub fn build_hnsw_index(
        &self,
        vector_index: &FsVectorIndex,
        data_dir: &Path,
        m: Option<usize>,
        ef_construction: Option<usize>,
    ) -> Result<PathBuf> {
        let m = m.unwrap_or(FS_HNSW_DEFAULT_M);
        let ef_construction = ef_construction.unwrap_or(FS_HNSW_DEFAULT_EF_CONSTRUCTION);

        tracing::info!(
            embedder = self.embedder_id(),
            count = vector_index.record_count(),
            m,
            ef_construction,
            "Building HNSW index for approximate nearest neighbor search"
        );

        let config = FsHnswConfig {
            m,
            ef_construction,
            ..FsHnswConfig::default()
        };
        let hnsw = FsHnswIndex::build_from_vector_index(vector_index, config)
            .map_err(|err| anyhow::anyhow!("build HNSW index failed: {err}"))?;

        let hnsw_path = hnsw_index_path(data_dir, self.embedder_id());
        if let Some(parent) = hnsw_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        hnsw.save(&hnsw_path)
            .map_err(|err| anyhow::anyhow!("save HNSW index failed: {err}"))?;

        tracing::info!(?hnsw_path, "Saved HNSW index");
        Ok(hnsw_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_batch_embedding() {
        let indexer = SemanticIndexer::new("hash", None).unwrap();
        let messages = vec![
            EmbeddingInput::new(1, "Hello world"),
            EmbeddingInput::new(2, "Goodbye world"),
        ];

        let embeddings = indexer.embed_messages(&messages).unwrap();

        assert_eq!(embeddings.len(), 2);
        assert_eq!(embeddings[0].message_id, 1);
        assert_eq!(embeddings[1].message_id, 2);
        assert_eq!(embeddings[0].embedding.len(), indexer.embedder_dimension());
    }

    #[test]
    fn test_progress_indicator() {
        let indexer = SemanticIndexer::new("hash", None).unwrap();
        let messages: Vec<_> = (0..1000)
            .map(|i| EmbeddingInput::new(i as u64, format!("Message {}", i)))
            .collect();

        let embeddings = indexer.embed_messages(&messages).unwrap();
        assert_eq!(embeddings.len(), messages.len());
    }

    #[test]
    fn test_build_and_save_index() {
        let indexer = SemanticIndexer::new("hash", None).unwrap();
        let messages = vec![
            EmbeddingInput::new(1, "Hello world"),
            EmbeddingInput::new(2, "Goodbye world"),
        ];

        let embeddings = indexer.embed_messages(&messages).unwrap();
        let tmp = tempdir().unwrap();
        let index = indexer
            .build_and_save_index(embeddings, tmp.path())
            .unwrap();
        assert_eq!(index.embedder_id(), indexer.embedder_id());
        assert_eq!(index.dimension(), indexer.embedder_dimension());
        assert_eq!(index.record_count(), 2);
    }

    /// Golden-output regression: any change to the embedding prep pipeline,
    /// the canonicalizer, the hash embedder's deterministic projection, or
    /// the ordering semantics of `embed_messages` must not silently mutate
    /// the bytes we write to the vector index. This digest is derived from a
    /// frozen 64-message corpus processed through the hash embedder; a
    /// mismatch means one of those contracts moved.
    #[test]
    fn embed_messages_golden_digest_hash_embedder() {
        use ring::digest::{Context, SHA256};

        let corpus: Vec<EmbeddingInput> = (0..64)
            .map(|i| {
                let body = match i % 5 {
                    0 => format!("plain text message number {i}"),
                    1 => format!("**bold** line {i} with _emphasis_"),
                    2 => format!("```rust\nfn f_{i}() {{ println!(\"{i}\"); }}\n```"),
                    3 => format!("   whitespace {i}   "),
                    _ => format!("unicode \u{00E9}\u{0301} + emoji \u{1F600} {i}"),
                };
                EmbeddingInput::new(i as u64, body)
            })
            .collect();

        let indexer = SemanticIndexer::new("hash", None)
            .unwrap()
            .with_batch_size(16)
            .unwrap();
        let embeddings = indexer.embed_messages(&corpus).unwrap();

        // Digest over (message_id, content_hash, embedding f32 bytes) for every
        // embedded message, in the order emitted. Preserves order + content +
        // numeric equality without having to compare raw floats directly.
        let mut ctx = Context::new(&SHA256);
        for em in &embeddings {
            ctx.update(&em.message_id.to_le_bytes());
            ctx.update(&em.content_hash);
            for v in &em.embedding {
                ctx.update(&v.to_le_bytes());
            }
        }
        let digest = hex::encode(ctx.finish().as_ref());

        // Captured 2026-04-21 against a freshly built hash embedder, batch
        // size 16, the frozen 64-message corpus above. Stable so long as
        // the prep pipeline, canonicalizer, and HashEmbedder::embed
        // implementation are all byte-preserving. If you intentionally
        // changed any of those, update this value AND record the reason
        // in the commit message.
        const EXPECTED: &str = "eb6f86cc3b4a87e28711705ba0c28f7f3cb5760796cbbea4d0f9177a0103a03f";
        assert_eq!(
            digest, EXPECTED,
            "embed_messages golden digest drifted; if this was intentional, \
             update EXPECTED in this test and record the reason in the commit message"
        );
    }

    #[test]
    fn parallel_prep_matches_serial_prep_bitwise() {
        // Mix of short, long, empty, markdown, code-block, and unicode inputs
        // to make sure the canonicalizer is exercised across all of its paths.
        let inputs: Vec<EmbeddingInput> = (0..500)
            .map(|i| {
                let text = match i % 7 {
                    0 => format!("Plain message number {i} with some ordinary words."),
                    1 => format!("**Bold** and _italic_ markdown line {i}"),
                    2 => format!(
                        "```rust\nfn example_{i}() {{\n    println!(\"code block {i}\");\n}}\n```\nfollow-up text"
                    ),
                    3 => String::new(), // empty — should be filtered
                    4 => format!("   whitespace   galore   {i}   "),
                    5 => format!("Unicode \u{00E9}\u{0301} (combining accent) and emoji \u{1F600} line {i}"),
                    _ => format!(
                        "Mixed line {i}: `inline_code`, [link](http://x), {{braces}}, and \u{201C}curly quotes\u{201D}."
                    ),
                };
                EmbeddingInput::new(i as u64, text)
            })
            .collect();

        let serial = prepare_window(&inputs, true);
        let parallel = prepare_window(&inputs, false);

        assert_eq!(
            serial.len(),
            parallel.len(),
            "serial and parallel prep should skip the same number of empty canonicals"
        );

        for (s, p) in serial.iter().zip(parallel.iter()) {
            assert_eq!(
                s.msg.message_id, p.msg.message_id,
                "ordering must be preserved between serial and parallel prep"
            );
            assert_eq!(
                s.canonical, p.canonical,
                "canonical form diverged between serial and parallel prep"
            );
            assert_eq!(
                s.hash, p.hash,
                "content hash diverged between serial and parallel prep"
            );
        }
    }

    #[test]
    fn parallel_prep_filters_empty_canonicals() {
        let inputs = vec![
            EmbeddingInput::new(1, "valid content"),
            EmbeddingInput::new(2, ""),
            EmbeddingInput::new(3, "   \n\n   \t  "),
            EmbeddingInput::new(4, "more valid content"),
        ];

        let prepared = prepare_window(&inputs, false);
        let ids: Vec<u64> = prepared.iter().map(|p| p.msg.message_id).collect();

        assert!(ids.contains(&1));
        assert!(ids.contains(&4));
        // ids 2 and 3 should be dropped because their canonicals are empty.
        assert!(!ids.contains(&2));
        assert!(!ids.contains(&3));
    }

    #[test]
    fn default_batch_size_uses_new_value() {
        // The test setup must not leak a caller-provided CASS_SEMANTIC_BATCH_SIZE
        // override, which would mask the constant bump we're asserting on.
        let prior = std::env::var("CASS_SEMANTIC_BATCH_SIZE").ok();
        // SAFETY: test-local env mutation.
        unsafe {
            std::env::remove_var("CASS_SEMANTIC_BATCH_SIZE");
        }
        let indexer = SemanticIndexer::new("hash", None).unwrap();
        assert_eq!(indexer.batch_size(), DEFAULT_SEMANTIC_BATCH_SIZE);
        // Restore whatever was there before.
        if let Some(v) = prior {
            // SAFETY: test-local env mutation.
            unsafe {
                std::env::set_var("CASS_SEMANTIC_BATCH_SIZE", v);
            }
        }
    }
}
