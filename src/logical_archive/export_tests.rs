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
