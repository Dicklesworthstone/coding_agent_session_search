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
    let rows = connection.query(
        "SELECT name, substr(sql, 1, 65537) FROM sqlite_master WHERE type = 'trigger' ORDER BY name LIMIT 257",
    )?;
    ensure!(
        rows.len() <= 256,
        "canonical schema exceeds restore trigger limit"
    );
    let mut statements = Vec::new();
    let mut bytes = 0usize;
    for row in rows {
        let name = row.get_typed::<String>(0)?;
        let sql = row.get_typed::<String>(1)?;
        bytes = bytes.saturating_add(sql.len());
        ensure!(
            sql.len() <= 65536 && bytes <= MAX_TRIGGER_BYTES,
            "canonical trigger definitions exceed restore budget"
        );
        connection.execute(&format!("DROP TRIGGER {}", export::quoted(&name)?))?;
        statements.push(sql);
    }
    Ok(statements)
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
    check.context("cannot check restored archive relationships")?;

    let mut rows = 0usize;
    let check = connection.query_with_params_for_each("PRAGMA integrity_check", &[], |row| {
        rows = rows.saturating_add(1);
        if row.get_typed::<String>(0)? != "ok" {
            return Err(FrankenError::Internal(
                "restore integrity check failed".to_owned(),
            ));
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
    ensure!(
        export::schema_version(connection)? == header.storage_schema_version,
        "logical archive storage schema differs from this binary; cross-schema migration is not supported"
    );
    let mut validator = Validator::new(header).map_err(super::integrity_unless_io)?;
    connection.execute("PRAGMA foreign_keys = OFF")?;
    ensure!(
        connection
            .query_row("PRAGMA foreign_keys")?
            .get_typed::<i64>(0)?
            == 0,
        "cannot suspend foreign-key enforcement for ordered restoration"
    );
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
        // Archive-side checks are integrity verdicts; SQLite failures below are not.
        validator
            .push(&record)
            .map_err(|error| super::integrity(format!("record {line}: {error}")))?;
        match record {
            Record::Table { table } => {
                ensure!(
                    expected.get(table_count) == Some(&table),
                    "record {line}: logical table does not match this binary's canonical schema"
                );
                // One prepared INSERT per descriptor, not one SQL parse/compile
                // per archive row. It remains idle across private batch commits.
                statement = Some(connection.prepare(&insert_sql(&table)?)?);
                table_count += 1;
            }
            Record::Row { values: cells } => {
                let insert = statement.as_ref().ok_or_else(|| {
                    super::integrity(format!("record {line}: row precedes its table"))
                })?;
                let row = values(cells).map_err(super::integrity_unless_io)?;
                insert.execute_with_params(&row)
                    .map_err(|_| anyhow!("record {line}: canonical row insertion failed; no destination was published"))?;
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

/// Materialize committed replay state through the engine, not a main-file copy
/// or a journal-mode switch. The replay DB may legitimately retain WAL/journal
/// files after close; none of those names may be moved into the publication.
fn materialize_candidate(connection: &Connection, candidate: &Path) -> Result<()> {
    require_new_destination(candidate)?;
    connection
        .execute_with_params(
            "VACUUM INTO ?1",
            &[SqliteValue::Text(export::path_text(candidate)?.into())],
        )
        .context("cannot materialize a self-contained restore publication image")?;
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
    let mut input = Input::new(BufReader::new(open_input(input)?));
    let Some(Record::Header { header }) = input.record(1)? else {
        return Err(super::integrity("logical archive must begin with a header"));
    };
    header.validate().map_err(super::integrity_unless_io)?;
    ensure!(
        header.archive_id == expected_archive_id,
        "logical archive identity does not match --archive-id"
    );
    let _lock = DestinationLock::acquire(destination)?;
    if if_identical && fs::symlink_metadata(destination).is_ok() {
        let (header, completion) =
            super::reimport::verify_existing(&mut input, header, destination)?;
        return Ok((header, completion, false));
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
    let connection = Connection::open(export::path_text(&replay_path)?)?;
    connection.execute("PRAGMA busy_timeout = 5000")?;
    // Replay uses the canonical WAL writer contract. A separate engine snapshot
    // below, not changing journal mode, establishes a single-file publication.
    let mode = connection
        .query_row("PRAGMA journal_mode = WAL")?
        .get_typed::<String>(0)?;
    ensure!(
        mode.eq_ignore_ascii_case("wal"),
        "cannot enable WAL for private canonical replay"
    );
    connection.execute("PRAGMA synchronous = FULL")?;
    let result = restore(&connection, &mut input, header)?;
    // restore() has verified the complete input and committed every private
    // batch. VACUUM INTO includes committed WAL rows while leaving the replay
    // files in place. Never delete or ignore them to make publication pass.
    materialize_candidate(&connection, &candidate)?;
    connection.close()?;

    // Reopen the persisted database and hash its actual typed rows, descriptors,
    // metadata and relationships. Input verification alone cannot detect an
    // affinity conversion, initializer side effect, or storage write defect.
    let reader = export::open_source(&candidate)?;
    verify_database(&reader)?;
    let actual = export::snapshot(&reader, result.0.archive_id.clone(), &mut io::sink())?;
    reader.execute("ROLLBACK")?;
    reader.close_without_checkpoint()?;
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

        // Parameter binding must handle both quotes and Unicode in the path.
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
}
