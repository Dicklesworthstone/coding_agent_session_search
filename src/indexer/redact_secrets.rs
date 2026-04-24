//! Ingestion-time secret redaction for message content and metadata.
//!
//! Prevents secrets (API keys, tokens, passwords, private keys) leaked in
//! tool-result blocks from being persisted into the cass database.
//!
//! This module runs at ingestion time in `map_to_internal()`, before any data
//! reaches SQLite or the FTS index.  It is intentionally conservative: it uses
//! well-known prefix patterns rather than high-entropy heuristics to avoid
//! false positives on normal code content.
//!
//! See also: `pages::secret_scan` (post-hoc scanning of existing data).

use std::borrow::Cow;

use once_cell::sync::Lazy;
use regex::{Regex, RegexSet};

/// Placeholder inserted where a secret was found.
const REDACTED: &str = "[REDACTED]";

/// A compiled secret-detection pattern.
struct SecretPattern {
    pattern: &'static str,
    regex: Regex,
}

/// All built-in patterns, compiled once on first use.
static SECRET_PATTERNS: Lazy<Vec<SecretPattern>> = Lazy::new(|| {
    vec![
        // AWS Access Key ID (always starts with AKIA)
        SecretPattern {
            pattern: r"\bAKIA[0-9A-Z]{16}\b",
            regex: Regex::new(r"\bAKIA[0-9A-Z]{16}\b").expect("aws access key regex"),
        },
        // AWS Secret Key in assignment context
        SecretPattern {
            pattern: r#"(?i)aws(.{0,20})?(secret|access)?[_-]?key\s*[:=]\s*['"]?[A-Za-z0-9/+=]{40}['"]?"#,
            regex: Regex::new(
                r#"(?i)aws(.{0,20})?(secret|access)?[_-]?key\s*[:=]\s*['"]?[A-Za-z0-9/+=]{40}['"]?"#,
            )
            .expect("aws secret regex"),
        },
        // GitHub PAT (ghp_, gho_, ghu_, ghs_, ghr_)
        SecretPattern {
            pattern: r"\bgh[pousr]_[A-Za-z0-9]{36}\b",
            regex: Regex::new(r"\bgh[pousr]_[A-Za-z0-9]{36}\b").expect("github pat regex"),
        },
        // OpenAI API key (sk-...)
        SecretPattern {
            pattern: r"\bsk-[A-Za-z0-9]{20,}\b",
            regex: Regex::new(r"\bsk-[A-Za-z0-9]{20,}\b").expect("openai key regex"),
        },
        // Anthropic API key (sk-ant-...)
        SecretPattern {
            pattern: r"\bsk-ant-[A-Za-z0-9]{20,}\b",
            regex: Regex::new(r"\bsk-ant-[A-Za-z0-9]{20,}\b").expect("anthropic key regex"),
        },
        // Bearer tokens in authorization headers
        SecretPattern {
            pattern: r"(?i)Bearer\s+[A-Za-z0-9_\-.]{20,}",
            regex: Regex::new(r"(?i)Bearer\s+[A-Za-z0-9_\-.]{20,}").expect("bearer token regex"),
        },
        // JWT tokens (eyJ...)
        SecretPattern {
            pattern: r"\beyJ[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+\b",
            regex: Regex::new(r"\beyJ[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+\b")
                .expect("jwt regex"),
        },
        // PEM private keys
        SecretPattern {
            pattern: r"-----BEGIN (?:RSA|EC|DSA|OPENSSH|PGP) PRIVATE KEY-----",
            regex: Regex::new(r"-----BEGIN (?:RSA|EC|DSA|OPENSSH|PGP) PRIVATE KEY-----")
                .expect("private key regex"),
        },
        // Database connection URLs with credentials
        SecretPattern {
            pattern: r"(?i)\b(postgres|postgresql|mysql|mongodb|redis)://[^\s]{8,}",
            regex: Regex::new(
                r"(?i)\b(postgres|postgresql|mysql|mongodb|redis)://[^\s]{8,}",
            )
            .expect("db url regex"),
        },
        // Generic key/token/secret/password assignments
        SecretPattern {
            pattern: r#"(?i)(api[_-]?key|api[_-]?secret|auth[_-]?token|access[_-]?token|secret[_-]?key|password|passwd)\s*[:=]\s*['"]?[A-Za-z0-9_\-/+=]{8,}['"]?"#,
            regex: Regex::new(
                r#"(?i)(api[_-]?key|api[_-]?secret|auth[_-]?token|access[_-]?token|secret[_-]?key|password|passwd)\s*[:=]\s*['"]?[A-Za-z0-9_\-/+=]{8,}['"]?"#,
            )
            .expect("generic api key regex"),
        },
        // Slack tokens (xoxb-, xoxp-, xoxs-, xoxa-, xoxo-, xoxr-)
        SecretPattern {
            pattern: r"\bxox[bpsar]-[A-Za-z0-9\-]{10,}",
            regex: Regex::new(r"\bxox[bpsar]-[A-Za-z0-9\-]{10,}").expect("slack token regex"),
        },
        // Stripe keys (sk_live_, pk_live_, rk_live_)
        SecretPattern {
            pattern: r"\b[spr]k_live_[A-Za-z0-9]{20,}",
            regex: Regex::new(r"\b[spr]k_live_[A-Za-z0-9]{20,}").expect("stripe key regex"),
        },
    ]
});

/// Fast pre-check for the common no-secret path. Keeps pattern ordering aligned
/// with `SECRET_PATTERNS` so matched set indices can select replacement regexes.
static SECRET_REGEX_SET: Lazy<RegexSet> = Lazy::new(|| {
    RegexSet::new(SECRET_PATTERNS.iter().map(|pattern| pattern.pattern)).expect("secret regex set")
});

/// Redact secrets from a plain-text string.
///
/// Returns the input unchanged if no secrets are detected.
pub fn redact_text(input: &str) -> Cow<'_, str> {
    let matches = SECRET_REGEX_SET.matches(input);
    if !matches.matched_any() {
        return Cow::Borrowed(input);
    }

    let mut output = Cow::Borrowed(input);
    for idx in matches.iter() {
        let replaced = SECRET_PATTERNS[idx]
            .regex
            .replace_all(output.as_ref(), REDACTED);
        if let Cow::Owned(redacted) = replaced {
            output = Cow::Owned(redacted);
        }
    }
    output
}

/// Redact secrets from a JSON value, recursively walking strings.
///
/// - String values are redacted in-place.
/// - Arrays and objects are walked recursively.
/// - Numbers, booleans, and null are left untouched.
pub fn redact_json(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) => {
            let redacted = redact_text(s).into_owned();
            serde_json::Value::String(redacted)
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(redact_json).collect())
        }
        serde_json::Value::Object(obj) => {
            let mut new_obj = serde_json::Map::new();
            for (k, v) in obj {
                let redacted_key = redact_text(k).into_owned();
                new_obj.insert(redacted_key, redact_json(v));
            }
            serde_json::Value::Object(new_obj)
        }
        other => other.clone(),
    }
}

/// Returns true if redaction is enabled (default: true).
///
/// Set `CASS_REDACT_SECRETS=0` or `CASS_REDACT_SECRETS=false` to disable.
pub fn redaction_enabled() -> bool {
    match dotenvy::var("CASS_REDACT_SECRETS") {
        Ok(val) => !matches!(val.as_str(), "0" | "false" | "off" | "no"),
        Err(_) => true,
    }
}

/// Stable identifier for the compiled SECRET_PATTERNS list.
///
/// Memoization keys for [`MemoizingRedactor`] combine input content
/// with this fingerprint so a pattern bump (new regex added, existing
/// regex tightened) automatically invalidates every prior cache entry
/// — silent stale cross-version reuse is impossible by construction.
///
/// The fingerprint is `redact-v1:<blake3-hex>` where the hash covers
/// every pattern source string concatenated with NUL separators. The
/// `v1` epoch lets future maintainers force a manual bump even when
/// the regex source set hasn't changed (e.g. if the replacement
/// constant changes from `[REDACTED]` to something else).
pub fn redaction_algorithm_fingerprint() -> String {
    static FINGERPRINT: Lazy<String> = Lazy::new(|| {
        let mut hasher = blake3::Hasher::new();
        for pattern in SECRET_PATTERNS.iter() {
            hasher.update(pattern.pattern.as_bytes());
            hasher.update(&[0]);
        }
        hasher.update(REDACTED.as_bytes());
        format!("redact-v1:{}", hasher.finalize().to_hex())
    });
    FINGERPRINT.clone()
}

/// Content-addressed memoizing redactor for the ingestion hot path.
///
/// `coding_agent_session_search-ibuuh.34`: redaction is a pure,
/// regex-heavy transformation that runs against every persisted message
/// content + metadata blob. Salvage replays, repeated assistant
/// boilerplate, and historical re-ingest all feed identical content
/// through the regex engine over and over. This wrapper keys
/// [`ContentAddressedMemoCache`] on the input bytes plus the algorithm
/// fingerprint so repeated content stops paying the regex cost while a
/// pattern bump invalidates every prior entry transparently.
///
/// The wrapper preserves the legacy [`redact_text`]/[`redact_json`]
/// contract byte-for-byte: see
/// `memoizing_redactor_matches_uncached_for_arbitrary_input` for the
/// equivalence gate. When the cache is hit, the recorded value is
/// returned directly; on miss, the legacy regex path runs and the
/// result is inserted under the content+algorithm key.
///
/// `MemoizingRedactor` is `pub(crate)` so the live persist path can
/// adopt it without leaking the memoization vocabulary into public
/// API. Wiring lives in the indexer crate.
#[allow(dead_code)]
pub(crate) struct MemoizingRedactor {
    text_cache: crate::indexer::memoization::ContentAddressedMemoCache<String>,
    algorithm_fingerprint: String,
}

#[allow(dead_code)]
impl MemoizingRedactor {
    /// Default cache capacity for typical refresh batches. Sized to
    /// cover a few thousand distinct message bodies before LRU
    /// eviction kicks in.
    pub(crate) const DEFAULT_CAPACITY: usize = 4096;

    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            text_cache: crate::indexer::memoization::ContentAddressedMemoCache::with_capacity(
                capacity,
            ),
            algorithm_fingerprint: redaction_algorithm_fingerprint(),
        }
    }

    pub(crate) fn new() -> Self {
        Self::with_capacity(Self::DEFAULT_CAPACITY)
    }

    pub(crate) fn algorithm_fingerprint(&self) -> &str {
        &self.algorithm_fingerprint
    }

    pub(crate) fn stats(&self) -> &crate::indexer::memoization::MemoCacheStats {
        self.text_cache.stats()
    }

    /// Memoized counterpart to [`redact_text`]. Returns an owned String
    /// (not Cow) because caching forces a copy on first compute anyway,
    /// and downstream callers (`map_to_internal`) immediately call
    /// `.into_owned()` regardless. Skipping the Cow indirection keeps
    /// the cached-hit path branchless.
    ///
    /// Each cache decision emits a structured `tracing` event so
    /// operators can audit hit / miss / insert / evict / quarantine
    /// behavior from logs alone (per `coding_agent_session_search-ibuuh.34`
    /// AC: "operator-auditable through structured hit, miss,
    /// invalidation, eviction, quarantine, and budget logs").
    pub(crate) fn redact_text(&mut self, input: &str) -> String {
        let (output, _audit) = self.redact_text_with_audit(input);
        output
    }

    /// Audit-bearing variant: returns the redacted text plus the
    /// structured cache-decision records (lookup audit, plus insert
    /// audit on miss). Callers that want to forward records to a
    /// subscriber (telemetry sink, doctor diagnostics, etc.) use this
    /// directly; the convenience `redact_text` wrapper drops them
    /// after emitting tracing events.
    pub(crate) fn redact_text_with_audit(
        &mut self,
        input: &str,
    ) -> (
        String,
        Vec<crate::indexer::memoization::MemoCacheAuditRecord>,
    ) {
        // Empty fast-path matches the uncached contract and bypasses
        // the cache entirely (see memoizing_redactor_empty_input_skips_cache).
        if input.is_empty() {
            return (String::new(), Vec::new());
        }
        let key = self.key_for(input);
        let (lookup, lookup_audit) = self.text_cache.get_with_audit(&key);
        Self::trace_audit(&lookup_audit);
        match lookup {
            crate::indexer::memoization::MemoLookup::Hit { value } => (value, vec![lookup_audit]),
            crate::indexer::memoization::MemoLookup::Quarantined { reason } => {
                // Quarantined entry: never serve a stale value;
                // recompute via the legacy regex path, but DO NOT
                // re-insert (the entry stays quarantined for operator
                // inspection until explicitly lifted via
                // `lift_quarantine_for`).
                tracing::warn!(
                    quarantine_reason = %reason,
                    algorithm = %self.algorithm_fingerprint,
                    "redaction memo entry is quarantined; falling back to direct regex pass"
                );
                let redacted = redact_text(input).into_owned();
                (redacted, vec![lookup_audit])
            }
            crate::indexer::memoization::MemoLookup::Miss => {
                let redacted = redact_text(input).into_owned();
                let insert_audit = self.text_cache.insert_with_audit(key, redacted.clone());
                Self::trace_audit(&insert_audit);
                (redacted, vec![lookup_audit, insert_audit])
            }
        }
    }

    /// Invalidate a cached redaction for the given input. Returns
    /// `true` only when an entry was actually removed (matches the
    /// underlying `ContentAddressedMemoCache` contract). Mostly
    /// useful for tests and for operator tooling that wants to bust
    /// individual cache entries without restarting the process.
    pub(crate) fn invalidate(&mut self, input: &str) -> bool {
        if input.is_empty() {
            return false;
        }
        let key = self.key_for(input);
        let audit = self.text_cache.invalidate_with_audit(&key);
        Self::trace_audit(&audit);
        audit.changed
    }

    /// Quarantine a cached entry: subsequent lookups will return
    /// [`MemoLookup::Quarantined`] (handled by `redact_text` as a
    /// fallthrough to the direct regex path) instead of the cached
    /// value. The reason is preserved for operator inspection. Used
    /// when telemetry detects a poisoned redaction (e.g. unexpected
    /// regex behavior under a hot pattern bump that the algorithm
    /// fingerprint didn't catch).
    pub(crate) fn quarantine(&mut self, input: &str, reason: impl Into<String>) {
        if input.is_empty() {
            return;
        }
        let key = self.key_for(input);
        let audit = self.text_cache.quarantine_with_audit(key, reason);
        Self::trace_audit(&audit);
    }

    fn trace_audit(audit: &crate::indexer::memoization::MemoCacheAuditRecord) {
        // Severity tiers match operator expectations: hits are noise
        // (trace), misses + inserts are routine (debug), evictions
        // are noteworthy (info), invalidations and quarantines are
        // alarming enough to warn so they show up in default-level
        // logs without dredging.
        use crate::indexer::memoization::MemoCacheEvent;
        match audit.event {
            MemoCacheEvent::Hit => tracing::trace!(
                target: "cass::redact::memo",
                algorithm = %audit.key.algorithm,
                stats = ?audit.stats,
                "redact memo hit"
            ),
            MemoCacheEvent::Miss => tracing::debug!(
                target: "cass::redact::memo",
                algorithm = %audit.key.algorithm,
                stats = ?audit.stats,
                "redact memo miss"
            ),
            MemoCacheEvent::Insert => tracing::debug!(
                target: "cass::redact::memo",
                algorithm = %audit.key.algorithm,
                live_entries = audit.stats.live_entries,
                "redact memo insert"
            ),
            MemoCacheEvent::Evict { ref reason } => tracing::info!(
                target: "cass::redact::memo",
                evict_reason = ?reason,
                live_entries = audit.stats.live_entries,
                evictions_capacity = audit.stats.evictions_capacity,
                "redact memo eviction"
            ),
            MemoCacheEvent::Invalidate => tracing::warn!(
                target: "cass::redact::memo",
                changed = audit.changed,
                live_entries = audit.stats.live_entries,
                invalidations = audit.stats.invalidations,
                "redact memo invalidate"
            ),
            MemoCacheEvent::Quarantine { ref reason } => tracing::warn!(
                target: "cass::redact::memo",
                quarantine_reason = %reason,
                quarantined_entries = audit.quarantined_entries,
                "redact memo quarantine"
            ),
        }
    }

    /// Memoized counterpart to [`redact_json`]. Recurses through the
    /// JSON value, memoizing each string scalar (and each object key)
    /// independently — JSON arrays / objects themselves are not
    /// cached because their structural identity dominates compared to
    /// per-string regex cost.
    pub(crate) fn redact_json(&mut self, value: &serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::String(s) => serde_json::Value::String(self.redact_text(s)),
            serde_json::Value::Array(arr) => {
                serde_json::Value::Array(arr.iter().map(|v| self.redact_json(v)).collect())
            }
            serde_json::Value::Object(obj) => {
                let mut new_obj = serde_json::Map::with_capacity(obj.len());
                for (k, v) in obj {
                    let redacted_key = self.redact_text(k);
                    new_obj.insert(redacted_key, self.redact_json(v));
                }
                serde_json::Value::Object(new_obj)
            }
            other => other.clone(),
        }
    }

    fn key_for(&self, input: &str) -> crate::indexer::memoization::MemoKey {
        // Hash content with blake3 for a fixed-width key (avoids
        // pathological 1-MiB-content cache keys that would otherwise
        // dominate cache memory).
        let mut hasher = blake3::Hasher::new();
        hasher.update(input.as_bytes());
        let content_hash = crate::indexer::memoization::MemoContentHash::from_bytes(
            hasher.finalize().as_bytes().to_vec(),
        );
        crate::indexer::memoization::MemoKey::new(
            content_hash,
            "redact_text",
            self.algorithm_fingerprint.clone(),
        )
    }
}

impl Default for MemoizingRedactor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use serial_test::serial;

    #[test]
    fn redacts_openai_key() {
        let input = "my key is sk-ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij";
        let output = redact_text(input);
        assert_eq!(output, "my key is [REDACTED]");
        assert!(!output.contains("sk-ABCDE"));
    }

    #[test]
    fn redacts_anthropic_key() {
        let input = "sk-ant-ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij";
        let output = redact_text(input);
        assert_eq!(output, "[REDACTED]");
    }

    #[test]
    fn redacts_github_pat() {
        let input = "token ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij";
        let output = redact_text(input);
        assert_eq!(output, "token [REDACTED]");
    }

    #[test]
    fn redacts_bearer_token() {
        let input = "Authorization: Bearer eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9.payload.signature";
        let output = redact_text(input);
        assert!(!output.contains("eyJhbGci"));
    }

    #[test]
    fn redacts_aws_access_key() {
        let input = "AKIAIOSFODNN7EXAMPLE";
        let output = redact_text(input);
        assert_eq!(output, "[REDACTED]");
    }

    #[test]
    fn redacts_private_key_header() {
        let input = "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAK...";
        let output = redact_text(input);
        assert!(output.starts_with("[REDACTED]"));
    }

    #[test]
    fn redacts_generic_api_key_assignment() {
        let input = "api_key=abcdefgh12345678";
        let output = redact_text(input);
        assert_eq!(output, "[REDACTED]");
    }

    #[test]
    fn redacts_database_url() {
        let input = "DATABASE_URL=postgres://user:pass@host:5432/db";
        let output = redact_text(input);
        assert!(!output.contains("user:pass"));
    }

    #[test]
    fn redacts_stripe_key() {
        // Build the test key dynamically to avoid GitHub push protection flagging it
        let input = format!("{}_{}", "sk_live", "AAAABBBBCCCCDDDDEEEEFFFFGGGG");
        let output = redact_text(&input);
        assert_eq!(output, "[REDACTED]");
    }

    #[test]
    fn redacts_slack_token() {
        let input = "xoxb-123456789-abcdefghij";
        let output = redact_text(input);
        assert_eq!(output, "[REDACTED]");
    }

    #[test]
    fn leaves_normal_text_unchanged() {
        let input = "Hello, this is a normal message about code review.";
        let output = redact_text(input);
        assert_eq!(output, input);
        assert!(
            matches!(output, Cow::Borrowed(_)),
            "no-secret path should not allocate"
        );
    }

    #[test]
    fn leaves_short_tokens_unchanged() {
        // Short strings should not match (below minimum lengths)
        let input = "sk-abc";
        let output = redact_text(input);
        assert_eq!(output, input);
    }

    #[test]
    fn redacts_json_string_values() {
        let input = json!({
            "tool_result": "Response contains sk-ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij",
            "safe": "no secrets here",
            "number": 42
        });
        let output = redact_json(&input);
        assert_eq!(output["tool_result"], json!("Response contains [REDACTED]"));
        assert_eq!(output["safe"], json!("no secrets here"));
        assert_eq!(output["number"], json!(42));
    }

    #[test]
    fn redacts_nested_json() {
        let input = json!({
            "outer": {
                "inner": "ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij"
            },
            "array": ["safe", "sk-ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij"]
        });
        let output = redact_json(&input);
        assert_eq!(output["outer"]["inner"], json!("[REDACTED]"));
        assert_eq!(output["array"][0], json!("safe"));
        assert_eq!(output["array"][1], json!("[REDACTED]"));
    }

    #[test]
    #[serial]
    fn redaction_enabled_default() {
        // When env var is not set, should be enabled
        // Safety: only called in single-threaded test context
        unsafe { std::env::remove_var("CASS_REDACT_SECRETS") };
        assert!(redaction_enabled());
    }

    #[test]
    #[serial]
    fn redaction_can_be_disabled() {
        unsafe { std::env::set_var("CASS_REDACT_SECRETS", "0") };
        assert!(!redaction_enabled());

        unsafe { std::env::set_var("CASS_REDACT_SECRETS", "false") };
        assert!(!redaction_enabled());

        // Restore for other tests
        unsafe { std::env::remove_var("CASS_REDACT_SECRETS") };
    }

    #[test]
    fn multiple_secrets_in_one_string() {
        let input = "key1=sk-ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij and key2=ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij";
        let output = redact_text(input);
        assert!(!output.contains("sk-ABCDE"));
        assert!(!output.contains("ghp_ABCDE"));
        assert_eq!(output.matches("[REDACTED]").count(), 2);
        assert!(
            matches!(output, Cow::Owned(_)),
            "matched secret path should return owned redacted text"
        );
    }

    /// `coding_agent_session_search-ibuuh.34` (memoization equivalence
    /// gate): the memoizing redactor must produce byte-identical
    /// output to the legacy `redact_text` path on every input.
    /// Equivalence is checked across:
    /// - clean inputs with no secret matches
    /// - single-secret inputs (every supported pattern fires at least once)
    /// - multi-secret inputs (multiple replacement passes)
    /// - empty input (fast-path)
    /// - long boilerplate-style inputs (large blob with no secrets)
    ///
    /// First and second invocations on the same input must agree
    /// (cache-hit invariance) AND match the uncached result.
    #[test]
    fn memoizing_redactor_matches_uncached_for_arbitrary_input() {
        // Diagnostic-message slice helper: MUST land on a UTF-8 char
        // boundary so we can extend this fixture set with multi-byte
        // inputs in the future without panicking on byte-slice
        // boundary errors. (MEMORY.md flagged this exact pattern as
        // a recurring footgun; this helper inoculates the test.)
        fn safe_prefix(s: &str, max_bytes: usize) -> &str {
            let mut end = s.len().min(max_bytes);
            while end > 0 && !s.is_char_boundary(end) {
                end -= 1;
            }
            &s[..end]
        }
        let twenty_kib_unicode = "🔐abc".repeat(2_048);
        let inputs: &[&str] = &[
            "",
            "no secrets here, just prose",
            "my key is sk-ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij",
            "sk-ant-ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij followed by AKIAABCDEFGHIJKLMNOP",
            "Authorization: Bearer eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9.payload.signature",
            "ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij and another ghp_ZYXWVUTSRQPONMLKJIHGFEDCBA0123456789",
            // Multi-byte UTF-8 input: pins that the memoized path's
            // hashing + cache key construction handles non-ASCII
            // content (blake3 over .as_bytes() handles any byte
            // sequence). Pre-fixup, the diagnostic prefix slice
            // below would have panicked on this input.
            "🔐 user pasted sk-ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij from 测试",
            &twenty_kib_unicode,
            &"a".repeat(10_000),
        ];
        let mut redactor = MemoizingRedactor::with_capacity(64);
        for input in inputs {
            let uncached = redact_text(input).into_owned();
            let memoized_first = redactor.redact_text(input);
            let memoized_second = redactor.redact_text(input);
            assert_eq!(
                uncached,
                memoized_first,
                "memoized first call must match legacy uncached redact_text for input prefix: {:?}",
                safe_prefix(input, 64)
            );
            assert_eq!(
                uncached,
                memoized_second,
                "memoized second call must match legacy uncached for input prefix: {:?}",
                safe_prefix(input, 64)
            );
        }
    }

    /// Repeated identical content must hit the cache rather than
    /// re-running the regex set. Pinning hits/misses is the operator
    /// audit signal the bead acceptance asks for.
    #[test]
    fn memoizing_redactor_reuses_cache_for_repeated_content() {
        let mut redactor = MemoizingRedactor::with_capacity(16);
        let payload = "boilerplate assistant prompt: please help with sk-ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij";
        // Three identical calls: 1 miss + 2 hits. Empty-string
        // fast-path is never accounted in the cache, so it does not
        // perturb the counters.
        let _ = redactor.redact_text("");
        let _ = redactor.redact_text(payload);
        let _ = redactor.redact_text(payload);
        let _ = redactor.redact_text(payload);
        let stats = redactor.stats();
        assert_eq!(stats.misses, 1, "first call must be a cache miss");
        assert_eq!(
            stats.hits, 2,
            "subsequent identical calls must be cache hits"
        );
        assert_eq!(stats.inserts, 1, "exactly one redacted result inserted");
    }

    /// A pattern bump (algorithm fingerprint change) must invalidate
    /// every prior memo entry. We simulate this by constructing two
    /// `MemoizingRedactor` instances whose algorithm fingerprints
    /// differ — entries from one cannot serve hits to the other,
    /// guaranteeing safe cross-version semantics. Pinning the
    /// fingerprint structure (`redact-v1:<hex>`) guards against an
    /// accidental hash-format change that would silently break
    /// invalidation.
    #[test]
    fn memoizing_redactor_keys_isolate_by_algorithm_fingerprint() {
        let fingerprint = redaction_algorithm_fingerprint();
        assert!(
            fingerprint.starts_with("redact-v1:"),
            "fingerprint must carry an explicit version epoch, got: {fingerprint}"
        );
        let hex_part = fingerprint.strip_prefix("redact-v1:").unwrap();
        assert_eq!(
            hex_part.len(),
            64,
            "fingerprint hash must be a 64-char blake3 hex digest"
        );
        // Same compiled patterns ⇒ same fingerprint across calls.
        assert_eq!(fingerprint, redaction_algorithm_fingerprint());

        // Two fresh redactors share the algorithm fingerprint, so they
        // would route hits/misses through the same key shape. Pinning
        // both fingerprints equal guards against a thread-local /
        // process-singleton bug that could silently desync cache
        // versions across parallel persist workers.
        let r1 = MemoizingRedactor::new();
        let r2 = MemoizingRedactor::new();
        assert_eq!(r1.algorithm_fingerprint(), r2.algorithm_fingerprint());
    }

    /// `redact_json` round-trip via the memoizing path must agree with
    /// the legacy `redact_json` for non-trivial JSON shapes (nested
    /// arrays, nested objects, mixed scalars). Pins the recursive
    /// projection so a regression in either path's traversal trips a
    /// clear assertion.
    #[test]
    fn memoizing_redactor_redact_json_matches_uncached_for_nested_shapes() {
        let value = json!({
            "session": {
                "auth": "Bearer eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9.payload.signature",
                "history": [
                    "no secret",
                    "ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij",
                    {"key": "value", "leak": "sk-ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij"},
                    null,
                    42,
                    true,
                ],
                "metadata": {
                    "leaked_field": "sk-ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij",
                    "safe_field": "noop",
                },
            },
            "version": 7,
        });
        let uncached = redact_json(&value);
        let memoized = MemoizingRedactor::new().redact_json(&value);
        assert_eq!(
            uncached, memoized,
            "memoizing redact_json must match legacy redact_json byte-for-byte"
        );
    }

    /// Repeated metadata / extra_json structures are common in salvage
    /// replays and assistant boilerplate. The memoized JSON walker must
    /// reuse repeated object keys and repeated scalar values instead of
    /// re-running the regex set for every copy.
    #[test]
    fn memoizing_redactor_redact_json_reuses_repeated_keys_and_values() {
        let repeated_secret =
            "Authorization: Bearer eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9.payload.signature";
        let repeated_note = "same assistant boilerplate without secrets";
        let value = json!({
            "events": [
                {"token": repeated_secret, "note": repeated_note},
                {"token": repeated_secret, "note": repeated_note},
                {"token": repeated_secret, "note": repeated_note},
            ],
            "footer": repeated_note,
        });

        let uncached = redact_json(&value);
        let mut redactor = MemoizingRedactor::with_capacity(32);
        let memoized = redactor.redact_json(&value);

        assert_eq!(
            uncached, memoized,
            "memoized JSON redaction must preserve legacy output exactly"
        );
        assert!(
            !memoized.to_string().contains("eyJhbGci"),
            "memoized JSON redaction must still remove repeated secrets"
        );

        let stats = redactor.stats();
        assert_eq!(
            stats.misses, 6,
            "first occurrences of root keys, repeated child keys, and scalar values should miss once"
        );
        assert_eq!(
            stats.inserts, 6,
            "each distinct JSON key/value string should be inserted once"
        );
        assert_eq!(
            stats.hits, 9,
            "repeated child keys and repeated scalar values should hit the memo cache"
        );
    }

    /// Emptiness fast-path: zero-length input must NOT increment the
    /// cache miss counter. Otherwise an ingestion run with thousands
    /// of empty system messages would burn cache slots for
    /// content-equivalent empty strings.
    #[test]
    #[serial]
    fn memoizing_redactor_empty_input_skips_cache() {
        let mut redactor = MemoizingRedactor::with_capacity(8);
        let _ = redactor.redact_text("");
        let _ = redactor.redact_text("");
        let _ = redactor.redact_text("");
        let stats = redactor.stats();
        assert_eq!(stats.misses, 0, "empty input must not count as miss");
        assert_eq!(stats.hits, 0, "empty input must not count as hit");
        assert_eq!(stats.inserts, 0, "empty input must not insert into cache");
    }

    /// `coding_agent_session_search-ibuuh.34` (operator-audit gate):
    /// every cache decision must surface a structured
    /// MemoCacheAuditRecord so telemetry sinks / doctor diagnostics
    /// can reason about cache health without grepping internal stats.
    /// First call on a new content emits Lookup(Miss) + Insert.
    /// Second call emits Lookup(Hit). Pinning the audit shape directly
    /// closes the bead's "operator-auditable through structured hit,
    /// miss, invalidation, eviction, quarantine, and budget logs"
    /// requirement for the redaction sink.
    #[test]
    fn memoizing_redactor_with_audit_emits_lookup_and_insert_records() {
        use crate::indexer::memoization::{MemoCacheEvent, MemoCacheOperation};
        let mut redactor = MemoizingRedactor::with_capacity(8);
        let payload =
            "Authorization: Bearer eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9.payload.signature";

        let (first_output, first_audit) = redactor.redact_text_with_audit(payload);
        assert!(!first_output.contains("eyJhbGci"));
        assert_eq!(
            first_audit.len(),
            2,
            "first call must emit a lookup audit + an insert audit"
        );
        assert!(matches!(
            first_audit[0].operation,
            MemoCacheOperation::Lookup
        ));
        assert!(matches!(first_audit[0].event, MemoCacheEvent::Miss));
        assert!(matches!(
            first_audit[1].operation,
            MemoCacheOperation::Insert
        ));
        assert!(matches!(first_audit[1].event, MemoCacheEvent::Insert));
        assert_eq!(first_audit[1].stats.live_entries, 1);

        let (second_output, second_audit) = redactor.redact_text_with_audit(payload);
        assert_eq!(first_output, second_output);
        assert_eq!(
            second_audit.len(),
            1,
            "second call must emit only the lookup audit (cache hit)"
        );
        assert!(matches!(second_audit[0].event, MemoCacheEvent::Hit));
        assert_eq!(second_audit[0].stats.hits, 1);

        // Algorithm key carried on every audit record so a downstream
        // sink can disambiguate cache events when multiple
        // ContentAddressedMemoCaches share the same logger target.
        for record in first_audit.iter().chain(second_audit.iter()) {
            assert_eq!(record.key.algorithm, "redact_text");
            assert!(record.key.algorithm_version.starts_with("redact-v1:"));
        }
    }

    /// Invalidate must remove the cached entry so the next call is a
    /// miss + re-insert. Pin the changed/no-op semantics so a caller
    /// can rely on the boolean return value to know whether anything
    /// was actually evicted.
    #[test]
    fn memoizing_redactor_invalidate_drops_cached_entry() {
        let mut redactor = MemoizingRedactor::with_capacity(8);
        let payload = "no secret here, just a sentence";

        // Prime the cache.
        let _ = redactor.redact_text(payload);
        assert_eq!(redactor.stats().inserts, 1);
        assert_eq!(redactor.stats().misses, 1);
        let _ = redactor.redact_text(payload);
        assert_eq!(redactor.stats().hits, 1);

        // Invalidate must report the change.
        assert!(
            redactor.invalidate(payload),
            "invalidate must return true when an entry was removed"
        );
        assert_eq!(redactor.stats().invalidations, 1);
        // A second invalidate on the same key is a no-op.
        assert!(
            !redactor.invalidate(payload),
            "second invalidate must be a no-op"
        );
        assert_eq!(redactor.stats().invalidations, 1);

        // Empty input invalidate is a no-op (matches the empty-input
        // fast-path: nothing was ever cached).
        assert!(
            !redactor.invalidate(""),
            "invalidating empty input must be a no-op"
        );

        // Next call must miss again, not hit.
        let _ = redactor.redact_text(payload);
        assert_eq!(
            redactor.stats().misses,
            2,
            "post-invalidate call must register as a miss"
        );
        assert_eq!(redactor.stats().hits, 1, "hits counter must not regress");
    }

    /// Quarantined entries must NEVER serve a cached value. After
    /// quarantine, the redactor falls through to the direct
    /// `redact_text` regex path and the cached value remains
    /// quarantined for operator inspection. This satisfies the bead's
    /// "suspected corruption or stale-entry quarantine" coverage
    /// requirement.
    #[test]
    fn memoizing_redactor_quarantined_entries_fall_through_to_direct_redaction() {
        use crate::indexer::memoization::{MemoCacheEvent, MemoCacheOperation};
        let mut redactor = MemoizingRedactor::with_capacity(8);
        let payload =
            "user=admin password=hunter2hunter2 token=ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij";

        // Prime + verify hit.
        let _ = redactor.redact_text(payload);
        let _ = redactor.redact_text(payload);
        assert_eq!(redactor.stats().hits, 1);

        // Quarantine the entry; subsequent lookup must report the
        // Quarantined outcome via audit AND fall through to direct
        // regex redaction (so the user-visible result is still the
        // correct redacted text).
        redactor.quarantine(payload, "telemetry: poisoned redaction signal");
        assert_eq!(redactor.stats().quarantined, 1);

        let (output, audit) = redactor.redact_text_with_audit(payload);
        assert!(
            !output.contains("ghp_ABCDE"),
            "post-quarantine redaction must still scrub secrets via direct regex pass"
        );
        assert!(
            !output.contains("password=hunter2hunter2"),
            "post-quarantine redaction must scrub generic password assignments"
        );
        assert_eq!(
            audit.len(),
            1,
            "quarantine fallthrough emits the lookup audit only (no insert)"
        );
        assert!(matches!(audit[0].operation, MemoCacheOperation::Lookup));
        assert!(matches!(audit[0].event, MemoCacheEvent::Quarantine { .. }));

        // Re-quarantining the same key with the same reason is a
        // no-op for the quarantine counter (already quarantined).
        redactor.quarantine(payload, "telemetry: poisoned redaction signal");
        assert_eq!(
            redactor.stats().quarantined,
            1,
            "re-quarantining the same key with the same reason must not double-count"
        );

        // Empty input quarantine is a no-op.
        redactor.quarantine("", "ignored");
        assert_eq!(redactor.stats().quarantined, 1);
    }
}
