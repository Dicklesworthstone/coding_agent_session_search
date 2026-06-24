//! Bounded, resumable retry of retry-eligible quarantine entries (bead
//! cass-fleet-resilience-20260608-uojcg.3.2).
//!
//! The report's local node showed 133 irreducible ingest-OOM quarantines plus a
//! tail of legacy (`#258`, no `cass_version_at_quarantine`) and version-stale
//! entries. `.3.1`'s [`quarantine_status`](crate::search::quarantine_status)
//! surface lets an agent *see* which entries are retry-eligible; this module is
//! the matching *action*: a bounded, resumable retry that re-attempts only the
//! eligible (legacy / version-stale) entries, leaves irreducible same-version
//! entries untouched, skips entries whose source log is gone, and re-quarantines
//! anything that OOMs again.
//!
//! Two pure entry points mirror the dry-run / execute split the bead requires:
//!
//! - [`plan_retry`] is the **dry-run / plan**: it classifies every quarantine
//!   entry into a [`PlannedDisposition`] (retry / skip-irreducible /
//!   skip-source-missing / skip-budget-exhausted) without mutating anything or
//!   touching disk. Serialise its [`RetryPlan`] for `--dry-run` JSON.
//! - [`execute_retry`] walks that same plan and invokes a caller-supplied
//!   re-ingest function for each `Retry` entry, recording a per-entry
//!   [`RetryOutcome`], clearing the quarantine record on success and
//!   re-quarantining (which stamps the current version, demoting the entry to
//!   irreducible same-version) on OOM/failure.
//!
//! Bounding is the whole point: `max_attempts` caps the work so a 100k-entry
//! corpus never turns one command into an unbounded re-ingest storm — deferred
//! eligible entries are reported as `skipped_budget_exhausted`, never silently
//! dropped. Resumption needs no separate checkpoint file: the durable
//! `quarantine_state.json` *is* the checkpoint. Cleared entries are gone and
//! re-quarantined entries are now same-version, so a second pass naturally
//! re-plans against the shrunken eligible set and only attempts what is left.
//!
//! Non-goals (enforced here): never clear an irreducible entry silently — the
//! `eligible_only` contract defaults on, and the only way to attempt an
//! irreducible entry is the explicit `eligible_only=false` operator override;
//! never delete a source log; never trigger a full index rebuild when a targeted
//! retry suffices. All enums serialise snake_case; `next_safe_command` is always
//! a safe, non-destructive pointer.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::quarantine::{QuarantineKey, QuarantineState};

/// Stable wire-format version for [`RetryPlan`] / [`RetryReport`] JSON, pinned
/// the same way the sibling resilience modules pin theirs.
pub const QUARANTINE_RETRY_SCHEMA_VERSION: u32 = 1;

/// How a bounded retry pass is bounded and filtered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryConfig {
    /// Maximum number of entries to **attempt** in a single pass. `None` leaves
    /// the pass bounded only by the eligible set; `Some(n)` caps the work so a
    /// huge backlog stays a bounded, resumable operation. Eligible entries
    /// beyond the cap are reported as `skipped_budget_exhausted`, never dropped.
    #[serde(default)]
    pub max_attempts: Option<usize>,
    /// When `true` (the default and the bead's safe contract) only
    /// retry-eligible (legacy / version-stale) entries are attempted;
    /// irreducible same-version entries are reported but never re-ingested,
    /// honouring the "do not clear irreducible entries silently" non-goal.
    /// `false` is an explicit operator override that force-retries every entry
    /// (still never silent — it is requested per-invocation).
    #[serde(default = "default_eligible_only")]
    pub eligible_only: bool,
}

fn default_eligible_only() -> bool {
    true
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: None,
            eligible_only: true,
        }
    }
}

/// What a planned entry will do this pass, decided before any re-ingest runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlannedDisposition {
    /// Eligible, source present, and within budget — will be re-ingested.
    Retry,
    /// Irreducible same-version (not eligible) — skipped; never auto-cleared.
    SkipIrreducible,
    /// The conversation's source log is gone — re-ingest is impossible.
    SkipSourceMissing,
    /// Eligible, but the pass budget is already spent — deferred to a resume.
    SkipBudgetExhausted,
}

/// One planned entry (a dry-run row). Carries no raw conversation content —
/// only the identity, version provenance, and prior-attempt bookkeeping.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedEntry {
    pub conversation_id: String,
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cass_version_at_quarantine: Option<String>,
    pub disposition: PlannedDisposition,
    pub attempt_count: u64,
    pub last_reason: String,
}

/// The dry-run plan for a bounded retry pass. Pure: no mutation, no I/O.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryPlan {
    pub current_version: String,
    /// Total quarantine entries inspected.
    pub total_quarantined: usize,
    /// Entries that are version-eligible (legacy / version-stale), independent
    /// of source presence and budget — the size of the retry pool.
    pub eligible_total: usize,
    /// Entries with disposition [`PlannedDisposition::Retry`].
    pub planned_attempts: usize,
    pub skip_irreducible: usize,
    pub skip_source_missing: usize,
    pub skip_budget_exhausted: usize,
    /// Per-entry rows in deterministic storage-key order.
    pub entries: Vec<PlannedEntry>,
    /// True when eligible entries were deferred by the budget — a resume would
    /// make further progress.
    pub resume_recommended: bool,
    pub summary: String,
    pub next_safe_command: String,
}

/// The result of one re-ingest attempt, returned by the injected attempt fn.
/// This is the seam that keeps the executor pure and fully testable: tests
/// return a scripted result per conversation; production calls the real
/// single-conversation re-ingest path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttemptResult {
    /// The conversation re-ingested cleanly; the quarantine entry is cleared.
    Reindexed,
    /// The conversation hit an irreducible streaming OOM again; it is
    /// re-quarantined (stamped with the current version → irreducible).
    OutOfMemory,
    /// The attempt failed for another reason; re-quarantined with the reason.
    Failed(String),
}

/// What actually happened to one entry during execution. Snake_case JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryOutcome {
    /// Re-ingested and removed from quarantine.
    RetriedCleared,
    /// OOM'd again; stays quarantined (now same-version irreducible).
    ReQuarantinedOom,
    /// Failed for a non-OOM reason; stays quarantined.
    ReQuarantinedFailed,
    /// Source log gone — skipped without attempting.
    SkippedSourceMissing,
    /// Irreducible same-version — skipped without attempting.
    SkippedIrreducible,
    /// Budget exhausted — deferred to a resume.
    SkippedBudgetExhausted,
}

/// Per-entry execution result. No raw conversation content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryEntryResult {
    pub conversation_id: String,
    pub schema_version: u32,
    pub outcome: RetryOutcome,
    /// Attempt count **before** this pass (so a consumer can see how many
    /// times an entry has already failed without it being mutated under them).
    pub attempt_count_before: u64,
    pub reason: String,
}

/// The executed bounded-retry report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryReport {
    pub current_version: String,
    /// Quarantine size before the pass.
    pub total_quarantined_before: usize,
    /// Entries an attempt fn was actually invoked for.
    pub attempted: usize,
    pub cleared: usize,
    pub re_quarantined_oom: usize,
    pub re_quarantined_failed: usize,
    pub skipped_source_missing: usize,
    pub skipped_irreducible: usize,
    pub skipped_budget_exhausted: usize,
    /// Quarantine size after the pass (`cleared` fewer than before).
    pub remaining_quarantined: usize,
    /// True when at least one entry was cleared this pass.
    pub made_progress: bool,
    /// True when entries were attempted but none cleared — retrying is not
    /// helping, so the operator should inspect rather than loop. This is the
    /// no-progress/stall signal the bead requires.
    pub stalled: bool,
    /// True when eligible entries remain that a resume could attempt.
    pub resume_recommended: bool,
    pub entries: Vec<RetryEntryResult>,
    pub summary: String,
    pub next_safe_command: String,
}

/// Whether an entry should be attempted under `config`, ignoring source
/// presence and budget (those are separate, higher-priority skip reasons).
fn entry_is_attemptable(record_is_stale: bool, eligible_only: bool) -> bool {
    // Force-retry override: attempt everything. Otherwise only version-eligible.
    !eligible_only || record_is_stale
}

/// Whether the per-pass attempt budget is already spent.
fn budget_reached(planned_so_far: usize, max_attempts: Option<usize>) -> bool {
    matches!(max_attempts, Some(max) if planned_so_far >= max)
}

/// Build the dry-run plan for a bounded retry pass. Pure: no mutation, no I/O.
///
/// - `current_version`: the running `cass` version, for eligibility.
/// - `source_missing_ids`: conversation ids whose source path is gone (the
///   caller joins this from the source table; empty when not evaluated). A
///   source-missing entry can never be re-ingested, so it is skipped *before*
///   eligibility or budget are even considered.
#[must_use]
pub fn plan_retry(
    state: &QuarantineState,
    current_version: &str,
    config: &RetryConfig,
    source_missing_ids: &BTreeSet<String>,
) -> RetryPlan {
    let mut entries = Vec::new();
    let mut planned_attempts = 0usize;
    let mut skip_irreducible = 0usize;
    let mut skip_source_missing = 0usize;
    let mut skip_budget_exhausted = 0usize;
    let mut eligible_total = 0usize;
    let mut total = 0usize;

    for (key, record) in state.iter() {
        total += 1;
        let is_stale = record.is_version_stale_for_retry(current_version);
        if is_stale {
            eligible_total += 1;
        }
        let source_missing = source_missing_ids.contains(&key.conversation_id);

        // Skip priority: source-missing (can never retry) > not-attemptable
        // (irreducible, or filtered out) > budget-exhausted > retry.
        let disposition = if source_missing {
            skip_source_missing += 1;
            PlannedDisposition::SkipSourceMissing
        } else if !entry_is_attemptable(is_stale, config.eligible_only) {
            skip_irreducible += 1;
            PlannedDisposition::SkipIrreducible
        } else if budget_reached(planned_attempts, config.max_attempts) {
            skip_budget_exhausted += 1;
            PlannedDisposition::SkipBudgetExhausted
        } else {
            planned_attempts += 1;
            PlannedDisposition::Retry
        };

        entries.push(PlannedEntry {
            conversation_id: key.conversation_id.clone(),
            schema_version: key.schema_version,
            cass_version_at_quarantine: record.cass_version_at_quarantine.clone(),
            disposition,
            attempt_count: record.attempt_count,
            last_reason: record.last_reason.clone(),
        });
    }

    let resume_recommended = skip_budget_exhausted > 0;
    let summary = plan_summary(
        total,
        planned_attempts,
        skip_irreducible,
        skip_source_missing,
        skip_budget_exhausted,
    );
    let next_safe_command = plan_next_command(total, planned_attempts, skip_irreducible);

    RetryPlan {
        current_version: current_version.to_string(),
        total_quarantined: total,
        eligible_total,
        planned_attempts,
        skip_irreducible,
        skip_source_missing,
        skip_budget_exhausted,
        entries,
        resume_recommended,
        summary,
        next_safe_command,
    }
}

/// Execute a bounded retry pass, mutating `state` in place and invoking
/// `attempt` once per [`PlannedDisposition::Retry`] entry.
///
/// `attempt` is the seam to the real re-ingest path; it receives the entry's
/// [`QuarantineKey`] and returns an [`AttemptResult`]. On `Reindexed` the entry
/// is cleared; on `OutOfMemory`/`Failed` it is re-quarantined via
/// [`QuarantineState::record_attempt`] (which stamps the current version,
/// demoting it to irreducible same-version so the next pass suppresses it). The
/// caller is responsible for persisting `state` afterwards via
/// [`QuarantineState::save`] — that persisted state is the resume checkpoint.
pub fn execute_retry<F>(
    state: &mut QuarantineState,
    current_version: &str,
    config: &RetryConfig,
    source_missing_ids: &BTreeSet<String>,
    now: DateTime<Utc>,
    mut attempt: F,
) -> RetryReport
where
    F: FnMut(&QuarantineKey) -> AttemptResult,
{
    let plan = plan_retry(state, current_version, config, source_missing_ids);
    let total_before = plan.total_quarantined;

    let mut entries = Vec::with_capacity(plan.entries.len());
    let mut attempted = 0usize;
    let mut cleared = 0usize;
    let mut re_quarantined_oom = 0usize;
    let mut re_quarantined_failed = 0usize;

    for planned in &plan.entries {
        let key = QuarantineKey::new(planned.conversation_id.clone(), planned.schema_version);
        let (outcome, reason) = match planned.disposition {
            PlannedDisposition::SkipSourceMissing => (
                RetryOutcome::SkippedSourceMissing,
                planned.last_reason.clone(),
            ),
            PlannedDisposition::SkipIrreducible => (
                RetryOutcome::SkippedIrreducible,
                planned.last_reason.clone(),
            ),
            PlannedDisposition::SkipBudgetExhausted => (
                RetryOutcome::SkippedBudgetExhausted,
                planned.last_reason.clone(),
            ),
            PlannedDisposition::Retry => {
                attempted += 1;
                match attempt(&key) {
                    AttemptResult::Reindexed => {
                        state.clear(&key);
                        cleared += 1;
                        (RetryOutcome::RetriedCleared, planned.last_reason.clone())
                    }
                    AttemptResult::OutOfMemory => {
                        state.record_attempt(&key, "ingest_oom", now);
                        re_quarantined_oom += 1;
                        (RetryOutcome::ReQuarantinedOom, "ingest_oom".to_string())
                    }
                    AttemptResult::Failed(failure_reason) => {
                        state.record_attempt(&key, failure_reason.clone(), now);
                        re_quarantined_failed += 1;
                        (RetryOutcome::ReQuarantinedFailed, failure_reason)
                    }
                }
            }
        };

        entries.push(RetryEntryResult {
            conversation_id: planned.conversation_id.clone(),
            schema_version: planned.schema_version,
            outcome,
            attempt_count_before: planned.attempt_count,
            reason,
        });
    }

    let remaining_quarantined = state.len();
    let made_progress = cleared > 0;
    // Attempted-but-cleared-nothing means retrying is not helping right now.
    let stalled = attempted > 0 && cleared == 0;
    let resume_recommended = plan.skip_budget_exhausted > 0;
    let summary = exec_summary(
        attempted,
        cleared,
        re_quarantined_oom,
        re_quarantined_failed,
        remaining_quarantined,
    );
    let next_safe_command = exec_next_command(remaining_quarantined, stalled, resume_recommended);

    RetryReport {
        current_version: current_version.to_string(),
        total_quarantined_before: total_before,
        attempted,
        cleared,
        re_quarantined_oom,
        re_quarantined_failed,
        skipped_source_missing: plan.skip_source_missing,
        skipped_irreducible: plan.skip_irreducible,
        skipped_budget_exhausted: plan.skip_budget_exhausted,
        remaining_quarantined,
        made_progress,
        stalled,
        resume_recommended,
        entries,
        summary,
        next_safe_command,
    }
}

fn plan_summary(
    total: usize,
    planned_attempts: usize,
    skip_irreducible: usize,
    skip_source_missing: usize,
    skip_budget_exhausted: usize,
) -> String {
    if total == 0 {
        return "no quarantined conversations to retry".to_string();
    }
    format!(
        "{planned_attempts} of {total} entries planned for retry; \
         {skip_irreducible} irreducible, {skip_source_missing} source-missing, \
         {skip_budget_exhausted} deferred by budget"
    )
}

fn plan_next_command(total: usize, planned_attempts: usize, skip_irreducible: usize) -> String {
    if total == 0 {
        // Nothing quarantined — readiness is the right place to look next.
        "cass status --json".to_string()
    } else if planned_attempts > 0 {
        // Re-running the index re-attempts eligible (legacy / version-stale)
        // entries and clears them on success — the existing, non-destructive
        // retry trigger (matching `.3.1`'s eligible-case recommendation). A
        // deferred-by-budget remainder is drained by re-running the same
        // command (the durable quarantine_state.json is the checkpoint).
        "cass index".to_string()
    } else if skip_irreducible > 0 {
        // Everything left is irreducible same-version — inspect, do not loop.
        "cass diag --json --quarantine".to_string()
    } else {
        // Only source-missing entries remain; nothing a retry can fix.
        "cass diag --json --quarantine".to_string()
    }
}

fn exec_summary(
    attempted: usize,
    cleared: usize,
    re_quarantined_oom: usize,
    re_quarantined_failed: usize,
    remaining: usize,
) -> String {
    format!(
        "attempted {attempted}, cleared {cleared}, re-quarantined \
         {re_quarantined_oom} (oom) + {re_quarantined_failed} (other); \
         {remaining} still quarantined"
    )
}

fn exec_next_command(remaining: usize, stalled: bool, resume_recommended: bool) -> String {
    if remaining == 0 {
        // Quarantine drained — confirm readiness.
        "cass status --json".to_string()
    } else if stalled {
        // Retrying is not helping; inspect before looping further.
        "cass diag --json --quarantine".to_string()
    } else if resume_recommended {
        // Eligible work remains under the budget cap; re-running the index
        // resumes the bounded retry (durable state is the checkpoint).
        "cass index".to_string()
    } else {
        // Progress made and nothing eligible deferred; the leftover is
        // irreducible / source-missing. Inspect it, do not retry blindly.
        "cass diag --json --quarantine".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexer::quarantine::QuarantineRecord;
    use chrono::DateTime;

    const CURRENT: &str = "0.6.17";

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

    /// `storage_key()` is `"{conversation_id}::v{schema_version}"`.
    fn insert(state: &mut QuarantineState, conv: &str, schema: u32, rec: QuarantineRecord) {
        state.entries.insert(format!("{conv}::v{schema}"), rec);
    }

    /// 2 same-version irreducible (ingest_oom), 1 legacy eligible, 1
    /// version-stale eligible (different cause).
    fn mixed_state() -> QuarantineState {
        let mut s = QuarantineState::default();
        insert(
            &mut s,
            "c-same-1",
            3,
            record(Some(CURRENT), "ingest_oom", 4),
        );
        insert(
            &mut s,
            "c-same-2",
            3,
            record(Some(CURRENT), "ingest_oom", 7),
        );
        insert(&mut s, "c-legacy", 1, record(None, "ingest_oom", 1));
        insert(
            &mut s,
            "c-stale",
            2,
            record(Some("0.5.1"), "validation_failed", 2),
        );
        s
    }

    fn no_missing() -> BTreeSet<String> {
        BTreeSet::new()
    }

    // ---- plan (dry-run) ----

    #[test]
    fn plan_classifies_eligible_irreducible_and_source_missing() {
        let plan = plan_retry(
            &mixed_state(),
            CURRENT,
            &RetryConfig::default(),
            &no_missing(),
        );
        assert_eq!(plan.total_quarantined, 4);
        // legacy + stale are eligible; the two same-version are irreducible.
        assert_eq!(plan.eligible_total, 2);
        assert_eq!(plan.planned_attempts, 2);
        assert_eq!(plan.skip_irreducible, 2);
        assert_eq!(plan.skip_source_missing, 0);
        assert_eq!(plan.skip_budget_exhausted, 0);
        assert!(!plan.resume_recommended);
    }

    #[test]
    fn plan_skips_source_missing_before_eligibility() {
        let mut missing = BTreeSet::new();
        // c-legacy would otherwise be eligible; source-missing wins.
        missing.insert("c-legacy".to_string());
        let plan = plan_retry(&mixed_state(), CURRENT, &RetryConfig::default(), &missing);
        assert_eq!(plan.skip_source_missing, 1);
        // Only c-stale remains attemptable now.
        assert_eq!(plan.planned_attempts, 1);
        let legacy = plan
            .entries
            .iter()
            .find(|e| e.conversation_id == "c-legacy")
            .expect("legacy entry present");
        assert_eq!(legacy.disposition, PlannedDisposition::SkipSourceMissing);
    }

    #[test]
    fn plan_budget_defers_eligible_entries_and_recommends_resume() {
        let config = RetryConfig {
            max_attempts: Some(1),
            eligible_only: true,
        };
        let plan = plan_retry(&mixed_state(), CURRENT, &config, &no_missing());
        // 2 eligible, but only 1 attempt budgeted: 1 retry + 1 deferred.
        assert_eq!(plan.planned_attempts, 1);
        assert_eq!(plan.skip_budget_exhausted, 1);
        assert!(plan.resume_recommended);
    }

    #[test]
    fn plan_force_override_attempts_irreducible_entries() {
        let config = RetryConfig {
            max_attempts: None,
            eligible_only: false,
        };
        let plan = plan_retry(&mixed_state(), CURRENT, &config, &no_missing());
        // With the override, all 4 (none source-missing) are attempted.
        assert_eq!(plan.planned_attempts, 4);
        assert_eq!(plan.skip_irreducible, 0);
    }

    #[test]
    fn plan_empty_state_recommends_status() {
        let plan = plan_retry(
            &QuarantineState::default(),
            CURRENT,
            &RetryConfig::default(),
            &no_missing(),
        );
        assert_eq!(plan.total_quarantined, 0);
        assert_eq!(plan.next_safe_command, "cass status --json");
        assert!(plan.summary.contains("no quarantined"));
    }

    #[test]
    fn plan_all_irreducible_recommends_inspection_not_retry() {
        let mut s = QuarantineState::default();
        for i in 0..133 {
            insert(
                &mut s,
                &format!("c{i}"),
                3,
                record(Some(CURRENT), "ingest_oom", 5),
            );
        }
        let plan = plan_retry(&s, CURRENT, &RetryConfig::default(), &no_missing());
        assert_eq!(plan.planned_attempts, 0);
        assert_eq!(plan.skip_irreducible, 133);
        assert_eq!(plan.next_safe_command, "cass diag --json --quarantine");
    }

    #[test]
    fn plan_entries_are_in_deterministic_storage_key_order() {
        let plan = plan_retry(
            &mixed_state(),
            CURRENT,
            &RetryConfig::default(),
            &no_missing(),
        );
        let ids: Vec<&str> = plan
            .entries
            .iter()
            .map(|e| e.conversation_id.as_str())
            .collect();
        assert_eq!(ids, vec!["c-legacy", "c-same-1", "c-same-2", "c-stale"]);
    }

    // ---- execute ----

    /// Successful retry removal: an eligible entry that re-ingests cleanly is
    /// cleared from quarantine.
    #[test]
    fn execute_clears_eligible_entry_on_success() {
        let mut state = mixed_state();
        let report = execute_retry(
            &mut state,
            CURRENT,
            &RetryConfig::default(),
            &no_missing(),
            ts(1_800_000_000),
            // Every attempted (eligible) entry re-ingests cleanly.
            |_key| AttemptResult::Reindexed,
        );
        assert_eq!(report.attempted, 2);
        assert_eq!(report.cleared, 2);
        assert!(report.made_progress);
        assert!(!report.stalled);
        // The two eligible entries are gone; the two irreducible remain.
        assert_eq!(report.remaining_quarantined, 2);
        assert!(!state.entries.contains_key("c-legacy::v1"));
        assert!(!state.entries.contains_key("c-stale::v2"));
        assert!(state.entries.contains_key("c-same-1::v3"));
    }

    /// Repeated same-version suppression: irreducible same-version entries are
    /// never attempted, so a second pass keeps suppressing them.
    #[test]
    fn execute_suppresses_irreducible_same_version() {
        let mut state = mixed_state();
        let mut attempted_ids: Vec<String> = Vec::new();
        let report = execute_retry(
            &mut state,
            CURRENT,
            &RetryConfig::default(),
            &no_missing(),
            ts(1_800_000_000),
            |key| {
                attempted_ids.push(key.conversation_id.clone());
                AttemptResult::Reindexed
            },
        );
        // Same-version entries were never handed to the attempt fn.
        assert!(!attempted_ids.iter().any(|c| c == "c-same-1"));
        assert!(!attempted_ids.iter().any(|c| c == "c-same-2"));
        assert_eq!(report.skipped_irreducible, 2);
    }

    /// Source-missing skip: an entry whose source log is gone is skipped without
    /// being attempted, even though it is version-eligible.
    #[test]
    fn execute_skips_source_missing_without_attempting() {
        let mut state = mixed_state();
        let mut missing = BTreeSet::new();
        missing.insert("c-legacy".to_string());
        let mut attempted_ids: Vec<String> = Vec::new();
        let report = execute_retry(
            &mut state,
            CURRENT,
            &RetryConfig::default(),
            &missing,
            ts(1_800_000_000),
            |key| {
                attempted_ids.push(key.conversation_id.clone());
                AttemptResult::Reindexed
            },
        );
        assert!(!attempted_ids.iter().any(|c| c == "c-legacy"));
        assert_eq!(report.skipped_source_missing, 1);
        // The source-missing entry is preserved, not deleted.
        assert!(state.entries.contains_key("c-legacy::v1"));
    }

    /// OOM re-quarantine: an entry that OOMs again stays quarantined, its
    /// attempt count advances, and it is stamped with the current version so a
    /// subsequent pass treats it as irreducible same-version.
    #[test]
    fn execute_re_quarantines_on_repeat_oom() {
        let mut state = QuarantineState::default();
        // A single version-stale (eligible) entry that will OOM again.
        insert(
            &mut state,
            "c-oom",
            2,
            record(Some("0.5.1"), "ingest_oom", 3),
        );
        let report = execute_retry(
            &mut state,
            CURRENT,
            &RetryConfig::default(),
            &no_missing(),
            ts(1_800_000_000),
            |_key| AttemptResult::OutOfMemory,
        );
        assert_eq!(report.attempted, 1);
        assert_eq!(report.cleared, 0);
        assert_eq!(report.re_quarantined_oom, 1);
        assert!(report.stalled, "attempted-but-nothing-cleared is a stall");
        assert!(!report.made_progress);
        assert_eq!(report.remaining_quarantined, 1);
        // The re-quarantined entry is now stamped with the current version and
        // its attempt count advanced (4 = 3 before + this pass).
        let rec = state.entries.get("c-oom::v2").expect("entry still present");
        assert_eq!(rec.cass_version_at_quarantine.as_deref(), Some(CURRENT));
        assert_eq!(rec.attempt_count, 4);
        assert!(!rec.is_version_stale_for_retry(CURRENT));
        // The stall recommendation is to inspect, not loop.
        assert_eq!(report.next_safe_command, "cass diag --json --quarantine");

        // Second pass: the same entry is now irreducible same-version and is
        // suppressed — confirming OOM re-quarantine prevents a retry storm. The
        // attempt fn records any call so we can assert it was never invoked
        // (rather than panicking, which trips the no-panic-in-tests gate).
        let mut pass2_attempts: Vec<String> = Vec::new();
        let report2 = execute_retry(
            &mut state,
            CURRENT,
            &RetryConfig::default(),
            &no_missing(),
            ts(1_800_000_100),
            |key| {
                pass2_attempts.push(key.conversation_id.clone());
                AttemptResult::Reindexed
            },
        );
        assert!(
            pass2_attempts.is_empty(),
            "must not attempt an irreducible same-version entry"
        );
        assert_eq!(report2.attempted, 0);
        assert_eq!(report2.skipped_irreducible, 1);
    }

    /// Interrupted retry resume: with a budget of 1, two passes drain a
    /// 2-eligible backlog, each entry attempted exactly once. The durable state
    /// is the only checkpoint — no entry is retried twice, none is missed.
    #[test]
    fn execute_resume_drains_backlog_without_double_attempt() {
        let mut state = QuarantineState::default();
        insert(&mut state, "c-a", 1, record(None, "ingest_oom", 1));
        insert(&mut state, "c-b", 1, record(None, "ingest_oom", 1));
        let config = RetryConfig {
            max_attempts: Some(1),
            eligible_only: true,
        };

        let mut all_attempts: Vec<String> = Vec::new();

        // Pass 1: budget 1 → attempts the first storage-key entry (c-a).
        let r1 = execute_retry(
            &mut state,
            CURRENT,
            &config,
            &no_missing(),
            ts(1_800_000_000),
            |key| {
                all_attempts.push(key.conversation_id.clone());
                AttemptResult::Reindexed
            },
        );
        assert_eq!(r1.attempted, 1);
        assert_eq!(r1.cleared, 1);
        assert_eq!(r1.skipped_budget_exhausted, 1);
        assert!(r1.resume_recommended);
        assert_eq!(r1.next_safe_command, "cass index");
        assert_eq!(state.len(), 1, "one eligible entry remains for the resume");

        // Pass 2 (resume): re-plans against the shrunken state, attempts c-b.
        let r2 = execute_retry(
            &mut state,
            CURRENT,
            &config,
            &no_missing(),
            ts(1_800_000_100),
            |key| {
                all_attempts.push(key.conversation_id.clone());
                AttemptResult::Reindexed
            },
        );
        assert_eq!(r2.attempted, 1);
        assert_eq!(r2.cleared, 1);
        assert_eq!(r2.skipped_budget_exhausted, 0);
        assert!(!r2.resume_recommended);
        assert_eq!(state.len(), 0, "backlog fully drained across two passes");

        // Each entry attempted exactly once, in deterministic order.
        assert_eq!(all_attempts, vec!["c-a".to_string(), "c-b".to_string()]);
        assert_eq!(r2.next_safe_command, "cass status --json");
    }

    #[test]
    fn execute_failed_attempt_re_quarantines_with_reason() {
        let mut state = QuarantineState::default();
        insert(&mut state, "c-fail", 2, record(None, "ingest_oom", 1));
        let report = execute_retry(
            &mut state,
            CURRENT,
            &RetryConfig::default(),
            &no_missing(),
            ts(1_800_000_000),
            |_key| AttemptResult::Failed("schema_decode_error".to_string()),
        );
        assert_eq!(report.re_quarantined_failed, 1);
        assert!(report.stalled);
        let entry = report
            .entries
            .iter()
            .find(|e| e.conversation_id == "c-fail")
            .expect("entry present");
        assert_eq!(entry.outcome, RetryOutcome::ReQuarantinedFailed);
        assert_eq!(entry.reason, "schema_decode_error");
        assert_eq!(entry.attempt_count_before, 1);
    }

    #[test]
    fn execute_mixed_outcomes_count_correctly() {
        // c-a eligible→success, c-b eligible→oom, c-same irreducible→skip.
        let mut state = QuarantineState::default();
        insert(&mut state, "c-a", 1, record(None, "ingest_oom", 1));
        insert(&mut state, "c-b", 1, record(Some("0.5.1"), "ingest_oom", 2));
        insert(
            &mut state,
            "c-same",
            1,
            record(Some(CURRENT), "ingest_oom", 9),
        );
        // Record every attempt so we can assert the irreducible `c-same` was
        // never handed to the attempt fn — without panicking inside it.
        let mut attempts: Vec<String> = Vec::new();
        let report = execute_retry(
            &mut state,
            CURRENT,
            &RetryConfig::default(),
            &no_missing(),
            ts(1_800_000_000),
            |key| {
                attempts.push(key.conversation_id.clone());
                match key.conversation_id.as_str() {
                    "c-b" => AttemptResult::OutOfMemory,
                    // c-a succeeds; an unexpected id (e.g. the irreducible
                    // c-same) also lands here but is caught by the assert below.
                    _ => AttemptResult::Reindexed,
                }
            },
        );
        attempts.sort();
        assert_eq!(
            attempts,
            vec!["c-a".to_string(), "c-b".to_string()],
            "only the two eligible entries are attempted; c-same is suppressed"
        );
        assert_eq!(report.attempted, 2);
        assert_eq!(report.cleared, 1);
        assert_eq!(report.re_quarantined_oom, 1);
        assert_eq!(report.skipped_irreducible, 1);
        assert!(report.made_progress, "one cleared => progress, not stalled");
        assert!(!report.stalled);
        assert_eq!(report.remaining_quarantined, 2);
    }

    // ---- serialization / safety ----

    #[test]
    fn enums_serialize_snake_case() {
        assert_eq!(
            serde_json::to_string(&PlannedDisposition::SkipBudgetExhausted).unwrap(),
            "\"skip_budget_exhausted\""
        );
        assert_eq!(
            serde_json::to_string(&RetryOutcome::ReQuarantinedOom).unwrap(),
            "\"re_quarantined_oom\""
        );
    }

    #[test]
    fn plan_round_trips_through_json() {
        let plan = plan_retry(
            &mixed_state(),
            CURRENT,
            &RetryConfig::default(),
            &no_missing(),
        );
        let json = serde_json::to_string(&plan).unwrap();
        let parsed: RetryPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, plan);
    }

    #[test]
    fn report_round_trips_through_json() {
        let mut state = mixed_state();
        let report = execute_retry(
            &mut state,
            CURRENT,
            &RetryConfig::default(),
            &no_missing(),
            ts(1_800_000_000),
            |_key| AttemptResult::Reindexed,
        );
        let json = serde_json::to_string(&report).unwrap();
        let parsed: RetryReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, report);
    }

    #[test]
    fn config_deserializes_with_defaults() {
        // An empty object must yield the safe defaults (eligible_only=true).
        let config: RetryConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(config, RetryConfig::default());
        assert!(config.eligible_only);
        assert_eq!(config.max_attempts, None);
    }

    #[test]
    fn next_commands_are_never_destructive() {
        // Exercise every command-producing branch and assert all are safe.
        let mut commands: Vec<String> = Vec::new();
        commands.push(plan_next_command(0, 0, 0));
        commands.push(plan_next_command(4, 2, 2));
        commands.push(plan_next_command(4, 1, 0));
        commands.push(plan_next_command(3, 0, 3));
        commands.push(exec_next_command(0, false, false));
        commands.push(exec_next_command(2, true, false));
        commands.push(exec_next_command(2, false, true));
        commands.push(exec_next_command(2, false, false));
        for cmd in &commands {
            assert!(cmd.starts_with("cass "), "must be a cass command: {cmd}");
            for bad in [
                "rm ",
                "--force-clean",
                "--purge",
                "delete ",
                "DROP ",
                "--delete",
                ">",
            ] {
                assert!(!cmd.contains(bad), "command must stay safe: {cmd}");
            }
        }
    }
}
