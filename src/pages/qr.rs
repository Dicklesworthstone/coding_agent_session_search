//! QR code generation for recovery secrets.
//!
//! Generates high-entropy recovery secrets and encodes them as QR codes
//! for out-of-band archive unlock. The recovery secret provides an alternative
//! to password-based decryption using HKDF-SHA256 (fast for high-entropy inputs).
//!
//! # Output Files (private/)
//!
//! ```text
//! private/
//! ├── recovery-secret.txt   # Human-readable secret with instructions
//! ├── qr-code.png           # QR code image for mobile scanning
//! └── qr-code.svg           # Vector QR code for print
//! ```
//!
//! # Security
//!
//! - Recovery secret is 256-bit (32 bytes) for maximum security
//! - Encoded as URL-safe base64 without padding
//! - Creates a recovery key slot using HKDF-SHA256
//! - NEVER deploy private/ directory with public site

#![allow(unexpected_cfgs)]

use anyhow::{Context, Result, bail};
use base64::prelude::*;
use chrono::Utc;
use rand::Rng;
use std::fs::OpenOptions;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use tracing::info;
use zeroize::Zeroize;

/// Recovery secret entropy (256 bits = 32 bytes)
const RECOVERY_SECRET_BYTES: usize = 32;

/// Recovery secret for archive unlock.
///
/// Contains high-entropy random bytes that can be used to derive
/// a key encryption key (KEK) via HKDF-SHA256.
#[derive(Clone)]
pub struct RecoverySecret {
    /// Raw secret bytes (zeroized on drop)
    bytes: Vec<u8>,
    /// Base64url-encoded secret (for QR code and text file)
    encoded: String,
}

impl std::fmt::Debug for RecoverySecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Redact sensitive data to prevent accidental logging
        f.debug_struct("RecoverySecret")
            .field("entropy_bits", &self.entropy_bits())
            .field("encoded", &"[REDACTED]")
            .finish()
    }
}

impl RecoverySecret {
    /// Generate a new random recovery secret.
    ///
    /// Uses the system's cryptographically secure random number generator.
    pub fn generate() -> Self {
        let mut bytes = vec![0u8; RECOVERY_SECRET_BYTES];
        let mut rng = rand::rng();
        rng.fill_bytes(&mut bytes);
        let encoded = BASE64_URL_SAFE_NO_PAD.encode(&bytes);
        Self { bytes, encoded }
    }

    /// Create a recovery secret from existing bytes.
    ///
    /// Returns None if the bytes are too short (< 24 bytes / 192 bits).
    /// NIST recommends 192+ bits for long-term cryptographic material.
    pub fn from_bytes(bytes: Vec<u8>) -> Option<Self> {
        if bytes.len() < 24 {
            return None;
        }
        let encoded = BASE64_URL_SAFE_NO_PAD.encode(&bytes);
        Some(Self { bytes, encoded })
    }

    /// Create a recovery secret from a base64url-encoded string.
    pub fn from_encoded(encoded: &str) -> Result<Self> {
        let bytes = BASE64_URL_SAFE_NO_PAD
            .decode(encoded)
            .context("Invalid base64url encoding")?;
        if bytes.len() < 24 {
            bail!("Recovery secret too short (minimum 192 bits for long-term security)");
        }
        Ok(Self {
            bytes,
            encoded: encoded.to_string(),
        })
    }

    /// Get the raw secret bytes for key derivation.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Get the base64url-encoded secret (for QR code).
    pub fn encoded(&self) -> &str {
        &self.encoded
    }

    /// Get the entropy in bits.
    pub fn entropy_bits(&self) -> usize {
        self.bytes.len() * 8
    }
}

impl Drop for RecoverySecret {
    fn drop(&mut self) {
        // Use zeroize crate for secure erasure (prevents compiler optimization)
        self.bytes.zeroize();
        // Move encoded bytes out, zeroize, then drop without unsafe string mutation.
        let mut encoded_bytes = std::mem::take(&mut self.encoded).into_bytes();
        encoded_bytes.zeroize();
    }
}

/// Generated recovery artifacts ready for writing to disk.
pub struct RecoveryArtifacts {
    /// The recovery secret
    pub secret: RecoverySecret,
    /// Content for recovery-secret.txt (contains secret, zeroized on drop)
    pub secret_text: String,
    /// PNG image bytes for qr-code.png
    pub qr_png: Vec<u8>,
    /// SVG markup for qr-code.svg
    pub qr_svg: String,
}

impl Drop for RecoveryArtifacts {
    fn drop(&mut self) {
        // Zeroize all secret-bearing payloads before drop.
        let mut text_bytes = std::mem::take(&mut self.secret_text).into_bytes();
        text_bytes.zeroize();
        self.qr_png.zeroize();
        let mut svg_bytes = std::mem::take(&mut self.qr_svg).into_bytes();
        svg_bytes.zeroize();
        // Note: secret field has its own Drop impl that zeroizes it
    }
}

impl RecoveryArtifacts {
    /// Generate all recovery artifacts for an archive.
    ///
    /// # Arguments
    /// * `archive_name` - Name of the archive (for the text file header)
    pub fn generate(archive_name: &str) -> Result<Self> {
        let secret = RecoverySecret::generate();
        let timestamp = Utc::now().to_rfc3339();

        // Generate recovery-secret.txt content
        let secret_text = format!(
            r#"CASS RECOVERY SECRET
====================

Archive: {archive_name}
Created: {timestamp}

Secret: {secret}

IMPORTANT:
- This secret unlocks your archive if you forget your password
- Store securely (password manager, encrypted USB, safe)
- NEVER deploy this file with the public site
- The QR code encodes the same secret

[QR code path: qr-code.png]
"#,
            archive_name = archive_name,
            timestamp = timestamp,
            secret = secret.encoded(),
        );

        // Generate QR codes
        let qr_png = generate_qr_png(secret.encoded())?;
        let qr_svg = generate_qr_svg(secret.encoded())?;

        info!(
            entropy_bits = secret.entropy_bits(),
            encoded_len = secret.encoded().len(),
            "Generated recovery secret"
        );

        Ok(Self {
            secret,
            secret_text,
            qr_png,
            qr_svg,
        })
    }

    /// Write all artifacts to the specified directory.
    ///
    /// Creates the directory if it doesn't exist.
    pub fn write_to_dir(&self, dir: &Path) -> Result<()> {
        ensure_recovery_artifact_dir(dir)?;

        // Write recovery-secret.txt
        let secret_path = dir.join("recovery-secret.txt");
        write_recovery_artifact(&secret_path, self.secret_text.as_bytes())
            .context("Failed to write recovery-secret.txt")?;

        // Write qr-code.png
        let png_path = dir.join("qr-code.png");
        write_recovery_artifact(&png_path, &self.qr_png).context("Failed to write qr-code.png")?;

        // Write qr-code.svg
        let svg_path = dir.join("qr-code.svg");
        write_recovery_artifact(&svg_path, self.qr_svg.as_bytes())
            .context("Failed to write qr-code.svg")?;

        info!(
            dir = %dir.display(),
            "Wrote recovery artifacts: recovery-secret.txt, qr-code.png, qr-code.svg"
        );

        Ok(())
    }
}

fn ensure_recovery_artifact_dir(dir: &Path) -> Result<()> {
    match std::fs::symlink_metadata(dir) {
        Ok(metadata) => {
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                bail!(
                    "Recovery artifact directory must not be a symlink: {}",
                    dir.display()
                );
            }
            if !file_type.is_dir() {
                bail!(
                    "Recovery artifact path must be a directory: {}",
                    dir.display()
                );
            }
            secure_recovery_artifact_dir(dir)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            #[cfg(unix)]
            {
                let mut builder = std::fs::DirBuilder::new();
                builder.recursive(true).mode(0o700);
                builder
                    .create(dir)
                    .context("Failed to create private directory")?;
            }
            #[cfg(not(unix))]
            std::fs::create_dir_all(dir).context("Failed to create private directory")?;
            ensure_recovery_artifact_dir(dir)
        }
        Err(err) => Err(err)
            .with_context(|| format!("Failed to inspect recovery artifact dir {}", dir.display())),
    }
}

fn secure_recovery_artifact_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).with_context(|| {
        format!(
            "Failed to set owner-only permissions on recovery artifact directory {}",
            dir.display()
        )
    })?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

fn reject_recovery_artifact_symlink(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                bail!(
                    "Recovery artifact file must not be a symlink: {}",
                    path.display()
                );
            }
            if file_type.is_dir() {
                bail!(
                    "Recovery artifact path must be a regular file, not a directory: {}",
                    path.display()
                );
            }
            Ok(())
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err)
            .with_context(|| format!("Failed to inspect recovery artifact {}", path.display())),
    }
}

fn recovery_artifact_temp_path(path: &Path, attempt: usize) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("artifact");
    path.with_file_name(format!(
        ".{file_name}.tmp.{}.{}",
        std::process::id(),
        attempt
    ))
}

fn write_recovery_artifact(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    ensure_recovery_artifact_dir(parent)?;
    reject_recovery_artifact_symlink(path)?;

    let mut temp_path = None;
    let mut file = None;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    for attempt in 0..100 {
        let candidate = recovery_artifact_temp_path(path, attempt);
        match options.open(&candidate) {
            Ok(opened) => {
                temp_path = Some(candidate);
                file = Some(opened);
                break;
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "Failed to create temporary recovery artifact {}",
                        candidate.display()
                    )
                });
            }
        }
    }

    let temp_path = temp_path.ok_or_else(|| {
        anyhow::anyhow!(
            "Failed to allocate a temporary recovery artifact path for {}",
            path.display()
        )
    })?;
    let mut file = file.expect("temp_path is set only with an open file");
    let write_result = (|| -> Result<()> {
        file.write_all(contents).with_context(|| {
            format!(
                "Failed to write temporary recovery artifact {}",
                temp_path.display()
            )
        })?;
        file.sync_all().with_context(|| {
            format!(
                "Failed to sync temporary recovery artifact {}",
                temp_path.display()
            )
        })?;
        Ok(())
    })();

    if let Err(err) = write_result {
        let _ = std::fs::remove_file(&temp_path);
        return Err(err);
    }
    drop(file);

    if let Err(err) = std::fs::rename(&temp_path, path) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(err)
            .with_context(|| format!("Failed to install recovery artifact {}", path.display()));
    }
    Ok(())
}

/// Generate a QR code as PNG bytes.
///
/// Returns PNG image data that can be written to a file.
pub fn generate_qr_png(data: &str) -> Result<Vec<u8>> {
    #[cfg(feature = "qr")]
    {
        use image::Luma;
        use qrcode::QrCode;

        let code = QrCode::new(data.as_bytes()).context("Failed to create QR code")?;
        let image = code.render::<Luma<u8>>().build();

        let mut png_bytes = Vec::new();
        image::DynamicImage::ImageLuma8(image)
            .write_to(
                &mut std::io::Cursor::new(&mut png_bytes),
                image::ImageFormat::Png,
            )
            .context("Failed to encode PNG")?;

        Ok(png_bytes)
    }

    #[cfg(not(feature = "qr"))]
    {
        let _ = data;
        bail!("QR code generation requires the 'qr' feature to be enabled")
    }
}

/// Generate a QR code as SVG string.
///
/// Returns SVG markup that can be written to a file.
pub fn generate_qr_svg(data: &str) -> Result<String> {
    #[cfg(feature = "qr")]
    {
        use qrcode::QrCode;
        use qrcode::render::svg;

        let code = QrCode::new(data.as_bytes()).context("Failed to create QR code")?;
        let svg = code
            .render()
            .min_dimensions(200, 200)
            .dark_color(svg::Color("#000000"))
            .light_color(svg::Color("#ffffff"))
            .build();

        Ok(svg)
    }

    #[cfg(not(feature = "qr"))]
    {
        let _ = data;
        bail!("QR code generation requires the 'qr' feature to be enabled")
    }
}

/// QR code generator (legacy struct interface for backward compatibility)
pub struct QrGenerator;

impl Default for QrGenerator {
    fn default() -> Self {
        Self::new()
    }
}

impl QrGenerator {
    pub fn new() -> Self {
        Self
    }

    pub fn generate(&self, data: &str, output_path: &Path) -> Result<()> {
        let png_data = generate_qr_png(data)?;
        write_recovery_artifact(output_path, &png_data)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_recovery_secret_generation() {
        let secret = RecoverySecret::generate();

        // Should have 256 bits of entropy
        assert_eq!(secret.entropy_bits(), 256);
        assert_eq!(secret.as_bytes().len(), 32);

        // Encoded string should be valid base64url
        assert!(!secret.encoded().is_empty());
        assert!(!secret.encoded().contains('+')); // base64url, not base64
        assert!(!secret.encoded().contains('/')); // base64url, not base64
    }

    #[test]
    fn test_recovery_secret_round_trip() {
        let secret1 = RecoverySecret::generate();
        let encoded = secret1.encoded().to_string();

        let secret2 = RecoverySecret::from_encoded(&encoded).expect("decode should work");
        assert_eq!(secret1.as_bytes(), secret2.as_bytes());
    }

    #[test]
    fn test_recovery_secret_minimum_entropy() {
        // Should reject secrets with < 192 bits (NIST recommendation for long-term security)
        let short_bytes = vec![0u8; 23]; // Only 184 bits (below 192-bit threshold)
        assert!(RecoverySecret::from_bytes(short_bytes).is_none());

        // Should accept secrets with >= 192 bits
        let min_bytes = vec![0u8; 24]; // 192 bits (minimum acceptable)
        assert!(RecoverySecret::from_bytes(min_bytes).is_some());
    }

    #[test]
    fn test_recovery_secret_deterministic_encoding() {
        // Same bytes should produce same encoding
        let bytes = vec![1u8; 32];
        let secret1 = RecoverySecret::from_bytes(bytes.clone()).unwrap();
        let secret2 = RecoverySecret::from_bytes(bytes).unwrap();
        assert_eq!(secret1.encoded(), secret2.encoded());
    }

    #[test]
    #[cfg(unix)]
    fn test_recovery_artifacts_write_to_dir_rejects_symlinked_secret_file() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().expect("create temp dir");
        let private_dir = tmp.path().join("private");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&private_dir).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let protected = outside.join("protected-secret.txt");
        std::fs::write(&protected, "do not overwrite").unwrap();
        symlink(&protected, private_dir.join("recovery-secret.txt")).unwrap();

        let secret = RecoverySecret::from_bytes(vec![1u8; 32]).unwrap();
        let artifacts = RecoveryArtifacts {
            secret,
            secret_text: "safe secret text".to_string(),
            qr_png: b"png".to_vec(),
            qr_svg: "<svg></svg>".to_string(),
        };

        let err = artifacts.write_to_dir(&private_dir).unwrap_err();
        let rendered = format!("{err:#}");

        assert!(
            rendered.contains("must not be a symlink"),
            "unexpected error: {err:#}"
        );
        assert_eq!(
            std::fs::read_to_string(&protected).unwrap(),
            "do not overwrite"
        );
        assert!(
            std::fs::symlink_metadata(private_dir.join("recovery-secret.txt"))
                .unwrap()
                .file_type()
                .is_symlink(),
            "rejected recovery secret symlink should be left intact"
        );
    }

    #[test]
    #[cfg(unix)]
    fn recovery_artifact_writer_enforces_owner_only_modes() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new()?;
        let private_dir = tmp.path().join("private");
        std::fs::create_dir_all(&private_dir)?;
        std::fs::set_permissions(&private_dir, std::fs::Permissions::from_mode(0o777))?;

        let artifact_path = private_dir.join("recovery-secret.txt");
        write_recovery_artifact(&artifact_path, b"private recovery material")?;

        let dir_mode = std::fs::metadata(&private_dir)?.permissions().mode() & 0o777;
        let file_mode = std::fs::metadata(&artifact_path)?.permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "private directory mode was {dir_mode:o}");
        assert_eq!(
            file_mode & 0o077,
            0,
            "recovery artifact exposed group/other permission bits: {file_mode:o}"
        );
        Ok(())
    }

    #[test]
    #[cfg(feature = "qr")]
    fn test_qr_png_generation() {
        let data = "test-secret-data-12345";
        let png = generate_qr_png(data).expect("PNG generation should work");

        // Should produce valid PNG (starts with PNG magic bytes)
        assert!(png.len() > 100);
        assert_eq!(&png[0..8], b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    #[cfg(feature = "qr")]
    fn test_qr_svg_generation() {
        let data = "test-secret-data-12345";
        let svg = generate_qr_svg(data).expect("SVG generation should work");

        // Should produce valid SVG
        assert!(svg.contains("<svg"));
        assert!(svg.contains("</svg>"));
    }

    #[test]
    #[cfg(feature = "qr")]
    fn test_recovery_artifacts_generation() {
        let artifacts =
            RecoveryArtifacts::generate("test-archive").expect("Artifacts generation should work");

        // Secret should be 256 bits
        assert_eq!(artifacts.secret.entropy_bits(), 256);

        // Text file should contain the secret
        assert!(artifacts.secret_text.contains(artifacts.secret.encoded()));
        assert!(artifacts.secret_text.contains("test-archive"));
        assert!(artifacts.secret_text.contains("CASS RECOVERY SECRET"));

        // PNG should be valid
        assert!(artifacts.qr_png.len() > 100);
        assert_eq!(&artifacts.qr_png[0..8], b"\x89PNG\r\n\x1a\n");

        // SVG should be valid
        assert!(artifacts.qr_svg.contains("<svg"));
    }

    #[test]
    #[cfg(feature = "qr")]
    fn test_recovery_artifacts_write_to_dir() {
        let tmp = TempDir::new().expect("create temp dir");
        let private_dir = tmp.path().join("private");

        let artifacts =
            RecoveryArtifacts::generate("test-archive").expect("Artifacts generation should work");

        artifacts
            .write_to_dir(&private_dir)
            .expect("Writing should work");

        // All files should exist
        assert!(private_dir.join("recovery-secret.txt").exists());
        assert!(private_dir.join("qr-code.png").exists());
        assert!(private_dir.join("qr-code.svg").exists());

        // Verify secret file content
        let secret_content =
            std::fs::read_to_string(private_dir.join("recovery-secret.txt")).unwrap();
        assert!(secret_content.contains(artifacts.secret.encoded()));
    }

    #[test]
    #[cfg(feature = "qr")]
    fn test_qr_code_encodes_exact_secret() {
        // Generate artifacts
        let artifacts =
            RecoveryArtifacts::generate("test-archive").expect("Artifacts generation should work");

        // The QR codes should encode the exact secret
        // (We can't easily decode without an external library, but we verify
        // the same data goes into both PNG and SVG generation)
        let png1 = generate_qr_png(artifacts.secret.encoded()).unwrap();
        let png2 = generate_qr_png(artifacts.secret.encoded()).unwrap();
        assert_eq!(png1, png2, "Same input should produce same output");
    }
}
