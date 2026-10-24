//! Bounded wire admission for the explicitly consented external provider.
//! Request sizing must not allocate an encoded copy of an unadmitted input.

use std::io::{self, Write};

/// Exact compact-serde JSON string size, including quotes and escaping.
/// The caller retains the original text; scratch here is constant-sized.
/// Do not replace this with character counts or an upper-bound multiplier:
/// both would change which complete inputs fit an existing byte budget.
pub(super) fn json_string_bytes(text: &str, limit: usize) -> Option<usize> {
    // Every UTF-8 input byte survives or expands, and JSON adds two quotes.
    // This lower bound also avoids scanning a giant already-over-budget input.
    if text.len() > limit.checked_sub(2)? {
        return None;
    }
    let mut size = LimitedSize { bytes: 0, limit };
    serde_json::to_writer(&mut size, text).ok()?;
    Some(size.bytes)
}

struct LimitedSize {
    bytes: usize,
    limit: usize,
}

impl Write for LimitedSize {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit - self.bytes {
            return Err(io::Error::other("external JSON exceeds its byte budget"));
        }
        self.bytes += bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::embedder::Embedder;

    #[test]
    fn string_admission_matches_the_shipping_json_encoder_at_every_boundary() {
        let mut cases = vec![
            String::new(),
            "plain ASCII / slash".into(),
            "quotes \" and backslash \\".into(),
            "\0\u{0001}\u{0008}\t\n\u{000c}\r\u{001f}".into(),
            "café 日本語 😀 e\u{301}\u{2028}\u{2029}".into(),
        ];
        cases.extend((0..=127_u8).map(|byte| char::from(byte).to_string()));
        for text in cases {
            let actual = serde_json::to_vec(&text).unwrap().len();
            assert_eq!(json_string_bytes(&text, actual - 1), None);
            assert_eq!(json_string_bytes(&text, actual), Some(actual));
            assert_eq!(json_string_bytes(&text, actual + 1), Some(actual));
            assert_eq!(json_string_bytes(&text, usize::MAX), Some(actual));
        }
    }

    #[test]
    fn every_unicode_scalar_has_the_same_size_as_serde_without_input_copies() {
        let mut utf8 = [0_u8; 4];
        for scalar in 0..=0x10ffff {
            let Some(character) = char::from_u32(scalar) else {
                continue;
            };
            let text = character.encode_utf8(&mut utf8);
            let encoded = serde_json::to_vec(&*text).unwrap();
            assert_eq!(json_string_bytes(text, encoded.len()), Some(encoded.len()));
            assert_eq!(json_string_bytes(text, encoded.len() - 1), None);
        }
    }

    #[test]
    fn escaped_and_raw_oversized_inputs_are_refused() {
        for text in ["x".repeat(4 * 1024 * 1024), "\0".repeat(1024)] {
            assert_eq!(json_string_bytes(&text, 1024), None);
        }
        assert_eq!(json_string_bytes("", 0), None);
        assert_eq!(json_string_bytes("", 1), None);
        assert_eq!(json_string_bytes("", 2), Some(2));
    }

    #[test]
    fn a_refused_write_does_not_advance_the_size_or_wrap_arithmetic() {
        let mut size = LimitedSize { bytes: 0, limit: 4 };
        size.write_all(b"abcd").unwrap();
        assert!(size.write_all(b"x").is_err());
        assert_eq!(size.bytes, 4);
        let mut size = LimitedSize {
            bytes: usize::MAX - 1,
            limit: usize::MAX,
        };
        assert!(size.write_all(b"xx").is_err());
        size.write_all(b"x").unwrap();
        assert_eq!(size.bytes, usize::MAX);
    }

    #[test]
    fn request_envelopes_and_separators_keep_exact_existing_admission() {
        use super::super::EmbeddingRequest;
        for model in ["model", "model\\with\"quotes", "日本語😀"] {
            let encode = |input: &[&str]| {
                serde_json::to_vec(&EmbeddingRequest {
                    model,
                    input,
                    encoding_format: "float",
                })
                .unwrap()
            };
            let texts = ["a", "\0\n\"\\", "δ😀"];
            let overhead = encode(&[]).len();
            for count in 1..=texts.len() {
                let expected = overhead
                    + texts[..count]
                        .iter()
                        .map(|text| json_string_bytes(text, usize::MAX).unwrap())
                        .sum::<usize>()
                    + count
                    - 1;
                assert_eq!(expected, encode(&texts[..count]).len());
            }
        }
    }

    #[test]
    fn late_invalid_input_is_refused_before_any_request_is_sent() {
        use super::super::{ExternalEmbedder, ExternalEmbeddingConfig};
        use std::net::TcpListener;
        use std::sync::{Arc, Mutex};
        use std::time::Duration;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}/v1/embeddings", listener.local_addr().unwrap());
        // Exercise the real production preflight, not connect()'s public probes.
        // Any accidental send reaches this real socket and fails the assertions.
        crate::ensure_rustls_crypto_provider();
        let config = ExternalEmbeddingConfig {
            endpoint: url::Url::parse(&endpoint).unwrap(),
            model: "fixture".into(),
            dimension: 3,
            revision: "1".into(),
            batch_size: 1,
            max_request_bytes: 1024,
            timeout: Duration::from_millis(100),
            max_retries: 0,
            api_key: None,
        };
        let provider = ExternalEmbedder {
            id: config.identity(),
            config,
            client: reqwest::blocking::Client::builder()
                .no_proxy()
                .timeout(Duration::from_millis(100))
                .build()
                .unwrap(),
            cancelled: Arc::new(|| false),
            request_gate: Mutex::new(()),
        };
        for oversized in ["x".repeat(4096), "\0".repeat(1024)] {
            let error = provider
                .embed_batch_sync(&["valid first input", &oversized])
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("external_input_too_large: input 1"),
                "{error}"
            );
            assert_eq!(
                listener.accept().unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
        }
    }
}
