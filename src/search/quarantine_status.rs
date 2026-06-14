// Dead-code tolerated module-wide: this quarantine status grouping lands
// ahead of its projection into `cass quarantine status --json` / the status
// subsection in src/lib.rs. Bounded-budget wrapping is .2.2's concern.
#![allow(dead_code)]

//! Quarantine status grouped by cause, version, and eligibility (bead
//! cass-fleet-resilience-20260608-uojcg.3.1).
//!
//! The report's local node showed 133 irreducible ingest-OOM quarantines,
//! and #258 showed legacy entries (no `cass_version_at_quarantine`) being
//! silently orphaned. A status surface must therefore group the quarantine
//! state so an agent can tell an irreducible same-version failure from a
//! legacy/version-stale entry that is retry-eligible, and from a
//! source-missing entry.
//!
//! [`quarantine_status`] consumes the canonical
//! [`QuarantineState`](crate::indexer::quarantine::QuarantineState) (the
//! `.3.4` model) plus the current `cass` version and a caller-supplied set of
//! conversation ids whose source path is gone, and produces a
//! [`QuarantineStatusReport`] with `total_excluded_conversations`, grouped
//! counts (by cause, version bucket, schema_version, eligibility), a
//! representative entry per group, retry-eligibility reasons,
//! last-attempt timestamps, and the single next safe command. All enums
//! serialize as snake_case; the next command is never bare/destructive.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::indexer::quarantine::QuarantineState;

/// How an entry's `cass_version_at_quarantine` relates to the current binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum VersionBucket {
    /// Stamped with the current version — already retried under this binary.
    SameVersion,
    /// Stamped with an older version — a bump may have fixed the cause.
    VersionStale,
    /// No version recorded (pre-v0.6.x); the #258 legacy carry-over class.
    Legacy,
}

/// Whether an entry should be re-attempted, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RetryEligibility {
    /// Retry-eligible (legacy or version-stale): a newer binary may fix it.
    Eligible,
    /// Not eligible: irreducible failure already retried under this version.
    IrreducibleSameVersion,
}

/// A representative entry for a group (no raw conversation content).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RepresentativeEntry {
    pub conversation_id: String,
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cass_version_at_quarantine: Option<String>,
    pub version_bucket: VersionBucket,
    pub eligibility: RetryEligibility,
    pub attempt_count: u64,
    /// RFC3339 last-attempt timestamp.
    pub last_attempt_at: String,
    pub last_reason: String,
    /// Whether the conversation's source path is gone (when known).
    pub source_missing: bool,
}

/// The grouped quarantine status report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct QuarantineStatusReport {
    pub total_excluded_conversations: usize,
    /// Count by `last_reason` (cause code, e.g. `ingest_oom`).
    pub by_cause: BTreeMap<String, usize>,
    /// Count by version bucket (same/stale/legacy).
    pub by_version_bucket: BTreeMap<VersionBucket, usize>,
    /// Count by schema_version.
    pub by_schema_version: BTreeMap<u32, usize>,
    /// Count by retry eligibility.
    pub by_eligibility: BTreeMap<RetryEligibility, usize>,
    /// Number of entries whose source path is gone.
    pub source_missing_count: usize,
    /// One representative entry per (cause, version_bucket) group.
    pub representative_entries: Vec<RepresentativeEntry>,
    /// One-line retry-eligibility summary reason.
    pub eligibility_reason: String,
    /// The single next safe command (never bare/destructive).
    pub next_safe_command: String,
}

/// Build the grouped quarantine status from the canonical state.
///
/// - `current_version`: the running `cass` version, for eligibility.
/// - `source_missing_ids`: conversation ids whose source path is gone (the
///   caller joins this from the source table; empty when not evaluated).
pub(crate) fn quarantine_status(
    state: &QuarantineState,
    current_version: &str,
    source_missing_ids: &BTreeSet<String>,
) -> QuarantineStatusReport {
    let mut by_cause: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_version_bucket: BTreeMap<VersionBucket, usize> = BTreeMap::new();
    let mut by_schema_version: BTreeMap<u32, usize> = BTreeMap::new();
    let mut by_eligibility: BTreeMap<RetryEligibility, usize> = BTreeMap::new();
    let mut source_missing_count = 0usize;
    // One representative per (cause, bucket) so the list stays bounded.
    let mut reps: BTreeMap<(String, VersionBucket), RepresentativeEntry> = BTreeMap::new();
    let mut total = 0usize;

    for (key, record) in state.iter() {
        total += 1;

        let bucket = match &record.cass_version_at_quarantine {
            None => VersionBucket::Legacy,
            Some(v) if v == current_version => VersionBucket::SameVersion,
            Some(_) => VersionBucket::VersionStale,
        };
        let eligibility = if record.is_version_stale_for_retry(current_version) {
            RetryEligibility::Eligible
        } else {
            RetryEligibility::IrreducibleSameVersion
        };
        let source_missing = source_missing_ids.contains(&key.conversation_id);
        if source_missing {
            source_missing_count += 1;
        }

        *by_cause.entry(record.last_reason.clone()).or_default() += 1;
        *by_version_bucket.entry(bucket).or_default() += 1;
        *by_schema_version.entry(key.schema_version).or_default() += 1;
        *by_eligibility.entry(eligibility).or_default() += 1;

        reps.entry((record.last_reason.clone(), bucket))
            .or_insert_with(|| RepresentativeEntry {
                conversation_id: key.conversation_id.clone(),
                schema_version: key.schema_version,
                cass_version_at_quarantine: record.cass_version_at_quarantine.clone(),
                version_bucket: bucket,
                eligibility,
                attempt_count: record.attempt_count,
                last_attempt_at: record.last_attempt_at.to_rfc3339(),
                last_reason: record.last_reason.clone(),
                source_missing,
            });
    }

    let eligible = by_eligibility
        .get(&RetryEligibility::Eligible)
        .copied()
        .unwrap_or(0);
    let irreducible = by_eligibility
        .get(&RetryEligibility::IrreducibleSameVersion)
        .copied()
        .unwrap_or(0);

    let (eligibility_reason, next_safe_command) = if total == 0 {
        (
            "no quarantined conversations".to_string(),
            "cass status --json".to_string(),
        )
    } else if eligible > 0 {
        (
            format!(
                "{eligible} entries are retry-eligible (legacy/version-stale); {irreducible} are irreducible under the current version"
            ),
            // Re-running the index retries eligible entries; non-destructive.
            "cass index".to_string(),
        )
    } else {
        (
            format!(
                "all {irreducible} entries are irreducible same-version failures; inspect before any action"
            ),
            "cass diag --json --quarantine".to_string(),
        )
    };

    QuarantineStatusReport {
        total_excluded_conversations: total,
        by_cause,
        by_version_bucket,
        by_schema_version,
        by_eligibility,
        source_missing_count,
        representative_entries: reps.into_values().collect(),
        eligibility_reason,
        next_safe_command,
    }
}

/// Whether quarantine exclusions currently degrade search completeness
/// (bead cass-fleet-resilience-20260608-uojcg.3.3). Serializes snake_case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SearchCompletenessStatus {
    /// Nothing is quarantined — search covers the whole archive.
    Ok,
    /// Conversations are quarantined — known content is excluded from results.
    Degraded,
}

/// Compact search-completeness verdict shared by the readiness surfaces
/// (`cass health`/`status`/`triage`) and `cass search --robot-meta`
/// (bead cass-fleet-resilience-20260608-uojcg.3.3).
///
/// Stale lexical search is still usable, but quarantine *exclusions* mean known
/// conversations are simply absent from results — a different failure than
/// staleness. Agents need to tell the two apart. This verdict answers the one
/// question that matters when results look thin: *is search missing known
/// conversations, and can it still run?* It is derived from the cheap ingest
/// quarantine summary (counts + circuit-breaker flag) so even the <50ms health
/// surface can carry it without a filesystem walk. All fields serialize
/// snake_case; the `next_command` is always a safe, non-destructive pointer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SearchCompleteness {
    /// `ok` when nothing is excluded, `degraded` when conversations are quarantined.
    pub quarantine_status: SearchCompletenessStatus,
    /// Conversations excluded from search because they are quarantined.
    pub quarantined_conversations: u64,
    /// Whether search currently covers every known conversation.
    pub complete: bool,
    /// Whether ordinary search can still proceed. Quarantine never blocks
    /// search — it only narrows coverage — so this stays `true`; it exists so
    /// agents can branch on availability vs. completeness without parsing prose.
    pub can_search: bool,
    /// Whether coverage is *suspect* because the ingest circuit breaker tripped
    /// on a recent quarantine burst (vs. a stable, already-triaged backlog).
    pub coverage_suspect: bool,
    /// One-line impact statement agents may surface verbatim.
    pub impact: String,
    /// The single safe next command for inspecting or retrying quarantine.
    pub next_command: String,
}

/// Project the search-completeness verdict from the cheap ingest-quarantine
/// facts every readiness surface already gathers
/// (`quarantined_conversations` + `circuit_breaker_active`). Pure: no I/O.
pub(crate) fn project_search_completeness(
    quarantined_conversations: u64,
    circuit_breaker_active: bool,
) -> SearchCompleteness {
    // Coverage is degraded when conversations are excluded OR a recent burst
    // tripped the breaker (the breaker implies recent exclusions in practice).
    let degraded = quarantined_conversations > 0 || circuit_breaker_active;
    if !degraded {
        return SearchCompleteness {
            quarantine_status: SearchCompletenessStatus::Ok,
            quarantined_conversations: 0,
            complete: true,
            can_search: true,
            coverage_suspect: false,
            impact: "search covers all known conversations; nothing is quarantined".to_string(),
            next_command: "cass status --json".to_string(),
        };
    }

    let impact = if circuit_breaker_active {
        format!(
            "{quarantined_conversations} conversation(s) are quarantined and the ingest circuit breaker is active; search coverage is suspect until the watcher/source path is inspected"
        )
    } else {
        format!(
            "{quarantined_conversations} conversation(s) are excluded from search after irreducible ingest quarantine; the rest of the archive is searchable"
        )
    };

    SearchCompleteness {
        quarantine_status: SearchCompletenessStatus::Degraded,
        quarantined_conversations,
        complete: false,
        // Quarantine narrows coverage; it does not stop ordinary search.
        can_search: true,
        coverage_suspect: circuit_breaker_active,
        impact,
        // Inspect-first pointer is always safe; eligibility-aware retry lives in
        // `quarantine_status()` / `cass index`, which the operator runs after review.
        next_command: "cass diag --json --quarantine".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexer::quarantine::QuarantineRecord;
    use chrono::{DateTime, Utc};

    fn ts(secs: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(secs, 0).expect("valid timestamp")
    }

    fn record(version: Option<&str>, reason: &str, attempts: u64) -> QuarantineRecord {
        QuarantineRecord {
            first_attempt_at: ts(1_700_000_000),
            last_attempt_at: ts(1_700_000_500),
            attempt_count: attempts,
            last_reason: reason.to_string(),
            cass_version_at_quarantine: version.map(str::to_string),
        }
    }

    /// State with: 2 same-version ingest_oom (irreducible), 1 legacy (eligible),
    /// 1 older-version (eligible), 1 different cause.
    fn mixed_state() -> QuarantineState {
        let mut s = QuarantineState::default();
        s.entries.insert(
            "c-same-1::v3".to_string(),
            record(Some("0.6.13"), "ingest_oom", 4),
        );
        s.entries.insert(
            "c-same-2::v3".to_string(),
            record(Some("0.6.13"), "ingest_oom", 7),
        );
        s.entries
            .insert("c-legacy::v1".to_string(), record(None, "ingest_oom", 1));
        s.entries.insert(
            "c-old::v2".to_string(),
            record(Some("0.5.1"), "validation_failed", 2),
        );
        s
    }

    #[test]
    fn enums_serialize_snake_case() {
        assert_eq!(
            serde_json::to_string(&VersionBucket::VersionStale).unwrap(),
            "\"version_stale\""
        );
        assert_eq!(
            serde_json::to_string(&RetryEligibility::IrreducibleSameVersion).unwrap(),
            "\"irreducible_same_version\""
        );
    }

    #[test]
    fn distinguishes_irreducible_same_version_from_legacy_and_stale() {
        let report = quarantine_status(&mixed_state(), "0.6.13", &BTreeSet::new());
        assert_eq!(report.total_excluded_conversations, 4);
        // 2 same-version irreducible.
        assert_eq!(
            report.by_version_bucket.get(&VersionBucket::SameVersion),
            Some(&2)
        );
        assert_eq!(
            report.by_version_bucket.get(&VersionBucket::Legacy),
            Some(&1)
        );
        assert_eq!(
            report.by_version_bucket.get(&VersionBucket::VersionStale),
            Some(&1)
        );
        // Eligibility: legacy + stale = 2 eligible; same-version = 2 irreducible.
        assert_eq!(
            report.by_eligibility.get(&RetryEligibility::Eligible),
            Some(&2)
        );
        assert_eq!(
            report
                .by_eligibility
                .get(&RetryEligibility::IrreducibleSameVersion),
            Some(&2)
        );
    }

    #[test]
    fn groups_by_cause_and_schema_version() {
        let report = quarantine_status(&mixed_state(), "0.6.13", &BTreeSet::new());
        assert_eq!(report.by_cause.get("ingest_oom"), Some(&3));
        assert_eq!(report.by_cause.get("validation_failed"), Some(&1));
        assert_eq!(report.by_schema_version.get(&3), Some(&2));
        assert_eq!(report.by_schema_version.get(&1), Some(&1));
    }

    #[test]
    fn all_irreducible_recommends_inspection_not_retry() {
        let mut s = QuarantineState::default();
        // 133 same-version ingest-OOM (the report's local node).
        for i in 0..133 {
            s.entries
                .insert(format!("c{i}::v3"), record(Some("0.6.13"), "ingest_oom", 5));
        }
        let report = quarantine_status(&s, "0.6.13", &BTreeSet::new());
        assert_eq!(report.total_excluded_conversations, 133);
        assert_eq!(
            report
                .by_eligibility
                .get(&RetryEligibility::IrreducibleSameVersion),
            Some(&133)
        );
        assert!(report.eligibility_reason.contains("irreducible"));
        assert_eq!(report.next_safe_command, "cass diag --json --quarantine");
    }

    #[test]
    fn eligible_entries_recommend_a_nondestructive_retry() {
        let report = quarantine_status(&mixed_state(), "0.6.13", &BTreeSet::new());
        assert_eq!(report.next_safe_command, "cass index");
        for bad in ["rm ", "--force-clean", "delete ", "DROP "] {
            assert!(!report.next_safe_command.contains(bad));
        }
    }

    #[test]
    fn source_missing_entries_are_counted() {
        let mut missing = BTreeSet::new();
        missing.insert("c-legacy".to_string());
        let report = quarantine_status(&mixed_state(), "0.6.13", &missing);
        assert_eq!(report.source_missing_count, 1);
    }

    #[test]
    fn representative_entries_carry_eligibility_and_timestamp() {
        let report = quarantine_status(&mixed_state(), "0.6.13", &BTreeSet::new());
        assert!(!report.representative_entries.is_empty());
        for rep in &report.representative_entries {
            assert!(!rep.last_attempt_at.is_empty());
            // A same-version ingest_oom rep is irreducible; legacy/old eligible.
            match rep.version_bucket {
                VersionBucket::SameVersion => {
                    assert_eq!(rep.eligibility, RetryEligibility::IrreducibleSameVersion)
                }
                VersionBucket::Legacy | VersionBucket::VersionStale => {
                    assert_eq!(rep.eligibility, RetryEligibility::Eligible)
                }
            }
        }
    }

    #[test]
    fn empty_state_reports_nothing_excluded() {
        let report = quarantine_status(&QuarantineState::default(), "0.6.13", &BTreeSet::new());
        assert_eq!(report.total_excluded_conversations, 0);
        assert!(report.eligibility_reason.contains("no quarantined"));
    }

    #[test]
    fn report_round_trips_through_json() {
        let report = quarantine_status(&mixed_state(), "0.6.13", &BTreeSet::new());
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("\"total_excluded_conversations\":4"));
        assert!(json.contains("\"next_safe_command\""));
        let parsed: QuarantineStatusReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, report);
    }

    // --- search-completeness verdict (uojcg.3.3) ---

    #[test]
    fn completeness_is_ok_when_nothing_is_quarantined() {
        let c = project_search_completeness(0, false);
        assert_eq!(c.quarantine_status, SearchCompletenessStatus::Ok);
        assert_eq!(c.quarantined_conversations, 0);
        assert!(c.complete);
        assert!(c.can_search);
        assert!(!c.coverage_suspect);
        assert_eq!(c.next_command, "cass status --json");
    }

    #[test]
    fn completeness_is_degraded_but_searchable_for_irreducible_backlog() {
        // Quarantined entries exist, but no recent burst (breaker inactive):
        // search remains usable; coverage is incomplete but not suspect.
        let c = project_search_completeness(133, false);
        assert_eq!(c.quarantine_status, SearchCompletenessStatus::Degraded);
        assert_eq!(c.quarantined_conversations, 133);
        assert!(!c.complete);
        assert!(c.can_search, "quarantine never blocks ordinary search");
        assert!(!c.coverage_suspect);
        assert!(c.impact.contains("133"));
        assert!(c.impact.contains("rest of the archive is searchable"));
        assert_eq!(c.next_command, "cass diag --json --quarantine");
    }

    #[test]
    fn completeness_marks_coverage_suspect_when_circuit_breaker_active() {
        let c = project_search_completeness(7, true);
        assert_eq!(c.quarantine_status, SearchCompletenessStatus::Degraded);
        assert!(c.coverage_suspect, "active breaker => coverage suspect");
        assert!(c.can_search);
        assert!(c.impact.contains("circuit breaker is active"));
    }

    #[test]
    fn completeness_next_command_is_never_destructive() {
        for (count, breaker) in [(0u64, false), (5, false), (5, true)] {
            let c = project_search_completeness(count, breaker);
            for bad in ["rm ", "--force-clean", "delete ", "DROP ", "--purge"] {
                assert!(
                    !c.next_command.contains(bad),
                    "next_command must stay safe: {}",
                    c.next_command
                );
            }
        }
    }

    #[test]
    fn completeness_enum_serializes_snake_case_and_round_trips() {
        assert_eq!(
            serde_json::to_string(&SearchCompletenessStatus::Degraded).unwrap(),
            "\"degraded\""
        );
        let c = project_search_completeness(2, false);
        let json = serde_json::to_string(&c).unwrap();
        assert!(json.contains("\"quarantine_status\":\"degraded\""));
        assert!(json.contains("\"can_search\":true"));
        let parsed: SearchCompleteness = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, c);
    }
}
