//! Lossless, database-free recovery of one conversation and all its message rows.
//! This is a transcript excerpt, NOT a canonical database restoration input.
//! Nothing is published until the entire pinned source and staged output verify.

use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Seek, Write};
use std::path::Path;

use anyhow::{Result, anyhow, bail, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::codec::{self, Cell, Record, Table, Validator};
use super::export::{self, DestinationLock};

const FORMAT: &str = "cass.conversation_extract";
const IO_BYTES: usize = 64 * 1024;

fn validate_request(conversation_id: i64, digest: &str) -> Result<()> {
    if conversation_id <= 0 {
        return Err(super::ArchiveUsageError("--conversation-id must be positive".into()).into());
    }
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(super::ArchiveUsageError(
            "--content-sha256 requires 64 lowercase hex digits from archive search or verify".into(),
        )
        .into());
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum Scope {
    Other,
    Conversation { id: usize },
    Messages { id: usize, parent: usize },
}

impl Scope {
    fn for_table(table: &Table) -> Result<Self> {
        if !matches!(table.name.as_str(), "conversations" | "messages") {
            return Ok(Self::Other);
        }
        let column = |name: &str| {
            table
                .columns
                .iter()
                .position(|column| column == name)
                .ok_or_else(|| anyhow!("conversation extraction lacks {}.{name}", table.name))
        };
        let id = column("id")?;
        ensure!(
            table.primary_key == [id],
            "conversation extraction requires {}.id as its sole primary key",
            table.name
        );
        if table.name == "conversations" {
            Ok(Self::Conversation { id })
        } else {
            Ok(Self::Messages {
                id,
                parent: column("conversation_id")?,
            })
        }
    }
}

fn integer(values: &[Cell], offset: usize) -> Result<i64> {
    match values.get(offset) {
        Some(Cell::Integer(value)) => Ok(*value),
        _ => Err(super::integrity(
            "conversation extraction requires integer conversation and message identities",
        )),
    }
}

#[derive(Debug, PartialEq, Eq)]
struct OutputDigest {
    bytes: u64,
    sha256: String,
}

/// Hash exactly the bytes accepted by the private sink, including partial writes.
/// No buffer or destructor performs implicit writes on an error path.
struct HashedOutput<W> {
    output: W,
    digest: Sha256,
    bytes: u64,
}

impl<W> HashedOutput<W> {
    fn new(output: W) -> Self {
        Self {
            output,
            digest: Sha256::new(),
            bytes: 0,
        }
    }

    fn snapshot(&self) -> OutputDigest {
        OutputDigest {
            bytes: self.bytes,
            sha256: hex::encode(self.digest.clone().finalize()),
        }
    }
}

impl<W: Write> Write for HashedOutput<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let _maximum = u64::try_from(bytes.len())
            .ok()
            .and_then(|length| self.bytes.checked_add(length))
            .ok_or_else(|| io::Error::other("conversation extraction byte count overflow"))?;
        let count = self.output.write(bytes)?;
        if count > bytes.len() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        self.digest.update(&bytes[..count]);
        // Conversion and addition were checked for the entire offered slice.
        self.bytes += u64::try_from(count)
            .map_err(|_| io::Error::other("conversation extraction byte count overflow"))?;
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}

/// Only fixed-size control metadata reaches this encoder, never message fields.
fn write_control(output: &mut impl Write, value: Value) -> Result<()> {
    let mut bytes = serde_json::to_vec(&value)
        .map_err(|_| anyhow!("cannot encode conversation extraction metadata"))?;
    ensure!(
        bytes.len() < codec::MAX_RECORD_BYTES,
        "oversized extraction metadata"
    );
    bytes.push(b'\n');
    output
        .write_all(&bytes)
        .map_err(|error| export::io_failure("cannot write private conversation extraction", error))
}

fn scan(
    input: &mut impl BufRead,
    output: &mut impl Write,
    conversation_id: i64,
    expected_digest: &str,
) -> Result<(Value, OutputDigest)> {
    let Some(Record::Header { header }) = codec::read_record(input, 1)? else {
        return Err(super::integrity("logical archive must begin with a header"));
    };
    let mut validator = Validator::new(header.clone()).map_err(super::integrity_unless_io)?;
    let mut output = HashedOutput::new(output);
    write_control(
        &mut output,
        json!({
            "type": "conversation_extract", "format": FORMAT, "schema_version": 1,
            "source_header": header, "source_content_sha256": expected_digest,
            "conversation_id": conversation_id, "contains_private_data": true,
            "row_transport": "cass.logical_archive.v2",
            "row_order": "source_primary_key", "restorable_as_archive": false,
        }),
    )?;
    let mut scope = Scope::Other;
    let mut found = false;
    let mut messages_table = false;
    let mut messages = 0_u64;
    let mut line = 2_u64;
    while let Some(record) = codec::read_record(input, line)? {
        // The same pass authenticates ALL records, including nonselected rows.
        // prepare also gives selected large rows bounded v2 continuation output.
        let prepared = validator
            .prepare(&record)
            .map_err(|error| super::integrity(format!("record {line}: {error}")))?;
        let selected = match &record {
            Record::Table { table } => {
                scope = Scope::for_table(table).map_err(super::integrity_unless_io)?;
                messages_table |= matches!(scope, Scope::Messages { .. });
                !matches!(scope, Scope::Other)
            }
            Record::Row { values } => match scope {
                Scope::Conversation { id } => {
                    let selected = integer(values, id)? == conversation_id;
                    if selected {
                        ensure!(!found, "selected conversation identity is ambiguous");
                        found = true;
                    }
                    selected
                }
                Scope::Messages { id, parent } => {
                    integer(values, id)?;
                    let selected = integer(values, parent)? == conversation_id;
                    if selected {
                        messages = messages
                            .checked_add(1)
                            .ok_or_else(|| anyhow!("extracted message count overflow"))?;
                    }
                    selected
                }
                Scope::Other => false,
            },
            Record::Header { .. } | Record::Completion { .. } => false,
        };
        if selected {
            prepared.write_to(&mut output).map_err(|error| {
                export::io_failure("cannot write private conversation row", error)
            })?;
        }
        line = line
            .checked_add(1)
            .ok_or_else(|| anyhow!("logical record position overflow"))?;
    }
    let (header, completion) = validator.finish().map_err(super::integrity_unless_io)?;
    ensure!(
        completion.content_sha256 == expected_digest,
        "logical backup does not match --content-sha256; rerun search before selecting a conversation"
    );
    ensure!(
        found,
        "conversation ID not found in the verified backup; no output was published"
    );
    ensure!(
        messages_table,
        "logical backup has no messages table; no output was published"
    );
    let prefix = output.snapshot();
    write_control(
        &mut output,
        json!({
            "type": "extraction_completion", "conversation_rows": 1,
            "message_rows": messages, "source_content_sha256": completion.content_sha256,
            "output_prefix_bytes": prefix.bytes, "output_prefix_sha256": prefix.sha256,
        }),
    )?;
    output
        .flush()
        .map_err(|error| export::io_failure("cannot flush private conversation extraction", error))?;
    let digest = output.snapshot();
    let receipt = json!({
        "operation": "extract-conversation", "format": FORMAT, "schema_version": 1,
        "archive_id": header.archive_id, "source_schema_version": header.schema_version,
        "content_sha256": completion.content_sha256, "conversation_id": conversation_id,
        "conversation_rows": 1, "message_rows": messages,
        "output_bytes": digest.bytes, "output_sha256": digest.sha256,
        "destination_status": "created", "contains_private_data": true,
        "integrity_verified": true, "restorable_as_archive": false,
        "database_opened": false, "provider_files_opened": false,
        "database_integrity_checked": false,
    });
    Ok((receipt, digest))
}

fn require_absent(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(export::io_failure(
            "cannot inspect extraction destination",
            error,
        )),
        Ok(_) => bail!("extraction destination already exists; nothing was replaced"),
    }
}

fn verify_stage(file: &mut File, expected: &OutputDigest) -> Result<()> {
    file.rewind()
        .map_err(|error| export::io_failure("cannot rewind private conversation extraction", error))?;
    let mut output = HashedOutput::new(io::sink());
    let mut buffer = [0_u8; IO_BYTES];
    loop {
        let count = match file.read(&mut buffer) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => result.map_err(|error| {
                export::io_failure("cannot verify private conversation extraction", error)
            })?,
        };
        if count == 0 {
            break;
        }
        output.write_all(&buffer[..count])?;
        ensure!(
            output.bytes <= expected.bytes,
            "staged conversation extraction length changed"
        );
    }
    ensure!(
        output.snapshot() == *expected,
        "staged conversation extraction failed read-back verification"
    );
    Ok(())
}

pub(super) fn extract_file(
    input: &Path,
    destination: &Path,
    conversation_id: i64,
    expected_digest: &str,
) -> Result<Value> {
    validate_request(conversation_id, expected_digest)?;
    let mut input = BufReader::with_capacity(IO_BYTES, super::import::open_input(input)?);
    let _lock = DestinationLock::acquire(destination)?;
    require_absent(destination)?;
    let mut stage = tempfile::NamedTempFile::new_in(export::parent(destination)?).map_err(|error| {
        export::io_failure("cannot create private conversation extraction", error)
    })?;
    let (receipt, digest) = scan(
        &mut input,
        stage.as_file_mut(),
        conversation_id,
        expected_digest,
    )?;
    stage
        .as_file()
        .sync_all()
        .map_err(|error| export::io_failure("cannot sync private conversation extraction", error))?;
    verify_stage(stage.as_file_mut(), &digest)?;
    stage.persist_noclobber(destination).map_err(|error| {
        export::io_failure(
            "cannot publish conversation extraction without replacing existing data",
            error.error,
        )
    })?;
    export::sync_parent(destination)?;
    Ok(receipt)
}

#[cfg(test)]
#[path = "conversation_extract_tests.rs"]
mod tests;
