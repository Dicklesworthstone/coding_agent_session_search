// Dead-code tolerated module-wide: this incident redaction layer lands ahead
// of the bounded discovery (.10.2) that produces raw evidence and the robot
// surfaces that emit the redacted summary.
#![allow(dead_code)]

//! Redaction provenance and privacy audit for incident mining (bead
//! cass-fleet-resilience-20260608-uojcg.10.5).
//!
//! The report's temporary mining artifacts lived in `/tmp` and contained raw
//! session-derived evidence (prompts, tool payloads). A product surface must
//! be stricter: by default an incident summary stores **counts, content
//! fingerprints, and redacted paths only** — never raw private text — and
//! ships a [`RedactionManifest`] stating exactly which fields were emitted,
//! which were suppressed, the hash strategy, the private-text policy, and the
//! opt-in flags that would unlock richer evidence.
//!
//! [`redact`] is the single chokepoint: it consumes [`RawIncidentEvidence`]
//! and a [`RedactionPolicy`] and produces a [`RedactedIncident`] plus the
//! manifest. The default policy ([`PrivateTextPolicy::SuppressAll`])
//! guarantees no raw prompt/tool payload appears in the serialized output —
//! the property the tests enforce. Categories/privacy tiers reuse
//! [`crate::search::incident_categories`]. All enums serialize as snake_case;
//! the content fingerprint is a stable, non-cryptographic dedup id (never the
//! raw text).

use std::hash::{Hash, Hasher};

use serde::{Deserialize, Serialize};

use crate::search::incident_categories::IncidentCategory;

/// How private session text is treated in an incident summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PrivateTextPolicy {
    /// Default: no raw text and no snippet; fingerprint + counts only.
    SuppressAll,
    /// Emit a length-bounded, masked snippet (opt-in).
    RedactedSnippets,
    /// Emit raw text verbatim — explicit opt-in only, never the default.
    RawOptIn,
}

impl Default for PrivateTextPolicy {
    fn default() -> Self {
        Self::SuppressAll
    }
}

/// How raw text is fingerprinted for dedup/correlation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HashStrategy {
    /// A stable 64-bit non-cryptographic content fingerprint (hex). Used for
    /// dedup/correlation only — never reversible to the raw text.
    Fingerprint64,
    /// No fingerprint emitted.
    None,
}

/// The redaction policy applied to mined evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RedactionPolicy {
    pub private_text: PrivateTextPolicy,
    pub hash: HashStrategy,
    /// Emit full source paths (true) vs basename-only redaction (false).
    pub allow_full_paths: bool,
}

impl Default for RedactionPolicy {
    fn default() -> Self {
        Self {
            private_text: PrivateTextPolicy::SuppressAll,
            hash: HashStrategy::Fingerprint64,
            allow_full_paths: false,
        }
    }
}

/// Raw mined evidence for one incident occurrence — contains private content
/// and must never be serialized to a robot surface directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawIncidentEvidence {
    pub category: IncidentCategory,
    pub occurrence_count: u64,
    /// Raw prompt text (private).
    pub raw_prompt_text: Option<String>,
    /// Raw tool payload (private).
    pub raw_tool_payload: Option<String>,
    /// Full source path (potentially identifying).
    pub source_path: Option<String>,
}

/// The redacted, surface-safe incident summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RedactedIncident {
    pub category: IncidentCategory,
    pub occurrence_count: u64,
    /// Stable content fingerprint of the raw evidence (hex), when hashed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_fingerprint: Option<String>,
    /// A masked snippet, only under `RedactedSnippets`/`RawOptIn`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    /// Source path, redacted to basename unless `allow_full_paths`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
}

/// Provenance of what redaction did — the auditable manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RedactionManifest {
    pub private_text_policy: PrivateTextPolicy,
    pub hash_strategy: HashStrategy,
    /// Fields that were emitted in the redacted summary.
    pub fields_emitted: Vec<String>,
    /// Fields that were suppressed (raw private content).
    pub fields_suppressed: Vec<String>,
    /// Opt-in flags that would unlock richer (less redacted) evidence.
    pub opt_in_flags: Vec<String>,
}

/// Stable non-cryptographic 64-bit fingerprint of `text`, as hex. Never the
/// raw text; safe to surface.
fn fingerprint64(text: &str) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Mask a snippet: bound length and replace long alphanumeric runs (possible
/// tokens) and `@`-bearing words (possible emails) so even an opted-in
/// snippet does not surface secrets verbatim.
fn masked_snippet(text: &str, max_chars: usize) -> String {
    let bounded: String = text.chars().take(max_chars).collect();
    bounded
        .split_whitespace()
        .map(|word| {
            let alnum_run = word.chars().filter(|c| c.is_alphanumeric()).count();
            if word.contains('@') || alnum_run >= 16 {
                "[redacted]".to_string()
            } else {
                word.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn basename(path: &str) -> String {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .to_string()
}

/// Redact raw incident evidence into a surface-safe summary plus its
/// provenance manifest, per `policy`. The default policy suppresses all raw
/// private text.
pub(crate) fn redact(
    evidence: &RawIncidentEvidence,
    policy: RedactionPolicy,
) -> (RedactedIncident, RedactionManifest) {
    let mut emitted = vec!["category".to_string(), "occurrence_count".to_string()];
    let mut suppressed = Vec::new();

    // Content fingerprint over the concatenated raw evidence (never the text).
    let content_fingerprint = match policy.hash {
        HashStrategy::Fingerprint64 => {
            let raw = format!(
                "{}\u{1f}{}",
                evidence.raw_prompt_text.as_deref().unwrap_or(""),
                evidence.raw_tool_payload.as_deref().unwrap_or("")
            );
            emitted.push("content_fingerprint".to_string());
            Some(fingerprint64(&raw))
        }
        HashStrategy::None => None,
    };

    // Private text handling.
    let snippet = match policy.private_text {
        PrivateTextPolicy::SuppressAll => {
            if evidence.raw_prompt_text.is_some() {
                suppressed.push("raw_prompt_text".to_string());
            }
            if evidence.raw_tool_payload.is_some() {
                suppressed.push("raw_tool_payload".to_string());
            }
            None
        }
        PrivateTextPolicy::RedactedSnippets => {
            // Tool payload stays suppressed; prompt becomes a masked snippet.
            if evidence.raw_tool_payload.is_some() {
                suppressed.push("raw_tool_payload".to_string());
            }
            evidence.raw_prompt_text.as_deref().map(|t| {
                emitted.push("snippet".to_string());
                masked_snippet(t, 80)
            })
        }
        PrivateTextPolicy::RawOptIn => {
            // Explicit opt-in: raw prompt emitted verbatim as the snippet.
            evidence.raw_prompt_text.as_deref().map(|t| {
                emitted.push("snippet".to_string());
                t.to_string()
            })
        }
    };

    // Source path: full only when explicitly allowed, else basename.
    let source_path = evidence.source_path.as_deref().map(|p| {
        emitted.push("source_path".to_string());
        if policy.allow_full_paths {
            p.to_string()
        } else {
            basename(p)
        }
    });

    let redacted = RedactedIncident {
        category: evidence.category,
        occurrence_count: evidence.occurrence_count,
        content_fingerprint,
        snippet,
        source_path,
    };

    let manifest = RedactionManifest {
        private_text_policy: policy.private_text,
        hash_strategy: policy.hash,
        fields_emitted: emitted,
        fields_suppressed: suppressed,
        // The flags an operator could pass to unlock richer evidence.
        opt_in_flags: vec![
            "--include-redacted-snippets".to_string(),
            "--include-raw-evidence".to_string(),
            "--allow-full-paths".to_string(),
        ],
    };

    (redacted, manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET_PROMPT: &str = "please use api key sk_live_ABCDEF0123456789TOKEN to call svc";
    const SECRET_TOOL: &str = "{\"tool\":\"bash\",\"args\":\"cat /home/dev/.env\"}";

    fn evidence() -> RawIncidentEvidence {
        RawIncidentEvidence {
            category: IncidentCategory::QuarantineOom,
            occurrence_count: 7,
            raw_prompt_text: Some(SECRET_PROMPT.to_string()),
            raw_tool_payload: Some(SECRET_TOOL.to_string()),
            source_path: Some("/home/dev/proj/session.jsonl".to_string()),
        }
    }

    #[test]
    fn enums_serialize_snake_case() {
        assert_eq!(
            serde_json::to_string(&PrivateTextPolicy::SuppressAll).unwrap(),
            "\"suppress_all\""
        );
        assert_eq!(
            serde_json::to_string(&HashStrategy::Fingerprint64).unwrap(),
            "\"fingerprint64\""
        );
    }

    #[test]
    fn default_policy_leaks_no_raw_prompt_or_tool_payload() {
        let (redacted, manifest) = redact(&evidence(), RedactionPolicy::default());
        let json = serde_json::to_string(&redacted).unwrap();
        // The critical guarantee: no raw private content anywhere in output.
        assert!(!json.contains("sk_live_ABCDEF0123456789TOKEN"), "{json}");
        assert!(!json.contains(SECRET_PROMPT), "{json}");
        assert!(!json.contains(SECRET_TOOL), "{json}");
        assert!(!json.contains(".env"), "{json}");
        assert!(redacted.snippet.is_none());
        // Both raw fields are recorded as suppressed in the manifest.
        assert!(
            manifest
                .fields_suppressed
                .contains(&"raw_prompt_text".to_string())
        );
        assert!(
            manifest
                .fields_suppressed
                .contains(&"raw_tool_payload".to_string())
        );
        // A fingerprint is emitted (and is not the raw text).
        let fp = redacted.content_fingerprint.unwrap();
        assert_eq!(fp.len(), 16);
        assert!(!fp.contains("sk_live"));
    }

    #[test]
    fn default_policy_redacts_source_path_to_basename() {
        let (redacted, _) = redact(&evidence(), RedactionPolicy::default());
        assert_eq!(redacted.source_path.as_deref(), Some("session.jsonl"));
    }

    #[test]
    fn allow_full_paths_emits_the_full_path() {
        let policy = RedactionPolicy {
            allow_full_paths: true,
            ..RedactionPolicy::default()
        };
        let (redacted, _) = redact(&evidence(), policy);
        assert_eq!(
            redacted.source_path.as_deref(),
            Some("/home/dev/proj/session.jsonl")
        );
    }

    #[test]
    fn fingerprint_is_deterministic_and_distinguishes_content() {
        let a = redact(&evidence(), RedactionPolicy::default())
            .0
            .content_fingerprint;
        let b = redact(&evidence(), RedactionPolicy::default())
            .0
            .content_fingerprint;
        assert_eq!(a, b, "fingerprint must be deterministic");
        let mut other = evidence();
        other.raw_prompt_text = Some("a different prompt".to_string());
        let c = redact(&other, RedactionPolicy::default())
            .0
            .content_fingerprint;
        assert_ne!(a, c, "different content must fingerprint differently");
    }

    #[test]
    fn redacted_snippets_policy_masks_tokens_and_still_suppresses_tool_payload() {
        let policy = RedactionPolicy {
            private_text: PrivateTextPolicy::RedactedSnippets,
            ..RedactionPolicy::default()
        };
        let (redacted, manifest) = redact(&evidence(), policy);
        let snippet = redacted.snippet.unwrap();
        // The long token is masked even in an opted-in snippet.
        assert!(
            !snippet.contains("sk_live_ABCDEF0123456789TOKEN"),
            "{snippet}"
        );
        assert!(snippet.contains("[redacted]"), "{snippet}");
        // Tool payload is still suppressed.
        assert!(
            manifest
                .fields_suppressed
                .contains(&"raw_tool_payload".to_string())
        );
        assert!(manifest.fields_emitted.contains(&"snippet".to_string()));
    }

    #[test]
    fn raw_opt_in_emits_verbatim_only_when_explicitly_selected() {
        let policy = RedactionPolicy {
            private_text: PrivateTextPolicy::RawOptIn,
            ..RedactionPolicy::default()
        };
        let (redacted, _) = redact(&evidence(), policy);
        // Opt-in is the ONLY path that surfaces raw text.
        assert_eq!(redacted.snippet.as_deref(), Some(SECRET_PROMPT));
        // And it is never the default.
        assert_ne!(
            RedactionPolicy::default().private_text,
            PrivateTextPolicy::RawOptIn
        );
    }

    #[test]
    fn manifest_records_policy_hash_and_opt_in_flags() {
        let (_, manifest) = redact(&evidence(), RedactionPolicy::default());
        assert_eq!(manifest.private_text_policy, PrivateTextPolicy::SuppressAll);
        assert_eq!(manifest.hash_strategy, HashStrategy::Fingerprint64);
        assert!(manifest.fields_emitted.contains(&"category".to_string()));
        assert!(
            manifest
                .fields_emitted
                .contains(&"content_fingerprint".to_string())
        );
        assert!(
            manifest
                .opt_in_flags
                .iter()
                .any(|f| f.contains("raw-evidence"))
        );
    }

    #[test]
    fn redacted_incident_round_trips_through_json() {
        let (redacted, manifest) = redact(&evidence(), RedactionPolicy::default());
        let rj = serde_json::to_string(&redacted).unwrap();
        assert!(rj.contains("\"category\":\"quarantine_oom\""));
        assert_eq!(
            serde_json::from_str::<RedactedIncident>(&rj).unwrap(),
            redacted
        );
        let mj = serde_json::to_string(&manifest).unwrap();
        assert!(mj.contains("\"private_text_policy\":\"suppress_all\""));
        assert_eq!(
            serde_json::from_str::<RedactionManifest>(&mj).unwrap(),
            manifest
        );
    }

    #[test]
    fn hash_strategy_none_emits_no_fingerprint() {
        let policy = RedactionPolicy {
            hash: HashStrategy::None,
            ..RedactionPolicy::default()
        };
        let (redacted, _) = redact(&evidence(), policy);
        assert!(redacted.content_fingerprint.is_none());
    }
}
