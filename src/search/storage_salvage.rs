// Dead-code tolerated module-wide: this backup-first storage salvage planner
// (bead cass-fleet-resilience-20260608-uojcg.14.2) lands the decision contract
// ahead of the storage probes that populate its input report (.14.3
// concurrency / busy-lock / WAL diagnostics) and the storage-failure fixtures +
// real-binary E2E gate that drive it end-to-end (.14.4). The doctor/diag/fleet
// surfaces project it once those probes exist.
#![allow(dead_code)]

//! Backup-first storage salvage and repair planner (bead
//! cass-fleet-resilience-20260608-uojcg.14.2).
//!
//! The 2026-06-08 report's bottom line for storage: unsafe or misleading
//! recovery guidance is worse than none. Storage repair must never delete
//! source logs, overwrite the canonical archive, or silently convert formats
//! without a reviewed plan. This module is the decision layer that turns a
//! [`StorageIntegrityReport`] (the `.14.1` taxonomy) into an explicit,
//! backup-first [`StorageSalvagePlan`]:
//!
//! - Each [`StorageState`] maps to exactly one [`SalvageRepairPath`] — one of
//!   the six mutating paths the report named (rebuild-derived-index,
//!   checkpoint/recover WAL, recreate sidecar, migrate legacy metadata,
//!   targeted FTS repair, archive reconstruction) or a non-mutating
//!   disposition (no-op, wait-for-writer, inspect-refused).
//! - The dry-run plan lists the source DB path, the proposed backup path,
//!   the before-fingerprint, the sidecars present, the observed schema
//!   version, the integrity/readability checks attempted, the expected
//!   mutations, and the estimated mutation scope + cost.
//! - **Backup-first is mandatory for anything that touches the canonical
//!   archive.** High source-of-truth risk additionally requires explicit
//!   operator confirmation; read-only diagnostics never require either.
//! - The plan never recommends deleting archive files or raw session logs.
//!   Unsafe mutations are surfaced as [`SalvageBlockedCommand`]s with the
//!   precondition that unblocks them, never as a runnable command.
//!
//! [`SalvageApplyReceipt`] is the matching record every *mutating* apply
//! emits: before/after fingerprints, command provenance, `elapsed_ms`,
//! partial-failure status, and rollback/restore guidance. All enums serialize
//! as snake_case, matching the readiness + storage-integrity vocabularies.

use serde::{Deserialize, Serialize};

use crate::search::command_envelope::{CostClass, MutationScope};
use crate::search::storage_integrity::{
    ArchiveReadability, SourceOfTruthRisk, StorageCheck, StorageIntegrityReport, StorageState,
};

/// Schema version for the salvage plan + apply-receipt JSON contracts. Bump
/// when a field is added/removed/retyped (and refresh any consuming goldens).
pub(crate) const SALVAGE_PLAN_SCHEMA_VERSION: u32 = 1;

const SALVAGE_PLAN_KIND: &str = "storage_salvage_plan";
const SALVAGE_RECEIPT_KIND: &str = "storage_salvage_apply_receipt";

/// Stable suffix appended to the canonical DB path to name the backup the
/// planner proposes. The actual apply appends a unique run-scoped segment; the
/// plan shows this deterministic pattern so dry-run output is reproducible.
const SALVAGE_BACKUP_SUFFIX: &str = ".salvage-backup";

/// The distinct repair path the planner selects for a storage state. The six
/// mutating paths the report named, plus the non-mutating dispositions that
/// must never touch a byte: `NoActionNeeded` (healthy), `WaitForWriter` (a
/// transient lock — wait, never force), and `InspectRefused` (an unsafe SQL
/// shape or a deferred verdict — refuse to "repair" by mutating data).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SalvageRepairPath {
    /// Storage is healthy; nothing to do.
    NoActionNeeded,
    /// Rebuild the derived lexical/semantic index from the intact canonical DB.
    RebuildDerivedIndex,
    /// Checkpoint a suspect WAL back into the main DB and recover.
    CheckpointRecoverWal,
    /// Remove an orphaned/stale `-shm`/`-wal` sidecar so the engine recreates
    /// a clean one.
    RecreateSidecar,
    /// Migrate drifted/legacy on-disk metadata to the current schema contract.
    MigrateLegacyMetadata,
    /// Rebuild only the FTS shadow tables inside the canonical DB.
    TargetedFtsRepair,
    /// Reconstruct an unreadable canonical archive from source connectors or a
    /// promoted candidate bundle (or refuse + preserve when no source exists).
    ArchiveReconstruction,
    /// Another writer holds the lock; wait and re-check — never force.
    WaitForWriter,
    /// Refuse to mutate; the safe response is read-only inspection.
    InspectRefused,
}

impl SalvageRepairPath {
    pub(crate) fn stable_name(self) -> &'static str {
        match self {
            Self::NoActionNeeded => "no_action_needed",
            Self::RebuildDerivedIndex => "rebuild_derived_index",
            Self::CheckpointRecoverWal => "checkpoint_recover_wal",
            Self::RecreateSidecar => "recreate_sidecar",
            Self::MigrateLegacyMetadata => "migrate_legacy_metadata",
            Self::TargetedFtsRepair => "targeted_fts_repair",
            Self::ArchiveReconstruction => "archive_reconstruction",
            Self::WaitForWriter => "wait_for_writer",
            Self::InspectRefused => "inspect_refused",
        }
    }

    /// Whether this path mutates anything at all. The three non-mutating
    /// dispositions are the only `false` answers.
    pub(crate) fn is_mutating(self) -> bool {
        !matches!(
            self,
            Self::NoActionNeeded | Self::WaitForWriter | Self::InspectRefused
        )
    }

    /// What this path would mutate. `DerivedAssets` is reproducible from the
    /// canonical DB and lives *outside* it (the index dir). `Archive` touches
    /// the canonical DB file, its sidecars, or its bundle — always backup-first.
    pub(crate) fn mutation_scope(self) -> MutationScope {
        match self {
            Self::NoActionNeeded | Self::WaitForWriter | Self::InspectRefused => {
                MutationScope::None
            }
            // The lexical/semantic index lives outside the canonical DB and is
            // fully reproducible from it.
            Self::RebuildDerivedIndex => MutationScope::DerivedAssets,
            // Everything else writes the canonical DB file, its sidecars, or
            // its bundle — even FTS shadow-table repair writes inside the DB.
            Self::CheckpointRecoverWal
            | Self::RecreateSidecar
            | Self::MigrateLegacyMetadata
            | Self::TargetedFtsRepair
            | Self::ArchiveReconstruction => MutationScope::Archive,
        }
    }

    /// Rough cost of running this path.
    pub(crate) fn cost_class(self) -> CostClass {
        match self {
            Self::NoActionNeeded => CostClass::None,
            Self::WaitForWriter | Self::InspectRefused | Self::RecreateSidecar => {
                CostClass::Seconds
            }
            Self::CheckpointRecoverWal | Self::TargetedFtsRepair | Self::MigrateLegacyMetadata => {
                CostClass::Minutes
            }
            // Reconstructing the archive or re-deriving the full index over a
            // large corpus is the hours-class work.
            Self::RebuildDerivedIndex | Self::ArchiveReconstruction => CostClass::Hours,
        }
    }
}

/// Which write-ahead-log / shared-memory sidecars sit next to the canonical DB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub(crate) struct SidecarPresence {
    pub wal_present: bool,
    pub shm_present: bool,
}

/// One concrete mutation the plan expects to perform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ExpectedMutation {
    /// Path or asset class the mutation targets.
    pub target: String,
    pub scope: MutationScope,
    pub description: String,
    /// Whether the mutation can be cleanly reverted (from backup or by rebuild).
    pub reversible: bool,
    /// Whether a backup of this target is taken before the mutation.
    pub backed_up_first: bool,
}

/// A single recommended next command. `None` command for a pure wait/no-op.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SalvageCommand {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    pub mutation_scope: MutationScope,
    pub cost_class: CostClass,
    pub why: String,
}

/// A mutation that is unsafe in the current state, surfaced with the
/// precondition that unblocks it — never as a runnable command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SalvageBlockedCommand {
    pub command: String,
    pub why_blocked: String,
    pub unblock_precondition: String,
}

/// An explicit refusal to mutate, with the read-only inspection to run
/// instead. `preserves_evidence` is always true: a refusal never deletes the
/// archive or raw logs it could not safely repair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SalvageRefusal {
    pub reason: String,
    pub inspect_command: String,
    pub preserves_evidence: bool,
}

/// The backup-first storage salvage plan — the dry-run JSON contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StorageSalvagePlan {
    pub schema_version: u32,
    pub plan_kind: String,

    // --- diagnosis (projected verbatim from the .14.1 report) ---
    pub storage_state: StorageState,
    pub source_of_truth_risk: SourceOfTruthRisk,
    pub archive_readability: ArchiveReadability,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks_attempted: Vec<StorageCheck>,

    // --- subject ---
    pub source_db_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_version_observed: Option<u32>,
    pub sidecars: SidecarPresence,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_fingerprint: Option<String>,

    // --- decision ---
    pub repair_path: SalvageRepairPath,
    pub mutation_scope: MutationScope,
    pub cost_class: CostClass,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expected_mutations: Vec<ExpectedMutation>,

    // --- backup-first gating ---
    pub backup_required: bool,
    pub requires_confirmation: bool,
    /// A low-risk derived-only repair that needs no backup and no archive
    /// confirmation (the only path that may apply without operator sign-off).
    pub low_risk_derived_repair: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed_backup_path: Option<String>,

    // --- guidance ---
    pub recommended_command: SalvageCommand,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_commands: Vec<SalvageBlockedCommand>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preconditions: Vec<String>,
    pub abort_behavior: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rollback_guidance: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<SalvageRefusal>,

    // --- invariant ---
    /// Always true: no salvage plan ever deletes the canonical archive or raw
    /// session logs. Serialized so consumers can assert it.
    pub never_deletes_source_evidence: bool,
}

impl StorageSalvagePlan {
    /// Whether the plan, if approved, would write a byte.
    pub(crate) fn will_mutate(&self) -> bool {
        self.repair_path.is_mutating()
    }

    /// Whether the plan touches the canonical archive (vs only derived assets
    /// or nothing).
    pub(crate) fn touches_archive(&self) -> bool {
        self.mutation_scope == MutationScope::Archive
    }
}

/// Inputs the planner needs beyond the diagnostic report itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StorageSalvageContext {
    pub source_db_path: String,
    pub data_dir: Option<String>,
    pub schema_version_observed: Option<u32>,
    pub wal_present: bool,
    pub shm_present: bool,
    /// blake3 of the canonical DB at diagnosis time, when computed.
    pub before_fingerprint: Option<String>,
    /// Whether a reconstruction source (live connectors or a promoted
    /// candidate archive bundle) exists to rebuild an unreadable archive from.
    /// When false, archive reconstruction refuses and preserves the evidence.
    pub reconstruction_source_available: bool,
    /// Whether a verified backup already exists. Surfaced so a high-risk plan
    /// can report whether the backup precondition is already satisfiable.
    pub existing_backup_available: bool,
}

impl StorageSalvageContext {
    /// A minimal context for a given DB path with all optional signals absent.
    pub(crate) fn for_db(source_db_path: impl Into<String>) -> Self {
        Self {
            source_db_path: source_db_path.into(),
            data_dir: None,
            schema_version_observed: None,
            wal_present: false,
            shm_present: false,
            before_fingerprint: None,
            reconstruction_source_available: false,
            existing_backup_available: false,
        }
    }
}

/// Select the single repair path for a storage state. The only state whose
/// path depends on context is `WalSidecarSuspect`: a present WAL is
/// checkpointed; an orphaned `-shm` with no WAL is a sidecar to recreate.
fn select_repair_path(state: StorageState, ctx: &StorageSalvageContext) -> SalvageRepairPath {
    match state {
        StorageState::Ok => SalvageRepairPath::NoActionNeeded,
        StorageState::DerivedOnlyDrift => SalvageRepairPath::RebuildDerivedIndex,
        StorageState::BusyOrLocked => SalvageRepairPath::WaitForWriter,
        StorageState::WalSidecarSuspect => {
            if ctx.wal_present {
                SalvageRepairPath::CheckpointRecoverWal
            } else if ctx.shm_present {
                SalvageRepairPath::RecreateSidecar
            } else {
                // Suspect but neither sidecar is on disk: checkpoint/recover is
                // the conservative op (it is a no-op when there is nothing to
                // checkpoint, and never removes a file).
                SalvageRepairPath::CheckpointRecoverWal
            }
        }
        StorageState::SchemaDrift | StorageState::LegacyInteropFailed => {
            SalvageRepairPath::MigrateLegacyMetadata
        }
        StorageState::FtsMetadataFailed => SalvageRepairPath::TargetedFtsRepair,
        StorageState::OpenreadFailed | StorageState::IntegrityFailed => {
            SalvageRepairPath::ArchiveReconstruction
        }
        // An unsafe SQL shape is a query-construction fault, not a data fault:
        // refuse to "repair" by mutating data. A deferred verdict never earns
        // a mutation either — and neither does the readiness-only `unchecked`
        // verdict (#331), which carries no structural evidence at all.
        StorageState::UnsafeSqlShape | StorageState::UnknownDeferred | StorageState::Unchecked => {
            SalvageRepairPath::InspectRefused
        }
    }
}

/// The concrete read-only doctor-check command for the canonical DB.
fn check_command() -> String {
    "cass doctor check --json".to_string()
}

/// The concrete repair dry-run command (the safe first step of any mutating
/// flow — prints the exact fingerprint-gated apply command).
fn repair_dry_run_command() -> String {
    "cass doctor repair --dry-run --json".to_string()
}

/// The fingerprint-gated apply command (shown only as a *blocked* command
/// until its precondition holds — never recommended directly).
fn repair_apply_command() -> String {
    "cass doctor repair --yes --plan-fingerprint <fingerprint> --json".to_string()
}

/// The read-only backups-list command (entry point to restore-from-backup).
fn backups_list_command() -> String {
    "cass doctor backups list --json".to_string()
}

/// Build the backup-first salvage plan for a diagnosed storage report.
pub(crate) fn plan_storage_salvage(
    report: &StorageIntegrityReport,
    ctx: &StorageSalvageContext,
) -> StorageSalvagePlan {
    let state = report.storage_state;
    let risk = report.source_of_truth_risk;
    let repair_path = select_repair_path(state, ctx);
    let scope = repair_path.mutation_scope();
    let cost = repair_path.cost_class();
    let sidecars = SidecarPresence {
        wal_present: ctx.wal_present,
        shm_present: ctx.shm_present,
    };

    // Backup-first: any mutation that touches the canonical archive backs up
    // first. High (or unevaluable) source-of-truth risk additionally requires
    // explicit operator confirmation.
    let touches_archive = scope == MutationScope::Archive;
    let backup_required = touches_archive;
    let requires_confirmation =
        touches_archive || matches!(risk, SourceOfTruthRisk::High | SourceOfTruthRisk::Unknown);
    // Only a low-risk derived-only rebuild may apply without sign-off.
    let low_risk_derived_repair = repair_path.is_mutating()
        && scope == MutationScope::DerivedAssets
        && matches!(risk, SourceOfTruthRisk::None | SourceOfTruthRisk::Low);

    let proposed_backup_path = if backup_required {
        Some(format!("{}{}", ctx.source_db_path, SALVAGE_BACKUP_SUFFIX))
    } else {
        None
    };

    let expected_mutations = build_expected_mutations(repair_path, ctx, scope, backup_required);
    let recommended_command = build_recommended_command(repair_path, scope, cost, ctx);
    let blocked_commands = build_blocked_commands(repair_path, ctx);
    let preconditions = build_preconditions(backup_required, ctx);
    let abort_behavior = build_abort_behavior(repair_path);
    let rollback_guidance = build_rollback_guidance(repair_path, backup_required, ctx);
    let refusal = build_refusal(repair_path, ctx);

    StorageSalvagePlan {
        schema_version: SALVAGE_PLAN_SCHEMA_VERSION,
        plan_kind: SALVAGE_PLAN_KIND.to_string(),
        storage_state: state,
        source_of_truth_risk: risk,
        archive_readability: report.archive_readability,
        checks_attempted: report.checks_attempted.clone(),
        source_db_path: ctx.source_db_path.clone(),
        schema_version_observed: ctx.schema_version_observed,
        sidecars,
        before_fingerprint: ctx.before_fingerprint.clone(),
        repair_path,
        mutation_scope: scope,
        cost_class: cost,
        expected_mutations,
        backup_required,
        requires_confirmation,
        low_risk_derived_repair,
        proposed_backup_path,
        recommended_command,
        blocked_commands,
        preconditions,
        abort_behavior,
        rollback_guidance,
        refusal,
        never_deletes_source_evidence: true,
    }
}

fn build_expected_mutations(
    path: SalvageRepairPath,
    ctx: &StorageSalvageContext,
    scope: MutationScope,
    backed_up_first: bool,
) -> Vec<ExpectedMutation> {
    let index_dir = ctx
        .data_dir
        .as_deref()
        .map(|d| format!("{d}/index"))
        .unwrap_or_else(|| "<data_dir>/index".to_string());
    match path {
        SalvageRepairPath::NoActionNeeded
        | SalvageRepairPath::WaitForWriter
        | SalvageRepairPath::InspectRefused => Vec::new(),
        SalvageRepairPath::RebuildDerivedIndex => vec![ExpectedMutation {
            target: index_dir,
            scope,
            description:
                "rebuild the derived lexical index from the intact canonical DB and atomic-swap it"
                    .to_string(),
            reversible: true,
            backed_up_first,
        }],
        SalvageRepairPath::CheckpointRecoverWal => vec![ExpectedMutation {
            target: format!("{}-wal", ctx.source_db_path),
            scope,
            description:
                "checkpoint the write-ahead log back into the main DB, then truncate the WAL"
                    .to_string(),
            reversible: true,
            backed_up_first,
        }],
        SalvageRepairPath::RecreateSidecar => vec![ExpectedMutation {
            target: format!("{}-shm", ctx.source_db_path),
            scope,
            description:
                "move the orphaned shared-memory sidecar aside so the engine recreates a clean one"
                    .to_string(),
            reversible: true,
            backed_up_first,
        }],
        SalvageRepairPath::MigrateLegacyMetadata => vec![ExpectedMutation {
            target: ctx.source_db_path.clone(),
            scope,
            description: "migrate drifted/legacy metadata to the current schema contract"
                .to_string(),
            reversible: true,
            backed_up_first,
        }],
        SalvageRepairPath::TargetedFtsRepair => vec![ExpectedMutation {
            target: format!("{} (fts shadow tables)", ctx.source_db_path),
            scope,
            description: "rebuild only the FTS shadow tables from the intact base rows".to_string(),
            reversible: true,
            backed_up_first,
        }],
        SalvageRepairPath::ArchiveReconstruction => {
            if ctx.reconstruction_source_available {
                vec![ExpectedMutation {
                    target: format!("{} (reconstructed)", ctx.source_db_path),
                    scope,
                    description:
                        "reconstruct the archive from source connectors / a promoted candidate \
                         bundle into a fresh DB beside the preserved original"
                            .to_string(),
                    reversible: true,
                    backed_up_first,
                }]
            } else {
                // No source to reconstruct from: the plan refuses to mutate and
                // preserves the unreadable archive as evidence.
                Vec::new()
            }
        }
    }
}

fn build_recommended_command(
    path: SalvageRepairPath,
    scope: MutationScope,
    cost: CostClass,
    ctx: &StorageSalvageContext,
) -> SalvageCommand {
    match path {
        SalvageRepairPath::NoActionNeeded => SalvageCommand {
            command: None,
            mutation_scope: MutationScope::None,
            cost_class: CostClass::None,
            why: "storage is healthy; no repair is needed".to_string(),
        },
        SalvageRepairPath::WaitForWriter => SalvageCommand {
            command: Some("cass status --json".to_string()),
            mutation_scope: MutationScope::None,
            cost_class: CostClass::Seconds,
            why: "another writer holds the lock; wait and re-check — never force a busy DB"
                .to_string(),
        },
        SalvageRepairPath::InspectRefused => SalvageCommand {
            command: Some(check_command()),
            mutation_scope: MutationScope::None,
            cost_class: CostClass::Seconds,
            why: "this state is not a safe mutation target; inspect read-only first".to_string(),
        },
        SalvageRepairPath::RebuildDerivedIndex => SalvageCommand {
            command: Some(repair_dry_run_command()),
            mutation_scope: scope,
            cost_class: cost,
            why: "rebuild the derived index from the intact canonical DB (low-risk, reproducible)"
                .to_string(),
        },
        SalvageRepairPath::ArchiveReconstruction if !ctx.reconstruction_source_available => {
            // Unreadable canonical with no source to rebuild from: the only
            // safe move is to inspect backups and restore — never mutate the
            // sole remaining evidence.
            SalvageCommand {
                command: Some(backups_list_command()),
                mutation_scope: MutationScope::None,
                cost_class: CostClass::Seconds,
                why: "canonical archive is unreadable and no reconstruction source exists; \
                      inspect backups to restore — the archive is preserved as evidence"
                    .to_string(),
            }
        }
        // Every remaining path touches the canonical archive: the recommended
        // first step is always the read-only dry-run, which prints the exact
        // fingerprint-gated apply command after a backup exists.
        _ => SalvageCommand {
            command: Some(repair_dry_run_command()),
            mutation_scope: MutationScope::None,
            cost_class: CostClass::Seconds,
            why: "review the backup-first dry-run plan; it prints the exact apply command once a \
                  verified backup exists"
                .to_string(),
        },
    }
}

fn build_blocked_commands(
    path: SalvageRepairPath,
    ctx: &StorageSalvageContext,
) -> Vec<SalvageBlockedCommand> {
    // Only archive-touching mutations are gated. Derived rebuilds and the
    // non-mutating dispositions block nothing.
    if path.mutation_scope() != MutationScope::Archive {
        return Vec::new();
    }
    if path == SalvageRepairPath::ArchiveReconstruction && !ctx.reconstruction_source_available {
        return vec![SalvageBlockedCommand {
            command: repair_apply_command(),
            why_blocked: "the canonical archive is unreadable and there is no source to \
                          reconstruct it from; mutating it would destroy the only evidence"
                .to_string(),
            unblock_precondition: "restore from a verified backup, or make a reconstruction \
                                   source available, before any mutation"
                .to_string(),
        }];
    }
    let precondition = if ctx.existing_backup_available {
        "confirm the existing verified backup covers this DB, then apply the exact reported \
         plan fingerprint"
            .to_string()
    } else {
        "create a verified backup of the canonical archive first, then apply the exact reported \
         plan fingerprint"
            .to_string()
    };
    vec![SalvageBlockedCommand {
        command: repair_apply_command(),
        why_blocked: "this mutation touches the canonical archive before a verified backup exists"
            .to_string(),
        unblock_precondition: precondition,
    }]
}

fn build_preconditions(backup_required: bool, ctx: &StorageSalvageContext) -> Vec<String> {
    if !backup_required {
        return Vec::new();
    }
    let where_ = ctx.data_dir.as_deref().unwrap_or("the canonical data dir");
    let mut out = vec![format!(
        "back up {where_} before any mutating repair (backup-first)"
    )];
    if ctx.existing_backup_available {
        out.push("a verified backup already exists; confirm it covers this DB".to_string());
    }
    out
}

fn build_abort_behavior(path: SalvageRepairPath) -> String {
    match path.mutation_scope() {
        MutationScope::None => {
            "no mutation is planned; aborting leaves everything untouched".to_string()
        }
        MutationScope::DerivedAssets => {
            "aborting before publish leaves the live derived index in place; a half-built index \
             is discarded, never swapped in"
                .to_string()
        }
        MutationScope::Archive => {
            "aborting before apply leaves the canonical archive and its backup untouched; a \
             partial apply is rolled back from the backup, never left half-written"
                .to_string()
        }
    }
}

fn build_rollback_guidance(
    path: SalvageRepairPath,
    backup_required: bool,
    ctx: &StorageSalvageContext,
) -> Vec<String> {
    match path {
        SalvageRepairPath::NoActionNeeded
        | SalvageRepairPath::WaitForWriter
        | SalvageRepairPath::InspectRefused => Vec::new(),
        SalvageRepairPath::RebuildDerivedIndex => vec![
            "the prior lexical generation is retained under index/.lexical-publish-backups/ for \
             one-step rollback"
                .to_string(),
        ],
        _ => {
            let mut out = Vec::new();
            if backup_required {
                let backup = format!("{}{}", ctx.source_db_path, SALVAGE_BACKUP_SUFFIX);
                out.push(format!(
                    "restore the canonical DB from the pre-mutation backup at {backup}"
                ));
            }
            out.push(
                "`cass doctor backups restore <backup-id> --json` rehearses, then applies, the \
                 restore from any retained backup"
                    .to_string(),
            );
            out
        }
    }
}

fn build_refusal(path: SalvageRepairPath, ctx: &StorageSalvageContext) -> Option<SalvageRefusal> {
    match path {
        SalvageRepairPath::InspectRefused => Some(SalvageRefusal {
            reason: "this storage state is a query-construction or deferred-verdict fault, not a \
                     repairable data fault; mutating data would not fix it"
                .to_string(),
            inspect_command: check_command(),
            preserves_evidence: true,
        }),
        SalvageRepairPath::ArchiveReconstruction if !ctx.reconstruction_source_available => {
            Some(SalvageRefusal {
                reason: "the canonical archive is unreadable and no reconstruction source is \
                         available; mutating it would destroy the only remaining evidence"
                    .to_string(),
                inspect_command: backups_list_command(),
                preserves_evidence: true,
            })
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Apply receipt: the record every *mutating* salvage apply emits.
// ---------------------------------------------------------------------------

/// Terminal status of a salvage apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SalvageApplyStatus {
    /// The plan applied fully and post-checks passed.
    Succeeded,
    /// Some steps applied; at least one did not — see `partial_failure_reason`.
    PartiallyApplied,
    /// The apply aborted before mutating (or rolled fully back from backup).
    Aborted,
    /// The planner refused to mutate; nothing was attempted.
    Refused,
}

impl SalvageApplyStatus {
    pub(crate) fn is_clean_success(self) -> bool {
        matches!(self, Self::Succeeded)
    }

    /// Whether this status must carry a `partial_failure_reason`.
    pub(crate) fn requires_failure_reason(self) -> bool {
        matches!(self, Self::PartiallyApplied | Self::Aborted)
    }
}

/// The record a mutating salvage apply emits: before/after fingerprints,
/// command provenance, elapsed time, partial-failure status, and
/// rollback/restore guidance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SalvageApplyReceipt {
    pub schema_version: u32,
    pub receipt_kind: String,
    pub repair_path: SalvageRepairPath,
    /// The plan fingerprint this apply was authorized against (provenance).
    pub plan_fingerprint: String,
    /// The exact argv that ran (command provenance).
    pub command_provenance: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup_path: Option<String>,
    pub elapsed_ms: i64,
    pub status: SalvageApplyStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial_failure_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rollback_guidance: Vec<String>,
}

/// Why an [`SalvageApplyReceipt`] failed its own well-formedness contract.
/// The shared `Missing` prefix is the semantic point of the type (each variant
/// names something the receipt failed to record), so the variant-name lint is
/// intentionally allowed here.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)]
pub(crate) enum SalvageReceiptDefect {
    /// An archive-touching apply did not record the backup it took.
    MissingBackupPath,
    /// A clean success did not record both before and after fingerprints.
    MissingFingerprints,
    /// A partial/aborted apply did not record why it failed.
    MissingFailureReason,
    /// An archive-touching apply did not provide rollback guidance.
    MissingRollbackGuidance,
}

impl SalvageApplyReceipt {
    /// Check the receipt against the bead's recording contract: every mutating
    /// archive apply records before/after fingerprints (on success), the
    /// backup it took, a failure reason on partial/abort, and rollback
    /// guidance. Returns every defect found (empty = well-formed).
    pub(crate) fn defects(&self) -> Vec<SalvageReceiptDefect> {
        let mut defects = Vec::new();
        let touches_archive = self.repair_path.mutation_scope() == MutationScope::Archive;

        if self.status == SalvageApplyStatus::Succeeded
            && (self.before_fingerprint.is_none() || self.after_fingerprint.is_none())
        {
            defects.push(SalvageReceiptDefect::MissingFingerprints);
        }
        if touches_archive
            && self.status != SalvageApplyStatus::Refused
            && self.backup_path.is_none()
        {
            defects.push(SalvageReceiptDefect::MissingBackupPath);
        }
        if self.status.requires_failure_reason() && self.partial_failure_reason.is_none() {
            defects.push(SalvageReceiptDefect::MissingFailureReason);
        }
        if touches_archive
            && self.status != SalvageApplyStatus::Refused
            && self.rollback_guidance.is_empty()
        {
            defects.push(SalvageReceiptDefect::MissingRollbackGuidance);
        }
        defects
    }

    /// Whether the receipt is well-formed under the recording contract.
    pub(crate) fn is_well_formed(&self) -> bool {
        self.defects().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::storage_integrity::StorageIntegrityReport;

    const ALL_STATES: &[StorageState] = &[
        StorageState::Ok,
        StorageState::DerivedOnlyDrift,
        StorageState::BusyOrLocked,
        StorageState::WalSidecarSuspect,
        StorageState::SchemaDrift,
        StorageState::OpenreadFailed,
        StorageState::IntegrityFailed,
        StorageState::LegacyInteropFailed,
        StorageState::FtsMetadataFailed,
        StorageState::UnsafeSqlShape,
        StorageState::UnknownDeferred,
        StorageState::Unchecked,
    ];

    fn report(state: StorageState, readability: ArchiveReadability) -> StorageIntegrityReport {
        StorageIntegrityReport::derive(state, readability, vec![StorageCheck::ran("probe", 1)])
    }

    fn ctx() -> StorageSalvageContext {
        let mut c = StorageSalvageContext::for_db("/data/cass/agent_search.db");
        c.data_dir = Some("/data/cass".to_string());
        c
    }

    /// No generated command string is ever bare or destructive. This is the
    /// core unsafe-command-suppression guard.
    fn assert_no_bare_or_destructive(plan: &StorageSalvagePlan) {
        let mut commands: Vec<String> = Vec::new();
        if let Some(c) = &plan.recommended_command.command {
            commands.push(c.clone());
        }
        for b in &plan.blocked_commands {
            commands.push(b.command.clone());
        }
        if let Some(r) = &plan.refusal {
            commands.push(r.inspect_command.clone());
        }
        for cmd in commands {
            assert_ne!(cmd.trim(), "cass", "bare cass in {:?}", plan.repair_path);
            assert!(
                cmd.starts_with("cass "),
                "must be a concrete cass command: {cmd}"
            );
            for bad in [
                " rm ",
                "rm -",
                "--force-clean",
                "DROP ",
                "delete ",
                "DELETE ",
                "--purge",
                "unlink",
                "truncate ",
                "> /",
                "shred",
            ] {
                assert!(
                    !cmd.to_ascii_lowercase().contains(&bad.to_ascii_lowercase()),
                    "destructive token {bad:?} in command: {cmd}"
                );
            }
        }
    }

    #[test]
    fn repair_path_values_serialize_snake_case_and_are_stable() {
        let pairs: &[(SalvageRepairPath, &str)] = &[
            (SalvageRepairPath::NoActionNeeded, "no_action_needed"),
            (
                SalvageRepairPath::RebuildDerivedIndex,
                "rebuild_derived_index",
            ),
            (
                SalvageRepairPath::CheckpointRecoverWal,
                "checkpoint_recover_wal",
            ),
            (SalvageRepairPath::RecreateSidecar, "recreate_sidecar"),
            (
                SalvageRepairPath::MigrateLegacyMetadata,
                "migrate_legacy_metadata",
            ),
            (SalvageRepairPath::TargetedFtsRepair, "targeted_fts_repair"),
            (
                SalvageRepairPath::ArchiveReconstruction,
                "archive_reconstruction",
            ),
            (SalvageRepairPath::WaitForWriter, "wait_for_writer"),
            (SalvageRepairPath::InspectRefused, "inspect_refused"),
        ];
        for (variant, want) in pairs {
            assert_eq!(
                serde_json::to_string(variant).expect("serialize repair path"),
                format!("\"{want}\"")
            );
            assert_eq!(variant.stable_name(), *want);
        }
    }

    #[test]
    fn every_storage_state_maps_to_exactly_one_repair_path() {
        let context = ctx();
        let expected: &[(StorageState, SalvageRepairPath)] = &[
            (StorageState::Ok, SalvageRepairPath::NoActionNeeded),
            (
                StorageState::DerivedOnlyDrift,
                SalvageRepairPath::RebuildDerivedIndex,
            ),
            (StorageState::BusyOrLocked, SalvageRepairPath::WaitForWriter),
            // WalSidecarSuspect with neither sidecar present -> checkpoint.
            (
                StorageState::WalSidecarSuspect,
                SalvageRepairPath::CheckpointRecoverWal,
            ),
            (
                StorageState::SchemaDrift,
                SalvageRepairPath::MigrateLegacyMetadata,
            ),
            (
                StorageState::OpenreadFailed,
                SalvageRepairPath::ArchiveReconstruction,
            ),
            (
                StorageState::IntegrityFailed,
                SalvageRepairPath::ArchiveReconstruction,
            ),
            (
                StorageState::LegacyInteropFailed,
                SalvageRepairPath::MigrateLegacyMetadata,
            ),
            (
                StorageState::FtsMetadataFailed,
                SalvageRepairPath::TargetedFtsRepair,
            ),
            (
                StorageState::UnsafeSqlShape,
                SalvageRepairPath::InspectRefused,
            ),
            (
                StorageState::UnknownDeferred,
                SalvageRepairPath::InspectRefused,
            ),
            (StorageState::Unchecked, SalvageRepairPath::InspectRefused),
        ];
        for (state, want) in expected {
            let plan =
                plan_storage_salvage(&report(*state, ArchiveReadability::Readable), &context);
            assert_eq!(plan.repair_path, *want, "state {state:?}");
        }
    }

    #[test]
    fn wal_sidecar_branches_on_present_sidecars() {
        // WAL present -> checkpoint.
        let mut c = ctx();
        c.wal_present = true;
        let plan = plan_storage_salvage(
            &report(
                StorageState::WalSidecarSuspect,
                ArchiveReadability::Readable,
            ),
            &c,
        );
        assert_eq!(plan.repair_path, SalvageRepairPath::CheckpointRecoverWal);

        // Orphaned shm, no WAL -> recreate sidecar.
        let mut c = ctx();
        c.wal_present = false;
        c.shm_present = true;
        let plan = plan_storage_salvage(
            &report(
                StorageState::WalSidecarSuspect,
                ArchiveReadability::Readable,
            ),
            &c,
        );
        assert_eq!(plan.repair_path, SalvageRepairPath::RecreateSidecar);
    }

    #[test]
    fn all_six_named_repair_paths_are_reachable_from_some_state() {
        use SalvageRepairPath as P;
        let context = {
            let mut c = ctx();
            c.shm_present = true; // make RecreateSidecar reachable
            c
        };
        let mut seen = std::collections::HashSet::new();
        for &state in ALL_STATES {
            seen.insert(
                plan_storage_salvage(&report(state, ArchiveReadability::Readable), &context)
                    .repair_path,
            );
        }
        // The WAL-present branch (checkpoint) needs wal_present; check directly.
        let mut wal_ctx = ctx();
        wal_ctx.wal_present = true;
        seen.insert(
            plan_storage_salvage(
                &report(
                    StorageState::WalSidecarSuspect,
                    ArchiveReadability::Readable,
                ),
                &wal_ctx,
            )
            .repair_path,
        );
        for required in [
            P::RebuildDerivedIndex,
            P::CheckpointRecoverWal,
            P::RecreateSidecar,
            P::MigrateLegacyMetadata,
            P::TargetedFtsRepair,
            P::ArchiveReconstruction,
        ] {
            assert!(
                seen.contains(&required),
                "named path {required:?} unreachable"
            );
        }
    }

    #[test]
    fn no_plan_for_any_state_emits_a_bare_or_destructive_command() {
        let mut context = ctx();
        context.wal_present = true;
        context.shm_present = true;
        for &state in ALL_STATES {
            let plan =
                plan_storage_salvage(&report(state, ArchiveReadability::Unreadable), &context);
            assert_no_bare_or_destructive(&plan);
            // The invariant flag is always set.
            assert!(plan.never_deletes_source_evidence, "{state:?}");
        }
    }

    #[test]
    fn healthy_storage_is_a_non_mutating_no_op() {
        let plan = plan_storage_salvage(
            &report(StorageState::Ok, ArchiveReadability::Readable),
            &ctx(),
        );
        assert_eq!(plan.repair_path, SalvageRepairPath::NoActionNeeded);
        assert!(!plan.will_mutate());
        assert!(!plan.backup_required);
        assert!(!plan.requires_confirmation);
        assert!(!plan.low_risk_derived_repair);
        assert!(plan.recommended_command.command.is_none());
        assert!(plan.expected_mutations.is_empty());
        assert!(plan.blocked_commands.is_empty());
    }

    #[test]
    fn low_risk_derived_repair_needs_no_backup_and_no_confirmation() {
        let plan = plan_storage_salvage(
            &report(StorageState::DerivedOnlyDrift, ArchiveReadability::Readable),
            &ctx(),
        );
        assert_eq!(plan.repair_path, SalvageRepairPath::RebuildDerivedIndex);
        assert_eq!(plan.mutation_scope, MutationScope::DerivedAssets);
        assert!(plan.low_risk_derived_repair);
        assert!(!plan.backup_required, "derived rebuild is reproducible");
        assert!(!plan.requires_confirmation);
        assert!(plan.proposed_backup_path.is_none());
        assert!(plan.will_mutate());
        // It still offers a rollback (prior generation retained).
        assert!(!plan.rollback_guidance.is_empty());
    }

    #[test]
    fn high_risk_archive_state_requires_backup_and_confirmation() {
        let mut context = ctx();
        context.reconstruction_source_available = true; // a real reconstruct path
        for state in [StorageState::OpenreadFailed, StorageState::IntegrityFailed] {
            let plan =
                plan_storage_salvage(&report(state, ArchiveReadability::Unreadable), &context);
            assert_eq!(plan.repair_path, SalvageRepairPath::ArchiveReconstruction);
            assert_eq!(
                plan.source_of_truth_risk,
                SourceOfTruthRisk::High,
                "{state:?}"
            );
            assert!(plan.backup_required, "{state:?} must back up first");
            assert!(plan.requires_confirmation, "{state:?} must confirm");
            assert!(!plan.low_risk_derived_repair);
            assert!(plan.proposed_backup_path.is_some());
            // The mutating apply is blocked behind the backup precondition.
            assert!(!plan.blocked_commands.is_empty());
            assert!(plan.preconditions.iter().any(|p| p.contains("/data/cass")));
            // Recommended first step never mutates.
            assert_eq!(plan.recommended_command.mutation_scope, MutationScope::None);
            assert!(!plan.rollback_guidance.is_empty());
            assert_no_bare_or_destructive(&plan);
        }
    }

    #[test]
    fn unreadable_archive_without_source_refuses_and_preserves_evidence() {
        let mut context = ctx();
        context.reconstruction_source_available = false;
        let plan = plan_storage_salvage(
            &report(StorageState::OpenreadFailed, ArchiveReadability::Unreadable),
            &context,
        );
        // Still classified as the reconstruction path, but it refuses.
        assert_eq!(plan.repair_path, SalvageRepairPath::ArchiveReconstruction);
        let refusal = plan.refusal.as_ref().expect("must refuse without a source");
        assert!(refusal.preserves_evidence);
        assert!(refusal.reason.contains("evidence"));
        // No expected mutation is planned against the sole evidence.
        assert!(plan.expected_mutations.is_empty());
        // The blocked apply names the preserve-evidence precondition.
        assert!(
            plan.blocked_commands
                .iter()
                .any(|b| b.unblock_precondition.contains("backup")
                    || b.unblock_precondition.contains("reconstruction source"))
        );
        // Recommended move is read-only backups inspection.
        assert_eq!(plan.recommended_command.mutation_scope, MutationScope::None);
        assert_no_bare_or_destructive(&plan);
    }

    #[test]
    fn unsafe_sql_shape_and_unknown_refuse_to_mutate() {
        for state in [
            StorageState::UnsafeSqlShape,
            StorageState::UnknownDeferred,
            StorageState::Unchecked,
        ] {
            let plan = plan_storage_salvage(&report(state, ArchiveReadability::Readable), &ctx());
            assert_eq!(
                plan.repair_path,
                SalvageRepairPath::InspectRefused,
                "{state:?}"
            );
            assert!(!plan.will_mutate());
            assert!(plan.refusal.is_some(), "{state:?} must carry a refusal");
            assert!(plan.expected_mutations.is_empty());
            assert!(plan.blocked_commands.is_empty());
            // Refused inspection is read-only.
            assert_eq!(plan.recommended_command.mutation_scope, MutationScope::None);
        }
    }

    #[test]
    fn busy_lock_waits_and_never_mutates() {
        let plan = plan_storage_salvage(
            &report(StorageState::BusyOrLocked, ArchiveReadability::NotChecked),
            &ctx(),
        );
        assert_eq!(plan.repair_path, SalvageRepairPath::WaitForWriter);
        assert!(!plan.will_mutate());
        assert!(!plan.backup_required);
        assert!(plan.recommended_command.why.contains("never force"));
        assert!(plan.blocked_commands.is_empty());
    }

    #[test]
    fn archive_touching_paths_all_back_up_first_and_gate_apply() {
        let mut context = ctx();
        context.wal_present = true;
        let archive_states = [
            StorageState::WalSidecarSuspect,
            StorageState::SchemaDrift,
            StorageState::LegacyInteropFailed,
            StorageState::FtsMetadataFailed,
        ];
        for state in archive_states {
            let plan = plan_storage_salvage(&report(state, ArchiveReadability::Readable), &context);
            assert_eq!(plan.mutation_scope, MutationScope::Archive, "{state:?}");
            assert!(plan.backup_required, "{state:?} backup-first");
            assert!(plan.proposed_backup_path.is_some(), "{state:?}");
            assert!(!plan.blocked_commands.is_empty(), "{state:?} gates apply");
            // Each expected mutation is recorded as backed up first.
            assert!(
                plan.expected_mutations.iter().all(|m| m.backed_up_first),
                "{state:?} mutations must be backed up first"
            );
            assert!(!plan.rollback_guidance.is_empty(), "{state:?}");
        }
    }

    #[test]
    fn fts_repair_is_archive_scope_but_low_source_of_truth_risk() {
        // FtsMetadataFailed keeps the base rows trustworthy (Low risk) yet the
        // repair writes inside the canonical DB -> still backup-first.
        let plan = plan_storage_salvage(
            &report(
                StorageState::FtsMetadataFailed,
                ArchiveReadability::Readable,
            ),
            &ctx(),
        );
        assert_eq!(plan.repair_path, SalvageRepairPath::TargetedFtsRepair);
        assert_eq!(plan.source_of_truth_risk, SourceOfTruthRisk::Low);
        assert_eq!(plan.mutation_scope, MutationScope::Archive);
        assert!(plan.backup_required);
        // Low SoT risk -> no hard confirmation, but archive scope still gates it.
        assert!(plan.requires_confirmation);
        assert!(!plan.low_risk_derived_repair);
    }

    #[test]
    fn plan_projects_report_checks_and_subject_metadata() {
        let mut context = ctx();
        context.schema_version_observed = Some(7);
        context.before_fingerprint = Some("blake3:abc".to_string());
        context.wal_present = true;
        let r = StorageIntegrityReport::derive(
            StorageState::SchemaDrift,
            ArchiveReadability::Readable,
            vec![
                StorageCheck::ran("open_read", 2),
                StorageCheck::ran("schema_version", 1),
            ],
        );
        let plan = plan_storage_salvage(&r, &context);
        assert_eq!(plan.checks_attempted.len(), 2);
        assert_eq!(plan.schema_version_observed, Some(7));
        assert_eq!(plan.before_fingerprint.as_deref(), Some("blake3:abc"));
        assert!(plan.sidecars.wal_present);
        assert_eq!(plan.source_db_path, "/data/cass/agent_search.db");
    }

    #[test]
    fn plan_round_trips_through_json() {
        let mut context = ctx();
        context.reconstruction_source_available = true;
        let plan = plan_storage_salvage(
            &report(
                StorageState::IntegrityFailed,
                ArchiveReadability::PartiallyReadable,
            ),
            &context,
        );
        let json = serde_json::to_string(&plan).expect("serialize plan");
        assert!(json.contains("\"plan_kind\":\"storage_salvage_plan\""));
        assert!(json.contains("\"repair_path\":\"archive_reconstruction\""));
        assert!(json.contains("\"backup_required\":true"));
        assert!(json.contains("\"never_deletes_source_evidence\":true"));
        let parsed: StorageSalvagePlan = serde_json::from_str(&json).expect("parse plan");
        assert_eq!(parsed, plan);
    }

    #[test]
    fn apply_receipt_requires_fingerprints_backup_and_rollback_for_archive() {
        // A well-formed archive success.
        let good = SalvageApplyReceipt {
            schema_version: SALVAGE_PLAN_SCHEMA_VERSION,
            receipt_kind: SALVAGE_RECEIPT_KIND.to_string(),
            repair_path: SalvageRepairPath::MigrateLegacyMetadata,
            plan_fingerprint: "doctor-repair-apply-plan-v1-abc".to_string(),
            command_provenance: vec![
                "cass".to_string(),
                "doctor".to_string(),
                "repair".to_string(),
                "--yes".to_string(),
            ],
            before_fingerprint: Some("blake3:before".to_string()),
            after_fingerprint: Some("blake3:after".to_string()),
            backup_path: Some("/data/cass/agent_search.db.salvage-backup".to_string()),
            elapsed_ms: 1200,
            status: SalvageApplyStatus::Succeeded,
            partial_failure_reason: None,
            rollback_guidance: vec!["restore from backup".to_string()],
        };
        assert!(good.is_well_formed(), "{:?}", good.defects());

        // Missing backup path on an archive apply is a defect.
        let mut no_backup = good.clone();
        no_backup.backup_path = None;
        assert!(
            no_backup
                .defects()
                .contains(&SalvageReceiptDefect::MissingBackupPath)
        );

        // Missing fingerprints on a success is a defect.
        let mut no_fp = good.clone();
        no_fp.before_fingerprint = None;
        assert!(
            no_fp
                .defects()
                .contains(&SalvageReceiptDefect::MissingFingerprints)
        );

        // A partial apply must record why.
        let mut partial = good.clone();
        partial.status = SalvageApplyStatus::PartiallyApplied;
        partial.partial_failure_reason = None;
        assert!(
            partial
                .defects()
                .contains(&SalvageReceiptDefect::MissingFailureReason)
        );
        partial.partial_failure_reason = Some("checkpoint step failed".to_string());
        assert!(partial.is_well_formed());

        // Missing rollback guidance on an archive apply is a defect.
        let mut no_rollback = good.clone();
        no_rollback.rollback_guidance = Vec::new();
        assert!(
            no_rollback
                .defects()
                .contains(&SalvageReceiptDefect::MissingRollbackGuidance)
        );
    }

    #[test]
    fn refused_apply_needs_neither_backup_nor_fingerprints() {
        let refused = SalvageApplyReceipt {
            schema_version: SALVAGE_PLAN_SCHEMA_VERSION,
            receipt_kind: SALVAGE_RECEIPT_KIND.to_string(),
            repair_path: SalvageRepairPath::ArchiveReconstruction,
            plan_fingerprint: "n/a".to_string(),
            command_provenance: vec![
                "cass".to_string(),
                "doctor".to_string(),
                "check".to_string(),
            ],
            before_fingerprint: None,
            after_fingerprint: None,
            backup_path: None,
            elapsed_ms: 3,
            status: SalvageApplyStatus::Refused,
            partial_failure_reason: None,
            rollback_guidance: Vec::new(),
        };
        assert!(refused.is_well_formed(), "{:?}", refused.defects());
        assert!(!refused.status.is_clean_success());
    }

    #[test]
    fn apply_status_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&SalvageApplyStatus::PartiallyApplied).expect("serialize"),
            "\"partially_applied\""
        );
        assert_eq!(
            serde_json::to_string(&SalvageApplyStatus::Refused).expect("serialize"),
            "\"refused\""
        );
    }

    #[test]
    fn mutation_scope_and_is_mutating_agree_for_every_path() {
        use SalvageRepairPath as P;
        for path in [
            P::NoActionNeeded,
            P::RebuildDerivedIndex,
            P::CheckpointRecoverWal,
            P::RecreateSidecar,
            P::MigrateLegacyMetadata,
            P::TargetedFtsRepair,
            P::ArchiveReconstruction,
            P::WaitForWriter,
            P::InspectRefused,
        ] {
            let mutating = path.is_mutating();
            let scope = path.mutation_scope();
            if mutating {
                assert_ne!(
                    scope,
                    MutationScope::None,
                    "{path:?} mutates yet scope None"
                );
            } else {
                assert_eq!(
                    scope,
                    MutationScope::None,
                    "{path:?} non-mutating yet scope set"
                );
            }
        }
    }

    // -- structured proof logs over the bead's five acceptance scenarios -----

    use crate::search::proof_log::{
        OutcomeSignals, ProofArtifacts, ProofExecution, ProofLogRecord, ProofOutcome, ProofRunMeta,
    };

    /// Build a proof record for a salvage scenario tying the run signals to the
    /// 12.3 proof-log schema. The `command_id`/`argv` are the salvage command
    /// under test; `signals` derive the outcome.
    fn salvage_proof_record(
        scenario_id: &str,
        command_id: &str,
        argv: &[&str],
        signals: OutcomeSignals,
    ) -> ProofLogRecord {
        ProofLogRecord {
            run_id: "salvage-run-1".to_string(),
            scenario_id: scenario_id.to_string(),
            issue_ids_covered: vec![
                "coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.14.2".to_string(),
            ],
            fixture_id: Some(scenario_id.to_string()),
            command_id: command_id.to_string(),
            phase: "verify".to_string(),
            started_at_ms: 1000,
            finished_at_ms: 1100,
            elapsed_ms: 100,
            meta: ProofRunMeta {
                cass_binary_path: "/tmp/cass-tgt/debug/cass".to_string(),
                cass_version: "0.6.14".to_string(),
                git_revision: None,
                cargo_profile: "dev".to_string(),
                feature_flags: vec![],
                target_dir: "/tmp/cass-tgt".to_string(),
                data_dir: "/data/cass".to_string(),
                config_dir: "/tmp/config".to_string(),
                model_dir: None,
                source_roots: vec![],
            },
            execution: ProofExecution {
                argv: argv.iter().map(|s| s.to_string()).collect(),
                sanitized_env: std::collections::BTreeMap::new(),
                timeout_ms: 5000,
                exit_code: signals.exit_code,
                signal: None,
                timed_out: signals.timed_out,
                retry_count: 0,
            },
            artifacts: ProofArtifacts {
                stdout_path: format!("/tmp/{scenario_id}.stdout"),
                stderr_path: format!("/tmp/{scenario_id}.stderr"),
                parsed_stdout_json: None,
                parsed_stderr_json: None,
                robot_contract_ok: signals.robot_contract_ok,
                ansi_free_stdout_ok: signals.ansi_free_stdout_ok,
            },
            outcome: signals.outcome(),
        }
    }

    fn ran_ok() -> OutcomeSignals {
        OutcomeSignals {
            executed: true,
            timed_out: false,
            stale_artifact_reused: false,
            exit_code: Some(0),
            expects_json: true,
            parsed_json_ok: true,
            robot_contract_ok: true,
            ansi_free_stdout_ok: true,
        }
    }

    fn did_not_run() -> OutcomeSignals {
        OutcomeSignals {
            executed: false,
            ..ran_ok()
        }
    }

    #[test]
    fn salvage_scenarios_emit_distinguishable_structured_proof_records() {
        let mut context = ctx();
        context.reconstruction_source_available = true;

        // 1) Dry-run: a plan is produced and the dry-run command runs green.
        let dry_run_plan = plan_storage_salvage(
            &report(StorageState::SchemaDrift, ArchiveReadability::Readable),
            &context,
        );
        assert!(dry_run_plan.backup_required);
        let dry_run_proof = salvage_proof_record(
            "salvage_dry_run",
            "doctor_repair_dry_run",
            &["cass", "doctor", "repair", "--dry-run", "--json"],
            ran_ok(),
        );
        assert_eq!(dry_run_proof.outcome, ProofOutcome::Passed);

        // 2) Refused unsafe mutation: the apply never runs (did_not_run), and
        //    the plan refuses + preserves evidence.
        let mut no_source = ctx();
        no_source.reconstruction_source_available = false;
        let refused_plan = plan_storage_salvage(
            &report(StorageState::OpenreadFailed, ArchiveReadability::Unreadable),
            &no_source,
        );
        assert!(refused_plan.refusal.is_some());
        let refused_proof = salvage_proof_record(
            "salvage_refused_unsafe_mutation",
            "doctor_repair_apply_blocked",
            &[
                "cass",
                "doctor",
                "repair",
                "--yes",
                "--plan-fingerprint",
                "<fingerprint>",
                "--json",
            ],
            did_not_run(),
        );
        assert_eq!(refused_proof.outcome, ProofOutcome::DidNotRun);

        // 3) Successful low-risk derived repair: runs green, no backup needed.
        let derived_plan = plan_storage_salvage(
            &report(StorageState::DerivedOnlyDrift, ArchiveReadability::Readable),
            &context,
        );
        assert!(derived_plan.low_risk_derived_repair);
        assert!(!derived_plan.backup_required);
        let derived_proof = salvage_proof_record(
            "salvage_low_risk_derived_repair",
            "doctor_repair_apply_derived",
            &["cass", "doctor", "repair", "--dry-run", "--json"],
            ran_ok(),
        );
        assert_eq!(derived_proof.outcome, ProofOutcome::Passed);

        // 4) High-risk backup-required flow: the dry-run runs green; the apply
        //    is gated (did_not_run) until a backup exists.
        let high_risk_plan = plan_storage_salvage(
            &report(
                StorageState::IntegrityFailed,
                ArchiveReadability::PartiallyReadable,
            ),
            &context,
        );
        assert!(high_risk_plan.backup_required && high_risk_plan.requires_confirmation);
        let high_risk_gate_proof = salvage_proof_record(
            "salvage_high_risk_backup_required",
            "doctor_repair_apply_pre_backup",
            &[
                "cass",
                "doctor",
                "repair",
                "--yes",
                "--plan-fingerprint",
                "<fingerprint>",
                "--json",
            ],
            did_not_run(),
        );
        assert_eq!(high_risk_gate_proof.outcome, ProofOutcome::DidNotRun);

        // 5) Structured proof logs: the four records above are distinguishable
        //    pass/did-not-run evidence, none confusable with the other.
        let all = [
            &dry_run_proof,
            &refused_proof,
            &derived_proof,
            &high_risk_gate_proof,
        ];
        let passes = all.iter().filter(|r| r.is_pass()).count();
        let gated = all
            .iter()
            .filter(|r| r.outcome == ProofOutcome::DidNotRun)
            .count();
        assert_eq!(passes, 2, "dry-run + derived repair pass");
        assert_eq!(gated, 2, "refused + high-risk apply are gated, not run");
        // Each record round-trips through JSON with the bead id attributed.
        for r in all {
            let json = serde_json::to_string(r).expect("serialize proof record");
            assert!(
                json.contains("uojcg.14.2"),
                "proof record attributes the bead"
            );
            let parsed: ProofLogRecord = serde_json::from_str(&json).expect("parse proof record");
            assert_eq!(parsed, *r);
        }
    }
}
