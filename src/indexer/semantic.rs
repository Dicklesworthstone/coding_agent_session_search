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

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};

use crate::indexer::semantic_progress::SemanticProgressSink;
use crate::search::semantic_manifest::{SemanticManifest, TierKind};
use crate::storage::sqlite::FrankenStorage;

/// Internal parent/worker contract. A nightly worker may finish the active
/// checkpoint, but must not replace another tier's or producer's cursor.
pub(crate) const SCHEDULE_PRESERVE_CHECKPOINT: &str = "CASS_SCHEDULE_PRESERVE_CHECKPOINT";

fn scheduled_worker_preserves_checkpoint() -> bool {
    matches!(
        std::env::var(SCHEDULE_PRESERVE_CHECKPOINT).as_deref(),
        Ok("1")
    )
}

/// An external worker's preflight is not a lease on its starting ledger. Retain the
/// admission until the first artifact lease verifies it, including when there
/// was no checkpoint yet. This is never authority to select a stored provider.
struct ScheduledProviderAdmission {
    data_dir: PathBuf,
    manifest: SemanticManifest,
}

impl ScheduledProviderAdmission {
    fn load(data_dir: &Path, selected: &str) -> Result<Self> {
        let manifest = SemanticManifest::load(data_dir)
            .context("semantic_checkpoint_unreadable: scheduled worker retained the ledger")?
            .unwrap_or_default();
        if let Some(checkpoint) = &manifest.checkpoint {
            let identity = if crate::search::embedder_registry::selects_external(Some(selected)) {
                // External producers are quality-only. Refuse foreign fast
                // ownership before resolving credentials or constructing HTTP.
                ensure_scheduled_checkpoint_owner(
                    &manifest,
                    TierKind::Quality,
                    &checkpoint.embedder_id,
                )?;
                crate::search::external_embedder::ExternalEmbeddingConfig::from_env()?
                    .context(
                        "external_disabled: consent is required to resume the external checkpoint",
                    )?
                    .identity()
            } else {
                crate::search::embedder_registry::EmbedderRegistry::new(data_dir)
                    .get(selected)
                    .context(
                        "semantic_checkpoint_owned: selected provider has no registered identity",
                    )?
                    .id
                    .to_owned()
            };
            // Local hash is also allowed in a quality plan; the exact tier is
            // checked when the caller supplies that plan, before artifact work.
            ensure_scheduled_checkpoint_owner(&manifest, checkpoint.tier, &identity)?;
        }
        Ok(Self {
            data_dir: data_dir.canonicalize()?,
            manifest,
        })
    }

    fn validate(&self, data_dir: &Path, input: &SemanticManifest) -> Result<()> {
        let admitted = &self.manifest;
        // Match the artifact lease's authority fields. Backlog estimates may
        // legitimately be refreshed between initialization and the first batch.
        if data_dir.canonicalize()? != self.data_dir
            || admitted.manifest_version != input.manifest_version
            || admitted.updated_at_ms != input.updated_at_ms
            || admitted.fast_tier != input.fast_tier
            || admitted.quality_tier != input.quality_tier
            || admitted.hnsw != input.hnsw
            || admitted.checkpoint != input.checkpoint
        {
            return Err(BackfillManifestChanged.into());
        }
        Ok(())
    }
}

fn ensure_scheduled_checkpoint_owner(
    manifest: &SemanticManifest,
    tier: TierKind,
    embedder_id: &str,
) -> Result<()> {
    anyhow::ensure!(
        manifest.checkpoint.as_ref().is_none_or(|checkpoint| {
            checkpoint.tier == tier && checkpoint.embedder_id == embedder_id
        }),
        "semantic_checkpoint_owned: another tier or embedding space has unfinished work; resume its configured provider before switching; the scheduled worker did not replace its checkpoint"
    );
    Ok(())
}

/// Admit a scheduled command under its canonical maintenance lock, before
/// loading weights, connecting an endpoint, or opening writable storage. Keep
/// this exact ledger snapshot through the later artifact-lease validation.
/// Stored identities only constrain an explicitly selected provider; they
/// never select a provider or authorize network traffic.
pub(crate) fn load_scheduled_checkpoint(
    data_dir: &Path,
    tier: TierKind,
    selected: &str,
    retained_identity: Option<&str>,
) -> Result<SemanticManifest> {
    let manifest = SemanticManifest::load(data_dir)
        .context("semantic_checkpoint_unreadable: scheduled backfill retained the ledger")?
        .unwrap_or_default();
    if let Some(checkpoint) = &manifest.checkpoint {
        // Reject a foreign tier before even resolving external configuration.
        ensure_scheduled_checkpoint_owner(&manifest, tier, &checkpoint.embedder_id)?;
        let identity = if let Some(id) = retained_identity {
            id.to_owned()
        } else if crate::search::embedder_registry::selects_external(Some(selected)) {
            crate::search::external_embedder::ExternalEmbeddingConfig::from_env()?
                .context(
                    "external_disabled: consent is required to resume the external checkpoint",
                )?
                .identity()
        } else {
            crate::search::embedder_registry::EmbedderRegistry::new(data_dir)
                .get(selected)
                .context("semantic_checkpoint_owned: selected provider has no registered identity")?
                .id
                .to_owned()
        };
        ensure_scheduled_checkpoint_owner(&manifest, tier, &identity)?;
    }
    Ok(manifest)
}

/// Semantic indexer with lock-scoped ownership of backfill scratch artifacts.
/// Embedding and non-backfill operations retain the engine's existing API.
pub struct SemanticIndexer {
    inner: engine::SemanticIndexer,
    startup_admission: Mutex<Option<ScheduledProviderAdmission>>,
}

impl SemanticIndexer {
    pub fn new(embedder_type: &str, data_dir: Option<&Path>) -> Result<Self> {
        Self::construct(
            embedder_type,
            data_dir,
            crate::search::embedder_registry::selects_external(Some(embedder_type))
                && scheduled_worker_preserves_checkpoint(),
            || engine::SemanticIndexer::new(embedder_type, data_dir),
        )
    }

    /// Retain the existing artifact lease and checkpoint/publication path while
    /// sharing cancellation with the explicitly selected endpoint provider.
    pub fn new_with_cancel(
        embedder_type: &str,
        data_dir: Option<&Path>,
        cancelled: crate::search::external_embedder::CancelCheck,
    ) -> Result<Self> {
        Self::construct(
            embedder_type,
            data_dir,
            crate::search::embedder_registry::selects_external(Some(embedder_type))
                && scheduled_worker_preserves_checkpoint(),
            || engine::SemanticIndexer::new_with_cancel(embedder_type, data_dir, cancelled),
        )
    }

    fn construct(
        selected: &str,
        data_dir: Option<&Path>,
        preserve_checkpoint: bool,
        initialize: impl FnOnce() -> Result<engine::SemanticIndexer>,
    ) -> Result<Self> {
        let external = crate::search::embedder_registry::selects_external(Some(selected));
        let result = (|| {
            let admission = if preserve_checkpoint {
                Some(ScheduledProviderAdmission::load(
                    data_dir.context("semantic_checkpoint_owned: scheduled worker requires its data directory before provider initialization")?,
                    selected,
                )?)
            } else {
                None
            };
            // No model load or endpoint preflight occurs until ownership and
            // readable-ledger checks have succeeded. Defaults keep the old path.
            let inner = initialize()?;
            if let Some(checkpoint) = admission
                .as_ref()
                .and_then(|admission| admission.manifest.checkpoint.as_ref())
            {
                anyhow::ensure!(
                    checkpoint.embedder_id == inner.embedder_id(),
                    "semantic_checkpoint_owned: initialized provider differs from the admitted owner"
                );
            }
            Ok(Self {
                inner,
                startup_admission: Mutex::new(admission),
            })
        })();
        external_diagnostic_result(external, result)
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
        .map(|inner| Self {
            inner,
            startup_admission: Mutex::new(None),
        })
    }

    pub fn with_batch_size(self, batch_size: usize) -> Result<Self> {
        let Self {
            inner,
            startup_admission,
        } = self;
        inner.with_batch_size(batch_size).map(|inner| Self {
            inner,
            startup_admission,
        })
    }

    fn admit_scheduled_checkpoint(
        &self,
        manifest: &SemanticManifest,
        tier: TierKind,
    ) -> Result<()> {
        if scheduled_worker_preserves_checkpoint() {
            ensure_scheduled_checkpoint_owner(manifest, tier, self.inner.embedder_id())?;
        }
        Ok(())
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
        let mut startup_admission = self
            .startup_admission
            .try_lock()
            .map_err(|error| match error {
                std::sync::TryLockError::Poisoned(_) => {
                    anyhow::anyhow!("semantic_checkpoint_owned: worker admission lock poisoned")
                }
                std::sync::TryLockError::WouldBlock => {
                    anyhow::anyhow!("semantic_checkpoint_owned: another call is admitting this worker; retry without replacing its checkpoint")
                }
            })?;
        if let Some(admission) = startup_admission.as_ref() {
            // The CLI may have loaded a newer ledger after blocking preflight.
            // A replacement is not a newly authorized starting checkpoint.
            admission.validate(data_dir, manifest)?;
        }
        // begin validates this exact input checkpoint against the durable
        // ledger under its lease BEFORE reclamation. A new owner appearing
        // after the scheduler planned the run therefore cannot be overwritten.
        let artifacts = artifacts::BackfillArtifacts::begin(data_dir, manifest)?;
        // The first lease has now admitted the captured ledger. Later batches
        // use their normal fresh input/lease checks; don't pin an old cursor.
        *startup_admission = None;
        drop(startup_admission);
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
        self.admit_scheduled_checkpoint(manifest, plan.tier)?;
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
        self.admit_scheduled_checkpoint(manifest, plan.tier)?;
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
        self.admit_scheduled_checkpoint(manifest, plan.tier)?;
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

    #[test]
    fn scheduled_admission_uses_retained_identity_and_keeps_the_admitted_snapshot() {
        use crate::search::semantic_manifest::BuildCheckpoint;
        let dir = tempfile::tempdir().unwrap();
        // Without a checkpoint, normal provider initialization owns admission.
        // Even an unrecognized provider is not resolved by this read-only step.
        assert!(
            load_scheduled_checkpoint(dir.path(), TierKind::Fast, "not-a-provider", None)
                .unwrap()
                .checkpoint
                .is_none()
        );
        let mut manifest = SemanticManifest::default();
        manifest.checkpoint = Some(BuildCheckpoint {
            tier: TierKind::Quality,
            embedder_id: "retained-provider".into(),
            last_offset: 1,
            docs_embedded: 1,
            conversations_processed: 1,
            total_conversations: 3,
            db_fingerprint: "old-archive-fingerprint".into(),
            schema_version: crate::search::policy::SEMANTIC_SCHEMA_VERSION,
            chunking_version: crate::search::policy::CHUNKING_STRATEGY_VERSION,
            saved_at_ms: 1,
            last_message_id: Some(1),
            cursor_exhausted: false,
        });
        manifest.save(dir.path()).unwrap();
        let before = std::fs::read(SemanticManifest::path(dir.path())).unwrap();
        // Use the already initialized provider; don't re-resolve an endpoint
        // from ambient configuration in the middle of a multi-batch command.
        let admitted = load_scheduled_checkpoint(
            dir.path(),
            TierKind::Quality,
            "external",
            Some("retained-provider"),
        )
        .unwrap();
        assert_eq!(admitted.checkpoint, manifest.checkpoint);
        assert_eq!(
            std::fs::read(SemanticManifest::path(dir.path())).unwrap(),
            before
        );
        assert!(
            load_scheduled_checkpoint(
                dir.path(),
                TierKind::Quality,
                "external",
                Some("different-retained-provider"),
            )
            .is_err()
        );

        // A writer after planning/admission must cause a lease refusal rather
        // than have its checkpoint silently loaded and treated as permission.
        manifest.checkpoint.as_mut().unwrap().embedder_id = "concurrent-provider".into();
        manifest.save(dir.path()).unwrap();
        let current = std::fs::read(SemanticManifest::path(dir.path())).unwrap();
        let error = match artifacts::BackfillArtifacts::begin(dir.path(), &admitted) {
            Ok(_) => panic!("stale scheduled admission must not enter reclamation"),
            Err(error) => error,
        };
        assert!(error.downcast_ref::<BackfillManifestChanged>().is_some());
        assert_eq!(
            std::fs::read(SemanticManifest::path(dir.path())).unwrap(),
            current
        );
    }

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

    #[test]
    fn scheduled_guard_requires_both_the_checkpoint_tier_and_producer() {
        use crate::search::semantic_manifest::BuildCheckpoint;
        let mut manifest = SemanticManifest::default();
        assert!(ensure_scheduled_checkpoint_owner(&manifest, TierKind::Fast, "fnv1a-384").is_ok());
        manifest.checkpoint = Some(BuildCheckpoint {
            tier: TierKind::Quality,
            embedder_id: "original-producer".into(),
            last_offset: 1,
            docs_embedded: 1,
            conversations_processed: 1,
            total_conversations: 3,
            db_fingerprint: "old-archive-fingerprint".into(),
            schema_version: crate::search::policy::SEMANTIC_SCHEMA_VERSION,
            chunking_version: crate::search::policy::CHUNKING_STRATEGY_VERSION,
            saved_at_ms: 1,
            last_message_id: Some(1),
            cursor_exhausted: false,
        });
        let before = serde_json::to_vec(&manifest).unwrap();
        for (tier, id) in [
            (TierKind::Fast, "original-producer"),
            (TierKind::Fast, "fnv1a-384"),
            (TierKind::Quality, "changed-producer"),
        ] {
            let error = ensure_scheduled_checkpoint_owner(&manifest, tier, id).unwrap_err();
            assert!(error.to_string().contains("semantic_checkpoint_owned"));
            assert_eq!(serde_json::to_vec(&manifest).unwrap(), before);
        }
        assert!(
            ensure_scheduled_checkpoint_owner(&manifest, TierKind::Quality, "original-producer")
                .is_ok()
        );
        assert_eq!(serde_json::to_vec(&manifest).unwrap(), before);
    }

    fn worker_checkpoint() -> crate::search::semantic_manifest::BuildCheckpoint {
        crate::search::semantic_manifest::BuildCheckpoint {
            tier: TierKind::Fast,
            embedder_id: "fnv1a-384".into(),
            last_offset: 1,
            docs_embedded: 1,
            conversations_processed: 1,
            total_conversations: 3,
            db_fingerprint: "worker-fixture".into(),
            schema_version: crate::search::policy::SEMANTIC_SCHEMA_VERSION,
            chunking_version: crate::search::policy::CHUNKING_STRATEGY_VERSION,
            saved_at_ms: 1,
            last_message_id: Some(1),
            cursor_exhausted: false,
        }
    }

    #[test]
    fn worker_refuses_foreign_and_corrupt_ledgers_before_initialization() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut checkpoint = worker_checkpoint();
        checkpoint.embedder_id = "another-producer".into();
        let mut manifest = SemanticManifest {
            checkpoint: Some(checkpoint),
            ..SemanticManifest::default()
        };
        manifest.save(dir.path())?;
        let path = SemanticManifest::path(dir.path());
        let before = std::fs::read(&path)?;
        let result = SemanticIndexer::construct("hash", Some(dir.path()), true, || {
            panic!("foreign ownership must be rejected before provider construction")
        });
        let error = match result {
            Ok(_) => panic!("foreign ownership was admitted"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("semantic_checkpoint_owned"));
        assert_eq!(std::fs::read(&path)?, before);

        std::fs::write(&path, b"{broken-checkpoint")?;
        let result = SemanticIndexer::construct("external", Some(dir.path()), true, || {
            panic!("corrupt ledger must be rejected before endpoint configuration or preflight")
        });
        let error = match result {
            Ok(_) => panic!("unreadable ownership was admitted"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("semantic_checkpoint_unreadable"));
        assert_eq!(std::fs::read(&path)?, b"{broken-checkpoint");
        // Ordinary foreground construction does not acquire this restriction.
        let foreground = SemanticIndexer::construct("hash", Some(dir.path()), false, || {
            engine::SemanticIndexer::new("hash", Some(dir.path()))
        })?;
        assert_eq!(foreground.embedder_id(), "fnv1a-384");
        assert_eq!(std::fs::read(&path)?, b"{broken-checkpoint");
        Ok(())
    }

    #[test]
    fn worker_retains_starting_ledger_across_preflight_and_batch_size_changes() -> Result<()> {
        for started_with_checkpoint in [false, true] {
            let dir = tempfile::tempdir()?;
            let mut original = SemanticManifest {
                checkpoint: started_with_checkpoint.then(worker_checkpoint),
                ..SemanticManifest::default()
            };
            original.save(dir.path())?;
            let worker = SemanticIndexer::construct("hash", Some(dir.path()), true, || {
                // Interleave a new cursor where real provider preflight runs.
                // Both owners deliberately have the same tier and producer.
                let mut replacement = SemanticManifest::load(dir.path())?.unwrap();
                replacement.checkpoint = Some(worker_checkpoint());
                replacement.checkpoint.as_mut().unwrap().last_offset = 7;
                replacement.save(dir.path())?;
                engine::SemanticIndexer::new("hash", Some(dir.path()))
            })?
            .with_batch_size(2)?;
            let path = SemanticManifest::path(dir.path());
            let before = std::fs::read(&path)?;
            let reloaded = SemanticManifest::load(dir.path())?.unwrap();
            for mut input in [reloaded, original] {
                let result = worker.with_backfill_artifacts(dir.path(), &mut input, |_, _| {
                    panic!(
                        "neither a reloaded nor a stale input may enter embedding or publication"
                    )
                });
                let error = result.unwrap_err();
                assert!(error.downcast_ref::<BackfillManifestChanged>().is_some());
                assert_eq!(std::fs::read(&path)?, before);
                assert!(worker.startup_admission.lock().unwrap().is_some());
            }
        }
        Ok(())
    }

    #[test]
    fn worker_first_lease_releases_startup_snapshot_for_later_durable_batches() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let worker = SemanticIndexer::construct("hash", Some(dir.path()), true, || {
            engine::SemanticIndexer::new("hash", Some(dir.path()))
        })?;
        let mut manifest = SemanticManifest::default();
        // Progress estimates are not cursor/publication authority.
        manifest.backlog.total_conversations = 2;
        let plan = |last_offset, exhausted| SemanticBackfillBatchPlan {
            tier: TierKind::Fast,
            db_fingerprint: "worker-fixture".into(),
            model_revision: "hash-fixture".into(),
            total_conversations: 2,
            conversations_in_batch: 1,
            last_offset,
            cursor_exhausted: exhausted,
        };
        let first = worker.run_backfill_batch(
            &[EmbeddingInput::new(1, "first durable worker message")],
            dir.path(),
            &mut manifest,
            plan(1, false),
        )?;
        assert!(first.checkpoint_saved && !first.published);
        assert!(worker.startup_admission.lock().unwrap().is_none());
        let second = worker.run_backfill_batch(
            &[EmbeddingInput::new(2, "second durable worker message")],
            dir.path(),
            &mut manifest,
            plan(2, true),
        )?;
        assert!(second.published);
        assert!(manifest.checkpoint.is_none());
        let index = frankensearch::index::VectorIndex::open_read_only(&second.index_path)?;
        assert_eq!(index.record_count(), 2);
        assert_eq!(index.embedder_id(), "fnv1a-384");
        Ok(())
    }
}
