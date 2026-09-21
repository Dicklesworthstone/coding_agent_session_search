//! Bounded JSON-lines transport. Budget failures never publish partial frames.
//! Transport I/O errors terminate the session.

use std::io::{self, BufRead, Write};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub(super) const VERSION: u32 = 1;
pub(super) const MAX_REQUEST_BYTES: usize = 64 * 1024;
pub(super) const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
pub(super) const MAX_QUERY_BYTES: usize = 4096;
pub(super) const MAX_LIMIT: usize = 100;
pub(super) const MAX_WINDOW: usize = 1024;
pub(super) const MAX_FILTERS: usize = 32;
pub(super) const MAX_IDENTITY_BYTES: usize = 4096;

fn default_limit() -> usize {
    10
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Filters {
    #[serde(default)]
    pub agents: Vec<String>,
    #[serde(default)]
    pub workspaces: Vec<String>,
    pub source_id: Option<String>,
    pub created_from: Option<i64>,
    pub created_to: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Request {
    Search {
        id: u64,
        query: String,
        #[serde(default = "default_limit")]
        limit: usize,
        #[serde(default)]
        offset: usize,
        #[serde(default)]
        filters: Filters,
    },
    Status {
        id: u64,
    },
    Reload {
        id: u64,
    },
    Shutdown {
        id: u64,
    },
}

#[derive(Debug, Serialize)]
pub(super) struct Failure {
    pub kind: &'static str,
    pub message: String,
}

#[derive(Debug, Serialize)]
pub(super) struct Reply {
    pub schema_version: u32,
    pub id: Option<u64>,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Failure>,
}

impl Reply {
    pub fn success(id: u64, result: Value) -> Self {
        Self {
            schema_version: VERSION,
            id: Some(id),
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    pub fn failure(id: Option<u64>, kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            schema_version: VERSION,
            id,
            ok: false,
            result: None,
            error: Some(Failure {
                kind,
                message: message.into(),
            }),
        }
    }
}

pub(super) fn validate_search(
    query: &str,
    limit: usize,
    offset: usize,
    filters: &Filters,
) -> Result<(), &'static str> {
    if query.trim().is_empty() || query.len() > MAX_QUERY_BYTES {
        return Err("query must be nonempty and at most 4096 UTF-8 bytes");
    }
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err("limit must be between 1 and 100; unlimited requests are not supported");
    }
    if offset
        .checked_add(limit)
        .and_then(|window| window.checked_add(1))
        .is_none_or(|window| window > MAX_WINDOW)
    {
        return Err("offset + limit + 1 must not exceed the 1024-hit page window");
    }
    for values in [&filters.agents, &filters.workspaces] {
        if values.len() > MAX_FILTERS {
            return Err("at most 32 values are allowed in each filter");
        }
        if values
            .iter()
            .any(|value| value.trim().is_empty() || value.len() > MAX_IDENTITY_BYTES)
        {
            return Err("filter values must be nonempty and at most 4096 UTF-8 bytes");
        }
    }
    if filters.source_id.as_ref().is_some_and(|value| {
        value.trim().is_empty() || value != value.trim() || value.len() > MAX_IDENTITY_BYTES
    }) {
        return Err("source_id must be an unpadded, nonempty exact ID of at most 4096 bytes");
    }
    if let (Some(from), Some(to)) = (filters.created_from, filters.created_to)
        && from > to
    {
        return Err("created_from must not exceed created_to");
    }
    Ok(())
}

pub(super) enum Frame {
    Line(Vec<u8>),
    End,
    TooLarge,
}

/// Bound allocation before deserialization. An oversized frame terminates the
/// session rather than draining an attacker-controlled, potentially endless line.
pub(super) fn read_frame(input: &mut impl BufRead) -> io::Result<Frame> {
    let mut line = Vec::new();
    loop {
        let available = input.fill_buf()?;
        if available.is_empty() {
            return Ok(if line.is_empty() {
                Frame::End
            } else {
                Frame::Line(line)
            });
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let count = newline.unwrap_or(available.len());
        if count > MAX_REQUEST_BYTES - line.len() {
            return Ok(Frame::TooLarge);
        }
        line.extend_from_slice(&available[..count]);
        input.consume(count + usize::from(newline.is_some()));
        if newline.is_some() {
            return Ok(Frame::Line(line));
        }
    }
}

struct LimitedBuffer(Vec<u8>);

impl Write for LimitedBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        // Reserve one byte for the line terminator, including escaped JSON bytes.
        if bytes.len() > (MAX_RESPONSE_BYTES - 1) - self.0.len() {
            return Err(io::Error::other("search service response exceeds its byte limit"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Encode one complete bounded frame before publishing any bytes to the peer.
pub(super) fn encode_line(value: &impl Serialize) -> io::Result<Vec<u8>> {
    let mut buffer = LimitedBuffer(Vec::new());
    serde_json::to_writer(&mut buffer, value).map_err(io::Error::other)?;
    buffer.0.push(b'\n');
    Ok(buffer.0)
}

pub(super) fn write_reply(output: &mut impl Write, reply: &Reply) -> io::Result<()> {
    let bytes = encode_line(reply).or_else(|_| {
        // No partial result has reached stdout. Never truncate identity fields.
        encode_line(&Reply::failure(
            reply.id,
            "response_too_large",
            "response exceeded the 1 MiB encoded limit; request fewer hits",
        ))
    })?;
    output.write_all(&bytes)?;
    output.flush()
}
