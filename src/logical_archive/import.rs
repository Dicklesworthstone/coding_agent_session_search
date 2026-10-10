//! Restore a verified logical archive into a NEW canonical database. The only
//! executable schema comes from this binary's storage initializer, never input.
//! Batches are private until the whole stream and the persisted rows are proved.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use coding_agent_search::franken_sync::compat::RowExt;
use coding_agent_search::franken_sync::{Connection, FrankenError, SqliteValue};
use coding_agent_search::storage::sqlite::{SqliteStorage, active_schema_migration_versions};

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
        Self {
            inner,
            record_bytes: 0,
        }
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
    open_regular(path, true)?
        .sync_all()
        .context("cannot sync the verified private restore candidate")
}

fn open_regular(path: &Path, writable: bool) -> Result<File> {
    let metadata = fs::symlink_metadata(path).context("cannot inspect logical archive file")?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "logical archive file must be a regular, non-symlink file"
    );
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
    let file = options
        .open(path)
        .context("cannot open logical archive file")?;
    ensure!(
        file.metadata()?.is_file(),
        "logical archive file is not a regular file"
    );
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        ensure!(
            file.metadata()?.file_attributes() & 0x400 == 0,
            "logical archive file is a reparse point"
        );
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
        _ => bail!(
            "restore destination or SQLite sidecar already exists or cannot be inspected; nothing was replaced"
        ),
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
    cells
        .into_iter()
        .map(|cell| {
            cell.validate()?;
            Ok(match cell {
                Cell::Null => SqliteValue::Null,
                Cell::Integer(value) => SqliteValue::Integer(value),
                Cell::Real(bits) => SqliteValue::Float(f64::from_bits(
                    u64::from_str_radix(&bits, 16).map_err(|_| anyhow!("invalid REAL encoding"))?,
                )),
                Cell::Text(value) => SqliteValue::Text(value.into()),
                Cell::Blob(value) => SqliteValue::Blob(
                    STANDARD
                        .decode(value)
                        .map_err(|_| anyhow!("invalid BLOB encoding"))?
                        .into(),
                ),
            })
        })
        .collect()
}

fn insert_sql(table: &Table) -> Result<String> {
    table.validate()?;
    let columns = table
        .columns
        .iter()
        .map(|name| export::quoted(name))
        .collect::<Result<Vec<_>>>()?
        .join(", ");
    let placeholders = vec!["?"; table.columns.len()].join(", ");
    // No OR REPLACE / IGNORE: constraints and duplicate identities must fail.
    Ok(format!(
        "INSERT INTO {} ({columns}) VALUES ({placeholders})",
        export::quoted(&table.name)?
    ))
}

/// These statements are read ONLY from the freshly initialized private schema.
/// Suspending its triggers avoids replaying derived writes while importing the
/// corresponding canonical ledger rows. Restore the same trusted definitions.
fn suspend_triggers(connection: &Connection) -> Result<Vec<String>> {
    let rows = connection
        .query(
            "SELECT name, substr(sql, 1, 65537) FROM sqlite_master WHERE type = 'trigger' ORDER BY name LIMIT 257",
        )
        .map_err(|error| super::database_failure("cannot read canonical restore triggers", error))?;
    ensure!(
        rows.len() <= 256,
        "canonical schema exceeds restore trigger limit"
    );
    let mut statements = Vec::new();
    let mut bytes = 0usize;
    for row in rows {
        let name = row.get_typed::<String>(0).map_err(|error| {
            super::database_failure("cannot decode a canonical restore trigger name", error)
        })?;
        let sql = row.get_typed::<String>(1).map_err(|error| {
            super::database_failure(
                "cannot decode a canonical restore trigger definition",
                error,
            )
        })?;
        bytes = bytes.saturating_add(sql.len());
        ensure!(
            sql.len() <= 65536 && bytes <= MAX_TRIGGER_BYTES,
            "canonical trigger definitions exceed restore budget"
        );
        connection
            .execute(&format!("DROP TRIGGER {}", export::quoted(&name)?))
            .map_err(|error| {
                super::database_failure("cannot suspend a canonical restore trigger", error)
            })?;
        statements.push(sql);
    }
    Ok(statements)
}

/// Canonical row equality does not establish the schema authority that ordinary
/// storage opens will use. Check both markers and every active migration through
/// the admitted version in the caller's existing snapshot. The engine replays
/// missing lower versions even when the maximum version is already current.
/// The admitted version may be historical during a read-only identical retry;
/// new restores have already matched it to this binary's initializer.
pub(super) fn verify_schema_authority(
    connection: &Connection,
    expected_storage_version: &str,
) -> Result<()> {
    let expected = expected_storage_version
        .parse::<i64>()
        .context("logical archive storage schema version is invalid")?;
    let version = connection
        .query_row("SELECT MAX(version) FROM _schema_migrations")
        .map_err(|error| {
            super::database_failure("cannot read the canonical schema-migration authority", error)
        })?
        .get_typed::<Option<i64>>(0)
        .map_err(|error| {
            super::database_failure(
                "cannot decode the canonical schema-migration authority",
                error,
            )
        })?;
    ensure!(
        version == Some(expected),
        "canonical schema-migration authority disagrees with the archive storage schema"
    );
    // Fresh databases start at the combined v13 migration. Legacy v1..v12
    // entries are optional; names and timestamps are historical data, not a
    // replacement for the initializer's actual set of executable steps.
    // Indexed point probes keep this check bounded by the binary's step count,
    // independently of the archive's size or the numeric header version.
    for required in active_schema_migration_versions().filter(|&version| version <= expected) {
        let rows = connection
            .query_with_params(
                "SELECT version FROM _schema_migrations WHERE version = ?1 LIMIT 1",
                &[SqliteValue::Integer(required)],
            )
            .map_err(|error| {
                super::database_failure(
                    format!("cannot read canonical schema-migration authority for v{required}"),
                    error,
                )
            })?;
        let recorded = rows
            .first()
            .map(|row| row.get_typed::<i64>(0))
            .transpose()
            .map_err(|error| {
                super::database_failure(
                    format!("cannot decode canonical schema-migration authority for v{required}"),
                    error,
                )
            })?;
        ensure!(
            recorded == Some(required),
            "canonical schema-migration authority is missing required migration v{required}; normal storage opening would replay it"
        );
    }
    ensure!(
        export::schema_version(connection)? == expected_storage_version,
        "canonical schema marker disagrees with the archive storage schema"
    );
    Ok(())
}

pub(super) fn verify_database(connection: &Connection) -> Result<()> {
    let mut violated = false;
    let check = connection.query_with_params_for_each("PRAGMA foreign_key_check", &[], |_| {
        violated = true;
        Err(FrankenError::Internal(
            "restore foreign-key check failed".to_owned(),
        ))
    });
    ensure!(
        !violated,
        "restored archive has broken canonical relationships"
    );
    check.map_err(|error| {
        super::database_failure("cannot check restored archive relationships", error)
    })?;

    let mut rows = 0usize;
    let mut invalid = false;
    let check = connection.query_with_params_for_each("PRAGMA integrity_check", &[], |row| {
        rows = rows.saturating_add(1);
        if row.get_typed::<String>(0)? != "ok" {
            invalid = true;
            return Err(FrankenError::Internal(
                "restore integrity check failed".to_owned(),
            ));
        }
        Ok(())
    });
    ensure!(!invalid, "restored database failed integrity verification");
    // A failed read supplies no integrity verdict. Retain the engine cause so
    // I/O and busy failures keep their retryable classes without exposing the
    // engine's arbitrary diagnostic payload or a corrupt row's contents.
    check.map_err(|error| {
        super::database_failure("cannot read restored database integrity results", error)
    })?;
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
    ensure!(
        export::schema_version(connection)? == header.storage_schema_version,
        "logical archive storage schema differs from this binary; cross-schema migration is not supported"
    );
    let mut validator = Validator::new(header).map_err(super::integrity_unless_io)?;
    connection
        .execute("PRAGMA foreign_keys = OFF")
        .map_err(|error| {
            super::database_failure("cannot suspend restore foreign-key enforcement", error)
        })?;
    ensure!(
        connection
            .query_row("PRAGMA foreign_keys")
            .map_err(|error| {
                super::database_failure("cannot read restore foreign-key enforcement", error)
            })?
            .get_typed::<i64>(0)
            .map_err(|error| {
                super::database_failure("cannot decode restore foreign-key enforcement", error)
            })?
            == 0,
        "cannot suspend foreign-key enforcement for ordered restoration"
    );
    connection.execute("BEGIN IMMEDIATE").map_err(|error| {
        super::database_failure("cannot begin the private restore transaction", error)
    })?;
    let triggers = suspend_triggers(connection)?;
    for table in &expected {
        // Remove initializer seeds only in this unpublished, freshly made DB.
        connection
            .execute(&format!("DELETE FROM {}", export::quoted(&table.name)?))
            .map_err(|error| {
                super::database_failure(
                    format!(
                        "cannot clear initializer rows in logical table {}",
                        table.name
                    ),
                    error,
                )
            })?;
    }
    let mut table_count = 0usize;
    let mut statement = None;
    let mut table_row = 0u64;
    let mut batch_records = 0usize;
    let mut batch_bytes = 0usize;
    let mut line = 2u64;
    while let Some(record) = input.record(line)? {
        if batch_records == MAX_BATCH_RECORDS
            || input.record_bytes > MAX_BATCH_BYTES.saturating_sub(batch_bytes)
        {
            connection.execute("COMMIT").map_err(|error| {
                super::database_failure(
                    format!("cannot commit the private restore batch before record {line}"),
                    error,
                )
            })?;
            connection.execute("BEGIN IMMEDIATE").map_err(|error| {
                super::database_failure(
                    format!("cannot begin the private restore batch for record {line}"),
                    error,
                )
            })?;
            batch_records = 0;
            batch_bytes = 0;
        }
        // Archive-side checks are integrity verdicts; SQLite failures below are not.
        validator
            .validate(&record)
            .map_err(|error| super::integrity(format!("record {line}: {error}")))?;
        match record {
            Record::Table { table } => {
                ensure!(
                    expected.get(table_count) == Some(&table),
                    "record {line}: logical table does not match this binary's canonical schema"
                );
                // One prepared INSERT per descriptor, not one SQL parse/compile
                // per archive row. It remains idle across private batch commits.
                let insert = connection.prepare(&insert_sql(&table)?).map_err(|error| {
                    super::database_failure(
                        format!(
                            "record {line}: cannot prepare logical table {} insertion",
                            table.name
                        ),
                        error,
                    )
                })?;
                statement = Some((insert, table));
                table_count += 1;
                table_row = 0;
            }
            Record::Row { values: cells } => {
                let (insert, table) = statement.as_ref().ok_or_else(|| {
                    super::integrity(format!("record {line}: row precedes its table"))
                })?;
                table_row = table_row
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("logical table row position overflow"))?;
                let row = values(cells).map_err(super::integrity_unless_io)?;
                insert.execute_with_params(&row).map_err(|error| {
                    super::database_failure(
                        format!(
                            "record {line}: {}, canonical row insertion failed; no destination was published",
                            export::row_location(table, table_row, &row)
                        ),
                        error,
                    )
                })?;
            }
            Record::Completion { .. } => {}
            Record::Header { .. } => {
                return Err(super::integrity(format!(
                    "record {line}: duplicate archive header"
                )));
            }
        }
        batch_records += 1;
        batch_bytes += input.record_bytes;
        line = line
            .checked_add(1)
            .ok_or_else(|| anyhow!("logical record position overflow"))?;
    }
    let result = validator.finish().map_err(super::integrity_unless_io)?;
    ensure!(
        table_count == expected.len(),
        "logical archive omits canonical tables required by this binary"
    );
    ensure!(
        export::schema_version(connection)? == result.0.storage_schema_version,
        "archive header and canonical schema metadata disagree"
    );
    // Release the final prepared program before restoring schema objects.
    drop(statement);
    for statement in triggers {
        connection.execute_batch(&statement).map_err(|error| {
            super::database_failure("cannot reinstate canonical restore triggers", error)
        })?;
    }
    verify_database(connection)?;
    connection.execute("COMMIT").map_err(|error| {
        super::database_failure("cannot commit the verified private restore database", error)
    })?;
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

/// Copy the private, committed replay state through the engine's verified page
/// backup. VACUUM INTO hydrates every table in the pinned engine; this path
/// checkpoints the sole replay writer and copies with a fixed-size buffer.
/// The replay's WAL/journal names must never be moved into the publication.
fn materialize_candidate(connection: &Connection, candidate: &Path) -> Result<()> {
    require_new_destination(candidate)?;
    connection
        .backup_exact_to(candidate)
        .map_err(|error| {
            super::database_failure(
                "cannot materialize a self-contained restore publication image",
                error,
            )
        })?;
    require_candidate_without_sidecars(candidate)
}

fn require_candidate_without_sidecars(candidate: &Path) -> Result<()> {
    for sidecar in sidecars(candidate) {
        require_absent(&sidecar)
            .context("restore publication image still depends on SQLite sidecars")?;
    }
    Ok(())
}

/// The boolean receipt is true only when this call publishes a NEW database.
/// With opt-in, an existing target can succeed solely as a read-only comparison.
pub fn import_file_with_policy(
    input: &Path,
    destination: &Path,
    expected_archive_id: &str,
    if_identical: bool,
) -> Result<(Header, Completion, bool)> {
    import_open_file(
        open_input(input)?,
        destination,
        expected_archive_id,
        if_identical,
        None,
    )
}

/// Continue a caller's completed inspection using the SAME admitted file.
/// Reopening its pathname could silently restore a different, valid archive.
/// A descriptor alone does not prevent in-place changes, so require the
/// inspected header and completion again before publishing or reporting a match.
pub(super) fn import_inspected_file(
    mut input: File,
    destination: &Path,
    header: &Header,
    completion: &Completion,
    if_identical: bool,
) -> Result<(Header, Completion, bool)> {
    input
        .rewind()
        .map_err(|error| export::io_failure("cannot rewind inspected logical archive", error))?;
    import_open_file(
        input,
        destination,
        &header.archive_id,
        if_identical,
        Some((header, completion)),
    )
}

fn require_inspected_receipt(
    actual: &(Header, Completion),
    inspected: Option<(&Header, &Completion)>,
) -> Result<()> {
    if let Some((header, completion)) = inspected
        && (&actual.0 != header || &actual.1 != completion)
    {
        return Err(super::integrity(
            "logical archive changed after inspection; no destination was published or replaced",
        ));
    }
    Ok(())
}

fn import_open_file(
    input: File,
    destination: &Path,
    expected_archive_id: &str,
    if_identical: bool,
    inspected: Option<(&Header, &Completion)>,
) -> Result<(Header, Completion, bool)> {
    let mut input = Input::new(BufReader::new(input));
    let Some(Record::Header { header }) = input.record(1)? else {
        return Err(super::integrity("logical archive must begin with a header"));
    };
    header.validate().map_err(super::integrity_unless_io)?;
    ensure!(
        header.archive_id == expected_archive_id,
        "logical archive identity does not match --archive-id"
    );
    if let Some((expected, _)) = inspected
        && &header != expected
    {
        return Err(super::integrity(
            "logical archive header changed after inspection; no destination was published or replaced",
        ));
    }
    let _lock = DestinationLock::acquire(destination)?;
    if if_identical && fs::symlink_metadata(destination).is_ok() {
        let result = super::reimport::verify_existing(&mut input, header, destination)?;
        require_inspected_receipt(&result, inspected)?;
        return Ok((result.0, result.1, false));
    }
    require_new_destination(destination)?;

    let staging = tempfile::Builder::new()
        .prefix(".cass-restore-")
        .tempdir_in(export::parent(destination)?)?;
    let replay_path = staging.path().join("agent_search.db");
    let candidate = staging.path().join("publication.db");
    let storage = SqliteStorage::open(&replay_path)
        .context("cannot initialize canonical restore candidate")?;
    drop(storage);
    let connection = Connection::open(export::path_text(&replay_path)?).map_err(|error| {
        super::database_failure("cannot open the private restore database", error)
    })?;
    connection
        .execute("PRAGMA busy_timeout = 5000")
        .map_err(|error| {
            super::database_failure("cannot set the private restore busy timeout", error)
        })?;
    // Replay uses the canonical WAL writer contract. A separate engine snapshot
    // below, not changing journal mode, establishes a single-file publication.
    let mode = connection
        .query_row("PRAGMA journal_mode = WAL")
        .map_err(|error| {
            super::database_failure("cannot enable WAL for private canonical replay", error)
        })?
        .get_typed::<String>(0)
        .map_err(|error| {
            super::database_failure("cannot decode the private replay journal mode", error)
        })?;
    ensure!(
        mode.eq_ignore_ascii_case("wal"),
        "cannot enable WAL for private canonical replay"
    );
    connection
        .execute("PRAGMA synchronous = FULL")
        .map_err(|error| {
            super::database_failure("cannot require durable private restore writes", error)
        })?;
    let result = restore(&connection, &mut input, header)?;
    // Bind the replay to its inspection before materialization or publication.
    require_inspected_receipt(&result, inspected)?;
    verify_schema_authority(&connection, &result.0.storage_schema_version)?;
    // restore() has verified the complete input and committed every private
    // batch. The engine backup checkpoints this private writer before copying
    // its verified image; it must not run on the user's live source database.
    materialize_candidate(&connection, &candidate)?;
    connection.close().map_err(|error| {
        super::database_failure(
            "cannot close the materialized private restore database",
            error,
        )
    })?;

    // Reopen the persisted database and hash its actual typed rows, descriptors,
    // metadata and relationships. Input verification alone cannot detect an
    // affinity conversion, initializer side effect, or storage write defect.
    let reader = export::open_source(&candidate)?;
    verify_schema_authority(&reader, &result.0.storage_schema_version)?;
    verify_database(&reader)?;
    let actual = export::snapshot(&reader, result.0.archive_id.clone(), &mut io::sink())?;
    reader.execute("ROLLBACK").map_err(|error| {
        super::database_failure(
            "cannot release the restored database verification snapshot",
            error,
        )
    })?;
    reader.close_without_checkpoint().map_err(|error| {
        super::database_failure(
            "cannot close the restored database verification reader",
            error,
        )
    })?;
    ensure!(
        actual.1 == result.1,
        "restored database does not reproduce the archive's canonical digest"
    );
    require_candidate_without_sidecars(&candidate)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&candidate, fs::Permissions::from_mode(0o600))?;
    }
    sync_candidate(&candidate)?;
    #[cfg(all(test, unix))]
    publication_tests::pause_verified_crash_child(destination, "before-publish")?;
    require_new_destination(destination)?;
    // Same-filesystem hard-link publication is atomic and never replaces an
    // existing name (including symlinks). No rename/copy-over fallback is safe.
    fs::hard_link(&candidate, destination)
        .context("cannot publish restored archive without replacing existing data; destination must support hard links")?;
    #[cfg(all(test, unix))]
    publication_tests::pause_verified_crash_child(destination, "after-publish")?;
    export::sync_parent(destination)?;
    Ok((result.0, result.1, true))
}

#[cfg(test)]
#[path = "import_tests.rs"]
mod tests;

#[cfg(test)]
mod publication_tests {
    use super::*;

    // Test-binary-only barrier: the parent kills a real import after all
    // persisted-image validation and fsync, while its destination lock is held.
    // Release binaries contain neither this hook nor its environment control.
    #[cfg(unix)]
    pub(super) fn pause_verified_crash_child(destination: &Path, phase: &str) -> Result<()> {
        if let Ok(root) = dotenvy::var("CASS_TEST_LOGICAL_ARCHIVE_CRASH_ROOT") {
            let root = PathBuf::from(root);
            let requested = dotenvy::var("CASS_TEST_LOGICAL_ARCHIVE_CRASH_PHASE")
                .unwrap_or_else(|_| "before-publish".to_owned());
            if destination == root.join("restored.db") && requested == phase {
                fs::write(root.join("verified-ready"), phase.as_bytes())?;
                loop {
                    std::thread::park();
                }
            }
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "subprocess entry point driven by killed_verified_import_publishes_nothing_and_retries"]
    fn crash_import_subprocess() {
        let root = PathBuf::from(
            dotenvy::var("CASS_TEST_LOGICAL_ARCHIVE_CRASH_ROOT").expect("parent supplies fixture"),
        );
        import_file(
            &root.join("history.jsonl"),
            &root.join("restored.db"),
            "crash-archive",
        )
        .unwrap();
        panic!("verified-image crash barrier was not armed");
    }

    #[cfg(unix)]
    #[test]
    fn killed_verified_import_publishes_nothing_and_retries() {
        assert_interrupted_import_recovers(false);
    }

    #[cfg(unix)]
    #[test]
    fn published_import_without_a_receipt_can_be_verified_and_retried() {
        assert_interrupted_import_recovers(true);
    }

    #[cfg(unix)]
    fn assert_interrupted_import_recovers(published: bool) {
        use std::process::{Child, Command, Stdio};
        use std::time::{Duration, Instant};

        struct KillOnDrop(Child);
        impl Drop for KillOnDrop {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.db");
        drop(SqliteStorage::open(&source).unwrap());
        let input = root.path().join("history.jsonl");
        let expected = export::export_file(&source, &input, "crash-archive".to_owned()).unwrap();
        let input_before = fs::read(&input).unwrap();
        let source_before = fs::read(&source).unwrap();
        let destination = root.path().join("restored.db");
        let phase = if published {
            "after-publish"
        } else {
            "before-publish"
        };
        let mut child = KillOnDrop(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    "logical_archive::import::publication_tests::crash_import_subprocess",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env("CASS_TEST_LOGICAL_ARCHIVE_CRASH_ROOT", root.path())
                .env("CASS_TEST_LOGICAL_ARCHIVE_CRASH_PHASE", phase)
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if fs::read(root.path().join("verified-ready"))
                .is_ok_and(|bytes| bytes == phase.as_bytes())
            {
                break;
            }
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "restore child exited before verifying its image"
            );
            assert!(
                Instant::now() < deadline,
                "restore child missed its deadline"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(destination.exists(), published);
        child.0.kill().unwrap();
        assert!(!child.0.wait().unwrap().success());

        // SIGKILL skips both the destination-lock and TempDir destructors.
        // Before the atomic link, no restore exists. After it, the public name
        // identifies the entire verified image even without a success receipt.
        assert_eq!(destination.exists(), published);
        let retained = fs::read_dir(root.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".cass-restore-")
            })
            .unwrap()
            .join("publication.db");
        assert!(retained.is_file());
        require_candidate_without_sidecars(&retained).unwrap();
        let reader = export::open_source(&retained).unwrap();
        assert_eq!(
            export::snapshot(&reader, "crash-archive".to_owned(), &mut io::sink())
                .unwrap()
                .1,
            expected.1
        );
        reader.execute("ROLLBACK").unwrap();
        reader.close_without_checkpoint().unwrap();
        // The OS released the crashed process's lock. A retry either creates
        // a fresh image or proves the already published image without writes.
        if published {
            let before = fs::read(&destination).unwrap();
            assert!(import_file(&input, &destination, "crash-archive").is_err());
            let (header, completion, created) =
                import_file_with_policy(&input, &destination, "crash-archive", true).unwrap();
            assert!(!created, "lost success receipt must not cause replacement");
            assert_eq!((header, completion), expected);
            assert_eq!(fs::read(&destination).unwrap(), before);
        } else {
            let retried = import_file(&input, &destination, "crash-archive").unwrap();
            assert_eq!(retried, expected);
        }
        assert!(destination.is_file());
        assert!(retained.is_file());
        assert_eq!(fs::read(&input).unwrap(), input_before);
        assert_eq!(fs::read(&source).unwrap(), source_before);
    }

    #[test]
    fn publication_materializes_wal_rows_without_borrowing_replay_sidecars() {
        let root = tempfile::tempdir().unwrap();
        let replay_path = root.path().join("replay.db");
        let writer = Connection::open(export::path_text(&replay_path).unwrap()).unwrap();
        writer.execute("PRAGMA journal_mode = WAL").unwrap();
        writer.execute("PRAGMA wal_autocheckpoint = 0").unwrap();
        writer
            .execute_batch(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO meta VALUES ('schema_version', '9');
             CREATE TABLE messages (id INTEGER PRIMARY KEY, body TEXT NOT NULL);",
            )
            .unwrap();
        let body = "committed WAL-only transcript δ ".repeat(1024);
        writer
            .execute_with_params(
                "INSERT INTO messages VALUES (7, ?1)",
                &[SqliteValue::Text(body.clone().into())],
            )
            .unwrap();
        let expected = export::snapshot(&writer, "wal-publication".to_owned(), &mut io::sink())
            .unwrap()
            .1;
        // Negative control: copying only the replay's main file cannot satisfy
        // this fixture. The new production path must include committed WAL data.
        let incomplete = root.path().join("main-only.db");
        fs::copy(&replay_path, &incomplete).unwrap();
        let copied = export::open_source(&incomplete).and_then(|reader| {
            export::snapshot(&reader, "wal-publication".to_owned(), &mut io::sink())
        });
        if let Ok((_, completion)) = copied {
            assert_ne!(
                completion, expected,
                "fixture must require its committed WAL"
            );
        }

        // The path API must handle both quotes and Unicode without SQL quoting.
        let candidate = root.path().join("publication 'δ'.db");
        materialize_candidate(&writer, &candidate).unwrap();
        let reader = export::open_source(&candidate).unwrap();
        verify_database(&reader).unwrap();
        assert_eq!(
            export::snapshot(&reader, "wal-publication".to_owned(), &mut io::sink())
                .unwrap()
                .1,
            expected
        );
        assert_eq!(
            reader
                .query_row("SELECT body FROM messages WHERE id = 7")
                .unwrap()
                .get_typed::<String>(0)
                .unwrap(),
            body
        );
        reader.execute("ROLLBACK").unwrap();
        reader.close_without_checkpoint().unwrap();
        require_candidate_without_sidecars(&candidate).unwrap();
        // The writer stays usable; snapshot materialization is not relocation.
        assert_eq!(
            writer
                .query_row("SELECT COUNT(*) FROM messages")
                .unwrap()
                .get_typed::<i64>(0)
                .unwrap(),
            1
        );
        writer.close().unwrap();
    }

    #[test]
    fn publication_does_not_clobber_an_image_or_its_orphan_sidecars() {
        let root = tempfile::tempdir().unwrap();
        let writer = Connection::open(":memory:").unwrap();
        writer
            .execute("CREATE TABLE messages (id INTEGER PRIMARY KEY)")
            .unwrap();
        for (ordinal, suffix) in ["", "-wal", "-shm", "-journal"].iter().enumerate() {
            let candidate = root.path().join(format!("image-{ordinal}.db"));
            let mut occupied = candidate.as_os_str().to_os_string();
            occupied.push(suffix);
            let occupied = PathBuf::from(occupied);
            fs::write(&occupied, b"prior authority").unwrap();
            assert!(materialize_candidate(&writer, &candidate).is_err());
            assert_eq!(fs::read(&occupied).unwrap(), b"prior authority");
            if !suffix.is_empty() {
                assert!(!candidate.exists());
            }
        }
    }

    fn schema_authority_database_files(
        path: &Path,
    ) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
        std::iter::once(path.to_path_buf())
            .chain(sidecars(path))
            .filter_map(|path| match fs::read(&path) {
                Ok(bytes) => Some((path, bytes)),
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => panic!("cannot inspect test database: {error}"),
            })
            .collect()
    }

    #[test]
    fn schema_authority_checks_each_active_step_in_fresh_and_legacy_histories() -> Result<()> {
        for expected in [20_i64, 21, 22] {
            for legacy in [false, true] {
                let connection = Connection::open(":memory:")?;
                connection.execute_batch(
                    "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                     CREATE TABLE _schema_migrations (version INTEGER PRIMARY KEY, name TEXT NOT NULL);",
                )?;
                connection.execute_with_params(
                    "INSERT INTO meta VALUES ('schema_version', ?1)",
                    &[SqliteValue::Text(expected.to_string().into())],
                )?;
                let first = if legacy { 1 } else { 13 };
                for version in first..=expected {
                    connection.execute_with_params(
                        "INSERT INTO _schema_migrations VALUES (?1, ?2)",
                        &[
                            SqliteValue::Integer(version),
                            SqliteValue::Text("PRIVATE-HISTORICAL-MIGRATION-NAME".into()),
                        ],
                    )?;
                }
                verify_schema_authority(&connection, &expected.to_string())?;
                for missing in 13..expected {
                    connection.execute_with_params(
                        "DELETE FROM _schema_migrations WHERE version = ?1",
                        &[SqliteValue::Integer(missing)],
                    )?;
                    assert_eq!(
                        connection
                            .query_row("SELECT MAX(version) FROM _schema_migrations")?
                            .get_typed::<i64>(0)?,
                        expected,
                        "the declared maximum alone cannot detect this hole"
                    );
                    let error = verify_schema_authority(&connection, &expected.to_string())
                        .expect_err("every executable historical migration must be recorded");
                    assert!(
                        error
                            .to_string()
                            .contains(&format!("missing required migration v{missing}"))
                    );
                    assert!(!error.to_string().contains("PRIVATE-HISTORICAL-MIGRATION-NAME"));
                    connection.execute_with_params(
                        "INSERT INTO _schema_migrations VALUES (?1, 'historical name')",
                        &[SqliteValue::Integer(missing)],
                    )?;
                }
                verify_schema_authority(&connection, &expected.to_string())?;
                connection.close()?;
            }
        }
        Ok(())
    }

    #[test]
    fn sparse_history_refuses_restore_before_normal_open_can_replay_v18() -> Result<()> {
        use coding_agent_search::storage::sqlite::CURRENT_SCHEMA_VERSION;

        let root = tempfile::tempdir()?;
        let source = root.path().join("source.db");
        drop(SqliteStorage::open(&source)?);
        let writer = Connection::open(export::path_text(&source)?)?;
        writer.execute_batch(
            "INSERT INTO agents (id, slug, name, kind, created_at, updated_at)
             VALUES (1, 'tail-fixture', 'Tail Fixture', 'cli', 0, 0);
             INSERT INTO conversations
                 (id, agent_id, source_path, ended_at, last_message_idx, last_message_created_at)
             VALUES (1, 1, 'PRIVATE-SCHEMA-AUTHORITY-SOURCE', 100, 1, 100);
             INSERT INTO conversation_tail_state
                 (conversation_id, ended_at, last_message_idx, last_message_created_at)
             VALUES (1, 999, 99, 999);
             DELETE FROM _schema_migrations WHERE version = 18;",
        )?;
        for index in 0_i64..100 {
            writer.execute_with_params(
                "INSERT INTO messages (id, conversation_id, idx, role, created_at, content)
                 VALUES (?1, 1, ?2, 'user', ?3, 'PRIVATE-SCHEMA-AUTHORITY-MESSAGE')",
                &[
                    SqliteValue::Integer(index + 1),
                    SqliteValue::Integer(index),
                    SqliteValue::Integer(index * 10 + 9),
                ],
            )?;
        }
        assert_eq!(
            writer
                .query_row("SELECT MAX(version) FROM _schema_migrations")?
                .get_typed::<i64>(0)?,
            CURRENT_SCHEMA_VERSION
        );
        verify_database(&writer)?;
        writer.close()?;

        let input = root.path().join("history.jsonl");
        let expected = export::export_file(&source, &input, "sparse-history".to_owned())?;
        assert_eq!(expected, export::verify_file(&input)?);
        let source_before = schema_authority_database_files(&source);
        let input_before = fs::read(&input)?;
        let destination = root.path().join("restored.db");
        let error = import_file(&input, &destination, "sparse-history")
            .expect_err("a digest-valid sparse history must not be published");
        assert!(error.to_string().contains("missing required migration v18"));
        assert!(!error.to_string().contains("PRIVATE-SCHEMA-AUTHORITY"));
        require_new_destination(&destination)?;
        let error = import_file_with_policy(&input, &source, "sparse-history", true)
            .expect_err("identical rows cannot authorize a migration replay on ordinary open");
        assert!(error.to_string().contains("missing required migration v18"));
        assert!(!error.to_string().contains("PRIVATE-SCHEMA-AUTHORITY"));
        assert_eq!(source_before, schema_authority_database_files(&source));
        assert_eq!(input_before, fs::read(&input)?);

        let reader = export::open_source(&source)?;
        assert_eq!(
            reader
                .query_row("SELECT last_message_idx FROM conversation_tail_state WHERE conversation_id = 1")?
                .get_typed::<i64>(0)?,
            99,
            "both refused import paths must preserve the newer canonical tail"
        );
        reader.execute("ROLLBACK")?;
        reader.close_without_checkpoint()?;

        // Positive control on this disposable source fixture, after proving
        // both archive paths leave it untouched. The actual storage opener
        // replays a missing v18 even though MAX(version) remains current, and
        // its INSERT OR REPLACE overwrites the newer hot tail with legacy data.
        let reopened = SqliteStorage::open(&source)?;
        assert_eq!(
            reopened
                .raw()
                .query_row("SELECT last_message_idx FROM conversation_tail_state WHERE conversation_id = 1")?
                .get_typed::<i64>(0)?,
            1
        );
        assert_eq!(reopened.schema_version()?, CURRENT_SCHEMA_VERSION);
        Ok(())
    }

    #[test]
    fn contradictory_schema_authority_never_publishes_or_confirms_an_existing_database()
    -> Result<()> {
        use coding_agent_search::storage::sqlite::CURRENT_SCHEMA_VERSION;

        for case in ["empty", "stale", "future"] {
            let root = tempfile::tempdir()?;
            let source = root.path().join("source.db");
            drop(SqliteStorage::open(&source)?);
            let writer = Connection::open(export::path_text(&source)?)?;
            match case {
                "empty" => {
                    writer.execute("DELETE FROM _schema_migrations")?;
                }
                "stale" => {
                    writer.execute_with_params(
                        "DELETE FROM _schema_migrations WHERE version = ?1",
                        &[SqliteValue::Integer(CURRENT_SCHEMA_VERSION)],
                    )?;
                }
                "future" => {
                    writer.execute_with_params(
                        "INSERT INTO _schema_migrations (version, name) VALUES (?1, ?2)",
                        &[
                            SqliteValue::Integer(CURRENT_SCHEMA_VERSION + 1),
                            SqliteValue::Text("PRIVATE-FUTURE-SCHEMA-AUTHORITY".into()),
                        ],
                    )?;
                }
                _ => unreachable!(),
            }
            assert_eq!(
                export::schema_version(&writer)?,
                CURRENT_SCHEMA_VERSION.to_string()
            );
            writer.close()?;

            let input = root.path().join("history.jsonl");
            let expected =
                export::export_file(&source, &input, "schema-authority".to_owned())?;
            assert_eq!(expected, export::verify_file(&input)?);
            let source_before = schema_authority_database_files(&source);
            let input_before = fs::read(&input)?;
            let destination = root.path().join("restored.db");
            let error = import_file(&input, &destination, "schema-authority")
                .expect_err("matching rows cannot authorize a contradictory migration ledger");
            assert!(error.to_string().contains("schema-migration authority"));
            assert!(!error.to_string().contains("PRIVATE-FUTURE-SCHEMA-AUTHORITY"));
            require_new_destination(&destination)?;

            let error = import_file_with_policy(&input, &source, "schema-authority", true)
                .expect_err("an identical retry must validate the existing migration authority");
            assert!(error.to_string().contains("schema-migration authority"));
            assert!(!error.to_string().contains("PRIVATE-FUTURE-SCHEMA-AUTHORITY"));
            assert_eq!(source_before, schema_authority_database_files(&source));
            assert_eq!(input_before, fs::read(&input)?);
        }
        Ok(())
    }

    #[test]
    fn exact_restore_schema_authority_survives_normal_reopen_and_read_only_retry() -> Result<()> {
        use coding_agent_search::storage::sqlite::CURRENT_SCHEMA_VERSION;

        for legacy in [false, true] {
            let root = tempfile::tempdir()?;
            let source = root.path().join("source.db");
            drop(SqliteStorage::open(&source)?);
            if legacy {
                let writer = Connection::open(export::path_text(&source)?)?;
                for version in 1_i64..13 {
                    writer.execute_with_params(
                        "INSERT INTO _schema_migrations (version, name) VALUES (?1, ?2)",
                        &[
                            SqliteValue::Integer(version),
                            SqliteValue::Text("historical migration".into()),
                        ],
                    )?;
                }
                writer.close()?;
            }
            let input = root.path().join("history.jsonl");
            let expected =
                export::export_file(&source, &input, "schema-authority".to_owned())?;
            let destination = root.path().join("restored.db");
            assert_eq!(
                import_file(&input, &destination, "schema-authority")?,
                expected
            );
            let reopened = SqliteStorage::open(&destination)?;
            assert_eq!(reopened.schema_version()?, CURRENT_SCHEMA_VERSION);
            drop(reopened);
            let before = schema_authority_database_files(&destination);
            let (header, completion, created) =
                import_file_with_policy(&input, &destination, "schema-authority", true)?;
            assert!(!created);
            assert_eq!((header, completion), expected);
            assert_eq!(before, schema_authority_database_files(&destination));
        }
        Ok(())
    }

    #[test]
    fn unreadable_schema_authority_keeps_typed_private_engine_failures() -> Result<()> {
        use fsqlite_types::cx::CancelReason;

        let connection = Connection::open(":memory:")?;
        connection.execute_batch(
            "CREATE TABLE _schema_migrations (version TEXT PRIMARY KEY);
             INSERT INTO _schema_migrations VALUES ('PRIVATE-UNREADABLE-SCHEMA-AUTHORITY');",
        )?;
        let error = verify_schema_authority(&connection, "22")
            .expect_err("an undecodable ledger is not a supported schema authority");
        assert!(
            error
                .to_string()
                .contains("cannot decode the canonical schema-migration authority")
        );
        assert!(!error.to_string().contains("PRIVATE-UNREADABLE-SCHEMA-AUTHORITY"));
        assert!(
            error
                .chain()
                .any(|cause| cause.downcast_ref::<FrankenError>().is_some())
        );

        let (operation, relay) = connection
            .as_async()
            .root_cx()
            .create_child_with_local_cancel_relay();
        assert!(relay.cancel_local(CancelReason::UserInterrupt));
        let error = {
            let _binding = connection.as_async().bind_operation_cx(&operation);
            verify_schema_authority(&connection, "22")
                .expect_err("a cancelled authority probe cannot certify the database")
        };
        assert!(
            error
                .to_string()
                .contains("cannot read the canonical schema-migration authority")
        );
        assert!(!error.to_string().contains("PRIVATE-UNREADABLE-SCHEMA-AUTHORITY"));
        assert!(error.chain().any(|cause| {
            matches!(
                cause.downcast_ref::<FrankenError>(),
                Some(FrankenError::Abort)
            )
        }));
        assert!(connection.as_async().root_cx().checkpoint().is_ok());
        Ok(())
    }
}

#[cfg(test)]
#[path = "pinned_import_tests.rs"]
mod pinned_tests;
