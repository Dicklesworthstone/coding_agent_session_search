use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use ring::{
    hkdf::{self as ring_hkdf, KeyType},
    hmac,
};

pub use argon2::Params as Argon2Params;

const AES_GCM_KEY_LEN: usize = 32;
const AES_GCM_NONCE_LEN: usize = 12;
const AES_GCM_TAG_LEN: usize = 16;

struct HkdfOutputLen(usize);

impl KeyType for HkdfOutputLen {
    fn len(&self) -> usize {
        self.0
    }
}

fn validate_length(label: &str, actual: usize, expected: usize) -> Result<(), String> {
    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "{} length invalid: expected {} bytes, got {}",
            label, expected, actual
        ))
    }
}

pub fn aes_gcm_encrypt(
    key: &[u8],
    nonce: &[u8],
    plaintext: &[u8],
    aad: &[u8],
) -> Result<(Vec<u8>, Vec<u8>), String> {
    validate_length("AES-GCM key", key.len(), AES_GCM_KEY_LEN)?;
    validate_length("AES-GCM nonce", nonce.len(), AES_GCM_NONCE_LEN)?;

    let key = Key::<Aes256Gcm>::from_slice(key);
    let cipher = Aes256Gcm::new(key);
    let nonce = Nonce::from_slice(nonce);
    let payload = Payload {
        msg: plaintext,
        aad,
    };

    // aes-gcm returns ciphertext + tag appended.
    let ciphertext_with_tag = cipher
        .encrypt(nonce, payload)
        .map_err(|e| format!("encryption failure: {}", e))?;

    if ciphertext_with_tag.len() < AES_GCM_TAG_LEN {
        return Err("encryption failure: ciphertext too short".to_string());
    }

    // Tag is 16 bytes for AES-256-GCM
    let split_idx = ciphertext_with_tag.len() - AES_GCM_TAG_LEN;
    let (cipher, tag) = ciphertext_with_tag.split_at(split_idx);

    Ok((cipher.to_vec(), tag.to_vec()))
}

pub fn aes_gcm_decrypt(
    key: &[u8],
    nonce: &[u8],
    ciphertext: &[u8],
    aad: &[u8],
    tag: &[u8],
) -> Result<Vec<u8>, String> {
    validate_length("AES-GCM key", key.len(), AES_GCM_KEY_LEN)?;
    validate_length("AES-GCM nonce", nonce.len(), AES_GCM_NONCE_LEN)?;
    validate_length("AES-GCM tag", tag.len(), AES_GCM_TAG_LEN)?;

    let key = Key::<Aes256Gcm>::from_slice(key);
    let cipher = Aes256Gcm::new(key);
    let nonce = Nonce::from_slice(nonce);

    // Combine ciphertext and tag for decryption (aes-gcm crate expects them together)
    // Use the Payload API directly to avoid manual concatenation.
    let mut payload_vec = Vec::with_capacity(ciphertext.len() + tag.len());
    payload_vec.extend_from_slice(ciphertext);
    payload_vec.extend_from_slice(tag);

    let payload = Payload {
        msg: &payload_vec,
        aad,
    };

    cipher
        .decrypt(nonce, payload)
        .map_err(|e| format!("decryption failed: {}", e))
}

pub fn argon2id_hash(
    password: &[u8],
    salt: &[u8],
    params: &Argon2Params,
) -> Result<Vec<u8>, String> {
    let argon2 = argon2::Argon2::new(
        argon2::Algorithm::Argon2id,
        argon2::Version::V0x13,
        params.clone(),
    );

    let mut output = vec![0u8; params.output_len().unwrap_or(32)];
    argon2
        .hash_password_into(password, salt, &mut output)
        .map_err(|e| format!("argon2 hashing failed: {}", e))?;
    Ok(output)
}

pub fn hkdf_extract_expand(
    ikm: &[u8],
    salt: &[u8],
    info: &[u8],
    len: usize,
) -> Result<Vec<u8>, String> {
    let salt = ring_hkdf::Salt::new(ring_hkdf::HKDF_SHA256, salt);
    let prk = salt.extract(ikm);
    let okm = prk
        .expand(&[info], HkdfOutputLen(len))
        .map_err(|_| "hkdf expand failed: invalid output length".to_string())?;
    let mut output = vec![0u8; len];
    okm.fill(&mut output)
        .map_err(|_| "hkdf expand failed: unable to fill output buffer".to_string())?;
    Ok(output)
}

pub fn hkdf_extract(salt: &[u8], ikm: &[u8]) -> Vec<u8> {
    let key = hmac::Key::new(hmac::HMAC_SHA256, salt);
    hmac::sign(&key, ikm).as_ref().to_vec()
}

// =============================================================================
// Unit Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // AES-GCM Encrypt/Decrypt Tests
    // =========================================================================

    #[test]
    fn aes_gcm_encrypt_decrypt_round_trip() {
        let key = [0u8; 32];
        let nonce = [0u8; 12];
        let plaintext = b"Hello, world!";
        let aad = b"additional data";

        let (ciphertext, tag) = aes_gcm_encrypt(&key, &nonce, plaintext, aad).unwrap();

        let decrypted = aes_gcm_decrypt(&key, &nonce, &ciphertext, aad, &tag).unwrap();

        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn aes_gcm_round_trip_empty_plaintext() {
        let key = [0u8; 32];
        let nonce = [0u8; 12];
        let plaintext = b"";
        let aad = b"";

        let (ciphertext, tag) = aes_gcm_encrypt(&key, &nonce, plaintext, aad).unwrap();

        assert!(ciphertext.is_empty());
        assert_eq!(tag.len(), 16);

        let decrypted = aes_gcm_decrypt(&key, &nonce, &ciphertext, aad, &tag).unwrap();
        assert!(decrypted.is_empty());
    }

    #[test]
    fn aes_gcm_round_trip_large_data() {
        let key = [0xab; 32];
        let nonce = [0xcd; 12];
        let plaintext: Vec<u8> = (0..10000).map(|i| (i % 256) as u8).collect();
        let aad = b"large data test";

        let (ciphertext, tag) = aes_gcm_encrypt(&key, &nonce, &plaintext, aad).unwrap();

        assert_eq!(ciphertext.len(), plaintext.len());

        let decrypted = aes_gcm_decrypt(&key, &nonce, &ciphertext, aad, &tag).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn aes_gcm_round_trip_unicode_data() {
        let key = [0x42; 32];
        let nonce = [0x13; 12];
        let plaintext = "日本語テスト 🦀 Rust".as_bytes();
        let aad = "unicode AAD: émojis 🎉".as_bytes();

        let (ciphertext, tag) = aes_gcm_encrypt(&key, &nonce, plaintext, aad).unwrap();
        let decrypted = aes_gcm_decrypt(&key, &nonce, &ciphertext, aad, &tag).unwrap();

        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn aes_gcm_encrypt_invalid_key_length() {
        let key = [0u8; 16]; // Should be 32 bytes
        let nonce = [0u8; 12];
        let plaintext = b"test";
        let aad = b"";

        let result = aes_gcm_encrypt(&key, &nonce, plaintext, aad);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("key length invalid"));
    }

    #[test]
    fn aes_gcm_encrypt_invalid_nonce_length() {
        let key = [0u8; 32];
        let nonce = [0u8; 16]; // Should be 12 bytes
        let plaintext = b"test";
        let aad = b"";

        let result = aes_gcm_encrypt(&key, &nonce, plaintext, aad);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("nonce length invalid"));
    }

    #[test]
    fn aes_gcm_decrypt_invalid_key_length() {
        let key = [0u8; 31]; // Should be 32 bytes
        let nonce = [0u8; 12];
        let ciphertext = b"ciphertext";
        let aad = b"";
        let tag = [0u8; 16];

        let result = aes_gcm_decrypt(&key, &nonce, ciphertext, aad, &tag);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("key length invalid"));
    }

    #[test]
    fn aes_gcm_decrypt_invalid_nonce_length() {
        let key = [0u8; 32];
        let nonce = [0u8; 8]; // Should be 12 bytes
        let ciphertext = b"ciphertext";
        let aad = b"";
        let tag = [0u8; 16];

        let result = aes_gcm_decrypt(&key, &nonce, ciphertext, aad, &tag);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("nonce length invalid"));
    }

    #[test]
    fn aes_gcm_decrypt_invalid_tag_length() {
        let key = [0u8; 32];
        let nonce = [0u8; 12];
        let ciphertext = b"ciphertext";
        let aad = b"";
        let tag = [0u8; 8]; // Should be 16 bytes

        let result = aes_gcm_decrypt(&key, &nonce, ciphertext, aad, &tag);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("tag length invalid"));
    }

    #[test]
    fn aes_gcm_decrypt_wrong_key_fails() {
        let key = [0u8; 32];
        let nonce = [0u8; 12];
        let plaintext = b"secret message";
        let aad = b"aad";

        let (ciphertext, tag) = aes_gcm_encrypt(&key, &nonce, plaintext, aad).unwrap();

        // Use different key for decryption
        let wrong_key = [1u8; 32];
        let result = aes_gcm_decrypt(&wrong_key, &nonce, &ciphertext, aad, &tag);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("decryption failed"));
    }

    #[test]
    fn aes_gcm_decrypt_wrong_aad_fails() {
        let key = [0u8; 32];
        let nonce = [0u8; 12];
        let plaintext = b"secret message";
        let aad = b"correct aad";

        let (ciphertext, tag) = aes_gcm_encrypt(&key, &nonce, plaintext, aad).unwrap();

        // Use different AAD for decryption
        let wrong_aad = b"wrong aad";
        let result = aes_gcm_decrypt(&key, &nonce, &ciphertext, wrong_aad, &tag);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("decryption failed"));
    }

    #[test]
    fn aes_gcm_decrypt_tampered_ciphertext_fails() {
        let key = [0u8; 32];
        let nonce = [0u8; 12];
        let plaintext = b"secret message";
        let aad = b"aad";

        let (mut ciphertext, tag) = aes_gcm_encrypt(&key, &nonce, plaintext, aad).unwrap();

        // Tamper with ciphertext
        ciphertext[0] ^= 0xff;
        let result = aes_gcm_decrypt(&key, &nonce, &ciphertext, aad, &tag);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("decryption failed"));
    }

    #[test]
    fn aes_gcm_decrypt_tampered_tag_fails() {
        let key = [0u8; 32];
        let nonce = [0u8; 12];
        let plaintext = b"secret message";
        let aad = b"aad";

        let (ciphertext, mut tag) = aes_gcm_encrypt(&key, &nonce, plaintext, aad).unwrap();

        // Tamper with tag
        tag[0] ^= 0xff;
        let result = aes_gcm_decrypt(&key, &nonce, &ciphertext, aad, &tag);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("decryption failed"));
    }

    #[test]
    fn aes_gcm_tag_is_correct_size() {
        let key = [0u8; 32];
        let nonce = [0u8; 12];
        let plaintext = b"test";
        let aad = b"";

        let (_, tag) = aes_gcm_encrypt(&key, &nonce, plaintext, aad).unwrap();
        assert_eq!(tag.len(), 16);
    }

    #[test]
    fn aes_gcm_different_nonces_produce_different_ciphertext() {
        let key = [0u8; 32];
        let plaintext = b"same plaintext";
        let aad = b"same aad";

        let nonce1 = [0u8; 12];
        let nonce2 = [1u8; 12];

        let (ciphertext1, _) = aes_gcm_encrypt(&key, &nonce1, plaintext, aad).unwrap();
        let (ciphertext2, _) = aes_gcm_encrypt(&key, &nonce2, plaintext, aad).unwrap();

        assert_ne!(ciphertext1, ciphertext2);
    }

    // =========================================================================
    // Argon2id Tests
    // =========================================================================

    #[test]
    fn argon2id_hash_produces_deterministic_output() {
        let password = b"password123";
        let salt = b"randomsalt123456"; // 16 bytes
        let params = Argon2Params::new(1024, 1, 1, Some(32)).unwrap();

        let hash1 = argon2id_hash(password, salt, &params).unwrap();
        let hash2 = argon2id_hash(password, salt, &params).unwrap();

        assert_eq!(hash1, hash2);
        assert_eq!(hash1.len(), 32);
    }

    #[test]
    fn argon2id_hash_different_passwords_produce_different_hashes() {
        let salt = b"randomsalt123456";
        let params = Argon2Params::new(1024, 1, 1, Some(32)).unwrap();

        let hash1 = argon2id_hash(b"password1", salt, &params).unwrap();
        let hash2 = argon2id_hash(b"password2", salt, &params).unwrap();

        assert_ne!(hash1, hash2);
    }

    #[test]
    fn argon2id_hash_different_salts_produce_different_hashes() {
        let password = b"samepassword";
        let params = Argon2Params::new(1024, 1, 1, Some(32)).unwrap();

        let hash1 = argon2id_hash(password, b"salt1234567890ab", &params).unwrap();
        let hash2 = argon2id_hash(password, b"salt0987654321xy", &params).unwrap();

        assert_ne!(hash1, hash2);
    }

    #[test]
    fn argon2id_hash_respects_output_length() {
        let password = b"password";
        let salt = b"salt1234567890ab";

        let params_32 = Argon2Params::new(1024, 1, 1, Some(32)).unwrap();
        let params_64 = Argon2Params::new(1024, 1, 1, Some(64)).unwrap();

        let hash_32 = argon2id_hash(password, salt, &params_32).unwrap();
        let hash_64 = argon2id_hash(password, salt, &params_64).unwrap();

        assert_eq!(hash_32.len(), 32);
        assert_eq!(hash_64.len(), 64);
    }

    #[test]
    fn argon2id_hash_empty_password() {
        let password = b"";
        let salt = b"randomsalt123456";
        let params = Argon2Params::new(1024, 1, 1, Some(32)).unwrap();

        let result = argon2id_hash(password, salt, &params);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 32);
    }

    #[test]
    fn argon2id_hash_unicode_password() {
        let password = "日本語パスワード🔐".as_bytes();
        let salt = b"randomsalt123456";
        let params = Argon2Params::new(1024, 1, 1, Some(32)).unwrap();

        let result = argon2id_hash(password, salt, &params);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 32);
    }

    // =========================================================================
    // HKDF Tests
    // =========================================================================

    #[test]
    fn hkdf_extract_expand_produces_deterministic_output() {
        let ikm = b"input key material";
        let salt = b"salt value";
        let info = b"context info";

        let okm1 = hkdf_extract_expand(ikm, salt, info, 32).unwrap();
        let okm2 = hkdf_extract_expand(ikm, salt, info, 32).unwrap();

        assert_eq!(okm1, okm2);
        assert_eq!(okm1.len(), 32);
    }

    #[test]
    fn hkdf_extract_expand_respects_output_length() {
        let ikm = b"input key material";
        let salt = b"salt value";
        let info = b"context info";

        let okm_16 = hkdf_extract_expand(ikm, salt, info, 16).unwrap();
        let okm_64 = hkdf_extract_expand(ikm, salt, info, 64).unwrap();

        assert_eq!(okm_16.len(), 16);
        assert_eq!(okm_64.len(), 64);
    }

    #[test]
    fn hkdf_extract_expand_different_info_produces_different_output() {
        let ikm = b"input key material";
        let salt = b"salt value";

        let okm1 = hkdf_extract_expand(ikm, salt, b"info1", 32).unwrap();
        let okm2 = hkdf_extract_expand(ikm, salt, b"info2", 32).unwrap();

        assert_ne!(okm1, okm2);
    }

    #[test]
    fn hkdf_extract_expand_different_salt_produces_different_output() {
        let ikm = b"input key material";
        let info = b"context info";

        let okm1 = hkdf_extract_expand(ikm, b"salt1", info, 32).unwrap();
        let okm2 = hkdf_extract_expand(ikm, b"salt2", info, 32).unwrap();

        assert_ne!(okm1, okm2);
    }

    #[test]
    fn hkdf_extract_expand_empty_inputs() {
        let ikm = b"input key material";

        // Empty salt
        let okm1 = hkdf_extract_expand(ikm, b"", b"info", 32).unwrap();
        assert_eq!(okm1.len(), 32);

        // Empty info
        let okm2 = hkdf_extract_expand(ikm, b"salt", b"", 32).unwrap();
        assert_eq!(okm2.len(), 32);
    }

    #[test]
    fn hkdf_extract_expand_too_long_output_fails() {
        let ikm = b"input key material";
        let salt = b"salt";
        let info = b"info";

        // HKDF-SHA256 max output is 255 * 32 = 8160 bytes
        let result = hkdf_extract_expand(ikm, salt, info, 8161);
        assert!(result.is_err());
    }

    #[test]
    fn hkdf_extract_produces_deterministic_output() {
        let salt = b"salt value";
        let ikm = b"input key material";

        let prk1 = hkdf_extract(salt, ikm);
        let prk2 = hkdf_extract(salt, ikm);

        assert_eq!(prk1, prk2);
        // SHA256 output is 32 bytes
        assert_eq!(prk1.len(), 32);
    }

    #[test]
    fn hkdf_extract_different_ikm_produces_different_output() {
        let salt = b"salt value";

        let prk1 = hkdf_extract(salt, b"ikm1");
        let prk2 = hkdf_extract(salt, b"ikm2");

        assert_ne!(prk1, prk2);
    }

    #[test]
    fn hkdf_extract_different_salt_produces_different_output() {
        let ikm = b"input key material";

        let prk1 = hkdf_extract(b"salt1", ikm);
        let prk2 = hkdf_extract(b"salt2", ikm);

        assert_ne!(prk1, prk2);
    }

    #[test]
    fn hkdf_extract_empty_salt() {
        let ikm = b"input key material";

        let prk = hkdf_extract(b"", ikm);
        assert_eq!(prk.len(), 32);
    }

    #[test]
    fn hkdf_extract_empty_ikm() {
        let salt = b"salt value";

        let prk = hkdf_extract(salt, b"");
        assert_eq!(prk.len(), 32);
    }

    // =========================================================================
    // Integration: Key Derivation + Encryption
    // =========================================================================

    #[test]
    fn integration_argon2_derived_key_for_aes_gcm() {
        let password = b"user_password";
        let salt = b"randomsalt123456";
        let params = Argon2Params::new(1024, 1, 1, Some(32)).unwrap();

        // Derive key from password
        let key = argon2id_hash(password, salt, &params).unwrap();
        assert_eq!(key.len(), 32);

        // Use derived key for encryption
        let nonce = [0u8; 12];
        let plaintext = b"sensitive data";
        let aad = b"";

        let (ciphertext, tag) = aes_gcm_encrypt(&key, &nonce, plaintext, aad).unwrap();
        let decrypted = aes_gcm_decrypt(&key, &nonce, &ciphertext, aad, &tag).unwrap();

        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn integration_hkdf_derived_key_for_aes_gcm() {
        let master_secret = b"master_secret";
        let salt = b"application_salt";
        let info = b"encryption_key";

        // Derive key using HKDF
        let key = hkdf_extract_expand(master_secret, salt, info, 32).unwrap();
        assert_eq!(key.len(), 32);

        // Use derived key for encryption
        let nonce = [0u8; 12];
        let plaintext = b"sensitive data";
        let aad = b"";

        let (ciphertext, tag) = aes_gcm_encrypt(&key, &nonce, plaintext, aad).unwrap();
        let decrypted = aes_gcm_decrypt(&key, &nonce, &ciphertext, aad, &tag).unwrap();

        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn integration_extract_then_expand() {
        let salt = b"random_salt";
        let ikm = b"initial_key_material";
        let info = b"derived_key";

        // Extract then expand (standard HKDF flow)
        let prk = hkdf_extract(salt, ikm);
        let key = hkdf_extract_expand(&prk, b"", info, 32).unwrap();

        assert_eq!(key.len(), 32);

        // Verify key works for encryption
        let nonce = [0u8; 12];
        let plaintext = b"test data";
        let aad = b"";

        let (ciphertext, tag) = aes_gcm_encrypt(&key, &nonce, plaintext, aad).unwrap();
        let decrypted = aes_gcm_decrypt(&key, &nonce, &ciphertext, aad, &tag).unwrap();

        assert_eq!(decrypted, plaintext);
    }
}
