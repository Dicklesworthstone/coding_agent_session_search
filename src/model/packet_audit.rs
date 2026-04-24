//! ConversationPacket equivalence audit (bead `coding_agent_session_search-ibuuh.32`).
//!
//! The packet contract promises that the live persist path and the rebuild
//! path can both feed downstream sinks from the same canonical projections.
//! Until every sink consumes the projections directly, we want a non-invasive
//! way to *prove* the canonical persist sink is producing packet-equivalent
//! output for what the source-of-truth raw scan would have produced. This
//! module is that compare-mode hook.
//!
//! Two packets compared by [`PacketEquivalenceAuditor::audit_pair`] are
//! considered equivalent when their projections agree byte-for-byte and
//! their hashes either match or are explicitly excused by a documented
//! tolerance (e.g. secret redaction is enabled, so the canonical content
//! string differs from the raw content string and `semantic_hash` is
//! expected to drift while `analytics`/`lexical`/`semantic` projections
//! are still required to match).
//!
//! The audit is intentionally pure: it consumes already-built packets and
//! returns a structured outcome. Callers wire the env-gated kill-switch
//! (`CASS_INDEXER_PACKET_EQUIVALENCE_AUDIT`) at their site so this module
//! stays cheap to import and trivially testable.

use serde::{Deserialize, Serialize};

use crate::model::conversation_packet::{
    ConversationPacket, ConversationPacketAnalyticsProjection, ConversationPacketLexicalProjection,
    ConversationPacketSemanticProjection,
};

/// Env knob (1/true/yes ⇒ enabled) that opts the live persist path into
/// emitting compare-mode audit records. Default is off so production cost
/// stays at zero.
pub const PACKET_EQUIVALENCE_AUDIT_ENV: &str = "CASS_INDEXER_PACKET_EQUIVALENCE_AUDIT";

/// Returns `true` when the env knob explicitly opts in. Anything else
/// (unset, "0", "false", "no", "off") leaves the audit disabled.
pub fn packet_equivalence_audit_enabled() -> bool {
    match dotenvy::var(PACKET_EQUIVALENCE_AUDIT_ENV) {
        Ok(value) => matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Err(_) => false,
    }
}

/// Tolerances applied while comparing packets. Each field documents *why*
/// a category of drift is acceptable, so future agents can decide whether
/// a hit was a real bug or a documented exemption.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PacketEquivalenceTolerance {
    /// When `true`, content drift driven by secret redaction (canonical
    /// persistence applies `redact_text`/`redact_json` while raw scans
    /// don't) is permitted, so `semantic_hash` and `message_hash` may
    /// differ while projections must still match.
    pub allow_redaction_drift: bool,
}

impl PacketEquivalenceTolerance {
    pub fn strict() -> Self {
        Self::default()
    }

    pub fn allow_redaction() -> Self {
        Self {
            allow_redaction_drift: true,
        }
    }
}

/// Distinct projections that can disagree between two packets. Carrying
/// the variant explicitly keeps audit logs grep-friendly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PacketProjectionDifference {
    AnalyticsRoleCounts {
        a: ConversationPacketAnalyticsProjection,
        b: ConversationPacketAnalyticsProjection,
    },
    LexicalProjection {
        a: ConversationPacketLexicalProjection,
        b: ConversationPacketLexicalProjection,
    },
    SemanticProjection {
        a: ConversationPacketSemanticProjection,
        b: ConversationPacketSemanticProjection,
    },
}

/// Distinct hash classes that can disagree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PacketHashDifference {
    SemanticHash { a: String, b: String },
    MessageHash { a: String, b: String },
}

/// Why two packets did not match. Multiple categories may fire from a
/// single audit (e.g. content drift changes both hashes *and* analytics
/// counts), so we ship a vector of structured items rather than a single
/// catch-all string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PacketEquivalenceMismatch {
    pub version_a: u32,
    pub version_b: u32,
    pub projection_differences: Vec<PacketProjectionDifference>,
    pub hash_differences: Vec<PacketHashDifference>,
}

impl PacketEquivalenceMismatch {
    /// True when the only disagreements are hash-level (i.e. content
    /// mutated but every byte-budget projection still agrees). This is
    /// the shape we expect under `allow_redaction_drift` and helps
    /// callers downgrade those cases to debug-level logs while real
    /// projection drift escalates to warn.
    pub fn is_hash_only(&self) -> bool {
        self.projection_differences.is_empty() && !self.hash_differences.is_empty()
    }
}

/// Result of an equivalence audit. The `Match` variant carries the
/// agreed semantic hash so downstream callers can fingerprint the audited
/// pair in their own logs/ledgers without re-computing it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum PacketEquivalenceOutcome {
    Match { semantic_hash: String },
    Mismatch(PacketEquivalenceMismatch),
}

impl PacketEquivalenceOutcome {
    pub fn is_match(&self) -> bool {
        matches!(self, Self::Match { .. })
    }

    pub fn is_mismatch(&self) -> bool {
        matches!(self, Self::Mismatch(_))
    }
}

/// Runs equivalence audits between pairs of packets. The auditor itself
/// is stateless; tolerances are provided per-call so a single instance
/// can serve both strict (rebuild-path) and redaction-aware (live-persist
/// path) call sites.
#[derive(Debug, Default, Clone, Copy)]
pub struct PacketEquivalenceAuditor;

impl PacketEquivalenceAuditor {
    pub fn new() -> Self {
        Self
    }

    /// Compare two packets under the supplied tolerance. The `a`/`b`
    /// labelling is symmetric — swapping arguments returns the same
    /// classification with `a`/`b` swapped inside differences.
    pub fn audit_pair(
        self,
        a: &ConversationPacket,
        b: &ConversationPacket,
        tolerance: &PacketEquivalenceTolerance,
    ) -> PacketEquivalenceOutcome {
        let mut projection_differences = Vec::new();
        if a.projections.analytics != b.projections.analytics {
            projection_differences.push(PacketProjectionDifference::AnalyticsRoleCounts {
                a: a.projections.analytics.clone(),
                b: b.projections.analytics.clone(),
            });
        }
        if a.projections.lexical != b.projections.lexical {
            projection_differences.push(PacketProjectionDifference::LexicalProjection {
                a: a.projections.lexical.clone(),
                b: b.projections.lexical.clone(),
            });
        }
        if a.projections.semantic != b.projections.semantic {
            projection_differences.push(PacketProjectionDifference::SemanticProjection {
                a: a.projections.semantic.clone(),
                b: b.projections.semantic.clone(),
            });
        }

        let mut hash_differences = Vec::new();
        let hashes_match = a.hashes.semantic_hash == b.hashes.semantic_hash
            && a.hashes.message_hash == b.hashes.message_hash;
        if !hashes_match && !tolerance.allow_redaction_drift {
            if a.hashes.semantic_hash != b.hashes.semantic_hash {
                hash_differences.push(PacketHashDifference::SemanticHash {
                    a: a.hashes.semantic_hash.clone(),
                    b: b.hashes.semantic_hash.clone(),
                });
            }
            if a.hashes.message_hash != b.hashes.message_hash {
                hash_differences.push(PacketHashDifference::MessageHash {
                    a: a.hashes.message_hash.clone(),
                    b: b.hashes.message_hash.clone(),
                });
            }
        }

        if a.version == b.version
            && projection_differences.is_empty()
            && hash_differences.is_empty()
        {
            PacketEquivalenceOutcome::Match {
                semantic_hash: a.hashes.semantic_hash.clone(),
            }
        } else {
            PacketEquivalenceOutcome::Mismatch(PacketEquivalenceMismatch {
                version_a: a.version,
                version_b: b.version,
                projection_differences,
                hash_differences,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::{NormalizedConversation, NormalizedMessage, NormalizedSnippet};
    use crate::model::conversation_packet::{ConversationPacket, ConversationPacketProvenance};
    use crate::model::types::{Conversation, Message, MessageRole, Snippet};
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    fn raw_conversation() -> NormalizedConversation {
        NormalizedConversation {
            agent_slug: "codex".to_string(),
            external_id: Some("session-audit".to_string()),
            title: Some("Audit fixture".to_string()),
            workspace: Some(PathBuf::from("/work/audit")),
            source_path: PathBuf::from("/work/audit/.codex/session.jsonl"),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_000_010_000),
            metadata: json!({"model": "gpt-5"}),
            messages: vec![
                NormalizedMessage {
                    idx: 0,
                    role: "user".to_string(),
                    author: Some("human".to_string()),
                    created_at: Some(1_700_000_000_000),
                    content: "audit the live persist sink".to_string(),
                    extra: json!({"turn": 1}),
                    snippets: vec![NormalizedSnippet {
                        file_path: Some(PathBuf::from("src/audit.rs")),
                        start_line: Some(1),
                        end_line: Some(1),
                        language: Some("rust".to_string()),
                        snippet_text: Some("// audit".to_string()),
                    }],
                    invocations: Vec::new(),
                },
                NormalizedMessage {
                    idx: 1,
                    role: "assistant".to_string(),
                    author: None,
                    created_at: Some(1_700_000_001_000),
                    content: "auditing".to_string(),
                    extra: json!({}),
                    snippets: Vec::new(),
                    invocations: Vec::new(),
                },
            ],
        }
    }

    fn canonical_conversation() -> Conversation {
        Conversation {
            id: Some(7),
            agent_slug: "codex".to_string(),
            workspace: Some(PathBuf::from("/work/audit")),
            external_id: Some("session-audit".to_string()),
            title: Some("Audit fixture".to_string()),
            source_path: PathBuf::from("/work/audit/.codex/session.jsonl"),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_000_010_000),
            approx_tokens: None,
            metadata_json: json!({"model": "gpt-5"}),
            source_id: "local".to_string(),
            origin_host: None,
            messages: vec![
                Message {
                    id: Some(70),
                    idx: 0,
                    role: MessageRole::User,
                    author: Some("human".to_string()),
                    created_at: Some(1_700_000_000_000),
                    content: "audit the live persist sink".to_string(),
                    extra_json: json!({"turn": 1}),
                    snippets: vec![Snippet {
                        id: Some(700),
                        file_path: Some(PathBuf::from("src/audit.rs")),
                        start_line: Some(1),
                        end_line: Some(1),
                        language: Some("rust".to_string()),
                        snippet_text: Some("// audit".to_string()),
                    }],
                },
                Message {
                    id: Some(71),
                    idx: 1,
                    role: MessageRole::Agent,
                    author: None,
                    created_at: Some(1_700_000_001_000),
                    content: "auditing".to_string(),
                    extra_json: json!({}),
                    snippets: Vec::new(),
                },
            ],
        }
    }

    #[test]
    fn raw_and_canonical_packet_audit_matches_when_content_agrees() {
        let provenance = ConversationPacketProvenance::local();
        let raw = ConversationPacket::from_normalized_conversation(
            &raw_conversation(),
            provenance.clone(),
        );
        let canonical =
            ConversationPacket::from_canonical_replay(&canonical_conversation(), provenance);

        let auditor = PacketEquivalenceAuditor::new();
        let outcome = auditor.audit_pair(&raw, &canonical, &PacketEquivalenceTolerance::strict());
        assert!(outcome.is_match(), "expected match, got {outcome:?}");
        if let PacketEquivalenceOutcome::Match { semantic_hash } = outcome {
            assert_eq!(semantic_hash, raw.hashes.semantic_hash);
            assert_eq!(semantic_hash.len(), 64, "blake3 hex digest is 64 chars");
        }
    }

    #[test]
    fn role_count_drift_surfaces_as_analytics_projection_difference() {
        let provenance = ConversationPacketProvenance::local();
        let raw = ConversationPacket::from_normalized_conversation(
            &raw_conversation(),
            provenance.clone(),
        );

        let mut canonical_data = canonical_conversation();
        canonical_data.messages.push(Message {
            id: Some(72),
            idx: 2,
            role: MessageRole::Tool,
            author: Some("ripgrep".to_string()),
            created_at: Some(1_700_000_002_000),
            content: "tool output".to_string(),
            extra_json: json!({}),
            snippets: Vec::new(),
        });
        let canonical = ConversationPacket::from_canonical_replay(&canonical_data, provenance);

        let auditor = PacketEquivalenceAuditor::new();
        let outcome = auditor.audit_pair(&raw, &canonical, &PacketEquivalenceTolerance::strict());
        let PacketEquivalenceOutcome::Mismatch(mismatch) = outcome else {
            panic!("expected mismatch when role counts diverge");
        };
        assert!(
            mismatch.projection_differences.iter().any(|diff| matches!(
                diff,
                PacketProjectionDifference::AnalyticsRoleCounts { a, b }
                    if a.tool_messages == 0 && b.tool_messages == 1
            )),
            "expected analytics tool-message drift, got {:?}",
            mismatch.projection_differences
        );
        assert!(
            !mismatch.is_hash_only(),
            "projection drift must not be downgraded to hash-only"
        );
    }

    #[test]
    fn redaction_drift_is_excused_only_under_explicit_tolerance() {
        let provenance = ConversationPacketProvenance::local();
        let raw = ConversationPacket::from_normalized_conversation(
            &raw_conversation(),
            provenance.clone(),
        );
        let mut redacted = canonical_conversation();
        // Simulate redaction substituting content while preserving byte
        // count (the projection contract requires byte-for-byte length
        // agreement; secret-redactors that change length would break the
        // analytics projection regardless of tolerance, which is correct).
        let redacted_text = "█".repeat(raw.payload.messages[0].content.chars().count());
        debug_assert_eq!(
            redacted_text.chars().count(),
            raw.payload.messages[0].content.chars().count()
        );
        // Match the byte length of the original content to keep the
        // lexical/semantic byte projections aligned (the redactor in
        // production is responsible for the same invariant; this test
        // pins the contract).
        let want_bytes = raw.payload.messages[0].content.len();
        let mut bytes = Vec::with_capacity(want_bytes);
        bytes.resize(want_bytes, b'#');
        redacted.messages[0].content = String::from_utf8(bytes).unwrap();
        let canonical = ConversationPacket::from_canonical_replay(&redacted, provenance);

        let auditor = PacketEquivalenceAuditor::new();

        let strict = auditor.audit_pair(&raw, &canonical, &PacketEquivalenceTolerance::strict());
        let PacketEquivalenceOutcome::Mismatch(mismatch) = strict else {
            panic!("strict audit should flag content/hash drift");
        };
        assert!(
            mismatch.is_hash_only(),
            "byte-length-preserving redaction should leave only hash drift, got {:?}",
            mismatch
        );
        assert!(
            mismatch
                .hash_differences
                .iter()
                .any(|d| matches!(d, PacketHashDifference::SemanticHash { .. }))
        );

        let tolerant = auditor.audit_pair(
            &raw,
            &canonical,
            &PacketEquivalenceTolerance::allow_redaction(),
        );
        assert!(
            tolerant.is_match(),
            "redaction-tolerant audit must match when only hashes drift, got {tolerant:?}"
        );
    }

    #[test]
    fn audit_env_gate_is_off_by_default_and_respects_explicit_opt_in() {
        let _guard = env_lock();
        let previous = std::env::var(PACKET_EQUIVALENCE_AUDIT_ENV).ok();

        // SAFETY: single-threaded test holding env_lock; restored below.
        unsafe {
            std::env::remove_var(PACKET_EQUIVALENCE_AUDIT_ENV);
        }
        assert!(
            !packet_equivalence_audit_enabled(),
            "audit must default to OFF so production cost stays at zero"
        );

        for value in ["1", "true", "TRUE", "yes", "on"] {
            // SAFETY: single-threaded test holding env_lock.
            unsafe {
                std::env::set_var(PACKET_EQUIVALENCE_AUDIT_ENV, value);
            }
            assert!(
                packet_equivalence_audit_enabled(),
                "value {value:?} should opt into the audit"
            );
        }

        for value in ["0", "false", "no", "off", ""] {
            // SAFETY: single-threaded test holding env_lock.
            unsafe {
                std::env::set_var(PACKET_EQUIVALENCE_AUDIT_ENV, value);
            }
            assert!(
                !packet_equivalence_audit_enabled(),
                "value {value:?} must NOT opt into the audit"
            );
        }

        // Restore the caller's env to keep parallel tests deterministic.
        // SAFETY: single-threaded test holding env_lock.
        unsafe {
            match previous {
                Some(v) => std::env::set_var(PACKET_EQUIVALENCE_AUDIT_ENV, v),
                None => std::env::remove_var(PACKET_EQUIVALENCE_AUDIT_ENV),
            }
        }
    }

    #[test]
    fn audit_outcome_serializes_with_outcome_tag() {
        let provenance = ConversationPacketProvenance::local();
        let raw = ConversationPacket::from_normalized_conversation(
            &raw_conversation(),
            provenance.clone(),
        );
        let canonical =
            ConversationPacket::from_canonical_replay(&canonical_conversation(), provenance);
        let outcome = PacketEquivalenceAuditor::new().audit_pair(
            &raw,
            &canonical,
            &PacketEquivalenceTolerance::strict(),
        );
        let serialized = serde_json::to_string(&outcome).expect("serialize match outcome");
        assert!(
            serialized.contains("\"outcome\":\"match\""),
            "match outcome should serialize with snake_case `outcome` tag, got {serialized}"
        );
        assert!(serialized.contains("\"semantic_hash\""));
    }
}
