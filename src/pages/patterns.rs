//! Pattern library for privacy profiles.
//!
//! This module provides pre-defined regex patterns for redacting sensitive data.
//! Patterns are categorized by type and can be composed into profiles with different
//! privacy levels.

use once_cell::sync::Lazy;
use regex::Regex;

use crate::pages::redact::CustomPattern;

/// Categories of sensitive patterns for organizational clarity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PatternCategory {
    /// API keys and tokens (AWS, OpenAI, Anthropic, GitHub, etc.)
    ApiKeys,
    /// Private keys (SSH, PEM, PGP)
    PrivateKeys,
    /// Database and service connection strings
    ConnectionStrings,
    /// Personal identifiable information
    PersonalInfo,
    /// Internal infrastructure references
    InternalUrls,
}

impl PatternCategory {
    pub fn label(self) -> &'static str {
        match self {
            PatternCategory::ApiKeys => "API Keys & Tokens",
            PatternCategory::PrivateKeys => "Private Keys",
            PatternCategory::ConnectionStrings => "Connection Strings",
            PatternCategory::PersonalInfo => "Personal Information",
            PatternCategory::InternalUrls => "Internal URLs",
        }
    }
}

/// A pattern definition with metadata for display and categorization.
#[derive(Debug, Clone)]
pub struct PatternDef {
    pub id: &'static str,
    pub name: &'static str,
    pub category: PatternCategory,
    pub description: &'static str,
    pub pattern: &'static str,
    pub replacement: &'static str,
}

// ============================================================================
// API Keys & Tokens
// ============================================================================

pub static AWS_ACCESS_KEY: PatternDef = PatternDef {
    id: "aws_access_key",
    name: "AWS Access Key ID",
    category: PatternCategory::ApiKeys,
    description: "AWS access key identifiers (AKIA...)",
    pattern: r"\bAKIA[0-9A-Z]{16}\b",
    replacement: "[AWS_KEY_REDACTED]",
};

pub static AWS_SECRET_KEY: PatternDef = PatternDef {
    id: "aws_secret_key",
    name: "AWS Secret Key",
    category: PatternCategory::ApiKeys,
    description: "AWS secret access keys in configuration contexts",
    pattern: r#"(?i)aws(.{0,20})?(secret|access)?[_-]?key\s*[:=]\s*['"]?[A-Za-z0-9/+=]{40}['"]?"#,
    replacement: "[AWS_SECRET_REDACTED]",
};

pub static OPENAI_KEY: PatternDef = PatternDef {
    id: "openai_key",
    name: "OpenAI API Key",
    category: PatternCategory::ApiKeys,
    description: "OpenAI API keys (sk-...)",
    pattern: r"\bsk-[A-Za-z0-9]{20,}\b",
    replacement: "[OPENAI_KEY_REDACTED]",
};

pub static ANTHROPIC_KEY: PatternDef = PatternDef {
    id: "anthropic_key",
    name: "Anthropic API Key",
    category: PatternCategory::ApiKeys,
    description: "Anthropic API keys (sk-ant-...)",
    pattern: r"\bsk-ant-[A-Za-z0-9\-]{20,}\b",
    replacement: "[ANTHROPIC_KEY_REDACTED]",
};

pub static GITHUB_TOKEN: PatternDef = PatternDef {
    id: "github_token",
    name: "GitHub Token",
    category: PatternCategory::ApiKeys,
    description: "GitHub personal access tokens and app tokens",
    pattern: r"\bgh[pousr]_[A-Za-z0-9]{36}\b",
    replacement: "[GITHUB_TOKEN_REDACTED]",
};

pub static GENERIC_API_KEY: PatternDef = PatternDef {
    id: "generic_api_key",
    name: "Generic API Key",
    category: PatternCategory::ApiKeys,
    description: "Generic API keys, tokens, and secrets in assignment contexts",
    pattern: r#"(?i)(api[_-]?key|api[_-]?token|auth[_-]?token|access[_-]?token|secret[_-]?key)\s*[:=]\s*['"]?[A-Za-z0-9_\-]{16,}['"]?"#,
    replacement: "[API_KEY_REDACTED]",
};

pub static BEARER_TOKEN: PatternDef = PatternDef {
    id: "bearer_token",
    name: "Bearer Token",
    category: PatternCategory::ApiKeys,
    description: "Bearer authorization tokens in headers",
    pattern: r"(?i)Bearer\s+[A-Za-z0-9\-_.~+/]+=*",
    replacement: "Bearer [TOKEN_REDACTED]",
};

// ============================================================================
// Private Keys
// ============================================================================

pub static SSH_PRIVATE_KEY: PatternDef = PatternDef {
    id: "ssh_private_key",
    name: "SSH Private Key",
    category: PatternCategory::PrivateKeys,
    description: "SSH and OpenSSH private key headers",
    pattern: r"-----BEGIN (?:RSA |EC |DSA |OPENSSH )?PRIVATE KEY-----",
    replacement: "[PRIVATE_KEY_REDACTED]",
};

pub static PEM_PRIVATE_KEY: PatternDef = PatternDef {
    id: "pem_private_key",
    name: "PEM Private Key",
    category: PatternCategory::PrivateKeys,
    description: "PEM-encoded private keys",
    pattern: r"-----BEGIN (?:ENCRYPTED )?PRIVATE KEY-----",
    replacement: "[PRIVATE_KEY_REDACTED]",
};

pub static PGP_PRIVATE_KEY: PatternDef = PatternDef {
    id: "pgp_private_key",
    name: "PGP Private Key",
    category: PatternCategory::PrivateKeys,
    description: "PGP/GPG private key blocks",
    pattern: r"-----BEGIN PGP PRIVATE KEY BLOCK-----",
    replacement: "[PGP_KEY_REDACTED]",
};

// ============================================================================
// Connection Strings
// ============================================================================

pub static DATABASE_URL: PatternDef = PatternDef {
    id: "database_url",
    name: "Database URL",
    category: PatternCategory::ConnectionStrings,
    description: "PostgreSQL, MySQL, MongoDB, and Redis connection strings",
    pattern: r#"(?i)\b(postgres(?:ql)?|mysql|mongodb(?:\+srv)?|redis|amqp)://[^\s'""]+"#,
    replacement: "[DATABASE_URL_REDACTED]",
};

pub static DATABASE_PASSWORD: PatternDef = PatternDef {
    id: "database_password",
    name: "Database Password",
    category: PatternCategory::ConnectionStrings,
    description: "Database passwords in configuration",
    pattern: r#"(?i)(db[_-]?pass(?:word)?|database[_-]?pass(?:word)?)\s*[:=]\s*['"]?[^\s'"]{4,}['"]?"#,
    replacement: "[DB_PASSWORD_REDACTED]",
};

pub static CONNECTION_STRING: PatternDef = PatternDef {
    id: "connection_string",
    name: "Connection String",
    category: PatternCategory::ConnectionStrings,
    description: "Generic connection strings with credentials",
    pattern: r#"(?i)(connection[_-]?string|conn[_-]?str)\s*[:=]\s*['"][^'"]+['"]"#,
    replacement: "[CONNECTION_STRING_REDACTED]",
};

// ============================================================================
// Personal Information
// ============================================================================

pub static EMAIL_ADDRESS: PatternDef = PatternDef {
    id: "email_address",
    name: "Email Address",
    category: PatternCategory::PersonalInfo,
    description: "Email addresses",
    pattern: r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}\b",
    replacement: "[EMAIL_REDACTED]",
};

pub static PHONE_NUMBER: PatternDef = PatternDef {
    id: "phone_number",
    name: "Phone Number",
    category: PatternCategory::PersonalInfo,
    description: "Phone numbers in various formats",
    pattern: r"\b(?:\+?1[-.\s]?)?\(?[0-9]{3}\)?[-.\s]?[0-9]{3}[-.\s]?[0-9]{4}\b",
    replacement: "[PHONE_REDACTED]",
};

pub static IP_ADDRESS: PatternDef = PatternDef {
    id: "ip_address",
    name: "IP Address",
    category: PatternCategory::PersonalInfo,
    description: "IPv4 addresses (all addresses matched; private ranges handled separately)",
    pattern: r"\b(?:(?:25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)\.){3}(?:25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)\b",
    replacement: "[IP_REDACTED]",
};

pub static SOCIAL_SECURITY: PatternDef = PatternDef {
    id: "social_security",
    name: "Social Security Number",
    category: PatternCategory::PersonalInfo,
    description: "US Social Security Numbers",
    pattern: r"\b[0-9]{3}-[0-9]{2}-[0-9]{4}\b",
    replacement: "[SSN_REDACTED]",
};

pub static CREDIT_CARD: PatternDef = PatternDef {
    id: "credit_card",
    name: "Credit Card Number",
    category: PatternCategory::PersonalInfo,
    description: "Credit card numbers (basic pattern)",
    pattern: r"\b(?:4[0-9]{12}(?:[0-9]{3})?|5[1-5][0-9]{14}|3[47][0-9]{13}|6(?:011|5[0-9]{2})[0-9]{12})\b",
    replacement: "[CARD_REDACTED]",
};

// ============================================================================
// Internal URLs
// ============================================================================

pub static INTERNAL_URL: PatternDef = PatternDef {
    id: "internal_url",
    name: "Internal URL",
    category: PatternCategory::InternalUrls,
    description: "URLs with internal/corporate domains",
    pattern: r"https?://[a-zA-Z0-9.-]+\.(internal|local|corp|intra|private|lan)\b[^\s]*",
    replacement: "[INTERNAL_URL_REDACTED]",
};

pub static LOCALHOST_URL: PatternDef = PatternDef {
    id: "localhost_url",
    name: "Localhost URL",
    category: PatternCategory::InternalUrls,
    description: "Localhost and 127.0.0.1 URLs",
    pattern: r"https?://(?:localhost|127\.0\.0\.1)(?::[0-9]+)?[^\s]*",
    replacement: "[LOCALHOST_URL_REDACTED]",
};

pub static PRIVATE_IP_URL: PatternDef = PatternDef {
    id: "private_ip_url",
    name: "Private IP URL",
    category: PatternCategory::InternalUrls,
    description: "URLs with private IP addresses",
    pattern: r"https?://(?:10\.|192\.168\.|172\.(?:1[6-9]|2[0-9]|3[01])\.)[0-9.]+(?::[0-9]+)?[^\s]*",
    replacement: "[PRIVATE_IP_URL_REDACTED]",
};

// ============================================================================
// Pattern Collections
// ============================================================================

/// All defined patterns for iteration.
pub static ALL_PATTERNS: Lazy<Vec<&'static PatternDef>> = Lazy::new(|| {
    vec![
        // API Keys
        &AWS_ACCESS_KEY,
        &AWS_SECRET_KEY,
        &OPENAI_KEY,
        &ANTHROPIC_KEY,
        &GITHUB_TOKEN,
        &GENERIC_API_KEY,
        &BEARER_TOKEN,
        // Private Keys
        &SSH_PRIVATE_KEY,
        &PEM_PRIVATE_KEY,
        &PGP_PRIVATE_KEY,
        // Connection Strings
        &DATABASE_URL,
        &DATABASE_PASSWORD,
        &CONNECTION_STRING,
        // Personal Info
        &EMAIL_ADDRESS,
        &PHONE_NUMBER,
        &IP_ADDRESS,
        &SOCIAL_SECURITY,
        &CREDIT_CARD,
        // Internal URLs
        &INTERNAL_URL,
        &LOCALHOST_URL,
        &PRIVATE_IP_URL,
    ]
});

impl PatternDef {
    /// Convert this pattern definition to a CustomPattern for the redaction engine.
    pub fn to_custom_pattern(&self) -> Option<CustomPattern> {
        let regex = Regex::new(self.pattern).ok()?;
        Some(CustomPattern {
            name: self.name.to_string(),
            pattern: regex,
            replacement: self.replacement.to_string(),
            enabled: true,
        })
    }
}

/// Get patterns for public sharing (maximum redaction).
///
/// Includes all pattern categories for thorough data sanitization.
pub fn patterns_for_public() -> Vec<CustomPattern> {
    let patterns = [
        // All API keys and tokens
        &AWS_ACCESS_KEY,
        &AWS_SECRET_KEY,
        &OPENAI_KEY,
        &ANTHROPIC_KEY,
        &GITHUB_TOKEN,
        &GENERIC_API_KEY,
        &BEARER_TOKEN,
        // All private keys
        &SSH_PRIVATE_KEY,
        &PEM_PRIVATE_KEY,
        &PGP_PRIVATE_KEY,
        // All connection strings
        &DATABASE_URL,
        &DATABASE_PASSWORD,
        &CONNECTION_STRING,
        // All personal info
        &EMAIL_ADDRESS,
        &PHONE_NUMBER,
        &IP_ADDRESS,
        &SOCIAL_SECURITY,
        &CREDIT_CARD,
        // All internal URLs
        &INTERNAL_URL,
        &LOCALHOST_URL,
        &PRIVATE_IP_URL,
    ];

    patterns
        .iter()
        .filter_map(|p| p.to_custom_pattern())
        .collect()
}

/// Get patterns for team sharing (moderate redaction).
///
/// Includes external credentials but allows internal references.
pub fn patterns_for_team() -> Vec<CustomPattern> {
    let patterns = [
        // External API keys only
        &AWS_ACCESS_KEY,
        &AWS_SECRET_KEY,
        &OPENAI_KEY,
        &ANTHROPIC_KEY,
        &GITHUB_TOKEN,
        // Private keys (always sensitive)
        &SSH_PRIVATE_KEY,
        &PEM_PRIVATE_KEY,
        &PGP_PRIVATE_KEY,
        // External service credentials
        &DATABASE_URL,
        &DATABASE_PASSWORD,
        // External personal info
        &EMAIL_ADDRESS,
        &SOCIAL_SECURITY,
        &CREDIT_CARD,
    ];

    patterns
        .iter()
        .filter_map(|p| p.to_custom_pattern())
        .collect()
}

/// Get patterns for personal backup (minimal redaction).
///
/// Only removes critical secrets like private keys and cloud credentials.
pub fn patterns_for_personal() -> Vec<CustomPattern> {
    let patterns = [
        // Critical private keys only
        &SSH_PRIVATE_KEY,
        &PEM_PRIVATE_KEY,
        &PGP_PRIVATE_KEY,
        // Cloud provider keys
        &AWS_ACCESS_KEY,
        &AWS_SECRET_KEY,
        // Database credentials with passwords
        &DATABASE_PASSWORD,
    ];

    patterns
        .iter()
        .filter_map(|p| p.to_custom_pattern())
        .collect()
}

/// Get patterns by category.
pub fn patterns_by_category(category: PatternCategory) -> Vec<&'static PatternDef> {
    ALL_PATTERNS
        .iter()
        .filter(|p| p.category == category)
        .copied()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_all_patterns_compile() {
        for pattern in ALL_PATTERNS.iter() {
            let result = Regex::new(pattern.pattern);
            assert!(
                result.is_ok(),
                "Pattern {} failed to compile: {:?}",
                pattern.id,
                result.err()
            );
        }
    }

    #[test]
    fn test_to_custom_pattern() {
        let custom = AWS_ACCESS_KEY.to_custom_pattern();
        assert!(custom.is_some());
        let custom = custom.unwrap();
        assert_eq!(custom.name, "AWS Access Key ID");
        assert!(custom.enabled);
    }

    #[test]
    fn test_public_has_most_patterns() {
        let public = patterns_for_public();
        let team = patterns_for_team();
        let personal = patterns_for_personal();

        assert!(public.len() >= team.len());
        assert!(team.len() >= personal.len());
    }

    #[test]
    fn test_personal_has_critical_patterns() {
        let personal = patterns_for_personal();

        // Should have private key patterns
        assert!(personal.iter().any(|p| p.name.contains("Private Key")));

        // Should have AWS patterns
        assert!(personal.iter().any(|p| p.name.contains("AWS")));
    }

    #[test]
    fn test_patterns_by_category() {
        let api_patterns = patterns_by_category(PatternCategory::ApiKeys);
        assert!(!api_patterns.is_empty());
        assert!(
            api_patterns
                .iter()
                .all(|p| p.category == PatternCategory::ApiKeys)
        );
    }

    #[test]
    fn test_pattern_matches_aws_key() {
        let pattern = Regex::new(AWS_ACCESS_KEY.pattern).unwrap();
        assert!(pattern.is_match("Found key AKIAIOSFODNN7EXAMPLE in config"));
        assert!(!pattern.is_match("Not a key"));
    }

    #[test]
    fn test_pattern_matches_openai_key() {
        let pattern = Regex::new(OPENAI_KEY.pattern).unwrap();
        assert!(pattern.is_match("Using sk-abc123def456ghi789jkl012mno345pqr678"));
        assert!(!pattern.is_match("sk-short")); // Too short
    }

    #[test]
    fn test_pattern_matches_email() {
        let pattern = Regex::new(EMAIL_ADDRESS.pattern).unwrap();
        assert!(pattern.is_match("Contact user@example.com for help"));
        assert!(pattern.is_match("test.user+tag@sub.domain.org"));
    }

    #[test]
    fn test_email_pattern_uses_ascii_letter_classes() {
        let pattern = Regex::new(EMAIL_ADDRESS.pattern).unwrap();

        assert!(pattern.is_match("Contact USER_123@example.COM"));
        assert!(!pattern.is_match("Contact user@example.δοκιμή"));
        assert!(EMAIL_ADDRESS.pattern.contains("[A-Za-z]"));
        assert!(!EMAIL_ADDRESS.pattern.contains("\\p"));
    }

    #[test]
    fn test_pattern_matches_database_url() {
        let pattern = Regex::new(DATABASE_URL.pattern).unwrap();
        assert!(pattern.is_match("postgres://user:pass@host:5432/db"));
        assert!(pattern.is_match("mongodb+srv://user:pass@cluster.mongodb.net/db"));
        assert!(pattern.is_match("redis://localhost:6379"));
    }

    #[test]
    fn test_pattern_matches_private_key() {
        let pattern = Regex::new(SSH_PRIVATE_KEY.pattern).unwrap();
        assert!(pattern.is_match("-----BEGIN RSA PRIVATE KEY-----"));
        assert!(pattern.is_match("-----BEGIN OPENSSH PRIVATE KEY-----"));
        assert!(pattern.is_match("-----BEGIN PRIVATE KEY-----"));
    }
}
