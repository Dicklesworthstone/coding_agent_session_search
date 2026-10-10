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

fn row_limit(format_version: u32) -> Result<usize> {
    match format_version {
        codec::VERSION => Ok(codec::MAX_RECORD_BYTES),
        codec::CHUNKED_VERSION => Ok(codec::MAX_ROW_BYTES),
        _ => Err(super::ArchiveUsageError("--format-version must be 1 or 2".into()).into()),
    }
}

pub fn cells(values: &[SqliteValue]) -> Result<Vec<Cell>> {
    cells_for_version(values, codec::CHUNKED_VERSION)
}

fn cells_for_version(values: &[SqliteValue], format_version: u32) -> Result<Vec<Cell>> {
    let limit = row_limit(format_version)?;
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
        // The selected transport determines admission. Neither version
        // truncates a row or changes the shared physical-frame bound.
        ensure!(
            bytes < limit,
            "logical row exceeds {} MiB",
            limit / (1024 * 1024)
        );
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
pub(super) fn row_location(table: &Table, position: u64, values: &[SqliteValue]) -> String {
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
    let failure = super::database_failure(context, error);
    let message = format!("{failure}; source was not repaired");
    failure.context(message)
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

/// Let the engine walk primary-key indexes when the persisted schema proves
/// every column uses the default BINARY collation. Explicit COLLATE wrappers
/// prevent the pinned engine from recognizing text/composite index ordering.
/// A bounded DDL check is deliberately conservative: comments or string values
/// containing COLLATE keep the explicit ordering, as do absent/large definitions.
/// Never infer default collation from an index, whose key terms may override it.
pub(super) fn ordered_scan_sql(connection: &Connection, table: &Table) -> Result<String> {
    table.validate()?;
    let metadata = connection
        .query_with_params(
            "SELECT CASE WHEN sql IS NOT NULL AND length(sql) <= 65536 \
             THEN instr(upper(sql), 'COLLATE') = 0 ELSE 0 END \
             FROM sqlite_master WHERE type = 'table' AND name = ?1 LIMIT 2",
            &[SqliteValue::Text(table.name.clone().into())],
        )
        .map_err(|error| {
            source_failure(
                format!("cannot inspect ordering for logical table {}", table.name),
                error,
            )
        })?;
    ensure!(
        metadata.len() <= 1,
        "logical table ordering metadata is ambiguous"
    );
    let default_binary = metadata
        .first()
        .map(|row| row.get_typed::<i64>(0))
        .transpose()
        .map_err(|error| {
            source_failure(
                format!("cannot decode ordering for logical table {}", table.name),
                error,
            )
        })?
        == Some(1);
    let columns = table
        .columns
        .iter()
        .map(|name| quoted(name))
        .collect::<Result<Vec<_>>>()?
        .join(", ");
    let collation = if default_binary { "" } else { " COLLATE BINARY" };
    let order = table
        .primary_key
        .iter()
        .map(|&offset| Ok(format!("{}{collation} ASC", quoted(&table.columns[offset])?)))
        .collect::<Result<Vec<_>>>()?
        .join(", ");
    Ok(format!(
        "SELECT {columns} FROM {} ORDER BY {order}",
        quoted(&table.name)?
    ))
}

pub fn snapshot(
    connection: &Connection,
    archive_id: String,
    output: &mut impl Write,
) -> Result<(Header, Completion)> {
    snapshot_for_version(connection, archive_id, codec::CHUNKED_VERSION, output)
}

fn snapshot_for_version(
    connection: &Connection,
    archive_id: String,
    format_version: u32,
    output: &mut impl Write,
) -> Result<(Header, Completion)> {
    row_limit(format_version)?;
    let tables = tables(connection)?;
    let header = Header {
        format: codec::FORMAT.to_owned(),
        schema_version: format_version,
        archive_id,
        exported_at_ms: chrono::Utc::now().timestamp_millis(),
        storage_schema_version: schema_version(connection)?,
        record_types: codec::record_types(format_version),
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
        let sql = ordered_scan_sql(connection, &table)?;
        let mut failure = None;
        let mut rows_written = 0u64;
        let streamed = connection.query_with_params_for_each(&sql, &[], |row| {
            let result = (|| -> Result<()> {
                // This is a one-based position in the table's exported PK
                // order. row_location labels an integer key separately.
                let row_number = rows_written
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("logical table row position overflow"))?;
                let converted = if format_version == codec::CHUNKED_VERSION {
                    cells(row.values())
                } else {
                    cells_for_version(row.values(), format_version)
                };
                let record = Record::Row {
                    values: converted.map_err(|error| {
                        row_failure(
                            row_location(&table, row_number, row.values()),
                            "cell conversion",
                            error,
                        )
                    })?,
                };
                let prepared = validator.prepare(&record).map_err(|error| {
                    row_failure(
                        row_location(&table, row_number, row.values()),
                        "record validation/encoding",
                        error,
                    )
                })?;
                prepared.write_to(output).map_err(|error| {
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
    buffered_snapshot_for_version(connection, archive_id, codec::CHUNKED_VERSION, output)
}

fn buffered_snapshot_for_version(
    connection: &Connection,
    archive_id: String,
    format_version: u32,
    output: &mut impl Write,
) -> Result<(Header, Completion)> {
    let mut buffered = BufWriter::with_capacity(EXPORT_IO_BUFFER_BYTES, output);
    // snapshot explicitly flushes before returning success. On any error,
    // discard buffered bytes rather than letting Drop silently retry a failed
    // write or flush an abandoned prefix. Publication remains export_file's
    // responsibility, after sync and read-back verification.
    let result = if format_version == codec::CHUNKED_VERSION {
        snapshot(connection, archive_id, &mut buffered)
    } else {
        snapshot_for_version(connection, archive_id, format_version, &mut buffered)
    };
    let _ = buffered.into_parts();
    result
}

pub fn export_file(
    source: &Path,
    destination: &Path,
    archive_id: String,
) -> Result<(Header, Completion)> {
    export_file_for_version(source, destination, archive_id, codec::CHUNKED_VERSION)
}

pub(super) fn export_file_for_version(
    source: &Path,
    destination: &Path,
    archive_id: String,
    format_version: u32,
) -> Result<(Header, Completion)> {
    row_limit(format_version)?; // Refuse unsupported selections before filesystem mutation.
    let _lock = DestinationLock::acquire(destination)?;
    ensure!(
        fs::symlink_metadata(destination)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
        "export destination already exists or cannot be inspected; it was not replaced"
    );
    let connection = open_source(source)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent(destination)?)
        .map_err(|error| io_failure("cannot create staged logical archive", error))?;
    let result = if format_version == codec::CHUNKED_VERSION {
        buffered_snapshot(&connection, archive_id, &mut temporary)?
    } else {
        buffered_snapshot_for_version(&connection, archive_id, format_version, &mut temporary)?
    };
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

#[cfg(test)]
#[path = "v2_export_tests.rs"]
mod v2_tests;

#[cfg(test)]
mod ordering_tests {
    use super::*;

    fn opcodes(connection: &Connection, sql: &str) -> Result<Vec<String>> {
        connection
            .query(&format!("EXPLAIN {sql}"))?
            .iter()
            .map(|row| row.get_typed::<String>(1).map_err(anyhow::Error::from))
            .collect()
    }

    #[test]
    fn implicit_binary_primary_key_scans_avoid_sorters_and_verify() -> Result<()> {
        let source = tempfile::tempdir()?;
        let destination = tempfile::tempdir()?;
        let database = source.path().join("source.db");
        let connection = Connection::open(path_text(&database)?)?;
        connection.execute_batch(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO meta VALUES ('schema_version', '9');
             CREATE TABLE binary_keys (k BLOB PRIMARY KEY, body TEXT);
             INSERT INTO binary_keys VALUES
               ('a', 'private body'), (7, 'private body'), (X'FF', 'private body'),
               ('Z', 'private body'), (-2, 'private body'), (X'0001', 'private body'),
               (X'00', 'private body'), ('007', 'private body');
             CREATE TABLE composite_keys (
               tenant TEXT, sequence INTEGER, body TEXT,
               PRIMARY KEY (tenant, sequence));
             INSERT INTO composite_keys VALUES
               ('a', 2, 'private body'), ('Z', 1, 'private body'),
               ('A', 4, 'private body'), ('a', -3, 'private body'),
               ('A', -2, 'private body');
             CREATE TABLE large_keys (k TEXT PRIMARY KEY, body TEXT);",
        )?;
        for key in ["\0", "a\0z", "é"] {
            connection.execute_with_params(
                "INSERT INTO binary_keys VALUES (?1, 'private body')",
                &[SqliteValue::Text(key.to_owned().into())],
            )?;
        }
        connection.execute("BEGIN")?;
        for index in (0..512).rev() {
            connection.execute_with_params(
                "INSERT INTO large_keys VALUES (?1, ?2)",
                &[
                    SqliteValue::Text(format!("key-{index:06}").into()),
                    SqliteValue::Text("private collection body ".repeat(16).into()),
                ],
            )?;
        }
        connection.execute("COMMIT")?;
        connection.close()?;
        let connection = open_source(&database)?;
        let descriptors = tables(&connection)?;
        for name in ["binary_keys", "composite_keys", "large_keys"] {
            let table = descriptors.iter().find(|table| table.name == name).unwrap();
            let sql = ordered_scan_sql(&connection, table)?;
            let plan = opcodes(&connection, &sql)?;
            assert!(
                plan.iter().any(|opcode| opcode == "OpenRead"),
                "{name}: {plan:?}"
            );
            assert!(
                !plan.iter().any(|opcode| opcode.starts_with("Sorter")),
                "{name}: {plan:?}"
            );
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
            let original = format!(
                "SELECT {columns} FROM {} ORDER BY {order}",
                quoted(&table.name)?
            );
            assert!(
                opcodes(&connection, &original)?
                    .iter()
                    .any(|opcode| opcode == "SorterOpen"),
                "positive control did not exercise the old sorter for {name}"
            );
            if name == "large_keys" {
                let mut count = 0;
                connection.query_with_params_for_each(&sql, &[], |row| {
                    assert_eq!(row.get_typed::<String>(0)?, format!("key-{count:06}"));
                    count += 1;
                    Ok(())
                })?;
                assert_eq!(count, 512);
            }
        }
        let table = descriptors
            .iter()
            .find(|table| table.name == "binary_keys")
            .unwrap();
        let actual = connection
            .query(&ordered_scan_sql(&connection, table)?)?
            .iter()
            .map(|row| cells(row.values()).map(|values| values[0].clone()))
            .collect::<Result<Vec<_>>>()?;
        assert_eq!(
            actual,
            vec![
                Cell::Integer(-2),
                Cell::Integer(7),
                Cell::Text("\0".into()),
                Cell::Text("007".into()),
                Cell::Text("Z".into()),
                Cell::Text("a".into()),
                Cell::Text("a\0z".into()),
                Cell::Text("é".into()),
                Cell::Blob("AA==".into()),
                Cell::Blob("AAE=".into()),
                Cell::Blob("/w==".into()),
            ]
        );
        let table = descriptors
            .iter()
            .find(|table| table.name == "composite_keys")
            .unwrap();
        let actual = connection
            .query(&ordered_scan_sql(&connection, table)?)?
            .iter()
            .map(|row| Ok((row.get_typed::<String>(0)?, row.get_typed::<i64>(1)?)))
            .collect::<Result<Vec<_>>>()?;
        assert_eq!(
            actual,
            vec![
                ("A".into(), -2),
                ("A".into(), 4),
                ("Z".into(), 1),
                ("a".into(), -3),
                ("a".into(), 2),
            ]
        );
        connection.execute("ROLLBACK")?;
        connection.close_without_checkpoint()?;
        let output = destination.path().join("archive.jsonl");
        let receipt = export_file(&database, &output, "binary-index-scan".into())?;
        assert_eq!(receipt, verify_file(&output)?);
        assert_eq!(receipt.1.tables["large_keys"], 512);
        assert_eq!(receipt.1.tables["binary_keys"], 11);
        assert_eq!(receipt.1.tables["composite_keys"], 5);
        Ok(())
    }

    #[test]
    fn explicit_or_unattested_collations_keep_binary_order_and_verify() -> Result<()> {
        let source = tempfile::tempdir()?;
        let destination = tempfile::tempdir()?;
        let database = source.path().join("source.db");
        let connection = Connection::open(path_text(&database)?)?;
        connection.execute_batch(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO meta VALUES ('schema_version', '9');
             CREATE TABLE declared_nocase (k TEXT COLLATE NOCASE PRIMARY KEY, body TEXT);
             CREATE TABLE pk_nocase (k TEXT, body TEXT, PRIMARY KEY(k COLLATE NOCASE));
             CREATE TABLE overridden_binary (
               k TEXT COLLATE NOCASE, body TEXT, PRIMARY KEY(k COLLATE BINARY));
             CREATE TABLE explicit_binary (k TEXT COLLATE BINARY PRIMARY KEY, body TEXT);
             CREATE TABLE literal_keyword (
               k TEXT PRIMARY KEY, body TEXT DEFAULT 'PRIVATE-COLLATE-DEFAULT');",
        )?;
        let large_default = "x".repeat(65_537);
        connection.execute(&format!(
            "CREATE TABLE oversized_definition (
               k TEXT PRIMARY KEY, body TEXT DEFAULT 'PRIVATE-LONG-DEFAULT-{large_default}')"
        ))?;
        let names = [
            "declared_nocase",
            "pk_nocase",
            "overridden_binary",
            "explicit_binary",
            "literal_keyword",
            "oversized_definition",
        ];
        for name in names {
            connection.execute(&format!(
                "INSERT INTO {} VALUES ('a', 'private body'), ('Z', 'private body')",
                quoted(name)?
            ))?;
        }
        connection.close()?;
        let connection = open_source(&database)?;
        let descriptors = tables(&connection)?;
        for name in names {
            let table = descriptors.iter().find(|table| table.name == name).unwrap();
            let sql = ordered_scan_sql(&connection, table)?;
            assert!(
                sql.contains("\"k\" COLLATE BINARY ASC"),
                "fallback was lost for {name}"
            );
            assert!(!sql.contains("PRIVATE-"), "DDL contents entered the scan SQL");
            let actual = connection
                .query(&sql)?
                .iter()
                .map(|row| row.get_typed::<String>(0).map_err(anyhow::Error::from))
                .collect::<Result<Vec<_>>>()?;
            assert_eq!(
                actual,
                vec!["Z".to_owned(), "a".to_owned()],
                "BINARY order changed for {name}"
            );
        }
        connection.execute("ROLLBACK")?;
        connection.close_without_checkpoint()?;
        for version in [codec::VERSION, codec::CHUNKED_VERSION] {
            let output = destination.path().join(format!("archive-v{version}.jsonl"));
            let receipt =
                export_file_for_version(&database, &output, "collation-fallback".into(), version)?;
            assert_eq!(receipt, verify_file(&output)?);
            for name in names {
                assert_eq!(receipt.1.tables[name], 2);
            }
        }
        Ok(())
    }

    #[test]
    fn absent_ordering_metadata_preserves_the_database_read_error() -> Result<()> {
        let connection = Connection::open(":memory:")?;
        let table = Table {
            name: "absent_table".into(),
            columns: vec!["k".into(), "body".into()],
            primary_key: vec![0],
        };
        let sql = ordered_scan_sql(&connection, &table)?;
        assert!(sql.contains("\"k\" COLLATE BINARY ASC"));
        let error = connection
            .query_with_params_for_each(&sql, &[], |_| Ok(()))
            .expect_err("a missing catalog entry must still reach the original table read");
        assert!(matches!(error, FrankenError::NoSuchTable { .. }));
        let error = scan_failure(&table.name, 1, error);
        assert_eq!(
            super::super::classify_failure(&error),
            (9, "logical-archive-error", false)
        );
        assert!(error.to_string().contains("absent_table before row 1"));
        assert!(matches!(
            error.downcast_ref::<FrankenError>(),
            Some(FrankenError::NoSuchTable { .. })
        ));
        connection.close()?;
        Ok(())
    }

    #[test]
    fn indexed_and_collated_text_key_write_failures_remain_private() -> Result<()> {
        struct LimitedWriter {
            remaining: usize,
        }

        impl Write for LimitedWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if self.remaining == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "PRIVATE-WRITER-DETAIL",
                    ));
                }
                let count = bytes.len().min(self.remaining);
                self.remaining -= count;
                Ok(count)
            }

            fn flush(&mut self) -> io::Result<()> {
                panic!("an incomplete snapshot must not be flushed");
            }
        }

        for collation in ["", " COLLATE NOCASE"] {
            let source = tempfile::tempdir()?;
            let database = source.path().join("source.db");
            let connection = Connection::open(path_text(&database)?)?;
            connection.execute_batch(&format!(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO meta VALUES ('schema_version', '9');
                 CREATE TABLE a_private (
                   k TEXT{collation} PRIMARY KEY, body TEXT DEFAULT 'PRIVATE-DDL-CONTENT');
                 INSERT INTO a_private VALUES ('PRIVATE-KEY-ONE', 'PRIVATE-MESSAGE-ONE');
                 INSERT INTO a_private VALUES ('PRIVATE-KEY-TWO', 'PRIVATE-MESSAGE-TWO');"
            ))?;
            connection.close()?;
            let connection = open_source(&database)?;
            let mut complete = Vec::new();
            snapshot(&connection, "private-key-failure".into(), &mut complete)?;
            let mut offset = 0;
            let mut failure_offset = None;
            for line in complete.split_inclusive(|byte| *byte == b'\n') {
                if let Record::Row { values } = serde_json::from_slice(line)?
                    && values[0] == Cell::Text("PRIVATE-KEY-TWO".into())
                {
                    failure_offset = Some(offset + line.len() / 2);
                    break;
                }
                offset += line.len();
            }
            let mut writer = LimitedWriter {
                remaining: failure_offset.expect("fixture has a second private row"),
            };
            let error = snapshot(&connection, "private-key-failure".into(), &mut writer)
                .expect_err("a partial row must abort either ordering path");
            let message = error.to_string();
            assert!(
                message.contains("logical table a_private, row 2"),
                "{message}"
            );
            assert!(message.contains("WriteZero"), "{message}");
            assert!(!message.contains("PRIVATE-"), "{message}");
            assert_eq!(
                super::super::classify_failure(&error),
                (14, "logical-archive-io", true)
            );
            assert_eq!(writer.remaining, 0);
            connection.execute("ROLLBACK")?;
            connection.close_without_checkpoint()?;
        }
        Ok(())
    }
}
