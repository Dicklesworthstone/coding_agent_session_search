//! Extraction + redaction layer that mines cass's own local evidence — landed
//! commit summaries, closed bead reasons, and proof-run records — into redacted
//! [`LessonCandidate`]s for the durable [`crate::lessons::LessonGraph`].
//!
//! Bead: coding_agent_session_search-guided-ops-repro-trust-5u82n.4
//! ("Extract durable lessons and decisions from closed sessions").
//!
//! ## Where this sits
//!
//! [`crate::lessons`] is the metadata-first record contract and graph core: it
//! dedupes by a content-stable id and resolves supersession. It deliberately
//! says nothing about *how* candidates are sourced. This module is that source:
//! deterministic, pure classification of evidence into [`LessonCandidate`]s plus
//! the redaction pass that keeps raw private text out of the summaries.
//!
//! ## No raw leakage (by construction)
//!
//! Every free-text field that becomes a candidate's `redacted_summary` first
//! passes through [`redact`], which removes home-directory paths (the part that
//! reveals a username), e-mail addresses, and long opaque digests. The
//! [`RedactionReport`] counts what was removed so a reviewer can audit the pass.
//! Provenance (commit sha, bead id, proof name) flows into `source_refs`, never
//! into the summary, so identifiers stay attributable without smuggling text.
//!
//! ## Pure and deterministic
//!
//! Callers supply already-loaded evidence ([`LessonsEvidence`]); this module
//! does no I/O. The same evidence always yields the same candidates, manifest,
//! and redaction report, so the output is golden-stable and safe to test
//! against a checked-in fixture corpus.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::lessons::{LessonCandidate, LessonConfidence, LessonKind};

/// Stable schema version for the evidence wire format consumed here.
pub const LESSONS_EVIDENCE_SCHEMA_VERSION: u32 = 1;

fn default_project() -> String {
    "cass".to_string()
}

/// A landed git commit: its summary line is the durable lesson, the sha is
/// provenance, and the timestamp drives freshness/supersession.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitEvidence {
    /// Commit hash (provenance only; never placed in the summary).
    pub sha: String,
    /// Conventional-commit subject line.
    pub subject: String,
    /// Optional first body paragraph (extra context).
    #[serde(default)]
    pub body: String,
    /// Author/commit time as epoch-ms (caller-supplied for determinism).
    pub timestamp_ms: u64,
}

/// A bead (issue) and the reason it closed — the richest local source of
/// "decisions that landed" and "approaches that failed".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BeadEvidence {
    /// Bead id (provenance only).
    pub id: String,
    /// Bead title.
    pub title: String,
    /// Close reason (or resolution note); may be empty.
    #[serde(default)]
    pub close_reason: String,
    /// Issue type: `bug`, `task`, `feature`, `epic`, ...
    #[serde(default)]
    pub issue_type: String,
    /// Lifecycle: `closed`, `open`, `in_progress`, ...
    #[serde(default)]
    pub status: String,
    /// Labels (used for topic + applies_to hints).
    #[serde(default)]
    pub labels: Vec<String>,
    /// Last-updated time as epoch-ms.
    pub updated_ms: u64,
}

/// A recorded proof run (test / gauntlet / smoke gate). A passing proof is a
/// reusable invariant; a failing/timed-out one is a known footgun.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofEvidence {
    /// Proof/test name (provenance + topic).
    pub name: String,
    /// Outcome: `pass`, `fail`, `timeout`, `stale-artifact`, ...
    pub status: String,
    /// Command that produced the proof (redacted into the summary).
    #[serde(default)]
    pub command: String,
    /// When the proof ran, epoch-ms.
    pub timestamp_ms: u64,
}

/// The full evidence bundle handed to [`extract`]. Built from a fixture file in
/// tests/replay, or gathered from local sources (beads JSONL, git log, proof
/// manifest) in the live path.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LessonsEvidence {
    /// Project the evidence belongs to.
    #[serde(default = "default_project")]
    pub project: String,
    /// Landed commits.
    #[serde(default)]
    pub commits: Vec<CommitEvidence>,
    /// Beads (closed or otherwise).
    #[serde(default)]
    pub beads: Vec<BeadEvidence>,
    /// Proof runs.
    #[serde(default)]
    pub proofs: Vec<ProofEvidence>,
}

/// Tally of what the redaction pass removed, by class.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedactionReport {
    /// `/home/<user>` and `/Users/<user>` prefixes whose username was stripped.
    pub home_paths: usize,
    /// E-mail addresses removed.
    pub emails: usize,
    /// Long opaque digests / key-like strings removed.
    pub digests: usize,
}

impl RedactionReport {
    /// Total redactions across all classes.
    pub fn total(self) -> usize {
        self.home_paths + self.emails + self.digests
    }

    fn add(&mut self, other: RedactionReport) {
        self.home_paths += other.home_paths;
        self.emails += other.emails;
        self.digests += other.digests;
    }
}

/// Strip leading/trailing punctuation from a token, returning
/// `(leading, core, trailing)` so a redacted core can be re-wrapped.
fn split_affixes(word: &str) -> (&str, &str, &str) {
    const LEAD: &[char] = &['"', '\'', '(', '<', '[', '{', '`', '=', ':'];
    const TRAIL: &[char] = &[
        '"', '\'', ')', '>', ']', '}', '`', ',', '.', ';', ':', '!', '?',
    ];
    let core = word.trim_start_matches(LEAD);
    let lead = &word[..word.len() - core.len()];
    let core_trimmed = core.trim_end_matches(TRAIL);
    let trail = &core[core_trimmed.len()..];
    (lead, core_trimmed, trail)
}

/// Whether a token core looks like an e-mail address.
fn is_email(core: &str) -> bool {
    let Some(at) = core.find('@') else {
        return false;
    };
    let (local, domain) = core.split_at(at);
    let domain = &domain[1..];
    !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !local.contains('@')
        && !domain.contains('@')
}

/// Whether a token core is a long opaque digest or key-like string we should
/// never surface. Precise on purpose: short shas (provenance) are left intact.
fn is_opaque_digest(core: &str) -> bool {
    // Known credential-ish prefixes of any length.
    const PREFIXES: &[&str] = &["ghp_", "gho_", "sk-", "xoxb-", "xoxp-", "AKIA", "ASIA"];
    if PREFIXES.iter().any(|p| core.starts_with(p)) && core.len() >= 12 {
        return true;
    }
    // A 32+ char all-hex run (blake3/sha256-class digests).
    core.len() >= 32 && core.chars().all(|c| c.is_ascii_hexdigit())
}

/// Redact a home path prefix, returning the rewritten path if it matched.
fn redact_home_path(core: &str) -> Option<String> {
    for root in ["/home/", "/Users/"] {
        if let Some(rest) = core.strip_prefix(root) {
            // Drop the username segment, keep the remaining (project-relative) tail.
            let tail = rest.split_once('/').map(|(_, tail)| tail).unwrap_or("");
            return Some(if tail.is_empty() {
                "<home>".to_string()
            } else {
                format!("<home>/{tail}")
            });
        }
    }
    None
}

/// Redact a single text field: removes home-path usernames, e-mails, and opaque
/// digests, preserving the original whitespace layout. Returns the redacted
/// string and a per-class [`RedactionReport`].
pub fn redact(input: &str) -> (String, RedactionReport) {
    let mut report = RedactionReport::default();
    let mut out = String::with_capacity(input.len());
    // Walk char by char, preserving whitespace verbatim and transforming each
    // maximal non-whitespace word as it completes.
    let mut word = String::new();
    for c in input.chars() {
        if c.is_whitespace() {
            if !word.is_empty() {
                out.push_str(&redact_word(&word, &mut report));
                word.clear();
            }
            out.push(c);
        } else {
            word.push(c);
        }
    }
    if !word.is_empty() {
        out.push_str(&redact_word(&word, &mut report));
    }
    (out, report)
}

fn redact_word(word: &str, report: &mut RedactionReport) -> String {
    let (lead, core, trail) = split_affixes(word);
    if core.is_empty() {
        return word.to_string();
    }
    if is_email(core) {
        report.emails += 1;
        return format!("{lead}<email>{trail}");
    }
    if let Some(redacted) = redact_home_path(core) {
        report.home_paths += 1;
        return format!("{lead}{redacted}{trail}");
    }
    if is_opaque_digest(core) {
        report.digests += 1;
        return format!("{lead}<digest>{trail}");
    }
    word.to_string()
}

/// Security-relevant keywords that override the default classification.
const SECURITY_KEYWORDS: &[&str] = &[
    "security",
    "vuln",
    "cve",
    "injection",
    "exploit",
    "rce",
    "xss",
    "csrf",
    "ssrf",
    "sandbox escape",
    "privilege escalation",
    "unsafe",
];

/// Keywords that mark an approach as a dead end.
const FAILED_KEYWORDS: &[&str] = &[
    "revert",
    "abandon",
    "wontfix",
    "won't fix",
    "not viable",
    "dead end",
    "doesn't work",
    "does not work",
    "gave up",
    "rolled back",
];

/// Keywords that mark advice as outdated/superseded at the source.
const OUTDATED_KEYWORDS: &[&str] = &[
    "supersed",
    "obsolet",
    "outdated",
    "deprecat",
    "no longer",
    "stale",
];

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    let lower = haystack.to_ascii_lowercase();
    needles.iter().any(|n| lower.contains(n))
}

/// Parse a conventional-commit `type(scope): summary` line into `(type, scope)`.
fn parse_conventional(subject: &str) -> (Option<String>, Option<String>) {
    let Some((head, _)) = subject.split_once(':') else {
        return (None, None);
    };
    let head = head.trim();
    if head.is_empty() || head.contains(' ') {
        // Not a conventional prefix (a colon mid-sentence); bail.
        return (None, None);
    }
    if let Some(open) = head.find('(')
        && let Some(close) = head.find(')')
        && close > open
    {
        let kind = head[..open].trim().to_ascii_lowercase();
        let scope = head[open + 1..close].trim().to_ascii_lowercase();
        return (
            Some(kind),
            if scope.is_empty() { None } else { Some(scope) },
        );
    }
    (Some(head.to_ascii_lowercase()), None)
}

/// First non-empty, lowercased word of `text` (a fallback topic).
fn first_word(text: &str) -> String {
    text.split_whitespace()
        .next()
        .unwrap_or("general")
        .trim_matches(|c: char| !c.is_alphanumeric())
        .to_ascii_lowercase()
}

/// Classify a commit into a [`LessonKind`] and a topic.
fn classify_commit(commit: &CommitEvidence) -> (LessonKind, String) {
    let combined = format!("{} {}", commit.subject, commit.body);
    let (ctype, scope) = parse_conventional(&commit.subject);
    let topic = scope.unwrap_or_else(|| first_word(&commit.subject));
    if contains_any(&combined, SECURITY_KEYWORDS) {
        return (LessonKind::SecurityWarning, topic);
    }
    let kind = match ctype.as_deref() {
        Some("revert") => LessonKind::FailedApproach,
        Some("fix") => LessonKind::Gotcha,
        Some("test") => LessonKind::Invariant,
        Some("feat") | Some("refactor") | Some("perf") | Some("deps") => {
            LessonKind::ReusableDecision
        }
        _ if combined.to_ascii_lowercase().starts_with("revert ") => LessonKind::FailedApproach,
        _ => LessonKind::ReusableDecision,
    };
    (kind, topic)
}

/// Classify a bead into a [`LessonKind`], a topic, and an outdated flag.
fn classify_bead(bead: &BeadEvidence) -> (LessonKind, String, bool) {
    let combined = format!(
        "{} {} {}",
        bead.title,
        bead.close_reason,
        bead.labels.join(" ")
    );
    let topic = bead
        .labels
        .first()
        .map(|l| l.to_ascii_lowercase())
        .unwrap_or_else(|| first_word(&bead.title));
    let outdated = contains_any(&combined, OUTDATED_KEYWORDS);
    let kind = if contains_any(&combined, SECURITY_KEYWORDS) {
        LessonKind::SecurityWarning
    } else if contains_any(&combined, FAILED_KEYWORDS) {
        LessonKind::FailedApproach
    } else if bead.issue_type.eq_ignore_ascii_case("bug") {
        LessonKind::Gotcha
    } else {
        LessonKind::ReusableDecision
    };
    (kind, topic, outdated)
}

/// Classify a proof run into a [`LessonKind`] and a topic.
fn classify_proof(proof: &ProofEvidence) -> (LessonKind, String) {
    let topic = first_word(&proof.name);
    let kind = if matches!(
        proof.status.to_ascii_lowercase().as_str(),
        "pass" | "ok" | "passed" | "green"
    ) {
        LessonKind::Invariant
    } else {
        LessonKind::Gotcha
    };
    (kind, topic)
}

/// A clean, non-empty single-line summary derived from `parts` (joined with
/// " — "), or `None` if everything was empty.
fn summary_from(parts: &[&str]) -> Option<String> {
    let joined = parts
        .iter()
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(" — ");
    if joined.is_empty() {
        None
    } else {
        Some(joined)
    }
}

/// First non-empty line of `text` (commit bodies can be multi-paragraph).
fn first_line(text: &str) -> &str {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
}

/// The result of an extraction pass: the candidates to feed the graph plus an
/// auditable manifest. This is an in-memory handoff type — the candidates flow
/// into [`crate::lessons::LessonGraph::build`] and the serialized surface is the
/// resulting graph plus the [`ExtractionManifest`], both of which are
/// `Serialize`. `LessonCandidate` itself is deliberately not serialized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractionResult {
    /// Candidates ready for [`crate::lessons::LessonGraph::build`].
    pub candidates: Vec<LessonCandidate>,
    /// Auditable manifest of what was scanned and redacted.
    pub manifest: ExtractionManifest,
}

/// An auditable summary of one extraction pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractionManifest {
    /// Mirrors [`LESSONS_EVIDENCE_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Project the evidence belongs to.
    pub project: String,
    /// Commits scanned.
    pub commits_scanned: usize,
    /// Beads scanned.
    pub beads_scanned: usize,
    /// Proof runs scanned.
    pub proofs_scanned: usize,
    /// Candidates emitted (before dedup in the graph).
    pub candidates_emitted: usize,
    /// Candidate count by [`LessonKind`] wire label (deterministic order).
    pub by_kind: BTreeMap<String, usize>,
    /// Redactions performed across all summaries.
    pub redaction: RedactionReport,
}

/// Extract redacted [`LessonCandidate`]s from `evidence`. Pure and
/// deterministic: no I/O, stable ordering, identical output for identical input.
pub fn extract(evidence: &LessonsEvidence) -> ExtractionResult {
    let project = if evidence.project.trim().is_empty() {
        default_project()
    } else {
        evidence.project.clone()
    };
    let mut candidates: Vec<LessonCandidate> = Vec::new();
    let mut redaction = RedactionReport::default();

    for commit in &evidence.commits {
        let (kind, topic) = classify_commit(commit);
        let (subject, r1) = redact(&commit.subject);
        redaction.add(r1);
        let body_line = first_line(&commit.body);
        let (body_red, r2) = redact(body_line);
        redaction.add(r2);
        let Some(summary) = summary_from(&[&subject, &body_red]) else {
            continue;
        };
        candidates.push(LessonCandidate {
            topic,
            project: project.clone(),
            kind,
            source_refs: vec![format!("commit:{}", commit.sha)],
            confidence: LessonConfidence::High,
            freshness_ms: commit.timestamp_ms,
            outdated: false,
            applies_to: Vec::new(),
            redacted_summary: summary,
        });
    }

    for bead in &evidence.beads {
        let (kind, topic, outdated) = classify_bead(bead);
        let (reason, r1) = redact(&bead.close_reason);
        redaction.add(r1);
        let (title, r2) = redact(&bead.title);
        redaction.add(r2);
        // Prefer the close reason; fall back to the title.
        let Some(summary) = summary_from(&[&reason]).or_else(|| summary_from(&[&title])) else {
            continue;
        };
        let confidence = if bead.status.eq_ignore_ascii_case("closed") {
            LessonConfidence::High
        } else {
            LessonConfidence::Medium
        };
        let mut applies_to: Vec<String> =
            bead.labels.iter().map(|l| l.to_ascii_lowercase()).collect();
        applies_to.sort();
        applies_to.dedup();
        candidates.push(LessonCandidate {
            topic,
            project: project.clone(),
            kind,
            source_refs: vec![format!("bead:{}", bead.id)],
            confidence,
            freshness_ms: bead.updated_ms,
            outdated,
            applies_to,
            redacted_summary: summary,
        });
    }

    for proof in &evidence.proofs {
        let (kind, topic) = classify_proof(proof);
        let (command, r1) = redact(&proof.command);
        redaction.add(r1);
        let status = proof.status.to_ascii_lowercase();
        let summary = match summary_from(&[&command]) {
            Some(cmd) => format!("{cmd} → {status}"),
            None => format!("{} → {status}", proof.name.trim()),
        };
        let confidence = if matches!(status.as_str(), "pass" | "ok" | "passed" | "green") {
            LessonConfidence::High
        } else {
            LessonConfidence::Medium
        };
        candidates.push(LessonCandidate {
            topic,
            project: project.clone(),
            kind,
            source_refs: vec![format!("proof:{}", proof.name)],
            confidence,
            freshness_ms: proof.timestamp_ms,
            outdated: false,
            applies_to: Vec::new(),
            redacted_summary: summary,
        });
    }

    let mut by_kind: BTreeMap<String, usize> = BTreeMap::new();
    for c in &candidates {
        *by_kind.entry(c.kind.as_str().to_string()).or_insert(0) += 1;
    }

    let manifest = ExtractionManifest {
        schema_version: LESSONS_EVIDENCE_SCHEMA_VERSION,
        project,
        commits_scanned: evidence.commits.len(),
        beads_scanned: evidence.beads.len(),
        proofs_scanned: evidence.proofs.len(),
        candidates_emitted: candidates.len(),
        by_kind,
        redaction,
    };

    ExtractionResult {
        candidates,
        manifest,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lessons::{LessonGraph, LessonStatus};

    fn commit(sha: &str, subject: &str, ts: u64) -> CommitEvidence {
        CommitEvidence {
            sha: sha.to_string(),
            subject: subject.to_string(),
            body: String::new(),
            timestamp_ms: ts,
        }
    }

    fn bead(id: &str, title: &str, reason: &str, itype: &str, ts: u64) -> BeadEvidence {
        BeadEvidence {
            id: id.to_string(),
            title: title.to_string(),
            close_reason: reason.to_string(),
            issue_type: itype.to_string(),
            status: "closed".to_string(),
            labels: Vec::new(),
            updated_ms: ts,
        }
    }

    // ---- redaction --------------------------------------------------------

    #[test]
    fn redact_strips_home_username_email_and_digest() {
        let input = "ran at /home/alice/projects/cass by alice@example.com hash 0123456789abcdef0123456789abcdef0123456789abcdef";
        let (out, report) = redact(input);
        assert!(!out.contains("alice"), "username must be gone: {out}");
        assert!(!out.contains("@example.com"), "email must be gone: {out}");
        assert!(out.contains("<home>/projects/cass"), "tail kept: {out}");
        assert!(out.contains("<email>"));
        assert!(out.contains("<digest>"));
        assert_eq!(report.home_paths, 1);
        assert_eq!(report.emails, 1);
        assert_eq!(report.digests, 1);
        assert_eq!(report.total(), 3);
    }

    #[test]
    fn redact_keeps_short_shas_and_normal_paths() {
        // Short shas are provenance, not sensitive; relative paths are useful.
        let input = "commit deadbeef touched src/lib.rs and Cargo.toml";
        let (out, report) = redact(input);
        assert_eq!(out, input, "nothing sensitive here");
        assert_eq!(report.total(), 0);
    }

    #[test]
    fn redact_preserves_macos_home_and_trailing_punct() {
        let (out, report) = redact("see /Users/bob/notes.md, ok?");
        assert!(out.contains("<home>/notes.md,"), "punct kept: {out}");
        assert!(!out.contains("bob"));
        assert_eq!(report.home_paths, 1);
    }

    // ---- classification ---------------------------------------------------

    #[test]
    fn commit_types_map_to_kinds_and_scope_is_topic() {
        assert_eq!(
            classify_commit(&commit("a", "feat(search): add hybrid fallback", 1)),
            (LessonKind::ReusableDecision, "search".to_string())
        );
        assert_eq!(
            classify_commit(&commit("b", "fix(indexer): avoid double saturating_sub", 1)),
            (LessonKind::Gotcha, "indexer".to_string())
        );
        assert_eq!(
            classify_commit(&commit("c", "revert(daemon): undo cache change", 1)),
            (LessonKind::FailedApproach, "daemon".to_string())
        );
    }

    #[test]
    fn security_keyword_overrides_commit_kind() {
        let (kind, _topic) = classify_commit(&commit(
            "d",
            "fix(update): validate version chars to prevent shell injection",
            1,
        ));
        assert_eq!(kind, LessonKind::SecurityWarning);
    }

    // ---- end-to-end extraction over the required corpus -------------------

    #[test]
    fn repeated_fix_dedupes_to_one_active_lesson() {
        // The same fix mined from a commit and the closing bead: same topic +
        // summary => same stable id => one lesson, merged provenance.
        let evidence = LessonsEvidence {
            project: "cass".to_string(),
            commits: vec![commit(
                "abc123",
                "fix(rch): preflight broken on remote",
                100,
            )],
            beads: vec![BeadEvidence {
                id: "bd-1".to_string(),
                title: "fix(rch): preflight broken on remote".to_string(),
                close_reason: String::new(),
                issue_type: "bug".to_string(),
                status: "closed".to_string(),
                labels: vec!["rch".to_string()],
                updated_ms: 200,
            }],
            proofs: Vec::new(),
        };
        let result = extract(&evidence);
        assert_eq!(result.manifest.candidates_emitted, 2);
        let graph = LessonGraph::build(result.candidates);
        assert_eq!(graph.summary.total, 1, "identical lessons dedupe");
        let l = &graph.lessons[0];
        assert!(l.source_refs.contains(&"commit:abc123".to_string()));
        assert!(l.source_refs.contains(&"bead:bd-1".to_string()));
        assert_eq!(l.freshness_ms, 200, "freshest metadata kept");
        assert_eq!(l.status, LessonStatus::Active);
    }

    #[test]
    fn failed_workaround_is_superseded_by_landed_decision() {
        let evidence = LessonsEvidence {
            project: "cass".to_string(),
            commits: vec![commit(
                "new99",
                "feat(frankensqlite): use SUM(0) in grouped query",
                300,
            )],
            beads: vec![bead(
                "bd-2",
                "frankensqlite group-by workaround",
                "abandoned: bare 0 in grouped query does not work, rolled back",
                "task",
                100,
            )],
            proofs: Vec::new(),
        };
        // Force both onto the same (topic, project) so supersession applies.
        let mut result = extract(&evidence);
        for c in &mut result.candidates {
            c.topic = "frankensqlite-group-by".to_string();
        }
        let graph = LessonGraph::build(result.candidates);
        assert_eq!(graph.summary.total, 2);
        assert_eq!(graph.summary.active, 1);
        assert_eq!(graph.summary.superseded, 1);
        let active: Vec<_> = graph.active().collect();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].kind, LessonKind::ReusableDecision);
        assert_eq!(active[0].freshness_ms, 300);
    }

    #[test]
    fn outdated_advice_is_marked_and_never_active() {
        let evidence = LessonsEvidence {
            project: "cass".to_string(),
            commits: Vec::new(),
            beads: vec![bead(
                "bd-3",
                "rch local patch override",
                "deprecated: local patch override no longer needed; superseded by git pin",
                "task",
                50,
            )],
            proofs: Vec::new(),
        };
        let result = extract(&evidence);
        let graph = LessonGraph::build(result.candidates);
        assert_eq!(graph.summary.outdated, 1);
        assert_eq!(graph.summary.active, 0);
        assert_eq!(graph.lessons[0].status, LessonStatus::Outdated);
    }

    #[test]
    fn security_warning_bead_is_high_confidence_security_kind() {
        let evidence = LessonsEvidence {
            project: "cass".to_string(),
            commits: Vec::new(),
            beads: vec![BeadEvidence {
                id: "bd-sec".to_string(),
                title: "shell injection in update_check".to_string(),
                close_reason: "validate version chars before interpolation".to_string(),
                issue_type: "bug".to_string(),
                status: "closed".to_string(),
                labels: vec!["security".to_string()],
                updated_ms: 400,
            }],
            proofs: Vec::new(),
        };
        let result = extract(&evidence);
        let graph = LessonGraph::build(result.candidates);
        let l = &graph.lessons[0];
        assert_eq!(l.kind, LessonKind::SecurityWarning);
        assert_eq!(l.confidence, LessonConfidence::High);
        assert_eq!(l.status, LessonStatus::Active);
    }

    #[test]
    fn high_confidence_landed_decision_is_active() {
        let evidence = LessonsEvidence {
            project: "cass".to_string(),
            commits: vec![commit(
                "feat1",
                "feat(storage): atomic-swap lexical publish via renameat2",
                500,
            )],
            beads: Vec::new(),
            proofs: Vec::new(),
        };
        let result = extract(&evidence);
        let graph = LessonGraph::build(result.candidates);
        let l = &graph.lessons[0];
        assert_eq!(l.kind, LessonKind::ReusableDecision);
        assert_eq!(l.confidence, LessonConfidence::High);
        assert_eq!(l.status, LessonStatus::Active);
        assert_eq!(l.topic, "storage");
    }

    #[test]
    fn proof_pass_is_invariant_fail_is_gotcha() {
        let evidence = LessonsEvidence {
            project: "cass".to_string(),
            commits: Vec::new(),
            beads: Vec::new(),
            proofs: vec![
                ProofEvidence {
                    name: "storage_fingerprint_gate".to_string(),
                    status: "pass".to_string(),
                    command: "cargo test --test e2e_storage".to_string(),
                    timestamp_ms: 10,
                },
                ProofEvidence {
                    name: "lexical_rebuild_gate".to_string(),
                    status: "timeout".to_string(),
                    command: "cargo test --lib".to_string(),
                    timestamp_ms: 20,
                },
            ],
        };
        let result = extract(&evidence);
        let graph = LessonGraph::build(result.candidates);
        let kinds: Vec<LessonKind> = graph.lessons.iter().map(|l| l.kind).collect();
        assert!(kinds.contains(&LessonKind::Invariant));
        assert!(kinds.contains(&LessonKind::Gotcha));
    }

    #[test]
    fn extraction_never_leaks_raw_home_or_email() {
        let evidence = LessonsEvidence {
            project: "cass".to_string(),
            commits: vec![CommitEvidence {
                sha: "leak1".to_string(),
                subject: "fix(export): handle path".to_string(),
                body: "reported by realuser@corp.example from /home/realuser/private/notes"
                    .to_string(),
                timestamp_ms: 1,
            }],
            beads: Vec::new(),
            proofs: Vec::new(),
        };
        let result = extract(&evidence);
        let redaction_total = result.manifest.redaction.total();
        // The serialized surface is the graph (carrying redacted summaries).
        let graph = LessonGraph::build(result.candidates);
        let json = serde_json::to_string(&graph).unwrap();
        assert!(!json.contains("realuser"), "username leaked: {json}");
        assert!(!json.contains("@corp.example"), "email leaked: {json}");
        assert!(redaction_total >= 2);
    }

    #[test]
    fn manifest_counts_and_by_kind_are_stable() {
        let evidence = LessonsEvidence {
            project: "cass".to_string(),
            commits: vec![
                commit("c1", "feat(a): one", 1),
                commit("c2", "fix(b): two", 2),
            ],
            beads: vec![bead("b1", "task three", "landed cleanly", "task", 3)],
            proofs: vec![ProofEvidence {
                name: "gate".to_string(),
                status: "pass".to_string(),
                command: "cargo test".to_string(),
                timestamp_ms: 4,
            }],
        };
        let result = extract(&evidence);
        assert_eq!(result.manifest.commits_scanned, 2);
        assert_eq!(result.manifest.beads_scanned, 1);
        assert_eq!(result.manifest.proofs_scanned, 1);
        assert_eq!(result.manifest.candidates_emitted, 4);
        // by_kind is a BTreeMap => alphabetical, deterministic.
        let keys: Vec<&String> = result.manifest.by_kind.keys().collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted);
        // Round-trips.
        let value = serde_json::to_value(&result.manifest).unwrap();
        let back: ExtractionManifest = serde_json::from_value(value).unwrap();
        assert_eq!(back, result.manifest);
    }

    #[test]
    fn empty_evidence_yields_empty_result() {
        let result = extract(&LessonsEvidence::default());
        assert_eq!(result.manifest.candidates_emitted, 0);
        assert!(result.candidates.is_empty());
        assert_eq!(result.manifest.project, "cass");
    }
}
