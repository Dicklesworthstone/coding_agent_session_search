//! INV-cass-10 — AES-GCM AEAD + HKDF-SHA256 round-trip properties.
//!
//! Property-based coverage for the production crypto primitives in
//! `coding_agent_search::encryption`. The existing `tests/crypto_vectors.rs`
//! pins a handful of *known-answer* NIST/RFC test vectors; this file pins
//! the *universal* contracts those primitives must satisfy for every input:
//!
//!   1. **AES-256-GCM round-trip soundness** — for any (key, nonce, plaintext,
//!      aad), `decrypt(encrypt(...))` returns the original plaintext.
//!   2. **AEAD ciphertext authentication** — flipping any bit of the
//!      ciphertext causes decryption to fail (not silently return garbage).
//!   3. **AEAD AAD authentication** — changing the associated-data input
//!      causes decryption to fail. AAD binds context to the ciphertext.
//!   4. **AEAD key authentication** — decrypting with the wrong key fails.
//!      Combined with (2)/(3) this exercises all three AEAD bindings.
//!   5. **HKDF-SHA256 determinism** — same inputs produce the same output.
//!      Key-derivation must be a pure function; non-determinism here would
//!      break every cass crypto-asset that depends on a re-derived KEK
//!      (`pages/encrypt.rs::1241`, `pages/key_management.rs::506`).
//!   6. **HKDF-SHA256 output length contract** — the realized output length
//!      equals the requested length, for every length in the supported
//!      range. A short return would silently truncate downstream keys.
//!
//! These properties are not redundant with the test-vector golden file:
//! the golden file proves "we agree with NIST on these N points" while this
//! file proves "the binding holds across the input space".

use coding_agent_search::encryption::{aes_gcm_decrypt, aes_gcm_encrypt, hkdf_extract_expand};
use proptest::prelude::*;

/// AES-256-GCM constants. The encryption module rejects any other length.
const AES_GCM_KEY_LEN: usize = 32;
const AES_GCM_NONCE_LEN: usize = 12;

/// Plaintext/AAD upper bounds for proptest. Kept modest so the run stays
/// fast (AES-GCM is cheap; per-case cost dominates only when shrinking).
const MAX_PLAINTEXT_LEN: usize = 1024;
const MAX_AAD_LEN: usize = 256;

/// HKDF-SHA256 caps the output length at 8160 bytes (255 * HashLen).
/// Property cases stay well under that to keep ring-internal allocations
/// bounded.
const MAX_HKDF_OUTPUT_LEN: usize = 256;

fn key_strategy() -> impl Strategy<Value = [u8; AES_GCM_KEY_LEN]> {
    prop::array::uniform32(any::<u8>())
}

fn nonce_strategy() -> impl Strategy<Value = [u8; AES_GCM_NONCE_LEN]> {
    [any::<u8>(); AES_GCM_NONCE_LEN]
}

fn plaintext_strategy() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 0..=MAX_PLAINTEXT_LEN)
}

fn aad_strategy() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 0..=MAX_AAD_LEN)
}

proptest! {
    // 256 cases is enough to surface a true round-trip break (1 mismatch in
    // expectation would shrink to the minimal failing input). Higher counts
    // mostly burn CI time on already-validated combinations.
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn aes_gcm_roundtrip_returns_plaintext(
        key in key_strategy(),
        nonce in nonce_strategy(),
        plaintext in plaintext_strategy(),
        aad in aad_strategy(),
    ) {
        let (ciphertext, tag) =
            aes_gcm_encrypt(&key, &nonce, &plaintext, &aad).expect("encrypt should succeed");
        // Authenticated cipher: ciphertext length equals plaintext length; tag is separate.
        prop_assert_eq!(ciphertext.len(), plaintext.len(),
            "AES-GCM ciphertext should match plaintext length (tag returned separately)");
        prop_assert_eq!(tag.len(), 16,
            "AES-256-GCM tag must be exactly 16 bytes");

        let recovered = aes_gcm_decrypt(&key, &nonce, &ciphertext, &aad, &tag)
            .expect("decrypt with matching params should succeed");
        prop_assert_eq!(recovered, plaintext,
            "round-trip must return the exact original plaintext");
    }

    #[test]
    fn aes_gcm_decrypt_rejects_tampered_ciphertext(
        key in key_strategy(),
        nonce in nonce_strategy(),
        plaintext in prop::collection::vec(any::<u8>(), 1..=MAX_PLAINTEXT_LEN),
        aad in aad_strategy(),
        tamper_byte_idx in any::<u32>(),
        tamper_bit_mask in 1u8..=u8::MAX,
    ) {
        let (mut ciphertext, tag) =
            aes_gcm_encrypt(&key, &nonce, &plaintext, &aad).expect("encrypt should succeed");
        // Plaintext is non-empty so ciphertext is non-empty; flip a guaranteed-real bit.
        let idx = (tamper_byte_idx as usize) % ciphertext.len();
        ciphertext[idx] ^= tamper_bit_mask; // nonzero mask guarantees a real change

        let result = aes_gcm_decrypt(&key, &nonce, &ciphertext, &aad, &tag);
        prop_assert!(result.is_err(),
            "decrypt should reject ciphertext tampered at byte {}", idx);
    }

    #[test]
    fn aes_gcm_decrypt_rejects_wrong_aad(
        key in key_strategy(),
        nonce in nonce_strategy(),
        plaintext in plaintext_strategy(),
        aad in aad_strategy(),
        wrong_aad in aad_strategy(),
    ) {
        // Skip the (unlikely) random collision where the two AADs are identical;
        // the contract only constrains *different* AADs.
        prop_assume!(wrong_aad != aad);

        let (ciphertext, tag) =
            aes_gcm_encrypt(&key, &nonce, &plaintext, &aad).expect("encrypt should succeed");
        let result = aes_gcm_decrypt(&key, &nonce, &ciphertext, &wrong_aad, &tag);
        prop_assert!(result.is_err(),
            "decrypt with a different AAD must fail (AAD authenticates context binding)");
    }

    #[test]
    fn aes_gcm_decrypt_rejects_wrong_key(
        key in key_strategy(),
        wrong_key in key_strategy(),
        nonce in nonce_strategy(),
        plaintext in plaintext_strategy(),
        aad in aad_strategy(),
    ) {
        prop_assume!(wrong_key != key);

        let (ciphertext, tag) =
            aes_gcm_encrypt(&key, &nonce, &plaintext, &aad).expect("encrypt should succeed");
        let result = aes_gcm_decrypt(&wrong_key, &nonce, &ciphertext, &aad, &tag);
        prop_assert!(result.is_err(),
            "decrypt with a wrong key must fail");
    }
}

proptest! {
    // HKDF is even cheaper than AES-GCM; 256 cases stay comfortably within CI budget.
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn hkdf_extract_expand_is_deterministic(
        ikm in prop::collection::vec(any::<u8>(), 1..=256),
        salt in prop::collection::vec(any::<u8>(), 0..=64),
        info in prop::collection::vec(any::<u8>(), 0..=64),
        len in 1usize..=MAX_HKDF_OUTPUT_LEN,
    ) {
        let first = hkdf_extract_expand(&ikm, &salt, &info, len).expect("hkdf 1");
        let second = hkdf_extract_expand(&ikm, &salt, &info, len).expect("hkdf 2");
        prop_assert_eq!(first, second,
            "HKDF-SHA256 must be deterministic for identical inputs");
    }

    #[test]
    fn hkdf_extract_expand_output_length_matches_request(
        ikm in prop::collection::vec(any::<u8>(), 1..=256),
        salt in prop::collection::vec(any::<u8>(), 0..=64),
        info in prop::collection::vec(any::<u8>(), 0..=64),
        len in 1usize..=MAX_HKDF_OUTPUT_LEN,
    ) {
        let okm = hkdf_extract_expand(&ikm, &salt, &info, len).expect("hkdf");
        prop_assert_eq!(okm.len(), len,
            "HKDF output length must match the requested length");
    }
}
