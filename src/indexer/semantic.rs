use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};

use crate::search::canonicalize::{canonicalize_for_embedding, content_hash};
use crate::search::embedder::Embedder;
use crate::search::fastembed_embedder::FastEmbedder;
use crate::search::hash_embedder::HashEmbedder;
use crate::search::vector_index::{Quantization, ROLE_USER, VectorEntry, VectorIndex, vector_index_path};

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

impl EmbeddedMessage {
    pub fn into_vector_entry(self) -> VectorEntry {
        VectorEntry {
            message_id: self.message_id,
            created_at_ms: self.created_at_ms,
            agent_id: self.agent_id,
            workspace_id: self.workspace_id,
            source_id: self.source_id,
            role: self.role,
            chunk_idx: self.chunk_idx,
            content_hash: self.content_hash,
            vector: self.embedding,
        }
    }
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
                .embed_batch(&texts)
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

    pub fn build_index<I>(&self, embedded_messages: I) -> Result<VectorIndex>
    where
        I: IntoIterator<Item = EmbeddedMessage>,
    {
        let entries = embedded_messages
            .into_iter()
            .map(|embedded| embedded.into_vector_entry());

        VectorIndex::build(
            self.embedder_id(),
            "1.0",
            self.embedder_dimension(),
            Quantization::F32,
            entries,
        )
    }

    pub fn save_index(&self, index: &VectorIndex, data_dir: &Path) -> Result<PathBuf> {
        let header = index.header();
        if header.embedder_id != self.embedder_id() {
            bail!(
                "embedder_id mismatch: index header '{}' vs indexer '{}'",
                header.embedder_id,
                self.embedder_id()
            );
        }
        if header.dimension as usize != self.embedder_dimension() {
            bail!(
                "dimension mismatch: index header {} vs indexer {}",
                header.dimension,
                self.embedder_dimension()
            );
        }

        let index_path = vector_index_path(data_dir, header.embedder_id.as_str());
        if let Some(parent) = index_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        index.save(&index_path)?;
        Ok(index_path)
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
        let index = indexer.build_index(embeddings).unwrap();

        let tmp = tempdir().unwrap();
        let path = indexer.save_index(&index, tmp.path()).unwrap();
        assert!(path.is_file());

        let loaded = VectorIndex::load(&path).unwrap();
        assert_eq!(loaded.header().embedder_id, indexer.embedder_id());
        assert_eq!(
            loaded.header().dimension,
            indexer.embedder_dimension() as u32
        );
    }
}
