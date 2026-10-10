use super::*;
use std::io::Cursor;

fn header(version: u32) -> Header {
    Header {
        format: FORMAT.to_owned(),
        schema_version: version,
        archive_id: "oracle".to_owned(),
        exported_at_ms: 0,
        storage_schema_version: "9".to_owned(),
        record_types: record_types(version),
        contains_private_data: true,
        omissions: vec!["derived_search_assets".to_owned()],
    }
}

fn table() -> Record {
    Record::Table {
        table: Table {
            name: "messages".to_owned(),
            columns: vec!["id".to_owned(), "body".to_owned()],
            primary_key: vec![0],
        },
    }
}

fn row(id: i64, body: Cell) -> Record {
    Record::Row {
        values: vec![Cell::Integer(id), body],
    }
}

#[test]
fn streaming_validation_matches_the_independent_v1_digest_oracle() {
    for version in [VERSION, CHUNKED_VERSION] {
        let mut validator = Validator::new(header(version)).unwrap();
        validator.validate(&table()).unwrap();
        validator
            .validate(&row(7, Cell::Text("hello".to_owned())))
            .unwrap();
        let completion = validator.completion();
        assert_eq!(
            completion.content_sha256,
            "9b661100473f2d708f17f7d77abf4f80261c77b4c9066f545545d9fc9ccf03d2"
        );
        validator
            .validate(&Record::Completion {
                completion: completion.clone(),
            })
            .unwrap();
        assert_eq!(validator.finish().unwrap().1, completion);
    }
}

#[test]
fn streaming_and_materialized_validation_have_identical_sizes_and_receipts() {
    for version in [VERSION, CHUNKED_VERSION] {
        let mut streaming = Validator::new(header(version)).unwrap();
        let mut materialized = Validator::new(header(version)).unwrap();
        let mut wire = encode(&Record::Header {
            header: header(version),
        })
        .unwrap();
        for record in [
            table(),
            row(i64::MIN, Cell::Null),
            row(-1, Cell::Real(format!("{:016x}", (-0.0_f64).to_bits()))),
            row(7, Cell::Text("雪\0\"\\\n😀".repeat(20_000))),
            row(i64::MAX, Cell::Blob(STANDARD.encode(vec![0xa5; 25_000]))),
        ] {
            let size = streaming.validate(&record).unwrap();
            let bytes = materialized.push(&record).unwrap();
            assert_eq!(size, bytes.len());
            assert_eq!(streaming.completion(), materialized.completion());
            write_encoded(&bytes, &mut wire).unwrap();
        }
        let end = Record::Completion {
            completion: streaming.completion(),
        };
        let size = streaming.validate(&end).unwrap();
        let bytes = materialized.push(&end).unwrap();
        assert_eq!(size, bytes.len());
        wire.extend(bytes);
        let expected = streaming.finish().unwrap();
        assert_eq!(materialized.finish().unwrap(), expected);
        assert_eq!(verify(&mut Cursor::new(wire)).unwrap(), expected);
    }
}

#[test]
fn canonical_hash_handles_fragmentation_bulk_writes_and_flushes() {
    let input: Vec<u8> = (0..100_003).map(|value| (value % 251) as u8).collect();
    let expected = Sha256::digest(&input);
    for width in [1, 7, 8191, 8192, 8193, 65_536, input.len()] {
        let mut hash = CanonicalHash::new(Sha256::new(), input.len());
        for (ordinal, bytes) in input.chunks(width).enumerate() {
            hash.write_all(bytes).unwrap();
            if ordinal % 3 == 0 {
                hash.flush().unwrap();
            }
        }
        let (digest, count) = hash.finish();
        assert_eq!(count, input.len());
        assert_eq!(digest.finalize(), expected);
    }
    // The hash accumulator has no row-sized output allocation.
    assert!(std::mem::size_of::<CanonicalHash>() < 9 * 1024);
}

#[test]
fn canonical_hash_rejects_bytes_before_changing_count_or_digest() {
    let mut hash = CanonicalHash::new(Sha256::new(), 3);
    hash.write_all(b"ab").unwrap();
    assert!(hash.write_all(b"cd").is_err());
    hash.write_all(b"c").unwrap();
    let (digest, count) = hash.finish();
    assert_eq!(count, 3);
    assert_eq!(digest.finalize(), Sha256::digest(b"abc"));
}

#[test]
fn streaming_limit_counts_escaping_and_newline_without_advancing_on_failure() {
    let overhead = encode(&row(7, Cell::Text(String::new()))).unwrap().len();
    let exact = row(7, Cell::Text("x".repeat(MAX_RECORD_BYTES - overhead)));
    let mut validator = Validator::new(header(VERSION)).unwrap();
    validator.validate(&table()).unwrap();
    assert_eq!(validator.validate(&exact).unwrap(), MAX_RECORD_BYTES);

    let mut validator = Validator::new(header(VERSION)).unwrap();
    validator.validate(&table()).unwrap();
    let before = validator.completion();
    let oversized = row(7, Cell::Text("\0".repeat(MAX_RECORD_BYTES / 6)));
    assert!(validator.validate(&oversized).is_err());
    assert_eq!(validator.completion(), before);
    // A failed attempt did not consume this primary key.
    validator.validate(&row(7, Cell::Null)).unwrap();
}

#[test]
fn streaming_v2_accepts_large_rows_and_v1_still_refuses_them() {
    let record = row(7, Cell::Text("雪\0".repeat(MAX_RECORD_BYTES / 9 + 1)));
    let bytes = encode_with_limit(&record, MAX_ROW_BYTES).unwrap();
    assert!(bytes.len() > MAX_RECORD_BYTES);
    let mut large = Validator::new(header(CHUNKED_VERSION)).unwrap();
    let mut legacy = Validator::new(header(VERSION)).unwrap();
    large.validate(&table()).unwrap();
    legacy.validate(&table()).unwrap();
    assert_eq!(large.validate(&record).unwrap(), bytes.len());
    assert!(legacy.validate(&record).is_err());
    assert_eq!(legacy.completion().records, 0);
}

#[test]
fn streaming_rejections_leave_identity_counts_and_digest_unchanged() {
    let mut validator = Validator::new(header(CHUNKED_VERSION)).unwrap();
    validator.validate(&table()).unwrap();
    validator.validate(&row(7, Cell::Null)).unwrap();
    let before = validator.completion();
    for invalid in [
        row(7, Cell::Null),
        row(6, Cell::Null),
        row(8, Cell::Blob("PRIVATE-NOT-BASE64".to_owned())),
        Record::Row { values: vec![] },
        table(),
        Record::Completion {
            completion: Completion {
                records: 999,
                ..before.clone()
            },
        },
    ] {
        assert!(validator.validate(&invalid).is_err());
        assert_eq!(validator.completion(), before);
    }
    validator.validate(&row(8, Cell::Null)).unwrap();
    let complete = validator.completion();
    validator
        .validate(&Record::Completion {
            completion: complete,
        })
        .unwrap();
    assert!(validator.validate(&row(9, Cell::Null)).is_err());
}

#[test]
fn row_count_overflow_does_not_partially_advance_the_validator() {
    let mut validator = Validator::new(header(VERSION)).unwrap();
    validator.validate(&table()).unwrap();
    validator.counts.insert("messages".to_owned(), u64::MAX);
    let before = validator.completion();
    assert!(validator.validate(&row(7, Cell::Null)).is_err());
    assert_eq!(validator.completion(), before);
    assert!(validator.last_key.is_none());
}

#[test]
fn bounded_blob_validation_agrees_with_the_full_standard_decoder() {
    let check = |encoded: String| {
        let expected = STANDARD.decode(&encoded).is_ok();
        assert_eq!(Cell::Blob(encoded).validate().is_ok(), expected);
    };
    for size in [0, 1, 2, 3, 4, 3071, 3072, 3073, 6143, 6144, 6145, 20_000] {
        let encoded = STANDARD.encode(vec![0xff; size]);
        check(encoded.clone());
        for suffix in ["=", "A", "AA==", "\n", "%"] {
            check(format!("{encoded}{suffix}"));
        }
        for position in [0, 1, 4094, 4095, 4096, encoded.len().saturating_sub(1)] {
            if position < encoded.len() {
                for replacement in ["=", "%", " "] {
                    let mut changed = encoded.clone();
                    changed.replace_range(position..position + 1, replacement);
                    check(changed);
                }
            }
        }
    }
    for invalid in ["AB==", "AAB=", "AA", "A", "AA=A", "AA==AAAA", "雪"] {
        check(invalid.to_owned());
    }
}
