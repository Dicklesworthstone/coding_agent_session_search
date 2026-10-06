//! Semantic indexing and the ownership boundary for resumable backfill artifacts.
//!
//! The engine performs embedding/publication; this facade holds the artifact
//! lease across each complete backfill call. Cleanup is deliberately outside
//! the engine's error paths: a failed save may have renamed the manifest without
//! making that rename durable, so it must not authorize retiring older staging.

mod artifacts;
mod delegate;
mod engine;
#[cfg(unix)]
mod unchanged;

pub use artifacts::{
    BackfillArtifactCandidate, BackfillArtifactReclaimPlan, BackfillArtifactReclaimReport,
    BackfillManifestChanged, apply_backfill_artifact_plan, plan_backfill_artifacts,
    reclaim_backfill_artifacts,
};
pub use engine::*;

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

impl SemanticIndexer {
    pub fn new(embedder_type: &str, data_dir: Option<&Path>) -> Result<Self> {
        external_diagnostic_result(
            crate::search::embedder_registry::selects_external(Some(embedder_type)),
            engine::SemanticIndexer::new(embedder_type, data_dir),
        )
        .map(|inner| Self { inner })
    }

    /// Retain the existing artifact lease and checkpoint/publication path while
    /// sharing cancellation with the explicitly selected endpoint provider.
    pub fn new_with_cancel(
        embedder_type: &str,
        data_dir: Option<&Path>,
        cancelled: crate::search::external_embedder::CancelCheck,
    ) -> Result<Self> {
        external_diagnostic_result(
            crate::search::embedder_registry::selects_external(Some(embedder_type)),
            engine::SemanticIndexer::new_with_cancel(embedder_type, data_dir, cancelled),
        )
        .map(|inner| Self { inner })
    }

    /// Explicit, consented configuration for embedding/backfill job owners.
    pub fn with_external_config(
        config: crate::search::external_embedder::ExternalEmbeddingConfig,
        cancelled: crate::search::external_embedder::CancelCheck,
    ) -> Result<Self> {
        external_diagnostic_result(
            true,
            engine::SemanticIndexer::with_external_config(config, cancelled),
        )
        .map(|inner| Self { inner })
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
        F: FnOnce(
            &engine::SemanticIndexer,
            &mut SemanticManifest,
        ) -> Result<SemanticBackfillBatchOutcome>,
    {
        self.inner.check_external_cancelled()?;
        let artifacts = artifacts::BackfillArtifacts::begin(data_dir, manifest)?;
        // The ownership lease may have waited behind another writer.
        self.inner.check_external_cancelled()?;
        let result = run(&self.inner, manifest);
        if let Ok(outcome) = &result {
            // A writer returns after manifest.save's file/directory fsync;
            // a proved no-op retains the already durable publication. Temporary
            // snapshots/readers have closed in either case.
            // Never do this in Drop: an error or unwind is not a durable commit.
            artifacts.after_success(manifest, &outcome.index_path);
        }
        external_diagnostic_result(
            crate::search::external_embedder::is_external_identity(self.inner.embedder_id()),
            result,
        )
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
                messages,
                data_dir,
                manifest,
                plan,
                last_message_id,
                sink,
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
            #[cfg(unix)]
            if let Some(outcome) =
                unchanged::try_retain_completed(engine, storage, data_dir, manifest, &plan, sink)?
            {
                return Ok(outcome);
            }
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
            #[cfg(unix)]
            if let Some(outcome) =
                unchanged::try_retain_completed(engine, storage, data_dir, manifest, &plan, sink)?
            {
                return Ok(outcome);
            }
            engine
                .run_capped_backfill_from_storage_with_sink(storage, data_dir, manifest, plan, sink)
        })
    }
}

#[cfg(test)]
mod artifact_lifecycle_tests;

/// Legacy CLI callers display only the outer anyhow context. For an explicitly
/// selected endpoint, retain its sanitized cause in that display while keeping
/// the original error chain and typed downcasts intact. Local errors are unchanged.
fn external_diagnostic_result<T>(external: bool, result: Result<T>) -> Result<T> {
    result.map_err(|error| {
        if external {
            let diagnostic = format!("{error:#}");
            error.context(diagnostic)
        } else {
            error
        }
    })
}

#[cfg(test)]
mod external_diagnostic_tests {
    use super::*;

    fn failure() -> anyhow::Error {
        anyhow::Error::new(std::io::Error::other(
            "external_dimension_mismatch: expected 384 dimensions",
        ))
        .context("external provider preflight failed")
    }

    #[test]
    fn external_display_preserves_provider_cause_and_typed_error() {
        let error = external_diagnostic_result::<()>(true, Err(failure())).unwrap_err();
        assert!(error.to_string().contains("external_dimension_mismatch"));
        assert!(
            error
                .to_string()
                .contains("external provider preflight failed")
        );
        assert!(error.downcast_ref::<std::io::Error>().is_some());
    }

    #[test]
    fn local_display_and_success_are_unchanged() {
        let error = external_diagnostic_result::<()>(false, Err(failure())).unwrap_err();
        assert_eq!(error.to_string(), "external provider preflight failed");
        assert!(error.downcast_ref::<std::io::Error>().is_some());
        assert_eq!(external_diagnostic_result(true, Ok(7)).unwrap(), 7);
    }
}
