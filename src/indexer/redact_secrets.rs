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

use once_cell::sync::Lazy;
use regex::Regex;

/// Placeholder inserted where a secret was found.
const REDACTED: &str = "[REDACTED]";

/// A compiled secret-detection pattern.
struct SecretPattern {
    regex: Regex,
}

/// All built-in patterns, compiled once on first use.
static SECRET_PATTERNS: Lazy<Vec<SecretPattern>> = Lazy::new(|| {
    vec![
        // AWS Access Key ID (always starts with AKIA)
        SecretPattern {
            regex: Regex::new(r"\bAKIA[0-9A-Z]{16}\b").expect("aws access key regex"),
        },
        // AWS Secret Key in assignment context
        SecretPattern {
            regex: Regex::new(
                r#"(?i)aws(.{0,20})?(secret|access)?[_-]?key\s*[:=]\s*['"]?[A-Za-z0-9/+=]{40}['"]?"#,
            )
            .expect("aws secret regex"),
        },
        // GitHub PAT (ghp_, gho_, ghu_, ghs_, ghr_)
        SecretPattern {
            regex: Regex::new(r"\bgh[pousr]_[A-Za-z0-9]{36}\b").expect("github pat regex"),
        },
        // OpenAI API key (sk-...)
        SecretPattern {
            regex: Regex::new(r"\bsk-[A-Za-z0-9]{20,}\b").expect("openai key regex"),
        },
        // Anthropic API key (sk-ant-...)
        SecretPattern {
            regex: Regex::new(r"\bsk-ant-[A-Za-z0-9]{20,}\b").expect("anthropic key regex"),
        },
        // Bearer tokens in authorization headers
        SecretPattern {
            regex: Regex::new(r"(?i)Bearer\s+[A-Za-z0-9_\-.]{20,}").expect("bearer token regex"),
        },
        // JWT tokens (eyJ...)
        SecretPattern {
            regex: Regex::new(r"\beyJ[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+\b")
                .expect("jwt regex"),
        },
        // PEM private keys
        SecretPattern {
            regex: Regex::new(r"-----BEGIN (?:RSA|EC|DSA|OPENSSH|PGP) PRIVATE KEY-----")
                .expect("private key regex"),
        },
        // Database connection URLs with credentials
        SecretPattern {
            regex: Regex::new(
                r"(?i)\b(postgres|postgresql|mysql|mongodb|redis)://[^\s]{8,}",
            )
            .expect("db url regex"),
        },
        // Generic key/token/secret/password assignments
        SecretPattern {
            regex: Regex::new(
                r#"(?i)(api[_-]?key|api[_-]?secret|auth[_-]?token|access[_-]?token|secret[_-]?key|password|passwd)\s*[:=]\s*['"]?[A-Za-z0-9_\-/+=]{8,}['"]?"#,
            )
            .expect("generic api key regex"),
        },
        // Slack tokens (xoxb-, xoxp-, xoxs-, xoxa-, xoxo-, xoxr-)
        SecretPattern {
            regex: Regex::new(r"\bxox[bpsar]-[A-Za-z0-9\-]{10,}").expect("slack token regex"),
        },
        // Stripe keys (sk_live_, pk_live_, rk_live_)
        SecretPattern {
            regex: Regex::new(r"\b[spr]k_live_[A-Za-z0-9]{20,}").expect("stripe key regex"),
        },
    ]
});

/// Redact secrets from a plain-text string.
///
/// Returns the input unchanged if no secrets are detected.
pub fn redact_text(input: &str) -> String {
    let mut output = input.to_string();
    for pat in SECRET_PATTERNS.iter() {
        output = pat.regex.replace_all(&output, REDACTED).into_owned();
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
            let redacted = redact_text(s);
            serde_json::Value::String(redacted)
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(redact_json).collect())
        }
        serde_json::Value::Object(obj) => {
            let mut new_obj = serde_json::Map::new();
            for (k, v) in obj {
                new_obj.insert(k.clone(), redact_json(v));
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
    match std::env::var("CASS_REDACT_SECRETS") {
        Ok(val) => !matches!(val.as_str(), "0" | "false" | "off" | "no"),
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
        assert_eq!(
            output["tool_result"],
            json!("Response contains [REDACTED]")
        );
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
    fn redaction_enabled_default() {
        // When env var is not set, should be enabled
        // Safety: only called in single-threaded test context
        unsafe { std::env::remove_var("CASS_REDACT_SECRETS") };
        assert!(redaction_enabled());
    }

    #[test]
    fn multiple_secrets_in_one_string() {
        let input = "key1=sk-ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij and key2=ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij";
        let output = redact_text(input);
        assert!(!output.contains("sk-ABCDE"));
        assert!(!output.contains("ghp_ABCDE"));
        assert_eq!(output.matches("[REDACTED]").count(), 2);
    }
}
