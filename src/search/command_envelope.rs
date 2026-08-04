// Dead-code tolerated module-wide: this safe/unsafe next-command envelope
// lands ahead of its projection into the status/triage/fleet JSON builders in
// src/lib.rs (the .1.2 surface-wiring slice consumes it).
#![allow(dead_code)]

//! Safe/unsafe next-command envelope for readiness outputs (bead
//! cass-fleet-resilience-20260608-uojcg.1.4).
//!
//! The report's bottom line: agents need prescriptive status/triage/fleet
//! guidance, not prose they have to guess from. This module replaces the
//! scattered recommendation strings with one structured envelope —
//! [`NextCommandEnvelope`] — listing the recommended commands (with their
//! mutation scope, cost class, and why), the unsafe commands (with why they
//! are blocked and the precondition to unblock them), and the preconditions
//! that must hold. It is derived from the canonical [`DerivedAssetTruthTable`]
//! via the existing `safe_next_command` + `archive_safety_envelope`, so the
//! search-now / refresh-later / backup-first / wait-boundedly /
//! retry-quarantine / upgrade-host decisions are explicit and machine-
//! consumable.
//!
//! Command examples are concrete `cass` invocations — never a bare
//! `cass`/`bv`, never destructive cleanup. All enums serialize as snake_case.

use serde::{Deserialize, Serialize};

use crate::search::readiness::{DerivedAssetTruthTable, SafeNextAction};

/// What running a command would mutate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MutationScope {
    /// Read-only / wait / setup — mutates nothing.
    None,
    /// Rebuilds derived assets (lexical/semantic index) from the canonical DB.
    DerivedAssets,
    /// Could touch the canonical archive itself (data-loss surface).
    Archive,
}

/// Rough cost of running a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CostClass {
    None,
    Seconds,
    Minutes,
    Hours,
}

/// A recommended next command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RecommendedCommand {
    pub action: SafeNextAction,
    /// Concrete command, or `None` for a pure wait/none action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    pub mutation_scope: MutationScope,
    pub cost_class: CostClass,
    pub why: String,
}

/// A command that is unsafe in the current state, and how to unblock it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct UnsafeCommand {
    pub action: SafeNextAction,
    pub command: String,
    pub why_blocked: String,
    pub unblock_precondition: String,
}

/// The structured next-command envelope a readiness surface emits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct NextCommandEnvelope {
    pub recommended_commands: Vec<RecommendedCommand>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unsafe_commands: Vec<UnsafeCommand>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preconditions: Vec<String>,
}

/// The mutation scope a [`SafeNextAction`] would have.
fn mutation_scope(action: SafeNextAction) -> MutationScope {
    use SafeNextAction as A;
    match action {
        A::IndexFull | A::RepairLexical | A::RefreshLexical | A::RebuildForCurrentBinary => {
            MutationScope::DerivedAssets
        }
        _ => MutationScope::None,
    }
}

/// The cost class a [`SafeNextAction`] roughly incurs.
fn cost_class(action: SafeNextAction) -> CostClass {
    use SafeNextAction as A;
    match action {
        A::None | A::WaitForMaintenance | A::WaitForSemantic | A::HostUnreachable => {
            CostClass::None
        }
        A::InspectCanonicalDb | A::InspectQuarantine | A::ConfigureSources | A::ReconnectSource => {
            CostClass::Seconds
        }
        A::RefreshLexical
        | A::RepairLexical
        | A::RebuildForCurrentBinary
        | A::InstallSemanticModel
        | A::UpgradeBinary
        | A::BackupThenRepair => CostClass::Minutes,
        // A full index over a large corpus is the only hours-class action.
        A::IndexFull => CostClass::Hours,
    }
}

impl DerivedAssetTruthTable {
    /// Build the structured next-command envelope. `data_dir` is surfaced in
    /// the backup precondition when archive risk gates commands.
    pub(crate) fn next_command_envelope(&self, data_dir: Option<&str>) -> NextCommandEnvelope {
        let safe = self.safe_next_command();
        let archive = self.archive_safety_envelope(data_dir);

        let recommended = RecommendedCommand {
            action: safe.action,
            command: safe.command.clone(),
            mutation_scope: mutation_scope(safe.action),
            cost_class: cost_class(safe.action),
            why: safe.reason.clone(),
        };

        // Under high archive risk, the gated mutating actions are unsafe
        // until a backup/fingerprinted plan exists.
        let unsafe_commands: Vec<UnsafeCommand> = archive
            .unsafe_until_backup
            .iter()
            .map(|&action| UnsafeCommand {
                action,
                command: command_for(action),
                why_blocked:
                    "high archive risk: this would touch the only copy before a backup exists"
                        .to_string(),
                unblock_precondition:
                    "create a verified backup or a fingerprinted recovery plan first".to_string(),
            })
            .collect();

        let mut preconditions = Vec::new();
        if archive.backup_recommended {
            let where_ = data_dir.unwrap_or("the canonical data dir");
            preconditions.push(format!("back up {where_} before any mutating repair"));
        }

        NextCommandEnvelope {
            recommended_commands: vec![recommended],
            unsafe_commands,
            preconditions,
        }
    }
}

/// The concrete command string for a gated action (kept in sync with the
/// safe-next-command vocabulary). Never bare/destructive.
fn command_for(action: SafeNextAction) -> String {
    use SafeNextAction as A;
    match action {
        A::IndexFull | A::RebuildForCurrentBinary => "cass index --full",
        A::RefreshLexical => "cass index",
        A::RepairLexical => "cass index --full",
        _ => "cass status --json",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::readiness::{
        ArchiveRiskLevel, CanonicalDbAvailability, LexicalMetadata, LexicalReadinessState,
        MaintenanceActivity, QuarantineSummary, ReadinessSnapshot, SemanticReadinessState,
        SourceCoverageState, fleet_fixtures,
    };

    fn fixture(name: &str) -> DerivedAssetTruthTable {
        fleet_fixtures()
            .into_iter()
            .find(|(n, _)| *n == name)
            .unwrap_or_else(|| panic!("missing fixture {name}"))
            .1
    }

    fn assert_no_bare_or_destructive(env: &NextCommandEnvelope) {
        let all = env
            .recommended_commands
            .iter()
            .filter_map(|c| c.command.clone())
            .chain(env.unsafe_commands.iter().map(|c| c.command.clone()));
        for cmd in all {
            assert_ne!(cmd.trim(), "cass", "bare cass");
            assert_ne!(cmd.trim(), "bv", "bare bv");
            assert!(
                cmd.starts_with("cass "),
                "must be a concrete cass command: {cmd}"
            );
            for bad in ["rm ", "rm -", "--force-clean", "DROP ", "delete "] {
                assert!(!cmd.contains(bad), "destructive: {cmd}");
            }
        }
    }

    #[test]
    fn enums_serialize_snake_case() {
        assert_eq!(
            serde_json::to_string(&MutationScope::DerivedAssets).unwrap(),
            "\"derived_assets\""
        );
        assert_eq!(
            serde_json::to_string(&CostClass::Hours).unwrap(),
            "\"hours\""
        );
    }

    #[test]
    fn stale_searchable_recommends_refresh_minutes_derived_assets() {
        let env = fixture("css_stale_existing_index").next_command_envelope(Some("/data/cass"));
        let rec = &env.recommended_commands[0];
        assert_eq!(rec.action, SafeNextAction::RefreshLexical);
        assert_eq!(rec.mutation_scope, MutationScope::DerivedAssets);
        assert_eq!(rec.cost_class, CostClass::Minutes);
        assert!(env.unsafe_commands.is_empty());
        assert_no_bare_or_destructive(&env);
    }

    #[test]
    fn missing_lexical_metadata_recommends_repair() {
        let env = fixture("csd_missing_lexical_metadata").next_command_envelope(Some("/data/cass"));
        let rec = &env.recommended_commands[0];
        assert_eq!(rec.action, SafeNextAction::RepairLexical);
        assert_eq!(rec.mutation_scope, MutationScope::DerivedAssets);
        assert_no_bare_or_destructive(&env);
    }

    #[test]
    fn high_archive_risk_gates_mutating_commands_with_backup_precondition() {
        let env = fixture("ts1_high_archive_risk").next_command_envelope(Some("/data/cass"));
        let rec = &env.recommended_commands[0];
        assert_eq!(rec.action, SafeNextAction::BackupThenRepair);
        assert_eq!(
            rec.mutation_scope,
            MutationScope::None,
            "backup-first is non-mutating"
        );
        // Mutating rebuilds are listed unsafe with an unblock precondition.
        assert!(!env.unsafe_commands.is_empty());
        for u in &env.unsafe_commands {
            assert!(mutation_scope(u.action) == MutationScope::DerivedAssets);
            assert!(!u.unblock_precondition.is_empty());
            assert!(u.why_blocked.contains("archive risk"));
        }
        // A backup precondition naming the data dir is present.
        assert!(env.preconditions.iter().any(|p| p.contains("/data/cass")));
        assert_no_bare_or_destructive(&env);
    }

    #[test]
    fn semantic_absent_recommends_install_no_unsafe() {
        // Ready lexical + semantic absent -> install the model (opportunistic).
        let t = DerivedAssetTruthTable {
            db: CanonicalDbAvailability::Available,
            source_coverage: SourceCoverageState::Complete,
            scan_watermark_ms: Some(1),
            last_projection_ms: Some(1),
            lexical_metadata: LexicalMetadata::default(),
            readiness: ReadinessSnapshot::new(
                LexicalReadinessState::Ready,
                SemanticReadinessState::Absent,
            ),
            quarantine: QuarantineSummary::default(),
            maintenance: MaintenanceActivity::Idle,
            archive_risk: ArchiveRiskLevel::Low,
            binary: crate::search::readiness::BinaryCompatibility::Current,
        };
        let env = t.next_command_envelope(Some("/data/cass"));
        assert_eq!(
            env.recommended_commands[0].action,
            SafeNextAction::InstallSemanticModel
        );
        assert!(env.unsafe_commands.is_empty());
        assert!(
            env.preconditions.is_empty(),
            "low risk: no backup precondition"
        );
        assert_no_bare_or_destructive(&env);
    }

    #[test]
    fn slow_status_semantic_backfill_waits_boundedly() {
        // ts2: semantic backfilling -> wait for semantic (cost None, no mutation).
        let env = fixture("ts2_fast_health_slow_status").next_command_envelope(Some("/data/cass"));
        let rec = &env.recommended_commands[0];
        assert_eq!(rec.action, SafeNextAction::WaitForSemantic);
        assert_eq!(rec.cost_class, CostClass::None);
        assert_eq!(rec.mutation_scope, MutationScope::None);
        assert!(rec.command.is_none(), "a wait has no command");
    }

    #[test]
    fn source_path_mismatch_recommends_reconnect() {
        // Configured sources unreachable (path mismatch / unmounted) ->
        // reconnect before sync; read-only, seconds.
        let t = DerivedAssetTruthTable {
            db: CanonicalDbAvailability::Available,
            source_coverage: SourceCoverageState::Unavailable,
            scan_watermark_ms: Some(1),
            last_projection_ms: Some(1),
            lexical_metadata: LexicalMetadata::default(),
            readiness: ReadinessSnapshot::new(
                LexicalReadinessState::Ready,
                SemanticReadinessState::HybridReady,
            ),
            quarantine: QuarantineSummary::default(),
            maintenance: MaintenanceActivity::Idle,
            archive_risk: ArchiveRiskLevel::Low,
            binary: crate::search::readiness::BinaryCompatibility::Current,
        };
        let env = t.next_command_envelope(Some("/data/cass"));
        let rec = &env.recommended_commands[0];
        assert_eq!(rec.action, SafeNextAction::ReconnectSource);
        assert_eq!(rec.mutation_scope, MutationScope::None);
        assert_eq!(rec.cost_class, CostClass::Seconds);
        assert_no_bare_or_destructive(&env);
    }

    #[test]
    fn envelope_round_trips_through_json() {
        let env = fixture("ts1_high_archive_risk").next_command_envelope(Some("/data/cass"));
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains("\"recommended_commands\""));
        assert!(json.contains("\"unsafe_commands\""));
        assert!(json.contains("\"unblock_precondition\""));
        let parsed: NextCommandEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, env);
    }
}
