// Dead-code tolerated module-wide: this granular salvage-ledger model lands
// ahead of the historical-salvage loop that will consult and update it.
// Downstream bead .11.5 (integrated golden + E2E gate) exercises it.
#![allow(dead_code)]

//! Granular historical-salvage bundle ledger (bead
//! cass-fleet-resilience-20260608-uojcg.4.2).
//!
//! Issue #247 found `cass` re-scanning historical bundles for 5–12 minutes
//! only to import zero new conversations, because the salvage ledger was
//! *binary* (seen / not-seen) and could not record that a bundle had already
//! been fully covered or had yielded nothing new. This module makes the
//! ledger granular: per bundle it records the source fingerprint it was
//! covered against, batches inspected, imported conversations, skipped rows,
//! a completion marker, and a reason a future run may skip it.
//!
//! From that, [`BundleLedger::decision`] returns whether a bundle should be
//! re-inspected or skipped (and why). The rule is conservative and
//! correctness-first: a changed source fingerprint always forces a
//! re-inspect (the bundle's contents may differ), and a partial/interrupted
//! bundle always resumes; only a `Completed` or `ZeroNew` bundle whose
//! source fingerprint is unchanged is skipped.
//!
//! New granular fields are `#[serde(default)]`, so a legacy binary ledger
//! deserializes cleanly and is treated conservatively (re-inspect) rather
//! than trusted as complete. All enums serialize as snake_case. The salvage
//! loop wiring is deferred; this is the pure, unit-testable core.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// How completely a bundle was processed on its last salvage pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BundleCompletion {
    /// Never finished — interrupted or only partially inspected. Resume.
    #[default]
    Partial,
    /// Fully inspected and all eligible rows imported.
    Completed,
    /// Fully inspected but imported zero new conversations.
    ZeroNew,
}

/// Why a future salvage run may skip a bundle. `None` when it must be
/// inspected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SkipReason {
    /// Previously completed against the same source fingerprint.
    CompletedCoverage,
    /// Previously imported zero new conversations against the same source
    /// fingerprint.
    ZeroNewLastTime,
}

/// The decision for a bundle on the current pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "action", content = "reason")]
pub(crate) enum SalvageDecision {
    /// Inspect the bundle (never seen, source changed, or only partial).
    Inspect,
    /// Skip the bundle, with the reason it is safe to skip.
    Skip(SkipReason),
}

impl SalvageDecision {
    pub(crate) fn is_skip(self) -> bool {
        matches!(self, Self::Skip(_))
    }
}

/// One bundle's granular ledger record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BundleLedgerEntry {
    /// Source fingerprint the bundle was last covered against. A change here
    /// invalidates the skip and forces a re-inspect.
    pub source_fingerprint: String,
    #[serde(default)]
    pub batches_inspected: u64,
    #[serde(default)]
    pub imported_conversations: u64,
    #[serde(default)]
    pub skipped_rows: u64,
    /// Completion marker. Legacy ledgers without this field default to
    /// `Partial` (conservative re-inspect).
    #[serde(default)]
    pub completion: BundleCompletion,
    /// Human reason recorded for a future skip; advisory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_note: Option<String>,
}

impl BundleLedgerEntry {
    /// A freshly completed-coverage entry.
    pub(crate) fn completed(
        source_fingerprint: impl Into<String>,
        batches_inspected: u64,
        imported_conversations: u64,
        skipped_rows: u64,
    ) -> Self {
        Self {
            source_fingerprint: source_fingerprint.into(),
            batches_inspected,
            imported_conversations,
            skipped_rows,
            completion: if imported_conversations == 0 {
                BundleCompletion::ZeroNew
            } else {
                BundleCompletion::Completed
            },
            skip_note: None,
        }
    }
}

/// The granular salvage ledger, keyed by bundle fingerprint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BundleLedger {
    #[serde(default = "default_ledger_version")]
    pub version: u32,
    #[serde(default)]
    pub bundles: BTreeMap<String, BundleLedgerEntry>,
}

fn default_ledger_version() -> u32 {
    2
}

impl Default for BundleLedger {
    fn default() -> Self {
        Self {
            version: default_ledger_version(),
            bundles: BTreeMap::new(),
        }
    }
}

impl BundleLedger {
    /// Decide whether to inspect or skip `bundle_fingerprint` on this pass,
    /// given the bundle's current `source_fingerprint`.
    ///
    /// Skips only when the recorded entry is `Completed`/`ZeroNew` AND its
    /// source fingerprint matches; any change, partial state, or absent
    /// entry forces an inspect.
    pub(crate) fn decision(
        &self,
        bundle_fingerprint: &str,
        source_fingerprint: &str,
    ) -> SalvageDecision {
        let Some(entry) = self.bundles.get(bundle_fingerprint) else {
            return SalvageDecision::Inspect; // never seen
        };
        if entry.source_fingerprint != source_fingerprint {
            return SalvageDecision::Inspect; // source changed; re-scan
        }
        match entry.completion {
            BundleCompletion::Completed => SalvageDecision::Skip(SkipReason::CompletedCoverage),
            BundleCompletion::ZeroNew => SalvageDecision::Skip(SkipReason::ZeroNewLastTime),
            BundleCompletion::Partial => SalvageDecision::Inspect, // resume
        }
    }

    /// Record (insert or replace) a bundle's coverage outcome.
    pub(crate) fn record(
        &mut self,
        bundle_fingerprint: impl Into<String>,
        entry: BundleLedgerEntry,
    ) {
        self.bundles.insert(bundle_fingerprint.into(), entry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "src-fp-1";

    fn completed_ledger() -> BundleLedger {
        let mut l = BundleLedger::default();
        l.record("bundle-a", BundleLedgerEntry::completed(SRC, 8, 120, 4));
        l.record("bundle-zero", BundleLedgerEntry::completed(SRC, 8, 0, 256));
        l
    }

    #[test]
    fn enums_serialize_as_snake_case() {
        assert_eq!(
            serde_json::to_string(&BundleCompletion::ZeroNew).unwrap(),
            "\"zero_new\""
        );
        assert_eq!(
            serde_json::to_string(&SkipReason::CompletedCoverage).unwrap(),
            "\"completed_coverage\""
        );
    }

    #[test]
    fn completed_constructor_marks_zero_new_when_nothing_imported() {
        assert_eq!(
            BundleLedgerEntry::completed(SRC, 8, 0, 9).completion,
            BundleCompletion::ZeroNew
        );
        assert_eq!(
            BundleLedgerEntry::completed(SRC, 8, 3, 9).completion,
            BundleCompletion::Completed
        );
    }

    #[test]
    fn unseen_bundle_is_inspected() {
        let l = BundleLedger::default();
        assert_eq!(l.decision("new-bundle", SRC), SalvageDecision::Inspect);
    }

    #[test]
    fn zero_new_bundle_is_skipped_with_reason() {
        let l = completed_ledger();
        assert_eq!(
            l.decision("bundle-zero", SRC),
            SalvageDecision::Skip(SkipReason::ZeroNewLastTime)
        );
        assert!(l.decision("bundle-zero", SRC).is_skip());
    }

    #[test]
    fn completed_bundle_is_skipped_with_reason() {
        let l = completed_ledger();
        assert_eq!(
            l.decision("bundle-a", SRC),
            SalvageDecision::Skip(SkipReason::CompletedCoverage)
        );
    }

    #[test]
    fn source_fingerprint_change_forces_reinspect_even_when_completed() {
        let l = completed_ledger();
        assert_eq!(
            l.decision("bundle-a", "src-fp-CHANGED"),
            SalvageDecision::Inspect
        );
        assert_eq!(
            l.decision("bundle-zero", "src-fp-CHANGED"),
            SalvageDecision::Inspect
        );
    }

    #[test]
    fn partial_completion_resumes_via_inspect() {
        let mut l = BundleLedger::default();
        l.record(
            "bundle-partial",
            BundleLedgerEntry {
                source_fingerprint: SRC.to_string(),
                batches_inspected: 3,
                imported_conversations: 10,
                skipped_rows: 0,
                completion: BundleCompletion::Partial,
                skip_note: None,
            },
        );
        // An interrupted/partial bundle must be re-inspected (resumed), never
        // skipped.
        assert_eq!(l.decision("bundle-partial", SRC), SalvageDecision::Inspect);
    }

    #[test]
    fn legacy_binary_ledger_migrates_and_is_conservatively_reinspected() {
        // A legacy ledger: no `version`, entries lack the granular fields and
        // the completion marker (only the source fingerprint survives).
        let legacy = serde_json::json!({
            "bundles": {
                "legacy-bundle": { "source_fingerprint": SRC }
            }
        });
        let l: BundleLedger = serde_json::from_value(legacy).unwrap();
        // Missing version defaults; missing completion defaults to Partial.
        assert_eq!(l.version, 2);
        let entry = &l.bundles["legacy-bundle"];
        assert_eq!(entry.completion, BundleCompletion::Partial);
        assert_eq!(entry.batches_inspected, 0);
        // A legacy entry must NOT be trusted as complete; it is re-inspected.
        assert_eq!(l.decision("legacy-bundle", SRC), SalvageDecision::Inspect);
    }

    #[test]
    fn ledger_round_trips_through_json() {
        let l = completed_ledger();
        let json = serde_json::to_string(&l).unwrap();
        let parsed: BundleLedger = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, l);
        // Decision JSON is tagged for stable machine consumption.
        let skip = serde_json::to_string(&l.decision("bundle-a", SRC)).unwrap();
        assert!(skip.contains("\"action\":\"skip\""));
        assert!(skip.contains("\"reason\":\"completed_coverage\""));
    }
}
