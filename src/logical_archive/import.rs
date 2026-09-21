//! Restore a verified logical archive into a NEW canonical database. The only
//! executable schema comes from this binary's storage initializer, never input.
//! Batches are private until the whole stream and the persisted rows are proved.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use coding_agent_search::franken_sync::compat::RowExt;
use coding_agent_search::franken_sync::{Connection, FrankenError, SqliteValue};
use coding_agent_search::storage::sqlite::SqliteStorage;

use super::codec::{self, Cell, Completion, Header, Record, Table, Validator};
use super::export::{self, DestinationLock};

const MAX_BATCH_RECORDS: usize = 128;
const MAX_BATCH_BYTES: usize = 16 * 1024 * 1024;
const MAX_TRIGGER_BYTES: usize = 1024 * 1024;

/// Count consumed input, not re-encoded JSON: whitespace also costs admission.
struct Input<R> {
    inner: R,
    record_bytes: usize,
}

impl<R: BufRead> Input<R> {
    fn new(inner: R) -> Self {
        Self { inner, record_bytes: 0 }
    }

    fn record(&mut self, line: u64) -> Result<Option<Record>> {
        self.record_bytes = 0;
        codec::read_record(self, line)
    }
}

impl<R: BufRead> Read for Input<R> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let count = self.inner.read(bytes)?;
        self.record_bytes = self.record_bytes.saturating_add(count);
        Ok(count)
    }
}

impl<R: BufRead> BufRead for Input<R> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        self.inner.fill_buf()
    }

    fn consume(&mut self, count: usize) {
        self.inner.consume(count);
        self.record_bytes = self.record_bytes.saturating_add(count);
    }
}

/// Open one pinned regular file. Do not block on a FIFO, follow a link, or reopen
/// the pathname between header admission and completion verification.
pub(super) fn open_input(path: &Path) -> Result<File> {
    open_regular(path, false)
}

fn sync_candidate(path: &Path) -> Result<()> {
    // Windows FlushFileBuffers requires GENERIC_WRITE. A read-only File::open
    // can read back a valid candidate but cannot durably flush it there.
    // Only our unpublished candidate reaches this writable path; verification
    // and existing-destination comparisons keep their strictly read-only opens.
    open_regular(path, true)?.sync_all()
        .context("cannot sync the verified private restore candidate")
}

fn open_regular(path: &Path, writable: bool) -> Result<File> {
    let metadata = fs::symlink_metadata(path).context("cannot inspect logical archive file")?;
    ensure!(metadata.is_file() && !metadata.file_type().is_symlink(), "logical archive file must be a regular, non-symlink file");
    let mut options = OpenOptions::new();
    options.read(true).write(writable);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    let file = options.open(path).context("cannot open logical archive file")?;
    ensure!(file.metadata()?.is_file(), "logical archive file is not a regular file");
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        ensure!(file.metadata()?.file_attributes() & 0x400 == 0, "logical archive file is a reparse point");
    }
    Ok(file)
}

fn sidecars(path: &Path) -> [PathBuf; 3] {
    ["-wal", "-shm", "-journal"].map(|suffix| {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        PathBuf::from(name)
    })
}

fn require_absent(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        _ => bail!("restore destination or SQLite sidecar already exists or cannot be inspected; nothing was replaced"),
    }
}

fn require_new_destination(path: &Path) -> Result<()> {
    require_absent(path)?;
    for sidecar in sidecars(path) {
        require_absent(&sidecar)?;
    }
    Ok(())
}

fn values(cells: Vec<Cell>) -> Result<Vec<SqliteValue>> {
    cells.into_iter().map(|cell| {
        cell.validate()?;
        Ok(match cell {
            Cell::Null => SqliteValue::Null,
            Cell::Integer(value) => SqliteValue::Integer(value),
            Cell::Real(bits) => SqliteValue::Float(f64::from_bits(
                u64::from_str_radix(&bits, 16).map_err(|_| anyhow!("invalid REAL encoding"))?,
            )),
            Cell::Text(value) => SqliteValue::Text(value.into()),
            Cell::Blob(value) => SqliteValue::Blob(
                STANDARD.decode(value).map_err(|_| anyhow!("invalid BLOB encoding"))?.into(),
            ),
        })
    }).collect()
}

fn insert_sql(table: &Table) -> Result<String> {
    table.validate()?;
    let columns = table.columns.iter().map(|name| export::quoted(name))
        .collect::<Result<Vec<_>>>()?.join(", ");
    let placeholders = vec!["?"; table.columns.len()].join(", ");
    // No OR REPLACE / IGNORE: constraints and duplicate identities must fail.
    Ok(format!("INSERT INTO {} ({columns}) VALUES ({placeholders})", export::quoted(&table.name)?))
}

/// These statements are read ONLY from the freshly initialized private schema.
/// Suspending its triggers avoids replaying derived writes while importing the
/// corresponding canonical ledger rows. Restore the same trusted definitions.
fn suspend_triggers(connection: &Connection) -> Result<Vec<String>> {
    let rows = connection.query(
        "SELECT name, substr(sql, 1, 65537) FROM sqlite_master WHERE type = 'trigger' ORDER BY name LIMIT 257",
    )?;
    ensure!(rows.len() <= 256, "canonical schema exceeds restore trigger limit");
    let mut statements = Vec::new();
    let mut bytes = 0usize;
    for row in rows {
        let name = row.get_typed::<String>(0)?;
        let sql = row.get_typed::<String>(1)?;
        bytes = bytes.saturating_add(sql.len());
        ensure!(sql.len() <= 65536 && bytes <= MAX_TRIGGER_BYTES, "canonical trigger definitions exceed restore budget");
        connection.execute(&format!("DROP TRIGGER {}", export::quoted(&name)?))?;
        statements.push(sql);
    }
    Ok(statements)
}

pub(super) fn verify_database(connection: &Connection) -> Result<()> {
    let mut violated = false;
    let check = connection.query_with_params_for_each("PRAGMA foreign_key_check", &[], |_| {
        violated = true;
        Err(FrankenError::Internal("restore foreign-key check failed".to_owned()))
    });
    ensure!(!violated, "restored archive has broken canonical relationships");
    check.context("cannot check restored archive relationships")?;

    let mut rows = 0usize;
    let check = connection.query_with_params_for_each("PRAGMA integrity_check", &[], |row| {
        rows = rows.saturating_add(1);
        if row.get_typed::<String>(0)? != "ok" {
            return Err(FrankenError::Internal("restore integrity check failed".to_owned()));
        }
        Ok(())
    });
    check.map_err(|_| anyhow!("restored database failed integrity verification"))?;
    ensure!(rows > 0, "restored database supplied no integrity result");
    Ok(())
}

/// Restore only an exact schema produced by this binary. A header claiming the
/// right version is insufficient: every descriptor must match, with none absent.
fn restore<R: BufRead>(
    connection: &Connection,
    input: &mut Input<R>,
    header: Header,
) -> Result<(Header, Completion)> {
    let expected = export::tables(connection)?;
    ensure!(export::schema_version(connection)? == header.storage_schema_version, "logical archive storage schema differs from this binary; cross-schema migration is not supported");
    let mut validator = Validator::new(header)?;
    connection.execute("PRAGMA foreign_keys = OFF")?;
    ensure!(connection.query_row("PRAGMA foreign_keys")?.get_typed::<i64>(0)? == 0, "cannot suspend foreign-key enforcement for ordered restoration");
    connection.execute("BEGIN IMMEDIATE")?;
    let triggers = suspend_triggers(connection)?;
    for table in &expected {
        // Remove initializer seeds only in this unpublished, freshly made DB.
        connection.execute(&format!("DELETE FROM {}", export::quoted(&table.name)?))?;
    }
    let mut table_count = 0usize;
    let mut statement = None;
    let mut batch_records = 0usize;
    let mut batch_bytes = 0usize;
    let mut line = 2u64;
    while let Some(record) = input.record(line)? {
        if batch_records == MAX_BATCH_RECORDS
            || input.record_bytes > MAX_BATCH_BYTES.saturating_sub(batch_bytes)
        {
            connection.execute("COMMIT")?;
            connection.execute("BEGIN IMMEDIATE")?;
            batch_records = 0;
            batch_bytes = 0;
        }
        validator.push(&record).map_err(|error| anyhow!("record {line}: {error}"))?;
        match record {
            Record::Table { table } => {
                ensure!(expected.get(table_count) == Some(&table), "record {line}: logical table does not match this binary's canonical schema");
                statement = Some(insert_sql(&table)?);
                table_count += 1;
            }
            Record::Row { values: cells } => {
                let sql = statement.as_ref().ok_or_else(|| anyhow!("record {line}: row precedes its table"))?;
                connection.execute_with_params(sql, &values(cells)?)
                    .map_err(|_| anyhow!("record {line}: canonical row insertion failed; no destination was published"))?;
            }
            Record::Completion { .. } => {}
            Record::Header { .. } => bail!("record {line}: duplicate archive header"),
        }
        batch_records += 1;
        batch_bytes += input.record_bytes;
        line = line.checked_add(1).ok_or_else(|| anyhow!("logical record position overflow"))?;
    }
    let result = validator.finish()?;
    ensure!(table_count == expected.len(), "logical archive omits canonical tables required by this binary");
    ensure!(export::schema_version(connection)? == result.0.storage_schema_version, "archive header and canonical schema metadata disagree");
    for statement in triggers {
        connection.execute_batch(&statement)?;
    }
    verify_database(connection)?;
    connection.execute("COMMIT")?;
    Ok(result)
}

pub fn import_file(
    input: &Path,
    destination: &Path,
    expected_archive_id: &str,
) -> Result<(Header, Completion)> {
    let (header, completion, _) =
        import_file_with_policy(input, destination, expected_archive_id, false)?;
    Ok((header, completion))
}

/// The boolean receipt is true only when this call publishes a NEW database.
/// With opt-in, an existing target can succeed solely as a read-only comparison.
pub fn import_file_with_policy(
    input: &Path,
    destination: &Path,
    expected_archive_id: &str,
    if_identical: bool,
) -> Result<(Header, Completion, bool)> {
    let mut input = Input::new(BufReader::new(open_input(input)?));
    let Some(Record::Header { header }) = input.record(1)? else {
        bail!("logical archive must begin with a header");
    };
    header.validate()?;
    ensure!(header.archive_id == expected_archive_id, "logical archive identity does not match --archive-id");
    let _lock = DestinationLock::acquire(destination)?;
    if if_identical && fs::symlink_metadata(destination).is_ok() {
        let (header, completion) =
            super::reimport::verify_existing(&mut input, header, destination)?;
        return Ok((header, completion, false));
    }
    require_new_destination(destination)?;

    let staging = tempfile::Builder::new().prefix(".cass-restore-")
        .tempdir_in(export::parent(destination)?)?;
    let candidate = staging.path().join("agent_search.db");
    let storage = SqliteStorage::open(&candidate).context("cannot initialize canonical restore candidate")?;
    drop(storage);
    let connection = Connection::open(export::path_text(&candidate)?)?;
    connection.execute("PRAGMA busy_timeout = 5000")?;
    // A single-file publication must never depend on an unpublished WAL.
    let mode = connection.query_row("PRAGMA journal_mode = DELETE")?.get_typed::<String>(0)?;
    ensure!(mode.eq_ignore_ascii_case("delete"), "cannot make restore candidate independent of WAL sidecars");
    connection.execute("PRAGMA synchronous = FULL")?;
    let result = restore(&connection, &mut input, header)?;
    connection.close()?;

    // Reopen the persisted database and hash its actual typed rows, descriptors,
    // metadata and relationships. Input verification alone cannot detect an
    // affinity conversion, initializer side effect, or storage write defect.
    let reader = export::open_source(&candidate)?;
    let actual = export::snapshot(&reader, result.0.archive_id.clone(), &mut io::sink())?;
    reader.execute("ROLLBACK")?;
    reader.close_without_checkpoint()?;
    ensure!(actual.1 == result.1, "restored database does not reproduce the archive's canonical digest");
    for sidecar in sidecars(&candidate) {
        require_absent(&sidecar).context("restore candidate still depends on SQLite sidecars")?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&candidate, fs::Permissions::from_mode(0o600))?;
    }
    sync_candidate(&candidate)?;
    require_new_destination(destination)?;
    // Same-filesystem hard-link publication is atomic and never replaces an
    // existing name (including symlinks). No rename/copy-over fallback is safe.
    fs::hard_link(&candidate, destination)
        .context("cannot publish restored archive without replacing existing data; destination must support hard links")?;
    export::sync_parent(destination)?;
    Ok((result.0, result.1, true))
}

#[cfg(test)]
#[path = "import_tests.rs"]
mod tests;
