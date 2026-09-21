use super::*;
use std::io::Cursor;

fn header() -> Header {
    Header {
        format: codec::FORMAT.to_owned(),
        schema_version: codec::VERSION,
        archive_id: "restore-test".to_owned(),
        exported_at_ms: 17,
        storage_schema_version: "9".to_owned(),
        record_types: ["table", "row", "completion"].into_iter().map(str::to_owned).collect(),
        contains_private_data: true,
        omissions: vec!["derived_search_assets".to_owned()],
    }
}

fn fixture() -> Connection {
    let connection = Connection::open(":memory:").unwrap();
    connection.execute_batch(
        "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         INSERT INTO meta VALUES ('schema_version', '9');
         CREATE TABLE parents (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
         CREATE TABLE children (id INTEGER PRIMARY KEY, parent_id INTEGER NOT NULL REFERENCES parents(id), payload BLOB, number REAL);",
    ).unwrap();
    connection
}

fn encoded(connection: &Connection) -> Vec<u8> {
    let mut bytes = Vec::new();
    export::snapshot(connection, "restore-test".to_owned(), &mut bytes).unwrap();
    bytes
}

fn replay(connection: &Connection, bytes: &[u8]) -> Result<(Header, Completion)> {
    let mut reader = Input::new(Cursor::new(bytes));
    let Some(Record::Header { header }) = reader.record(1)? else {
        bail!("missing test header");
    };
    restore(connection, &mut reader, header)
}

#[test]
fn typed_rows_and_child_before_parent_relationships_round_trip() {
    let source = fixture();
    source.execute_batch(
        "INSERT INTO parents VALUES (42, 'private δ text');
         INSERT INTO children VALUES (7, 42, X'0001FF', -1.25);",
    ).unwrap();
    let bytes = encoded(&source);
    let target = fixture();
    let receipt = replay(&target, &bytes).unwrap();
    let after = encoded(&target);
    assert_eq!(receipt.1, codec::verify(&mut Cursor::new(after)).unwrap().1);
    let child = target.query_row("SELECT parent_id, payload, number FROM children WHERE id = 7").unwrap();
    assert_eq!(child.get_typed::<i64>(0).unwrap(), 42);
    assert_eq!(child.get_typed::<Vec<u8>>(1).unwrap(), vec![0, 1, 255]);
    assert_eq!(child.get_typed::<f64>(2).unwrap().to_bits(), (-1.25_f64).to_bits());
}

#[test]
fn initializer_triggers_are_suspended_and_restored_not_replayed() {
    let source = fixture();
    source.execute("INSERT INTO parents VALUES (1, 'source')").unwrap();
    let target = fixture();
    target.execute_batch(
        "CREATE TRIGGER add_shadow_parent AFTER INSERT ON parents WHEN NEW.id < 100
         BEGIN INSERT INTO parents VALUES (NEW.id + 100, 'trigger'); END;",
    ).unwrap();
    replay(&target, &encoded(&source)).unwrap();
    assert_eq!(target.query_row("SELECT COUNT(*) FROM parents").unwrap().get_typed::<i64>(0).unwrap(), 1);
    target.execute("INSERT INTO parents VALUES (2, 'later write')").unwrap();
    assert_eq!(target.query_row("SELECT name FROM parents WHERE id = 102").unwrap().get_typed::<String>(0).unwrap(), "trigger");
}

#[test]
fn missing_or_changed_schema_is_not_accepted_by_version_alone() {
    let source = fixture();
    source.execute("CREATE TABLE extra (id INTEGER PRIMARY KEY)").unwrap();
    assert!(replay(&fixture(), &encoded(&source)).is_err());
    let source = fixture();
    let target = fixture();
    target.execute("CREATE TABLE extra (id INTEGER PRIMARY KEY)").unwrap();
    assert!(replay(&target, &encoded(&source)).is_err());
    let source = fixture();
    source.execute("ALTER TABLE children ADD COLUMN secret TEXT").unwrap();
    assert!(replay(&fixture(), &encoded(&source)).is_err());
}

#[test]
fn broken_relationship_is_rejected_even_with_a_valid_digest() {
    let source = fixture();
    source.execute("PRAGMA foreign_keys = OFF").unwrap();
    source.execute("INSERT INTO children VALUES (1, 999, NULL, NULL)").unwrap();
    let bytes = encoded(&source);
    assert!(codec::verify(&mut Cursor::new(&bytes)).is_ok());
    assert!(replay(&fixture(), &bytes).is_err());
}

#[test]
fn input_accounting_includes_whitespace_and_resets_between_records() {
    let record = codec::encode(&Record::Header { header: header() }).unwrap();
    let mut bytes = vec![b' '; 4096];
    bytes.extend_from_slice(&record);
    bytes.extend_from_slice(&record);
    let mut reader = Input::new(BufReader::with_capacity(7, Cursor::new(bytes)));
    assert!(reader.record(1).unwrap().is_some());
    assert_eq!(reader.record_bytes, 4096 + record.len());
    assert!(reader.record(2).unwrap().is_some());
    assert_eq!(reader.record_bytes, record.len());
}

#[test]
fn replay_spans_bounded_batches_without_losing_rows() {
    let source = fixture();
    for id in 0..(MAX_BATCH_RECORDS * 3 + 1) {
        source.execute_with_params(
            "INSERT INTO parents VALUES (?, ?)",
            &[SqliteValue::Integer(id as i64), SqliteValue::Text(format!("row-{id}").into())],
        ).unwrap();
    }
    let bytes = encoded(&source);
    let target = fixture();
    let receipt = replay(&target, &bytes).unwrap();
    assert_eq!(receipt.1.tables["parents"], (MAX_BATCH_RECORDS * 3 + 1) as u64);
    assert_eq!(receipt.1, codec::verify(&mut Cursor::new(encoded(&target))).unwrap().1);
}

#[test]
fn cell_conversion_preserves_binary_and_integer_extremes() {
    let cells = vec![
        Cell::Null, Cell::Integer(i64::MIN), Cell::Integer(i64::MAX),
        Cell::Real(format!("{:016x}", (-0.0_f64).to_bits())),
        Cell::Text("quotes '\" ; \0 δ".to_owned()), Cell::Blob("AAH/".to_owned()),
    ];
    assert_eq!(export::cells(&values(cells.clone()).unwrap()).unwrap(), cells);
    assert!(values(vec![Cell::Real("7ff0000000000000".to_owned())]).is_err());
    assert!(values(vec![Cell::Blob("!!".to_owned())]).is_err());
}

#[test]
fn complete_canonical_file_round_trip_preserves_source_and_published_digest() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source.db");
    drop(SqliteStorage::open(&source).unwrap());
    let archive = root.path().join("history.jsonl");
    let exported = export::export_file(&source, &archive, "restore-test".to_owned()).unwrap();
    let source_before = fs::read(&source).unwrap();
    let input_before = fs::read(&archive).unwrap();
    let target = root.path().join("restored.db");
    let imported = import_file(&archive, &target, "restore-test").unwrap();
    assert_eq!(imported, exported);
    assert_eq!(source_before, fs::read(&source).unwrap());
    assert_eq!(input_before, fs::read(&archive).unwrap());
    let after = root.path().join("restored.jsonl");
    assert_eq!(exported.1, export::export_file(&target, &after, "restore-test".to_owned()).unwrap().1);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(&target).unwrap().permissions().mode() & 0o077, 0);
    }
}

#[test]
fn existing_database_or_orphan_sidecar_is_never_replaced() {
    let root = tempfile::tempdir().unwrap();
    let archive = root.path().join("input.jsonl");
    fs::write(&archive, encoded(&fixture())).unwrap();
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let destination = root.path().join(format!("target{suffix}.db"));
        let mut occupied = destination.as_os_str().to_os_string();
        occupied.push(suffix);
        let occupied = PathBuf::from(occupied);
        fs::write(&occupied, b"existing authority").unwrap();
        assert!(import_file(&archive, &destination, "restore-test").is_err());
        assert_eq!(fs::read(&occupied).unwrap(), b"existing authority");
        if !suffix.is_empty() { assert!(!destination.exists()); }
    }
}

#[test]
fn invalid_streams_never_publish_a_database() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source.db");
    drop(SqliteStorage::open(&source).unwrap());
    let archive = root.path().join("valid.jsonl");
    export::export_file(&source, &archive, "restore-test".to_owned()).unwrap();
    let valid = fs::read(&archive).unwrap();
    let completion_start = valid[..valid.len() - 1].iter().rposition(|byte| *byte == b'\n').unwrap() + 1;
    let mut extra_record = valid.clone();
    extra_record.extend_from_slice(b"{}\n");
    let mut bad_digest = valid.clone();
    let end = bad_digest.len() - 5;
    bad_digest[end] = if bad_digest[end] == b'0' { b'1' } else { b'0' };
    for (index, bytes) in [vec![], valid[..completion_start].to_vec(), valid[..valid.len() - 1].to_vec(), extra_record, bad_digest].into_iter().enumerate() {
        let input = root.path().join(format!("bad-{index}.jsonl"));
        let output = root.path().join(format!("bad-{index}.db"));
        fs::write(&input, bytes).unwrap();
        assert!(import_file(&input, &output, "restore-test").is_err());
        assert!(!output.exists());
    }
    let output = root.path().join("wrong-id.db");
    assert!(import_file(&archive, &output, "different-archive").is_err());
    assert!(!output.exists());
}

#[cfg(unix)]
#[test]
fn input_and_destination_symlinks_are_refused() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let archive = root.path().join("input.jsonl");
    fs::write(&archive, encoded(&fixture())).unwrap();
    let linked_input = root.path().join("linked.jsonl");
    symlink(&archive, &linked_input).unwrap();
    assert!(open_input(&linked_input).is_err());
    let output = root.path().join("output.db");
    let original = root.path().join("original.db");
    fs::write(&original, b"keep this").unwrap();
    symlink(&original, &output).unwrap();
    assert!(import_file(&archive, &output, "restore-test").is_err());
    assert_eq!(fs::read(&original).unwrap(), b"keep this");
}

#[test]
fn populated_canonical_archive_preserves_provider_messages_and_provenance() {
    use coding_agent_search::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source.db");
    let storage = SqliteStorage::open(&source).unwrap();
    let agent_id = storage.ensure_agent(&Agent {
        id: None, slug: "claude_code".to_owned(), name: "Claude Code".to_owned(),
        version: None, kind: AgentKind::Cli,
    }).unwrap();
    let conversation = Conversation {
        id: None, agent_slug: "claude_code".to_owned(), workspace: None,
        external_id: Some("remote-session-identity".to_owned()),
        title: Some("Private portable session".to_owned()),
        source_path: PathBuf::from("/vanished/remote/session.jsonl"),
        started_at: Some(1_733_000_000_000), ended_at: None, approx_tokens: None,
        metadata_json: serde_json::json!({"portable": true}),
        messages: vec![Message {
            id: None, idx: 0, role: MessageRole::User, author: Some("operator".to_owned()),
            created_at: Some(1_733_000_000_000), content: "private δ message\nline two".to_owned(),
            extra_json: serde_json::json!({"usage": {"input_tokens": 17}}), snippets: Vec::new(),
        }],
        source_id: "remote-host".to_owned(), origin_host: Some("remote-host".to_owned()),
    };
    storage.insert_conversation_tree(agent_id, None, &conversation).unwrap();
    drop(storage);
    let archive = root.path().join("history.jsonl");
    let before = export::export_file(&source, &archive, "restore-test".to_owned()).unwrap();
    assert_eq!(before.1.tables["messages"], 1);
    let target = root.path().join("restored.db");
    let after = import_file(&archive, &target, "restore-test").unwrap();
    assert_eq!(before, after);
    let reader = export::open_source(&target).unwrap();
    let row = reader.query_row(
        "SELECT c.external_id, c.source_id, c.origin_host, m.content
         FROM conversations c JOIN messages m ON m.conversation_id = c.id",
    ).unwrap();
    assert_eq!(row.get_typed::<String>(0).unwrap(), "remote-session-identity");
    assert_eq!(row.get_typed::<String>(1).unwrap(), "remote-host");
    assert_eq!(row.get_typed::<String>(2).unwrap(), "remote-host");
    assert_eq!(row.get_typed::<String>(3).unwrap(), "private δ message\nline two");
    reader.execute("ROLLBACK").unwrap();
    reader.close_without_checkpoint().unwrap();
}
