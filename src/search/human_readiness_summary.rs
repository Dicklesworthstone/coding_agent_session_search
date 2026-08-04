//! Bounded human CLI/TUI readiness summaries that mirror the robot JSON
//! (bead cass-fleet-resilience-20260608-uojcg.13.2).
//!
//! Robot JSON is the source of truth for agents, but humans still need a
//! concise summary that never *contradicts* the machine contract. A human
//! should not have to learn the internal distinction between the SQLite
//! archive, the lexical index, semantic vectors, quarantines, and source
//! mappings before deciding what is safe to do.
//!
//! This module is the single human projection of the canonical
//! [`DerivedAssetTruthTable`]. It is deliberately layered *on top of* the
//! robot projection [`crate::search::readiness_projection::project`] so the
//! human and robot surfaces can never disagree: the headline class, the
//! searchable bit, and the safe-next-action code are taken verbatim from the
//! same [`ReadinessSummary`] the robot JSON serializes, and the recommended
//! command is taken verbatim from [`DerivedAssetTruthTable::safe_next_command`]
//! (which already gates destructive cleanup and high-archive-risk rebuilds).
//!
//! Invariants the tests enforce:
//! - **Parity, not paraphrase.** `class`, `search_usable_now`, and
//!   `safe_next_action` equal the robot [`ReadinessSummary`] fields for the
//!   same truth table. The human copy references the same stable snake_case
//!   state codes (`lexical=stale_but_searchable`, `next=refresh_lexical`, …)
//!   so an operator can grep them straight out of `--json`.
//! - **Nuance preserved.** stale-but-searchable ≠ missing; semantic-absent ≠
//!   lexical-broken; quarantine-incomplete ≠ no-results; an unreachable
//!   source ≠ a removed source. Each maps to distinct copy.
//! - **Unsafe-command suppression.** The only command a summary can surface
//!   comes from the curated [`crate::search::readiness::SafeNextCommand`]
//!   all-list; it never emits destructive cleanup (`rm`, `--delete`,
//!   `reset --hard`, …), bare interactive automation (a sub-command-less
//!   `cass` TUI launch), or a mutating rebuild while archive risk is high.
//! - **Bounded + degradable.** Output is a short, capped line list. Cheap
//!   surfaces (`health`) defer the rich probes (quarantine scan, projection
//!   age) and render a compact partial that still carries the core readiness
//!   class — it never blocks on a slow probe.

use serde::{Deserialize, Serialize};

use crate::search::readiness::{
    ArchiveRiskLevel, BinaryCompatibility, CanonicalDbAvailability, DerivedAssetTruthTable,
    LexicalReadinessState, SafeNextAction, SemanticReadinessState, SourceCoverageState,
};
use crate::search::readiness_projection::{ReadinessClass, ReadinessSummary, SurfaceKind, project};

/// Command fragments that must never appear in operator-facing copy. Any
/// destructive cleanup, blind overwrite, or repo-wrecking verb listed here is
/// treated as a contract violation by [`HumanReadinessSummary::is_safe`].
pub(crate) const UNSAFE_COMMAND_FRAGMENTS: &[&str] = &[
    "rm -rf",
    "rm -r ",
    "rm -f",
    " rm ",
    "rmdir",
    "--delete",
    "reset --hard",
    "git clean",
    "checkout --",
    "drop table",
    "drop database",
    "truncate",
    "mkfs",
    "dd if=",
    "shred",
    "--purge",
    "> /dev/sd",
];

/// The curated set of `cass` sub-command prefixes a safe-next-command may use.
/// Every surfaced command must match one of these — an allow-list, so a new
/// (accidentally destructive) command cannot slip through unnoticed. None of
/// these mutate user data: `index` rebuilds derived assets from the canonical
/// SQLite source of truth, and the rest are read-only or setup.
const SAFE_COMMAND_PREFIXES: &[&str] = &[
    "cass diag",
    "cass health",
    "cass status",
    "cass doctor",
    "cass index",
    "cass sources",
    "cass models",
    "cass self-update",
    "cass triage",
];

/// Whether a candidate operator command is free of destructive fragments AND
/// matches the safe sub-command allow-list. A `None` command (pure wait/none)
/// is always safe.
pub(crate) fn command_is_safe(command: Option<&str>) -> bool {
    let Some(cmd) = command else {
        return true;
    };
    let lower = cmd.to_ascii_lowercase();
    if UNSAFE_COMMAND_FRAGMENTS
        .iter()
        .any(|frag| lower.contains(frag))
    {
        return false;
    }
    // Must be a recognised, sub-commanded `cass` invocation — never a bare
    // `cass` (which would launch the interactive TUI).
    SAFE_COMMAND_PREFIXES
        .iter()
        .any(|prefix| lower.starts_with(prefix))
}

/// Whether free-form copy contains any destructive command fragment. Used to
/// keep the prose (headline / why / missing notes) clean, not just the
/// machine command field.
pub(crate) fn text_is_clean(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    !UNSAFE_COMMAND_FRAGMENTS
        .iter()
        .any(|frag| lower.contains(frag))
}

/// The bounded, human-facing readiness summary every CLI/TUI surface renders.
/// Output-only prose plus the machine-matchable codes it mirrors, so the same
/// struct backs the rendered text *and* the parity proof against robot JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HumanReadinessSummary {
    /// Which surface produced this (controls compactness, never vocabulary).
    pub surface: SurfaceKind,
    /// The canonical readiness class, verbatim from the robot
    /// [`ReadinessSummary`] (mirrors robot JSON `class`).
    pub class: ReadinessClass,
    /// Icon + one-line dominant-state headline.
    pub headline: String,
    /// Whether ordinary search is usable right now (mirrors robot
    /// `is_searchable`).
    pub search_usable_now: bool,
    /// Why the dominant state matters — one plain sentence.
    pub why_it_matters: String,
    /// What is missing or stale, nuance-preserving; `None` when fully
    /// converged. Bounded to a few notes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub whats_missing_or_stale: Option<String>,
    /// The single safest next action as human prose (verbatim from the
    /// curated [`crate::search::readiness::SafeNextCommand`] reason).
    pub safest_next_action: String,
    /// Machine-matchable safe-next-action code (mirrors robot JSON `next`).
    pub safe_next_action: SafeNextAction,
    /// The copy-pasteable safe command, when one applies (`None` for
    /// wait/none). Guaranteed to pass [`command_is_safe`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub safe_command: Option<String>,
    /// Compact `key=value` reference of the canonical codes so an operator can
    /// cross-reference `--json` output for support/debugging.
    pub state_codes: String,
    /// Fields deferred for speed on cheap surfaces (so the human knows what is
    /// *not* yet probed rather than reading silence as "fine"). Empty on full
    /// surfaces.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deferred_fields: Vec<String>,
    /// True when this is a compact partial (rich probes deferred / slow).
    pub partial: bool,
}

/// Max rendered lines for the bounded line list — keeps CLI/TUI output tight.
const MAX_RENDER_LINES: usize = 7;
/// Max characters per rendered line.
const MAX_LINE_LEN: usize = 200;

impl HumanReadinessSummary {
    /// Render the bounded multi-line human summary (CLI). Each line is short;
    /// the whole block is capped at [`MAX_RENDER_LINES`].
    pub(crate) fn render_lines(&self) -> Vec<String> {
        let mut lines = Vec::with_capacity(MAX_RENDER_LINES);
        lines.push(self.headline.clone());
        lines.push(format!(
            "  Search usable now: {}",
            if self.search_usable_now { "yes" } else { "no" }
        ));
        lines.push(format!("  Why: {}", self.why_it_matters));
        if let Some(missing) = &self.whats_missing_or_stale {
            lines.push(format!("  Missing/stale: {missing}"));
        }
        let action_line = match &self.safe_command {
            Some(cmd) => format!("  Safest next step: {} → `{cmd}`", self.safest_next_action),
            None => format!("  Safest next step: {}", self.safest_next_action),
        };
        lines.push(action_line);
        if self.partial && !self.deferred_fields.is_empty() {
            lines.push(format!(
                "  (compact: deferred {})",
                self.deferred_fields.join(", ")
            ));
        }
        lines.push(format!("  State codes: {}", self.state_codes));
        // Enforce the bound defensively: truncate over-long lines and cap count.
        lines.truncate(MAX_RENDER_LINES);
        for line in &mut lines {
            if line.chars().count() > MAX_LINE_LEN {
                let truncated: String = line.chars().take(MAX_LINE_LEN - 1).collect();
                *line = format!("{truncated}…");
            }
        }
        lines
    }

    /// Render a single compact line (TUI status bar / footer).
    pub(crate) fn render_compact_line(&self) -> String {
        let usable = if self.search_usable_now {
            "searchable"
        } else {
            "not searchable"
        };
        let mut line = format!(
            "{} · {usable} · next: {}",
            self.headline, self.safest_next_action
        );
        if line.chars().count() > MAX_LINE_LEN {
            let truncated: String = line.chars().take(MAX_LINE_LEN - 1).collect();
            line = format!("{truncated}…");
        }
        line
    }

    /// Whether this summary is safe to show: no destructive fragment anywhere
    /// in the prose, the command (if any) is on the safe allow-list, and — the
    /// archive-first invariant — no *mutating* command is surfaced under high
    /// archive risk.
    pub(crate) fn is_safe(&self, archive_risk: ArchiveRiskLevel) -> bool {
        if !command_is_safe(self.safe_command.as_deref()) {
            return false;
        }
        let prose_clean = [
            self.headline.as_str(),
            self.why_it_matters.as_str(),
            self.safest_next_action.as_str(),
        ]
        .iter()
        .chain(self.whats_missing_or_stale.as_deref().iter())
        .all(|t| text_is_clean(t));
        if !prose_clean {
            return false;
        }
        // High archive risk must never surface a coverage-reducing rebuild.
        if archive_risk == ArchiveRiskLevel::High && self.safe_next_action.is_mutating() {
            return false;
        }
        true
    }
}

/// Render the stable snake_case enum value (strip the JSON quotes) so the
/// human `state_codes` line carries exactly the strings the robot JSON emits.
fn code<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value)
        .ok()
        .map(|s| s.trim_matches('"').to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Project the canonical truth table into the bounded human summary for
/// `surface`. Layered on the robot projection so parity is structural, not
/// hand-maintained.
pub(crate) fn project_human_summary(
    table: &DerivedAssetTruthTable,
    surface: SurfaceKind,
) -> HumanReadinessSummary {
    let robot: ReadinessSummary = project(table, surface);
    let safe_cmd = table.safe_next_command();

    let headline = headline_for(robot.class);
    let why_it_matters = why_for(robot.class).to_string();
    let whats_missing_or_stale = missing_or_stale_note(table, &robot);

    let state_codes = format!(
        "class={} db={} lexical={} semantic={} source={} archive_risk={} binary={} next={}",
        code(&robot.class),
        code(&table.db),
        code(&table.readiness.lexical),
        code(&table.readiness.semantic),
        code(&table.source_coverage),
        code(&table.archive_risk),
        code(&table.binary),
        code(&safe_cmd.action),
    );

    HumanReadinessSummary {
        surface,
        class: robot.class,
        headline,
        search_usable_now: robot.is_searchable,
        why_it_matters,
        whats_missing_or_stale,
        // The human next-step prose and command are taken verbatim from the
        // curated safe-next-command, so suppression is structural.
        safest_next_action: safe_cmd.reason.clone(),
        safe_next_action: safe_cmd.action,
        safe_command: safe_cmd.command.clone(),
        state_codes,
        deferred_fields: robot.deferred_fields.clone(),
        partial: !robot.deferred_fields.is_empty(),
    }
}

/// One-line headline (icon + dominant-state phrase) per readiness class.
fn headline_for(class: ReadinessClass) -> String {
    match class {
        ReadinessClass::Ready => "✓ Search ready",
        ReadinessClass::StaleSearchable => "≈ Search ready (index stale)",
        ReadinessClass::Repairing => "↻ Search degraded (rebuild in progress)",
        ReadinessClass::Missing => "✗ Search unavailable (no index)",
        ReadinessClass::CorruptQuarantined => "✗ Search unavailable (index corrupt/quarantined)",
        ReadinessClass::DbUnusable => "✗ Search unavailable (database unusable)",
        ReadinessClass::Unreachable => "? Host unreachable",
    }
    .to_string()
}

/// One-sentence "why it matters" per readiness class. Conservative and
/// archive-first: never implies a blind rebuild over an unusable DB.
fn why_for(class: ReadinessClass) -> &'static str {
    match class {
        ReadinessClass::Ready => {
            "All derived search assets are converged; results are complete and current."
        }
        ReadinessClass::StaleSearchable => {
            "Search is fully correct for everything already indexed; only the most recent \
             sessions may not appear yet."
        }
        ReadinessClass::Repairing => {
            "A rebuild is running; queries still work but may return partial results until it \
             settles — wait or attach, do not start a second rebuild."
        }
        ReadinessClass::Missing => {
            "No usable lexical index exists yet, so search returns nothing until it is rebuilt \
             from the canonical archive."
        }
        ReadinessClass::CorruptQuarantined => {
            "The lexical index failed validation and was quarantined; it needs inspection before \
             any auto-recovery is safe."
        }
        ReadinessClass::DbUnusable => {
            "The canonical database could not be opened or validated; inspect it read-only first \
             — never blind-rebuild over it."
        }
        ReadinessClass::Unreachable => {
            "The host backing this node did not answer the probe, so no local state is \
             trustworthy; retry from a reachable node."
        }
    }
}

/// Build the nuance-preserving "what is missing or stale" note from the
/// component facts. Bounded to at most three notes, in priority order, so the
/// summary stays compact. Cheap (health) surfaces drop the advisory quarantine
/// detail they deferred. Returns `None` when nothing is missing/stale.
fn missing_or_stale_note(
    table: &DerivedAssetTruthTable,
    robot: &ReadinessSummary,
) -> Option<String> {
    let mut notes: Vec<String> = Vec::new();
    let compact = !robot.deferred_fields.is_empty();

    // Source coverage nuance — "unreachable remote" is distinct from
    // "removed/unconfigured source".
    match table.source_coverage {
        SourceCoverageState::Unconfigured => {
            notes.push(
                "no sources are configured (source=unconfigured) — nothing indexed yet".into(),
            );
        }
        SourceCoverageState::Unavailable => notes.push(
            "configured sources are unreachable (source=unavailable); the archive is preserved — \
             reconnect before syncing, do not remove the source"
                .into(),
        ),
        SourceCoverageState::Partial => notes.push(
            "some configured sources are unreachable (source=partial); the index reflects the \
             reachable subset"
                .into(),
        ),
        SourceCoverageState::Complete | SourceCoverageState::Unknown => {}
    }

    // Lexical staleness (search still correct for indexed content).
    if table.readiness.lexical == LexicalReadinessState::StaleButSearchable {
        notes.push(
            "lexical index lags recent ingests (lexical=stale_but_searchable) — search works, a \
             refresh picks up the newest sessions"
                .into(),
        );
    }

    // Semantic axis — absent / backfilling / policy-disabled are distinct and
    // none of them break lexical search.
    if table.db == CanonicalDbAvailability::Available && table.readiness.lexical.is_searchable() {
        match table.readiness.semantic {
            SemanticReadinessState::Absent => notes.push(
                "semantic refinement unavailable (semantic=absent); results are lexical-only until \
                 a model is installed"
                    .into(),
            ),
            SemanticReadinessState::Backfilling => notes.push(
                "semantic refinement still backfilling (semantic=backfilling); hybrid quality \
                 improves on its own"
                    .into(),
            ),
            SemanticReadinessState::PolicyDisabled => notes.push(
                "semantic refinement disabled by policy (semantic=policy_disabled) — intentional, \
                 not a fault"
                    .into(),
            ),
            SemanticReadinessState::FastTierReady | SemanticReadinessState::HybridReady => {}
        }
    }

    // Binary skew.
    match table.binary {
        BinaryCompatibility::Outdated => notes.push(
            "this binary is older than the fleet baseline (binary=outdated) — upgrade before \
             trusting or rebuilding assets"
                .into(),
        ),
        BinaryCompatibility::Ahead => notes.push(
            "this binary is newer than the on-disk assets (binary=ahead) — rebuild assets for the \
             current schema"
                .into(),
        ),
        BinaryCompatibility::Current | BinaryCompatibility::Unknown => {}
    }

    // Quarantine is advisory and never makes results incorrect; a cheap
    // surface defers the detail rather than blocking on the scan.
    if robot.quarantine_incomplete {
        if compact {
            notes
                .push("quarantined artifacts present (advisory; detail deferred for speed)".into());
        } else {
            notes.push(format!(
                "{} artifact(s) quarantined (advisory; results are unaffected)",
                table.quarantine.quarantined_count
            ));
        }
    }

    if notes.is_empty() {
        return None;
    }
    notes.truncate(3);
    Some(notes.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::readiness::{
        ArchiveRiskLevel, LexicalMetadata, MaintenanceActivity, QuarantineSummary,
        ReadinessSnapshot, fleet_fixtures,
    };
    use std::collections::BTreeMap;

    fn fixture(name: &str) -> DerivedAssetTruthTable {
        fleet_fixtures()
            .into_iter()
            .find(|(n, _)| *n == name)
            .expect("missing fleet fixture (name not present in fleet_fixtures)")
            .1
    }

    /// A fully-converged table (no fleet fixture seeds the all-green node).
    fn converged() -> DerivedAssetTruthTable {
        DerivedAssetTruthTable {
            db: CanonicalDbAvailability::Available,
            source_coverage: SourceCoverageState::Complete,
            scan_watermark_ms: Some(1),
            last_projection_ms: Some(1),
            lexical_metadata: LexicalMetadata {
                present: true,
                schema_hash: Some("schema-current".into()),
                storage_fingerprint: Some("fp-current".into()),
                built_at_ms: Some(1),
            },
            readiness: ReadinessSnapshot::new(
                LexicalReadinessState::Ready,
                SemanticReadinessState::HybridReady,
            ),
            quarantine: QuarantineSummary::default(),
            maintenance: MaintenanceActivity::Idle,
            archive_risk: ArchiveRiskLevel::None,
            binary: BinaryCompatibility::Current,
        }
    }

    fn with_lexical_semantic(
        lexical: LexicalReadinessState,
        semantic: SemanticReadinessState,
    ) -> DerivedAssetTruthTable {
        let mut t = converged();
        t.readiness = ReadinessSnapshot::new(lexical, semantic);
        t
    }

    // ---- struct/enum serialization -------------------------------------

    #[test]
    fn summary_serializes_and_round_trips() {
        let s = project_human_summary(&converged(), SurfaceKind::Status);
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"class\":\"ready\""));
        assert!(json.contains("\"search_usable_now\":true"));
        let parsed: HumanReadinessSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, s);
    }

    // ---- state-to-copy mapping -----------------------------------------

    #[test]
    fn headline_distinguishes_every_class() {
        // Distinct copy for each class — the nuance the single health bit lost.
        let classes = [
            ReadinessClass::Ready,
            ReadinessClass::StaleSearchable,
            ReadinessClass::Repairing,
            ReadinessClass::Missing,
            ReadinessClass::CorruptQuarantined,
            ReadinessClass::DbUnusable,
            ReadinessClass::Unreachable,
        ];
        let headlines: BTreeMap<String, ()> =
            classes.iter().map(|c| (headline_for(*c), ())).collect();
        assert_eq!(headlines.len(), classes.len(), "headlines must be distinct");
    }

    #[test]
    fn stale_searchable_is_distinct_from_missing() {
        let stale =
            project_human_summary(&fixture("css_stale_existing_index"), SurfaceKind::Status);
        let missing = project_human_summary(
            &fixture("csd_missing_lexical_metadata"),
            SurfaceKind::Status,
        );
        assert_eq!(stale.class, ReadinessClass::StaleSearchable);
        assert!(stale.search_usable_now, "stale index is still searchable");
        assert_eq!(missing.class, ReadinessClass::Missing);
        assert!(
            !missing.search_usable_now,
            "missing index is not searchable"
        );
        assert_ne!(stale.headline, missing.headline);
        assert_ne!(stale.safe_next_action, missing.safe_next_action);
    }

    #[test]
    fn semantic_absent_does_not_read_as_lexical_broken() {
        // Lexical ready + semantic absent: search IS usable; the note is about
        // refinement, not a broken index.
        let s = project_human_summary(
            &with_lexical_semantic(LexicalReadinessState::Ready, SemanticReadinessState::Absent),
            SurfaceKind::Status,
        );
        assert!(s.search_usable_now);
        assert_eq!(s.class, ReadinessClass::Ready);
        let missing = s.whats_missing_or_stale.as_deref().unwrap_or_default();
        assert!(missing.contains("semantic=absent"), "got: {missing}");
        assert_eq!(s.safe_next_action, SafeNextAction::InstallSemanticModel);
    }

    #[test]
    fn unreachable_source_is_distinct_from_removed_source() {
        let unavailable = {
            let mut t = converged();
            t.source_coverage = SourceCoverageState::Unavailable;
            project_human_summary(&t, SurfaceKind::Status)
        };
        let unconfigured = {
            let mut t = converged();
            t.source_coverage = SourceCoverageState::Unconfigured;
            project_human_summary(&t, SurfaceKind::Status)
        };
        let unavailable_note = unavailable.whats_missing_or_stale.unwrap_or_default();
        let unconfigured_note = unconfigured.whats_missing_or_stale.unwrap_or_default();
        assert!(unavailable_note.contains("source=unavailable"));
        assert!(
            unavailable_note.contains("do not remove the source"),
            "unreachable must warn against removal: {unavailable_note}"
        );
        assert!(unconfigured_note.contains("source=unconfigured"));
        assert_ne!(unavailable_note, unconfigured_note);
    }

    #[test]
    fn quarantine_incomplete_is_distinct_from_no_results() {
        // local_stale_quarantine has quarantined artifacts but is searchable.
        let s = project_human_summary(&fixture("local_stale_quarantine"), SurfaceKind::Status);
        assert!(
            s.search_usable_now,
            "quarantine never makes search unusable"
        );
        let note = s.whats_missing_or_stale.unwrap_or_default();
        assert!(note.contains("quarantined"), "got: {note}");
        assert!(note.contains("advisory"), "quarantine is advisory: {note}");
    }

    // ---- parity with the robot projection (E2E-style proof) ------------

    #[test]
    fn human_summary_never_contradicts_robot_for_any_fleet_state() {
        // The 13.2 core invariant: emit structured parity proof lines and
        // assert the human projection matches the robot JSON for every
        // canonical fleet fixture, across every surface.
        for (name, table) in fleet_fixtures() {
            for surface in [
                SurfaceKind::Health,
                SurfaceKind::Status,
                SurfaceKind::Triage,
                SurfaceKind::SearchMeta,
            ] {
                let robot = project(&table, surface);
                let human = project_human_summary(&table, surface);

                // Machine-matchable fields are taken verbatim from the robot
                // summary, so they must be byte-identical.
                assert_eq!(human.class, robot.class, "{name}/{surface:?} class");
                assert_eq!(
                    human.search_usable_now, robot.is_searchable,
                    "{name}/{surface:?} searchable"
                );
                assert_eq!(
                    human.safe_next_action, robot.safe_next_action,
                    "{name}/{surface:?} next action"
                );

                // The human copy references the same stable state codes the
                // robot JSON serializes.
                assert!(
                    human
                        .state_codes
                        .contains(&format!("class={}", code(&robot.class))),
                    "{name}/{surface:?} class code missing from human copy"
                );
                assert!(
                    human
                        .state_codes
                        .contains(&format!("next={}", code(&robot.safe_next_action))),
                    "{name}/{surface:?} next code missing from human copy"
                );

                // Structured parity log line (visible with --nocapture); proves
                // human↔robot agreement for this fixture/surface.
                let proof = serde_json::json!({
                    "event": "human_robot_parity",
                    "fixture": name,
                    "surface": code(&surface),
                    "robot": {
                        "class": code(&robot.class),
                        "is_searchable": robot.is_searchable,
                        "next": code(&robot.safe_next_action),
                    },
                    "human": {
                        "class": code(&human.class),
                        "search_usable_now": human.search_usable_now,
                        "next": code(&human.safe_next_action),
                        "state_codes": human.state_codes,
                    },
                    "parity": true,
                });
                println!("{proof}");
            }
        }
    }

    // ---- unsafe-command suppression ------------------------------------

    #[test]
    fn no_summary_surfaces_a_destructive_command() {
        // Every fleet fixture plus synthetic high-risk / corrupt / missing
        // cases must pass the safety gate.
        let mut tables: Vec<(String, DerivedAssetTruthTable)> = fleet_fixtures()
            .into_iter()
            .map(|(n, t)| (n.to_string(), t))
            .collect();
        tables.push(("converged".into(), converged()));
        // High archive risk must never surface a mutating rebuild.
        tables.push(("high_risk".into(), fixture("ts1_high_archive_risk")));

        for (name, table) in tables {
            for surface in [SurfaceKind::Health, SurfaceKind::Status] {
                let human = project_human_summary(&table, surface);
                assert!(
                    human.is_safe(table.archive_risk),
                    "{name}/{surface:?} produced an unsafe summary: {:?}",
                    human.safe_command
                );
                // No rendered line carries a destructive fragment either.
                for line in human.render_lines() {
                    assert!(
                        text_is_clean(&line),
                        "{name}/{surface:?} unsafe line: {line}"
                    );
                }
            }
        }
    }

    #[test]
    fn high_archive_risk_is_backup_first_never_a_rebuild() {
        let human = project_human_summary(&fixture("ts1_high_archive_risk"), SurfaceKind::Status);
        assert_eq!(human.safe_next_action, SafeNextAction::BackupThenRepair);
        assert!(!human.safe_next_action.is_mutating());
        assert!(human.is_safe(ArchiveRiskLevel::High));
        // The surfaced command, if any, is the read-only backup-first review.
        if let Some(cmd) = &human.safe_command {
            assert!(cmd.starts_with("cass doctor"), "got: {cmd}");
        }
    }

    #[test]
    fn command_safety_allow_list_rejects_destructive_and_bare_cass() {
        // Curated safe commands pass.
        assert!(command_is_safe(Some("cass index --full")));
        assert!(command_is_safe(Some("cass diag --json --quarantine")));
        assert!(command_is_safe(None));
        // Destructive / bare / off-list commands are rejected.
        assert!(!command_is_safe(Some("rm -rf ~/.local/share/cass")));
        assert!(!command_is_safe(Some("cass index --full && rm -rf foo")));
        assert!(!command_is_safe(Some("cass")), "bare cass launches the TUI");
        assert!(!command_is_safe(Some("git reset --hard")));
        assert!(!command_is_safe(Some("cass sources remove laptop --purge")));
    }

    // ---- bounded + degradable ------------------------------------------

    #[test]
    fn render_is_bounded() {
        for (_, table) in fleet_fixtures() {
            let human = project_human_summary(&table, SurfaceKind::Status);
            let lines = human.render_lines();
            assert!(lines.len() <= MAX_RENDER_LINES, "too many lines");
            for line in &lines {
                assert!(
                    line.chars().count() <= MAX_LINE_LEN,
                    "line too long: {line}"
                );
            }
            assert!(human.render_compact_line().chars().count() <= MAX_LINE_LEN);
        }
    }

    #[test]
    fn cheap_health_surface_degrades_to_compact_partial() {
        // The slow-probe degradation: health defers the rich quarantine /
        // projection probes and renders a compact partial that still carries
        // the core readiness class.
        let table = fixture("local_stale_quarantine");
        let health = project_human_summary(&table, SurfaceKind::Health);
        let status = project_human_summary(&table, SurfaceKind::Status);

        assert!(health.partial, "health is a compact partial");
        assert!(!health.deferred_fields.is_empty(), "health marks deferrals");
        assert!(!status.partial, "status probes fully");
        assert!(status.deferred_fields.is_empty());

        // Core readiness is identical despite the partial probe.
        assert_eq!(health.class, status.class);
        assert_eq!(health.search_usable_now, status.search_usable_now);
        assert_eq!(health.safe_next_action, status.safe_next_action);

        // The compact partial says quarantine detail is deferred rather than
        // reporting a precise count it did not probe.
        let health_note = health.whats_missing_or_stale.clone().unwrap_or_default();
        let status_note = status.whats_missing_or_stale.clone().unwrap_or_default();
        assert!(
            health_note.contains("deferred"),
            "health note: {health_note}"
        );
        assert!(
            status_note.contains("quarantined"),
            "status note: {status_note}"
        );
    }

    // ---- golden / approval ---------------------------------------------

    /// Representative cases frozen as a golden document. The fleet fixtures
    /// cover the seven canonical node states; the synthetic cases add the
    /// all-green and semantic-axis variations the fixtures do not seed.
    fn golden_cases() -> Vec<(String, DerivedAssetTruthTable)> {
        let mut cases: Vec<(String, DerivedAssetTruthTable)> = fleet_fixtures()
            .into_iter()
            .map(|(n, t)| (n.to_string(), t))
            .collect();
        cases.push(("converged_all_green".into(), converged()));
        cases.push((
            "ready_semantic_absent".into(),
            with_lexical_semantic(LexicalReadinessState::Ready, SemanticReadinessState::Absent),
        ));
        cases.push((
            "ready_semantic_policy_disabled".into(),
            with_lexical_semantic(
                LexicalReadinessState::Ready,
                SemanticReadinessState::PolicyDisabled,
            ),
        ));
        cases
    }

    fn render_golden_document() -> String {
        let mut out = String::new();
        out.push_str("# Golden: human readiness summaries (bead 13.2)\n");
        out.push_str("# Regenerate: RCH_CARGO_WRAPPER_BYPASS=1 UPDATE_GOLDENS=1 cargo test \\\n");
        out.push_str("#   --lib search::human_readiness_summary::tests::golden_human_summaries\n");
        for (name, table) in golden_cases() {
            let human = project_human_summary(&table, SurfaceKind::Status);
            out.push_str("\n========================================\n");
            out.push_str(&format!("case: {name}\n"));
            out.push_str("----------------------------------------\n");
            for line in human.render_lines() {
                out.push_str(&line);
                out.push('\n');
            }
        }
        out
    }

    #[test]
    fn golden_human_summaries() {
        let golden_path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/golden/human/readiness_summaries.txt"
        );
        let rendered = render_golden_document();

        if std::env::var("UPDATE_GOLDENS").is_ok() {
            if let Some(parent) = std::path::Path::new(golden_path).parent() {
                std::fs::create_dir_all(parent).expect("create golden dir");
            }
            std::fs::write(golden_path, &rendered).expect("write golden");
            return;
        }

        let expected = std::fs::read_to_string(golden_path).expect(
            "missing golden tests/golden/human/readiness_summaries.txt — regenerate with \
             UPDATE_GOLDENS=1 (see AGENTS.md)",
        );
        assert_eq!(
            rendered, expected,
            "human readiness golden drift — review then regenerate with UPDATE_GOLDENS=1"
        );
    }
}
