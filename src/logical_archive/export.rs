//! Read-only canonical snapshot export. Derived virtual/shadow tables are not
//! authority and are omitted; unfamiliar unkeyed tables fail rather than vanish.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, ensure};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use coding_agent_search::franken_sync::compat::{OpenFlags, RowExt, open_with_flags};
use coding_agent_search::franken_sync::{Connection, FrankenError, SqliteValue};

use super::codec::{self, Cell, Completion, Header, Record, Table, Validator};

// Bound extra I/O memory independently of archive size. The existing codec
// still enforces its separate 8 MiB per-record limit.
const EXPORT_IO_BUFFER_BYTES: usize = 256 * 1024;

/// A separate destination lock, not the unrelated source-mirroring sync.lock.
/// Lock files persist so contenders cannot accidentally lock different inodes.
pub struct DestinationLock {
    _file: File,
}

impl DestinationLock {
    pub fn acquire(destination: &Path) -> Result<Self> {
        let parent = parent(destination)?;
        ensure!(parent.is_dir(), "destination parent must already exist");
        let name = destination
            .file_name()
            .ok_or_else(|| anyhow!("destination requires a file name"))?;
        let mut lock_name = std::ffi::OsString::from(".");
        lock_name.push(name);
        lock_name.push(".logical-archive.lock");
        let path = parent.join(lock_name);
        if let Ok(metadata) = fs::symlink_metadata(&path) {
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "destination lock is not a regular file"
            );
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
        }
        let file = options
            .open(&path)
            .context("cannot open logical archive destination lock")?;
        ensure!(
            file.metadata()?.is_file(),
            "destination lock is not a regular file"
        );
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            ensure!(
                file.metadata()?.file_attributes() & 0x400 == 0,
                "destination lock is a reparse point"
            );
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            // std's try_lock reports contention as WouldBlock on every
            // platform; fs2 surfaced Windows contention as raw
            // ERROR_LOCK_VIOLATION, which failed instead of waiting (2l1b0.74).
            match file.try_lock() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(std::fs::TryLockError::WouldBlock) => {
                    if Instant::now() >= deadline {
                        return Err(super::ArchiveBusyError(
                            "logical archive destination remained locked for five seconds".into(),
                        )
                        .into());
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(std::fs::TryLockError::Error(error)) => {
                    return Err(anyhow::Error::new(error)
                        .context("cannot acquire logical archive destination lock"));
                }
            }
        }
    }
}

pub fn parent(path: &Path) -> Result<&Path> {
    path.parent()
        .map(|parent| {
            if parent.as_os_str().is_empty() {
                Path::new(".")
            } else {
                parent
            }
        })
        .ok_or_else(|| anyhow!("destination requires a parent directory"))
}

pub fn path_text(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| anyhow!("FrankenSQLite database paths must be valid UTF-8"))
}

pub fn quoted(name: &str) -> Result<String> {
    ensure!(codec::identifier(name), "invalid logical SQL identifier");
    Ok(format!("\"{name}\""))
}

pub fn open_source(path: &Path) -> Result<Connection> {
    let metadata = fs::symlink_metadata(path).context("cannot inspect source archive")?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "source archive must be a regular, non-symlink file"
    );
    let connection = open_with_flags(path_text(path)?, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|error| source_failure("cannot open source archive read-only", error))?;
    connection
        .execute("PRAGMA busy_timeout = 5000")
        .map_err(|error| source_failure("cannot set source busy timeout", error))?;
    connection
        .execute("PRAGMA query_only = ON")
        .map_err(|error| source_failure("cannot enforce read-only source queries", error))?;
    connection
        .execute("BEGIN")
        .map_err(|error| source_failure("cannot begin source snapshot", error))?;
    Ok(connection)
}

pub fn schema_version(connection: &Connection) -> Result<String> {
    connection
        .query_row("SELECT value FROM meta WHERE key = 'schema_version'")
        .map_err(|error| source_failure("cannot read source schema version", error))?
        .get_typed::<String>(0)
        .map_err(|_| anyhow!("source does not contain a supported canonical schema version"))
}

/// FTS5 owns a closed set of shadow names, not the entire `<root>_` namespace.
/// Call only for a discovered virtual root: without that owner even an exact
/// shadow-like name is ordinary data and must be exported or explicitly refused.
fn is_fts5_shadow_table(name: &str, root: &str) -> bool {
    name.strip_prefix(root).is_some_and(|suffix| {
        matches!(
            suffix,
            "_config" | "_content" | "_data" | "_docsize" | "_idx"
        )
    })
}

/// Metadata is bounded independently of the number of canonical rows. Never
/// execute stored CREATE statements: only inspect a short virtual-table prefix.
pub fn tables(connection: &Connection) -> Result<Vec<Table>> {
    let rows = connection
        .query(
            "SELECT name, substr(sql, 1, 64) FROM sqlite_master WHERE type = 'table' ORDER BY name LIMIT 1025",
        )
        .map_err(|error| source_failure("cannot read source table catalog", error))?;
    ensure!(
        rows.len() <= 1024,
        "source has too many schema objects for logical export"
    );
    let mut physical = Vec::new();
    let mut virtual_roots = Vec::new();
    for row in rows {
        let name = row
            .get_typed::<String>(0)
            .map_err(|error| source_failure("cannot decode source table name", error))?;
        let sql = row
            .get_typed::<Option<String>>(1)
            .map_err(|error| source_failure("cannot decode source table definition", error))?
            .unwrap_or_default();
        if sql.to_ascii_uppercase().contains("VIRTUAL TABLE") {
            ensure!(
                name == "fts_messages",
                "unrecognized virtual table; refusing an export with undeclared omissions"
            );
            virtual_roots.push(name);
        } else {
            physical.push(name);
        }
    }
    let mut output = Vec::new();
    for name in physical {
        if name.starts_with("sqlite_")
            || virtual_roots
                .iter()
                .any(|root| is_fts5_shadow_table(&name, root))
        {
            continue;
        }
        ensure!(
            output.len() < codec::MAX_TABLES,
            "source exceeds 256 logical tables"
        );
        let columns = connection
            .query(&format!("PRAGMA table_info({})", quoted(&name)?))
            .map_err(|error| {
                source_failure(format!("cannot read logical table {name} schema"), error)
            })?;
        ensure!(
            columns.len() <= codec::MAX_COLUMNS,
            "source table exceeds 256 columns"
        );
        let mut table = Table {
            name,
            columns: Vec::new(),
            primary_key: Vec::new(),
        };
        let mut primary_key = Vec::new();
        for (offset, column) in columns.into_iter().enumerate() {
            table
                .columns
                .push(column.get_typed::<String>(1).map_err(|error| {
                    source_failure(
                        format!("cannot decode logical table {} column", table.name),
                        error,
                    )
                })?);
            let ordinal = column.get_typed::<i64>(5).map_err(|error| {
                source_failure(
                    format!("cannot decode logical table {} primary key", table.name),
                    error,
                )
            })?;
            if ordinal > 0 {
                primary_key.push((ordinal, offset));
            }
        }
        primary_key.sort_unstable();
        table.primary_key = primary_key.into_iter().map(|(_, offset)| offset).collect();
        table.validate().map_err(|error| {
            row_failure(
                format!("unsupported logical table {}", table.name),
                "schema validation",
                error,
            )
        })?;
        output.push(table);
    }
    ensure!(
        !output.is_empty(),
        "source contains no canonical logical tables"
    );
    Ok(output)
}

pub fn cells(values: &[SqliteValue]) -> Result<Vec<Cell>> {
    // Check payload lower bounds before cloning text or base64-expanding blobs.
    let mut bytes = 0usize;
    for value in values {
        let size = match value {
            SqliteValue::Text(text) => {
                // SQLite TEXT can carry invalid UTF-8. SmallText's Display
                // substitutes replacement characters for those bytes, which
                // would silently make a supposedly lossless backup lossy.
                ensure!(text.is_valid_utf8(), "logical TEXT contains invalid UTF-8");
                text.len()
            }
            SqliteValue::Blob(blob) => blob
                .len()
                .checked_add(2)
                .and_then(|n| n.checked_div(3))
                .and_then(|n| n.checked_mul(4))
                .ok_or_else(|| anyhow!("oversized logical BLOB"))?,
            // This is a lower bound, not an estimated encoded size. In
            // particular, charging 32 bytes for each NULL can reject a row
            // whose actual JSON (including framing) fits the wire limit.
            // The bounded encoder accounts for scalar cells and escaping.
            _ => 0,
        };
        bytes = bytes
            .checked_add(size)
            .ok_or_else(|| anyhow!("oversized logical row"))?;
        // v2 splits a large logical row into bounded physical frames. Admit
        // that row here; the validator still enforces its exact encoded size.
        ensure!(bytes < codec::MAX_ROW_BYTES, "logical row exceeds 256 MiB");
    }
    values
        .iter()
        .map(|value| {
            Ok(match value {
                SqliteValue::Null => Cell::Null,
                SqliteValue::Integer(value) => Cell::Integer(*value),
                SqliteValue::Float(value) => {
                    ensure!(value.is_finite(), "non-finite REAL is not portable");
                    Cell::Real(format!("{:016x}", value.to_bits()))
                }
                SqliteValue::Text(value) => Cell::Text(value.to_string()),
                SqliteValue::Blob(value) => Cell::Blob(STANDARD.encode(value.as_ref())),
            })
        })
        .collect()
}

/// Only structural coordinates may enter diagnostics. Text/blob primary keys
/// can themselves contain private session data, so report their row position
/// without printing the key. A single integer key makes a failed message easy
/// to locate without confusing its ID with the one-based position in the scan.
fn row_location(table: &Table, position: u64, values: &[SqliteValue]) -> String {
    let mut location = format!("logical table {}, row {position}", table.name);
    if let [key] = table.primary_key.as_slice()
        && let Some(SqliteValue::Integer(value)) = values.get(*key)
    {
        use std::fmt::Write as _;
        let _ = write!(location, " ({}={value})", table.columns[*key]);
    }
    location
}

/// I/O error messages may contain a pathname or caller-supplied private data.
/// Keep the typed cause for classification, but expose only its kind and OS
/// code in the outer message consumed by the JSON CLI.
pub(super) fn io_failure(context: impl std::fmt::Display, error: io::Error) -> anyhow::Error {
    let message = match error.raw_os_error() {
        Some(code) => format!("{context}: I/O {:?} (OS error {code})", error.kind()),
        None => format!("{context}: I/O {:?}", error.kind()),
    };
    anyhow::Error::new(error).context(message)
}

/// The engine's Display/Debug payloads can include SQL, paths or cell values.
/// Its fieldless error code and numeric I/O coordinates are safe to report.
/// Retain the original typed error so busy and I/O failures remain retryable.
fn source_failure(context: impl std::fmt::Display, error: FrankenError) -> anyhow::Error {
    let detail = match &error {
        FrankenError::Io(error) => match error.raw_os_error() {
            Some(code) => format!("; I/O {:?} (OS error {code})", error.kind()),
            None => format!("; I/O {:?}", error.kind()),
        },
        FrankenError::IoRead { page } => format!("; cannot read database page {page}"),
        FrankenError::IoWrite { page } => format!("; cannot write database page {page}"),
        FrankenError::ShortRead { expected, actual } => {
            format!("; short read: expected {expected} bytes, received {actual}")
        }
        FrankenError::NoSuchTable { .. } => "; missing table".to_owned(),
        FrankenError::NoSuchColumn { .. } => "; missing column".to_owned(),
        FrankenError::QueryReturnedNoRows => "; required row missing".to_owned(),
        FrankenError::QueryReturnedMultipleRows => "; expected a single row".to_owned(),
        FrankenError::SnapshotTooOld { .. } => "; source snapshot is too old".to_owned(),
        FrankenError::WalCorrupt { .. } => "; corrupt WAL".to_owned(),
        _ => String::new(),
    };
    let message = format!(
        "{context}: FrankenSQLite {:?} (code {}){detail}; source was not repaired",
        error.error_code(),
        error.extended_error_code(),
    );
    anyhow::Error::new(error).context(message)
}

/// cells() and Validator emit content-free format diagnostics. Preserve those
/// diagnostics at the outermost level, rather than leaving the CLI with just
/// a table name. Do not use this for arbitrary engine/decoder error strings.
fn row_failure(location: String, stage: &str, error: anyhow::Error) -> anyhow::Error {
    let message = format!("{location}, {stage}: {error}");
    error.context(message)
}

/// A scan error arrives between callbacks, so only the completed prefix and
/// next table-local position are known. Never infer a primary key for it.
fn scan_failure(table: &str, next_row: u64, error: FrankenError) -> anyhow::Error {
    source_failure(
        format!(
            "cannot stream logical table {table} before row {next_row}, after {} complete rows",
            next_row.saturating_sub(1)
        ),
        error,
    )
}

pub fn snapshot(
    connection: &Connection,
    archive_id: String,
    output: &mut impl Write,
) -> Result<(Header, Completion)> {
    let tables = tables(connection)?;
    let header = Header {
        format: codec::FORMAT.to_owned(),
        schema_version: codec::CHUNKED_VERSION,
        archive_id,
        exported_at_ms: chrono::Utc::now().timestamp_millis(),
        storage_schema_version: schema_version(connection)?,
        record_types: codec::record_types(codec::CHUNKED_VERSION),
        contains_private_data: true,
        omissions: vec!["derived_search_assets".to_owned()],
    };
    let mut validator = Validator::new(header.clone())?;
    output
        .write_all(&codec::encode(&Record::Header {
            header: header.clone(),
        })?)
        .map_err(|error| io_failure("cannot write logical archive header", error))?;
    for table in tables {
        output
            .write_all(&validator.push(&Record::Table {
                table: table.clone(),
            })?)
            .map_err(|error| {
                io_failure(
                    format!("cannot write logical table {} descriptor", table.name),
                    error,
                )
            })?;
        let columns = table
            .columns
            .iter()
            .map(|name| quoted(name))
            .collect::<Result<Vec<_>>>()?
            .join(", ");
        let order = table
            .primary_key
            .iter()
            .map(|&offset| {
                Ok(format!(
                    "{} COLLATE BINARY ASC",
                    quoted(&table.columns[offset])?
                ))
            })
            .collect::<Result<Vec<_>>>()?
            .join(", ");
        let sql = format!(
            "SELECT {columns} FROM {} ORDER BY {order}",
            quoted(&table.name)?
        );
        let mut failure = None;
        let mut rows_written = 0u64;
        let streamed = connection.query_with_params_for_each(&sql, &[], |row| {
            let result = (|| -> Result<()> {
                // This is a one-based position in the table's exported PK
                // order. row_location labels an integer key separately.
                let row_number = rows_written
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("logical table row position overflow"))?;
                let record = Record::Row {
                    values: cells(row.values()).map_err(|error| {
                        row_failure(
                            row_location(&table, row_number, row.values()),
                            "cell conversion",
                            error,
                        )
                    })?,
                };
                let encoded = validator.push(&record).map_err(|error| {
                    row_failure(
                        row_location(&table, row_number, row.values()),
                        "record validation/encoding",
                        error,
                    )
                })?;
                codec::write_encoded(&encoded, output).map_err(|error| {
                    io_failure(
                        format!(
                            "cannot write {}",
                            row_location(&table, row_number, row.values())
                        ),
                        error,
                    )
                })?;
                rows_written = row_number;
                Ok(())
            })();
            if let Err(error) = result {
                failure = Some(error);
                return Err(FrankenError::Internal(
                    "logical archive stream aborted".to_owned(),
                ));
            }
            Ok(())
        });
        if let Some(error) = failure {
            // Do not hide the actionable row/stage behind a table-only
            // context, or return the engine's callback-aborted sentinel.
            return Err(error);
        }
        streamed
            .map_err(|error| scan_failure(&table.name, rows_written.saturating_add(1), error))?;
    }
    let completion = validator.completion();
    output
        .write_all(&validator.push(&Record::Completion { completion })?)
        .map_err(|error| io_failure("cannot write logical archive completion", error))?;
    output
        .flush()
        .map_err(|error| io_failure("cannot flush logical archive", error))?;
    validator.finish()
}

/// Coalesce small records instead of issuing one file write per canonical
/// row. Large records bypass BufWriter's fixed-size buffer without growing it.
fn buffered_snapshot(
    connection: &Connection,
    archive_id: String,
    output: &mut impl Write,
) -> Result<(Header, Completion)> {
    let mut buffered = BufWriter::with_capacity(EXPORT_IO_BUFFER_BYTES, output);
    // snapshot explicitly flushes before returning success. On any error,
    // discard buffered bytes rather than letting Drop silently retry a failed
    // write or flush an abandoned prefix. Publication remains export_file's
    // responsibility, after sync and read-back verification.
    let result = snapshot(connection, archive_id, &mut buffered);
    let _ = buffered.into_parts();
    result
}

pub fn export_file(
    source: &Path,
    destination: &Path,
    archive_id: String,
) -> Result<(Header, Completion)> {
    let _lock = DestinationLock::acquire(destination)?;
    ensure!(
        fs::symlink_metadata(destination)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
        "export destination already exists or cannot be inspected; it was not replaced"
    );
    let connection = open_source(source)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent(destination)?)
        .map_err(|error| io_failure("cannot create staged logical archive", error))?;
    let result = buffered_snapshot(&connection, archive_id, &mut temporary)?;
    connection
        .execute("ROLLBACK") // Release the consistent read snapshot.
        .map_err(|error| source_failure("cannot release source snapshot", error))?;
    connection
        .close_without_checkpoint()
        .map_err(|error| source_failure("cannot close read-only source archive", error))?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| io_failure("cannot sync staged logical archive", error))?;
    // Verify the actual bytes destined for publication, not just writer state.
    let input = File::open(temporary.path()).map_err(|error| {
        io_failure(
            "cannot reopen staged logical archive for verification",
            error,
        )
    })?;
    let actual = codec::verify(&mut BufReader::with_capacity(EXPORT_IO_BUFFER_BYTES, input))?;
    ensure!(
        actual == result,
        "staged export failed read-back verification"
    );
    temporary.persist_noclobber(destination).map_err(|error| {
        io_failure(
            "cannot publish logical archive without replacing an existing file",
            error.error,
        )
    })?;
    sync_parent(destination)?;
    Ok(result)
}

pub fn sync_parent(destination: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(parent(destination)?)
        .map_err(|error| {
            io_failure(
                "archive published, but cannot open parent directory for sync",
                error,
            )
        })?
        .sync_all()
        .map_err(|error| {
            io_failure(
                "archive published, but parent-directory durability could not be confirmed",
                error,
            )
        })?;
    #[cfg(not(unix))]
    let _ = destination;
    Ok(())
}

pub fn verify_file(path: &Path) -> Result<(Header, Completion)> {
    // Apply the same regular-file admission as import before any blocking read.
    // File::open alone can wait forever on a FIFO before framing limits apply.
    let input = super::import::open_input(path)?;
    codec::verify(&mut BufReader::with_capacity(EXPORT_IO_BUFFER_BYTES, input))
}

#[cfg(test)]
#[path = "export_tests.rs"]
mod tests;
