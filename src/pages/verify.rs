//! Verify command for CI pipelines.
//!
//! Provides `cass pages --verify <PATH>` to validate an existing export bundle for CI/CD.
//! The verifier confirms correct structure, config schema, payload integrity, and
//! the absence of secrets in site/.

use anyhow::{Context, Result};
use base64::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use super::archive_config::{ArchiveConfig, UnencryptedConfig};
use super::bundle::IntegrityManifest;
use super::encrypt::EncryptionConfig;
use std::fmt;

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

/// JSON keys in config.json that indicate plaintext secret leakage.
const FORBIDDEN_CONFIG_KEYS: &[(&str, &str)] = &[
    ("password", "password field"),
    ("secret", "secret field"),
    ("private_key", "private_key field"),
    ("master_key", "master_key field"),
    ("recovery_secret", "recovery_secret"),
];

const ENCRYPTED_CONFIG_KEYS: &[&str] = &[
    "version",
    "export_id",
    "base_nonce",
    "compression",
    "kdf_defaults",
    "payload",
    "key_slots",
];
const UNENCRYPTED_CONFIG_KEYS: &[&str] = &["encrypted", "version", "payload", "warning"];
const ENCRYPTED_PAYLOAD_KEYS: &[&str] = &[
    "chunk_size",
    "chunk_count",
    "total_compressed_size",
    "total_plaintext_size",
    "files",
];
const UNENCRYPTED_PAYLOAD_KEYS: &[&str] = &["path", "format", "size_bytes"];
const ARGON2_PARAM_KEYS: &[&str] = &["memory_kb", "iterations", "parallelism"];
const KEY_SLOT_KEYS: &[&str] = &[
    "id",
    "slot_type",
    "kdf",
    "salt",
    "wrapped_dek",
    "nonce",
    "argon2_params",
];

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

    let warnings = Vec::new();

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
        CheckResult::fail("integrity.json missing — bundle integrity cannot be verified")
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
    super::resolve_site_dir(path)
}

/// Check that all required files exist
fn check_required_files(site_dir: &Path) -> CheckResult {
    let mut missing = Vec::new();
    let mut invalid = Vec::new();

    for file in REQUIRED_FILES {
        let path = site_dir.join(file);
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                let file_type = metadata.file_type();
                if file_type.is_file() {
                    continue;
                }
                if file_type.is_symlink()
                    && let Ok(target_meta) = fs::metadata(&path)
                    && target_meta.file_type().is_file()
                {
                    continue;
                }
                invalid.push(format!("{file} (must be a regular file)"));
            }
            Err(_) => missing.push(*file),
        }
    }

    // Also check payload/ directory exists
    if !site_dir.join("payload").is_dir() {
        missing.push("payload/");
    }

    if missing.is_empty() && invalid.is_empty() {
        CheckResult::pass()
    } else {
        let mut parts = Vec::new();
        if !missing.is_empty() {
            parts.push(format!("Missing files: {}", missing.join(", ")));
        }
        if !invalid.is_empty() {
            parts.push(format!("Invalid required files: {}", invalid.join(", ")));
        }
        CheckResult::fail(parts.join("; "))
    }
}

/// Check config.json schema validity
fn check_config_schema(site_dir: &Path) -> CheckResult {
    let config_path = site_dir.join("config.json");

    let content = match fs::read_to_string(&config_path).context("Failed to read config.json") {
        Ok(content) => content,
        Err(e) => return CheckResult::fail(format!("Failed to read config.json: {}", e)),
    };

    let config_json: Value =
        match serde_json::from_str(&content).context("Failed to parse JSON syntax") {
            Ok(json) => json,
            Err(e) => return CheckResult::fail(format!("Failed to parse config.json: {}", e)),
        };

    let unknown_field_errors = find_unknown_config_fields(&config_json);
    if !unknown_field_errors.is_empty() {
        return CheckResult::fail(unknown_field_errors.join("; "));
    }

    let config: ArchiveConfig = match serde_json::from_value(config_json) {
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

fn find_unknown_config_fields(value: &Value) -> Vec<String> {
    let mut errors = Vec::new();
    let Some(root) = value.as_object() else {
        return errors;
    };

    if root.contains_key("encrypted") {
        collect_unknown_fields(root, UNENCRYPTED_CONFIG_KEYS, "", &mut errors);
        if let Some(payload) = root.get("payload").and_then(Value::as_object) {
            collect_unknown_fields(payload, UNENCRYPTED_PAYLOAD_KEYS, "payload", &mut errors);
        }
    } else {
        collect_unknown_fields(root, ENCRYPTED_CONFIG_KEYS, "", &mut errors);
        if let Some(payload) = root.get("payload").and_then(Value::as_object) {
            collect_unknown_fields(payload, ENCRYPTED_PAYLOAD_KEYS, "payload", &mut errors);
        }
        if let Some(params) = root.get("kdf_defaults").and_then(Value::as_object) {
            collect_unknown_fields(params, ARGON2_PARAM_KEYS, "kdf_defaults", &mut errors);
        }
        if let Some(slots) = root.get("key_slots").and_then(Value::as_array) {
            for (idx, slot) in slots.iter().enumerate() {
                if let Some(slot_obj) = slot.as_object() {
                    let slot_path = format!("key_slots[{idx}]");
                    collect_unknown_fields(slot_obj, KEY_SLOT_KEYS, &slot_path, &mut errors);
                    if let Some(params) = slot_obj.get("argon2_params").and_then(Value::as_object) {
                        collect_unknown_fields(
                            params,
                            ARGON2_PARAM_KEYS,
                            &format!("{slot_path}.argon2_params"),
                            &mut errors,
                        );
                    }
                }
            }
        }
    }

    errors
}

fn collect_unknown_fields(
    object: &Map<String, Value>,
    allowed_keys: &[&str],
    current_path: &str,
    errors: &mut Vec<String>,
) {
    for key in object.keys() {
        if !allowed_keys.contains(&key.as_str()) {
            let path = if current_path.is_empty() {
                key.clone()
            } else {
                format!("{current_path}.{key}")
            };
            errors.push(format!("config.json contains unknown field: {path}"));
        }
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
            errors.push(format!("payload.files[{}] must reside under payload/", i));
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
        validate_payload_path(&mut errors, "payload.path", path);
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

fn validate_payload_path(errors: &mut Vec<String>, label: &str, path: &Path) -> bool {
    let mut ok = true;
    if path.is_absolute() {
        errors.push(format!("{label} must be relative"));
        ok = false;
    }
    if path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        errors.push(format!("{label} must not contain '..'"));
        ok = false;
    }
    if !path.starts_with("payload") {
        errors.push(format!("{label} must reside under payload/"));
        ok = false;
    }
    ok
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
                // Security: Verify filename follows expected pattern first (defense-in-depth)
                // This also implicitly prevents path traversal since valid patterns are "payload/chunk-NNNNN.bin"
                let expected_name = format!("payload/chunk-{:05}.bin", i);
                if *expected_file != expected_name {
                    errors.push(format!(
                        "Chunk {} has unexpected name: {} (expected {})",
                        i, expected_file, expected_name
                    ));
                    // Skip existence check for malformed paths to prevent path traversal
                    continue;
                }

                let chunk_path = site_dir.join(expected_file);
                match fs::symlink_metadata(&chunk_path) {
                    Ok(meta) => {
                        let file_type = meta.file_type();
                        if file_type.is_symlink() {
                            errors.push(format!("{expected_file} must not be a symlink"));
                        } else if !file_type.is_file() {
                            errors.push(format!("{expected_file} must be a regular file"));
                        }
                    }
                    Err(_) => errors.push(format!("Missing chunk file: {}", expected_file)),
                }
            }

            // Inventory chunk files to detect malformed names and out-of-range indices.
            match fs::read_dir(&payload_dir) {
                Ok(entries) => {
                    for entry in entries {
                        let entry = match entry {
                            Ok(entry) => entry,
                            Err(err) => {
                                errors
                                    .push(format!("Failed to read payload directory entry: {err}"));
                                continue;
                            }
                        };
                        let name = entry.file_name();
                        let name_str = name.to_string_lossy();
                        if !name_str.starts_with("chunk-") || !name_str.ends_with(".bin") {
                            continue;
                        }

                        let Some(num_str) = name_str
                            .strip_prefix("chunk-")
                            .and_then(|s| s.strip_suffix(".bin"))
                        else {
                            errors.push(format!("Malformed chunk filename: {name_str}"));
                            continue;
                        };

                        if num_str.len() < 5 || !num_str.chars().all(|c| c.is_ascii_digit()) {
                            errors.push(format!("Malformed chunk filename: {name_str}"));
                            continue;
                        }

                        let idx = match num_str.parse::<usize>() {
                            Ok(idx) => idx,
                            Err(_) => {
                                errors.push(format!("Malformed chunk filename: {name_str}"));
                                continue;
                            }
                        };

                        if idx >= enc.payload.files.len() {
                            errors.push(format!("Unexpected chunk file index: chunk-{idx:05}.bin"));
                        }
                    }
                }
                Err(err) => errors.push(format!("Failed to read payload/ directory: {err}")),
            }
        }
        ArchiveConfig::Unencrypted(unenc) => {
            let rel_path = Path::new(&unenc.payload.path);
            if validate_payload_path(&mut errors, "payload.path", rel_path) {
                let payload_path = site_dir.join(rel_path);
                match fs::symlink_metadata(&payload_path) {
                    Ok(meta) => {
                        let file_type = meta.file_type();
                        if file_type.is_symlink() {
                            errors.push(format!("{} must not be a symlink", unenc.payload.path));
                        } else if !file_type.is_file() {
                            errors.push(format!("{} must be a regular file", unenc.payload.path));
                        }
                    }
                    Err(_) => errors.push(format!("Missing payload file: {}", unenc.payload.path)),
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
            } else {
                match fs::read_dir(&payload_dir) {
                    Ok(entries) => {
                        for entry in entries {
                            let entry = match entry {
                                Ok(entry) => entry,
                                Err(err) => {
                                    errors.push(format!(
                                        "Failed to read payload directory entry: {err}"
                                    ));
                                    continue;
                                }
                            };
                            let path = entry.path();
                            if path.extension().map(|e| e == "bin").unwrap_or(false) {
                                match fs::symlink_metadata(&path) {
                                    Ok(meta) => {
                                        let file_type = meta.file_type();
                                        if file_type.is_symlink() {
                                            errors.push(format!(
                                                "{} must not be a symlink",
                                                path.file_name()
                                                    .unwrap_or_default()
                                                    .to_string_lossy()
                                            ));
                                            continue;
                                        }
                                        if !file_type.is_file() {
                                            errors.push(format!(
                                                "{} must be a regular file",
                                                path.file_name()
                                                    .unwrap_or_default()
                                                    .to_string_lossy()
                                            ));
                                            continue;
                                        }
                                        if meta.len() > MAX_CHUNK_SIZE {
                                            errors.push(format!(
                                                "{} exceeds 100MB limit ({} bytes)",
                                                path.file_name()
                                                    .unwrap_or_default()
                                                    .to_string_lossy(),
                                                meta.len()
                                            ));
                                        }
                                    }
                                    Err(err) => errors.push(format!(
                                        "failed to stat {}: {}",
                                        path.file_name().unwrap_or_default().to_string_lossy(),
                                        err
                                    )),
                                }
                            }
                        }
                    }
                    Err(err) => errors.push(format!("Failed to read payload/ directory: {err}")),
                }
            }
        }
        ArchiveConfig::Unencrypted(unenc) => {
            let payload_path = Path::new(&unenc.payload.path);
            if validate_payload_path(&mut errors, "payload.path", payload_path) {
                let payload_path = site_dir.join(payload_path);
                if !payload_path.exists() {
                    errors.push(format!(
                        "payload file not found for size check: {}",
                        unenc.payload.path
                    ));
                } else {
                    match fs::symlink_metadata(&payload_path) {
                        Ok(meta) => {
                            let file_type = meta.file_type();
                            if file_type.is_symlink() {
                                errors
                                    .push(format!("{} must not be a symlink", unenc.payload.path));
                            } else if !file_type.is_file() {
                                errors
                                    .push(format!("{} must be a regular file", unenc.payload.path));
                            } else if meta.len() > MAX_CHUNK_SIZE {
                                errors.push(format!(
                                    "{} exceeds 100MB limit ({} bytes)",
                                    unenc.payload.path,
                                    meta.len()
                                ));
                            }
                        }
                        Err(err) => errors.push(format!(
                            "failed to stat payload file {}: {}",
                            unenc.payload.path, err
                        )),
                    }
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
    let canonical_site = match site_dir.canonicalize() {
        Ok(path) => path,
        Err(e) => {
            return CheckResult::fail(format!(
                "Failed to resolve site directory for integrity checks: {}",
                e
            ));
        }
    };

    // Verify each file in manifest
    for (rel_path, entry) in &manifest.files {
        checked_files.insert(rel_path.clone());

        if let Some(reason) = detect_encoded_path_violation(rel_path) {
            errors.push(format!(
                "integrity.json contains {reason} (security violation): {}",
                rel_path
            ));
            continue;
        }

        // Security: Validate path doesn't escape site_dir via traversal
        let path = Path::new(rel_path);
        if path.is_absolute() {
            errors.push(format!(
                "integrity.json contains absolute path (security violation): {}",
                rel_path
            ));
            continue;
        }
        if path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            errors.push(format!(
                "integrity.json contains path traversal (security violation): {}",
                rel_path
            ));
            continue;
        }

        let file_path = site_dir.join(rel_path);
        let metadata = match fs::symlink_metadata(&file_path) {
            Ok(meta) => meta,
            Err(_) => {
                errors.push(format!("File in manifest but missing: {}", rel_path));
                continue;
            }
        };

        let file_type = metadata.file_type();

        if !file_type.is_file() && !file_type.is_symlink() {
            errors.push(format!(
                "integrity.json references non-file entry (security violation): {}",
                rel_path
            ));
            continue;
        }

        // Resolve symlinks and ensure final target remains within site_dir.
        let canonical_file = match file_path.canonicalize() {
            Ok(path) => path,
            Err(_) => {
                errors.push(format!("File in manifest but missing: {}", rel_path));
                continue;
            }
        };
        if !canonical_file.starts_with(&canonical_site) {
            errors.push(format!(
                "integrity.json path escapes site directory (security violation): {}",
                rel_path
            ));
            continue;
        }

        // For symlinks, only permit links to regular files within site_dir.
        if file_type.is_symlink() {
            match fs::metadata(&file_path) {
                Ok(target_meta) if target_meta.file_type().is_file() => {}
                Ok(_) => {
                    errors.push(format!(
                        "integrity.json symlink target is not a regular file (security violation): {}",
                        rel_path
                    ));
                    continue;
                }
                Err(e) => {
                    errors.push(format!(
                        "Failed to resolve symlink target for {}: {}",
                        rel_path, e
                    ));
                    continue;
                }
            }
        }

        // Fast-fail on size mismatch before the expensive SHA256 hash.
        // Use the canonical path so symlinks resolve to the actual target size.
        if let Ok(actual_meta) = fs::metadata(&canonical_file)
            && actual_meta.len() != entry.size
        {
            errors.push(format!(
                "Size mismatch for {}: expected {}, got {}",
                rel_path,
                entry.size,
                actual_meta.len()
            ));
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
    let actual_files = match collect_all_files(site_dir) {
        Ok(files) => files,
        Err(e) => return CheckResult::fail(format!("Failed to enumerate files: {}", e)),
    };
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

#[derive(Debug)]
enum PercentDecodeError {
    InvalidEncoding,
    InvalidUtf8,
    NullByte,
}

impl fmt::Display for PercentDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidEncoding => write!(f, "invalid percent-encoding"),
            Self::InvalidUtf8 => write!(f, "invalid UTF-8 after percent-decoding"),
            Self::NullByte => write!(f, "null byte in decoded path"),
        }
    }
}

struct DecodeOutcome {
    decoded: String,
    changed: bool,
}

fn percent_decode_once(input: &str) -> Result<DecodeOutcome, PercentDecodeError> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    let mut changed = false;

    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return Err(PercentDecodeError::InvalidEncoding);
            }
            let hi = bytes[i + 1];
            let lo = bytes[i + 2];
            let hex = [hi, lo];
            let hex_str =
                std::str::from_utf8(&hex).map_err(|_| PercentDecodeError::InvalidEncoding)?;
            let val =
                u8::from_str_radix(hex_str, 16).map_err(|_| PercentDecodeError::InvalidEncoding)?;
            out.push(val);
            i += 3;
            changed = true;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }

    if out.contains(&0) {
        return Err(PercentDecodeError::NullByte);
    }

    let decoded = String::from_utf8(out).map_err(|_| PercentDecodeError::InvalidUtf8)?;
    Ok(DecodeOutcome { decoded, changed })
}

fn contains_path_traversal_like(input: &str) -> bool {
    input.split(['/', '\\']).any(|segment| segment == "..")
}

fn is_absolute_like(input: &str) -> bool {
    let normalized = input.replace('\\', "/");
    if normalized.starts_with('/') || normalized.starts_with("//") {
        return true;
    }
    let bytes = normalized.as_bytes();
    bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic()
}

/// Check for Unicode characters that are visual look-alikes for path-sensitive
/// ASCII characters (`.`, `/`, `\`). These could bypass text-based path checks
/// on filesystems that perform Unicode compatibility normalization (NFKC).
fn contains_unicode_path_attack(input: &str) -> bool {
    for ch in input.chars() {
        match ch {
            // Fullwidth look-alikes (NFKC maps to ASCII equivalents)
            '\u{FF0E}' // FULLWIDTH FULL STOP → .
            | '\u{FF0F}' // FULLWIDTH SOLIDUS → /
            | '\u{FF3C}' // FULLWIDTH REVERSE SOLIDUS → \
            // Small form variants
            | '\u{FE52}' // SMALL FULL STOP → .
            // Dot leaders / ellipsis components
            | '\u{2024}' // ONE DOT LEADER → .
            // Halfwidth forms
            | '\u{FF61}' // HALFWIDTH IDEOGRAPHIC FULL STOP
            // Combining characters that could modify path-sensitive chars
            | '\u{0338}' // COMBINING LONG SOLIDUS OVERLAY (could visually disguise)
            | '\u{0337}' // COMBINING SHORT SOLIDUS OVERLAY
            // Zero-width characters (invisible, could split tokens)
            | '\u{200D}' // ZERO WIDTH JOINER
            | '\u{200C}' // ZERO WIDTH NON-JOINER
            | '\u{200B}' // ZERO WIDTH SPACE
            | '\u{FEFF}' // BYTE ORDER MARK / ZERO WIDTH NO-BREAK SPACE
            // Right-to-left override (can visually reverse path display)
            | '\u{202E}' // RIGHT-TO-LEFT OVERRIDE
            | '\u{202D}' // LEFT-TO-RIGHT OVERRIDE
            | '\u{202C}' // POP DIRECTIONAL FORMATTING
            | '\u{202A}' // LEFT-TO-RIGHT EMBEDDING
            | '\u{202B}' // RIGHT-TO-LEFT EMBEDDING
            | '\u{2066}' // LEFT-TO-RIGHT ISOLATE
            | '\u{2067}' // RIGHT-TO-LEFT ISOLATE
            | '\u{2068}' // FIRST STRONG ISOLATE
            | '\u{2069}' // POP DIRECTIONAL ISOLATE
            // Confusable slash characters
            | '\u{2044}' // FRACTION SLASH (visually similar to /)
            | '\u{2215}' // DIVISION SLASH (visually similar to /)
            | '\u{29F8}' // BIG SOLIDUS
            | '\u{1735}' // PHILIPPINE SINGLE PUNCTUATION (looks like /)
            // Confusable dot characters
            | '\u{2E2E}' // REVERSED QUESTION MARK (can look like period in some fonts)
            | '\u{0701}' // SYRIAC SUPRALINEAR FULL STOP
            | '\u{0702}' // SYRIAC SUBLINEAR FULL STOP
            | '\u{A60E}' // VAI FULL STOP
            | '\u{10A50}' // KHAROSHTHI PUNCTUATION DOT
            => return true,
            _ => {}
        }
    }
    false
}

fn detect_encoded_path_violation(rel_path: &str) -> Option<String> {
    if contains_path_traversal_like(rel_path) {
        return Some("path traversal".to_string());
    }
    if is_absolute_like(rel_path) {
        return Some("absolute path".to_string());
    }
    if contains_unicode_path_attack(rel_path) {
        return Some("unicode normalization attack".to_string());
    }

    if !rel_path.contains('%') {
        return None;
    }

    let mut current = rel_path.to_string();
    for _ in 0..3 {
        let outcome = match percent_decode_once(&current) {
            Ok(o) => o,
            Err(e) => return Some(e.to_string()),
        };
        if !outcome.changed {
            break;
        }
        current = outcome.decoded;
        if contains_path_traversal_like(&current) {
            return Some("url-encoded path traversal".to_string());
        }
        if is_absolute_like(&current) {
            return Some("url-encoded absolute path".to_string());
        }
        if contains_unicode_path_attack(&current) {
            return Some("url-encoded unicode normalization attack".to_string());
        }
        if !current.contains('%') {
            break;
        }
    }

    None
}

/// Check for secret leakage in site/
fn check_no_secrets(site_dir: &Path) -> CheckResult {
    let mut errors = Vec::new();

    // Check for forbidden files
    for file in SECRET_FILES {
        let path = site_dir.join(file);
        if fs::symlink_metadata(&path).is_ok() {
            errors.push(format!("Secret file found in site/: {}", file));
        }
    }

    // Check for forbidden directories
    for dir in SECRET_DIRS {
        let path = site_dir.join(dir);
        if let Ok(metadata) = fs::symlink_metadata(&path) {
            let file_type = metadata.file_type();
            if file_type.is_dir() || file_type.is_symlink() {
                errors.push(format!("Secret directory found in site/: {}/", dir));
            }
        }
    }

    // Recursive scan: detect secret files/dirs hidden in subdirectories
    find_secrets_recursive(site_dir, site_dir, &mut errors);

    // Check config.json doesn't contain plaintext secrets.
    // Walk the parsed JSON tree instead of doing brittle raw substring checks so
    // formatting changes like `"secret" : "..."` or nested objects can't hide leakage.
    let config_path = site_dir.join("config.json");
    if config_path.exists()
        && let Ok(content) = fs::read_to_string(&config_path)
        && let Ok(config_json) = serde_json::from_str::<Value>(&content)
    {
        find_forbidden_config_keys(&config_json, "", &mut errors);
    }

    if errors.is_empty() {
        CheckResult::pass()
    } else {
        CheckResult::fail(errors.join("; "))
    }
}

fn find_forbidden_config_keys(value: &Value, current_path: &str, findings: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let child_path = if current_path.is_empty() {
                    key.clone()
                } else {
                    format!("{current_path}.{key}")
                };
                if let Some((_, description)) = FORBIDDEN_CONFIG_KEYS
                    .iter()
                    .find(|(forbidden, _)| key.eq_ignore_ascii_case(forbidden))
                {
                    findings.push(format!(
                        "config.json contains forbidden field: {} at {}",
                        description, child_path
                    ));
                }
                find_forbidden_config_keys(child, &child_path, findings);
            }
        }
        Value::Array(items) => {
            for (idx, child) in items.iter().enumerate() {
                let child_path = if current_path.is_empty() {
                    format!("[{idx}]")
                } else {
                    format!("{current_path}[{idx}]")
                };
                find_forbidden_config_keys(child, &child_path, findings);
            }
        }
        _ => {}
    }
}

/// Recursively scan a directory tree for secret files and directories.
/// Finds entries whose name (not full path) matches SECRET_FILES or SECRET_DIRS
/// at any depth, catching secrets hidden in subdirectories.
fn find_secrets_recursive(base: &Path, current: &Path, findings: &mut Vec<String>) {
    let entries = match fs::read_dir(current) {
        Ok(entries) => entries,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => continue,
        };
        let name = match entry.file_name().to_str() {
            Some(n) => n.to_string(),
            None => continue,
        };
        let is_secret_file = SECRET_FILES.contains(&name.as_str());
        let is_secret_dir = SECRET_DIRS.contains(&name.as_str());

        let rel_path = path
            .strip_prefix(base)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");

        if file_type.is_dir() {
            if is_secret_dir {
                // Skip if this is a top-level match (already caught above)
                if current != base {
                    findings.push(format!(
                        "Secret directory found in site subdirectory: {}/",
                        rel_path
                    ));
                }
            }
            // Only recurse into real directories. Symlinked directories are handled below
            // so a malicious or accidental loop cannot drag verification outside site/.
            find_secrets_recursive(base, &path, findings);
        } else if file_type.is_symlink() {
            if is_secret_dir {
                if current != base {
                    findings.push(format!(
                        "Secret directory found in site subdirectory: {}/",
                        rel_path
                    ));
                }
            } else if is_secret_file && current != base {
                findings.push(format!(
                    "Secret file found in site subdirectory: {}",
                    rel_path
                ));
            }
        } else if file_type.is_file() && is_secret_file {
            // Skip if this is a top-level match (already caught above)
            if current != base {
                findings.push(format!(
                    "Secret file found in site subdirectory: {}",
                    rel_path
                ));
            }
        }
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
fn collect_all_files(dir: &Path) -> Result<Vec<String>> {
    let mut files = Vec::new();
    collect_files_recursive(dir, dir, &mut files)?;
    Ok(files)
}

fn collect_files_recursive(base: &Path, current: &Path, files: &mut Vec<String>) -> Result<()> {
    for entry in fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        let file_type = metadata.file_type();

        if file_type.is_symlink() {
            if let Ok(rel) = path.strip_prefix(base) {
                files.push(rel.to_string_lossy().replace('\\', "/"));
            }
            continue;
        }

        if file_type.is_dir() {
            collect_files_recursive(base, &path, files)?;
        } else if file_type.is_file()
            && let Ok(rel) = path.strip_prefix(base)
        {
            files.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
    Ok(())
}

/// Calculate total size of a directory
fn calculate_dir_size(dir: &Path) -> Result<u64> {
    let mut total = 0u64;

    fn calc_recursive(path: &Path, total: &mut u64) -> Result<()> {
        let metadata = fs::symlink_metadata(path)?;
        let file_type = metadata.file_type();

        if file_type.is_symlink() {
            return Ok(());
        }

        if file_type.is_dir() {
            for entry in fs::read_dir(path)? {
                calc_recursive(&entry?.path(), total)?;
            }
        } else if file_type.is_file() {
            *total += metadata.len();
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
            // Truncate long error messages (char-safe slicing)
            let display = if details.chars().count() > 60 {
                let truncated: String = details.chars().take(60).collect();
                format!("{truncated}...")
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

    fn assert_integrity_path_blocked(rel_path: &str) {
        let temp = TempDir::new().unwrap();
        let site_dir = temp.path();

        let mut files = BTreeMap::new();
        files.insert(
            rel_path.to_string(),
            IntegrityEntry {
                sha256: "deadbeef".repeat(8),
                size: 100,
            },
        );
        let manifest = IntegrityManifest {
            version: 1,
            generated_at: "2025-01-01T00:00:00Z".to_string(),
            files,
        };
        let manifest_json = serde_json::to_string(&manifest).unwrap();
        fs::write(site_dir.join("integrity.json"), manifest_json).unwrap();

        let result = check_integrity(site_dir, false);
        assert!(!result.passed, "Path should be blocked: {rel_path}");
        assert!(
            result
                .details
                .as_ref()
                .map(|d| d.contains("security violation"))
                .unwrap_or(false),
            "Should mention security violation"
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_collect_all_files_lists_symlink_without_recursing() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();

        fs::write(temp.path().join("root.txt"), "root").unwrap();
        fs::create_dir_all(outside.path().join("nested")).unwrap();
        fs::write(outside.path().join("nested/hidden.txt"), "hidden").unwrap();
        symlink(
            outside.path().join("nested"),
            temp.path().join("linked-dir"),
        )
        .unwrap();

        let files = collect_all_files(temp.path()).unwrap();
        assert!(files.contains(&"root.txt".to_string()));
        assert!(files.contains(&"linked-dir".to_string()));
        assert!(!files.iter().any(|f| f.starts_with("linked-dir/")));
    }

    #[test]
    #[cfg(unix)]
    fn test_calculate_dir_size_skips_symlink_targets() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();

        fs::write(temp.path().join("small.txt"), vec![0u8; 8]).unwrap();
        fs::write(outside.path().join("large.bin"), vec![0u8; 8192]).unwrap();
        symlink(
            outside.path().join("large.bin"),
            temp.path().join("linked.bin"),
        )
        .unwrap();

        let size = calculate_dir_size(temp.path()).unwrap();
        assert_eq!(size, 8);
    }

    #[test]
    #[cfg(unix)]
    fn test_integrity_rejects_symlink_manifest_entry_to_directory() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let site_dir = temp.path();
        fs::create_dir_all(site_dir.join("payload/real-dir")).unwrap();
        fs::write(site_dir.join("payload/real-dir/content.txt"), b"payload").unwrap();
        symlink(
            site_dir.join("payload/real-dir"),
            site_dir.join("payload/alias-dir"),
        )
        .unwrap();

        let mut files = BTreeMap::new();
        files.insert(
            "payload/alias-dir".to_string(),
            IntegrityEntry {
                // Hash/size are irrelevant here; verification should fail before hashing.
                sha256: "deadbeef".repeat(8),
                size: 0,
            },
        );
        let manifest = IntegrityManifest {
            version: 1,
            generated_at: "2025-01-01T00:00:00Z".to_string(),
            files,
        };
        fs::write(
            site_dir.join("integrity.json"),
            serde_json::to_string(&manifest).unwrap(),
        )
        .unwrap();

        let result = check_integrity(site_dir, false);
        assert!(!result.passed);
        assert!(
            result
                .details
                .as_ref()
                .map(|d| d.contains("not a regular file"))
                .unwrap_or(false)
        );
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
    fn test_verify_rejects_required_file_replaced_by_directory() {
        let temp = TempDir::new().unwrap();
        let site_dir = temp.path().join("site");
        let viewer_backup = temp.path().join("viewer.js.backup");

        copy_fixture("valid", &site_dir).unwrap();
        fs::rename(site_dir.join("viewer.js"), &viewer_backup).unwrap();
        fs::create_dir(site_dir.join("viewer.js")).unwrap();

        let mut manifest: IntegrityManifest = serde_json::from_reader(BufReader::new(
            File::open(site_dir.join("integrity.json")).unwrap(),
        ))
        .unwrap();
        manifest.files.remove("viewer.js");
        fs::write(
            site_dir.join("integrity.json"),
            serde_json::to_string(&manifest).unwrap(),
        )
        .unwrap();

        let result = verify_bundle(&site_dir, false).unwrap();
        assert_eq!(result.status, "invalid");
        assert!(!result.checks.required_files.passed);
        assert!(
            result
                .checks
                .required_files
                .details
                .as_ref()
                .map(|details| details.contains("viewer.js (must be a regular file)"))
                .unwrap_or(false),
            "required file directories should be rejected: {:?}",
            result.checks.required_files.details
        );
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
    fn test_verify_rejects_unknown_config_fields() {
        let temp = TempDir::new().unwrap();
        let site_dir = temp.path().join("site");

        copy_fixture("valid", &site_dir).unwrap();
        fs::write(
            site_dir.join("config.json"),
            r#"{
                "encrypted": false,
                "version": "1.0",
                "payload": {
                    "path": "payload/data.sqlite",
                    "format": "sqlite"
                },
                "totally_unknown_field": 123
            }"#,
        )
        .unwrap();

        let result = verify_bundle(&site_dir, false).unwrap();
        assert!(!result.checks.config_schema.passed);
        assert!(
            result
                .checks
                .config_schema
                .details
                .as_ref()
                .map(|details| details.contains("unknown field"))
                .unwrap_or(false),
            "unknown config fields should fail schema validation: {:?}",
            result.checks.config_schema.details
        );
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
    fn test_check_no_secrets_flags_nested_config_secret_key_with_whitespace() {
        let temp = TempDir::new().unwrap();
        let site_dir = temp.path().join("site");
        fs::create_dir_all(&site_dir).unwrap();
        fs::write(
            site_dir.join("config.json"),
            r#"{
                "encrypted": false,
                "version": "1.0",
                "payload": { "path": "payload/data.sqlite", "format": "sqlite" },
                "metadata": { "secret" : "leaked" }
            }"#,
        )
        .unwrap();

        let result = check_no_secrets(&site_dir);
        assert!(!result.passed);
        assert!(
            result
                .details
                .as_ref()
                .map(|details| {
                    details.contains(
                        "config.json contains forbidden field: secret field at metadata.secret",
                    )
                })
                .unwrap_or(false),
            "nested secret key with whitespace should be detected: {:?}",
            result.details
        );
    }

    #[test]
    fn test_check_no_secrets_flags_forbidden_config_key_inside_array() {
        let temp = TempDir::new().unwrap();
        let site_dir = temp.path().join("site");
        fs::create_dir_all(&site_dir).unwrap();
        fs::write(
            site_dir.join("config.json"),
            r#"{
                "encrypted": false,
                "version": "1.0",
                "payload": { "path": "payload/data.sqlite", "format": "sqlite" },
                "metadata": [{ "private_key" : "leaked" }]
            }"#,
        )
        .unwrap();

        let result = check_no_secrets(&site_dir);
        assert!(!result.passed);
        assert!(
            result
                .details
                .as_ref()
                .map(|details| {
                    details.contains(
                        "config.json contains forbidden field: private_key field at metadata[0].private_key",
                    )
                })
                .unwrap_or(false),
            "forbidden key inside arrays should be detected: {:?}",
            result.details
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_check_no_secrets_does_not_follow_symlinked_directories() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let site_dir = temp.path().join("site");
        let outside_dir = temp.path().join("outside");
        fs::create_dir_all(&site_dir).unwrap();
        fs::create_dir_all(outside_dir.join("private")).unwrap();
        fs::write(outside_dir.join("private/recovery-secret.txt"), "secret").unwrap();
        symlink(&outside_dir, site_dir.join("linked-assets")).unwrap();

        let result = check_no_secrets(&site_dir);
        assert!(
            result.passed,
            "symlink targets outside site/ should not be scanned as in-tree secrets: {:?}",
            result.details
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_check_no_secrets_flags_secret_named_symlink_without_recursing() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let site_dir = temp.path().join("site");
        let benign_dir = temp.path().join("benign");
        fs::create_dir_all(site_dir.join("nested")).unwrap();
        fs::create_dir_all(&benign_dir).unwrap();
        symlink(&benign_dir, site_dir.join("nested/private")).unwrap();

        let result = check_no_secrets(&site_dir);
        assert!(!result.passed);
        assert!(
            result
                .details
                .as_ref()
                .map(|details| {
                    details.contains("Secret directory found in site subdirectory: nested/private/")
                })
                .unwrap_or(false),
            "secret-named symlink should still be reported: {:?}",
            result.details
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_check_no_secrets_flags_top_level_secret_file_broken_symlink() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let site_dir = temp.path().join("site");
        fs::create_dir_all(&site_dir).unwrap();
        symlink(
            temp.path().join("missing-recovery-secret"),
            site_dir.join("recovery-secret.txt"),
        )
        .unwrap();

        let result = check_no_secrets(&site_dir);
        assert!(!result.passed);
        assert!(
            result
                .details
                .as_ref()
                .map(|details| details.contains("Secret file found in site/: recovery-secret.txt"))
                .unwrap_or(false),
            "top-level dangling secret symlink should still be reported: {:?}",
            result.details
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_check_no_secrets_flags_top_level_secret_dir_broken_symlink() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let site_dir = temp.path().join("site");
        fs::create_dir_all(&site_dir).unwrap();
        symlink(
            temp.path().join("missing-private"),
            site_dir.join("private"),
        )
        .unwrap();

        let result = check_no_secrets(&site_dir);
        assert!(!result.passed);
        assert!(
            result
                .details
                .as_ref()
                .map(|details| details.contains("Secret directory found in site/: private/"))
                .unwrap_or(false),
            "top-level dangling private symlink should still be reported: {:?}",
            result.details
        );
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
        let details = result.checks.integrity.details.as_ref().unwrap();
        assert!(
            details.contains("Size mismatch") || details.contains("Hash mismatch"),
            "expected size or hash mismatch, got: {details}"
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
    #[cfg(unix)]
    fn test_resolve_site_dir_rejects_symlinked_site_directory() {
        use std::os::unix::fs::symlink;

        let bundle_root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let outside_site = outside.path().join("site");
        fs::create_dir_all(&outside_site).unwrap();
        fs::write(outside_site.join("index.html"), "<html></html>").unwrap();
        symlink(&outside_site, bundle_root.path().join("site")).unwrap();

        let err = resolve_site_dir(bundle_root.path())
            .unwrap_err()
            .to_string();
        assert!(err.contains("must not be a symlink"));

        let direct_err = resolve_site_dir(&bundle_root.path().join("site"))
            .unwrap_err()
            .to_string();
        assert!(direct_err.contains("must not be a symlink"));
    }

    #[test]
    fn test_chunk_size_limit() {
        let temp = TempDir::new().unwrap();
        let site_dir = temp.path();
        let payload_dir = site_dir.join("payload");
        fs::create_dir_all(&payload_dir).unwrap();

        // Create config.json for encrypted archive (required by check_size_limits)
        let config = r#"{
          "version": 2,
          "export_id": "AAAAAAAAAAAAAAAAAAAAAA==",
          "base_nonce": "AAAAAAAAAAAAAAAA",
          "compression": "deflate",
          "kdf_defaults": { "memory_kb": 65536, "iterations": 3, "parallelism": 4 },
          "payload": {
            "chunk_size": 1024,
            "chunk_count": 1,
            "total_compressed_size": 14,
            "total_plaintext_size": 100,
            "files": ["payload/chunk-00000.bin"]
          },
          "key_slots": [{
            "id": 0,
            "slot_type": "password",
            "kdf": "argon2id",
            "salt": "AAAAAAAAAAAAAAAAAAAAAA==",
            "wrapped_dek": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "nonce": "AAAAAAAAAAAAAAAA",
            "argon2_params": { "memory_kb": 65536, "iterations": 3, "parallelism": 4 }
          }]
        }"#;
        fs::write(site_dir.join("config.json"), config).unwrap();

        // Create a small file (should pass)
        fs::write(payload_dir.join("chunk-00000.bin"), "small").unwrap();

        let result = check_size_limits(site_dir);
        assert!(result.passed);
    }

    #[test]
    fn test_payload_manifest_rejects_unexpected_high_chunk_index() {
        let temp = TempDir::new().unwrap();
        let site_dir = temp.path();
        let payload_dir = site_dir.join("payload");
        fs::create_dir_all(&payload_dir).unwrap();

        let config = r#"{
          "version": 2,
          "export_id": "AAAAAAAAAAAAAAAAAAAAAA==",
          "base_nonce": "AAAAAAAAAAAAAAAA",
          "compression": "deflate",
          "kdf_defaults": { "memory_kb": 65536, "iterations": 3, "parallelism": 4 },
          "payload": {
            "chunk_size": 1024,
            "chunk_count": 1,
            "total_compressed_size": 14,
            "total_plaintext_size": 100,
            "files": ["payload/chunk-00000.bin"]
          },
          "key_slots": [{
            "id": 0,
            "slot_type": "password",
            "kdf": "argon2id",
            "salt": "AAAAAAAAAAAAAAAAAAAAAA==",
            "wrapped_dek": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "nonce": "AAAAAAAAAAAAAAAA",
            "argon2_params": { "memory_kb": 65536, "iterations": 3, "parallelism": 4 }
          }]
        }"#;
        fs::write(site_dir.join("config.json"), config).unwrap();

        fs::write(payload_dir.join("chunk-00000.bin"), "small").unwrap();
        fs::write(payload_dir.join("chunk-99999.bin"), "unexpected").unwrap();

        let result = check_payload_manifest(site_dir);
        assert!(!result.passed);
        let details = result.details.unwrap_or_default();
        assert!(details.contains("Unexpected chunk file index: chunk-99999.bin"));
    }

    #[test]
    fn test_payload_manifest_rejects_non_file_chunk_entry() {
        let temp = TempDir::new().unwrap();
        let site_dir = temp.path();
        let payload_dir = site_dir.join("payload");
        fs::create_dir_all(&payload_dir).unwrap();

        let config = r#"{
          "version": 2,
          "export_id": "AAAAAAAAAAAAAAAAAAAAAA==",
          "base_nonce": "AAAAAAAAAAAAAAAA",
          "compression": "deflate",
          "kdf_defaults": { "memory_kb": 65536, "iterations": 3, "parallelism": 4 },
          "payload": {
            "chunk_size": 1024,
            "chunk_count": 1,
            "total_compressed_size": 14,
            "total_plaintext_size": 100,
            "files": ["payload/chunk-00000.bin"]
          },
          "key_slots": [{
            "id": 0,
            "slot_type": "password",
            "kdf": "argon2id",
            "salt": "AAAAAAAAAAAAAAAAAAAAAA==",
            "wrapped_dek": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "nonce": "AAAAAAAAAAAAAAAA",
            "argon2_params": { "memory_kb": 65536, "iterations": 3, "parallelism": 4 }
          }]
        }"#;
        fs::write(site_dir.join("config.json"), config).unwrap();
        fs::create_dir_all(payload_dir.join("chunk-00000.bin")).unwrap();

        let result = check_payload_manifest(site_dir);
        assert!(!result.passed);
        assert!(
            result
                .details
                .as_ref()
                .map(|d| d.contains("payload/chunk-00000.bin must be a regular file"))
                .unwrap_or(false)
        );
    }

    #[test]
    fn test_payload_manifest_rejects_malformed_chunk_filename() {
        let temp = TempDir::new().unwrap();
        let site_dir = temp.path();
        let payload_dir = site_dir.join("payload");
        fs::create_dir_all(&payload_dir).unwrap();

        let config = r#"{
          "version": 2,
          "export_id": "AAAAAAAAAAAAAAAAAAAAAA==",
          "base_nonce": "AAAAAAAAAAAAAAAA",
          "compression": "deflate",
          "kdf_defaults": { "memory_kb": 65536, "iterations": 3, "parallelism": 4 },
          "payload": {
            "chunk_size": 1024,
            "chunk_count": 1,
            "total_compressed_size": 14,
            "total_plaintext_size": 100,
            "files": ["payload/chunk-00000.bin"]
          },
          "key_slots": [{
            "id": 0,
            "slot_type": "password",
            "kdf": "argon2id",
            "salt": "AAAAAAAAAAAAAAAAAAAAAA==",
            "wrapped_dek": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "nonce": "AAAAAAAAAAAAAAAA",
            "argon2_params": { "memory_kb": 65536, "iterations": 3, "parallelism": 4 }
          }]
        }"#;
        fs::write(site_dir.join("config.json"), config).unwrap();
        fs::write(payload_dir.join("chunk-00000.bin"), "small").unwrap();
        fs::write(payload_dir.join("chunk-1.bin"), "malformed").unwrap();

        let result = check_payload_manifest(site_dir);
        assert!(!result.passed);
        assert!(
            result
                .details
                .as_ref()
                .map(|d| d.contains("Malformed chunk filename: chunk-1.bin"))
                .unwrap_or(false)
        );
    }

    #[test]
    fn test_payload_manifest_treats_six_digit_chunk_name_as_unexpected_not_malformed() {
        let temp = TempDir::new().unwrap();
        let site_dir = temp.path();
        let payload_dir = site_dir.join("payload");
        fs::create_dir_all(&payload_dir).unwrap();

        let config = r#"{
          "version": 2,
          "export_id": "AAAAAAAAAAAAAAAAAAAAAA==",
          "base_nonce": "AAAAAAAAAAAAAAAA",
          "compression": "deflate",
          "kdf_defaults": { "memory_kb": 65536, "iterations": 3, "parallelism": 4 },
          "payload": {
            "chunk_size": 1024,
            "chunk_count": 1,
            "total_compressed_size": 14,
            "total_plaintext_size": 100,
            "files": ["payload/chunk-00000.bin"]
          },
          "key_slots": [{
            "id": 0,
            "slot_type": "password",
            "kdf": "argon2id",
            "salt": "AAAAAAAAAAAAAAAAAAAAAA==",
            "wrapped_dek": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "nonce": "AAAAAAAAAAAAAAAA",
            "argon2_params": { "memory_kb": 65536, "iterations": 3, "parallelism": 4 }
          }]
        }"#;
        fs::write(site_dir.join("config.json"), config).unwrap();
        fs::write(payload_dir.join("chunk-00000.bin"), "small").unwrap();
        fs::write(payload_dir.join("chunk-100000.bin"), "unexpected").unwrap();

        let result = check_payload_manifest(site_dir);
        assert!(!result.passed);
        let details = result.details.unwrap_or_default();
        assert!(details.contains("Unexpected chunk file index: chunk-100000.bin"));
        assert!(!details.contains("Malformed chunk filename: chunk-100000.bin"));
    }

    #[test]
    fn test_unencrypted_payload_must_be_regular_file() {
        let temp = TempDir::new().unwrap();
        let site_dir = temp.path();
        let payload_dir = site_dir.join("payload");
        fs::create_dir_all(&payload_dir).unwrap();
        fs::create_dir_all(payload_dir.join("data.sqlite")).unwrap();

        let config = r#"{
          "encrypted": false,
          "version": "1.0",
          "payload": {
            "path": "payload/data.sqlite",
            "format": "sqlite"
          }
        }"#;
        fs::write(site_dir.join("config.json"), config).unwrap();

        let manifest_result = check_payload_manifest(site_dir);
        assert!(!manifest_result.passed);
        assert!(
            manifest_result
                .details
                .as_ref()
                .map(|d| d.contains("payload/data.sqlite must be a regular file"))
                .unwrap_or(false)
        );

        let size_result = check_size_limits(site_dir);
        assert!(!size_result.passed);
        assert!(
            size_result
                .details
                .as_ref()
                .map(|d| d.contains("payload/data.sqlite must be a regular file"))
                .unwrap_or(false)
        );
    }

    #[test]
    fn test_size_limits_rejects_non_file_chunk_entry() {
        let temp = TempDir::new().unwrap();
        let site_dir = temp.path();
        let payload_dir = site_dir.join("payload");
        fs::create_dir_all(&payload_dir).unwrap();

        let config = r#"{
          "version": 2,
          "export_id": "AAAAAAAAAAAAAAAAAAAAAA==",
          "base_nonce": "AAAAAAAAAAAAAAAA",
          "compression": "deflate",
          "kdf_defaults": { "memory_kb": 65536, "iterations": 3, "parallelism": 4 },
          "payload": {
            "chunk_size": 1024,
            "chunk_count": 1,
            "total_compressed_size": 14,
            "total_plaintext_size": 100,
            "files": ["payload/chunk-00000.bin"]
          },
          "key_slots": [{
            "id": 0,
            "slot_type": "password",
            "kdf": "argon2id",
            "salt": "AAAAAAAAAAAAAAAAAAAAAA==",
            "wrapped_dek": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "nonce": "AAAAAAAAAAAAAAAA",
            "argon2_params": { "memory_kb": 65536, "iterations": 3, "parallelism": 4 }
          }]
        }"#;
        fs::write(site_dir.join("config.json"), config).unwrap();
        fs::create_dir_all(payload_dir.join("chunk-00000.bin")).unwrap();

        let result = check_size_limits(site_dir);
        assert!(!result.passed);
        assert!(
            result
                .details
                .as_ref()
                .map(|d| d.contains("chunk-00000.bin must be a regular file"))
                .unwrap_or(false)
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_size_limits_rejects_symlinked_chunk() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let site_dir = temp.path();
        let payload_dir = site_dir.join("payload");
        fs::create_dir_all(&payload_dir).unwrap();

        let config = r#"{
          "version": 2,
          "export_id": "AAAAAAAAAAAAAAAAAAAAAA==",
          "base_nonce": "AAAAAAAAAAAAAAAA",
          "compression": "deflate",
          "kdf_defaults": { "memory_kb": 65536, "iterations": 3, "parallelism": 4 },
          "payload": {
            "chunk_size": 1024,
            "chunk_count": 1,
            "total_compressed_size": 14,
            "total_plaintext_size": 100,
            "files": ["payload/chunk-00000.bin"]
          },
          "key_slots": [{
            "id": 0,
            "slot_type": "password",
            "kdf": "argon2id",
            "salt": "AAAAAAAAAAAAAAAAAAAAAA==",
            "wrapped_dek": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "nonce": "AAAAAAAAAAAAAAAA",
            "argon2_params": { "memory_kb": 65536, "iterations": 3, "parallelism": 4 }
          }]
        }"#;
        fs::write(site_dir.join("config.json"), config).unwrap();

        fs::write(outside.path().join("chunk-00000.bin"), "external").unwrap();
        symlink(
            outside.path().join("chunk-00000.bin"),
            payload_dir.join("chunk-00000.bin"),
        )
        .unwrap();

        let result = check_size_limits(site_dir);
        assert!(!result.passed);
        assert!(
            result
                .details
                .as_ref()
                .map(|d| d.contains("must not be a symlink"))
                .unwrap_or(false)
        );
    }

    #[test]
    fn test_integrity_path_traversal_blocked() {
        use std::collections::BTreeMap;

        let temp = TempDir::new().unwrap();
        let site_dir = temp.path();

        // Create integrity.json with path traversal attempt
        let mut files = BTreeMap::new();
        files.insert(
            "../../../etc/passwd".to_string(),
            crate::pages::bundle::IntegrityEntry {
                sha256: "deadbeef".repeat(8),
                size: 100,
            },
        );
        let manifest = IntegrityManifest {
            version: 1,
            generated_at: "2025-01-01T00:00:00Z".to_string(),
            files,
        };
        let manifest_json = serde_json::to_string(&manifest).unwrap();
        fs::write(site_dir.join("integrity.json"), manifest_json).unwrap();

        // Verify the check catches the path traversal
        let result = check_integrity(site_dir, false);
        assert!(!result.passed, "Path traversal should be blocked");
        assert!(
            result
                .details
                .as_ref()
                .map(|d| d.contains("security violation"))
                .unwrap_or(false),
            "Should mention security violation"
        );
    }

    #[test]
    fn test_integrity_absolute_path_blocked() {
        use std::collections::BTreeMap;

        let temp = TempDir::new().unwrap();
        let site_dir = temp.path();

        // Create integrity.json with absolute path
        let mut files = BTreeMap::new();
        files.insert(
            "/etc/passwd".to_string(),
            crate::pages::bundle::IntegrityEntry {
                sha256: "deadbeef".repeat(8),
                size: 100,
            },
        );
        let manifest = IntegrityManifest {
            version: 1,
            generated_at: "2025-01-01T00:00:00Z".to_string(),
            files,
        };
        let manifest_json = serde_json::to_string(&manifest).unwrap();
        fs::write(site_dir.join("integrity.json"), manifest_json).unwrap();

        // Verify the check catches the absolute path
        let result = check_integrity(site_dir, false);
        assert!(!result.passed, "Absolute path should be blocked");
        assert!(
            result
                .details
                .as_ref()
                .map(|d| d.contains("security violation"))
                .unwrap_or(false),
            "Should mention security violation"
        );
    }

    #[test]
    fn test_integrity_url_encoded_traversal_blocked_single() {
        assert_integrity_path_blocked("%2e%2e/%2e%2e/etc/passwd");
    }

    #[test]
    fn test_integrity_url_encoded_traversal_blocked_double() {
        assert_integrity_path_blocked("%252e%252e/%252e%252e/etc/passwd");
    }

    #[test]
    fn test_integrity_url_encoded_traversal_blocked_mixed() {
        assert_integrity_path_blocked("%2e./etc/passwd");
        assert_integrity_path_blocked(".%2e/etc/passwd");
        assert_integrity_path_blocked("..%2fetc/passwd");
    }

    #[test]
    fn test_integrity_url_encoded_traversal_blocked_uppercase() {
        assert_integrity_path_blocked("%2E%2E/%2Fetc/passwd");
    }

    #[test]
    fn test_integrity_url_encoded_traversal_blocked_overlong_utf8() {
        assert_integrity_path_blocked("%c0%ae%c0%ae/%c0%ae%c0%ae/etc/passwd");
    }

    #[test]
    fn test_integrity_url_encoded_traversal_blocked_null_byte() {
        assert_integrity_path_blocked("valid%00/../etc/passwd");
    }

    #[test]
    fn test_integrity_url_encoded_traversal_blocked_backslash() {
        assert_integrity_path_blocked("..\\..\\etc\\passwd");
        assert_integrity_path_blocked("..%5c..%5cetc%5cpasswd");
    }

    #[test]
    fn test_integrity_url_encoded_traversal_blocked_separator_confusion() {
        assert_integrity_path_blocked(r"..\/..\/etc\/passwd");
    }

    // --- Unicode normalization attack tests ---

    #[test]
    fn test_integrity_unicode_fullwidth_dots_blocked() {
        // U+FF0E FULLWIDTH FULL STOP looks like '.' but is a different codepoint.
        // Two fullwidth dots form a visual ".." that bypasses naive ASCII checks.
        assert_integrity_path_blocked("\u{FF0E}\u{FF0E}/etc/passwd");
    }

    #[test]
    fn test_integrity_unicode_fullwidth_slash_blocked() {
        // U+FF0F FULLWIDTH SOLIDUS looks like '/' but is a different codepoint.
        assert_integrity_path_blocked("payload\u{FF0F}..\\..\\etc\\passwd");
    }

    #[test]
    fn test_integrity_unicode_fullwidth_backslash_blocked() {
        // U+FF3C FULLWIDTH REVERSE SOLIDUS looks like '\' but is a different codepoint.
        assert_integrity_path_blocked("payload\u{FF3C}..\\..\\etc\\passwd");
    }

    #[test]
    fn test_integrity_unicode_small_full_stop_blocked() {
        // U+FE52 SMALL FULL STOP - a compatibility variant of '.'
        assert_integrity_path_blocked("\u{FE52}\u{FE52}/etc/passwd");
    }

    #[test]
    fn test_integrity_unicode_one_dot_leader_blocked() {
        // U+2024 ONE DOT LEADER - looks nearly identical to '.'
        assert_integrity_path_blocked("\u{2024}\u{2024}/etc/passwd");
    }

    #[test]
    fn test_integrity_unicode_halfwidth_ideographic_full_stop_blocked() {
        // U+FF61 HALFWIDTH IDEOGRAPHIC FULL STOP
        assert_integrity_path_blocked("\u{FF61}\u{FF61}/etc/passwd");
    }

    #[test]
    fn test_integrity_unicode_mixed_fullwidth_and_ascii_blocked() {
        // Mix fullwidth and ASCII dots — the fullwidth char alone should trigger
        assert_integrity_path_blocked(".\u{FF0E}/etc/passwd");
        assert_integrity_path_blocked("\u{FF0E}./etc/passwd");
    }

    #[test]
    fn test_integrity_percent_encoded_unicode_fullwidth_dot_blocked() {
        // Percent-encoded UTF-8 for U+FF0E (FULLWIDTH FULL STOP): 0xEF 0xBC 0x8E
        assert_integrity_path_blocked("%ef%bc%8e%ef%bc%8e/etc/passwd");
    }

    // --- Case sensitivity / Windows path tests ---

    #[test]
    fn test_integrity_windows_drive_letter_blocked() {
        assert_integrity_path_blocked("C:\\Windows\\System32\\config\\SAM");
    }

    #[test]
    fn test_integrity_windows_drive_letter_lowercase_blocked() {
        assert_integrity_path_blocked("c:\\windows\\system32");
    }

    #[test]
    fn test_integrity_windows_drive_letter_forward_slash_blocked() {
        assert_integrity_path_blocked("C:/Windows/System32");
    }

    #[test]
    fn test_integrity_windows_unc_path_blocked() {
        // UNC paths start with \\ — should be caught as absolute
        assert_integrity_path_blocked("\\\\server\\share\\file.txt");
    }

    // --- Symlink traversal tests ---

    #[test]
    #[cfg(unix)]
    fn test_integrity_symlink_traversal_blocked() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let site_dir = temp.path();

        // Create a target file outside the site directory
        let outside_dir = TempDir::new().unwrap();
        let secret_file = outside_dir.path().join("secret.txt");
        fs::write(&secret_file, "sensitive data").unwrap();

        // Create a symlink inside the site directory that points outside
        let link_path = site_dir.join("evil_link.txt");
        symlink(&secret_file, &link_path).unwrap();

        // Compute hash of the file the symlink points to
        let hash = compute_file_hash(&link_path).unwrap();
        let size = fs::metadata(&link_path).unwrap().len();

        let mut files = BTreeMap::new();
        files.insert(
            "evil_link.txt".to_string(),
            IntegrityEntry { sha256: hash, size },
        );
        let manifest = IntegrityManifest {
            version: 1,
            generated_at: "2025-01-01T00:00:00Z".to_string(),
            files,
        };
        fs::write(
            site_dir.join("integrity.json"),
            serde_json::to_string(&manifest).unwrap(),
        )
        .unwrap();

        // The canonicalize check should detect the symlink escapes site_dir
        let result = check_integrity(site_dir, false);
        assert!(
            !result.passed,
            "Symlink traversal outside site_dir should be blocked"
        );
        assert!(
            result
                .details
                .as_ref()
                .map(|d| d.contains("security violation"))
                .unwrap_or(false),
            "Should mention security violation for symlink escape"
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_integrity_symlink_within_site_dir_allowed() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let site_dir = temp.path();

        // Create a real file inside site_dir
        let real_file = site_dir.join("real.txt");
        fs::write(&real_file, "legitimate data").unwrap();

        // Create a symlink that points to a file inside site_dir
        let link_path = site_dir.join("link_to_real.txt");
        symlink(&real_file, &link_path).unwrap();

        let hash = compute_file_hash(&link_path).unwrap();
        let size = fs::metadata(&link_path).unwrap().len();

        let mut files = BTreeMap::new();
        files.insert(
            "link_to_real.txt".to_string(),
            IntegrityEntry { sha256: hash, size },
        );
        // Also include the real file and integrity.json in manifest
        let real_hash = compute_file_hash(&real_file).unwrap();
        let real_size = fs::metadata(&real_file).unwrap().len();
        files.insert(
            "real.txt".to_string(),
            IntegrityEntry {
                sha256: real_hash,
                size: real_size,
            },
        );

        let manifest = IntegrityManifest {
            version: 1,
            generated_at: "2025-01-01T00:00:00Z".to_string(),
            files,
        };
        fs::write(
            site_dir.join("integrity.json"),
            serde_json::to_string(&manifest).unwrap(),
        )
        .unwrap();

        // Symlink within site_dir should be OK
        let result = check_integrity(site_dir, false);
        assert!(
            result.passed,
            "Symlink within site_dir should be allowed: {:?}",
            result.details
        );
    }

    // --- False positive tests: legitimate paths should NOT be blocked ---

    #[test]
    fn test_integrity_legitimate_dotted_version_not_blocked() {
        // "v2.1.0" contains dots but they're version numbers, not traversal
        let temp = TempDir::new().unwrap();
        let site_dir = temp.path();
        let target = site_dir.join("assets/v2.1.0/bundle.js");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, "// bundle").unwrap();

        let hash = compute_file_hash(&target).unwrap();
        let size = fs::metadata(&target).unwrap().len();
        let mut files = BTreeMap::new();
        files.insert(
            "assets/v2.1.0/bundle.js".to_string(),
            IntegrityEntry { sha256: hash, size },
        );

        let manifest = IntegrityManifest {
            version: 1,
            generated_at: "2025-01-01T00:00:00Z".to_string(),
            files,
        };
        fs::write(
            site_dir.join("integrity.json"),
            serde_json::to_string(&manifest).unwrap(),
        )
        .unwrap();

        let result = check_integrity(site_dir, false);
        assert!(
            result.passed,
            "Dotted version path should not be blocked: {:?}",
            result.details
        );
    }

    #[test]
    fn test_integrity_legitimate_hidden_file_not_blocked() {
        // ".nojekyll" starts with a dot — should not be confused with traversal
        let temp = TempDir::new().unwrap();
        let site_dir = temp.path();
        let target = site_dir.join(".nojekyll");
        fs::write(&target, "").unwrap();

        let hash = compute_file_hash(&target).unwrap();
        let size = fs::metadata(&target).unwrap().len();
        let mut files = BTreeMap::new();
        files.insert(
            ".nojekyll".to_string(),
            IntegrityEntry { sha256: hash, size },
        );

        let manifest = IntegrityManifest {
            version: 1,
            generated_at: "2025-01-01T00:00:00Z".to_string(),
            files,
        };
        fs::write(
            site_dir.join("integrity.json"),
            serde_json::to_string(&manifest).unwrap(),
        )
        .unwrap();

        let result = check_integrity(site_dir, false);
        assert!(
            result.passed,
            "Hidden file (.nojekyll) should not be blocked: {:?}",
            result.details
        );
    }

    #[test]
    fn test_integrity_legitimate_payload_subdir_not_blocked() {
        let temp = TempDir::new().unwrap();
        let site_dir = temp.path();
        let target = site_dir.join("payload/data/sessions.db");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, "sqlite").unwrap();

        let hash = compute_file_hash(&target).unwrap();
        let size = fs::metadata(&target).unwrap().len();
        let mut files = BTreeMap::new();
        files.insert(
            "payload/data/sessions.db".to_string(),
            IntegrityEntry { sha256: hash, size },
        );

        let manifest = IntegrityManifest {
            version: 1,
            generated_at: "2025-01-01T00:00:00Z".to_string(),
            files,
        };
        fs::write(
            site_dir.join("integrity.json"),
            serde_json::to_string(&manifest).unwrap(),
        )
        .unwrap();

        let result = check_integrity(site_dir, false);
        assert!(
            result.passed,
            "Legitimate payload subdirectory should not be blocked: {:?}",
            result.details
        );
    }

    #[test]
    fn test_integrity_legitimate_hyphens_underscores_not_blocked() {
        let temp = TempDir::new().unwrap();
        let site_dir = temp.path();
        let target = site_dir.join("css/main-v2_final.css");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, "body{}").unwrap();

        let hash = compute_file_hash(&target).unwrap();
        let size = fs::metadata(&target).unwrap().len();
        let mut files = BTreeMap::new();
        files.insert(
            "css/main-v2_final.css".to_string(),
            IntegrityEntry { sha256: hash, size },
        );

        let manifest = IntegrityManifest {
            version: 1,
            generated_at: "2025-01-01T00:00:00Z".to_string(),
            files,
        };
        fs::write(
            site_dir.join("integrity.json"),
            serde_json::to_string(&manifest).unwrap(),
        )
        .unwrap();

        let result = check_integrity(site_dir, false);
        assert!(
            result.passed,
            "Path with hyphens/underscores should not be blocked: {:?}",
            result.details
        );
    }

    // --- Unit tests for helper functions ---

    #[test]
    fn test_contains_unicode_path_attack_detects_fullwidth_period() {
        assert!(contains_unicode_path_attack("\u{FF0E}"));
        assert!(contains_unicode_path_attack("foo\u{FF0E}bar"));
    }

    #[test]
    fn test_contains_unicode_path_attack_detects_fullwidth_solidus() {
        assert!(contains_unicode_path_attack("\u{FF0F}"));
    }

    #[test]
    fn test_contains_unicode_path_attack_detects_fullwidth_reverse_solidus() {
        assert!(contains_unicode_path_attack("\u{FF3C}"));
    }

    #[test]
    fn test_contains_unicode_path_attack_detects_small_full_stop() {
        assert!(contains_unicode_path_attack("\u{FE52}"));
    }

    #[test]
    fn test_contains_unicode_path_attack_detects_one_dot_leader() {
        assert!(contains_unicode_path_attack("\u{2024}"));
    }

    #[test]
    fn test_contains_unicode_path_attack_allows_ascii() {
        assert!(!contains_unicode_path_attack("payload/chunk-00000.bin"));
        assert!(!contains_unicode_path_attack("../etc/passwd")); // traversal, but ASCII
        assert!(!contains_unicode_path_attack(".nojekyll"));
    }

    #[test]
    fn test_detect_encoded_path_violation_unicode_attack() {
        let result = detect_encoded_path_violation("\u{FF0E}\u{FF0E}/etc/passwd");
        assert_eq!(result, Some("unicode normalization attack".to_string()));
    }

    #[test]
    fn test_detect_encoded_path_violation_percent_encoded_unicode() {
        // %EF%BC%8E = UTF-8 encoding of U+FF0E (FULLWIDTH FULL STOP)
        let result = detect_encoded_path_violation("%ef%bc%8e%ef%bc%8e/etc/passwd");
        assert_eq!(
            result,
            Some("url-encoded unicode normalization attack".to_string())
        );
    }

    // --- Additional Unicode normalization attack tests (coding_agent_session_search-13za) ---

    #[test]
    fn test_integrity_unicode_combining_long_solidus_overlay_blocked() {
        // U+0338 COMBINING LONG SOLIDUS OVERLAY - could visually disguise characters
        assert_integrity_path_blocked(".\u{0338}./etc/passwd");
    }

    #[test]
    fn test_integrity_unicode_combining_short_solidus_overlay_blocked() {
        // U+0337 COMBINING SHORT SOLIDUS OVERLAY
        assert_integrity_path_blocked(".\u{0337}./etc/passwd");
    }

    #[test]
    fn test_integrity_unicode_rtl_override_blocked() {
        // U+202E RIGHT-TO-LEFT OVERRIDE - can visually reverse path display
        // This could make "etc/passwd/../" appear as a safe path when it's actually traversal
        assert_integrity_path_blocked("etc/passwd/\u{202E}../");
    }

    #[test]
    fn test_integrity_unicode_ltr_override_blocked() {
        // U+202D LEFT-TO-RIGHT OVERRIDE - directional override
        assert_integrity_path_blocked("\u{202D}../etc/passwd");
    }

    #[test]
    fn test_integrity_unicode_rtl_embedding_blocked() {
        // U+202B RIGHT-TO-LEFT EMBEDDING
        assert_integrity_path_blocked("\u{202B}../etc/passwd");
    }

    #[test]
    fn test_integrity_unicode_rtl_isolate_blocked() {
        // U+2067 RIGHT-TO-LEFT ISOLATE
        assert_integrity_path_blocked("\u{2067}../etc/passwd");
    }

    #[test]
    fn test_integrity_unicode_zero_width_joiner_blocked() {
        // U+200D ZERO WIDTH JOINER - invisible character that could split tokens
        assert_integrity_path_blocked(".\u{200D}./etc/passwd");
    }

    #[test]
    fn test_integrity_unicode_zero_width_non_joiner_blocked() {
        // U+200C ZERO WIDTH NON-JOINER
        assert_integrity_path_blocked(".\u{200C}./etc/passwd");
    }

    #[test]
    fn test_integrity_unicode_zero_width_space_blocked() {
        // U+200B ZERO WIDTH SPACE - invisible character
        assert_integrity_path_blocked("..\u{200B}/etc/passwd");
    }

    #[test]
    fn test_integrity_unicode_bom_blocked() {
        // U+FEFF BYTE ORDER MARK (ZERO WIDTH NO-BREAK SPACE)
        assert_integrity_path_blocked("\u{FEFF}../etc/passwd");
    }

    #[test]
    fn test_integrity_unicode_fraction_slash_blocked() {
        // U+2044 FRACTION SLASH - visually similar to /
        assert_integrity_path_blocked("..\u{2044}etc\u{2044}passwd");
    }

    #[test]
    fn test_integrity_unicode_division_slash_blocked() {
        // U+2215 DIVISION SLASH - visually similar to /
        assert_integrity_path_blocked("..\u{2215}etc\u{2215}passwd");
    }

    #[test]
    fn test_integrity_unicode_big_solidus_blocked() {
        // U+29F8 BIG SOLIDUS - another slash look-alike
        assert_integrity_path_blocked("..\u{29F8}etc\u{29F8}passwd");
    }

    #[test]
    fn test_integrity_unicode_vai_full_stop_blocked() {
        // U+A60E VAI FULL STOP - dot look-alike
        assert_integrity_path_blocked("\u{A60E}\u{A60E}/etc/passwd");
    }

    #[test]
    fn test_integrity_unicode_syriac_full_stop_blocked() {
        // U+0701 SYRIAC SUPRALINEAR FULL STOP - dot look-alike
        assert_integrity_path_blocked("\u{0701}\u{0701}/etc/passwd");
    }

    // --- NFD/NFC normalization form tests ---

    #[test]
    fn test_integrity_unicode_nfd_decomposed_not_exploitable() {
        // NFD decomposition of certain characters could potentially be exploited
        // For example, some characters have canonical decompositions
        // This test verifies that legitimate paths with accented chars work
        let temp = TempDir::new().unwrap();
        let site_dir = temp.path();

        // Create a file with an accented filename (NFC form - precomposed)
        let target = site_dir.join("café.txt");
        fs::write(&target, "coffee").unwrap();

        let hash = compute_file_hash(&target).unwrap();
        let size = fs::metadata(&target).unwrap().len();
        let mut files = BTreeMap::new();
        files.insert(
            "café.txt".to_string(),
            IntegrityEntry { sha256: hash, size },
        );

        let manifest = IntegrityManifest {
            version: 1,
            generated_at: "2025-01-01T00:00:00Z".to_string(),
            files,
        };
        fs::write(
            site_dir.join("integrity.json"),
            serde_json::to_string(&manifest).unwrap(),
        )
        .unwrap();

        // Legitimate accented filenames should be allowed
        let result = check_integrity(site_dir, false);
        assert!(
            result.passed,
            "Legitimate accented filename should be allowed: {:?}",
            result.details
        );
    }

    // --- Unit tests for extended helper functions ---

    #[test]
    fn test_contains_unicode_path_attack_detects_combining_overlay() {
        assert!(contains_unicode_path_attack("\u{0338}")); // COMBINING LONG SOLIDUS OVERLAY
        assert!(contains_unicode_path_attack("\u{0337}")); // COMBINING SHORT SOLIDUS OVERLAY
    }

    #[test]
    fn test_contains_unicode_path_attack_detects_zero_width() {
        assert!(contains_unicode_path_attack("\u{200D}")); // ZERO WIDTH JOINER
        assert!(contains_unicode_path_attack("\u{200C}")); // ZERO WIDTH NON-JOINER
        assert!(contains_unicode_path_attack("\u{200B}")); // ZERO WIDTH SPACE
        assert!(contains_unicode_path_attack("\u{FEFF}")); // BOM
    }

    #[test]
    fn test_contains_unicode_path_attack_detects_rtl_overrides() {
        assert!(contains_unicode_path_attack("\u{202E}")); // RTL OVERRIDE
        assert!(contains_unicode_path_attack("\u{202D}")); // LTR OVERRIDE
        assert!(contains_unicode_path_attack("\u{202B}")); // RTL EMBEDDING
        assert!(contains_unicode_path_attack("\u{2067}")); // RTL ISOLATE
    }

    #[test]
    fn test_contains_unicode_path_attack_detects_confusable_slashes() {
        assert!(contains_unicode_path_attack("\u{2044}")); // FRACTION SLASH
        assert!(contains_unicode_path_attack("\u{2215}")); // DIVISION SLASH
        assert!(contains_unicode_path_attack("\u{29F8}")); // BIG SOLIDUS
    }

    #[test]
    fn test_contains_unicode_path_attack_detects_confusable_dots() {
        assert!(contains_unicode_path_attack("\u{A60E}")); // VAI FULL STOP
        assert!(contains_unicode_path_attack("\u{0701}")); // SYRIAC SUPRALINEAR FULL STOP
        assert!(contains_unicode_path_attack("\u{0702}")); // SYRIAC SUBLINEAR FULL STOP
    }

    #[test]
    fn test_detect_encoded_path_violation_rtl_override() {
        let result = detect_encoded_path_violation("etc/passwd/\u{202E}../");
        assert_eq!(result, Some("unicode normalization attack".to_string()));
    }

    #[test]
    fn test_detect_encoded_path_violation_zero_width_joiner() {
        let result = detect_encoded_path_violation(".\u{200D}./etc/passwd");
        assert_eq!(result, Some("unicode normalization attack".to_string()));
    }

    #[test]
    fn test_detect_encoded_path_violation_fraction_slash() {
        let result = detect_encoded_path_violation("..\u{2044}etc\u{2044}passwd");
        assert_eq!(result, Some("unicode normalization attack".to_string()));
    }
}
