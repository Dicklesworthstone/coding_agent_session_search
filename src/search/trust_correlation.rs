//! Project-scoped, identifier-anchored trust correlation.
//!
//! Bead: coding_agent_session_search-q4pau (follow-on to
//! coding_agent_session_search-guided-ops-repro-trust-5u82n.3).
//!
//! ## Why
//!
//! [`crate::search::trust_scoring`] reduces metadata-only signals to a verdict,
//! but its strongest inputs — a linked commit, a closed bead, proof status, and
//! a containing release tag — defaulted to "no signal" live because nothing
//! populated them. The recency / workspace / source / mode portion of the
//! verdict worked, but the headline "proven, landed, released vs. unverified"
//! distinction did not. This module fills that gap with a cheap, conservative
//! correlation that lights those fields up for results that belong to the
//! project the agent is working in right now.
//!
//! ## How (and why it does not fabricate trust)
//!
//! 1. Build once per query a [`CorrelationIndex`] for the CURRENT project (the
//!    git repo containing the working directory): closed/open bead facts from
//!    `.beads/issues.jsonl`, bead → commit links parsed from `(<project>-<id>)`
//!    commit subjects, and the set of known commit ids.
//! 2. For each on-project result, [`correlate`] scans the result's own indexed
//!    text for an EXPLICIT reference to a known bead id or commit id and links
//!    it. A temporal or workspace coincidence is never enough — we require an
//!    explicit identifier match — so an unrelated conversation never inherits a
//!    "trusted" verdict from work it merely happened near.
//! 3. Release containment (and thus release-backed proof) is resolved lazily per
//!    matched commit via `git tag --contains`, then cached.
//!
//! ## Pure vs. live
//!
//! The pure functions ([`correlate`], [`proof_for`], identifier extraction) are
//! deterministic functions of an injected [`CorrelationIndex`] + text, so they
//! are unit-tested without a repo. The live builder and release resolver are
//! fail-open: a missing repo, a git error, or an unreadable beads file yields an
//! empty index, and the verdict simply falls back to the recency / workspace /
//! source / mode portion. Correlation is advisory metadata only — like the rest
//! of the trust layer, it never changes result ordering and never emits raw
//! session text (only sanitized identifiers flow downstream).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use crate::search::trust_scoring::{OutcomeMarker, ProofStatus};

/// How many recent commits to scan for bead → commit links when building the
/// index. Bounded so the build stays one cheap `git log` invocation.
const GIT_LOG_LIMIT: usize = 5000;

/// Max characters of a result's text scanned for identifier references. Refs
/// (bead ids, commit ids) appear in titles/early lines in practice, so a bounded
/// prefix keeps per-result work cheap without missing the common case.
const MAX_SCAN_CHARS: usize = 8192;

/// Length of the commit-id prefix used to index/match known commits. Long enough
/// to be collision-free in any realistic project history, short enough to match
/// the abbreviated ids agents quote.
const COMMIT_PREFIX_LEN: usize = 12;

/// Facts about one bead needed for correlation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BeadFact {
    /// Whether the bead is closed (vs. open / in-progress).
    pub closed: bool,
}

/// The strongest commit/bead link found for one result. Release tag and final
/// proof status are resolved by the caller (release lookup is I/O), so this pure
/// result carries only what an explicit text match can establish on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitBeadLink {
    /// Linked commit id (full 40-char), if a closed bead's commit or a directly
    /// referenced known commit matched.
    pub linked_commit: Option<String>,
    /// Linked closed bead id, if a closed bead was referenced.
    pub linked_closed_bead: Option<String>,
    /// Content-stable lessons corroborated by the linked commit/closed Bead.
    /// These are citation-only and never affect the outcome or trust tier.
    pub linked_lessons: Vec<String>,
    /// Outcome marker implied by the match (`Landed` for a closed bead or a real
    /// commit, `Open` for an open bead, `Unknown` for no match).
    pub outcome: OutcomeMarker,
}

impl CommitBeadLink {
    /// The empty link (no correlation found).
    pub fn none() -> Self {
        CommitBeadLink {
            linked_commit: None,
            linked_closed_bead: None,
            linked_lessons: Vec::new(),
            outcome: OutcomeMarker::Unknown,
        }
    }

    /// Whether any correlation signal was found.
    pub fn is_empty(&self) -> bool {
        self.linked_commit.is_none()
            && self.linked_closed_bead.is_none()
            && matches!(self.outcome, OutcomeMarker::Unknown)
    }
}

/// A cheap, once-per-query correlation index for the current project.
#[derive(Debug, Default)]
pub struct CorrelationIndex {
    /// Bead id → facts, keyed by the full bead id (e.g.
    /// `coding_agent_session_search-q4pau`).
    beads: HashMap<String, BeadFact>,
    /// Bead id → the newest commit id whose subject referenced it.
    bead_commit: HashMap<String, String>,
    /// Known full commit ids, indexed by their [`COMMIT_PREFIX_LEN`]-char prefix
    /// for cheap reference matching.
    commit_by_prefix: HashMap<String, String>,
    /// Commit/Bead `source_ref` -> content-stable lesson ids derived from the
    /// same live repository evidence as `cass lessons`.
    lesson_by_source: HashMap<String, Vec<String>>,
    /// The `<project>-` prefix used to recognize this project's bead ids in text
    /// (e.g. `coding_agent_session_search-`). `None` disables bead matching.
    project_prefix: Option<String>,
    /// The project's git root, exposed as the cwd workspace anchor and used for
    /// lazy release resolution. `None` disables release lookups.
    repo_root: Option<PathBuf>,
    /// Cache of commit id → containing release tag (`None` = resolved, no tag).
    release_cache: Mutex<HashMap<String, Option<String>>>,
}

/// Return deterministic lesson ids whose source provenance was already
/// corroborated by the existing commit/closed-Bead correlation.
fn corroborated_lesson_ids(
    index: &CorrelationIndex,
    bead: Option<&str>,
    commit: Option<&str>,
) -> Vec<String> {
    let mut lessons = Vec::new();
    for source_ref in [
        bead.map(|id| format!("bead:{id}")),
        commit.map(|sha| format!("commit:{sha}")),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(ids) = index.lesson_by_source.get(&source_ref) {
            lessons.extend(ids.iter().cloned());
        }
    }
    lessons.sort();
    lessons.dedup();
    lessons
}

impl CorrelationIndex {
    /// Whether the index carries no correlatable facts (so [`correlate`] can
    /// early-out). An index with a repo root but no beads/commits is still empty
    /// for correlation purposes.
    pub fn is_empty(&self) -> bool {
        self.beads.is_empty() && self.commit_by_prefix.is_empty()
    }

    /// The current project's workspace anchor (git root as a string), used as the
    /// `query_workspace` for cwd-relative workspace-match scoring.
    pub fn project_workspace(&self) -> Option<String> {
        self.repo_root
            .as_ref()
            .map(|root| root.to_string_lossy().into_owned())
    }

    /// Resolve the earliest release tag containing `commit`, lazily and cached.
    /// Returns `None` when there is no repo root, no containing release, or git
    /// is unavailable. A "release tag" is a `v<digit>…` tag, so feature/test tags
    /// never masquerade as a release.
    pub fn release_tag_for_commit(&self, commit: &str) -> Option<String> {
        let root = self.repo_root.as_ref()?;
        if let Ok(cache) = self.release_cache.lock()
            && let Some(cached) = cache.get(commit)
        {
            return cached.clone();
        }
        let resolved = git_output(
            root,
            &[
                "tag",
                "--contains",
                commit,
                "--sort=creatordate",
                "--format=%(refname:short)",
            ],
        )
        .and_then(|out| {
            out.lines()
                .map(str::trim)
                .find(|tag| is_release_tag(tag))
                .map(str::to_string)
        });
        if let Ok(mut cache) = self.release_cache.lock() {
            cache.insert(commit.to_string(), resolved.clone());
        }
        resolved
    }
}

/// Whether `tag` looks like a release tag (`v` followed by a digit, e.g.
/// `v0.6.15`). Conservative so non-release tags never imply release-backed proof.
fn is_release_tag(tag: &str) -> bool {
    let mut chars = tag.chars();
    matches!(chars.next(), Some('v') | Some('V'))
        && matches!(chars.next(), Some(c) if c.is_ascii_digit())
}

/// Map a correlation result + resolved release containment to a proof status.
/// Pure. Release-contained landed work is `Proven`; landed-but-unreleased work is
/// `ProofDebt`; everything else is `Unknown` (an open bead or a bare bead
/// reference with no commit carries provenance but no proof).
pub fn proof_for(outcome: OutcomeMarker, has_commit: bool, has_release: bool) -> ProofStatus {
    match outcome {
        OutcomeMarker::Landed if has_release => ProofStatus::Proven,
        OutcomeMarker::Landed if has_commit => ProofStatus::ProofDebt,
        _ => ProofStatus::Unknown,
    }
}

/// Whether a result's workspace is the project the agent is in now (cwd-relative
/// workspace match, q4pau). Component-aware path containment: either path may be
/// the other's ancestor (a session run in a subdirectory still belongs to the
/// project), compared case-insensitively with a trailing slash trimmed. Pure.
pub fn workspace_matches(query_workspace: &str, result_workspace: &str) -> bool {
    let q = query_workspace.trim().trim_end_matches('/');
    let w = result_workspace.trim().trim_end_matches('/');
    if q.is_empty() || w.is_empty() {
        return false;
    }
    if q.eq_ignore_ascii_case(w) {
        return true;
    }
    // Ancestor containment on a path boundary (avoid "/proj/a" matching
    // "/proj/ab"): the longer must start with the shorter followed by '/'.
    let (shorter, longer) = if q.len() <= w.len() { (q, w) } else { (w, q) };
    longer.len() > shorter.len()
        && longer.as_bytes()[shorter.len()] == b'/'
        && longer[..shorter.len()].eq_ignore_ascii_case(shorter)
}

/// Join a result's text fields into a single bounded scan string for
/// [`correlate`], capped at [`MAX_SCAN_CHARS`] characters (char-safe, one
/// allocation). Refs appear early in practice, so a bounded prefix suffices.
pub fn scan_text(parts: &[&str]) -> String {
    let mut out = String::new();
    let mut budget = MAX_SCAN_CHARS;
    for part in parts {
        if budget == 0 {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
            budget -= 1;
        }
        for c in part.chars() {
            if budget == 0 {
                break;
            }
            out.push(c);
            budget -= 1;
        }
    }
    out
}

/// Whether a char can appear inside a bead id or an abbreviated commit id.
fn is_id_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')
}

/// Whether `word` is a plausible commit id reference (hex, long enough to be
/// collision-free, not so short it matches noise).
fn is_commit_word(word: &str) -> bool {
    word.len() >= COMMIT_PREFIX_LEN
        && word.len() <= 40
        && word.chars().all(|c| c.is_ascii_hexdigit())
}

/// Walk `text` (bounded) splitting it into maximal identifier words. Each word is
/// a run of [`is_id_char`] characters. Stops after [`MAX_SCAN_CHARS`] characters.
fn id_words(text: &str) -> Vec<&str> {
    let mut words = Vec::new();
    let mut start: Option<usize> = None;
    for (idx, c) in text.char_indices().take(MAX_SCAN_CHARS) {
        if is_id_char(c) {
            start.get_or_insert(idx);
        } else if let Some(s) = start.take() {
            words.push(&text[s..idx]);
        }
    }
    // A trailing word still open at the scan boundary.
    if let Some(s) = start {
        // Re-derive the end from the bounded scan to keep the slice in range.
        let end = text
            .char_indices()
            .take(MAX_SCAN_CHARS)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(text.len());
        if s < end {
            words.push(&text[s..end]);
        }
    }
    words
}

/// Scan `text` for an explicit reference to a known bead id or commit id and
/// return the strongest link found. Pure: a function of `index` + `text` only.
///
/// Resolution prefers the strongest provenance: a referenced closed bead (with
/// its linked commit when known) beats a bare referenced commit, which beats a
/// referenced open bead. Ties are broken deterministically by sorted id.
pub fn correlate(index: &CorrelationIndex, text: &str) -> CommitBeadLink {
    if index.is_empty() {
        return CommitBeadLink::none();
    }

    let mut closed_beads: Vec<&str> = Vec::new();
    let mut open_beads: Vec<&str> = Vec::new();
    let mut commits: Vec<&str> = Vec::new();

    for word in id_words(text) {
        // Bead reference?
        if let Some(prefix) = index.project_prefix.as_deref()
            && word.starts_with(prefix)
        {
            // Trim trailing id punctuation that commonly abuts a ref in prose
            // (e.g. "...-q4pau." or "...-q4pau)").
            let candidate = word.trim_end_matches(['.', '-']);
            if let Some(fact) = index.beads.get(candidate) {
                if fact.closed {
                    closed_beads.push(candidate);
                } else {
                    open_beads.push(candidate);
                }
                continue;
            }
        }
        // Commit reference?
        if is_commit_word(word) {
            let lower = &word[..COMMIT_PREFIX_LEN];
            // char_indices on ascii hex => byte slice is char-safe here.
            if index
                .commit_by_prefix
                .contains_key(&lower.to_ascii_lowercase())
            {
                commits.push(word);
            }
        }
    }

    closed_beads.sort_unstable();
    closed_beads.dedup();
    open_beads.sort_unstable();
    open_beads.dedup();
    commits.sort_unstable();
    commits.dedup();

    let direct_commit = commits.first().and_then(|word| {
        index
            .commit_by_prefix
            .get(&word[..COMMIT_PREFIX_LEN].to_ascii_lowercase())
            .cloned()
    });

    // Prefer a closed bead that carries a commit link; else the first closed
    // bead; else fall through to a bare commit / open bead.
    if let Some(&first_closed) = closed_beads.first() {
        let chosen = closed_beads
            .iter()
            .copied()
            .find(|id| index.bead_commit.contains_key(*id))
            .unwrap_or(first_closed);
        let linked_commit = index
            .bead_commit
            .get(chosen)
            .cloned()
            .or(direct_commit.clone());
        let linked_lessons = corroborated_lesson_ids(index, Some(chosen), linked_commit.as_deref());
        return CommitBeadLink {
            linked_commit,
            linked_closed_bead: Some(chosen.to_string()),
            linked_lessons,
            outcome: OutcomeMarker::Landed,
        };
    }
    if let Some(commit) = direct_commit {
        // A real commit in project history is landed work.
        let linked_lessons = corroborated_lesson_ids(index, None, Some(&commit));
        return CommitBeadLink {
            linked_commit: Some(commit),
            linked_closed_bead: None,
            linked_lessons,
            outcome: OutcomeMarker::Landed,
        };
    }
    if let Some(open) = open_beads.first() {
        return CommitBeadLink {
            linked_commit: index.bead_commit.get(*open).cloned(),
            linked_closed_bead: None,
            linked_lessons: Vec::new(),
            outcome: OutcomeMarker::Open,
        };
    }
    CommitBeadLink::none()
}

// ---------------------------------------------------------------------------
// Live index construction (fail-open I/O).
// ---------------------------------------------------------------------------

/// Build a correlation index for the project containing the working directory.
/// Always succeeds: returns an empty index when there is no repo, no beads, or
/// git is unavailable.
pub fn build_for_cwd() -> CorrelationIndex {
    std::env::current_dir()
        .ok()
        .and_then(|cwd| build_for_repo(&cwd))
        .unwrap_or_default()
}

/// Build a correlation index for the git repo containing `start`. `None` when
/// `start` is not inside a git repo (so the caller falls back to an empty index).
fn build_for_repo(start: &Path) -> Option<CorrelationIndex> {
    let root = git_output(start, &["rev-parse", "--show-toplevel"])?;
    let root = PathBuf::from(root);

    let (beads, project_name) = read_bead_facts(&root.join(".beads").join("issues.jsonl"));
    let project_prefix = project_name.map(|name| format!("{name}-"));
    let (bead_commit, commit_by_prefix) = read_git_links(&root, project_prefix.as_deref());
    let lesson_by_source = build_lesson_source_index(&root);

    Some(CorrelationIndex {
        beads,
        bead_commit,
        commit_by_prefix,
        lesson_by_source,
        project_prefix,
        repo_root: Some(root),
        release_cache: Mutex::new(HashMap::new()),
    })
}

/// Derive the actual current lesson ids once per live correlation index and
/// index them by their already-sanitized commit/Bead source references. This
/// preserves the content-stable id and supersession rules owned by the lessons
/// core instead of reimplementing either rule in the trust layer.
fn build_lesson_source_index(root: &Path) -> HashMap<String, Vec<String>> {
    let evidence = crate::gather_repository_lessons_evidence(root);
    let extraction = crate::lessons_extraction::extract(&evidence);
    let graph = crate::lessons::LessonGraph::build(extraction.candidates);
    let mut by_source: HashMap<String, Vec<String>> = HashMap::new();
    for lesson in graph.lessons {
        for source_ref in lesson.source_refs {
            by_source
                .entry(source_ref)
                .or_default()
                .push(lesson.lesson_id.clone());
        }
    }
    for ids in by_source.values_mut() {
        ids.sort();
        ids.dedup();
    }
    by_source
}

/// Read bead status facts from a beads `issues.jsonl` file. Returns the per-id
/// facts and the project's `source_repo` name (used to build the id prefix).
/// Fail-open: an unreadable/absent file yields no facts.
fn read_bead_facts(path: &Path) -> (HashMap<String, BeadFact>, Option<String>) {
    let mut facts = HashMap::new();
    let mut project_name = None;
    let Ok(contents) = std::fs::read_to_string(path) else {
        return (facts, None);
    };
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(id) = value.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        let closed = value
            .get("status")
            .and_then(|v| v.as_str())
            .is_some_and(|status| status.eq_ignore_ascii_case("closed"));
        facts.insert(id.to_string(), BeadFact { closed });
        if project_name.is_none()
            && let Some(repo) = value.get("source_repo").and_then(|v| v.as_str())
            && !repo.trim().is_empty()
        {
            project_name = Some(repo.trim().to_string());
        }
    }
    (facts, project_name)
}

/// Read bead → commit links and known commit prefixes from recent git history.
/// Parses `(<project>-<id>)` references out of commit subjects. Fail-open: no git
/// or no repo yields empty maps.
fn read_git_links(
    root: &Path,
    project_prefix: Option<&str>,
) -> (HashMap<String, String>, HashMap<String, String>) {
    let mut bead_commit = HashMap::new();
    let mut commit_by_prefix = HashMap::new();
    let log = git_output(
        root,
        &[
            "log",
            &format!("-n{GIT_LOG_LIMIT}"),
            "--no-color",
            "--format=%H%x09%s",
        ],
    );
    let Some(log) = log else {
        return (bead_commit, commit_by_prefix);
    };
    for line in log.lines() {
        let Some((sha, subject)) = line.split_once('\t') else {
            continue;
        };
        let sha = sha.trim();
        if sha.len() >= COMMIT_PREFIX_LEN && sha.chars().all(|c| c.is_ascii_hexdigit()) {
            commit_by_prefix
                .entry(sha[..COMMIT_PREFIX_LEN].to_ascii_lowercase())
                .or_insert_with(|| sha.to_ascii_lowercase());
        }
        if let Some(prefix) = project_prefix {
            for bead_id in parse_bead_refs(subject, prefix) {
                // git log is newest-first, so the first commit seen for a bead is
                // its newest reference — keep it.
                bead_commit
                    .entry(bead_id)
                    .or_insert_with(|| sha.to_ascii_lowercase());
            }
        }
    }
    (bead_commit, commit_by_prefix)
}

/// Extract `<project>-<id>` bead references from a commit subject. References are
/// expected in `(…)`, but a bare reference is accepted too. Pure.
fn parse_bead_refs(subject: &str, project_prefix: &str) -> Vec<String> {
    let mut refs = Vec::new();
    let mut rest = subject;
    while let Some(pos) = rest.find(project_prefix) {
        let tail = &rest[pos..];
        let end = tail
            .char_indices()
            .find(|(_, c)| !is_id_char(*c))
            .map(|(i, _)| i)
            .unwrap_or(tail.len());
        let candidate = tail[..end].trim_end_matches(['.', '-']);
        if candidate.len() > project_prefix.len() {
            refs.push(candidate.to_string());
        }
        rest = &tail[end.max(1)..];
    }
    refs
}

/// Run a read-only git command in `repo_path`, returning trimmed stdout on
/// success. Fail-open: any spawn error or non-zero exit yields `None`.
fn git_output(repo_path: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index_with(
        project_prefix: &str,
        beads: &[(&str, bool)],
        bead_commit: &[(&str, &str)],
        commits: &[&str],
    ) -> CorrelationIndex {
        let beads = beads
            .iter()
            .map(|(id, closed)| (id.to_string(), BeadFact { closed: *closed }))
            .collect();
        let bead_commit = bead_commit
            .iter()
            .map(|(id, sha)| (id.to_string(), sha.to_string()))
            .collect();
        let commit_by_prefix = commits
            .iter()
            .map(|sha| {
                (
                    sha[..COMMIT_PREFIX_LEN].to_ascii_lowercase(),
                    sha.to_string(),
                )
            })
            .collect();
        CorrelationIndex {
            beads,
            bead_commit,
            commit_by_prefix,
            lesson_by_source: HashMap::new(),
            project_prefix: Some(project_prefix.to_string()),
            repo_root: None,
            release_cache: Mutex::new(HashMap::new()),
        }
    }

    const SHA_A: &str = "ab0d12ef90abcdef1234567890abcdef12345678";
    const SHA_B: &str = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef";

    #[test]
    fn empty_index_correlates_to_nothing() {
        let idx = CorrelationIndex::default();
        assert_eq!(
            correlate(&idx, "mentions proj-q4pau"),
            CommitBeadLink::none()
        );
        assert!(idx.is_empty());
    }

    #[test]
    fn closed_bead_reference_links_landed_with_commit() {
        let idx = index_with(
            "proj-",
            &[("proj-q4pau", true)],
            &[("proj-q4pau", SHA_A)],
            &[SHA_A],
        );
        let link = correlate(&idx, "fixed in proj-q4pau, see the closeout");
        assert_eq!(link.outcome, OutcomeMarker::Landed);
        assert_eq!(link.linked_closed_bead.as_deref(), Some("proj-q4pau"));
        assert_eq!(link.linked_commit.as_deref(), Some(SHA_A));
    }

    #[test]
    fn lesson_refs_require_corroborated_source_provenance() {
        let mut idx = index_with(
            "proj-",
            &[("proj-q4pau", true), ("proj-other", true)],
            &[("proj-q4pau", SHA_A)],
            &[SHA_A],
        );
        idx.lesson_by_source.insert(
            "bead:proj-q4pau".to_string(),
            vec!["lsn-1111111111111111".to_string()],
        );
        idx.lesson_by_source.insert(
            "commit:ab0d12ef90abcdef1234567890abcdef12345678".to_string(),
            vec!["lsn-2222222222222222".to_string()],
        );
        idx.lesson_by_source.insert(
            "bead:proj-other".to_string(),
            vec!["lsn-3333333333333333".to_string()],
        );

        let linked = correlate(&idx, "fixed in proj-q4pau closeout");
        assert_eq!(
            linked.linked_lessons,
            vec![
                "lsn-1111111111111111".to_string(),
                "lsn-2222222222222222".to_string()
            ]
        );
        assert!(
            !linked
                .linked_lessons
                .contains(&"lsn-3333333333333333".to_string()),
            "an unrelated lesson must not be cited"
        );

        let unrelated = correlate(&idx, "no known identifier here");
        assert!(unrelated.linked_lessons.is_empty());
    }

    #[test]
    fn open_bead_reference_is_open_not_landed() {
        let idx = index_with("proj-", &[("proj-wip1", false)], &[], &[]);
        let link = correlate(&idx, "still working on proj-wip1 today");
        assert_eq!(link.outcome, OutcomeMarker::Open);
        assert_eq!(link.linked_closed_bead, None);
    }

    #[test]
    fn closed_bead_beats_open_bead_in_same_text() {
        let idx = index_with(
            "proj-",
            &[("proj-aaa", true), ("proj-zzz", false)],
            &[("proj-aaa", SHA_A)],
            &[SHA_A],
        );
        let link = correlate(&idx, "proj-zzz blocked until proj-aaa landed");
        assert_eq!(link.outcome, OutcomeMarker::Landed);
        assert_eq!(link.linked_closed_bead.as_deref(), Some("proj-aaa"));
    }

    #[test]
    fn bare_commit_reference_is_landed() {
        let idx = index_with("proj-", &[], &[], &[SHA_B]);
        let text = format!("landed as {SHA_B} last week");
        let link = correlate(&idx, &text);
        assert_eq!(link.outcome, OutcomeMarker::Landed);
        assert_eq!(link.linked_commit.as_deref(), Some(SHA_B));
        assert_eq!(link.linked_closed_bead, None);
    }

    #[test]
    fn abbreviated_commit_reference_matches_by_prefix() {
        let idx = index_with("proj-", &[], &[], &[SHA_A]);
        // 12-char abbreviation of SHA_A.
        let link = correlate(&idx, "see commit ab0d12ef90ab for the fix");
        assert_eq!(link.linked_commit.as_deref(), Some(SHA_A));
    }

    #[test]
    fn short_hex_below_prefix_len_is_ignored() {
        let idx = index_with("proj-", &[], &[], &[SHA_A]);
        // 7-char abbreviation is below COMMIT_PREFIX_LEN; not matched.
        let link = correlate(&idx, "see commit ab0d12e for the fix");
        assert!(link.is_empty());
    }

    #[test]
    fn unrelated_text_does_not_fabricate_a_link() {
        let idx = index_with(
            "proj-",
            &[("proj-q4pau", true)],
            &[("proj-q4pau", SHA_A)],
            &[SHA_A],
        );
        let link = correlate(&idx, "a totally unrelated conversation about cooking pasta");
        assert!(link.is_empty(), "no explicit id => no link");
    }

    #[test]
    fn trailing_punctuation_on_ref_still_matches() {
        let idx = index_with("proj-", &[("proj-q4pau", true)], &[], &[]);
        let link = correlate(&idx, "(closes proj-q4pau).");
        assert_eq!(link.linked_closed_bead.as_deref(), Some("proj-q4pau"));
    }

    #[test]
    fn proof_for_release_backed_is_proven() {
        assert_eq!(
            proof_for(OutcomeMarker::Landed, true, true),
            ProofStatus::Proven
        );
    }

    #[test]
    fn proof_for_landed_unreleased_is_proof_debt() {
        assert_eq!(
            proof_for(OutcomeMarker::Landed, true, false),
            ProofStatus::ProofDebt
        );
    }

    #[test]
    fn proof_for_open_or_no_commit_is_unknown() {
        assert_eq!(
            proof_for(OutcomeMarker::Open, false, false),
            ProofStatus::Unknown
        );
        assert_eq!(
            proof_for(OutcomeMarker::Landed, false, false),
            ProofStatus::Unknown
        );
    }

    #[test]
    fn is_release_tag_only_accepts_versioned_tags() {
        assert!(is_release_tag("v0.6.15"));
        assert!(is_release_tag("v1"));
        assert!(!is_release_tag("nightly"));
        assert!(!is_release_tag("vendor"));
        assert!(!is_release_tag("release-candidate"));
    }

    #[test]
    fn parse_bead_refs_extracts_parenthesized_and_bare() {
        let refs = parse_bead_refs(
            "feat(x): do thing (coding-q4pau) and coding-5u82n.3 too",
            "coding-",
        );
        assert!(refs.contains(&"coding-q4pau".to_string()));
        assert!(refs.contains(&"coding-5u82n.3".to_string()));
    }

    #[test]
    fn parse_bead_refs_ignores_bare_prefix() {
        // Prefix with no id suffix should not produce a ref.
        let refs = parse_bead_refs("just the coding- prefix alone", "coding-");
        assert!(refs.is_empty());
    }

    #[test]
    fn id_words_is_bounded_and_char_safe() {
        // Non-ASCII before/after an id must not panic or split a char.
        let text = "héllo proj-q4pau wörld";
        let words = id_words(text);
        assert!(words.contains(&"proj-q4pau"));
    }

    #[test]
    fn workspace_matches_handles_equality_containment_and_boundaries() {
        // Exact (case + trailing slash insensitive).
        assert!(workspace_matches("/proj/a", "/proj/a/"));
        assert!(workspace_matches("/Proj/A", "/proj/a"));
        // Ancestor containment either direction (session run in a subdir).
        assert!(workspace_matches("/proj/a", "/proj/a/src/search"));
        assert!(workspace_matches("/proj/a/src", "/proj/a"));
        // Sibling and prefix-not-on-boundary must NOT match.
        assert!(!workspace_matches("/proj/a", "/proj/b"));
        assert!(!workspace_matches("/proj/a", "/proj/ab"));
        // Empty never matches.
        assert!(!workspace_matches("", "/proj/a"));
        assert!(!workspace_matches("/proj/a", "   "));
    }

    #[test]
    fn scan_text_joins_and_bounds() {
        let joined = scan_text(&["title", "snippet", "body"]);
        assert_eq!(joined, "title snippet body");
        // Bounded to MAX_SCAN_CHARS.
        let big = "x".repeat(MAX_SCAN_CHARS * 2);
        assert_eq!(scan_text(&[&big]).chars().count(), MAX_SCAN_CHARS);
    }

    #[test]
    fn correlate_is_deterministic() {
        let idx = index_with(
            "proj-",
            &[("proj-aaa", true), ("proj-bbb", true)],
            &[("proj-aaa", SHA_A), ("proj-bbb", SHA_B)],
            &[SHA_A, SHA_B],
        );
        let text = "proj-bbb and proj-aaa both referenced";
        assert_eq!(correlate(&idx, text), correlate(&idx, text));
    }
}
