//! Archive configuration types for pages bundles.
//!
//! Supports both encrypted and unencrypted bundles via an untagged enum.

use serde::{Deserialize, Serialize};

use super::encrypt::EncryptionConfig;

/// Supported archive configuration formats.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ArchiveConfig {
    /// Encrypted bundle configuration (default).
    Encrypted(EncryptionConfig),
    /// Unencrypted bundle configuration.
    Unencrypted(UnencryptedConfig),
}

impl ArchiveConfig {
    /// Returns true if this config represents an encrypted bundle.
    pub fn is_encrypted(&self) -> bool {
        matches!(self, ArchiveConfig::Encrypted(_))
    }

    /// Get the encrypted config if available.
    pub fn as_encrypted(&self) -> Option<&EncryptionConfig> {
        match self {
            ArchiveConfig::Encrypted(cfg) => Some(cfg),
            ArchiveConfig::Unencrypted(_) => None,
        }
    }

    /// Get the unencrypted config if available.
    pub fn as_unencrypted(&self) -> Option<&UnencryptedConfig> {
        match self {
            ArchiveConfig::Encrypted(_) => None,
            ArchiveConfig::Unencrypted(cfg) => Some(cfg),
        }
    }
}

/// Unencrypted bundle configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnencryptedConfig {
    /// Whether the bundle is encrypted (must be false).
    pub encrypted: bool,
    /// Config version.
    pub version: String,
    /// Payload descriptor.
    pub payload: UnencryptedPayload,
    /// Optional warning message for viewers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// Unencrypted payload descriptor.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnencryptedPayload {
    /// Relative path to the SQLite database payload.
    pub path: String,
    /// Payload format (e.g., "sqlite").
    pub format: String,
    /// Optional byte size of the payload.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // Helper to create a minimal UnencryptedPayload
    fn make_unencrypted_payload() -> UnencryptedPayload {
        UnencryptedPayload {
            path: "data.sqlite".to_string(),
            format: "sqlite".to_string(),
            size_bytes: None,
        }
    }

    // Helper to create a minimal UnencryptedConfig
    fn make_unencrypted_config() -> UnencryptedConfig {
        UnencryptedConfig {
            encrypted: false,
            version: "1.0".to_string(),
            payload: make_unencrypted_payload(),
            warning: None,
        }
    }

    // Helper to create a minimal EncryptionConfig for testing
    fn make_encryption_config() -> EncryptionConfig {
        use crate::pages::encrypt::{Argon2Params, PayloadMeta};

        EncryptionConfig {
            version: 1,
            export_id: "AAAAAAAAAAAAAAAAAAAAAA==".to_string(),
            base_nonce: "AAAAAAAAAAAAAAA=".to_string(),
            compression: "deflate".to_string(),
            kdf_defaults: Argon2Params::default(),
            payload: PayloadMeta {
                chunk_size: 8 * 1024 * 1024,
                chunk_count: 1,
                total_compressed_size: 1024,
                total_plaintext_size: 2048,
                files: vec!["chunk_0".to_string()],
            },
            key_slots: vec![],
        }
    }

    // ==================== ArchiveConfig::is_encrypted() tests ====================

    #[test]
    fn test_is_encrypted_returns_true_for_encrypted_variant() {
        let config = ArchiveConfig::Encrypted(make_encryption_config());
        assert!(config.is_encrypted());
    }

    #[test]
    fn test_is_encrypted_returns_false_for_unencrypted_variant() {
        let config = ArchiveConfig::Unencrypted(make_unencrypted_config());
        assert!(!config.is_encrypted());
    }

    // ==================== ArchiveConfig::as_encrypted() tests ====================

    #[test]
    fn test_as_encrypted_returns_some_for_encrypted_variant() {
        let inner = make_encryption_config();
        let config = ArchiveConfig::Encrypted(inner.clone());
        let result = config.as_encrypted();
        assert!(result.is_some());
        assert_eq!(result.unwrap().version, inner.version);
        assert_eq!(result.unwrap().export_id, inner.export_id);
    }

    #[test]
    fn test_as_encrypted_returns_none_for_unencrypted_variant() {
        let config = ArchiveConfig::Unencrypted(make_unencrypted_config());
        assert!(config.as_encrypted().is_none());
    }

    // ==================== ArchiveConfig::as_unencrypted() tests ====================

    #[test]
    fn test_as_unencrypted_returns_some_for_unencrypted_variant() {
        let inner = make_unencrypted_config();
        let config = ArchiveConfig::Unencrypted(inner.clone());
        let result = config.as_unencrypted();
        assert!(result.is_some());
        assert_eq!(result.unwrap().version, inner.version);
        assert!(!result.unwrap().encrypted);
    }

    #[test]
    fn test_as_unencrypted_returns_none_for_encrypted_variant() {
        let config = ArchiveConfig::Encrypted(make_encryption_config());
        assert!(config.as_unencrypted().is_none());
    }

    // ==================== Serialization round-trip tests ====================

    #[test]
    fn test_unencrypted_config_serialization_roundtrip() {
        let original = make_unencrypted_config();
        let json = serde_json::to_string(&original).expect("serialize");
        let deserialized: UnencryptedConfig = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(original.encrypted, deserialized.encrypted);
        assert_eq!(original.version, deserialized.version);
        assert_eq!(original.payload.path, deserialized.payload.path);
        assert_eq!(original.payload.format, deserialized.payload.format);
        assert_eq!(original.warning, deserialized.warning);
    }

    #[test]
    fn test_unencrypted_config_with_optional_fields_roundtrip() {
        let original = UnencryptedConfig {
            encrypted: false,
            version: "2.0".to_string(),
            payload: UnencryptedPayload {
                path: "archive/data.sqlite".to_string(),
                format: "sqlite".to_string(),
                size_bytes: Some(123456),
            },
            warning: Some("This bundle is unencrypted!".to_string()),
        };

        let json = serde_json::to_string(&original).expect("serialize");
        let deserialized: UnencryptedConfig = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(original.payload.size_bytes, deserialized.payload.size_bytes);
        assert_eq!(original.warning, deserialized.warning);
    }

    #[test]
    fn test_archive_config_unencrypted_roundtrip() {
        let original = ArchiveConfig::Unencrypted(make_unencrypted_config());
        let json = serde_json::to_string(&original).expect("serialize");
        let deserialized: ArchiveConfig = serde_json::from_str(&json).expect("deserialize");

        assert!(!deserialized.is_encrypted());
        let inner = deserialized
            .as_unencrypted()
            .expect("should be unencrypted");
        assert_eq!(inner.version, "1.0");
    }

    #[test]
    fn test_archive_config_encrypted_roundtrip() {
        let original = ArchiveConfig::Encrypted(make_encryption_config());
        let json = serde_json::to_string(&original).expect("serialize");
        let deserialized: ArchiveConfig = serde_json::from_str(&json).expect("deserialize");

        assert!(deserialized.is_encrypted());
        let inner = deserialized.as_encrypted().expect("should be encrypted");
        assert_eq!(inner.version, 1);
        assert_eq!(inner.compression, "deflate");
    }

    // ==================== Serde untagged behavior tests ====================

    #[test]
    fn test_untagged_deserialize_encrypted_json() {
        // JSON that matches EncryptionConfig structure
        let json = r#"{
            "version": 1,
            "export_id": "dGVzdGV4cG9ydGlkMTIz",
            "base_nonce": "dGVzdG5vbmNlMTI=",
            "compression": "gzip",
            "kdf_defaults": {
                "memory_kb": 65536,
                "iterations": 3,
                "parallelism": 4
            },
            "payload": {
                "chunk_size": 4194304,
                "chunk_count": 2,
                "total_compressed_size": 2048,
                "total_plaintext_size": 4096,
                "files": ["chunk_0", "chunk_1"]
            },
            "key_slots": []
        }"#;

        let config: ArchiveConfig = serde_json::from_str(json).expect("deserialize");
        assert!(config.is_encrypted());
    }

    #[test]
    fn test_untagged_deserialize_unencrypted_json() {
        // JSON that matches UnencryptedConfig structure
        let json = r#"{
            "encrypted": false,
            "version": "1.0",
            "payload": {
                "path": "payload.sqlite",
                "format": "sqlite"
            }
        }"#;

        let config: ArchiveConfig = serde_json::from_str(json).expect("deserialize");
        assert!(!config.is_encrypted());
        let inner = config.as_unencrypted().expect("should be unencrypted");
        assert_eq!(inner.payload.path, "payload.sqlite");
    }

    #[test]
    fn test_untagged_deserialize_rejects_unknown_top_level_field() {
        let json = r#"{
            "encrypted": false,
            "version": "1.0",
            "payload": {
                "path": "payload.sqlite",
                "format": "sqlite"
            },
            "totally_unknown_field": 123
        }"#;

        serde_json::from_str::<ArchiveConfig>(json).expect_err("should reject unknown");
    }

    #[test]
    fn test_untagged_deserialize_rejects_unknown_nested_payload_field() {
        let json = r#"{
            "encrypted": false,
            "version": "1.0",
            "payload": {
                "path": "payload.sqlite",
                "format": "sqlite",
                "extra_payload_field": true
            }
        }"#;

        serde_json::from_str::<ArchiveConfig>(json).expect_err("should reject unknown");
    }

    // ==================== UnencryptedPayload tests ====================

    #[test]
    fn test_unencrypted_payload_minimal() {
        let payload = UnencryptedPayload {
            path: "db.sqlite".to_string(),
            format: "sqlite".to_string(),
            size_bytes: None,
        };

        let json = serde_json::to_string(&payload).expect("serialize");
        // size_bytes should be skipped when None
        assert!(!json.contains("size_bytes"));

        let deserialized: UnencryptedPayload = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(deserialized.path, "db.sqlite");
        assert!(deserialized.size_bytes.is_none());
    }

    #[test]
    fn test_unencrypted_payload_with_size() {
        let payload = UnencryptedPayload {
            path: "large.sqlite".to_string(),
            format: "sqlite".to_string(),
            size_bytes: Some(1_000_000),
        };

        let json = serde_json::to_string(&payload).expect("serialize");
        assert!(json.contains("size_bytes"));
        assert!(json.contains("1000000"));

        let deserialized: UnencryptedPayload = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(deserialized.size_bytes, Some(1_000_000));
    }

    // ==================== Edge case tests ====================

    #[test]
    fn test_unencrypted_config_warning_skipped_when_none() {
        let config = make_unencrypted_config();
        let json = serde_json::to_string(&config).expect("serialize");
        assert!(!json.contains("warning"));
    }

    #[test]
    fn test_unencrypted_config_warning_included_when_some() {
        let mut config = make_unencrypted_config();
        config.warning = Some("Be careful!".to_string());
        let json = serde_json::to_string(&config).expect("serialize");
        assert!(json.contains("warning"));
        assert!(json.contains("Be careful!"));
    }

    #[test]
    fn test_clone_preserves_all_fields() {
        let original = UnencryptedConfig {
            encrypted: false,
            version: "3.0".to_string(),
            payload: UnencryptedPayload {
                path: "test.sqlite".to_string(),
                format: "sqlite".to_string(),
                size_bytes: Some(999),
            },
            warning: Some("Cloned warning".to_string()),
        };

        let cloned = original.clone();
        assert_eq!(original.encrypted, cloned.encrypted);
        assert_eq!(original.version, cloned.version);
        assert_eq!(original.payload.path, cloned.payload.path);
        assert_eq!(original.payload.size_bytes, cloned.payload.size_bytes);
        assert_eq!(original.warning, cloned.warning);
    }

    #[test]
    fn test_archive_config_clone() {
        let original = ArchiveConfig::Unencrypted(make_unencrypted_config());
        let cloned = original.clone();
        assert!(!cloned.is_encrypted());
    }

    #[test]
    fn test_debug_impl_exists() {
        let config = make_unencrypted_config();
        let debug_str = format!("{:?}", config);
        assert!(debug_str.contains("UnencryptedConfig"));
        assert!(debug_str.contains("version"));
    }

    #[test]
    fn test_archive_config_debug_impl() {
        let encrypted = ArchiveConfig::Encrypted(make_encryption_config());
        let debug_str = format!("{:?}", encrypted);
        assert!(debug_str.contains("Encrypted"));

        let unencrypted = ArchiveConfig::Unencrypted(make_unencrypted_config());
        let debug_str = format!("{:?}", unencrypted);
        assert!(debug_str.contains("Unencrypted"));
    }
}
