//! Client-side encryption for HTML exports.
//!
//! Uses Web Crypto API compatible encryption (AES-GCM) with PBKDF2 key derivation.
//! The encryption happens in Rust, decryption happens in the browser via JavaScript.

use std::fmt;

/// Errors that can occur during encryption.
#[derive(Debug)]
pub enum EncryptionError {
    /// Key derivation failed
    KeyDerivation(String),
    /// Encryption operation failed
    EncryptionFailed(String),
    /// Invalid password
    InvalidPassword,
}

impl fmt::Display for EncryptionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EncryptionError::KeyDerivation(msg) => write!(f, "key derivation failed: {}", msg),
            EncryptionError::EncryptionFailed(msg) => write!(f, "encryption failed: {}", msg),
            EncryptionError::InvalidPassword => write!(f, "invalid password"),
        }
    }
}

impl std::error::Error for EncryptionError {}

/// Encrypted content bundle ready for embedding in HTML.
#[derive(Debug, Clone)]
pub struct EncryptedContent {
    /// Base64-encoded salt (16 bytes)
    pub salt: String,
    /// Base64-encoded IV/nonce (12 bytes for AES-GCM)
    pub iv: String,
    /// Base64-encoded ciphertext (includes GCM tag)
    pub ciphertext: String,
    /// PBKDF2 iteration count used for key derivation
    pub iterations: u32,
}

impl EncryptedContent {
    /// Convert to JSON for embedding in HTML.
    ///
    /// Note: Values are expected to be base64-encoded (safe characters only).
    /// JSON escaping is applied defensively in case of unexpected input.
    pub fn to_json(&self) -> String {
        format!(
            r#"{{"salt":"{}","iv":"{}","ciphertext":"{}","iterations":{}}}"#,
            json_escape_string(&self.salt),
            json_escape_string(&self.iv),
            json_escape_string(&self.ciphertext),
            self.iterations
        )
    }
}

/// Escape a string for safe inclusion in JSON.
/// Handles quotes, backslashes, and control characters.
fn json_escape_string(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => result.push_str("\\\""),
            '\\' => result.push_str("\\\\"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            c if c.is_control() => {
                // Escape other control characters as \uXXXX
                result.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => result.push(c),
        }
    }
    result
}

/// Encryption parameters matching Web Crypto API defaults.
#[derive(Debug, Clone)]
pub struct EncryptionParams {
    /// PBKDF2 iterations (600,000 recommended)
    pub iterations: u32,
    /// Salt length in bytes
    pub salt_len: usize,
    /// IV/nonce length in bytes (12 for AES-GCM)
    pub iv_len: usize,
}

impl Default for EncryptionParams {
    fn default() -> Self {
        Self {
            iterations: 600_000,
            salt_len: 16,
            iv_len: 12,
        }
    }
}

/// Encrypt content for client-side decryption.
///
/// This uses AES-256-GCM with PBKDF2-SHA256 key derivation,
/// matching the Web Crypto API implementation in scripts.rs.
///
/// # Note
/// This is a placeholder implementation. For production use,
/// integrate with a proper crypto library like `ring` or `aes-gcm`.
#[cfg(feature = "encryption")]
pub fn encrypt_content(
    plaintext: &str,
    password: &str,
    params: &EncryptionParams,
) -> Result<EncryptedContent, EncryptionError> {
    use aes_gcm::{
        Aes256Gcm, Nonce,
        aead::{Aead, KeyInit, OsRng},
    };
    use pbkdf2::pbkdf2_hmac;
    use rand::RngCore;
    use sha2::Sha256;

    if password.is_empty() {
        return Err(EncryptionError::InvalidPassword);
    }
    if params.iterations == 0 {
        return Err(EncryptionError::KeyDerivation(
            "iterations must be greater than zero".to_string(),
        ));
    }
    if params.salt_len == 0 {
        return Err(EncryptionError::KeyDerivation(
            "salt length must be greater than zero".to_string(),
        ));
    }
    if params.iv_len != 12 {
        return Err(EncryptionError::KeyDerivation(
            "iv length must be 12 bytes for AES-GCM".to_string(),
        ));
    }

    // Generate random salt and IV
    let mut salt = vec![0u8; params.salt_len];
    let mut iv = vec![0u8; params.iv_len];
    OsRng.fill_bytes(&mut salt);
    OsRng.fill_bytes(&mut iv);

    // Derive key using PBKDF2-SHA256
    let mut key = [0u8; 32]; // 256 bits for AES-256
    pbkdf2_hmac::<Sha256>(password.as_bytes(), &salt, params.iterations, &mut key);

    // Encrypt with AES-256-GCM
    let cipher = Aes256Gcm::new_from_slice(&key)
        .map_err(|e| EncryptionError::EncryptionFailed(e.to_string()))?;

    let nonce = Nonce::from_slice(&iv);
    let ciphertext = cipher
        .encrypt(nonce, plaintext.as_bytes())
        .map_err(|e| EncryptionError::EncryptionFailed(e.to_string()))?;

    Ok(EncryptedContent {
        salt: base64_encode(&salt),
        iv: base64_encode(&iv),
        ciphertext: base64_encode(&ciphertext),
        iterations: params.iterations,
    })
}

/// Placeholder encrypt function when encryption feature is disabled.
#[cfg(not(feature = "encryption"))]
pub fn encrypt_content(
    _plaintext: &str,
    _password: &str,
    _params: &EncryptionParams,
) -> Result<EncryptedContent, EncryptionError> {
    Err(EncryptionError::EncryptionFailed(
        "encryption feature not enabled - compile with --features encryption".to_string(),
    ))
}

/// Base64 encode bytes (standard alphabet).
#[cfg(feature = "encryption")]
fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut result = Vec::with_capacity(data.len().div_ceil(3) * 4);

    for chunk in data.chunks(3) {
        let mut buf = [0u8; 3];
        buf[..chunk.len()].copy_from_slice(chunk);

        let n = ((buf[0] as u32) << 16) | ((buf[1] as u32) << 8) | (buf[2] as u32);

        result.push(ALPHABET[((n >> 18) & 0x3F) as usize]);
        result.push(ALPHABET[((n >> 12) & 0x3F) as usize]);

        if chunk.len() > 1 {
            result.push(ALPHABET[((n >> 6) & 0x3F) as usize]);
        } else {
            result.push(b'=');
        }

        if chunk.len() > 2 {
            result.push(ALPHABET[(n & 0x3F) as usize]);
        } else {
            result.push(b'=');
        }
    }

    String::from_utf8(result).unwrap()
}

/// Generate HTML for encrypted content display.
///
/// The JSON is HTML-escaped to prevent XSS even if EncryptedContent
/// contains unexpected data (defensive programming).
pub fn render_encrypted_placeholder(encrypted: &EncryptedContent) -> String {
    // HTML-escape the JSON to prevent XSS if someone passes malicious data
    let json = encrypted.to_json();
    let escaped_json = html_escape_for_content(&json);
    format!(
        r#"            <!-- Encrypted content - requires password to decrypt -->
            <div id="encrypted-content" hidden>{}</div>
            <div class="encrypted-notice">
                <p>This conversation is encrypted. Enter the password above to view.</p>
            </div>"#,
        escaped_json
    )
}

/// Escape HTML special characters for safe embedding in HTML content.
fn html_escape_for_content(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => result.push_str("&amp;"),
            '<' => result.push_str("&lt;"),
            '>' => result.push_str("&gt;"),
            _ => result.push(c),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(feature = "encryption")]
    fn test_base64_encode() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn test_encrypted_content_to_json() {
        let content = EncryptedContent {
            salt: "abc123".to_string(),
            iv: "xyz789".to_string(),
            ciphertext: "encrypted_data".to_string(),
            iterations: 123_456,
        };

        let json = content.to_json();
        assert!(json.contains("\"salt\":\"abc123\""));
        assert!(json.contains("\"iv\":\"xyz789\""));
        assert!(json.contains("\"ciphertext\":\"encrypted_data\""));
        assert!(json.contains("\"iterations\":123456"));
    }

    #[test]
    fn test_json_escape_string() {
        use super::json_escape_string;

        // Normal strings pass through unchanged
        assert_eq!(json_escape_string("hello"), "hello");
        assert_eq!(json_escape_string("abc123+/="), "abc123+/=");

        // Quotes and backslashes are escaped
        assert_eq!(json_escape_string(r#"say "hi""#), r#"say \"hi\""#);
        assert_eq!(json_escape_string(r"path\to\file"), r"path\\to\\file");

        // Control characters are escaped
        assert_eq!(json_escape_string("line1\nline2"), "line1\\nline2");
        assert_eq!(json_escape_string("col1\tcol2"), "col1\\tcol2");
        assert_eq!(json_escape_string("cr\rhere"), "cr\\rhere");
    }

    #[test]
    fn test_encryption_params_default() {
        let params = EncryptionParams::default();
        assert_eq!(params.iterations, 600_000);
        assert_eq!(params.salt_len, 16);
        assert_eq!(params.iv_len, 12);
    }

    #[test]
    #[cfg(feature = "encryption")]
    fn test_encrypt_content_roundtrip() {
        use aes_gcm::{
            Aes256Gcm, Nonce,
            aead::{Aead, KeyInit},
        };
        use base64::Engine; // Required for decode() method
        use base64::prelude::BASE64_STANDARD;
        use pbkdf2::pbkdf2_hmac;
        use sha2::Sha256;

        let params = EncryptionParams {
            iterations: 1_000,
            salt_len: 16,
            iv_len: 12,
        };
        let plaintext = "Hello 🌍";
        let password = "p@ssw0rd";

        let encrypted = encrypt_content(plaintext, password, &params).expect("encrypt");
        assert_eq!(encrypted.iterations, params.iterations);

        let salt = BASE64_STANDARD
            .decode(encrypted.salt.as_bytes())
            .expect("salt b64");
        let iv = BASE64_STANDARD
            .decode(encrypted.iv.as_bytes())
            .expect("iv b64");
        let ciphertext = BASE64_STANDARD
            .decode(encrypted.ciphertext.as_bytes())
            .expect("ciphertext b64");

        let mut key = [0u8; 32];
        pbkdf2_hmac::<Sha256>(password.as_bytes(), &salt, params.iterations, &mut key);

        let cipher = Aes256Gcm::new_from_slice(&key).expect("cipher");
        let nonce = Nonce::from_slice(&iv);
        let decrypted = cipher.decrypt(nonce, ciphertext.as_ref()).expect("decrypt");

        assert_eq!(plaintext, String::from_utf8(decrypted).expect("utf8"));
    }

    #[test]
    #[cfg(feature = "encryption")]
    fn test_encrypt_rejects_empty_password() {
        let params = EncryptionParams {
            iterations: 1_000,
            salt_len: 16,
            iv_len: 12,
        };
        let result = encrypt_content("hello", "", &params);
        assert!(matches!(result, Err(EncryptionError::InvalidPassword)));
    }

    #[test]
    #[cfg(feature = "encryption")]
    fn test_encrypt_rejects_invalid_params() {
        let mut params = EncryptionParams {
            iterations: 1_000,
            salt_len: 16,
            iv_len: 12,
        };

        params.iterations = 0;
        let result = encrypt_content("hello", "pw", &params);
        assert!(matches!(result, Err(EncryptionError::KeyDerivation(_))));

        params.iterations = 1_000;
        params.salt_len = 0;
        let result = encrypt_content("hello", "pw", &params);
        assert!(matches!(result, Err(EncryptionError::KeyDerivation(_))));

        params.salt_len = 16;
        params.iv_len = 8;
        let result = encrypt_content("hello", "pw", &params);
        assert!(matches!(result, Err(EncryptionError::KeyDerivation(_))));
    }

    #[test]
    #[cfg(not(feature = "encryption"))]
    fn test_encrypt_without_feature_returns_error() {
        let result = encrypt_content("test", "password", &EncryptionParams::default());
        assert!(result.is_err());
    }
}
