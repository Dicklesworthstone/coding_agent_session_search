use super::*;
use std::io::Cursor;

fn fixture(version: u32, body: &str, raw: Cell) -> (Vec<u8>, String) {
    let header = Header {
        format: codec::FORMAT.into(),
        schema_version: version,
        archive_id: "extract-fixture".into(),
        exported_at_ms: 42,
        storage_schema_version: "22".into(),
        record_types: codec::record_types(version),
        contains_private_data: true,
        omissions: vec!["derived_search_assets".into()],
    };
    let mut bytes = codec::encode(&Record::Header {
        header: header.clone(),
    })
    .unwrap();
    let mut validator = Validator::new(header).unwrap();
    let table = Table {
        name: "messages".into(),
        // Deliberately not canonical column order: descriptors are authority.
        columns: ["extra_bin", "extra_json", "content", "id"]
            .map(str::to_owned)
            .to_vec(),
        primary_key: vec![3],
    };
    for record in [
        Record::Table { table },
        Record::Row {
            values: vec![
                raw,
                Cell::Text("{ \"private\": \"metadata δ\" }\n".into()),
                Cell::Text(body.into()),
                Cell::Integer(7),
            ],
        },
        Record::Row {
            values: vec![
                Cell::Null,
                Cell::Null,
                Cell::Text("not the selected message".into()),
                Cell::Integer(99),
            ],
        },
    ] {
        validator.prepare(&record).unwrap().write_to(&mut bytes).unwrap();
    }
    let completion = validator.completion();
    let digest = completion.content_sha256.clone();
    bytes.extend(validator.push(&Record::Completion { completion }).unwrap());
    (bytes, digest)
}

fn decode(bytes: &[u8], digest: &str, field: Field) -> (Vec<u8>, Payload) {
    let mut output = Vec::new();
    let (_, _, payload) = scan(&mut Cursor::new(bytes), &mut output, 7, digest, field).unwrap();
    assert_eq!(payload.bytes, output.len() as u64);
    assert_eq!(payload.sha256, hex::encode(Sha256::digest(&output)));
    (output, payload)
}

#[test]
fn complete_text_and_json_are_exact_in_both_versions_without_added_newlines() {
    for version in [codec::VERSION, codec::CHUNKED_VERSION] {
        let text = "PRIVATE-TEXT 雪\0line\r\nlast byte";
        let (bytes, digest) = fixture(version, text, Cell::Null);
        assert_eq!(decode(&bytes, &digest, Field::Content).0, text.as_bytes());
        assert_eq!(
            decode(&bytes, &digest, Field::ExtraJson).0,
            "{ \"private\": \"metadata δ\" }\n".as_bytes()
        );
    }
}

#[test]
fn binary_blocks_preserve_padding_tails_nuls_and_all_byte_values() {
    for length in [0, 1, 2, 3, BLOB_BYTES - 1, BLOB_BYTES, BLOB_BYTES + 1, BLOB_BYTES * 2 + 2] {
        let raw: Vec<u8> = (0..length).map(|index| (index % 256) as u8).collect();
        let (bytes, digest) = fixture(codec::VERSION, "body", Cell::Blob(STANDARD.encode(&raw)));
        assert_eq!(decode(&bytes, &digest, Field::ExtraBin).0, raw);
    }
}

#[test]
fn large_continued_message_is_not_subject_to_the_terminal_view_budget() {
    let text = "雪\0".repeat(codec::MAX_RECORD_BYTES / 4 + 1);
    let (bytes, digest) = fixture(codec::CHUNKED_VERSION, &text, Cell::Null);
    assert!(bytes.windows(b"row_start".len()).any(|window| window == b"row_start"));
    assert_eq!(decode(&bytes, &digest, Field::Content).0, text.as_bytes());
}

#[test]
fn empty_text_is_an_empty_file_not_a_missing_value() {
    let (bytes, digest) = fixture(codec::VERSION, "", Cell::Null);
    let (output, payload) = decode(&bytes, &digest, Field::Content);
    assert!(output.is_empty());
    assert_eq!(payload.bytes, 0);
    assert_eq!(payload.sha256, hex::encode(Sha256::digest(b"")));
}

#[test]
fn null_binary_and_wrong_storage_types_are_never_silently_converted() {
    let (bytes, digest) = fixture(codec::VERSION, "body", Cell::Null);
    let error = scan(&mut Cursor::new(bytes), &mut Vec::new(), 7, &digest, Field::ExtraBin)
        .unwrap_err();
    assert!(error.to_string().contains("NULL"));
    let (bytes, digest) = fixture(codec::VERSION, "body", Cell::Text("PRIVATE-WRONG-TYPE".into()));
    let error = scan(&mut Cursor::new(bytes), &mut Vec::new(), 7, &digest, Field::ExtraBin)
        .unwrap_err();
    assert!(error.to_string().contains("storage type"));
    assert!(!error.to_string().contains("PRIVATE-WRONG-TYPE"));
}

#[test]
fn valid_prefix_with_bad_footer_or_trailing_record_never_publishes() {
    let root = tempfile::tempdir().unwrap();
    let (valid, digest) = fixture(codec::VERSION, "PRIVATE-EXTRACT-ME", Cell::Null);
    let mut trailing = valid.clone();
    trailing.extend(b"{}\n");
    let mut changed = String::from_utf8(valid.clone()).unwrap();
    changed = changed.replace("not the selected message", "changed unselected body");
    for (index, bytes) in [valid[..valid.len() - 1].to_vec(), trailing, changed.into_bytes()]
        .into_iter()
        .enumerate()
    {
        let input = root.path().join(format!("input-{index}.jsonl"));
        fs::write(&input, &bytes).unwrap();
        let output = root.path().join(format!("output-{index}.txt"));
        let error = extract_file(&input, &output, 7, &digest, Field::Content).unwrap_err();
        assert_eq!(super::super::classify_failure(&error), (5, "logical-archive-integrity", false));
        assert!(!error.to_string().contains("PRIVATE-EXTRACT-ME"));
        assert!(!output.exists());
        assert_eq!(fs::read(&input).unwrap(), bytes);
    }
    // No abandoned temporary plaintext files: only inputs and persistent locks.
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 6);
}

#[test]
fn extraction_receipt_matches_private_file_and_leaves_the_backup_unchanged() {
    let root = tempfile::tempdir().unwrap();
    let (bytes, digest) = fixture(codec::CHUNKED_VERSION, "PRIVATE-COMPLETE\0δ", Cell::Null);
    let input = root.path().join("backup.jsonl");
    fs::write(&input, &bytes).unwrap();
    let output = root.path().join("message.txt");
    let receipt = extract_file(&input, &output, 7, &digest, Field::Content).unwrap();
    assert_eq!(fs::read(&output).unwrap(), "PRIVATE-COMPLETE\0δ".as_bytes());
    assert_eq!(receipt["operation"], "extract");
    assert_eq!(receipt["schema_version"], 2);
    assert_eq!(receipt["content_sha256"], digest);
    assert_eq!(receipt["field"], "content");
    assert_eq!(receipt["output_bytes"], fs::metadata(&output).unwrap().len());
    assert_eq!(receipt["output_sha256"], hex::encode(Sha256::digest(fs::read(&output).unwrap())));
    assert_eq!(receipt["database_opened"], false);
    assert_eq!(receipt["provider_files_opened"], false);
    assert_eq!(receipt["database_integrity_checked"], false);
    assert!(!receipt.to_string().contains("PRIVATE-COMPLETE"));
    assert_eq!(fs::read(&input).unwrap(), bytes);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(&output).unwrap().permissions().mode() & 0o077, 0);
    }
    let error = extract_file(&input, &output, 7, &digest, Field::Content).unwrap_err();
    assert!(error.to_string().contains("already exists"));
    assert_eq!(fs::read(output).unwrap(), "PRIVATE-COMPLETE\0δ".as_bytes());
}

#[test]
fn wrong_digest_or_missing_message_cannot_publish_a_substitute() {
    let root = tempfile::tempdir().unwrap();
    let (bytes, digest) = fixture(codec::VERSION, "PRIVATE-COMPLETE", Cell::Null);
    let input = root.path().join("backup.jsonl");
    let output = root.path().join("message.txt");
    fs::write(&input, bytes).unwrap();
    for (id, expected) in [(7, "0".repeat(64)), (8, digest)] {
        let error = extract_file(&input, &output, id, &expected, Field::Content).unwrap_err();
        assert!(!error.to_string().contains("PRIVATE-COMPLETE"));
        assert!(!output.exists());
    }
}

#[test]
fn invalid_request_is_usage_without_filesystem_mutation() {
    let root = tempfile::tempdir().unwrap();
    for (id, digest) in [(0, "0".repeat(64)), (-1, "0".repeat(64)), (7, "bad".into()), (7, "A".repeat(64))] {
        let error = extract_file(
            &root.path().join("missing-input"),
            &root.path().join("missing-output"),
            id,
            &digest,
            Field::Content,
        ).unwrap_err();
        assert_eq!(super::super::classify_failure(&error), (2, "logical-archive-usage", false));
    }
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
}

#[test]
fn original_input_and_existing_output_are_never_overwritten() {
    let root = tempfile::tempdir().unwrap();
    let (bytes, digest) = fixture(codec::VERSION, "body", Cell::Null);
    let input = root.path().join("backup.jsonl");
    fs::write(&input, &bytes).unwrap();
    assert!(extract_file(&input, &input, 7, &digest, Field::Content).is_err());
    assert_eq!(fs::read(&input).unwrap(), bytes);
    let directory = root.path().join("directory");
    fs::create_dir(&directory).unwrap();
    assert!(extract_file(&input, &directory, 7, &digest, Field::Content).is_err());
}

#[cfg(unix)]
#[test]
fn symlink_inputs_destinations_and_locks_are_refused() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let (bytes, digest) = fixture(codec::VERSION, "body", Cell::Null);
    let input = root.path().join("backup.jsonl");
    fs::write(&input, &bytes).unwrap();
    let linked = root.path().join("linked.jsonl");
    symlink(&input, &linked).unwrap();
    let output = root.path().join("message.txt");
    assert!(extract_file(&linked, &output, 7, &digest, Field::Content).is_err());
    symlink(&input, &output).unwrap();
    assert!(extract_file(&input, &output, 7, &digest, Field::Content).is_err());
    let locked_output = root.path().join("locked.txt");
    symlink(&input, root.path().join(".locked.txt.logical-archive.lock")).unwrap();
    assert!(extract_file(&input, &locked_output, 7, &digest, Field::Content).is_err());
    assert_eq!(fs::read(&input).unwrap(), bytes);
}

#[test]
fn payload_io_errors_keep_their_type_without_echoing_private_causes() {
    struct Fails;
    impl Write for Fails {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::StorageFull, "PRIVATE-SINK"))
        }
        fn flush(&mut self) -> io::Result<()> { Ok(()) }
    }
    let error = write_payload(Field::Content, &Cell::Text("PRIVATE-CONTENT".into()), &mut Fails)
        .unwrap_err();
    assert_eq!(super::super::classify_failure(&error), (14, "logical-archive-io", true));
    assert!(!error.to_string().contains("PRIVATE-SINK"));
    assert!(!error.to_string().contains("PRIVATE-CONTENT"));
    assert!(error.downcast_ref::<io::Error>().is_some());
}

#[test]
fn staged_file_readback_rejects_changed_bytes_and_lengths() {
    let mut stage = tempfile::NamedTempFile::new().unwrap();
    let expected = write_payload(Field::Content, &Cell::Text("original".into()), stage.as_file_mut())
        .unwrap();
    verify_payload(stage.as_file_mut(), &expected).unwrap();
    for replacement in [b"different".as_slice(), b"modified", b"short"] {
        stage.as_file_mut().set_len(0).unwrap();
        stage.as_file_mut().rewind().unwrap();
        stage.write_all(replacement).unwrap();
        assert!(verify_payload(stage.as_file_mut(), &expected).is_err());
    }
}
