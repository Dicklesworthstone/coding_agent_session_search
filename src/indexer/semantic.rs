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

use crate::search::canonicalize::{canonicalize_for_embedding, content_hash};
use crate::search::embedder::Embedder;
use crate::search::fastembed_embedder::FastEmbedder;
use crate::search::hash_embedder::HashEmbedder;
use crate::search::vector_index::{ROLE_USER, SemanticDocId, VECTOR_INDEX_DIR, vector_index_path};

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
            batch_size: 32,
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

        struct Prepared<'a> {
            msg: &'a EmbeddingInput,
            canonical: String,
            hash: [u8; 32],
        }

        let mut embeddings = Vec::with_capacity(messages.len());
        let mut batch: Vec<Prepared> = Vec::with_capacity(self.batch_size);

        let flush_batch = |batch: &mut Vec<Prepared>,
                           embeddings: &mut Vec<EmbeddedMessage>,
                           pb: &ProgressBar,
                           embedder: &dyn Embedder|
         -> Result<()> {
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

            for (prepared, vector) in batch.iter().zip(vectors.into_iter()) {
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
            batch.clear();
            Ok(())
        };

        for msg in messages {
            let canonical = canonicalize_for_embedding(&msg.content);
            if canonical.is_empty() {
                pb.inc(1);
                continue;
            }

            let hash = content_hash(&canonical);
            batch.push(Prepared {
                msg,
                canonical,
                hash,
            });

            if batch.len() >= self.batch_size {
                flush_batch(&mut batch, &mut embeddings, &pb, self.embedder.as_ref())?;
            }
        }

        if !batch.is_empty() {
            flush_batch(&mut batch, &mut embeddings, &pb, self.embedder.as_ref())?;
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
            let _ = std::fs::remove_file(&index_path);
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
}
