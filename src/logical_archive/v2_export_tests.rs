use super::*;

fn fixture(body: &str) -> Connection {
    let connection = Connection::open(":memory:").unwrap();
    connection
        .execute_batch(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO meta VALUES ('schema_version', '9');
             CREATE TABLE messages (id INTEGER PRIMARY KEY, body TEXT, raw BLOB);",
        )
        .unwrap();
    connection
        .execute_with_params(
            "INSERT INTO messages VALUES (7, ?1, ?2)",
            &[
                SqliteValue::Text(body.to_owned().into()),
                SqliteValue::Blob(vec![0_u8, 255, 128].into()),
            ],
        )
        .unwrap();
    connection
}

#[test]
fn v2_exports_json_expansion_that_v1_refuses_without_shortening_the_row() -> Result<()> {
    let body = format!(
        "PRIVATE-CONTINUED-ROW{}",
        "\0".repeat(codec::MAX_RECORD_BYTES / 6 + 1)
    );
    assert!(body.len() < codec::MAX_RECORD_BYTES);
    let connection = fixture(&body);
    assert!(
        snapshot_for_version(
            &connection,
            "bounded-export".into(),
            codec::VERSION,
            &mut io::sink(),
        )
        .is_err()
    );
    let mut bytes = Vec::new();
    let receipt = snapshot_for_version(
        &connection,
        "bounded-export".into(),
        codec::CHUNKED_VERSION,
        &mut bytes,
    )?;
    assert_eq!(receipt.0.schema_version, 2);
    assert_eq!(receipt.1.tables["messages"], 1);
    assert_eq!(codec::verify(&mut bytes.as_slice())?, receipt);
    assert!(
        bytes
            .split_inclusive(|byte| *byte == b'\n')
            .all(|frame| frame.len() <= codec::MAX_RECORD_BYTES)
    );
    let mut input = bytes.as_slice();
    let mut found = false;
    let mut position = 1;
    while let Some(record) = codec::read_record(&mut input, position)? {
        if let Record::Row { values } = &record
            && values.first() == Some(&Cell::Integer(7))
        {
            assert_eq!(values[1], Cell::Text(body.clone()));
            assert_eq!(values[2], Cell::Blob("AP+A".into()));
            assert!(
                codec::encode(&record).is_err(),
                "the v1 frame limit must stay unchanged"
            );
            found = true;
        }
        position += 1;
    }
    assert!(found, "an oversized canonical row must not disappear");
    connection.close()?;
    Ok(())
}

#[test]
fn v2_failed_chunk_output_keeps_io_class_and_does_not_retry_on_drop() -> Result<()> {
    struct Fails {
        writes: usize,
    }
    impl Write for Fails {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.writes += 1;
            if self.writes > 1 {
                Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "PRIVATE-SINK-DETAIL",
                ))
            } else {
                Ok(bytes.len())
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let connection = fixture(&"x".repeat(codec::MAX_RECORD_BYTES + 1));
    let mut output = Fails { writes: 0 };
    let error = buffered_snapshot_for_version(
        &connection,
        "failed-chunk".into(),
        codec::CHUNKED_VERSION,
        &mut output,
    )
    .expect_err("a failed chunk write must not produce a completion receipt");
    assert_eq!(
        super::super::classify_failure(&error),
        (14, "logical-archive-io", true)
    );
    assert_eq!(
        output.writes, 2,
        "Drop must not silently retry the failed write"
    );
    assert!(!error.to_string().contains("PRIVATE-SINK-DETAIL"));
    connection.close()?;
    Ok(())
}

#[test]
fn unsupported_export_format_is_refused_before_creating_files() {
    let root = tempfile::tempdir().unwrap();
    let output = root.path().join("must-not-exist.jsonl");
    let error = export_file_for_version(
        &root.path().join("missing.db"),
        &output,
        "invalid-format".into(),
        3,
    )
    .unwrap_err();
    assert_eq!(
        super::super::classify_failure(&error),
        (2, "logical-archive-usage", false)
    );
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
}
