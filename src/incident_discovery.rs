//! Bounded candidate discovery for native incident mining.
//!
//! Bead: coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.10.2
//! ("Implement bounded candidate discovery with caps progress and partial
//! results").
//!
//! Incident mining must be safe on huge corpora — the field hit 500k–4.86M parsed
//! lines from as few as 150 files. This module is the bounded-scan engine that
//! makes that safe: file/line/byte caps plus an elapsed budget, a running
//! accountant, and a partial/timed-out [`DiscoveryReport`] instead of an
//! unbounded raw scan. It is pure logic over scan progress (the caller does the
//! actual filesystem reads and feeds counts in), so it is fully deterministic and
//! unit-testable; it composes the bead-2.2 [`RobotBudget`](crate::robot_budget_envelope::RobotBudget)
//! for the time budget.
//!
//! Privacy: evidence is surfaced as bounded [`EvidencePointer`]s (file + line +
//! optional short reason), never raw long JSONL lines — the report's
//! "no raw long lines dumped by default" requirement.

use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use frankensqlite::compat::{ConnectionExt, ParamValue, RowExt};
use frankensqlite::{Connection, Row};
use serde::{Deserialize, Serialize};

use crate::analytics::{AnalyticsFilter, SourceFilter};
use crate::search::incident_categories::classify_text;
use crate::search::incident_redaction::{
    RawIncidentEvidence, RedactionManifest, RedactionPolicy, default_robot_manifest, redact,
};
use crate::top_session_summary::{
    IncidentHit, SessionExistsState, TopSessionAccumulator, TopSessionEntry,
};

/// Default caps, tuned so a worst-case corpus (millions of lines) cannot wedge a
/// robot command. Overridable per call.
pub const DEFAULT_MAX_FILES: u64 = 2_000;
pub const DEFAULT_MAX_LINES: u64 = 250_000;
pub const DEFAULT_MAX_BYTES: u64 = 256 * 1024 * 1024;
pub const DEFAULT_BUDGET_MS: u64 = 8_000;
/// Cap on retained evidence pointers, so the report itself stays bounded.
pub const DEFAULT_MAX_EVIDENCE: usize = 50;

/// The caps governing a bounded discovery scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveryCaps {
    pub max_files: u64,
    pub max_lines: u64,
    pub max_bytes: u64,
    pub budget_ms: u64,
    pub max_evidence: usize,
}

impl Default for DiscoveryCaps {
    fn default() -> Self {
        Self {
            max_files: DEFAULT_MAX_FILES,
            max_lines: DEFAULT_MAX_LINES,
            max_bytes: DEFAULT_MAX_BYTES,
            budget_ms: DEFAULT_BUDGET_MS,
            max_evidence: DEFAULT_MAX_EVIDENCE,
        }
    }
}

/// Why a bounded scan stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StopReason {
    /// All candidates were scanned within every cap.
    Completed,
    /// The file cap was reached.
    FilesCapped,
    /// The line cap was reached.
    LinesCapped,
    /// The byte cap was reached.
    BytesCapped,
    /// A single message exceeded the per-message fragment bound.
    MessageFragmentCapped,
    /// The elapsed-time budget was exceeded.
    TimedOut,
}

impl StopReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            StopReason::Completed => "completed",
            StopReason::FilesCapped => "files-capped",
            StopReason::LinesCapped => "lines-capped",
            StopReason::BytesCapped => "bytes-capped",
            StopReason::MessageFragmentCapped => "message-fragment-capped",
            StopReason::TimedOut => "timed-out",
        }
    }

    /// `true` for every reason except [`StopReason::Completed`] — i.e. the scan
    /// returned a partial result.
    pub const fn is_partial(self) -> bool {
        !matches!(self, StopReason::Completed)
    }
}

/// A bounded pointer to discovered evidence. Carries location + an optional short
/// reason, never a raw long line (privacy / bounded-output requirement).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidencePointer {
    pub file: String,
    pub line: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// Short, prose-free reason/marker (e.g. `"err.kind=OpenRead"`); NOT the raw
    /// JSONL line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Running accountant for a bounded scan. The caller drives the actual reads and
/// reports progress; this enforces caps and accumulates bounded evidence.
#[derive(Debug, Clone)]
pub struct DiscoveryAccountant {
    caps: DiscoveryCaps,
    files_considered: u64,
    files_scanned: u64,
    lines_scanned: u64,
    bytes_scanned: u64,
    evidence: Vec<EvidencePointer>,
    evidence_truncated: bool,
}

impl DiscoveryAccountant {
    pub fn new(caps: DiscoveryCaps) -> Self {
        Self {
            caps,
            files_considered: 0,
            files_scanned: 0,
            lines_scanned: 0,
            bytes_scanned: 0,
            evidence: Vec::new(),
            evidence_truncated: false,
        }
    }

    /// Record that a candidate file was considered (enumerated) but not
    /// necessarily scanned.
    pub fn note_file_considered(&mut self) {
        self.files_considered = self.files_considered.saturating_add(1);
    }

    /// Consider and begin one candidate file/conversation. Returns false before
    /// scanning when the time or file cap is already exhausted.
    pub fn begin_file(&mut self, elapsed_ms: u64) -> bool {
        self.note_file_considered();
        self.begin_scanned_file(elapsed_ms)
    }

    fn begin_scanned_file(&mut self, elapsed_ms: u64) -> bool {
        if self.scan_stop_reason(elapsed_ms).is_some() || self.files_scanned >= self.caps.max_files
        {
            return false;
        }
        self.files_scanned = self.files_scanned.saturating_add(1);
        true
    }

    /// Record a fully-scanned file's line/byte contribution.
    pub fn note_file_scanned(&mut self, lines: u64, bytes: u64) {
        self.files_scanned = self.files_scanned.saturating_add(1);
        self.lines_scanned = self.lines_scanned.saturating_add(lines);
        self.bytes_scanned = self.bytes_scanned.saturating_add(bytes);
    }

    /// Record bounded progress within the current file/conversation. Callers
    /// must cap `lines` and `bytes` to the remaining allowance first.
    pub fn note_scan_progress(&mut self, lines: u64, bytes: u64) {
        self.lines_scanned = self.lines_scanned.saturating_add(lines);
        self.bytes_scanned = self.bytes_scanned.saturating_add(bytes);
    }

    pub fn remaining_lines(&self) -> u64 {
        self.caps.max_lines.saturating_sub(self.lines_scanned)
    }

    pub fn remaining_bytes(&self) -> u64 {
        self.caps.max_bytes.saturating_sub(self.bytes_scanned)
    }

    /// Stop reason applicable while scanning inside the current candidate.
    /// File-cap checks belong at [`Self::begin_file`], otherwise beginning the
    /// final allowed file would incorrectly prevent scanning its messages.
    pub fn scan_stop_reason(&self, elapsed_ms: u64) -> Option<StopReason> {
        if elapsed_ms >= self.caps.budget_ms {
            Some(StopReason::TimedOut)
        } else if self.lines_scanned >= self.caps.max_lines {
            Some(StopReason::LinesCapped)
        } else if self.bytes_scanned >= self.caps.max_bytes {
            Some(StopReason::BytesCapped)
        } else {
            None
        }
    }

    /// Record a piece of evidence, bounded by `max_evidence` (further evidence is
    /// dropped and the report marks `evidence_truncated`).
    pub fn push_evidence(&mut self, pointer: EvidencePointer) {
        if self.evidence.len() < self.caps.max_evidence {
            self.evidence.push(pointer);
        } else {
            self.evidence_truncated = true;
        }
    }

    /// Decide whether the scan must stop now, given `elapsed_ms`. Returns `None`
    /// to continue. Time is checked first (a slow scan should yield promptly),
    /// then the size caps.
    pub fn stop_reason(&self, elapsed_ms: u64) -> Option<StopReason> {
        if elapsed_ms >= self.caps.budget_ms {
            Some(StopReason::TimedOut)
        } else if self.files_scanned >= self.caps.max_files {
            Some(StopReason::FilesCapped)
        } else if self.lines_scanned >= self.caps.max_lines {
            Some(StopReason::LinesCapped)
        } else if self.bytes_scanned >= self.caps.max_bytes {
            Some(StopReason::BytesCapped)
        } else {
            None
        }
    }

    /// Finalize into a [`DiscoveryReport`]. `elapsed_ms` is the scan's wall-clock;
    /// `all_considered_scanned` is whether every considered file was scanned
    /// (drives `Completed` vs. a cap reason when no cap tripped mid-scan).
    pub fn finalize(self, elapsed_ms: u64, all_considered_scanned: bool) -> DiscoveryReport {
        let stop_reason = if elapsed_ms >= self.caps.budget_ms {
            StopReason::TimedOut
        } else if all_considered_scanned {
            // Exact-at-cap is still complete when a limit+1 sentinel proved no
            // work remained. Hitting a numeric boundary is not itself partial.
            StopReason::Completed
        } else {
            self.stop_reason(elapsed_ms)
                .unwrap_or(StopReason::FilesCapped)
        };
        DiscoveryReport {
            schema_version: DISCOVERY_SCHEMA_VERSION,
            caps: self.caps,
            files_considered: self.files_considered,
            files_scanned: self.files_scanned,
            lines_scanned: self.lines_scanned,
            bytes_scanned: self.bytes_scanned,
            elapsed_ms,
            stop_reason,
            timed_out: stop_reason == StopReason::TimedOut,
            partial: stop_reason.is_partial(),
            evidence_truncated: self.evidence_truncated,
            evidence: self.evidence,
        }
    }
}

/// Stable schema version for the discovery-report wire format.
pub const DISCOVERY_SCHEMA_VERSION: u32 = 1;

/// The bounded-discovery report. Stable snake_case JSON; `partial`/`timed_out`
/// let an agent act on incomplete results, and evidence is bounded pointers only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveryReport {
    pub schema_version: u32,
    pub caps: DiscoveryCaps,
    pub files_considered: u64,
    pub files_scanned: u64,
    pub lines_scanned: u64,
    pub bytes_scanned: u64,
    pub elapsed_ms: u64,
    pub stop_reason: StopReason,
    pub timed_out: bool,
    pub partial: bool,
    /// `true` when more evidence was found than [`DiscoveryCaps::max_evidence`].
    pub evidence_truncated: bool,
    #[serde(default)]
    pub evidence: Vec<EvidencePointer>,
}

/// Stable schema for the live incident-mining report.
pub const INCIDENT_MINING_SCHEMA_VERSION: u32 = 2;
const CONVERSATION_QUERY_HARD_CAP: u64 = 10_000;
const CONVERSATION_BATCH_ROWS: usize = 128;
const MESSAGE_BATCH_ROWS: usize = 128;
const MESSAGE_FRAGMENT_CHARS: i64 = 4_096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncidentScanUnits {
    pub files: String,
    pub lines: String,
    pub bytes: String,
    pub candidate_order: String,
    pub message_fragment_chars: u64,
}

/// Live, bounded incident-mining report over canonical archive rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct IncidentMiningReport {
    pub schema_version: u32,
    /// Scope note is load-bearing when `discovery.partial=true`: every count is
    /// over scanned candidates, never an implied full-corpus total.
    pub count_scope: String,
    pub scan_units: IncidentScanUnits,
    pub discovery: DiscoveryReport,
    /// Ranked sessions, bounded by the requested top-N limit.
    pub top_sessions: Vec<TopSessionEntry>,
    /// Distinct incident-bearing sessions observed before the top-N limit.
    pub total_sessions: usize,
    /// Total categorized hits observed across the scanned candidate scope.
    pub total_hits: usize,
    /// Whether incident-bearing sessions were omitted by the top-N limit.
    pub top_sessions_truncated: bool,
    pub redaction: RedactionManifest,
}

#[derive(Debug)]
struct CandidateConversation {
    conversation_id: i64,
    session_id: String,
    agent: String,
    workspace_id: Option<i64>,
    source_path: String,
    source_id: String,
    origin_host: Option<String>,
    effective_started_at: i64,
}

struct CandidateWindow {
    candidates: Vec<CandidateConversation>,
    corpus_exhausted: bool,
    timed_out: bool,
}

#[derive(Debug)]
struct CandidateMessage {
    idx: i64,
    content: String,
    content_was_truncated: bool,
    fragment_limited_by_byte_budget: bool,
}

fn normalized_source_id(source_id: &str) -> String {
    let trimmed = source_id.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("local") {
        "local".to_string()
    } else {
        trimmed.to_string()
    }
}

fn normalized_source_identity(source_id: &str, origin_host: Option<&str>) -> String {
    let trimmed = source_id.trim();
    if trimmed.is_empty() {
        origin_host
            .map(str::trim)
            .filter(|host| !host.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| "local".to_string())
    } else {
        normalized_source_id(trimmed)
    }
}

fn normalize_epoch_millis(timestamp: i64) -> i64 {
    if (0..100_000_000_000).contains(&timestamp) {
        timestamp.saturating_mul(1_000)
    } else {
        timestamp
    }
}

fn query_candidate_conversations(
    conn: &Connection,
    max_files: u64,
    started: Instant,
    budget_ms: u64,
) -> Result<CandidateWindow> {
    let query_limit = max_files.min(CONVERSATION_QUERY_HARD_CAP).saturating_add(1);
    let query_limit = usize::try_from(query_limit).unwrap_or(usize::MAX);
    let mut candidates = Vec::with_capacity(query_limit.min(CONVERSATION_BATCH_ROWS));
    let mut before_id = None;
    let mut corpus_exhausted = false;
    let mut timed_out = false;

    while candidates.len() < query_limit {
        if elapsed_ms(started) >= budget_ms {
            timed_out = true;
            break;
        }
        let page_limit = (query_limit - candidates.len()).min(CONVERSATION_BATCH_ROWS);
        let (where_clause, params) = if let Some(id) = before_id {
            (
                "WHERE c.id < ?1",
                vec![
                    ParamValue::from(id),
                    ParamValue::from(i64::try_from(page_limit).unwrap_or(i64::MAX)),
                ],
            )
        } else {
            (
                "",
                vec![ParamValue::from(
                    i64::try_from(page_limit).unwrap_or(i64::MAX),
                )],
            )
        };
        let limit_parameter = if before_id.is_some() { 2 } else { 1 };
        let sql = format!(
            "SELECT c.id,
                    COALESCE(c.external_id, 'archive-row-' || c.id),
                    COALESCE((SELECT a.slug FROM agents a WHERE a.id = c.agent_id LIMIT 1), 'unknown'),
                    c.workspace_id,
                    c.source_path,
                    COALESCE(c.source_id, ''),
                    c.origin_host,
                    COALESCE(c.started_at, c.ended_at, 0)
               FROM conversations c
               {where_clause}
              ORDER BY c.id DESC
              LIMIT ?{limit_parameter}"
        );
        let page = conn
            .query_map_collect(&sql, &params, |row: &Row| {
                let raw_source_id: String = row.get_typed(5)?;
                let origin_host: Option<String> = row.get_typed(6)?;
                Ok(CandidateConversation {
                    conversation_id: row.get_typed(0)?,
                    session_id: row.get_typed(1)?,
                    agent: row.get_typed(2)?,
                    workspace_id: row.get_typed(3)?,
                    source_path: row.get_typed(4)?,
                    source_id: normalized_source_identity(&raw_source_id, origin_host.as_deref()),
                    origin_host,
                    effective_started_at: normalize_epoch_millis(row.get_typed(7)?),
                })
            })
            .context("querying bounded incident candidate conversation page")?;
        if page.is_empty() {
            corpus_exhausted = true;
            break;
        }
        let page_was_short = page.len() < page_limit;
        before_id = page.last().map(|candidate| candidate.conversation_id);
        candidates.extend(page);
        if page_was_short {
            corpus_exhausted = true;
            break;
        }
    }

    if candidates.len() > usize::try_from(max_files).unwrap_or(usize::MAX) {
        candidates.truncate(usize::try_from(max_files).unwrap_or(usize::MAX));
        corpus_exhausted = false;
    }

    Ok(CandidateWindow {
        candidates,
        corpus_exhausted,
        timed_out,
    })
}

fn candidate_matches_filter(candidate: &CandidateConversation, filter: &AnalyticsFilter) -> bool {
    if filter
        .since_ms
        .is_some_and(|since| candidate.effective_started_at < since)
        || filter
            .until_ms
            .is_some_and(|until| candidate.effective_started_at > until)
        || (!filter.agents.is_empty() && !filter.agents.contains(&candidate.agent))
        || (!filter.workspace_ids.is_empty()
            && candidate
                .workspace_id
                .is_none_or(|id| !filter.workspace_ids.contains(&id)))
    {
        return false;
    }

    match &filter.source {
        SourceFilter::All => true,
        SourceFilter::Local => candidate.source_id == "local",
        SourceFilter::Remote => candidate.source_id != "local",
        SourceFilter::Specific(source_id) => candidate.source_id == normalized_source_id(source_id),
    }
}

fn query_candidate_messages(
    conn: &Connection,
    conversation_id: i64,
    after_idx: i64,
    remaining_lines: u64,
    remaining_bytes: u64,
) -> Result<Vec<CandidateMessage>> {
    let batch_rows = usize::try_from(remaining_lines)
        .unwrap_or(usize::MAX)
        .min(MESSAGE_BATCH_ROWS);
    let limit = i64::try_from(batch_rows.saturating_add(1)).unwrap_or(i64::MAX);
    let fragment_chars = i64::try_from(remaining_bytes)
        .unwrap_or(i64::MAX)
        .clamp(1, MESSAGE_FRAGMENT_CHARS);
    let fragment_limited_by_byte_budget = fragment_chars < MESSAGE_FRAGMENT_CHARS;
    conn.query_map_collect(
        "SELECT idx, substr(content, 1, ?3), length(content) > ?3
           FROM messages
          WHERE conversation_id = ?1 AND idx > ?2
          ORDER BY idx ASC, id ASC
          LIMIT ?4",
        &[
            ParamValue::from(conversation_id),
            ParamValue::from(after_idx),
            ParamValue::from(fragment_chars),
            ParamValue::from(limit),
        ],
        |row: &Row| {
            Ok(CandidateMessage {
                idx: row.get_typed(0)?,
                content: row.get_typed(1)?,
                content_was_truncated: row.get_typed::<i64>(2)? != 0,
                fragment_limited_by_byte_budget,
            })
        },
    )
    .context("querying bounded incident candidate messages")
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn truncate_utf8_bytes(text: &str, max_bytes: usize) -> (&str, bool) {
    if text.len() <= max_bytes {
        return (text, false);
    }
    let mut end = max_bytes.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

fn existence_state(candidate: &CandidateConversation) -> SessionExistsState {
    if normalized_source_id(&candidate.source_id) != "local" {
        // A remote archive path is not a live path on this machine. Without an
        // explicit remote liveness receipt, existence is honestly unknown.
        return SessionExistsState::Unknown;
    }
    match Path::new(&candidate.source_path).try_exists() {
        Ok(true) => SessionExistsState::Exists,
        Ok(false) => SessionExistsState::ArchiveOnly,
        Err(_) => SessionExistsState::Unknown,
    }
}

fn evidence_line(idx: i64) -> u64 {
    u64::try_from(idx.saturating_add(1)).unwrap_or(1).max(1)
}

/// Scan canonical archive conversations and messages under strict caps. The
/// connection is supplied by the caller so CLI dispatch can enforce the
/// read-only/query-only lane.
pub(crate) fn scan_incidents(
    conn: &Connection,
    filter: &AnalyticsFilter,
    caps: DiscoveryCaps,
    top_n: usize,
    archive_db_path: Option<&Path>,
) -> Result<IncidentMiningReport> {
    let started = Instant::now();
    let candidate_window =
        query_candidate_conversations(conn, caps.max_files, started, caps.budget_ms)?;
    let mut accountant = DiscoveryAccountant::new(caps);
    let mut top_sessions = TopSessionAccumulator::default();
    let mut message_fragment_capped = false;
    let mut byte_cap_capped = false;
    let mut all_exhausted = candidate_window.corpus_exhausted && !candidate_window.timed_out;

    // `files_considered` is the number of archive rows the bounded candidate
    // query actually materialized, including rows later rejected by a
    // dimensional filter. Record the whole window before scanning so a query
    // that consumes the time budget still reports its enumerated work instead
    // of only the first row visited after the query returned.
    for _ in 0..candidate_window.candidates.len() {
        accountant.note_file_considered();
    }

    'conversations: for candidate in candidate_window.candidates {
        if !candidate_matches_filter(&candidate, filter) {
            continue;
        }
        let now_ms = elapsed_ms(started);
        if !accountant.begin_scanned_file(now_ms) {
            all_exhausted = false;
            break;
        }

        let exists_state = existence_state(&candidate);
        let host = candidate
            .origin_host
            .as_deref()
            .map(str::trim)
            .filter(|host| !host.is_empty())
            .unwrap_or(&candidate.source_id)
            .to_string();
        let mut after_idx = i64::MIN;

        loop {
            if accountant.scan_stop_reason(elapsed_ms(started)).is_some() {
                all_exhausted = false;
                break 'conversations;
            }
            let requested_rows = usize::try_from(accountant.remaining_lines())
                .unwrap_or(usize::MAX)
                .min(MESSAGE_BATCH_ROWS);
            let rows = query_candidate_messages(
                conn,
                candidate.conversation_id,
                after_idx,
                accountant.remaining_lines(),
                accountant.remaining_bytes(),
            )?;
            let has_more_rows = rows.len() > requested_rows;
            let batch_len = rows.len().min(requested_rows);
            if batch_len == 0 {
                break;
            }

            for message in rows.into_iter().take(batch_len) {
                if accountant.scan_stop_reason(elapsed_ms(started)) == Some(StopReason::TimedOut) {
                    all_exhausted = false;
                    break 'conversations;
                }
                if accountant.remaining_lines() == 0 || accountant.remaining_bytes() == 0 {
                    all_exhausted = false;
                    break 'conversations;
                }
                let remaining_bytes =
                    usize::try_from(accountant.remaining_bytes()).unwrap_or(usize::MAX);
                let (fragment, byte_cap_truncated) =
                    truncate_utf8_bytes(&message.content, remaining_bytes);
                accountant.note_scan_progress(1, u64::try_from(fragment.len()).unwrap_or(u64::MAX));

                for category in classify_text(fragment) {
                    let raw = RawIncidentEvidence {
                        category,
                        occurrence_count: 1,
                        raw_prompt_text: Some(fragment.to_string()),
                        raw_tool_payload: None,
                        source_path: Some(candidate.source_path.clone()),
                    };
                    let (redacted, _) = redact(&raw, RedactionPolicy::default());
                    accountant.push_evidence(EvidencePointer {
                        file: redacted
                            .source_path
                            .clone()
                            .unwrap_or_else(|| "[redacted]".to_string()),
                        line: evidence_line(message.idx),
                        category: Some(category.id().to_string()),
                        reason: redacted
                            .content_fingerprint
                            .as_ref()
                            .map(|fingerprint| format!("content_fingerprint={fingerprint}")),
                    });
                    top_sessions.push(IncidentHit {
                        conversation_id: candidate.conversation_id,
                        session_id: candidate.session_id.clone(),
                        agent: candidate.agent.clone(),
                        host: host.clone(),
                        source_path: candidate.source_path.clone(),
                        source_id: candidate.source_id.clone(),
                        origin_host: candidate.origin_host.clone(),
                        exists_state,
                        category,
                        redacted: true,
                        content_fingerprint: redacted.content_fingerprint,
                        evidence_path: redacted.source_path,
                    });
                }

                after_idx = message.idx;
                if byte_cap_truncated || message.content_was_truncated {
                    all_exhausted = false;
                    byte_cap_capped = byte_cap_truncated
                        || (message.content_was_truncated
                            && message.fragment_limited_by_byte_budget);
                    message_fragment_capped = message.content_was_truncated && !byte_cap_capped;
                    break 'conversations;
                }
            }

            if !has_more_rows {
                break;
            }
        }
    }

    let elapsed = elapsed_ms(started);
    let mut discovery = accountant.finalize(elapsed, all_exhausted);
    if byte_cap_capped && discovery.stop_reason != StopReason::TimedOut {
        discovery.stop_reason = StopReason::BytesCapped;
        discovery.partial = true;
    } else if message_fragment_capped && discovery.stop_reason != StopReason::TimedOut {
        discovery.stop_reason = StopReason::MessageFragmentCapped;
        discovery.partial = true;
    }
    let top_session_summary = top_sessions.finish(top_n, archive_db_path);
    Ok(IncidentMiningReport {
        schema_version: INCIDENT_MINING_SCHEMA_VERSION,
        count_scope: if discovery.partial {
            "scanned_candidates_partial".to_string()
        } else {
            "all_matching_candidates".to_string()
        },
        scan_units: IncidentScanUnits {
            files: "archive_conversations".to_string(),
            lines: "archive_messages".to_string(),
            bytes: "utf8_message_content_inspected".to_string(),
            candidate_order: "newest_archive_row_first".to_string(),
            message_fragment_chars: u64::try_from(MESSAGE_FRAGMENT_CHARS).unwrap_or(u64::MAX),
        },
        discovery,
        top_sessions: top_session_summary.top_sessions,
        total_sessions: top_session_summary.total_sessions,
        total_hits: top_session_summary.total_hits,
        top_sessions_truncated: top_session_summary.truncated,
        redaction: default_robot_manifest(),
    })
}

/// Build the truthful empty/partial response used when the CLI's outer hard
/// wall-clock guard expires before the synchronous storage worker can return a
/// verified scan result. The worker owns a read-only connection and its SQL is
/// independently row-bounded; this response deliberately claims no observed
/// rows because the in-flight result was not received.
pub(crate) fn hard_timeout_report(caps: DiscoveryCaps) -> IncidentMiningReport {
    let discovery = DiscoveryAccountant::new(caps).finalize(caps.budget_ms, false);
    IncidentMiningReport {
        schema_version: INCIDENT_MINING_SCHEMA_VERSION,
        count_scope: "no_verified_results_hard_timeout".to_string(),
        scan_units: IncidentScanUnits {
            files: "archive_conversations".to_string(),
            lines: "archive_messages".to_string(),
            bytes: "utf8_message_content_inspected".to_string(),
            candidate_order: "newest_archive_row_first".to_string(),
            message_fragment_chars: u64::try_from(MESSAGE_FRAGMENT_CHARS).unwrap_or(u64::MAX),
        },
        discovery,
        top_sessions: Vec::new(),
        total_sessions: 0,
        total_hits: 0,
        top_sessions_truncated: false,
        redaction: default_robot_manifest(),
    }
}

/// Run a read-only incident scan on a dedicated worker while keeping the
/// caller's result latency hard-bounded. Synchronous FrankenSQLite queries do
/// not currently expose an interrupt handle, so an expired caller discards the
/// in-flight result and returns [`hard_timeout_report`]; every worker query is
/// independently protected by the row/fragment caps in [`scan_incidents`].
pub(crate) fn run_incident_scan_with_hard_timeout<F>(
    caps: DiscoveryCaps,
    scan: F,
) -> Result<IncidentMiningReport>
where
    F: FnOnce() -> Result<IncidentMiningReport> + Send + 'static,
{
    let (scan_tx, scan_rx) = std::sync::mpsc::sync_channel(1);
    let _scan_worker = std::thread::Builder::new()
        .name("cass-incident-scan".to_string())
        .spawn(move || {
            let _ = scan_tx.send(scan());
        })
        .context("starting bounded incident scan worker")?;

    match scan_rx.recv_timeout(std::time::Duration::from_millis(caps.budget_ms)) {
        Ok(result) => result,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Ok(hard_timeout_report(caps)),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(anyhow!(
            "incident scan worker disconnected before returning a report"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn require(condition: bool, message: &str) -> Result<()> {
        if condition {
            Ok(())
        } else {
            Err(anyhow!("{message}"))
        }
    }

    fn incident_db() -> Connection {
        let conn = Connection::open(":memory:").unwrap();
        conn.execute_batch(
            "CREATE TABLE agents (
                 id INTEGER PRIMARY KEY,
                 slug TEXT NOT NULL
             );
             CREATE TABLE conversations (
                 id INTEGER PRIMARY KEY,
                 agent_id INTEGER NOT NULL,
                 workspace_id INTEGER,
                 source_id TEXT,
                 external_id TEXT,
                 source_path TEXT NOT NULL,
                 started_at INTEGER,
                 ended_at INTEGER,
                 origin_host TEXT
             );
             CREATE TABLE messages (
                 id INTEGER PRIMARY KEY,
                 conversation_id INTEGER NOT NULL,
                 idx INTEGER NOT NULL,
                 content TEXT NOT NULL
             );
             INSERT INTO agents(id, slug) VALUES (1, 'codex'), (2, 'claude_code');
             INSERT INTO conversations(
                 id, agent_id, workspace_id, source_id, external_id,
                 source_path, started_at, origin_host
             ) VALUES
                 (1, 1, 10, 'local', 'shared-session',
                  '/definitely/missing/private/local.jsonl', 200, NULL),
                 (2, 2, 20, 'remote-ts1', 'shared-session',
                  '/remote/private/remote.jsonl', 300, 'ts1'),
                 (3, 1, 10, 'local', 'ordinary-session',
                  '/definitely/missing/private/ordinary.jsonl', 100, NULL);
             INSERT INTO messages(id, conversation_id, idx, content) VALUES
                 (1, 1, 0,
                  'secret_token_123456789 cass health_class degraded recommended_action inspect semantic_fallback_lexical model missing database is locked busy'),
                 (2, 1, 1,
                  'cass index-ingest-out-of-memory quarantined_conversations=1'),
                 (3, 2, 0,
                  'cass source sync ssh permission denied auth timeout'),
                 (4, 3, 0,
                  'our app model auth timeout busy status rebuild is flaky');",
        )
        .unwrap();
        conn
    }

    fn scan_caps() -> DiscoveryCaps {
        DiscoveryCaps {
            max_files: 10,
            max_lines: 100,
            max_bytes: 1_000_000,
            budget_ms: 60_000,
            max_evidence: 20,
        }
    }

    fn small_caps() -> DiscoveryCaps {
        DiscoveryCaps {
            max_files: 3,
            max_lines: 100,
            max_bytes: 1_000,
            budget_ms: 1_000,
            max_evidence: 2,
        }
    }

    #[test]
    fn completed_when_all_scanned_within_caps() {
        let mut acc = DiscoveryAccountant::new(small_caps());
        acc.note_file_considered();
        acc.note_file_scanned(10, 100);
        let report = acc.finalize(50, true);
        assert_eq!(report.stop_reason, StopReason::Completed);
        assert!(!report.partial);
        assert!(!report.timed_out);
        assert_eq!(report.files_scanned, 1);
        assert_eq!(report.lines_scanned, 10);
    }

    #[test]
    fn files_cap_trips() {
        let mut acc = DiscoveryAccountant::new(small_caps());
        for _ in 0..3 {
            acc.note_file_considered();
            acc.note_file_scanned(1, 1);
        }
        assert_eq!(acc.stop_reason(10), Some(StopReason::FilesCapped));
        let report = acc.finalize(10, false);
        assert_eq!(report.stop_reason, StopReason::FilesCapped);
        assert!(report.partial);
    }

    #[test]
    fn lines_cap_trips() {
        let mut acc = DiscoveryAccountant::new(small_caps());
        acc.note_file_considered();
        acc.note_file_scanned(100, 10);
        assert_eq!(acc.stop_reason(10), Some(StopReason::LinesCapped));
    }

    #[test]
    fn bytes_cap_trips() {
        let mut acc = DiscoveryAccountant::new(small_caps());
        acc.note_file_considered();
        acc.note_file_scanned(1, 1_000);
        assert_eq!(acc.stop_reason(10), Some(StopReason::BytesCapped));
    }

    #[test]
    fn time_budget_takes_priority() {
        let mut acc = DiscoveryAccountant::new(small_caps());
        // Also over the line cap, but time is checked first.
        acc.note_file_scanned(100, 1);
        assert_eq!(acc.stop_reason(1_000), Some(StopReason::TimedOut));
        let report = acc.finalize(1_500, false);
        assert!(report.timed_out);
        assert!(report.partial);
        assert_eq!(report.stop_reason, StopReason::TimedOut);
    }

    #[test]
    fn evidence_is_bounded_and_marks_truncation() {
        let mut acc = DiscoveryAccountant::new(small_caps()); // max_evidence = 2
        for i in 0..5 {
            acc.push_evidence(EvidencePointer {
                file: format!("/s/{i}.jsonl"),
                line: i,
                category: Some("storage_busy_corrupt".to_string()),
                reason: Some("err.kind=OpenRead".to_string()),
            });
        }
        let report = acc.finalize(10, true);
        assert_eq!(report.evidence.len(), 2, "evidence retained is capped");
        assert!(report.evidence_truncated, "overflow marks truncation");
        // No raw long line is present — only bounded pointers.
        assert_eq!(
            report.evidence[0].reason.as_deref(),
            Some("err.kind=OpenRead")
        );
    }

    #[test]
    fn report_serializes_with_stable_fields() {
        let mut acc = DiscoveryAccountant::new(small_caps());
        acc.note_file_considered();
        acc.note_file_scanned(50, 500);
        acc.push_evidence(EvidencePointer {
            file: "/s/a.jsonl".to_string(),
            line: 12,
            category: None,
            reason: Some("oom".to_string()),
        });
        let report = acc.finalize(1_200, false);
        let value = serde_json::to_value(&report).unwrap();
        assert_eq!(value["schema_version"], DISCOVERY_SCHEMA_VERSION);
        assert_eq!(value["stop_reason"], "timed-out");
        assert_eq!(value["timed_out"], true);
        assert_eq!(value["partial"], true);
        assert_eq!(value["files_scanned"], 1);
        assert_eq!(value["lines_scanned"], 50);
        assert_eq!(value["bytes_scanned"], 500);
        assert_eq!(value["caps"]["max_files"], 3);
        assert_eq!(value["evidence"][0]["file"], "/s/a.jsonl");
        let back: DiscoveryReport = serde_json::from_value(value).unwrap();
        assert_eq!(back, report);
    }

    #[test]
    fn not_fully_scanned_without_cap_is_partial_not_completed() {
        let mut acc = DiscoveryAccountant::new(small_caps());
        acc.note_file_considered();
        acc.note_file_considered();
        acc.note_file_scanned(1, 1); // only 1 of 2 considered scanned
        let report = acc.finalize(10, false);
        assert!(report.partial, "incomplete scan must not claim completion");
        assert_ne!(report.stop_reason, StopReason::Completed);
    }

    #[test]
    fn stop_reason_wire_values_are_kebab() {
        for (r, w) in [
            (StopReason::Completed, "completed"),
            (StopReason::FilesCapped, "files-capped"),
            (StopReason::LinesCapped, "lines-capped"),
            (StopReason::BytesCapped, "bytes-capped"),
            (StopReason::MessageFragmentCapped, "message-fragment-capped"),
            (StopReason::TimedOut, "timed-out"),
        ] {
            assert_eq!(serde_json::to_string(&r).unwrap(), format!("\"{w}\""));
            assert_eq!(r.as_str(), w);
        }
    }

    #[test]
    fn default_caps_are_bounded() {
        let caps = DiscoveryCaps::default();
        assert!(caps.max_files > 0 && caps.max_lines > 0 && caps.max_bytes > 0);
        assert!(caps.budget_ms > 0 && caps.max_evidence > 0);
    }

    #[test]
    fn live_scanner_reports_bounded_redacted_top_sessions() {
        let report = scan_incidents(
            &incident_db(),
            &AnalyticsFilter::default(),
            scan_caps(),
            10,
            None,
        )
        .unwrap();

        assert_eq!(report.schema_version, INCIDENT_MINING_SCHEMA_VERSION);
        assert_eq!(report.discovery.stop_reason, StopReason::Completed);
        assert!(!report.discovery.partial);
        assert_eq!(report.discovery.files_scanned, 3);
        assert_eq!(report.discovery.lines_scanned, 4);
        assert_eq!(
            report.total_sessions, 2,
            "ordinary app text must not classify"
        );
        assert_eq!(report.top_sessions[0].conversation_id, 1);
        assert_eq!(report.top_sessions[0].session_id, "shared-session");
        assert_eq!(report.top_sessions[0].agent, "codex");
        assert_eq!(report.top_sessions[0].source_id, "local");
        assert_eq!(
            report.top_sessions[0].exists_state,
            SessionExistsState::ArchiveOnly
        );
        assert!(report.top_sessions[0].category_breadth >= 3);
        let remote = report
            .top_sessions
            .iter()
            .find(|session| session.conversation_id == 2)
            .expect("remote incident session");
        assert_eq!(remote.host, "ts1");
        assert_eq!(remote.exists_state, SessionExistsState::Unknown);
        assert_eq!(
            remote.suggested_command.argv[2],
            "/remote/private/remote.jsonl"
        );
        assert_eq!(remote.suggested_command.argv[4], "remote-ts1");

        let json = serde_json::to_string(&report).unwrap();
        assert!(
            !json.contains("secret_token_123456789"),
            "raw secret leaked: {json}"
        );
        for evidence in &report.top_sessions[0].evidence_summaries {
            assert!(
                evidence
                    .evidence_paths
                    .iter()
                    .all(|path| !path.contains('/')),
                "evidence paths must be basename-redacted: {evidence:?}"
            );
        }
        assert_eq!(
            report.redaction.private_text_policy,
            crate::search::incident_redaction::PrivateTextPolicy::SuppressAll
        );
        assert!(report.redaction.opt_in_flags.is_empty());
    }

    #[test]
    fn live_scanner_applies_source_and_agent_filters() {
        let filter = AnalyticsFilter {
            agents: vec!["claude_code".to_string()],
            source: SourceFilter::Remote,
            ..AnalyticsFilter::default()
        };
        let report = scan_incidents(&incident_db(), &filter, scan_caps(), 10, None).unwrap();
        assert_eq!(report.discovery.files_scanned, 1);
        assert_eq!(report.total_sessions, 1);
        assert_eq!(report.top_sessions[0].conversation_id, 2);
        assert_eq!(report.top_sessions[0].agent, "claude_code");
    }

    #[test]
    fn live_scanner_uses_origin_host_for_blank_legacy_source_identity() {
        let conn = incident_db();
        conn.execute_batch(
            "INSERT INTO conversations(
                 id, agent_id, workspace_id, source_id, external_id,
                 source_path, started_at, origin_host
             ) VALUES (4, 2, 20, '   ', 'legacy-remote',
                 '/remote/private/legacy.jsonl', 400, 'legacy-host');
             INSERT INTO messages(id, conversation_id, idx, content) VALUES
                 (5, 4, 0, 'cass remote sync over ssh failed: permission denied');",
        )
        .unwrap();

        let remote_filter = AnalyticsFilter {
            source: SourceFilter::Remote,
            ..AnalyticsFilter::default()
        };
        let remote = scan_incidents(&conn, &remote_filter, scan_caps(), 10, None).unwrap();
        let legacy = remote
            .top_sessions
            .iter()
            .find(|session| session.conversation_id == 4)
            .expect("blank-source legacy remote must pass remote filter");
        assert_eq!(legacy.source_id, "legacy-host");
        assert_eq!(legacy.host, "legacy-host");
        assert_eq!(legacy.exists_state, SessionExistsState::Unknown);

        let specific_filter = AnalyticsFilter {
            source: SourceFilter::Specific("legacy-host".to_string()),
            ..AnalyticsFilter::default()
        };
        let specific = scan_incidents(&conn, &specific_filter, scan_caps(), 10, None).unwrap();
        assert_eq!(specific.total_sessions, 1);
        assert_eq!(specific.top_sessions[0].conversation_id, 4);

        let local_filter = AnalyticsFilter {
            source: SourceFilter::Local,
            ..AnalyticsFilter::default()
        };
        let local = scan_incidents(&conn, &local_filter, scan_caps(), 10, None).unwrap();
        assert!(
            local
                .top_sessions
                .iter()
                .all(|session| session.conversation_id != 4)
        );
    }

    #[test]
    fn candidate_window_is_keyset_bounded_before_selective_filters() {
        let conn = incident_db();
        for id in 4..=515_i64 {
            conn.execute_compat(
                "INSERT INTO conversations(
                     id, agent_id, workspace_id, source_id, external_id,
                     source_path, started_at, origin_host
                 ) VALUES (?1, 1, 10, 'local', 'bulk', '/bulk.jsonl', 1, NULL)",
                frankensqlite::params![id],
            )
            .unwrap();
        }
        let filter = AnalyticsFilter {
            source: SourceFilter::Remote,
            ..AnalyticsFilter::default()
        };
        let mut caps = scan_caps();
        caps.max_files = 3;
        let report = scan_incidents(&conn, &filter, caps, 10, None).unwrap();
        assert_eq!(report.discovery.files_considered, 3);
        assert_eq!(report.discovery.files_scanned, 0);
        assert_eq!(report.discovery.stop_reason, StopReason::FilesCapped);
        assert_eq!(
            report.scan_units.candidate_order,
            "newest_archive_row_first"
        );
    }

    #[test]
    fn live_scanner_line_cap_returns_truthful_partial_scope() {
        let mut caps = scan_caps();
        caps.max_lines = 1;
        let report =
            scan_incidents(&incident_db(), &AnalyticsFilter::default(), caps, 10, None).unwrap();
        assert!(report.discovery.partial);
        assert_eq!(report.discovery.stop_reason, StopReason::LinesCapped);
        assert_eq!(report.discovery.lines_scanned, 1);
        assert_eq!(report.count_scope, "scanned_candidates_partial");
    }

    #[test]
    fn exact_line_cap_is_complete_when_sentinel_proves_corpus_exhausted() {
        let conn = incident_db();
        conn.execute_batch(
            "DELETE FROM messages WHERE id != 3;
             DELETE FROM conversations WHERE id != 2;",
        )
        .unwrap();
        let mut caps = scan_caps();
        caps.max_files = 1;
        caps.max_lines = 1;
        let report = scan_incidents(&conn, &AnalyticsFilter::default(), caps, 10, None).unwrap();
        assert_eq!(report.discovery.stop_reason, StopReason::Completed);
        assert!(!report.discovery.partial);
        assert_eq!(report.discovery.files_scanned, 1);
        assert_eq!(report.discovery.lines_scanned, 1);
    }

    #[test]
    fn live_scanner_byte_cap_never_overshoots() {
        let mut caps = scan_caps();
        caps.max_bytes = 12;
        let report =
            scan_incidents(&incident_db(), &AnalyticsFilter::default(), caps, 10, None).unwrap();
        assert!(report.discovery.partial);
        assert_eq!(report.discovery.stop_reason, StopReason::BytesCapped);
        assert_eq!(report.discovery.bytes_scanned, 12);
    }

    #[test]
    fn multibyte_scalar_larger_than_remaining_budget_is_bytes_capped() {
        let conn = incident_db();
        conn.execute_batch("UPDATE messages SET content = '😀' WHERE id = 3;")
            .unwrap();
        let mut caps = scan_caps();
        caps.max_bytes = 1;
        let report = scan_incidents(&conn, &AnalyticsFilter::default(), caps, 10, None).unwrap();
        assert!(report.discovery.partial);
        assert_eq!(report.discovery.stop_reason, StopReason::BytesCapped);
        assert!(report.discovery.bytes_scanned <= 1);
        assert_eq!(truncate_utf8_bytes("😀", 1), ("", true));
    }

    #[test]
    fn live_scanner_reports_partial_when_a_message_exceeds_fragment_bound() {
        let conn = incident_db();
        let oversized = format!("cass recommended_action unhealthy {}", "x".repeat(5_000));
        conn.execute_compat(
            "UPDATE messages SET content = ?1 WHERE id = 1",
            frankensqlite::params![oversized],
        )
        .unwrap();

        let report =
            scan_incidents(&conn, &AnalyticsFilter::default(), scan_caps(), 10, None).unwrap();
        assert!(report.discovery.partial);
        assert_eq!(report.count_scope, "scanned_candidates_partial");
        assert_eq!(
            report.discovery.stop_reason,
            StopReason::MessageFragmentCapped
        );
        assert_eq!(report.discovery.lines_scanned, 3);
        assert!(report.discovery.bytes_scanned <= 16_384);
    }

    #[test]
    fn live_scanner_empty_corpus_has_stable_complete_contract() {
        let conn = incident_db();
        conn.execute_batch("DELETE FROM messages; DELETE FROM conversations;")
            .unwrap();
        let report =
            scan_incidents(&conn, &AnalyticsFilter::default(), scan_caps(), 10, None).unwrap();
        assert_eq!(report.discovery.stop_reason, StopReason::Completed);
        assert_eq!(report.total_sessions, 0);
        assert_eq!(report.total_hits, 0);
        assert!(report.top_sessions.is_empty());
        let value = serde_json::to_value(&report).unwrap();
        assert_eq!(value["schema_version"], INCIDENT_MINING_SCHEMA_VERSION);
        assert_eq!(value["redaction"]["private_text_policy"], "suppress_all");
    }

    #[test]
    fn hard_timeout_report_claims_no_unreceived_observations() -> Result<()> {
        let caps = scan_caps();
        let report = hard_timeout_report(caps);
        require(
            report.count_scope == "no_verified_results_hard_timeout",
            "hard timeout count scope drifted",
        )?;
        require(
            report.discovery.stop_reason == StopReason::TimedOut,
            "hard timeout stop reason drifted",
        )?;
        require(report.discovery.timed_out, "timed_out must be true")?;
        require(report.discovery.partial, "partial must be true")?;
        require(
            report.discovery.elapsed_ms == caps.budget_ms,
            "elapsed time must equal the exhausted budget",
        )?;
        require(
            report.discovery.files_considered == 0,
            "unreceived rows must not be reported as considered",
        )?;
        require(
            report.discovery.files_scanned == 0,
            "unreceived rows must not be reported as scanned",
        )?;
        require(
            report.top_sessions.is_empty(),
            "unreceived sessions must not be reported",
        )
    }

    #[test]
    fn hard_timeout_worker_path_returns_before_slow_scan_finishes() -> Result<()> {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::{Duration, Instant};

        let mut caps = scan_caps();
        caps.budget_ms = 5;
        let worker_finished = Arc::new(AtomicBool::new(false));
        let worker_finished_in_scan = Arc::clone(&worker_finished);
        let started = Instant::now();
        let report = run_incident_scan_with_hard_timeout(caps, move || {
            std::thread::sleep(Duration::from_secs(1));
            worker_finished_in_scan.store(true, Ordering::Release);
            Ok(hard_timeout_report(caps))
        })?;

        require(
            report.count_scope == "no_verified_results_hard_timeout",
            "hard timeout count scope drifted",
        )?;
        require(
            report.discovery.stop_reason == StopReason::TimedOut,
            "hard timeout stop reason drifted",
        )?;
        require(
            started.elapsed() < Duration::from_millis(900),
            "hard guard waited for the one-second worker",
        )?;
        require(
            !worker_finished.load(Ordering::Acquire),
            "hard guard returned only after the slow worker completed",
        )
    }
}
