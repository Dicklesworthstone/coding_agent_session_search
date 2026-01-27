//! Verify command for CI pipelines.
//!
//! Provides `cass pages --verify <PATH>` to validate an existing export bundle for CI/CD.
//! The verifier confirms correct structure, config schema, payload integrity, and
//! the absence of secrets in site/.

use anyhow::{Context, Result, bail};
use base64::prelude::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use super::archive_config::{ArchiveConfig, UnencryptedConfig};
use super::bundle::IntegrityManifest;
use super::encrypt::EncryptionConfig;

/// Maximum chunk file size (GitHub Pages hard limit)
const MAX_CHUNK_SIZE: u64 = 100 * 1024 * 1024; // 100 MB

/// Maximum chunk_size config value (32 MiB)
const MAX_CONFIG_CHUNK_SIZE: usize = 32 * 1024 * 1024;

/// Required files that must exist in site/
const REQUIRED_FILES: &[&str] = &[
    "index.html",
    "config.json",
    "sw.js",
    "viewer.js",
    "auth.js",
    "styles.css",
    "robots.txt",
    ".nojekyll",
];

/// Files that indicate secret leakage
const SECRET_FILES: &[&str] = &[
    "recovery-secret.txt",
    "qr-code.png",
    "qr-code.svg",
    "master-key.json",
];

/// Directories that should not exist in site/
const SECRET_DIRS: &[&str] = &["private"];

/// Verification result for a single check
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckResult {
    /// Whether the check passed
    pub passed: bool,
    /// Details about the check (empty if passed, error message if failed)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
}

impl CheckResult {
    fn pass() -> Self {
        Self {
            passed: true,
            details: None,
        }
    }

    fn fail(details: impl Into<String>) -> Self {
        Self {
            passed: false,
            details: Some(details.into()),
        }
    }
}

/// Summary of all verification checks
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyChecks {
    pub required_files: CheckResult,
    pub config_schema: CheckResult,
    pub payload_manifest: CheckResult,
    pub size_limits: CheckResult,
    pub integrity: CheckResult,
    pub no_secrets_in_site: CheckResult,
}

impl VerifyChecks {
    /// Returns true if all checks passed
    pub fn all_passed(&self) -> bool {
        self.required_files.passed
            && self.config_schema.passed
            && self.payload_manifest.passed
            && self.size_limits.passed
            && self.integrity.passed
            && self.no_secrets_in_site.passed
    }
}

/// Complete verification result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyResult {
    /// Overall status: "valid" or "invalid"
    pub status: String,
    /// Individual check results
    pub checks: VerifyChecks,
    /// Warning messages (non-fatal issues)
    pub warnings: Vec<String>,
    /// Total site size in bytes
    pub site_size_bytes: u64,
}

/// Verify a bundle export
///
/// # Arguments
/// * `path` - Path to the export root (containing site/) or site/ directory itself
/// * `verbose` - Whether to print detailed progress
///
/// # Returns
/// `VerifyResult` with all check outcomes
pub fn verify_bundle(path: &Path, verbose: bool) -> Result<VerifyResult> {
    // Resolve to site/ directory
    let site_dir = resolve_site_dir(path)?;

    if verbose {
        println!("Verifying bundle at: {}", site_dir.display());
    }

    let mut warnings = Vec::new();

    // Check 1: Required files
    if verbose {
        println!("  Checking required files...");
    }
    let required_files = check_required_files(&site_dir);

    // Check 2: Config schema (only if config.json exists)
    if verbose {
        println!("  Checking config.json schema...");
    }
    let config_schema = if site_dir.join("config.json").exists() {
        check_config_schema(&site_dir)
    } else {
        CheckResult::fail("config.json not found")
    };

    // Check 3: Payload manifest
    if verbose {
        println!("  Checking payload manifest...");
    }
    let payload_manifest = check_payload_manifest(&site_dir);

    // Check 4: Size limits
    if verbose {
        println!("  Checking size limits...");
    }
    let size_limits = check_size_limits(&site_dir);

    // Check 5: Integrity (if integrity.json exists)
    if verbose {
        println!("  Checking integrity...");
    }
    let integrity = if site_dir.join("integrity.json").exists() {
        check_integrity(&site_dir, verbose)
    } else {
        warnings.push("integrity.json not present - skipping integrity check".to_string());
        CheckResult::pass()
    };

    // Check 6: No secrets in site/
    if verbose {
        println!("  Checking for secret leakage...");
    }
    let no_secrets_in_site = check_no_secrets(&site_dir);

    // Calculate total site size
    let site_size_bytes = calculate_dir_size(&site_dir)?;

    let checks = VerifyChecks {
        required_files,
        config_schema,
        payload_manifest,
        size_limits,
        integrity,
        no_secrets_in_site,
    };

    let status = if checks.all_passed() {
        "valid".to_string()
    } else {
        "invalid".to_string()
    };

    Ok(VerifyResult {
        status,
        checks,
        warnings,
        site_size_bytes,
    })
}

/// Resolve the site directory from a path
fn resolve_site_dir(path: &Path) -> Result<PathBuf> {
    if !path.exists() {
        bail!("Path does not exist: {}", path.display());
    }

    // If path ends with "site" or contains site/, use it
    if path.ends_with("site") || path.file_name().map(|n| n == "site").unwrap_or(false) {
        return Ok(path.to_path_buf());
    }

    // If path contains site/ subdirectory, use that
    let site_subdir = path.join("site");
    if site_subdir.exists() && site_subdir.is_dir() {
        return Ok(site_subdir);
    }

    // Otherwise treat path as site directory
    Ok(path.to_path_buf())
}

/// Check that all required files exist
fn check_required_files(site_dir: &Path) -> CheckResult {
    let mut missing = Vec::new();

    for file in REQUIRED_FILES {
        if !site_dir.join(file).exists() {
            missing.push(*file);
        }
    }

    // Also check payload/ directory exists
    if !site_dir.join("payload").is_dir() {
        missing.push("payload/");
    }

    if missing.is_empty() {
        CheckResult::pass()
    } else {
        CheckResult::fail(format!("Missing files: {}", missing.join(", ")))
    }
}

/// Check config.json schema validity
fn check_config_schema(site_dir: &Path) -> CheckResult {
    let config_path = site_dir.join("config.json");

    // Parse config
    let config: ArchiveConfig = match File::open(&config_path)
        .context("Failed to open config.json")
        .and_then(|f| serde_json::from_reader(BufReader::new(f)).context("Failed to parse JSON"))
    {
        Ok(c) => c,
        Err(e) => return CheckResult::fail(format!("Failed to parse config.json: {}", e)),
    };

    let errors = match &config {
        ArchiveConfig::Encrypted(enc) => validate_encrypted_config(enc),
        ArchiveConfig::Unencrypted(unenc) => validate_unencrypted_config(unenc),
    };

    if errors.is_empty() {
        CheckResult::pass()
    } else {
        CheckResult::fail(errors.join("; "))
    }
}

fn validate_encrypted_config(config: &EncryptionConfig) -> Vec<String> {
    let mut errors = Vec::new();

    // Validate export_id (base64, 16 bytes)
    match BASE64_STANDARD.decode(&config.export_id) {
        Ok(bytes) if bytes.len() == 16 => {}
        Ok(bytes) => errors.push(format!("export_id should be 16 bytes, got {}", bytes.len())),
        Err(e) => errors.push(format!("export_id is not valid base64: {}", e)),
    }

    // Validate base_nonce (base64, 12 bytes)
    match BASE64_STANDARD.decode(&config.base_nonce) {
        Ok(bytes) if bytes.len() == 12 => {}
        Ok(bytes) => errors.push(format!(
            "base_nonce should be 12 bytes, got {}",
            bytes.len()
        )),
        Err(e) => errors.push(format!("base_nonce is not valid base64: {}", e)),
    }

    // Validate compression
    let valid_compressions = ["deflate", "zstd", "none"];
    if !valid_compressions.contains(&config.compression.as_str()) {
        errors.push(format!(
            "compression should be one of {:?}, got '{}'",
            valid_compressions, config.compression
        ));
    }

    // Validate chunk_size
    if config.payload.chunk_size == 0 {
        errors.push("chunk_size cannot be zero".to_string());
    }
    if config.payload.chunk_size > MAX_CONFIG_CHUNK_SIZE {
        errors.push(format!(
            "chunk_size {} exceeds maximum {}",
            config.payload.chunk_size, MAX_CONFIG_CHUNK_SIZE
        ));
    }

    // Validate chunk_count
    if config.payload.chunk_count == 0 {
        errors.push("chunk_count cannot be zero".to_string());
    }

    // Validate files list matches chunk_count
    if config.payload.files.len() != config.payload.chunk_count {
        errors.push(format!(
            "files list length ({}) doesn't match chunk_count ({})",
            config.payload.files.len(),
            config.payload.chunk_count
        ));
    }

    // Validate payload file paths (relative, under payload/, no parent traversal)
    for (i, file) in config.payload.files.iter().enumerate() {
        let path = Path::new(file);
        if path.is_absolute() {
            errors.push(format!("payload.files[{}] must be relative", i));
        }
        if path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            errors.push(format!("payload.files[{}] must not contain '..'", i));
        }
        if !path.starts_with("payload") {
            errors.push(format!(
                "payload.files[{}] must reside under payload/",
                i
            ));
        }
    }

    // Validate key_slots
    if config.key_slots.is_empty() {
        errors.push("key_slots cannot be empty".to_string());
    }

    for (i, slot) in config.key_slots.iter().enumerate() {
        // Validate slot.salt is base64
        if BASE64_STANDARD.decode(&slot.salt).is_err() {
            errors.push(format!("key_slot[{}].salt is not valid base64", i));
        }

        // Validate slot.wrapped_dek is base64
        if BASE64_STANDARD.decode(&slot.wrapped_dek).is_err() {
            errors.push(format!("key_slot[{}].wrapped_dek is not valid base64", i));
        }

        // Validate slot.nonce is base64
        if BASE64_STANDARD.decode(&slot.nonce).is_err() {
            errors.push(format!("key_slot[{}].nonce is not valid base64", i));
        }
    }

    errors
}

fn validate_unencrypted_config(config: &UnencryptedConfig) -> Vec<String> {
    let mut errors = Vec::new();

    if config.encrypted {
        errors.push("unencrypted config must set encrypted=false".to_string());
    }

    if config.version.trim().is_empty() {
        errors.push("version cannot be empty".to_string());
    }

    if config.payload.path.trim().is_empty() {
        errors.push("payload.path cannot be empty".to_string());
    } else {
        let path = Path::new(&config.payload.path);
        if path.is_absolute() {
            errors.push("payload.path must be relative".to_string());
        }
        if path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            errors.push("payload.path must not contain '..'".to_string());
        }
        if !path.starts_with("payload") {
            errors.push("payload.path must reside under payload/".to_string());
        }
    }

    let valid_formats = ["sqlite"];
    if !valid_formats.contains(&config.payload.format.as_str()) {
        errors.push(format!(
            "payload.format should be one of {:?}, got '{}'",
            valid_formats, config.payload.format
        ));
    }

    errors
}

/// Check payload manifest validity
fn check_payload_manifest(site_dir: &Path) -> CheckResult {
    let config_path = site_dir.join("config.json");
    let payload_dir = site_dir.join("payload");

    if !payload_dir.exists() {
        return CheckResult::fail("payload/ directory not found");
    }

    // Parse config for expected payload
    let config: ArchiveConfig = match File::open(&config_path)
        .and_then(|f| Ok(serde_json::from_reader(BufReader::new(f))?))
    {
        Ok(c) => c,
        Err(_) => return CheckResult::fail("Could not parse config.json"),
    };

    let mut errors = Vec::new();

    match &config {
        ArchiveConfig::Encrypted(enc) => {
            // Check each expected chunk file exists
            for (i, expected_file) in enc.payload.files.iter().enumerate() {
                let chunk_path = site_dir.join(expected_file);
                if !chunk_path.exists() {
                    errors.push(format!("Missing chunk file: {}", expected_file));
                }

                // Verify filename follows pattern
                let expected_name = format!("payload/chunk-{:05}.bin", i);
                if *expected_file != expected_name {
                    errors.push(format!(
                        "Chunk {} has unexpected name: {} (expected {})",
                        i, expected_file, expected_name
                    ));
                }
            }

            // Check for contiguous chunk files (no gaps)
            let mut found_chunks: HashSet<u32> = HashSet::new();
            if let Ok(entries) = fs::read_dir(&payload_dir) {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let name_str = name.to_string_lossy();
                    if name_str.starts_with("chunk-")
                        && name_str.ends_with(".bin")
                        && let Some(num_str) = name_str
                            .strip_prefix("chunk-")
                            .and_then(|s| s.strip_suffix(".bin"))
                        && let Ok(num) = num_str.parse::<u32>()
                    {
                        found_chunks.insert(num);
                    }
                }
            }

            // Check for gaps
            if !found_chunks.is_empty() {
                let max_chunk = *found_chunks.iter().max().unwrap_or(&0);
                for i in 0..=max_chunk {
                    if !found_chunks.contains(&i) {
                        errors.push(format!(
                            "Gap in chunk sequence: chunk-{:05}.bin is missing",
                            i
                        ));
                    }
                }
            }
        }
        ArchiveConfig::Unencrypted(unenc) => {
            let rel_path = Path::new(&unenc.payload.path);
            if rel_path.is_absolute() {
                errors.push("payload.path must be relative".to_string());
            } else if rel_path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                errors.push("payload.path must not contain '..'".to_string());
            } else if !rel_path.starts_with("payload") {
                errors.push("payload.path must reside under payload/".to_string());
            } else {
                let payload_path = site_dir.join(rel_path);
                if !payload_path.exists() {
                    errors.push(format!("Missing payload file: {}", unenc.payload.path));
                }
            }
        }
    }

    if errors.is_empty() {
        CheckResult::pass()
    } else {
        CheckResult::fail(errors.join("; "))
    }
}

/// Check size limits for chunk files
fn check_size_limits(site_dir: &Path) -> CheckResult {
    let mut errors = Vec::new();

    let config_path = site_dir.join("config.json");
    let config: ArchiveConfig = match File::open(&config_path)
        .context("Failed to open config.json")
        .and_then(|f| serde_json::from_reader(BufReader::new(f)).context("Failed to parse JSON"))
    {
        Ok(c) => c,
        Err(e) => {
            return CheckResult::fail(format!("Failed to parse config.json: {}", e));
        }
    };

    match &config {
        ArchiveConfig::Encrypted(_) => {
            let payload_dir = site_dir.join("payload");
            if !payload_dir.is_dir() {
                errors.push("payload/ directory not found for size check".to_string());
            } else if let Ok(entries) = fs::read_dir(&payload_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().map(|e| e == "bin").unwrap_or(false)
                        && let Ok(meta) = path.metadata()
                        && meta.len() > MAX_CHUNK_SIZE
                    {
                        errors.push(format!(
                            "{} exceeds 100MB limit ({} bytes)",
                            path.file_name().unwrap_or_default().to_string_lossy(),
                            meta.len()
                        ));
                    }
                }
            }
        }
        ArchiveConfig::Unencrypted(unenc) => {
            let payload_path = site_dir.join(&unenc.payload.path);
            if !payload_path.exists() {
                errors.push(format!(
                    "payload file not found for size check: {}",
                    unenc.payload.path
                ));
            } else if let Ok(meta) = payload_path.metadata() {
                if meta.len() > MAX_CHUNK_SIZE {
                    errors.push(format!(
                        "{} exceeds 100MB limit ({} bytes)",
                        unenc.payload.path,
                        meta.len()
                    ));
                }
            }
        }
    }

    if errors.is_empty() {
        CheckResult::pass()
    } else {
        CheckResult::fail(errors.join("; "))
    }
}

/// Check integrity.json hashes match file contents
fn check_integrity(site_dir: &Path, verbose: bool) -> CheckResult {
    let integrity_path = site_dir.join("integrity.json");

    let manifest: IntegrityManifest = match File::open(&integrity_path)
        .context("Failed to open integrity.json")
        .and_then(|f| serde_json::from_reader(BufReader::new(f)).context("Failed to parse JSON"))
    {
        Ok(m) => m,
        Err(e) => return CheckResult::fail(format!("Failed to parse integrity.json: {}", e)),
    };

    let mut errors = Vec::new();
    let mut checked_files: HashSet<String> = HashSet::new();

    // Verify each file in manifest
    for (rel_path, entry) in &manifest.files {
        checked_files.insert(rel_path.clone());

        let file_path = site_dir.join(rel_path);
        if !file_path.exists() {
            errors.push(format!("File in manifest but missing: {}", rel_path));
            continue;
        }

        // Compute hash
        let computed_hash = match compute_file_hash(&file_path) {
            Ok(h) => h,
            Err(e) => {
                errors.push(format!("Failed to hash {}: {}", rel_path, e));
                continue;
            }
        };

        if computed_hash != entry.sha256 {
            errors.push(format!(
                "Hash mismatch for {}: expected {}, got {}",
                rel_path, entry.sha256, computed_hash
            ));
        } else if verbose {
            println!("    ✓ {}", rel_path);
        }
    }

    // Check for extra files not in manifest
    let actual_files = collect_all_files(site_dir);
    for file in actual_files {
        // Skip integrity.json itself
        if file == "integrity.json" {
            continue;
        }
        if !checked_files.contains(&file) {
            errors.push(format!("File not in manifest: {}", file));
        }
    }

    if errors.is_empty() {
        CheckResult::pass()
    } else {
        CheckResult::fail(errors.join("; "))
    }
}

/// Check for secret leakage in site/
fn check_no_secrets(site_dir: &Path) -> CheckResult {
    let mut errors = Vec::new();

    // Check for forbidden files
    for file in SECRET_FILES {
        let path = site_dir.join(file);
        if path.exists() {
            errors.push(format!("Secret file found in site/: {}", file));
        }
    }

    // Check for forbidden directories
    for dir in SECRET_DIRS {
        let path = site_dir.join(dir);
        if path.exists() && path.is_dir() {
            errors.push(format!("Secret directory found in site/: {}/", dir));
        }
    }

    // Check config.json doesn't contain plaintext secrets
    // Note: We're looking for actual secret values, not field names like "total_plaintext_size"
    let config_path = site_dir.join("config.json");
    if config_path.exists()
        && let Ok(content) = fs::read_to_string(&config_path)
    {
        let content_lower = content.to_lowercase();
        // Check for patterns that indicate actual secrets being stored
        // These patterns look for JSON keys that shouldn't exist in public config
        let forbidden_patterns = [
            ("\"password\":", "password field"), // Password stored in config
            ("\"secret\":", "secret field"),     // Secret stored directly
            ("\"private_key\":", "private_key field"), // Private key in config
            ("\"master_key\":", "master_key field"), // Master key exposed
            ("\"recovery_secret\":", "recovery_secret"), // Recovery secret in config
        ];
        for (pattern, description) in forbidden_patterns {
            if content_lower.contains(pattern) {
                errors.push(format!(
                    "config.json contains forbidden pattern: {}",
                    description
                ));
            }
        }
    }

    if errors.is_empty() {
        CheckResult::pass()
    } else {
        CheckResult::fail(errors.join("; "))
    }
}

/// Compute SHA256 hash of a file
fn compute_file_hash(path: &Path) -> Result<String> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8192];

    loop {
        let bytes_read = reader.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }

    Ok(format!("{:x}", hasher.finalize()))
}

/// Collect all files in a directory recursively
fn collect_all_files(dir: &Path) -> Vec<String> {
    let mut files = Vec::new();
    collect_files_recursive(dir, dir, &mut files);
    files
}

fn collect_files_recursive(base: &Path, current: &Path, files: &mut Vec<String>) {
    if let Ok(entries) = fs::read_dir(current) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_files_recursive(base, &path, files);
            } else if let Ok(rel) = path.strip_prefix(base) {
                files.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
}

/// Calculate total size of a directory
fn calculate_dir_size(dir: &Path) -> Result<u64> {
    let mut total = 0u64;

    fn calc_recursive(path: &Path, total: &mut u64) -> Result<()> {
        if path.is_dir() {
            for entry in fs::read_dir(path)? {
                calc_recursive(&entry?.path(), total)?;
            }
        } else {
            *total += path.metadata()?.len();
        }
        Ok(())
    }

    calc_recursive(dir, &mut total)?;
    Ok(total)
}

/// Print verification result in human-readable format
pub fn print_result(result: &VerifyResult, verbose: bool) {
    let status_icon = if result.status == "valid" {
        "✓"
    } else {
        "✗"
    };
    println!(
        "\n{} Bundle status: {}",
        status_icon,
        result.status.to_uppercase()
    );

    println!("\nChecks:");
    print_check("  Required files", &result.checks.required_files, verbose);
    print_check("  Config schema", &result.checks.config_schema, verbose);
    print_check(
        "  Payload manifest",
        &result.checks.payload_manifest,
        verbose,
    );
    print_check("  Size limits", &result.checks.size_limits, verbose);
    print_check("  Integrity", &result.checks.integrity, verbose);
    print_check("  No secrets", &result.checks.no_secrets_in_site, verbose);

    if !result.warnings.is_empty() {
        println!("\nWarnings:");
        for warning in &result.warnings {
            println!("  ⚠ {}", warning);
        }
    }

    println!(
        "\nTotal site size: {} bytes ({:.2} MB)",
        result.site_size_bytes,
        result.site_size_bytes as f64 / (1024.0 * 1024.0)
    );
}

fn print_check(name: &str, result: &CheckResult, verbose: bool) {
    let icon = if result.passed { "✓" } else { "✗" };
    print!("{}: {} ", name, icon);

    if result.passed {
        println!("OK");
    } else if let Some(details) = &result.details {
        if verbose {
            println!("FAILED");
            println!("      {}", details);
        } else {
            // Truncate long error messages
            let display = if details.len() > 60 {
                format!("{}...", &details[..60])
            } else {
                details.clone()
            };
            println!("FAILED: {}", display);
        }
    } else {
        println!("FAILED");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pages::bundle::IntegrityEntry;
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    /// Path to the pages_verify fixtures directory
    fn fixtures_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pages_verify")
    }

    /// Copy a fixture directory to the destination.
    /// `fixture_name` is the subdirectory under tests/fixtures/pages_verify/ (e.g., "valid", "unencrypted")
    fn copy_fixture(fixture_name: &str, dest: &Path) -> Result<()> {
        let src = fixtures_dir().join(fixture_name).join("site");
        copy_dir_recursive(&src, dest)
    }

    /// Recursively copy a directory and its contents
    fn copy_dir_recursive(src: &Path, dest: &Path) -> Result<()> {
        if !dest.exists() {
            fs::create_dir_all(dest)?;
        }
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            let dest_path = dest.join(entry.file_name());
            if file_type.is_dir() {
                copy_dir_recursive(&entry.path(), &dest_path)?;
            } else {
                fs::copy(entry.path(), &dest_path)?;
            }
        }
        Ok(())
    }

    #[test]
    fn test_verify_minimal_valid_site() {
        let temp = TempDir::new().unwrap();
        let site_dir = temp.path().join("site");

        // Copy the valid fixture to temp directory
        copy_fixture("valid", &site_dir).unwrap();

        let result = verify_bundle(&site_dir, true).unwrap();

        // Debug: print which checks failed
        if !result.checks.required_files.passed {
            eprintln!(
                "FAILED: required_files - {:?}",
                result.checks.required_files.details
            );
        }
        if !result.checks.config_schema.passed {
            eprintln!(
                "FAILED: config_schema - {:?}",
                result.checks.config_schema.details
            );
        }
        if !result.checks.payload_manifest.passed {
            eprintln!(
                "FAILED: payload_manifest - {:?}",
                result.checks.payload_manifest.details
            );
        }
        if !result.checks.size_limits.passed {
            eprintln!(
                "FAILED: size_limits - {:?}",
                result.checks.size_limits.details
            );
        }
        if !result.checks.integrity.passed {
            eprintln!("FAILED: integrity - {:?}", result.checks.integrity.details);
        }
        if !result.checks.no_secrets_in_site.passed {
            eprintln!(
                "FAILED: no_secrets_in_site - {:?}",
                result.checks.no_secrets_in_site.details
            );
        }

        assert_eq!(result.status, "valid");
        assert!(result.checks.required_files.passed);
        assert!(result.checks.config_schema.passed);
    }

    #[test]
    fn test_verify_unencrypted_site() {
        let temp = TempDir::new().unwrap();
        let site_dir = temp.path().join("site");

        // Copy the unencrypted fixture to temp directory
        copy_fixture("unencrypted", &site_dir).unwrap();

        let result = verify_bundle(&site_dir, true).unwrap();
        assert!(result.checks.config_schema.passed);
        assert!(result.checks.payload_manifest.passed);
        assert_eq!(result.status, "valid");
    }

    #[test]
    fn test_verify_missing_required_files() {
        let temp = TempDir::new().unwrap();
        let site_dir = temp.path().join("site");

        // Copy the missing_required_no_viewer fixture (missing viewer.js)
        copy_fixture("missing_required_no_viewer", &site_dir).unwrap();

        let result = verify_bundle(&site_dir, false).unwrap();
        assert_eq!(result.status, "invalid");
        assert!(!result.checks.required_files.passed);
    }

    #[test]
    fn test_verify_invalid_config() {
        let temp = TempDir::new().unwrap();
        let site_dir = temp.path().join("site");

        // Copy valid fixture then overwrite config with invalid one
        copy_fixture("valid", &site_dir).unwrap();

        // Write invalid config
        fs::write(
            site_dir.join("config.json"),
            r#"{"version": 2, "export_id": "invalid"}"#,
        )
        .unwrap();

        let result = verify_bundle(&site_dir, false).unwrap();
        assert!(!result.checks.config_schema.passed);
    }

    #[test]
    fn test_verify_secret_leakage() {
        let temp = TempDir::new().unwrap();
        let site_dir = temp.path().join("site");

        // Copy the secret_leak fixture (contains recovery-secret.txt)
        copy_fixture("secret_leak", &site_dir).unwrap();

        let result = verify_bundle(&site_dir, false).unwrap();
        assert!(!result.checks.no_secrets_in_site.passed);
    }

    #[test]
    fn test_verify_with_integrity() {
        let temp = TempDir::new().unwrap();
        let site_dir = temp.path().join("site");

        // Copy valid fixture
        copy_fixture("valid", &site_dir).unwrap();

        // Create integrity.json
        let mut files = BTreeMap::new();
        for file in REQUIRED_FILES {
            let hash = compute_file_hash(&site_dir.join(file)).unwrap();
            let size = fs::metadata(site_dir.join(file)).unwrap().len();
            files.insert(file.to_string(), IntegrityEntry { sha256: hash, size });
        }
        // Add payload chunk
        let chunk_hash = compute_file_hash(&site_dir.join("payload/chunk-00000.bin")).unwrap();
        let chunk_size = fs::metadata(site_dir.join("payload/chunk-00000.bin"))
            .unwrap()
            .len();
        files.insert(
            "payload/chunk-00000.bin".to_string(),
            IntegrityEntry {
                sha256: chunk_hash,
                size: chunk_size,
            },
        );

        let manifest = IntegrityManifest {
            version: 1,
            generated_at: "2024-01-01T00:00:00Z".to_string(),
            files,
        };
        fs::write(
            site_dir.join("integrity.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let result = verify_bundle(&site_dir, false).unwrap();
        assert!(result.checks.integrity.passed);
    }

    #[test]
    fn test_verify_integrity_mismatch() {
        let temp = TempDir::new().unwrap();
        let site_dir = temp.path().join("site");

        // Copy valid fixture
        copy_fixture("valid", &site_dir).unwrap();

        // Create integrity.json with wrong hash
        let mut files = BTreeMap::new();
        files.insert(
            "index.html".to_string(),
            IntegrityEntry {
                sha256: "0000000000000000000000000000000000000000000000000000000000000000"
                    .to_string(),
                size: 10,
            },
        );

        let manifest = IntegrityManifest {
            version: 1,
            generated_at: "2024-01-01T00:00:00Z".to_string(),
            files,
        };
        fs::write(
            site_dir.join("integrity.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let result = verify_bundle(&site_dir, false).unwrap();
        assert!(!result.checks.integrity.passed);
        assert!(
            result
                .checks
                .integrity
                .details
                .as_ref()
                .unwrap()
                .contains("Hash mismatch")
        );
    }

    #[test]
    fn test_resolve_site_dir() {
        let temp = TempDir::new().unwrap();

        // Test with site/ subdirectory
        let site_dir = temp.path().join("site");
        fs::create_dir_all(&site_dir).unwrap();

        let resolved = resolve_site_dir(temp.path()).unwrap();
        assert!(resolved.ends_with("site"));

        // Test with direct path
        let resolved_direct = resolve_site_dir(&site_dir).unwrap();
        assert_eq!(resolved_direct, site_dir);
    }

    #[test]
    fn test_chunk_size_limit() {
        let temp = TempDir::new().unwrap();
        let site_dir = temp.path();
        let payload_dir = site_dir.join("payload");
        fs::create_dir_all(&payload_dir).unwrap();

        // Create a small file (should pass)
        fs::write(payload_dir.join("chunk-00000.bin"), "small").unwrap();

        let result = check_size_limits(site_dir);
        assert!(result.passed);
    }
}
