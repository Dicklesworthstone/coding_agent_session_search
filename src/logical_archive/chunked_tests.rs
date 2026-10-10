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
        write_frame(&Frame::RowStart { bytes }, &mut input).unwrap();
        assert_integrity(&input);
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
        &Frame::RowEnd {
            sha256: "0".repeat(64),
        },
        &mut frames[last],
    )
    .unwrap();
    assert_integrity(&frames.concat());
    let Frame::RowChunk { sequence, mut data } = frame(&frames[1], 1).unwrap() else {
        panic!("first continuation must be a chunk");
    };
    data.replace_range(..1, "%");
    frames[1].clear();
    write_frame(&Frame::RowChunk { sequence, data }, &mut frames[1]).unwrap();
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
        write_frame(&Frame::RowChunk { sequence: 0, data }, &mut frames[1]).unwrap();
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
