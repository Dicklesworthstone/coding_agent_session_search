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
