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

#[test]
fn prepared_output_matches_existing_wire_bytes_across_all_buffer_boundaries() {
    for body in [
        Cell::Text("ordinary message".to_owned()),
        Cell::Text("x".repeat(64 * 1024)),
        Cell::Text("雪\0😀".repeat(64 * 1024)),
        Cell::Text("x".repeat(MAX_RECORD_BYTES)),
        Cell::Blob(STANDARD.encode(vec![0xa5; MAX_RECORD_BYTES])),
    ] {
        let record = row(7, body);
        let mut buffered = Validator::new(header(CHUNKED_VERSION)).unwrap();
        let mut streaming = Validator::new(header(CHUNKED_VERSION)).unwrap();
        buffered.validate(&table()).unwrap();
        streaming.validate(&table()).unwrap();
        let encoded = buffered.push(&record).unwrap();
        let mut expected = Vec::new();
        write_encoded(&encoded, &mut expected).unwrap();
        let prepared = streaming.prepare(&record).unwrap();
        assert_eq!(prepared.bytes, encoded.len());
        assert_eq!(prepared.encoded.is_some(), encoded.len() <= 64 * 1024);
        let mut actual = Vec::new();
        prepared.write_to(&mut actual).unwrap();
        assert_eq!(actual, expected);
        assert_eq!(streaming.completion(), buffered.completion());
        assert_eq!(
            read_record(&mut Cursor::new(&actual), 3).unwrap(),
            Some(record)
        );
        assert!(
            actual
                .split_inclusive(|byte| *byte == b'\n')
                .all(|frame| frame.len() <= MAX_RECORD_BYTES)
        );
    }
}

#[test]
fn prepared_v1_large_row_refusal_does_not_consume_its_identity() {
    let mut validator = Validator::new(header(VERSION)).unwrap();
    validator.validate(&table()).unwrap();
    let oversized = row(7, Cell::Text("x".repeat(MAX_RECORD_BYTES)));
    assert!(validator.prepare(&oversized).is_err());
    assert_eq!(validator.completion().records, 0);
    let record = row(7, Cell::Null);
    let prepared = validator.prepare(&record).unwrap();
    let mut output = Vec::new();
    prepared.write_to(&mut output).unwrap();
    assert_eq!(output, encode(&record).unwrap());
}

#[test]
fn streaming_output_preserves_os_errors_and_does_not_retry_or_flush_on_drop() {
    struct Failing {
        calls: usize,
        flushes: usize,
    }
    impl Write for Failing {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.calls += 1;
            if self.calls >= 2 {
                Err(io::Error::from_raw_os_error(28))
            } else {
                Ok(bytes.len())
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            Ok(())
        }
    }
    // Both direct serialization and the continuation serializer must retain
    // the original OS error through serde_json's intermediate error wrapper.
    for length in [65_536, MAX_RECORD_BYTES] {
        let record = row(7, Cell::Text("x".repeat(length)));
        let mut validator = Validator::new(header(CHUNKED_VERSION)).unwrap();
        validator.validate(&table()).unwrap();
        let prepared = validator.prepare(&record).unwrap();
        let mut output = Failing {
            calls: 0,
            flushes: 0,
        };
        let error = prepared.write_to(&mut output).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(28));
        assert_eq!(output.calls, 2);
        assert_eq!(output.flushes, 0);
    }
}

#[test]
fn streaming_serialization_retries_interruptions_and_finishes_partial_writes() {
    struct Partial {
        bytes: Vec<u8>,
        calls: usize,
    }
    impl Write for Partial {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.calls += 1;
            if matches!(self.calls, 3 | 5) {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let count = bytes.len().min(13);
            self.bytes.extend_from_slice(&bytes[..count]);
            Ok(count)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let record = row(7, Cell::Text("雪\0".repeat(20_000)));
    let expected = encode(&record).unwrap();
    let mut output = Partial {
        bytes: Vec::new(),
        calls: 0,
    };
    serialize_record(&record, expected.len(), &mut output).unwrap();
    assert_eq!(output.bytes, expected);
    assert!(output.calls > 5);
}

#[test]
fn streaming_serialization_preserves_write_zero_and_checks_its_exact_size() {
    struct Zero;
    impl Write for Zero {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Ok(0)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let record = row(7, Cell::Text("message".to_owned()));
    let size = encode(&record).unwrap().len();
    let error = serialize_record(&record, size, &mut Zero).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::WriteZero);
    for wrong_size in [size - 1, size + 1] {
        let error = serialize_record(&record, wrong_size, &mut io::sink()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}
