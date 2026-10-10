use super::*;
use std::io::{BufReader, Cursor, Read};

fn header() -> Header {
    Header {
        format: FORMAT.to_owned(),
        schema_version: VERSION,
        archive_id: "test-archive".to_owned(),
        exported_at_ms: 42,
        storage_schema_version: "9".to_owned(),
        record_types: ["table", "row", "completion"].map(str::to_owned).to_vec(),
        contains_private_data: true,
        omissions: vec!["derived_search_assets".to_owned()],
    }
}

fn table() -> Table {
    Table {
        name: "messages".to_owned(),
        columns: ["id", "body"].map(str::to_owned).to_vec(),
        primary_key: vec![0],
    }
}

fn archive(header: Header, values: Vec<Cell>) -> Vec<u8> {
    let mut bytes = encode(&Record::Header {
        header: header.clone(),
    })
    .unwrap();
    let mut validator = Validator::new(header).unwrap();
    bytes.extend(validator.push(&Record::Table { table: table() }).unwrap());
    bytes.extend(validator.push(&Record::Row { values }).unwrap());
    let completion = validator.completion();
    bytes.extend(validator.push(&Record::Completion { completion }).unwrap());
    bytes
}

fn valid() -> Vec<u8> {
    archive(
        header(),
        vec![Cell::Integer(7), Cell::Text("private\n雪".to_owned())],
    )
}

#[test]
fn complete_archive_round_trips_and_preserves_unicode() {
    let bytes = valid();
    let (got, completion) = verify(&mut Cursor::new(&bytes)).unwrap();
    assert_eq!(got, header());
    assert_eq!(completion.records, 1);
    assert_eq!(completion.tables["messages"], 1);
    assert_eq!(completion.content_sha256.len(), 64);
    let mut reader = Cursor::new(bytes);
    read_record(&mut reader, 1).unwrap();
    read_record(&mut reader, 2).unwrap();
    assert!(
        matches!(read_record(&mut reader, 3).unwrap(), Some(Record::Row { values })
        if values == vec![Cell::Integer(7), Cell::Text("private\n雪".to_owned())])
    );
}

#[test]
fn digest_is_timestamp_independent_but_binds_identity_and_content() {
    let digest = |bytes| verify(&mut Cursor::new(bytes)).unwrap().1.content_sha256;
    let original = digest(valid());
    let mut later = header();
    later.exported_at_ms = i64::MAX;
    let values = vec![Cell::Integer(7), Cell::Text("private\n雪".to_owned())];
    assert_eq!(original, digest(archive(later, values.clone())));
    let mut foreign = header();
    foreign.archive_id = "other".to_owned();
    assert_ne!(original, digest(archive(foreign, values)));
    assert_ne!(
        original,
        digest(archive(header(), vec![Cell::Integer(7), Cell::Null]))
    );
}

#[test]
fn digest_is_independent_of_input_json_whitespace_and_key_order() {
    let original = valid();
    let reordered = original
        .split_inclusive(|b| *b == b'\n')
        .flat_map(|line| {
            let value: serde_json::Value = serde_json::from_slice(line).unwrap();
            let mut bytes = serde_json::to_vec(&value).unwrap();
            bytes.push(b'\n');
            bytes
        })
        .collect::<Vec<_>>();
    assert_eq!(
        verify(&mut Cursor::new(original)).unwrap(),
        verify(&mut Cursor::new(reordered)).unwrap()
    );
}

#[test]
fn every_truncated_prefix_is_rejected() {
    let bytes = valid();
    for end in 0..bytes.len() {
        assert!(
            verify(&mut Cursor::new(&bytes[..end])).is_err(),
            "accepted prefix {end}"
        );
    }
}

#[test]
fn completion_counts_digest_and_trailing_records_are_checked() {
    for field in ["records", "tables", "content_sha256"] {
        let mut records = valid()
            .split_inclusive(|b| *b == b'\n')
            .map(|line| serde_json::from_slice::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        let completion = &mut records.last_mut().unwrap()["completion"];
        match field {
            "records" => completion[field] = 2.into(),
            "tables" => completion[field]["messages"] = 2.into(),
            _ => completion[field] = "0".repeat(64).into(),
        }
        let bytes = records
            .into_iter()
            .flat_map(|record| {
                let mut bytes = serde_json::to_vec(&record).unwrap();
                bytes.push(b'\n');
                bytes
            })
            .collect::<Vec<_>>();
        assert!(verify(&mut Cursor::new(bytes)).is_err());
    }
    let mut bytes = valid();
    bytes.extend(encode(&Record::Row { values: vec![] }).unwrap());
    assert!(verify(&mut Cursor::new(bytes)).is_err());
}

#[test]
fn unsupported_header_is_rejected_before_rows() {
    let mut unknown = header();
    unknown.schema_version += 1;
    assert!(Validator::new(unknown).is_err());
    let mut redacted = header();
    redacted.contains_private_data = false;
    assert!(Validator::new(redacted).is_err());
    let mut path_identity = header();
    path_identity.archive_id = "../somewhere".to_owned();
    assert!(Validator::new(path_identity).is_err());
}

#[test]
fn duplicate_and_out_of_order_identities_are_rejected() {
    for id in [6, 7] {
        let mut validator = Validator::new(header()).unwrap();
        validator.push(&Record::Table { table: table() }).unwrap();
        validator
            .push(&Record::Row {
                values: vec![Cell::Integer(7), Cell::Null],
            })
            .unwrap();
        assert!(
            validator
                .push(&Record::Row {
                    values: vec![Cell::Integer(id), Cell::Null]
                })
                .is_err()
        );
    }
    let mut validator = Validator::new(header()).unwrap();
    validator.push(&Record::Table { table: table() }).unwrap();
    assert!(validator.push(&Record::Table { table: table() }).is_err());
}

#[test]
fn exact_record_boundary_includes_the_newline() {
    let overhead = encode(&Record::Row {
        values: vec![Cell::Text(String::new())],
    })
    .unwrap()
    .len();
    let at_limit = Record::Row {
        values: vec![Cell::Text("x".repeat(MAX_RECORD_BYTES - overhead))],
    };
    let bytes = encode(&at_limit).unwrap();
    assert_eq!(bytes.len(), MAX_RECORD_BYTES);
    assert_eq!(
        read_record(&mut Cursor::new(bytes), 1).unwrap(),
        Some(at_limit)
    );
    let over_limit = Record::Row {
        values: vec![Cell::Text("x".repeat(MAX_RECORD_BYTES - overhead + 1))],
    };
    assert!(encode(&over_limit).is_err());
}

#[test]
fn an_unterminated_oversized_stream_is_stopped_before_unbounded_reading() {
    let mut reader = BufReader::with_capacity(1024, std::io::repeat(b'x').take(u64::MAX));
    assert!(
        read_record(&mut reader, 99)
            .unwrap_err()
            .to_string()
            .contains("99")
    );
}

#[test]
fn malformed_errors_do_not_echo_private_bodies() {
    // ubs:ignore[rust.security.hardcoded-secrets] -- Synthetic payload verifies diagnostic redaction.
    let secret = "PRIVATE_SESSION_SECRET";
    let bytes = format!("{{\"type\":\"{secret}\"}}\n");
    let error = read_record(&mut Cursor::new(bytes), 12)
        .unwrap_err()
        .to_string();
    assert!(error.contains("12"));
    assert!(!error.contains(secret));
    assert!(read_record(&mut Cursor::new(b"\xff\n"), 1).is_err());
    assert!(read_record(&mut Cursor::new(b"\n"), 1).is_err());
}

#[test]
fn failed_read_after_valid_records_reports_io_and_position_without_private_details() {
    struct FailedRead {
        prefix: Cursor<Vec<u8>>,
    }

    impl Read for FailedRead {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            let count = self.prefix.read(bytes)?;
            if count == 0 {
                Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "PRIVATE_READ_ERROR_DETAIL",
                ))
            } else {
                Ok(count)
            }
        }
    }

    // Header, table and one complete private row have already verified when
    // the underlying reader fails before the completion. This is not evidence
    // of an invalid archive, and the reader's arbitrary message is not public.
    let prefix = valid()
        .split_inclusive(|byte| *byte == b'\n')
        .take(3)
        .flatten()
        .copied()
        .collect();
    let mut input = BufReader::with_capacity(
        64,
        FailedRead {
            prefix: Cursor::new(prefix),
        },
    );
    let error = verify(&mut input).unwrap_err();
    let message = error.to_string();
    assert!(message.contains("record 4"), "{message}");
    assert!(message.contains("UnexpectedEof"), "{message}");
    assert!(!message.contains("PRIVATE_READ_ERROR_DETAIL"));
    assert!(!message.contains("private"));
    assert_eq!(
        super::super::classify_failure(&error),
        (14, "logical-archive-io", true)
    );
    assert!(error.downcast_ref::<std::io::Error>().is_some());
}

#[test]
fn interrupted_reads_resume_and_verify_the_complete_archive() {
    struct InterruptedOnce {
        bytes: Cursor<Vec<u8>>,
        interrupted: bool,
    }

    impl Read for InterruptedOnce {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            if !self.interrupted {
                self.interrupted = true;
                return Err(std::io::ErrorKind::Interrupted.into());
            }
            // Exercise records spanning many read buffers as well as EINTR.
            let take = bytes.len().min(7);
            self.bytes.read(&mut bytes[..take])
        }
    }

    let expected = verify(&mut Cursor::new(valid())).unwrap();
    let input = InterruptedOnce {
        bytes: Cursor::new(valid()),
        interrupted: false,
    };
    assert_eq!(verify(&mut BufReader::new(input)).unwrap(), expected);
}

#[test]
fn numeric_and_blob_encodings_are_lossless_and_strict() {
    for value in [0.0_f64, -0.0, f64::MIN_POSITIVE, f64::MAX, -1.25] {
        let cell = Cell::Real(format!("{:016x}", value.to_bits()));
        cell.validate().unwrap();
        let roundtrip: Cell = serde_json::from_slice(&serde_json::to_vec(&cell).unwrap()).unwrap();
        assert_eq!(cell, roundtrip);
    }
    for invalid in [
        "NaN",
        "7ff0000000000000",
        "7ff8000000000000",
        "ABCDEF0000000000",
    ] {
        assert!(Cell::Real(invalid.to_owned()).validate().is_err());
    }
    assert!(Cell::Blob("AAH/".to_owned()).validate().is_ok());
    assert!(Cell::Blob("%%%".to_owned()).validate().is_err());
}

#[test]
fn typed_cells_preserve_scalar_values_and_allow_the_kind_after_the_value() {
    let fixtures = [
        (r#"{"kind":"null"}"#, Cell::Null),
        (r#"{"kind":"null","value":null}"#, Cell::Null),
        (r#"{"value":null,"kind":"null"}"#, Cell::Null),
        (
            r#"{"value":-9223372036854775808,"kind":"integer"}"#,
            Cell::Integer(i64::MIN),
        ),
        (
            r#"{"kind":"integer","value":9223372036854775807}"#,
            Cell::Integer(i64::MAX),
        ),
        (r#"{"value":0,"kind":"integer"}"#, Cell::Integer(0)),
        (
            r#"{"value":"8000000000000000","kind":"real"}"#,
            Cell::Real("8000000000000000".to_owned()),
        ),
        (
            r#"{"value":"雪\u0000\"","kind":"text"}"#,
            Cell::Text("雪\0\"".to_owned()),
        ),
        (
            r#"{"value":"AP+A","kind":"blob"}"#,
            Cell::Blob("AP+A".to_owned()),
        ),
    ];
    for (wire, expected) in fixtures {
        assert_eq!(serde_json::from_str::<Cell>(wire).unwrap(), expected);
        assert_eq!(
            serde_json::from_reader::<_, Cell>(Cursor::new(wire)).unwrap(),
            expected
        );
    }
}

#[test]
fn typed_cells_reject_nested_values_overflow_floats_and_duplicate_fields() {
    for wire in [
        r#"{"kind":"integer","value":9223372036854775808}"#,
        r#"{"value":-9223372036854775809,"kind":"integer"}"#,
        r#"{"kind":"integer","value":18446744073709551616}"#,
        r#"{"value":1.0,"kind":"integer"}"#,
        r#"{"value":1e0,"kind":"integer"}"#,
        r#"{"value":"7","kind":"integer"}"#,
        r#"{"value":7,"kind":"text"}"#,
        r#"{"value":true,"kind":"integer"}"#,
        r#"{"value":[],"kind":"text"}"#,
        r#"{"value":{},"kind":"blob"}"#,
        r#"{"value":null,"kind":"text"}"#,
        r#"{"value":0,"kind":"null"}"#,
        r#"{"kind":"text"}"#,
        r#"{"value":"PRIVATE_VALUE"}"#,
        r#"{"kind":"null","kind":"null"}"#,
        r#"{"kind":"null","value":null,"value":null}"#,
        r#"{"kind":"null","PRIVATE_FIELD":null}"#,
        r#"{"kind":{"null":null}}"#,
        r#"{"kind":["null"]}"#,
        r#"{"kind":0}"#,
        r#"{"kind":null}"#,
    ] {
        assert!(serde_json::from_str::<Cell>(wire).is_err());
        let record = format!("{{\"type\":\"row\",\"values\":[{wire}]}}\n");
        let error = read_record(&mut Cursor::new(record), 23).unwrap_err();
        assert_eq!(
            super::super::classify_failure(&error),
            (5, "logical-archive-integrity", false)
        );
        assert!(error.to_string().contains("23"));
        assert!(!error.to_string().contains("PRIVATE_"));
    }
}

#[test]
fn typed_records_accept_payload_before_type_for_every_record_kind() {
    let records = [
        ("header", "header", Record::Header { header: header() }),
        ("table", "table", Record::Table { table: table() }),
        (
            "row",
            "values",
            Record::Row {
                values: vec![Cell::Integer(7), Cell::Text("雪\0".to_owned())],
            },
        ),
        (
            "completion",
            "completion",
            Record::Completion {
                completion: Completion {
                    records: 0,
                    tables: BTreeMap::new(),
                    content_sha256: "0".repeat(64),
                },
            },
        ),
    ];
    for (kind, field, expected) in records {
        let encoded = serde_json::to_value(&expected).unwrap();
        let payload = &encoded[field];
        let reordered = format!("{{\"{field}\":{payload},\"type\":\"{kind}\"}}\n");
        assert_eq!(
            read_record(&mut Cursor::new(reordered), 1).unwrap(),
            Some(expected.clone())
        );
        assert_eq!(
            read_record(&mut Cursor::new(encode(&expected).unwrap()), 1).unwrap(),
            Some(expected)
        );
    }
}

#[test]
fn typed_records_reject_unknown_duplicate_missing_and_conflicting_fields() {
    for wire in [
        r#"{"type":"row","values":[],"values":[]}"#,
        r#"{"type":"row","type":"row","values":[]}"#,
        r#"{"values":[],"type":"row","type":"row"}"#,
        r#"{"type":"row","values":[],"header":null}"#,
        r#"{"type":"table","values":[]}"#,
        r#"{"values":[],"type":"table"}"#,
        r#"{"values":[]}"#,
        r#"{"type":"row"}"#,
        r#"{"type":"row","values":null}"#,
        r#"{"type":"row","values":[],"PRIVATE_FIELD":[]}"#,
        r#"{"type":{"row":null},"values":[]}"#,
        r#"{"values":[],"type":["row"]}"#,
        r#"{"type":2,"values":[]}"#,
        r#"{"type":null,"values":[]}"#,
    ] {
        assert!(serde_json::from_str::<Record>(wire).is_err());
        let error = read_record(&mut Cursor::new(format!("{wire}\n")), 31).unwrap_err();
        assert_eq!(
            super::super::classify_failure(&error),
            (5, "logical-archive-integrity", false)
        );
        assert!(!error.to_string().contains("PRIVATE_"));
    }
}

#[test]
fn row_cell_limit_is_enforced_during_decoding_at_the_exact_boundary() {
    let at_limit = Record::Row {
        values: vec![Cell::Null; MAX_COLUMNS],
    };
    assert_eq!(
        read_record(&mut Cursor::new(encode(&at_limit).unwrap()), 1).unwrap(),
        Some(at_limit)
    );
    let oversized = Record::Row {
        values: vec![Cell::Null; MAX_COLUMNS + 1],
    };
    assert!(read_record(&mut Cursor::new(encode(&oversized).unwrap()), 1).is_err());
}

#[test]
fn invalid_descriptors_and_row_shapes_fail_closed() {
    let mut descriptor = table();
    descriptor.name = "messages; DROP TABLE meta".to_owned();
    assert!(descriptor.validate().is_err());
    descriptor = table();
    descriptor.columns[1] = "id".to_owned();
    assert!(descriptor.validate().is_err());
    descriptor = table();
    descriptor.primary_key = vec![100];
    assert!(descriptor.validate().is_err());
    let mut validator = Validator::new(header()).unwrap();
    assert!(validator.push(&Record::Row { values: vec![] }).is_err());
    validator.push(&Record::Table { table: table() }).unwrap();
    assert!(
        validator
            .push(&Record::Row {
                values: vec![Cell::Integer(1)]
            })
            .is_err()
    );
    assert!(
        validator
            .push(&Record::Row {
                values: vec![Cell::Null, Cell::Null]
            })
            .is_err()
    );
}

#[test]
fn digest_matches_independent_sha256_wire_fixture() {
    let mut fixture = header();
    fixture.archive_id = "oracle".to_owned();
    fixture.exported_at_ms = 0;
    let bytes = archive(
        fixture,
        vec![Cell::Integer(7), Cell::Text("hello".to_owned())],
    );
    let (_, completion) = verify(&mut Cursor::new(bytes)).unwrap();
    assert_eq!(
        completion.content_sha256,
        "9b661100473f2d708f17f7d77abf4f80261c77b4c9066f545545d9fc9ccf03d2"
    );
}
