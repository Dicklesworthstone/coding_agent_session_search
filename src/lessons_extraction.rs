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
//! Every free-text field that reaches a serialized lesson first passes through
//! [`redact`], which removes credential patterns, home-directory paths (the part
//! that reveals a username), e-mail addresses, and long opaque digests. The
//! [`RedactionReport`] counts what was removed so a reviewer can audit the pass.
//! Provenance flows into `source_refs`, but untrusted bead/proof identifiers are
//! redacted there too; only validated hexadecimal commit ids are preserved
//! verbatim.
//!
//! ## Pure and deterministic
//!
//! Callers supply already-loaded evidence ([`LessonsEvidence`]); this module
//! does no I/O. The same evidence always yields the same candidates, manifest,
//! and redaction report, so the output is golden-stable and safe to test
//! against a checked-in fixture corpus.

use std::collections::BTreeMap;

use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::lessons::{LessonCandidate, LessonConfidence, LessonKind};

/// Stable schema version for the evidence wire format consumed here.
pub const LESSONS_EVIDENCE_SCHEMA_VERSION: u32 = 2;

/// Local home-directory prefixes, including prefixes embedded in metadata
/// such as `cwd=/Users/alice/project` or `file:///home/alice/project`.
static HOME_PATH_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:/(?:home|users)/[a-z0-9._-]+|[a-z]:[\\/]users[\\/][a-z0-9._-]+)")
        .expect("durable lesson home-path regex")
});

/// Practical e-mail matcher for redaction. It intentionally favors privacy
/// over full RFC mailbox validation and also finds addresses after prefixes
/// such as `owner=` and `mailto:`.
///
/// `=` remains in the local-part class because it is valid mailbox content.
/// [`replace_emails_counted`] distinguishes recognized metadata prefixes from
/// real local parts so `owner=alice@example.com` keeps `owner=`, while
/// `alice=tag@example.com` is redacted in full.
static EMAIL_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)[a-z0-9.!#$%&'*+/=?^_`{|}~-]+@[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?(?:\.[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?)+",
    )
    .expect("durable lesson email regex")
});

const EMAIL_METADATA_PREFIXES: &[&str] = &["author", "contact", "email", "maintainer", "owner"];

/// Long hex material which may be a content digest, secret, or opaque local
/// identifier. Short git shas remain useful and are deliberately retained.
static OPAQUE_HEX_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\b[a-f0-9]{32,}\b").expect("durable lesson opaque-hex regex"));

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
        self.home_paths
            .saturating_add(self.emails)
            .saturating_add(self.digests)
    }

    fn add(&mut self, other: RedactionReport) {
        self.home_paths = self.home_paths.saturating_add(other.home_paths);
        self.emails = self.emails.saturating_add(other.emails);
        self.digests = self.digests.saturating_add(other.digests);
    }
}

fn replace_all_counted(
    input: String,
    pattern: &Regex,
    replacement: &str,
    counter: &mut usize,
) -> String {
    let replacements = pattern.find_iter(&input).count();
    if replacements == 0 {
        return input;
    }
    *counter = counter.saturating_add(replacements);
    pattern.replace_all(&input, replacement).into_owned()
}

fn replace_emails_counted(input: String, counter: &mut usize) -> String {
    let replacements = EMAIL_RE.find_iter(&input).count();
    if replacements == 0 {
        return input;
    }
    *counter = counter.saturating_add(replacements);
    EMAIL_RE
        .replace_all(&input, |captures: &regex::Captures<'_>| {
            let matched = captures.get(0).map_or("", |value| value.as_str());
            let local_part = matched.split_once('@').map_or(matched, |(local, _)| local);
            let metadata_prefix = local_part.split_once('=').and_then(|(prefix, _)| {
                EMAIL_METADATA_PREFIXES
                    .iter()
                    .any(|candidate| prefix.eq_ignore_ascii_case(candidate))
                    .then_some(prefix)
            });
            metadata_prefix.map_or_else(
                || "<email>".to_string(),
                |prefix| format!("{prefix}=<email>"),
            )
        })
        .into_owned()
}

/// Redact a single text field: removes home-path usernames, e-mails, and opaque
/// digests, preserving the original whitespace layout. Returns the redacted
/// string and a per-class [`RedactionReport`].
pub fn redact(input: &str) -> (String, RedactionReport) {
    let mut report = RedactionReport::default();
    let secret_redacted = crate::indexer::redact_secrets::redact_text(input);
    if secret_redacted.as_ref() != input {
        let before = input.matches("[REDACTED]").count();
        let after = secret_redacted.matches("[REDACTED]").count();
        report.digests = report
            .digests
            .saturating_add(after.saturating_sub(before).max(1));
    }
    let output = secret_redacted.into_owned();
    let output = replace_all_counted(output, &HOME_PATH_RE, "<home>", &mut report.home_paths);
    let output = replace_emails_counted(output, &mut report.emails);
    let output = replace_all_counted(output, &OPAQUE_HEX_RE, "<digest>", &mut report.digests);
    (output, report)
}

/// Security-relevant keywords that override the default classification.
const SECURITY_KEYWORDS: &[&str] = &[
    "security",
    "vuln",
    "injection",
    "exploit",
    "sandbox escape",
    "privilege escalation",
    "unsafe",
];

/// Short security acronyms must match as standalone ASCII words. Raw substring
/// matching would, for example, classify every mention of `source` as RCE.
const SECURITY_ACRONYMS: &[&str] = &["cve", "rce", "xss", "csrf", "ssrf"];

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

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    let lower = haystack.to_ascii_lowercase();
    needles.iter().any(|n| lower.contains(n))
}

fn contains_ascii_word(haystack: &str, needle: &str) -> bool {
    let lower = haystack.to_ascii_lowercase();
    lower.match_indices(needle).any(|(start, matched)| {
        let end = start + matched.len();
        let before_is_word = start > 0 && lower.as_bytes()[start - 1].is_ascii_alphanumeric();
        let after_is_word = end < lower.len() && lower.as_bytes()[end].is_ascii_alphanumeric();
        !before_is_word && !after_is_word
    })
}

fn contains_security_keyword(haystack: &str) -> bool {
    contains_any(haystack, SECURITY_KEYWORDS)
        || SECURITY_ACRONYMS
            .iter()
            .any(|acronym| contains_ascii_word(haystack, acronym))
}

/// Whether a closing reason explicitly retires the underlying advice.
///
/// Topical words such as "stale" and "deprecated" commonly describe the bug
/// that a successful fix removes. Looking only for explicit retirement language
/// in the close reason avoids turning those current fixes into outdated lessons.
fn bead_marks_advice_outdated(close_reason: &str) -> bool {
    let reason = close_reason
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    const RETIREMENT_PREFIXES: &[&str] = &[
        "superseded:",
        "superseded by ",
        "replaced by ",
        "replaced by:",
        "outdated:",
        "deprecated:",
        "obsolete:",
        "retired in favor of ",
        "retired in favor:",
    ];
    const RETIREMENT_PHRASES: &[&str] = &[
        "no longer applies",
        "no longer needed",
        "no longer supported",
        "no longer valid",
    ];
    RETIREMENT_PREFIXES
        .iter()
        .any(|prefix| reason.starts_with(prefix))
        || RETIREMENT_PHRASES
            .iter()
            .any(|phrase| reason.contains(phrase))
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
    let word = text
        .split_whitespace()
        .next()
        .unwrap_or("general")
        // Preserve path separators until the redaction pass sees them. Removing
        // a leading slash here would turn `/Users/alice/...` into an
        // unrecognizable relative token before the home-path scrubber runs.
        .trim_matches(|c: char| !c.is_alphanumeric() && !matches!(c, '/' | '\\'))
        .to_ascii_lowercase();
    if word.is_empty() {
        "general".to_string()
    } else {
        word
    }
}

/// Classify a commit into a [`LessonKind`] and a topic.
fn classify_commit(commit: &CommitEvidence) -> (LessonKind, String) {
    let combined = format!("{} {}", commit.subject, commit.body);
    let (ctype, scope) = parse_conventional(&commit.subject);
    let topic = scope.unwrap_or_else(|| first_word(&commit.subject));
    if contains_security_keyword(&combined) {
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
        .iter()
        .map(|label| label.trim())
        .find(|label| !label.is_empty())
        .map(str::to_ascii_lowercase)
        .unwrap_or_else(|| first_word(&bead.title));
    let outdated = bead_marks_advice_outdated(&bead.close_reason);
    let kind = if contains_security_keyword(&combined) {
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
        proof.status.trim().to_ascii_lowercase().as_str(),
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
        .map(|part| part.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|part| !part.is_empty())
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

/// Malformed live JSONL records omitted before extraction.
///
/// These counts stay separate from `*_scanned`: scanned counts describe the
/// normalized records handed to [`extract`], while rejected counts make a
/// partial live intake machine-visible without serializing raw input.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RejectedEvidenceRecords {
    /// Malformed records rejected from `.beads/issues.jsonl`.
    pub beads: usize,
    /// Malformed records rejected from the repository proof manifest.
    pub proofs: usize,
}

impl RejectedEvidenceRecords {
    /// Total malformed records omitted across supported live JSONL sources.
    #[must_use]
    pub fn total(&self) -> usize {
        self.beads.saturating_add(self.proofs)
    }
}

/// 98anf.1: how one live evidence source was read. Rejected-record counts
/// only describe malformed lines inside a *successfully decoded* source, so
/// they can never stand in for source completeness: a missing, unreadable,
/// or invalid-UTF-8 source used to look identical to a readable empty one.
/// Raw paths and OS error strings are deliberately not carried.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceSourceReadStatus {
    /// The source file was opened and decoded as UTF-8 (possibly empty).
    Read,
    /// The source file does not exist. Optional evidence; never fatal.
    #[default]
    Missing,
    /// The source exists but could not be read or decoded (permissions,
    /// I/O failure, invalid UTF-8). The intake is incomplete.
    Unreadable,
    /// Fixture mode: evidence came from an explicit fixture file, so the
    /// live sources were intentionally not consulted.
    Fixture,
}

/// Per-source read status for the live intake.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceSourceStatus {
    /// `.beads/issues.jsonl` (closed Beads).
    pub beads: EvidenceSourceReadStatus,
    /// `.cass/proofs/proof-manifest.jsonl` (proof runs).
    pub proofs: EvidenceSourceReadStatus,
}

impl EvidenceSourceStatus {
    /// Fixture-mode status for both sources.
    #[must_use]
    pub const fn fixture() -> Self {
        Self {
            beads: EvidenceSourceReadStatus::Fixture,
            proofs: EvidenceSourceReadStatus::Fixture,
        }
    }

    /// True when no source was unreadable. A missing source is complete
    /// (there was nothing to read); an unreadable one is not.
    #[must_use]
    pub fn intake_complete(&self) -> bool {
        !matches!(self.beads, EvidenceSourceReadStatus::Unreadable)
            && !matches!(self.proofs, EvidenceSourceReadStatus::Unreadable)
    }

    /// Bounded, raw-free labels of the unreadable sources.
    #[must_use]
    pub fn unreadable_sources(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if matches!(self.beads, EvidenceSourceReadStatus::Unreadable) {
            out.push("beads");
        }
        if matches!(self.proofs, EvidenceSourceReadStatus::Unreadable) {
            out.push("proofs");
        }
        out
    }
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
    /// Malformed live JSONL records rejected before extraction.
    ///
    /// A non-zero count means the resulting lesson set is partial. Fixture
    /// inputs use zero because malformed fixture JSON fails before extraction.
    pub rejected_records: RejectedEvidenceRecords,
    /// 98anf.1: per-source read status of the live intake (fixture mode is
    /// explicit). Zero rejected records with an `unreadable` source is NOT a
    /// complete intake.
    #[serde(default)]
    pub source_status: EvidenceSourceStatus,
    /// Whether every consulted source was readable (`false` iff any source
    /// is `unreadable`).
    #[serde(default)]
    pub intake_complete: bool,
    /// Candidates emitted (before dedup in the graph).
    pub candidates_emitted: usize,
    /// Candidate count by [`LessonKind`] wire label (deterministic order).
    pub by_kind: BTreeMap<String, usize>,
    /// Redactions performed across all summaries.
    pub redaction: RedactionReport,
}

fn redact_field(input: &str, report: &mut RedactionReport) -> String {
    let normalized = input.split_whitespace().collect::<Vec<_>>().join(" ");
    let (redacted, field_report) = redact(&normalized);
    report.add(field_report);
    redacted
}

fn commit_source_ref(sha: &str, report: &mut RedactionReport) -> String {
    let trimmed = sha.trim();
    if matches!(trimmed.len(), 40 | 64) && trimmed.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return format!("commit:{trimmed}");
    }

    // A malformed commit id is neither useful provenance nor safe metadata.
    // Scrub it once to account for any recognized secret/PII; if it was merely
    // an arbitrary opaque value, account for that rejection as a digest-class
    // redaction. Never serialize the unvalidated source bytes.
    let (_, field_report) = redact(trimmed);
    if field_report.total() == 0 && !trimmed.is_empty() {
        report.digests = report.digests.saturating_add(1);
    } else {
        report.add(field_report);
    }
    "commit:<invalid-id>".to_string()
}

/// Extract redacted [`LessonCandidate`]s from `evidence`. Pure and
/// deterministic: no I/O, stable ordering, identical output for identical input.
pub fn extract(evidence: &LessonsEvidence) -> ExtractionResult {
    let raw_project = if evidence.project.trim().is_empty() {
        default_project()
    } else {
        evidence.project.trim().to_string()
    };
    let mut candidates: Vec<LessonCandidate> = Vec::new();
    let mut redaction = RedactionReport::default();
    let project = redact_field(&raw_project, &mut redaction);

    for commit in &evidence.commits {
        let (kind, raw_topic) = classify_commit(commit);
        let topic = redact_field(&raw_topic, &mut redaction);
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
            source_refs: vec![commit_source_ref(&commit.sha, &mut redaction)],
            confidence: LessonConfidence::High,
            freshness_ms: commit.timestamp_ms,
            outdated: false,
            applies_to: Vec::new(),
            redacted_summary: summary,
        });
    }

    for bead in &evidence.beads {
        // An empty id cannot provide useful or attributable provenance. Treat
        // fixture input the same as live Beads gathering and skip it entirely.
        if bead.id.trim().is_empty() {
            continue;
        }
        let (kind, raw_topic, outdated) = classify_bead(bead);
        let topic = redact_field(&raw_topic, &mut redaction);
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
        let mut applies_to: Vec<String> = bead
            .labels
            .iter()
            .map(|label| redact_field(&label.trim().to_ascii_lowercase(), &mut redaction))
            .filter(|label| !label.is_empty())
            .collect();
        applies_to.sort();
        applies_to.dedup();
        let bead_id = redact_field(bead.id.trim(), &mut redaction);
        candidates.push(LessonCandidate {
            topic,
            project: project.clone(),
            kind,
            source_refs: vec![format!("bead:{bead_id}")],
            confidence,
            freshness_ms: bead.updated_ms,
            outdated,
            applies_to,
            redacted_summary: summary,
        });
    }

    for proof in &evidence.proofs {
        let (kind, raw_topic) = classify_proof(proof);
        let topic = redact_field(&raw_topic, &mut redaction);
        let proof_name = redact_field(proof.name.trim(), &mut redaction);
        if proof_name.is_empty() {
            continue;
        }
        let (command, r1) = redact(&proof.command);
        redaction.add(r1);
        let normalized_status = proof.status.trim().to_ascii_lowercase();
        let mut status = redact_field(&normalized_status, &mut redaction);
        if status.is_empty() {
            status = "unknown".to_string();
        }
        let summary = match summary_from(&[&command]) {
            Some(cmd) => format!("{cmd} → {status}"),
            None => format!("{proof_name} → {status}"),
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
            source_refs: vec![format!("proof:{proof_name}")],
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
        rejected_records: RejectedEvidenceRecords::default(),
        source_status: EvidenceSourceStatus::default(),
        intake_complete: true,
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

    const SHA1_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const SHA1_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const SHA1_C: &str = "cccccccccccccccccccccccccccccccccccccccc";
    const SHA1_D: &str = "dddddddddddddddddddddddddddddddddddddddd";
    const SHA256_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

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

    #[test]
    fn redact_strips_windows_home_username() {
        let (out, report) = redact(r"see C:\Users\bob\notes.md");
        assert_eq!(out, r"see <home>\notes.md");
        assert!(!out.contains("bob"));
        assert_eq!(report.home_paths, 1);
    }

    #[test]
    fn redact_finds_sensitive_substrings_inside_metadata_tokens() {
        let input = concat!(
            "cwd=file:///Users/alice/private ",
            "windows=C:/Users/bob/private ",
            "owner=alice@example.com ",
            "digest=0123456789abcdef0123456789abcdef"
        );
        let (out, report) = redact(input);
        assert_eq!(
            out,
            "cwd=file://<home>/private windows=<home>/private owner=<email> digest=<digest>"
        );
        assert_eq!(report.home_paths, 2);
        assert_eq!(report.emails, 1);
        assert_eq!(report.digests, 1);
    }

    #[test]
    fn redact_removes_equals_in_email_local_parts_without_losing_known_metadata_keys() {
        let input = concat!(
            "direct=alice=tag@example.com ",
            "uri=mailto:ops=alerts@example.com ",
            "owner=alice=tag@example.com"
        );
        let (out, report) = redact(input);
        assert_eq!(
            out, "<email> uri=mailto:<email> owner=<email>",
            "valid '=' characters inside mailbox local parts must not leak partial identities"
        );
        assert_eq!(report.emails, 3);
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
        assert_eq!(first_word("!!!"), "general");
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

    #[test]
    fn short_security_acronyms_require_word_boundaries() {
        let (source_kind, _) =
            classify_commit(&commit(SHA1_A, "fix(indexer): preserve source metadata", 1));
        assert_eq!(
            source_kind,
            LessonKind::Gotcha,
            "the `rce` substring inside `source` is not an RCE warning"
        );

        let (rce_kind, _) =
            classify_commit(&commit(SHA1_B, "fix(runtime): reject RCE payloads", 1));
        assert_eq!(rce_kind, LessonKind::SecurityWarning);
    }

    #[test]
    fn outdated_requires_explicit_retirement_language_in_close_reason() {
        let current = bead(
            "bd-current",
            "Remove stale deprecated cache entries",
            "fixed stale invalidation in the deprecated cache API and verified the current implementation",
            "bug",
            1,
        );
        assert!(
            !classify_bead(&current).2,
            "a successful fix for stale state is still current advice"
        );

        let retired = bead(
            "bd-retired",
            "Local patch override",
            "deprecated: no longer needed after the upstream release",
            "task",
            2,
        );
        assert!(classify_bead(&retired).2);
    }

    #[test]
    fn commit_source_refs_require_full_git_object_ids() {
        let mut report = RedactionReport::default();
        assert_eq!(
            commit_source_ref(SHA1_A, &mut report),
            format!("commit:{SHA1_A}")
        );
        assert_eq!(
            commit_source_ref(SHA256_A, &mut report),
            format!("commit:{SHA256_A}")
        );
        assert_eq!(report.total(), 0);

        for invalid in [
            "abc123",
            "0123456789abcdef0123456789abcdef",
            "0123456789abcdef0123456789abcdef0123456789abcdef",
        ] {
            let source_ref = commit_source_ref(invalid, &mut report);
            assert_eq!(source_ref, "commit:<invalid-id>");
            assert!(!source_ref.contains(invalid));
        }
        assert_eq!(report.digests, 3);
    }

    // ---- end-to-end extraction over the required corpus -------------------

    #[test]
    fn repeated_fix_dedupes_to_one_active_lesson() {
        // The same fix mined from a commit and the closing bead: same topic +
        // summary => same stable id => one lesson, merged provenance.
        let evidence = LessonsEvidence {
            project: "cass".to_string(),
            commits: vec![commit(SHA1_A, "fix(rch): preflight broken on remote", 100)],
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
        assert!(l.source_refs.contains(&format!("commit:{SHA1_A}")));
        assert!(l.source_refs.contains(&"bead:bd-1".to_string()));
        assert_eq!(l.freshness_ms, 200, "freshest metadata kept");
        assert_eq!(l.status, LessonStatus::Active);
    }

    #[test]
    fn failed_workaround_is_superseded_by_landed_decision() {
        let evidence = LessonsEvidence {
            project: "cass".to_string(),
            commits: vec![commit(
                SHA1_B,
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
                SHA1_C,
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
                sha: SHA1_D.to_string(),
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
    fn every_serialized_metadata_field_crosses_the_redaction_boundary() {
        let secret_status = format!("sk-proj-{}", "A".repeat(24));
        let evidence = LessonsEvidence {
            project: "repo=file:///Users/project-owner/private-repo".to_string(),
            commits: vec![CommitEvidence {
                sha: SHA1_A.to_string(),
                subject: "fix(cwd=/Users/commit-owner/private): keep metadata safe".to_string(),
                body: String::new(),
                timestamp_ms: 1,
            }],
            beads: vec![BeadEvidence {
                id: "owner=bead-owner@example.com".to_string(),
                title: "metadata boundary".to_string(),
                close_reason: "landed".to_string(),
                issue_type: "task".to_string(),
                status: "closed".to_string(),
                labels: vec!["path=/Users/label-owner/private".to_string()],
                updated_ms: 2,
            }],
            proofs: vec![ProofEvidence {
                name: "file:///Users/proof-owner/private-gate".to_string(),
                status: secret_status.clone(),
                command: String::new(),
                timestamp_ms: 3,
            }],
        };

        let result = extract(&evidence);
        let redaction_total = result.manifest.redaction.total();
        let graph = LessonGraph::build(result.candidates);
        let json = serde_json::to_string(&graph).unwrap();
        for sensitive in [
            "project-owner",
            "commit-owner",
            "bead-owner@example.com",
            "label-owner",
            "proof-owner",
            secret_status.as_str(),
        ] {
            assert!(
                !json.contains(sensitive),
                "metadata leaked {sensitive}: {json}"
            );
        }
        assert!(
            json.contains(&format!("commit:{SHA1_A}")),
            "validated commit id lost: {json}"
        );
        assert!(
            redaction_total >= 6,
            "redactions were not audited: {redaction_total}"
        );
    }

    #[test]
    fn malformed_commit_identifier_is_not_serialized_as_provenance() {
        let raw_id = "0123456789abcdef0123456789abcdef";
        let evidence = LessonsEvidence {
            project: "cass".to_string(),
            commits: vec![commit(raw_id, "fix(storage): retain safe provenance", 1)],
            beads: Vec::new(),
            proofs: Vec::new(),
        };

        let result = extract(&evidence);
        assert_eq!(result.manifest.redaction.digests, 1);
        let graph = LessonGraph::build(result.candidates);
        let json = serde_json::to_string(&graph).unwrap();
        assert!(!json.contains(raw_id), "malformed id leaked: {json}");
        assert!(json.contains("commit:<invalid-id>"));
    }

    #[test]
    fn bead_without_an_identifier_is_not_emitted() {
        let evidence = LessonsEvidence {
            project: "cass".to_string(),
            commits: Vec::new(),
            beads: vec![bead(
                " \n ",
                "fix(storage): retain a transaction boundary",
                "landed and verified",
                "bug",
                1,
            )],
            proofs: Vec::new(),
        };

        let result = extract(&evidence);
        assert_eq!(result.manifest.beads_scanned, 1);
        assert_eq!(result.manifest.candidates_emitted, 0);
        assert!(result.candidates.is_empty());
    }

    #[test]
    fn extraction_normalizes_metadata_and_skips_unattributed_proofs() {
        let evidence = LessonsEvidence {
            project: "  cass  ".to_string(),
            commits: Vec::new(),
            beads: vec![BeadEvidence {
                id: "  bead-1  ".to_string(),
                title: "storage lesson".to_string(),
                close_reason: "keep one\nlogical transaction".to_string(),
                issue_type: "task".to_string(),
                status: "closed".to_string(),
                labels: vec!["  ".to_string(), " Storage\n ".to_string()],
                updated_ms: 1,
            }],
            proofs: vec![
                ProofEvidence {
                    name: "  storage gate  ".to_string(),
                    status: " PASS \n".to_string(),
                    command: "cargo test\n--lib".to_string(),
                    timestamp_ms: 2,
                },
                ProofEvidence {
                    name: " \n ".to_string(),
                    status: "pass".to_string(),
                    command: "cargo test --ignored".to_string(),
                    timestamp_ms: 3,
                },
            ],
        };

        let result = extract(&evidence);
        assert_eq!(result.manifest.project, "cass");
        assert_eq!(result.manifest.proofs_scanned, 2);
        assert_eq!(result.manifest.candidates_emitted, 2);
        let graph = LessonGraph::build(result.candidates);
        assert!(
            graph
                .lessons
                .iter()
                .all(|lesson| !lesson.summary.contains('\n'))
        );
        let bead = graph
            .lessons
            .iter()
            .find(|lesson| {
                matches!(lesson.source_refs.as_slice(), [source] if source == "bead:bead-1")
            })
            .unwrap();
        assert_eq!(bead.topic, "storage");
        assert_eq!(bead.applies_to, vec!["storage".to_string()]);
        assert_eq!(bead.summary, "keep one logical transaction");
        let proof = graph
            .lessons
            .iter()
            .find(|lesson| {
                matches!(lesson.source_refs.as_slice(), [source] if source == "proof:storage gate")
            })
            .unwrap();
        assert_eq!(proof.kind, LessonKind::Invariant);
        assert_eq!(proof.confidence, LessonConfidence::High);
        assert_eq!(proof.summary, "cargo test --lib → pass");
    }

    #[test]
    fn manifest_counts_and_by_kind_are_stable() {
        let evidence = LessonsEvidence {
            project: "cass".to_string(),
            commits: vec![
                commit(SHA1_A, "feat(a): one", 1),
                commit(SHA1_B, "fix(b): two", 2),
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
        assert_eq!(result.manifest.schema_version, 2);
        assert_eq!(result.manifest.commits_scanned, 2);
        assert_eq!(result.manifest.beads_scanned, 1);
        assert_eq!(result.manifest.proofs_scanned, 1);
        assert_eq!(result.manifest.rejected_records.beads, 0);
        assert_eq!(result.manifest.rejected_records.proofs, 0);
        assert_eq!(result.manifest.rejected_records.total(), 0);
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
