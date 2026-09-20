//! Semantic indexing and the ownership boundary for resumable backfill artifacts.
//!
//! The engine performs embedding/publication; this facade holds the artifact
//! lease across each complete backfill call. Cleanup is deliberately outside
//! the engine's error paths: a failed save may have renamed the manifest without
//! making that rename durable, so it must not authorize retiring older staging.

mod artifacts;
mod engine;

pub use artifacts::{BackfillArtifactReclaimReport, reclaim_backfill_artifacts};
pub use engine::*;

use std::ops::Deref;
use std::path::Path;

use anyhow::Result;

use crate::indexer::semantic_progress::SemanticProgressSink;
use crate::search::semantic_manifest::SemanticManifest;
use crate::storage::sqlite::FrankenStorage;

/// Semantic indexer with lock-scoped ownership of backfill scratch artifacts.
/// Embedding and non-backfill operations retain the engine's existing API.
pub struct SemanticIndexer {
    inner: engine::SemanticIndexer,
}

impl Deref for SemanticIndexer {
    type Target = engine::SemanticIndexer;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl SemanticIndexer {
    pub fn new(embedder_type: &str, data_dir: Option<&Path>) -> Result<Self> {
        engine::SemanticIndexer::new(embedder_type, data_dir).map(|inner| Self { inner })
    }

    pub fn with_batch_size(self, batch_size: usize) -> Result<Self> {
        self.inner
            .with_batch_size(batch_size)
            .map(|inner| Self { inner })
    }

    fn with_backfill_artifacts<F>(
        &self,
        data_dir: &Path,
        manifest: &mut SemanticManifest,
        run: F,
    ) -> Result<SemanticBackfillBatchOutcome>
    where
        F: FnOnce(&engine::SemanticIndexer, &mut SemanticManifest)
            -> Result<SemanticBackfillBatchOutcome>,
    {
        let artifacts = artifacts::BackfillArtifacts::begin(data_dir, manifest)?;
        let result = run(&self.inner, manifest);
        if let Ok(outcome) = &result {
            // The engine has returned only after manifest.save's file and
            // directory fsync, and its temporary snapshots/readers have closed.
            // Never do this in Drop: an error or unwind is not a durable commit.
            artifacts.after_success(manifest, &outcome.index_path);
        }
        result
    }

    pub fn run_backfill_batch(
        &self,
        messages: &[EmbeddingInput],
        data_dir: &Path,
        manifest: &mut SemanticManifest,
        plan: SemanticBackfillBatchPlan,
    ) -> Result<SemanticBackfillBatchOutcome> {
        self.run_backfill_batch_with_sink(
            messages,
            data_dir,
            manifest,
            plan,
            None,
            &SemanticProgressSink::disabled(),
        )
    }

    pub fn run_backfill_batch_with_sink(
        &self,
        messages: &[EmbeddingInput],
        data_dir: &Path,
        manifest: &mut SemanticManifest,
        plan: SemanticBackfillBatchPlan,
        last_message_id: Option<i64>,
        sink: &SemanticProgressSink,
    ) -> Result<SemanticBackfillBatchOutcome> {
        self.with_backfill_artifacts(data_dir, manifest, |engine, manifest| {
            engine.run_backfill_batch_with_sink(
                messages, data_dir, manifest, plan, last_message_id, sink,
            )
        })
    }

    pub fn run_backfill_from_storage(
        &self,
        storage: &FrankenStorage,
        data_dir: &Path,
        manifest: &mut SemanticManifest,
        plan: SemanticBackfillStoragePlan,
    ) -> Result<SemanticBackfillBatchOutcome> {
        self.run_backfill_from_storage_with_sink(
            storage,
            data_dir,
            manifest,
            plan,
            &SemanticProgressSink::disabled(),
        )
    }

    pub fn run_backfill_from_storage_with_sink(
        &self,
        storage: &FrankenStorage,
        data_dir: &Path,
        manifest: &mut SemanticManifest,
        plan: SemanticBackfillStoragePlan,
        sink: &SemanticProgressSink,
    ) -> Result<SemanticBackfillBatchOutcome> {
        self.with_backfill_artifacts(data_dir, manifest, |engine, manifest| {
            engine.run_backfill_from_storage_with_sink(storage, data_dir, manifest, plan, sink)
        })
    }

    pub fn run_capped_backfill_from_storage_with_sink(
        &self,
        storage: &FrankenStorage,
        data_dir: &Path,
        manifest: &mut SemanticManifest,
        plan: SemanticBackfillStoragePlan,
        sink: &SemanticProgressSink,
    ) -> Result<SemanticBackfillBatchOutcome> {
        self.with_backfill_artifacts(data_dir, manifest, |engine, manifest| {
            engine.run_capped_backfill_from_storage_with_sink(storage, data_dir, manifest, plan, sink)
        })
    }
}

#[cfg(test)]
mod artifact_lifecycle_tests;
