//! Versioned, bounded JSONL framing. No SQL or filesystem paths are executable data.

use std::collections::BTreeMap;
use std::io::{self, BufRead, Write};

use anyhow::{Result, anyhow, bail, ensure};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[path = "chunked.rs"]
mod chunked;

pub const FORMAT: &str = "cass.logical_archive";
/// Version 1 remains readable, with its original per-row limit and digest.
pub const VERSION: u32 = 1;
pub const CHUNKED_VERSION: u32 = 2;
pub const MAX_RECORD_BYTES: usize = 8 * 1024 * 1024;
/// Separate from the wire-frame bound. Reconstruction and SQLite parameter
/// binding require one complete logical row; never assemble an unbounded row.
pub const MAX_ROW_BYTES: usize = 256 * 1024 * 1024;
pub const MAX_TABLES: usize = 256;
pub const MAX_COLUMNS: usize = 256;
const MAX_KEY_BYTES: usize = 64 * 1024;

pub fn record_types(version: u32) -> Vec<String> {
    let names: &[&str] = if version == CHUNKED_VERSION {
        &[
            "table",
            "row",
            "row_start",
            "row_chunk",
            "row_end",
            "completion",
        ]
    } else {
        &["table", "row", "completion"]
    };
    names.iter().map(|name| (*name).to_owned()).collect()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Header {
    pub format: String,
    pub schema_version: u32,
    pub archive_id: String,
    pub exported_at_ms: i64,
    pub storage_schema_version: String,
    pub record_types: Vec<String>,
    pub contains_private_data: bool,
    pub omissions: Vec<String>,
}

impl Header {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.format == FORMAT, "unknown logical archive format");
        ensure!(
            matches!(self.schema_version, VERSION | CHUNKED_VERSION),
            "unsupported logical archive version"
        );
        ensure!(
            !self.archive_id.is_empty()
                && self.archive_id.len() <= 128
                && self
                    .archive_id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)),
            "archive identity must contain 1 to 128 ASCII letters, digits, hyphens or underscores"
        );
        ensure!(
            !self.storage_schema_version.is_empty()
                && self.storage_schema_version.len() <= 16
                && self
                    .storage_schema_version
                    .bytes()
                    .all(|b| b.is_ascii_digit()),
            "invalid canonical storage schema version"
        );
        ensure!(
            self.record_types == record_types(self.schema_version),
            "unsupported record types"
        );
        ensure!(
            self.contains_private_data,
            "redacted archives are not lossless restoration inputs"
        );
        ensure!(
            self.omissions == ["derived_search_assets"],
            "unsupported archive omissions"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Table {
    pub name: String,
    pub columns: Vec<String>,
    /// Column offsets in the source's declared primary-key order.
    pub primary_key: Vec<usize>,
}

impl Table {
    pub fn validate(&self) -> Result<()> {
        ensure!(identifier(&self.name), "invalid logical table identifier");
        ensure!(
            !self.columns.is_empty() && self.columns.len() <= MAX_COLUMNS,
            "unsupported logical column count"
        );
        let mut columns = std::collections::BTreeSet::new();
        for column in &self.columns {
            ensure!(
                identifier(column) && columns.insert(column),
                "invalid or duplicate logical column"
            );
        }
        ensure!(
            !self.primary_key.is_empty(),
            "logical tables require a declared primary key"
        );
        let mut keys = std::collections::BTreeSet::new();
        for &key in &self.primary_key {
            ensure!(
                key < self.columns.len() && keys.insert(key),
                "invalid logical primary key"
            );
        }
        Ok(())
    }
}

pub fn identifier(value: &str) -> bool {
    let mut bytes = value.bytes();
    value.len() <= 128
        && bytes
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// REAL is an exact, lowercase IEEE-754 binary64 bit string; JSON's number
/// rounding cannot change a restored value. Integers remain signed 64-bit.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Cell {
    Null,
    Integer(i64),
    Real(String),
    Text(String),
    Blob(String),
}

#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "snake_case")]
enum CellKind {
    Null,
    Integer,
    Real,
    Text,
    Blob,
}

enum Scalar {
    Integer(i64),
    Text(String),
}

impl<'de> Deserialize<'de> for Scalar {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ScalarVisitor;

        impl Visitor<'_> for ScalarVisitor {
            type Value = Scalar;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a signed 64-bit integer or a string")
            }

            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Scalar, E> {
                Ok(Scalar::Integer(value))
            }

            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Scalar, E> {
                i64::try_from(value)
                    .map(Scalar::Integer)
                    .map_err(|_| E::custom("logical integer is outside the signed 64-bit range"))
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Scalar, E> {
                Ok(Scalar::Text(value.to_owned()))
            }

            fn visit_string<E: de::Error>(self, value: String) -> Result<Scalar, E> {
                Ok(Scalar::Text(value))
            }
        }

        // Reject arrays, objects, booleans and floating-point numbers at their
        // first token, including when the value precedes its kind tag.
        deserializer.deserialize_any(ScalarVisitor)
    }
}

impl<'de> Deserialize<'de> for Cell {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            kind: CellKind,
            value: Option<Scalar>,
        }

        let fields = Fields::deserialize(deserializer)?;
        match (fields.kind, fields.value) {
            (CellKind::Null, None) => Ok(Self::Null),
            (CellKind::Integer, Some(Scalar::Integer(value))) => Ok(Self::Integer(value)),
            (CellKind::Real, Some(Scalar::Text(value))) => Ok(Self::Real(value)),
            (CellKind::Text, Some(Scalar::Text(value))) => Ok(Self::Text(value)),
            (CellKind::Blob, Some(Scalar::Text(value))) => Ok(Self::Blob(value)),
            _ => Err(de::Error::custom(
                "logical cell kind and scalar value disagree",
            )),
        }
    }
}

impl Cell {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Real(bits) => {
                ensure!(
                    bits.len() == 16
                        && bits
                            .bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                    "invalid REAL encoding"
                );
                let bits =
                    u64::from_str_radix(bits, 16).map_err(|_| anyhow!("invalid REAL encoding"))?;
                ensure!(
                    f64::from_bits(bits).is_finite(),
                    "non-finite REAL is not portable"
                );
            }
            Self::Blob(encoded) => {
                ensure!(encoded.len() <= MAX_ROW_BYTES, "oversized BLOB encoding");
                // Validation must not allocate a decoded copy of a potentially
                // 192 MiB BLOB only to throw it away. Every nonfinal block ends
                // on a base64 quartet and must not contain padding. The same
                // STANDARD engine checks the final block's padding/tail bits.
                let mut decoded = [0_u8; 3 * 1024];
                let mut blocks = encoded.as_bytes().chunks(4 * 1024).peekable();
                while let Some(block) = blocks.next() {
                    ensure!(
                        blocks.peek().is_none() || !block.contains(&b'='),
                        "invalid BLOB encoding"
                    );
                    STANDARD
                        .decode_slice(block, &mut decoded)
                        .map_err(|_| anyhow!("invalid BLOB encoding"))?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Completion {
    pub records: u64,
    pub tables: BTreeMap<String, u64>,
    pub content_sha256: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Record {
    Header { header: Header },
    Table { table: Table },
    Row { values: Vec<Cell> },
    Completion { completion: Completion },
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(field_identifier, rename_all = "snake_case")]
enum RecordKind {
    Header,
    Table,
    Row,
    Completion,
}

#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "snake_case")]
enum RecordField {
    Type,
    Header,
    Table,
    Values,
    Completion,
}

struct RowValues;

impl<'de> DeserializeSeed<'de> for RowValues {
    type Value = Vec<Cell>;

    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_seq(self)
    }
}

impl<'de> Visitor<'de> for RowValues {
    type Value = Vec<Cell>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("at most 256 logical cells")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Self::Value, A::Error> {
        struct NextCell {
            admitted: bool,
        }

        impl<'de> DeserializeSeed<'de> for NextCell {
            type Value = Cell;

            fn deserialize<D: serde::Deserializer<'de>>(
                self,
                deserializer: D,
            ) -> Result<Cell, D::Error> {
                if !self.admitted {
                    return Err(de::Error::custom("logical row exceeds 256 columns"));
                }
                Cell::deserialize(deserializer)
            }
        }

        let mut values = Vec::with_capacity(sequence.size_hint().unwrap_or(0).min(MAX_COLUMNS));
        while let Some(cell) = sequence.next_element_seed(NextCell {
            admitted: values.len() < MAX_COLUMNS,
        })? {
            values.push(cell);
        }
        Ok(values)
    }
}

struct RecordVisitor {
    row_only: bool,
}

impl<'de> Visitor<'de> for RecordVisitor {
    type Value = Record;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a logical record object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Record, A::Error> {
        let mut kind = None;
        let mut payload = None;
        while let Some(field) = map.next_key::<RecordField>()? {
            if !matches!(field, RecordField::Type) && payload.is_some() {
                return Err(de::Error::custom(
                    "duplicate or conflicting logical record payload",
                ));
            }
            if self.row_only && !matches!(field, RecordField::Type | RecordField::Values) {
                return Err(de::Error::custom("only logical rows may use continuations"));
            }
            let (payload_kind, record) = match field {
                RecordField::Type => {
                    if kind.is_some() {
                        return Err(de::Error::duplicate_field("type"));
                    }
                    let next = map.next_value::<RecordKind>()?;
                    if self.row_only && next != RecordKind::Row {
                        return Err(de::Error::custom("only logical rows may use continuations"));
                    }
                    kind = Some(next);
                    continue;
                }
                RecordField::Header => (
                    RecordKind::Header,
                    Record::Header {
                        header: map.next_value()?,
                    },
                ),
                RecordField::Table => (
                    RecordKind::Table,
                    Record::Table {
                        table: map.next_value()?,
                    },
                ),
                RecordField::Values => (
                    RecordKind::Row,
                    Record::Row {
                        values: map.next_value_seed(RowValues)?,
                    },
                ),
                RecordField::Completion => (
                    RecordKind::Completion,
                    Record::Completion {
                        completion: map.next_value()?,
                    },
                ),
            };
            if kind.is_some_and(|kind| kind != payload_kind) {
                return Err(de::Error::custom(
                    "logical record type and payload disagree",
                ));
            }
            payload = Some((payload_kind, record));
        }
        let kind = kind.ok_or_else(|| de::Error::missing_field("type"))?;
        match payload {
            Some((payload_kind, record)) if kind == payload_kind => Ok(record),
            Some(_) => Err(de::Error::custom(
                "logical record type and payload disagree",
            )),
            None => Err(de::Error::custom("logical record payload is missing")),
        }
    }
}

impl<'de> Deserialize<'de> for Record {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Internally tagged enum derivation first buffers an untyped JSON tree.
        // Typed fields keep rows bounded before validating the table shape and
        // still allow the type discriminator to follow its payload.
        deserializer.deserialize_map(RecordVisitor { row_only: false })
    }
}

pub(super) struct ContinuedRow(pub Record);

impl<'de> Deserialize<'de> for ContinuedRow {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer
            .deserialize_map(RecordVisitor { row_only: true })
            .map(Self)
    }
}

struct LimitedBuffer(Vec<u8>, usize);

impl Write for LimitedBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        // Leave room for the mandatory newline; never allocate an oversized
        // encoded message before discovering that it violates the contract.
        if bytes.len() > (self.1 - 1).saturating_sub(self.0.len()) {
            return Err(std::io::Error::other("logical encoding exceeds its bound"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub fn encode(record: &Record) -> Result<Vec<u8>> {
    encode_with_limit(record, MAX_RECORD_BYTES)
}

fn encode_with_limit(record: &Record, limit: usize) -> Result<Vec<u8>> {
    let mut buffer = LimitedBuffer(Vec::with_capacity(4096), limit);
    serde_json::to_writer(&mut buffer, record).map_err(|_| {
        anyhow!(
            "logical record cannot be encoded within {} MiB",
            limit / (1024 * 1024)
        )
    })?;
    buffer.0.push(b'\n');
    Ok(buffer.0)
}

/// Canonical hashing with fixed scratch space, not a second full encoded row.
/// Buffer small serializer writes so field punctuation does not require a
/// separate SHA-256 update; large strings are hashed directly in whole blocks.
struct CanonicalHash {
    digest: Sha256,
    bytes: usize,
    limit: usize,
    buffer: [u8; 8192],
    buffered: usize,
}

impl CanonicalHash {
    fn new(digest: Sha256, limit: usize) -> Self {
        Self {
            digest,
            bytes: 0,
            limit,
            buffer: [0; 8192],
            buffered: 0,
        }
    }

    fn finish(mut self) -> (Sha256, usize) {
        self.digest.update(&self.buffer[..self.buffered]);
        (self.digest, self.bytes)
    }
}

impl Write for CanonicalHash {
    fn write(&mut self, mut bytes: &[u8]) -> std::io::Result<usize> {
        let count = bytes.len();
        if count > self.limit.saturating_sub(self.bytes) {
            return Err(std::io::Error::other("logical encoding exceeds its bound"));
        }
        self.bytes += count;
        if self.buffered != 0 {
            let take = bytes.len().min(self.buffer.len() - self.buffered);
            self.buffer[self.buffered..self.buffered + take].copy_from_slice(&bytes[..take]);
            self.buffered += take;
            bytes = &bytes[take..];
            if self.buffered == self.buffer.len() {
                self.digest.update(self.buffer.as_slice());
                self.buffered = 0;
            }
        }
        if bytes.len() >= self.buffer.len() {
            let take = bytes.len() / self.buffer.len() * self.buffer.len();
            self.digest.update(&bytes[..take]);
            bytes = &bytes[take..];
        }
        self.buffer[self.buffered..self.buffered + bytes.len()].copy_from_slice(bytes);
        self.buffered += bytes.len();
        Ok(count)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.digest.update(&self.buffer[..self.buffered]);
        self.buffered = 0;
        Ok(())
    }
}

/// An admitted immutable record. Ordinary small rows keep the one-pass
/// buffered path; larger rows are serialized directly to bounded transport
/// frames after their exact size and canonical digest have been checked.
pub struct PreparedRecord<'a> {
    record: &'a Record,
    encoded: Option<Vec<u8>>,
    bytes: usize,
}

impl PreparedRecord<'_> {
    pub fn write_to(&self, output: &mut impl Write) -> io::Result<()> {
        if let Some(bytes) = &self.encoded {
            write_encoded(bytes, output)
        } else if self.bytes <= MAX_RECORD_BYTES {
            serialize_record(self.record, self.bytes, output)
        } else {
            chunked::write_record(self.record, self.bytes, output)
        }
    }
}

/// serde_json wraps I/O failures. Retain the original typed cause, including
/// the OS code, rather than degrading disk-full/permission errors to Other.
/// This adapter never flushes on drop or retries a non-interrupted failure.
struct CheckedOutput<'a, W> {
    output: &'a mut W,
    remaining: usize,
    failure: Option<io::Error>,
}

impl<W: Write> CheckedOutput<'_, W> {
    fn fail(&mut self, error: io::Error) -> io::Result<usize> {
        self.failure = Some(error);
        Err(io::Error::other("logical output write failed"))
    }
}

impl<W: Write> Write for CheckedOutput<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.failure.is_some() {
            return Err(io::Error::other("logical output write failed"));
        }
        if bytes.len() > self.remaining {
            return self.fail(io::Error::new(
                io::ErrorKind::InvalidInput,
                "logical record exceeded its validated size",
            ));
        }
        let written = loop {
            match self.output.write(bytes) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return self.fail(error),
                Ok(0) if !bytes.is_empty() => {
                    return self.fail(io::ErrorKind::WriteZero.into());
                }
                Ok(written) if written > bytes.len() => {
                    return self.fail(io::ErrorKind::InvalidData.into());
                }
                Ok(written) => break written,
            }
        };
        self.remaining -= written;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}

fn serialize_record(record: &Record, bytes: usize, output: &mut impl Write) -> io::Result<()> {
    let mut checked = CheckedOutput {
        output,
        remaining: bytes,
        failure: None,
    };
    let result = serde_json::to_writer(&mut checked, record)
        .map_err(io::Error::other)
        .and_then(|()| checked.write_all(b"\n"));
    if let Some(error) = checked.failure.take() {
        return Err(error);
    }
    result?;
    if checked.remaining != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "logical record did not reach its validated size",
        ));
    }
    Ok(())
}

/// Emit already validated canonical bytes, splitting an oversized v2 row into
/// bounded frames. Small records retain the exact v1 representation. Callers
/// must use Validator::push first; it enforces the header's version and limits.
pub fn write_encoded(bytes: &[u8], output: &mut impl Write) -> std::io::Result<()> {
    if bytes.len() <= MAX_RECORD_BYTES {
        output.write_all(bytes)
    } else {
        chunked::write(bytes, output)
    }
}

/// Enforce the encoded limit while reading, rather than after read_line has
/// already allocated an attacker-controlled buffer. EOF is valid between, not
/// within, newline-terminated records. Parsing errors never echo input bytes.
pub fn read_record(reader: &mut impl BufRead, line: u64) -> Result<Option<Record>> {
    let Some(bytes) = read_record_bytes(reader, line)? else {
        return Ok(None);
    };
    // The ordinary path still parses once. Continuations are decoded here, so
    // import, migration, reimport, search and view all see one ordinary row.
    let record = match serde_json::from_slice(&bytes) {
        Ok(record) => record,
        Err(_) => chunked::read(reader, &bytes, line)?,
    };
    Ok(Some(record))
}

fn read_record_bytes(reader: &mut impl BufRead, line: u64) -> Result<Option<Vec<u8>>> {
    let mut bytes = Vec::with_capacity(4096);
    loop {
        let available = match reader.fill_buf() {
            Ok(available) => available,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            // Keep the I/O cause: a failed read is not a verdict on the archive.
            Err(error) => {
                return Err(super::export::io_failure(
                    format!("cannot read logical archive at record {line}"),
                    error,
                ));
            }
        };
        if available.is_empty() {
            if !bytes.is_empty() {
                return Err(super::integrity(format!(
                    "unterminated logical archive record {line}"
                )));
            }
            return Ok(None);
        }
        let newline = available.iter().position(|&byte| byte == b'\n');
        let take = newline.map_or(available.len(), |position| position + 1);
        if take > MAX_RECORD_BYTES.saturating_sub(bytes.len()) {
            return Err(super::integrity(format!(
                "logical archive record {line} exceeds 8 MiB"
            )));
        }
        bytes.extend_from_slice(&available[..take]);
        reader.consume(take);
        if newline.is_some() {
            return Ok(Some(bytes));
        }
    }
}

fn key(values: &[Cell], table: &Table) -> Result<Vec<KeyCell>> {
    let mut bytes = 0usize;
    table
        .primary_key
        .iter()
        .map(|&index| {
            let cell = match &values[index] {
                Cell::Integer(value) => KeyCell::Integer(*value),
                Cell::Text(value) => {
                    bytes = bytes.saturating_add(value.len());
                    ensure!(bytes <= MAX_KEY_BYTES, "logical primary key exceeds 64 KiB");
                    KeyCell::Text(value.clone())
                }
                Cell::Blob(value) => {
                    bytes = bytes.saturating_add(value.len());
                    ensure!(bytes <= MAX_KEY_BYTES, "logical primary key exceeds 64 KiB");
                    KeyCell::Blob(
                        STANDARD
                            .decode(value)
                            .map_err(|_| anyhow!("invalid BLOB key"))?,
                    )
                }
                Cell::Null | Cell::Real(_) => {
                    bail!("logical primary keys must be non-null integers, text or blobs")
                }
            };
            Ok(cell)
        })
        .collect()
}

// SQLite's storage-class ordering for the supported primary-key types, with
// BINARY collation. Floating-point and NULL primary keys are refused explicitly.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum KeyCell {
    Integer(i64),
    Text(String),
    Blob(Vec<u8>),
}

/// Bounded validation state: one descriptor, one primary key, and <=256 counts.
/// A completion authenticates canonical records, not incidental JSON whitespace
/// or the export timestamp. It is an integrity digest, not a signature.
pub struct Validator {
    pub header: Header,
    current: Option<Table>,
    last_key: Option<Vec<KeyCell>>,
    counts: BTreeMap<String, u64>,
    rows: u64,
    digest: Sha256,
    completed: bool,
}

impl Validator {
    pub fn new(header: Header) -> Result<Self> {
        header.validate()?;
        let mut identity = header.clone();
        identity.exported_at_ms = 0;
        // v2 changes transport, not canonical identity. Keeping the v1 digest
        // domain also lets a v1 backup compare with a freshly exported v2
        // snapshot during restore verification and read-only --if-identical.
        identity.schema_version = VERSION;
        identity.record_types = record_types(VERSION);
        let mut digest = Sha256::new();
        digest.update(b"cass.logical_archive.v1\0");
        digest.update(encode(&Record::Header { header: identity })?);
        Ok(Self {
            header,
            current: None,
            last_key: None,
            counts: BTreeMap::new(),
            rows: 0,
            digest,
            completed: false,
        })
    }

    pub fn push(&mut self, record: &Record) -> Result<Vec<u8>> {
        ensure!(!self.completed, "records follow the archive completion");
        let bytes = encode_with_limit(record, self.record_limit(record))?;
        let mut digest = self.digest.clone();
        digest.update(&bytes);
        self.accept(record, digest)?;
        Ok(bytes)
    }

    /// Admit an output row without ever allocating a full large encoding.
    /// Most session messages fit the 64 KiB fast path and serialize only once.
    /// Oversize/escaping failures in the small buffer fall through to the
    /// unchanged version-specific limits, not to truncation or omission.
    pub fn prepare<'a>(&mut self, record: &'a Record) -> Result<PreparedRecord<'a>> {
        ensure!(!self.completed, "records follow the archive completion");
        match encode_with_limit(record, self.record_limit(record).min(64 * 1024)) {
            Ok(encoded) => {
                let mut digest = self.digest.clone();
                digest.update(&encoded);
                self.accept(record, digest)?;
                Ok(PreparedRecord {
                    record,
                    bytes: encoded.len(),
                    encoded: Some(encoded),
                })
            }
            Err(_) => {
                let bytes = self.validate(record)?;
                Ok(PreparedRecord {
                    record,
                    encoded: None,
                    bytes,
                })
            }
        }
    }

    /// Verify a record and advance canonical identity without allocating its
    /// serialized bytes. Return its exact encoded size, including the newline.
    /// All checks and hashing precede state mutation, just as in `push`.
    pub fn validate(&mut self, record: &Record) -> Result<usize> {
        ensure!(!self.completed, "records follow the archive completion");
        let limit = self.record_limit(record);
        let mut hash = CanonicalHash::new(self.digest.clone(), limit);
        let encode_error = || {
            anyhow!(
                "logical record cannot be encoded within {} MiB",
                limit / (1024 * 1024)
            )
        };
        serde_json::to_writer(&mut hash, record).map_err(|_| encode_error())?;
        hash.write_all(b"\n").map_err(|_| encode_error())?;
        let (digest, bytes) = hash.finish();
        self.accept(record, digest)?;
        Ok(bytes)
    }

    fn record_limit(&self, record: &Record) -> usize {
        if self.header.schema_version == CHUNKED_VERSION && matches!(record, Record::Row { .. }) {
            MAX_ROW_BYTES
        } else {
            MAX_RECORD_BYTES
        }
    }

    /// The single invariant/state transition path for writers and readers.
    /// A rejected record never advances the key, counts, or canonical digest.
    fn accept(&mut self, record: &Record, digest: Sha256) -> Result<()> {
        match record {
            Record::Header { .. } => bail!("duplicate archive header"),
            Record::Table { table } => {
                table.validate()?;
                ensure!(
                    self.counts.len() < MAX_TABLES,
                    "logical archive exceeds 256 tables"
                );
                ensure!(
                    self.current
                        .as_ref()
                        .is_none_or(|previous| previous.name < table.name),
                    "logical tables must be unique and ordered"
                );
                self.counts.insert(table.name.clone(), 0);
                self.current = Some(table.clone());
                self.last_key = None;
            }
            Record::Row { values } => {
                let table = self
                    .current
                    .as_ref()
                    .ok_or_else(|| anyhow!("row precedes its table"))?;
                ensure!(
                    values.len() == table.columns.len(),
                    "logical row has the wrong column count"
                );
                for cell in values {
                    cell.validate()?;
                }
                let next_key = key(values, table)?;
                ensure!(
                    self.last_key
                        .as_ref()
                        .is_none_or(|previous| *previous < next_key),
                    "duplicate or unordered logical record identity"
                );
                let rows = self
                    .rows
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("logical record count overflow"))?;
                let count = self
                    .counts
                    .get_mut(&table.name)
                    .expect("current table has a counter");
                let next_count = count
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("logical table count overflow"))?;
                *count = next_count;
                self.rows = rows;
                self.last_key = Some(next_key);
            }
            Record::Completion { completion } => {
                ensure!(self.current.is_some(), "logical archive contains no tables");
                ensure!(
                    *completion == self.completion(),
                    "logical archive completion count or digest mismatch"
                );
                self.completed = true;
                return Ok(());
            }
        }
        self.digest = digest;
        Ok(())
    }

    pub fn completion(&self) -> Completion {
        Completion {
            records: self.rows,
            tables: self.counts.clone(),
            content_sha256: hex::encode(self.digest.clone().finalize()),
        }
    }

    pub fn finish(self) -> Result<(Header, Completion)> {
        ensure!(
            self.completed,
            "logical archive is incomplete: completion record missing"
        );
        let completion = self.completion();
        Ok((self.header, completion))
    }
}

/// Decode-only: every failure other than I/O is an integrity verdict.
pub fn verify(reader: &mut impl BufRead) -> Result<(Header, Completion)> {
    let Some(Record::Header { header }) = read_record(reader, 1)? else {
        return Err(super::integrity("logical archive must begin with a header"));
    };
    let mut validator = Validator::new(header).map_err(super::integrity_unless_io)?;
    let mut line = 2u64;
    while let Some(record) = read_record(reader, line)? {
        validator
            .validate(&record)
            .map_err(|error| super::integrity(format!("record {line}: {error}")))?;
        line = line
            .checked_add(1)
            .ok_or_else(|| anyhow!("logical record position overflow"))?;
    }
    validator.finish().map_err(super::integrity_unless_io)
}

#[cfg(test)]
#[path = "codec_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "streaming_tests.rs"]
mod streaming_tests;
