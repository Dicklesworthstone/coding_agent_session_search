//! v2 transport for oversized logical rows. Each physical JSONL record still
//! fits the v1 8 MiB bound. Only a complete row is returned to existing readers;
//! chunks never become independently restorable rows or executable references.

use std::io::{self, BufRead, BufReader, Read, Write};

use anyhow::Result;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{ContinuedRow, MAX_RECORD_BYTES, MAX_ROW_BYTES, Record, read_record_bytes};

const CHUNK_BYTES: usize = 1024 * 1024;

#[derive(Serialize)]
#[serde(tag = "type", deny_unknown_fields)]
enum Frame {
    #[serde(rename = "row_start")]
    Start { bytes: usize },
    #[serde(rename = "row_chunk")]
    Chunk { sequence: usize, data: String },
    #[serde(rename = "row_end")]
    End { sha256: String },
}

#[derive(Deserialize)]
#[serde(field_identifier)]
enum FrameKind {
    #[serde(rename = "row_start")]
    Start,
    #[serde(rename = "row_chunk")]
    Chunk,
    #[serde(rename = "row_end")]
    End,
}

fn present_field<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

impl<'de> Deserialize<'de> for Frame {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            #[serde(rename = "type")]
            kind: FrameKind,
            #[serde(default, deserialize_with = "present_field")]
            bytes: Option<usize>,
            #[serde(default, deserialize_with = "present_field")]
            sequence: Option<usize>,
            #[serde(default, deserialize_with = "present_field")]
            data: Option<String>,
            #[serde(default, deserialize_with = "present_field")]
            sha256: Option<String>,
        }

        // This parser also handles the ordinary-record fallback. Never buffer
        // arbitrary JSON before discovering an unknown field or wrong scalar
        // type; absence and explicit null must remain distinct for each field.
        let fields = Fields::deserialize(deserializer)?;
        match (
            fields.kind,
            fields.bytes,
            fields.sequence,
            fields.data,
            fields.sha256,
        ) {
            (FrameKind::Start, Some(bytes), None, None, None) => Ok(Self::Start { bytes }),
            (FrameKind::Chunk, None, Some(sequence), Some(data), None) => {
                Ok(Self::Chunk { sequence, data })
            }
            (FrameKind::End, None, None, None, Some(sha256)) => Ok(Self::End { sha256 }),
            _ => Err(serde::de::Error::custom(
                "continuation type and payload disagree",
            )),
        }
    }
}

fn invalid(line: u64, message: impl std::fmt::Display) -> anyhow::Error {
    super::super::integrity(format!("logical archive record {line}: {message}"))
}

fn frame(bytes: &[u8], line: u64) -> Result<Frame> {
    serde_json::from_slice(bytes).map_err(|error| {
        invalid(
            line,
            format!("malformed continuation frame, column {}", error.column()),
        )
    })
}

fn write_frame(frame: &Frame, output: &mut impl Write) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(frame).map_err(io::Error::other)?;
    if bytes.len() >= MAX_RECORD_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "oversized continuation frame",
        ));
    }
    bytes.push(b'\n');
    output.write_all(&bytes)
}

/// Input is the canonical row JSON, including its final newline. Framing is
/// streamed: never build a second, base64-expanded copy of the complete row.
pub(super) fn write(bytes: &[u8], output: &mut impl Write) -> io::Result<()> {
    if bytes.len() <= MAX_RECORD_BYTES || bytes.len() > MAX_ROW_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid continued row size",
        ));
    }
    write_frame(&Frame::Start { bytes: bytes.len() }, output)?;
    for (sequence, chunk) in bytes.chunks(CHUNK_BYTES).enumerate() {
        write_frame(
            &Frame::Chunk {
                sequence,
                data: STANDARD.encode(chunk),
            },
            output,
        )?;
    }
    write_frame(
        &Frame::End {
            sha256: hex::encode(Sha256::digest(bytes)),
        },
        output,
    )
}

/// Serialize an admitted row without a row-sized canonical JSON allocation.
/// Size/digest admission is already complete; this pass needs only one raw
/// chunk plus its bounded base64/frame encoding. Drop never publishes an end.
pub(super) fn write_record(
    record: &Record,
    bytes: usize,
    output: &mut impl Write,
) -> io::Result<()> {
    if !matches!(record, Record::Row { .. })
        || bytes <= MAX_RECORD_BYTES
        || bytes > MAX_ROW_BYTES
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid continued row size or type",
        ));
    }
    write_frame(&Frame::Start { bytes }, output)?;
    let mut encoder = Encoder {
        output,
        remaining: bytes,
        sequence: 0,
        buffer: Vec::with_capacity(CHUNK_BYTES),
        digest: Sha256::new(),
    };
    super::serialize_record(record, bytes, &mut encoder)?;
    encoder.finish()
}

struct Encoder<'a, W> {
    output: &'a mut W,
    remaining: usize,
    sequence: usize,
    buffer: Vec<u8>,
    digest: Sha256,
}

impl<W: Write> Encoder<'_, W> {
    fn emit_chunk(&mut self) -> io::Result<()> {
        write_frame(
            &Frame::Chunk {
                sequence: self.sequence,
                data: STANDARD.encode(&self.buffer),
            },
            self.output,
        )?;
        self.digest.update(&self.buffer);
        self.sequence += 1;
        self.buffer.clear();
        Ok(())
    }

    fn finish(mut self) -> io::Result<()> {
        if self.remaining != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "continued row did not reach its validated size",
            ));
        }
        if !self.buffer.is_empty() {
            self.emit_chunk()?;
        }
        write_frame(
            &Frame::End {
                sha256: hex::encode(self.digest.finalize()),
            },
            self.output,
        )
    }
}

impl<W: Write> Write for Encoder<'_, W> {
    fn write(&mut self, mut bytes: &[u8]) -> io::Result<usize> {
        let count = bytes.len();
        if count > self.remaining {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "continued row exceeded its validated size",
            ));
        }
        while !bytes.is_empty() {
            let take = bytes.len().min(CHUNK_BYTES - self.buffer.len());
            self.buffer.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if self.buffer.len() == CHUNK_BYTES {
                self.emit_chunk()?;
            }
        }
        self.remaining -= count;
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        // A short final chunk is legal only after all declared bytes arrive.
        // Only finish() seals it; serde serialization does not call flush().
        self.output.flush()
    }
}

/// A bounded decoder for one group. Only one decoded 1 MiB chunk is retained;
/// serde constructs the logical row directly, rather than first retaining a
/// second complete copy of its encoded JSON. Binding that row into SQLite and
/// canonical digest validation still require memory proportional to one row.
struct Chunks<'a, R> {
    reader: &'a mut R,
    line: u64,
    remaining: usize,
    sequence: usize,
    buffer: Vec<u8>,
    position: usize,
    digest: Sha256,
    finished: bool,
    failure: Option<anyhow::Error>,
}

impl<R: BufRead> Chunks<'_, R> {
    fn advance(&mut self) -> Result<()> {
        let bytes = read_record_bytes(self.reader, self.line)?
            .ok_or_else(|| invalid(self.line, "incomplete continued row"))?;
        let next = frame(&bytes, self.line)?;
        if self.remaining == 0 {
            let Frame::End { sha256 } = next else {
                return Err(invalid(
                    self.line,
                    "expected row_end after the declared row bytes",
                ));
            };
            if sha256 != hex::encode(self.digest.clone().finalize()) {
                return Err(invalid(self.line, "continued row checksum mismatch"));
            }
            self.finished = true;
            self.buffer.clear();
            self.position = 0;
            return Ok(());
        }
        let Frame::Chunk { sequence, data } = next else {
            return Err(invalid(self.line, "expected the next row_chunk"));
        };
        if sequence != self.sequence {
            return Err(invalid(
                self.line,
                "missing, duplicate or unordered row_chunk",
            ));
        }
        // Canonical chunk sizes bound both allocation and frame count (at
        // most 256), including hostile streams made of tiny or empty chunks.
        let expected = self.remaining.min(CHUNK_BYTES);
        let encoded = expected.div_ceil(3) * 4;
        if data.len() != encoded {
            return Err(invalid(self.line, "row_chunk has the wrong encoded length"));
        }
        let decoded = STANDARD
            .decode(data)
            .map_err(|_| invalid(self.line, "invalid row_chunk base64"))?;
        if decoded.len() != expected {
            return Err(invalid(self.line, "row_chunk has the wrong decoded length"));
        }
        self.digest.update(&decoded);
        self.remaining -= expected;
        self.sequence += 1;
        self.buffer = decoded;
        self.position = 0;
        Ok(())
    }
}

impl<R: BufRead> Read for Chunks<'_, R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() || self.finished {
            return Ok(0);
        }
        if self.failure.is_some() {
            return Err(io::Error::other("logical continuation read failed"));
        }
        if self.position == self.buffer.len()
            && let Err(error) = self.advance()
        {
            // serde wraps reader errors. Keep the original typed cause out
            // of band so storage I/O remains retryable, not "corrupt input".
            self.failure = Some(error);
            return Err(io::Error::other("logical continuation read failed"));
        }
        let count = output.len().min(self.buffer.len() - self.position);
        output[..count].copy_from_slice(&self.buffer[self.position..self.position + count]);
        self.position += count;
        Ok(count)
    }
}

/// Count canonical bytes without allocating another complete serialized row.
struct Size(usize);

impl Write for Size {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_ROW_BYTES.saturating_sub(self.0) {
            return Err(io::Error::other("continued row exceeds 256 MiB"));
        }
        self.0 += bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn read(reader: &mut impl BufRead, start: &[u8], line: u64) -> Result<Record> {
    let Frame::Start { bytes } = frame(start, line)? else {
        return Err(invalid(line, "continuation without row_start"));
    };
    if bytes <= MAX_RECORD_BYTES || bytes > MAX_ROW_BYTES {
        return Err(invalid(
            line,
            "continued row must exceed 8 MiB and fit within 256 MiB",
        ));
    }
    let mut chunks = Chunks {
        reader,
        line,
        remaining: bytes,
        sequence: 0,
        buffer: Vec::new(),
        position: 0,
        digest: Sha256::new(),
        finished: false,
        failure: None,
    };
    let decoded = serde_json::from_reader::<_, ContinuedRow>(BufReader::with_capacity(
        64 * 1024,
        &mut chunks,
    ));
    if let Some(error) = chunks.failure.take() {
        return Err(error);
    }
    let ContinuedRow(record) = decoded.map_err(|error| {
        invalid(
            line,
            format!("malformed continued row, column {}", error.column()),
        )
    })?;
    if !chunks.finished || chunks.remaining != 0 {
        return Err(invalid(line, "continued row was not completed"));
    }
    if !matches!(record, Record::Row { .. }) {
        return Err(invalid(line, "only logical rows may use continuations"));
    }
    let mut size = Size(1); // Count the canonical mandatory newline too.
    serde_json::to_writer(&mut size, &record)
        .map_err(|_| invalid(line, "continued row exceeds its canonical byte budget"))?;
    if size.0 <= MAX_RECORD_BYTES {
        // This also prevents accepting continuation frames under a v1 header:
        // every reconstructed row must exceed v1's unchanged Validator bound.
        // Whitespace-padding a small row cannot bypass that version admission.
        return Err(invalid(
            line,
            "a single-record row must not use continuations",
        ));
    }
    Ok(record)
}

#[cfg(test)]
#[path = "chunked_tests.rs"]
mod tests;
