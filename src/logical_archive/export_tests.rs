use super::*;
use std::collections::BTreeMap;
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
fn oversized_cells_report_the_bound_without_echoing_the_value() {
    let private = "PRIVATE-OVERSIZED-CELL".repeat(codec::MAX_RECORD_BYTES / 20 + 1);
    let cause = cells(&[SqliteValue::Text(private.into())]).unwrap_err();
    let error = row_failure("messages", 19, "cell conversion", cause);
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
        public_message.contains("messages, row 2, record validation/encoding"),
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
    assert!(public_message.contains("messages, row 2"), "{public_message}");
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
    assert!(error.to_string().contains("logical archive flush"));
    assert!(!error.to_string().contains("PRIVATE-FLUSH-DETAIL"));
    assert_eq!(writer.flushes, 1);
    connection.execute("ROLLBACK")?;
    connection.close_without_checkpoint()?;
    Ok(())
}
