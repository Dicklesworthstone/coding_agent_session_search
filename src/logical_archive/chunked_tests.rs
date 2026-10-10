use super::super::{self as codec, Cell, Completion, Header, Table, Validator};
use super::*;
use std::io::Cursor;
use std::sync::OnceLock;

fn row_bytes() -> &'static [u8] {
    static BYTES: OnceLock<Vec<u8>> = OnceLock::new();
    BYTES.get_or_init(|| {
        // JSON escapes the NUL and carries UTF-8 verbatim. Chunk boundaries
        // deliberately fall inside both UTF-8 and escaped JSON sequences.
        let row = Record::Row {
            values: vec![
                Cell::Integer(7),
                Cell::Text(format!(
                    "PRIVATE-ROW{}",
                    "雪\0".repeat(MAX_RECORD_BYTES / 9 + 1)
                )),
                Cell::Blob(STANDARD.encode([0_u8, 255, 128].repeat(1024))),
            ],
        };
        codec::encode_with_limit(&row, MAX_ROW_BYTES).unwrap()
    })
}

fn wire() -> Vec<u8> {
    let mut output = Vec::new();
    write(row_bytes(), &mut output).unwrap();
    output
}

fn header(version: u32) -> Header {
    Header {
        format: codec::FORMAT.to_owned(),
        schema_version: version,
        archive_id: "chunk-proof".to_owned(),
        exported_at_ms: 42,
        storage_schema_version: "9".to_owned(),
        record_types: codec::record_types(version),
        contains_private_data: true,
        omissions: vec!["derived_search_assets".to_owned()],
    }
}

fn table() -> Table {
    Table {
        name: "messages".to_owned(),
        columns: ["id", "body", "raw"].map(str::to_owned).to_vec(),
        primary_key: vec![0],
    }
}

fn archive(version: u32, row: &Record) -> (Vec<u8>, Completion) {
    let header = header(version);
    let mut output = codec::encode(&Record::Header {
        header: header.clone(),
    })
    .unwrap();
    let mut validator = Validator::new(header).unwrap();
    for record in [Record::Table { table: table() }, row.clone()] {
        codec::write_encoded(&validator.push(&record).unwrap(), &mut output).unwrap();
    }
    let completion = validator.completion();
    output.extend(
        validator
            .push(&Record::Completion {
                completion: completion.clone(),
            })
            .unwrap(),
    );
    (output, completion)
}

fn assert_integrity(bytes: &[u8]) {
    let error = codec::read_record(&mut Cursor::new(bytes), 17).unwrap_err();
    assert_eq!(
        super::super::super::classify_failure(&error),
        (5, "logical-archive-integrity", false)
    );
    assert!(error.to_string().contains("17"));
    assert!(!error.to_string().contains("PRIVATE-ROW"));
}

#[test]
fn continuation_frames_keep_the_v2_wire_names() {
    let fixtures = [
        (
            Frame::Start { bytes: 8_388_609 },
            b"{\"type\":\"row_start\",\"bytes\":8388609}\n".as_slice(),
        ),
        (
            Frame::Chunk {
                sequence: 0,
                data: "AA==".to_owned(),
            },
            b"{\"type\":\"row_chunk\",\"sequence\":0,\"data\":\"AA==\"}\n".as_slice(),
        ),
        (
            Frame::End {
                sha256: "0".repeat(64),
            },
            b"{\"type\":\"row_end\",\"sha256\":\"0000000000000000000000000000000000000000000000000000000000000000\"}\n".as_slice(),
        ),
    ];
    for (record, expected) in fixtures {
        let mut encoded = Vec::new();
        write_frame(&record, &mut encoded).unwrap();
        assert_eq!(encoded, expected);
        let decoded = frame(expected, 1).unwrap();
        let mut roundtrip = Vec::new();
        write_frame(&decoded, &mut roundtrip).unwrap();
        assert_eq!(roundtrip, expected);
        // The independent object ordering places bytes/data/sha256 before type.
        let reordered =
            serde_json::to_vec(&serde_json::from_slice::<serde_json::Value>(expected).unwrap())
                .unwrap();
        let mut reordered_roundtrip = Vec::new();
        write_frame(&frame(&reordered, 1).unwrap(), &mut reordered_roundtrip).unwrap();
        assert_eq!(reordered_roundtrip, expected);
    }
}

#[test]
fn chunked_unicode_and_blob_are_byte_exact_and_each_frame_is_bounded() {
    let bytes = wire();
    let frames: Vec<_> = bytes.split_inclusive(|byte| *byte == b'\n').collect();
    assert!(frames.len() > 10);
    assert!(frames.iter().all(|frame| frame.len() <= MAX_RECORD_BYTES));
    let mut reader = BufReader::with_capacity(7, Cursor::new(bytes));
    let actual = codec::read_record(&mut reader, 1).unwrap().unwrap();
    let expected: Record = serde_json::from_slice(row_bytes()).unwrap();
    assert_eq!(actual, expected);
    assert!(codec::read_record(&mut reader, 2).unwrap().is_none());
}

#[test]
fn continuation_group_is_one_logical_row_and_leaves_the_next_record_unread() {
    let row: Record = serde_json::from_slice(row_bytes()).unwrap();
    let (bytes, expected) = archive(codec::CHUNKED_VERSION, &row);
    let (header, completion) = codec::verify(&mut Cursor::new(&bytes)).unwrap();
    assert_eq!(header.schema_version, codec::CHUNKED_VERSION);
    assert_eq!(completion, expected);
    assert_eq!(completion.records, 1);
    assert_eq!(completion.tables["messages"], 1);
    let mut reader = Cursor::new(&bytes);
    for position in 1..=3 {
        assert!(codec::read_record(&mut reader, position).unwrap().is_some());
    }
    assert!(matches!(
        codec::read_record(&mut reader, 4).unwrap(),
        Some(Record::Completion { .. })
    ));
    assert!(codec::read_record(&mut reader, 5).unwrap().is_none());
}

#[test]
fn v1_and_v2_preserve_the_same_small_row_content_digest() {
    let row = Record::Row {
        values: vec![
            Cell::Integer(7),
            Cell::Text("legacy 雪".to_owned()),
            Cell::Null,
        ],
    };
    let (old, old_completion) = archive(codec::VERSION, &row);
    let (new, new_completion) = archive(codec::CHUNKED_VERSION, &row);
    assert_eq!(old_completion, new_completion);
    assert_eq!(
        codec::verify(&mut Cursor::new(old)).unwrap().1,
        old_completion
    );
    assert_eq!(
        codec::verify(&mut Cursor::new(new)).unwrap().1,
        new_completion
    );
}

#[test]
fn v1_header_cannot_authorize_a_continued_row() {
    let row: Record = serde_json::from_slice(row_bytes()).unwrap();
    let (bytes, _) = archive(codec::CHUNKED_VERSION, &row);
    let header_end = bytes.iter().position(|byte| *byte == b'\n').unwrap() + 1;
    let mut downgraded = codec::encode(&Record::Header {
        header: header(codec::VERSION),
    })
    .unwrap();
    downgraded.extend_from_slice(&bytes[header_end..]);
    let error = codec::verify(&mut Cursor::new(downgraded)).unwrap_err();
    assert_eq!(
        super::super::super::classify_failure(&error),
        (5, "logical-archive-integrity", false)
    );
    assert!(error.to_string().contains("8 MiB"));
}

#[test]
fn unsupported_versions_and_inconsistent_record_type_lists_are_refused() {
    let mut unknown = header(3);
    assert!(Validator::new(unknown.clone()).is_err());
    unknown.schema_version = codec::CHUNKED_VERSION;
    assert!(Validator::new(unknown).is_err()); // v1 names do not admit v2.
    let mut downgrade = header(codec::CHUNKED_VERSION);
    downgrade.schema_version = codec::VERSION;
    assert!(Validator::new(downgrade).is_err());
}

#[test]
fn declared_row_limit_is_checked_before_reading_chunks() {
    for bytes in [0, MAX_RECORD_BYTES, MAX_ROW_BYTES + 1, usize::MAX] {
        let mut input = Vec::new();
        write_frame(&Frame::Start { bytes }, &mut input).unwrap();
        assert_integrity(&input);
    }
}

struct ForbiddenTail<'a>(&'a std::cell::Cell<bool>);

impl Read for ForbiddenTail<'_> {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        self.0.set(true);
        Err(io::Error::new(
            io::ErrorKind::ConnectionReset,
            "PRIVATE_UNREAD_TAIL",
        ))
    }
}

fn assert_group_rejected_before_next_chunk(prefix: &str) {
    // The stream claims 256 MiB, but the malformed shape is decisive in the
    // first chunk. A parser buffering arbitrary JSON would request the tail
    // and report I/O instead; no huge fixture or process-memory guess is needed.
    let mut chunk = prefix.as_bytes().to_vec();
    assert!(chunk.len() < CHUNK_BYTES);
    chunk.resize(CHUNK_BYTES, b' ');
    let mut wire = Vec::new();
    write_frame(
        &Frame::Start {
            bytes: MAX_ROW_BYTES,
        },
        &mut wire,
    )
    .unwrap();
    write_frame(
        &Frame::Chunk {
            sequence: 0,
            data: STANDARD.encode(chunk),
        },
        &mut wire,
    )
    .unwrap();
    let tail_read = std::cell::Cell::new(false);
    let stream = Cursor::new(wire).chain(ForbiddenTail(&tail_read));
    let error = codec::read_record(&mut BufReader::with_capacity(37, stream), 41).unwrap_err();
    assert_eq!(
        super::super::super::classify_failure(&error),
        (5, "logical-archive-integrity", false)
    );
    assert!(!tail_read.get());
    assert!(!error.to_string().contains("PRIVATE_"));
    assert!(error.to_string().contains("41"));
}

#[test]
fn frames_reject_untyped_values_and_unknown_fields_before_reading_the_tail() {
    for prefix in [
        r#"{"data":["#,
        r#"{"bytes":{"#,
        r#"{"sequence":["#,
        r#"{"sha256":["#,
        r#"{"PRIVATE_FIELD":["#,
        r#"{"values":["#,
    ] {
        let tail_read = std::cell::Cell::new(false);
        let input = Cursor::new(prefix).chain(ForbiddenTail(&tail_read));
        let error = serde_json::from_reader::<_, Frame>(input).err().unwrap();
        assert!(!error.is_io());
        assert!(!tail_read.get());
    }
}

#[test]
fn frames_reject_null_duplicate_and_cross_kind_fields() {
    for wire in [
        r#"{"type":"row_start","bytes":8388609,"data":null}"#,
        r#"{"type":"row_start","bytes":null}"#,
        r#"{"type":"row_start","bytes":8388609,"bytes":8388609}"#,
        r#"{"type":"row_start","bytes":8388609,"type":"row_start"}"#,
        r#"{"type":"row_chunk","sequence":0,"data":"AA==","sha256":"PRIVATE_HASH"}"#,
        r#"{"type":"row_chunk","data":"AA=="}"#,
        r#"{"type":"row_chunk","sequence":0,"data":null}"#,
        r#"{"type":"row_chunk","sequence":-1,"data":"AA=="}"#,
        r#"{"type":"row_chunk","sequence":0.0,"data":"AA=="}"#,
        r#"{"type":"row_end","sha256":"PRIVATE_HASH","sequence":0}"#,
        r#"{"type":"row_end","sha256":null}"#,
        r#"{"type":{"row_start":null},"bytes":8388609}"#,
        r#"{"bytes":8388609,"type":["row_start"]}"#,
        r#"{"type":0,"bytes":8388609}"#,
        r#"{"type":null,"bytes":8388609}"#,
    ] {
        let error = frame(wire.as_bytes(), 47).err().unwrap();
        assert_eq!(
            super::super::super::classify_failure(&error),
            (5, "logical-archive-integrity", false)
        );
        assert!(!error.to_string().contains("PRIVATE_"));
        assert!(error.to_string().contains("47"));
    }
}

#[test]
fn continued_rows_reject_excess_cells_before_reading_the_rest_of_the_group() {
    for start in [r#"{"type":"row","values":["#, r#"{"values":["#] {
        let prefix = format!(
            "{start}{}{{\"kind\":\"text\",\"value\":\"PRIVATE_CELL",
            r#"{"kind":"null"},"#.repeat(codec::MAX_COLUMNS)
        );
        assert_group_rejected_before_next_chunk(&prefix);
    }
}

#[test]
fn continued_cells_refuse_untyped_nested_payloads_before_buffering_them() {
    for prefix in [
        r#"{"type":"row","values":[{"value":["#,
        r#"{"values":[{"value":{"PRIVATE_KEY":"#,
        r#"{"type":"row","values":[{"kind":"text","value":["#,
    ] {
        assert_group_rejected_before_next_chunk(prefix);
    }
}

#[test]
fn continued_rows_refuse_non_row_and_duplicate_payloads_before_reading_them() {
    for prefix in [
        r#"{"type":"header","header":{"record_types":["#,
        r#"{"header":{"record_types":["#,
        r#"{"table":{"columns":["#,
        r#"{"completion":{"tables":{"#,
        r#"{"type":"row","values":[],"values":["#,
        r#"{"values":[],"PRIVATE_FIELD":["#,
    ] {
        assert_group_rejected_before_next_chunk(prefix);
    }
}

#[test]
fn omitted_reordered_and_duplicate_chunks_are_rejected() {
    let bytes = wire();
    let original: Vec<Vec<u8>> = bytes
        .split_inclusive(|byte| *byte == b'\n')
        .map(<[u8]>::to_vec)
        .collect();
    let mut omitted = original.clone();
    omitted.remove(2);
    assert_integrity(&omitted.concat());
    let mut reordered = original.clone();
    reordered.swap(1, 2);
    assert_integrity(&reordered.concat());
    let mut duplicated = original;
    duplicated.insert(2, duplicated[1].clone());
    assert_integrity(&duplicated.concat());
}

#[test]
fn every_frame_boundary_truncation_and_missing_final_newline_are_rejected() {
    let bytes = wire();
    let mut end = 0;
    for frame in bytes.split_inclusive(|byte| *byte == b'\n') {
        end += frame.len();
        if end < bytes.len() {
            assert_integrity(&bytes[..end]);
        }
    }
    assert_integrity(&bytes[..bytes.len() - 1]);
}

#[test]
fn checksum_tampering_and_invalid_base64_are_rejected_without_echoing_payloads() {
    let bytes = wire();
    let mut frames: Vec<Vec<u8>> = bytes
        .split_inclusive(|byte| *byte == b'\n')
        .map(<[u8]>::to_vec)
        .collect();
    let last = frames.len() - 1;
    frames[last].clear();
    write_frame(
        &Frame::End {
            sha256: "0".repeat(64),
        },
        &mut frames[last],
    )
    .unwrap();
    assert_integrity(&frames.concat());
    let Frame::Chunk { sequence, mut data } = frame(&frames[1], 1).unwrap() else {
        panic!("first continuation must be a chunk");
    };
    data.replace_range(..1, "%");
    frames[1].clear();
    write_frame(&Frame::Chunk { sequence, data }, &mut frames[1]).unwrap();
    assert_integrity(&frames.concat());
}

#[test]
fn empty_short_and_extra_chunks_are_rejected() {
    let bytes = wire();
    let original: Vec<Vec<u8>> = bytes
        .split_inclusive(|byte| *byte == b'\n')
        .map(<[u8]>::to_vec)
        .collect();
    for data in [String::new(), STANDARD.encode(b"short")] {
        let mut frames = original.clone();
        frames[1].clear();
        write_frame(&Frame::Chunk { sequence: 0, data }, &mut frames[1]).unwrap();
        assert_integrity(&frames.concat());
    }
    let mut frames = original;
    frames.insert(frames.len() - 1, frames[1].clone());
    assert_integrity(&frames.concat());
}

#[test]
fn whitespace_padding_cannot_smuggle_a_small_row_through_v1() {
    let mut padded = codec::encode(&Record::Row {
        values: vec![Cell::Integer(7)],
    })
    .unwrap();
    padded.resize(MAX_RECORD_BYTES + 1, b' ');
    let mut input = Vec::new();
    write(&padded, &mut input).unwrap();
    assert_integrity(&input);
}

#[test]
fn a_valid_group_checksum_does_not_replace_the_archive_completion_digest() {
    let row: Record = serde_json::from_slice(row_bytes()).unwrap();
    let (bytes, _) = archive(codec::CHUNKED_VERSION, &row);
    let mut frames: Vec<Vec<u8>> = bytes
        .split_inclusive(|byte| *byte == b'\n')
        .map(<[u8]>::to_vec)
        .collect();
    let mut changed = row_bytes().to_vec();
    let at = changed
        .windows(b"PRIVATE-ROW".len())
        .position(|window| window == b"PRIVATE-ROW")
        .unwrap();
    changed[at] = b'X';
    let mut group = Vec::new();
    write(&changed, &mut group).unwrap(); // Its per-row checksum is correct.
    let completion = frames.pop().unwrap();
    let mut tampered = frames[0].clone();
    tampered.extend_from_slice(&frames[1]);
    tampered.extend(group);
    tampered.extend(completion); // But the archive's original digest is not.
    assert!(codec::verify(&mut Cursor::new(tampered)).is_err());
}

#[test]
fn continued_input_io_failure_keeps_its_retryable_class() {
    struct Fails;
    impl Read for Fails {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "PRIVATE-READ-DETAIL",
            ))
        }
    }
    let bytes = wire();
    let input = Cursor::new(&bytes[..bytes.len() / 2]).chain(Fails);
    let mut reader = BufReader::with_capacity(13, input);
    let error = codec::read_record(&mut reader, 17).unwrap_err();
    assert_eq!(
        super::super::super::classify_failure(&error),
        (14, "logical-archive-io", true)
    );
    assert!(!error.to_string().contains("PRIVATE-READ-DETAIL"));
}

#[test]
fn a_failed_frame_write_stops_before_row_end() {
    struct FailsAfterStart {
        calls: usize,
    }
    impl Write for FailsAfterStart {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.calls += 1;
            if self.calls > 1 {
                Err(io::Error::new(io::ErrorKind::WriteZero, "private sink"))
            } else {
                Ok(bytes.len())
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut output = FailsAfterStart { calls: 0 };
    let error = write(row_bytes(), &mut output).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::WriteZero);
    assert_eq!(output.calls, 2);
}

#[test]
fn streaming_encoder_has_fixed_chunk_storage_and_matches_existing_frames() {
    let mut output = Vec::new();
    let mut encoder = Encoder {
        output: &mut output,
        remaining: row_bytes().len(),
        sequence: 0,
        buffer: Vec::with_capacity(CHUNK_BYTES),
        digest: Sha256::new(),
    };
    let capacity = encoder.buffer.capacity();
    for bytes in row_bytes().chunks(7919) {
        encoder.write_all(bytes).unwrap();
        assert!(encoder.buffer.len() < CHUNK_BYTES);
        assert_eq!(encoder.buffer.capacity(), capacity);
    }
    encoder.finish().unwrap();
    let expected = wire();
    let after_start = expected.iter().position(|byte| *byte == b'\n').unwrap() + 1;
    assert_eq!(output, expected[after_start..]);
}

#[test]
fn streaming_size_mismatch_never_emits_a_valid_row_end() {
    let record: Record = serde_json::from_slice(row_bytes()).unwrap();
    for size in [row_bytes().len() - 1, row_bytes().len() + 1] {
        let mut output = Vec::new();
        assert!(write_record(&record, size, &mut output).is_err());
        assert!(
            !output
                .windows(b"\"row_end\"".len())
                .any(|window| window == b"\"row_end\"")
        );
        assert_integrity(&output);
    }
}
