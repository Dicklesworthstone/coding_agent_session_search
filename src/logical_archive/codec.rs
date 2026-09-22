//! Versioned, bounded JSONL framing. No SQL or filesystem paths are executable data.

use std::collections::BTreeMap;
use std::io::{BufRead, Write};

use anyhow::{Result, anyhow, bail, ensure};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const FORMAT: &str = "cass.logical_archive";
pub const VERSION: u32 = 1;
pub const MAX_RECORD_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_TABLES: usize = 256;
pub const MAX_COLUMNS: usize = 256;
const MAX_KEY_BYTES: usize = 64 * 1024;

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
            self.schema_version == VERSION,
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
            self.record_types == ["table", "row", "completion"],
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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
                ensure!(encoded.len() <= MAX_RECORD_BYTES, "oversized BLOB encoding");
                STANDARD
                    .decode(encoded)
                    .map_err(|_| anyhow!("invalid BLOB encoding"))?;
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Record {
    Header { header: Header },
    Table { table: Table },
    Row { values: Vec<Cell> },
    Completion { completion: Completion },
}

struct LimitedBuffer(Vec<u8>);

impl Write for LimitedBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        // Leave room for the mandatory newline; never allocate an oversized
        // encoded message before discovering that it violates the contract.
        if bytes.len() > (MAX_RECORD_BYTES - 1).saturating_sub(self.0.len()) {
            return Err(std::io::Error::other("logical record exceeds 8 MiB"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub fn encode(record: &Record) -> Result<Vec<u8>> {
    let mut buffer = LimitedBuffer(Vec::with_capacity(4096));
    serde_json::to_writer(&mut buffer, record)
        .map_err(|_| anyhow!("logical record cannot be encoded within 8 MiB"))?;
    buffer.0.push(b'\n');
    Ok(buffer.0)
}

/// Enforce the encoded limit while reading, rather than after read_line has
/// already allocated an attacker-controlled buffer. EOF is valid between, not
/// within, newline-terminated records. Parsing errors never echo input bytes.
pub fn read_record(reader: &mut impl BufRead, line: u64) -> Result<Option<Record>> {
    let mut bytes = Vec::with_capacity(4096);
    loop {
        let available = match reader.fill_buf() {
            Ok(available) => available,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            // Keep the I/O cause: a failed read is not a verdict on the archive.
            Err(error) => {
                return Err(anyhow::Error::new(error)
                    .context(format!("cannot read logical archive at record {line}")));
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
            let record = serde_json::from_slice(&bytes).map_err(|error| {
                super::integrity(format!(
                    "malformed logical archive record {line}, column {}",
                    error.column()
                ))
            })?;
            return Ok(Some(record));
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
                self.last_key = Some(next_key);
                self.rows = self
                    .rows
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("logical record count overflow"))?;
                let count = self
                    .counts
                    .get_mut(&table.name)
                    .expect("current table has a counter");
                *count = count
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("logical table count overflow"))?;
            }
            Record::Completion { completion } => {
                ensure!(self.current.is_some(), "logical archive contains no tables");
                ensure!(
                    *completion == self.completion(),
                    "logical archive completion count or digest mismatch"
                );
                self.completed = true;
                return encode(record);
            }
        }
        let bytes = encode(record)?;
        self.digest.update(&bytes);
        Ok(bytes)
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
            .push(&record)
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
