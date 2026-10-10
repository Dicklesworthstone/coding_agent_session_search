use super::*;
use std::collections::BTreeMap;
use std::io::BufRead as _;
use std::path::PathBuf;

fn fixture(path: &Path) {
    let connection = Connection::open(path.to_str().unwrap()).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         INSERT INTO meta VALUES ('schema_version', '9');
         CREATE TABLE messages (id INTEGER PRIMARY KEY, body TEXT, usage REAL, raw BLOB);
         INSERT INTO messages VALUES (7, 'private transcript', 1.25, X'0001FF');
         INSERT INTO messages VALUES (3, 'earlier', NULL, NULL);",
        )
        .unwrap();
    connection.close().unwrap();
}

fn contents(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    let mut contents = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            let metadata = fs::symlink_metadata(&path).unwrap();
            assert!(!metadata.file_type().is_symlink());
            if metadata.is_dir() {
                contents.insert(path.clone(), None);
                pending.push(path);
            } else {
                assert!(metadata.is_file());
                contents.insert(path.clone(), Some(fs::read(path).unwrap()));
            }
        }
    }
    contents
}

#[test]
fn export_preserves_every_source_file_and_verifies_published_bytes() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let database = source.path().join("agent_search.db");
    fixture(&database);
    let before = contents(source.path());
    let output = destination.path().join("archive.jsonl");
    let receipt = export_file(&database, &output, "stable-archive".to_owned()).unwrap();
    assert_eq!(receipt, verify_file(&output).unwrap());
    assert_eq!(receipt.1.records, 3);
    assert_eq!(receipt.1.tables["messages"], 2);
    assert_eq!(before, contents(source.path()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(output).unwrap().permissions().mode() & 0o077,
            0
        );
    }
}

#[test]
fn existing_output_is_never_replaced() {
    let root = tempfile::tempdir().unwrap();
    let output = root.path().join("archive.jsonl");
    fs::write(&output, "previous output").unwrap();
    assert!(export_file(&root.path().join("missing.db"), &output, "test".to_owned()).is_err());
    assert_eq!(fs::read(&output).unwrap(), b"previous output");
}

#[test]
fn missing_source_is_not_created_and_failed_export_is_not_published() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("missing.db");
    let output = root.path().join("archive.jsonl");
    assert!(export_file(&source, &output, "test".to_owned()).is_err());
    assert!(!source.exists());
    assert!(!output.exists());
}

#[test]
fn unknown_unkeyed_tables_are_not_silently_omitted() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("agent_search.db");
    fixture(&source);
    let connection = Connection::open(source.to_str().unwrap()).unwrap();
    connection
        .execute("CREATE TABLE unknown_data (body TEXT)")
        .unwrap();
    connection.close().unwrap();
    let output = root.path().join("archive.jsonl");
    assert!(export_file(&source, &output, "test".to_owned()).is_err());
    assert!(!output.exists());
}

#[test]
fn fts5_shadow_names_are_exact_not_prefixes() {
    for suffix in ["config", "content", "data", "docsize", "idx"] {
        assert!(is_fts5_shadow_table(
            &format!("fts_messages_{suffix}"),
            "fts_messages"
        ));
    }
    for name in [
        "fts_messages",
        "fts_messages_",
        "fts_messages_notes",
        "fts_messages_data_backup",
        "fts_messages_datax",
        "other_fts_messages_data",
        "fts_messages2_data",
    ] {
        assert!(
            !is_fts5_shadow_table(name, "fts_messages"),
            "omitted {name}"
        );
    }
}

#[test]
fn similarly_named_logical_tables_survive_export_and_affect_the_digest() -> Result<()> {
    let source = tempfile::tempdir()?;
    let destination = tempfile::tempdir()?;
    let database = source.path().join("agent_search.db");
    fixture(&database);
    let connection = Connection::open(path_text(&database)?)?;
    connection.execute_batch(
        "CREATE VIRTUAL TABLE fts_messages USING fts5(body);
         INSERT INTO fts_messages(rowid, body) VALUES (7, 'derived transcript');
         CREATE TABLE fts_messages_notes (id INTEGER PRIMARY KEY, note TEXT NOT NULL);
         INSERT INTO fts_messages_notes VALUES (1, 'authoritative annotation');
         CREATE TABLE fts_messages_data_backup (id INTEGER PRIMARY KEY, raw BLOB);
         INSERT INTO fts_messages_data_backup VALUES (2, X'0001FF');",
    )?;
    connection.close()?;
    let before = contents(source.path());
    let output = destination.path().join("archive.jsonl");
    let first = export_file(&database, &output, "shadow-scope".to_owned())?;
    assert_eq!(first, verify_file(&output)?);
    assert_eq!(first.1.records, 5);
    assert_eq!(first.1.tables["fts_messages_notes"], 1);
    assert_eq!(first.1.tables["fts_messages_data_backup"], 1);
    for name in [
        "fts_messages",
        "fts_messages_config",
        "fts_messages_content",
        "fts_messages_data",
        "fts_messages_docsize",
        "fts_messages_idx",
    ] {
        assert!(
            !first.1.tables.contains_key(name),
            "exported derived table {name}"
        );
    }
    let mut reader = BufReader::new(File::open(&output)?);
    let mut saw_note = false;
    let mut saw_blob = false;
    let mut table_name = String::new();
    let mut line = 1;
    while let Some(record) = codec::read_record(&mut reader, line)? {
        match record {
            Record::Table { table } => table_name = table.name,
            Record::Row { values } if table_name == "fts_messages_notes" => {
                assert_eq!(
                    values,
                    vec![
                        Cell::Integer(1),
                        Cell::Text("authoritative annotation".to_owned())
                    ]
                );
                saw_note = true;
            }
            Record::Row { values } if table_name == "fts_messages_data_backup" => {
                assert_eq!(
                    values,
                    vec![Cell::Integer(2), Cell::Blob("AAH/".to_owned())]
                );
                saw_blob = true;
            }
            _ => {}
        }
        line += 1;
    }
    assert!(
        saw_note && saw_blob,
        "real typed rows, not just descriptors, must survive"
    );
    assert_eq!(before, contents(source.path()));

    let connection = Connection::open(path_text(&database)?)?;
    connection.execute("UPDATE fts_messages_notes SET note = 'changed annotation' WHERE id = 1")?;
    connection.close()?;
    let changed = destination.path().join("changed.jsonl");
    let second = export_file(&database, &changed, "shadow-scope".to_owned())?;
    assert_eq!(second, verify_file(&changed)?);
    assert_eq!(first.1.tables, second.1.tables);
    assert_ne!(first.1.content_sha256, second.1.content_sha256);
    Ok(())
}

#[test]
fn shadow_like_names_without_a_virtual_owner_are_exported() -> Result<()> {
    let source = tempfile::tempdir()?;
    let destination = tempfile::tempdir()?;
    let database = source.path().join("agent_search.db");
    fixture(&database);
    let connection = Connection::open(path_text(&database)?)?;
    connection.execute_batch(
        "CREATE TABLE fts_messages_data (id INTEGER PRIMARY KEY, body TEXT);
         INSERT INTO fts_messages_data VALUES (1, 'not owned by FTS5');",
    )?;
    connection.close()?;
    let output = destination.path().join("archive.jsonl");
    let receipt = export_file(&database, &output, "no-virtual-owner".to_owned())?;
    assert_eq!(receipt, verify_file(&output)?);
    assert_eq!(receipt.1.tables["fts_messages_data"], 1);
    assert_eq!(receipt.1.records, 4);
    Ok(())
}

#[test]
fn unkeyed_fts_prefix_table_is_refused_not_silently_omitted() -> Result<()> {
    let source = tempfile::tempdir()?;
    let destination = tempfile::tempdir()?;
    let database = source.path().join("agent_search.db");
    fixture(&database);
    let connection = Connection::open(path_text(&database)?)?;
    connection.execute_batch(
        "CREATE VIRTUAL TABLE fts_messages USING fts5(body);
         CREATE TABLE fts_messages_notes (body TEXT);
         INSERT INTO fts_messages_notes VALUES ('do not silently lose me');",
    )?;
    connection.close()?;
    let before = contents(source.path());
    let output = destination.path().join("archive.jsonl");
    let error = export_file(&database, &output, "unkeyed-prefix".to_owned())
        .expect_err("unsupported authoritative data must refuse the whole export");
    assert!(
        error.to_string().contains("fts_messages_notes"),
        "{error:#}"
    );
    assert!(!output.exists());
    assert_eq!(before, contents(source.path()));
    Ok(())
}

#[test]
fn canonical_empty_archive_schema_has_an_exportable_snapshot() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let database = source.path().join("agent_search.db");
    let storage = coding_agent_search::storage::sqlite::SqliteStorage::open(&database).unwrap();
    drop(storage);
    let before = contents(source.path());
    let output = destination.path().join("archive.jsonl");
    let receipt = export_file(&database, &output, "canonical-test".to_owned()).unwrap();
    assert_eq!(receipt, verify_file(&output).unwrap());
    assert!(receipt.1.tables.contains_key("messages"));
    assert!(receipt.1.tables.contains_key("conversations"));
    assert!(receipt.1.tables.contains_key("meta"));
    assert_eq!(before, contents(source.path()));
}

#[cfg(unix)]
#[test]
fn symlink_sources_and_destination_locks_are_refused() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("agent_search.db");
    fixture(&database);
    let linked = root.path().join("linked.db");
    symlink(&database, &linked).unwrap();
    assert!(open_source(&linked).is_err());
    let output = root.path().join("archive.jsonl");
    symlink(
        &database,
        root.path().join(".archive.jsonl.logical-archive.lock"),
    )
    .unwrap();
    assert!(DestinationLock::acquire(&output).is_err());
}

#[cfg(unix)]
#[test]
fn verification_refuses_nonregular_inputs_before_decoding() -> Result<()> {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir()?;
    let target = root.path().join("private.jsonl");
    fs::write(&target, "PRIVATE-NOT-JSON\n")?;
    let linked = root.path().join("linked.jsonl");
    symlink(&target, &linked)?;
    for input in [root.path(), linked.as_path(), Path::new("/dev/null")] {
        let error = verify_file(input).expect_err("nonregular input must be refused");
        let message = format!("{error:#}");
        assert!(message.contains("regular, non-symlink"), "{message}");
        assert!(!message.contains("PRIVATE-NOT-JSON"));
    }
    assert_eq!(fs::read(&target)?, b"PRIVATE-NOT-JSON\n");
    assert_eq!(fs::read_link(&linked)?, target);
    Ok(())
}

#[test]
fn scan_failure_keeps_busy_errors_retryable() {
    for cause in [FrankenError::Busy, FrankenError::BusyRecovery] {
        let error = scan_failure("messages", 468_001, cause);
        assert_eq!(
            super::super::classify_failure(&error),
            (7, "logical-archive-busy", true)
        );
        assert!(error.downcast_ref::<FrankenError>().is_some());
        assert!(error.to_string().contains("before row 468001"));
    }
}

#[test]
fn scan_failure_keeps_the_cause_but_redacts_its_message() {
    let error = scan_failure(
        "messages",
        42,
        FrankenError::Internal("PRIVATE-DATABASE-CONTENT".to_owned()),
    );
    assert_eq!(
        super::super::classify_failure(&error),
        (9, "logical-archive-error", false)
    );
    let public_message = error.to_string();
    assert!(public_message.contains("messages before row 42"));
    assert!(public_message.contains("source was not repaired"));
    assert!(!public_message.contains("PRIVATE-DATABASE-CONTENT"));
    assert!(matches!(
        error.downcast_ref::<FrankenError>(),
        Some(FrankenError::Internal(message)) if message == "PRIVATE-DATABASE-CONTENT"
    ));
}

#[test]
fn exact_limit_null_heavy_row_is_published_and_verified() -> Result<()> {
    let root = tempfile::tempdir()?;
    let database = root.path().join("source.db");
    let output = root.path().join("archive.jsonl");
    let connection = Connection::open(path_text(&database)?)?;
    let nullable_columns = (0..62)
        .map(|index| format!("optional_{index} TEXT"))
        .collect::<Vec<_>>()
        .join(", ");
    connection.execute_batch(&format!(
        "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         INSERT INTO meta VALUES ('schema_version', '9');
         CREATE TABLE messages (id INTEGER PRIMARY KEY, body TEXT, {nullable_columns});"
    ))?;

    // Determine the wire framing with an independent JSON serializer. The
    // body contains no escaped bytes, so each added byte grows the line by one.
    let mut values = vec![Cell::Integer(17), Cell::Text(String::new())];
    values.extend(std::iter::repeat_n(Cell::Null, 62));
    let framing = serde_json::to_vec(&Record::Row { values })?.len() + 1;
    let body_length = codec::MAX_RECORD_BYTES - framing;
    // The former 32-byte scalar estimate rejected this supported row.
    assert!(body_length + 63 * 32 >= codec::MAX_RECORD_BYTES);
    connection.execute_with_params(
        "INSERT INTO messages (id, body) VALUES (17, ?1)",
        &[SqliteValue::Text("s".repeat(body_length).into())],
    )?;
    connection.execute("INSERT INTO messages (id, body) VALUES (19, 'after the large row')")?;
    connection.close()?;

    let receipt = export_file(&database, &output, "boundary-archive".to_owned())?;
    assert_eq!(receipt, verify_file(&output)?);
    assert_eq!(receipt.1.records, 3);
    assert_eq!(receipt.1.tables["messages"], 2);
    let mut input = BufReader::new(File::open(&output)?);
    let mut line = Vec::new();
    let mut exact_rows = 0;
    while input.read_until(b'\n', &mut line)? != 0 {
        assert!(line.len() <= codec::MAX_RECORD_BYTES);
        if line.len() == codec::MAX_RECORD_BYTES {
            let Record::Row { values } = serde_json::from_slice(&line)? else {
                panic!("the boundary record must be a row"); // ubs:ignore -- intentional unit-test contract assertion.
            };
            assert_eq!(values.len(), 64);
            assert_eq!(values[0], Cell::Integer(17));
            let Cell::Text(body) = &values[1] else {
                panic!("the large body must remain text"); // ubs:ignore -- intentional unit-test contract assertion.
            };
            assert_eq!(body.len(), body_length);
            assert!(body.bytes().all(|byte| byte == b's'));
            assert!(values[2..].iter().all(|value| *value == Cell::Null));
            exact_rows += 1;
        }
        line.clear();
    }
    assert_eq!(exact_rows, 1);
    Ok(())
}

#[test]
fn invalid_utf8_text_is_refused_without_lossy_backup_or_private_diagnostic() -> Result<()> {
    let source = tempfile::tempdir()?;
    let destination = tempfile::tempdir()?;
    let database = source.path().join("source.db");
    let output = destination.path().join("archive.jsonl");
    fixture(&database);
    let connection = Connection::open(path_text(&database)?)?;
    let mut raw = b"PRIVATE-INVALID-UTF8".to_vec();
    raw.extend_from_slice(&[0x80, 0xff]);
    connection.execute_with_params(
        "INSERT INTO messages VALUES (47, CAST(?1 AS TEXT), NULL, NULL)",
        &[SqliteValue::Blob(raw.into())],
    )?;
    let row = connection.query_row("SELECT body FROM messages WHERE id = 47")?;
    assert!(matches!(
        &row.values()[0],
        SqliteValue::Text(text) if !text.is_valid_utf8()
    ));
    connection.close()?;
    let before = contents(source.path());

    let error = export_file(&database, &output, "invalid-text".to_owned())
        .expect_err("a JSON string cannot preserve invalid UTF-8 TEXT bytes");
    let message = error.to_string();
    assert!(
        message.contains("logical table messages, row 3"),
        "{message}"
    );
    assert!(message.contains("id=47"), "{message}");
    assert!(message.contains("invalid UTF-8"), "{message}");
    assert!(!message.contains("PRIVATE-INVALID-UTF8"));
    assert_eq!(
        super::super::classify_failure(&error),
        (9, "logical-archive-error", false)
    );
    assert!(!output.exists());
    assert_eq!(before, contents(source.path()));
    Ok(())
}

#[test]
fn oversized_cells_report_the_bound_without_echoing_the_value() {
    let private = "PRIVATE-OVERSIZED-CELL".repeat(codec::MAX_RECORD_BYTES / 20 + 1);
    let cause = cells(&[SqliteValue::Text(private.into())]).unwrap_err();
    let error = row_failure(
        "logical table messages, row 19".to_owned(),
        "cell conversion",
        cause,
    );
    let public_message = error.to_string();
    assert!(public_message.contains("messages, row 19, cell conversion"));
    assert!(public_message.contains("8 MiB"));
    assert!(!public_message.contains("PRIVATE-OVERSIZED-CELL"));
}

#[test]
fn json_expansion_failure_names_the_row_and_never_publishes() -> Result<()> {
    let source = tempfile::tempdir()?;
    let destination = tempfile::tempdir()?;
    let database = source.path().join("agent_search.db");
    fixture(&database);
    // Raw text fits the cell bound, but JSON's six-byte NUL escapes exceed
    // the encoded-record limit. This is distinct from an oversized raw cell.
    let body = format!(
        "PRIVATE-ENCODED-ROW{}",
        "\0".repeat(codec::MAX_RECORD_BYTES / 6 + 1)
    );
    assert!(body.len() < codec::MAX_RECORD_BYTES);
    let connection = Connection::open(path_text(&database)?)?;
    connection.execute_with_params(
        "UPDATE messages SET body = ?1 WHERE id = 7",
        &[SqliteValue::Text(body.into())],
    )?;
    connection.close()?;
    let before = contents(source.path());
    let output = destination.path().join("archive.jsonl");
    let error = export_file(&database, &output, "encoding-failure".to_owned())
        .expect_err("oversized JSON must refuse, never truncate or skip the row");
    let public_message = error.to_string();
    assert!(
        public_message.contains("messages, row 2"),
        "{public_message}"
    );
    assert!(public_message.contains("id=7"), "{public_message}");
    assert!(
        public_message.contains("record validation/encoding"),
        "{public_message}"
    );
    assert!(public_message.contains("8 MiB"), "{public_message}");
    assert!(!public_message.contains("PRIVATE-ENCODED-ROW"));
    assert!(!output.exists());
    assert_eq!(before, contents(source.path()));
    assert_eq!(
        fs::read_dir(destination.path())?.count(),
        1,
        "only the persistent destination lock may remain, not a partial backup"
    );
    Ok(())
}

#[derive(Default)]
struct FaultWriter {
    writes: usize,
    flushes: usize,
    fail_at: Option<usize>,
    fail_flush: bool,
}

impl Write for FaultWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let position = self.writes;
        self.writes += 1;
        if self.fail_at == Some(position) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "PRIVATE-WRITER-DETAIL",
            ));
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.flushes += 1;
        if self.fail_flush {
            return Err(std::io::Error::other("PRIVATE-FLUSH-DETAIL"));
        }
        Ok(())
    }
}

/// Exercise Write's real short-write/Interrupted contract. Error payloads
/// deliberately contain private text that the JSON diagnostic must withhold.
struct PartialWriter {
    bytes: Vec<u8>,
    fail_after: Option<usize>,
    interrupted_writes: bool,
    interrupt_next: bool,
    interruptions: usize,
    fail_flush: bool,
    flushes: usize,
}

impl PartialWriter {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            fail_after: None,
            interrupted_writes: false,
            interrupt_next: false,
            interruptions: 0,
            fail_flush: false,
            flushes: 0,
        }
    }
}

impl Write for PartialWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.interrupt_next {
            self.interrupt_next = false;
            self.interruptions += 1;
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "PRIVATE-WRITER-DIAGNOSTIC",
            ));
        }
        let remaining = self
            .fail_after
            .map_or(usize::MAX, |limit| limit.saturating_sub(self.bytes.len()));
        if remaining == 0 {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "PRIVATE-WRITER-DIAGNOSTIC",
            ));
        }
        let length = bytes.len().min(7).min(remaining);
        self.bytes.extend_from_slice(&bytes[..length]);
        self.interrupt_next = self.interrupted_writes;
        Ok(length)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flushes += 1;
        if self.fail_flush {
            Err(io::Error::other("PRIVATE-FLUSH-DIAGNOSTIC"))
        } else {
            Ok(())
        }
    }
}

#[test]
fn row_write_failure_keeps_io_class_and_public_location() -> Result<()> {
    let source = tempfile::tempdir()?;
    let database = source.path().join("agent_search.db");
    fixture(&database);
    let connection = open_source(&database)?;
    // Header, messages descriptor, first row, then fail on the second row.
    let mut writer = FaultWriter {
        fail_at: Some(3),
        ..Default::default()
    };
    let error = snapshot(&connection, "write-failure".to_owned(), &mut writer).unwrap_err();
    assert_eq!(
        super::super::classify_failure(&error),
        (14, "logical-archive-io", true)
    );
    let public_message = error.to_string();
    assert!(
        public_message.contains("messages, row 2"),
        "{public_message}"
    );
    assert!(public_message.contains("WriteZero"), "{public_message}");
    assert!(!public_message.contains("PRIVATE-WRITER-DETAIL"));
    assert_eq!(writer.writes, 4);
    assert_eq!(writer.flushes, 0);
    connection.execute("ROLLBACK")?;
    connection.close_without_checkpoint()?;
    Ok(())
}

#[test]
fn header_and_completion_write_errors_do_not_echo_writer_details() -> Result<()> {
    let source = tempfile::tempdir()?;
    let database = source.path().join("agent_search.db");
    fixture(&database);
    let connection = open_source(&database)?;
    for (fail_at, location) in [(0, "header"), (6, "completion")] {
        let mut writer = FaultWriter {
            fail_at: Some(fail_at),
            ..Default::default()
        };
        let error = snapshot(&connection, "write-failure".to_owned(), &mut writer).unwrap_err();
        assert_eq!(
            super::super::classify_failure(&error),
            (14, "logical-archive-io", true)
        );
        let public_message = error.to_string();
        assert!(public_message.contains(location), "{public_message}");
        assert!(!public_message.contains("PRIVATE-WRITER-DETAIL"));
    }
    connection.execute("ROLLBACK")?;
    connection.close_without_checkpoint()?;
    Ok(())
}

#[test]
fn partial_row_write_preserves_io_cause_and_private_row_location() -> Result<()> {
    let root = tempfile::tempdir()?;
    let database = root.path().join("source.db");
    fixture(&database);
    let connection = open_source(&database)?;
    let mut complete = Vec::new();
    snapshot(&connection, "partial-write".to_owned(), &mut complete)?;
    let mut offset = 0;
    let mut failure_offset = None;
    for line in complete.split_inclusive(|byte| *byte == b'\n') {
        if let Record::Row { values } = serde_json::from_slice(line)?
            && values[0] == Cell::Integer(7)
        {
            failure_offset = Some(offset + line.len() / 2);
            break;
        }
        offset += line.len();
    }
    let failure_offset = failure_offset.expect("fixture has a second message");
    let mut output = PartialWriter::new();
    output.fail_after = Some(failure_offset);
    let error = snapshot(&connection, "partial-write".to_owned(), &mut output)
        .expect_err("an incomplete row must fail the snapshot");
    let message = error.to_string();
    assert!(
        message.contains("logical table messages, row 2"),
        "{message}"
    );
    assert!(message.contains("id=7"), "{message}");
    assert!(message.contains("BrokenPipe"), "{message}");
    assert!(!message.contains("PRIVATE-WRITER-DIAGNOSTIC"));
    assert!(!message.contains("private transcript"));
    assert_eq!(
        super::super::classify_failure(&error),
        (14, "logical-archive-io", true)
    );
    assert!(error.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(|error| error.kind() == io::ErrorKind::BrokenPipe)
    }));
    assert_eq!(output.bytes.len(), failure_offset);
    assert_eq!(output.flushes, 0);
    assert!(codec::verify(&mut output.bytes.as_slice()).is_err());
    connection.execute("ROLLBACK")?;
    connection.close_without_checkpoint()?;
    Ok(())
}

#[test]
fn final_flush_failure_is_not_a_successful_snapshot() -> Result<()> {
    let source = tempfile::tempdir()?;
    let database = source.path().join("agent_search.db");
    fixture(&database);
    let connection = open_source(&database)?;
    let mut writer = FaultWriter {
        fail_flush: true,
        ..Default::default()
    };
    let error = snapshot(&connection, "flush-failure".to_owned(), &mut writer).unwrap_err();
    assert_eq!(
        super::super::classify_failure(&error),
        (14, "logical-archive-io", true)
    );
    assert!(error.to_string().contains("cannot flush logical archive"));
    assert!(!error.to_string().contains("PRIVATE-FLUSH-DETAIL"));
    assert_eq!(writer.flushes, 1);
    connection.execute("ROLLBACK")?;
    connection.close_without_checkpoint()?;
    Ok(())
}

#[test]
fn interrupted_short_writes_resume_without_duplicating_records() -> Result<()> {
    let root = tempfile::tempdir()?;
    let database = root.path().join("source.db");
    fixture(&database);
    let connection = open_source(&database)?;
    let mut output = PartialWriter::new();
    output.interrupted_writes = true;
    output.interrupt_next = true;
    let receipt = snapshot(&connection, "interrupted-writes".to_owned(), &mut output)?;
    assert!(output.interruptions > 3);
    assert_eq!(output.flushes, 1);
    assert_eq!(receipt, codec::verify(&mut output.bytes.as_slice())?);
    assert_eq!(receipt.1.tables["messages"], 2);
    assert_eq!(receipt.1.records, 3);
    connection.execute("ROLLBACK")?;
    connection.close_without_checkpoint()?;
    Ok(())
}

#[derive(Default)]
struct CountingWriter {
    bytes: Vec<u8>,
    writes: usize,
    flushes: usize,
    largest_write: usize,
}

impl Write for CountingWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.writes += 1;
        self.largest_write = self.largest_write.max(bytes.len());
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.flushes += 1;
        Ok(())
    }
}

#[test]
fn flush_failure_reports_stage_and_kind_without_private_writer_payload() -> Result<()> {
    let root = tempfile::tempdir()?;
    let database = root.path().join("source.db");
    fixture(&database);
    let connection = open_source(&database)?;
    let mut output = PartialWriter::new();
    output.fail_flush = true;
    let error = snapshot(&connection, "flush-failure".to_owned(), &mut output)
        .expect_err("a failed flush must not return a successful backup receipt");
    let message = error.to_string();
    assert!(
        message.contains("cannot flush logical archive"),
        "{message}"
    );
    assert!(message.contains("I/O Other"), "{message}");
    assert!(!message.contains("PRIVATE-FLUSH-DIAGNOSTIC"));
    assert_eq!(
        super::super::classify_failure(&error),
        (14, "logical-archive-io", true)
    );
    assert_eq!(output.flushes, 1);
    // The error is specifically in the final flush, after complete wire data.
    assert_eq!(codec::verify(&mut output.bytes.as_slice())?.1.records, 3);
    connection.execute("ROLLBACK")?;
    connection.close_without_checkpoint()?;
    Ok(())
}

#[test]
fn database_error_diagnostics_hide_private_payloads_and_preserve_typed_causes() {
    let context = "cannot read logical table messages after 23 complete rows";
    let corruption = source_failure(
        context,
        FrankenError::DatabaseCorrupt {
            detail: "PRIVATE-DATABASE-PAYLOAD".to_owned(),
        },
    );
    let message = corruption.to_string();
    assert!(message.starts_with(context), "{message}");
    assert!(message.contains("FrankenSQLite Corrupt (code 11)"));
    assert!(!message.contains("PRIVATE-DATABASE-PAYLOAD"));
    assert!(matches!(
        corruption.downcast_ref::<FrankenError>(),
        Some(FrankenError::DatabaseCorrupt { detail }) if detail == "PRIVATE-DATABASE-PAYLOAD"
    ));

    let read_error = source_failure(
        context,
        FrankenError::Io(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "PRIVATE-DATABASE-PATH",
        )),
    );
    let message = read_error.to_string();
    assert!(message.starts_with(context), "{message}");
    assert!(message.contains("FrankenSQLite IoErr (code 10)"));
    assert!(message.contains("I/O PermissionDenied"));
    assert!(!message.contains("PRIVATE-DATABASE-PATH"));
    assert!(matches!(
        read_error.downcast_ref::<FrankenError>(),
        Some(FrankenError::Io(error)) if error.kind() == io::ErrorKind::PermissionDenied
    ));
    assert!(read_error.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(|error| error.kind() == io::ErrorKind::PermissionDenied)
    }));
    assert_eq!(
        super::super::classify_failure(&read_error),
        (14, "logical-archive-io", true)
    );
}

/// Observe complete records even if the exporter changes its write chunking.
/// Used to interleave a real engine event at a known point in a small fixture.
struct ObservingWriter<F> {
    bytes: Vec<u8>,
    next_line: usize,
    observe: F,
}

impl<F: FnMut(&Record)> Write for ObservingWriter<F> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes.extend_from_slice(bytes);
        while let Some(end) = self.bytes[self.next_line..]
            .iter()
            .position(|byte| *byte == b'\n')
        {
            let end = self.next_line + end + 1;
            let record = serde_json::from_slice(&self.bytes[self.next_line..end])
                .map_err(io::Error::other)?;
            self.next_line = end;
            (self.observe)(&record);
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn buffered_snapshot_coalesces_rows_without_changing_the_verified_digest() -> Result<()> {
    let source = tempfile::tempdir()?;
    let database = source.path().join("agent_search.db");
    fixture(&database);
    let connection = Connection::open(path_text(&database)?)?;
    let body = SqliteValue::Text("buffered transcript ".repeat(80).into());
    connection.execute("BEGIN")?;
    for id in 100..612 {
        connection.execute_with_params(
            "INSERT INTO messages(id, body) VALUES (?1, ?2)",
            &[SqliteValue::Integer(id), body.clone()],
        )?;
    }
    connection.execute("COMMIT")?;
    connection.close()?;
    let connection = open_source(&database)?;
    let mut direct = CountingWriter::default();
    let direct_receipt = snapshot(&connection, "buffer-proof".to_owned(), &mut direct)?;
    let mut buffered = CountingWriter::default();
    let buffered_receipt =
        buffered_snapshot(&connection, "buffer-proof".to_owned(), &mut buffered)?;
    // The timestamp may differ, but canonical rows and their digest must not.
    assert_eq!(direct_receipt.1, buffered_receipt.1);
    assert_eq!(
        codec::verify(&mut BufReader::new(buffered.bytes.as_slice()))?,
        buffered_receipt
    );
    assert_eq!(buffered_receipt.1.tables["messages"], 514);
    assert!(buffered.bytes.len() > 2 * EXPORT_IO_BUFFER_BYTES);
    assert!(buffered.writes > 1, "exercise more than one buffer flush");
    assert!(
        buffered.writes * 16 < direct.writes,
        "small records were not coalesced: direct={}, buffered={}",
        direct.writes,
        buffered.writes
    );
    assert!(buffered.largest_write <= EXPORT_IO_BUFFER_BYTES);
    assert_eq!(buffered.flushes, 1);
    connection.execute("ROLLBACK")?;
    connection.close_without_checkpoint()?;
    Ok(())
}

#[test]
fn buffered_snapshot_preserves_records_larger_than_its_io_buffer() -> Result<()> {
    let source = tempfile::tempdir()?;
    let database = source.path().join("agent_search.db");
    fixture(&database);
    let connection = Connection::open(path_text(&database)?)?;
    connection.execute_with_params(
        "UPDATE messages SET body = ?1 WHERE id = 7",
        &[SqliteValue::Text(
            "x".repeat(EXPORT_IO_BUFFER_BYTES * 2).into(),
        )],
    )?;
    connection.close()?;
    let connection = open_source(&database)?;
    let mut writer = CountingWriter::default();
    let receipt = buffered_snapshot(&connection, "large-record".to_owned(), &mut writer)?;
    assert!(writer.largest_write > EXPORT_IO_BUFFER_BYTES);
    assert_eq!(
        codec::verify(&mut BufReader::new(writer.bytes.as_slice()))?,
        receipt
    );
    assert_eq!(receipt.1.tables["messages"], 2);
    connection.execute("ROLLBACK")?;
    connection.close_without_checkpoint()?;
    Ok(())
}

#[test]
fn buffered_snapshot_does_not_retry_failed_writes_on_drop() -> Result<()> {
    let source = tempfile::tempdir()?;
    let database = source.path().join("agent_search.db");
    fixture(&database);
    let connection = open_source(&database)?;
    // This small fixture reaches the sink on the final explicit flush. A
    // normal BufWriter drop would retry the still-buffered failed write.
    let mut writer = FaultWriter {
        fail_at: Some(0),
        ..Default::default()
    };
    let error = buffered_snapshot(&connection, "failed-buffer".to_owned(), &mut writer)
        .expect_err("a failed final write must not produce a receipt");
    assert_eq!(
        super::super::classify_failure(&error),
        (14, "logical-archive-io", true)
    );
    assert!(error.to_string().contains("cannot flush logical archive"));
    assert!(!error.to_string().contains("PRIVATE-WRITER-DETAIL"));
    assert_eq!(
        writer.writes, 1,
        "an error must not trigger an implicit retry"
    );
    assert_eq!(writer.flushes, 0);
    connection.execute("ROLLBACK")?;
    connection.close_without_checkpoint()?;
    Ok(())
}

#[test]
fn buffered_snapshot_propagates_underlying_flush_errors() -> Result<()> {
    let source = tempfile::tempdir()?;
    let database = source.path().join("agent_search.db");
    fixture(&database);
    let connection = open_source(&database)?;
    let mut writer = FaultWriter {
        fail_flush: true,
        ..Default::default()
    };
    let error = buffered_snapshot(&connection, "failed-flush".to_owned(), &mut writer).unwrap_err();
    assert_eq!(
        super::super::classify_failure(&error),
        (14, "logical-archive-io", true)
    );
    assert!(error.to_string().contains("cannot flush logical archive"));
    assert!(!error.to_string().contains("PRIVATE-FLUSH-DETAIL"));
    assert_eq!(writer.writes, 1);
    assert_eq!(writer.flushes, 1);
    connection.execute("ROLLBACK")?;
    connection.close_without_checkpoint()?;
    Ok(())
}

#[test]
fn engine_cancellation_after_emitted_rows_retains_source_cause_and_progress() -> Result<()> {
    use fsqlite_types::cx::CancelReason;
    use std::cell::Cell as Counter;

    let connection = Connection::open(":memory:")?;
    connection.execute_batch(
        "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         INSERT INTO meta VALUES ('schema_version', '9');
         CREATE TABLE messages (id INTEGER PRIMARY KEY, body TEXT);",
    )?;
    let rows = (1..=2048)
        .map(|id| format!("({id}, 'PRIVATE-CANCELLED-TRANSCRIPT')"))
        .collect::<Vec<_>>()
        .join(", ");
    connection.execute(&format!("INSERT INTO messages VALUES {rows}"))?;
    connection.execute("BEGIN")?;
    let (operation, relay) = connection
        .as_async()
        .root_cx()
        .create_child_with_local_cancel_relay();
    let rows_seen = Counter::new(0u64);
    let mut output = ObservingWriter {
        bytes: Vec::new(),
        next_line: 0,
        observe: |record: &Record| {
            if matches!(record, Record::Row { .. }) {
                rows_seen.set(rows_seen.get() + 1);
                if rows_seen.get() == 1 {
                    assert!(relay.cancel_local(CancelReason::UserInterrupt));
                }
            }
        },
    };
    let error = {
        let _binding = connection.as_async().bind_operation_cx(&operation);
        snapshot(&connection, "cancelled-source".to_owned(), &mut output)
            .expect_err("engine cancellation must fail the active table scan")
    };
    assert!((1..2048).contains(&rows_seen.get()));
    let message = error.to_string();
    assert!(
        message.contains("cannot stream logical table messages"),
        "{message}"
    );
    assert!(
        message.contains(&format!("after {} complete rows", rows_seen.get())),
        "{message}"
    );
    assert!(
        message.contains("FrankenSQLite Abort (code 4)"),
        "{message}"
    );
    assert!(!message.contains("PRIVATE-CANCELLED-TRANSCRIPT"));
    assert!(error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<FrankenError>(),
            Some(FrankenError::Abort)
        )
    }));
    assert!(codec::verify(&mut output.bytes.as_slice()).is_err());
    // The per-operation cancellation must leave rollback and later reads usable.
    assert!(connection.as_async().root_cx().checkpoint().is_ok());
    connection.execute("ROLLBACK")?;
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM messages")?
            .get_typed::<i64>(0)?,
        2048
    );
    connection.close()?;
    Ok(())
}

#[test]
fn wal_writer_committing_during_export_cannot_mix_snapshot_generations() -> Result<()> {
    use std::cell::Cell as Flag;

    let root = tempfile::tempdir()?;
    let database = root.path().join("source.db");
    fixture(&database);
    let writer = Connection::open(path_text(&database)?)?;
    writer.execute("PRAGMA journal_mode = WAL")?;
    let reader = open_source(&database)?;
    let changed = Flag::new(false);
    let mut output = ObservingWriter {
        bytes: Vec::new(),
        next_line: 0,
        observe: |record: &Record| {
            if let Record::Row { values } = record
                && values[0] == Cell::Integer(3)
                && !changed.replace(true)
            {
                assert!(reader.as_async().in_transaction());
                writer
                    .execute_batch(
                        "BEGIN;
                         UPDATE messages SET body = 'PRIVATE-NEW-GENERATION' WHERE id = 7;
                         INSERT INTO messages VALUES (9, 'PRIVATE-NEW-GENERATION', NULL, NULL);
                         UPDATE meta SET value = '10' WHERE key = 'schema_version';
                         COMMIT;",
                    )
                    .expect(
                        "the independent WAL writer must commit while the read snapshot is held",
                    );
            }
        },
    };
    let receipt = snapshot(&reader, "stable-snapshot".to_owned(), &mut output)?;
    assert!(
        changed.get(),
        "the concurrent commit must actually have run"
    );
    assert_eq!(receipt.0.storage_schema_version, "9");
    assert_eq!(receipt.1.tables["messages"], 2);
    assert_eq!(receipt.1.records, 3);
    assert_eq!(receipt, codec::verify(&mut output.bytes.as_slice())?);
    assert!(reader.as_async().in_transaction());
    let mut saw_old_body = false;
    let mut saw_old_meta = false;
    for line in output.bytes.split_inclusive(|byte| *byte == b'\n') {
        if let Record::Row { values } = serde_json::from_slice(line)? {
            if values[0] == Cell::Integer(7) {
                assert_eq!(values[1], Cell::Text("private transcript".to_owned()));
                saw_old_body = true;
            } else if values[0] == Cell::Text("schema_version".to_owned()) {
                assert_eq!(values[1], Cell::Text("9".to_owned()));
                saw_old_meta = true;
            }
        }
    }
    assert!(saw_old_body && saw_old_meta);
    assert!(!String::from_utf8_lossy(&output.bytes).contains("PRIVATE-NEW-GENERATION"));
    reader.execute("ROLLBACK")?;
    reader.close_without_checkpoint()?;
    assert_eq!(
        writer
            .query_row("SELECT COUNT(*) FROM messages")?
            .get_typed::<i64>(0)?,
        3
    );
    assert_eq!(
        writer
            .query_row("SELECT value FROM meta WHERE key = 'schema_version'")?
            .get_typed::<String>(0)?,
        "10"
    );
    writer.close()?;
    Ok(())
}
