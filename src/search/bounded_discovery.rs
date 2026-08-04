// Dead-code tolerated module-wide: this bounded-discovery engine lands ahead
// of the native incident miner (.10.x) that feeds it real history files and
// the robot surface that emits its report.
#![allow(dead_code)]

//! Bounded candidate discovery for incident mining (bead
//! cass-fleet-resilience-20260608-uojcg.10.2).
//!
//! Incident mining must be safe on huge corpora: the report saw 500k–4.86M
//! parsed lines per host from raw scans. This module is the bounded engine —
//! [`BoundedDiscovery`] enforces file/line/byte caps and an elapsed budget as
//! the caller feeds it files and lines, stops at the first cap, and emits a
//! [`DiscoveryReport`] with `partial=true` and the cap(s) that stopped it.
//!
//! Evidence is recorded as bounded [`EvidencePointer`]s (category + file/line/
//! byte location) — never raw JSONL lines — so a default report can never dump
//! private content (the redaction is `.10.5`). Deterministic: the caller
//! passes `now_ms`, so there are no clock calls. All enums serialize as
//! snake_case.

use serde::{Deserialize, Serialize};

use crate::search::incident_categories::IncidentCategory;

/// The caps that bound a discovery pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DiscoveryCaps {
    pub max_files: u64,
    pub max_lines: u64,
    pub max_bytes: u64,
    pub time_budget_ms: i64,
    /// Max evidence pointers kept (bounds report size).
    pub max_evidence: usize,
}

impl Default for DiscoveryCaps {
    fn default() -> Self {
        Self {
            max_files: 200,
            max_lines: 200_000,
            max_bytes: 64 * 1024 * 1024, // 64 MiB
            time_budget_ms: 5_000,
            max_evidence: 50,
        }
    }
}

/// Which cap stopped (or would stop) the pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BudgetHit {
    Files,
    Lines,
    Bytes,
    Time,
}

/// A bounded pointer to evidence — location only, never raw content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct EvidencePointer {
    pub category: IncidentCategory,
    pub file_index: u64,
    pub line_number: u64,
    pub byte_offset: u64,
}

/// The report a bounded discovery pass emits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DiscoveryReport {
    pub caps: DiscoveryCaps,
    pub files_considered: u64,
    pub files_scanned: u64,
    pub lines_scanned: u64,
    pub bytes_scanned: u64,
    pub elapsed_ms: i64,
    /// True when a cap stopped the pass before exhausting the corpus.
    pub partial: bool,
    /// The cap(s) that were hit (sorted, deduped).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub budget_hits: Vec<BudgetHit>,
    /// Whether the time budget was exceeded (a `timed_out` category).
    pub timed_out: bool,
    /// Bounded top evidence pointers (no raw content).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub top_evidence: Vec<EvidencePointer>,
}

/// The bounded discovery accumulator. Feed it files via [`begin_file`] and
/// lines via [`record_line`]; both return `false` once a cap is hit so the
/// caller stops promptly. Call [`finish`] for the report.
pub(crate) struct BoundedDiscovery {
    caps: DiscoveryCaps,
    started_at_ms: i64,
    files_considered: u64,
    files_scanned: u64,
    lines_scanned: u64,
    bytes_scanned: u64,
    budget_hits: Vec<BudgetHit>,
    evidence: Vec<EvidencePointer>,
    stopped: bool,
}

impl BoundedDiscovery {
    pub(crate) fn start(caps: DiscoveryCaps, started_at_ms: i64) -> Self {
        Self {
            caps,
            started_at_ms,
            files_considered: 0,
            files_scanned: 0,
            lines_scanned: 0,
            bytes_scanned: 0,
            budget_hits: Vec::new(),
            evidence: Vec::new(),
            stopped: false,
        }
    }

    fn note_hit(&mut self, hit: BudgetHit) {
        self.stopped = true;
        if !self.budget_hits.contains(&hit) {
            self.budget_hits.push(hit);
        }
    }

    fn time_exceeded(&self, now_ms: i64) -> bool {
        (now_ms - self.started_at_ms).max(0) > self.caps.time_budget_ms
    }

    /// Consider a file. Returns `false` (and does not scan) when the file cap
    /// or time budget is already hit.
    pub(crate) fn begin_file(&mut self, now_ms: i64) -> bool {
        if self.stopped {
            return false;
        }
        if self.time_exceeded(now_ms) {
            self.note_hit(BudgetHit::Time);
            return false;
        }
        self.files_considered += 1;
        if self.files_scanned >= self.caps.max_files {
            self.note_hit(BudgetHit::Files);
            return false;
        }
        self.files_scanned += 1;
        true
    }

    /// Record a scanned line of `byte_len` bytes. Returns `false` when a cap
    /// is hit (the caller must stop scanning).
    pub(crate) fn record_line(&mut self, byte_len: u64, now_ms: i64) -> bool {
        if self.stopped {
            return false;
        }
        self.lines_scanned += 1;
        self.bytes_scanned += byte_len;
        if self.lines_scanned >= self.caps.max_lines {
            self.note_hit(BudgetHit::Lines);
            return false;
        }
        if self.bytes_scanned >= self.caps.max_bytes {
            self.note_hit(BudgetHit::Bytes);
            return false;
        }
        if self.time_exceeded(now_ms) {
            self.note_hit(BudgetHit::Time);
            return false;
        }
        true
    }

    /// Record an evidence pointer (bounded to `max_evidence`).
    pub(crate) fn record_evidence(&mut self, pointer: EvidencePointer) {
        if self.evidence.len() < self.caps.max_evidence {
            self.evidence.push(pointer);
        }
    }

    /// Whether a cap has stopped the pass.
    pub(crate) fn is_stopped(&self) -> bool {
        self.stopped
    }

    /// Finalize the report.
    pub(crate) fn finish(mut self, now_ms: i64) -> DiscoveryReport {
        let mut budget_hits = std::mem::take(&mut self.budget_hits);
        budget_hits.sort();
        DiscoveryReport {
            caps: self.caps,
            files_considered: self.files_considered,
            files_scanned: self.files_scanned,
            lines_scanned: self.lines_scanned,
            bytes_scanned: self.bytes_scanned,
            elapsed_ms: (now_ms - self.started_at_ms).max(0),
            partial: self.stopped,
            timed_out: budget_hits.contains(&BudgetHit::Time),
            budget_hits,
            top_evidence: self.evidence,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small_caps() -> DiscoveryCaps {
        DiscoveryCaps {
            max_files: 3,
            max_lines: 10,
            max_bytes: 1_000,
            time_budget_ms: 1_000,
            max_evidence: 2,
        }
    }

    #[test]
    fn enums_serialize_snake_case() {
        assert_eq!(
            serde_json::to_string(&BudgetHit::Lines).unwrap(),
            "\"lines\""
        );
    }

    #[test]
    fn under_caps_completes_without_partial() {
        let mut d = BoundedDiscovery::start(small_caps(), 0);
        assert!(d.begin_file(0));
        for _ in 0..5 {
            assert!(d.record_line(10, 0));
        }
        let report = d.finish(100);
        assert!(!report.partial);
        assert!(report.budget_hits.is_empty());
        assert!(!report.timed_out);
        assert_eq!(report.files_scanned, 1);
        assert_eq!(report.lines_scanned, 5);
        assert_eq!(report.bytes_scanned, 50);
        assert_eq!(report.elapsed_ms, 100);
    }

    #[test]
    fn line_cap_stops_the_pass_partial() {
        let mut d = BoundedDiscovery::start(small_caps(), 0);
        d.begin_file(0);
        let mut keep = true;
        let mut n = 0;
        while keep {
            keep = d.record_line(1, 0);
            n += 1;
            assert!(n <= 10, "must stop at the line cap");
        }
        let report = d.finish(10);
        assert!(report.partial);
        assert!(report.budget_hits.contains(&BudgetHit::Lines));
        assert_eq!(report.lines_scanned, small_caps().max_lines);
    }

    #[test]
    fn byte_cap_stops_the_pass() {
        let mut d = BoundedDiscovery::start(small_caps(), 0);
        d.begin_file(0);
        // 1000-byte cap: a single 1000-byte line trips it.
        assert!(!d.record_line(1_000, 0));
        let report = d.finish(5);
        assert!(report.partial);
        assert!(report.budget_hits.contains(&BudgetHit::Bytes));
    }

    #[test]
    fn file_cap_stops_further_files() {
        let mut d = BoundedDiscovery::start(small_caps(), 0);
        assert!(d.begin_file(0));
        assert!(d.begin_file(0));
        assert!(d.begin_file(0));
        // Fourth file exceeds max_files=3.
        assert!(!d.begin_file(0));
        let report = d.finish(1);
        assert!(report.partial);
        assert!(report.budget_hits.contains(&BudgetHit::Files));
        assert_eq!(report.files_scanned, 3);
        assert_eq!(report.files_considered, 4);
    }

    #[test]
    fn time_budget_stops_and_marks_timed_out() {
        let mut d = BoundedDiscovery::start(small_caps(), 0);
        assert!(d.begin_file(0));
        // now_ms beyond the 1000ms budget on the next line.
        assert!(!d.record_line(1, 2_000));
        let report = d.finish(2_000);
        assert!(report.partial);
        assert!(report.timed_out);
        assert!(report.budget_hits.contains(&BudgetHit::Time));
    }

    #[test]
    fn begin_file_refuses_once_time_budget_exceeded() {
        let mut d = BoundedDiscovery::start(small_caps(), 0);
        assert!(!d.begin_file(5_000), "over budget: no new file scanned");
        let report = d.finish(5_000);
        assert!(report.timed_out);
        assert_eq!(report.files_scanned, 0);
    }

    #[test]
    fn evidence_pointers_are_bounded_and_carry_no_raw_content() {
        let mut d = BoundedDiscovery::start(small_caps(), 0);
        d.begin_file(0);
        for i in 0..5 {
            d.record_evidence(EvidencePointer {
                category: IncidentCategory::QuarantineOom,
                file_index: 0,
                line_number: i,
                byte_offset: i * 10,
            });
        }
        let report = d.finish(1);
        // Bounded to max_evidence=2.
        assert_eq!(report.top_evidence.len(), 2);
        // EvidencePointer is location-only — serialized form has no text field.
        let json = serde_json::to_string(&report.top_evidence[0]).unwrap();
        assert!(json.contains("\"line_number\""));
        assert!(!json.contains("text") && !json.contains("content"));
    }

    #[test]
    fn report_round_trips_through_json() {
        let mut d = BoundedDiscovery::start(small_caps(), 0);
        d.begin_file(0);
        d.record_line(1_000, 0);
        let report = d.finish(5);
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("\"partial\":true"));
        assert!(json.contains("\"budget_hits\":[\"bytes\"]"));
        let parsed: DiscoveryReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, report);
    }
}
