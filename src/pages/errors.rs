//! Centralized error types for the pages export system.
//!
//! This module provides user-friendly error types with:
//! - Clear error messages without technical jargon
//! - Recovery suggestions for each error type
//! - Security-conscious design (no secret leakage)
//!
//! # Security Considerations
//!
//! - Error messages never include passwords or secrets
//! - Debug output is sanitized
//! - Timing-safe comparisons where applicable

use std::fmt;

/// Encryption/decryption errors.
///
/// These errors are designed to be user-friendly and security-conscious.
/// They never leak sensitive information like passwords or internal state.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecryptError {
    /// Password authentication failed.
    #[error("The password you entered is incorrect.")]
    AuthenticationFailed,
    /// Empty password provided.
    #[error("Please enter a password.")]
    EmptyPassword,
    /// Invalid archive format.
    #[error("This file is not a valid archive.")]
    InvalidFormat(String),
    /// Archive integrity check failed (tampering detected).
    #[error("The archive appears to be corrupted or tampered with.")]
    IntegrityCheckFailed,
    /// Archive version not supported.
    #[error("This archive requires a newer version of the software (version {0}).")]
    UnsupportedVersion(u8),
    /// No matching key slot found.
    #[error("No matching key slot found for the provided credentials.")]
    NoMatchingKeySlot,
    /// Internal cryptographic error.
    #[error("An error occurred during decryption.")]
    CryptoError(String),
}

impl DecryptError {
    /// Get a user-friendly recovery suggestion for this error.
    pub fn suggestion(&self) -> &'static str {
        match self {
            Self::AuthenticationFailed => {
                "Double-check your password. Passwords are case-sensitive."
            }
            Self::EmptyPassword => "Please enter a password.",
            Self::InvalidFormat(_) => {
                "This file may not be a CASS archive, or it may be corrupted."
            }
            Self::IntegrityCheckFailed => {
                "The archive appears to be corrupted. Try downloading it again."
            }
            Self::UnsupportedVersion(_) => {
                "This archive was created with a newer version. Please update CASS."
            }
            Self::NoMatchingKeySlot => {
                "The credentials you provided don't match any key slot in this archive."
            }
            Self::CryptoError(_) => {
                "Please try again. If the problem persists, the archive may be corrupted."
            }
        }
    }

    /// Get a sanitized error message suitable for logging.
    ///
    /// This method ensures no sensitive information is included in logs.
    pub fn log_message(&self) -> String {
        match self {
            Self::AuthenticationFailed => "Authentication failed (wrong password)".to_string(),
            Self::EmptyPassword => "Empty password provided".to_string(),
            Self::InvalidFormat(detail) => format!("Invalid format: {}", detail),
            Self::IntegrityCheckFailed => "Integrity check failed".to_string(),
            Self::UnsupportedVersion(v) => format!("Unsupported version: {}", v),
            Self::NoMatchingKeySlot => "No matching key slot".to_string(),
            Self::CryptoError(e) => format!("Crypto error: {}", e),
        }
    }
}

/// Database errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DbError {
    /// Database file is corrupted.
    CorruptDatabase(String),
    /// Required table is missing.
    MissingTable(String),
    /// Query syntax error.
    InvalidQuery(String),
    /// Database is locked by another process.
    DatabaseLocked,
    /// Query returned no results.
    NoResults,
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CorruptDatabase(_) => {
                write!(f, "The database appears to be corrupted.")
            }
            Self::MissingTable(_) => {
                write!(f, "The archive is missing required data.")
            }
            Self::InvalidQuery(_) => {
                // Don't expose query details to users
                write!(f, "Your search could not be processed.")
            }
            Self::DatabaseLocked => {
                write!(f, "The database is currently in use by another process.")
            }
            Self::NoResults => {
                write!(f, "No results found.")
            }
        }
    }
}

impl std::error::Error for DbError {}

impl DbError {
    /// Get a user-friendly recovery suggestion.
    pub fn suggestion(&self) -> &'static str {
        match self {
            Self::CorruptDatabase(_) => {
                "The archive may be corrupted. Try downloading it again or use a backup."
            }
            Self::MissingTable(_) => "The archive may be incomplete. Try exporting again.",
            Self::InvalidQuery(_) => {
                "Try simplifying your search query or removing special characters."
            }
            Self::DatabaseLocked => {
                "Close any other applications that might be using this archive."
            }
            Self::NoResults => "Try broadening your search or using different keywords.",
        }
    }

    /// Get a sanitized error message suitable for logging.
    pub fn log_message(&self) -> String {
        match self {
            Self::CorruptDatabase(detail) => format!("Corrupt database: {}", detail),
            Self::MissingTable(table) => format!("Missing table: {}", table),
            Self::InvalidQuery(detail) => format!("Invalid query: {}", detail),
            Self::DatabaseLocked => "Database locked".to_string(),
            Self::NoResults => "No results".to_string(),
        }
    }
}

/// Browser/runtime errors (for web viewer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserError {
    /// Browser doesn't support required features.
    UnsupportedBrowser(String),
    /// WebAssembly not available.
    WasmNotSupported,
    /// WebCrypto not available.
    CryptoNotSupported,
    /// Storage quota exceeded.
    StorageQuotaExceeded,
    /// SharedArrayBuffer not available (COI not enabled).
    SharedArrayBufferNotAvailable,
}

impl fmt::Display for BrowserError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedBrowser(missing) => {
                write!(
                    f,
                    "Your browser doesn't support required features: {}",
                    missing
                )
            }
            Self::WasmNotSupported => {
                write!(f, "Your browser doesn't support WebAssembly.")
            }
            Self::CryptoNotSupported => {
                write!(f, "Your browser doesn't support secure cryptography.")
            }
            Self::StorageQuotaExceeded => {
                write!(f, "Not enough storage space available.")
            }
            Self::SharedArrayBufferNotAvailable => {
                write!(f, "Cross-origin isolation is required but not enabled.")
            }
        }
    }
}

impl std::error::Error for BrowserError {}

impl BrowserError {
    /// Get a user-friendly recovery suggestion.
    pub fn suggestion(&self) -> &'static str {
        match self {
            Self::UnsupportedBrowser(_) => {
                "Please use a modern browser like Chrome, Firefox, Edge, or Safari."
            }
            Self::WasmNotSupported => "Please update your browser to the latest version.",
            Self::CryptoNotSupported => "Please use HTTPS or update your browser.",
            Self::StorageQuotaExceeded => {
                "Clear some browser storage or use a browser with more available space."
            }
            Self::SharedArrayBufferNotAvailable => {
                "The page must be served with proper cross-origin isolation headers."
            }
        }
    }
}

/// Network errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkError {
    /// Failed to fetch resource.
    FetchFailed(String),
    /// Partial/incomplete download.
    IncompleteDownload { expected: u64, received: u64 },
    /// Connection timeout.
    Timeout,
    /// Server error.
    ServerError(u16),
}

impl fmt::Display for NetworkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FetchFailed(_) => {
                write!(f, "Failed to download the archive.")
            }
            Self::IncompleteDownload { .. } => {
                write!(f, "The download was incomplete.")
            }
            Self::Timeout => {
                write!(f, "The connection timed out.")
            }
            Self::ServerError(code) => {
                write!(f, "The server returned an error ({})", code)
            }
        }
    }
}

impl std::error::Error for NetworkError {}

impl NetworkError {
    /// Get a user-friendly recovery suggestion.
    pub fn suggestion(&self) -> &'static str {
        match self {
            Self::FetchFailed(_) => "Check your internet connection and try again.",
            Self::IncompleteDownload { .. } => {
                "Try downloading again. If the problem persists, the server may be having issues."
            }
            Self::Timeout => "Check your internet connection and try again.",
            Self::ServerError(code) if *code >= 500 => {
                "The server is having issues. Please try again later."
            }
            Self::ServerError(_) => "Please check the URL and try again.",
        }
    }
}

/// Export errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportError {
    /// No conversations to export.
    NoConversations,
    /// Source database error.
    SourceDatabaseError(String),
    /// Output directory error.
    OutputError(String),
    /// Filter matched nothing.
    FilterMatchedNothing,
}

impl fmt::Display for ExportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoConversations => {
                write!(f, "No conversations found to export.")
            }
            Self::SourceDatabaseError(_) => {
                write!(f, "Could not read the source database.")
            }
            Self::OutputError(_) => {
                write!(f, "Could not write to the output location.")
            }
            Self::FilterMatchedNothing => {
                write!(f, "No conversations matched your filter criteria.")
            }
        }
    }
}

impl std::error::Error for ExportError {}

impl ExportError {
    /// Get a user-friendly recovery suggestion.
    pub fn suggestion(&self) -> &'static str {
        match self {
            Self::NoConversations => {
                "Make sure you have some agent sessions recorded before exporting."
            }
            Self::SourceDatabaseError(_) => "Check that the CASS database exists and is readable.",
            Self::OutputError(_) => "Check that you have write permission to the output directory.",
            Self::FilterMatchedNothing => {
                "Try broadening your filter criteria or removing some filters."
            }
        }
    }
}

/// Error code for external reference (e.g., documentation).
pub trait ErrorCode {
    /// Get a unique error code for this error type.
    fn error_code(&self) -> &'static str;
}

impl ErrorCode for DecryptError {
    fn error_code(&self) -> &'static str {
        match self {
            Self::AuthenticationFailed => "E1001",
            Self::EmptyPassword => "E1002",
            Self::InvalidFormat(_) => "E1003",
            Self::IntegrityCheckFailed => "E1004",
            Self::UnsupportedVersion(_) => "E1005",
            Self::NoMatchingKeySlot => "E1006",
            Self::CryptoError(_) => "E1007",
        }
    }
}

impl ErrorCode for DbError {
    fn error_code(&self) -> &'static str {
        match self {
            Self::CorruptDatabase(_) => "E2001",
            Self::MissingTable(_) => "E2002",
            Self::InvalidQuery(_) => "E2003",
            Self::DatabaseLocked => "E2004",
            Self::NoResults => "E2005",
        }
    }
}

impl ErrorCode for BrowserError {
    fn error_code(&self) -> &'static str {
        match self {
            Self::UnsupportedBrowser(_) => "E3001",
            Self::WasmNotSupported => "E3002",
            Self::CryptoNotSupported => "E3003",
            Self::StorageQuotaExceeded => "E3004",
            Self::SharedArrayBufferNotAvailable => "E3005",
        }
    }
}

impl ErrorCode for NetworkError {
    fn error_code(&self) -> &'static str {
        match self {
            Self::FetchFailed(_) => "E4001",
            Self::IncompleteDownload { .. } => "E4002",
            Self::Timeout => "E4003",
            Self::ServerError(_) => "E4004",
        }
    }
}

impl ErrorCode for ExportError {
    fn error_code(&self) -> &'static str {
        match self {
            Self::NoConversations => "E5001",
            Self::SourceDatabaseError(_) => "E5002",
            Self::OutputError(_) => "E5003",
            Self::FilterMatchedNothing => "E5004",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decrypt_error_display_is_user_friendly() {
        let errors = vec![
            (DecryptError::AuthenticationFailed, "incorrect"),
            (DecryptError::EmptyPassword, "enter a password"),
            (
                DecryptError::InvalidFormat("test".into()),
                "not a valid archive",
            ),
            (DecryptError::IntegrityCheckFailed, "corrupted"),
            (DecryptError::UnsupportedVersion(99), "newer version"),
        ];

        for (error, expected_substring) in errors {
            let message = error.to_string().to_lowercase();
            assert!(
                message.contains(expected_substring),
                "Error {:?} should mention '{}', got: {}",
                error,
                expected_substring,
                message
            );
        }
    }

    #[test]
    fn test_decrypt_error_display_and_source_are_preserved() {
        let cases = vec![
            (
                DecryptError::AuthenticationFailed,
                "The password you entered is incorrect.",
            ),
            (DecryptError::EmptyPassword, "Please enter a password."),
            (
                DecryptError::InvalidFormat("header mismatch".into()),
                "This file is not a valid archive.",
            ),
            (
                DecryptError::IntegrityCheckFailed,
                "The archive appears to be corrupted or tampered with.",
            ),
            (
                DecryptError::UnsupportedVersion(99),
                "This archive requires a newer version of the software (version 99).",
            ),
            (
                DecryptError::NoMatchingKeySlot,
                "No matching key slot found for the provided credentials.",
            ),
            (
                DecryptError::CryptoError("GCM tag mismatch".into()),
                "An error occurred during decryption.",
            ),
        ];

        for (error, expected_display) in cases {
            assert_eq!(error.to_string(), expected_display);
            assert!(std::error::Error::source(&error).is_none());
        }
    }

    #[test]
    fn test_decrypt_error_no_technical_jargon() {
        let errors = vec![
            DecryptError::AuthenticationFailed,
            DecryptError::EmptyPassword,
            DecryptError::InvalidFormat("header mismatch".into()),
            DecryptError::IntegrityCheckFailed,
            DecryptError::UnsupportedVersion(2),
            DecryptError::CryptoError("GCM tag mismatch".into()),
        ];

        let jargon = ["GCM", "tag", "nonce", "AEAD", "AES", "cipher", "MAC"];

        for error in errors {
            let display = error.to_string();
            for word in jargon {
                assert!(
                    !display.contains(word),
                    "Error {:?} should not contain '{}' in display: {}",
                    error,
                    word,
                    display
                );
            }
        }
    }

    #[test]
    fn test_all_errors_have_suggestions() {
        let decrypt_errors = vec![
            DecryptError::AuthenticationFailed,
            DecryptError::EmptyPassword,
            DecryptError::InvalidFormat("test".into()),
            DecryptError::IntegrityCheckFailed,
            DecryptError::UnsupportedVersion(2),
            DecryptError::NoMatchingKeySlot,
            DecryptError::CryptoError("test".into()),
        ];

        for error in decrypt_errors {
            let suggestion = error.suggestion();
            assert!(!suggestion.is_empty(), "{:?} has no suggestion", error);
            assert!(
                suggestion.ends_with('.') || suggestion.ends_with('!'),
                "{:?} suggestion should be a complete sentence: {}",
                error,
                suggestion
            );
        }
    }

    #[test]
    fn test_db_error_display_is_user_friendly() {
        let errors = vec![
            (DbError::CorruptDatabase("test".into()), "corrupted"),
            (DbError::MissingTable("messages".into()), "missing"),
            (DbError::InvalidQuery("syntax error".into()), "search"),
            (DbError::DatabaseLocked, "in use"),
            (DbError::NoResults, "no results"),
        ];

        for (error, expected_substring) in errors {
            let message = error.to_string().to_lowercase();
            assert!(
                message.contains(expected_substring),
                "Error {:?} should mention '{}', got: {}",
                error,
                expected_substring,
                message
            );
        }
    }

    #[test]
    fn test_db_error_no_internal_details() {
        let error = DbError::InvalidQuery("SELECT * FROM sqlite_master WHERE type='table'".into());
        let display = error.to_string();

        // Should not expose SQL details
        assert!(
            !display.contains("sqlite"),
            "Should not expose sqlite in display: {}",
            display
        );
        assert!(
            !display.contains("SELECT"),
            "Should not expose SQL in display: {}",
            display
        );
    }

    #[test]
    fn test_error_codes_are_unique() {
        let mut codes = std::collections::HashSet::new();

        let decrypt_errors = vec![
            DecryptError::AuthenticationFailed,
            DecryptError::EmptyPassword,
            DecryptError::InvalidFormat("".into()),
            DecryptError::IntegrityCheckFailed,
            DecryptError::UnsupportedVersion(0),
            DecryptError::NoMatchingKeySlot,
            DecryptError::CryptoError("".into()),
        ];

        for error in decrypt_errors {
            let code = error.error_code();
            assert!(codes.insert(code), "Duplicate error code: {}", code);
        }

        let db_errors = vec![
            DbError::CorruptDatabase("".into()),
            DbError::MissingTable("".into()),
            DbError::InvalidQuery("".into()),
            DbError::DatabaseLocked,
            DbError::NoResults,
        ];

        for error in db_errors {
            let code = error.error_code();
            assert!(codes.insert(code), "Duplicate error code: {}", code);
        }
    }

    #[test]
    fn test_browser_error_suggestions() {
        let errors = vec![
            BrowserError::UnsupportedBrowser("IndexedDB".into()),
            BrowserError::WasmNotSupported,
            BrowserError::CryptoNotSupported,
            BrowserError::StorageQuotaExceeded,
            BrowserError::SharedArrayBufferNotAvailable,
        ];

        for error in errors {
            let suggestion = error.suggestion();
            assert!(!suggestion.is_empty(), "{:?} has no suggestion", error);
        }
    }

    #[test]
    fn test_network_error_suggestions() {
        let errors = vec![
            NetworkError::FetchFailed("connection refused".into()),
            NetworkError::IncompleteDownload {
                expected: 1000,
                received: 500,
            },
            NetworkError::Timeout,
            NetworkError::ServerError(500),
            NetworkError::ServerError(404),
        ];

        for error in errors {
            let suggestion = error.suggestion();
            assert!(!suggestion.is_empty(), "{:?} has no suggestion", error);
        }
    }
}
