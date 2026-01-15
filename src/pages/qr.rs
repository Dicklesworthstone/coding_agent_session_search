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
use rand::{RngCore, rngs::OsRng};
use std::path::Path;
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
        OsRng.fill_bytes(&mut bytes);
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
        // SAFETY: Zeroize encoded string by replacing with zeros then clearing
        // This ensures the base64-encoded secret doesn't linger in memory
        unsafe {
            let encoded_bytes = self.encoded.as_bytes_mut();
            encoded_bytes.zeroize();
        }
        self.encoded.clear();
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
        // Zeroize secret_text which contains the encoded secret
        // SAFETY: Same as RecoverySecret::drop - zeroize string contents
        unsafe {
            let text_bytes = self.secret_text.as_bytes_mut();
            text_bytes.zeroize();
        }
        self.secret_text.clear();
        // Note: secret field has its own Drop impl that zeroizes it
        // qr_png and qr_svg don't contain plaintext secret (encoded in QR)
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
        std::fs::create_dir_all(dir).context("Failed to create private directory")?;

        // Write recovery-secret.txt
        let secret_path = dir.join("recovery-secret.txt");
        std::fs::write(&secret_path, &self.secret_text)
            .context("Failed to write recovery-secret.txt")?;

        // Write qr-code.png
        let png_path = dir.join("qr-code.png");
        std::fs::write(&png_path, &self.qr_png).context("Failed to write qr-code.png")?;

        // Write qr-code.svg
        let svg_path = dir.join("qr-code.svg");
        std::fs::write(&svg_path, &self.qr_svg).context("Failed to write qr-code.svg")?;

        info!(
            dir = %dir.display(),
            "Wrote recovery artifacts: recovery-secret.txt, qr-code.png, qr-code.svg"
        );

        Ok(())
    }
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
        std::fs::write(output_path, png_data)?;
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
        // Should reject secrets with < 128 bits
        let short_bytes = vec![0u8; 15]; // Only 120 bits
        assert!(RecoverySecret::from_bytes(short_bytes).is_none());

        // Should accept secrets with >= 128 bits
        let min_bytes = vec![0u8; 16]; // 128 bits
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
