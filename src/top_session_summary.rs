//! Bounded top-session summaries for recurrent problem clusters.
//!
//! Bead: coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.10.3
//! ("Preserve top-session pointers and category breadth summaries").
//!
//! Incident mining over a large corpus can surface tens of thousands of raw
//! matches across many categories. Dumping those is useless and unsafe. This
//! module is the pure **summarizer**: it collapses a stream of categorized hits
//! into a bounded, ranked list of the top sessions/files carrying the most (and
//! the broadest) problem clusters, each with category breadth, dominant
//! categories, composite host/path/source identity, archive-only state,
//! redaction status, bounded evidence summaries, and a single safe `cass view`
//! pointer carrying the effective database and canonical archive row id —
//! instead of the raw matches.
//!
//! Pure and offline: the caller (incident mining, bead `10.2`) feeds already
//! bounded, redacted hits; this module only aggregates and ranks. The suggested
//! command is always a safe, read-only `--json` pointer.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::search::incident_categories::IncidentCategory;

/// Stable schema version for the top-session summary wire format.
pub const TOP_SESSION_SCHEMA_VERSION: u32 = 2;

/// Whether the underlying session file still exists, or is known only from the
/// archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionExistsState {
    /// The source file exists on disk.
    Exists,
    /// Known only from the archive; the live source is gone/pruned.
    ArchiveOnly,
    /// Existence could not be determined.
    Unknown,
}

impl SessionExistsState {
    /// Stable snake_case wire label.
    pub fn as_str(self) -> &'static str {
        match self {
            SessionExistsState::Exists => "exists",
            SessionExistsState::ArchiveOnly => "archive_only",
            SessionExistsState::Unknown => "unknown",
        }
    }
}

/// Redaction status of the evidence summary carried for a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RedactionStatus {
    /// Evidence was redacted before inclusion.
    Redacted,
    /// No sensitive content; nothing to redact.
    NotApplicable,
}

impl RedactionStatus {
    /// Stable snake_case wire label.
    pub fn as_str(self) -> &'static str {
        match self {
            RedactionStatus::Redacted => "redacted",
            RedactionStatus::NotApplicable => "not_applicable",
        }
    }
}

/// A single categorized and redacted incident hit, as fed by the mining layer.
/// `category` is a stable canonical category id. Raw evidence is deliberately
/// absent: only a content fingerprint and basename-redacted evidence path may
/// cross this boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IncidentHit {
    /// Canonical archive row id. This is the aggregation identity; display
    /// metadata may drift, but one archive row must never split into two entries.
    pub conversation_id: i64,
    /// Session identifier the hit belongs to.
    pub session_id: String,
    /// Agent/connector slug.
    pub agent: String,
    /// Host the session came from.
    pub host: String,
    /// Canonical archived source path used by `cass view`.
    pub source_path: String,
    /// Canonical source id used to disambiguate local and mirrored sessions.
    pub source_id: String,
    /// Original host metadata when present.
    pub origin_host: Option<String>,
    /// Existence/archive state when known.
    pub exists_state: SessionExistsState,
    /// Canonical problem category.
    pub category: IncidentCategory,
    /// Whether this hit's evidence is redacted.
    pub redacted: bool,
    /// Stable redacted-content fingerprint for bounded correlation.
    pub content_fingerprint: Option<String>,
    /// Basename-redacted evidence path, never a private full path.
    pub evidence_path: Option<String>,
}

/// Bounded redacted evidence rollup for one category within a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CategoryEvidenceSummary {
    /// Stable canonical incident category id.
    pub category: String,
    /// Hits attributed to this category in the session.
    pub hit_count: usize,
    /// Distinct content fingerprints, sorted and bounded.
    pub content_fingerprints: Vec<String>,
    /// Basename-redacted evidence paths, sorted and bounded.
    pub evidence_paths: Vec<String>,
}

/// Safe follow-up command. `argv` is the executable contract; `display` is a
/// shell-escaped convenience string and must never be reparsed to recover argv.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuggestedCommand {
    pub kind: String,
    pub argv: Vec<String>,
    pub display: String,
}

/// One ranked top-session entry in the summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopSessionEntry {
    /// Canonical archive row id used as the aggregation identity.
    pub conversation_id: i64,
    /// Session identifier.
    pub session_id: String,
    /// Agent/connector slug.
    pub agent: String,
    /// Source host.
    pub host: String,
    /// Canonical archived source path.
    pub source_path: String,
    /// Canonical source id.
    pub source_id: String,
    /// Original host metadata when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_host: Option<String>,
    /// Existence/archive state.
    pub exists_state: SessionExistsState,
    /// Total hits in this session.
    pub hit_count: usize,
    /// Number of distinct categories (breadth).
    pub category_breadth: usize,
    /// Dominant categories, most-frequent first (ties broken by label), capped.
    pub dominant_categories: Vec<String>,
    /// Redaction status of the carried evidence.
    pub redaction_status: RedactionStatus,
    /// Bounded evidence summaries for the dominant categories. No raw content.
    pub evidence_summaries: Vec<CategoryEvidenceSummary>,
    /// Safe, read-only pointer command for an operator/agent.
    pub suggested_command: SuggestedCommand,
}

/// The bounded top-session summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopSessionSummary {
    /// Mirrors [`TOP_SESSION_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Ranked top sessions (bounded by the cap).
    pub top_sessions: Vec<TopSessionEntry>,
    /// Distinct sessions observed (pre-cap).
    pub total_sessions: usize,
    /// Total hits observed.
    pub total_hits: usize,
    /// True when more sessions existed than the cap returned.
    pub truncated: bool,
}

/// How many dominant categories to list per session.
const DOMINANT_CATEGORY_CAP: usize = 5;
/// Per-category cap for distinct evidence fingerprints and basename paths.
const EVIDENCE_VALUE_CAP: usize = 3;

struct SessionAccum {
    conversation_id: i64,
    session_id: String,
    agent: String,
    host: String,
    source_id: String,
    source_path: String,
    origin_host: Option<String>,
    exists_state: SessionExistsState,
    hit_count: usize,
    any_redacted: bool,
    categories: BTreeMap<String, CategoryAccum>,
}

#[derive(Default)]
struct CategoryAccum {
    hit_count: usize,
    content_fingerprints: std::collections::BTreeSet<String>,
    evidence_paths: std::collections::BTreeSet<String>,
}

/// Online accumulator used by the bounded scanner. It stores one compact
/// aggregate per archive conversation/category, not one allocation per raw hit.
#[derive(Default)]
pub(crate) struct TopSessionAccumulator {
    sessions: BTreeMap<i64, SessionAccum>,
    total_hits: usize,
}

fn existence_rank(state: SessionExistsState) -> u8 {
    match state {
        SessionExistsState::Unknown => 0,
        SessionExistsState::ArchiveOnly => 1,
        SessionExistsState::Exists => 2,
    }
}

/// POSIX-shell quote an operator-controlled CLI value. Safe alphanumeric path
/// characters remain readable; everything else is single-quoted with embedded
/// quotes escaped using the standard `'<quote>'` sequence.
fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | ':' | '@'))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

impl TopSessionAccumulator {
    /// Record one already-redacted categorized hit.
    pub(crate) fn push(&mut self, hit: IncidentHit) {
        self.total_hits = self.total_hits.saturating_add(1);
        let entry = self
            .sessions
            .entry(hit.conversation_id)
            .or_insert_with(|| SessionAccum {
                conversation_id: hit.conversation_id,
                session_id: hit.session_id.clone(),
                agent: hit.agent.clone(),
                host: hit.host.clone(),
                source_id: hit.source_id.clone(),
                source_path: hit.source_path.clone(),
                origin_host: hit.origin_host.clone(),
                exists_state: hit.exists_state,
                hit_count: 0,
                any_redacted: false,
                categories: BTreeMap::new(),
            });
        entry.hit_count += 1;
        entry.any_redacted |= hit.redacted;
        if entry.origin_host.is_none() {
            entry.origin_host.clone_from(&hit.origin_host);
        }
        let category = entry
            .categories
            .entry(hit.category.id().to_string())
            .or_default();
        category.hit_count += 1;
        if category.content_fingerprints.len() < EVIDENCE_VALUE_CAP
            && let Some(fingerprint) = &hit.content_fingerprint
        {
            category
                .content_fingerprints
                .insert(fingerprint.to_string());
        }
        if category.evidence_paths.len() < EVIDENCE_VALUE_CAP
            && let Some(path) = &hit.evidence_path
        {
            category.evidence_paths.insert(path.to_string());
        }
        // Deterministic precedence: live evidence beats archive-only, which
        // beats unknown, regardless of hit order.
        if existence_rank(hit.exists_state) > existence_rank(entry.exists_state) {
            entry.exists_state = hit.exists_state;
        }
    }

    /// Produce the bounded ranked summary. `top_n` of zero still reports the
    /// totals and marks truncation when matching conversations exist.
    pub(crate) fn finish(self, top_n: usize, archive_db_path: Option<&Path>) -> TopSessionSummary {
        let total_sessions = self.sessions.len();

        let mut ranked: Vec<SessionAccum> = self.sessions.into_values().collect();
        // Rank: hit_count desc, breadth desc, then canonical archive row id.
        ranked.sort_by(|a, b| {
            b.hit_count
                .cmp(&a.hit_count)
                .then_with(|| b.categories.len().cmp(&a.categories.len()))
                .then_with(|| a.conversation_id.cmp(&b.conversation_id))
        });

        let truncated = top_n < ranked.len();
        let top_sessions: Vec<TopSessionEntry> = ranked
            .into_iter()
            .take(top_n)
            .map(|acc| {
                // Dominant categories: count desc, then label asc; capped.
                let mut cats: Vec<(String, usize)> = acc
                    .categories
                    .iter()
                    .map(|(k, v)| (k.clone(), v.hit_count))
                    .collect();
                cats.sort_by(|(a_k, a_v), (b_k, b_v)| b_v.cmp(a_v).then_with(|| a_k.cmp(b_k)));
                let dominant_categories: Vec<String> = cats
                    .into_iter()
                    .take(DOMINANT_CATEGORY_CAP)
                    .map(|(k, _)| k)
                    .collect();
                let evidence_summaries = dominant_categories
                    .iter()
                    .filter_map(|category| {
                        acc.categories
                            .get(category)
                            .map(|evidence| CategoryEvidenceSummary {
                                category: category.clone(),
                                hit_count: evidence.hit_count,
                                content_fingerprints: evidence
                                    .content_fingerprints
                                    .iter()
                                    .take(EVIDENCE_VALUE_CAP)
                                    .cloned()
                                    .collect(),
                                evidence_paths: evidence
                                    .evidence_paths
                                    .iter()
                                    .take(EVIDENCE_VALUE_CAP)
                                    .cloned()
                                    .collect(),
                            })
                    })
                    .collect();
                let redaction_status = if acc.any_redacted {
                    RedactionStatus::Redacted
                } else {
                    RedactionStatus::NotApplicable
                };
                let mut argv = vec!["cass".to_string()];
                if let Some(db_path) = archive_db_path {
                    argv.extend(["--db".to_string(), db_path.to_string_lossy().into_owned()]);
                }
                argv.extend([
                    "view".to_string(),
                    acc.source_path.clone(),
                    "--source".to_string(),
                    acc.source_id.clone(),
                    "--conversation-id".to_string(),
                    acc.conversation_id.to_string(),
                    "--json".to_string(),
                ]);
                let display = argv
                    .iter()
                    .map(|argument| shell_quote(argument))
                    .collect::<Vec<_>>()
                    .join(" ");
                let suggested_command = SuggestedCommand {
                    kind: "view".to_string(),
                    argv,
                    display,
                };
                TopSessionEntry {
                    conversation_id: acc.conversation_id,
                    session_id: acc.session_id,
                    agent: acc.agent,
                    host: acc.host,
                    source_path: acc.source_path,
                    source_id: acc.source_id,
                    origin_host: acc.origin_host,
                    exists_state: acc.exists_state,
                    hit_count: acc.hit_count,
                    category_breadth: acc.categories.len(),
                    dominant_categories,
                    redaction_status,
                    evidence_summaries,
                    suggested_command,
                }
            })
            .collect();

        TopSessionSummary {
            schema_version: TOP_SESSION_SCHEMA_VERSION,
            top_sessions,
            total_sessions,
            total_hits: self.total_hits,
            truncated,
        }
    }
}

/// Summarize categorized hits into the top `top_n` sessions by hit count (then
/// category breadth, then stable archive row id). Pure; no I/O.
#[cfg(test)]
pub(crate) fn summarize_top_sessions(hits: &[IncidentHit], top_n: usize) -> TopSessionSummary {
    let mut accumulator = TopSessionAccumulator::default();
    for hit in hits {
        accumulator.push(hit.clone());
    }
    accumulator.finish(top_n, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_conversation_id(session: &str) -> i64 {
        session
            .bytes()
            .fold(17_i64, |acc, byte| acc.saturating_mul(31) + i64::from(byte))
    }

    fn hit(session: &str, host: &str, category: &str) -> IncidentHit {
        IncidentHit {
            conversation_id: fixture_conversation_id(session),
            session_id: session.to_string(),
            agent: "codex".to_string(),
            host: host.to_string(),
            source_path: format!("/sessions/{host}/{session}.jsonl"),
            source_id: host.to_string(),
            origin_host: (host != "local").then(|| host.to_string()),
            exists_state: SessionExistsState::Exists,
            category: IncidentCategory::from_id(category).expect("canonical fixture category"),
            redacted: false,
            content_fingerprint: None,
            evidence_path: None,
        }
    }

    #[test]
    fn aggregates_hits_and_category_breadth_per_session() {
        let hits = vec![
            hit("s1", "local", "remote_sync_auth"),
            hit("s1", "local", "storage_busy_corrupt"),
            hit("s1", "local", "remote_sync_auth"),
            hit("s2", "ts1", "cass_status_health"),
        ];
        let summary = summarize_top_sessions(&hits, 10);
        assert_eq!(summary.total_sessions, 2);
        assert_eq!(summary.total_hits, 4);
        assert!(!summary.truncated);
        let s1 = &summary.top_sessions[0];
        assert_eq!(s1.session_id, "s1");
        assert_eq!(s1.hit_count, 3);
        assert_eq!(s1.category_breadth, 2);
        assert_eq!(s1.dominant_categories[0], "remote_sync_auth");
    }

    #[test]
    fn ranks_by_hit_count_then_breadth() {
        let mut hits = Vec::new();
        // busy: 5 hits, 1 category.
        for _ in 0..5 {
            hits.push(hit("busy", "local", "remote_sync_auth"));
        }
        // broad: 5 hits, 3 categories (more breadth, same hit count).
        hits.push(hit("broad", "ts1", "cass_status_health"));
        hits.push(hit("broad", "ts1", "index_stale_missing"));
        hits.push(hit("broad", "ts1", "search_zero_workspace"));
        hits.push(hit("broad", "ts1", "cass_status_health"));
        hits.push(hit("broad", "ts1", "index_stale_missing"));
        let summary = summarize_top_sessions(&hits, 10);
        // Equal hit_count (5), broader session ranks first.
        assert_eq!(summary.top_sessions[0].session_id, "broad");
        assert_eq!(summary.top_sessions[0].category_breadth, 3);
    }

    #[test]
    fn caps_to_top_n_and_marks_truncated() {
        let hits: Vec<IncidentHit> = (0..20)
            .flat_map(|i| {
                let n = 20 - i; // session i gets 20-i hits => i ordering by count
                (0..n).map(move |_| hit(&format!("s{i:02}"), "local", "storage_busy_corrupt"))
            })
            .collect();
        let summary = summarize_top_sessions(&hits, 3);
        assert_eq!(summary.top_sessions.len(), 3);
        assert_eq!(summary.total_sessions, 20);
        assert!(summary.truncated);
        // Top session has the most hits (s00 with 20).
        assert_eq!(summary.top_sessions[0].session_id, "s00");
    }

    #[test]
    fn archive_only_state_and_redaction_are_preserved() {
        let mut h1 = hit("gone", "mac-mini-max", "storage_busy_corrupt");
        h1.exists_state = SessionExistsState::ArchiveOnly;
        h1.redacted = true;
        let summary = summarize_top_sessions(&[h1], 10);
        let s = &summary.top_sessions[0];
        assert_eq!(s.exists_state, SessionExistsState::ArchiveOnly);
        assert_eq!(s.redaction_status, RedactionStatus::Redacted);
    }

    #[test]
    fn definite_exists_state_wins_over_unknown() {
        let mut unknown = hit("s", "local", "cass_status_health");
        unknown.exists_state = SessionExistsState::Unknown;
        let mut known = hit("s", "local", "index_stale_missing");
        known.exists_state = SessionExistsState::ArchiveOnly;
        let summary = summarize_top_sessions(&[unknown, known], 10);
        assert_eq!(
            summary.top_sessions[0].exists_state,
            SessionExistsState::ArchiveOnly
        );
    }

    #[test]
    fn live_exists_state_wins_over_archive_only_regardless_of_order() {
        let mut archived = hit("s", "local", "cass_status_health");
        archived.exists_state = SessionExistsState::ArchiveOnly;
        let mut live = hit("s", "local", "index_stale_missing");
        live.exists_state = SessionExistsState::Exists;

        for hits in [vec![archived.clone(), live.clone()], vec![live, archived]] {
            let summary = summarize_top_sessions(&hits, 10);
            assert_eq!(
                summary.top_sessions[0].exists_state,
                SessionExistsState::Exists
            );
        }
    }

    #[test]
    fn composite_provenance_keeps_same_session_id_separate() {
        let local = hit("shared", "local", "storage_busy_corrupt");
        let mut remote = hit("shared", "ts1", "remote_sync_auth");
        remote.conversation_id = remote.conversation_id.saturating_add(1);
        remote.source_id = "remote-ts1".to_string();
        let summary = summarize_top_sessions(&[local, remote], 10);
        assert_eq!(summary.total_sessions, 2);
        assert_eq!(summary.top_sessions[0].session_id, "shared");
        assert_ne!(
            summary.top_sessions[0].source_id,
            summary.top_sessions[1].source_id
        );
    }

    #[test]
    fn dominant_categories_are_capped_and_ordered() {
        let mut hits = Vec::new();
        for (cat, n) in [
            ("cass_status_health", 6),
            ("index_stale_missing", 5),
            ("index_stall_progress", 4),
            ("quarantine_oom", 3),
            ("remote_sync_auth", 2),
            ("semantic", 1),
        ] {
            for _ in 0..n {
                hits.push(hit("s", "local", cat));
            }
        }
        let summary = summarize_top_sessions(&hits, 10);
        let s = &summary.top_sessions[0];
        assert_eq!(s.category_breadth, 6);
        assert_eq!(s.dominant_categories.len(), DOMINANT_CATEGORY_CAP); // capped at 5
        assert_eq!(
            s.dominant_categories,
            vec![
                "cass_status_health",
                "index_stale_missing",
                "index_stall_progress",
                "quarantine_oom",
                "remote_sync_auth",
            ]
        );
    }

    #[test]
    fn suggested_command_is_safe_and_read_only() {
        let mut unsafe_values = hit("s1", "local", "other");
        unsafe_values.source_path = "/tmp/session with ' quote.jsonl".to_string();
        unsafe_values.source_id = "remote host".to_string();
        let summary = summarize_top_sessions(&[unsafe_values], 10);
        let cmd = summary.top_sessions[0]
            .suggested_command
            .display
            .to_ascii_lowercase();
        assert!(cmd.starts_with("cass view "));
        assert!(cmd.contains("--source 'remote host'"));
        assert!(
            cmd.contains("'\"'\"'"),
            "embedded quote must be escaped: {cmd}"
        );
        assert!(cmd.contains("--json"));
        for needle in ["--delete", "rm -rf", "prune", "index", "repair"] {
            assert!(!cmd.contains(needle), "unsafe suggested command: {cmd:?}");
        }
        assert_eq!(
            summary.top_sessions[0].suggested_command.argv,
            vec![
                "cass".to_string(),
                "view".to_string(),
                "/tmp/session with ' quote.jsonl".to_string(),
                "--source".to_string(),
                "remote host".to_string(),
                "--conversation-id".to_string(),
                fixture_conversation_id("s1").to_string(),
                "--json".to_string(),
            ]
        );
    }

    #[test]
    fn evidence_summaries_are_redacted_sorted_and_bounded() {
        let hits: Vec<IncidentHit> = (0..6)
            .map(|i| {
                let mut h = hit("s", "local", "storage_busy_corrupt");
                h.redacted = true;
                h.content_fingerprint = Some(format!("fp-{i}"));
                h.evidence_path = Some(format!("session-{i}.jsonl"));
                h
            })
            .collect();
        let summary = summarize_top_sessions(&hits, 10);
        let evidence = &summary.top_sessions[0].evidence_summaries[0];
        assert_eq!(evidence.hit_count, 6);
        assert_eq!(evidence.content_fingerprints.len(), EVIDENCE_VALUE_CAP);
        assert_eq!(evidence.evidence_paths.len(), EVIDENCE_VALUE_CAP);
        assert_eq!(
            summary.top_sessions[0].redaction_status,
            RedactionStatus::Redacted
        );
        let json = serde_json::to_string(evidence).unwrap();
        assert!(
            !json.contains("/sessions/"),
            "evidence must not expose full paths"
        );
    }

    #[test]
    fn empty_input_and_zero_cap_are_well_defined() {
        let empty = summarize_top_sessions(&[], 10);
        assert_eq!(empty.total_sessions, 0);
        assert!(empty.top_sessions.is_empty());
        assert!(!empty.truncated);

        let zero_cap = summarize_top_sessions(&[hit("s", "h", "search_zero_workspace")], 0);
        assert!(zero_cap.top_sessions.is_empty());
        assert_eq!(zero_cap.total_sessions, 1);
        assert!(zero_cap.truncated);
    }

    #[test]
    fn json_contract_is_stable_and_round_trips() {
        let summary = summarize_top_sessions(&[hit("s1", "local", "remote_sync_auth")], 10);
        let value = serde_json::to_value(&summary).unwrap();
        assert_eq!(value["schema_version"], TOP_SESSION_SCHEMA_VERSION);
        assert_eq!(value["top_sessions"][0]["exists_state"], "exists");
        assert_eq!(
            value["top_sessions"][0]["redaction_status"],
            "not_applicable"
        );
        let back: TopSessionSummary = serde_json::from_value(value).unwrap();
        assert_eq!(back, summary);
    }
}
