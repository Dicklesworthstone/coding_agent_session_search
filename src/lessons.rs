//! Durable lessons graph: reusable decisions, failed approaches, recipes,
//! invariants, and gotchas mined from closed sessions.
//!
//! Bead: coding_agent_session_search-guided-ops-repro-trust-5u82n.4
//! ("Extract durable lessons and decisions from closed sessions").
//!
//! CASS should not only *find* conversations; it should make recurring
//! knowledge easy to reuse and hard to lose once session logs grow huge. This
//! module is the **metadata-first record contract and graph core**: it defines
//! the durable [`LessonRecord`], computes a content-stable [`LessonRecord::lesson_id`]
//! (so the same lesson dedupes across runs), and resolves supersession/staleness
//! within a topic — independent of *how* lessons are sourced. Failed approaches
//! are retired only by related, fresher reusable decisions on the same topic;
//! independent current lessons can coexist within a topic.
//!
//! ## Redaction trust boundary
//!
//! Every string-bearing [`LessonCandidate`] field is expected to come from the
//! reviewed extraction/redaction layer. This graph core does not inspect or
//! re-redact those strings; it preserves the supplied topic, project,
//! provenance, applicability, and summary metadata. Production callers must
//! therefore cross [`crate::lessons_extraction::extract`] before building a
//! graph. Keeping that boundary explicit avoids implying that the Rust type
//! alone can distinguish redacted text from raw private text.

use serde::{Deserialize, Serialize};

/// Stable schema version for the lessons wire format.
pub const LESSONS_SCHEMA_VERSION: u32 = 1;

/// The kind of durable knowledge a lesson captures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LessonKind {
    /// A decision that landed and should be reused.
    ReusableDecision,
    /// An approach that was tried and failed (don't repeat it).
    FailedApproach,
    /// A concrete command recipe that works.
    CommandRecipe,
    /// An invariant that must hold.
    Invariant,
    /// A non-obvious gotcha / footgun.
    Gotcha,
    /// A security-relevant warning.
    SecurityWarning,
}

impl LessonKind {
    /// Stable snake_case wire label.
    pub fn as_str(self) -> &'static str {
        match self {
            LessonKind::ReusableDecision => "reusable_decision",
            LessonKind::FailedApproach => "failed_approach",
            LessonKind::CommandRecipe => "command_recipe",
            LessonKind::Invariant => "invariant",
            LessonKind::Gotcha => "gotcha",
            LessonKind::SecurityWarning => "security_warning",
        }
    }
}

/// Confidence in a lesson's reliability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LessonConfidence {
    Low,
    Medium,
    High,
}

impl LessonConfidence {
    /// Stable snake_case wire label.
    pub fn as_str(self) -> &'static str {
        match self {
            LessonConfidence::Low => "low",
            LessonConfidence::Medium => "medium",
            LessonConfidence::High => "high",
        }
    }
}

/// Lifecycle status of a lesson within its topic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LessonStatus {
    /// Current, independently useful lesson.
    Active,
    /// Failed approach replaced by a related, fresher reusable decision.
    Superseded,
    /// Known to be out of date (advice no longer applies).
    Outdated,
}

impl LessonStatus {
    /// Stable snake_case wire label.
    pub fn as_str(self) -> &'static str {
        match self {
            LessonStatus::Active => "active",
            LessonStatus::Superseded => "superseded",
            LessonStatus::Outdated => "outdated",
        }
    }
}

/// A lesson candidate: metadata plus an already-redacted summary. The producer
/// (extraction/redaction layer) guarantees `redacted_summary` carries no raw
/// private text; this module stores exactly what it is given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LessonCandidate {
    /// Topic the lesson is about (used with `project` when retiring failed
    /// approaches).
    pub topic: String,
    /// Project the lesson belongs to.
    pub project: String,
    /// Kind of knowledge.
    pub kind: LessonKind,
    /// Where this lesson came from (bead id, commit sha, artifact path, …).
    pub source_refs: Vec<String>,
    /// Confidence.
    pub confidence: LessonConfidence,
    /// Freshness as an epoch-ms timestamp (caller-supplied for determinism).
    pub freshness_ms: u64,
    /// Whether the underlying advice is already known outdated.
    pub outdated: bool,
    /// Paths/areas this lesson applies to.
    pub applies_to: Vec<String>,
    /// Already-redacted, reviewable summary. NEVER raw session/prompt text.
    pub redacted_summary: String,
}

/// A durable, metadata-first lesson record. Carries no raw text — only the
/// redacted summary and provenance refs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LessonRecord {
    /// Mirrors [`LESSONS_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Content-stable id (same topic/project/kind/summary => same id across runs).
    pub lesson_id: String,
    /// Topic.
    pub topic: String,
    /// Project.
    pub project: String,
    /// Kind of knowledge.
    pub kind: LessonKind,
    /// Provenance references (deduplicated, sorted).
    pub source_refs: Vec<String>,
    /// Confidence.
    pub confidence: LessonConfidence,
    /// Freshness (epoch ms).
    pub freshness_ms: u64,
    /// Lifecycle status.
    pub status: LessonStatus,
    /// Paths/areas this lesson applies to.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub applies_to: Vec<String>,
    /// Redacted, reviewable summary.
    pub summary: String,
}

/// Compute the content-stable lesson id from the identity-bearing fields. Two
/// candidates with the same topic/project/kind/summary get the same id, so they
/// dedupe deterministically across runs. Freshness/source/confidence are
/// intentionally excluded so a re-mined lesson keeps its id.
pub fn stable_lesson_id(topic: &str, project: &str, kind: LessonKind, summary: &str) -> String {
    // Length-prefixed fields so concatenation is unambiguous.
    let material = format!(
        "v{LESSONS_SCHEMA_VERSION}|{}:{topic}|{}:{project}|{}|{}:{summary}",
        topic.len(),
        project.len(),
        kind.as_str(),
        summary.len(),
    );
    let hex = blake3::hash(material.as_bytes()).to_hex();
    format!("lsn-{}", &hex[..16])
}

fn significant_summary_tokens(summary: &str) -> std::collections::BTreeSet<String> {
    summary
        .split(|character: char| !character.is_ascii_alphanumeric())
        .map(str::to_ascii_lowercase)
        .filter(|token| {
            token.len() >= 4
                && token.bytes().any(|byte| byte.is_ascii_alphabetic())
                && !matches!(
                    token.as_str(),
                    "about"
                        | "after"
                        | "before"
                        | "change"
                        | "commit"
                        | "does"
                        | "digest"
                        | "email"
                        | "failed"
                        | "failure"
                        | "fixed"
                        | "fixes"
                        | "from"
                        | "have"
                        | "home"
                        | "into"
                        | "invalid"
                        | "issue"
                        | "landed"
                        | "lesson"
                        | "must"
                        | "proof"
                        | "redacted"
                        | "should"
                        | "that"
                        | "this"
                        | "using"
                        | "were"
                        | "when"
                        | "where"
                        | "which"
                        | "while"
                        | "will"
                        | "with"
                        | "without"
                        | "work"
                        | "works"
                )
        })
        .collect()
}

fn summaries_share_replacement_context(left: &str, right: &str) -> bool {
    let left = significant_summary_tokens(left);
    let right = significant_summary_tokens(right);
    left.intersection(&right).take(2).count() >= 2
}

impl LessonRecord {
    /// Build a record from a candidate, computing the stable id and normalizing
    /// provenance (deduped + sorted). Status starts `Outdated` if the candidate
    /// is flagged outdated, else `Active` (supersession is resolved later by
    /// [`LessonGraph::build`]). Stores only the candidate's redacted summary.
    pub fn from_candidate(candidate: LessonCandidate) -> Self {
        let lesson_id = stable_lesson_id(
            &candidate.topic,
            &candidate.project,
            candidate.kind,
            &candidate.redacted_summary,
        );
        let mut source_refs = candidate.source_refs;
        source_refs.sort();
        source_refs.dedup();
        let mut applies_to = candidate.applies_to;
        applies_to.sort();
        applies_to.dedup();
        LessonRecord {
            schema_version: LESSONS_SCHEMA_VERSION,
            lesson_id,
            topic: candidate.topic,
            project: candidate.project,
            kind: candidate.kind,
            source_refs,
            confidence: candidate.confidence,
            freshness_ms: candidate.freshness_ms,
            status: if candidate.outdated {
                LessonStatus::Outdated
            } else {
                LessonStatus::Active
            },
            applies_to,
            summary: candidate.redacted_summary,
        }
    }
}

/// Aggregate counts over a lessons graph.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LessonsSummary {
    /// Total distinct lessons after dedup.
    pub total: usize,
    /// Active, non-outdated lessons.
    pub active: usize,
    /// Failed approaches superseded by a related fresher reusable decision.
    pub superseded: usize,
    /// Explicitly outdated.
    pub outdated: usize,
}

/// The durable lessons graph: distinct lesson records plus a rollup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LessonGraph {
    /// Mirrors [`LESSONS_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Distinct lessons, sorted by lesson_id for stable output.
    pub lessons: Vec<LessonRecord>,
    /// Rollup.
    pub summary: LessonsSummary,
}

impl LessonGraph {
    /// Build a graph from candidates: dedupe by stable id (merging provenance
    /// refs and keeping the freshest metadata), then retire failed approaches
    /// that have a strictly fresher reusable decision on the same (topic,
    /// project) with at least two shared significant summary tokens. Independent
    /// non-outdated lessons remain `Active`. Pure and deterministic.
    pub fn build(candidates: Vec<LessonCandidate>) -> Self {
        use std::collections::BTreeMap;

        // 1) Dedupe by stable id; merge provenance, keep freshest metadata.
        let mut by_id: BTreeMap<String, LessonRecord> = BTreeMap::new();
        for candidate in candidates {
            let record = LessonRecord::from_candidate(candidate);
            by_id
                .entry(record.lesson_id.clone())
                .and_modify(|existing| {
                    existing
                        .source_refs
                        .extend(record.source_refs.iter().cloned());
                    existing.source_refs.sort();
                    existing.source_refs.dedup();
                    existing
                        .applies_to
                        .extend(record.applies_to.iter().cloned());
                    existing.applies_to.sort();
                    existing.applies_to.dedup();
                    if record.freshness_ms > existing.freshness_ms {
                        existing.freshness_ms = record.freshness_ms;
                    }
                    // Highest confidence wins.
                    if record.confidence > existing.confidence {
                        existing.confidence = record.confidence;
                    }
                    // An outdated flag on any copy sticks.
                    if record.status == LessonStatus::Outdated {
                        existing.status = LessonStatus::Outdated;
                    }
                })
                .or_insert(record);
        }

        let mut lessons: Vec<LessonRecord> = by_id.into_values().collect();

        // 2) Supersession is deliberately narrow. A topic is a coarse classifier,
        //    not an identity key, so distinct current lessons on one topic must
        //    coexist. Only a failed approach with a strictly fresher reusable
        //    decision and meaningful summary overlap is known to be replaced.
        let mut landed_decisions: BTreeMap<(String, String), Vec<(u64, String)>> = BTreeMap::new();
        for l in &lessons {
            if l.status == LessonStatus::Outdated || l.kind != LessonKind::ReusableDecision {
                continue;
            }
            let key = (l.topic.clone(), l.project.clone());
            landed_decisions
                .entry(key)
                .or_default()
                .push((l.freshness_ms, l.summary.clone()));
        }
        for l in &mut lessons {
            if l.status == LessonStatus::Outdated {
                continue;
            }
            let key = (l.topic.clone(), l.project.clone());
            let replaced_failed_approach = l.kind == LessonKind::FailedApproach
                && landed_decisions.get(&key).is_some_and(|decisions| {
                    decisions.iter().any(|(freshness, summary)| {
                        *freshness > l.freshness_ms
                            && summaries_share_replacement_context(&l.summary, summary)
                    })
                });
            l.status = if replaced_failed_approach {
                LessonStatus::Superseded
            } else {
                LessonStatus::Active
            };
        }

        lessons.sort_by(|a, b| a.lesson_id.cmp(&b.lesson_id));

        let mut summary = LessonsSummary {
            total: lessons.len(),
            ..Default::default()
        };
        for l in &lessons {
            match l.status {
                LessonStatus::Active => summary.active += 1,
                LessonStatus::Superseded => summary.superseded += 1,
                LessonStatus::Outdated => summary.outdated += 1,
            }
        }

        LessonGraph {
            schema_version: LESSONS_SCHEMA_VERSION,
            lessons,
            summary,
        }
    }

    /// Active lessons only (the current reusable knowledge).
    pub fn active(&self) -> impl Iterator<Item = &LessonRecord> {
        self.lessons
            .iter()
            .filter(|l| l.status == LessonStatus::Active)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(
        topic: &str,
        kind: LessonKind,
        conf: LessonConfidence,
        freshness_ms: u64,
        summary: &str,
        source: &str,
    ) -> LessonCandidate {
        LessonCandidate {
            topic: topic.to_string(),
            project: "cass".to_string(),
            kind,
            source_refs: vec![source.to_string()],
            confidence: conf,
            freshness_ms,
            outdated: false,
            applies_to: Vec::new(),
            redacted_summary: summary.to_string(),
        }
    }

    #[test]
    fn lesson_id_is_content_stable_and_distinguishing() {
        let a = stable_lesson_id("rch", "cass", LessonKind::Gotcha, "preflight broken");
        let b = stable_lesson_id("rch", "cass", LessonKind::Gotcha, "preflight broken");
        assert_eq!(a, b, "same content must yield the same id across runs");
        assert!(a.starts_with("lsn-"));
        // Any identity field change yields a different id.
        assert_ne!(
            a,
            stable_lesson_id("rch", "cass", LessonKind::Gotcha, "other")
        );
        assert_ne!(
            a,
            stable_lesson_id("rch", "cass", LessonKind::Invariant, "preflight broken")
        );
        assert_ne!(
            a,
            stable_lesson_id("other", "cass", LessonKind::Gotcha, "preflight broken")
        );
    }

    #[test]
    fn duplicate_candidates_dedupe_and_merge_provenance() {
        // Same lesson mined twice from different sources (a "repeated fix").
        let g = LessonGraph::build(vec![
            candidate(
                "commit-race",
                LessonKind::Gotcha,
                LessonConfidence::Medium,
                100,
                "use bare git commit on index",
                "bead-1",
            ),
            candidate(
                "commit-race",
                LessonKind::Gotcha,
                LessonConfidence::High,
                200,
                "use bare git commit on index",
                "commit-abc",
            ),
        ]);
        assert_eq!(g.summary.total, 1, "identical lessons must dedupe");
        let l = &g.lessons[0];
        assert_eq!(
            l.source_refs,
            vec!["bead-1".to_string(), "commit-abc".to_string()]
        );
        assert_eq!(l.freshness_ms, 200, "freshest metadata kept");
        assert_eq!(
            l.confidence,
            LessonConfidence::High,
            "highest confidence wins"
        );
        assert_eq!(l.status, LessonStatus::Active);
    }

    #[test]
    fn fresher_landed_lesson_supersedes_failed_approach_on_same_topic() {
        // A "failed workaround" replaced by a "high-confidence landed decision".
        let g = LessonGraph::build(vec![
            candidate(
                "frankensqlite-group-by",
                LessonKind::FailedApproach,
                LessonConfidence::Low,
                100,
                "tried bare 0 in grouped query",
                "old",
            ),
            candidate(
                "frankensqlite-group-by",
                LessonKind::ReusableDecision,
                LessonConfidence::High,
                300,
                "use SUM(0) in grouped query",
                "new",
            ),
        ]);
        assert_eq!(g.summary.total, 2);
        assert_eq!(g.summary.active, 1);
        assert_eq!(g.summary.superseded, 1);
        let active: Vec<&LessonRecord> = g.active().collect();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].freshness_ms, 300);
        assert_eq!(active[0].kind, LessonKind::ReusableDecision);
    }

    #[test]
    fn independent_lessons_on_same_topic_remain_active() {
        let g = LessonGraph::build(vec![
            candidate(
                "search",
                LessonKind::Gotcha,
                LessonConfidence::High,
                100,
                "cursor pagination must preserve the stable tiebreak",
                "commit-cursor",
            ),
            candidate(
                "search",
                LessonKind::ReusableDecision,
                LessonConfidence::High,
                300,
                "hybrid search fails open to lexical",
                "commit-hybrid",
            ),
        ]);

        assert_eq!(g.summary.total, 2);
        assert_eq!(g.summary.active, 2);
        assert_eq!(g.summary.superseded, 0);
        assert!(
            g.lessons
                .iter()
                .all(|lesson| lesson.status == LessonStatus::Active)
        );
    }

    #[test]
    fn newer_gotcha_does_not_supersede_failed_approach() {
        let g = LessonGraph::build(vec![
            candidate(
                "search",
                LessonKind::FailedApproach,
                LessonConfidence::High,
                100,
                "abandoned an unrelated remote cache experiment",
                "bead-cache",
            ),
            candidate(
                "search",
                LessonKind::Gotcha,
                LessonConfidence::High,
                300,
                "cursor pagination must preserve the stable tiebreak",
                "commit-cursor",
            ),
        ]);

        assert_eq!(g.summary.active, 2);
        assert_eq!(g.summary.superseded, 0);
    }

    #[test]
    fn one_shared_topical_word_does_not_supersede_unrelated_failed_approach() {
        let g = LessonGraph::build(vec![
            candidate(
                "search",
                LessonKind::FailedApproach,
                LessonConfidence::High,
                100,
                "abandoned a search cache experiment after corruption",
                "bead-cache",
            ),
            candidate(
                "search",
                LessonKind::ReusableDecision,
                LessonConfidence::High,
                300,
                "hybrid search fails open to lexical",
                "commit-hybrid",
            ),
        ]);

        assert_eq!(g.summary.active, 2);
        assert_eq!(g.summary.superseded, 0);
    }

    #[test]
    fn redaction_placeholders_do_not_create_supersession_overlap() {
        assert!(!summaries_share_replacement_context(
            "failed at <home> for <email>",
            "decision at <home> for <email>"
        ));
    }

    #[test]
    fn outdated_advice_is_marked_and_never_active() {
        // "outdated advice" in the corpus.
        let mut c = candidate(
            "rch-local-patch",
            LessonKind::CommandRecipe,
            LessonConfidence::Medium,
            50,
            "use local patch override",
            "old-doc",
        );
        c.outdated = true;
        let g = LessonGraph::build(vec![c]);
        assert_eq!(g.summary.outdated, 1);
        assert_eq!(g.summary.active, 0);
        assert_eq!(g.lessons[0].status, LessonStatus::Outdated);
        // Outdated lessons do not participate in supersession/active selection.
        assert_eq!(g.active().count(), 0);
    }

    #[test]
    fn security_warning_high_confidence_is_preserved() {
        let g = LessonGraph::build(vec![candidate(
            "shell-injection",
            LessonKind::SecurityWarning,
            LessonConfidence::High,
            400,
            "validate version chars before interpolation",
            "bead-sec",
        )]);
        let l = &g.lessons[0];
        assert_eq!(l.kind, LessonKind::SecurityWarning);
        assert_eq!(l.confidence, LessonConfidence::High);
        assert_eq!(l.status, LessonStatus::Active);
    }

    #[test]
    fn record_serializes_only_the_explicit_candidate_metadata() {
        // Redaction is the caller's contract. The graph neither invents a hidden
        // raw-text field nor claims to sanitize the explicit metadata itself.
        let mut c = candidate(
            "topic",
            LessonKind::Gotcha,
            LessonConfidence::Low,
            1,
            "REDACTED-SUMMARY",
            "ref",
        );
        c.applies_to.push("src/storage".to_string());
        let rec = LessonRecord::from_candidate(c);
        assert_eq!(rec.summary, "REDACTED-SUMMARY");
        let value = serde_json::to_value(&rec).unwrap();
        assert_eq!(value["summary"], "REDACTED-SUMMARY");
        let mut keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "applies_to",
                "confidence",
                "freshness_ms",
                "kind",
                "lesson_id",
                "project",
                "schema_version",
                "source_refs",
                "status",
                "summary",
                "topic",
            ]
        );
    }

    #[test]
    fn duplicate_candidates_union_applicability_independent_of_input_order() {
        let mut first = candidate(
            "storage",
            LessonKind::Gotcha,
            LessonConfidence::Medium,
            100,
            "keep one logical transaction",
            "bead-1",
        );
        first.applies_to = vec!["src/storage".to_string(), "src/indexer".to_string()];
        let mut second = candidate(
            "storage",
            LessonKind::Gotcha,
            LessonConfidence::High,
            200,
            "keep one logical transaction",
            "commit-abc",
        );
        second.applies_to = vec!["src/storage".to_string(), "tests/storage".to_string()];

        let forward = LessonGraph::build(vec![first.clone(), second.clone()]);
        let reverse = LessonGraph::build(vec![second, first]);
        assert_eq!(
            forward, reverse,
            "candidate order must not change the graph"
        );
        assert_eq!(
            forward.lessons[0].applies_to,
            vec![
                "src/indexer".to_string(),
                "src/storage".to_string(),
                "tests/storage".to_string(),
            ]
        );
    }

    #[test]
    fn graph_json_contract_is_stable_and_round_trips() {
        let g = LessonGraph::build(vec![
            candidate(
                "a",
                LessonKind::Invariant,
                LessonConfidence::High,
                10,
                "x",
                "r1",
            ),
            candidate(
                "b",
                LessonKind::Gotcha,
                LessonConfidence::Low,
                20,
                "y",
                "r2",
            ),
        ]);
        let value = serde_json::to_value(&g).unwrap();
        assert_eq!(value["schema_version"], LESSONS_SCHEMA_VERSION);
        assert_eq!(value["summary"]["total"], 2);
        assert!(value["lessons"][0]["kind"].as_str().is_some());
        let back: LessonGraph = serde_json::from_value(value).unwrap();
        assert_eq!(back, g);
        // Stable ordering: lessons are sorted by lesson_id.
        let ids: Vec<&str> = g.lessons.iter().map(|l| l.lesson_id.as_str()).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted);
    }
}
