// Allow dead-code warnings for this module until downstream slices wire the
// manifest into the rebuild pipeline. The types, helpers, and unit tests here
// are the foundation ibuuh.30 needs in place before scratch-build integration
// and crash-resume lands.
#![allow(dead_code)]

//! Lexical generation manifests and publish-state vocabulary (bead ibuuh.30).
//!
//! The authoritative lexical rebuild pipeline from bead ibuuh.29 emits an
//! equivalence ledger proving what it ingested, but publish semantics are still
//! "one mutable `<data_dir>/index` directory". That leaves ordinary search
//! vulnerable to half-built artifacts during rebuild, crash, or parallel
//! experimentation.
//!
//! This module lands the *vocabulary* for the generation-based publish path:
//! a versioned manifest that describes a single lexical generation's
//! identity, build state, publish state, source fingerprint, and failure
//! history, plus atomic load / store helpers. It is intentionally isolated
//! from the rebuild pipeline in this slice; downstream slices will wire the
//! authoritative rebuild to produce these manifests in scratch directories,
//! promote them to `published`, and teach startup recovery to choose the
//! right generation.
//!
//! Invariants the type enforces:
//! - The schema version is explicit so future migrations can refuse or
//!   upgrade older manifests cleanly.
//! - Build state and publish state are independent enums so the lifecycle
//!   ("built but not yet validated", "validated but not yet published",
//!   "published but superseded") is representable without overloading a
//!   single state field.
//! - Failure history is an append-only log so crash-resume tooling can see
//!   why previous attempts were abandoned, including which attempt id, at
//!   which phase, and with what message.
//! - Counts and fingerprints live alongside state so a single manifest
//!   answers both "is this generation safe to serve?" and "does it
//!   correspond to the expected DB?".

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Current manifest format version. Bump whenever the struct layout changes
/// in a way that older or newer readers cannot ignore.
pub(crate) const LEXICAL_GENERATION_MANIFEST_VERSION: u32 = 1;

/// File name used inside a generation directory for the manifest artifact.
pub(crate) const LEXICAL_GENERATION_MANIFEST_FILE: &str = "lexical-generation-manifest.json";

/// Build-side lifecycle: what the rebuild has accomplished for this
/// generation so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LexicalGenerationBuildState {
    /// The generation directory exists but no docs have been written yet.
    Scratch,
    /// Docs are being written; the writer holds exclusive access.
    Building,
    /// Writer finished cleanly; artifacts are present but have not yet been
    /// validated against the equivalence ledger or schema expectations.
    Built,
    /// Validation is running (manifest fingerprint check, doc-count parity,
    /// golden-query digest check, Tantivy open probe, ...).
    Validating,
    /// Validation succeeded; the generation is a candidate for publish.
    Validated,
    /// Validation failed; the generation must not be served. The failure
    /// reason is recorded in `failure_history`.
    Failed,
}

/// Publish-side lifecycle: whether this generation is the live search
/// surface, has been superseded by a newer one, or has been quarantined for
/// forensic inspection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LexicalGenerationPublishState {
    /// Generation exists on disk but has not been offered to search yet.
    Staged,
    /// Generation is the current live search surface.
    Published,
    /// Generation was live at some point but a newer generation replaced it.
    Superseded,
    /// Generation is quarantined: keep the artifacts on disk for inspection
    /// but never serve them. Used for debugging failed rebuilds.
    Quarantined,
}

/// Single entry in a generation's append-only failure log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LexicalGenerationFailure {
    /// Distinct attempt identity; different from `generation_id` because
    /// multiple attempts can be made before one succeeds.
    pub attempt_id: String,
    /// Unix ms at which the failure was observed.
    pub at_ms: i64,
    /// Coarse classification: "build", "validate", "publish", "recover".
    pub phase: String,
    /// Operator-readable message explaining the failure.
    pub message: String,
}

/// Canonical manifest describing one lexical rebuild generation.
///
/// Stored atomically at `<generation_dir>/lexical-generation-manifest.json`
/// via [`store_manifest`]. The entire manifest is re-serialized on every
/// state transition so crash-resume readers always see a consistent snapshot
/// rather than a partial update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LexicalGenerationManifest {
    /// Schema version for this manifest format.
    pub manifest_version: u32,
    /// Monotone, opaque generation identity. Recommended convention is a
    /// zero-padded sequence number combined with a short random suffix so
    /// simultaneous builds do not collide.
    pub generation_id: String,
    /// Attempt identity for the most recent build attempt that wrote this
    /// manifest. Rolls forward on retries while `generation_id` stays fixed
    /// for a logically planned generation.
    pub attempt_id: String,
    /// Unix ms at which the manifest was first created.
    pub created_at_ms: i64,
    /// Unix ms at which the manifest was last updated (build_state or
    /// publish_state transition, failure log append, etc.).
    pub updated_at_ms: i64,
    /// Source DB fingerprint the generation was built against. Kept aligned
    /// with the lexical-rebuild-state.json fingerprint so comparisons are
    /// trivial.
    pub source_db_fingerprint: String,
    /// Total conversations observed by the rebuild.
    pub conversation_count: u64,
    /// Total canonical messages observed by the rebuild.
    pub message_count: u64,
    /// Total indexed lexical documents committed to the generation.
    pub indexed_doc_count: u64,
    /// Optional pointer to the equivalence ledger fingerprint (bead
    /// ibuuh.29) so generation acceptance can cross-check the streaming
    /// accumulator digest.
    pub equivalence_manifest_fingerprint: Option<String>,
    pub build_state: LexicalGenerationBuildState,
    pub publish_state: LexicalGenerationPublishState,
    /// Append-only history of attempts that failed under this
    /// `generation_id`. Latest entry is the most recent failure.
    pub failure_history: Vec<LexicalGenerationFailure>,
}

impl LexicalGenerationManifest {
    /// Create a fresh manifest in Scratch/Staged state for the given
    /// generation id, attempt id, and source db fingerprint. Caller fills
    /// in counts as the build progresses.
    pub(crate) fn new_scratch(
        generation_id: impl Into<String>,
        attempt_id: impl Into<String>,
        source_db_fingerprint: impl Into<String>,
        now_ms: i64,
    ) -> Self {
        Self {
            manifest_version: LEXICAL_GENERATION_MANIFEST_VERSION,
            generation_id: generation_id.into(),
            attempt_id: attempt_id.into(),
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
            source_db_fingerprint: source_db_fingerprint.into(),
            conversation_count: 0,
            message_count: 0,
            indexed_doc_count: 0,
            equivalence_manifest_fingerprint: None,
            build_state: LexicalGenerationBuildState::Scratch,
            publish_state: LexicalGenerationPublishState::Staged,
            failure_history: Vec::new(),
        }
    }

    /// Record a build-state transition, bumping `updated_at_ms`.
    pub(crate) fn transition_build(&mut self, state: LexicalGenerationBuildState, now_ms: i64) {
        self.build_state = state;
        self.updated_at_ms = now_ms;
    }

    /// Record a publish-state transition, bumping `updated_at_ms`.
    pub(crate) fn transition_publish(&mut self, state: LexicalGenerationPublishState, now_ms: i64) {
        self.publish_state = state;
        self.updated_at_ms = now_ms;
    }

    /// Append a failure record and bump `updated_at_ms`. Callers should set
    /// `build_state` to [`LexicalGenerationBuildState::Failed`] separately
    /// when the failure is terminal for the attempt.
    pub(crate) fn record_failure(
        &mut self,
        attempt_id: impl Into<String>,
        phase: impl Into<String>,
        message: impl Into<String>,
        now_ms: i64,
    ) {
        self.failure_history.push(LexicalGenerationFailure {
            attempt_id: attempt_id.into(),
            at_ms: now_ms,
            phase: phase.into(),
            message: message.into(),
        });
        self.updated_at_ms = now_ms;
    }

    /// Whether this generation is safe to serve to ordinary search queries.
    pub(crate) fn is_serveable(&self) -> bool {
        matches!(self.build_state, LexicalGenerationBuildState::Validated)
            && matches!(self.publish_state, LexicalGenerationPublishState::Published)
    }
}

/// Canonical manifest path inside a generation directory.
pub(crate) fn manifest_path(generation_dir: &Path) -> PathBuf {
    generation_dir.join(LEXICAL_GENERATION_MANIFEST_FILE)
}

/// Current unix time in milliseconds, saturating on clock rollback.
pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|delta| i64::try_from(delta.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Atomically write a manifest to `<generation_dir>/lexical-generation-manifest.json`.
///
/// Uses tmp-file + rename so partial writes are never observable to
/// readers. The parent directory is created if necessary.
pub(crate) fn store_manifest(
    generation_dir: &Path,
    manifest: &LexicalGenerationManifest,
) -> Result<()> {
    fs::create_dir_all(generation_dir).with_context(|| {
        format!(
            "creating lexical generation directory {}",
            generation_dir.display()
        )
    })?;
    let final_path = manifest_path(generation_dir);
    let tmp_path = generation_dir.join(format!(
        "{}.tmp-{}",
        LEXICAL_GENERATION_MANIFEST_FILE, manifest.attempt_id
    ));
    let serialized =
        serde_json::to_vec_pretty(manifest).context("serializing lexical generation manifest")?;
    fs::write(&tmp_path, &serialized).with_context(|| {
        format!(
            "writing scratch lexical generation manifest at {}",
            tmp_path.display()
        )
    })?;
    fs::rename(&tmp_path, &final_path).with_context(|| {
        format!(
            "atomically publishing lexical generation manifest to {}",
            final_path.display()
        )
    })?;
    Ok(())
}

/// Load a manifest from `<generation_dir>/lexical-generation-manifest.json`.
/// Returns `Ok(None)` when the file does not exist so callers can
/// distinguish "no manifest" from "corrupt manifest".
pub(crate) fn load_manifest(generation_dir: &Path) -> Result<Option<LexicalGenerationManifest>> {
    let path = manifest_path(generation_dir);
    match fs::read(&path) {
        Ok(bytes) => {
            let manifest: LexicalGenerationManifest =
                serde_json::from_slice(&bytes).with_context(|| {
                    format!("parsing lexical generation manifest at {}", path.display())
                })?;
            if manifest.manifest_version > LEXICAL_GENERATION_MANIFEST_VERSION {
                anyhow::bail!(
                    "lexical generation manifest at {} has future manifest_version {} (current runtime supports <= {})",
                    path.display(),
                    manifest.manifest_version,
                    LEXICAL_GENERATION_MANIFEST_VERSION,
                );
            }
            Ok(Some(manifest))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err)
            .with_context(|| format!("reading lexical generation manifest at {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn manifest_round_trips_through_json() {
        let mut manifest = LexicalGenerationManifest::new_scratch(
            "gen-00000001-abc",
            "attempt-00000001",
            "fp-deadbeef",
            1_700_000_000_000,
        );
        manifest.conversation_count = 12;
        manifest.message_count = 34;
        manifest.indexed_doc_count = 34;
        manifest.equivalence_manifest_fingerprint = Some("eq-fp-123".into());
        manifest.transition_build(LexicalGenerationBuildState::Validated, 1_700_000_000_500);
        manifest.transition_publish(LexicalGenerationPublishState::Published, 1_700_000_001_000);

        let bytes = serde_json::to_vec(&manifest).unwrap();
        let parsed: LexicalGenerationManifest = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed, manifest);
        assert!(parsed.is_serveable());
    }

    #[test]
    fn build_and_publish_states_serialize_as_snake_case_strings() {
        let states: Vec<(LexicalGenerationBuildState, &str)> = vec![
            (LexicalGenerationBuildState::Scratch, "scratch"),
            (LexicalGenerationBuildState::Building, "building"),
            (LexicalGenerationBuildState::Built, "built"),
            (LexicalGenerationBuildState::Validating, "validating"),
            (LexicalGenerationBuildState::Validated, "validated"),
            (LexicalGenerationBuildState::Failed, "failed"),
        ];
        for (state, expected) in states {
            let json = serde_json::to_string(&state).unwrap();
            assert_eq!(json, format!("\"{expected}\""));
        }
        let publish_states: Vec<(LexicalGenerationPublishState, &str)> = vec![
            (LexicalGenerationPublishState::Staged, "staged"),
            (LexicalGenerationPublishState::Published, "published"),
            (LexicalGenerationPublishState::Superseded, "superseded"),
            (LexicalGenerationPublishState::Quarantined, "quarantined"),
        ];
        for (state, expected) in publish_states {
            let json = serde_json::to_string(&state).unwrap();
            assert_eq!(json, format!("\"{expected}\""));
        }
    }

    #[test]
    fn failure_history_is_append_only_and_bumps_updated_at() {
        let mut manifest =
            LexicalGenerationManifest::new_scratch("gen-x", "attempt-1", "fp-x", 1_000_000);
        assert_eq!(manifest.updated_at_ms, 1_000_000);
        manifest.record_failure("attempt-1", "build", "oom during Tantivy merge", 2_000_000);
        manifest.record_failure("attempt-2", "validate", "doc count mismatch", 3_000_000);
        assert_eq!(manifest.failure_history.len(), 2);
        assert_eq!(manifest.failure_history[0].attempt_id, "attempt-1");
        assert_eq!(manifest.failure_history[0].phase, "build");
        assert_eq!(manifest.failure_history[1].attempt_id, "attempt-2");
        assert_eq!(manifest.failure_history[1].phase, "validate");
        assert_eq!(manifest.updated_at_ms, 3_000_000);
    }

    #[test]
    fn store_and_load_round_trip_through_disk() {
        let tmp = TempDir::new().unwrap();
        let gen_dir = tmp.path().join("gen-1");
        assert_eq!(load_manifest(&gen_dir).unwrap(), None);

        let manifest = LexicalGenerationManifest::new_scratch(
            "gen-1",
            "attempt-1",
            "fp-abc",
            1_700_000_000_000,
        );
        store_manifest(&gen_dir, &manifest).unwrap();
        let loaded = load_manifest(&gen_dir).unwrap().unwrap();
        assert_eq!(loaded, manifest);
        assert!(manifest_path(&gen_dir).exists());
    }

    #[test]
    fn load_refuses_future_manifest_version() {
        let tmp = TempDir::new().unwrap();
        let gen_dir = tmp.path().join("gen-future");
        fs::create_dir_all(&gen_dir).unwrap();
        let future = serde_json::json!({
            "manifest_version": LEXICAL_GENERATION_MANIFEST_VERSION + 99,
            "generation_id": "gen-future",
            "attempt_id": "attempt-future",
            "created_at_ms": 1i64,
            "updated_at_ms": 1i64,
            "source_db_fingerprint": "fp-future",
            "conversation_count": 0u64,
            "message_count": 0u64,
            "indexed_doc_count": 0u64,
            "equivalence_manifest_fingerprint": null,
            "build_state": "scratch",
            "publish_state": "staged",
            "failure_history": [],
        });
        fs::write(
            manifest_path(&gen_dir),
            serde_json::to_vec(&future).unwrap(),
        )
        .unwrap();
        let err = load_manifest(&gen_dir).unwrap_err().to_string();
        assert!(
            err.contains("future manifest_version"),
            "expected future-version rejection, got {err}"
        );
    }

    #[test]
    fn store_is_atomic_rename_and_overwrites_existing_manifest() {
        let tmp = TempDir::new().unwrap();
        let gen_dir = tmp.path().join("gen-atomic");
        let mut manifest =
            LexicalGenerationManifest::new_scratch("gen-atomic", "attempt-a", "fp-v1", 1_000_000);
        store_manifest(&gen_dir, &manifest).unwrap();

        manifest.transition_build(LexicalGenerationBuildState::Built, 2_000_000);
        manifest.attempt_id = "attempt-b".into();
        store_manifest(&gen_dir, &manifest).unwrap();

        // No leftover tmp files — the rename should have swept them.
        let entries: Vec<_> = fs::read_dir(&gen_dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().into_string().unwrap())
            .collect();
        assert_eq!(entries, vec![LEXICAL_GENERATION_MANIFEST_FILE.to_string()]);

        let reloaded = load_manifest(&gen_dir).unwrap().unwrap();
        assert_eq!(reloaded.attempt_id, "attempt-b");
        assert_eq!(reloaded.build_state, LexicalGenerationBuildState::Built);
    }

    #[test]
    fn is_serveable_requires_validated_and_published() {
        let mut manifest =
            LexicalGenerationManifest::new_scratch("gen-serve", "attempt-1", "fp", 1);
        assert!(!manifest.is_serveable());
        manifest.transition_build(LexicalGenerationBuildState::Validated, 2);
        assert!(!manifest.is_serveable(), "validated but not yet published");
        manifest.transition_publish(LexicalGenerationPublishState::Published, 3);
        assert!(manifest.is_serveable());
        manifest.transition_publish(LexicalGenerationPublishState::Superseded, 4);
        assert!(!manifest.is_serveable(), "superseded must not serve");
    }
}
