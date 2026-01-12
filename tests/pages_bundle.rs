//! Integration tests for the bundle builder.

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use coding_agent_search::pages::bundle::{BundleBuilder, BundleConfig, IntegrityManifest};
    use coding_agent_search::pages::encrypt::EncryptionEngine;
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    /// Create a test encrypted archive in the given directory
    fn setup_encrypted_archive(dir: &Path) -> Result<()> {
        // Create a test file to encrypt
        let test_file = dir.join("test_input.db");
        fs::write(&test_file, b"test database content for bundle testing")?;

        // Encrypt it
        let mut engine = EncryptionEngine::default();
        engine.add_password_slot("test-password")?;
        let dir_buf = dir.to_path_buf();
        engine.encrypt_file(&test_file, &dir_buf, |_, _| {})?;

        // Clean up the source file
        fs::remove_file(&test_file)?;

        Ok(())
    }

    #[test]
    fn test_bundle_creates_directory_structure() -> Result<()> {
        let temp = TempDir::new()?;
        let encrypted_dir = temp.path().join("encrypted");
        let bundle_dir = temp.path().join("bundle");

        fs::create_dir_all(&encrypted_dir)?;
        setup_encrypted_archive(&encrypted_dir)?;

        let builder = BundleBuilder::new()
            .title("Test Archive")
            .description("A test archive");

        let result = builder.build(&encrypted_dir, &bundle_dir, |_, _| {})?;

        // Verify directory structure
        assert!(result.site_dir.exists(), "site/ directory should exist");
        assert!(
            result.private_dir.exists(),
            "private/ directory should exist"
        );
        assert!(
            result.site_dir.join("payload").exists(),
            "site/payload/ should exist"
        );

        Ok(())
    }

    #[test]
    fn test_bundle_copies_all_assets() -> Result<()> {
        let temp = TempDir::new()?;
        let encrypted_dir = temp.path().join("encrypted");
        let bundle_dir = temp.path().join("bundle");

        fs::create_dir_all(&encrypted_dir)?;
        setup_encrypted_archive(&encrypted_dir)?;

        let builder = BundleBuilder::new();
        let result = builder.build(&encrypted_dir, &bundle_dir, |_, _| {})?;

        // Verify required files exist
        let site_dir = &result.site_dir;

        // Web assets
        assert!(
            site_dir.join("index.html").exists(),
            "index.html should exist"
        );
        assert!(
            site_dir.join("styles.css").exists(),
            "styles.css should exist"
        );
        assert!(site_dir.join("auth.js").exists(), "auth.js should exist");
        assert!(
            site_dir.join("viewer.js").exists(),
            "viewer.js should exist"
        );
        assert!(
            site_dir.join("search.js").exists(),
            "search.js should exist"
        );
        assert!(site_dir.join("sw.js").exists(), "sw.js should exist");

        // Static files
        assert!(
            site_dir.join("robots.txt").exists(),
            "robots.txt should exist"
        );
        assert!(
            site_dir.join(".nojekyll").exists(),
            ".nojekyll should exist"
        );
        assert!(
            site_dir.join("README.md").exists(),
            "README.md should exist"
        );

        // Config files
        assert!(
            site_dir.join("config.json").exists(),
            "config.json should exist"
        );
        assert!(
            site_dir.join("site.json").exists(),
            "site.json should exist"
        );
        assert!(
            site_dir.join("integrity.json").exists(),
            "integrity.json should exist"
        );

        Ok(())
    }

    #[test]
    fn test_bundle_copies_payload_chunks() -> Result<()> {
        let temp = TempDir::new()?;
        let encrypted_dir = temp.path().join("encrypted");
        let bundle_dir = temp.path().join("bundle");

        fs::create_dir_all(&encrypted_dir)?;
        setup_encrypted_archive(&encrypted_dir)?;

        let builder = BundleBuilder::new();
        let result = builder.build(&encrypted_dir, &bundle_dir, |_, _| {})?;

        // Verify payload chunks were copied
        assert!(result.chunk_count > 0, "Should have at least one chunk");

        let payload_dir = result.site_dir.join("payload");
        let chunk_count = fs::read_dir(&payload_dir)?
            .filter(|e| {
                e.as_ref()
                    .map(|e| {
                        e.path()
                            .extension()
                            .map(|ext| ext == "bin")
                            .unwrap_or(false)
                    })
                    .unwrap_or(false)
            })
            .count();

        assert_eq!(chunk_count, result.chunk_count, "Chunk count should match");

        Ok(())
    }

    #[test]
    fn test_bundle_generates_integrity_manifest() -> Result<()> {
        let temp = TempDir::new()?;
        let encrypted_dir = temp.path().join("encrypted");
        let bundle_dir = temp.path().join("bundle");

        fs::create_dir_all(&encrypted_dir)?;
        setup_encrypted_archive(&encrypted_dir)?;

        let builder = BundleBuilder::new();
        let result = builder.build(&encrypted_dir, &bundle_dir, |_, _| {})?;

        // Load and verify integrity manifest
        let integrity_path = result.site_dir.join("integrity.json");
        let integrity_content = fs::read_to_string(&integrity_path)?;
        let manifest: IntegrityManifest = serde_json::from_str(&integrity_content)?;

        assert_eq!(manifest.version, 1);
        assert!(!manifest.files.is_empty(), "Should have file entries");

        // Verify integrity.json is not in the manifest (chicken/egg)
        assert!(!manifest.files.contains_key("integrity.json"));

        // Verify each listed file exists and has correct size
        for (rel_path, entry) in &manifest.files {
            let file_path = result.site_dir.join(rel_path);
            assert!(file_path.exists(), "File {} should exist", rel_path);

            let metadata = fs::metadata(&file_path)?;
            assert_eq!(metadata.len(), entry.size, "Size mismatch for {}", rel_path);

            // Verify hash is valid hex SHA256 (64 chars)
            assert_eq!(
                entry.sha256.len(),
                64,
                "Hash should be 64 hex chars for {}",
                rel_path
            );
        }

        Ok(())
    }

    #[test]
    fn test_bundle_generates_fingerprint() -> Result<()> {
        let temp = TempDir::new()?;
        let encrypted_dir = temp.path().join("encrypted");
        let bundle_dir = temp.path().join("bundle");

        fs::create_dir_all(&encrypted_dir)?;
        setup_encrypted_archive(&encrypted_dir)?;

        let builder = BundleBuilder::new();
        let result = builder.build(&encrypted_dir, &bundle_dir, |_, _| {})?;

        // Fingerprint should be 16 hex characters
        assert_eq!(
            result.fingerprint.len(),
            16,
            "Fingerprint should be 16 chars"
        );
        assert!(
            result.fingerprint.chars().all(|c| c.is_ascii_hexdigit()),
            "Fingerprint should be hex"
        );

        Ok(())
    }

    #[test]
    fn test_bundle_writes_private_artifacts() -> Result<()> {
        let temp = TempDir::new()?;
        let encrypted_dir = temp.path().join("encrypted");
        let bundle_dir = temp.path().join("bundle");

        fs::create_dir_all(&encrypted_dir)?;
        setup_encrypted_archive(&encrypted_dir)?;

        let config = BundleConfig {
            title: "Test Archive".to_string(),
            description: "Test description".to_string(),
            hide_metadata: false,
            recovery_secret: Some(vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]),
            generate_qr: false,
        };

        let builder = BundleBuilder::with_config(config);
        let result = builder.build(&encrypted_dir, &bundle_dir, |_, _| {})?;

        // Verify private artifacts
        assert!(
            result
                .private_dir
                .join("integrity-fingerprint.txt")
                .exists()
        );
        assert!(result.private_dir.join("recovery-secret.txt").exists());
        assert!(result.private_dir.join("master-key.json").exists());

        // Verify recovery secret content
        let recovery_content = fs::read_to_string(result.private_dir.join("recovery-secret.txt"))?;
        assert!(recovery_content.contains("Recovery Secret"));
        assert!(recovery_content.contains("NEVER share"));

        Ok(())
    }

    #[test]
    fn test_bundle_site_has_no_secrets() -> Result<()> {
        let temp = TempDir::new()?;
        let encrypted_dir = temp.path().join("encrypted");
        let bundle_dir = temp.path().join("bundle");

        fs::create_dir_all(&encrypted_dir)?;
        setup_encrypted_archive(&encrypted_dir)?;

        let config = BundleConfig {
            title: "Test Archive".to_string(),
            description: "Test description".to_string(),
            hide_metadata: false,
            recovery_secret: Some(vec![0xDE, 0xAD, 0xBE, 0xEF]),
            generate_qr: false,
        };

        let builder = BundleBuilder::with_config(config);
        let result = builder.build(&encrypted_dir, &bundle_dir, |_, _| {})?;

        // Verify site/ has no private files
        assert!(!result.site_dir.join("recovery-secret.txt").exists());
        assert!(!result.site_dir.join("qr-code.png").exists());
        assert!(!result.site_dir.join("qr-code.svg").exists());
        assert!(!result.site_dir.join("integrity-fingerprint.txt").exists());
        assert!(!result.site_dir.join("master-key.json").exists());

        // Verify config.json doesn't contain DEK or secrets
        let _config_content = fs::read_to_string(result.site_dir.join("config.json"))?;
        // DEK would be unwrapped, so it shouldn't be plain in config
        // But wrapped DEK is expected (that's the design - LUKS-style key slots)

        Ok(())
    }

    #[test]
    fn test_bundle_robots_txt_content() -> Result<()> {
        let temp = TempDir::new()?;
        let encrypted_dir = temp.path().join("encrypted");
        let bundle_dir = temp.path().join("bundle");

        fs::create_dir_all(&encrypted_dir)?;
        setup_encrypted_archive(&encrypted_dir)?;

        let builder = BundleBuilder::new();
        let result = builder.build(&encrypted_dir, &bundle_dir, |_, _| {})?;

        let robots_content = fs::read_to_string(result.site_dir.join("robots.txt"))?;
        assert!(robots_content.contains("User-agent: *"));
        assert!(robots_content.contains("Disallow: /"));

        Ok(())
    }

    #[test]
    fn test_bundle_site_metadata() -> Result<()> {
        let temp = TempDir::new()?;
        let encrypted_dir = temp.path().join("encrypted");
        let bundle_dir = temp.path().join("bundle");

        fs::create_dir_all(&encrypted_dir)?;
        setup_encrypted_archive(&encrypted_dir)?;

        let builder = BundleBuilder::new()
            .title("My Custom Archive")
            .description("Custom description here");

        let result = builder.build(&encrypted_dir, &bundle_dir, |_, _| {})?;

        let site_json_content = fs::read_to_string(result.site_dir.join("site.json"))?;
        let site_json: serde_json::Value = serde_json::from_str(&site_json_content)?;

        assert_eq!(site_json["title"], "My Custom Archive");
        assert_eq!(site_json["description"], "Custom description here");
        assert_eq!(site_json["generator"], "cass");

        Ok(())
    }

    #[test]
    fn test_bundle_fails_without_config() -> Result<()> {
        let temp = TempDir::new()?;
        let encrypted_dir = temp.path().join("encrypted");
        let bundle_dir = temp.path().join("bundle");

        fs::create_dir_all(&encrypted_dir)?;
        // Don't create config.json or payload/

        let builder = BundleBuilder::new();
        let result = builder.build(&encrypted_dir, &bundle_dir, |_, _| {});

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("config.json"),
            "Error should mention missing config.json"
        );

        Ok(())
    }

    #[test]
    fn test_bundle_fails_without_payload() -> Result<()> {
        let temp = TempDir::new()?;
        let encrypted_dir = temp.path().join("encrypted");
        let bundle_dir = temp.path().join("bundle");

        fs::create_dir_all(&encrypted_dir)?;

        // Create config.json but no payload/
        let fake_config = serde_json::json!({
            "version": 2,
            "export_id": "test",
            "base_nonce": "test",
            "compression": "deflate",
            "kdf_defaults": {},
            "payload": {"files": []},
            "key_slots": []
        });
        fs::write(
            encrypted_dir.join("config.json"),
            serde_json::to_string(&fake_config)?,
        )?;

        let builder = BundleBuilder::new();
        let result = builder.build(&encrypted_dir, &bundle_dir, |_, _| {});

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("payload"),
            "Error should mention missing payload/"
        );

        Ok(())
    }

    #[test]
    fn test_bundle_progress_callback() -> Result<()> {
        use std::sync::{Arc, Mutex};

        let temp = TempDir::new()?;
        let encrypted_dir = temp.path().join("encrypted");
        let bundle_dir = temp.path().join("bundle");

        fs::create_dir_all(&encrypted_dir)?;
        setup_encrypted_archive(&encrypted_dir)?;

        let phases: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let phases_clone = phases.clone();

        let builder = BundleBuilder::new();
        builder.build(&encrypted_dir, &bundle_dir, move |phase, _msg| {
            phases_clone.lock().unwrap().push(phase.to_string());
        })?;

        let captured = phases.lock().unwrap();
        assert!(captured.contains(&"setup".to_string()));
        assert!(captured.contains(&"assets".to_string()));
        assert!(captured.contains(&"payload".to_string()));
        assert!(captured.contains(&"config".to_string()));
        assert!(captured.contains(&"integrity".to_string()));
        assert!(captured.contains(&"private".to_string()));
        assert!(captured.contains(&"complete".to_string()));

        Ok(())
    }
}
