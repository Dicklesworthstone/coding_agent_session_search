use super::*;
use std::collections::BTreeMap;
use std::path::PathBuf;

fn fixture(path: &Path) {
    let connection = Connection::open(path.to_str().unwrap()).unwrap();
    connection.execute_batch(
        "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         INSERT INTO meta VALUES ('schema_version', '9');
         CREATE TABLE messages (id INTEGER PRIMARY KEY, body TEXT, usage REAL, raw BLOB);
         INSERT INTO messages VALUES (7, 'private transcript', 1.25, X'0001FF');
         INSERT INTO messages VALUES (3, 'earlier', NULL, NULL);",
    ).unwrap();
    connection.close().unwrap();
}

fn contents(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fs::read_dir(root).unwrap().map(|entry| {
        let path = entry.unwrap().path();
        (path.clone(), fs::read(path).unwrap())
    }).collect()
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
        assert_eq!(fs::metadata(output).unwrap().permissions().mode() & 0o077, 0);
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
    connection.execute("CREATE TABLE unknown_data (body TEXT)").unwrap();
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
        assert!(!is_fts5_shadow_table(name, "fts_messages"), "omitted {name}");
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
    assert!(error.to_string().contains("fts_messages_notes"), "{error:#}");
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
    symlink(&database, root.path().join(".archive.jsonl.logical-archive.lock")).unwrap();
    assert!(DestinationLock::acquire(&output).is_err());
}
