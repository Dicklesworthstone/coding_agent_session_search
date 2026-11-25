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
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::Arc;

use once_cell::sync::Lazy;
use regex::{Regex, RegexSet};

use crate::indexer::memoization::MemoRetentionBudget;

/// Placeholder inserted where a secret was found.
const REDACTED: &str = "[REDACTED]";

/// One allowance shared by every production redactor, including Rayon workers
/// inside a giant conversation. Entry-count overrides cannot multiply this
/// retained-value ceiling. This measures String buffer capacity, not process
/// RSS, map metadata, allocator retention, or caller-owned output strings.
const REDACTION_RETAINED_VALUE_BYTES: usize = 64 * 1024 * 1024;
static REDACTION_RETENTION: Lazy<Arc<MemoRetentionBudget>> =
    Lazy::new(|| MemoRetentionBudget::new(REDACTION_RETAINED_VALUE_BYTES));

/// The resident-memory sampler is the only production policy writer. It uses
/// its already-hysteretic byte allowance, not CPU load, to shrink retained
/// values. No per-message OS probes or new sampler thread are introduced.
pub(super) fn update_redaction_retention_limit(inflight_byte_allowance: usize) {
    apply_redaction_retention_limit(&REDACTION_RETENTION, inflight_byte_allowance);
}

fn apply_redaction_retention_limit(
    retention: &MemoRetentionBudget,
    inflight_byte_allowance: usize,
) {
    retention.set_limit(inflight_byte_allowance.min(REDACTION_RETAINED_VALUE_BYTES));
}

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
            | "proxyauthorization"
            | "httpauthorization"
            | "httpproxyauthorization"
            | "cookie"
            | "setcookie"
            | "httpcookie"
            | "httpsetcookie"
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

// Word boundaries are ASCII (`(?-u:\b)`), never Unicode `\b`. A Unicode word
// boundary stops the regex crate's lazy DFA on any haystack with a non-ASCII
// byte, and the whole RegexSet then runs on the PikeVM: on 593 MiB of real
// session text the Unicode set scanned 10 MiB/s and the ASCII set 261-284 MiB/s,
// with identical pattern matches on all 3.1M strings. Every boundary here sits
// next to an ASCII word character, and ASCII word characters are a subset of
// Unicode ones, so the ASCII form matches wherever the Unicode form did (plus a
// token directly touching a non-ASCII letter): redaction can only get stricter.
pub(crate) const AWS_ACCESS_KEY_PATTERN: &str = r"(?-u:\b)(?:AKIA|ASIA)[0-9A-Z]{16}(?-u:\b)";
// Quoted assignments can contain LF/CRLF and escaped newlines or quotes.
// A truncated quoted value is sensitive through EOF, including a final escape.
// Keep the established minimum lengths and unquoted alternatives.
pub(crate) const AWS_SECRET_KEY_PATTERN: &str = concat!(
    r"(?i)aws(.{0,20})?(secret|access)?[_-]?key\s*[:=]\s*",
    r#"(?:"(?:\\(?s:.)|[^"\\]){40,}(?:"|(?:\\)?\z)|"#,
    r#"'(?:\\(?s:.)|[^'\\]){40,}(?:'|(?:\\)?\z)|[A-Za-z0-9/+=]{40}['"]?)"#,
);
pub(crate) const AWS_SESSION_TOKEN_PATTERN: &str = concat!(
    r"(?i)(?-u:\b)aws[_-]?(?:session|security)[_-]?token\s*[:=]\s*",
    r#"(?:"(?:\\(?s:.)|[^"\\]){8,}(?:"|(?:\\)?\z)|"#,
    r#"'(?:\\(?s:.)|[^'\\]){8,}(?:'|(?:\\)?\z)|[^\s,;}\]]{8,})"#,
);
pub(crate) const GITHUB_TOKEN_PATTERN: &str =
    r"(?-u:\b)(?:gh[pousr]_[A-Za-z0-9]{36}|github_pat_[A-Za-z0-9_]{20,})(?-u:\b)";
pub(crate) const OPENAI_API_KEY_PATTERN: &str =
    r"(?-u:\b)(?:sk-(?:proj-|admin-)[A-Za-z0-9_-]{19,}[A-Za-z0-9_]|sk-[A-Za-z0-9]{20,})(?-u:\b)";
pub(crate) const ANTHROPIC_API_KEY_PATTERN: &str =
    r"(?-u:\b)sk-ant-(?:api[0-9]{2}-)?[A-Za-z0-9_-]{19,}[A-Za-z0-9_](?-u:\b)";
pub(crate) const BEARER_TOKEN_PATTERN: &str = r"(?i)(?-u:\b)Bearer[ \t]+[A-Za-z0-9._~+/=-]{8,}";
// Authentication schemes carry credentials after a separating space, which
// the generic assignment pattern cannot cover. Token68 schemes have a narrow
// payload; parameter schemes occupy the complete logical header value. Only
// HTTP folded continuations cross a newline, never the next unindented header.
// An already-redacted payload is not admitted as a credential.
pub(crate) const HTTP_AUTHORIZATION_PATTERN: &str = concat!(
    r"(?i)(?-u:\b)(?:http[_-])?(?:proxy[_-])?authorization[ \t]*[:=]",
    r"(?:[ \t]|\r?\n[ \t]+)*",
    r"(?:",
    r"(?:basic|negotiate|ntlm|token)(?:[ \t]|\r?\n[ \t]+)+[A-Za-z0-9._~+/-]+=*|",
    r"bearer[ \t]*\r?\n[ \t]+[A-Za-z0-9._~+/-]+=*|",
    r"(?:digest|aws|aws4-hmac-sha256|oauth)(?:[ \t]|\r?\n[ \t]+)+",
    r"[A-Za-z][^\r\n]*(?:\r?\n[ \t]+[^\r\n]+)*",
    r")",
);
// Cookie headers can carry opaque session credentials with no token prefix.
// Require an actual cookie-name assignment after the explicit header/CGI key,
// rather than matching cookie prose. Quoted CGI values retain the existing
// multiline/truncated-secret policy; unquoted values stop at the next header.
pub(crate) const HTTP_COOKIE_PATTERN: &str = concat!(
    r"(?i)(?-u:\b)(?:http[_-])?(?:set[_-])?cookie[ \t]*[:=]",
    r"(?:[ \t]|\r?\n[ \t]+)*",
    r#"(?:"[ \t]*[!#$%&'*+.^_\x60|~0-9A-Za-z-]+[ \t]*=(?:\\(?s:.)|[^"\\])*(?:"|(?:\\)?\z)|"#,
    r#"'[ \t]*[!#$%&'*+.^_\x60|~0-9A-Za-z-]+[ \t]*=(?:\\(?s:.)|[^'\\])*(?:'|(?:\\)?\z)|"#,
    r"[!#$%&'*+.^_\x60|~0-9A-Za-z-]+[ \t]*=[^\r\n]*(?:\r?\n[ \t]+[^\r\n]+)*)",
);
pub(crate) const JWT_PATTERN: &str =
    r"(?-u:\b)eyJ[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+(?-u:\b)";
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
    r#"(?i)(?-u:\b)(postgres|postgresql|mysql|mongodb(?:\+srv)?|redis|amqp)://[^\s'"]{8,}"#;
// Credential labels also use camelCase/PascalCase in YAML, TOML and shell
// output. Admit the complete identifier before its sensitive suffix, just as
// structured JSON does. The assignment delimiter must follow that suffix, so
// token_count, passwordLength and accessKeyCount remain ordinary fields.
pub(crate) const GENERIC_SECRET_ASSIGNMENT_PATTERN: &str = concat!(
    r"(?i)(?-u:\b)[A-Za-z0-9_-]*(?:api[ _-]?(?:key|secret|token)|auth[ _-]?token|access[ _-]?(?:token|key)|secret[ _-]?key|session[ _-]?token|password|passwd|passphrase|token|secret|authorization)\s*[:=]\s*",
    r#"(?:"(?:\\(?s:.)|[^"\\]){4,}(?:"|(?:\\)?\z)|"#,
    r#"'(?:\\(?s:.)|[^'\\]){4,}(?:'|(?:\\)?\z)|[^\s,;}\]]{8,})"#,
);
pub(crate) const SLACK_TOKEN_PATTERN: &str = r"(?-u:\b)xox[bpsaor]-[A-Za-z0-9\-]{10,}";
pub(crate) const STRIPE_KEY_PATTERN: &str = r"(?-u:\b)[spr]k_live_[A-Za-z0-9]{20,}";

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
        SecretPattern {
            pattern: HTTP_AUTHORIZATION_PATTERN,
            regex: Regex::new(HTTP_AUTHORIZATION_PATTERN).expect("HTTP authorization regex"),
        },
        SecretPattern {
            pattern: HTTP_COOKIE_PATTERN,
            regex: Regex::new(HTTP_COOKIE_PATTERN).expect("HTTP cookie regex"),
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

/// Redact the union of canonical credentials and caller-supplied original
/// matches. Replacements are fixed markers selected from each complete span.
pub(crate) fn redact_text_with_additional_patterns<'a, 'r>(
    input: &'a str,
    additional_patterns: impl IntoIterator<Item = &'r Regex>,
    mut replacement: impl FnMut(&str) -> &'static str,
) -> Cow<'a, str> {
    let mut streams: Vec<std::iter::Peekable<regex::Matches<'r, 'a>>> = additional_patterns
        .into_iter()
        .map(|pattern| pattern.find_iter(input).peekable())
        .collect();
    let matches = SECRET_REGEX_SET.matches(input);
    streams.extend(
        matches
            .iter()
            .map(|idx| SECRET_PATTERNS[idx].regex.find_iter(input).peekable()),
    );
    apply_match_streams(input, streams, &mut replacement)
}

fn apply_replacements<'a>(input: &'a str, matches: &regex::SetMatches) -> Cow<'a, str> {
    let streams = matches
        .iter()
        .map(|idx| SECRET_PATTERNS[idx].regex.find_iter(input).peekable())
        .collect();
    apply_match_streams(input, streams, &mut |_| REDACTED)
}

/// Replace strictly overlapping original matches with one marker. Keep only
/// one pending match per canonical or additional pattern, regardless of input
/// size; adjacent independent matches retain separate markers.
fn apply_match_streams<'a, 'r>(
    input: &'a str,
    mut streams: Vec<std::iter::Peekable<regex::Matches<'r, 'a>>>,
    replacement: &mut impl FnMut(&str) -> &'static str,
) -> Cow<'a, str> {
    if streams.iter_mut().all(|stream| stream.peek().is_none()) {
        return Cow::Borrowed(input);
    }
    let mut output = String::with_capacity(input.len());
    let mut copied_until = 0;
    let mut pending: Option<std::ops::Range<usize>> = None;

    loop {
        let next = streams
            .iter_mut()
            .enumerate()
            .filter_map(|(idx, stream)| {
                stream.peek().map(|found| (idx, found.start(), found.end()))
            })
            .min_by_key(|&(_, start, end)| (start, end));
        let Some((idx, start, end)) = next else {
            break;
        };
        let _ = streams[idx].next();
        if let Some(range) = pending.as_mut()
            && start < range.end
        {
            range.end = range.end.max(end);
            continue;
        }
        if let Some(range) = pending.replace(start..end) {
            output.push_str(&input[copied_until..range.start]);
            output.push_str(replacement(&input[range.start..range.end]));
            copied_until = range.end;
        }
    }

    let Some(range) = pending else {
        return Cow::Borrowed(input);
    };
    output.push_str(&input[copied_until..range.start]);
    output.push_str(replacement(&input[range.start..range.end]));
    output.push_str(&input[range.end..]);
    Cow::Owned(output)
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

// Bound recursively JSON-encoded strings separately from serde_json's own
// container nesting limit. At the boundary, hide the remaining structured
// payload rather than returning credential fields that were never inspected.
const MAX_EMBEDDED_JSON_LAYERS: usize = 8;

/// Whether a complete embedded JSON value contains duplicate object keys.
///
/// Check before constructing a Value: its map keeps only the last duplicate,
/// which could hide a credential-bearing value and incorrectly leave the
/// original string unchanged. This visitor retains only the keys of active
/// objects; the subsequent ordinary Value parser keeps its number semantics.
struct EmbeddedJsonKeyCheck(bool);

impl<'de> serde::Deserialize<'de> for EmbeddedJsonKeyCheck {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(Self(false))
    }
}

impl<'de> serde::de::Visitor<'de> for EmbeddedJsonKeyCheck {
    type Value = Self;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_bool<E>(self, _value: bool) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(Self(false))
    }

    fn visit_i64<E>(self, _value: i64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(Self(false))
    }

    fn visit_u64<E>(self, _value: u64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(Self(false))
    }

    fn visit_f64<E>(self, _value: f64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(Self(false))
    }

    fn visit_str<E>(self, _value: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(Self(false))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(Self(false))
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::SeqAccess<'de>,
    {
        let mut has_duplicates = false;
        while let Some(child) = sequence.next_element::<Self>()? {
            has_duplicates |= child.0;
        }
        Ok(Self(has_duplicates))
    }

    fn visit_map<A>(self, mut object: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::MapAccess<'de>,
    {
        let mut keys = HashSet::new();
        let mut has_duplicates = false;
        while let Some((key, child)) = object.next_entry::<String, Self>()? {
            has_duplicates |= !keys.insert(key) || child.0;
        }
        Ok(Self(has_duplicates))
    }
}

/// A Value map loses earlier members with identical decoded keys.
pub(crate) fn json_has_duplicate_keys(text: &str) -> serde_json::Result<bool> {
    serde_json::from_str::<EmbeddedJsonKeyCheck>(text).map(|check| check.0)
}

/// Observations from the same bounded JSON walk used by credential redaction.
/// `layer` identifies decoded string nesting, so a scanner can reconcile a
/// token seen in an outer representation with the same decoded occurrence.
#[derive(Clone, Copy)]
pub(crate) struct JsonSourceSpan {
    pub start: usize,
    pub end: usize,
}

pub(crate) enum JsonPrivacyEvent<'a> {
    DecodedText {
        text: &'a str,
        layer: usize,
        source_span: Option<JsonSourceSpan>,
    },
    SensitiveField {
        value: &'a serde_json::Value,
        layer: usize,
        source_span: Option<JsonSourceSpan>,
    },
    SensitiveFieldEnd,
    Ambiguous,
}

fn ignore_json_privacy_event(_: JsonPrivacyEvent<'_>) {}

/// Inspect transcript JSON without inferring findings from rewritten output.
/// Field, duplicate-key, malformed-fragment and encoded-depth decisions come
/// from the production redactor; callers retain their own reporting policy.
pub(crate) fn inspect_json_content(input: &str, observe: &mut impl FnMut(JsonPrivacyEvent<'_>)) {
    let _ = redact_json_string(input, &mut str::to_owned, observe, 0, true);
}

/// Redact secrets from a JSON value, recursively walking strings.
///
/// - String values are redacted without changing their JSON type.
/// - Objects/arrays and quoted JSON strings encoded as strings use the same
///   field policy. Unchanged valid strings retain their exact bytes.
///   JSON objects, arrays and encoded strings inside prose or code fences
///   receive the same policy. Truncated containers, duplicate keys, parser
///   refusals and payloads beyond eight encoded layers are hidden. Ordinary
///   malformed quoted prose and bracketed log labels retain their bytes.
/// - The original whole-text policy runs before structural interpretation.
///   If its changes cannot be reconciled exactly with structured redaction,
///   the containing text is hidden in full rather than risking a secret tail.
/// - Values under credential-bearing object keys are replaced in full.
/// - Arrays and objects are walked recursively.
/// - Numbers, booleans, and null are left untouched.
pub fn redact_json(value: &serde_json::Value) -> serde_json::Value {
    redact_json_with_text_redactor(value, &mut |input| redact_text(input).into_owned())
}

/// Redact transcript text with the same credential-field and encoded-JSON
/// policy as metadata, without wrapping the input in a temporary JSON value.
pub(crate) fn redact_content(input: &str) -> String {
    redact_content_with_text_redactor(input, &mut |text| redact_text(text).into_owned())
}

/// Apply a caller's plain-text policy while retaining the shared structured
/// credential policy and encoded-depth limit.
pub(crate) fn redact_content_with_text_redactor(
    input: &str,
    redact_plain_text: &mut impl FnMut(&str) -> String,
) -> String {
    redact_json_string(
        input,
        redact_plain_text,
        &mut ignore_json_privacy_event,
        0,
        false,
    )
}

/// Inspect original JSON keys and encoded values before applying a text policy.
/// Rewriting first can hide a credential-bearing key or invalidate quote escapes.
/// Keep all callers on the same field, duplicate-key, and encoded-depth rules.
pub(crate) fn redact_json_with_text_redactor(
    value: &serde_json::Value,
    redact_plain_text: &mut impl FnMut(&str) -> String,
) -> serde_json::Value {
    redact_json_with(
        value,
        redact_plain_text,
        &mut ignore_json_privacy_event,
        0,
        None,
        false,
    )
}

fn redact_json_with(
    value: &serde_json::Value,
    redact_plain_text: &mut impl FnMut(&str) -> String,
    observe: &mut impl FnMut(JsonPrivacyEvent<'_>),
    embedded_layers: usize,
    source_span: Option<JsonSourceSpan>,
    inspect_sensitive_values: bool,
) -> serde_json::Value {
    match value {
        serde_json::Value::String(input) => {
            observe(JsonPrivacyEvent::DecodedText {
                text: input,
                layer: embedded_layers,
                source_span,
            });
            serde_json::Value::String(redact_json_string(
                input,
                redact_plain_text,
                observe,
                embedded_layers,
                inspect_sensitive_values,
            ))
        }
        serde_json::Value::Array(arr) => serde_json::Value::Array(
            arr.iter()
                .map(|value| {
                    redact_json_with(
                        value,
                        redact_plain_text,
                        observe,
                        embedded_layers,
                        source_span,
                        inspect_sensitive_values,
                    )
                })
                .collect(),
        ),
        serde_json::Value::Object(obj) => {
            let mut new_obj = serde_json::Map::with_capacity(obj.len());
            let mut next_suffixes = HashMap::new();
            for (key, value) in obj {
                observe(JsonPrivacyEvent::DecodedText {
                    text: key,
                    layer: embedded_layers,
                    source_span,
                });
                let redacted_key = redact_plain_text(key);
                if is_sensitive_json_field(key) && !value.is_null() {
                    observe(JsonPrivacyEvent::SensitiveField {
                        value,
                        layer: embedded_layers,
                        source_span,
                    });
                    if inspect_sensitive_values {
                        // Redaction can replace the whole value immediately.
                        // Inspection must still find decoded denylist matches
                        // and credential-bearing keys inside it. A string's
                        // initial text event has already been represented by
                        // SensitiveField; inspect only its embedded layers.
                        if let serde_json::Value::String(text) = value {
                            let _ = redact_json_string(
                                text,
                                redact_plain_text,
                                observe,
                                embedded_layers,
                                true,
                            );
                        } else {
                            let _ = redact_json_with(
                                value,
                                redact_plain_text,
                                observe,
                                embedded_layers,
                                source_span,
                                true,
                            );
                        }
                    }
                    observe(JsonPrivacyEvent::SensitiveFieldEnd);
                }
                let redacted_value = redact_sensitive_json_value(key, value, || {
                    redact_json_with(
                        value,
                        redact_plain_text,
                        observe,
                        embedded_layers,
                        source_span,
                        inspect_sensitive_values,
                    )
                });
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

fn redact_json_string(
    input: &str,
    redact_plain_text: &mut impl FnMut(&str) -> String,
    observe: &mut impl FnMut(JsonPrivacyEvent<'_>),
    embedded_layers: usize,
    inspect_sensitive_values: bool,
) -> String {
    // Capture the original whole-string obligation before decoding or
    // replacing a fragment can hide a credential prefix or custom policy.
    // This is also the only callback on the ordinary no-JSON fast path.
    let original_plain = redact_plain_text(input);
    if !input.contains(['{', '[', '"']) {
        return original_plain;
    }

    let trimmed = input.trim();
    let structured = if matches!(trimmed.as_bytes().first(), Some(b'{' | b'[' | b'"'))
        && let Ok(keys) = serde_json::from_str::<EmbeddedJsonKeyCheck>(trimmed)
    {
        redact_complete_json(
            input,
            redact_plain_text,
            observe,
            embedded_layers,
            keys,
            JsonSourceSpan {
                start: 0,
                end: input.len(),
            },
            inspect_sensitive_values,
        )
    } else {
        redact_mixed_json_fragments(
            input,
            redact_plain_text,
            observe,
            embedded_layers,
            inspect_sensitive_values,
        )
    };
    if original_plain == input {
        return structured;
    }
    if structured == input {
        return original_plain;
    }
    let combined = redact_plain_text(&structured);
    if combined == original_plain {
        return combined;
    }
    // The two policies changed different representations. Without original
    // span proof, hide the containing text instead of retaining an uncertain
    // suffix. This also protects custom policies across JSON/prose boundaries.
    observe(JsonPrivacyEvent::Ambiguous);
    REDACTED.to_owned()
}

fn redact_complete_json(
    input: &str,
    redact_plain_text: &mut impl FnMut(&str) -> String,
    observe: &mut impl FnMut(JsonPrivacyEvent<'_>),
    embedded_layers: usize,
    keys: EmbeddedJsonKeyCheck,
    source_span: JsonSourceSpan,
    inspect_sensitive_values: bool,
) -> String {
    if keys.0 || embedded_layers >= MAX_EMBEDDED_JSON_LAYERS {
        observe(JsonPrivacyEvent::Ambiguous);
        return REDACTED.to_owned();
    }
    let Ok(embedded) = serde_json::from_str::<serde_json::Value>(input.trim()) else {
        observe(JsonPrivacyEvent::Ambiguous);
        return REDACTED.to_owned();
    };
    let redacted = redact_json_with(
        &embedded,
        redact_plain_text,
        observe,
        embedded_layers + 1,
        Some(source_span),
        inspect_sensitive_values,
    );
    if redacted != embedded {
        return redacted.to_string();
    }
    // Do not reformat clean tool output or falsely report that it changed.
    input.to_owned()
}

/// Admit JSON-looking containers without treating log labels or source-code
/// braces as JSON. On refusal the returned cursor skips all inspected leading
/// arrays and whitespace, so a long run of '[' cannot cause quadratic retries.
fn json_fragment_prefix(input: &str, start: usize, escaped_keys: bool) -> (bool, usize) {
    let bytes = input.as_bytes();
    let mut cursor = start;
    if bytes[cursor] == b'"' {
        return (true, cursor + 1);
    }
    while bytes.get(cursor) == Some(&b'[') {
        cursor += 1;
        while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
            cursor += 1;
        }
    }
    if bytes.get(cursor) == Some(&b'{') {
        cursor += 1;
        while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
            cursor += 1;
        }
        if escaped_keys {
            while bytes.get(cursor) == Some(&b'\\') {
                cursor += 1;
            }
        }
        return (
            matches!(bytes.get(cursor), Some(b'"' | b'}')),
            cursor.max(start + 1),
        );
    }
    if escaped_keys {
        while bytes.get(cursor) == Some(&b'\\') {
            cursor += 1;
        }
    }
    (
        matches!(bytes.get(cursor), Some(b'"' | b']' | b'-' | b'0'..=b'9'))
            || input[cursor..].starts_with("true")
            || input[cursor..].starts_with("false")
            || input[cursor..].starts_with("null"),
        cursor.max(start + 1),
    )
}

/// Look inside a quoted span for an actual container prefix. Ordinary source
/// braces such as "{value}" are not enough; encoded keys need an escaped quote.
/// The prefix helper advances past every inspected array/whitespace run.
fn quoted_container_start(
    input: &str,
    mut cursor: usize,
    end: usize,
    escaped_keys: bool,
) -> Option<usize> {
    while cursor < end {
        let offset = input[cursor..end].find(['{', '['])?;
        let start = cursor + offset;
        let (admitted, resume) = json_fragment_prefix(input, start, escaped_keys);
        if admitted {
            return Some(start);
        }
        cursor = resume;
    }
    None
}

/// Find a candidate span with bounded retained state. A mismatched closer or
/// excessive nesting makes the remainder ambiguous and belongs to this unsafe
/// candidate; stopping at that closer could expose later credential fields.
/// Quotes and escapes shield delimiters. serde still checks the full grammar.
fn json_fragment_end(input: &str, start: usize) -> (usize, bool) {
    let bytes = input.as_bytes();
    let opening = bytes[start];
    let quoted = opening == b'"';
    let mut closers = [0_u8; 128];
    closers[0] = if opening == b'{' { b'}' } else { b']' };
    let mut depth = 1_usize;
    let mut in_string = quoted;
    let mut escaped = false;
    for (offset, &byte) in bytes[start + 1..].iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                if quoted {
                    return (start + offset + 2, true);
                }
                in_string = false;
            }
        } else if byte == b'"' {
            in_string = true;
        } else if matches!(byte, b'{' | b'[') {
            if depth == closers.len() {
                return (input.len(), false);
            }
            closers[depth] = if byte == b'{' { b'}' } else { b']' };
            depth += 1;
        } else if matches!(byte, b'}' | b']') {
            if byte != closers[depth - 1] {
                return (input.len(), false);
            }
            depth -= 1;
            if depth == 0 {
                return (start + offset + 2, true);
            }
        }
    }
    (input.len(), false)
}

/// Inspect candidates with a forward-only cursor. An unchanged quote may
/// also open the next literal (a Rust raw string around encoded JSON), so its
/// closing quote is reconsidered once. Escaped quotes are never retry points;
/// each intervening byte range is scanned a bounded number of times. No span
/// list grows with input size. Decoding retains serde's nesting limit and the
/// same eight-layer encoded-string limit as whole JSON.
fn redact_mixed_json_fragments(
    input: &str,
    redact_plain_text: &mut impl FnMut(&str) -> String,
    observe: &mut impl FnMut(JsonPrivacyEvent<'_>),
    embedded_layers: usize,
    inspect_sensitive_values: bool,
) -> String {
    let mut cursor = 0;
    let mut copied_until = 0;
    let mut output: Option<String> = None;
    while let Some(offset) = input[cursor..].find(['{', '[', '"']) {
        let start = cursor + offset;
        if input.as_bytes()[start] == b'"'
            && input.as_bytes()[..start]
                .iter()
                .rev()
                .take_while(|&&byte| byte == b'\\')
                .count()
                % 2
                != 0
        {
            // An inner-container reroute can expose source-level escaped
            // quotes. Starting a new literal there would repeatedly scan the
            // remaining suffix. Each preceding backslash run is disjoint.
            cursor = start + 1;
            continue;
        }
        let (admitted, resume) = json_fragment_prefix(input, start, false);
        if !admitted {
            cursor = resume;
            continue;
        }
        let (end, complete) = json_fragment_end(input, start);
        cursor = end;
        let fragment = &input[start..end];
        let quoted = input.as_bytes()[start] == b'"';
        // A prose quote can end on a JSON object's first key quote: "{" is a
        // complete string, but consuming it would hide the object's opener.
        // Resume at that inner container, never at every intervening quote.
        if quoted && let Some(inner) = quoted_container_start(input, start + 1, end, false) {
            if let Ok(keys) = serde_json::from_str::<EmbeddedJsonKeyCheck>(fragment) {
                // This interpretation is speculative: only the final
                // ambiguous result, not its decoded fields, is observable.
                if redact_complete_json(
                    fragment,
                    redact_plain_text,
                    &mut ignore_json_privacy_event,
                    embedded_layers,
                    keys,
                    JsonSourceSpan { start, end },
                    false,
                ) != fragment
                {
                    // Decoding this overlapping prose quote also revealed a
                    // secret. Its two interpretations cannot be spliced safely.
                    observe(JsonPrivacyEvent::Ambiguous);
                    return REDACTED.to_owned();
                }
            } else if input[start + 1..end]
                .match_indices('{')
                .any(|(offset, _)| json_fragment_prefix(input, start + 1 + offset, true).0)
            {
                // A malformed literal can contain a harmless raw array before
                // an encoded object. Rerouting to the array would abandon the
                // object's escaped keys, so hide this ambiguous containing text.
                observe(JsonPrivacyEvent::Ambiguous);
                return REDACTED.to_owned();
            }
            cursor = inner;
            continue;
        }
        let protected = match serde_json::from_str::<EmbeddedJsonKeyCheck>(fragment) {
            Ok(keys) => redact_complete_json(
                fragment,
                redact_plain_text,
                observe,
                embedded_layers,
                keys,
                JsonSourceSpan { start, end },
                inspect_sensitive_values,
            ),
            Err(_) if quoted && quoted_container_start(input, start + 1, end, true).is_none() => {
                // Windows paths and ordinary quoted prose need not be JSON
                // strings. Without an actual (possibly encoded) container
                // prefix, keep the original whole-text policy.
                if !complete {
                    break;
                }
                cursor = end - 1;
                continue;
            }
            Err(_) if fragment.starts_with('[') && !fragment.contains(['{', '"']) => {
                // A bracketed timestamp or numeric log label has no object
                // fields or encoded strings. Keep its exact original bytes.
                continue;
            }
            Err(_) => {
                observe(JsonPrivacyEvent::Ambiguous);
                REDACTED.to_owned()
            }
        };
        if protected != fragment {
            if quoted
                && (input.as_bytes().get(end) == Some(&b'\\')
                    || (matches!(input.as_bytes().get(end), Some(b'{' | b'['))
                        && json_fragment_prefix(input, end, true).0))
            {
                // A changed decoded literal overlaps another possible encoded
                // container. Neither interpretation may expose the other.
                observe(JsonPrivacyEvent::Ambiguous);
                return REDACTED.to_owned();
            }
            let buffer = output.get_or_insert_with(|| String::with_capacity(input.len()));
            buffer.push_str(&input[copied_until..start]);
            buffer.push_str(&protected);
            copied_until = end;
        } else if quoted && complete {
            cursor = end - 1;
        }
    }
    match output {
        Some(mut output) => {
            output.push_str(&input[copied_until..]);
            output
        }
        None => input.to_owned(),
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
    redaction_enabled_from_values(
        dotenvy::var("CASS_INDEX_REDACTION").ok().as_deref(),
        dotenvy::var("CASS_REDACT_SECRETS").ok().as_deref(),
    )
}

fn redaction_enabled_from_values(modern: Option<&str>, legacy: Option<&str>) -> bool {
    if let Some(val) = modern {
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
    !matches!(legacy, Some("0" | "false" | "off" | "no"))
}

/// Stable identifier for the compiled SECRET_PATTERNS list.
///
/// Memoization keys for [`MemoizingRedactor`] combine input content
/// with this fingerprint so a pattern bump (new regex added, existing
/// regex tightened) automatically invalidates every prior cache entry
/// — silent stale cross-version reuse is impossible by construction.
///
/// The fingerprint is `redact-v4:<blake3-hex>` where the hash covers
/// every pattern source string concatenated with NUL separators and the
/// replacement marker. The `v4` epoch covers mixed-text JSON fragments and
/// original whole-text policy reconciliation at every decoded layer.
/// Bump the epoch for algorithm changes even when the pattern set is unchanged.
pub fn redaction_algorithm_fingerprint() -> String {
    static FINGERPRINT: Lazy<String> = Lazy::new(|| {
        let mut hasher = blake3::Hasher::new();
        for pattern in SECRET_PATTERNS.iter() {
            hasher.update(pattern.pattern.as_bytes());
            hasher.update(&[0]);
        }
        hasher.update(REDACTED.as_bytes());
        format!("redact-v4:{}", hasher.finalize().to_hex())
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
/// [`crate::indexer::memoization::ContentAddressedMemoCache`] on the input
/// bytes plus the algorithm fingerprint so repeated content stops paying the
/// regex cost while a pattern bump invalidates every prior entry transparently.
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
/// returned directly; on miss, the legacy regex path runs. Retention is
/// optional: byte-pressure refusal never skips or truncates redaction.
///
/// `MemoizingRedactor` is `pub(crate)` so the live persist path can
/// adopt it without leaking the memoization vocabulary into public
/// API. Wiring lives in the indexer crate.
#[allow(dead_code)]
pub(crate) struct MemoizingRedactor {
    text_cache: crate::indexer::memoization::ContentAddressedMemoCache<String>,
    algorithm_fingerprint: String,
    retention: Arc<MemoRetentionBudget>,
    last_retention_limit: usize,
}

#[allow(dead_code)]
impl MemoizingRedactor {
    /// Default per-owner entry ceiling. Production owners additionally share
    /// REDACTION_RETAINED_VALUE_BYTES, regardless of their number or capacity.
    pub(crate) const DEFAULT_CAPACITY: usize = 4096;

    /// Inputs above this size bypass memoization entirely. Smaller inputs
    /// still need byte admission for their actual owned output buffer; this
    /// per-input cutoff alone is not an aggregate cache-memory bound.
    pub(crate) const MAX_MEMOIZED_INPUT_BYTES: usize = 64 * 1024;

    /// Explicit isolated cache for callers such as equivalence tests and the
    /// fuzz adapter. Live persistence workers use new(), sharing one allowance.
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self::with_retention_budget(
            capacity,
            MemoRetentionBudget::new(REDACTION_RETAINED_VALUE_BYTES),
        )
    }

    fn with_retention_budget(capacity: usize, retention: Arc<MemoRetentionBudget>) -> Self {
        Self {
            text_cache:
                crate::indexer::memoization::ContentAddressedMemoCache::with_retention_budget(
                    capacity,
                    Arc::clone(&retention),
                    String::capacity,
                ),
            algorithm_fingerprint: redaction_algorithm_fingerprint(),
            last_retention_limit: retention.limit(),
            retention,
        }
    }

    pub(crate) fn new() -> Self {
        Self::with_retention_budget(
            Self::configured_capacity(),
            Arc::clone(&REDACTION_RETENTION),
        )
    }

    /// Resolve the per-owner entry ceiling, honoring the optional
    /// `CASS_REDACT_MEMO_CAPACITY` override (#291). Raising it never bypasses
    /// the shared byte allowance. A `0`, empty, or unparseable value falls
    /// back to the default.
    pub(crate) fn configured_capacity() -> usize {
        dotenvy::var("CASS_REDACT_MEMO_CAPACITY")
            .ok()
            .and_then(|raw| raw.trim().parse::<usize>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(Self::DEFAULT_CAPACITY)
    }

    /// One atomic read on the usual path; no key hashing, cache lookup, OS
    /// probe or LRU traversal unless the memory allowance changed. Check even
    /// empty/clean inputs so a previously warm cache can shed retained values
    /// when the rest of a transcript contains no more secret candidates.
    fn refresh_retention_limit(&mut self) {
        let limit = self.retention.limit();
        if limit != self.last_retention_limit {
            self.last_retention_limit = limit;
            self.text_cache.trim_to_budget();
        }
    }

    pub(crate) fn algorithm_fingerprint(&self) -> &str {
        &self.algorithm_fingerprint
    }

    pub(crate) fn stats(&self) -> &crate::indexer::memoization::MemoCacheStats {
        self.text_cache.stats()
    }

    /// Memoized counterpart to [`redact_text`]. Returns an owned String
    /// (not Cow) because downstream callers (`map_to_internal`) immediately
    /// call `.into_owned()` regardless. Cache admission is independent of
    /// producing this output: a refused retained copy changes no source byte.
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
        self.refresh_retention_limit();
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
    /// [`crate::indexer::memoization::MemoLookup::Quarantined`] (handled by
    /// `redact_text` as a fallthrough to the direct regex path) instead of the
    /// cached value. Memory pressure never lifts a quarantine.
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
            MemoCacheEvent::BudgetBypass => tracing::debug!(
                target: "cass::redact::memo",
                algorithm = %audit.key.algorithm,
                live_entries = audit.stats.live_entries,
                "redact memo byte allowance refused retention; full redaction preserved"
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

    /// Structured transcript text keeps the same field policy as metadata;
    /// only the plain-text leaves participate in the existing bounded cache.
    pub(crate) fn redact_content(&mut self, input: &str) -> String {
        redact_content_with_text_redactor(input, &mut |text| self.redact_text(text))
    }

    /// Memoized counterpart to the shared JSON redactor. Field semantics and
    /// bounded embedded-JSON handling stay identical while plain-text string
    /// values and object keys are memoized independently.
    /// Arrays and objects themselves are not cached.
    pub(crate) fn redact_json(&mut self, value: &serde_json::Value) -> serde_json::Value {
        redact_json_with_text_redactor(value, &mut |input| self.redact_text(input))
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

    /// A Unicode `\b` in any pattern drops the whole RegexSet onto the PikeVM
    /// for every non-ASCII message (26x slower on real session text); see the
    /// note above AWS_ACCESS_KEY_PATTERN.
    #[test]
    fn secret_patterns_use_only_ascii_word_boundaries() {
        for pattern in SECRET_PATTERNS.iter() {
            let without_ascii_boundaries = pattern.pattern.replace(r"(?-u:\b)", "");
            assert!(
                !without_ascii_boundaries.contains(r"\b"),
                "pattern uses a Unicode word boundary: {}",
                pattern.pattern
            );
        }
    }

    #[test]
    fn secret_touching_non_ascii_text_is_redacted() {
        let token = "ghp_0123456789abcdefghijABCDEFGHIJ012345";
        for input in [
            format!("凭据{token}结束"),
            format!("é{token}é"),
            format!("🔐 {token} 测试"),
        ] {
            let output = redact_text(&input);
            assert!(!output.contains(token), "{input:?} -> {output:?}");
            assert!(output.contains(REDACTED), "{input:?} -> {output:?}");
        }
    }

    /// Historical sequential implementation from the 2026-08 redaction-perf
    /// baseline. Keep its body unchanged: the literal golden corpus still
    /// checks established disjoint-match behavior, while the overlap regression
    /// below demonstrates why v2 intentionally differs. Arbitrary overlapping
    /// inputs are checked against the independent original-span oracle.
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

    /// Independent test oracle: collect all original spans, sort/merge them,
    /// then replace from the end. Production instead sweeps bounded streams
    /// forward without retaining a match list proportional to input size.
    fn redact_text_original_span_reference(input: &str) -> String {
        let mut spans = SECRET_PATTERNS
            .iter()
            .flat_map(|pattern| pattern.regex.find_iter(input).map(|found| found.range()))
            .collect::<Vec<_>>();
        spans.sort_unstable_by_key(|range| (range.start, range.end));
        let mut merged: Vec<std::ops::Range<usize>> = Vec::new();
        for span in spans {
            if let Some(previous) = merged.last_mut()
                && span.start < previous.end
            {
                previous.end = previous.end.max(span.end);
            } else {
                merged.push(span);
            }
        }
        let mut output = input.to_owned();
        for span in merged.into_iter().rev() {
            output.replace_range(span, REDACTED);
        }
        output
    }

    #[test]
    fn quoted_secret_assignments_cover_multiline_escaped_and_truncated_values() {
        let aws_snake = format!(
            "AWS_SECRET_ACCESS_KEY=\"{}\nsynthetic-tail\"\nKEEP=public",
            "A".repeat(40)
        );
        let aws_camel = format!(
            "awsSecretAccessKey=\"{}\nsynthetic-tail\"\nKEEP=public",
            "A".repeat(40)
        );
        let mut memo = MemoizingRedactor::with_capacity(32);
        for (input, expected) in [
            (
                "DB_PASSWORD=\"first-private-line\nsecond-private-line\"\nKEEP=public",
                "[REDACTED]\nKEEP=public",
            ),
            (
                "DB_PASSWORD='first-private-line\r\nsecond-private-line'\r\nKEEP=public",
                "[REDACTED]\r\nKEEP=public",
            ),
            (
                "PASSWORD=\"before\\\"quoted\nsynthetic-tail\"; KEEP=public",
                "[REDACTED]; KEEP=public",
            ),
            (
                "PASSWORD=\"before\\\\\"\nKEEP=public",
                "[REDACTED]\nKEEP=public",
            ),
            (
                "MY_API_KEY=\"first-private-line\\\nsecond-private-line\"\nKEEP=public",
                "[REDACTED]\nKEEP=public",
            ),
            (
                "DB_PASSWORD=\"first-private-line\nunfinished-secret-line",
                "[REDACTED]",
            ),
            (
                "DB_PASSWORD='first-private-line\nunfinished-secret-line\\",
                "[REDACTED]",
            ),
            (
                "AWS_SESSION_TOKEN=\"first-private-line\nsecond-private-line\"\nKEEP=public",
                "[REDACTED]\nKEEP=public",
            ),
            (aws_snake.as_str(), "[REDACTED]\nKEEP=public"),
            (aws_camel.as_str(), "[REDACTED]\nKEEP=public"),
            ("password='a'abcdefghij", "[REDACTED]"),
        ] {
            assert_eq!(redact_text(input), expected, "{input:?}");
            assert_eq!(redact_text_original_span_reference(input), expected);
            assert_eq!(memo.redact_text(input), expected);
            assert_eq!(memo.redact_text(input), expected);
            assert_eq!(redact_text(expected), expected);
            let envelope = json!({"content": input, "message_index": 7});
            let expected_envelope = json!({"content": expected, "message_index": 7});
            assert_eq!(redact_json(&envelope), expected_envelope);
            assert_eq!(memo.redact_json(&envelope), expected_envelope);
        }
        for input in [
            "password=short",
            "password=\"abc\"",
            "DB_PASSWORD_LENGTH=\"first-public-line\nsecond-public-line\"",
            "TOKENIZER=\"first-public-line\nsecond-public-line\"",
            "ordinary \"first-public-line\nsecond-public-line\" prose",
        ] {
            assert!(matches!(redact_text(input), Cow::Borrowed(_)));
            assert_eq!(redact_text(input), input);
            assert_eq!(redact_text_reference(input), input);
            assert_eq!(memo.redact_text(input), input);
        }
    }

    #[test]
    fn original_spans_prevent_provider_replacements_from_hiding_outer_credentials() {
        // Keep this outer label AWS-specific: a generic credential suffix
        // would independently cover it after the inner replacement shrinks
        // the value below the AWS pattern's forty-character minimum.
        let shrinking =
            "awsSigningKey=\"AKIAABCDEFGHIJKLMNOP abcdefghijklmnopqrst\nTAIL\"\nKEEP=public";
        assert!(redact_text_reference(shrinking).contains("TAIL"));
        let private_key = concat!(
            "API_SECRET=-----BEGIN RSA PRIVATE KEY-----\n",
            "SYNTHETIC_PRIVATE_BODY\n-----END RSA PRIVATE KEY-----\nKEEP=public"
        ); // ubs:ignore — synthetic key-block redaction fixture, not a private key.
        let provider_in_password = format!(
            "PASSWORD=\"before ghp_{}\nsynthetic-tail\"\nKEEP=public",
            "A".repeat(36)
        );
        let mut memo = MemoizingRedactor::with_capacity(16);
        for input in [shrinking, private_key, provider_in_password.as_str()] {
            assert_eq!(redact_text(input), "[REDACTED]\nKEEP=public");
            assert_eq!(
                redact_text_original_span_reference(input),
                "[REDACTED]\nKEEP=public"
            );
            assert_eq!(memo.redact_text(input), "[REDACTED]\nKEEP=public");
            assert_eq!(memo.redact_text(input), "[REDACTED]\nKEEP=public");
            let unicode = format!("前🔐 {input} 界");
            assert_eq!(redact_text(&unicode), "前🔐 [REDACTED]\nKEEP=public 界");
        }
        let adjacent = "password=\"abcd\"password=\"efgh\"";
        assert_eq!(redact_text(adjacent), "[REDACTED][REDACTED]");
        assert_eq!(redact_text_reference(adjacent), "[REDACTED][REDACTED]");
        assert_eq!(
            redact_text_original_span_reference(adjacent),
            "[REDACTED][REDACTED]"
        );
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
    fn redacts_http_authentication_without_consuming_the_next_header() -> Result<(), String> {
        let mut memoized = MemoizingRedactor::with_capacity(16);
        for header in [
            "Authorization: Basic dXNlcjpwYXNz",
            "Proxy-Authorization: Basic dTo=",
            "HTTP_AUTHORIZATION=NTLM YWJjZGVm",
            "HTTP_PROXY_AUTHORIZATION=Negotiate YWJjZGVm",
            "authorization: Token opaque-credential",
            "Authorization:\r\n Basic dXNlcjpwYXNz",
            "Authorization: Basic\r\n\tdXNlcjpwYXNz",
            "Authorization: Bearer\r\n\topaque-credential",
            "Authorization: Digest username=\"Mufasa\", realm=\"http-auth\", response=\"inert-response\"",
            "Proxy-Authorization: Digest username=\"Mufasa\",\r\n\tresponse=\"inert-response\"",
            "Authorization: AWS4-HMAC-SHA256 Credential=inert-id, SignedHeaders=host, Signature=inert-signature",
            "Authorization: AWS inert-id:inert-signature",
            "Authorization: OAuth oauth_consumer_key=\"inert-client\", oauth_signature=\"inert-signature\"",
        ] {
            let input = format!("café 界\n{header}\r\nX-Public: visible");
            let expected = "café 界\n[REDACTED]\r\nX-Public: visible";
            if redact_text(&input) != expected || redact_content(&input) != expected {
                return Err("HTTP credential or neighboring header was handled incorrectly".into());
            }
            // Exercise both cache insertion and reuse through production entry points.
            for _ in 0..2 {
                if memoized.redact_text(&input) != expected
                    || memoized.redact_content(&input) != expected
                {
                    return Err("memoized HTTP credential policy diverged".into());
                }
            }
            if redact_content(expected) != expected {
                return Err("HTTP redaction was not idempotent".into());
            }
        }
        Ok(())
    }

    #[test]
    fn http_authentication_redaction_preserves_noncredential_context() -> Result<(), String> {
        let input = "curl -H 'Proxy-Authorization: Basic dTo=' --version";
        if redact_text(input) != "curl -H '[REDACTED]' --version" {
            return Err("token68 redaction consumed surrounding command syntax".into());
        }
        for benign in [
            "Basic dXNlcjpwYXNz is an example without an authorization field",
            "authorization_status=Basic dXNlcjpwYXNz",
            "Authorization: Basic [REDACTED]",
            "Authorization: Digest [REDACTED]",
            "Explain basic authentication; café 界",
        ] {
            if redact_content(benign) != benign {
                return Err("HTTP redaction changed noncredential context".into());
            }
        }
        if Regex::new(HTTP_AUTHORIZATION_PATTERN)
            .map_err(|error| error.to_string())?
            .is_match("Authorization: [REDACTED]")
        {
            return Err("HTTP detector matched an existing redaction marker".into());
        }
        Ok(())
    }

    #[test]
    fn redacts_structured_proxy_and_cgi_authorization_fields() -> Result<(), String> {
        let original = serde_json::json!({
            "Proxy-Authorization": "Basic dXNlcjpwYXNz",
            "HTTP_AUTHORIZATION": "NTLM YWJjZGVm",
            "HTTP_PROXY_AUTHORIZATION": "Basic dTo=",
            "authorization_mode": "Basic",
            "message_index": 7,
            "null_header": {"Proxy-Authorization": null}
        });
        let expected = serde_json::json!({
            "Proxy-Authorization": REDACTED,
            "HTTP_AUTHORIZATION": REDACTED,
            "HTTP_PROXY_AUTHORIZATION": REDACTED,
            "authorization_mode": "Basic",
            "message_index": 7,
            "null_header": {"Proxy-Authorization": null}
        });
        let mut memoized = MemoizingRedactor::with_capacity(16);
        if redact_json(&original) != expected
            || memoized.redact_json(&original) != expected
            || redact_json(&expected) != expected
        {
            return Err("structured authorization policy diverged or changed public fields".into());
        }
        let encoded = serde_json::Value::String(original.to_string());
        let protected = redact_json(&encoded);
        let decoded = serde_json::from_str::<serde_json::Value>(
            protected
                .as_str()
                .ok_or("encoded metadata changed JSON type")?,
        )
        .map_err(|error| error.to_string())?;
        if decoded != expected {
            return Err("JSON-encoded proxy credentials were not protected".into());
        }
        Ok(())
    }

    #[test]
    fn redacts_http_cookie_headers_and_cgi_values() -> Result<(), String> {
        let mut memoized = MemoizingRedactor::with_capacity(16);
        for header in [
            "Cookie: session=inert-session; csrf=inert-csrf",
            "Set-Cookie: __Host-session=\"inert-session\"; Path=/; Secure; HttpOnly; SameSite=Lax",
            "Cookie: session=\"inert-session\"; csrf=\"inert-csrf\"",
            "HTTP_COOKIE=session=inert-session; csrf=inert-csrf",
            "HTTP_COOKIE=\"session=inert-session; csrf=inert-csrf\"",
            "HTTP_COOKIE='session=inert-session; csrf=inert-csrf'",
            "HTTP_SET_COOKIE=\"__Host-session=inert-session; HttpOnly\"",
            "Cookie: session=inert-session;\r\n\tcsrf=inert-csrf",
            "Set-Cookie:\r\n __Secure-session=inert-session; Secure",
            "Set-Cookie: session=\"inert-truncated",
            "Cookie: session=",
            "cookie: a=1",
            "HTTP_COOKIE=\"session=inert-session;\ncsrf=inert-csrf\"",
        ] {
            let input = format!("café 界\n{header}\r\nX-Public: visible");
            let expected = "café 界\n[REDACTED]\r\nX-Public: visible";
            if redact_text(&input) != expected || redact_content(&input) != expected {
                return Err("HTTP cookie or neighboring header was handled incorrectly".into());
            }
            for _ in 0..2 {
                if memoized.redact_text(&input) != expected
                    || memoized.redact_content(&input) != expected
                {
                    return Err("memoized HTTP cookie policy diverged".into());
                }
            }
            if redact_content(expected) != expected {
                return Err("HTTP cookie redaction was not idempotent".into());
            }
        }
        Ok(())
    }

    #[test]
    fn http_cookie_redaction_preserves_context_and_masks_truncated_cgi() -> Result<(), String> {
        for benign in [
            "Discuss cookie persistence and CSRF validation; café 界",
            "cookie_count=3",
            "cookie_recipe=chocolate",
            "cookies: session=inert-session",
            "Cookie: [REDACTED]",
            "Set-Cookie: [REDACTED]",
            "cookie: \"recommendations for baking\"",
            "session=inert-session; csrf=inert-csrf",
        ] {
            if redact_content(benign) != benign {
                return Err(
                    "HTTP cookie policy changed unrelated prose or a redaction marker".into(),
                );
            }
        }
        let quoted = "HTTP_COOKIE=\"session=inert-session\" --next public";
        if redact_content(quoted) != "[REDACTED] --next public" {
            return Err("quoted CGI cookie redaction consumed trailing command syntax".into());
        }
        for truncated in [
            "HTTP_COOKIE=\"session=inert-truncated\nstill-private",
            "HTTP_COOKIE='session=inert-truncated\nstill-private",
            "HTTP_COOKIE=\"session=inert-truncated\\",
        ] {
            if redact_content(truncated) != REDACTED {
                return Err("truncated CGI cookie retained private suffix bytes".into());
            }
        }
        Ok(())
    }

    #[test]
    fn redacts_structured_http_cookie_fields() -> Result<(), String> {
        let original = serde_json::json!({
            "Cookie": "session=inert-session; csrf=inert-csrf",
            "Set-Cookie": "__Host-session=inert-session; Secure",
            "HTTP_COOKIE": "session=inert-session",
            "HTTP_SET_COOKIE": ["session=inert-session", "csrf=inert-csrf"],
            "cookie_count": 2,
            "cookie_policy": "same-origin",
            "null_header": {"HTTP_COOKIE": null}
        });
        let expected = serde_json::json!({
            "Cookie": REDACTED,
            "Set-Cookie": REDACTED,
            "HTTP_COOKIE": REDACTED,
            "HTTP_SET_COOKIE": REDACTED,
            "cookie_count": 2,
            "cookie_policy": "same-origin",
            "null_header": {"HTTP_COOKIE": null}
        });
        let mut memoized = MemoizingRedactor::with_capacity(16);
        if redact_json(&original) != expected
            || memoized.redact_json(&original) != expected
            || redact_json(&expected) != expected
        {
            return Err(
                "structured HTTP cookie policy changed credentials or public metadata".into(),
            );
        }
        let encoded = serde_json::Value::String(original.to_string());
        let protected = redact_json(&encoded);
        let decoded = serde_json::from_str::<serde_json::Value>(
            protected
                .as_str()
                .ok_or("encoded cookie metadata changed JSON type")?,
        )
        .map_err(|error| error.to_string())?;
        if decoded != expected {
            return Err("JSON-encoded HTTP cookies were not protected".into());
        }
        Ok(())
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
    fn gh547_prefixed_secret_assignments_redact_across_text_paths() {
        let mut memo = MemoizingRedactor::with_capacity(16);
        for input in [
            "DB_PASSWORD=plainvalue222",
            "GITHUB_TOKEN=plainvalue333",
            "STRIPE_SECRET=plainvalue444",
            "MY_API_KEY=plainvalue555",
            "_TOKEN=plainvalue777",
            "SERVICE__ACCESS_TOKEN=\"opaque value\"",
            "deploy-client-secret='opaque value'",
        ] {
            assert_eq!(redact_text(input), REDACTED, "{input}");
            assert_eq!(redact_text_reference(input), REDACTED, "{input}");
            assert_eq!(memo.redact_text(input), REDACTED, "{input}");
            assert_eq!(memo.redact_text(input), REDACTED, "{input}");
        }
        assert_eq!(memo.stats().misses, 7);
        assert_eq!(memo.stats().hits, 7);
    }

    #[test]
    fn camel_case_credential_assignments_redact_across_text_and_json_paths() {
        let mut memo = MemoizingRedactor::with_capacity(32);
        for key in [
            "clientSecret",
            "refreshToken",
            "redisPassword",
            "tenantApiKey",
            "serviceAccessToken",
            "ClientSecret",
            "RefreshToken",
            "redispassword",
        ] {
            assert!(is_sensitive_json_field(key));
            for (assignment, suffix) in [
                (format!("{key}: opaque-camel-fixture"), "\nKEEP=public"),
                (format!("{key} = 'opaque camel fixture'"), "\nKEEP=public"),
                (
                    format!("{key}=\"opaque first line\nopaque second line\""),
                    "\nKEEP=public",
                ),
                (format!("{key}=\"opaque truncated line\nopaque tail\\"), ""),
            ] {
                let input = format!("public café 界\n{assignment}{suffix}");
                let expected = format!("public café 界\n{REDACTED}{suffix}");
                assert_eq!(redact_text(&input), expected);
                assert_eq!(redact_content(&input), expected);
                assert_eq!(redact_json(&json!(input)), json!(expected));
                // Both the first computation and a cached result must apply
                // the same policy used for archived and direct-source exports.
                for _ in 0..2 {
                    assert_eq!(memo.redact_text(&input), expected);
                    assert_eq!(memo.redact_content(&input), expected);
                    assert_eq!(memo.redact_json(&json!(input)), json!(expected));
                }
            }
        }
    }

    #[test]
    fn gh547_assignment_prefixes_do_not_match_nonsecret_suffixes() {
        let mut memo = MemoizingRedactor::with_capacity(8);
        for input in [
            "TOKEN_COUNT=1234567890",
            "DB_PASSWORD_LENGTH=1234567890",
            "SECRETARY=ordinaryvalue",
            "MONKEY=ordinaryvalue",
            "TOKENIZER=ordinaryvalue",
            "GITHUB_TOKEN=short",
            "token_count=1234567890",
            "passwordLength='ordinary quoted value'",
            "accessKeyCount=1234567890",
            "apiKeyLabel='ordinary quoted value'",
            "keyframe=ordinaryvalue",
            "publicKey=ordinaryvalue",
            "monkey=ordinaryvalue",
            "tokenizer=ordinaryvalue",
        ] {
            assert_eq!(redact_text(input), input);
            assert_eq!(redact_text_reference(input), input);
            assert_eq!(redact_content(input), input);
            assert_eq!(redact_json(&json!(input)), json!(input));
            assert_eq!(memo.redact_text(input), input);
            assert_eq!(memo.redact_text(input), input);
        }
        assert_eq!(memo.stats().misses, 0);
        assert_eq!(memo.stats().hits, 0);
    }

    #[test]
    fn structured_content_entry_points_share_json_policy() -> Result<(), serde_json::Error> {
        let mut memo = MemoizingRedactor::with_capacity(8);
        for input in [
            r#"{"registration_token":"tiny","token_count":42}"#,
            r#"{"nested":[{"passphrase":"short"}]}"#,
            "ordinary café 界",
        ] {
            let expected = redact_json(&json!(input));
            assert_eq!(json!(redact_content(input)), expected);
            assert_eq!(json!(memo.redact_content(input)), expected);
            assert_eq!(json!(memo.redact_content(input)), expected);
            let encoded = serde_json::to_string(input)?;
            assert_eq!(
                json!(redact_content(&encoded)),
                redact_json(&json!(encoded))
            );
        }
        let assignment = "DB_PASSWORD=synthetic-assignment";
        assert_eq!(redact_content(assignment), REDACTED);
        assert_eq!(memo.redact_content(assignment), REDACTED);
        let encoded_assignment = serde_json::to_string(assignment)?;
        assert_eq!(redact_content(&encoded_assignment), REDACTED);
        assert_eq!(memo.redact_content(&encoded_assignment), REDACTED);
        Ok(())
    }

    #[test]
    fn structured_content_preserves_cross_field_plaintext_secret_spans()
    -> Result<(), serde_json::Error> {
        let input = concat!(
            r#"{"head":"-----BEGIN PRIVATE KEY-----","#,
            r#""tail":"synthetic-private-body","#,
            r#""end":"-----END PRIVATE KEY-----","safe":42}"#,
        );
        assert!(!redact_text(input).contains("synthetic-private-body"));
        assert_eq!(redact_content(input), REDACTED);
        let mut memo = MemoizingRedactor::with_capacity(8);
        assert_eq!(memo.redact_content(input), REDACTED);
        assert_eq!(memo.redact_content(input), REDACTED);
        let encoded = serde_json::to_string(input)?;
        assert_eq!(redact_content(&encoded), REDACTED);
        assert_eq!(memo.redact_content(&encoded), REDACTED);

        let single_field = json!({
            "key": "-----BEGIN PRIVATE KEY-----\nsynthetic-private-body\n-----END PRIVATE KEY-----",
            "safe": 42
        });
        assert_eq!(
            redact_content(&single_field.to_string()),
            json!({"key": REDACTED, "safe": 42}).to_string()
        );
        Ok(())
    }

    #[test]
    fn structured_content_preserves_original_encoded_secret_matches() {
        let input = r#"{"s":"password=\u0061\u0062, SAFE"}"#;
        let expected = redact_text(input).into_owned();
        assert_ne!(expected, input);
        assert_eq!(redact_content(input), expected);
        let mut memo = MemoizingRedactor::with_capacity(8);
        assert_eq!(memo.redact_content(input), expected);

        let with_field_secret =
            r#"{"s":"password=\u0061\u0062, SAFE","registration_token":"tiny"}"#;
        assert_eq!(redact_content(with_field_secret), REDACTED);
        assert_eq!(memo.redact_content(with_field_secret), REDACTED);
    }

    #[test]
    fn structured_content_preserves_plain_log_prefixes() {
        let mut memo = MemoizingRedactor::with_capacity(8);
        for input in [
            "[2026-11-22 22:35:42] Build succeeded",
            "[INFO] Build succeeded",
            "{ not JSON } but ordinary source code",
            "{\"safe\":true} followed by commentary",
            "\"ordinary quoted prose\" with a suffix",
        ] {
            assert_eq!(redact_content(input), input);
            assert_eq!(memo.redact_content(input), input);
        }
    }

    #[test]
    fn gh551_embedded_json_uses_sensitive_field_policy() -> Result<(), Box<dyn std::error::Error>> {
        let embedded = r#" {
            "ok": true,
            "registration_token": "synthetic-short",
            "nested": [{"passphrase": "tiny", "token_count": 42}],
            "null_token": null,
            "safe": "kept café 界"
        } "#;
        let input = json!({"message_index": 7, "content": embedded});
        let output = redact_json(&input);
        let decoded: serde_json::Value = serde_json::from_str(
            output["content"]
                .as_str()
                .ok_or("content changed JSON type")?,
        )?;
        assert_eq!(
            decoded,
            json!({
                "ok": true,
                "registration_token": REDACTED,
                "nested": [{"passphrase": REDACTED, "token_count": 42}],
                "null_token": null,
                "safe": "kept café 界"
            })
        );
        assert_eq!(output["message_index"], 7);
        assert_eq!(redact_json(&output), output);
        let mut memo = MemoizingRedactor::with_capacity(16);
        assert_eq!(memo.redact_json(&input), output);
        assert_eq!(memo.redact_json(&input), output);
        Ok(())
    }

    #[test]
    fn gh551_nested_embedded_json_reconciles_original_matches_and_reuses_memo() {
        let details = json!([
            {"registration_token": "opaque-short-value", "detail": "DB_PASSWORD=synthetic-assignment"},
            {"detail": "DB_PASSWORD=synthetic-assignment"}
        ]);
        let input = json!({"content": json!({"output": details.to_string()}).to_string()});
        // The original unquoted assignment match consumes an encoded closing
        // quote. A field-by-field rewrite cannot prove that original span was
        // preserved, so this containing encoded payload is hidden in full.
        let expected = json!({"content": REDACTED});
        assert_eq!(redact_json(&input), expected);
        let mut memo = MemoizingRedactor::with_capacity(16);
        assert_eq!(memo.redact_json(&input), expected);
        let misses = memo.stats().misses;
        let hits = memo.stats().hits;
        assert!(
            misses > 0,
            "the original candidate-bearing text must be checked"
        );
        assert!(
            hits > 0,
            "the repeated plain leaf must reuse its cached result"
        );
        assert_eq!(memo.redact_json(&input), expected);
        assert_eq!(memo.stats().misses, misses);
        assert!(memo.stats().hits > hits);
        assert_eq!(redact_json(&expected), expected);
    }

    #[test]
    fn gh551_quoted_json_strings_preserve_layers_and_redact_fields()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut memo = MemoizingRedactor::with_capacity(8);
        for container in [
            json!({"registration_token": "tiny", "safe": "kept café 界"}),
            json!([{"passphrase": "tiny", "token_count": 42}]),
        ] {
            let mut expected = container.clone();
            if expected.is_array() {
                expected[0]["passphrase"] = json!(REDACTED);
            } else {
                expected["registration_token"] = json!(REDACTED);
            }
            for wrappers in [1, 3] {
                let mut input = container.to_string();
                let mut expected = expected.to_string();
                for _ in 0..wrappers {
                    input = serde_json::to_string(&input)?;
                    expected = serde_json::to_string(&expected)?;
                }
                let input = json!(input);
                let expected = json!(expected);
                assert_eq!(redact_json(&input), expected);
                assert_eq!(memo.redact_json(&input), expected);
                assert_eq!(memo.redact_json(&input), expected);
                assert_eq!(redact_json(&expected), expected);
            }
        }
        for input in [
            r#""ordinary quoted café 界""#,
            r#""{\"safe\":\"kept café 界\"}""#,
            r#""an unfinished quoted phrase"#,
            r#""ordinary prose" with a suffix"#,
        ] {
            let input = json!(input);
            assert_eq!(redact_json(&input), input);
            assert_eq!(memo.redact_json(&input), input);
        }
        for wrappers in [MAX_EMBEDDED_JSON_LAYERS, MAX_EMBEDDED_JSON_LAYERS + 1] {
            let mut encoded = json!({"registration_token": "synthetic-deep-secret"}).to_string();
            for _ in 0..wrappers {
                encoded = serde_json::to_string(&encoded)?;
            }
            let input = json!(encoded);
            let output = redact_json(&input);
            assert!(!output.to_string().contains("synthetic-deep-secret"));
            assert_eq!(redact_json(&output), output);
            assert_eq!(memo.redact_json(&input), output);
        }
        Ok(())
    }

    #[test]
    fn gh551_clean_embedded_json_preserves_exact_bytes() {
        let mut memo = MemoizingRedactor::with_capacity(8);
        for input in [
            " \n { \"token_count\": 2048, \"safe\": [1, true, null, \"café 界\"] } \t ",
            "[\n {\"public_key\": \"public material\"} \n]",
            r#"{"large":18446744073709551615,"small":-9223372036854775808,"decimal":1.25}"#,
            "{}",
            "[]",
        ] {
            let value = json!(input);
            assert_eq!(redact_json(&value), value);
            assert_eq!(memo.redact_json(&value), value);
            assert_eq!(memo.redact_json(&value), value);
        }
        assert_eq!(memo.stats().misses, 0);
        assert_eq!(memo.stats().hits, 0);
    }

    #[test]
    fn gh551_mixed_text_json_fragments_redact_without_changing_context()
    -> Result<(), Box<dyn std::error::Error>> {
        let structured = json!({
            "registration_token": "synthetic-wrapped-credential",
            "safe": "kept café 界",
            "token_count": 42,
            "nested": [{"passphrase": "tiny"}]
        });
        let protected = json!({
            "registration_token": REDACTED,
            "safe": "kept café 界",
            "token_count": 42,
            "nested": [{"passphrase": REDACTED}]
        });
        let mut memo = MemoizingRedactor::with_capacity(8);
        for (prefix, suffix) in [
            ("Tool response: ", "\ncomplete"),
            ("```json\n", "\n```"),
            ("before café 界\n", "\nafter café 界"),
            ("Result: \"prefix ", " suffix\""),
            ("r#\"", "\"#"),
        ] {
            let mut payload = structured.to_string();
            let mut expected_payload = protected.to_string();
            for _ in 0..3 {
                let input = format!("{prefix}{payload}{suffix}");
                let expected = format!("{prefix}{expected_payload}{suffix}");
                assert_eq!(redact_content(&input), expected, "{input}");
                assert_eq!(redact_json(&json!(input)), json!(expected));
                assert_eq!(memo.redact_content(&input), expected);
                assert_eq!(memo.redact_content(&input), expected);
                assert_eq!(redact_content(&expected), expected);
                payload = serde_json::to_string(&payload)?;
                expected_payload = serde_json::to_string(&expected_payload)?;
            }
        }
        let input = format!("first: {structured}\nsecond: [{structured}]\nfinished");
        let expected = format!("first: {protected}\nsecond: [{protected}]\nfinished");
        assert_eq!(redact_content(&input), expected);
        assert_eq!(memo.redact_content(&input), expected);
        assert_eq!(redact_content(&expected), expected);
        Ok(())
    }

    #[test]
    fn gh551_mixed_text_malformed_and_deep_json_fails_closed() {
        let deep = format!(
            "{}{}{}",
            "[".repeat(130),
            r#"{"registration_token":"synthetic-hidden"}"#,
            "]".repeat(130)
        );
        let mut memo = MemoizingRedactor::with_capacity(8);
        for fragment in [
            r#"{"registration_token":"synthetic-hidden","registration_token":null}"#,
            r#"{"pin":"synthetic-hidden","\u0070in":null}"#,
            r#"{"registration_token":"synthetic-hidden","number":1e400}"#,
            r#"{"registration_token":"synthetic-hidden","text":"\uD800"}"#,
        ] {
            let input = format!("result: {fragment}\nfinished");
            let expected = "result: [REDACTED]\nfinished";
            assert_eq!(redact_content(&input), expected, "{input}");
            assert_eq!(memo.redact_content(&input), expected);
            assert_eq!(redact_content(expected), expected);
        }
        for fragment in [
            deep.as_str(),
            r#"[{"unclosed":], "registration_token":"synthetic-hidden"} trailing"#,
            r#"[{"unclosed":]} , "registration_token":"synthetic-hidden"} trailing"#,
            r#"{"registration_token":"synthetic-hidden""#,
            r#"[{"registration_token":"synthetic-hidden"}"#,
            r#"{"registration_token":"synthetic-hidden"#,
            r#"{"registration_token":"synthetic-hidden","broken":[}"#,
            r#""{\"registration_token\":\"synthetic-hidden\""#,
        ] {
            let input = format!("result: {fragment}");
            let expected = "result: [REDACTED]";
            assert_eq!(redact_content(&input), expected, "{input}");
            assert_eq!(memo.redact_content(&input), expected);
        }
    }

    #[test]
    fn gh551_mixed_text_clean_bytes_and_plain_policy_preserved() {
        let mut memo = MemoizingRedactor::with_capacity(8);
        for input in [
            "[2026-11-22 22:35:42] Build succeeded",
            "[INFO] response: { \"token_count\": 42, \"safe\": \"café 界\" }\ndone",
            "```json\n[ { \"null_token\": null, \"safe\": true } ]\n```",
            "code: [{ id: 1 }] and {not valid JSON}; [unfinished",
            "\"ordinary quoted café 界\" with a suffix",
            "{ not-json } then { \"safe\": 42 }",
            r#"let message = "use {value}"#,
            r#"prefix "ordinary invalid \q prose" suffix"#,
            r#"response: "{\"safe\":\"kept café 界\"}" done"#,
        ] {
            assert_eq!(redact_content(input), input);
            assert_eq!(redact_json(&json!(input)), json!(input));
            assert_eq!(memo.redact_content(input), input);
            assert_eq!(memo.redact_content(input), input);
        }
        for input in [
            "DB_PASSWORD=\"before {} after\"",
            r#"PASSWORD="before {\"ok\":true} after""#,
            "-----BEGIN PRIVATE KEY-----\n{\"safe\":true}\n-----END PRIVATE KEY-----",
        ] {
            assert_eq!(redact_text(input), REDACTED);
            assert_eq!(redact_content(input), REDACTED);
            assert_eq!(memo.redact_content(input), REDACTED);
        }
    }

    #[test]
    fn gh551_mixed_text_preserves_original_and_custom_cross_boundary_protection() {
        let mut memo = MemoizingRedactor::with_capacity(8);
        for input in [
            concat!(
                "-----BEGIN PRIVATE KEY-----\n",
                "{\"registration_token\":\"tiny\",\"note\":\"PUBLICISH\"}\n",
                "synthetic-private-tail\n-----END PRIVATE KEY-----",
            ),
            concat!(
                "log: {\"note\":\"-----BEGIN PRIVATE KEY-----\"}\n",
                "synthetic-private-tail\n-----END PRIVATE KEY-----",
            ),
            r#"log: "\u0073k-ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij {"pin":12}"#,
        ] {
            let output = redact_content(input);
            assert_eq!(output, REDACTED, "{input}");
            assert_eq!(memo.redact_content(input), output);
            assert_eq!(redact_json(&json!(input)), json!(output));
        }
        let custom = Regex::new(r"(?s)PRIVATE_BEGIN.*?PRIVATE_END").unwrap();
        let input = concat!(
            "before PRIVATE_BEGIN {\"registration_token\":\"tiny\"} ",
            "synthetic-custom-tail PRIVATE_END after",
        );
        let mut scalar = |text: &str| custom.replace_all(text, "[CUSTOM]").into_owned();
        assert_eq!(
            redact_content_with_text_redactor(input, &mut scalar),
            "before [CUSTOM] after"
        );
        assert_eq!(
            redact_json_with_text_redactor(&json!({"content": input}), &mut scalar),
            json!({"content": "before [CUSTOM] after"})
        );
        let input =
            r#"before {"registration_token":"PRIVATE_BEGIN"} synthetic-tail PRIVATE_END after"#;
        assert_eq!(
            redact_content_with_text_redactor(input, &mut scalar),
            REDACTED
        );
    }

    #[test]
    fn gh551_mixed_text_escaped_layers_keep_original_custom_refusals()
    -> Result<(), Box<dyn std::error::Error>> {
        let payload = json!({
            "registration_token": "tiny",
            "command": "rch exec -- sh -c $(synthetic-command)"
        })
        .to_string();
        let mut scalar = |text: &str| {
            if text.contains("$(") {
                "[UNSAFE COMMAND]".to_owned()
            } else {
                text.to_owned()
            }
        };
        let mut encoded = payload;
        for _ in 0..4 {
            let input = format!("Tool response: {encoded}");
            assert_eq!(
                redact_content_with_text_redactor(&input, &mut scalar),
                REDACTED
            );
            encoded = serde_json::to_string(&encoded)?;
        }
        Ok(())
    }

    #[test]
    fn gh551_mixed_text_escaped_quotes_do_not_restart_suffix_scans()
    -> Result<(), Box<dyn std::error::Error>> {
        for repetitions in [256, 512] {
            let decoded = "[1] \" café 界 ".repeat(repetitions);
            let input = format!("prefix {}", serde_json::to_string(&decoded)?);
            let mut inspected_bytes = 0;
            let mut scalar = |text: &str| {
                inspected_bytes += text.len();
                text.to_owned()
            };
            assert_eq!(
                redact_content_with_text_redactor(&input, &mut scalar),
                input
            );
            // Repeatedly decoding the suffix at every escaped quote makes
            // the total inspected text quadratic. The bound allows ordinary
            // whole-string, decoded-string, and short inner-literal passes.
            assert!(inspected_bytes <= 32 * input.len(), "{inspected_bytes}");
            assert_eq!(redact_content(&input), input);
        }
        for input in [
            r#"prefix "bad \q [1] {\"registration_token\":\"tiny\"}" suffix"#,
            r#"prefix "bad \q [1] {\"registration_token\":\"tiny\"}"#,
        ] {
            assert_eq!(redact_content(input), REDACTED);
            assert_eq!(redact_json(&json!(input)), json!(REDACTED));
        }
        Ok(())
    }

    #[test]
    fn gh551_mixed_text_long_refused_prefixes_keep_later_credentials_visible() {
        let input = format!(
            "before {}source-code {} after",
            "[".repeat(16_384),
            r#"{"pin":"synthetic-hidden"}"#,
        );
        let output = redact_content(&input);
        assert!(output.starts_with("before "));
        assert!(output.ends_with(" after"));
        assert!(!output.contains("synthetic-hidden"));
        assert!(output.contains(REDACTED));

        let input = format!(
            "before {}{}",
            "[".repeat(16_384),
            r#"{"pin":"synthetic-hidden"}"#,
        );
        assert_eq!(redact_content(&input), "before [REDACTED]");
    }

    #[test]
    fn gh551_json_prefix_admission_separates_structured_data_from_prose() {
        let mut memo = MemoizingRedactor::with_capacity(8);
        for (input, expected) in [
            (r#"{"note": DB_PASSWORD=synthetic-assignment }"#, REDACTED),
            (r#"[null, {"registration_token":"tiny"}, ]"#, REDACTED),
            (r#"{"registration_token":"tiny""#, REDACTED),
            (r#"[{"passphrase":"tiny"}"#, REDACTED),
            (r#"{"registration_token":"tiny"#, REDACTED),
            ("{not valid JSON}", "{not valid JSON}"),
            ("[not valid JSON]", "[not valid JSON]"),
            ("[unfinished", "[unfinished"),
            ("[{ id: 1 }]", "[{ id: 1 }]"),
            ("[ [{ id: 1 }] ]", "[ [{ id: 1 }] ]"),
            ("{ DB_PASSWORD=synthetic-assignment }", "{ [REDACTED] }"),
        ] {
            let value = json!(input);
            let expected = json!(expected);
            assert_eq!(redact_json(&value), expected);
            assert_eq!(memo.redact_json(&value), expected);
            assert_eq!(memo.redact_json(&value), expected);
        }
    }

    #[test]
    fn gh551_structured_parser_failures_cannot_expose_credentials() {
        let deep = format!(
            "{}{}{}",
            "[".repeat(130),
            r#"{"registration_token":"tiny"}"#,
            "]".repeat(130)
        );
        let mut memo = MemoizingRedactor::with_capacity(8);
        for input in [
            deep,
            r#"{"registration_token":"tiny","number":1e400}"#.to_owned(),
            r#"{"registration_token":"tiny","text":"\uD800"}"#.to_owned(),
            r#"{"registration_token":"tiny","text":"\x41"}"#.to_owned(),
            r#"{"registration_token":"tiny","broken":[}"#.to_owned(),
        ] {
            let value = json!(input);
            let expected = json!(REDACTED);
            assert_eq!(redact_json(&value), expected);
            assert_eq!(memo.redact_json(&value), expected);
            assert_eq!(memo.redact_json(&value), expected);
            assert_eq!(redact_json(&expected), expected);
        }
    }

    #[test]
    fn gh551_duplicate_keys_fail_closed_at_every_object_depth() {
        let mut memo = MemoizingRedactor::with_capacity(8);
        for input in [
            r#"{"registration_token":"synthetic-hidden","registration_token":null}"#,
            r#"{"outer":{"authorization":"synthetic-hidden"},"outer":{}}"#,
            r#"[{"nested":{"passphrase":"synthetic-hidden"},"nested":"safe"}]"#,
            r#"{"registration_token":"synthetic-hidden","\u0072egistration_token":null}"#,
            r#"{"safe":1,"safe":2}"#,
        ] {
            let value = json!(input);
            let expected = json!(REDACTED);
            assert_eq!(redact_json(&value), expected);
            assert_eq!(memo.redact_json(&value), expected);
            assert_eq!(memo.redact_json(&value), expected);
            assert_eq!(redact_json(&expected), expected);
        }
    }

    #[test]
    fn gh551_embedded_json_depth_is_bounded_and_idempotent()
    -> Result<(), Box<dyn std::error::Error>> {
        for wrappers in [MAX_EMBEDDED_JSON_LAYERS - 1, MAX_EMBEDDED_JSON_LAYERS] {
            let mut encoded = r#"{"registration_token":"synthetic-deep-secret"}"#.to_owned();
            for _ in 0..wrappers {
                encoded = json!({"payload": encoded}).to_string();
            }
            let input = json!(encoded);
            let output = redact_json(&input);
            assert!(!output.to_string().contains("synthetic-deep-secret"));
            assert_eq!(redact_json(&output), output);
            let mut memo = MemoizingRedactor::with_capacity(8);
            assert_eq!(memo.redact_json(&input), output);
            assert_eq!(memo.redact_json(&input), output);
            let mut decoded = output
                .as_str()
                .ok_or("top-level string missing")?
                .to_owned();
            for _ in 0..wrappers {
                let object: serde_json::Value = serde_json::from_str(&decoded)?;
                decoded = object["payload"]
                    .as_str()
                    .ok_or("encoded child missing")?
                    .to_owned();
            }
            if wrappers == MAX_EMBEDDED_JSON_LAYERS {
                assert_eq!(decoded, REDACTED, "over-depth payload must be hidden");
            } else {
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(&decoded)?,
                    json!({"registration_token": REDACTED})
                );
            }
        }
        Ok(())
    }

    #[test]
    fn redaction_enabled_default() -> Result<(), String> {
        for modern in [None, Some(""), Some(" \t\n")] {
            if !redaction_enabled_from_values(modern, None) {
                return Err(format!("unset policy must enable redaction: {modern:?}"));
            }
        }
        Ok(())
    }

    #[test]
    fn redaction_can_be_disabled() -> Result<(), String> {
        for modern in [None, Some(""), Some(" \t\n")] {
            for legacy in ["0", "false", "off", "no"] {
                if redaction_enabled_from_values(modern, Some(legacy)) {
                    return Err(format!(
                        "legacy disable was ignored: {modern:?}, {legacy:?}"
                    ));
                }
            }
            for legacy in ["", "1", "true", "OFF", " false ", "unknown"] {
                if !redaction_enabled_from_values(modern, Some(legacy)) {
                    return Err(format!(
                        "legacy exact matching changed: {modern:?}, {legacy:?}"
                    ));
                }
            }
        }
        Ok(())
    }

    /// `CASS_INDEX_REDACTION` is the documented operator switch for
    /// index-time redaction: `full` (default) / `off`, with precedence
    /// over the legacy `CASS_REDACT_SECRETS` toggle and a warn+default
    /// path for unrecognized values.
    #[test]
    fn cass_index_redaction_switch_controls_and_overrides_legacy() -> Result<(), String> {
        for legacy in [None, Some("0"), Some("1"), Some("unknown")] {
            for modern in ["off", "0", "false", "no", "none", "disabled", " OFF \t"] {
                if redaction_enabled_from_values(Some(modern), legacy) {
                    return Err(format!(
                        "modern disable lost precedence: {modern:?}, {legacy:?}"
                    ));
                }
            }
            for modern in ["full", "on", "1", "true", "yes", " FULL \t", "lazy", "無効"] {
                if !redaction_enabled_from_values(Some(modern), legacy) {
                    return Err(format!(
                        "modern full/fallback lost precedence: {modern:?}, {legacy:?}"
                    ));
                }
            }
        }
        Ok(())
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
    /// fingerprint structure (`redact-v4:<hex>`) guards against an
    /// accidental hash-format change that would silently break
    /// invalidation.
    #[test]
    fn memoizing_redactor_keys_isolate_by_algorithm_fingerprint() {
        let fingerprint = redaction_algorithm_fingerprint();
        assert!(
            fingerprint.starts_with("redact-v4:"),
            "fingerprint must carry an explicit version epoch, got: {fingerprint}"
        );
        let hex_part = fingerprint.strip_prefix("redact-v4:").unwrap();
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
            assert!(record.key.algorithm_version.starts_with("redact-v4:"));
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

    /// Every production path must match an independent union of original
    /// secret spans, including overlapping patterns, quoted multiline values,
    /// adjacent credentials, near-misses, and Unicode boundaries. Sequential
    /// replacement is deliberately not the oracle: it can hide a later match.
    #[test]
    fn production_redaction_paths_match_original_span_reference() {
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
            "PASSWORD=\"first-private-line\nsecond-private-line\"",
            "AWS_SESSION_TOKEN='first-private-line\r\nsecond-private-line'",
            "awsSecretAccessKey=\"AKIAABCDEFGHIJKLMNOP abcdefghijklmnopqrst\nTAIL\"",
            "password=\"abcd\"password=\"efgh\"",
            "PASSWORD=\"unterminated-private-line\nsynthetic-tail\\",
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
                let reference = redact_text_original_span_reference(&input);
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

    #[test]
    fn redaction_workers_share_retention_without_changing_parallel_output() {
        let first = MemoizingRedactor::new();
        let second = MemoizingRedactor::new();
        assert!(Arc::ptr_eq(&first.retention, &second.retention));
        assert!(Arc::ptr_eq(&first.retention, &REDACTION_RETENTION));

        let budget = MemoRetentionBudget::new(64 * 1024);
        std::thread::scope(|scope| {
            for worker in 0..8 {
                let budget = Arc::clone(&budget);
                scope.spawn(move || {
                    let mut redactor =
                        MemoizingRedactor::with_retention_budget(4096, Arc::clone(&budget));
                    for round in 0..16 {
                        let input = format!(
                            "worker {worker} message {round} {} password=hunter2hunter2",
                            "unicode λ padding ".repeat(1024),
                        );
                        assert!(input.len() < MemoizingRedactor::MAX_MEMOIZED_INPUT_BYTES);
                        let expected = redact_text_reference(&input).into_owned();
                        assert_eq!(redactor.redact_text(&input), expected);
                        assert_eq!(redactor.redact_text(&input), expected);
                        assert!(budget.used() <= 64 * 1024);
                    }
                });
            }
        });
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn redaction_retention_pressure_releases_values_on_clean_input_and_recovers() {
        let budget = MemoRetentionBudget::new(16 * 1024);
        let mut redactor = MemoizingRedactor::with_retention_budget(16, Arc::clone(&budget));
        let input = format!("{} password=hunter2hunter2", "padding ".repeat(256));
        let expected = redact_text_reference(&input).into_owned();
        assert_eq!(redactor.redact_text(&input), expected);
        assert_eq!(redactor.redact_text(&input), expected);
        assert_eq!(redactor.stats().hits, 1);
        assert!(budget.used() > 0);

        apply_redaction_retention_limit(&budget, 0);
        let (clean, audit) = redactor.redact_text_with_audit("ordinary clean text");
        assert_eq!(clean, "ordinary clean text");
        assert!(audit.is_empty());
        assert_eq!(budget.used(), 0);
        assert_eq!(redactor.stats().live_entries, 0);
        assert_eq!(redactor.stats().hits, 1);
        assert_eq!(redactor.redact_text(&input), expected);
        assert_eq!(budget.used(), 0);

        apply_redaction_retention_limit(&budget, usize::MAX);
        assert_eq!(budget.limit(), REDACTION_RETAINED_VALUE_BYTES);
        assert_eq!(budget.used(), 0, "growth does not restore evicted values");
        assert_eq!(redactor.redact_text(&input), expected);
        assert_eq!(redactor.stats().hits, 1, "the old value must be recomputed");
        assert_eq!(redactor.redact_text(&input), expected);
        assert_eq!(redactor.stats().hits, 2);
    }

    #[test]
    fn redaction_retention_bypass_never_bypasses_secret_redaction() {
        use crate::indexer::memoization::MemoCacheEvent;
        let budget = MemoRetentionBudget::new(8);
        let mut redactor = MemoizingRedactor::with_retention_budget(16, Arc::clone(&budget));
        let input = "Bearer abcdefghijklmnopqrstuvwxyz1234";
        for _ in 0..2 {
            let (output, audit) = redactor.redact_text_with_audit(input);
            assert_eq!(output, REDACTED);
            assert_eq!(output, redact_text_reference(input));
            assert_eq!(audit.len(), 2);
            assert_eq!(audit[0].event, MemoCacheEvent::Miss);
            assert_eq!(audit[1].event, MemoCacheEvent::BudgetBypass);
            assert!(!audit[1].changed);
            assert_eq!(budget.used(), 0);
        }
        assert_eq!(redactor.stats().inserts, 0);
        assert_eq!(redactor.stats().hits, 0);
        assert_eq!(redactor.stats().misses, 2);
    }

    #[test]
    fn redaction_retention_pressure_preserves_quarantine_and_nested_json() {
        use crate::indexer::memoization::MemoCacheEvent;
        let budget = MemoRetentionBudget::new(64 * 1024);
        let mut redactor = MemoizingRedactor::with_retention_budget(16, Arc::clone(&budget));
        let input = "line password=hunter2hunter2 trailing text";
        redactor.redact_text(input);
        redactor.quarantine(input, "retain quarantine through memory pressure");
        apply_redaction_retention_limit(&budget, 0);
        let value = json!({
            "line": input,
            "history": [input, "clean λ", null, 42],
            "password": "opaque-short-value", // ubs:ignore — synthetic redaction fixture.
            "api_key=abcdefgh12345678": "first", // ubs:ignore — synthetic collision fixture.
            "password=abcdefgh12345678": "second", // ubs:ignore — synthetic collision fixture.
            "[REDACTED]#2": "preexisting",
        });
        assert_eq!(redactor.redact_json(&value), redact_json(&value));
        assert_eq!(budget.used(), 0);
        assert_eq!(redactor.stats().quarantined, 1);
        apply_redaction_retention_limit(&budget, 4096);
        let (output, audit) = redactor.redact_text_with_audit(input);
        assert_eq!(output, redact_text_reference(input));
        assert_eq!(audit.len(), 1);
        assert!(matches!(audit[0].event, MemoCacheEvent::Quarantine { .. }));
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn json_privacy_inspection_decodes_content_metadata_and_keys() {
        let inner = r#"{"p\u0069n":"7319","ordinary":"public"}"#;
        let envelope = json!({"tool_result": inner, "\u{03bb}": true});
        let mut fields = Vec::new();
        let mut layers = Vec::new();
        inspect_json_content(&envelope.to_string(), &mut |event| match event {
            JsonPrivacyEvent::SensitiveField { value, layer, .. } => {
                fields.push(value.clone());
                layers.push(layer);
            }
            JsonPrivacyEvent::DecodedText { .. }
            | JsonPrivacyEvent::SensitiveFieldEnd
            | JsonPrivacyEvent::Ambiguous => {}
        });
        assert_eq!(fields, [json!("7319")]);
        assert_eq!(layers, [2]);
        assert!(!redact_json(&envelope).to_string().contains("7319"));

        let input = format!("tool output:\n```json\n{inner}\n```\npublic tail");
        let mut content_fields = Vec::new();
        inspect_json_content(&input, &mut |event| {
            if let JsonPrivacyEvent::SensitiveField { value, .. } = event {
                content_fields.push(value.clone());
            }
        });
        assert_eq!(content_fields, fields);
        let protected = redact_content(&input);
        assert!(!protected.contains("7319"));
        assert!(protected.contains("public tail"));
    }

    #[test]
    fn json_privacy_inspection_reports_canonical_refusals() {
        let mut deeply_encoded = r#"{"pin":"7319"}"#.to_owned();
        for _ in 0..MAX_EMBEDDED_JSON_LAYERS {
            deeply_encoded = serde_json::to_string(&deeply_encoded).unwrap();
        }
        for input in [
            r#"tool output: {"pin":"7319""#.to_owned(),
            r#"{"pin":"7319","p\u0069n":null}"#.to_owned(),
            deeply_encoded,
        ] {
            let mut refused = false;
            inspect_json_content(&input, &mut |event| {
                refused |= matches!(event, JsonPrivacyEvent::Ambiguous);
            });
            assert!(refused, "canonical refusal must be observable");
            assert!(!redact_content(&input).contains("7319"));
        }
        for input in [
            "plain discussion of pin and password fields",
            r#"Windows path "C:\Users\demo" and {value}"#,
            "[2026-11-24] public log message",
            r#"{"safe":"public","token_count":4}"#,
        ] {
            let mut refused = false;
            let mut fields = 0;
            inspect_json_content(input, &mut |event| match event {
                JsonPrivacyEvent::Ambiguous => refused = true,
                JsonPrivacyEvent::SensitiveField { .. } => fields += 1,
                JsonPrivacyEvent::DecodedText { .. } | JsonPrivacyEvent::SensitiveFieldEnd => {}
            });
            assert!(!refused, "ordinary prose must remain inspectable: {input}");
            assert_eq!(fields, 0);
            assert_eq!(redact_content(input), input);
        }
    }
}
