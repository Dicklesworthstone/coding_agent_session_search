use super::*;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use std::io::Cursor;

fn fixture(version: u32, large: bool, empty: bool) -> (Vec<u8>, String, Vec<Record>) {
    let header = codec::Header {
        format: codec::FORMAT.into(),
        schema_version: version,
        archive_id: "conversation-recovery".into(),
        exported_at_ms: 42,
        storage_schema_version: "22".into(),
        record_types: codec::record_types(version),
        contains_private_data: true,
        omissions: vec!["derived_search_assets".into()],
    };
    let table = |name: &str, columns: &[&str]| Record::Table {
        table: Table {
            name: name.into(),
            columns: columns.iter().map(|name| (*name).into()).collect(),
            primary_key: vec![0],
        },
    };
    let row = |values| Record::Row { values };
    let conversation_table = table("conversations", &["id", "source_path", "title", "extra_json"]);
    let conversation = row(vec![
        Cell::Integer(7),
        Cell::Text("/missing/provider/session.jsonl".into()),
        Cell::Text("PRIVATE-TARGET title 雪".into()),
        Cell::Text("{\"nested\":[1,true,null]}".into()),
    ]);
    let messages_table = table(
        "messages",
        &["id", "conversation_id", "idx", "role", "content", "extra_json", "extra_bin", "temperature", "optional"],
    );
    let message = |id, parent, idx, content: String, binary| row(vec![
        Cell::Integer(id), Cell::Integer(parent), Cell::Integer(idx),
        Cell::Text("assistant".into()), Cell::Text(content),
        Cell::Text("{ \"preserve_whitespace\" : true }".into()),
        Cell::Blob(STANDARD.encode(binary)),
        Cell::Real("8000000000000000".into()), Cell::Null,
    ]);
    let first = message(
        10, 7, 2,
        if large { "雪\0".repeat(codec::MAX_RECORD_BYTES / 9 + 1) }
        else { "PRIVATE-TARGET message\0 café\n".into() },
        vec![0_u8, 255, 128, 1].repeat(17_000),
    );
    let last = message(18, 7, 8, "PRIVATE-TARGET last message".into(), vec![]);
    let mut records = vec![
        table("agents", &["id", "name"]),
        row(vec![Cell::Integer(1), Cell::Text("PRIVATE-UNRELATED-AGENT".into())]),
        conversation_table.clone(),
        row(vec![Cell::Integer(1), Cell::Null, Cell::Text("PRIVATE-UNRELATED-TITLE".into()), Cell::Null]),
        conversation.clone(),
        messages_table.clone(),
    ];
    if !empty { records.push(first.clone()); }
    records.push(message(11, 1, 0, "PRIVATE-UNRELATED-MESSAGE".into(), vec![8_u8, 9]));
    if !empty { records.push(last.clone()); }
    let mut expected = vec![conversation_table, conversation, messages_table];
    if !empty { expected.extend([first, last]); }
    let mut validator = Validator::new(header.clone()).unwrap();
    let mut bytes = codec::encode(&Record::Header { header }).unwrap();
    for record in records {
        codec::write_encoded(&validator.push(&record).unwrap(), &mut bytes).unwrap();
    }
    let completion = validator.completion();
    let digest = completion.content_sha256.clone();
    bytes.extend(validator.push(&Record::Completion { completion }).unwrap());
    validator.finish().unwrap();
    (bytes, digest, expected)
}

fn decode_output(bytes: &[u8]) -> (Value, Value, Vec<Record>) {
    assert_eq!(bytes.last(), Some(&b'\n'));
    let first_end = bytes.iter().position(|byte| *byte == b'\n').unwrap() + 1;
    let footer_start = bytes[..bytes.len() - 1].iter().rposition(|byte| *byte == b'\n').unwrap() + 1;
    let header: Value = serde_json::from_slice(&bytes[..first_end]).unwrap();
    let footer: Value = serde_json::from_slice(&bytes[footer_start..]).unwrap();
    assert_eq!(header["format"], FORMAT);
    assert_eq!(header["restorable_as_archive"], false);
    assert_eq!(footer["type"], "extraction_completion");
    assert_eq!(footer["output_prefix_bytes"], footer_start as u64);
    assert_eq!(footer["output_prefix_sha256"], hex::encode(Sha256::digest(&bytes[..footer_start])));
    let mut input = Cursor::new(&bytes[first_end..footer_start]);
    let mut records = Vec::new();
    while let Some(record) = codec::read_record(&mut input, records.len() as u64 + 1).unwrap() {
        records.push(record);
    }
    (header, footer, records)
}

fn assert_integrity(error: anyhow::Error) {
    assert_eq!(super::super::classify_failure(&error), (5, "logical-archive-integrity", false));
    assert!(!error.to_string().contains("PRIVATE-"));
}

#[test]
fn recovers_all_typed_fields_and_only_the_selected_conversation_for_both_versions() {
    for version in [codec::VERSION, codec::CHUNKED_VERSION] {
        let (source, digest, expected) = fixture(version, false, false);
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("backup.jsonl");
        let destination = root.path().join("conversation.jsonl");
        fs::write(&input, &source).unwrap();
        let receipt = extract_file(&input, &destination, 7, &digest).unwrap();
        let bytes = fs::read(&destination).unwrap();
        let (header, footer, records) = decode_output(&bytes);
        assert_eq!(records, expected);
        assert_eq!(header["source_header"]["schema_version"], version);
        assert_eq!(header["source_content_sha256"], digest);
        assert_eq!(footer["message_rows"], 2);
        assert_eq!(receipt["message_rows"], 2);
        assert_eq!(receipt["output_bytes"], bytes.len() as u64);
        assert_eq!(receipt["output_sha256"], hex::encode(Sha256::digest(&bytes)));
        assert_eq!(receipt["database_opened"], false);
        assert_eq!(receipt["provider_files_opened"], false);
        assert!(!receipt.to_string().contains("PRIVATE-"));
        assert!(!bytes.windows(b"PRIVATE-UNRELATED".len()).any(|s| s == b"PRIVATE-UNRELATED"));
        assert_eq!(fs::read(&input).unwrap(), source);
        // This excerpt cannot be confused with a lossless full-archive backup.
        assert!(codec::verify(&mut Cursor::new(&bytes)).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&destination).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }
}

#[test]
fn giant_message_rows_round_trip_without_truncation_and_keep_the_frame_bound() {
    let (source, digest, expected) = fixture(codec::CHUNKED_VERSION, true, false);
    let mut output = Vec::new();
    let (receipt, _) = scan(&mut BufReader::with_capacity(37, Cursor::new(source)), &mut output, 7, &digest).unwrap();
    assert!(output.windows(b"row_start".len()).any(|s| s == b"row_start"));
    assert!(output.split_inclusive(|b| *b == b'\n').all(|frame| frame.len() <= codec::MAX_RECORD_BYTES));
    assert_eq!(decode_output(&output).2, expected);
    assert_eq!(receipt["message_rows"], 2);
}

#[test]
fn zero_message_conversation_is_valid_but_a_missing_conversation_never_publishes() {
    let (source, digest, expected) = fixture(codec::VERSION, false, true);
    let mut output = Vec::new();
    let (receipt, _) = scan(&mut Cursor::new(&source), &mut output, 7, &digest).unwrap();
    assert_eq!(receipt["message_rows"], 0);
    assert_eq!(decode_output(&output).2, expected);
    let root = tempfile::tempdir().unwrap();
    let input = root.path().join("backup.jsonl");
    let destination = root.path().join("missing.jsonl");
    fs::write(&input, source).unwrap();
    assert!(extract_file(&input, &destination, 99, &digest).is_err());
    assert!(!destination.exists());
}

#[test]
fn wrong_snapshot_and_corruption_after_the_selected_rows_publish_nothing() {
    let (source, digest, _) = fixture(codec::CHUNKED_VERSION, false, false);
    let root = tempfile::tempdir().unwrap();
    let mut cases = vec![source[..source.len() - 1].to_vec()];
    let mut extra = source.clone();
    extra.extend_from_slice(b"{\"type\":\"row\",\"values\":[]}\n");
    cases.push(extra);
    let mut changed = source.clone();
    let at = changed.windows(b"PRIVATE-UNRELATED-MESSAGE".len()).position(|s| s == b"PRIVATE-UNRELATED-MESSAGE").unwrap();
    changed[at] = b'X';
    cases.push(changed);
    for (index, bytes) in cases.iter().enumerate() {
        let input = root.path().join(format!("bad-{index}.jsonl"));
        let output = root.path().join(format!("never-{index}.jsonl"));
        fs::write(&input, bytes).unwrap();
        assert_integrity(extract_file(&input, &output, 7, &digest).unwrap_err());
        assert!(!output.exists());
        assert_eq!(fs::read(&input).unwrap(), *bytes);
    }
    let input = root.path().join("valid.jsonl");
    let output = root.path().join("wrong-snapshot.jsonl");
    fs::write(&input, &source).unwrap();
    assert!(extract_file(&input, &output, 7, &"0".repeat(64)).is_err());
    assert!(!output.exists());
}

#[test]
fn existing_outputs_and_links_are_never_replaced_and_invalid_requests_do_no_io() {
    let root = tempfile::tempdir().unwrap();
    let missing = root.path().join("missing.jsonl");
    let uncreated = root.path().join("never-created").join("output");
    for (id, digest) in [(0, "0".repeat(64)), (-1, "0".repeat(64)), (7, "BAD".into())] {
        let error = extract_file(&missing, &uncreated, id, &digest).unwrap_err();
        assert_eq!(super::super::classify_failure(&error), (2, "logical-archive-usage", false));
        assert!(!root.path().join("never-created").exists());
    }
    let (source, digest, _) = fixture(codec::VERSION, false, false);
    fs::write(&missing, source).unwrap();
    let destination = root.path().join("keep.jsonl");
    fs::write(&destination, b"prior authority").unwrap();
    assert!(extract_file(&missing, &destination, 7, &digest).is_err());
    assert_eq!(fs::read(&destination).unwrap(), b"prior authority");
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let link = root.path().join("link.jsonl");
        symlink(&destination, &link).unwrap();
        assert!(extract_file(&missing, &link, 7, &digest).is_err());
        assert!(extract_file(&link, &uncreated, 7, &digest).is_err());
        assert_eq!(fs::read(&destination).unwrap(), b"prior authority");
    }
}

#[test]
fn staged_output_corruption_is_detected_by_read_back() {
    let root = tempfile::tempdir().unwrap();
    let mut stage = tempfile::NamedTempFile::new_in(root.path()).unwrap();
    let expected = OutputDigest { bytes: 3, sha256: hex::encode(Sha256::digest(b"abc")) };
    stage.write_all(b"abc").unwrap();
    verify_stage(stage.as_file_mut(), &expected).unwrap();
    stage.as_file_mut().rewind().unwrap();
    stage.write_all(b"XYZ").unwrap();
    assert!(verify_stage(stage.as_file_mut(), &expected).is_err());
    stage.as_file_mut().set_len(2).unwrap();
    assert!(verify_stage(stage.as_file_mut(), &expected).is_err());
}

#[test]
fn late_source_io_failure_is_not_reclassified_as_corruption_or_completed_output() {
    struct Fail;
    impl Read for Fail {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "PRIVATE-IO-CAUSE"))
        }
    }
    let (source, digest, _) = fixture(codec::VERSION, false, false);
    let mut output = Vec::new();
    let mut input = BufReader::with_capacity(17, Cursor::new(source).chain(Fail));
    let error = scan(&mut input, &mut output, 7, &digest).unwrap_err();
    assert_eq!(super::super::classify_failure(&error), (14, "logical-archive-io", true));
    assert!(!error.to_string().contains("PRIVATE-IO-CAUSE"));
    assert!(!output.windows(b"extraction_completion".len()).any(|s| s == b"extraction_completion"));
}

#[test]
fn partial_interrupted_and_failed_output_writes_preserve_the_actual_byte_digest() {
    struct ShortOutput { bytes: Vec<u8>, calls: usize, fail: bool }
    impl Write for ShortOutput {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.calls += 1;
            if self.fail {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, "PRIVATE-SINK"));
            }
            if self.calls == 1 { return Err(io::ErrorKind::Interrupted.into()); }
            let count = bytes.len().min(3);
            self.bytes.extend_from_slice(&bytes[..count]);
            Ok(count)
        }
        fn flush(&mut self) -> io::Result<()> { Ok(()) }
    }
    let mut sink = ShortOutput { bytes: vec![], calls: 0, fail: false };
    let mut output = HashedOutput::new(&mut sink);
    output.write_all(b"0123456789").unwrap();
    assert_eq!(output.snapshot(), OutputDigest { bytes: 10, sha256: hex::encode(Sha256::digest(b"0123456789")) });
    drop(output);
    assert_eq!(sink.bytes, b"0123456789");
    sink.fail = true;
    let before = sink.calls;
    let (source, digest, _) = fixture(codec::VERSION, false, false);
    let error = scan(&mut Cursor::new(source), &mut sink, 7, &digest).unwrap_err();
    assert_eq!(super::super::classify_failure(&error), (14, "logical-archive-io", true));
    assert!(!error.to_string().contains("PRIVATE-SINK"));
    assert_eq!(sink.calls, before + 1, "no destructor may retry the failed write");
}
