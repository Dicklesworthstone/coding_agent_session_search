//! Fleet-safe upgrade rehearsal and post-upgrade verification.
//!
//! Bead: coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.6.6
//! ("Add fleet-safe upgrade rehearsal and post-upgrade verification").
//!
//! The report found real fleet version skew (local/ts1/css on 0.6.13, ts2 on
//! 0.6.10, csd/mac-mini-max on 0.4.1). Detecting skew and recommending an
//! upgrade (bead `6.3`) is not enough: operators need confidence that the
//! upgrade *path itself* is bounded, archive-safe, and followed by a proof check
//! that confirms cass is actually healthier afterward — not merely running a new
//! binary against still-stale derived assets.
//!
//! This module is the pure composition layer that turns the upstream signals
//! into a tested operator journey:
//!
//! * version skew + install hints come from [`VersionAssessment`] (bead `6.3`);
//! * archive risk + source-coverage preflight come from
//!   [`ArchiveCoverageSummary`] (bead `6.4`);
//! * the bounded post-upgrade checks are driven by the shared E2E runner (bead
//!   `12.2`) and classified through [`ProofArtifact`] (bead `11.4`/`12.3`).
//!
//! What it produces:
//!
//! 1. A per-host **rehearsal plan** ([`HostUpgradeRehearsal`]) — a dry run that
//!    explains which host is old, which installer channel/command would be used,
//!    what is checked *before* any mutation, and what is explicitly **not
//!    touched** (source logs, archives, semantic models). The five distinct
//!    upgrade actions (binary upgrade, data/schema migration, model install,
//!    source sync, derived-index refresh) are separate steps each carrying their
//!    own mutation scope, backup-first gating, and proof requirement.
//! 2. A **post-upgrade verification** ([`PostUpgradeVerification`]) — bounded
//!    post-checks (api-version, health/status readiness, source coverage,
//!    quarantine, lexical/semantic fallback, human/robot parity) classified into
//!    proof artifacts, plus a before/after comparison that distinguishes a real
//!    fix from "only changed the binary". Partial failures are never hidden.
//!
//! Invariants (asserted by tests): no default path mutates source logs, deletes
//! archives, auto-downloads semantic models, or hides a partial failure. Every
//! emitted command is a concrete, non-destructive `cass` invocation referring to
//! hosts by alias only (no embedded SSH credentials).
//!
//! Like its siblings this is pure, side-effect-free logic: producers run the
//! probes and supply the assessments/coverage; this turns them into the plan and
//! verification. A later surface-wiring bead exposes it through the CLI.

use serde::{Deserialize, Serialize};

use crate::e2e_runner::RunMode;
use crate::fleet_archive_coverage::{ArchiveCoverageSummary, CoverageState};
use crate::fleet_doctor_schema::{
    ArchiveRisk, HostDoctorReport, HostOs, HostProbeStatus, ReadinessState, RemoteSyncState,
    SemanticState,
};
use crate::fleet_version_skew::{CapabilityGap, UpgradeMethod, VersionAssessment};
use crate::proof_artifact::{ProofArtifact, ProofRun, ProofStatus};

/// Stable schema version for the upgrade-rehearsal wire format.
pub const FLEET_UPGRADE_REHEARSAL_SCHEMA_VERSION: u32 = 1;

/// Whether the rehearsal is a deterministic fixture run or an opt-in live probe.
/// Fixture mode never reaches a real host; live mode is opt-in and must never be
/// mandatory for CI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RehearsalMode {
    /// Deterministic old-host fixtures; no real host contacted.
    Fixture,
    /// Opt-in live fleet upgrade probes against real hosts.
    Live,
}

impl RehearsalMode {
    /// Stable wire label.
    pub const fn as_str(self) -> &'static str {
        match self {
            RehearsalMode::Fixture => "fixture",
            RehearsalMode::Live => "live",
        }
    }

    /// The shared E2E runner mode this rehearsal drives. Fixture maps to the
    /// deterministic CI suite; live maps to the opt-in live mode.
    pub const fn run_mode(self) -> RunMode {
        match self {
            RehearsalMode::Fixture => RunMode::Ci,
            RehearsalMode::Live => RunMode::Live,
        }
    }
}

/// What a single upgrade action would mutate. Ordered low→high so a rollup can
/// take the worst scope with `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UpgradeActionScope {
    /// Read-only / wait — mutates nothing.
    None,
    /// Replaces the `cass` binary only; touches no indexed or archive data.
    Binary,
    /// Rebuilds derived assets (lexical/semantic index) from the canonical DB.
    DerivedAssets,
    /// Could touch the canonical archive / source logs (data-loss surface). Never
    /// a default recommended command — always backup-gated.
    Archive,
}

impl UpgradeActionScope {
    /// Stable wire label.
    pub const fn as_str(self) -> &'static str {
        match self {
            UpgradeActionScope::None => "none",
            UpgradeActionScope::Binary => "binary",
            UpgradeActionScope::DerivedAssets => "derived-assets",
            UpgradeActionScope::Archive => "archive",
        }
    }
}

/// The distinct upgrade actions, each carrying its own safety + proof
/// requirements. They are NOT a single "upgrade" — conflating them is exactly
/// how an operator ends up with a new binary on stale data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UpgradeAction {
    /// Replace the host's `cass` binary with the current version.
    BinaryUpgrade,
    /// Migrate the canonical DB schema after a major-version jump.
    DataSchemaMigration,
    /// Install a semantic embedding model (opt-in; cass never auto-downloads).
    ModelInstall,
    /// Sync remote source mirrors (additive-only).
    SourceSync,
    /// Rebuild derived lexical/semantic indexes from the canonical DB.
    DerivedIndexRefresh,
}

impl UpgradeAction {
    /// Stable wire label.
    pub const fn as_str(self) -> &'static str {
        match self {
            UpgradeAction::BinaryUpgrade => "binary-upgrade",
            UpgradeAction::DataSchemaMigration => "data-schema-migration",
            UpgradeAction::ModelInstall => "model-install",
            UpgradeAction::SourceSync => "source-sync",
            UpgradeAction::DerivedIndexRefresh => "derived-index-refresh",
        }
    }

    /// The mutation scope this action carries by nature.
    pub const fn scope(self) -> UpgradeActionScope {
        match self {
            UpgradeAction::BinaryUpgrade => UpgradeActionScope::Binary,
            // Schema migration rewrites the canonical store: the worst-case
            // surface, so it is always backup-gated.
            UpgradeAction::DataSchemaMigration => UpgradeActionScope::Archive,
            // Model files live beside the index; installing them touches neither
            // indexed nor archive data.
            UpgradeAction::ModelInstall => UpgradeActionScope::None,
            UpgradeAction::SourceSync => UpgradeActionScope::DerivedAssets,
            UpgradeAction::DerivedIndexRefresh => UpgradeActionScope::DerivedAssets,
        }
    }
}

/// The overall disposition of a host's upgrade journey. The primary state an
/// operator branches on before reading the per-action detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HostUpgradeDisposition {
    /// Host is already current; no upgrade needed (post-checks may still verify).
    UpToDate,
    /// Old, reachable, with a known installer path: rehearsal proceeds.
    UpgradeReady,
    /// Old, but the upgrade requires a manual installer path (self-update is
    /// unsafe from a very old binary, or the platform needs a package manager).
    NeedsManualUpgrade,
    /// Old, but high archive risk gates the data-affecting actions until a
    /// backup is taken first.
    UpgradeGatedByArchive,
    /// No supported installer/channel for this host: operator must intervene.
    InstallerUnavailable,
    /// The host could not be reached: nothing can be rehearsed now.
    Unreachable,
}

impl HostUpgradeDisposition {
    /// Stable wire label.
    pub const fn as_str(self) -> &'static str {
        match self {
            HostUpgradeDisposition::UpToDate => "up-to-date",
            HostUpgradeDisposition::UpgradeReady => "upgrade-ready",
            HostUpgradeDisposition::NeedsManualUpgrade => "needs-manual-upgrade",
            HostUpgradeDisposition::UpgradeGatedByArchive => "upgrade-gated-by-archive",
            HostUpgradeDisposition::InstallerUnavailable => "installer-unavailable",
            HostUpgradeDisposition::Unreachable => "unreachable",
        }
    }

    /// Whether this host can actually be upgraded right now (in the recommended
    /// order). Unreachable / installer-unavailable hosts cannot.
    pub const fn is_actionable_now(self) -> bool {
        matches!(
            self,
            HostUpgradeDisposition::UpgradeReady
                | HostUpgradeDisposition::NeedsManualUpgrade
                | HostUpgradeDisposition::UpgradeGatedByArchive
        )
    }
}

/// The archive-risk + source-coverage preflight that gates any data-affecting
/// upgrade or repair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpgradePreflight {
    /// Worst archive risk across the host report and its coverage summary.
    pub archive_risk: ArchiveRisk,
    /// Coverage / freshness state of the host's source roots.
    pub coverage_state: CoverageState,
    /// Whether source roots are readable and accounted for (no missing/unreadable
    /// session-bearing roots).
    pub source_coverage_ok: bool,
    /// Whether the archive is safe enough to proceed with data-affecting actions
    /// without a fresh backup first.
    pub archive_safe_to_proceed: bool,
    /// Whether a backup is required before any [`UpgradeActionScope::Archive`]
    /// action runs.
    pub backup_required: bool,
    /// Reasons the data-affecting actions are gated, if any.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocking_reasons: Vec<String>,
}

/// One step of the upgrade plan: a distinct action with its own scope, gating,
/// and proof requirement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpgradeActionStep {
    /// Which action this step is.
    pub action: UpgradeAction,
    /// Whether this action is needed for this host at all.
    pub applicable: bool,
    /// What the action mutates.
    pub mutation_scope: UpgradeActionScope,
    /// The concrete `cass` command to run, or `None` for a pure no-op.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Whether the action is opt-in only and must never run by default (model
    /// install: cass never auto-downloads).
    pub opt_in: bool,
    /// Whether explicit operator confirmation is required before running.
    pub requires_confirmation: bool,
    /// Whether a backup must be taken before this action runs.
    pub backup_required: bool,
    /// Whether this action is blocked in the current state (e.g. pending backup).
    pub blocked: bool,
    /// Why the action is blocked, when it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_reason: Option<String>,
    /// The precondition that unblocks the action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unblock_precondition: Option<String>,
    /// Why this action is part of the plan.
    pub why: String,
    /// The post-check that proves this action did its job.
    pub proof_check: PostUpgradeCheck,
}

/// A command that is blocked in the current state, kept separate from the safe
/// next commands so it can never be run until its precondition is met.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockedUpgradeCommand {
    /// The blocked command.
    pub command: String,
    /// Why it is blocked.
    pub why_blocked: String,
    /// The precondition that unblocks it.
    pub unblock_precondition: String,
}

/// The bounded post-upgrade checks. Each verifies one facet that an upgrade is
/// supposed to improve (or at least not regress).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PostUpgradeCheck {
    /// `cass api-version --json` — contract version is current.
    ApiVersion,
    /// `cass health --json` / `cass status --json` — readiness recovered.
    HealthStatusReadiness,
    /// Source roots are still covered (no archive lost in the upgrade).
    SourceCoverage,
    /// Quarantine state did not regress.
    QuarantineStatus,
    /// Lexical/semantic fallback state is truthful.
    LexicalSemanticFallback,
    /// The human summary matches the robot JSON (no parity drift).
    HumanRobotParity,
}

impl PostUpgradeCheck {
    /// Stable wire label.
    pub const fn as_str(self) -> &'static str {
        match self {
            PostUpgradeCheck::ApiVersion => "api-version",
            PostUpgradeCheck::HealthStatusReadiness => "health-status-readiness",
            PostUpgradeCheck::SourceCoverage => "source-coverage",
            PostUpgradeCheck::QuarantineStatus => "quarantine-status",
            PostUpgradeCheck::LexicalSemanticFallback => "lexical-semantic-fallback",
            PostUpgradeCheck::HumanRobotParity => "human-robot-parity",
        }
    }

    /// The concrete bounded `cass` command this check runs.
    pub const fn command(self) -> &'static str {
        match self {
            PostUpgradeCheck::ApiVersion => "cass api-version --json",
            PostUpgradeCheck::HealthStatusReadiness => "cass health --json",
            PostUpgradeCheck::SourceCoverage => "cass status --json",
            PostUpgradeCheck::QuarantineStatus => "cass diag --json --quarantine",
            PostUpgradeCheck::LexicalSemanticFallback => "cass status --json",
            PostUpgradeCheck::HumanRobotParity => "cass status --json",
        }
    }

    /// The bounded timeout (ms) for this check. All comfortably under any host
    /// time budget so post-checks can never wedge the rehearsal.
    pub const fn timeout_ms(self) -> u64 {
        match self {
            PostUpgradeCheck::ApiVersion => 5_000,
            PostUpgradeCheck::HealthStatusReadiness => 5_000,
            PostUpgradeCheck::SourceCoverage => 10_000,
            PostUpgradeCheck::QuarantineStatus => 10_000,
            PostUpgradeCheck::LexicalSemanticFallback => 5_000,
            PostUpgradeCheck::HumanRobotParity => 10_000,
        }
    }

    /// The full bounded post-check battery, in run order.
    pub const fn battery() -> [PostUpgradeCheck; 6] {
        [
            PostUpgradeCheck::ApiVersion,
            PostUpgradeCheck::HealthStatusReadiness,
            PostUpgradeCheck::SourceCoverage,
            PostUpgradeCheck::QuarantineStatus,
            PostUpgradeCheck::LexicalSemanticFallback,
            PostUpgradeCheck::HumanRobotParity,
        ]
    }
}

/// The bounded spec for one post-upgrade check: the command, its timeout, and
/// whether it is required for the verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostUpgradeCheckSpec {
    /// Which check this spec drives.
    pub check: PostUpgradeCheck,
    /// The bounded `cass` command to run.
    pub command: String,
    /// Hard timeout in milliseconds.
    pub timeout_ms: u64,
    /// Whether the check must pass for the upgrade to count as verified.
    pub required: bool,
}

impl PostUpgradeCheckSpec {
    fn for_check(check: PostUpgradeCheck) -> Self {
        // Every facet is required: a "verified" upgrade must clear all of them.
        Self {
            check,
            command: check.command().to_string(),
            timeout_ms: check.timeout_ms(),
            required: true,
        }
    }

    /// The default bounded battery of post-upgrade check specs.
    pub fn battery() -> Vec<PostUpgradeCheckSpec> {
        PostUpgradeCheck::battery()
            .into_iter()
            .map(PostUpgradeCheckSpec::for_check)
            .collect()
    }
}

/// The per-host upgrade rehearsal (dry-run) plan. Serializes with stable
/// snake_case fields and an embedded [`schema_version`](Self::schema_version).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostUpgradeRehearsal {
    /// Mirrors [`FLEET_UPGRADE_REHEARSAL_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Host identity (alias only — never an SSH credential).
    pub host_alias: String,
    /// The host's upgrade disposition.
    pub disposition: HostUpgradeDisposition,
    /// Version the host reported, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_version: Option<String>,
    /// The version the fleet is converging to.
    pub target_version: String,
    /// Capability gap classification (from bead 6.3).
    pub capability_gap: CapabilityGap,
    /// Whether the binary must be upgraded before any repair is attempted.
    pub upgrade_before_repair: bool,
    /// Resolved install method.
    pub install_method: UpgradeMethod,
    /// The single best install/upgrade command, when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_command: Option<String>,
    /// The upgrade channel label (e.g. `"self-update"`, `"homebrew"`).
    pub channel: String,
    /// Archive-risk + source-coverage preflight that gates mutation.
    pub preflight: UpgradePreflight,
    /// The distinct upgrade actions, each separately gated.
    pub actions: Vec<UpgradeActionStep>,
    /// The checks performed BEFORE any mutation (the preflight, made explicit).
    pub will_check_before_mutation: Vec<String>,
    /// What this rehearsal explicitly does NOT touch.
    pub not_touched: Vec<String>,
    /// The bounded post-upgrade checks to run afterward.
    pub post_checks: Vec<PostUpgradeCheckSpec>,
    /// Safe next commands an operator may run now.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub safe_next_commands: Vec<String>,
    /// Commands blocked until their precondition is met.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_next_commands: Vec<BlockedUpgradeCommand>,
    /// Whether this is a fixture or live rehearsal.
    pub rehearsal_mode: RehearsalMode,
    /// Live-only steps that were skipped in fixture mode (never silently
    /// omitted: they are named here).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub live_steps_skipped: Vec<String>,
    /// What happens if the operator aborts mid-upgrade.
    pub abort_behavior: String,
    /// Whether any emitted field needed redaction (always recorded so the
    /// no-credential-leak invariant is auditable).
    pub redaction_applied: bool,
}

/// Before/after facts captured around an upgrade, so an operator can tell a real
/// fix from a binary swap on stale data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpgradeBeforeAfter {
    /// Version before the upgrade.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_version: Option<String>,
    /// Version after the upgrade.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_version: Option<String>,
    /// Whether the binary version actually changed (and reached target).
    pub binary_upgraded: bool,
    /// Readiness before the upgrade.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_readiness: Option<ReadinessState>,
    /// Readiness after the upgrade.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_readiness: Option<ReadinessState>,
    /// Whether readiness improved (or was already ready).
    pub readiness_improved: bool,
    /// Whether derived assets remain stale after the binary upgrade — the
    /// "new binary on old index" trap.
    pub derived_assets_still_stale: bool,
    /// Coverage state before.
    pub coverage_before: CoverageState,
    /// Coverage state after.
    pub coverage_after: CoverageState,
}

impl UpgradeBeforeAfter {
    /// Whether readiness ended in a healthy state (improved to, or stayed at,
    /// ready).
    pub fn ended_ready(&self) -> bool {
        matches!(self.after_readiness, Some(ReadinessState::Ready))
    }
}

/// The post-upgrade verification: bounded check proofs + before/after, rolled up
/// into one trustworthy verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostUpgradeVerification {
    /// Mirrors [`FLEET_UPGRADE_REHEARSAL_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Host identity (alias only).
    pub host_alias: String,
    /// Before/after comparison.
    pub before_after: UpgradeBeforeAfter,
    /// One classified proof artifact per post-check.
    pub check_proofs: Vec<PostUpgradeCheckProof>,
    /// Worst status across all check proofs (the rollup `max`).
    pub overall_status: ProofStatus,
    /// Whether the upgrade truly fixed the host, not merely changed the binary.
    pub upgrade_truly_fixed: bool,
    /// Partial failures, surfaced explicitly (never hidden).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub partial_failures: Vec<String>,
    /// One-line human summary (facts live in the structured fields).
    pub summary: String,
}

/// A post-upgrade check paired with its classified proof artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostUpgradeCheckProof {
    /// Which facet this proves.
    pub check: PostUpgradeCheck,
    /// The classified proof artifact for the check run.
    pub proof: ProofArtifact,
}

/// Per-host inputs for [`rehearse_fleet`]: the probe report plus the upstream
/// version assessment (bead 6.3) and archive coverage (bead 6.4).
pub struct HostRehearsalInput<'a> {
    /// The host probe report.
    pub report: &'a HostDoctorReport,
    /// The version-skew assessment for this host.
    pub assessment: &'a VersionAssessment,
    /// The archive-coverage summary for this host.
    pub coverage: &'a ArchiveCoverageSummary,
}

/// A fleet-wide upgrade rehearsal: every host's plan plus an orchestration
/// rollup. Serializes with stable snake_case fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetUpgradeRehearsal {
    /// Mirrors [`FLEET_UPGRADE_REHEARSAL_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// The version the fleet is converging to.
    pub target_version: String,
    /// Fixture or live rehearsal.
    pub rehearsal_mode: RehearsalMode,
    /// Per-host rehearsal plans.
    pub hosts: Vec<HostUpgradeRehearsal>,
    /// Hosts already at the target version.
    pub hosts_up_to_date: u64,
    /// Hosts that need an upgrade (any non-current, actionable or not).
    pub hosts_needing_upgrade: u64,
    /// Hosts that could not be reached.
    pub hosts_unreachable: u64,
    /// Hosts whose data-affecting actions are gated by archive risk.
    pub hosts_gated_by_archive: u64,
    /// Hosts with no supported installer/channel.
    pub hosts_installer_unavailable: u64,
    /// Recommended upgrade order (host aliases), lowest-risk first; only
    /// actionable hosts appear.
    pub recommended_order: Vec<String>,
    /// Highest archive risk across the fleet (a `max` rollup).
    pub highest_archive_risk: ArchiveRisk,
    /// Whether any host has a blocked action.
    pub any_blocked: bool,
}

/// Resolve the upgrade channel label from the install command / method.
fn channel_label(method: UpgradeMethod, command: Option<&str>, os: HostOs) -> String {
    match method {
        UpgradeMethod::SelfUpdate => "self-update".to_string(),
        UpgradeMethod::Unsupported => "unsupported".to_string(),
        UpgradeMethod::ManualInstaller => match command {
            Some(cmd) if cmd.contains("brew") => "homebrew".to_string(),
            Some(cmd) if cmd.contains("scoop") => "scoop".to_string(),
            Some(cmd) if cmd.contains("install.sh") => "installer-script".to_string(),
            _ => match os {
                HostOs::MacOs => "homebrew".to_string(),
                HostOs::Windows => "scoop".to_string(),
                HostOs::Linux => "installer-script".to_string(),
                HostOs::Other => "manual".to_string(),
            },
        },
    }
}

/// Whether the source coverage is intact (no missing/unreadable session-bearing
/// roots) per the coverage summary's provenance gaps.
fn source_coverage_ok(coverage: &ArchiveCoverageSummary) -> bool {
    use crate::fleet_archive_coverage::ProvenanceGapKind as G;
    !coverage
        .provenance_gaps
        .iter()
        .any(|g| matches!(g.kind, G::MissingRoot | G::UnreadableRoot))
}

/// Build the archive/coverage preflight, taking the worse of the report's and
/// the coverage summary's archive risk.
fn build_preflight(
    report: &HostDoctorReport,
    coverage: &ArchiveCoverageSummary,
) -> UpgradePreflight {
    let archive_risk = report.archive_risk.max(coverage.archive_risk);
    let coverage_ok = source_coverage_ok(coverage);
    let high_risk = matches!(archive_risk, ArchiveRisk::High);
    let archive_safe = !high_risk && coverage_ok;
    let mut blocking_reasons = Vec::new();
    if high_risk {
        blocking_reasons.push(
            "archive risk is high: back up the canonical store before any data-affecting action"
                .to_string(),
        );
    }
    if !coverage_ok {
        blocking_reasons.push(
            "source roots are missing or unreadable: confirm coverage before mutating".to_string(),
        );
    }
    UpgradePreflight {
        archive_risk,
        coverage_state: coverage.coverage_state,
        source_coverage_ok: coverage_ok,
        archive_safe_to_proceed: archive_safe,
        backup_required: high_risk,
        blocking_reasons,
    }
}

/// Decide whether a single action is applicable for this host, and its `why`.
fn action_applicability(
    action: UpgradeAction,
    assessment: &VersionAssessment,
    report: &HostDoctorReport,
    coverage: &ArchiveCoverageSummary,
) -> (bool, String) {
    match action {
        UpgradeAction::BinaryUpgrade => (
            assessment.upgrade_needed,
            "binary is behind the fleet target version".to_string(),
        ),
        UpgradeAction::DataSchemaMigration => (
            // Major jumps may carry schema changes; conservatively plan (and
            // gate) the migration.
            matches!(assessment.capability_gap, CapabilityGap::Major),
            "major version jump may require a canonical schema migration".to_string(),
        ),
        UpgradeAction::ModelInstall => (
            matches!(report.semantic, Some(SemanticState::AssetsMissing)),
            "semantic model assets are missing (opt-in install; never auto-downloaded)".to_string(),
        ),
        UpgradeAction::SourceSync => (
            matches!(
                report.remote_sync,
                Some(RemoteSyncState::Stale) | Some(RemoteSyncState::NeverSynced)
            ),
            "remote source mirror is stale or never synced".to_string(),
        ),
        UpgradeAction::DerivedIndexRefresh => (
            matches!(
                coverage.coverage_state,
                CoverageState::MissingDerivedAssets
                    | CoverageState::Stale
                    | CoverageState::RemoteCopyAhead
            ),
            "derived index is missing or stale relative to the canonical store".to_string(),
        ),
    }
}

/// The concrete command for an action.
fn action_command(action: UpgradeAction, install_command: Option<&str>) -> Option<String> {
    match action {
        UpgradeAction::BinaryUpgrade => install_command.map(str::to_string),
        // doctor handles safe migrations/rebuilds; the mutating apply is gated by
        // a backup, so the dry-run inspection command is what we surface here.
        UpgradeAction::DataSchemaMigration => Some("cass doctor --json".to_string()),
        UpgradeAction::ModelInstall => Some("cass models install --model minilm".to_string()),
        UpgradeAction::SourceSync => Some("cass sources sync --all --json".to_string()),
        UpgradeAction::DerivedIndexRefresh => Some("cass index --full".to_string()),
    }
}

/// Build the [`UpgradeActionStep`] for one action, applying preflight gating.
fn build_action_step(
    action: UpgradeAction,
    assessment: &VersionAssessment,
    report: &HostDoctorReport,
    coverage: &ArchiveCoverageSummary,
    preflight: &UpgradePreflight,
    install_command: Option<&str>,
) -> UpgradeActionStep {
    let (applicable, why) = action_applicability(action, assessment, report, coverage);
    let scope = action.scope();
    let opt_in = matches!(action, UpgradeAction::ModelInstall);
    // An Archive-scope action always needs a backup; everything else inherits the
    // preflight's backup requirement only when it is data-affecting.
    let backup_required = matches!(scope, UpgradeActionScope::Archive)
        || (preflight.backup_required && matches!(scope, UpgradeActionScope::DerivedAssets));
    // Confirmation: opt-in actions and archive-scope mutations always need it.
    let requires_confirmation = opt_in || matches!(scope, UpgradeActionScope::Archive);
    // Blocked when applicable but its safety precondition is not yet met:
    //   * opt-in actions are blocked until the operator opts in;
    //   * backup-required actions are blocked until the backup exists.
    let (blocked, block_reason, unblock_precondition) = if !applicable {
        (false, None, None)
    } else if opt_in {
        (
            true,
            Some("opt-in only: cass never auto-downloads semantic models".to_string()),
            Some("operator explicitly requests the model install".to_string()),
        )
    } else if backup_required {
        (
            true,
            Some("data-affecting action requires a fresh backup first".to_string()),
            Some("a verified backup of the canonical store exists".to_string()),
        )
    } else {
        (false, None, None)
    };
    UpgradeActionStep {
        action,
        applicable,
        mutation_scope: scope,
        command: action_command(action, install_command),
        opt_in,
        requires_confirmation,
        backup_required,
        blocked,
        block_reason,
        unblock_precondition,
        why,
        proof_check: action_proof_check(action),
    }
}

/// The post-check that proves an action did its job.
fn action_proof_check(action: UpgradeAction) -> PostUpgradeCheck {
    match action {
        UpgradeAction::BinaryUpgrade => PostUpgradeCheck::ApiVersion,
        UpgradeAction::DataSchemaMigration => PostUpgradeCheck::HealthStatusReadiness,
        UpgradeAction::ModelInstall => PostUpgradeCheck::LexicalSemanticFallback,
        UpgradeAction::SourceSync => PostUpgradeCheck::SourceCoverage,
        UpgradeAction::DerivedIndexRefresh => PostUpgradeCheck::HealthStatusReadiness,
    }
}

/// The standard "not touched" attestation: the surfaces a safe upgrade never
/// mutates by default.
fn not_touched_list() -> Vec<String> {
    vec![
        "provider session logs (source of truth) are read-only".to_string(),
        "canonical archive DB is never deleted".to_string(),
        "semantic models are never auto-downloaded".to_string(),
        "derived index is only rebuilt by an explicit, named refresh step".to_string(),
        "remote hosts are never mutated while classifying".to_string(),
    ]
}

/// The explicit pre-mutation checks (the preflight, surfaced for the operator).
fn pre_mutation_checks(preflight: &UpgradePreflight) -> Vec<String> {
    let mut checks = vec![
        format!("archive risk assessed: {}", preflight.archive_risk_label()),
        format!("source coverage ok: {}", preflight.source_coverage_ok),
        format!(
            "coverage state: {}",
            coverage_state_label(preflight.coverage_state)
        ),
        "binary reachability confirmed before any install".to_string(),
    ];
    if preflight.backup_required {
        checks.push("backup of canonical store required before data-affecting actions".to_string());
    }
    checks
}

impl UpgradePreflight {
    fn archive_risk_label(&self) -> &'static str {
        archive_risk_label(self.archive_risk)
    }
}

/// Stable label for an archive-risk level.
fn archive_risk_label(risk: ArchiveRisk) -> &'static str {
    match risk {
        ArchiveRisk::Unknown => "unknown",
        ArchiveRisk::Low => "low",
        ArchiveRisk::Medium => "medium",
        ArchiveRisk::High => "high",
    }
}

/// Stable label for a coverage state.
fn coverage_state_label(state: CoverageState) -> &'static str {
    match state {
        CoverageState::Fresh => "fresh",
        CoverageState::MissingDerivedAssets => "missing-derived-assets",
        CoverageState::SourcePruned => "source-pruned",
        CoverageState::LocalArchiveAhead => "local-archive-ahead",
        CoverageState::RemoteCopyAhead => "remote-copy-ahead",
        CoverageState::Stale => "stale",
        CoverageState::Unknown => "unknown",
    }
}

/// Classify a host's overall upgrade disposition from the assessment + preflight.
fn classify_disposition(
    report: &HostDoctorReport,
    assessment: &VersionAssessment,
    preflight: &UpgradePreflight,
) -> HostUpgradeDisposition {
    // Unreachable hosts yield no deep state — nothing can be rehearsed now.
    if report.unreachable || matches!(report.status, HostProbeStatus::Unreachable) {
        return HostUpgradeDisposition::Unreachable;
    }
    if !assessment.upgrade_needed {
        return HostUpgradeDisposition::UpToDate;
    }
    // No supported installer/channel: a failed installer probe lands here.
    if matches!(assessment.install_hint.method, UpgradeMethod::Unsupported)
        || assessment.install_hint.command.is_none()
    {
        return HostUpgradeDisposition::InstallerUnavailable;
    }
    // High archive risk gates the data-affecting actions until a backup exists.
    if preflight.backup_required {
        return HostUpgradeDisposition::UpgradeGatedByArchive;
    }
    // An old binary may lack the repair surfaces; force the manual installer path.
    if assessment.upgrade_before_repair {
        return HostUpgradeDisposition::NeedsManualUpgrade;
    }
    HostUpgradeDisposition::UpgradeReady
}

/// Build a single host's upgrade rehearsal plan from its probe report, version
/// assessment (bead 6.3), and archive coverage (bead 6.4).
pub fn rehearse_host(
    report: &HostDoctorReport,
    assessment: &VersionAssessment,
    coverage: &ArchiveCoverageSummary,
    mode: RehearsalMode,
) -> HostUpgradeRehearsal {
    let preflight = build_preflight(report, coverage);
    let disposition = classify_disposition(report, assessment, &preflight);
    let install_command = assessment.install_hint.command.clone();
    let channel = channel_label(
        assessment.install_hint.method,
        install_command.as_deref(),
        report.platform.os,
    );

    // Per-action steps: only build them when the host is reachable. An
    // unreachable host has no actionable plan (only a retry).
    let reachable = !matches!(disposition, HostUpgradeDisposition::Unreachable);
    let actions: Vec<UpgradeActionStep> = if reachable {
        [
            UpgradeAction::BinaryUpgrade,
            UpgradeAction::DataSchemaMigration,
            UpgradeAction::ModelInstall,
            UpgradeAction::SourceSync,
            UpgradeAction::DerivedIndexRefresh,
        ]
        .into_iter()
        .map(|action| {
            build_action_step(
                action,
                assessment,
                report,
                coverage,
                &preflight,
                install_command.as_deref(),
            )
        })
        .collect()
    } else {
        Vec::new()
    };

    // Safe vs blocked next commands, kept strictly separate.
    let safe_next_commands = build_safe_commands(&disposition, &actions);
    let blocked_next_commands = build_blocked_commands(&actions);

    // Live-only steps are named when skipped in fixture mode, never silently
    // dropped.
    let live_steps_skipped = if matches!(mode, RehearsalMode::Fixture) && reachable {
        vec![
            "live binary install on the remote host".to_string(),
            "live remote source sync".to_string(),
            "live post-upgrade probe against the real host".to_string(),
        ]
    } else {
        Vec::new()
    };

    HostUpgradeRehearsal {
        schema_version: FLEET_UPGRADE_REHEARSAL_SCHEMA_VERSION,
        host_alias: report.host_alias.clone(),
        disposition,
        observed_version: assessment.observed_version.clone(),
        target_version: assessment.current_repo_version.clone(),
        capability_gap: assessment.capability_gap,
        upgrade_before_repair: assessment.upgrade_before_repair,
        install_method: assessment.install_hint.method,
        install_command,
        channel,
        will_check_before_mutation: pre_mutation_checks(&preflight),
        not_touched: not_touched_list(),
        post_checks: PostUpgradeCheckSpec::battery(),
        safe_next_commands,
        blocked_next_commands,
        rehearsal_mode: mode,
        live_steps_skipped,
        abort_behavior:
            "aborting before a mutating step leaves the host unchanged; a binary swap is reversible by reinstalling the prior version, and no archive or source log is touched"
                .to_string(),
        redaction_applied: false,
        preflight,
        actions,
    }
}

/// Build the safe (unblocked, runnable-now) next commands for a host.
fn build_safe_commands(
    disposition: &HostUpgradeDisposition,
    actions: &[UpgradeActionStep],
) -> Vec<String> {
    match disposition {
        HostUpgradeDisposition::Unreachable => {
            vec!["cass doctor --check --json   # retry the bounded probe".to_string()]
        }
        HostUpgradeDisposition::InstallerUnavailable => {
            vec!["cass api-version --json   # confirm the contract gap".to_string()]
        }
        HostUpgradeDisposition::UpToDate => {
            vec!["cass health --json   # already current; verify readiness".to_string()]
        }
        _ => actions
            .iter()
            .filter(|step| step.applicable && !step.blocked)
            .filter_map(|step| step.command.clone())
            .collect(),
    }
}

/// Build the blocked next commands (those gated until a precondition is met).
fn build_blocked_commands(actions: &[UpgradeActionStep]) -> Vec<BlockedUpgradeCommand> {
    actions
        .iter()
        .filter(|step| step.applicable && step.blocked)
        .filter_map(|step| {
            let command = step.command.clone()?;
            Some(BlockedUpgradeCommand {
                command,
                why_blocked: step
                    .block_reason
                    .clone()
                    .unwrap_or_else(|| "blocked by an unmet precondition".to_string()),
                unblock_precondition: step
                    .unblock_precondition
                    .clone()
                    .unwrap_or_else(|| "precondition satisfied".to_string()),
            })
        })
        .collect()
}

/// Risk-ordering key for the recommended upgrade order (lower = upgrade first).
fn order_key(disposition: HostUpgradeDisposition) -> u8 {
    match disposition {
        // Self-update-ready hosts are the safest, do them first.
        HostUpgradeDisposition::UpgradeReady => 0,
        // Manual installer next.
        HostUpgradeDisposition::NeedsManualUpgrade => 1,
        // Archive-gated hosts last among the actionable ones (need a backup).
        HostUpgradeDisposition::UpgradeGatedByArchive => 2,
        // Non-actionable dispositions are excluded from the order entirely.
        _ => u8::MAX,
    }
}

/// Build a fleet-wide rehearsal from per-host inputs.
pub fn rehearse_fleet(
    inputs: &[HostRehearsalInput<'_>],
    target_version: &str,
    mode: RehearsalMode,
) -> FleetUpgradeRehearsal {
    let hosts: Vec<HostUpgradeRehearsal> = inputs
        .iter()
        .map(|input| rehearse_host(input.report, input.assessment, input.coverage, mode))
        .collect();

    let mut hosts_up_to_date = 0u64;
    let mut hosts_needing_upgrade = 0u64;
    let mut hosts_unreachable = 0u64;
    let mut hosts_gated_by_archive = 0u64;
    let mut hosts_installer_unavailable = 0u64;
    let mut highest_archive_risk = ArchiveRisk::Unknown;
    let mut any_blocked = false;

    for host in &hosts {
        match host.disposition {
            HostUpgradeDisposition::UpToDate => hosts_up_to_date += 1,
            HostUpgradeDisposition::Unreachable => hosts_unreachable += 1,
            HostUpgradeDisposition::UpgradeGatedByArchive => {
                hosts_gated_by_archive += 1;
                hosts_needing_upgrade += 1;
            }
            HostUpgradeDisposition::InstallerUnavailable => {
                hosts_installer_unavailable += 1;
                hosts_needing_upgrade += 1;
            }
            HostUpgradeDisposition::UpgradeReady | HostUpgradeDisposition::NeedsManualUpgrade => {
                hosts_needing_upgrade += 1
            }
        }
        highest_archive_risk = highest_archive_risk.max(host.preflight.archive_risk);
        if !host.blocked_next_commands.is_empty() {
            any_blocked = true;
        }
    }

    // Recommended order: only actionable hosts, lowest-risk first, ties broken by
    // alias for determinism.
    let mut orderable: Vec<&HostUpgradeRehearsal> = hosts
        .iter()
        .filter(|h| h.disposition.is_actionable_now())
        .collect();
    orderable.sort_by(|a, b| {
        order_key(a.disposition)
            .cmp(&order_key(b.disposition))
            .then_with(|| a.host_alias.cmp(&b.host_alias))
    });
    let recommended_order: Vec<String> = orderable
        .into_iter()
        .map(|h| h.host_alias.clone())
        .collect();

    FleetUpgradeRehearsal {
        schema_version: FLEET_UPGRADE_REHEARSAL_SCHEMA_VERSION,
        target_version: target_version.to_string(),
        rehearsal_mode: mode,
        hosts,
        hosts_up_to_date,
        hosts_needing_upgrade,
        hosts_unreachable,
        hosts_gated_by_archive,
        hosts_installer_unavailable,
        recommended_order,
        highest_archive_risk,
        any_blocked,
    }
}

/// Verify an upgrade after the fact: classify each bounded post-check run into a
/// proof artifact, roll up the worst status, and decide whether the upgrade
/// *truly* fixed the host (not merely changed the binary). Partial failures are
/// always surfaced. Pure: pass in the recorded [`ProofRun`]s and `stale_after_ms`.
pub fn verify_post_upgrade(
    host_alias: &str,
    before_after: UpgradeBeforeAfter,
    checks: Vec<(PostUpgradeCheck, ProofRun)>,
    stale_after_ms: u64,
) -> PostUpgradeVerification {
    let check_proofs: Vec<PostUpgradeCheckProof> = checks
        .into_iter()
        .map(|(check, run)| PostUpgradeCheckProof {
            check,
            proof: ProofArtifact::from_run_with_window(run, stale_after_ms),
        })
        .collect();

    // Rollup: the worst (max) status across all checks. With no checks the run
    // proved nothing, so treat it as generated-only rather than a silent pass.
    let overall_status = check_proofs
        .iter()
        .map(|cp| cp.proof.status)
        .max()
        .unwrap_or(ProofStatus::GeneratedOnly);

    // Partial failures: any check that did not yield a trustworthy pass, plus the
    // stale-derived-assets trap, named explicitly.
    let mut partial_failures: Vec<String> = check_proofs
        .iter()
        .filter(|cp| !cp.proof.is_trustworthy_pass())
        .map(|cp| format!("{}: {}", cp.check.as_str(), cp.proof.status.as_str()))
        .collect();
    if before_after.derived_assets_still_stale {
        partial_failures.push(
            "derived-index-refresh: derived assets remain stale after the binary upgrade"
                .to_string(),
        );
    }

    // "Truly fixed" requires: all checks a trustworthy pass, readiness healthy
    // (improved or already ready), and no leftover stale derived assets. A binary
    // swap that leaves the index stale is NOT a fix.
    let all_pass = overall_status.is_trustworthy_pass();
    let readiness_ok = before_after.readiness_improved || before_after.ended_ready();
    let upgrade_truly_fixed = all_pass && readiness_ok && !before_after.derived_assets_still_stale;

    let summary = build_verification_summary(
        host_alias,
        &before_after,
        overall_status,
        upgrade_truly_fixed,
        partial_failures.len(),
    );

    PostUpgradeVerification {
        schema_version: FLEET_UPGRADE_REHEARSAL_SCHEMA_VERSION,
        host_alias: host_alias.to_string(),
        before_after,
        check_proofs,
        overall_status,
        upgrade_truly_fixed,
        partial_failures,
        summary,
    }
}

/// Build the one-line verification summary.
fn build_verification_summary(
    host_alias: &str,
    ba: &UpgradeBeforeAfter,
    overall: ProofStatus,
    truly_fixed: bool,
    partial_count: usize,
) -> String {
    let from = ba.before_version.as_deref().unwrap_or("unknown");
    let to = ba.after_version.as_deref().unwrap_or("unknown");
    if truly_fixed {
        format!("{host_alias}: upgrade {from} -> {to} verified (readiness improved, checks pass)")
    } else if ba.binary_upgraded {
        format!(
            "{host_alias}: binary upgraded {from} -> {to} but NOT fully fixed ({} unresolved, overall={})",
            partial_count,
            overall.as_str()
        )
    } else {
        format!(
            "{host_alias}: upgrade not verified (overall={}, {} unresolved)",
            overall.as_str(),
            partial_count
        )
    }
}

/// Whether a string carries an SSH credential leak (a `user@host` token). Used by
/// tests to prove the no-credential-leak invariant; commands here use aliases
/// only, so this should never fire on emitted fields.
#[cfg(test)]
fn contains_credential_leak(s: &str) -> bool {
    s.split_whitespace().any(|tok| {
        tok.contains('@') && {
            let mut parts = tok.splitn(2, '@');
            let user = parts.next().unwrap_or("");
            let host = parts.next().unwrap_or("");
            !user.is_empty() && host.contains('.')
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet_archive_coverage::{ProvenanceGap, ProvenanceGapKind};
    use crate::fleet_doctor_schema::{PathStyle, Platform};
    use crate::fleet_version_skew::assess_host;

    const TARGET: &str = "0.6.13";

    fn linux_host(alias: &str, version: Option<&str>, status: HostProbeStatus) -> HostDoctorReport {
        let mut h = HostDoctorReport::skeleton(alias, Platform::linux_x86_64(), status, 50);
        h.cass_version = version.map(str::to_string);
        h
    }

    fn macos_host(alias: &str, version: Option<&str>) -> HostDoctorReport {
        let platform = Platform {
            os: HostOs::MacOs,
            arch: "aarch64".to_string(),
            path_style: PathStyle::Posix,
            tool_notes: vec![],
        };
        let mut h = HostDoctorReport::skeleton(alias, platform, HostProbeStatus::Ok, 60);
        h.cass_version = version.map(str::to_string);
        h
    }

    fn coverage_with(state: CoverageState, risk: ArchiveRisk) -> ArchiveCoverageSummary {
        ArchiveCoverageSummary {
            schema_version: crate::fleet_archive_coverage::ARCHIVE_COVERAGE_SCHEMA_VERSION,
            root_kind_counts: Default::default(),
            total_estimated_sessions: 100,
            total_estimated_bytes: 4096,
            approximate: false,
            newest_index_ms: Some(1_000),
            newest_sync_ms: Some(1_000),
            provenance_gaps: Vec::new(),
            coverage_state: state,
            archive_risk: risk,
        }
    }

    fn fresh_coverage() -> ArchiveCoverageSummary {
        coverage_with(CoverageState::Fresh, ArchiveRisk::Low)
    }

    // --- Acceptance scenario 1: old but reachable host -------------------------

    #[test]
    fn old_reachable_host_is_upgrade_ready_with_self_update() {
        // ts2 ran 0.6.10 vs target 0.6.13 — a minor gap, self-update path.
        let host = linux_host("ts2", Some("0.6.10"), HostProbeStatus::Ok);
        let assessment = assess_host(&host, TARGET);
        let plan = rehearse_host(
            &host,
            &assessment,
            &fresh_coverage(),
            RehearsalMode::Fixture,
        );

        assert_eq!(plan.disposition, HostUpgradeDisposition::UpgradeReady);
        assert_eq!(plan.channel, "self-update");
        assert_eq!(plan.install_command.as_deref(), Some("cass self-update"));
        // Binary upgrade is applicable and NOT blocked (low archive risk).
        let binary = plan
            .actions
            .iter()
            .find(|a| matches!(a.action, UpgradeAction::BinaryUpgrade))
            .expect("binary action present");
        assert!(binary.applicable);
        assert!(!binary.blocked);
        assert_eq!(binary.mutation_scope, UpgradeActionScope::Binary);
        // The install command is among the safe next commands.
        assert!(
            plan.safe_next_commands
                .iter()
                .any(|c| c.contains("self-update"))
        );
        // Fixture mode names the skipped live steps rather than hiding them.
        assert!(!plan.live_steps_skipped.is_empty());
    }

    // --- Acceptance scenario 2: unreachable host -------------------------------

    #[test]
    fn unreachable_host_yields_no_mutating_plan_only_a_retry() {
        let host = linux_host("gone", None, HostProbeStatus::Unreachable);
        let mut host = host;
        host.unreachable = true;
        let assessment = assess_host(&host, TARGET);
        let plan = rehearse_host(
            &host,
            &assessment,
            &fresh_coverage(),
            RehearsalMode::Fixture,
        );

        assert_eq!(plan.disposition, HostUpgradeDisposition::Unreachable);
        // No per-action steps and no blocked mutations — only a bounded retry.
        assert!(plan.actions.is_empty());
        assert!(plan.blocked_next_commands.is_empty());
        assert!(
            plan.safe_next_commands
                .iter()
                .any(|c| c.contains("doctor --check"))
        );
        // Live steps are not "skipped" because there was nothing reachable to do.
        assert!(plan.live_steps_skipped.is_empty());
    }

    // --- Acceptance scenario 3: high archive-risk host -------------------------

    #[test]
    fn high_archive_risk_host_gates_data_actions_behind_a_backup() {
        let host = linux_host("csd", Some("0.6.10"), HostProbeStatus::Ok);
        let assessment = assess_host(&host, TARGET);
        let coverage = coverage_with(CoverageState::Stale, ArchiveRisk::High);
        let plan = rehearse_host(&host, &assessment, &coverage, RehearsalMode::Fixture);

        assert_eq!(
            plan.disposition,
            HostUpgradeDisposition::UpgradeGatedByArchive
        );
        assert!(plan.preflight.backup_required);
        assert_eq!(plan.preflight.archive_risk, ArchiveRisk::High);
        assert!(!plan.preflight.archive_safe_to_proceed);
        assert!(!plan.preflight.blocking_reasons.is_empty());

        // The derived-index refresh (data-affecting) is blocked pending a backup.
        let refresh = plan
            .actions
            .iter()
            .find(|a| matches!(a.action, UpgradeAction::DerivedIndexRefresh))
            .expect("refresh action present");
        assert!(
            refresh.applicable,
            "stale coverage makes refresh applicable"
        );
        assert!(refresh.blocked);
        assert!(refresh.backup_required);
        // The blocked command is surfaced separately, never among the safe set.
        assert!(
            plan.blocked_next_commands
                .iter()
                .any(|b| b.command.contains("index --full"))
        );
        assert!(
            !plan
                .safe_next_commands
                .iter()
                .any(|c| c.contains("index --full"))
        );
        // The binary upgrade itself touches no data, so it is NOT backup-gated.
        let binary = plan
            .actions
            .iter()
            .find(|a| matches!(a.action, UpgradeAction::BinaryUpgrade))
            .expect("binary action present");
        assert!(!binary.backup_required);
        assert!(!binary.blocked);
    }

    // --- Acceptance scenario 4: failed installer/channel probe -----------------

    #[test]
    fn unsupported_platform_is_installer_unavailable() {
        let platform = Platform {
            os: HostOs::Other,
            arch: "riscv64".to_string(),
            path_style: PathStyle::Posix,
            tool_notes: vec![],
        };
        let mut host =
            HostDoctorReport::skeleton("exotic", platform, HostProbeStatus::CommandNotFound, 10);
        host.cass_version = None;
        let assessment = assess_host(&host, TARGET);
        let plan = rehearse_host(
            &host,
            &assessment,
            &fresh_coverage(),
            RehearsalMode::Fixture,
        );

        assert_eq!(
            plan.disposition,
            HostUpgradeDisposition::InstallerUnavailable
        );
        assert_eq!(plan.install_method, UpgradeMethod::Unsupported);
        assert!(plan.install_command.is_none());
        // No blocked-or-runnable binary command — there is no channel.
        assert!(
            !plan
                .safe_next_commands
                .iter()
                .any(|c| c.contains("self-update") || c.contains("brew"))
        );
    }

    // --- Acceptance scenario 5: successful binary upgrade, stale assets --------

    #[test]
    fn binary_upgraded_but_stale_derived_assets_is_not_truly_fixed() {
        let before_after = UpgradeBeforeAfter {
            before_version: Some("0.4.1".to_string()),
            after_version: Some("0.6.13".to_string()),
            binary_upgraded: true,
            before_readiness: Some(ReadinessState::NotReady),
            after_readiness: Some(ReadinessState::Degraded),
            readiness_improved: true,
            derived_assets_still_stale: true, // the trap
            coverage_before: CoverageState::Stale,
            coverage_after: CoverageState::Stale,
        };
        // Every bounded post-check actually passed (assertions ran, exit 0).
        let checks = passing_checks();
        let v = verify_post_upgrade("csd", before_after, checks, 86_400_000);

        // Binary changed, but the upgrade did NOT truly fix the host.
        assert!(!v.upgrade_truly_fixed);
        assert!(v.before_after.binary_upgraded);
        // The stale-derived-assets failure is surfaced, not hidden.
        assert!(
            v.partial_failures
                .iter()
                .any(|f| f.contains("derived assets remain stale"))
        );
        assert!(v.summary.contains("NOT fully fixed"));
    }

    // --- Acceptance scenario 6: successful upgrade + readiness improvement ------

    #[test]
    fn successful_upgrade_with_readiness_improvement_is_truly_fixed() {
        let before_after = UpgradeBeforeAfter {
            before_version: Some("0.6.10".to_string()),
            after_version: Some("0.6.13".to_string()),
            binary_upgraded: true,
            before_readiness: Some(ReadinessState::Degraded),
            after_readiness: Some(ReadinessState::Ready),
            readiness_improved: true,
            derived_assets_still_stale: false,
            coverage_before: CoverageState::Stale,
            coverage_after: CoverageState::Fresh,
        };
        let v = verify_post_upgrade("ts2", before_after, passing_checks(), 86_400_000);

        assert!(v.upgrade_truly_fixed);
        assert_eq!(v.overall_status, ProofStatus::Pass);
        assert!(v.partial_failures.is_empty());
        assert!(v.summary.contains("verified"));
        assert_eq!(v.check_proofs.len(), 6);
    }

    // --- Post-upgrade verification edge cases ----------------------------------

    #[test]
    fn a_timed_out_post_check_rolls_up_to_timeout_and_blocks_fix() {
        let before_after = UpgradeBeforeAfter {
            before_version: Some("0.6.10".to_string()),
            after_version: Some("0.6.13".to_string()),
            binary_upgraded: true,
            before_readiness: Some(ReadinessState::Degraded),
            after_readiness: Some(ReadinessState::Ready),
            readiness_improved: true,
            derived_assets_still_stale: false,
            coverage_before: CoverageState::Fresh,
            coverage_after: CoverageState::Fresh,
        };
        // First check times out; the rest pass.
        let mut checks = passing_checks();
        checks[0].1.timed_out = true;
        let v = verify_post_upgrade("ts2", before_after, checks, 86_400_000);

        assert_eq!(v.overall_status, ProofStatus::Timeout);
        assert!(!v.upgrade_truly_fixed, "a timeout can never read as a fix");
        assert!(v.partial_failures.iter().any(|f| f.contains("timeout")));
    }

    #[test]
    fn no_post_checks_is_generated_only_never_a_silent_pass() {
        let before_after = UpgradeBeforeAfter {
            before_version: Some("0.6.13".to_string()),
            after_version: Some("0.6.13".to_string()),
            binary_upgraded: false,
            before_readiness: Some(ReadinessState::Ready),
            after_readiness: Some(ReadinessState::Ready),
            readiness_improved: false,
            derived_assets_still_stale: false,
            coverage_before: CoverageState::Fresh,
            coverage_after: CoverageState::Fresh,
        };
        let v = verify_post_upgrade("local", before_after, Vec::new(), 86_400_000);
        assert_eq!(v.overall_status, ProofStatus::GeneratedOnly);
        assert!(!v.upgrade_truly_fixed);
    }

    // --- Distinct actions, opt-in model install --------------------------------

    #[test]
    fn model_install_is_opt_in_and_never_auto() {
        let mut host = linux_host("ts2", Some("0.6.10"), HostProbeStatus::Ok);
        host.semantic = Some(SemanticState::AssetsMissing);
        let assessment = assess_host(&host, TARGET);
        let plan = rehearse_host(
            &host,
            &assessment,
            &fresh_coverage(),
            RehearsalMode::Fixture,
        );

        let model = plan
            .actions
            .iter()
            .find(|a| matches!(a.action, UpgradeAction::ModelInstall))
            .expect("model action present");
        assert!(model.applicable);
        assert!(model.opt_in);
        assert!(model.blocked, "opt-in install is blocked by default");
        assert!(model.requires_confirmation);
        // Never auto-downloaded: it appears only among the blocked commands.
        assert!(
            plan.blocked_next_commands
                .iter()
                .any(|b| b.command.contains("models install"))
        );
        assert!(
            !plan
                .safe_next_commands
                .iter()
                .any(|c| c.contains("models install"))
        );
    }

    #[test]
    fn five_distinct_actions_each_carry_a_scope_and_proof_check() {
        let host = linux_host("ts2", Some("0.4.1"), HostProbeStatus::Ok);
        let assessment = assess_host(&host, "1.0.0"); // force a major gap
        let plan = rehearse_host(
            &host,
            &assessment,
            &coverage_with(CoverageState::MissingDerivedAssets, ArchiveRisk::Low),
            RehearsalMode::Fixture,
        );
        assert_eq!(plan.actions.len(), 5);
        // Schema migration is archive-scope and always backup-gated.
        let migration = plan
            .actions
            .iter()
            .find(|a| matches!(a.action, UpgradeAction::DataSchemaMigration))
            .expect("migration present");
        assert_eq!(migration.mutation_scope, UpgradeActionScope::Archive);
        assert!(migration.backup_required);
        assert!(migration.requires_confirmation);
    }

    // --- macOS prefers installer, channel resolution ---------------------------

    #[test]
    fn macos_minor_gap_needs_manual_upgrade_via_homebrew() {
        let host = macos_host("mac-mini-max", Some("0.6.10"));
        let assessment = assess_host(&host, TARGET);
        let plan = rehearse_host(
            &host,
            &assessment,
            &fresh_coverage(),
            RehearsalMode::Fixture,
        );
        // macOS forces the installer path even for a minor gap.
        assert_eq!(plan.install_method, UpgradeMethod::ManualInstaller);
        assert_eq!(plan.channel, "homebrew");
    }

    // --- Up-to-date host --------------------------------------------------------

    #[test]
    fn current_host_is_up_to_date_with_no_applicable_binary_upgrade() {
        let host = linux_host("local", Some("0.6.13"), HostProbeStatus::Ok);
        let assessment = assess_host(&host, TARGET);
        let plan = rehearse_host(
            &host,
            &assessment,
            &fresh_coverage(),
            RehearsalMode::Fixture,
        );
        assert_eq!(plan.disposition, HostUpgradeDisposition::UpToDate);
        let binary = plan
            .actions
            .iter()
            .find(|a| matches!(a.action, UpgradeAction::BinaryUpgrade))
            .expect("binary action present");
        assert!(!binary.applicable);
    }

    // --- Fleet rollup -----------------------------------------------------------

    #[test]
    fn fleet_rollup_counts_orders_and_takes_max_archive_risk() {
        let local = linux_host("local", Some("0.6.13"), HostProbeStatus::Ok);
        let ts2 = linux_host("ts2", Some("0.6.10"), HostProbeStatus::Ok);
        let csd = linux_host("csd", Some("0.6.10"), HostProbeStatus::Ok);
        let mut gone = linux_host("gone", None, HostProbeStatus::Unreachable);
        gone.unreachable = true;

        let a_local = assess_host(&local, TARGET);
        let a_ts2 = assess_host(&ts2, TARGET);
        let a_csd = assess_host(&csd, TARGET);
        let a_gone = assess_host(&gone, TARGET);

        let cov_fresh = fresh_coverage();
        let cov_high = coverage_with(CoverageState::Stale, ArchiveRisk::High);

        let inputs = vec![
            HostRehearsalInput {
                report: &local,
                assessment: &a_local,
                coverage: &cov_fresh,
            },
            HostRehearsalInput {
                report: &ts2,
                assessment: &a_ts2,
                coverage: &cov_fresh,
            },
            HostRehearsalInput {
                report: &csd,
                assessment: &a_csd,
                coverage: &cov_high,
            },
            HostRehearsalInput {
                report: &gone,
                assessment: &a_gone,
                coverage: &cov_fresh,
            },
        ];
        let fleet = rehearse_fleet(&inputs, TARGET, RehearsalMode::Fixture);

        assert_eq!(fleet.hosts_up_to_date, 1); // local
        assert_eq!(fleet.hosts_unreachable, 1); // gone (unreachable, not counted as needing)
        assert_eq!(fleet.hosts_gated_by_archive, 1); // csd
        assert_eq!(fleet.hosts_needing_upgrade, 2); // ts2 (ready) + csd (archive-gated)
        assert_eq!(fleet.highest_archive_risk, ArchiveRisk::High);
        assert!(fleet.any_blocked);
        // Recommended order: ts2 (self-update, lowest risk) before csd (archive-gated).
        assert_eq!(fleet.recommended_order, vec!["ts2", "csd"]);
    }

    // --- Invariants: no credential leak, stable serialization ------------------

    #[test]
    fn emitted_commands_never_leak_ssh_credentials() {
        let host = linux_host("ts2", Some("0.6.10"), HostProbeStatus::Ok);
        let assessment = assess_host(&host, TARGET);
        let plan = rehearse_host(
            &host,
            &assessment,
            &fresh_coverage(),
            RehearsalMode::Fixture,
        );
        let json = serde_json::to_string(&plan).expect("serialize");
        assert!(
            !contains_credential_leak(&json),
            "rehearsal must not embed user@host credentials"
        );
        // The leak detector itself works.
        assert!(contains_credential_leak("ssh deploy@host.example.com"));
        assert!(!contains_credential_leak("cass self-update"));
    }

    #[test]
    fn rehearsal_serializes_with_stable_fields_and_round_trips() {
        let host = linux_host("ts2", Some("0.6.10"), HostProbeStatus::Ok);
        let assessment = assess_host(&host, TARGET);
        let plan = rehearse_host(
            &host,
            &assessment,
            &fresh_coverage(),
            RehearsalMode::Fixture,
        );
        let value = serde_json::to_value(&plan).expect("to_value");
        assert_eq!(
            value["schema_version"],
            FLEET_UPGRADE_REHEARSAL_SCHEMA_VERSION
        );
        assert_eq!(value["host_alias"], "ts2");
        assert_eq!(value["disposition"], "upgrade-ready");
        assert_eq!(value["channel"], "self-update");
        assert_eq!(value["rehearsal_mode"], "fixture");
        assert_eq!(value["capability_gap"], "minor");
        // Post-checks are the full bounded battery.
        assert_eq!(value["post_checks"].as_array().map(|a| a.len()), Some(6));
        let back: HostUpgradeRehearsal = serde_json::from_value(value).expect("round-trip");
        assert_eq!(back, plan);
    }

    #[test]
    fn wire_labels_are_stable_kebab() {
        assert_eq!(RehearsalMode::Fixture.as_str(), "fixture");
        assert_eq!(RehearsalMode::Live.as_str(), "live");
        assert_eq!(UpgradeAction::BinaryUpgrade.as_str(), "binary-upgrade");
        assert_eq!(
            UpgradeAction::DataSchemaMigration.as_str(),
            "data-schema-migration"
        );
        assert_eq!(UpgradeActionScope::DerivedAssets.as_str(), "derived-assets");
        assert_eq!(
            HostUpgradeDisposition::UpgradeGatedByArchive.as_str(),
            "upgrade-gated-by-archive"
        );
        assert_eq!(PostUpgradeCheck::ApiVersion.as_str(), "api-version");
        // serde matches the as_str labels.
        assert_eq!(
            serde_json::to_string(&UpgradeAction::SourceSync).expect("ser"),
            "\"source-sync\""
        );
    }

    #[test]
    fn rehearsal_mode_maps_to_runner_mode() {
        assert_eq!(RehearsalMode::Fixture.run_mode(), RunMode::Ci);
        assert_eq!(RehearsalMode::Live.run_mode(), RunMode::Live);
    }

    #[test]
    fn missing_root_breaks_source_coverage_ok() {
        let mut coverage = fresh_coverage();
        coverage.provenance_gaps.push(ProvenanceGap {
            kind: ProvenanceGapKind::MissingRoot,
            path: "/home/user/.claude/projects".to_string(),
        });
        let host = linux_host("ts2", Some("0.6.10"), HostProbeStatus::Ok);
        let assessment = assess_host(&host, TARGET);
        let plan = rehearse_host(&host, &assessment, &coverage, RehearsalMode::Fixture);
        assert!(!plan.preflight.source_coverage_ok);
        assert!(!plan.preflight.archive_safe_to_proceed);
    }

    // --- helpers ---------------------------------------------------------------

    /// A passing [`ProofRun`] for a given post-check command.
    fn passing_run(command: &str) -> ProofRun {
        ProofRun {
            command: command.to_string(),
            binary_path: Some("/usr/local/bin/cass".to_string()),
            binary_version: Some("0.6.13".to_string()),
            data_dir_or_fixture: Some("fixture:upgraded".to_string()),
            exit_code: Some(0),
            elapsed_ms: 80,
            timeout_ms: 5_000,
            timed_out: false,
            skipped: false,
            assertions_ran: true,
            produced_artifact: true,
            completed: true,
            artifact_age_ms: Some(10),
            stdout_path: None,
            stderr_path: None,
        }
    }

    /// One passing run per check in the bounded battery.
    fn passing_checks() -> Vec<(PostUpgradeCheck, ProofRun)> {
        PostUpgradeCheck::battery()
            .into_iter()
            .map(|c| (c, passing_run(c.command())))
            .collect()
    }
}
