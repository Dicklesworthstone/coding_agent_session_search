//! Bundle builder for pages export.
//!
//! Creates the deployable static site bundle (site/) and private offline artifacts (private/)
//! from an encrypted export. Output is safe for public hosting (GitHub Pages / Cloudflare Pages).

use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read};
use std::path::{Path, PathBuf};

use super::encrypt::EncryptionConfig;

/// Files embedded from pages_assets at compile time
const PAGES_ASSETS: &[(&str, &[u8])] = &[
    ("index.html", include_bytes!("../pages_assets/index.html")),
    ("styles.css", include_bytes!("../pages_assets/styles.css")),
    ("auth.js", include_bytes!("../pages_assets/auth.js")),
    ("viewer.js", include_bytes!("../pages_assets/viewer.js")),
    ("search.js", include_bytes!("../pages_assets/search.js")),
    (
        "conversation.js",
        include_bytes!("../pages_assets/conversation.js"),
    ),
    ("database.js", include_bytes!("../pages_assets/database.js")),
    ("session.js", include_bytes!("../pages_assets/session.js")),
    ("sw.js", include_bytes!("../pages_assets/sw.js")),
    (
        "sw-register.js",
        include_bytes!("../pages_assets/sw-register.js"),
    ),
    (
        "crypto_worker.js",
        include_bytes!("../pages_assets/crypto_worker.js"),
    ),
    (
        "virtual-list.js",
        include_bytes!("../pages_assets/virtual-list.js"),
    ),
    (
        "coi-detector.js",
        include_bytes!("../pages_assets/coi-detector.js"),
    ),
];

/// Integrity entry for a single file
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntegrityEntry {
    /// SHA256 hash as hex string
    pub sha256: String,
    /// File size in bytes
    pub size: u64,
}

/// Full integrity manifest
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntegrityManifest {
    /// Schema version for integrity format
    pub version: u8,
    /// Generated timestamp
    pub generated_at: String,
    /// Map of relative path -> integrity entry
    pub files: BTreeMap<String, IntegrityEntry>,
}

/// Site metadata for public config
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SiteMetadata {
    pub title: String,
    pub description: String,
    pub generated_at: String,
    pub generator: String,
    pub generator_version: String,
}

/// Bundle configuration
#[derive(Debug, Clone)]
pub struct BundleConfig {
    /// Archive title
    pub title: String,
    /// Archive description
    pub description: String,
    /// Whether to obfuscate metadata (workspace paths etc)
    pub hide_metadata: bool,
    /// Recovery secret bytes (if generated)
    pub recovery_secret: Option<Vec<u8>>,
    /// Whether to generate QR codes for recovery
    pub generate_qr: bool,
}

impl Default for BundleConfig {
    fn default() -> Self {
        Self {
            title: "cass Archive".to_string(),
            description: "Encrypted archive of AI coding agent conversations".to_string(),
            hide_metadata: false,
            recovery_secret: None,
            generate_qr: false,
        }
    }
}

/// Bundle builder for creating static site exports
pub struct BundleBuilder {
    config: BundleConfig,
}

impl Default for BundleBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl BundleBuilder {
    /// Create a new bundle builder with default config
    pub fn new() -> Self {
        Self {
            config: BundleConfig::default(),
        }
    }

    /// Create a bundle builder with specific config
    pub fn with_config(config: BundleConfig) -> Self {
        Self { config }
    }

    /// Set the archive title
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.config.title = title.into();
        self
    }

    /// Set the archive description
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.config.description = description.into();
        self
    }

    /// Set metadata hiding option
    pub fn hide_metadata(mut self, hide: bool) -> Self {
        self.config.hide_metadata = hide;
        self
    }

    /// Set the recovery secret
    pub fn recovery_secret(mut self, secret: Option<Vec<u8>>) -> Self {
        self.config.recovery_secret = secret;
        self
    }

    /// Set QR code generation option
    pub fn generate_qr(mut self, generate: bool) -> Self {
        self.config.generate_qr = generate;
        self
    }

    /// Build the bundle from encrypted output
    ///
    /// # Arguments
    /// * `encrypted_dir` - Directory containing encryption output (config.json, payload/)
    /// * `output_dir` - Directory to write the bundle (will create site/ and private/ subdirs)
    /// * `progress` - Progress callback (phase, message)
    pub fn build<P: AsRef<Path>>(
        &self,
        encrypted_dir: P,
        output_dir: P,
        progress: impl Fn(&str, &str),
    ) -> Result<BundleResult> {
        let encrypted_dir = encrypted_dir.as_ref();
        let output_dir = output_dir.as_ref();

        // Validate encrypted_dir has required files
        let config_path = encrypted_dir.join("config.json");
        let payload_dir = encrypted_dir.join("payload");

        if !config_path.exists() {
            bail!("Missing config.json in encrypted directory");
        }
        if !payload_dir.exists() {
            bail!("Missing payload/ directory in encrypted directory");
        }

        // Load encryption config
        let enc_config: EncryptionConfig = {
            let file = File::open(&config_path).context("Failed to open config.json")?;
            serde_json::from_reader(BufReader::new(file))?
        };

        progress("setup", "Creating directory structure...");

        // Create output structure
        let site_dir = output_dir.join("site");
        let private_dir = output_dir.join("private");

        fs::create_dir_all(&site_dir).context("Failed to create site/ directory")?;
        fs::create_dir_all(&private_dir).context("Failed to create private/ directory")?;

        // Create site subdirectories
        let site_payload_dir = site_dir.join("payload");
        fs::create_dir_all(&site_payload_dir).context("Failed to create site/payload/")?;

        progress("assets", "Copying web assets...");

        // Copy embedded assets to site/
        for (name, content) in PAGES_ASSETS {
            let dest_path = site_dir.join(name);
            fs::write(&dest_path, content).with_context(|| format!("Failed to write {}", name))?;
        }

        progress("payload", "Copying encrypted payload...");

        // Copy payload chunks to site/payload/
        let chunk_count = copy_payload_chunks(&payload_dir, &site_payload_dir)?;

        progress("config", "Writing configuration files...");

        // Write config.json to site/ (already has public params only)
        let site_config_path = site_dir.join("config.json");
        let config_file = File::create(&site_config_path)?;
        serde_json::to_writer_pretty(BufWriter::new(config_file), &enc_config)?;

        // Write site metadata
        let site_metadata = SiteMetadata {
            title: self.config.title.clone(),
            description: self.config.description.clone(),
            generated_at: Utc::now().to_rfc3339(),
            generator: "cass".to_string(),
            generator_version: env!("CARGO_PKG_VERSION").to_string(),
        };
        let site_json_path = site_dir.join("site.json");
        let site_json_file = File::create(&site_json_path)?;
        serde_json::to_writer_pretty(BufWriter::new(site_json_file), &site_metadata)?;

        progress("static", "Writing static files...");

        // Write robots.txt
        let robots_content = "User-agent: *\nDisallow: /\n";
        fs::write(site_dir.join("robots.txt"), robots_content)?;

        // Write .nojekyll (empty file to disable Jekyll processing)
        fs::write(site_dir.join(".nojekyll"), "")?;

        // Write public README.md
        let public_readme = generate_public_readme(&self.config.title, &self.config.description);
        fs::write(site_dir.join("README.md"), public_readme)?;

        progress("integrity", "Generating integrity manifest...");

        // Generate integrity.json for all files in site/
        let integrity_manifest = generate_integrity_manifest(&site_dir)?;
        let integrity_path = site_dir.join("integrity.json");
        let integrity_file = File::create(&integrity_path)?;
        serde_json::to_writer_pretty(BufWriter::new(integrity_file), &integrity_manifest)?;

        // Compute integrity fingerprint (short hash for visual verification)
        let fingerprint = compute_fingerprint(&integrity_manifest);

        progress("private", "Writing private artifacts...");

        // Write private artifacts
        write_private_artifacts(&private_dir, &self.config, &fingerprint, &enc_config)?;

        progress("complete", "Bundle complete!");

        Ok(BundleResult {
            site_dir,
            private_dir,
            chunk_count,
            fingerprint,
            total_files: integrity_manifest.files.len(),
        })
    }
}

/// Result from bundle building
#[derive(Debug, Clone)]
pub struct BundleResult {
    /// Path to site/ directory (deploy this)
    pub site_dir: PathBuf,
    /// Path to private/ directory (never deploy)
    pub private_dir: PathBuf,
    /// Number of encrypted payload chunks
    pub chunk_count: usize,
    /// Integrity fingerprint (for visual verification)
    pub fingerprint: String,
    /// Total number of files in site/
    pub total_files: usize,
}

/// Copy payload chunks from source to destination
fn copy_payload_chunks(src_dir: &Path, dest_dir: &Path) -> Result<usize> {
    let mut count = 0;

    for entry in fs::read_dir(src_dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.extension().map(|e| e == "bin").unwrap_or(false) {
            let filename = path.file_name().unwrap();
            let dest_path = dest_dir.join(filename);
            fs::copy(&path, &dest_path)?;
            count += 1;
        }
    }

    Ok(count)
}

/// Generate integrity manifest for all files in a directory
fn generate_integrity_manifest(dir: &Path) -> Result<IntegrityManifest> {
    let mut files = BTreeMap::new();

    collect_file_hashes(dir, dir, &mut files)?;

    Ok(IntegrityManifest {
        version: 1,
        generated_at: Utc::now().to_rfc3339(),
        files,
    })
}

/// Recursively collect SHA256 hashes of all files
fn collect_file_hashes(
    base_dir: &Path,
    current_dir: &Path,
    files: &mut BTreeMap<String, IntegrityEntry>,
) -> Result<()> {
    for entry in fs::read_dir(current_dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_dir() {
            collect_file_hashes(base_dir, &path, files)?;
        } else {
            // Compute relative path
            let rel_path = path.strip_prefix(base_dir)?;
            let rel_str = rel_path.to_string_lossy().replace('\\', "/");

            // Skip integrity.json itself (chicken/egg)
            if rel_str == "integrity.json" {
                continue;
            }

            // Compute hash and size
            let file = File::open(&path)?;
            let metadata = file.metadata()?;
            let size = metadata.len();

            let mut hasher = Sha256::new();
            let mut reader = BufReader::new(file);
            let mut buffer = [0u8; 8192];

            loop {
                let bytes_read = reader.read(&mut buffer)?;
                if bytes_read == 0 {
                    break;
                }
                hasher.update(&buffer[..bytes_read]);
            }

            let hash = format!("{:x}", hasher.finalize());

            files.insert(rel_str, IntegrityEntry { sha256: hash, size });
        }
    }

    Ok(())
}

/// Compute a short fingerprint from the integrity manifest
fn compute_fingerprint(manifest: &IntegrityManifest) -> String {
    // Compute a fingerprint by hashing the sorted list of file hashes
    let mut hasher = Sha256::new();

    for (path, entry) in &manifest.files {
        hasher.update(path.as_bytes());
        hasher.update(entry.sha256.as_bytes());
    }

    let hash = hasher.finalize();

    // Return first 16 hex chars as fingerprint
    format!("{:x}", hash)[..16].to_string()
}

/// Write private artifacts that should never be deployed
fn write_private_artifacts(
    private_dir: &Path,
    config: &BundleConfig,
    fingerprint: &str,
    enc_config: &EncryptionConfig,
) -> Result<()> {
    // Write integrity fingerprint
    let fingerprint_content = format!(
        "Integrity Fingerprint: {}\n\n\
        Generated: {}\n\n\
        Verify this fingerprint matches the one displayed in the web viewer\n\
        before entering your password. If it doesn't match, the archive may\n\
        have been tampered with.\n",
        fingerprint,
        Utc::now().to_rfc3339()
    );
    fs::write(
        private_dir.join("integrity-fingerprint.txt"),
        fingerprint_content,
    )?;

    // Write recovery secret if provided
    if let Some(secret) = &config.recovery_secret {
        let recovery_b64 = BASE64.encode(secret);
        let recovery_content = format!(
            "Recovery Secret\n\
            ================\n\n\
            This secret can unlock your archive if you forget your password.\n\
            Store it securely and NEVER share it.\n\n\
            Secret (base64):\n\
            {}\n\n\
            To use: Click \"Scan Recovery QR Code\" in the web viewer, or\n\
            use this base64 value with the recovery function.\n\n\
            Archive Export ID: {}\n\
            Generated: {}\n",
            recovery_b64,
            enc_config.export_id,
            Utc::now().to_rfc3339()
        );
        fs::write(private_dir.join("recovery-secret.txt"), recovery_content)?;

        // Generate QR code if requested
        if config.generate_qr {
            generate_qr_codes(private_dir, &recovery_b64)?;
        }
    }

    // Write master key backup (encrypted DEK wrapped with KEK)
    let master_key_backup = serde_json::json!({
        "export_id": enc_config.export_id,
        "key_slots": enc_config.key_slots,
        "note": "This file contains the wrapped DEK. Keep it with your recovery secret.",
        "generated_at": Utc::now().to_rfc3339(),
    });
    let master_key_path = private_dir.join("master-key.json");
    let master_key_file = File::create(&master_key_path)?;
    serde_json::to_writer_pretty(BufWriter::new(master_key_file), &master_key_backup)?;

    Ok(())
}

/// Generate QR code images for recovery secret
fn generate_qr_codes(private_dir: &Path, recovery_b64: &str) -> Result<()> {
    // Use the qr module from pages if available
    if let Ok(qr_png) = super::qr::generate_qr_png(recovery_b64) {
        fs::write(private_dir.join("qr-code.png"), qr_png)?;
    }

    if let Ok(qr_svg) = super::qr::generate_qr_svg(recovery_b64) {
        fs::write(private_dir.join("qr-code.svg"), qr_svg)?;
    }

    Ok(())
}

/// Generate public README for the site directory
fn generate_public_readme(title: &str, description: &str) -> String {
    format!(
        r#"# {}

{}

## About This Archive

This is an encrypted, searchable archive of AI coding agent conversations
generated by [cass](https://github.com/Dicklesworthstone/coding_agent_session_search).

## Security

- All data is encrypted with AES-256-GCM
- Password-based key derivation uses Argon2id
- The archive can be safely hosted on public servers
- No data is accessible without the correct password

## How to Open

1. Host these files on any static web server
2. Open index.html in a modern browser
3. Verify the fingerprint matches your records
4. Enter your password to decrypt

## Technical Details

- Encryption: AES-256-GCM with chunked streaming
- KDF: Argon2id (64MB memory, 3 iterations)
- Search: SQLite with FTS5 (runs in browser via sql.js)
- Requires: SharedArrayBuffer (COOP/COEP headers)

## Files

- `index.html` - Entry point
- `config.json` - Public encryption parameters (no secrets)
- `integrity.json` - SHA256 hashes for all files
- `payload/` - Encrypted database chunks
- `*.js` - Application code
- `styles.css` - Styling

## Hosting Requirements

For the viewer to function correctly, your web server must set:

```
Cross-Origin-Opener-Policy: same-origin
Cross-Origin-Embedder-Policy: require-corp
```

The included service worker (sw.js) handles this automatically for
most static hosts (GitHub Pages, Cloudflare Pages, etc.).

---

Generated by cass v{}
"#,
        title,
        description,
        env!("CARGO_PKG_VERSION")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_bundle_builder_default() {
        let builder = BundleBuilder::new();
        assert_eq!(builder.config.title, "cass Archive");
        assert!(!builder.config.hide_metadata);
        assert!(!builder.config.generate_qr);
    }

    #[test]
    fn test_bundle_builder_fluent() {
        let builder = BundleBuilder::new()
            .title("My Archive")
            .description("Test description")
            .hide_metadata(true)
            .generate_qr(true);

        assert_eq!(builder.config.title, "My Archive");
        assert_eq!(builder.config.description, "Test description");
        assert!(builder.config.hide_metadata);
        assert!(builder.config.generate_qr);
    }

    #[test]
    fn test_compute_fingerprint() {
        let mut files = BTreeMap::new();
        files.insert(
            "test.txt".to_string(),
            IntegrityEntry {
                sha256: "abc123".to_string(),
                size: 100,
            },
        );

        let manifest = IntegrityManifest {
            version: 1,
            generated_at: "2024-01-01T00:00:00Z".to_string(),
            files,
        };

        let fingerprint = compute_fingerprint(&manifest);
        assert_eq!(fingerprint.len(), 16);

        // Same manifest should produce same fingerprint
        let fingerprint2 = compute_fingerprint(&manifest);
        assert_eq!(fingerprint, fingerprint2);
    }

    #[test]
    fn test_generate_public_readme() {
        let readme = generate_public_readme("Test Archive", "A test archive");
        assert!(readme.contains("Test Archive"));
        assert!(readme.contains("A test archive"));
        assert!(readme.contains("AES-256-GCM"));
        assert!(readme.contains("Argon2id"));
    }

    #[test]
    fn test_integrity_manifest_excludes_itself() {
        let temp = TempDir::new().unwrap();
        let temp_path = temp.path();

        // Create some test files
        fs::write(temp_path.join("test.txt"), "hello").unwrap();
        fs::write(temp_path.join("integrity.json"), "{}").unwrap();

        let manifest = generate_integrity_manifest(temp_path).unwrap();

        // Should include test.txt but not integrity.json
        assert!(manifest.files.contains_key("test.txt"));
        assert!(!manifest.files.contains_key("integrity.json"));
    }

    #[test]
    fn test_collect_file_hashes() {
        let temp = TempDir::new().unwrap();
        let temp_path = temp.path();

        // Create nested structure
        fs::create_dir_all(temp_path.join("subdir")).unwrap();
        fs::write(temp_path.join("root.txt"), "root").unwrap();
        fs::write(temp_path.join("subdir/nested.txt"), "nested").unwrap();

        let mut files = BTreeMap::new();
        collect_file_hashes(temp_path, temp_path, &mut files).unwrap();

        assert_eq!(files.len(), 2);
        assert!(files.contains_key("root.txt"));
        assert!(files.contains_key("subdir/nested.txt"));

        // Verify hash is SHA256 hex (64 chars)
        for entry in files.values() {
            assert_eq!(entry.sha256.len(), 64);
        }
    }
}
