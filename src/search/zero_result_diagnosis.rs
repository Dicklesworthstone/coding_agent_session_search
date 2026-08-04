// Dead-code tolerated module-wide: the zero-result workspace diagnosis
// lands here ahead of the search pipeline that will populate
// `candidate_workspaces` from the live workspace list and project this into
// the search `--robot-meta` JSON. Downstream bead .7.4 (moved-workspace and
// stale-source fixture suite) consumes these types.
#![allow(dead_code)]

//! Zero-result workspace diagnosis for filtered searches (bead
//! cass-fleet-resilience-20260608-uojcg.7.1).
//!
//! The 2026-06-08 report found many workspace-mismatch / zero-hit cases
//! (especially on mac-mini-max after a moved checkout or a macOS-vs-Linux
//! path difference). A `--workspace`-filtered query that returns zero hits
//! looks identical to "this corpus genuinely has nothing" — so agents
//! overtrust a false-empty result instead of fixing the filter.
//!
//! This module diagnoses that situation from pure inputs: the requested
//! filter, the set of known workspace keys, and whether the *unfiltered*
//! query had hits. It produces a [`ZeroResultReport`] with a
//! `zero_result_diagnosis`, ranked `candidate_workspaces` (with the kind of
//! match and a per-candidate confidence), an overall `confidence`, and a
//! `suggested_rerun` when a canonical workspace is likely the right filter.
//!
//! It is decoupled from storage (the caller passes the known workspace
//! keys), so every case — exact miss, moved checkout, case/path
//! normalization, macOS/Linux path variants, source_id filters, and genuine
//! no-match — is unit-testable without a database. All enums serialize as
//! snake_case.

use serde::{Deserialize, Serialize};

/// The overall verdict for a zero-result filtered search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ZeroResultDiagnosis {
    /// The unfiltered query also returned nothing: the corpus genuinely has
    /// no match, the filter is not at fault.
    GenuineNoMatch,
    /// The filter doesn't match any known workspace but a near-miss
    /// canonical workspace exists; the filter is probably wrong.
    WorkspaceFilterLikelyWrong,
    /// The filter exactly matches a known workspace, yet that workspace has
    /// no hits while the global query does — a real per-workspace empty.
    WorkspaceHasNoMatch,
    /// The filter looks like a numeric source_id rather than a workspace
    /// path; workspace suggestions do not apply.
    SourceIdFilter,
    /// The filter matches no known workspace and nothing is close enough to
    /// suggest; it may name an un-indexed workspace.
    WorkspaceNotIndexed,
}

/// How a candidate workspace matched the requested filter, strongest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MatchKind {
    /// Byte-for-byte identical.
    Exact,
    /// Differs only by ASCII case (macOS case-insensitive paths).
    CaseInsensitive,
    /// Differs only by path normalization (trailing slash, `./`, `//`).
    PathNormalized,
    /// Differs by a known platform path convention (`/Users/` vs `/home/`).
    PlatformPathVariant,
    /// Same final path component (basename), different parent — a moved
    /// checkout.
    BasenameMoved,
}

impl MatchKind {
    /// The confidence a single match of this kind warrants.
    fn confidence(self) -> Confidence {
        match self {
            Self::Exact | Self::CaseInsensitive | Self::PathNormalized => Confidence::High,
            Self::PlatformPathVariant => Confidence::Medium,
            Self::BasenameMoved => Confidence::Low,
        }
    }
}

/// Confidence in a suggestion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Confidence {
    Low,
    Medium,
    High,
}

/// A suggested canonical workspace, with how it matched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WorkspaceCandidate {
    pub workspace: String,
    pub match_kind: MatchKind,
    pub confidence: Confidence,
}

/// The diagnosis a search surface attaches to a zero-result filtered query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ZeroResultReport {
    pub diagnosis: ZeroResultDiagnosis,
    /// Ranked canonical workspace suggestions (strongest first). Empty for
    /// genuine no-match, exact-but-empty, and source_id filters.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidate_workspaces: Vec<WorkspaceCandidate>,
    pub confidence: Confidence,
    /// A copy-pasteable rerun hint naming the canonical workspace, when one
    /// is confidently suggested. Never a bare `cass`/`bv`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested_rerun: Option<String>,
}

/// Lowercase + strip a single trailing separator and a leading `./`.
fn normalize_path(s: &str) -> String {
    let trimmed = s.trim();
    let no_lead = trimmed.strip_prefix("./").unwrap_or(trimmed);
    let collapsed = no_lead.replace("//", "/");
    collapsed
        .strip_suffix('/')
        .unwrap_or(&collapsed)
        .to_string()
}

fn normalize_case(s: &str) -> String {
    normalize_path(s).to_ascii_lowercase()
}

/// Swap the two common home-prefix conventions so a macOS `/Users/<u>/…`
/// path and a Linux `/home/<u>/…` path compare equal.
fn platform_variant(s: &str) -> Option<String> {
    let n = normalize_path(s);
    if let Some(rest) = n.strip_prefix("/Users/") {
        Some(format!("/home/{rest}"))
    } else {
        n.strip_prefix("/home/")
            .map(|rest| format!("/Users/{rest}"))
    }
}

fn basename(s: &str) -> &str {
    let n = s.trim().trim_end_matches('/');
    n.rsplit('/').next().unwrap_or(n)
}

/// Classify how `requested` matches `known`, if at all (strongest kind).
fn match_kind(requested: &str, known: &str) -> Option<MatchKind> {
    if requested == known {
        return Some(MatchKind::Exact);
    }
    if normalize_path(requested) == normalize_path(known) {
        return Some(MatchKind::PathNormalized);
    }
    if normalize_case(requested) == normalize_case(known) {
        return Some(MatchKind::CaseInsensitive);
    }
    if let Some(variant) = platform_variant(requested)
        && normalize_case(&variant) == normalize_case(known)
    {
        return Some(MatchKind::PlatformPathVariant);
    }
    let rb = basename(requested);
    if !rb.is_empty() && normalize_case(rb) == normalize_case(basename(known)) {
        return Some(MatchKind::BasenameMoved);
    }
    None
}

/// Whether the filter is a bare numeric source_id rather than a workspace
/// path.
fn looks_like_source_id(requested: &str) -> bool {
    let t = requested.trim();
    !t.is_empty() && t.chars().all(|c| c.is_ascii_digit())
}

/// Diagnose a zero-result filtered search.
///
/// - `requested_workspace`: the `--workspace` (or filter) value the query
///   used.
/// - `known_workspaces`: the canonical workspace keys present in the index.
/// - `global_had_hits`: whether the same query *without* the filter matched.
pub(crate) fn diagnose_zero_result(
    requested_workspace: &str,
    known_workspaces: &[String],
    global_had_hits: bool,
) -> ZeroResultReport {
    // A bare numeric filter is a source_id, not a workspace path.
    if looks_like_source_id(requested_workspace) {
        return ZeroResultReport {
            diagnosis: ZeroResultDiagnosis::SourceIdFilter,
            candidate_workspaces: Vec::new(),
            confidence: Confidence::High,
            suggested_rerun: Some(
                "list sources with `cass sources list --json` and filter by an existing source_id"
                    .to_string(),
            ),
        };
    }

    // If the unfiltered query also found nothing, the filter is not at fault.
    if !global_had_hits {
        return ZeroResultReport {
            diagnosis: ZeroResultDiagnosis::GenuineNoMatch,
            candidate_workspaces: Vec::new(),
            confidence: Confidence::High,
            suggested_rerun: None,
        };
    }

    // Collect every near-miss candidate, strongest match kind per workspace.
    let mut candidates: Vec<WorkspaceCandidate> = known_workspaces
        .iter()
        .filter_map(|known| {
            match_kind(requested_workspace, known).map(|kind| WorkspaceCandidate {
                workspace: known.clone(),
                match_kind: kind,
                confidence: kind.confidence(),
            })
        })
        .collect();
    // Strongest match kind first; ties broken by workspace for determinism.
    candidates.sort_by(|a, b| {
        a.match_kind
            .cmp(&b.match_kind)
            .then_with(|| a.workspace.cmp(&b.workspace))
    });

    // An exact match that still came back empty is a real per-workspace
    // empty, not a wrong filter.
    if matches!(
        candidates.first().map(|c| c.match_kind),
        Some(MatchKind::Exact)
    ) {
        return ZeroResultReport {
            diagnosis: ZeroResultDiagnosis::WorkspaceHasNoMatch,
            candidate_workspaces: Vec::new(),
            confidence: Confidence::High,
            suggested_rerun: None,
        };
    }

    if candidates.is_empty() {
        return ZeroResultReport {
            diagnosis: ZeroResultDiagnosis::WorkspaceNotIndexed,
            candidate_workspaces: Vec::new(),
            confidence: Confidence::Low,
            suggested_rerun: None,
        };
    }

    let top = &candidates[0];
    let overall = top.confidence;
    let suggested_rerun = Some(format!(
        "re-run with the canonical workspace: --workspace \"{}\"",
        top.workspace
    ));
    ZeroResultReport {
        diagnosis: ZeroResultDiagnosis::WorkspaceFilterLikelyWrong,
        candidate_workspaces: candidates,
        confidence: overall,
        suggested_rerun,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn enums_serialize_as_snake_case() {
        assert_eq!(
            serde_json::to_string(&ZeroResultDiagnosis::WorkspaceFilterLikelyWrong).unwrap(),
            "\"workspace_filter_likely_wrong\""
        );
        assert_eq!(
            serde_json::to_string(&MatchKind::PlatformPathVariant).unwrap(),
            "\"platform_path_variant\""
        );
        assert_eq!(
            serde_json::to_string(&Confidence::High).unwrap(),
            "\"high\""
        );
    }

    #[test]
    fn genuine_no_match_when_global_query_also_empty() {
        let r = diagnose_zero_result("/home/u/proj", &ws(&["/home/u/other"]), false);
        assert_eq!(r.diagnosis, ZeroResultDiagnosis::GenuineNoMatch);
        assert!(r.candidate_workspaces.is_empty());
        assert!(r.suggested_rerun.is_none());
    }

    #[test]
    fn exact_workspace_with_no_hits_is_per_workspace_empty_not_wrong_filter() {
        let r = diagnose_zero_result(
            "/home/u/proj",
            &ws(&["/home/u/proj", "/home/u/other"]),
            true,
        );
        assert_eq!(r.diagnosis, ZeroResultDiagnosis::WorkspaceHasNoMatch);
        assert!(r.candidate_workspaces.is_empty());
    }

    #[test]
    fn case_insensitive_miss_suggests_canonical_with_high_confidence() {
        let r = diagnose_zero_result("/Home/U/Proj", &ws(&["/home/u/proj"]), true);
        assert_eq!(r.diagnosis, ZeroResultDiagnosis::WorkspaceFilterLikelyWrong);
        assert_eq!(r.candidate_workspaces.len(), 1);
        assert_eq!(
            r.candidate_workspaces[0].match_kind,
            MatchKind::CaseInsensitive
        );
        assert_eq!(r.confidence, Confidence::High);
        assert!(r.suggested_rerun.unwrap().contains("/home/u/proj"));
    }

    #[test]
    fn trailing_slash_is_path_normalized_match() {
        let r = diagnose_zero_result("/home/u/proj/", &ws(&["/home/u/proj"]), true);
        assert_eq!(r.diagnosis, ZeroResultDiagnosis::WorkspaceFilterLikelyWrong);
        assert_eq!(
            r.candidate_workspaces[0].match_kind,
            MatchKind::PathNormalized
        );
        assert_eq!(r.confidence, Confidence::High);
    }

    #[test]
    fn macos_linux_path_variant_is_medium_confidence() {
        // requested a macOS-style path; the index holds the Linux form.
        let r = diagnose_zero_result("/Users/jeff/cass", &ws(&["/home/jeff/cass"]), true);
        assert_eq!(r.diagnosis, ZeroResultDiagnosis::WorkspaceFilterLikelyWrong);
        assert_eq!(
            r.candidate_workspaces[0].match_kind,
            MatchKind::PlatformPathVariant
        );
        assert_eq!(r.confidence, Confidence::Medium);
    }

    #[test]
    fn moved_checkout_matches_on_basename_with_low_confidence() {
        let r = diagnose_zero_result(
            "/old/location/myproj",
            &ws(&["/new/home/myproj", "/unrelated/thing"]),
            true,
        );
        assert_eq!(r.diagnosis, ZeroResultDiagnosis::WorkspaceFilterLikelyWrong);
        assert_eq!(r.candidate_workspaces[0].workspace, "/new/home/myproj");
        assert_eq!(
            r.candidate_workspaces[0].match_kind,
            MatchKind::BasenameMoved
        );
        assert_eq!(r.confidence, Confidence::Low);
    }

    #[test]
    fn source_id_filter_is_recognized_and_not_workspace_suggested() {
        let r = diagnose_zero_result("42", &ws(&["/home/u/proj"]), true);
        assert_eq!(r.diagnosis, ZeroResultDiagnosis::SourceIdFilter);
        assert!(r.candidate_workspaces.is_empty());
        assert!(r.suggested_rerun.unwrap().contains("sources list"));
    }

    #[test]
    fn no_close_workspace_is_not_indexed() {
        let r = diagnose_zero_result(
            "/home/u/ghost",
            &ws(&["/home/u/real", "/var/data/thing"]),
            true,
        );
        assert_eq!(r.diagnosis, ZeroResultDiagnosis::WorkspaceNotIndexed);
        assert!(r.candidate_workspaces.is_empty());
        assert_eq!(r.confidence, Confidence::Low);
    }

    #[test]
    fn candidates_are_ranked_strongest_match_first() {
        // A case-insensitive match and a basename match both exist; the
        // stronger (case-insensitive) must rank first.
        let r = diagnose_zero_result(
            "/HOME/U/PROJ",
            &ws(&["/elsewhere/proj", "/home/u/proj"]),
            true,
        );
        assert_eq!(
            r.candidate_workspaces[0].match_kind,
            MatchKind::CaseInsensitive
        );
        assert_eq!(r.candidate_workspaces[0].workspace, "/home/u/proj");
        assert_eq!(
            r.candidate_workspaces[1].match_kind,
            MatchKind::BasenameMoved
        );
    }

    #[test]
    fn report_round_trips_through_json() {
        let r = diagnose_zero_result("/Users/jeff/cass", &ws(&["/home/jeff/cass"]), true);
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"diagnosis\":\"workspace_filter_likely_wrong\""));
        assert!(json.contains("\"match_kind\":\"platform_path_variant\""));
        let parsed: ZeroResultReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, r);
    }
}
