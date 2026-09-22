//! Read-only canonical snapshot export. Derived virtual/shadow tables are not
//! authority and are omitted; unfamiliar unkeyed tables fail rather than vanish.

use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, ensure};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use coding_agent_search::franken_sync::compat::{OpenFlags, RowExt, open_with_flags};
use coding_agent_search::franken_sync::{Connection, FrankenError, SqliteValue};
use fs2::FileExt;

use super::codec::{self, Cell, Completion, Header, Record, Table, Validator};

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
            match FileExt::try_lock_exclusive(&file) {
                Ok(()) => return Ok(Self { _file: file }),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Err(super::ArchiveBusyError(
                            "logical archive destination remained locked for five seconds".into(),
                        )
                        .into());
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(error) => {
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
        .context("cannot open source archive read-only; no repair was attempted")?;
    connection.execute("PRAGMA busy_timeout = 5000")?;
    connection.execute("PRAGMA query_only = ON")?;
    connection.execute("BEGIN")?;
    Ok(connection)
}

pub fn schema_version(connection: &Connection) -> Result<String> {
    connection
        .query_row("SELECT value FROM meta WHERE key = 'schema_version'")?
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
    let rows = connection.query(
        "SELECT name, substr(sql, 1, 64) FROM sqlite_master WHERE type = 'table' ORDER BY name LIMIT 1025",
    )?;
    ensure!(
        rows.len() <= 1024,
        "source has too many schema objects for logical export"
    );
    let mut physical = Vec::new();
    let mut virtual_roots = Vec::new();
    for row in rows {
        let name = row.get_typed::<String>(0)?;
        let sql = row.get_typed::<Option<String>>(1)?.unwrap_or_default();
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
        let columns = connection.query(&format!("PRAGMA table_info({})", quoted(&name)?))?;
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
            table.columns.push(column.get_typed::<String>(1)?);
            let ordinal = column.get_typed::<i64>(5)?;
            if ordinal > 0 {
                primary_key.push((ordinal, offset));
            }
        }
        primary_key.sort_unstable();
        table.primary_key = primary_key.into_iter().map(|(_, offset)| offset).collect();
        table
            .validate()
            .with_context(|| format!("unsupported logical table {}", table.name))?;
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
            SqliteValue::Text(text) => text.len(),
            SqliteValue::Blob(blob) => blob
                .len()
                .checked_add(2)
                .and_then(|n| n.checked_div(3))
                .and_then(|n| n.checked_mul(4))
                .ok_or_else(|| anyhow!("oversized logical BLOB"))?,
            _ => 32,
        };
        bytes = bytes
            .checked_add(size)
            .ok_or_else(|| anyhow!("oversized logical row"))?;
        ensure!(bytes < codec::MAX_RECORD_BYTES, "logical row exceeds 8 MiB");
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

pub fn snapshot(
    connection: &Connection,
    archive_id: String,
    output: &mut impl Write,
) -> Result<(Header, Completion)> {
    let tables = tables(connection)?;
    let header = Header {
        format: codec::FORMAT.to_owned(),
        schema_version: codec::VERSION,
        archive_id,
        exported_at_ms: chrono::Utc::now().timestamp_millis(),
        storage_schema_version: schema_version(connection)?,
        record_types: ["table", "row", "completion"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        contains_private_data: true,
        omissions: vec!["derived_search_assets".to_owned()],
    };
    let mut validator = Validator::new(header.clone())?;
    output.write_all(&codec::encode(&Record::Header {
        header: header.clone(),
    })?)?;
    for table in tables {
        output.write_all(&validator.push(&Record::Table {
            table: table.clone(),
        })?)?;
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
        let streamed = connection.query_with_params_for_each(&sql, &[], |row| {
            let result = (|| -> Result<()> {
                let record = Record::Row {
                    values: cells(row.values())?,
                };
                output.write_all(&validator.push(&record)?)?;
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
            return Err(error).with_context(|| format!("logical table {}", table.name));
        }
        streamed.map_err(|_| {
            anyhow!(
                "cannot stream logical table {}; source was not repaired",
                table.name
            )
        })?;
    }
    let completion = validator.completion();
    output.write_all(&validator.push(&Record::Completion { completion })?)?;
    output.flush()?;
    validator.finish()
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
    let mut temporary = tempfile::NamedTempFile::new_in(parent(destination)?)?;
    let result = snapshot(&connection, archive_id, &mut temporary)?;
    connection.execute("ROLLBACK")?; // Release the consistent read snapshot.
    connection.close_without_checkpoint()?;
    temporary.as_file().sync_all()?;
    // Verify the actual bytes destined for publication, not just writer state.
    let actual = codec::verify(&mut BufReader::new(File::open(temporary.path())?))?;
    ensure!(
        actual == result,
        "staged export failed read-back verification"
    );
    temporary.persist_noclobber(destination).map_err(|_| {
        anyhow!("cannot publish logical archive without replacing an existing file")
    })?;
    sync_parent(destination)?;
    Ok(result)
}

pub fn sync_parent(destination: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(parent(destination)?)?
        .sync_all()
        .context("archive published, but parent-directory durability could not be confirmed")?;
    #[cfg(not(unix))]
    let _ = destination;
    Ok(())
}

pub fn verify_file(path: &Path) -> Result<(Header, Completion)> {
    // Apply the same regular-file admission as import before any blocking read.
    // File::open alone can wait forever on a FIFO before framing limits apply.
    let input = super::import::open_input(path)?;
    codec::verify(&mut BufReader::new(input))
}

#[cfg(test)]
#[path = "export_tests.rs"]
mod tests;
