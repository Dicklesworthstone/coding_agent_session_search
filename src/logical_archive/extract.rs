//! Recover one complete message field without restoring a database. The selected
//! field stays private until the entire pinned backup, including EOF, verifies.

use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;

use anyhow::{Result, anyhow, bail, ensure};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use clap::ValueEnum;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::codec::{self, Cell, Completion, Header, Record, Table, Validator};
use super::export::{self, DestinationLock};

const IO_BYTES: usize = 64 * 1024;
// Aligned base64 blocks: no full decoded BLOB allocation in addition to the row.
const BLOB_BYTES: usize = IO_BYTES / 4 * 3;

#[derive(Clone, Copy, Debug, ValueEnum)]
pub(super) enum Field {
    Content,
    ExtraJson,
    ExtraBin,
}

impl Field {
    fn column(self) -> &'static str {
        match self {
            Self::Content => "content",
            Self::ExtraJson => "extra_json",
            Self::ExtraBin => "extra_bin",
        }
    }

    fn encoding(self) -> &'static str {
        match self {
            Self::Content | Self::ExtraJson => "utf-8",
            Self::ExtraBin => "binary",
        }
    }
}

fn validate_request(message_id: i64, digest: &str) -> Result<()> {
    if message_id <= 0 {
        return Err(super::ArchiveUsageError("--message-id must be positive".into()).into());
    }
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(super::ArchiveUsageError(
            "--content-sha256 requires the 64 lowercase hex digits from archive search or verify"
                .into(),
        )
        .into());
    }
    Ok(())
}

fn columns(table: &Table, field: Field) -> Result<(usize, usize)> {
    let column = |name: &str| {
        table
            .columns
            .iter()
            .position(|column| column == name)
            .ok_or_else(|| anyhow!("logical backup lacks required messages.{name} column"))
    };
    let id = column("id")?;
    ensure!(
        table.primary_key == [id],
        "logical extraction requires messages.id as its sole primary key"
    );
    Ok((id, column(field.column())?))
}

#[derive(Debug, PartialEq, Eq)]
struct Payload {
    bytes: u64,
    sha256: String,
}

/// The caller validates the whole row first, including strict BLOB padding and
/// tail bits. Small output chunks bound extra memory; no newline is appended.
fn write_payload(field: Field, cell: &Cell, output: &mut impl Write) -> Result<Payload> {
    let mut digest = Sha256::new();
    let mut count = 0_u64;
    let mut write = |bytes: &[u8]| -> Result<()> {
        output
            .write_all(bytes)
            .map_err(|error| export::io_failure("cannot write private extracted field", error))?;
        digest.update(bytes);
        count = count
            .checked_add(u64::try_from(bytes.len())?)
            .ok_or_else(|| anyhow!("extracted field byte count overflow"))?;
        Ok(())
    };
    match (field, cell) {
        (Field::Content | Field::ExtraJson, Cell::Text(text)) => {
            for chunk in text.as_bytes().chunks(IO_BYTES) {
                write(chunk)?;
            }
        }
        (Field::ExtraBin, Cell::Blob(encoded)) => {
            let mut decoded = [0_u8; BLOB_BYTES];
            for chunk in encoded.as_bytes().chunks(IO_BYTES) {
                let length = STANDARD
                    .decode_slice(chunk, &mut decoded)
                    .map_err(|_| super::integrity("invalid extracted BLOB encoding"))?;
                write(&decoded[..length])?;
            }
        }
        (_, Cell::Null) => bail!(
            "selected message field {} is NULL; no output was published",
            field.column()
        ),
        _ => bail!(
            "selected message field {} has an unsupported storage type",
            field.column()
        ),
    }
    Ok(Payload {
        bytes: count,
        sha256: hex::encode(digest.finalize()),
    })
}

fn scan(
    input: &mut impl BufRead,
    output: &mut impl Write,
    message_id: i64,
    expected_digest: &str,
    field: Field,
) -> Result<(Header, Completion, Payload)> {
    let Some(Record::Header { header }) = codec::read_record(input, 1)? else {
        return Err(super::integrity("logical archive must begin with a header"));
    };
    let mut validator = Validator::new(header).map_err(super::integrity_unless_io)?;
    let mut selected = None;
    let mut offsets = None;
    let mut line = 2_u64;
    while let Some(record) = codec::read_record(input, line)? {
        validator
            .validate(&record)
            .map_err(|error| super::integrity(format!("record {line}: {error}")))?;
        match &record {
            Record::Table { table } => {
                offsets = if table.name == "messages" {
                    Some(columns(table, field)?)
                } else {
                    None
                };
            }
            Record::Row { values } => {
                if let Some((id, value)) = offsets {
                    let Cell::Integer(id) = &values[id] else {
                        bail!("logical extraction requires integer message identities");
                    };
                    if *id == message_id {
                        ensure!(selected.is_none(), "selected message identity is ambiguous");
                        selected = Some(write_payload(field, &values[value], output)?);
                    }
                }
            }
            Record::Header { .. } | Record::Completion { .. } => {}
        }
        line = line
            .checked_add(1)
            .ok_or_else(|| anyhow!("logical record position overflow"))?;
    }
    let (header, completion) = validator.finish().map_err(super::integrity_unless_io)?;
    ensure!(
        completion.content_sha256 == expected_digest,
        "logical backup does not match --content-sha256; rerun search instead of reusing a different snapshot's message ID"
    );
    let payload = selected.ok_or_else(|| {
        anyhow!("message ID not found in the verified logical backup; no substitute was extracted")
    })?;
    output
        .flush()
        .map_err(|error| export::io_failure("cannot flush private extracted field", error))?;
    Ok((header, completion, payload))
}

fn require_absent(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(export::io_failure("cannot inspect extraction destination", error)),
        Ok(_) => bail!("extraction destination already exists; nothing was replaced"),
    }
}

/// Check the actual staged bytes using the same descriptor, not a pathname
/// reopen. Receipt size/hash describe the file, not merely the encoder's state.
fn verify_payload(file: &mut File, expected: &Payload) -> Result<()> {
    file.seek(SeekFrom::Start(0))
        .map_err(|error| export::io_failure("cannot rewind private extracted field", error))?;
    let mut digest = Sha256::new();
    let mut bytes = 0_u64;
    let mut buffer = [0_u8; IO_BYTES];
    loop {
        let count = match file.read(&mut buffer) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => result.map_err(|error| {
                export::io_failure("cannot verify private extracted field", error)
            })?,
        };
        if count == 0 {
            break;
        }
        bytes = bytes
            .checked_add(u64::try_from(count)?)
            .ok_or_else(|| anyhow!("extracted field byte count overflow"))?;
        ensure!(bytes <= expected.bytes, "staged extracted field length changed");
        digest.update(&buffer[..count]);
    }
    ensure!(
        bytes == expected.bytes && hex::encode(digest.finalize()) == expected.sha256,
        "staged extracted field failed read-back verification"
    );
    Ok(())
}

pub(super) fn extract_file(
    input: &Path,
    destination: &Path,
    message_id: i64,
    expected_digest: &str,
    field: Field,
) -> Result<Value> {
    validate_request(message_id, expected_digest)?;
    // Pin one regular input; no database, provider path, model or index opens.
    let mut input = BufReader::with_capacity(IO_BYTES, super::import::open_input(input)?);
    let _lock = DestinationLock::acquire(destination)?;
    require_absent(destination)?;
    let mut stage = tempfile::NamedTempFile::new_in(export::parent(destination)?)
        .map_err(|error| export::io_failure("cannot create private extraction stage", error))?;
    let (header, completion, payload) = scan(
        &mut input,
        stage.as_file_mut(),
        message_id,
        expected_digest,
        field,
    )?;
    stage
        .as_file()
        .sync_all()
        .map_err(|error| export::io_failure("cannot sync private extracted field", error))?;
    verify_payload(stage.as_file_mut(), &payload)?;
    stage.persist_noclobber(destination).map_err(|error| {
        export::io_failure(
            "cannot publish extracted field without replacing existing data",
            error.error,
        )
    })?;
    export::sync_parent(destination)?;
    Ok(json!({
        "operation": "extract", "format": codec::FORMAT,
        "schema_version": header.schema_version, "archive_id": header.archive_id,
        "content_sha256": completion.content_sha256, "message_id": message_id,
        "field": field.column(), "encoding": field.encoding(),
        "output_bytes": payload.bytes, "output_sha256": payload.sha256,
        "destination_status": "created", "integrity_verified": true,
        "contains_private_data": true, "content_source": "logical_archive",
        "database_opened": false, "provider_files_opened": false,
        "database_integrity_checked": false,
    }))
}

#[cfg(test)]
#[path = "extract_tests.rs"]
mod tests;
