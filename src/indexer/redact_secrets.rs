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
use std::collections::HashMap;
use std::fmt::Write as _;

use once_cell::sync::Lazy;
use regex::{Regex, RegexSet};

/// Placeholder inserted where a secret was found.
const REDACTED: &str = "[REDACTED]";

/// Return whether a JSON object key names a credential-bearing field.
///
/// Plain-text patterns can only redact a value when its label and value are in
/// the same string (for example, `password=...`). Structured metadata stores
/// them as separate JSON nodes, so the object walker must use the key's
/// semantics. Normalization deliberately accepts the common snake/kebab/camel
/// spellings while using an exact allowlist to avoid broad false positives such
/// as `keyframe`, `monkey`, or `token_count`.
pub(crate) fn is_sensitive_json_field(key: &str) -> bool {
    let normalized = key
        .bytes()
        .filter(|byte| byte.is_ascii_alphanumeric())
        .map(|byte| char::from(byte.to_ascii_lowercase()))
        .collect::<String>();

    matches!(
        normalized.as_str(),
        "passwd"
            | "pwd"
            | "passphrase"
            | "pin"
            | "apikey"
            | "apisecret"
            | "token"
            | "authtoken"
            | "accesstoken"
            | "refreshtoken"
            | "idtoken"
            | "sessiontoken"
            | "bearertoken"
            | "secrettoken"
            | "oauthtoken"
            | "secret"
            | "secretkey"
            | "accesskey"
            | "awsaccesskeyid"
            | "awssecretaccesskey"
            | "awssessiontoken"
            | "awssecuritytoken"
            | "clienttoken"
            | "credential"
            | "credentials"
            | "authorization"
            | "cookie"
            | "setcookie"
            | "privatekey"
            | "privatekeypem"
            | "secretkeybase"
            | "databaseurl"
            | "connectionstring"
    ) || normalized.ends_with("password")
        || normalized.ends_with("passwordhash")
        || normalized.ends_with("hashedpassword")
        || normalized.ends_with("secret")
        || normalized.ends_with("token")
        || normalized.ends_with("apikey")
        || normalized.ends_with("credential")
        || normalized.ends_with("privatekey")
        || normalized.ends_with("secretkey")
        || normalized.ends_with("secretkeybase")
        || normalized.ends_with("apikeys")
        || normalized.ends_with("credentials")
}

fn redact_sensitive_json_value(
    key: &str,
    value: &serde_json::Value,
    otherwise: impl FnOnce() -> serde_json::Value,
) -> serde_json::Value {
    if is_sensitive_json_field(key) && !value.is_null() {
        serde_json::Value::String(REDACTED.to_owned())
    } else {
        otherwise()
    }
}

/// A compiled secret-detection pattern.
struct SecretPattern {
    pattern: &'static str,
    regex: Regex,
}

pub(crate) const AWS_ACCESS_KEY_PATTERN: &str = r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b";
pub(crate) const AWS_SECRET_KEY_PATTERN: &str =
    r#"(?i)aws(.{0,20})?(secret|access)?[_-]?key\s*[:=]\s*['"]?[A-Za-z0-9/+=]{40}['"]?"#;
pub(crate) const AWS_SESSION_TOKEN_PATTERN: &str = r#"(?i)\baws[_-]?(?:session|security)[_-]?token\s*[:=]\s*(?:"(?:\\.|[^"\\\r\n]){8,}"|'(?:\\.|[^'\\\r\n]){8,}'|[^\s,;}\]]{8,})"#;
pub(crate) const GITHUB_TOKEN_PATTERN: &str =
    r"\b(?:gh[pousr]_[A-Za-z0-9]{36}|github_pat_[A-Za-z0-9_]{20,})\b";
pub(crate) const OPENAI_API_KEY_PATTERN: &str =
    r"\b(?:sk-(?:proj-|admin-)[A-Za-z0-9_-]{19,}[A-Za-z0-9_]|sk-[A-Za-z0-9]{20,})\b";
pub(crate) const ANTHROPIC_API_KEY_PATTERN: &str =
    r"\bsk-ant-(?:api[0-9]{2}-)?[A-Za-z0-9_-]{19,}[A-Za-z0-9_]\b";
pub(crate) const BEARER_TOKEN_PATTERN: &str = r"(?i)\bBearer[ \t]+[A-Za-z0-9._~+/=-]{8,}";
pub(crate) const JWT_PATTERN: &str = r"\beyJ[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+\b";
pub(crate) const PRIVATE_KEY_BLOCK_PATTERN: &str = concat!(
    r"(?s)(?:",
    r"-----BEGIN RSA PRIVATE KEY-----.*?(?:-----END RSA PRIVATE KEY-----|\z)|", // ubs:ignore — public key-block regex, not embedded credentials.
    r"-----BEGIN EC PRIVATE KEY-----.*?(?:-----END EC PRIVATE KEY-----|\z)|", // ubs:ignore — public key-block regex, not embedded credentials.
    r"-----BEGIN DSA PRIVATE KEY-----.*?(?:-----END DSA PRIVATE KEY-----|\z)|", // ubs:ignore — public key-block regex, not embedded credentials.
    r"-----BEGIN OPENSSH PRIVATE KEY-----.*?(?:-----END OPENSSH PRIVATE KEY-----|\z)|", // ubs:ignore — public key-block regex, not embedded credentials.
    r"-----BEGIN PRIVATE KEY-----.*?(?:-----END PRIVATE KEY-----|\z)|", // ubs:ignore — public key-block regex, not embedded credentials.
    r"-----BEGIN ENCRYPTED PRIVATE KEY-----.*?(?:-----END ENCRYPTED PRIVATE KEY-----|\z)|", // ubs:ignore — public key-block regex, not embedded credentials.
    r"-----BEGIN PGP PRIVATE KEY BLOCK-----.*?(?:-----END PGP PRIVATE KEY BLOCK-----|\z)", // ubs:ignore — public key-block regex, not embedded credentials.
    r")",
);
pub(crate) const DATABASE_URL_PATTERN: &str =
    r#"(?i)\b(postgres|postgresql|mysql|mongodb(?:\+srv)?|redis|amqp)://[^\s'"]{8,}"#;
pub(crate) const GENERIC_SECRET_ASSIGNMENT_PATTERN: &str = r#"(?i)\b(?:api[ _-]?(?:key|secret|token)|auth[ _-]?token|access[ _-]?(?:token|key)|secret[ _-]?key|session[ _-]?token|password|passwd|passphrase|token|secret|authorization)\s*[:=]\s*(?:"(?:\\.|[^"\\\r\n]){4,}"|'(?:\\.|[^'\\\r\n]){4,}'|[^\s,;}\]]{8,})"#;
pub(crate) const SLACK_TOKEN_PATTERN: &str = r"\bxox[bpsaor]-[A-Za-z0-9\-]{10,}";
pub(crate) const STRIPE_KEY_PATTERN: &str = r"\b[spr]k_live_[A-Za-z0-9]{20,}";

/// All built-in patterns, compiled once on first use.
static SECRET_PATTERNS: Lazy<Vec<SecretPattern>> = Lazy::new(|| {
    vec![
        // AWS access key IDs: AKIA for long-lived IAM credentials and ASIA
        // for temporary STS credentials.
        SecretPattern {
            pattern: AWS_ACCESS_KEY_PATTERN,
            regex: Regex::new(AWS_ACCESS_KEY_PATTERN).expect("aws access key regex"),
        },
        // AWS Secret Key in assignment context
        SecretPattern {
            pattern: AWS_SECRET_KEY_PATTERN,
            regex: Regex::new(AWS_SECRET_KEY_PATTERN).expect("aws secret regex"),
        },
        // AWS STS session/security token in configuration context.
        SecretPattern {
            pattern: AWS_SESSION_TOKEN_PATTERN,
            regex: Regex::new(AWS_SESSION_TOKEN_PATTERN).expect("aws session token regex"),
        },
        // GitHub classic/app tokens and fine-grained PATs.
        SecretPattern {
            pattern: GITHUB_TOKEN_PATTERN,
            regex: Regex::new(GITHUB_TOKEN_PATTERN).expect("github token regex"),
        },
        // OpenAI legacy, project, and admin API keys.
        SecretPattern {
            pattern: OPENAI_API_KEY_PATTERN,
            regex: Regex::new(OPENAI_API_KEY_PATTERN).expect("openai key regex"),
        },
        // Anthropic API keys, including current apiNN segmented keys.
        SecretPattern {
            pattern: ANTHROPIC_API_KEY_PATTERN,
            regex: Regex::new(ANTHROPIC_API_KEY_PATTERN).expect("anthropic key regex"),
        },
        // Bearer tokens in authorization headers
        SecretPattern {
            pattern: BEARER_TOKEN_PATTERN,
            regex: Regex::new(BEARER_TOKEN_PATTERN).expect("bearer token regex"),
        },
        // JWT tokens (eyJ...)
        SecretPattern {
            pattern: JWT_PATTERN,
            regex: Regex::new(JWT_PATTERN).expect("jwt regex"),
        },
        // PEM/OpenSSH/PGP private-key blocks. Match through the corresponding
        // footer, or through end-of-input for a truncated paste. Redacting
        // only the header leaves the encoded private key searchable.
        SecretPattern {
            pattern: PRIVATE_KEY_BLOCK_PATTERN,
            regex: Regex::new(PRIVATE_KEY_BLOCK_PATTERN).expect("private key block regex"),
        },
        // Database connection URLs with credentials
        SecretPattern {
            pattern: DATABASE_URL_PATTERN,
            regex: Regex::new(DATABASE_URL_PATTERN).expect("db url regex"),
        },
        // Generic key/token/secret/password assignments
        SecretPattern {
            pattern: GENERIC_SECRET_ASSIGNMENT_PATTERN,
            regex: Regex::new(GENERIC_SECRET_ASSIGNMENT_PATTERN)
                .expect("generic secret assignment regex"),
        },
        // Slack tokens (xoxb-, xoxp-, xoxs-, xoxa-, xoxo-, xoxr-).
        SecretPattern {
            pattern: SLACK_TOKEN_PATTERN,
            regex: Regex::new(SLACK_TOKEN_PATTERN).expect("slack token regex"),
        },
        // Stripe keys (sk_live_, pk_live_, rk_live_)
        SecretPattern {
            pattern: STRIPE_KEY_PATTERN,
            regex: Regex::new(STRIPE_KEY_PATTERN).expect("stripe key regex"),
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
    apply_replacements(input, &matches)
}

/// Ordered per-pattern replacement passes for an input whose RegexSet
/// prefilter already reported candidate matches. Shared by the plain
/// [`redact_text`] path and the memoizing miss path so the candidate
/// scan runs exactly once per input. Replacement order (ascending
/// pattern index, sequential `replace_all`) is part of the frozen
/// behavior contract — see `redact_text_reference` in the tests.
fn apply_replacements<'a>(input: &'a str, matches: &regex::SetMatches) -> Cow<'a, str> {
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

/// Insert a redacted JSON object entry without discarding an earlier value
/// whose distinct source key redacted to the same placeholder. Generated
/// suffixes contain only public punctuation/digits, so collision handling
/// never reintroduces source-key bytes.
pub(crate) fn insert_redacted_json_entry(
    object: &mut serde_json::Map<String, serde_json::Value>,
    next_suffixes: &mut HashMap<String, usize>,
    redacted_key: String,
    value: serde_json::Value,
) {
    if !object.contains_key(&redacted_key) {
        object.insert(redacted_key, value);
        return;
    }

    let next_suffix = next_suffixes.entry(redacted_key.clone()).or_insert(2);
    let mut candidate = String::with_capacity(redacted_key.len() + 21);
    loop {
        candidate.clear();
        candidate.push_str(&redacted_key);
        candidate.push('#');
        let _ = write!(&mut candidate, "{next_suffix}");
        *next_suffix = next_suffix.saturating_add(1);
        if !object.contains_key(&candidate) {
            object.insert(candidate, value);
            return;
        }
    }
}

/// Redact secrets from a JSON value, recursively walking strings.
///
/// - String values are redacted in-place.
/// - Values under credential-bearing object keys are replaced in full.
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
            let mut new_obj = serde_json::Map::with_capacity(obj.len());
            let mut next_suffixes = HashMap::new();
            for (k, v) in obj {
                let redacted_key = redact_text(k).into_owned();
                let redacted_value = redact_sensitive_json_value(k, v, || redact_json(v));
                insert_redacted_json_entry(
                    &mut new_obj,
                    &mut next_suffixes,
                    redacted_key,
                    redacted_value,
                );
            }
            serde_json::Value::Object(new_obj)
        }
        other => other.clone(),
    }
}

#[doc(hidden)]
pub fn fuzz_redact_json_with_memoizing_redactor(
    value: &serde_json::Value,
    capacity: usize,
) -> serde_json::Value {
    MemoizingRedactor::with_capacity(capacity.clamp(1, 1024)).redact_json(value)
}

/// Returns true if index-time redaction is enabled (default: true).
///
/// Operator control, checked in order:
///
/// 1. `CASS_INDEX_REDACTION` — the documented switch.
///    - `full` (default): secret redaction runs on every persisted
///      message body, title, snippet, and metadata blob before anything
///      reaches SQLite or the lexical index.
///    - `off`: redaction is skipped entirely at index time. **Raw text
///      is then indexed as-is.** Note the trade honestly: the original
///      session files AND the cass raw-mirror blobs
///      (`<data_dir>/raw-mirror/v1/`, captured unredacted with
///      encryption state "none") already contain the same raw text on
///      the same disk, so index-time redaction protects the *queryable
///      surfaces* (DB rows, FTS/lexical index, exports, robot output),
///      not disk-at-rest secrecy. `off` trades that protection for
///      ingest speed. Accepted aliases: `off|0|false|no|none|disabled`
///      and `full|on|1|true|yes`.
///    - Any other value: warn and default to `full`.
/// 2. Legacy `CASS_REDACT_SECRETS` — `0|false|off|no` disables. Kept
///    for backward compatibility; `CASS_INDEX_REDACTION` wins when both
///    are set.
pub fn redaction_enabled() -> bool {
    if let Ok(val) = dotenvy::var("CASS_INDEX_REDACTION") {
        let normalized = val.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "off" | "0" | "false" | "no" | "none" | "disabled" => return false,
            "full" | "on" | "1" | "true" | "yes" => return true,
            // Empty behaves as unset: fall through to the legacy switch.
            "" => {}
            other => {
                tracing::warn!(
                    value = %other,
                    "unrecognized CASS_INDEX_REDACTION value; defaulting to full redaction"
                );
                return true;
            }
        }
    }
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
/// Prefilter-first scope (redaction-perf campaign, xu3jq): the cache is
/// consulted only for inputs whose RegexSet prefilter reports at least
/// one candidate pattern match. Clean inputs — the overwhelming
/// majority of real corpora — bypass hashing, LRU bookkeeping, and
/// audit records entirely, because profiling showed that bookkeeping
/// costing ~18x the actual regex scanning on a 21.8MB real codex
/// corpus. Hit/miss/insert telemetry therefore describes
/// candidate-bearing content only.
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

    /// Byte ceiling for memoizing a single input (xu3jq round 3).
    /// Candidate-bearing inputs larger than this are redacted directly
    /// and never enter the cache: caching them would pin up to
    /// `capacity x input_size` of message bodies in memory (the
    /// original giant-rollout incident ran the host into swap), while
    /// the hit-rate on multi-hundred-KB distinct tool outputs is ~0.
    /// 64 KiB bounds worst-case cache value memory at
    /// ~4096 x 64KiB = 256 MiB and still covers every capped codex
    /// message body (128 KiB content cap applies upstream, but titles,
    /// metadata blobs, and extra_json strings are typically far
    /// smaller).
    pub(crate) const MAX_MEMOIZED_INPUT_BYTES: usize = 64 * 1024;

    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            text_cache: crate::indexer::memoization::ContentAddressedMemoCache::with_capacity(
                capacity,
            ),
            algorithm_fingerprint: redaction_algorithm_fingerprint(),
        }
    }

    pub(crate) fn new() -> Self {
        Self::with_capacity(Self::configured_capacity())
    }

    /// Resolve the memo-cache capacity, honoring the optional
    /// `CASS_REDACT_MEMO_CAPACITY` override (#291). On a very large,
    /// subagent-heavy corpus the 4096 default thrashes ~one eviction per
    /// insert; operators can raise the ceiling to cut that churn. A `0`,
    /// empty, or unparseable value falls back to the default.
    pub(crate) fn configured_capacity() -> usize {
        dotenvy::var("CASS_REDACT_MEMO_CAPACITY")
            .ok()
            .and_then(|raw| raw.trim().parse::<usize>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(Self::DEFAULT_CAPACITY)
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
        // Prefilter-first fast path (redaction-perf campaign, xu3jq):
        // run the RegexSet candidate scan BEFORE any cache machinery.
        // Profiling the previous shape on a real 21.8MB codex corpus
        // showed the memo bookkeeping (blake3 content hashing, MemoKey
        // clones, and O(capacity) VecDeque LRU scans in touch/retain)
        // costing ~18x the actual secret scanning, because the
        // overwhelming majority of message bodies contain no secret
        // candidates at all. Clean inputs now pay exactly one RegexSet
        // scan plus the one unavoidable copy into the owned return
        // value — no hashing, no LRU traffic, no audit records. Only
        // candidate-bearing inputs (rare, and the ones whose ordered
        // replace_all passes are genuinely expensive) consult the
        // cache. Consequence: cache hit/miss/insert audit telemetry now
        // describes candidate-bearing content only; clean content is
        // silent by design.
        let matches = SECRET_REGEX_SET.matches(input);
        if !matches.matched_any() {
            return (input.to_owned(), Vec::new());
        }
        // Oversized candidate-bearing inputs: redact directly, never
        // cache (see MAX_MEMOIZED_INPUT_BYTES for the memory-bound
        // rationale). Output is identical to the cached path by
        // construction — both run apply_replacements on the same
        // matches.
        if input.len() > Self::MAX_MEMOIZED_INPUT_BYTES {
            return (apply_replacements(input, &matches).into_owned(), Vec::new());
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
                let redacted = apply_replacements(input, &matches).into_owned();
                (redacted, vec![lookup_audit])
            }
            crate::indexer::memoization::MemoLookup::Miss => {
                let redacted = apply_replacements(input, &matches).into_owned();
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
        // are routine churn on large corpora (debug — #291: at info they
        // pegged a core with 30k+ lines in minutes), invalidations and
        // quarantines are alarming enough to warn so they show up in
        // default-level logs without dredging.
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
            MemoCacheEvent::Evict { ref reason } => tracing::debug!(
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
                let mut next_suffixes = HashMap::new();
                for (k, v) in obj {
                    let redacted_key = self.redact_text(k);
                    let redacted_value = redact_sensitive_json_value(k, v, || self.redact_json(v));
                    insert_redacted_json_entry(
                        &mut new_obj,
                        &mut next_suffixes,
                        redacted_key,
                        redacted_value,
                    );
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

    /// FROZEN reference implementation of the redaction algorithm as of
    /// the 2026-08 redaction-perf campaign baseline. This is a verbatim
    /// copy of the pre-optimization `redact_text` body (RegexSet
    /// prefilter + ordered per-pattern `replace_all` passes). Every
    /// optimization to the production paths (`redact_text`,
    /// `MemoizingRedactor::redact_text`, `redact_json`) must stay
    /// byte-identical to THIS function on arbitrary input — enforced by
    /// `production_redaction_paths_match_frozen_reference` below. Do
    /// not "modernize" this copy alongside a production change; it is
    /// the fixed point the equivalence proof hangs on. (A deliberate
    /// pattern-list change is the one legitimate reason to update it,
    /// together with the algorithm fingerprint bump.)
    fn redact_text_reference(input: &str) -> Cow<'_, str> {
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

    #[test]
    fn redacts_openai_key() {
        let input = "my key is sk-ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij";
        let output = redact_text(input);
        assert_eq!(output, "my key is [REDACTED]");
        assert!(!output.contains("sk-ABCDE"));

        for current in [
            "sk-proj-AbCdEf_0123456789-xYz987654321",
            "sk-admin-AbCdEf_0123456789-xYz987654321",
        ] {
            assert_eq!(redact_text(current), REDACTED);
        }
        assert_eq!(
            redact_text("sk-project-AbCdEf_0123456789-xYz987654321"),
            "sk-project-AbCdEf_0123456789-xYz987654321"
        );
    }

    #[test]
    fn redacts_anthropic_key() {
        let input = "sk-ant-ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij";
        let output = redact_text(input);
        assert_eq!(output, "[REDACTED]");
        assert_eq!(
            redact_text("sk-ant-api03-AbCdEf_0123456789-xYz987654321"),
            REDACTED
        );
        assert_eq!(redact_text("sk-ant-api03-short"), "sk-ant-api03-short");
    }

    #[test]
    fn redacts_github_pat() {
        let input = "token ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij";
        let output = redact_text(input);
        assert_eq!(output, "token [REDACTED]");
        assert_eq!(
            redact_text("github_pat_AbCdEf_0123456789_xYz987654321"),
            REDACTED
        );
        assert_eq!(redact_text("github_pat_short"), "github_pat_short");
    }

    #[test]
    fn redacts_bearer_token() {
        let input = "Authorization: Bearer eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9.payload.signature";
        let output = redact_text(input);
        assert!(!output.contains("eyJhbGci"));
    }

    #[test]
    fn redacts_aws_access_key() -> Result<(), String> {
        for input in ["AKIAIOSFODNN7EXAMPLE", "ASIAIOSFODNN7EXAMPLE"] {
            if redact_text(input) != "[REDACTED]" {
                return Err(format!("access-key prefix was not redacted in {input}"));
            }
        }
        for near_miss in ["ASIAIOSFODNN7EXAMPL", "asiaiosfodnn7example"] {
            if redact_text(near_miss) != near_miss {
                return Err(format!("near-miss access key was redacted: {near_miss}"));
            }
        }
        Ok(())
    }

    #[test]
    fn redacts_private_key_header() -> Result<(), String> {
        let input = "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAK...";
        let output = redact_text(input);
        (output == "[REDACTED]" && !output.contains("MIIEowIBAAK"))
            .then_some(())
            .ok_or_else(|| format!("truncated private-key body remained visible: {output}"))
    }

    #[test]
    fn redacts_complete_and_truncated_private_key_bodies() -> Result<(), String> {
        fn require(condition: bool, message: &'static str) -> Result<(), String> {
            condition.then_some(()).ok_or_else(|| message.to_owned())
        }

        let complete = "before\n-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBgkqhkiG9w0BAQEFAASC\n-----END PRIVATE KEY-----\nafter";
        require(
            redact_text(complete) == "before\n[REDACTED]\nafter",
            "complete PKCS#8 key was not fully redacted",
        )?;

        let encrypted = "-----BEGIN ENCRYPTED PRIVATE KEY-----\nENCRYPTEDSECRETBODY\n-----END ENCRYPTED PRIVATE KEY-----";
        require(
            redact_text(encrypted) == "[REDACTED]",
            "encrypted private key was not fully redacted",
        )?;

        let truncated = "prefix\n-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA";
        let output = redact_text(truncated);
        require(
            output == "prefix\n[REDACTED]" && !output.contains("b3BlbnNzaC1rZXktdjE"),
            "truncated OpenSSH private key was not fully redacted",
        )?;

        let mismatched_footer = "-----BEGIN RSA PRIVATE KEY-----\nFIRST_SECRET_HALF\n-----END EC PRIVATE KEY-----\nSECOND_SECRET_HALF\n-----END RSA PRIVATE KEY-----\nafter"; // ubs:ignore — synthetic malformed-key fixture verifies fail-closed redaction.
        let output = redact_text(mismatched_footer);
        require(
            output == "[REDACTED]\nafter"
                && !output.contains("FIRST_SECRET_HALF")
                && !output.contains("SECOND_SECRET_HALF"),
            "mismatched footer terminated private-key redaction early",
        )
    }

    #[test]
    fn redacts_generic_api_key_assignment() {
        for input in [
            "api_key=abcdefgh12345678",
            "password=\"correct horse battery staple!\"",
            "password=P@ssw0rd!",
            "api_key:'abc.def$ghi'",
            "AWS_SESSION_TOKEN=AQoEXAMPLE0123456789/value+=",
        ] {
            assert_eq!(redact_text(input), REDACTED, "secret survived in {input:?}");
        }
        for near_miss in ["password=short", "AWS_SESSION_TOKEN=short"] {
            assert_eq!(redact_text(near_miss), near_miss);
        }
    }

    #[test]
    fn redacts_database_url() {
        for input in [
            "DATABASE_URL=postgres://user:pass@host:5432/db",
            "mongodb+srv://user:pass@cluster.mongodb.net/db",
            "amqp://user:pass@broker.internal/vhost",
        ] {
            let output = redact_text(input);
            assert!(
                !output.contains("user:pass"),
                "credential URL survived: {output}"
            );
        }
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
        for input in ["xoxb-123456789-abcdefghij", "xoxo-123456789-abcdefghij"] {
            assert_eq!(redact_text(input), REDACTED);
        }
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
    fn redacted_json_key_collisions_preserve_every_value_without_leaking_keys() -> Result<(), String>
    {
        let input = json!({
            "api_key=abcdefgh12345678": "first", // ubs:ignore — synthetic collision fixture, not a credential.
            "password=abcdefgh12345678": "second", // ubs:ignore — synthetic collision fixture, not a credential.
            "[REDACTED]#2": "preexisting",
        });

        let plain = redact_json(&input);
        let memoized = MemoizingRedactor::with_capacity(8).redact_json(&input);
        if plain != memoized {
            return Err("plain and memoized JSON walkers disagreed".to_owned());
        }

        let object = plain
            .as_object()
            .ok_or_else(|| "redacted JSON was not an object".to_owned())?;
        if object.len() != 3 {
            return Err("redaction overwrote an object value".to_owned());
        }
        let mut values = object
            .values()
            .filter_map(serde_json::Value::as_str)
            .collect::<Vec<_>>();
        values.sort_unstable();
        if values != ["first", "preexisting", "second"] {
            return Err(format!("redacted object values changed: {values:?}"));
        }
        object
            .keys()
            .all(|key| !key.contains("abcdefgh12345678"))
            .then_some(())
            .ok_or_else(|| "collision suffix leaked source-key bytes".to_owned())
    }

    #[test]
    fn structured_credential_fields_redact_values_by_key_semantics() -> Result<(), String> {
        let input = json!({
            "password": "correct horse battery staple!", // ubs:ignore -- synthetic redaction fixture.
            "API-Key": "abc.def$ghi", // ubs:ignore -- synthetic redaction fixture.
            "aws_secret_access_key": "0123456789012345678901234567890123456789", // ubs:ignore -- synthetic redaction fixture.
            "AWS_SESSION_TOKEN": "AQoEXAMPLE-session/value+=with.punctuation", // ubs:ignore -- synthetic redaction fixture.
            "pin": 123456,
            "credentials": {
                "opaque": [true, 42, "not-pattern-shaped"]
            },
            "nested": {
                "clientSecret": ["short", "values"],
                "private_key_pem": {"body": "short"},
                "sshPrivateKey": "opaque-short-key",
                "client_secret_key": "opaque-short-key",
                "rails_secret_key_base": "opaque-short-key",
                "service_api_keys": ["opaque-short-key"],
                "service_credentials": ["opaque-short-credential"],
                "oauth_token": "opaque-short-value",
                "service_password_hash": "not-pattern-shaped",
                "cookie": "session=short",
                "connection_string": "custom-driver opaque value"
            },
            "null_password": null,
            "keyframe": "animation-safe",
            "monkey": "animal-safe",
            "token_count": 2048,
            "private_key_count": 2,
            "secret_key_enabled": true,
            "public_key": "ssh-ed25519 AAAATESTPUBLICMATERIAL",
        });

        let plain = redact_json(&input);
        let memoized = MemoizingRedactor::with_capacity(32).redact_json(&input);
        if plain != memoized {
            return Err("plain and memoized key-aware JSON redaction diverged".to_owned());
        }

        for pointer in [
            "/password",
            "/API-Key",
            "/aws_secret_access_key",
            "/AWS_SESSION_TOKEN",
            "/pin",
            "/credentials",
            "/nested/clientSecret",
            "/nested/private_key_pem",
            "/nested/sshPrivateKey",
            "/nested/client_secret_key",
            "/nested/rails_secret_key_base",
            "/nested/service_api_keys",
            "/nested/service_credentials",
            "/nested/oauth_token",
            "/nested/service_password_hash",
            "/nested/cookie",
            "/nested/connection_string",
        ] {
            if plain.pointer(pointer) != Some(&json!(REDACTED)) {
                return Err(format!("sensitive field was not fully redacted: {pointer}"));
            }
        }

        for pointer in [
            "/null_password",
            "/keyframe",
            "/monkey",
            "/token_count",
            "/private_key_count",
            "/secret_key_enabled",
            "/public_key",
        ] {
            if plain.pointer(pointer) != input.pointer(pointer) {
                return Err(format!("safe near-miss field changed: {pointer}"));
            }
        }
        Ok(())
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

    /// `CASS_INDEX_REDACTION` is the documented operator switch for
    /// index-time redaction: `full` (default) / `off`, with precedence
    /// over the legacy `CASS_REDACT_SECRETS` toggle and a warn+default
    /// path for unrecognized values.
    #[test]
    #[serial]
    fn cass_index_redaction_switch_controls_and_overrides_legacy() {
        // Safety: serial test context; single-threaded env mutation.
        unsafe {
            std::env::remove_var("CASS_INDEX_REDACTION");
            std::env::remove_var("CASS_REDACT_SECRETS");
        }
        assert!(redaction_enabled(), "default must be full redaction");

        unsafe { std::env::set_var("CASS_INDEX_REDACTION", "off") };
        assert!(!redaction_enabled(), "off must disable redaction");
        unsafe { std::env::set_var("CASS_INDEX_REDACTION", "OFF") };
        assert!(!redaction_enabled(), "value must be case-insensitive");
        unsafe { std::env::set_var("CASS_INDEX_REDACTION", "full") };
        assert!(redaction_enabled(), "full must enable redaction");

        // Precedence: CASS_INDEX_REDACTION wins over the legacy switch
        // in BOTH directions.
        unsafe {
            std::env::set_var("CASS_INDEX_REDACTION", "full");
            std::env::set_var("CASS_REDACT_SECRETS", "0");
        }
        assert!(
            redaction_enabled(),
            "explicit full must override legacy disable"
        );
        unsafe {
            std::env::set_var("CASS_INDEX_REDACTION", "off");
            std::env::set_var("CASS_REDACT_SECRETS", "1");
        }
        assert!(
            !redaction_enabled(),
            "explicit off must override legacy enable"
        );

        // Unrecognized value: fail safe to full redaction.
        unsafe {
            std::env::set_var("CASS_INDEX_REDACTION", "lazy");
            std::env::remove_var("CASS_REDACT_SECRETS");
        }
        assert!(
            redaction_enabled(),
            "unrecognized value must default to full redaction"
        );

        // Empty value behaves as unset: legacy switch applies again.
        unsafe {
            std::env::set_var("CASS_INDEX_REDACTION", "");
            std::env::set_var("CASS_REDACT_SECRETS", "0");
        }
        assert!(
            !redaction_enabled(),
            "empty CASS_INDEX_REDACTION must fall through to legacy switch"
        );

        unsafe {
            std::env::remove_var("CASS_INDEX_REDACTION");
            std::env::remove_var("CASS_REDACT_SECRETS");
        }
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

    /// Repeated secret-bearing values inside metadata / extra_json are
    /// common in salvage replays. The memoized JSON walker must reuse
    /// the cached redaction for repeated candidate-bearing scalars
    /// instead of re-running the replacement passes for every copy —
    /// while clean keys and clean values bypass the cache entirely
    /// (prefilter-first, redaction-perf campaign).
    #[test]
    fn memoizing_redactor_redact_json_reuses_repeated_keys_and_values() {
        let repeated_secret =
            "Authorization: Bearer eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9.payload.signature";
        let repeated_note = "same assistant boilerplate without secrets";
        // The field name must NOT be a sensitive key: gh#419's key-aware
        // walker replaces values under sensitive keys (`token`, `secret`, …)
        // wholesale without ever consulting redact_text, so such values can
        // never exercise the memo cache this test pins. A neutral key routes
        // the candidate-bearing scalar through the cached text path.
        let value = json!({
            "events": [
                {"line": repeated_secret, "note": repeated_note},
                {"line": repeated_secret, "note": repeated_note},
                {"line": repeated_secret, "note": repeated_note},
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
            stats.misses, 1,
            "only the first occurrence of the candidate-bearing secret value should miss; clean keys/values bypass the cache"
        );
        assert_eq!(
            stats.inserts, 1,
            "only the distinct candidate-bearing value should be inserted"
        );
        assert_eq!(
            stats.hits, 2,
            "repeated candidate-bearing values should hit the memo cache; clean strings never do"
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

    /// Oversized candidate-bearing inputs (> MAX_MEMOIZED_INPUT_BYTES)
    /// must be redacted correctly WITHOUT entering the cache — the
    /// memory bound that prevents the memo cache from pinning
    /// gigabytes of giant tool outputs (xu3jq round 3).
    #[test]
    fn memoizing_redactor_oversized_candidate_input_redacts_without_caching() {
        let mut redactor = MemoizingRedactor::with_capacity(8);
        let mut giant = "x".repeat(MemoizingRedactor::MAX_MEMOIZED_INPUT_BYTES);
        giant.push_str(" sk-ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij tail");
        assert!(giant.len() > MemoizingRedactor::MAX_MEMOIZED_INPUT_BYTES);

        let (output, audit) = redactor.redact_text_with_audit(&giant);
        assert!(
            !output.contains("sk-ABCDE"),
            "oversized input must still be redacted"
        );
        assert_eq!(
            output,
            redact_text(&giant).into_owned(),
            "oversized bypass must match the plain redaction path byte-for-byte"
        );
        assert!(
            audit.is_empty(),
            "oversized input must not produce cache audit records"
        );
        let _ = redactor.redact_text(&giant);
        let stats = redactor.stats();
        assert_eq!(stats.misses, 0, "oversized input must never miss the cache");
        assert_eq!(stats.inserts, 0, "oversized input must never be inserted");
        assert_eq!(stats.hits, 0, "oversized input must never hit the cache");

        // A small candidate-bearing input still uses the cache.
        let small = "token ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij";
        let _ = redactor.redact_text(small);
        let _ = redactor.redact_text(small);
        assert_eq!(redactor.stats().misses, 1);
        assert_eq!(redactor.stats().hits, 1);
    }

    /// Prefilter-first bypass (redaction-perf campaign): inputs with
    /// no secret candidates must never touch the memo cache — no miss,
    /// no insert, no audit records — while still returning the input
    /// text unchanged. This pins the fast path that removed the ~18x
    /// memo-bookkeeping overhead on clean corpora.
    #[test]
    fn memoizing_redactor_clean_input_bypasses_cache_entirely() {
        let mut redactor = MemoizingRedactor::with_capacity(8);
        let clean = "no secret here, just a sentence";
        let (output, audit) = redactor.redact_text_with_audit(clean);
        assert_eq!(output, clean, "clean input must pass through unchanged");
        assert!(
            audit.is_empty(),
            "clean input must not produce cache audit records"
        );
        let _ = redactor.redact_text(clean);
        let stats = redactor.stats();
        assert_eq!(stats.misses, 0, "clean input must not count as miss");
        assert_eq!(stats.hits, 0, "clean input must not count as hit");
        assert_eq!(stats.inserts, 0, "clean input must not insert");
        // Invalidate on never-cached clean content is a no-op.
        assert!(!redactor.invalidate(clean));
    }

    /// Invalidate must remove the cached entry so the next call is a
    /// miss + re-insert. Pin the changed/no-op semantics so a caller
    /// can rely on the boolean return value to know whether anything
    /// was actually evicted. (Payload carries a secret candidate:
    /// since the prefilter-first bypass, only candidate-bearing
    /// content enters the cache at all.)
    #[test]
    fn memoizing_redactor_invalidate_drops_cached_entry() {
        let mut redactor = MemoizingRedactor::with_capacity(8);
        let payload = "token ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij here";

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

    /// Golden corpus for the redaction-perf campaign: literal expected
    /// outputs for one planted secret per pattern class PLUS
    /// adversarial near-misses that must pass through untouched. These
    /// are byte-pinned so any optimization that changes output shape
    /// fails loudly. Checked against the plain path, a fresh memoizing
    /// redactor, and a warmed (cache-hit) memoizing redactor.
    #[test]
    fn golden_redaction_corpus_is_byte_stable_across_paths() {
        let ghp = format!("ghp_{}", "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij");
        let sk_live = format!("{}_{}", "sk_live", "AAAABBBBCCCCDDDDEEEEFFFFGGGG");
        let cases: Vec<(String, String)> = vec![
            // --- planted secrets, one per pattern class ---
            (
                "aws AKIAIOSFODNN7EXAMPLE key".into(),
                "aws [REDACTED] key".into(),
            ),
            (format!("token {ghp} end"), "token [REDACTED] end".into()),
            (
                "openai sk-ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij".into(),
                "openai [REDACTED]".into(),
            ),
            (
                "sk-ant-ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij".into(),
                "[REDACTED]".into(),
            ),
            (
                "Authorization: Bearer abcdefghijklmnopqrstuvwxyz1234".into(),
                "Authorization: [REDACTED]".into(),
            ),
            (
                "jwt eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiIxMjMifQ.c2lnbmF0dXJl done".into(),
                "jwt [REDACTED] done".into(),
            ),
            (
                "-----BEGIN RSA PRIVATE KEY-----".into(),
                "[REDACTED]".into(),
            ),
            (
                "url postgres://user:pass@host:5432/db".into(),
                "url [REDACTED]".into(),
            ),
            ("api_key=abcdefgh12345678".into(), "[REDACTED]".into()),
            (
                "slack xoxb-123456789-abcdefghij".into(),
                "slack [REDACTED]".into(),
            ),
            (format!("stripe {sk_live}!"), "stripe [REDACTED]!".into()),
            // --- multi-secret input: two patterns fire in one string ---
            (
                format!("a=sk-ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij b={ghp}"),
                "a=[REDACTED] b=[REDACTED]".into(),
            ),
            // --- adversarial near-misses: MUST pass through unchanged ---
            ("sk-abc".into(), "sk-abc".into()), // too short
            ("AKIAIOSFODN7EXAMPL".into(), "AKIAIOSFODN7EXAMPL".into()), // 14 chars, not 16
            ("akiaiosfodnn7example".into(), "akiaiosfodnn7example".into()), // lowercase AKIA
            (
                "ghx_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij".into(),
                "ghx_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij".into(),
            ), // bad PAT prefix
            ("eyJhbGciOiJSUzI1NiJ9".into(), "eyJhbGciOiJSUzI1NiJ9".into()), // single JWT segment
            ("Bearer short".into(), "Bearer short".into()), // bearer token too short
            (
                "xoxz-123456789-abcdefghij".into(),
                "xoxz-123456789-abcdefghij".into(),
            ), // bad slack prefix
            (
                format!("{}_{}", "sk_test", "AAAABBBBCCCCDDDDEEEEFFFFGGGG"),
                format!("{}_{}", "sk_test", "AAAABBBBCCCCDDDDEEEEFFFFGGGG"),
            ), // test-mode stripe key
            ("api_key=short".into(), "api_key=short".into()), // value below 8 chars
            (
                "-----BEGIN CERTIFICATE-----".into(),
                "-----BEGIN CERTIFICATE-----".into(),
            ),
            (
                "visit https://example.com/path for docs".into(),
                "visit https://example.com/path for docs".into(),
            ),
            (
                "plain prose with no secrets at all".into(),
                "plain prose with no secrets at all".into(),
            ),
            ("".into(), "".into()),
            (
                "🔐 unicode near sk-abc miss 测试".into(),
                "🔐 unicode near sk-abc miss 测试".into(),
            ),
        ];

        let mut warmed = MemoizingRedactor::with_capacity(128);
        // Prime the cache so the second pass below exercises the hit path.
        for (input, _) in &cases {
            let _ = warmed.redact_text(input);
        }
        for (input, expected) in &cases {
            assert_eq!(
                &redact_text(input).into_owned(),
                expected,
                "plain redact_text golden mismatch for {input:?}"
            );
            assert_eq!(
                &redact_text_reference(input).into_owned(),
                expected,
                "frozen reference golden mismatch for {input:?}"
            );
            let mut fresh = MemoizingRedactor::with_capacity(128);
            assert_eq!(
                &fresh.redact_text(input),
                expected,
                "fresh memoizing redactor golden mismatch for {input:?}"
            );
            assert_eq!(
                &warmed.redact_text(input),
                expected,
                "warmed (cache-hit) memoizing redactor golden mismatch for {input:?}"
            );
        }
    }

    /// Property-based equivalence gate for the redaction-perf campaign:
    /// on inputs assembled from adversarial fragments (real secret
    /// shapes, near-misses, whitespace variants, unicode, JSON-ish
    /// punctuation), every production path must be byte-identical to
    /// the FROZEN pre-optimization reference:
    ///   redact_text == MemoizingRedactor (miss) == MemoizingRedactor
    ///   (hit) == redact_text_reference.
    #[test]
    fn production_redaction_paths_match_frozen_reference() {
        use proptest::prelude::*;

        let fragment = proptest::sample::select(vec![
            "sk-ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij",
            "sk-ant-ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij",
            "AKIAIOSFODNN7EXAMPLE",
            "ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij",
            "Bearer abcdefghijklmnopqrstuvwxyz1234",
            "Bearer\tabcdefghijklmnopqrstuvwxyz1234",
            "eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiIxMjMifQ.c2lnbmF0dXJl",
            "-----BEGIN OPENSSH PRIVATE KEY-----",
            "postgres://user:pass@host:5432/db",
            "api_key = \"abcdefgh12345678\"",
            "password:hunter2hunter2",
            "xoxb-123456789-abcdefghij",
            // concat! so push-protection does not treat the fixture as a live Stripe key
            concat!("sk_live_", "AAAABBBBCCCCDDDDEEEEFFFFGGGG"),
            // near-misses & noise
            "sk-abc",
            "AKIA1234",
            "ghx_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij",
            "Bearer x",
            "api_key=short",
            "password=",
            "xoxz-nope",
            "https://example.com/path?q=1",
            "plain words",
            "{\"k\":\"v\"}",
            "\\\"escaped\\\"",
            "🔐测试🗝️",
            " ",
            "\n",
            "\t",
            ":",
            "=",
        ]);
        let input_strategy =
            proptest::collection::vec(fragment, 0..12).prop_map(|parts| parts.concat());

        let mut runner =
            proptest::test_runner::TestRunner::new(proptest::test_runner::Config::with_cases(512));
        runner
            .run(&input_strategy, |input| {
                let reference = redact_text_reference(&input).into_owned();
                let plain = redact_text(&input).into_owned();
                prop_assert_eq!(&plain, &reference, "plain redact_text diverged");
                let mut memo = MemoizingRedactor::with_capacity(64);
                let first = memo.redact_text(&input);
                prop_assert_eq!(&first, &reference, "memoized miss path diverged");
                let second = memo.redact_text(&input);
                prop_assert_eq!(&second, &reference, "memoized hit path diverged");
                Ok(())
            })
            .unwrap();
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
