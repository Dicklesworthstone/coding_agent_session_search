//! Per-host source-root archive coverage and sync-state summary.
//!
//! Bead: coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.6.4
//! ("Summarize source roots archive coverage and sync state per host").
//!
//! Pure logic over per-root observations (which the bounded probes of `6.2`
//! populate) that lets a fleet doctor — and an agent reading its JSON — decide
//! whether a host is fresh, stale, missing derived assets, source-pruned, has a
//! local archive ahead of its remote mirror, has a remote copy ahead of local,
//! or is at high archive risk. It composes with the fleet-doctor schema (`6.1`)
//! by reusing [`ArchiveRisk`] and [`RemoteSyncState`].
//!
//! Boundedness: root inventories in the field held thousands of
//! Claude/Codex/Gemini files plus CASS/dependency workspaces, so session/byte
//! counts are explicitly *estimates*. When a probe could only sample a capped
//! prefix, [`SourceRootStat::approximate`] is set and it propagates to the
//! summary; consumers must treat counts as lower-bound approximations, never
//! exact.

use crate::fleet_doctor_schema::{ArchiveRisk, RemoteSyncState};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Stable schema version for the archive-coverage wire format.
pub const ARCHIVE_COVERAGE_SCHEMA_VERSION: u32 = 1;

/// Default staleness threshold: derived state older than this (vs. the probe
/// time) is considered stale. 7 days in milliseconds.
pub const DEFAULT_STALE_AFTER_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// What kind of source root this is — used to separate real agent histories
/// from noisy dependency/workspace roots that should not inflate coverage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RootKind {
    Claude,
    Codex,
    Gemini,
    /// Antigravity (agy) — successor to the Gemini CLI; shares the ~/.gemini
    /// parent but is its own session-bearing agent.
    Antigravity,
    /// A CASS data/workspace directory (the controller's own state).
    CassWorkspace,
    /// A source/dependency code workspace that is noise for session coverage.
    DependencyWorkspace,
    /// Could not be classified.
    Other,
}

impl RootKind {
    /// Stable kebab wire value (drift-guarded by a unit test against serde).
    pub const fn as_str(self) -> &'static str {
        match self {
            RootKind::Claude => "claude",
            RootKind::Codex => "codex",
            RootKind::Gemini => "gemini",
            RootKind::Antigravity => "antigravity",
            RootKind::CassWorkspace => "cass-workspace",
            RootKind::DependencyWorkspace => "dependency-workspace",
            RootKind::Other => "other",
        }
    }

    /// `true` for roots that carry real agent session history and should count
    /// toward coverage (Claude/Codex/Gemini).
    pub const fn is_session_bearing(self) -> bool {
        matches!(
            self,
            RootKind::Claude | RootKind::Codex | RootKind::Gemini | RootKind::Antigravity
        )
    }
}

/// Best-effort classification of a root from its path and any detected agent.
/// Conservative: an unrecognized path is [`RootKind::Other`], not a guess.
pub fn classify_root_kind(path: &str, agent: Option<&str>) -> RootKind {
    if let Some(agent) = agent {
        match agent.to_ascii_lowercase().as_str() {
            "claude" => return RootKind::Claude,
            "codex" => return RootKind::Codex,
            "antigravity" => return RootKind::Antigravity,
            "gemini" => return RootKind::Gemini,
            _ => {}
        }
    }
    let lower = path.to_ascii_lowercase();
    if lower.contains(".claude") || lower.contains("/claude") {
        RootKind::Claude
    } else if lower.contains(".codex") || lower.contains("/codex") {
        RootKind::Codex
    } else if lower.contains("antigravity-cli") || lower.contains("antigravity") {
        // Must precede the .gemini check: agy lives under ~/.gemini/antigravity-cli.
        RootKind::Antigravity
    } else if lower.contains(".gemini") || lower.contains("/gemini") {
        RootKind::Gemini
    } else if lower.contains(".cass") || lower.contains("coding_agent_session_search") {
        RootKind::CassWorkspace
    } else if lower.contains("/data/projects")
        || lower.contains("node_modules")
        || lower.contains("target")
    {
        RootKind::DependencyWorkspace
    } else {
        RootKind::Other
    }
}

/// A single per-root observation produced by a bounded probe. Timestamps are
/// epoch-millis; counts are estimates (see [`Self::approximate`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRootStat {
    /// Root path on the host.
    pub path: String,
    /// Classified kind.
    pub kind: RootKind,
    /// Whether the path exists.
    pub exists: bool,
    /// Whether the probe could read the root (permissions / IO).
    pub readable: bool,
    /// Whether this root is backed by a CASS archive (vs. live-only source).
    pub archived: bool,
    /// Estimated session count discovered under the root.
    pub estimated_sessions: u64,
    /// Estimated on-disk size in bytes.
    pub estimated_bytes: u64,
    /// `true` when counts were sampled/capped rather than fully enumerated.
    pub approximate: bool,
    /// Live (currently-present) session count, when known. Less than
    /// `estimated_sessions` implies the live source was pruned but the archive
    /// retains history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_sessions: Option<u64>,
    /// Last time CASS indexed/derived from this root (epoch ms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_indexed_ms: Option<u64>,
    /// Last time this root was synced to/from its remote mirror (epoch ms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_synced_ms: Option<u64>,
}

/// Coverage / freshness state for a host's source roots, in priority order of
/// operator concern.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CoverageState {
    /// Indexed recently and consistent with the remote mirror.
    Fresh,
    /// Sessions exist but were never indexed / have no derived assets.
    MissingDerivedAssets,
    /// Live source was pruned; only the archive still holds history.
    SourcePruned,
    /// Local derived/archive state is newer than the remote mirror.
    LocalArchiveAhead,
    /// The remote mirror holds newer data than has been indexed locally.
    RemoteCopyAhead,
    /// Derived state is older than the staleness threshold.
    Stale,
    /// No readable session-bearing roots; nothing can be said.
    Unknown,
}

/// A specific provenance/coverage gap worth surfacing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProvenanceGapKind {
    /// A configured root does not exist.
    MissingRoot,
    /// A root exists but could not be read.
    UnreadableRoot,
    /// A root could not be classified.
    UnknownKind,
    /// A session-bearing root has no derived assets.
    NoDerivedAssets,
    /// A root was never synced to its remote mirror.
    NeverSynced,
}

/// A provenance gap tied to a specific root path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvenanceGap {
    pub kind: ProvenanceGapKind,
    pub path: String,
}

/// The per-host archive-coverage summary. Serializes with stable snake_case
/// fields and an embedded [`schema_version`](Self::schema_version).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveCoverageSummary {
    /// Mirrors [`ARCHIVE_COVERAGE_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Count of roots by kind (stable kebab keys).
    pub root_kind_counts: BTreeMap<String, u64>,
    /// Sum of estimated sessions across session-bearing roots.
    pub total_estimated_sessions: u64,
    /// Sum of estimated bytes across all roots.
    pub total_estimated_bytes: u64,
    /// `true` if ANY contributing root was approximate, so totals are
    /// lower-bound estimates.
    pub approximate: bool,
    /// Newest `last_indexed_ms` across roots, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub newest_index_ms: Option<u64>,
    /// Newest `last_synced_ms` across roots, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub newest_sync_ms: Option<u64>,
    /// Provenance/coverage gaps found.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provenance_gaps: Vec<ProvenanceGap>,
    /// Overall coverage state.
    pub coverage_state: CoverageState,
    /// Archive-risk implication of the coverage state + gaps.
    pub archive_risk: ArchiveRisk,
}

/// Summarize a host's source roots into coverage + sync + archive-risk facts.
///
/// `now_ms` is the probe time (epoch ms) against which staleness is judged;
/// passing it in keeps this pure and testable. `remote_sync` is the host-level
/// remote mirror state from the fleet-doctor report.
pub fn summarize_coverage(
    roots: &[SourceRootStat],
    remote_sync: RemoteSyncState,
    now_ms: u64,
    stale_after_ms: u64,
) -> ArchiveCoverageSummary {
    let mut root_kind_counts: BTreeMap<String, u64> = BTreeMap::new();
    let mut total_estimated_sessions = 0u64;
    let mut total_estimated_bytes = 0u64;
    let mut approximate = false;
    let mut newest_index_ms: Option<u64> = None;
    let mut newest_sync_ms: Option<u64> = None;
    let mut gaps: Vec<ProvenanceGap> = Vec::new();

    let mut any_readable_session_root = false;
    let mut any_missing_derived = false;
    let mut any_source_pruned = false;

    for root in roots {
        *root_kind_counts
            .entry(root.kind.as_str().to_string())
            .or_insert(0) += 1;
        total_estimated_bytes = total_estimated_bytes.saturating_add(root.estimated_bytes);
        approximate |= root.approximate;

        if !root.exists {
            gaps.push(ProvenanceGap {
                kind: ProvenanceGapKind::MissingRoot,
                path: root.path.clone(),
            });
            continue;
        }
        if !root.readable {
            gaps.push(ProvenanceGap {
                kind: ProvenanceGapKind::UnreadableRoot,
                path: root.path.clone(),
            });
            continue;
        }
        if root.kind == RootKind::Other {
            gaps.push(ProvenanceGap {
                kind: ProvenanceGapKind::UnknownKind,
                path: root.path.clone(),
            });
        }

        if let Some(ts) = root.last_indexed_ms {
            newest_index_ms = Some(newest_index_ms.map_or(ts, |cur| cur.max(ts)));
        }
        if let Some(ts) = root.last_synced_ms {
            newest_sync_ms = Some(newest_sync_ms.map_or(ts, |cur| cur.max(ts)));
        }

        // Only session-bearing roots contribute to coverage signals.
        if root.kind.is_session_bearing() {
            any_readable_session_root = true;
            total_estimated_sessions =
                total_estimated_sessions.saturating_add(root.estimated_sessions);

            if root.estimated_sessions > 0 && root.last_indexed_ms.is_none() {
                any_missing_derived = true;
                gaps.push(ProvenanceGap {
                    kind: ProvenanceGapKind::NoDerivedAssets,
                    path: root.path.clone(),
                });
            }
            if root.archived
                && let Some(live) = root.live_sessions
                && live < root.estimated_sessions
            {
                any_source_pruned = true;
            }
            if root.last_synced_ms.is_none() && remote_sync != RemoteSyncState::NotConfigured {
                gaps.push(ProvenanceGap {
                    kind: ProvenanceGapKind::NeverSynced,
                    path: root.path.clone(),
                });
            }
        }
    }

    let coverage_state = derive_coverage_state(
        any_readable_session_root,
        any_missing_derived,
        any_source_pruned,
        newest_index_ms,
        newest_sync_ms,
        remote_sync,
        now_ms,
        stale_after_ms,
    );
    let archive_risk = derive_archive_risk(coverage_state, &gaps, remote_sync);

    ArchiveCoverageSummary {
        schema_version: ARCHIVE_COVERAGE_SCHEMA_VERSION,
        root_kind_counts,
        total_estimated_sessions,
        total_estimated_bytes,
        approximate,
        newest_index_ms,
        newest_sync_ms,
        provenance_gaps: gaps,
        coverage_state,
        archive_risk,
    }
}

#[allow(clippy::too_many_arguments)]
fn derive_coverage_state(
    any_readable_session_root: bool,
    any_missing_derived: bool,
    any_source_pruned: bool,
    newest_index_ms: Option<u64>,
    newest_sync_ms: Option<u64>,
    remote_sync: RemoteSyncState,
    now_ms: u64,
    stale_after_ms: u64,
) -> CoverageState {
    if !any_readable_session_root {
        return CoverageState::Unknown;
    }
    // Highest operator concern first.
    if any_missing_derived {
        return CoverageState::MissingDerivedAssets;
    }
    if any_source_pruned {
        return CoverageState::SourcePruned;
    }
    // Stale derived state.
    if let Some(idx) = newest_index_ms
        && now_ms.saturating_sub(idx) > stale_after_ms
    {
        return CoverageState::Stale;
    }
    // Local vs remote ordering. Treat a configured-but-stale remote, or a local
    // index newer than the last sync, as "local ahead". A remote/sync newer than
    // the local index means the mirror has data not yet indexed here.
    match (newest_index_ms, newest_sync_ms) {
        (Some(idx), Some(sync)) if idx > sync => CoverageState::LocalArchiveAhead,
        (Some(idx), Some(sync)) if sync > idx => CoverageState::RemoteCopyAhead,
        _ => {
            if remote_sync == RemoteSyncState::Stale {
                CoverageState::LocalArchiveAhead
            } else {
                CoverageState::Fresh
            }
        }
    }
}

fn derive_archive_risk(
    state: CoverageState,
    gaps: &[ProvenanceGap],
    remote_sync: RemoteSyncState,
) -> ArchiveRisk {
    let has_unreadable = gaps
        .iter()
        .any(|g| g.kind == ProvenanceGapKind::UnreadableRoot);
    let never_synced = gaps
        .iter()
        .any(|g| g.kind == ProvenanceGapKind::NeverSynced)
        || remote_sync == RemoteSyncState::NeverSynced
        || remote_sync == RemoteSyncState::Failed;

    match state {
        // Source pruned with no safe remote copy = history lives in one place.
        CoverageState::SourcePruned if never_synced => ArchiveRisk::High,
        CoverageState::SourcePruned => ArchiveRisk::Medium,
        // Missing derived assets means a re-index is needed; high if also
        // unreadable roots block recovery.
        CoverageState::MissingDerivedAssets if has_unreadable => ArchiveRisk::High,
        CoverageState::MissingDerivedAssets => ArchiveRisk::Medium,
        // Local-only newer data that was never synced is at risk of loss.
        CoverageState::LocalArchiveAhead if never_synced => ArchiveRisk::High,
        CoverageState::LocalArchiveAhead
        | CoverageState::Stale
        | CoverageState::RemoteCopyAhead => ArchiveRisk::Medium,
        CoverageState::Unknown => {
            if has_unreadable {
                ArchiveRisk::High
            } else {
                ArchiveRisk::Unknown
            }
        }
        CoverageState::Fresh => {
            if has_unreadable {
                ArchiveRisk::Medium
            } else {
                ArchiveRisk::Low
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_000_000_000_000;
    const DAY: u64 = 24 * 60 * 60 * 1000;

    fn root(kind: RootKind, path: &str) -> SourceRootStat {
        SourceRootStat {
            path: path.to_string(),
            kind,
            exists: true,
            readable: true,
            archived: true,
            estimated_sessions: 100,
            estimated_bytes: 1_000_000,
            approximate: false,
            live_sessions: Some(100),
            last_indexed_ms: Some(NOW - DAY),
            last_synced_ms: Some(NOW - DAY),
        }
    }

    #[test]
    fn classify_uses_agent_then_path() {
        assert_eq!(
            classify_root_kind("/whatever", Some("Claude")),
            RootKind::Claude
        );
        assert_eq!(
            classify_root_kind("/home/u/.codex/sessions", None),
            RootKind::Codex
        );
        assert_eq!(
            classify_root_kind("/home/u/.gemini", None),
            RootKind::Gemini
        );
        assert_eq!(
            classify_root_kind("/home/u/.gemini/antigravity-cli", None),
            RootKind::Antigravity
        );
        assert_eq!(
            classify_root_kind("/x", Some("antigravity")),
            RootKind::Antigravity
        );
        assert_eq!(
            classify_root_kind("/home/u/.cass", None),
            RootKind::CassWorkspace
        );
        assert_eq!(
            classify_root_kind("/data/projects/frankensqlite", None),
            RootKind::DependencyWorkspace
        );
        assert_eq!(classify_root_kind("/opt/random", None), RootKind::Other);
    }

    #[test]
    fn fresh_synced_host_is_low_risk() {
        let roots = vec![root(RootKind::Claude, "/home/u/.claude")];
        let s = summarize_coverage(&roots, RemoteSyncState::Synced, NOW, DEFAULT_STALE_AFTER_MS);
        assert_eq!(s.coverage_state, CoverageState::Fresh);
        assert_eq!(s.archive_risk, ArchiveRisk::Low);
        assert_eq!(s.total_estimated_sessions, 100);
        assert!(!s.approximate);
        assert!(s.provenance_gaps.is_empty());
    }

    #[test]
    fn huge_root_marks_summary_approximate_and_bounds_counts() {
        let mut r = root(RootKind::Claude, "/home/u/.claude");
        r.estimated_sessions = 50_000;
        r.estimated_bytes = 9_000_000_000;
        r.live_sessions = Some(50_000);
        r.approximate = true;
        let s = summarize_coverage(&[r], RemoteSyncState::Synced, NOW, DEFAULT_STALE_AFTER_MS);
        assert!(
            s.approximate,
            "approximate must propagate from a sampled huge root"
        );
        assert_eq!(s.total_estimated_sessions, 50_000);
    }

    #[test]
    fn noisy_dependency_root_does_not_count_as_sessions() {
        let roots = vec![root(
            RootKind::DependencyWorkspace,
            "/data/projects/frankensqlite",
        )];
        let s = summarize_coverage(
            &roots,
            RemoteSyncState::NotConfigured,
            NOW,
            DEFAULT_STALE_AFTER_MS,
        );
        assert_eq!(
            s.total_estimated_sessions, 0,
            "dependency roots are not session-bearing"
        );
        // No readable session-bearing root => Unknown coverage.
        assert_eq!(s.coverage_state, CoverageState::Unknown);
        assert_eq!(*s.root_kind_counts.get("dependency-workspace").unwrap(), 1);
    }

    #[test]
    fn missing_root_is_a_provenance_gap() {
        let mut r = root(RootKind::Claude, "/home/u/.claude");
        r.exists = false;
        let s = summarize_coverage(&[r], RemoteSyncState::Synced, NOW, DEFAULT_STALE_AFTER_MS);
        assert!(
            s.provenance_gaps
                .iter()
                .any(|g| g.kind == ProvenanceGapKind::MissingRoot)
        );
        // No readable session root remained.
        assert_eq!(s.coverage_state, CoverageState::Unknown);
    }

    #[test]
    fn unreadable_root_raises_risk() {
        let mut r = root(RootKind::Claude, "/home/u/.claude");
        r.readable = false;
        let s = summarize_coverage(&[r], RemoteSyncState::Synced, NOW, DEFAULT_STALE_AFTER_MS);
        assert!(
            s.provenance_gaps
                .iter()
                .any(|g| g.kind == ProvenanceGapKind::UnreadableRoot)
        );
        assert_eq!(s.coverage_state, CoverageState::Unknown);
        assert_eq!(
            s.archive_risk,
            ArchiveRisk::High,
            "unreadable root is high risk"
        );
    }

    #[test]
    fn never_indexed_sessions_is_missing_derived_assets() {
        let mut r = root(RootKind::Claude, "/home/u/.claude");
        r.last_indexed_ms = None;
        let s = summarize_coverage(&[r], RemoteSyncState::Synced, NOW, DEFAULT_STALE_AFTER_MS);
        assert_eq!(s.coverage_state, CoverageState::MissingDerivedAssets);
        assert_eq!(s.archive_risk, ArchiveRisk::Medium);
        assert!(
            s.provenance_gaps
                .iter()
                .any(|g| g.kind == ProvenanceGapKind::NoDerivedAssets)
        );
    }

    #[test]
    fn pruned_live_source_with_no_remote_is_high_risk() {
        let mut r = root(RootKind::Claude, "/home/u/.claude");
        r.live_sessions = Some(10); // 10 live vs 100 archived => pruned
        let s = summarize_coverage(
            &[r],
            RemoteSyncState::NeverSynced,
            NOW,
            DEFAULT_STALE_AFTER_MS,
        );
        assert_eq!(s.coverage_state, CoverageState::SourcePruned);
        assert_eq!(s.archive_risk, ArchiveRisk::High);
    }

    #[test]
    fn stale_index_is_detected() {
        let mut r = root(RootKind::Claude, "/home/u/.claude");
        r.last_indexed_ms = Some(NOW - 30 * DAY);
        r.last_synced_ms = Some(NOW - 30 * DAY);
        let s = summarize_coverage(&[r], RemoteSyncState::Synced, NOW, DEFAULT_STALE_AFTER_MS);
        assert_eq!(s.coverage_state, CoverageState::Stale);
        assert_eq!(s.archive_risk, ArchiveRisk::Medium);
    }

    #[test]
    fn local_archive_ahead_of_remote() {
        let mut r = root(RootKind::Claude, "/home/u/.claude");
        r.last_indexed_ms = Some(NOW - DAY);
        r.last_synced_ms = Some(NOW - 3 * DAY); // synced older than indexed
        let s = summarize_coverage(&[r], RemoteSyncState::Stale, NOW, DEFAULT_STALE_AFTER_MS);
        assert_eq!(s.coverage_state, CoverageState::LocalArchiveAhead);
    }

    #[test]
    fn remote_copy_ahead_of_local() {
        let mut r = root(RootKind::Claude, "/home/u/.claude");
        r.last_indexed_ms = Some(NOW - 3 * DAY);
        r.last_synced_ms = Some(NOW - DAY); // synced newer than indexed
        let s = summarize_coverage(&[r], RemoteSyncState::Synced, NOW, DEFAULT_STALE_AFTER_MS);
        assert_eq!(s.coverage_state, CoverageState::RemoteCopyAhead);
        assert_eq!(s.archive_risk, ArchiveRisk::Medium);
    }

    #[test]
    fn root_kind_wire_values_match_as_str() {
        for kind in [
            RootKind::Claude,
            RootKind::Codex,
            RootKind::Gemini,
            RootKind::Antigravity,
            RootKind::CassWorkspace,
            RootKind::DependencyWorkspace,
            RootKind::Other,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(json, format!("\"{}\"", kind.as_str()));
            assert_eq!(serde_json::from_str::<RootKind>(&json).unwrap(), kind);
        }
    }

    #[test]
    fn summary_serializes_with_stable_fields_and_round_trips() {
        let roots = vec![
            root(RootKind::Claude, "/home/u/.claude"),
            root(RootKind::Codex, "/home/u/.codex"),
        ];
        let s = summarize_coverage(&roots, RemoteSyncState::Synced, NOW, DEFAULT_STALE_AFTER_MS);
        let value = serde_json::to_value(&s).unwrap();
        assert_eq!(value["schema_version"], ARCHIVE_COVERAGE_SCHEMA_VERSION);
        assert_eq!(value["coverage_state"], "fresh");
        assert_eq!(value["archive_risk"], "low");
        assert_eq!(value["total_estimated_sessions"], 200);
        assert_eq!(value["root_kind_counts"]["claude"], 1);
        assert_eq!(value["root_kind_counts"]["codex"], 1);
        let back: ArchiveCoverageSummary = serde_json::from_value(value).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn missing_derived_takes_priority_over_pruned_and_stale() {
        // A root that is simultaneously never-indexed, pruned, and old must
        // report the most actionable state (missing derived assets).
        let mut r = root(RootKind::Claude, "/home/u/.claude");
        r.last_indexed_ms = None;
        r.last_synced_ms = Some(NOW - 30 * DAY);
        r.live_sessions = Some(1);
        let s = summarize_coverage(&[r], RemoteSyncState::Stale, NOW, DEFAULT_STALE_AFTER_MS);
        assert_eq!(s.coverage_state, CoverageState::MissingDerivedAssets);
    }
}
