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

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Current manifest format version. Bump whenever the struct layout changes
/// in a way that older or newer readers cannot ignore.
pub(crate) const LEXICAL_GENERATION_MANIFEST_VERSION: u32 = 3;

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

/// Per-shard lifecycle state. This is intentionally richer than the
/// generation-level state so recovery can reason from durable facts instead
/// of directory names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LexicalShardLifecycleState {
    /// Shard is planned but no output exists yet.
    Planned,
    /// Builder is actively writing the shard.
    Building,
    /// Output exists in a staged directory but has not been validated.
    Staged,
    /// Validation succeeded; the shard can be included in publish.
    Validated,
    /// Shard is part of a published generation.
    Published,
    /// Shard has staged output that recovery can safely continue.
    Resumable,
    /// Shard must be retained for inspection and excluded from serving.
    Quarantined,
    /// Shard is invalid or intentionally abandoned; rebuild from source.
    Abandoned,
}

/// Shard-plan identity for a generation. All shard manifests in a generation
/// must agree with this plan id before publish.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LexicalGenerationShardPlan {
    pub plan_id: String,
    pub planner_version: u32,
    pub shard_count: u32,
    pub packet_contract_version: u32,
    pub source_db_fingerprint: String,
}

/// Effective build budget and controller context that shaped a generation.
/// This keeps postmortems explainable without dragging runtime-only planner
/// structs into the durable manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LexicalGenerationBuildBudget {
    pub policy_id: String,
    pub effective_settings_fingerprint: String,
    pub max_inflight_message_bytes: u64,
    pub producer_queue_pages: u64,
    pub batch_conversation_limit: u64,
    pub worker_threads: u64,
    pub controller_reason: Option<String>,
    #[serde(default)]
    pub extra_limits: BTreeMap<String, u64>,
}

/// Deferred merge/compaction lifecycle for a published shard generation.
///
/// Search-ready and fully consolidated are intentionally separate states: a
/// published generation can be safe to query while still carrying background
/// merge debt that cleanup/compaction workers may handle later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LexicalGenerationMergeDebtState {
    /// No deferred consolidation work is known for this generation.
    #[default]
    None,
    /// Consolidation is required but intentionally kept off the publish path.
    Pending,
    /// A background worker is currently consolidating this generation.
    Running,
    /// Work yielded to foreground pressure and can resume later.
    Paused,
    /// Work is blocked by policy, locks, or another explicit operator reason.
    Blocked,
    /// Deferred consolidation completed; generation is fully settled.
    Complete,
    /// Work was cancelled without invalidating the published generation.
    Cancelled,
}

/// Durable merge-debt accounting surfaced through the generation manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LexicalGenerationMergeDebt {
    pub state: LexicalGenerationMergeDebtState,
    pub updated_at_ms: Option<i64>,
    pub pending_shard_count: u64,
    pub pending_artifact_bytes: u64,
    pub reason: Option<String>,
    pub controller_reason: Option<String>,
}

impl Default for LexicalGenerationMergeDebt {
    fn default() -> Self {
        Self {
            state: LexicalGenerationMergeDebtState::None,
            updated_at_ms: None,
            pending_shard_count: 0,
            pending_artifact_bytes: 0,
            reason: None,
            controller_reason: None,
        }
    }
}

impl LexicalGenerationMergeDebt {
    pub(crate) fn has_pending_work(&self) -> bool {
        matches!(
            self.state,
            LexicalGenerationMergeDebtState::Pending
                | LexicalGenerationMergeDebtState::Running
                | LexicalGenerationMergeDebtState::Paused
                | LexicalGenerationMergeDebtState::Blocked
                | LexicalGenerationMergeDebtState::Cancelled
        )
    }

    pub(crate) fn is_fully_settled(&self) -> bool {
        matches!(
            self.state,
            LexicalGenerationMergeDebtState::None | LexicalGenerationMergeDebtState::Complete
        )
    }
}

/// Durable footprint and retention metadata for one shard artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LexicalShardManifest {
    pub shard_id: String,
    pub shard_ordinal: u32,
    pub state: LexicalShardLifecycleState,
    pub updated_at_ms: i64,
    pub indexed_doc_count: u64,
    pub message_count: u64,
    pub artifact_bytes: u64,
    pub stable_hash: Option<String>,
    pub reclaimable: bool,
    pub pinned: bool,
    pub recovery_reason: Option<String>,
    pub quarantine_reason: Option<String>,
}

impl LexicalShardManifest {
    pub(crate) fn planned(shard_id: impl Into<String>, shard_ordinal: u32, now_ms: i64) -> Self {
        Self {
            shard_id: shard_id.into(),
            shard_ordinal,
            state: LexicalShardLifecycleState::Planned,
            updated_at_ms: now_ms,
            indexed_doc_count: 0,
            message_count: 0,
            artifact_bytes: 0,
            stable_hash: None,
            reclaimable: true,
            pinned: false,
            recovery_reason: None,
            quarantine_reason: None,
        }
    }

    pub(crate) fn transition(&mut self, state: LexicalShardLifecycleState, now_ms: i64) {
        self.state = state;
        self.updated_at_ms = now_ms;
    }
}

/// Crash-startup decision derived only from manifest state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LexicalGenerationRecoveryAction {
    AttachPublished,
    PublishValidated,
    ResumeStaged,
    KeepQuarantined,
    DiscardAndRebuild,
    IgnoreSuperseded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LexicalGenerationRecoveryDecision {
    pub action: LexicalGenerationRecoveryAction,
    pub reason: String,
    pub resumable_shards: Vec<String>,
    pub quarantined_shards: Vec<String>,
    pub abandoned_shards: Vec<String>,
}

/// `coding_agent_session_search-ibuuh.19` classification predicate:
/// returns `true` when `disposition` is one of the variants that the
/// dry-run / apply gates MUST keep on disk. The non-protected
/// variants are exactly `SupersededReclaimable` + `FailedReclaimable`
/// — the two states the policy contract says are safe to reclaim.
///
/// Lifted out of the `LexicalCleanupDryRunPlan` impl so it has a
/// single canonical home AND so the test module's exhaustiveness gate
/// can compare it against `LexicalCleanupDisposition::all_variants()`
/// directly. Mirroring impl method calls through to this function
/// keeps existing call sites unchanged.
pub(crate) fn is_protected_retention_disposition(disposition: LexicalCleanupDisposition) -> bool {
    matches!(
        disposition,
        LexicalCleanupDisposition::CurrentPublished
            | LexicalCleanupDisposition::ActiveWork
            | LexicalCleanupDisposition::QuarantinedRetained
            | LexicalCleanupDisposition::SupersededRetained
            | LexicalCleanupDisposition::FailedRetained
            | LexicalCleanupDisposition::PinnedRetained
    )
}

/// Dry-run cleanup classification for one lexical artifact or generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LexicalCleanupDisposition {
    /// The artifact is part of the currently published search surface.
    CurrentPublished,
    /// The artifact is still being built, validated, resumed, or merged.
    ActiveWork,
    /// The artifact is intentionally retained for operator inspection.
    QuarantinedRetained,
    /// A superseded artifact is no longer pinned and can be reclaimed.
    SupersededReclaimable,
    /// A superseded artifact must stay on disk because policy still pins it.
    SupersededRetained,
    /// A failed or abandoned artifact can be reclaimed after dry-run approval.
    FailedReclaimable,
    /// A failed or abandoned artifact must stay on disk for inspection.
    FailedRetained,
    /// The artifact is explicitly pinned by current policy.
    PinnedRetained,
}

impl LexicalCleanupDisposition {
    fn as_str(self) -> &'static str {
        match self {
            Self::CurrentPublished => "current_published",
            Self::ActiveWork => "active_work",
            Self::QuarantinedRetained => "quarantined_retained",
            Self::SupersededReclaimable => "superseded_reclaimable",
            Self::SupersededRetained => "superseded_retained",
            Self::FailedReclaimable => "failed_reclaimable",
            Self::FailedRetained => "failed_retained",
            Self::PinnedRetained => "pinned_retained",
        }
    }

    /// Every variant in declaration order. Used by the
    /// `coding_agent_session_search-ibuuh.19` golden gate to assert
    /// every variant has both an `as_str()` arm AND a serde
    /// representation, AND that the protected-vs-reclaimable
    /// classification covers every variant exhaustively. A new
    /// variant added without registering it here is a compile error
    /// (no `_ => ...` catch-all in the test).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn all_variants() -> &'static [Self] {
        &[
            Self::CurrentPublished,
            Self::ActiveWork,
            Self::QuarantinedRetained,
            Self::SupersededReclaimable,
            Self::SupersededRetained,
            Self::FailedReclaimable,
            Self::FailedRetained,
            Self::PinnedRetained,
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LexicalShardCleanupInventory {
    pub shard_id: String,
    pub state: LexicalShardLifecycleState,
    pub disposition: LexicalCleanupDisposition,
    pub reason: String,
    pub artifact_bytes: u64,
    pub reclaimable_bytes: u64,
    pub retained_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LexicalGenerationCleanupInventory {
    pub generation_id: String,
    pub build_state: LexicalGenerationBuildState,
    pub publish_state: LexicalGenerationPublishState,
    pub disposition: LexicalCleanupDisposition,
    pub reason: String,
    pub retain_until_ms: Option<i64>,
    pub retention_reason: String,
    pub artifact_bytes: u64,
    pub reclaimable_bytes: u64,
    pub retained_bytes: u64,
    pub shards: Vec<LexicalShardCleanupInventory>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LexicalCleanupDryRunPlan {
    pub dry_run: bool,
    pub approval_fingerprint: String,
    pub generation_count: usize,
    pub total_artifact_bytes: u64,
    pub total_reclaimable_bytes: u64,
    pub total_retained_bytes: u64,
    #[serde(default)]
    pub reclaim_candidates: Vec<LexicalCleanupReclaimCandidate>,
    pub reclaimable_generation_ids: Vec<String>,
    pub fully_retained_generation_ids: Vec<String>,
    #[serde(default)]
    pub protected_generation_ids: Vec<String>,
    pub protected_retained_bytes: u64,
    pub quarantined_generation_ids: Vec<String>,
    pub active_generation_ids: Vec<String>,
    pub disposition_counts: BTreeMap<LexicalCleanupDisposition, usize>,
    #[serde(default)]
    pub generation_disposition_summaries:
        BTreeMap<LexicalCleanupDisposition, LexicalCleanupGenerationDispositionSummary>,
    #[serde(default)]
    pub inspection_items: Vec<LexicalCleanupInspectionItem>,
    #[serde(default)]
    pub inspection_required_generation_ids: Vec<String>,
    #[serde(default)]
    pub inspection_required_count: usize,
    #[serde(default)]
    pub inspection_required_retained_bytes: u64,
    #[serde(default)]
    pub shard_disposition_summaries:
        BTreeMap<LexicalCleanupDisposition, LexicalCleanupDispositionSummary>,
    pub inventories: Vec<LexicalGenerationCleanupInventory>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LexicalCleanupReclaimCandidate {
    pub generation_id: String,
    pub shard_id: String,
    pub disposition: LexicalCleanupDisposition,
    pub reason: String,
    pub reclaimable_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LexicalCleanupInspectionItem {
    pub generation_id: String,
    pub shard_id: Option<String>,
    pub disposition: LexicalCleanupDisposition,
    pub reason: String,
    pub retained_bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LexicalCleanupDispositionSummary {
    pub shard_count: usize,
    pub artifact_bytes: u64,
    pub reclaimable_bytes: u64,
    pub retained_bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LexicalCleanupGenerationDispositionSummary {
    pub generation_count: usize,
    pub artifact_bytes: u64,
    pub reclaimable_bytes: u64,
    pub retained_bytes: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LexicalCleanupApprovalFingerprintStatus {
    #[default]
    NotRequested,
    Missing,
    Matched,
    Mismatched,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LexicalCleanupApplyBlocker {
    NoReclaimableCandidates,
    OperatorApprovalRequired,
    ApprovalFingerprintMissing,
    ApprovalFingerprintMismatched,
    ActiveGenerationWork,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LexicalCleanupApplyGate {
    pub apply_allowed: bool,
    pub dry_run: bool,
    pub explicit_operator_approval: bool,
    #[serde(default)]
    pub approval_fingerprint: String,
    #[serde(default)]
    pub provided_approval_fingerprint: Option<String>,
    #[serde(default)]
    pub approval_fingerprint_status: LexicalCleanupApprovalFingerprintStatus,
    #[serde(default)]
    pub approval_fingerprint_matches: bool,
    #[serde(default)]
    pub generation_count: usize,
    #[serde(default)]
    pub total_artifact_bytes: u64,
    #[serde(default)]
    pub total_retained_bytes: u64,
    pub candidate_count: usize,
    pub reclaimable_bytes: u64,
    #[serde(default)]
    pub disposition_counts: BTreeMap<LexicalCleanupDisposition, usize>,
    #[serde(default)]
    pub generation_disposition_summaries:
        BTreeMap<LexicalCleanupDisposition, LexicalCleanupGenerationDispositionSummary>,
    #[serde(default)]
    pub shard_disposition_summaries:
        BTreeMap<LexicalCleanupDisposition, LexicalCleanupDispositionSummary>,
    #[serde(default)]
    pub candidate_previews: Vec<LexicalCleanupReclaimCandidate>,
    #[serde(default)]
    pub reclaimable_generation_ids: Vec<String>,
    #[serde(default)]
    pub fully_retained_generation_ids: Vec<String>,
    #[serde(default)]
    pub quarantined_generation_ids: Vec<String>,
    #[serde(default)]
    pub blocker_codes: Vec<LexicalCleanupApplyBlocker>,
    pub blocked_reasons: Vec<String>,
    #[serde(default)]
    pub active_generation_ids: Vec<String>,
    #[serde(default)]
    pub protected_generation_ids: Vec<String>,
    #[serde(default)]
    pub protected_retained_bytes: u64,
    #[serde(default)]
    pub inspection_previews: Vec<LexicalCleanupInspectionItem>,
    #[serde(default)]
    pub inspection_required_count: usize,
    #[serde(default)]
    pub inspection_required_retained_bytes: u64,
    #[serde(default)]
    pub inspection_required_generation_ids: Vec<String>,
}

impl LexicalCleanupDryRunPlan {
    pub(crate) fn from_manifests<'a>(
        manifests: impl IntoIterator<Item = &'a LexicalGenerationManifest>,
    ) -> Self {
        let mut plan = Self {
            dry_run: true,
            approval_fingerprint: String::new(),
            generation_count: 0,
            total_artifact_bytes: 0,
            total_reclaimable_bytes: 0,
            total_retained_bytes: 0,
            reclaim_candidates: Vec::new(),
            reclaimable_generation_ids: Vec::new(),
            fully_retained_generation_ids: Vec::new(),
            protected_generation_ids: Vec::new(),
            protected_retained_bytes: 0,
            quarantined_generation_ids: Vec::new(),
            active_generation_ids: Vec::new(),
            disposition_counts: BTreeMap::new(),
            generation_disposition_summaries: BTreeMap::new(),
            inspection_items: Vec::new(),
            inspection_required_generation_ids: Vec::new(),
            inspection_required_count: 0,
            inspection_required_retained_bytes: 0,
            shard_disposition_summaries: BTreeMap::new(),
            inventories: Vec::new(),
        };

        for manifest in manifests {
            plan.record_inventory(manifest.cleanup_inventory());
        }
        plan.approval_fingerprint = plan.compute_approval_fingerprint();

        plan
    }

    pub(crate) fn has_reclaimable_artifacts(&self) -> bool {
        self.total_reclaimable_bytes > 0
    }

    pub(crate) fn reclaim_candidates(&self) -> Vec<LexicalCleanupReclaimCandidate> {
        self.reclaim_candidates.clone()
    }

    pub(crate) fn apply_gate(&self, explicit_operator_approval: bool) -> LexicalCleanupApplyGate {
        self.apply_gate_with_fingerprint(explicit_operator_approval, None)
    }

    pub(crate) fn apply_gate_with_fingerprint(
        &self,
        explicit_operator_approval: bool,
        provided_approval_fingerprint: Option<&str>,
    ) -> LexicalCleanupApplyGate {
        let mut blocked_reasons = Vec::new();
        let mut blocker_codes = Vec::new();
        if self.reclaim_candidates.is_empty() {
            blocked_reasons.push("no reclaimable cleanup candidates".to_string());
            blocker_codes.push(LexicalCleanupApplyBlocker::NoReclaimableCandidates);
        }
        if !explicit_operator_approval {
            blocked_reasons.push(
                "destructive cleanup requires explicit operator approval after dry-run".to_string(),
            );
            blocker_codes.push(LexicalCleanupApplyBlocker::OperatorApprovalRequired);
        }
        let approval_fingerprint_status =
            match (explicit_operator_approval, provided_approval_fingerprint) {
                (false, _) => LexicalCleanupApprovalFingerprintStatus::NotRequested,
                (true, Some(fingerprint)) if fingerprint == self.approval_fingerprint => {
                    LexicalCleanupApprovalFingerprintStatus::Matched
                }
                (true, Some(_)) => LexicalCleanupApprovalFingerprintStatus::Mismatched,
                (true, None) => LexicalCleanupApprovalFingerprintStatus::Missing,
            };
        let approval_fingerprint_matches =
            approval_fingerprint_status == LexicalCleanupApprovalFingerprintStatus::Matched;
        match approval_fingerprint_status {
            LexicalCleanupApprovalFingerprintStatus::Mismatched => blocked_reasons.push(
                "provided cleanup approval fingerprint does not match dry-run plan".to_string(),
            ),
            LexicalCleanupApprovalFingerprintStatus::Missing => {
                blocked_reasons.push(format!(
                    "cleanup apply requires confirming approval fingerprint {}",
                    self.approval_fingerprint
                ));
                blocker_codes.push(LexicalCleanupApplyBlocker::ApprovalFingerprintMissing);
            }
            LexicalCleanupApprovalFingerprintStatus::NotRequested
            | LexicalCleanupApprovalFingerprintStatus::Matched => {}
        }
        if approval_fingerprint_status == LexicalCleanupApprovalFingerprintStatus::Mismatched {
            blocker_codes.push(LexicalCleanupApplyBlocker::ApprovalFingerprintMismatched);
        }
        if !self.active_generation_ids.is_empty() {
            blocked_reasons.push(format!(
                "active generation work must settle before cleanup apply: {}",
                self.active_generation_ids.join(",")
            ));
            blocker_codes.push(LexicalCleanupApplyBlocker::ActiveGenerationWork);
        }
        let inspection_required_generation_ids = self.inspection_required_generation_ids();

        LexicalCleanupApplyGate {
            apply_allowed: blocked_reasons.is_empty(),
            dry_run: self.dry_run,
            explicit_operator_approval,
            approval_fingerprint: self.approval_fingerprint.clone(),
            provided_approval_fingerprint: provided_approval_fingerprint.map(str::to_owned),
            approval_fingerprint_status,
            approval_fingerprint_matches,
            generation_count: self.generation_count,
            total_artifact_bytes: self.total_artifact_bytes,
            total_retained_bytes: self.total_retained_bytes,
            candidate_count: self.reclaim_candidates.len(),
            reclaimable_bytes: self.total_reclaimable_bytes,
            disposition_counts: self.disposition_counts.clone(),
            generation_disposition_summaries: self.generation_disposition_summaries.clone(),
            shard_disposition_summaries: self.shard_disposition_summaries.clone(),
            candidate_previews: self.reclaim_candidates.clone(),
            reclaimable_generation_ids: self.reclaimable_generation_ids.clone(),
            fully_retained_generation_ids: self.fully_retained_generation_ids.clone(),
            quarantined_generation_ids: self.quarantined_generation_ids.clone(),
            blocker_codes,
            blocked_reasons,
            active_generation_ids: self.active_generation_ids.clone(),
            protected_generation_ids: self.protected_generation_ids.clone(),
            protected_retained_bytes: self.protected_retained_bytes,
            inspection_previews: self.inspection_items.clone(),
            inspection_required_count: self.inspection_required_count,
            inspection_required_retained_bytes: self.inspection_required_retained_bytes,
            inspection_required_generation_ids,
        }
    }

    pub(crate) fn inspection_required_generation_ids(&self) -> Vec<String> {
        self.inspection_required_generation_ids.clone()
    }

    pub(crate) fn inspection_required_retained_bytes(&self) -> u64 {
        self.inspection_required_retained_bytes
    }

    fn record_inspection_item(&mut self, item: LexicalCleanupInspectionItem) {
        self.inspection_required_count = self.inspection_required_count.saturating_add(1);
        self.inspection_required_retained_bytes = self
            .inspection_required_retained_bytes
            .saturating_add(item.retained_bytes);
        if !self
            .inspection_required_generation_ids
            .contains(&item.generation_id)
        {
            self.inspection_required_generation_ids
                .push(item.generation_id.clone());
        }
        self.inspection_items.push(item);
    }

    fn record_inventory(&mut self, inventory: LexicalGenerationCleanupInventory) {
        self.generation_count = self.generation_count.saturating_add(1);
        self.total_artifact_bytes = self
            .total_artifact_bytes
            .saturating_add(inventory.artifact_bytes);
        self.total_reclaimable_bytes = self
            .total_reclaimable_bytes
            .saturating_add(inventory.reclaimable_bytes);
        self.total_retained_bytes = self
            .total_retained_bytes
            .saturating_add(inventory.retained_bytes);
        *self
            .disposition_counts
            .entry(inventory.disposition)
            .or_insert(0) += 1;
        let generation_summary = self
            .generation_disposition_summaries
            .entry(inventory.disposition)
            .or_default();
        generation_summary.generation_count = generation_summary.generation_count.saturating_add(1);
        generation_summary.artifact_bytes = generation_summary
            .artifact_bytes
            .saturating_add(inventory.artifact_bytes);
        generation_summary.reclaimable_bytes = generation_summary
            .reclaimable_bytes
            .saturating_add(inventory.reclaimable_bytes);
        generation_summary.retained_bytes = generation_summary
            .retained_bytes
            .saturating_add(inventory.retained_bytes);

        if inventory.reclaimable_bytes > 0 {
            self.reclaimable_generation_ids
                .push(inventory.generation_id.clone());
        } else {
            self.fully_retained_generation_ids
                .push(inventory.generation_id.clone());
        }
        if matches!(
            inventory.disposition,
            LexicalCleanupDisposition::QuarantinedRetained
        ) {
            self.quarantined_generation_ids
                .push(inventory.generation_id.clone());
        }
        if matches!(inventory.disposition, LexicalCleanupDisposition::ActiveWork) {
            self.active_generation_ids
                .push(inventory.generation_id.clone());
        }
        let mut has_protected_retention =
            Self::is_protected_retention(inventory.disposition) && inventory.retained_bytes > 0;
        let inventory_requires_inspection = Self::requires_inspection(inventory.disposition);
        let inventory_allows_reclaim_candidates = matches!(
            inventory.disposition,
            LexicalCleanupDisposition::SupersededReclaimable
                | LexicalCleanupDisposition::FailedReclaimable
        );
        let mut shard_inspection_items = 0usize;
        for shard in &inventory.shards {
            let summary = self
                .shard_disposition_summaries
                .entry(shard.disposition)
                .or_default();
            summary.shard_count = summary.shard_count.saturating_add(1);
            summary.artifact_bytes = summary.artifact_bytes.saturating_add(shard.artifact_bytes);
            summary.reclaimable_bytes = summary
                .reclaimable_bytes
                .saturating_add(shard.reclaimable_bytes);
            summary.retained_bytes = summary.retained_bytes.saturating_add(shard.retained_bytes);
            if Self::is_protected_retention(shard.disposition) && shard.retained_bytes > 0 {
                has_protected_retention = true;
            }

            if Self::requires_inspection(shard.disposition) {
                shard_inspection_items = shard_inspection_items.saturating_add(1);
                self.record_inspection_item(LexicalCleanupInspectionItem {
                    generation_id: inventory.generation_id.clone(),
                    shard_id: Some(shard.shard_id.clone()),
                    disposition: shard.disposition,
                    reason: shard.reason.clone(),
                    retained_bytes: shard.retained_bytes,
                });
            }

            if shard.reclaimable_bytes == 0 || !inventory_allows_reclaim_candidates {
                continue;
            }
            self.reclaim_candidates
                .push(LexicalCleanupReclaimCandidate {
                    generation_id: inventory.generation_id.clone(),
                    shard_id: shard.shard_id.clone(),
                    disposition: shard.disposition,
                    reason: shard.reason.clone(),
                    reclaimable_bytes: shard.reclaimable_bytes,
                });
        }

        if inventory_requires_inspection && shard_inspection_items == 0 {
            self.record_inspection_item(LexicalCleanupInspectionItem {
                generation_id: inventory.generation_id.clone(),
                shard_id: None,
                disposition: inventory.disposition,
                reason: inventory.reason.clone(),
                retained_bytes: inventory.retained_bytes,
            });
        }

        if has_protected_retention {
            if !self
                .protected_generation_ids
                .contains(&inventory.generation_id)
            {
                self.protected_generation_ids
                    .push(inventory.generation_id.clone());
            }
            self.protected_retained_bytes = self
                .protected_retained_bytes
                .saturating_add(inventory.retained_bytes);
        }

        // [coding_agent_session_search-ibuuh.19] Emit a structured
        // tracing event per generation classification so operators can
        // trace from logs alone WHY each artifact was reclaimed or
        // preserved (the bead's "structured logs that let a future
        // agent understand exactly why disk was reclaimed or preserved"
        // SCOPE bullet). Severity tiers match operator expectations:
        //
        // - SupersededReclaimable / FailedReclaimable -> debug
        //   (routine cleanup eligibility — high volume on big corpora)
        // - QuarantinedRetained / FailedRetained -> warn
        //   (operator inspection required — surface in default logs)
        // - ActiveWork / CurrentPublished / SupersededRetained /
        //   PinnedRetained -> info (preserved by design, but worth
        //   surfacing when the operator runs cleanup so they see WHY)
        //
        // Every event carries the disposition string + reason +
        // generation_id + reclaimable/retained byte counts so a single
        // log line answers "what got pruned?" and "why was X kept?"
        // without grepping multiple sources.
        let disposition_str = inventory.disposition.as_str();
        // [coding_agent_session_search-urscl] Pre-fix this match
        // repeated the same 8-field tracing payload across three
        // tracing::{debug,warn,info}! call sites. A field added in
        // one branch but forgotten in another would silently ship.
        // The local `emit_tier!` macro inlines the shared payload at
        // each call site (no runtime cost — same code generation),
        // so adding a field once propagates to all three tiers and
        // the per-tier difference is reduced to (macro ident,
        // message literal). The exhaustive disposition severity
        // tests (record_inventory_emits_correct_severity_for_every_disposition_variant)
        // continue to observe the per-tier level + message exactly
        // as before because each branch still expands to the same
        // tracing macro call.
        let shard_count = inventory.shards.len();
        macro_rules! emit_tier {
            ($macro:ident, $msg:literal) => {
                tracing::$macro!(
                    target: "cass::indexer::lexical_cleanup",
                    generation_id = %inventory.generation_id,
                    disposition = disposition_str,
                    reason = %inventory.reason,
                    reclaimable_bytes = inventory.reclaimable_bytes,
                    retained_bytes = inventory.retained_bytes,
                    artifact_bytes = inventory.artifact_bytes,
                    shard_count,
                    inspection_required = inventory_requires_inspection,
                    $msg
                )
            };
        }
        match inventory.disposition {
            LexicalCleanupDisposition::SupersededReclaimable
            | LexicalCleanupDisposition::FailedReclaimable => {
                emit_tier!(
                    debug,
                    "lexical cleanup classified generation as reclaimable"
                );
            }
            LexicalCleanupDisposition::QuarantinedRetained
            | LexicalCleanupDisposition::FailedRetained => {
                emit_tier!(
                    warn,
                    "lexical cleanup retained generation pending operator inspection"
                );
            }
            LexicalCleanupDisposition::ActiveWork
            | LexicalCleanupDisposition::CurrentPublished
            | LexicalCleanupDisposition::SupersededRetained
            | LexicalCleanupDisposition::PinnedRetained => {
                emit_tier!(info, "lexical cleanup retained generation by policy");
            }
        }
        // Suppress the "unused" lint for diagnostics-only locals so the
        // compiler doesn't warn even if a future variant addition
        // routes through a no-emission arm.
        let _ = (
            shard_inspection_items,
            inventory_allows_reclaim_candidates,
            has_protected_retention,
        );

        self.inventories.push(inventory);
    }

    fn requires_inspection(disposition: LexicalCleanupDisposition) -> bool {
        matches!(
            disposition,
            LexicalCleanupDisposition::QuarantinedRetained
                | LexicalCleanupDisposition::FailedRetained
        )
    }

    fn is_protected_retention(disposition: LexicalCleanupDisposition) -> bool {
        is_protected_retention_disposition(disposition)
    }

    fn compute_approval_fingerprint(&self) -> String {
        // Deterministic: hash over sorted snapshots so the fingerprint is
        // invariant under manifest/shard iteration order (filesystem scans,
        // HashMap-backed callers, etc.). BTreeMaps already iterate in order.
        let mut hasher = blake3::Hasher::new();
        hash_str(&mut hasher, "cass.lexical_cleanup_approval.v1");
        hash_usize(&mut hasher, self.generation_count);
        hash_u64(&mut hasher, self.total_artifact_bytes);
        hash_u64(&mut hasher, self.total_reclaimable_bytes);
        hash_u64(&mut hasher, self.total_retained_bytes);
        hash_u64(&mut hasher, self.protected_retained_bytes);
        hash_usize(&mut hasher, self.inspection_required_count);
        hash_u64(&mut hasher, self.inspection_required_retained_bytes);

        let mut candidates: Vec<&LexicalCleanupReclaimCandidate> =
            self.reclaim_candidates.iter().collect();
        candidates.sort_by(|a, b| {
            (
                &a.generation_id,
                &a.shard_id,
                a.disposition.as_str(),
                &a.reason,
                a.reclaimable_bytes,
            )
                .cmp(&(
                    &b.generation_id,
                    &b.shard_id,
                    b.disposition.as_str(),
                    &b.reason,
                    b.reclaimable_bytes,
                ))
        });
        for candidate in candidates {
            hash_str(&mut hasher, &candidate.generation_id);
            hash_str(&mut hasher, &candidate.shard_id);
            hash_str(&mut hasher, candidate.disposition.as_str());
            hash_str(&mut hasher, &candidate.reason);
            hash_u64(&mut hasher, candidate.reclaimable_bytes);
        }

        let mut inspections: Vec<&LexicalCleanupInspectionItem> =
            self.inspection_items.iter().collect();
        inspections.sort_by(|a, b| {
            (
                &a.generation_id,
                a.shard_id.as_deref().unwrap_or(""),
                a.disposition.as_str(),
                &a.reason,
                a.retained_bytes,
            )
                .cmp(&(
                    &b.generation_id,
                    b.shard_id.as_deref().unwrap_or(""),
                    b.disposition.as_str(),
                    &b.reason,
                    b.retained_bytes,
                ))
        });
        for item in inspections {
            hash_str(&mut hasher, &item.generation_id);
            hash_str(&mut hasher, item.shard_id.as_deref().unwrap_or(""));
            hash_str(&mut hasher, item.disposition.as_str());
            hash_str(&mut hasher, &item.reason);
            hash_u64(&mut hasher, item.retained_bytes);
        }

        let mut active: Vec<&String> = self.active_generation_ids.iter().collect();
        active.sort();
        for generation_id in active {
            hash_str(&mut hasher, generation_id);
        }
        let mut protected: Vec<&String> = self.protected_generation_ids.iter().collect();
        protected.sort();
        for generation_id in protected {
            hash_str(&mut hasher, generation_id);
        }
        for (disposition, count) in &self.disposition_counts {
            hash_str(&mut hasher, disposition.as_str());
            hash_usize(&mut hasher, *count);
        }
        for (disposition, summary) in &self.generation_disposition_summaries {
            hash_str(&mut hasher, "generation_disposition_summary");
            hash_str(&mut hasher, disposition.as_str());
            hash_usize(&mut hasher, summary.generation_count);
            hash_u64(&mut hasher, summary.artifact_bytes);
            hash_u64(&mut hasher, summary.reclaimable_bytes);
            hash_u64(&mut hasher, summary.retained_bytes);
        }
        for (disposition, summary) in &self.shard_disposition_summaries {
            hash_str(&mut hasher, "shard_disposition_summary");
            hash_str(&mut hasher, disposition.as_str());
            hash_usize(&mut hasher, summary.shard_count);
            hash_u64(&mut hasher, summary.artifact_bytes);
            hash_u64(&mut hasher, summary.reclaimable_bytes);
            hash_u64(&mut hasher, summary.retained_bytes);
        }

        format!("cleanup-v1-{}", hasher.finalize().to_hex())
    }
}

fn hash_str(hasher: &mut blake3::Hasher, value: &str) {
    hash_usize(hasher, value.len());
    hasher.update(value.as_bytes());
}

fn hash_u64(hasher: &mut blake3::Hasher, value: u64) {
    hasher.update(&value.to_le_bytes());
}

fn hash_usize(hasher: &mut blake3::Hasher, value: usize) {
    hasher.update(&u64::try_from(value).unwrap_or(u64::MAX).to_le_bytes());
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
    /// Shard-plan identity, present for shard-farm generations.
    #[serde(default)]
    pub shard_plan: Option<LexicalGenerationShardPlan>,
    /// Build-budget and effective-settings context that governed this run.
    #[serde(default)]
    pub build_budget: Option<LexicalGenerationBuildBudget>,
    /// Durable per-shard state. Empty for legacy single-generation builds.
    #[serde(default)]
    pub shards: Vec<LexicalShardManifest>,
    /// Deferred merge/compaction debt that may be handled after publish.
    #[serde(default)]
    pub merge_debt: LexicalGenerationMergeDebt,
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
            shard_plan: None,
            build_budget: None,
            shards: Vec::new(),
            merge_debt: LexicalGenerationMergeDebt::default(),
            build_state: LexicalGenerationBuildState::Scratch,
            publish_state: LexicalGenerationPublishState::Staged,
            failure_history: Vec::new(),
        }
    }

    /// Attach shard-plan and budget context. The manifest records this once
    /// near generation creation so later recovery can explain which plan and
    /// controller limits produced the staged outputs.
    pub(crate) fn set_shard_plan_and_budget(
        &mut self,
        shard_plan: LexicalGenerationShardPlan,
        build_budget: LexicalGenerationBuildBudget,
        now_ms: i64,
    ) {
        self.shard_plan = Some(shard_plan);
        self.build_budget = Some(build_budget);
        self.updated_at_ms = now_ms;
    }

    /// Replace the durable shard list. Callers should provide one entry per
    /// planned shard in ordinal order.
    pub(crate) fn set_shards(&mut self, shards: Vec<LexicalShardManifest>, now_ms: i64) {
        self.shards = shards;
        self.updated_at_ms = now_ms;
    }

    /// Transition a known shard by id. Returns true when the shard existed.
    pub(crate) fn transition_shard(
        &mut self,
        shard_id: &str,
        state: LexicalShardLifecycleState,
        now_ms: i64,
        reason: Option<String>,
    ) -> bool {
        let Some(shard) = self
            .shards
            .iter_mut()
            .find(|candidate| candidate.shard_id == shard_id)
        else {
            return false;
        };
        shard.transition(state, now_ms);
        match state {
            LexicalShardLifecycleState::Quarantined => {
                shard.quarantine_reason = reason;
                shard.reclaimable = false;
            }
            LexicalShardLifecycleState::Resumable => {
                shard.recovery_reason = reason;
            }
            LexicalShardLifecycleState::Published => {
                shard.pinned = true;
                shard.reclaimable = false;
            }
            LexicalShardLifecycleState::Abandoned => {
                shard.recovery_reason = reason;
                shard.reclaimable = true;
            }
            _ => {}
        }
        self.updated_at_ms = now_ms;
        true
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

    /// Record or update deferred merge debt without changing serveability.
    pub(crate) fn record_merge_debt(
        &mut self,
        pending_shard_count: u64,
        pending_artifact_bytes: u64,
        reason: impl Into<String>,
        now_ms: i64,
    ) {
        self.merge_debt = LexicalGenerationMergeDebt {
            state: if pending_shard_count == 0 && pending_artifact_bytes == 0 {
                LexicalGenerationMergeDebtState::Complete
            } else {
                LexicalGenerationMergeDebtState::Pending
            },
            updated_at_ms: Some(now_ms),
            pending_shard_count,
            pending_artifact_bytes,
            reason: Some(reason.into()),
            controller_reason: None,
        };
        self.updated_at_ms = now_ms;
    }

    /// Move deferred merge work between background lifecycle states.
    pub(crate) fn transition_merge_debt(
        &mut self,
        state: LexicalGenerationMergeDebtState,
        now_ms: i64,
        reason: Option<String>,
        controller_reason: Option<String>,
    ) {
        self.merge_debt.state = state;
        self.merge_debt.updated_at_ms = Some(now_ms);
        self.merge_debt.reason = reason;
        self.merge_debt.controller_reason = controller_reason;
        if self.merge_debt.is_fully_settled() {
            self.merge_debt.pending_shard_count = 0;
            self.merge_debt.pending_artifact_bytes = 0;
        }
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

    /// Whether published artifacts have no known deferred merge debt.
    pub(crate) fn is_fully_consolidated(&self) -> bool {
        self.is_serveable() && self.merge_debt.is_fully_settled()
    }

    /// Build an auditable dry-run inventory for cleanup/quarantine decisions.
    pub(crate) fn cleanup_inventory(&self) -> LexicalGenerationCleanupInventory {
        let shards: Vec<_> = self
            .shards
            .iter()
            .map(|shard| self.classify_shard_for_cleanup(shard))
            .collect();
        let shard_artifact_bytes = shards.iter().map(|shard| shard.artifact_bytes).sum::<u64>();
        let shard_reclaimable_bytes = shards
            .iter()
            .map(|shard| shard.reclaimable_bytes)
            .sum::<u64>();
        let pending_merge_bytes = if self.merge_debt.has_pending_work() {
            self.merge_debt.pending_artifact_bytes
        } else {
            0
        };
        let artifact_bytes = shard_artifact_bytes.saturating_add(pending_merge_bytes);
        let generation_reclaimable_bytes = if self.generation_cleanup_allows_reclaim() {
            shard_reclaimable_bytes
        } else {
            0
        };
        let retained_bytes = artifact_bytes.saturating_sub(generation_reclaimable_bytes);
        let (disposition, reason) =
            self.classify_generation_for_cleanup(generation_reclaimable_bytes);
        let (retain_until_ms, retention_reason) =
            self.classify_generation_retention_window(disposition);

        LexicalGenerationCleanupInventory {
            generation_id: self.generation_id.clone(),
            build_state: self.build_state,
            publish_state: self.publish_state,
            disposition,
            reason,
            retain_until_ms,
            retention_reason,
            artifact_bytes,
            reclaimable_bytes: generation_reclaimable_bytes,
            retained_bytes,
            shards,
        }
    }

    /// Derive the crash-startup action from durable manifest state. This is
    /// intentionally conservative: any quarantined or abandoned shard prevents
    /// partial shard sets from becoming visible to search.
    pub(crate) fn recovery_decision(&self) -> LexicalGenerationRecoveryDecision {
        let resumable_shards = self.shards_with_state(&[
            LexicalShardLifecycleState::Building,
            LexicalShardLifecycleState::Staged,
            LexicalShardLifecycleState::Resumable,
        ]);
        let quarantined_shards = self.shards_with_state(&[LexicalShardLifecycleState::Quarantined]);
        let abandoned_shards = self.shards_with_state(&[LexicalShardLifecycleState::Abandoned]);

        let (action, reason) = if matches!(
            self.publish_state,
            LexicalGenerationPublishState::Superseded
        ) {
            (
                LexicalGenerationRecoveryAction::IgnoreSuperseded,
                format!(
                    "generation {} was superseded by a newer publish",
                    self.generation_id
                ),
            )
        } else if !quarantined_shards.is_empty()
            || matches!(
                self.publish_state,
                LexicalGenerationPublishState::Quarantined
            )
        {
            (
                LexicalGenerationRecoveryAction::KeepQuarantined,
                format!(
                    "generation {} has quarantined shard state and must stay out of search",
                    self.generation_id
                ),
            )
        } else if !abandoned_shards.is_empty()
            || matches!(self.build_state, LexicalGenerationBuildState::Failed)
        {
            (
                LexicalGenerationRecoveryAction::DiscardAndRebuild,
                format!(
                    "generation {} has abandoned or failed state and must rebuild from source",
                    self.generation_id
                ),
            )
        } else if self.is_serveable() {
            (
                LexicalGenerationRecoveryAction::AttachPublished,
                format!(
                    "generation {} is validated and published",
                    self.generation_id
                ),
            )
        } else if matches!(self.build_state, LexicalGenerationBuildState::Validated)
            && self.all_shards_publish_ready()
        {
            (
                LexicalGenerationRecoveryAction::PublishValidated,
                format!(
                    "generation {} is validated with a complete publish-ready shard set",
                    self.generation_id
                ),
            )
        } else if !resumable_shards.is_empty()
            || matches!(
                self.build_state,
                LexicalGenerationBuildState::Scratch
                    | LexicalGenerationBuildState::Building
                    | LexicalGenerationBuildState::Built
                    | LexicalGenerationBuildState::Validating
            )
        {
            (
                LexicalGenerationRecoveryAction::ResumeStaged,
                format!(
                    "generation {} has staged or in-progress work that can be resumed",
                    self.generation_id
                ),
            )
        } else {
            (
                LexicalGenerationRecoveryAction::DiscardAndRebuild,
                format!(
                    "generation {} does not contain a safe publish or resume state",
                    self.generation_id
                ),
            )
        };

        LexicalGenerationRecoveryDecision {
            action,
            reason,
            resumable_shards,
            quarantined_shards,
            abandoned_shards,
        }
    }

    fn shards_with_state(&self, states: &[LexicalShardLifecycleState]) -> Vec<String> {
        self.shards
            .iter()
            .filter(|shard| states.contains(&shard.state))
            .map(|shard| shard.shard_id.clone())
            .collect()
    }

    fn all_shards_publish_ready(&self) -> bool {
        !self.shards.is_empty()
            && self.shards.iter().all(|shard| {
                matches!(
                    shard.state,
                    LexicalShardLifecycleState::Validated | LexicalShardLifecycleState::Published
                )
            })
            && match self.shard_plan.as_ref() {
                Some(plan) => usize::try_from(plan.shard_count) == Ok(self.shards.len()),
                None => true,
            }
    }

    fn classify_shard_for_cleanup(
        &self,
        shard: &LexicalShardManifest,
    ) -> LexicalShardCleanupInventory {
        let (disposition, reason) =
            if matches!(self.publish_state, LexicalGenerationPublishState::Published) {
                (
                    LexicalCleanupDisposition::CurrentPublished,
                    "shard is part of the published search surface".to_string(),
                )
            } else if shard.pinned {
                (
                    LexicalCleanupDisposition::PinnedRetained,
                    "shard is pinned by current retention policy".to_string(),
                )
            } else if matches!(
                self.publish_state,
                LexicalGenerationPublishState::Quarantined
            ) || matches!(shard.state, LexicalShardLifecycleState::Quarantined)
            {
                (
                    LexicalCleanupDisposition::QuarantinedRetained,
                    shard
                        .quarantine_reason
                        .clone()
                        .or_else(|| shard.recovery_reason.clone())
                        .unwrap_or_else(|| "quarantined shard requires inspection".to_string()),
                )
            } else if self.generation_has_active_work()
                || matches!(
                    shard.state,
                    LexicalShardLifecycleState::Building
                        | LexicalShardLifecycleState::Staged
                        | LexicalShardLifecycleState::Resumable
                )
            {
                (
                    LexicalCleanupDisposition::ActiveWork,
                    "shard belongs to active or resumable maintenance work".to_string(),
                )
            } else if matches!(
                self.publish_state,
                LexicalGenerationPublishState::Superseded
            ) {
                if shard.reclaimable {
                    (
                        LexicalCleanupDisposition::SupersededReclaimable,
                        "superseded shard is unpinned and safe to reclaim after dry-run approval"
                            .to_string(),
                    )
                } else {
                    (
                        LexicalCleanupDisposition::SupersededRetained,
                        "superseded shard is retained by policy".to_string(),
                    )
                }
            } else if matches!(shard.state, LexicalShardLifecycleState::Abandoned)
                || matches!(self.build_state, LexicalGenerationBuildState::Failed)
            {
                if shard.reclaimable {
                    (
                        LexicalCleanupDisposition::FailedReclaimable,
                        shard.recovery_reason.clone().unwrap_or_else(|| {
                            "failed shard can be rebuilt from source".to_string()
                        }),
                    )
                } else {
                    (
                        LexicalCleanupDisposition::FailedRetained,
                        shard.recovery_reason.clone().unwrap_or_else(|| {
                            "failed shard is retained for inspection".to_string()
                        }),
                    )
                }
            } else {
                (
                    LexicalCleanupDisposition::ActiveWork,
                    "shard is staged until generation lifecycle reaches a terminal state"
                        .to_string(),
                )
            };

        let reclaimable_bytes = if matches!(
            disposition,
            LexicalCleanupDisposition::SupersededReclaimable
                | LexicalCleanupDisposition::FailedReclaimable
        ) && shard.reclaimable
            && !shard.pinned
        {
            shard.artifact_bytes
        } else {
            0
        };

        LexicalShardCleanupInventory {
            shard_id: shard.shard_id.clone(),
            state: shard.state,
            disposition,
            reason,
            artifact_bytes: shard.artifact_bytes,
            reclaimable_bytes,
            retained_bytes: shard.artifact_bytes.saturating_sub(reclaimable_bytes),
        }
    }

    fn classify_generation_for_cleanup(
        &self,
        reclaimable_bytes: u64,
    ) -> (LexicalCleanupDisposition, String) {
        if self.is_serveable() {
            return (
                LexicalCleanupDisposition::CurrentPublished,
                "current published lexical generation is never reclaimable".to_string(),
            );
        }
        if matches!(
            self.publish_state,
            LexicalGenerationPublishState::Quarantined
        ) || self
            .shards
            .iter()
            .any(|shard| matches!(shard.state, LexicalShardLifecycleState::Quarantined))
        {
            return (
                LexicalCleanupDisposition::QuarantinedRetained,
                "quarantined lexical generation is retained for inspection".to_string(),
            );
        }
        if self.generation_has_active_work() {
            return (
                LexicalCleanupDisposition::ActiveWork,
                "active lexical generation work is retained".to_string(),
            );
        }
        if matches!(
            self.publish_state,
            LexicalGenerationPublishState::Superseded
        ) {
            return if reclaimable_bytes > 0 {
                (
                    LexicalCleanupDisposition::SupersededReclaimable,
                    "superseded lexical generation has unpinned reclaimable artifacts".to_string(),
                )
            } else {
                (
                    LexicalCleanupDisposition::SupersededRetained,
                    "superseded lexical generation is retained by policy".to_string(),
                )
            };
        }
        if matches!(self.build_state, LexicalGenerationBuildState::Failed)
            || self
                .shards
                .iter()
                .any(|shard| matches!(shard.state, LexicalShardLifecycleState::Abandoned))
        {
            return if reclaimable_bytes > 0 {
                (
                    LexicalCleanupDisposition::FailedReclaimable,
                    "failed lexical generation can be rebuilt from canonical source".to_string(),
                )
            } else {
                (
                    LexicalCleanupDisposition::FailedRetained,
                    "failed lexical generation is retained for inspection".to_string(),
                )
            };
        }
        (
            LexicalCleanupDisposition::PinnedRetained,
            "lexical generation is retained until cleanup policy marks it reclaimable".to_string(),
        )
    }

    fn classify_generation_retention_window(
        &self,
        disposition: LexicalCleanupDisposition,
    ) -> (Option<i64>, String) {
        match disposition {
            LexicalCleanupDisposition::SupersededReclaimable => (
                Some(self.updated_at_ms),
                "superseded generation retention window has elapsed; reclaimable after dry-run approval"
                    .to_string(),
            ),
            LexicalCleanupDisposition::FailedReclaimable => (
                Some(self.updated_at_ms),
                "failed generation retention window has elapsed; canonical SQLite can rebuild it after dry-run approval"
                    .to_string(),
            ),
            LexicalCleanupDisposition::QuarantinedRetained => (
                None,
                "quarantined generation is retained indefinitely until operator inspection clears it"
                    .to_string(),
            ),
            LexicalCleanupDisposition::CurrentPublished => (
                None,
                "current published lexical generation has no retention expiry".to_string(),
            ),
            LexicalCleanupDisposition::ActiveWork => (
                None,
                "active lexical generation work has no retention expiry while locks or resumable work exist"
                    .to_string(),
            ),
            LexicalCleanupDisposition::SupersededRetained => (
                None,
                "superseded generation is retained by policy or pinned shard artifacts".to_string(),
            ),
            LexicalCleanupDisposition::FailedRetained => (
                None,
                "failed generation is retained indefinitely for inspection".to_string(),
            ),
            LexicalCleanupDisposition::PinnedRetained => (
                None,
                "pinned generation has no retention expiry until the pin is removed".to_string(),
            ),
        }
    }

    fn generation_cleanup_allows_reclaim(&self) -> bool {
        if matches!(
            self.publish_state,
            LexicalGenerationPublishState::Quarantined
        ) || self
            .shards
            .iter()
            .any(|shard| matches!(shard.state, LexicalShardLifecycleState::Quarantined))
        {
            return false;
        }
        (matches!(
            self.publish_state,
            LexicalGenerationPublishState::Superseded
        ) || matches!(self.build_state, LexicalGenerationBuildState::Failed)
            || self
                .shards
                .iter()
                .any(|shard| matches!(shard.state, LexicalShardLifecycleState::Abandoned)))
            && !self.generation_has_active_work()
    }

    fn generation_has_active_work(&self) -> bool {
        matches!(
            self.build_state,
            LexicalGenerationBuildState::Scratch
                | LexicalGenerationBuildState::Building
                | LexicalGenerationBuildState::Built
                | LexicalGenerationBuildState::Validating
        ) || matches!(
            self.merge_debt.state,
            LexicalGenerationMergeDebtState::Pending
                | LexicalGenerationMergeDebtState::Running
                | LexicalGenerationMergeDebtState::Paused
                | LexicalGenerationMergeDebtState::Blocked
        )
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
    {
        let file = fs::File::create(&tmp_path).with_context(|| {
            format!(
                "creating scratch lexical generation manifest at {}",
                tmp_path.display()
            )
        })?;
        let mut writer = BufWriter::new(file);
        writer.write_all(&serialized).with_context(|| {
            format!(
                "writing scratch lexical generation manifest at {}",
                tmp_path.display()
            )
        })?;
        writer.flush().with_context(|| {
            format!(
                "flushing scratch lexical generation manifest at {}",
                tmp_path.display()
            )
        })?;
        writer.get_ref().sync_all().with_context(|| {
            format!(
                "syncing scratch lexical generation manifest at {}",
                tmp_path.display()
            )
        })?;
    }
    fs::rename(&tmp_path, &final_path).with_context(|| {
        format!(
            "atomically publishing lexical generation manifest to {}",
            final_path.display()
        )
    })?;
    sync_parent_directory(&final_path)?;
    Ok(())
}

#[cfg(not(windows))]
fn sync_parent_directory(path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let directory = fs::File::open(parent)
        .with_context(|| format!("opening parent directory {} for sync", parent.display()))?;
    directory
        .sync_all()
        .with_context(|| format!("syncing parent directory {}", parent.display()))
}

#[cfg(windows)]
fn sync_parent_directory(_path: &Path) -> Result<()> {
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
        manifest.set_shard_plan_and_budget(
            LexicalGenerationShardPlan {
                plan_id: "plan-fp-deadbeef-2".into(),
                planner_version: 1,
                shard_count: 2,
                packet_contract_version: 1,
                source_db_fingerprint: "fp-deadbeef".into(),
            },
            LexicalGenerationBuildBudget {
                policy_id: "responsive-default".into(),
                effective_settings_fingerprint: "settings-fp-1".into(),
                max_inflight_message_bytes: 8 * 1024 * 1024,
                producer_queue_pages: 4,
                batch_conversation_limit: 64,
                worker_threads: 6,
                controller_reason: Some("reserved_2_cores_for_interactive_use".into()),
                extra_limits: BTreeMap::from([("staged_merge_jobs".into(), 2)]),
            },
            1_700_000_000_250,
        );
        let mut shard_a = LexicalShardManifest::planned("shard-0000", 0, 1_700_000_000_250);
        shard_a.indexed_doc_count = 20;
        shard_a.message_count = 20;
        shard_a.artifact_bytes = 4096;
        shard_a.stable_hash = Some("shard-hash-a".into());
        shard_a.transition(LexicalShardLifecycleState::Published, 1_700_000_000_900);
        shard_a.pinned = true;
        shard_a.reclaimable = false;
        let mut shard_b = LexicalShardManifest::planned("shard-0001", 1, 1_700_000_000_250);
        shard_b.indexed_doc_count = 14;
        shard_b.message_count = 14;
        shard_b.artifact_bytes = 2048;
        shard_b.stable_hash = Some("shard-hash-b".into());
        shard_b.transition(LexicalShardLifecycleState::Published, 1_700_000_000_900);
        shard_b.pinned = true;
        shard_b.reclaimable = false;
        manifest.set_shards(vec![shard_a, shard_b], 1_700_000_000_900);
        manifest.conversation_count = 12;
        manifest.message_count = 34;
        manifest.indexed_doc_count = 34;
        manifest.equivalence_manifest_fingerprint = Some("eq-fp-123".into());
        manifest.transition_build(LexicalGenerationBuildState::Validated, 1_700_000_000_500);
        manifest.transition_publish(LexicalGenerationPublishState::Published, 1_700_000_001_000);
        manifest.record_merge_debt(
            2,
            6144,
            "shard segments are queryable before background consolidation",
            1_700_000_001_100,
        );

        let bytes = serde_json::to_vec(&manifest).unwrap();
        let parsed: LexicalGenerationManifest = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed, manifest);
        assert_eq!(
            parsed.shard_plan.as_ref().unwrap().plan_id,
            "plan-fp-deadbeef-2"
        );
        assert_eq!(
            parsed
                .build_budget
                .as_ref()
                .unwrap()
                .effective_settings_fingerprint,
            "settings-fp-1"
        );
        assert_eq!(parsed.shards.len(), 2);
        assert!(parsed.is_serveable());
        assert!(parsed.merge_debt.has_pending_work());
        assert!(!parsed.is_fully_consolidated());
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
        let merge_debt_states: Vec<(LexicalGenerationMergeDebtState, &str)> = vec![
            (LexicalGenerationMergeDebtState::None, "none"),
            (LexicalGenerationMergeDebtState::Pending, "pending"),
            (LexicalGenerationMergeDebtState::Running, "running"),
            (LexicalGenerationMergeDebtState::Paused, "paused"),
            (LexicalGenerationMergeDebtState::Blocked, "blocked"),
            (LexicalGenerationMergeDebtState::Complete, "complete"),
            (LexicalGenerationMergeDebtState::Cancelled, "cancelled"),
        ];
        for (state, expected) in merge_debt_states {
            let json = serde_json::to_string(&state).unwrap();
            assert_eq!(json, format!("\"{expected}\""));
        }
        let shard_states: Vec<(LexicalShardLifecycleState, &str)> = vec![
            (LexicalShardLifecycleState::Planned, "planned"),
            (LexicalShardLifecycleState::Building, "building"),
            (LexicalShardLifecycleState::Staged, "staged"),
            (LexicalShardLifecycleState::Validated, "validated"),
            (LexicalShardLifecycleState::Published, "published"),
            (LexicalShardLifecycleState::Resumable, "resumable"),
            (LexicalShardLifecycleState::Quarantined, "quarantined"),
            (LexicalShardLifecycleState::Abandoned, "abandoned"),
        ];
        for (state, expected) in shard_states {
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

    #[test]
    fn published_generation_can_serve_before_deferred_merge_debt_settles() {
        let mut manifest = LexicalGenerationManifest::new_scratch("gen-debt", "attempt-1", "fp", 1);
        manifest.transition_build(LexicalGenerationBuildState::Validated, 2);
        manifest.transition_publish(LexicalGenerationPublishState::Published, 3);

        manifest.record_merge_debt(
            3,
            12_288,
            "segment consolidation is safe to defer after atomic publish",
            4,
        );

        assert!(
            manifest.is_serveable(),
            "merge debt must not drag safe published assets off the query path"
        );
        assert!(
            !manifest.is_fully_consolidated(),
            "pending debt should keep fully-settled status false"
        );
        assert_eq!(
            manifest.merge_debt.state,
            LexicalGenerationMergeDebtState::Pending
        );
        assert_eq!(manifest.merge_debt.pending_shard_count, 3);
        assert_eq!(manifest.merge_debt.pending_artifact_bytes, 12_288);
        assert!(manifest.merge_debt.has_pending_work());
    }

    #[test]
    fn merge_debt_tracks_background_pause_block_and_completion_reasons() {
        let mut manifest =
            LexicalGenerationManifest::new_scratch("gen-debt-flow", "attempt-1", "fp", 1);
        manifest.record_merge_debt(2, 2048, "two shard fragments need compaction", 2);

        manifest.transition_merge_debt(
            LexicalGenerationMergeDebtState::Running,
            3,
            Some("background worker acquired consolidation lease".into()),
            Some("controller admitted one low-priority merge job".into()),
        );
        assert_eq!(
            manifest.merge_debt.state,
            LexicalGenerationMergeDebtState::Running
        );
        assert_eq!(
            manifest.merge_debt.controller_reason.as_deref(),
            Some("controller admitted one low-priority merge job")
        );

        manifest.transition_merge_debt(
            LexicalGenerationMergeDebtState::Paused,
            4,
            Some("foreground search pressure exceeded reserve budget".into()),
            Some("controller yielded to interactive workload".into()),
        );
        assert_eq!(
            manifest.merge_debt.state,
            LexicalGenerationMergeDebtState::Paused
        );
        assert!(manifest.merge_debt.has_pending_work());

        manifest.transition_merge_debt(
            LexicalGenerationMergeDebtState::Blocked,
            5,
            Some("publish lock held by another generation".into()),
            Some("single-flight lock prevented duplicate compaction".into()),
        );
        assert_eq!(
            manifest.merge_debt.state,
            LexicalGenerationMergeDebtState::Blocked
        );

        manifest.transition_merge_debt(
            LexicalGenerationMergeDebtState::Complete,
            6,
            Some("background consolidation finished".into()),
            Some("controller budget remained below pressure threshold".into()),
        );
        assert!(manifest.merge_debt.is_fully_settled());
        assert_eq!(manifest.merge_debt.pending_shard_count, 0);
        assert_eq!(manifest.merge_debt.pending_artifact_bytes, 0);
        assert_eq!(manifest.updated_at_ms, 6);
    }

    #[test]
    fn recovery_decision_attaches_published_generation() {
        let mut manifest = LexicalGenerationManifest::new_scratch(
            "gen-published",
            "attempt-1",
            "fp-published",
            10,
        );
        manifest.set_shard_plan_and_budget(test_shard_plan("fp-published", 2), test_budget(), 11);
        let mut shard_a = LexicalShardManifest::planned("shard-a", 0, 11);
        shard_a.transition(LexicalShardLifecycleState::Published, 20);
        let mut shard_b = LexicalShardManifest::planned("shard-b", 1, 11);
        shard_b.transition(LexicalShardLifecycleState::Published, 20);
        manifest.set_shards(vec![shard_a, shard_b], 20);
        manifest.transition_build(LexicalGenerationBuildState::Validated, 30);
        manifest.transition_publish(LexicalGenerationPublishState::Published, 31);

        let decision = manifest.recovery_decision();
        assert_eq!(
            decision.action,
            LexicalGenerationRecoveryAction::AttachPublished
        );
        assert!(decision.resumable_shards.is_empty());
        assert!(decision.quarantined_shards.is_empty());
    }

    #[test]
    fn recovery_decision_publishes_complete_validated_shard_set() {
        let mut manifest = LexicalGenerationManifest::new_scratch(
            "gen-validated",
            "attempt-1",
            "fp-validated",
            10,
        );
        manifest.set_shard_plan_and_budget(test_shard_plan("fp-validated", 2), test_budget(), 11);
        let mut shard_a = LexicalShardManifest::planned("shard-a", 0, 11);
        shard_a.transition(LexicalShardLifecycleState::Validated, 20);
        let mut shard_b = LexicalShardManifest::planned("shard-b", 1, 11);
        shard_b.transition(LexicalShardLifecycleState::Validated, 20);
        manifest.set_shards(vec![shard_a, shard_b], 20);
        manifest.transition_build(LexicalGenerationBuildState::Validated, 30);

        let decision = manifest.recovery_decision();
        assert_eq!(
            decision.action,
            LexicalGenerationRecoveryAction::PublishValidated
        );
        assert!(decision.reason.contains("complete publish-ready shard set"));
    }

    #[test]
    fn recovery_decision_resumes_resumable_staged_shards() {
        let mut manifest =
            LexicalGenerationManifest::new_scratch("gen-resume", "attempt-1", "fp-resume", 10);
        manifest.set_shard_plan_and_budget(test_shard_plan("fp-resume", 2), test_budget(), 11);
        manifest.set_shards(
            vec![
                LexicalShardManifest::planned("shard-a", 0, 11),
                LexicalShardManifest::planned("shard-b", 1, 11),
            ],
            12,
        );
        assert!(manifest.transition_shard(
            "shard-a",
            LexicalShardLifecycleState::Resumable,
            20,
            Some("builder checkpoint reached after doc flush".into()),
        ));
        assert!(manifest.transition_shard("shard-b", LexicalShardLifecycleState::Staged, 21, None));
        manifest.transition_build(LexicalGenerationBuildState::Building, 30);

        let decision = manifest.recovery_decision();
        assert_eq!(
            decision.action,
            LexicalGenerationRecoveryAction::ResumeStaged
        );
        assert_eq!(
            decision.resumable_shards,
            vec!["shard-a".to_string(), "shard-b".to_string()]
        );
        assert!(decision.quarantined_shards.is_empty());
    }

    #[test]
    fn recovery_decision_keeps_quarantined_shards_out_of_search() {
        let mut manifest = LexicalGenerationManifest::new_scratch(
            "gen-quarantine",
            "attempt-1",
            "fp-quarantine",
            10,
        );
        manifest.set_shard_plan_and_budget(test_shard_plan("fp-quarantine", 2), test_budget(), 11);
        manifest.set_shards(
            vec![
                LexicalShardManifest::planned("shard-a", 0, 11),
                LexicalShardManifest::planned("shard-b", 1, 11),
            ],
            12,
        );
        assert!(manifest.transition_shard(
            "shard-b",
            LexicalShardLifecycleState::Quarantined,
            20,
            Some("tantivy open probe failed".into()),
        ));
        manifest.transition_build(LexicalGenerationBuildState::Validated, 30);

        let decision = manifest.recovery_decision();
        assert_eq!(
            decision.action,
            LexicalGenerationRecoveryAction::KeepQuarantined
        );
        assert_eq!(decision.quarantined_shards, vec!["shard-b".to_string()]);
        assert!(decision.reason.contains("must stay out of search"));
    }

    #[test]
    fn recovery_decision_discards_abandoned_or_failed_generation() {
        let mut manifest = LexicalGenerationManifest::new_scratch(
            "gen-abandoned",
            "attempt-1",
            "fp-abandoned",
            10,
        );
        manifest.set_shard_plan_and_budget(test_shard_plan("fp-abandoned", 1), test_budget(), 11);
        manifest.set_shards(vec![LexicalShardManifest::planned("shard-a", 0, 11)], 12);
        assert!(manifest.transition_shard(
            "shard-a",
            LexicalShardLifecycleState::Abandoned,
            20,
            Some("source fingerprint changed mid-build".into()),
        ));
        manifest.transition_build(LexicalGenerationBuildState::Failed, 30);

        let decision = manifest.recovery_decision();
        assert_eq!(
            decision.action,
            LexicalGenerationRecoveryAction::DiscardAndRebuild
        );
        assert_eq!(decision.abandoned_shards, vec!["shard-a".to_string()]);
        assert!(decision.reason.contains("must rebuild from source"));
    }

    #[test]
    fn shard_transition_records_retention_and_recovery_reasons() {
        let mut manifest =
            LexicalGenerationManifest::new_scratch("gen-retention", "attempt-1", "fp", 1);
        manifest.set_shards(vec![LexicalShardManifest::planned("shard-a", 0, 1)], 2);

        assert!(manifest.transition_shard(
            "shard-a",
            LexicalShardLifecycleState::Quarantined,
            3,
            Some("checksum mismatch".into()),
        ));
        let shard = &manifest.shards[0];
        assert!(!shard.reclaimable);
        assert!(!shard.pinned);
        assert_eq!(
            shard.quarantine_reason.as_deref(),
            Some("checksum mismatch")
        );

        assert!(manifest.transition_shard(
            "shard-a",
            LexicalShardLifecycleState::Published,
            4,
            None,
        ));
        let shard = &manifest.shards[0];
        assert!(shard.pinned);
        assert!(!shard.reclaimable);
    }

    #[test]
    fn cleanup_inventory_retains_current_published_generation() {
        let mut manifest =
            LexicalGenerationManifest::new_scratch("gen-current", "attempt-1", "fp", 1);
        let mut shard = test_shard("shard-live", 0, LexicalShardLifecycleState::Published, 4096);
        shard.pinned = true;
        shard.reclaimable = false;
        manifest.set_shards(vec![shard], 2);
        manifest.transition_build(LexicalGenerationBuildState::Validated, 3);
        manifest.transition_publish(LexicalGenerationPublishState::Published, 4);

        let inventory = manifest.cleanup_inventory();
        assert_eq!(
            inventory.disposition,
            LexicalCleanupDisposition::CurrentPublished
        );
        assert_eq!(inventory.artifact_bytes, 4096);
        assert_eq!(inventory.reclaimable_bytes, 0);
        assert_eq!(inventory.retained_bytes, 4096);
        assert_eq!(
            inventory.shards[0].disposition,
            LexicalCleanupDisposition::CurrentPublished
        );
    }

    #[test]
    fn cleanup_inventory_marks_superseded_unpinned_shards_reclaimable() {
        let mut manifest = LexicalGenerationManifest::new_scratch("gen-old", "attempt-1", "fp", 1);
        let mut reclaimable = test_shard(
            "shard-old-a",
            0,
            LexicalShardLifecycleState::Published,
            8192,
        );
        reclaimable.pinned = false;
        reclaimable.reclaimable = true;
        let mut retained = test_shard(
            "shard-old-b",
            1,
            LexicalShardLifecycleState::Published,
            2048,
        );
        retained.pinned = true;
        retained.reclaimable = false;
        manifest.set_shards(vec![reclaimable, retained], 2);
        manifest.transition_build(LexicalGenerationBuildState::Validated, 3);
        manifest.transition_publish(LexicalGenerationPublishState::Superseded, 4);

        let inventory = manifest.cleanup_inventory();
        assert_eq!(
            inventory.disposition,
            LexicalCleanupDisposition::SupersededReclaimable
        );
        assert_eq!(inventory.artifact_bytes, 10_240);
        assert_eq!(inventory.reclaimable_bytes, 8192);
        assert_eq!(inventory.retained_bytes, 2048);
        assert_eq!(
            inventory.retain_until_ms,
            Some(4),
            "reclaimable superseded generations should expose the retention-window boundary"
        );
        assert!(
            inventory.retention_reason.contains("superseded generation"),
            "superseded retention classification should explain why it is reclaimable"
        );
        assert_eq!(
            inventory.shards[0].disposition,
            LexicalCleanupDisposition::SupersededReclaimable
        );
        assert_eq!(
            inventory.shards[1].disposition,
            LexicalCleanupDisposition::PinnedRetained
        );
    }

    #[test]
    fn cleanup_inventory_keeps_quarantined_artifacts_for_inspection() {
        let mut manifest =
            LexicalGenerationManifest::new_scratch("gen-quarantined", "attempt-1", "fp", 1);
        let shard = test_shard(
            "shard-bad",
            0,
            LexicalShardLifecycleState::Quarantined,
            4096,
        );
        manifest.set_shards(vec![shard], 2);
        assert!(manifest.transition_shard(
            "shard-bad",
            LexicalShardLifecycleState::Quarantined,
            3,
            Some("manifest checksum mismatch".into()),
        ));
        manifest.transition_publish(LexicalGenerationPublishState::Quarantined, 4);

        let inventory = manifest.cleanup_inventory();
        assert_eq!(
            inventory.disposition,
            LexicalCleanupDisposition::QuarantinedRetained
        );
        assert_eq!(inventory.reclaimable_bytes, 0);
        assert_eq!(inventory.retained_bytes, 4096);
        assert_eq!(
            inventory.retain_until_ms, None,
            "quarantined generations are retained indefinitely until inspection"
        );
        assert!(
            inventory.retention_reason.contains("operator inspection"),
            "quarantined retention classification should explain the inspection hold"
        );
        assert_eq!(
            inventory.shards[0].reason,
            "manifest checksum mismatch".to_string()
        );
    }

    #[test]
    fn cleanup_inventory_preserves_active_merge_debt() {
        let mut manifest =
            LexicalGenerationManifest::new_scratch("gen-debt-active", "attempt-1", "fp", 1);
        let mut shard = test_shard(
            "shard-pending",
            0,
            LexicalShardLifecycleState::Published,
            1024,
        );
        shard.pinned = false;
        shard.reclaimable = true;
        manifest.set_shards(vec![shard], 2);
        manifest.transition_build(LexicalGenerationBuildState::Validated, 3);
        manifest.transition_publish(LexicalGenerationPublishState::Superseded, 4);
        manifest.record_merge_debt(1, 2048, "background merge still running", 5);

        let inventory = manifest.cleanup_inventory();
        assert_eq!(inventory.disposition, LexicalCleanupDisposition::ActiveWork);
        assert_eq!(inventory.artifact_bytes, 3072);
        assert_eq!(inventory.reclaimable_bytes, 0);
        assert_eq!(inventory.retained_bytes, 3072);
        assert!(inventory.reason.contains("active"));

        let plan = LexicalCleanupDryRunPlan::from_manifests([&manifest]);
        assert_eq!(plan.total_retained_bytes, 3072);
        assert_eq!(plan.protected_retained_bytes, 3072);
        assert_eq!(
            plan.protected_generation_ids,
            vec!["gen-debt-active".to_string()]
        );

        let gate = plan.apply_gate_with_fingerprint(true, Some(&plan.approval_fingerprint));
        assert_eq!(gate.protected_retained_bytes, 3072);
        assert_eq!(
            gate.protected_generation_ids,
            vec!["gen-debt-active".to_string()]
        );
    }

    #[test]
    fn cleanup_dry_run_plan_summarizes_reclaim_retain_and_quarantine_buckets() {
        let mut current =
            LexicalGenerationManifest::new_scratch("gen-current", "attempt-1", "fp", 1);
        let mut current_shard =
            test_shard("shard-live", 0, LexicalShardLifecycleState::Published, 4096);
        current_shard.pinned = true;
        current_shard.reclaimable = false;
        current.set_shards(vec![current_shard], 2);
        current.transition_build(LexicalGenerationBuildState::Validated, 3);
        current.transition_publish(LexicalGenerationPublishState::Published, 4);

        let mut superseded =
            LexicalGenerationManifest::new_scratch("gen-old", "attempt-2", "fp", 10);
        let mut reclaimable = test_shard(
            "shard-old-a",
            0,
            LexicalShardLifecycleState::Published,
            8192,
        );
        reclaimable.pinned = false;
        reclaimable.reclaimable = true;
        let mut retained = test_shard(
            "shard-old-b",
            1,
            LexicalShardLifecycleState::Published,
            1024,
        );
        retained.pinned = true;
        retained.reclaimable = false;
        superseded.set_shards(vec![reclaimable, retained], 11);
        superseded.transition_build(LexicalGenerationBuildState::Validated, 12);
        superseded.transition_publish(LexicalGenerationPublishState::Superseded, 13);

        let mut quarantined =
            LexicalGenerationManifest::new_scratch("gen-quarantined", "attempt-3", "fp", 20);
        let quarantined_shard = test_shard(
            "shard-bad",
            0,
            LexicalShardLifecycleState::Quarantined,
            2048,
        );
        quarantined.set_shards(vec![quarantined_shard], 21);
        assert!(quarantined.transition_shard(
            "shard-bad",
            LexicalShardLifecycleState::Quarantined,
            22,
            Some("checksum mismatch".into()),
        ));
        quarantined.transition_publish(LexicalGenerationPublishState::Quarantined, 23);

        let plan = LexicalCleanupDryRunPlan::from_manifests([&current, &superseded, &quarantined]);

        assert!(plan.dry_run);
        assert!(plan.has_reclaimable_artifacts());
        assert_eq!(plan.generation_count, 3);
        assert_eq!(plan.total_artifact_bytes, 15_360);
        assert_eq!(plan.total_reclaimable_bytes, 8192);
        assert_eq!(plan.total_retained_bytes, 7168);
        assert_eq!(plan.protected_retained_bytes, 7168);
        assert_eq!(plan.reclaimable_generation_ids, vec!["gen-old"]);
        assert_eq!(
            plan.fully_retained_generation_ids,
            vec!["gen-current", "gen-quarantined"]
        );
        assert_eq!(
            plan.protected_generation_ids,
            vec!["gen-current", "gen-old", "gen-quarantined"]
        );
        assert_eq!(plan.quarantined_generation_ids, vec!["gen-quarantined"]);
        assert_eq!(
            plan.inspection_required_generation_ids(),
            vec!["gen-quarantined".to_string()]
        );
        assert_eq!(plan.inspection_required_count, 1);
        assert_eq!(plan.inspection_required_retained_bytes, 2048);
        assert_eq!(plan.inspection_required_retained_bytes(), 2048);
        assert_eq!(
            plan.inspection_items,
            vec![LexicalCleanupInspectionItem {
                generation_id: "gen-quarantined".to_string(),
                shard_id: Some("shard-bad".to_string()),
                disposition: LexicalCleanupDisposition::QuarantinedRetained,
                reason: "checksum mismatch".to_string(),
                retained_bytes: 2048,
            }]
        );
        assert_eq!(
            plan.disposition_counts
                .get(&LexicalCleanupDisposition::CurrentPublished),
            Some(&1)
        );
        assert_eq!(
            plan.disposition_counts
                .get(&LexicalCleanupDisposition::SupersededReclaimable),
            Some(&1)
        );
        assert_eq!(
            plan.disposition_counts
                .get(&LexicalCleanupDisposition::QuarantinedRetained),
            Some(&1)
        );
        let current_generation_summary = plan
            .generation_disposition_summaries
            .get(&LexicalCleanupDisposition::CurrentPublished)
            .expect("current published generation summary");
        assert_eq!(current_generation_summary.generation_count, 1);
        assert_eq!(current_generation_summary.artifact_bytes, 4096);
        assert_eq!(current_generation_summary.reclaimable_bytes, 0);
        assert_eq!(current_generation_summary.retained_bytes, 4096);

        let superseded_generation_summary = plan
            .generation_disposition_summaries
            .get(&LexicalCleanupDisposition::SupersededReclaimable)
            .expect("superseded reclaimable generation summary");
        assert_eq!(superseded_generation_summary.generation_count, 1);
        assert_eq!(superseded_generation_summary.artifact_bytes, 9216);
        assert_eq!(superseded_generation_summary.reclaimable_bytes, 8192);
        assert_eq!(superseded_generation_summary.retained_bytes, 1024);

        let quarantined_generation_summary = plan
            .generation_disposition_summaries
            .get(&LexicalCleanupDisposition::QuarantinedRetained)
            .expect("quarantined generation summary");
        assert_eq!(quarantined_generation_summary.generation_count, 1);
        assert_eq!(quarantined_generation_summary.artifact_bytes, 2048);
        assert_eq!(quarantined_generation_summary.reclaimable_bytes, 0);
        assert_eq!(quarantined_generation_summary.retained_bytes, 2048);

        let reclaimable_summary = plan
            .shard_disposition_summaries
            .get(&LexicalCleanupDisposition::SupersededReclaimable)
            .expect("superseded reclaimable shard summary");
        assert_eq!(reclaimable_summary.shard_count, 1);
        assert_eq!(reclaimable_summary.artifact_bytes, 8192);
        assert_eq!(reclaimable_summary.reclaimable_bytes, 8192);
        assert_eq!(reclaimable_summary.retained_bytes, 0);

        let pinned_summary = plan
            .shard_disposition_summaries
            .get(&LexicalCleanupDisposition::PinnedRetained)
            .expect("pinned retained shard summary");
        assert_eq!(pinned_summary.shard_count, 1);
        assert_eq!(pinned_summary.artifact_bytes, 1024);
        assert_eq!(pinned_summary.reclaimable_bytes, 0);
        assert_eq!(pinned_summary.retained_bytes, 1024);

        let json = serde_json::to_value(&plan).expect("serialize cleanup dry-run plan");
        assert_eq!(json["protected_retained_bytes"], 7168);
        assert_eq!(json["protected_generation_ids"][0], "gen-current");
        assert_eq!(json["protected_generation_ids"][1], "gen-old");
        assert_eq!(json["protected_generation_ids"][2], "gen-quarantined");
        assert_eq!(
            json["generation_disposition_summaries"]["current_published"]["retained_bytes"],
            4096
        );
        assert_eq!(
            json["generation_disposition_summaries"]["superseded_reclaimable"]["generation_count"],
            1
        );
        assert_eq!(
            json["generation_disposition_summaries"]["superseded_reclaimable"]["reclaimable_bytes"],
            8192
        );
        assert_eq!(
            json["generation_disposition_summaries"]["quarantined_retained"]["retained_bytes"],
            2048
        );
        assert_eq!(
            json["shard_disposition_summaries"]["superseded_reclaimable"]["reclaimable_bytes"],
            8192
        );
        assert_eq!(
            json["shard_disposition_summaries"]["pinned_retained"]["retained_bytes"],
            1024
        );
        assert_eq!(
            json["inspection_items"][0]["generation_id"],
            "gen-quarantined"
        );
        assert_eq!(
            json["inspection_required_generation_ids"][0],
            "gen-quarantined"
        );
        assert_eq!(json["inspection_required_count"], 1);
        assert_eq!(json["inspection_required_retained_bytes"], 2048);
        assert_eq!(json["inspection_items"][0]["shard_id"], "shard-bad");
        assert_eq!(
            json["inspection_items"][0]["disposition"],
            "quarantined_retained"
        );
        assert_eq!(json["inspection_items"][0]["retained_bytes"], 2048);
        assert_eq!(plan.inventories.len(), 3);
    }

    #[test]
    fn cleanup_dry_run_plan_lists_only_reclaimable_shard_candidates() {
        let mut current =
            LexicalGenerationManifest::new_scratch("gen-current", "attempt-1", "fp", 1);
        let mut current_shard =
            test_shard("shard-live", 0, LexicalShardLifecycleState::Published, 4096);
        current_shard.pinned = true;
        current_shard.reclaimable = false;
        current.set_shards(vec![current_shard], 2);
        current.transition_build(LexicalGenerationBuildState::Validated, 3);
        current.transition_publish(LexicalGenerationPublishState::Published, 4);

        let mut superseded =
            LexicalGenerationManifest::new_scratch("gen-old", "attempt-2", "fp", 10);
        let mut old_a = test_shard(
            "shard-old-a",
            0,
            LexicalShardLifecycleState::Published,
            8192,
        );
        old_a.pinned = false;
        old_a.reclaimable = true;
        let mut old_b = test_shard(
            "shard-old-b",
            1,
            LexicalShardLifecycleState::Published,
            2048,
        );
        old_b.pinned = true;
        old_b.reclaimable = false;
        superseded.set_shards(vec![old_a, old_b], 11);
        superseded.transition_build(LexicalGenerationBuildState::Validated, 12);
        superseded.transition_publish(LexicalGenerationPublishState::Superseded, 13);

        let mut failed =
            LexicalGenerationManifest::new_scratch("gen-failed", "attempt-3", "fp", 20);
        let mut failed_shard = test_shard(
            "shard-failed",
            0,
            LexicalShardLifecycleState::Abandoned,
            1024,
        );
        failed_shard.reclaimable = true;
        failed.set_shards(vec![failed_shard], 21);
        assert!(failed.transition_shard(
            "shard-failed",
            LexicalShardLifecycleState::Abandoned,
            22,
            Some("source changed before publish".into()),
        ));
        failed.transition_build(LexicalGenerationBuildState::Failed, 23);

        let mut quarantined =
            LexicalGenerationManifest::new_scratch("gen-quarantined", "attempt-4", "fp", 30);
        let quarantined_shard =
            test_shard("shard-bad", 0, LexicalShardLifecycleState::Quarantined, 512);
        quarantined.set_shards(vec![quarantined_shard], 31);
        assert!(quarantined.transition_shard(
            "shard-bad",
            LexicalShardLifecycleState::Quarantined,
            32,
            Some("checksum mismatch".into()),
        ));
        quarantined.transition_publish(LexicalGenerationPublishState::Quarantined, 33);

        let plan = LexicalCleanupDryRunPlan::from_manifests([
            &current,
            &superseded,
            &failed,
            &quarantined,
        ]);
        let candidates = plan.reclaim_candidates();

        assert_eq!(plan.reclaim_candidates, candidates);
        assert_eq!(
            candidates,
            vec![
                LexicalCleanupReclaimCandidate {
                    generation_id: "gen-old".to_string(),
                    shard_id: "shard-old-a".to_string(),
                    disposition: LexicalCleanupDisposition::SupersededReclaimable,
                    reason:
                        "superseded shard is unpinned and safe to reclaim after dry-run approval"
                            .to_string(),
                    reclaimable_bytes: 8192,
                },
                LexicalCleanupReclaimCandidate {
                    generation_id: "gen-failed".to_string(),
                    shard_id: "shard-failed".to_string(),
                    disposition: LexicalCleanupDisposition::FailedReclaimable,
                    reason: "source changed before publish".to_string(),
                    reclaimable_bytes: 1024,
                },
            ]
        );
        assert_eq!(plan.total_reclaimable_bytes, 9216);
        assert_eq!(plan.total_retained_bytes, 6656);
        assert_eq!(plan.protected_retained_bytes, 6656);
        assert_eq!(
            plan.protected_generation_ids,
            vec!["gen-current", "gen-old", "gen-quarantined"]
        );

        let json = serde_json::to_value(&plan).expect("serialize cleanup dry-run plan");
        assert_eq!(json["protected_retained_bytes"], 6656);
        assert_eq!(json["reclaim_candidates"][0]["generation_id"], "gen-old");
        assert_eq!(json["reclaim_candidates"][0]["shard_id"], "shard-old-a");
        assert_eq!(
            json["reclaim_candidates"][0]["disposition"],
            "superseded_reclaimable"
        );
        assert_eq!(json["reclaim_candidates"][0]["reclaimable_bytes"], 8192);
        assert_eq!(json["reclaim_candidates"][1]["generation_id"], "gen-failed");
        assert_eq!(
            json["reclaim_candidates"][1]["disposition"],
            "failed_reclaimable"
        );
        assert_eq!(
            json["reclaim_candidates"]
                .as_array()
                .expect("reclaim_candidates must serialize as an array")
                .len(),
            2
        );
    }

    #[test]
    fn cleanup_apply_gate_requires_approval_and_blocks_active_work() {
        let mut superseded =
            LexicalGenerationManifest::new_scratch("gen-old", "attempt-1", "fp", 1);
        let mut reclaimable_shard =
            test_shard("shard-old", 0, LexicalShardLifecycleState::Published, 4096);
        reclaimable_shard.pinned = false;
        reclaimable_shard.reclaimable = true;
        superseded.set_shards(vec![reclaimable_shard], 2);
        superseded.transition_build(LexicalGenerationBuildState::Validated, 3);
        superseded.transition_publish(LexicalGenerationPublishState::Superseded, 4);

        let mut active =
            LexicalGenerationManifest::new_scratch("gen-active", "attempt-2", "fp", 10);
        active.set_shards(
            vec![test_shard(
                "shard-active",
                0,
                LexicalShardLifecycleState::Building,
                2048,
            )],
            11,
        );
        active.transition_build(LexicalGenerationBuildState::Building, 12);

        let mut quarantined =
            LexicalGenerationManifest::new_scratch("gen-quarantined", "attempt-3", "fp", 20);
        quarantined.set_shards(
            vec![test_shard(
                "shard-bad",
                0,
                LexicalShardLifecycleState::Quarantined,
                512,
            )],
            21,
        );
        assert!(quarantined.transition_shard(
            "shard-bad",
            LexicalShardLifecycleState::Quarantined,
            22,
            Some("checksum mismatch".into()),
        ));
        quarantined.transition_publish(LexicalGenerationPublishState::Quarantined, 23);

        let plan = LexicalCleanupDryRunPlan::from_manifests([&superseded, &active, &quarantined]);

        let blocked = plan.apply_gate(false);
        assert!(!blocked.apply_allowed);
        assert!(blocked.dry_run);
        assert!(!blocked.explicit_operator_approval);
        assert_eq!(
            blocked.approval_fingerprint_status,
            LexicalCleanupApprovalFingerprintStatus::NotRequested
        );
        assert_eq!(blocked.generation_count, 3);
        assert_eq!(blocked.total_artifact_bytes, 6656);
        assert_eq!(blocked.total_retained_bytes, 2560);
        assert_eq!(
            blocked
                .disposition_counts
                .get(&LexicalCleanupDisposition::SupersededReclaimable),
            Some(&1)
        );
        assert_eq!(
            blocked
                .generation_disposition_summaries
                .get(&LexicalCleanupDisposition::ActiveWork)
                .map(|summary| summary.retained_bytes),
            Some(2048)
        );
        assert_eq!(
            blocked
                .shard_disposition_summaries
                .get(&LexicalCleanupDisposition::QuarantinedRetained)
                .map(|summary| summary.retained_bytes),
            Some(512)
        );
        assert_eq!(
            blocked.blocker_codes,
            vec![
                LexicalCleanupApplyBlocker::OperatorApprovalRequired,
                LexicalCleanupApplyBlocker::ActiveGenerationWork,
            ]
        );
        assert_eq!(blocked.active_generation_ids, vec!["gen-active"]);
        assert_eq!(
            blocked.reclaimable_generation_ids,
            vec!["gen-old".to_string()]
        );
        assert_eq!(
            blocked.fully_retained_generation_ids,
            vec!["gen-active".to_string(), "gen-quarantined".to_string()]
        );
        assert_eq!(
            blocked.quarantined_generation_ids,
            vec!["gen-quarantined".to_string()]
        );
        assert_eq!(blocked.candidate_count, 1);
        assert_eq!(blocked.reclaimable_bytes, 4096);
        assert_eq!(
            blocked.candidate_previews,
            vec![LexicalCleanupReclaimCandidate {
                generation_id: "gen-old".to_string(),
                shard_id: "shard-old".to_string(),
                disposition: LexicalCleanupDisposition::SupersededReclaimable,
                reason: "superseded shard is unpinned and safe to reclaim after dry-run approval"
                    .to_string(),
                reclaimable_bytes: 4096,
            }]
        );
        assert_eq!(
            blocked.inspection_required_generation_ids,
            vec!["gen-quarantined".to_string()]
        );
        assert_eq!(
            blocked.inspection_previews,
            vec![LexicalCleanupInspectionItem {
                generation_id: "gen-quarantined".to_string(),
                shard_id: Some("shard-bad".to_string()),
                disposition: LexicalCleanupDisposition::QuarantinedRetained,
                reason: "checksum mismatch".to_string(),
                retained_bytes: 512,
            }]
        );
        assert_eq!(blocked.inspection_required_count, 1);
        assert_eq!(blocked.inspection_required_retained_bytes, 512);
        assert_eq!(
            blocked.protected_generation_ids,
            vec!["gen-active".to_string(), "gen-quarantined".to_string()]
        );
        assert_eq!(blocked.protected_retained_bytes, 2560);
        assert!(
            blocked
                .blocked_reasons
                .iter()
                .any(|reason| reason.contains("explicit operator approval")),
            "missing approval blocker: {:?}",
            blocked.blocked_reasons
        );
        assert!(
            blocked
                .blocked_reasons
                .iter()
                .any(|reason| reason.contains("gen-active")),
            "missing active-work blocker: {:?}",
            blocked.blocked_reasons
        );

        let active_still_blocks = plan.apply_gate(true);
        assert!(!active_still_blocks.apply_allowed);
        assert!(active_still_blocks.explicit_operator_approval);
        assert_eq!(
            active_still_blocks.approval_fingerprint_status,
            LexicalCleanupApprovalFingerprintStatus::Missing
        );
        assert_eq!(
            active_still_blocks.blocker_codes,
            vec![
                LexicalCleanupApplyBlocker::ApprovalFingerprintMissing,
                LexicalCleanupApplyBlocker::ActiveGenerationWork,
            ]
        );
        assert!(!active_still_blocks.approval_fingerprint_matches);
        assert!(
            active_still_blocks
                .blocked_reasons
                .iter()
                .any(|reason| reason.contains("approval fingerprint")),
            "missing fingerprint blocker: {:?}",
            active_still_blocks.blocked_reasons
        );

        let active_fingerprint_still_blocks =
            plan.apply_gate_with_fingerprint(true, Some(&plan.approval_fingerprint));
        assert!(!active_fingerprint_still_blocks.apply_allowed);
        assert_eq!(
            active_fingerprint_still_blocks.approval_fingerprint_status,
            LexicalCleanupApprovalFingerprintStatus::Matched
        );
        assert_eq!(
            active_fingerprint_still_blocks.blocker_codes,
            vec![LexicalCleanupApplyBlocker::ActiveGenerationWork]
        );
        assert!(active_fingerprint_still_blocks.approval_fingerprint_matches);
        assert_eq!(active_fingerprint_still_blocks.blocked_reasons.len(), 1);

        let safe_plan = LexicalCleanupDryRunPlan::from_manifests([&superseded]);
        let allowed =
            safe_plan.apply_gate_with_fingerprint(true, Some(&safe_plan.approval_fingerprint));
        assert!(allowed.apply_allowed);
        assert!(allowed.blocker_codes.is_empty());
        assert!(allowed.active_generation_ids.is_empty());
        assert!(allowed.protected_generation_ids.is_empty());
        assert_eq!(
            allowed.reclaimable_generation_ids,
            vec!["gen-old".to_string()]
        );
        assert!(allowed.fully_retained_generation_ids.is_empty());
        assert!(allowed.quarantined_generation_ids.is_empty());
        assert_eq!(allowed.protected_retained_bytes, 0);
        assert_eq!(allowed.inspection_required_count, 0);
        assert_eq!(allowed.inspection_required_retained_bytes, 0);
        assert!(allowed.blocked_reasons.is_empty());
        assert_eq!(
            allowed.approval_fingerprint_status,
            LexicalCleanupApprovalFingerprintStatus::Matched
        );
        assert!(allowed.approval_fingerprint_matches);
        assert_eq!(
            allowed.provided_approval_fingerprint.as_deref(),
            Some(safe_plan.approval_fingerprint.as_str())
        );
        assert_eq!(allowed.generation_count, 1);
        assert_eq!(allowed.total_artifact_bytes, 4096);
        assert_eq!(allowed.total_retained_bytes, 0);
        assert_eq!(
            allowed
                .disposition_counts
                .get(&LexicalCleanupDisposition::SupersededReclaimable),
            Some(&1)
        );
        assert_eq!(allowed.candidate_count, 1);
        assert_eq!(allowed.reclaimable_bytes, 4096);
        let allowed_json =
            serde_json::to_value(&allowed).expect("serialize cleanup apply gate preview");
        assert_eq!(
            allowed_json["provided_approval_fingerprint"],
            safe_plan.approval_fingerprint
        );
        assert_eq!(allowed_json["approval_fingerprint_matches"], true);
        assert_eq!(allowed_json["approval_fingerprint_status"], "matched");
        assert_eq!(allowed_json["blocker_codes"], serde_json::json!([]));
        assert_eq!(allowed_json["active_generation_ids"], serde_json::json!([]));
        assert_eq!(
            allowed_json["reclaimable_generation_ids"],
            serde_json::json!(["gen-old"])
        );
        assert_eq!(
            allowed_json["fully_retained_generation_ids"],
            serde_json::json!([])
        );
        assert_eq!(
            allowed_json["quarantined_generation_ids"],
            serde_json::json!([])
        );
        assert_eq!(allowed_json["generation_count"], 1);
        assert_eq!(allowed_json["total_artifact_bytes"], 4096);
        assert_eq!(allowed_json["total_retained_bytes"], 0);
        assert_eq!(
            allowed_json["disposition_counts"]["superseded_reclaimable"],
            1
        );
        assert_eq!(
            allowed_json["generation_disposition_summaries"]["superseded_reclaimable"]["reclaimable_bytes"],
            4096
        );
        assert_eq!(
            allowed_json["shard_disposition_summaries"]["superseded_reclaimable"]["shard_count"],
            1
        );
        assert_eq!(
            allowed_json["protected_generation_ids"],
            serde_json::json!([])
        );
        assert_eq!(allowed_json["protected_retained_bytes"], 0);
        assert_eq!(allowed_json["inspection_required_count"], 0);
        assert_eq!(allowed_json["inspection_required_retained_bytes"], 0);
        assert_eq!(allowed_json["inspection_previews"], serde_json::json!([]));
        assert_eq!(
            allowed_json["candidate_previews"][0]["generation_id"],
            "gen-old"
        );
        assert_eq!(
            allowed_json["candidate_previews"][0]["shard_id"],
            "shard-old"
        );
        assert_eq!(
            allowed_json["candidate_previews"][0]["reclaimable_bytes"],
            4096
        );

        let stale_fingerprint =
            safe_plan.apply_gate_with_fingerprint(true, Some("cleanup-v1-stale"));
        assert!(!stale_fingerprint.apply_allowed);
        assert_eq!(
            stale_fingerprint.approval_fingerprint_status,
            LexicalCleanupApprovalFingerprintStatus::Mismatched
        );
        assert_eq!(
            stale_fingerprint.blocker_codes,
            vec![LexicalCleanupApplyBlocker::ApprovalFingerprintMismatched]
        );
        assert!(!stale_fingerprint.approval_fingerprint_matches);
        assert!(
            stale_fingerprint
                .blocked_reasons
                .iter()
                .any(|reason| reason.contains("does not match")),
            "missing stale fingerprint blocker: {:?}",
            stale_fingerprint.blocked_reasons
        );

        let empty_plan = LexicalCleanupDryRunPlan::from_manifests([&quarantined]);
        let no_candidates =
            empty_plan.apply_gate_with_fingerprint(true, Some(&empty_plan.approval_fingerprint));
        assert!(!no_candidates.apply_allowed);
        assert_eq!(
            no_candidates.blocker_codes,
            vec![LexicalCleanupApplyBlocker::NoReclaimableCandidates]
        );
        assert_eq!(no_candidates.generation_count, 1);
        assert_eq!(no_candidates.total_artifact_bytes, 512);
        assert_eq!(no_candidates.total_retained_bytes, 512);
        assert!(no_candidates.reclaimable_generation_ids.is_empty());
        assert_eq!(
            no_candidates.fully_retained_generation_ids,
            vec!["gen-quarantined".to_string()]
        );
        assert_eq!(
            no_candidates.quarantined_generation_ids,
            vec!["gen-quarantined".to_string()]
        );
        assert_eq!(
            no_candidates.protected_generation_ids,
            vec!["gen-quarantined".to_string()]
        );
        assert_eq!(no_candidates.protected_retained_bytes, 512);
        assert_eq!(no_candidates.inspection_required_count, 1);
        assert_eq!(no_candidates.inspection_required_retained_bytes, 512);
        let no_candidates_json =
            serde_json::to_value(&no_candidates).expect("serialize no-candidate apply gate");
        assert_eq!(
            no_candidates_json["inspection_previews"][0]["generation_id"],
            "gen-quarantined"
        );
        assert_eq!(
            no_candidates_json["inspection_previews"][0]["retained_bytes"],
            512
        );
    }

    #[test]
    fn cleanup_apply_gate_deserializes_legacy_payload_without_lifecycle_summaries() {
        let legacy = serde_json::json!({
            "apply_allowed": false,
            "dry_run": true,
            "explicit_operator_approval": false,
            "candidate_count": 0,
            "reclaimable_bytes": 0,
            "blocked_reasons": []
        });

        let gate: LexicalCleanupApplyGate =
            serde_json::from_value(legacy).expect("legacy cleanup apply gate should deserialize");
        assert_eq!(
            gate.approval_fingerprint_status,
            LexicalCleanupApprovalFingerprintStatus::NotRequested
        );
        assert!(!gate.approval_fingerprint_matches);
        assert!(gate.active_generation_ids.is_empty());
        assert!(gate.protected_generation_ids.is_empty());
        assert_eq!(gate.protected_retained_bytes, 0);
        assert!(gate.inspection_previews.is_empty());
        assert_eq!(gate.inspection_required_count, 0);
        assert_eq!(gate.inspection_required_retained_bytes, 0);
        assert!(gate.inspection_required_generation_ids.is_empty());
    }

    #[test]
    fn cleanup_dry_run_plan_fingerprints_approval_surface() -> Result<(), serde_json::Error> {
        let mut superseded =
            LexicalGenerationManifest::new_scratch("gen-old", "attempt-1", "fp", 1);
        let mut reclaimable_shard =
            test_shard("shard-old", 0, LexicalShardLifecycleState::Published, 4096);
        reclaimable_shard.pinned = false;
        reclaimable_shard.reclaimable = true;
        superseded.set_shards(vec![reclaimable_shard], 2);
        superseded.transition_build(LexicalGenerationBuildState::Validated, 3);
        superseded.transition_publish(LexicalGenerationPublishState::Superseded, 4);

        let mut larger = LexicalGenerationManifest::new_scratch("gen-old", "attempt-2", "fp", 10);
        let mut larger_shard =
            test_shard("shard-old", 0, LexicalShardLifecycleState::Published, 8192);
        larger_shard.pinned = false;
        larger_shard.reclaimable = true;
        larger.set_shards(vec![larger_shard], 11);
        larger.transition_build(LexicalGenerationBuildState::Validated, 12);
        larger.transition_publish(LexicalGenerationPublishState::Superseded, 13);

        let mut quarantined =
            LexicalGenerationManifest::new_scratch("gen-quarantined", "attempt-3", "fp", 20);
        quarantined.set_shards(
            vec![test_shard(
                "shard-bad",
                0,
                LexicalShardLifecycleState::Quarantined,
                512,
            )],
            21,
        );
        assert!(quarantined.transition_shard(
            "shard-bad",
            LexicalShardLifecycleState::Quarantined,
            22,
            Some("checksum mismatch".into()),
        ));
        quarantined.transition_publish(LexicalGenerationPublishState::Quarantined, 23);

        let plan = LexicalCleanupDryRunPlan::from_manifests([&superseded, &quarantined]);
        let changed_plan = LexicalCleanupDryRunPlan::from_manifests([&larger, &quarantined]);

        assert!(plan.approval_fingerprint.starts_with("cleanup-v1-"));
        assert_eq!(plan.approval_fingerprint.len(), "cleanup-v1-".len() + 64);
        assert_ne!(
            plan.approval_fingerprint, changed_plan.approval_fingerprint,
            "approval fingerprint must change when reclaimable candidate bytes change"
        );

        let mut current =
            LexicalGenerationManifest::new_scratch("gen-current", "attempt-current", "fp", 30);
        current.set_shards(
            vec![test_shard(
                "shard-current",
                0,
                LexicalShardLifecycleState::Published,
                100,
            )],
            31,
        );
        current.transition_build(LexicalGenerationBuildState::Validated, 32);
        current.transition_publish(LexicalGenerationPublishState::Published, 33);

        let mut pinned =
            LexicalGenerationManifest::new_scratch("gen-pinned", "attempt-pinned", "fp", 40);
        pinned.set_shards(
            vec![test_shard(
                "shard-pinned",
                0,
                LexicalShardLifecycleState::Published,
                200,
            )],
            41,
        );
        pinned.transition_build(LexicalGenerationBuildState::Validated, 42);

        let mut larger_current =
            LexicalGenerationManifest::new_scratch("gen-current", "attempt-current", "fp", 50);
        larger_current.set_shards(
            vec![test_shard(
                "shard-current",
                0,
                LexicalShardLifecycleState::Published,
                200,
            )],
            51,
        );
        larger_current.transition_build(LexicalGenerationBuildState::Validated, 52);
        larger_current.transition_publish(LexicalGenerationPublishState::Published, 53);

        let mut smaller_pinned =
            LexicalGenerationManifest::new_scratch("gen-pinned", "attempt-pinned", "fp", 60);
        smaller_pinned.set_shards(
            vec![test_shard(
                "shard-pinned",
                0,
                LexicalShardLifecycleState::Published,
                100,
            )],
            61,
        );
        smaller_pinned.transition_build(LexicalGenerationBuildState::Validated, 62);

        let retained_plan = LexicalCleanupDryRunPlan::from_manifests([&current, &pinned]);
        let shifted_retained_plan =
            LexicalCleanupDryRunPlan::from_manifests([&larger_current, &smaller_pinned]);
        assert_eq!(
            retained_plan.total_retained_bytes,
            shifted_retained_plan.total_retained_bytes
        );
        assert_eq!(
            retained_plan.disposition_counts,
            shifted_retained_plan.disposition_counts
        );
        assert_ne!(
            retained_plan.approval_fingerprint, shifted_retained_plan.approval_fingerprint,
            "approval fingerprint must change when retained bytes move between protected disposition summaries"
        );

        let mut duplicate_key_a =
            LexicalGenerationManifest::new_scratch("gen-dup", "attempt-dup", "fp", 70);
        let mut dup_small = test_shard("dup-shard", 0, LexicalShardLifecycleState::Published, 100);
        dup_small.pinned = false;
        dup_small.reclaimable = true;
        let mut dup_large = test_shard("dup-shard", 1, LexicalShardLifecycleState::Published, 200);
        dup_large.pinned = false;
        dup_large.reclaimable = true;
        duplicate_key_a.set_shards(vec![dup_small.clone(), dup_large.clone()], 71);
        duplicate_key_a.transition_build(LexicalGenerationBuildState::Validated, 72);
        duplicate_key_a.transition_publish(LexicalGenerationPublishState::Superseded, 73);

        let mut duplicate_key_b =
            LexicalGenerationManifest::new_scratch("gen-dup", "attempt-dup", "fp", 80);
        duplicate_key_b.set_shards(vec![dup_large, dup_small], 81);
        duplicate_key_b.transition_build(LexicalGenerationBuildState::Validated, 82);
        duplicate_key_b.transition_publish(LexicalGenerationPublishState::Superseded, 83);

        let duplicate_order_plan_a = LexicalCleanupDryRunPlan::from_manifests([&duplicate_key_a]);
        let duplicate_order_plan_b = LexicalCleanupDryRunPlan::from_manifests([&duplicate_key_b]);
        assert_eq!(
            duplicate_order_plan_a.approval_fingerprint,
            duplicate_order_plan_b.approval_fingerprint,
            "approval fingerprint must sort equal generation/shard/disposition keys by the rest of the hashed candidate payload"
        );

        let gate = plan.apply_gate_with_fingerprint(true, Some(&plan.approval_fingerprint));
        assert_eq!(gate.approval_fingerprint, plan.approval_fingerprint);
        assert_eq!(
            gate.provided_approval_fingerprint.as_deref(),
            Some(plan.approval_fingerprint.as_str())
        );
        assert!(gate.approval_fingerprint_matches);
        let plan_json = serde_json::to_value(&plan)?;
        let gate_json = serde_json::to_value(&gate)?;
        assert_eq!(plan_json["approval_fingerprint"], plan.approval_fingerprint);
        assert_eq!(gate_json["approval_fingerprint"], plan.approval_fingerprint);
        assert_eq!(
            gate_json["provided_approval_fingerprint"],
            plan.approval_fingerprint
        );
        Ok(())
    }

    #[test]
    fn cleanup_dry_run_plan_lists_inspection_items_for_retained_risky_artifacts() {
        let mut quarantined =
            LexicalGenerationManifest::new_scratch("gen-quarantined", "attempt-1", "fp", 1);
        quarantined.set_shards(
            vec![test_shard(
                "shard-bad",
                0,
                LexicalShardLifecycleState::Quarantined,
                512,
            )],
            2,
        );
        assert!(quarantined.transition_shard(
            "shard-bad",
            LexicalShardLifecycleState::Quarantined,
            3,
            Some("checksum mismatch".into()),
        ));
        quarantined.transition_publish(LexicalGenerationPublishState::Quarantined, 4);

        let mut failed =
            LexicalGenerationManifest::new_scratch("gen-failed-retained", "attempt-2", "fp", 10);
        let mut failed_shard = test_shard(
            "shard-failed",
            0,
            LexicalShardLifecycleState::Abandoned,
            256,
        );
        failed_shard.reclaimable = false;
        failed.set_shards(vec![failed_shard], 11);
        assert!(failed.transition_shard(
            "shard-failed",
            LexicalShardLifecycleState::Abandoned,
            12,
            Some("operator retained failed shard for postmortem".into()),
        ));
        failed.shards[0].reclaimable = false;
        failed.transition_build(LexicalGenerationBuildState::Failed, 13);

        let plan = LexicalCleanupDryRunPlan::from_manifests([&quarantined, &failed]);

        assert_eq!(
            plan.inspection_required_generation_ids(),
            vec![
                "gen-quarantined".to_string(),
                "gen-failed-retained".to_string()
            ]
        );
        assert_eq!(
            plan.inspection_items,
            vec![
                LexicalCleanupInspectionItem {
                    generation_id: "gen-quarantined".to_string(),
                    shard_id: Some("shard-bad".to_string()),
                    disposition: LexicalCleanupDisposition::QuarantinedRetained,
                    reason: "checksum mismatch".to_string(),
                    retained_bytes: 512,
                },
                LexicalCleanupInspectionItem {
                    generation_id: "gen-failed-retained".to_string(),
                    shard_id: Some("shard-failed".to_string()),
                    disposition: LexicalCleanupDisposition::FailedRetained,
                    reason: "operator retained failed shard for postmortem".to_string(),
                    retained_bytes: 256,
                },
            ]
        );
        assert_eq!(plan.inspection_required_count, 2);
        assert_eq!(plan.inspection_required_retained_bytes, 768);
        assert_eq!(plan.inspection_required_retained_bytes(), 768);

        let json = serde_json::to_value(&plan).expect("serialize cleanup inspection dry-run plan");
        assert_eq!(json["inspection_required_count"], 2);
        assert_eq!(json["inspection_required_retained_bytes"], 768);
        assert_eq!(
            json["inspection_items"][0]["disposition"],
            "quarantined_retained"
        );
        assert_eq!(
            json["inspection_items"][1]["generation_id"],
            "gen-failed-retained"
        );
        assert_eq!(json["inspection_items"][1]["retained_bytes"], 256);
    }

    fn test_shard_plan(
        source_db_fingerprint: &str,
        shard_count: u32,
    ) -> LexicalGenerationShardPlan {
        LexicalGenerationShardPlan {
            plan_id: format!("plan-{source_db_fingerprint}-{shard_count}"),
            planner_version: 1,
            shard_count,
            packet_contract_version: 1,
            source_db_fingerprint: source_db_fingerprint.into(),
        }
    }

    fn test_budget() -> LexicalGenerationBuildBudget {
        LexicalGenerationBuildBudget {
            policy_id: "test-policy".into(),
            effective_settings_fingerprint: "settings-fp-test".into(),
            max_inflight_message_bytes: 4 * 1024 * 1024,
            producer_queue_pages: 2,
            batch_conversation_limit: 16,
            worker_threads: 2,
            controller_reason: Some("test budget".into()),
            extra_limits: BTreeMap::from([("staged_merge_jobs".into(), 1)]),
        }
    }

    fn test_shard(
        shard_id: &str,
        shard_ordinal: u32,
        state: LexicalShardLifecycleState,
        artifact_bytes: u64,
    ) -> LexicalShardManifest {
        let mut shard = LexicalShardManifest::planned(shard_id, shard_ordinal, 1);
        shard.transition(state, 2);
        shard.artifact_bytes = artifact_bytes;
        shard.reclaimable = matches!(
            state,
            LexicalShardLifecycleState::Planned | LexicalShardLifecycleState::Abandoned
        );
        shard.pinned = matches!(state, LexicalShardLifecycleState::Published);
        shard
    }

    /// `coding_agent_session_search-ibuuh.19` golden gate: every
    /// LexicalCleanupDisposition variant's `as_str()` must equal its
    /// serde-serialized name AND must be unique. Pre-fix this gate
    /// did not exist; the duplicate-naming class that bit ErrorKind
    /// (al19b — 3 real duplicates discovered in production)
    /// could land here unnoticed because the disposition feeds the
    /// machine-readable cleanup inventory operators read from
    /// `cass diag --quarantine`. A regression that:
    /// - added a new variant without an `as_str()` arm,
    /// - drifted serde rename_all away from snake_case,
    /// - introduced a duplicate string,
    ///   would trip this immediately.
    #[test]
    fn cleanup_disposition_as_str_matches_serde_serialization_byte_for_byte() {
        use std::collections::HashSet;

        let mut seen_strs: HashSet<&'static str> = HashSet::new();
        for &variant in LexicalCleanupDisposition::all_variants() {
            let as_str = variant.as_str();
            // Uniqueness gate: catches the al19b-class duplicate bug.
            assert!(
                seen_strs.insert(as_str),
                "duplicate disposition string detected: {variant:?} maps to {as_str:?} \
                 which was already registered by an earlier variant"
            );
            // serde alignment: rename_all = snake_case must produce
            // the exact same string as_str() returns. A regression in
            // either direction (variant rename vs as_str() drift, or
            // serde attribute change) trips here.
            let serde_str = serde_json::to_string(&variant).expect("serialize disposition");
            // serde wraps strings in quotes — strip them.
            let serde_str = serde_str.trim_matches('"');
            assert_eq!(
                serde_str, as_str,
                "serde serialization {serde_str:?} must equal as_str() {as_str:?} for {variant:?}"
            );
        }
        // All eight variants must be covered. A new variant added
        // without registering it in all_variants() would shrink the
        // count and fail this assertion.
        assert_eq!(
            LexicalCleanupDisposition::all_variants().len(),
            8,
            "disposition enum has 8 variants at landing time; bump this count + add the new \
             variant + extend is_protected_retention_disposition for any addition"
        );
    }

    /// `coding_agent_session_search-ibuuh.19` classification gate:
    /// `is_protected_retention_disposition()` must classify every
    /// LexicalCleanupDisposition variant — exactly six are protected
    /// (kept on disk) and exactly two are reclaimable. Pre-fix, a
    /// new variant added without thinking about retention safety
    /// would default to "not protected" silently and risk reclaiming
    /// state the operator wanted preserved (or vice versa). Pinning
    /// the partition explicitly closes that hole.
    #[test]
    fn cleanup_disposition_protected_retention_classification_is_exhaustive() {
        let protected: Vec<LexicalCleanupDisposition> = LexicalCleanupDisposition::all_variants()
            .iter()
            .copied()
            .filter(|d| is_protected_retention_disposition(*d))
            .collect();
        let reclaimable: Vec<LexicalCleanupDisposition> = LexicalCleanupDisposition::all_variants()
            .iter()
            .copied()
            .filter(|d| !is_protected_retention_disposition(*d))
            .collect();

        // Six protected + two reclaimable = eight variants total.
        // A new variant that defaults to non-protected without an
        // explicit policy decision will shift these counts and trip
        // the test, forcing a maintainer to think about retention.
        assert_eq!(
            protected.len(),
            6,
            "expected exactly 6 protected variants; got {protected:?}"
        );
        assert_eq!(
            reclaimable.len(),
            2,
            "expected exactly 2 reclaimable variants; got {reclaimable:?}"
        );

        // Pin the *exact* protected set so a regression that
        // misclassifies (e.g. moves CurrentPublished out of the
        // protected set, which would let cleanup nuke the live
        // search asset) trips the assertion with the variant name.
        for required_protected in [
            LexicalCleanupDisposition::CurrentPublished,
            LexicalCleanupDisposition::ActiveWork,
            LexicalCleanupDisposition::QuarantinedRetained,
            LexicalCleanupDisposition::SupersededRetained,
            LexicalCleanupDisposition::FailedRetained,
            LexicalCleanupDisposition::PinnedRetained,
        ] {
            assert!(
                protected.contains(&required_protected),
                "{required_protected:?} MUST be classified as protected — moving it out \
                 of the protected set risks reclaiming live or operator-flagged state"
            );
        }
        // Pin the reclaimable set too: a regression that
        // accidentally protected SupersededReclaimable would prevent
        // disk-budget pruning from ever reclaiming superseded
        // generations, leading to unbounded disk bloat (the bead
        // BACKGROUND warns about exactly this).
        for required_reclaimable in [
            LexicalCleanupDisposition::SupersededReclaimable,
            LexicalCleanupDisposition::FailedReclaimable,
        ] {
            assert!(
                reclaimable.contains(&required_reclaimable),
                "{required_reclaimable:?} MUST be reclaimable — protecting it would block \
                 disk-budget pruning of superseded/failed generations"
            );
        }
    }

    /// `coding_agent_session_search-ibuuh.19` round-trip gate: every
    /// disposition string MUST round-trip through
    /// `serde_json::to_string` → `serde_json::from_str` and yield the
    /// same variant. Pre-fix, a regression that broke deserialization
    /// (e.g. by dropping `Deserialize` derive or changing the rename
    /// strategy in one direction) would silently break operators
    /// reading `.lexical-rebuild-cleanup.json` artifacts back from
    /// disk for trend analysis.
    #[test]
    fn cleanup_disposition_serde_round_trips_for_every_variant() {
        for &variant in LexicalCleanupDisposition::all_variants() {
            let json = serde_json::to_string(&variant).expect("serialize");
            let parsed: LexicalCleanupDisposition =
                serde_json::from_str(&json).expect("deserialize");
            assert_eq!(
                parsed, variant,
                "disposition round-trip mismatch for {variant:?}: serialized as {json}, \
                 parsed as {parsed:?}"
            );
        }
    }

    /// `coding_agent_session_search-ibuuh.19` structured-logs gate:
    /// every cleanup classification MUST emit a tracing event whose
    /// fields let an operator answer "what got pruned?" and "why was
    /// X kept?" from logs alone — directly addressing the bead's
    /// "structured logs that let a future agent understand exactly
    /// why disk was reclaimed or preserved" SCOPE bullet. Pre-fix,
    /// `record_inventory` had ZERO tracing emissions (verified via
    /// grep -nE "tracing::" returning empty in this file). Post-fix,
    /// every classification routes to debug/info/warn per severity
    /// tier (reclaimable=debug, retained-by-policy=info,
    /// quarantined/failed-retained=warn).
    ///
    /// This test pins:
    /// 1. The QuarantinedRetained classification emits a `warn`
    ///    event (operator must see it in default logs).
    /// 2. The event carries `disposition`, `generation_id`,
    ///    `reason`, and `retained_bytes` fields so a single log
    ///    line is fully diagnostic.
    /// 3. The event target is `cass::indexer::lexical_cleanup`
    ///    (operators can grep / filter by target).
    #[test]
    fn record_inventory_emits_structured_classification_event_for_quarantined_generation() {
        use std::sync::{Arc, Mutex};
        use tracing::field::{Field, Visit};
        use tracing::{Event, Level, Subscriber};
        use tracing_subscriber::Registry;
        use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

        #[derive(Debug, Clone, Default)]
        struct CapturedEvent {
            level: String,
            target: String,
            fields: std::collections::HashMap<String, String>,
        }

        #[derive(Clone, Default)]
        struct ClassificationCollector {
            events: Arc<Mutex<Vec<CapturedEvent>>>,
        }

        impl<S: Subscriber> Layer<S> for ClassificationCollector {
            fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
                if event.metadata().target() != "cass::indexer::lexical_cleanup" {
                    return;
                }
                let mut visitor = StringVisitor::default();
                event.record(&mut visitor);
                self.events
                    .lock()
                    .expect("collector lock")
                    .push(CapturedEvent {
                        level: event.metadata().level().to_string(),
                        target: event.metadata().target().to_string(),
                        fields: visitor.fields,
                    });
            }
        }

        #[derive(Default)]
        struct StringVisitor {
            fields: std::collections::HashMap<String, String>,
        }

        impl Visit for StringVisitor {
            fn record_str(&mut self, field: &Field, value: &str) {
                self.fields
                    .insert(field.name().to_string(), value.to_string());
            }
            fn record_u64(&mut self, field: &Field, value: u64) {
                self.fields
                    .insert(field.name().to_string(), value.to_string());
            }
            fn record_i64(&mut self, field: &Field, value: i64) {
                self.fields
                    .insert(field.name().to_string(), value.to_string());
            }
            fn record_bool(&mut self, field: &Field, value: bool) {
                self.fields
                    .insert(field.name().to_string(), value.to_string());
            }
            fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                self.fields
                    .insert(field.name().to_string(), format!("{:?}", value));
            }
        }

        let collector = ClassificationCollector::default();
        let subscriber = Registry::default().with(collector.clone());

        // Build a quarantined fixture identical to the existing
        // `cleanup_inventory_keeps_quarantined_artifacts_for_inspection`
        // test (line ~2364) so the two tests share a factual basis.
        let mut manifest =
            LexicalGenerationManifest::new_scratch("gen-traced-quarantined", "attempt-1", "fp", 1);
        let shard = test_shard(
            "shard-bad",
            0,
            LexicalShardLifecycleState::Quarantined,
            4096,
        );
        manifest.set_shards(vec![shard], 2);
        assert!(manifest.transition_shard(
            "shard-bad",
            LexicalShardLifecycleState::Quarantined,
            3,
            Some("traced classification event regression".into()),
        ));
        manifest.transition_publish(LexicalGenerationPublishState::Quarantined, 4);

        // Drive record_inventory under our collector subscriber via
        // the existing public `from_manifests` entrypoint, which calls
        // record_inventory internally — same code path operators
        // exercise via cass diag/doctor.
        tracing::subscriber::with_default(subscriber, || {
            let _plan = LexicalCleanupDryRunPlan::from_manifests([&manifest]);
        });

        let captured = collector.events.lock().expect("collector lock").clone();
        // Exactly one classification event for one generation.
        assert_eq!(
            captured.len(),
            1,
            "record_inventory must emit exactly one classification event per generation; \
             got {captured:?}"
        );
        let event = &captured[0];

        // Invariant 1: target.
        assert_eq!(
            event.target, "cass::indexer::lexical_cleanup",
            "classification event must use the lexical_cleanup target so operators can \
             grep/filter by it"
        );

        // Invariant 2: WARN severity for quarantined (must surface in
        // default-level logs without dredging).
        assert_eq!(
            event.level,
            Level::WARN.to_string(),
            "QuarantinedRetained MUST emit at warn level so the inspection-required \
             state shows up in default operator logs; got {event:?}"
        );

        // Invariant 3: every diagnostic field is present so a single
        // log line answers "what got pruned?" and "why was X kept?"
        for required in [
            "disposition",
            "generation_id",
            "reason",
            "reclaimable_bytes",
            "retained_bytes",
            "artifact_bytes",
        ] {
            assert!(
                event.fields.contains_key(required),
                "classification event MUST include {required} field; got fields {:?}",
                event.fields.keys().collect::<Vec<_>>()
            );
        }

        // Invariant 4: field VALUES match the classification.
        assert_eq!(
            event.fields.get("disposition"),
            Some(&"quarantined_retained".to_string())
        );
        assert_eq!(
            event.fields.get("generation_id"),
            Some(&"gen-traced-quarantined".to_string())
        );
        assert_eq!(
            event.fields.get("retained_bytes"),
            Some(&"4096".to_string())
        );
        assert_eq!(
            event.fields.get("reclaimable_bytes"),
            Some(&"0".to_string())
        );
        // The reason string must explain WHY the generation is being
        // retained (operator inspection hold).
        let reason = event.fields.get("reason").expect("reason field present");
        assert!(
            reason.contains("operator inspection") || reason.contains("quarantined"),
            "reason field must explain the inspection hold; got {reason:?}"
        );
    }

    /// Bead coding_agent_session_search-yv5fn: coverage gap in 7d3297c7.
    /// The sibling test above exercises only `QuarantinedRetained`.
    /// This table-driven companion asserts correct severity routing
    /// for EVERY `LexicalCleanupDisposition` variant — 8 total across
    /// 3 severity tiers (2 DEBUG / 4 INFO / 2 WARN). A future refactor
    /// that re-routes any variant to the wrong tier, or drops emission
    /// entirely for one specific variant, would trip this test where
    /// the Quarantined-only test would have stayed green.
    ///
    /// Fixture shapes follow `classify_generation_for_cleanup` at
    /// src/indexer/lexical_generation.rs:~1522:
    ///   - `CurrentPublished`: build=Validated, publish=Published, a
    ///     single Validated shard so `is_serveable` returns true.
    ///   - `ActiveWork`: default `new_scratch` state (build=Scratch).
    ///   - `QuarantinedRetained`: publish=Quarantined.
    ///   - `SupersededReclaimable`: publish=Superseded with a Planned
    ///     shard (reclaimable=true, artifact_bytes>0).
    ///   - `SupersededRetained`: publish=Superseded with a Published
    ///     shard (reclaimable=false via test_shard) — no reclaim
    ///     bytes, so the classifier picks the retained variant.
    ///   - `FailedReclaimable`: build=Failed with an Abandoned shard
    ///     (reclaimable=true) carrying artifact_bytes>0.
    ///   - `FailedRetained`: build=Failed with a Published shard
    ///     (pinned=true, reclaimable=false).
    ///   - `PinnedRetained`: build=Validated, publish=Staged, zero
    ///     shards — classifier falls through to the pinned arm.
    #[test]
    fn record_inventory_emits_correct_severity_for_every_disposition_variant() {
        use std::sync::{Arc, Mutex};
        use tracing::field::{Field, Visit};
        use tracing::{Event, Level, Subscriber};
        use tracing_subscriber::Registry;
        use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

        #[derive(Debug, Clone, Default)]
        struct CapturedEvent {
            level: String,
            target: String,
            fields: std::collections::HashMap<String, String>,
        }

        #[derive(Clone, Default)]
        struct ClassificationCollector {
            events: Arc<Mutex<Vec<CapturedEvent>>>,
        }

        impl<S: Subscriber> Layer<S> for ClassificationCollector {
            fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
                if event.metadata().target() != "cass::indexer::lexical_cleanup" {
                    return;
                }
                let mut visitor = StringVisitor::default();
                event.record(&mut visitor);
                self.events
                    .lock()
                    .expect("collector lock")
                    .push(CapturedEvent {
                        level: event.metadata().level().to_string(),
                        target: event.metadata().target().to_string(),
                        fields: visitor.fields,
                    });
            }
        }

        #[derive(Default)]
        struct StringVisitor {
            fields: std::collections::HashMap<String, String>,
        }

        impl Visit for StringVisitor {
            fn record_str(&mut self, field: &Field, value: &str) {
                self.fields
                    .insert(field.name().to_string(), value.to_string());
            }
            fn record_u64(&mut self, field: &Field, value: u64) {
                self.fields
                    .insert(field.name().to_string(), value.to_string());
            }
            fn record_i64(&mut self, field: &Field, value: i64) {
                self.fields
                    .insert(field.name().to_string(), value.to_string());
            }
            fn record_bool(&mut self, field: &Field, value: bool) {
                self.fields
                    .insert(field.name().to_string(), value.to_string());
            }
            fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                self.fields
                    .insert(field.name().to_string(), format!("{:?}", value));
            }
        }

        // Build a manifest that classifies to the requested disposition.
        fn fixture_for(
            disposition: LexicalCleanupDisposition,
            generation_id: &str,
        ) -> LexicalGenerationManifest {
            let mut m = LexicalGenerationManifest::new_scratch(generation_id, "attempt-1", "fp", 1);
            match disposition {
                LexicalCleanupDisposition::CurrentPublished => {
                    // `all_shards_publish_ready` accepts Validated /
                    // Published shards; with shard_plan=None the
                    // plan-count check is skipped, so a single shard
                    // is enough to make `is_serveable` true once
                    // build=Validated + publish=Published land.
                    let shard = test_shard(
                        "shard-current",
                        0,
                        LexicalShardLifecycleState::Validated,
                        128,
                    );
                    m.set_shards(vec![shard], 2);
                    m.transition_build(LexicalGenerationBuildState::Validated, 3);
                    m.transition_publish(LexicalGenerationPublishState::Published, 4);
                }
                LexicalCleanupDisposition::ActiveWork => {
                    // Default new_scratch has build=Scratch → active.
                }
                LexicalCleanupDisposition::QuarantinedRetained => {
                    let shard =
                        test_shard("shard-q", 0, LexicalShardLifecycleState::Quarantined, 256);
                    m.set_shards(vec![shard], 2);
                    m.transition_publish(LexicalGenerationPublishState::Quarantined, 3);
                }
                LexicalCleanupDisposition::SupersededReclaimable => {
                    // Planned shards are reclaimable=true per test_shard.
                    let shard =
                        test_shard("shard-s-r", 0, LexicalShardLifecycleState::Planned, 512);
                    m.set_shards(vec![shard], 2);
                    m.transition_build(LexicalGenerationBuildState::Validated, 3);
                    m.transition_publish(LexicalGenerationPublishState::Superseded, 4);
                }
                LexicalCleanupDisposition::SupersededRetained => {
                    // Published shards are pinned=true, reclaimable=false.
                    let shard = test_shard(
                        "shard-s-keep",
                        0,
                        LexicalShardLifecycleState::Published,
                        512,
                    );
                    m.set_shards(vec![shard], 2);
                    m.transition_build(LexicalGenerationBuildState::Validated, 3);
                    m.transition_publish(LexicalGenerationPublishState::Superseded, 4);
                }
                LexicalCleanupDisposition::FailedReclaimable => {
                    // Abandoned shards are reclaimable=true per test_shard.
                    let shard =
                        test_shard("shard-f-r", 0, LexicalShardLifecycleState::Abandoned, 1024);
                    m.set_shards(vec![shard], 2);
                    m.transition_build(LexicalGenerationBuildState::Failed, 3);
                }
                LexicalCleanupDisposition::FailedRetained => {
                    // Published shards are pinned=true, reclaimable=false.
                    let shard = test_shard(
                        "shard-f-keep",
                        0,
                        LexicalShardLifecycleState::Published,
                        1024,
                    );
                    m.set_shards(vec![shard], 2);
                    m.transition_build(LexicalGenerationBuildState::Failed, 3);
                }
                LexicalCleanupDisposition::PinnedRetained => {
                    // No shards, build=Validated, publish=Staged →
                    // falls through every branch in the classifier.
                    m.transition_build(LexicalGenerationBuildState::Validated, 2);
                }
            }
            m
        }

        // Table of (variant → expected severity tier).
        let cases: &[(LexicalCleanupDisposition, Level)] = &[
            (
                LexicalCleanupDisposition::SupersededReclaimable,
                Level::DEBUG,
            ),
            (LexicalCleanupDisposition::FailedReclaimable, Level::DEBUG),
            (LexicalCleanupDisposition::ActiveWork, Level::INFO),
            (LexicalCleanupDisposition::CurrentPublished, Level::INFO),
            (LexicalCleanupDisposition::SupersededRetained, Level::INFO),
            (LexicalCleanupDisposition::PinnedRetained, Level::INFO),
            (LexicalCleanupDisposition::QuarantinedRetained, Level::WARN),
            (LexicalCleanupDisposition::FailedRetained, Level::WARN),
        ];

        // Sanity: exactly 8 variants in the table — if a new variant is
        // ever added, `LexicalCleanupDisposition::all_variants` would
        // count higher than this table, so this assertion fires the
        // update before the mismatched-severity scenarios can slip.
        assert_eq!(
            LexicalCleanupDisposition::all_variants().len(),
            cases.len(),
            "table must cover every LexicalCleanupDisposition variant; \
             the classifier adds variants and this list must follow"
        );

        for (variant, expected_level) in cases {
            let collector = ClassificationCollector::default();
            let subscriber = Registry::default().with(collector.clone());
            let manifest = fixture_for(*variant, &format!("gen-{}", variant.as_str()));

            // Sanity: the fixture actually classifies to the variant
            // we intended. A mismatch here points to a fixture bug,
            // not to the tracing emission — keeps the severity
            // assertion below meaningful.
            let inventory_disposition = manifest.cleanup_inventory().disposition;
            assert_eq!(
                inventory_disposition, *variant,
                "fixture for variant {variant:?} must actually classify to {variant:?}, \
                 got {inventory_disposition:?}"
            );

            tracing::subscriber::with_default(subscriber, || {
                let _plan = LexicalCleanupDryRunPlan::from_manifests([&manifest]);
            });

            let captured = collector.events.lock().expect("collector lock").clone();
            assert_eq!(
                captured.len(),
                1,
                "variant {variant:?}: record_inventory must emit exactly one classification event; \
                 got {captured:?}"
            );
            let event = &captured[0];
            assert_eq!(
                event.level,
                expected_level.to_string(),
                "variant {variant:?} must emit at {expected_level:?} tier; got {event:?}"
            );
            assert_eq!(
                event.target, "cass::indexer::lexical_cleanup",
                "variant {variant:?}: target must stay grep-stable"
            );
            assert_eq!(
                event.fields.get("disposition").map(String::as_str),
                Some(variant.as_str()),
                "variant {variant:?}: disposition field must match the enum as_str"
            );
            for required in [
                "generation_id",
                "reason",
                "reclaimable_bytes",
                "retained_bytes",
                "artifact_bytes",
            ] {
                assert!(
                    event.fields.contains_key(required),
                    "variant {variant:?}: field {required} must be present; got {:?}",
                    event.fields.keys().collect::<Vec<_>>()
                );
            }
        }
    }
}
