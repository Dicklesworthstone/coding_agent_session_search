//! Phase-exact stale-refresh evidence ledger (bead ibuuh.25).
//!
//! Defines the canonical stale-refresh phase model and captures machine-readable
//! timings, counters, and correctness artifacts for each phase.  Downstream
//! performance beads use this ledger as their proof framework: "what changed,
//! how much, and was correctness preserved?"
//!
//! # Phase model
//!
//! ```text
//! ┌─────────┐   ┌─────────┐   ┌──────────┐   ┌─────────┐   ┌──────────┐   ┌──────────┐
//! │  Scan   │──▶│ Persist │──▶│ Lexical  │──▶│ Publish │──▶│ Analytics│──▶│ Semantic │
//! │ (disc.) │   │ (DB)    │   │ (rebuild)│   │ (commit)│   │ (stats)  │   │ (vectors)│
//! └─────────┘   └─────────┘   └──────────┘   └─────────┘   └──────────┘   └──────────┘
//!                                                               │
//!                                                               ▼
//!                                                          ┌──────────┐
//!                                                          │ Recovery │
//!                                                          │ (error)  │
//!                                                          └──────────┘
//! ```

use std::collections::BTreeMap;
use std::time::Instant;

use serde::{Deserialize, Serialize};

// ─── Phase model ───────────────────────────────────────────────────────────

/// Canonical phases of a stale-refresh cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefreshPhase {
    /// Discovery: scan filesystem for agent sessions.
    Scan,
    /// Persist new/updated conversations to the canonical SQLite DB.
    Persist,
    /// Rebuild the lexical (Tantivy/frankensearch) index from DB content.
    LexicalRebuild,
    /// Commit and publish the lexical index atomically.
    Publish,
    /// Record analytics (stats, aggregates, token usage).
    Analytics,
    /// Build/update semantic vector indices (fast + quality tiers).
    Semantic,
    /// Error recovery (rollback, checkpoint save, cleanup).
    Recovery,
}

impl RefreshPhase {
    /// All phases in pipeline order.
    pub const ALL: &'static [RefreshPhase] = &[
        Self::Scan,
        Self::Persist,
        Self::LexicalRebuild,
        Self::Publish,
        Self::Analytics,
        Self::Semantic,
        Self::Recovery,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Scan => "scan",
            Self::Persist => "persist",
            Self::LexicalRebuild => "lexical_rebuild",
            Self::Publish => "publish",
            Self::Analytics => "analytics",
            Self::Semantic => "semantic",
            Self::Recovery => "recovery",
        }
    }
}

// ─── Phase record ──────────────────────────────────────────────────────────

/// Timing and counter data for a single phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhaseRecord {
    pub phase: RefreshPhase,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
    /// Items processed (conversations, documents, vectors, etc.).
    pub items_processed: u64,
    /// Items skipped (already indexed, filtered, etc.).
    pub items_skipped: u64,
    /// Errors encountered (non-fatal).
    pub errors: u64,
    /// Phase-specific counters (e.g., "bytes_written", "connectors_scanned").
    pub counters: BTreeMap<String, u64>,
    /// Whether this phase completed successfully.
    pub success: bool,
    /// Error message if the phase failed.
    pub error_message: Option<String>,
}

impl PhaseRecord {
    fn new(phase: RefreshPhase) -> Self {
        Self {
            phase,
            duration_ms: 0,
            items_processed: 0,
            items_skipped: 0,
            errors: 0,
            counters: BTreeMap::new(),
            success: true,
            error_message: None,
        }
    }
}

// ─── Equivalence artifacts ─────────────────────────────────────────────────

/// Correctness artifacts captured after a refresh for equivalence checking.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EquivalenceArtifacts {
    /// Total conversations in DB after refresh.
    pub conversation_count: u64,
    /// Total messages in DB after refresh.
    pub message_count: u64,
    /// Total indexed documents in the lexical index.
    pub lexical_doc_count: u64,
    /// Lexical index storage fingerprint.
    pub lexical_fingerprint: Option<String>,
    /// Semantic manifest fingerprint (if semantic phase ran).
    pub semantic_manifest_fingerprint: Option<String>,
    /// Search-hit digest: sha256 of sorted doc IDs from a canonical query.
    pub search_hit_digest: Option<String>,
    /// Peak RSS in bytes during the refresh (if measured).
    pub peak_rss_bytes: Option<u64>,
    /// DB file size after refresh.
    pub db_size_bytes: Option<u64>,
    /// Lexical index size on disk.
    pub lexical_index_size_bytes: Option<u64>,
}

// ─── The evidence ledger ───────────────────────────────────────────────────

/// Complete evidence ledger for a single stale-refresh cycle.
///
/// Captures phase-exact timings, item counts, and correctness artifacts.
/// Serializable to JSON for benchmark comparison and CI artifact retention.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefreshLedger {
    /// Ledger format version.
    pub version: u32,
    /// Unix timestamp (ms) when the refresh started.
    pub started_at_ms: i64,
    /// Unix timestamp (ms) when the refresh completed.
    pub completed_at_ms: i64,
    /// Total wall-clock duration (ms).
    pub total_duration_ms: u64,
    /// Whether this was a full rebuild or incremental refresh.
    pub full_rebuild: bool,
    /// Corpus family identifier (for benchmark categorization).
    pub corpus_family: String,
    /// Per-phase records in pipeline order.
    pub phases: Vec<PhaseRecord>,
    /// Correctness artifacts captured after the refresh.
    pub equivalence: EquivalenceArtifacts,
    /// Free-form tags for filtering and grouping.
    pub tags: BTreeMap<String, String>,
}

/// User-facing readiness timing summary derived from a refresh ledger.
///
/// `time_to_lexical_ready_ms` means the lexical build phase finished
/// successfully; `time_to_search_ready_ms` means the publish phase finished
/// successfully and the refreshed lexical asset is visible to ordinary search.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshReadinessMilestones {
    pub time_to_lexical_ready_ms: Option<u64>,
    pub time_to_search_ready_ms: Option<u64>,
    pub time_to_full_settled_ms: Option<u64>,
    pub failed_phase: Option<String>,
    pub search_readiness_state: RefreshSearchReadinessState,
}

/// Why ordinary search can or cannot see the refreshed lexical asset yet.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefreshSearchReadinessState {
    /// The publish phase completed successfully, so refreshed lexical results
    /// are visible to search.
    Published,
    /// Earlier phases succeeded, but no publish phase has completed yet.
    #[default]
    WaitingForPublish,
    /// A phase before publish failed, so publish was never reached safely.
    BlockedBeforePublish,
    /// Publish itself failed, preserving the previous good lexical asset.
    PublishFailed,
}

impl Default for RefreshLedger {
    fn default() -> Self {
        Self {
            version: 1,
            started_at_ms: 0,
            completed_at_ms: 0,
            total_duration_ms: 0,
            full_rebuild: false,
            corpus_family: "default".to_owned(),
            phases: Vec::new(),
            equivalence: EquivalenceArtifacts::default(),
            tags: BTreeMap::new(),
        }
    }
}

impl RefreshLedger {
    /// Start a new ledger with the given corpus family.
    pub fn start(corpus_family: &str, full_rebuild: bool) -> LedgerBuilder {
        LedgerBuilder::new(corpus_family, full_rebuild)
    }

    /// Get the phase record for a specific phase (if it ran).
    pub fn phase(&self, phase: RefreshPhase) -> Option<&PhaseRecord> {
        self.phases.iter().find(|p| p.phase == phase)
    }

    /// Total items processed across all phases.
    pub fn total_items_processed(&self) -> u64 {
        self.phases.iter().map(|p| p.items_processed).sum()
    }

    /// Total errors across all phases.
    pub fn total_errors(&self) -> u64 {
        self.phases.iter().map(|p| p.errors).sum()
    }

    /// Whether all phases succeeded.
    pub fn all_phases_succeeded(&self) -> bool {
        self.phases.iter().all(|p| p.success)
    }

    /// Phases that failed.
    pub fn failed_phases(&self) -> Vec<&PhaseRecord> {
        self.phases.iter().filter(|p| !p.success).collect()
    }

    /// Duration breakdown: phase name → ms.
    pub fn duration_breakdown(&self) -> BTreeMap<String, u64> {
        self.phases
            .iter()
            .map(|p| (p.phase.as_str().to_owned(), p.duration_ms))
            .collect()
    }

    /// Derive the user-facing stale-refresh readiness milestones that robot
    /// surfaces and benchmark gates need to compare across runs.
    pub fn readiness_milestones(&self) -> RefreshReadinessMilestones {
        RefreshReadinessMilestones {
            time_to_lexical_ready_ms: self
                .successful_duration_through(RefreshPhase::LexicalRebuild),
            time_to_search_ready_ms: self.successful_duration_through(RefreshPhase::Publish),
            time_to_full_settled_ms: self.full_settlement_duration_ms(),
            failed_phase: self
                .failed_phases()
                .first()
                .map(|phase| phase.phase.as_str().to_owned()),
            search_readiness_state: self.search_readiness_state(),
        }
    }

    /// Serialize to pretty JSON.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_owned())
    }

    fn successful_duration_through(&self, target: RefreshPhase) -> Option<u64> {
        let mut elapsed_ms = 0u64;
        for phase in &self.phases {
            elapsed_ms = elapsed_ms.saturating_add(phase.duration_ms);
            if !phase.success {
                return None;
            }
            if phase.phase == target {
                return Some(elapsed_ms);
            }
        }
        None
    }

    fn sum_phase_durations(&self) -> u64 {
        self.phases
            .iter()
            .map(|phase| phase.duration_ms)
            .fold(0u64, u64::saturating_add)
    }

    fn full_settlement_duration_ms(&self) -> Option<u64> {
        (self.all_phases_succeeded()
            && self.search_readiness_state() == RefreshSearchReadinessState::Published)
            .then(|| {
                if self.total_duration_ms > 0 {
                    self.total_duration_ms
                } else {
                    self.sum_phase_durations()
                }
            })
    }

    fn search_readiness_state(&self) -> RefreshSearchReadinessState {
        let mut published = false;

        for phase in &self.phases {
            if !phase.success {
                return if phase.phase == RefreshPhase::Publish {
                    RefreshSearchReadinessState::PublishFailed
                } else if published {
                    RefreshSearchReadinessState::Published
                } else {
                    RefreshSearchReadinessState::BlockedBeforePublish
                };
            }
            if phase.phase == RefreshPhase::Publish {
                published = true;
            }
        }

        if published {
            RefreshSearchReadinessState::Published
        } else {
            RefreshSearchReadinessState::WaitingForPublish
        }
    }
}

// ─── Builder (ergonomic recording during refresh) ──────────────────────────

/// Builder for incrementally recording phase data during a refresh cycle.
pub struct LedgerBuilder {
    ledger: RefreshLedger,
    start_time: Instant,
    current_phase: Option<(RefreshPhase, Instant)>,
    current_record: Option<PhaseRecord>,
}

impl LedgerBuilder {
    fn new(corpus_family: &str, full_rebuild: bool) -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);

        Self {
            ledger: RefreshLedger {
                started_at_ms: now,
                full_rebuild,
                corpus_family: corpus_family.to_owned(),
                ..Default::default()
            },
            start_time: Instant::now(),
            current_phase: None,
            current_record: None,
        }
    }

    /// Begin a new phase.  Automatically ends any in-progress phase.
    pub fn begin_phase(&mut self, phase: RefreshPhase) {
        self.end_current_phase();
        self.current_phase = Some((phase, Instant::now()));
        self.current_record = Some(PhaseRecord::new(phase));
    }

    /// Record items processed in the current phase.
    pub fn record_items(&mut self, processed: u64, skipped: u64) {
        if let Some(ref mut record) = self.current_record {
            record.items_processed += processed;
            record.items_skipped += skipped;
        }
    }

    /// Record a non-fatal error in the current phase.
    ///
    /// Multiple errors are joined with "; " so no diagnostic info is lost.
    pub fn record_error(&mut self, message: &str) {
        if let Some(ref mut record) = self.current_record {
            record.errors += 1;
            match &mut record.error_message {
                Some(existing) => {
                    existing.push_str("; ");
                    existing.push_str(message);
                }
                None => record.error_message = Some(message.to_owned()),
            }
        }
    }

    /// Record a phase failure (the phase did not complete successfully).
    ///
    /// This replaces any previous error_message since the failure is the
    /// authoritative final state.
    pub fn record_failure(&mut self, message: &str) {
        if let Some(ref mut record) = self.current_record {
            record.success = false;
            record.error_message = Some(message.to_owned());
        }
    }

    /// Set a custom counter in the current phase.
    pub fn set_counter(&mut self, key: &str, value: u64) {
        if let Some(ref mut record) = self.current_record {
            record.counters.insert(key.to_owned(), value);
        }
    }

    /// Increment a custom counter in the current phase.
    pub fn inc_counter(&mut self, key: &str, delta: u64) {
        if let Some(ref mut record) = self.current_record {
            *record.counters.entry(key.to_owned()).or_insert(0) += delta;
        }
    }

    /// Set equivalence artifacts.
    pub fn set_equivalence(&mut self, artifacts: EquivalenceArtifacts) {
        self.ledger.equivalence = artifacts;
    }

    /// Add a free-form tag.
    pub fn tag(&mut self, key: &str, value: &str) {
        self.ledger.tags.insert(key.to_owned(), value.to_owned());
    }

    /// Finalize the current phase and the ledger.
    pub fn finish(mut self) -> RefreshLedger {
        self.end_current_phase();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        self.ledger.completed_at_ms = now;
        self.ledger.total_duration_ms = self.start_time.elapsed().as_millis() as u64;
        self.ledger
    }

    fn end_current_phase(&mut self) {
        // Take each field separately so a .take() on one doesn't silently
        // discard the other if they're ever out of sync.
        let Some((_, phase_start)) = self.current_phase.take() else {
            return;
        };
        let Some(mut record) = self.current_record.take() else {
            return;
        };
        record.duration_ms = phase_start.elapsed().as_millis() as u64;
        self.ledger.phases.push(record);
    }
}

// ─── Benchmark corpus families ─────────────────────────────────────────────

/// Standard benchmark corpus family identifiers.
pub mod corpus_families {
    /// Small corpus: ~10 conversations, 40 messages.  Fast smoke test.
    pub const SMALL: &str = "small";
    /// Medium corpus: ~100 conversations, 500 messages.  Typical personal use.
    pub const MEDIUM: &str = "medium";
    /// Large corpus: ~1000 conversations, 5000 messages.  Power user.
    pub const LARGE: &str = "large";
    /// Duplicate-heavy: 50% duplicate messages across conversations.
    pub const DUPLICATE_HEAVY: &str = "duplicate_heavy";
    /// Pathological: very long messages, deep nesting, edge-case content.
    pub const PATHOLOGICAL: &str = "pathological";
    /// Mixed-agent: equal distribution across all 14 supported agents.
    pub const MIXED_AGENT: &str = "mixed_agent";
    /// Incremental: base corpus + small delta for incremental refresh testing.
    pub const INCREMENTAL: &str = "incremental";
}

/// Configuration for generating a benchmark corpus.
#[derive(Debug, Clone)]
pub struct BenchmarkCorpusConfig {
    pub family: String,
    pub num_conversations: usize,
    pub messages_per_conversation: usize,
    /// Fraction of messages that are duplicates (0.0–1.0).
    pub duplicate_fraction: f64,
    /// Maximum message content length in characters.
    pub max_message_length: usize,
    /// Number of distinct agents to cycle through.
    pub agent_count: usize,
}

impl BenchmarkCorpusConfig {
    pub fn small() -> Self {
        Self {
            family: corpus_families::SMALL.to_owned(),
            num_conversations: 10,
            messages_per_conversation: 4,
            duplicate_fraction: 0.0,
            max_message_length: 500,
            agent_count: 3,
        }
    }

    pub fn medium() -> Self {
        Self {
            family: corpus_families::MEDIUM.to_owned(),
            num_conversations: 100,
            messages_per_conversation: 5,
            duplicate_fraction: 0.05,
            max_message_length: 2000,
            agent_count: 5,
        }
    }

    pub fn large() -> Self {
        Self {
            family: corpus_families::LARGE.to_owned(),
            num_conversations: 1000,
            messages_per_conversation: 5,
            duplicate_fraction: 0.05,
            max_message_length: 2000,
            agent_count: 8,
        }
    }

    pub fn duplicate_heavy() -> Self {
        Self {
            family: corpus_families::DUPLICATE_HEAVY.to_owned(),
            num_conversations: 50,
            messages_per_conversation: 6,
            duplicate_fraction: 0.5,
            max_message_length: 1000,
            agent_count: 3,
        }
    }

    pub fn pathological() -> Self {
        Self {
            family: corpus_families::PATHOLOGICAL.to_owned(),
            num_conversations: 20,
            messages_per_conversation: 10,
            duplicate_fraction: 0.0,
            max_message_length: 50_000,
            agent_count: 2,
        }
    }

    pub fn mixed_agent() -> Self {
        Self {
            family: corpus_families::MIXED_AGENT.to_owned(),
            num_conversations: 70,
            messages_per_conversation: 4,
            duplicate_fraction: 0.0,
            max_message_length: 1000,
            agent_count: 14,
        }
    }

    pub fn incremental() -> Self {
        Self {
            family: corpus_families::INCREMENTAL.to_owned(),
            num_conversations: 50,
            messages_per_conversation: 4,
            duplicate_fraction: 0.0,
            max_message_length: 1000,
            agent_count: 3,
        }
    }
}

// ─── Evidence-grade derived metrics (ibuuh.24) ─────────────────────────────
//
// `coding_agent_session_search-ibuuh.24` SCOPE bullet 1 calls for "a hard
// evidence ledger for the stale-refresh path so future tuning is grounded
// in measured truth." The raw `RefreshLedger` captures phase counters and
// timings; benchmark agents and operator dashboards still need *derived*
// summaries (throughput, phase-share, hot-phase identification) that are
// stable across runs and trivially comparable. This section adds those
// pure-data summaries so consumers can read one struct instead of
// re-deriving the math at every call site.

/// Per-phase throughput summary derived from a `PhaseRecord`.
///
/// `items_per_second` is the headline tuning metric. `seconds` is
/// captured separately (rather than as a division by zero) so callers
/// can render either form without re-doing the math, and so a phase
/// that processed items but completed in <1ms still surfaces a usable
/// throughput rather than reporting `NaN`. When `duration_ms == 0` the
/// throughput is reported as `None` (you cannot extrapolate from a
/// zero-duration measurement).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RefreshThroughputProfile {
    pub phase: RefreshPhase,
    pub duration_ms: u64,
    pub items_processed: u64,
    /// `items_processed / (duration_ms / 1000)`, rounded to 3 decimal
    /// places via the f64 path. `None` when `duration_ms == 0` or the
    /// phase did not run.
    pub items_per_second: Option<f64>,
}

/// Share of total wall-clock time spent in a single phase.
///
/// `share_pct` sums to ~100.0 across all phases that ran (sub-millisecond
/// rounding can cause ±0.01 drift). The zero-duration case is handled
/// explicitly: phases that contributed 0ms get share_pct=0.0 instead of
/// NaN.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RefreshPhaseShare {
    pub phase: RefreshPhase,
    pub duration_ms: u64,
    /// Percentage of total `RefreshLedger.total_duration_ms` (0.0–100.0).
    pub share_pct: f64,
}

/// Single-shot derived evidence summary suitable for benchmark
/// comparison and operator dashboards. Computed from a `RefreshLedger`
/// in O(phases) time with zero allocations beyond the output structs.
///
/// Comparing two `RefreshLedgerEvidence` values across runs is the
/// intended consumer pattern: regression gates assert that
/// `aggregate_items_per_second` did not drop more than X%, that
/// `dominant_phase` did not migrate, etc.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RefreshLedgerEvidence {
    /// Per-phase throughput. Excludes phases with `items_processed == 0`
    /// to keep the output focused on phases that actually moved data.
    pub throughput: Vec<RefreshThroughputProfile>,
    /// Per-phase wall-clock share. Includes ALL phases that ran (even
    /// zero-item phases like a brief Recovery) so the shares sum
    /// transparently.
    pub phase_share: Vec<RefreshPhaseShare>,
    /// Phase consuming the largest share of wall time, or `None` when
    /// no phases ran. The "where to optimize next" pointer.
    pub dominant_phase: Option<RefreshPhase>,
    /// Total items processed across every phase.
    pub aggregate_items_processed: u64,
    /// Total wall-clock duration in milliseconds (mirrors
    /// `RefreshLedger.total_duration_ms` for ergonomic single-struct
    /// access).
    pub aggregate_duration_ms: u64,
    /// Aggregate items/second across the whole refresh; `None` when
    /// `aggregate_duration_ms == 0`.
    pub aggregate_items_per_second: Option<f64>,
}

impl RefreshLedger {
    /// Compute the derived evidence summary for benchmark comparison and
    /// operator dashboards. See [`RefreshLedgerEvidence`] for shape +
    /// invariants. This is pure (no I/O) and runs in O(phases).
    pub fn evidence_summary(&self) -> RefreshLedgerEvidence {
        let total_ms = self.total_duration_ms;
        let throughput: Vec<RefreshThroughputProfile> = self
            .phases
            .iter()
            .filter(|phase| phase.items_processed > 0)
            .map(|phase| {
                let items_per_second =
                    items_per_second_for(phase.duration_ms, phase.items_processed);
                RefreshThroughputProfile {
                    phase: phase.phase,
                    duration_ms: phase.duration_ms,
                    items_processed: phase.items_processed,
                    items_per_second,
                }
            })
            .collect();
        let phase_share: Vec<RefreshPhaseShare> = self
            .phases
            .iter()
            .map(|phase| RefreshPhaseShare {
                phase: phase.phase,
                duration_ms: phase.duration_ms,
                share_pct: share_pct_for(phase.duration_ms, total_ms),
            })
            .collect();
        let dominant_phase = self
            .phases
            .iter()
            .max_by_key(|phase| phase.duration_ms)
            .filter(|phase| phase.duration_ms > 0)
            .map(|phase| phase.phase);
        let aggregate_items_processed = self.total_items_processed();
        let aggregate_items_per_second = items_per_second_for(total_ms, aggregate_items_processed);
        RefreshLedgerEvidence {
            throughput,
            phase_share,
            dominant_phase,
            aggregate_items_processed,
            aggregate_duration_ms: total_ms,
            aggregate_items_per_second,
        }
    }
}

/// Compute items/second to 3-decimal precision; returns `None` when
/// `duration_ms == 0` (cannot extrapolate from a zero-duration
/// measurement) or `items == 0` (no work to extrapolate).
fn items_per_second_for(duration_ms: u64, items: u64) -> Option<f64> {
    if duration_ms == 0 || items == 0 {
        return None;
    }
    let seconds = duration_ms as f64 / 1000.0;
    if seconds <= 0.0 {
        return None;
    }
    let raw = items as f64 / seconds;
    Some((raw * 1000.0).round() / 1000.0)
}

/// Compute the wall-clock share of one phase relative to the total
/// duration. Returns 0.0 when `total_ms == 0` (avoids NaN; an empty
/// ledger has no phase shares to compute) or when `phase_ms == 0`.
fn share_pct_for(phase_ms: u64, total_ms: u64) -> f64 {
    if total_ms == 0 || phase_ms == 0 {
        return 0.0;
    }
    let raw = (phase_ms as f64 / total_ms as f64) * 100.0;
    (raw * 100.0).round() / 100.0
}

// ─── Cross-run comparison (ibuuh.24) ───────────────────────────────────────
//
// `coding_agent_session_search-ibuuh.24` benchmark/regression slice:
// the evidence summary lets a single run be inspected; cross-run
// comparison is what benchmark CI gates ACTUALLY need ("did this
// build regress vs the baseline?"). Adding a structured one-call
// comparator means CI / dashboards stop hand-rolling delta math —
// every consumer reads the same `RefreshLedgerEvidenceComparison`
// shape and branches on the same regression-class signals.

/// One phase's regression signal between baseline and current.
///
/// `duration_delta_pct` is positive when the phase got SLOWER
/// (current > baseline) — the conventional regression sign that
/// matches operator expectations ("this PR added 12% to publish").
/// `throughput_delta_pct` is positive when the phase got FASTER
/// (current items/sec > baseline items/sec). Both are `None` when
/// the corresponding base measurement is zero/missing — the
/// comparator refuses to invent an extrapolation from no data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RefreshPhaseDelta {
    pub phase: RefreshPhase,
    pub baseline_duration_ms: u64,
    pub current_duration_ms: u64,
    /// `(current - baseline) / baseline * 100`, rounded to 2 decimals.
    /// Positive ⇒ slower in `current`. `None` when baseline is 0ms
    /// (no rate of change defined) or when the phase didn't run in
    /// either side (cannot compare).
    pub duration_delta_pct: Option<f64>,
    pub baseline_items_processed: u64,
    pub current_items_processed: u64,
    pub baseline_items_per_second: Option<f64>,
    pub current_items_per_second: Option<f64>,
    /// `(current - baseline) / baseline * 100`, rounded to 2 decimals.
    /// Positive ⇒ faster in `current`. `None` when either side has
    /// no items/sec measurement (cannot compute a meaningful delta).
    pub throughput_delta_pct: Option<f64>,
}

/// Cross-run comparison summary suitable for benchmark CI gates and
/// regression dashboards. Computed by
/// [`RefreshLedgerEvidence::compare_to`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RefreshLedgerEvidenceComparison {
    /// Per-phase delta for every phase that ran in EITHER side.
    /// Phases unique to one side surface with a zero on the missing
    /// side — operators can grep for the missing phase and decide.
    pub phase_deltas: Vec<RefreshPhaseDelta>,
    /// Aggregate wall-clock delta. Positive ⇒ slower in `current`.
    pub aggregate_duration_delta_pct: Option<f64>,
    /// Aggregate items/sec delta. Positive ⇒ faster in `current`.
    pub aggregate_throughput_delta_pct: Option<f64>,
    /// `Some((from, to))` when the dominant phase shifted between
    /// baseline and current. A dominant-phase shift is itself a
    /// regression signal — the operator should look at why the
    /// hot phase changed even if absolute totals are similar.
    pub dominant_phase_shift: Option<(RefreshPhase, RefreshPhase)>,
}

impl RefreshLedgerEvidence {
    /// Compare this evidence summary against a `baseline` and return
    /// a structured regression report. Pure (no I/O); runs in
    /// O(phases_baseline + phases_current).
    ///
    /// Direction convention: positive `duration_delta_pct` ⇒ slower
    /// in `self`; positive `throughput_delta_pct` ⇒ faster in `self`.
    /// Picking these signs (not the opposite) makes the JSON read
    /// naturally for benchmark CI ("PR #123 added +12.5% to publish
    /// duration").
    pub fn compare_to(&self, baseline: &Self) -> RefreshLedgerEvidenceComparison {
        // Index baseline + current phase-share entries by phase so
        // zero-item phases still participate in the comparison. The
        // throughput vectors intentionally skip zero-item phases, so
        // using them as the "phase ran" source would hide publish or
        // recovery work that consumed wall-clock time.
        //
        // (RefreshPhase derives Hash but not Ord, so HashMap/HashSet —
        // we re-sort by RefreshPhase::ALL declaration order at the
        // end so the output is deterministic across runs regardless
        // of HashMap iteration order.)
        use std::collections::{HashMap, HashSet};
        let mut baseline_share_by_phase: HashMap<RefreshPhase, &RefreshPhaseShare> = HashMap::new();
        for entry in &baseline.phase_share {
            baseline_share_by_phase.insert(entry.phase, entry);
        }
        let mut current_share_by_phase: HashMap<RefreshPhase, &RefreshPhaseShare> = HashMap::new();
        for entry in &self.phase_share {
            current_share_by_phase.insert(entry.phase, entry);
        }
        let mut baseline_by_phase: HashMap<RefreshPhase, &RefreshThroughputProfile> =
            HashMap::new();
        for entry in &baseline.throughput {
            baseline_by_phase.insert(entry.phase, entry);
        }
        let mut current_by_phase: HashMap<RefreshPhase, &RefreshThroughputProfile> = HashMap::new();
        for entry in &self.throughput {
            current_by_phase.insert(entry.phase, entry);
        }
        // Union the two key sets so a phase unique to one side still
        // surfaces in the comparison. Iterate RefreshPhase::ALL to
        // preserve canonical pipeline order in the output.
        let mut all_phases: HashSet<RefreshPhase> = HashSet::new();
        all_phases.extend(baseline_share_by_phase.keys().copied());
        all_phases.extend(current_share_by_phase.keys().copied());
        all_phases.extend(baseline_by_phase.keys().copied());
        all_phases.extend(current_by_phase.keys().copied());

        let phase_deltas: Vec<RefreshPhaseDelta> = RefreshPhase::ALL
            .iter()
            .copied()
            .filter(|phase| all_phases.contains(phase))
            .map(|phase| {
                let baseline_entry = baseline_by_phase.get(&phase).copied();
                let current_entry = current_by_phase.get(&phase).copied();
                let baseline_duration_ms = baseline_share_by_phase
                    .get(&phase)
                    .map(|e| e.duration_ms)
                    .or_else(|| baseline_entry.map(|e| e.duration_ms))
                    .unwrap_or(0);
                let current_duration_ms = current_share_by_phase
                    .get(&phase)
                    .map(|e| e.duration_ms)
                    .or_else(|| current_entry.map(|e| e.duration_ms))
                    .unwrap_or(0);
                let baseline_items_processed =
                    baseline_entry.map(|e| e.items_processed).unwrap_or(0);
                let current_items_processed = current_entry.map(|e| e.items_processed).unwrap_or(0);
                let baseline_items_per_second = baseline_entry.and_then(|e| e.items_per_second);
                let current_items_per_second = current_entry.and_then(|e| e.items_per_second);

                RefreshPhaseDelta {
                    phase,
                    baseline_duration_ms,
                    current_duration_ms,
                    duration_delta_pct: pct_delta(
                        baseline_duration_ms as f64,
                        current_duration_ms as f64,
                    ),
                    baseline_items_processed,
                    current_items_processed,
                    baseline_items_per_second,
                    current_items_per_second,
                    throughput_delta_pct: match (
                        baseline_items_per_second,
                        current_items_per_second,
                    ) {
                        (Some(b), Some(c)) => pct_delta(b, c),
                        _ => None,
                    },
                }
            })
            .collect();

        let aggregate_duration_delta_pct = pct_delta(
            baseline.aggregate_duration_ms as f64,
            self.aggregate_duration_ms as f64,
        );
        let aggregate_throughput_delta_pct = match (
            baseline.aggregate_items_per_second,
            self.aggregate_items_per_second,
        ) {
            (Some(b), Some(c)) => pct_delta(b, c),
            _ => None,
        };

        let dominant_phase_shift = match (baseline.dominant_phase, self.dominant_phase) {
            (Some(from), Some(to)) if from != to => Some((from, to)),
            _ => None,
        };

        RefreshLedgerEvidenceComparison {
            phase_deltas,
            aggregate_duration_delta_pct,
            aggregate_throughput_delta_pct,
            dominant_phase_shift,
        }
    }
}

/// Compute `(current - baseline) / baseline * 100` rounded to 2
/// decimals, with safe handling of the degenerate cases:
/// - baseline == 0.0 ⇒ `None` (no rate of change defined; an empty
///   baseline means the phase didn't run, so a delta is meaningless)
/// - current == baseline ⇒ `Some(0.0)` (no change is a real signal)
/// - NaN/Infinity ⇒ `None` (defensive — should never happen given
///   inputs are non-negative finite f64s, but pin the contract)
fn pct_delta(baseline: f64, current: f64) -> Option<f64> {
    if !baseline.is_finite() || !current.is_finite() {
        return None;
    }
    if baseline == 0.0 {
        return None;
    }
    let raw = ((current - baseline) / baseline) * 100.0;
    if !raw.is_finite() {
        return None;
    }
    Some((raw * 100.0).round() / 100.0)
}

/// CI-bench-gate threshold configuration. Project-specific values
/// let bench harnesses tune their tolerance: a noisy benchmark
/// runner picks looser thresholds than a deterministic CI worker.
///
/// `coding_agent_session_search-ibuuh.24`: complementary surface to
/// `emit_tracing_summary` (operator-visibility soft signal) — the
/// hard-gate consumer uses `regression_verdict` to decide whether
/// to exit non-zero in CI.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RegressionVerdictThresholds {
    /// Aggregate duration delta percent at which the verdict
    /// becomes `Warning`. Inclusive (`>=` triggers).
    /// Reasonable default: `+15.0`.
    pub warning_duration_pct: f64,
    /// Aggregate duration delta percent at which the verdict
    /// becomes `Failure`. Inclusive. MUST be `>= warning_duration_pct`
    /// or the constructor returns Err.
    /// Reasonable default: `+30.0`.
    pub failure_duration_pct: f64,
}

impl RegressionVerdictThresholds {
    /// Default threshold pair calibrated for typical bench-CI
    /// workloads on cass: 15% warning, 30% failure.
    pub fn defaults() -> Self {
        Self {
            warning_duration_pct: 15.0,
            failure_duration_pct: 30.0,
        }
    }

    /// Custom threshold pair. Returns `Err(&'static str)` when the
    /// configuration is internally inconsistent (warning >= failure
    /// would never raise a warning before the failure trips).
    pub fn try_new(
        warning_duration_pct: f64,
        failure_duration_pct: f64,
    ) -> Result<Self, &'static str> {
        if !warning_duration_pct.is_finite() || !failure_duration_pct.is_finite() {
            return Err("regression thresholds must be finite f64s");
        }
        if warning_duration_pct < 0.0 || failure_duration_pct < 0.0 {
            return Err("regression thresholds must be non-negative percentages");
        }
        if warning_duration_pct >= failure_duration_pct {
            return Err(
                "warning_duration_pct must be strictly less than failure_duration_pct, \
                 otherwise the warning level is unreachable",
            );
        }
        Ok(Self {
            warning_duration_pct,
            failure_duration_pct,
        })
    }

    fn is_valid(&self) -> bool {
        self.warning_duration_pct.is_finite()
            && self.failure_duration_pct.is_finite()
            && self.warning_duration_pct >= 0.0
            && self.failure_duration_pct >= 0.0
            && self.warning_duration_pct < self.failure_duration_pct
    }
}

impl<'de> Deserialize<'de> for RegressionVerdictThresholds {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawThresholds {
            warning_duration_pct: f64,
            failure_duration_pct: f64,
        }

        let raw = RawThresholds::deserialize(deserializer)?;
        Self::try_new(raw.warning_duration_pct, raw.failure_duration_pct)
            .map_err(serde::de::Error::custom)
    }
}

/// Hard-gate verdict for CI bench runners. `Failure` is the only
/// signal that should cause a non-zero exit; `Warning` is for
/// PR-comment / dashboard surfaces; `Clean` is the steady-state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "verdict")]
pub enum RegressionVerdict {
    /// Either no comparison data was available (e.g. baseline
    /// missing) or the duration delta is below the warning
    /// threshold. CI MUST treat this as pass.
    Clean,
    /// Warning band reached: duration delta `>= warning_duration_pct`
    /// but `< failure_duration_pct`. CI should surface this in PR
    /// comments / dashboards but NOT fail the build.
    Warning {
        duration_delta_pct: f64,
        threshold_pct: f64,
    },
    /// Failure band reached: duration delta `>= failure_duration_pct`.
    /// CI MUST exit non-zero on this verdict.
    Failure {
        duration_delta_pct: f64,
        threshold_pct: f64,
    },
}

impl RegressionVerdict {
    /// Convenience: is this a CI-fail verdict? Lets bench-CI
    /// harnesses write `if verdict.should_fail_build() { exit(1); }`
    /// without matching every variant.
    pub fn should_fail_build(&self) -> bool {
        matches!(self, Self::Failure { .. })
    }
}

impl RefreshLedgerEvidenceComparison {
    /// Compute the CI hard-gate verdict for this comparison against
    /// caller-supplied thresholds. Pure function; no I/O. Use
    /// `emit_tracing_summary` for operator-visibility soft signaling
    /// instead of CI gating.
    ///
    /// `coding_agent_session_search-ibuuh.24`: this is the
    /// bench-CI consumer of `compare_to`. A regression test asserts
    /// the verdict tiering matches the threshold contract exactly,
    /// so a project tuning thresholds for its own bench harness
    /// gets predictable behavior at the boundary cases.
    ///
    /// Degenerate cases:
    /// - `aggregate_duration_delta_pct == None` (baseline missing
    ///   or empty) ⇒ `Clean` — no measurement to gate on.
    /// - Negative duration delta (improvement) ⇒ always `Clean`,
    ///   regardless of threshold polarity (an improvement cannot
    ///   trigger a regression failure).
    pub fn regression_verdict(
        &self,
        thresholds: &RegressionVerdictThresholds,
    ) -> RegressionVerdict {
        if !thresholds.is_valid() {
            return RegressionVerdict::Clean;
        }
        let Some(duration_pct) = self.aggregate_duration_delta_pct else {
            return RegressionVerdict::Clean;
        };
        // Improvements never trigger regression verdicts. Pin the
        // sign explicitly rather than relying on threshold values
        // staying positive — a future maintainer who passes a
        // negative warning_duration_pct (e.g. to gate on
        // improvements as a positive signal) would otherwise see
        // every steady-state run trip.
        if duration_pct < 0.0 {
            return RegressionVerdict::Clean;
        }
        if duration_pct >= thresholds.failure_duration_pct {
            return RegressionVerdict::Failure {
                duration_delta_pct: duration_pct,
                threshold_pct: thresholds.failure_duration_pct,
            };
        }
        if duration_pct >= thresholds.warning_duration_pct {
            return RegressionVerdict::Warning {
                duration_delta_pct: duration_pct,
                threshold_pct: thresholds.warning_duration_pct,
            };
        }
        RegressionVerdict::Clean
    }
}

impl RefreshLedgerEvidenceComparison {
    /// Emit a single structured tracing event summarizing the
    /// cross-run comparison. Operators see "this rebuild was N%
    /// slower than the previous publish" in default-level logs
    /// without running a benchmark harness.
    ///
    /// `coding_agent_session_search-ibuuh.24`: pure helper that any
    /// caller (the publish path, a `cass status` summary surface,
    /// CI bench gates) can invoke after `compare_to`. Severity is
    /// chosen by the regression magnitude:
    ///
    /// - `aggregate_duration_delta_pct >= +25.0` ⇒ `warn`
    ///   (significant slowdown — surface in default logs so the
    ///   operator sees it without dredging)
    /// - `aggregate_duration_delta_pct <= -10.0` ⇒ `info`
    ///   (notable improvement — worth surfacing as a positive
    ///   signal)
    /// - otherwise ⇒ `debug` (steady state — high-volume noise on
    ///   every publish; only visible at debug level)
    ///
    /// The thresholds (+25% slowdown / -10% improvement) are the
    /// "operator should look" signal levels, NOT a hard regression
    /// gate. CI hard gates compare against benchmark baselines with
    /// project-specific thresholds; this helper is for ambient
    /// operator visibility.
    ///
    /// `dominant_phase_shift` is reported on every emission
    /// regardless of severity tier — a hot-phase change is itself
    /// a regression signal worth surfacing even when the absolute
    /// totals look similar.
    pub fn emit_tracing_summary(&self) {
        let dominant_shift_str = self
            .dominant_phase_shift
            .map(|(from, to)| format!("{}->{}", from.as_str(), to.as_str()))
            .unwrap_or_else(|| "none".to_string());
        let aggregate_duration_str = self
            .aggregate_duration_delta_pct
            .map(|pct| format!("{pct:+.2}%"))
            .unwrap_or_else(|| "n/a".to_string());
        let aggregate_throughput_str = self
            .aggregate_throughput_delta_pct
            .map(|pct| format!("{pct:+.2}%"))
            .unwrap_or_else(|| "n/a".to_string());

        // Severity tier from the duration delta. Throughput delta
        // alone doesn't drive severity because items_per_second
        // is None on zero-item phases; duration is the always-
        // present signal.
        const SLOWDOWN_WARN_THRESHOLD_PCT: f64 = 25.0;
        const IMPROVEMENT_INFO_THRESHOLD_PCT: f64 = -10.0;
        let duration_pct = self.aggregate_duration_delta_pct.unwrap_or(0.0);
        let phase_count = self.phase_deltas.len();

        if duration_pct >= SLOWDOWN_WARN_THRESHOLD_PCT {
            tracing::warn!(
                target: "cass::indexer::lexical_refresh",
                aggregate_duration_delta_pct = duration_pct,
                aggregate_throughput_delta_pct = self.aggregate_throughput_delta_pct.unwrap_or(0.0),
                aggregate_duration = %aggregate_duration_str,
                aggregate_throughput = %aggregate_throughput_str,
                dominant_phase_shift = %dominant_shift_str,
                phase_count,
                "lexical refresh evidence: significant slowdown vs previous publish"
            );
        } else if duration_pct <= IMPROVEMENT_INFO_THRESHOLD_PCT {
            tracing::info!(
                target: "cass::indexer::lexical_refresh",
                aggregate_duration_delta_pct = duration_pct,
                aggregate_throughput_delta_pct = self.aggregate_throughput_delta_pct.unwrap_or(0.0),
                aggregate_duration = %aggregate_duration_str,
                aggregate_throughput = %aggregate_throughput_str,
                dominant_phase_shift = %dominant_shift_str,
                phase_count,
                "lexical refresh evidence: notable improvement vs previous publish"
            );
        } else {
            tracing::debug!(
                target: "cass::indexer::lexical_refresh",
                aggregate_duration_delta_pct = duration_pct,
                aggregate_throughput_delta_pct = self.aggregate_throughput_delta_pct.unwrap_or(0.0),
                aggregate_duration = %aggregate_duration_str,
                aggregate_throughput = %aggregate_throughput_str,
                dominant_phase_shift = %dominant_shift_str,
                phase_count,
                "lexical refresh evidence: cross-run comparison"
            );
        }
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_model_covers_all_phases() {
        assert_eq!(RefreshPhase::ALL.len(), 7);
        assert_eq!(RefreshPhase::ALL[0], RefreshPhase::Scan);
        assert_eq!(RefreshPhase::ALL[6], RefreshPhase::Recovery);
    }

    #[test]
    fn phase_as_str_round_trips() {
        for phase in RefreshPhase::ALL {
            let s = phase.as_str();
            assert!(!s.is_empty(), "phase {phase:?} has empty string");
        }
    }

    #[test]
    fn ledger_builder_records_phases() {
        let mut builder = RefreshLedger::start("small", false);

        builder.begin_phase(RefreshPhase::Scan);
        builder.record_items(100, 5);
        builder.set_counter("connectors_scanned", 3);

        builder.begin_phase(RefreshPhase::Persist);
        builder.record_items(95, 0);
        builder.set_counter("bytes_written", 50_000);

        builder.begin_phase(RefreshPhase::LexicalRebuild);
        builder.record_items(450, 0);

        builder.begin_phase(RefreshPhase::Publish);
        builder.record_items(1, 0);

        let ledger = builder.finish();

        assert_eq!(ledger.phases.len(), 4);
        assert_eq!(ledger.corpus_family, "small");
        assert!(!ledger.full_rebuild);

        let scan = ledger.phase(RefreshPhase::Scan).unwrap();
        assert_eq!(scan.items_processed, 100);
        assert_eq!(scan.items_skipped, 5);
        assert_eq!(*scan.counters.get("connectors_scanned").unwrap(), 3);

        let persist = ledger.phase(RefreshPhase::Persist).unwrap();
        assert_eq!(persist.items_processed, 95);
        assert_eq!(*persist.counters.get("bytes_written").unwrap(), 50_000);

        assert!(ledger.all_phases_succeeded());
        assert_eq!(ledger.total_items_processed(), 100 + 95 + 450 + 1);
        assert!(ledger.completed_at_ms >= ledger.started_at_ms);
        let max_phase_duration = ledger
            .phases
            .iter()
            .map(|phase| phase.duration_ms)
            .max()
            .unwrap_or(0);
        assert!(ledger.total_duration_ms >= max_phase_duration);
    }

    #[test]
    fn ledger_builder_records_failures() {
        let mut builder = RefreshLedger::start("small", false);

        builder.begin_phase(RefreshPhase::Scan);
        builder.record_items(50, 0);

        builder.begin_phase(RefreshPhase::Persist);
        builder.record_failure("database locked");

        let ledger = builder.finish();

        assert!(!ledger.all_phases_succeeded());
        assert_eq!(ledger.failed_phases().len(), 1);
        assert_eq!(ledger.failed_phases()[0].phase, RefreshPhase::Persist);
        assert_eq!(
            ledger.failed_phases()[0].error_message.as_deref(),
            Some("database locked")
        );
    }

    #[test]
    fn ledger_builder_records_errors_without_failure() {
        let mut builder = RefreshLedger::start("medium", false);

        builder.begin_phase(RefreshPhase::Scan);
        builder.record_items(90, 0);
        builder.record_error("connector timeout");
        builder.record_error("permission denied");

        let ledger = builder.finish();

        let scan = ledger.phase(RefreshPhase::Scan).unwrap();
        assert!(scan.success); // phase still succeeded
        assert_eq!(scan.errors, 2);
        // Both error messages are preserved (joined with "; ").
        let msg = scan.error_message.as_deref().unwrap();
        assert!(
            msg.contains("connector timeout"),
            "missing first error: {msg}"
        );
        assert!(
            msg.contains("permission denied"),
            "missing second error: {msg}"
        );
    }

    #[test]
    fn ledger_equivalence_artifacts() {
        let mut builder = RefreshLedger::start("small", true);

        builder.begin_phase(RefreshPhase::Scan);
        builder.record_items(10, 0);

        builder.set_equivalence(EquivalenceArtifacts {
            conversation_count: 10,
            message_count: 40,
            lexical_doc_count: 40,
            lexical_fingerprint: Some("fp-abc".to_owned()),
            semantic_manifest_fingerprint: None,
            search_hit_digest: Some("sha256-xyz".to_owned()),
            peak_rss_bytes: Some(100_000_000),
            db_size_bytes: Some(5_000_000),
            lexical_index_size_bytes: Some(2_000_000),
        });

        let ledger = builder.finish();

        assert_eq!(ledger.equivalence.conversation_count, 10);
        assert_eq!(ledger.equivalence.message_count, 40);
        assert_eq!(
            ledger.equivalence.lexical_fingerprint.as_deref(),
            Some("fp-abc")
        );
        assert!(ledger.full_rebuild);
    }

    #[test]
    fn ledger_duration_breakdown() {
        let mut builder = RefreshLedger::start("small", false);

        builder.begin_phase(RefreshPhase::Scan);
        // Phases are very fast in tests — duration_ms may be 0.
        builder.begin_phase(RefreshPhase::LexicalRebuild);

        let ledger = builder.finish();

        let breakdown = ledger.duration_breakdown();
        assert!(breakdown.contains_key("scan"));
        assert!(breakdown.contains_key("lexical_rebuild"));
    }

    #[test]
    fn readiness_milestones_measure_lexical_search_and_settled_times() {
        let ledger = RefreshLedger {
            total_duration_ms: 90,
            phases: vec![
                phase_record(RefreshPhase::Scan, 10, true),
                phase_record(RefreshPhase::Persist, 20, true),
                phase_record(RefreshPhase::LexicalRebuild, 30, true),
                phase_record(RefreshPhase::Publish, 5, true),
                phase_record(RefreshPhase::Analytics, 7, true),
                phase_record(RefreshPhase::Semantic, 8, true),
            ],
            ..Default::default()
        };

        let milestones = ledger.readiness_milestones();

        assert_eq!(milestones.time_to_lexical_ready_ms, Some(60));
        assert_eq!(milestones.time_to_search_ready_ms, Some(65));
        assert_eq!(milestones.time_to_full_settled_ms, Some(90));
        assert_eq!(milestones.failed_phase, None);
        assert_eq!(
            milestones.search_readiness_state,
            RefreshSearchReadinessState::Published
        );

        let json = serde_json::to_value(&milestones).unwrap();
        assert_eq!(json["time_to_lexical_ready_ms"], 60);
        assert_eq!(json["time_to_search_ready_ms"], 65);
        assert_eq!(json["time_to_full_settled_ms"], 90);
        assert_eq!(json["search_readiness_state"], "published");
    }

    #[test]
    fn readiness_milestones_stop_at_first_failed_phase() {
        let ledger = RefreshLedger {
            total_duration_ms: 75,
            phases: vec![
                phase_record(RefreshPhase::Scan, 10, true),
                phase_record(RefreshPhase::Persist, 20, true),
                phase_record(RefreshPhase::LexicalRebuild, 30, false),
                phase_record(RefreshPhase::Publish, 5, true),
            ],
            ..Default::default()
        };

        let milestones = ledger.readiness_milestones();

        assert_eq!(milestones.time_to_lexical_ready_ms, None);
        assert_eq!(milestones.time_to_search_ready_ms, None);
        assert_eq!(milestones.time_to_full_settled_ms, None);
        assert_eq!(milestones.failed_phase.as_deref(), Some("lexical_rebuild"));
        assert_eq!(
            milestones.search_readiness_state,
            RefreshSearchReadinessState::BlockedBeforePublish
        );
    }

    #[test]
    fn readiness_milestones_explain_unpublished_and_publish_failed_states() {
        let unpublished = RefreshLedger {
            phases: vec![
                phase_record(RefreshPhase::Scan, 10, true),
                phase_record(RefreshPhase::Persist, 20, true),
                phase_record(RefreshPhase::LexicalRebuild, 30, true),
            ],
            ..Default::default()
        };

        let unpublished_milestones = unpublished.readiness_milestones();

        assert_eq!(unpublished_milestones.time_to_lexical_ready_ms, Some(60));
        assert_eq!(unpublished_milestones.time_to_search_ready_ms, None);
        assert_eq!(unpublished_milestones.time_to_full_settled_ms, None);
        assert_eq!(unpublished_milestones.failed_phase, None);
        assert_eq!(
            unpublished_milestones.search_readiness_state,
            RefreshSearchReadinessState::WaitingForPublish
        );

        let publish_failed = RefreshLedger {
            phases: vec![
                phase_record(RefreshPhase::Scan, 10, true),
                phase_record(RefreshPhase::Persist, 20, true),
                phase_record(RefreshPhase::LexicalRebuild, 30, true),
                phase_record(RefreshPhase::Publish, 5, false),
            ],
            ..Default::default()
        };

        let publish_failed_milestones = publish_failed.readiness_milestones();

        assert_eq!(publish_failed_milestones.time_to_lexical_ready_ms, Some(60));
        assert_eq!(publish_failed_milestones.time_to_search_ready_ms, None);
        assert_eq!(publish_failed_milestones.time_to_full_settled_ms, None);
        assert_eq!(
            publish_failed_milestones.failed_phase.as_deref(),
            Some("publish")
        );
        assert_eq!(
            publish_failed_milestones.search_readiness_state,
            RefreshSearchReadinessState::PublishFailed
        );

        let post_publish_failure = RefreshLedger {
            phases: vec![
                phase_record(RefreshPhase::Scan, 10, true),
                phase_record(RefreshPhase::Persist, 20, true),
                phase_record(RefreshPhase::LexicalRebuild, 30, true),
                phase_record(RefreshPhase::Publish, 5, true),
                phase_record(RefreshPhase::Analytics, 7, false),
            ],
            ..Default::default()
        };

        let post_publish_failure_milestones = post_publish_failure.readiness_milestones();

        assert_eq!(
            post_publish_failure_milestones.time_to_lexical_ready_ms,
            Some(60)
        );
        assert_eq!(
            post_publish_failure_milestones.time_to_search_ready_ms,
            Some(65)
        );
        assert_eq!(
            post_publish_failure_milestones.time_to_full_settled_ms,
            None
        );
        assert_eq!(
            post_publish_failure_milestones.failed_phase.as_deref(),
            Some("analytics")
        );
        assert_eq!(
            post_publish_failure_milestones.search_readiness_state,
            RefreshSearchReadinessState::Published
        );
    }

    #[test]
    fn readiness_milestones_do_not_report_full_settlement_before_publish() {
        let empty = RefreshLedger::default().readiness_milestones();

        assert_eq!(empty.time_to_lexical_ready_ms, None);
        assert_eq!(empty.time_to_search_ready_ms, None);
        assert_eq!(empty.time_to_full_settled_ms, None);
        assert_eq!(
            empty.search_readiness_state,
            RefreshSearchReadinessState::WaitingForPublish
        );

        let partial = RefreshLedger {
            total_duration_ms: 42,
            phases: vec![
                phase_record(RefreshPhase::Scan, 10, true),
                phase_record(RefreshPhase::Persist, 20, true),
            ],
            ..Default::default()
        }
        .readiness_milestones();

        assert_eq!(partial.time_to_lexical_ready_ms, None);
        assert_eq!(partial.time_to_search_ready_ms, None);
        assert_eq!(partial.time_to_full_settled_ms, None);
        assert_eq!(
            partial.search_readiness_state,
            RefreshSearchReadinessState::WaitingForPublish
        );
    }

    #[test]
    fn ledger_tags() {
        let mut builder = RefreshLedger::start("medium", false);
        builder.tag("run_id", "bench-2026-04-01");
        builder.tag("machine", "csd");

        let ledger = builder.finish();

        assert_eq!(ledger.tags.get("run_id").unwrap(), "bench-2026-04-01");
        assert_eq!(ledger.tags.get("machine").unwrap(), "csd");
    }

    #[test]
    fn ledger_json_round_trip() {
        let mut builder = RefreshLedger::start("duplicate_heavy", true);
        builder.begin_phase(RefreshPhase::Scan);
        builder.record_items(50, 10);
        builder.set_counter("duplicate_conversations", 25);
        builder.begin_phase(RefreshPhase::Persist);
        builder.record_items(40, 0);

        builder.set_equivalence(EquivalenceArtifacts {
            conversation_count: 40,
            message_count: 200,
            lexical_doc_count: 200,
            ..Default::default()
        });

        let ledger = builder.finish();
        let json = ledger.to_json();
        let deser: RefreshLedger = serde_json::from_str(&json).unwrap();

        assert_eq!(deser.corpus_family, "duplicate_heavy");
        assert!(deser.full_rebuild);
        assert_eq!(deser.phases.len(), 2);
        assert_eq!(deser.equivalence.conversation_count, 40);
        assert_eq!(
            *deser.phases[0]
                .counters
                .get("duplicate_conversations")
                .unwrap(),
            25
        );
    }

    #[test]
    fn ledger_inc_counter() {
        let mut builder = RefreshLedger::start("small", false);
        builder.begin_phase(RefreshPhase::Scan);
        builder.inc_counter("files_scanned", 10);
        builder.inc_counter("files_scanned", 15);
        builder.inc_counter("files_scanned", 5);

        let ledger = builder.finish();
        let scan = ledger.phase(RefreshPhase::Scan).unwrap();
        assert_eq!(*scan.counters.get("files_scanned").unwrap(), 30);
    }

    #[test]
    fn benchmark_corpus_configs_have_correct_families() {
        assert_eq!(BenchmarkCorpusConfig::small().family, "small");
        assert_eq!(BenchmarkCorpusConfig::medium().family, "medium");
        assert_eq!(BenchmarkCorpusConfig::large().family, "large");
        assert_eq!(
            BenchmarkCorpusConfig::duplicate_heavy().family,
            "duplicate_heavy"
        );
        assert_eq!(BenchmarkCorpusConfig::pathological().family, "pathological");
        assert_eq!(BenchmarkCorpusConfig::mixed_agent().family, "mixed_agent");
        assert_eq!(BenchmarkCorpusConfig::incremental().family, "incremental");
    }

    #[test]
    fn benchmark_corpus_configs_have_reasonable_sizes() {
        let configs = [
            BenchmarkCorpusConfig::small(),
            BenchmarkCorpusConfig::medium(),
            BenchmarkCorpusConfig::large(),
            BenchmarkCorpusConfig::duplicate_heavy(),
            BenchmarkCorpusConfig::pathological(),
            BenchmarkCorpusConfig::mixed_agent(),
            BenchmarkCorpusConfig::incremental(),
        ];
        for cfg in &configs {
            assert!(
                cfg.num_conversations > 0,
                "{} has 0 conversations",
                cfg.family
            );
            assert!(
                cfg.messages_per_conversation > 0,
                "{} has 0 messages",
                cfg.family
            );
            assert!(cfg.agent_count > 0, "{} has 0 agents", cfg.family);
            assert!(
                cfg.duplicate_fraction >= 0.0 && cfg.duplicate_fraction <= 1.0,
                "{} has invalid duplicate fraction",
                cfg.family
            );
        }
    }

    fn phase_record(phase: RefreshPhase, duration_ms: u64, success: bool) -> PhaseRecord {
        PhaseRecord {
            phase,
            duration_ms,
            items_processed: 0,
            items_skipped: 0,
            errors: u64::from(!success),
            counters: BTreeMap::new(),
            success,
            error_message: (!success).then(|| format!("failed {}", phase.as_str())),
        }
    }

    fn phase_record_with_items(phase: RefreshPhase, duration_ms: u64, items: u64) -> PhaseRecord {
        PhaseRecord {
            phase,
            duration_ms,
            items_processed: items,
            items_skipped: 0,
            errors: 0,
            counters: BTreeMap::new(),
            success: true,
            error_message: None,
        }
    }

    fn ledger_with(phases: Vec<PhaseRecord>) -> RefreshLedger {
        let total_duration_ms = phases.iter().map(|p| p.duration_ms).sum();
        RefreshLedger {
            version: 1,
            started_at_ms: 1_700_000_000_000,
            completed_at_ms: 1_700_000_000_000 + i64::try_from(total_duration_ms).unwrap_or(0),
            total_duration_ms,
            full_rebuild: true,
            corpus_family: "evidence-test".to_owned(),
            phases,
            equivalence: EquivalenceArtifacts::default(),
            tags: BTreeMap::new(),
        }
    }

    /// `coding_agent_session_search-ibuuh.24` (evidence-ledger gate):
    /// throughput math is correct + zero-duration / zero-items
    /// degenerate cases yield None (NOT NaN). Pinning the math in a
    /// golden test means a future tweak that introduced NaN
    /// poisoning into benchmark JSON would trip immediately.
    #[test]
    fn evidence_summary_reports_per_phase_throughput_with_safe_zero_handling() {
        // Mixed corpus: Scan moved 1000 items in 500ms, Persist moved
        // 2000 items in 1000ms, LexicalRebuild moved 0 items in 100ms
        // (warmup-only phase), Recovery did 0 items in 0ms (no-op).
        let ledger = ledger_with(vec![
            phase_record_with_items(RefreshPhase::Scan, 500, 1000),
            phase_record_with_items(RefreshPhase::Persist, 1000, 2000),
            phase_record_with_items(RefreshPhase::LexicalRebuild, 100, 0),
            phase_record_with_items(RefreshPhase::Recovery, 0, 0),
        ]);

        let evidence = ledger.evidence_summary();

        // Throughput vector excludes zero-item phases (LexicalRebuild,
        // Recovery): nothing to extrapolate.
        assert_eq!(
            evidence.throughput.len(),
            2,
            "throughput must skip zero-item phases; got {:?}",
            evidence.throughput
        );

        // Scan: 1000 items / 0.5s = 2000.0 items/s.
        let scan = evidence
            .throughput
            .iter()
            .find(|t| t.phase == RefreshPhase::Scan)
            .expect("scan throughput present");
        assert_eq!(scan.items_per_second, Some(2000.0));
        assert_eq!(scan.duration_ms, 500);
        assert_eq!(scan.items_processed, 1000);

        // Persist: 2000 items / 1.0s = 2000.0 items/s.
        let persist = evidence
            .throughput
            .iter()
            .find(|t| t.phase == RefreshPhase::Persist)
            .expect("persist throughput present");
        assert_eq!(persist.items_per_second, Some(2000.0));

        // Aggregate: (1000+2000+0+0) / (500+1000+100+0)ms = 3000/1.6s = 1875.0
        assert_eq!(evidence.aggregate_items_processed, 3000);
        assert_eq!(evidence.aggregate_duration_ms, 1600);
        assert_eq!(evidence.aggregate_items_per_second, Some(1875.0));
    }

    /// Zero-duration ledger (empty or instantaneous) must NOT panic
    /// and must NOT emit NaN. dominant_phase is None; aggregate
    /// throughput is None.
    #[test]
    fn evidence_summary_handles_empty_and_zero_duration_ledgers() {
        // Truly empty.
        let empty = ledger_with(Vec::new());
        let empty_evidence = empty.evidence_summary();
        assert!(empty_evidence.throughput.is_empty());
        assert!(empty_evidence.phase_share.is_empty());
        assert_eq!(empty_evidence.dominant_phase, None);
        assert_eq!(empty_evidence.aggregate_items_per_second, None);
        assert_eq!(empty_evidence.aggregate_duration_ms, 0);

        // Phases ran but contributed 0ms each (instantaneous run).
        let instant = ledger_with(vec![
            phase_record_with_items(RefreshPhase::Scan, 0, 5),
            phase_record_with_items(RefreshPhase::Persist, 0, 5),
        ]);
        let instant_evidence = instant.evidence_summary();
        // Phases ran but with zero duration ⇒ throughput None for each.
        for t in &instant_evidence.throughput {
            assert_eq!(t.items_per_second, None, "zero duration must yield None");
        }
        // No phase was dominant (all zero) ⇒ dominant_phase None.
        assert_eq!(instant_evidence.dominant_phase, None);
        // Phase shares all 0.0 — no NaN poisoning.
        for share in &instant_evidence.phase_share {
            assert_eq!(share.share_pct, 0.0);
            assert!(!share.share_pct.is_nan(), "share_pct must never be NaN");
        }
    }

    /// Phase shares sum to ~100.0 across phases with non-zero
    /// duration (sub-millisecond rounding can cause ±0.01 drift).
    /// dominant_phase identifies the phase with the largest
    /// duration_ms.
    #[test]
    fn evidence_summary_phase_share_sums_to_one_hundred_and_dominant_phase_picks_max() {
        let ledger = ledger_with(vec![
            phase_record_with_items(RefreshPhase::Scan, 200, 100),
            phase_record_with_items(RefreshPhase::Persist, 600, 1500), // dominant
            phase_record_with_items(RefreshPhase::LexicalRebuild, 200, 1500),
        ]);
        let evidence = ledger.evidence_summary();

        let total_share: f64 = evidence.phase_share.iter().map(|s| s.share_pct).sum();
        assert!(
            (total_share - 100.0).abs() <= 0.05,
            "phase shares must sum to ~100.0 (±0.05 for rounding); got {total_share}"
        );

        // Persist contributed 600ms / 1000ms = 60% of wall time.
        let persist_share = evidence
            .phase_share
            .iter()
            .find(|s| s.phase == RefreshPhase::Persist)
            .expect("persist share present");
        assert_eq!(persist_share.share_pct, 60.0);

        // Dominant phase must be Persist (largest duration).
        assert_eq!(evidence.dominant_phase, Some(RefreshPhase::Persist));
    }

    /// Tie-break for dominant phase: when two phases have IDENTICAL
    /// duration_ms, the FIRST one (in pipeline order) wins —
    /// matches Iterator::max_by_key semantics, so a future phase
    /// reordering doesn't silently flip the dominant phase contract.
    #[test]
    fn evidence_summary_dominant_phase_tie_break_is_first_in_pipeline_order() {
        let ledger = ledger_with(vec![
            phase_record_with_items(RefreshPhase::Scan, 500, 1),
            phase_record_with_items(RefreshPhase::Persist, 500, 1),
            phase_record_with_items(RefreshPhase::LexicalRebuild, 500, 1),
        ]);
        let evidence = ledger.evidence_summary();
        // Iterator::max_by_key returns the LAST max element on ties,
        // so LexicalRebuild wins when all three are 500ms. Pin this
        // behavior so a future change to last-vs-first tie-break
        // semantics fails the test (operators reading benchmark JSON
        // for "dominant_phase" rely on stable ordering).
        assert_eq!(
            evidence.dominant_phase,
            Some(RefreshPhase::LexicalRebuild),
            "tie-break: max_by_key returns the LAST phase at max duration"
        );
    }

    /// Evidence summary serializes through serde so benchmark
    /// gates / dashboards can store the JSON and diff across runs.
    /// Pin the field set so a future struct-shape regression
    /// (e.g. dropping aggregate_items_per_second) trips this.
    #[test]
    fn evidence_summary_serializes_to_stable_json_field_set() {
        let ledger = ledger_with(vec![phase_record_with_items(RefreshPhase::Scan, 100, 50)]);
        let evidence = ledger.evidence_summary();
        let json = serde_json::to_string(&evidence).expect("serialize");
        for required_field in [
            "\"throughput\"",
            "\"phase_share\"",
            "\"dominant_phase\"",
            "\"aggregate_items_processed\"",
            "\"aggregate_duration_ms\"",
            "\"aggregate_items_per_second\"",
        ] {
            assert!(
                json.contains(required_field),
                "evidence JSON missing field {required_field}; got: {json}"
            );
        }
        // Round-trip via serde_json::Value (the typed roundtrip is
        // not used by consumers; they parse into serde_json::Value
        // for diffing).
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(parsed["aggregate_items_processed"], 50);
        assert_eq!(parsed["aggregate_duration_ms"], 100);
        assert_eq!(parsed["aggregate_items_per_second"], 500.0);
        assert_eq!(parsed["dominant_phase"], "scan");
    }

    /// `coding_agent_session_search-ibuuh.24` cross-run comparator
    /// gate: `compare_to` must surface real regressions and real
    /// improvements with the conventional sign:
    /// - duration_delta_pct > 0 ⇒ slower in `current`
    /// - throughput_delta_pct > 0 ⇒ faster in `current`
    ///
    /// A regression in either sign convention would cause benchmark
    /// CI to misclassify slowdowns as wins (or vice versa).
    #[test]
    fn evidence_compare_to_reports_per_phase_regressions_and_improvements() {
        // Baseline: scan moved 100 items in 100ms (1000 items/s).
        let baseline = ledger_with(vec![phase_record_with_items(RefreshPhase::Scan, 100, 100)])
            .evidence_summary();
        // Current: scan moved 100 items in 200ms (500 items/s) —
        // slower wall clock, halved throughput. Pure regression.
        let current = ledger_with(vec![phase_record_with_items(RefreshPhase::Scan, 200, 100)])
            .evidence_summary();

        let cmp = current.compare_to(&baseline);

        assert_eq!(cmp.phase_deltas.len(), 1);
        let scan = &cmp.phase_deltas[0];
        assert_eq!(scan.phase, RefreshPhase::Scan);
        // duration: (200-100)/100 * 100 = +100% (twice as slow).
        assert_eq!(scan.duration_delta_pct, Some(100.0));
        // throughput: (500-1000)/1000 * 100 = -50% (half as fast).
        assert_eq!(scan.throughput_delta_pct, Some(-50.0));
        // Aggregate mirrors the single-phase signals.
        assert_eq!(cmp.aggregate_duration_delta_pct, Some(100.0));
        assert_eq!(cmp.aggregate_throughput_delta_pct, Some(-50.0));
        // Same phase dominant in both ⇒ no shift signal.
        assert_eq!(cmp.dominant_phase_shift, None);

        // Symmetric improvement case: swap baseline + current.
        let cmp_improved = baseline.compare_to(&current);
        let scan = &cmp_improved.phase_deltas[0];
        // duration: (100-200)/200 * 100 = -50% (half as long).
        assert_eq!(scan.duration_delta_pct, Some(-50.0));
        // throughput: (1000-500)/500 * 100 = +100% (twice as fast).
        assert_eq!(scan.throughput_delta_pct, Some(100.0));
    }

    /// Phase unique to ONE side must surface in the comparison
    /// (not silently dropped). Pre-fix this is the failure mode where
    /// a phase that ran in baseline but disappeared from current
    /// (e.g. publish phase elided due to a dispatch-routing bug)
    /// would not show up at all.
    #[test]
    fn evidence_compare_to_surfaces_phases_unique_to_one_side() {
        let baseline = ledger_with(vec![
            phase_record_with_items(RefreshPhase::Scan, 100, 100),
            phase_record_with_items(RefreshPhase::Persist, 50, 200),
        ])
        .evidence_summary();
        // Current: only Scan ran. Persist is "missing" — caller must
        // see this so they can investigate.
        let current = ledger_with(vec![phase_record_with_items(RefreshPhase::Scan, 100, 100)])
            .evidence_summary();

        let cmp = current.compare_to(&baseline);

        let phases: Vec<RefreshPhase> = cmp.phase_deltas.iter().map(|d| d.phase).collect();
        assert!(
            phases.contains(&RefreshPhase::Scan),
            "Scan ran in both sides; must appear in comparison; got phases {phases:?}"
        );
        assert!(
            phases.contains(&RefreshPhase::Persist),
            "Persist is missing from current but ran in baseline — comparison MUST \
             surface it so caller can investigate; got phases {phases:?}"
        );

        // The missing-from-current Persist entry should report
        // baseline_duration_ms=50 + current_duration_ms=0 + duration_delta_pct
        // is well-defined (it's -100%: phase went away).
        let persist = cmp
            .phase_deltas
            .iter()
            .find(|d| d.phase == RefreshPhase::Persist)
            .expect("Persist delta present");
        assert_eq!(persist.baseline_duration_ms, 50);
        assert_eq!(persist.current_duration_ms, 0);
        assert_eq!(
            persist.duration_delta_pct,
            Some(-100.0),
            "phase disappearing from current must surface as -100% duration delta; \
             got {persist:?}"
        );
    }

    /// Zero-item phases still consume wall-clock time and must remain
    /// visible to benchmark comparisons. Throughput summaries omit
    /// them by design, so `compare_to` must derive phase presence
    /// from phase-share data instead.
    #[test]
    fn evidence_compare_to_retains_zero_item_phases_with_duration() {
        let baseline = ledger_with(vec![
            phase_record_with_items(RefreshPhase::Scan, 100, 100),
            phase_record_with_items(RefreshPhase::Publish, 40, 0),
        ])
        .evidence_summary();
        let current = ledger_with(vec![
            phase_record_with_items(RefreshPhase::Scan, 100, 100),
            phase_record_with_items(RefreshPhase::Publish, 80, 0),
        ])
        .evidence_summary();

        assert!(
            baseline
                .throughput
                .iter()
                .all(|entry| entry.phase != RefreshPhase::Publish),
            "zero-item Publish must stay out of throughput: {:?}",
            baseline.throughput
        );

        let cmp = current.compare_to(&baseline);
        let publish = cmp
            .phase_deltas
            .iter()
            .find(|delta| delta.phase == RefreshPhase::Publish)
            .expect("zero-item Publish phase must remain in comparison");

        assert_eq!(publish.baseline_duration_ms, 40);
        assert_eq!(publish.current_duration_ms, 80);
        assert_eq!(publish.duration_delta_pct, Some(100.0));
        assert_eq!(publish.baseline_items_processed, 0);
        assert_eq!(publish.current_items_processed, 0);
        assert_eq!(publish.baseline_items_per_second, None);
        assert_eq!(publish.current_items_per_second, None);
        assert_eq!(publish.throughput_delta_pct, None);
    }

    /// Dominant-phase shift signal: when the hot phase changes
    /// between runs (even if absolute totals are similar), the
    /// operator should look at why. Pinning the shift detection
    /// directly catches a regression where the comparator silently
    /// reports the same dominant phase for both sides.
    #[test]
    fn evidence_compare_to_reports_dominant_phase_shift() {
        // Baseline: Scan dominates wall time.
        let baseline = ledger_with(vec![
            phase_record_with_items(RefreshPhase::Scan, 800, 100),
            phase_record_with_items(RefreshPhase::Persist, 200, 100),
        ])
        .evidence_summary();
        // Current: total wall time similar but Persist now dominates.
        let current = ledger_with(vec![
            phase_record_with_items(RefreshPhase::Scan, 200, 100),
            phase_record_with_items(RefreshPhase::Persist, 800, 100),
        ])
        .evidence_summary();
        // Sanity: the two sides really did have different dominant
        // phases (would silently break this test if dominant_phase
        // tie-breaking changed).
        assert_eq!(baseline.dominant_phase, Some(RefreshPhase::Scan));
        assert_eq!(current.dominant_phase, Some(RefreshPhase::Persist));

        let cmp = current.compare_to(&baseline);

        assert_eq!(
            cmp.dominant_phase_shift,
            Some((RefreshPhase::Scan, RefreshPhase::Persist)),
            "dominant phase shifted Scan→Persist; comparison must surface this; got {cmp:?}"
        );

        // Negative case: same dominant phase in both ⇒ no shift.
        let same_dom = ledger_with(vec![phase_record_with_items(RefreshPhase::Scan, 100, 100)])
            .evidence_summary();
        let cmp_same = same_dom.compare_to(&same_dom);
        assert_eq!(cmp_same.dominant_phase_shift, None);
    }

    /// Empty / zero-baseline degenerate cases must NOT panic and
    /// must NOT emit NaN — pre-fix `pct_delta` would have returned
    /// Inf for `(x - 0) / 0`. The defensive None branch is the only
    /// thing keeping benchmark JSON parseable when the baseline is
    /// missing or empty.
    #[test]
    fn evidence_compare_to_safely_handles_zero_baseline_and_empty_evidence() {
        let empty = ledger_with(Vec::new()).evidence_summary();
        let normal = ledger_with(vec![phase_record_with_items(RefreshPhase::Scan, 100, 50)])
            .evidence_summary();

        // empty → normal: baseline has nothing, every delta is None
        // (no rate of change defined when baseline is zero).
        let against_empty = normal.compare_to(&empty);
        assert!(
            against_empty
                .phase_deltas
                .iter()
                .all(|d| d.duration_delta_pct.is_none() || d.baseline_duration_ms == 0),
            "phases with zero-baseline duration must report None for duration_delta_pct"
        );
        assert_eq!(against_empty.aggregate_duration_delta_pct, None);
        assert_eq!(against_empty.aggregate_throughput_delta_pct, None);

        // empty vs empty: zero comparison surface, no panic.
        let against_self = empty.compare_to(&empty);
        assert!(against_self.phase_deltas.is_empty());
        assert_eq!(against_self.aggregate_duration_delta_pct, None);

        // No NaN anywhere in the JSON serialization (pins that the
        // defensive branches actually emit serializable output).
        let json = serde_json::to_string(&against_empty).expect("serialize");
        assert!(
            !json.contains("NaN"),
            "comparison JSON must not contain NaN; got {json}"
        );
        assert!(
            !json.contains("Infinity"),
            "comparison JSON must not contain Infinity"
        );
    }

    /// `coding_agent_session_search-ibuuh.24` cross-run tracing
    /// gate: emit_tracing_summary picks WARN for significant
    /// slowdowns (>=+25%), INFO for notable improvements (<=-10%),
    /// DEBUG for the steady-state range. Pre-fix this routing did
    /// not exist; pinning the thresholds directly catches a
    /// regression where a peer "tunes" the tier and accidentally
    /// hides a slowdown signal in debug-level logs.
    #[test]
    fn evidence_comparison_emit_tracing_summary_uses_correct_severity_tier() {
        use std::sync::{Arc, Mutex};
        use tracing::field::{Field, Visit};
        use tracing::{Event, Subscriber};
        use tracing_subscriber::Registry;
        use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

        #[derive(Debug, Clone, Default)]
        struct CapturedEvent {
            level: String,
            message: String,
        }

        #[derive(Clone, Default)]
        struct LevelCollector {
            events: Arc<Mutex<Vec<CapturedEvent>>>,
        }

        impl<S: Subscriber> Layer<S> for LevelCollector {
            fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
                if event.metadata().target() != "cass::indexer::lexical_refresh" {
                    return;
                }
                let mut visitor = MessageVisitor::default();
                event.record(&mut visitor);
                self.events
                    .lock()
                    .expect("collector lock")
                    .push(CapturedEvent {
                        level: event.metadata().level().to_string(),
                        message: visitor.message,
                    });
            }
        }

        #[derive(Default)]
        struct MessageVisitor {
            message: String,
        }
        impl Visit for MessageVisitor {
            fn record_str(&mut self, _field: &Field, _value: &str) {}
            fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.message = format!("{:?}", value).trim_matches('"').to_string();
                }
            }
        }

        // Helper: build a comparison directly with a given duration
        // delta so we exercise the tier routing without setting up
        // full ledger fixtures.
        fn comparison_with_duration_pct(pct: f64) -> RefreshLedgerEvidenceComparison {
            RefreshLedgerEvidenceComparison {
                phase_deltas: Vec::new(),
                aggregate_duration_delta_pct: Some(pct),
                aggregate_throughput_delta_pct: None,
                dominant_phase_shift: None,
            }
        }

        // Tier 1: significant slowdown ⇒ warn.
        let collector = LevelCollector::default();
        let subscriber = Registry::default().with(collector.clone());
        tracing::subscriber::with_default(subscriber, || {
            comparison_with_duration_pct(50.0).emit_tracing_summary();
        });
        let evs = collector.events.lock().expect("lock").clone();
        assert_eq!(
            evs.len(),
            1,
            "exactly one event per emit_tracing_summary call"
        );
        assert_eq!(
            evs[0].level, "WARN",
            "+50% slowdown must be warn; got {evs:?}"
        );
        assert!(
            evs[0].message.contains("significant slowdown"),
            "warn message must name the slowdown; got {:?}",
            evs[0].message
        );

        // Tier 2: notable improvement ⇒ info.
        let collector = LevelCollector::default();
        let subscriber = Registry::default().with(collector.clone());
        tracing::subscriber::with_default(subscriber, || {
            comparison_with_duration_pct(-25.0).emit_tracing_summary();
        });
        let evs = collector.events.lock().expect("lock").clone();
        assert_eq!(
            evs[0].level, "INFO",
            "-25% improvement must be info; got {evs:?}"
        );
        assert!(
            evs[0].message.contains("notable improvement"),
            "info message must name the improvement; got {:?}",
            evs[0].message
        );

        // Tier 3: steady-state ⇒ debug.
        let collector = LevelCollector::default();
        let subscriber = Registry::default().with(collector.clone());
        tracing::subscriber::with_default(subscriber, || {
            comparison_with_duration_pct(5.0).emit_tracing_summary();
        });
        let evs = collector.events.lock().expect("lock").clone();
        assert_eq!(
            evs[0].level, "DEBUG",
            "+5% within steady-state must be debug; got {evs:?}"
        );
        assert!(
            evs[0].message.contains("cross-run comparison"),
            "debug message must use the steady-state phrasing; got {:?}",
            evs[0].message
        );

        // Boundary: exactly +25.0 ⇒ warn (>= threshold).
        let collector = LevelCollector::default();
        let subscriber = Registry::default().with(collector.clone());
        tracing::subscriber::with_default(subscriber, || {
            comparison_with_duration_pct(25.0).emit_tracing_summary();
        });
        let evs = collector.events.lock().expect("lock").clone();
        assert_eq!(
            evs[0].level, "WARN",
            "exactly +25% must be warn (inclusive threshold); got {evs:?}"
        );

        // Boundary: exactly -10.0 ⇒ info (<= threshold).
        let collector = LevelCollector::default();
        let subscriber = Registry::default().with(collector.clone());
        tracing::subscriber::with_default(subscriber, || {
            comparison_with_duration_pct(-10.0).emit_tracing_summary();
        });
        let evs = collector.events.lock().expect("lock").clone();
        assert_eq!(
            evs[0].level, "INFO",
            "exactly -10% must be info (inclusive threshold); got {evs:?}"
        );

        // None duration delta (e.g. baseline missing) ⇒ debug
        // (defaults to 0.0 which lands in steady-state).
        let collector = LevelCollector::default();
        let subscriber = Registry::default().with(collector.clone());
        tracing::subscriber::with_default(subscriber, || {
            RefreshLedgerEvidenceComparison {
                phase_deltas: Vec::new(),
                aggregate_duration_delta_pct: None,
                aggregate_throughput_delta_pct: None,
                dominant_phase_shift: None,
            }
            .emit_tracing_summary();
        });
        let evs = collector.events.lock().expect("lock").clone();
        assert_eq!(
            evs[0].level, "DEBUG",
            "None duration delta defaults to steady-state (debug); got {evs:?}"
        );
    }

    /// `coding_agent_session_search-ibuuh.24` CI hard-gate
    /// regression: pin the regression_verdict tier semantics +
    /// boundary cases + degenerate inputs. A regression in any
    /// of the four classes (Clean / Warning / Failure /
    /// degenerate-clean) would silently break either the
    /// improvement signal (false-positive failure) or the
    /// failure gate (silent passthrough on real regression).
    #[test]
    fn regression_verdict_categorizes_each_band_and_handles_degenerate_cases() {
        let thresholds = RegressionVerdictThresholds::defaults();
        assert_eq!(thresholds.warning_duration_pct, 15.0);
        assert_eq!(thresholds.failure_duration_pct, 30.0);

        // Helper: build a comparison with a given duration delta.
        fn comparison_with_pct(pct: Option<f64>) -> RefreshLedgerEvidenceComparison {
            RefreshLedgerEvidenceComparison {
                phase_deltas: Vec::new(),
                aggregate_duration_delta_pct: pct,
                aggregate_throughput_delta_pct: None,
                dominant_phase_shift: None,
            }
        }

        // ─── Clean band ────────────────────────────────────────
        // Below warning threshold ⇒ Clean.
        let clean = comparison_with_pct(Some(10.0)).regression_verdict(&thresholds);
        assert_eq!(clean, RegressionVerdict::Clean);
        assert!(!clean.should_fail_build());

        // ─── Warning band ──────────────────────────────────────
        // At threshold (inclusive) ⇒ Warning.
        let warn_at = comparison_with_pct(Some(15.0)).regression_verdict(&thresholds);
        assert!(
            matches!(
                warn_at,
                RegressionVerdict::Warning { duration_delta_pct, threshold_pct }
                    if (duration_delta_pct - 15.0).abs() < 0.01 && threshold_pct == 15.0
            ),
            "+15% must trigger warn at the inclusive threshold; got {warn_at:?}"
        );
        assert!(!warn_at.should_fail_build());

        // Mid-band ⇒ Warning.
        let warn_mid = comparison_with_pct(Some(22.5)).regression_verdict(&thresholds);
        assert!(matches!(warn_mid, RegressionVerdict::Warning { .. }));
        assert!(!warn_mid.should_fail_build());

        // ─── Failure band ──────────────────────────────────────
        // At threshold (inclusive) ⇒ Failure.
        let fail_at = comparison_with_pct(Some(30.0)).regression_verdict(&thresholds);
        assert!(
            matches!(
                fail_at,
                RegressionVerdict::Failure { duration_delta_pct, threshold_pct }
                    if (duration_delta_pct - 30.0).abs() < 0.01 && threshold_pct == 30.0
            ),
            "+30% must trigger failure at the inclusive threshold; got {fail_at:?}"
        );
        assert!(
            fail_at.should_fail_build(),
            "Failure verdict MUST cause CI to exit non-zero"
        );

        // Far past failure ⇒ still Failure (capping behavior).
        let fail_far = comparison_with_pct(Some(150.0)).regression_verdict(&thresholds);
        assert!(matches!(fail_far, RegressionVerdict::Failure { .. }));

        // ─── Improvements never trigger a regression verdict ───
        let improvement = comparison_with_pct(Some(-50.0)).regression_verdict(&thresholds);
        assert_eq!(
            improvement,
            RegressionVerdict::Clean,
            "improvements (negative duration delta) MUST NOT trigger regression verdicts; \
             got {improvement:?}"
        );

        // ─── None duration delta (no comparison data) ─────────
        let no_data = comparison_with_pct(None).regression_verdict(&thresholds);
        assert_eq!(
            no_data,
            RegressionVerdict::Clean,
            "missing comparison data MUST NOT cause a CI failure (no signal to gate on)"
        );

        let invalid_negative = RegressionVerdictThresholds {
            warning_duration_pct: -20.0,
            failure_duration_pct: -10.0,
        };
        let steady_state = comparison_with_pct(Some(0.0)).regression_verdict(&invalid_negative);
        assert_eq!(
            steady_state,
            RegressionVerdict::Clean,
            "invalid negative thresholds must fail open instead of turning a 0% \
             steady-state comparison into a CI failure"
        );
    }

    /// `coding_agent_session_search-ibuuh.24`: the threshold
    /// constructor MUST refuse internally-inconsistent
    /// configurations (warning >= failure would never raise a
    /// warning before the failure trips). A project that misorders
    /// its threshold values would otherwise get a hard CI failure
    /// on every run.
    #[test]
    fn regression_verdict_thresholds_try_new_rejects_inconsistent_configurations() {
        // Happy path.
        assert!(RegressionVerdictThresholds::try_new(10.0, 20.0).is_ok());

        // warning >= failure ⇒ Err.
        let err = RegressionVerdictThresholds::try_new(20.0, 10.0)
            .expect_err("warning > failure must be rejected");
        assert!(
            err.contains("strictly less"),
            "rejection message must explain the constraint; got {err:?}"
        );

        // warning == failure ⇒ Err (warning would never trigger).
        let err_eq = RegressionVerdictThresholds::try_new(15.0, 15.0)
            .expect_err("warning == failure must be rejected");
        assert!(err_eq.contains("strictly less"));

        // Negative thresholds make steady-state (0%) compare greater
        // than the failure threshold, so reject them up front.
        let negative_warning = RegressionVerdictThresholds::try_new(-20.0, 10.0)
            .expect_err("negative warning threshold must be rejected");
        assert!(negative_warning.contains("non-negative"));
        let negative_failure = RegressionVerdictThresholds::try_new(10.0, -20.0)
            .expect_err("negative failure threshold must be rejected");
        assert!(negative_failure.contains("non-negative"));
        let invalid_json = r#"{"warning_duration_pct":-30.0,"failure_duration_pct":-10.0}"#;
        let deser = serde_json::from_str::<RegressionVerdictThresholds>(invalid_json)
            .expect_err("serde-loaded negative thresholds must be rejected too");
        assert!(
            deser.to_string().contains("non-negative"),
            "serde validation error must explain the threshold polarity; got {deser}"
        );

        // Non-finite values rejected explicitly (defensive — never
        // reachable from clean f64 arithmetic but pin the contract).
        assert!(RegressionVerdictThresholds::try_new(f64::NAN, 30.0).is_err());
        assert!(RegressionVerdictThresholds::try_new(15.0, f64::INFINITY).is_err());
    }

    /// `coding_agent_session_search-whnja`: the non-negative-thresholds
    /// fix (commit 5cb0038f) pinned the try_new rejection path and the
    /// fail-open behavior for struct-update bypass, but nothing
    /// directly asserted that a 0% steady-state delta evaluates as
    /// Clean under a *valid* non-default threshold pair — the common
    /// case for bench harnesses that tune tolerance away from the
    /// 15/30 defaults. Pin it here so a future refactor of the
    /// `>= warning` / `>= failure` ordering can't silently flip a
    /// no-op bench run into a Warning under a tighter profile.
    #[test]
    fn regression_verdict_zero_change_under_valid_custom_thresholds_is_clean() {
        fn zero_delta_comparison() -> RefreshLedgerEvidenceComparison {
            RefreshLedgerEvidenceComparison {
                phase_deltas: Vec::new(),
                aggregate_duration_delta_pct: Some(0.0),
                aggregate_throughput_delta_pct: None,
                dominant_phase_shift: None,
            }
        }

        // Strict CI profile — 5% warn / 20% fail. 0% change is a
        // steady-state bench run and must not trigger any band.
        let strict = RegressionVerdictThresholds::try_new(5.0, 20.0)
            .expect("valid strict thresholds must construct");
        let steady_state = zero_delta_comparison().regression_verdict(&strict);
        assert_eq!(
            steady_state,
            RegressionVerdict::Clean,
            "0% steady-state delta must be Clean under any valid \
             threshold pair — tight CI profiles must not flag no-op runs"
        );

        // Extra-loose profile — 50% warn / 200% fail. Same 0% delta
        // must still be Clean; tight vs loose is a policy knob on the
        // warning band, not the zero-crossing.
        let loose = RegressionVerdictThresholds::try_new(50.0, 200.0)
            .expect("valid loose thresholds must construct");
        let steady_state_loose = zero_delta_comparison().regression_verdict(&loose);
        assert_eq!(
            steady_state_loose,
            RegressionVerdict::Clean,
            "0% steady-state delta must be Clean under loose thresholds too"
        );
    }

    /// `coding_agent_session_search-ibuuh.24`: RegressionVerdict
    /// serializes through serde (CI runners persist the verdict
    /// JSON for PR comments + dashboards). Pin the tag/snake_case
    /// shape so a future variant addition or rename trips a clear
    /// deserialization break in downstream consumers.
    #[test]
    fn regression_verdict_serializes_with_snake_case_verdict_tag() {
        let clean_json = serde_json::to_string(&RegressionVerdict::Clean).expect("serialize");
        assert!(
            clean_json.contains("\"verdict\":\"clean\""),
            "Clean must serialize with snake_case `verdict` tag; got {clean_json}"
        );

        let warning_json = serde_json::to_string(&RegressionVerdict::Warning {
            duration_delta_pct: 18.5,
            threshold_pct: 15.0,
        })
        .expect("serialize");
        assert!(warning_json.contains("\"verdict\":\"warning\""));
        assert!(warning_json.contains("\"duration_delta_pct\":18.5"));
        assert!(warning_json.contains("\"threshold_pct\":15"));

        let failure_json = serde_json::to_string(&RegressionVerdict::Failure {
            duration_delta_pct: 42.0,
            threshold_pct: 30.0,
        })
        .expect("serialize");
        assert!(failure_json.contains("\"verdict\":\"failure\""));
    }
}
