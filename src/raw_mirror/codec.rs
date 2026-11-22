//! Bounded, independently decodable zstd chunks for the raw mirror.
//!
//! The store authenticates both the encoded blob and the decoded source bytes.
//! This layer enforces the codec's resource limits and exact frame boundaries.

use anyhow::{Context, Result, ensure};
use std::io::Read;

pub(super) const MAX_DECODED_CHUNK_BYTES: usize = 4 * 1024 * 1024;
pub(super) const MAX_ENCODED_CHUNK_BYTES: usize = MAX_DECODED_CHUNK_BYTES + 64 * 1024;

const MAX_WINDOW_LOG: u32 = 22;
const ZSTD_FRAME_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];

pub(super) fn compress_chunk(bytes: &[u8]) -> Result<Vec<u8>> {
    ensure!(
        bytes.len() <= MAX_DECODED_CHUNK_BYTES,
        "raw mirror decoded chunk exceeds the {MAX_DECODED_CHUNK_BYTES} byte limit"
    );
    let mut compressor =
        zstd::bulk::Compressor::new(3).context("creating raw mirror zstd compressor")?;
    compressor
        .window_log(MAX_WINDOW_LOG)
        .context("setting raw mirror compression window limit")?;
    // compress() reserves zstd's compression bound for this already-bounded
    // source, including the overhead of an incompressible chunk.
    let encoded = compressor
        .compress(bytes)
        .context("compressing raw mirror chunk")?;
    ensure!(
        encoded.len() <= MAX_ENCODED_CHUNK_BYTES,
        "raw mirror encoded chunk exceeds the {MAX_ENCODED_CHUNK_BYTES} byte limit"
    );
    Ok(encoded)
}

pub(super) fn decode_chunk(bytes: &[u8], expected_size: u64) -> Result<Vec<u8>> {
    ensure!(
        bytes.len() <= MAX_ENCODED_CHUNK_BYTES,
        "raw mirror encoded chunk exceeds the {MAX_ENCODED_CHUNK_BYTES} byte limit"
    );
    ensure!(
        expected_size <= MAX_DECODED_CHUNK_BYTES as u64,
        "raw mirror decoded chunk exceeds the {MAX_DECODED_CHUNK_BYTES} byte limit"
    );
    // Skippable frames are not encoded source chunks, even when an empty
    // logical payload would otherwise make one appear valid.
    ensure!(
        bytes.starts_with(&ZSTD_FRAME_MAGIC),
        "raw mirror chunk is not a zstd data frame"
    );
    let expected = usize::try_from(expected_size).context("converting raw mirror chunk length")?;
    // A borrowed slice implements BufRead directly. Unlike a BufReader over a
    // Cursor, finish() therefore exposes precisely the unconsumed input.
    let mut decoder = zstd::stream::read::Decoder::with_buffer(bytes)
        .context("creating raw mirror zstd decoder")?
        .single_frame();
    decoder
        .window_log_max(MAX_WINDOW_LOG)
        .context("setting raw mirror decompression window limit")?;
    let mut decoded = Vec::with_capacity(expected + 1);
    // Read one sentinel byte beyond the declared size. This both bounds an
    // expanding frame and forces a correctly sized payload to reach frame EOF,
    // including any footer, rather than accepting a truncated prefix.
    decoder
        .by_ref()
        .take(expected_size + 1)
        .read_to_end(&mut decoded)
        .context("decoding raw mirror zstd chunk")?;
    ensure!(
        decoded.len() == expected,
        "raw mirror decoded chunk length mismatch: expected {expected} bytes, decoded {} bytes",
        decoded.len()
    );
    ensure!(
        decoder.finish().is_empty(),
        "raw mirror zstd chunk contains trailing bytes or concatenated frames"
    );
    Ok(decoded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn roundtrips_empty_binary_and_unicode_payloads() -> Result<()> {
        let binary = (0..=255).collect::<Vec<u8>>();
        for bytes in [
            b"".as_slice(),
            binary.as_slice(),
            "{\"message\":\"café 日本語 🦀\\nsecond line\"}\n".as_bytes(),
        ] {
            let encoded = compress_chunk(bytes)?;
            assert!(encoded.starts_with(&ZSTD_FRAME_MAGIC));
            assert_eq!(decode_chunk(&encoded, bytes.len() as u64)?, bytes);
        }
        Ok(())
    }

    #[test]
    fn repetitive_transcript_uses_substantially_fewer_physical_bytes() -> Result<()> {
        let transcript =
            b"{\"role\":\"assistant\",\"content\":\"Inspecting the source and checking the result.\"}\n"
                .repeat(4096);
        let encoded = compress_chunk(&transcript)?;
        assert!(encoded.len() * 10 < transcript.len());
        assert_eq!(
            decode_chunk(&encoded, transcript.len() as u64)?,
            transcript
        );
        Ok(())
    }

    #[test]
    fn maximum_incompressible_chunk_roundtrips_with_bounded_overhead() -> Result<()> {
        let mut bytes = Vec::with_capacity(MAX_DECODED_CHUNK_BYTES);
        let mut state = 0x2d35_8dcc_aa6c_78a5_u64;
        for _ in 0..MAX_DECODED_CHUNK_BYTES {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            bytes.push((state >> 32) as u8);
        }
        let encoded = compress_chunk(&bytes)?;
        assert!(encoded.len() > bytes.len(), "fixture must be incompressible");
        assert!(encoded.len() <= MAX_ENCODED_CHUNK_BYTES);
        assert_eq!(decode_chunk(&encoded, bytes.len() as u64)?, bytes);
        Ok(())
    }

    #[test]
    fn rejects_input_and_declared_output_beyond_the_limits() -> Result<()> {
        let oversized_source = vec![0; MAX_DECODED_CHUNK_BYTES + 1];
        assert!(compress_chunk(&oversized_source).is_err());
        let oversized_encoded = vec![0; MAX_ENCODED_CHUNK_BYTES + 1];
        assert!(decode_chunk(&oversized_encoded, 0).is_err());
        let encoded = compress_chunk(b"bounded")?;
        for expected in [MAX_DECODED_CHUNK_BYTES as u64 + 1, u64::MAX] {
            assert!(decode_chunk(&encoded, expected).is_err());
        }
        Ok(())
    }

    #[test]
    fn rejects_wrong_logical_lengths() -> Result<()> {
        let bytes = b"exact source bytes";
        let encoded = compress_chunk(bytes)?;
        for expected in [0, bytes.len() as u64 - 1, bytes.len() as u64 + 1] {
            assert!(decode_chunk(&encoded, expected).is_err());
        }
        Ok(())
    }

    #[test]
    fn rejects_every_truncated_prefix_of_a_complete_frame() -> Result<()> {
        let bytes = b"a complete source chunk must include its complete compressed frame";
        let encoded = compress_chunk(bytes)?;
        for end in 0..encoded.len() {
            assert!(
                decode_chunk(&encoded[..end], bytes.len() as u64).is_err(),
                "accepted truncated prefix of {end}/{} encoded bytes",
                encoded.len()
            );
        }
        assert_eq!(decode_chunk(&encoded, bytes.len() as u64)?, bytes);
        Ok(())
    }

    #[test]
    fn rejects_trailing_payloads_and_concatenated_frames() -> Result<()> {
        let bytes = b"first frame";
        let encoded = compress_chunk(bytes)?;
        for suffix in [
            vec![0],
            b"unframed trailing bytes".to_vec(),
            compress_chunk(b"second frame")?,
            compress_chunk(b"")?,
        ] {
            let mut joined = encoded.clone();
            joined.extend_from_slice(&suffix);
            assert!(decode_chunk(&joined, bytes.len() as u64).is_err());
        }
        let mut empty_then_data = compress_chunk(b"")?;
        empty_then_data.extend_from_slice(&encoded);
        assert!(decode_chunk(&empty_then_data, 0).is_err());
        Ok(())
    }

    #[test]
    fn rejects_unencoded_data_and_skippable_frames() {
        for bytes in [
            b"not compressed".as_slice(),
            // Zstd's skippable-frame magic plus a zero-byte payload length.
            &[0x50, 0x2a, 0x4d, 0x18, 0, 0, 0, 0],
        ] {
            assert!(decode_chunk(bytes, 0).is_err());
        }
    }

    #[test]
    fn rejects_a_valid_frame_requesting_an_excessive_window() -> Result<()> {
        // Non-single-segment frame, unknown content size, one final empty raw
        // block. Window descriptors 0x60/0x68 declare 2^22/2^23 bytes.
        let allowed = [0x28, 0xb5, 0x2f, 0xfd, 0, 0x60, 1, 0, 0];
        let oversized = [0x28, 0xb5, 0x2f, 0xfd, 0, 0x68, 1, 0, 0];
        assert!(decode_chunk(&allowed, 0)?.is_empty());
        let mut permissive = zstd::stream::read::Decoder::with_buffer(&oversized[..])?;
        permissive.window_log_max(MAX_WINDOW_LOG + 1)?;
        let mut control = Vec::new();
        permissive.read_to_end(&mut control)?;
        assert!(control.is_empty(), "control must be a valid empty frame");
        assert!(decode_chunk(&oversized, 0).is_err());
        Ok(())
    }

    #[test]
    fn expansion_bomb_stops_after_one_sentinel_byte() -> Result<()> {
        let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 3)?;
        encoder.window_log(MAX_WINDOW_LOG)?;
        let block = [b'x'; 64 * 1024];
        // Streaming omits the content-size hint. Its valid 4 MiB window must
        // not let an 8 MiB decoded payload escape the independent output cap.
        for _ in 0..128 {
            encoder.write_all(&block)?;
        }
        let encoded = encoder.finish()?;
        assert!(encoded.len() < MAX_ENCODED_CHUNK_BYTES);
        let error = decode_chunk(&encoded, MAX_DECODED_CHUNK_BYTES as u64)
            .expect_err("oversized logical payload must be refused");
        assert!(
            error.to_string().contains(&format!(
                "decoded {} bytes",
                MAX_DECODED_CHUNK_BYTES + 1
            )),
            "decoder must stop at the sentinel instead of expanding the entire frame: {error:#}"
        );
        Ok(())
    }
}
