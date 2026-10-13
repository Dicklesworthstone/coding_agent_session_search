use super::*;
use serde_json::Value;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

fn input<'a>(data: &'a Path, source: &'a Path) -> RawMirrorCaptureInput<'a> {
    RawMirrorCaptureInput {
        data_dir: data,
        provider: "claude_code",
        source_id: "local",
        origin_kind: "local",
        origin_host: None,
        source_path: source,
        db_links: &[],
    }
}

// Different, valid JSONL records prevent repeated fixture bytes from making
// whole-file or chunk deduplication appear better than it is on real logs.
fn lines(start: usize, count: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    for sequence in start..start + count {
        let text = blake3::hash(&sequence.to_le_bytes()).to_hex();
        writeln!(bytes, "{{\"sequence\":{sequence},\"text\":\"{text}\"}}")
            .expect("fixture JSONL");
    }
    bytes
}

fn read_capture(data: &Path, record: &RawMirrorCaptureRecord) -> Vec<u8> {
    let root = data.join("raw-mirror/v1");
    let manifest: Value = serde_json::from_slice(
        &fs::read(root.join(&record.manifest_relative_path)).expect("manifest"),
    )
    .expect("manifest JSON");
    let content = match manifest.get("content_storage") {
        Some(storage) if !storage.is_null() => {
            let descriptor = fs::read(root.join(&record.blob_relative_path)).unwrap();
            assert_eq!(blake3::hash(&descriptor).to_hex().as_str(), record.blob_blake3);
            assert_eq!(serde_json::from_slice::<Value>(&descriptor).unwrap(), *storage);
            let mut content = Vec::new();
            for chunk in storage["chunks"].as_array().unwrap() {
                let bytes = fs::read(root.join(chunk["blob_relative_path"].as_str().unwrap()))
                    .expect("chunk must survive subsequent captures");
                assert_eq!(bytes.len() as u64, chunk["blob_size_bytes"].as_u64().unwrap());
                assert_eq!(
                    blake3::hash(&bytes).to_hex().as_str(),
                    chunk["blob_blake3"].as_str().unwrap()
                );
                content.extend(bytes);
            }
            content
        }
        _ => fs::read(root.join(&record.blob_relative_path)).unwrap(),
    };
    assert_eq!(content.len() as u64, record.source_size_bytes);
    assert_eq!(blake3::hash(&content).to_hex().as_str(), record.source_content_blake3);
    content
}

#[test]
fn gh522_frequent_appends_bound_tail_amplification_and_preserve_every_capture() {
    // Exercise both sides of the old 8-MiB whole-blob threshold.
    for initial_lines in [35_000, 105_000] {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let source = dir.path().join("active.jsonl");
        let mut content = lines(0, initial_lines);
        fs::write(&source, &content).unwrap();
        let mut captures = Vec::new();
        let mut descriptor_bytes = 0_u64;
        for cycle in 0..32 {
            if cycle > 0 {
                let appended = lines(initial_lines + (cycle - 1) * 101, 101);
                OpenOptions::new().append(true).open(&source).unwrap()
                    .write_all(&appended).unwrap();
                content.extend(appended);
            }
            let capture = capture_source_file(input(&data, &source)).unwrap();
            assert_eq!(capture.storage_kind, "fixed_chunks_v1");
            descriptor_bytes += capture.blob_size_bytes;
            let unchanged = capture_source_file(input(&data, &source)).unwrap();
            assert_eq!(unchanged.manifest_id, capture.manifest_id);
            assert!(unchanged.already_present);
            captures.push(capture);
        }
        let summary = storage_summary(&data);
        assert_eq!(summary.manifest_count, 32);
        assert_eq!(summary.orphan_blob_count, 0);
        assert_eq!(summary.invalid_manifest_count, 0);
        assert_eq!(summary.missing_blob_count, 0);
        // One final transcript, at most one obsolete partial tail per prior
        // capture, and the measured descriptor overhead -- not N transcripts.
        assert!(
            summary.total_blob_bytes <= content.len() as u64
                + 31 * TRANSCRIPT_CHUNK_BYTES as u64 + descriptor_bytes,
            "unexpected amplification: {summary:?}"
        );
        for capture in captures {
            assert_eq!(
                read_capture(&data, &capture),
                content[..capture.source_size_bytes as usize]
            );
        }
    }
}

#[test]
fn gh522_prefix_rewrite_is_not_mistaken_for_an_append() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let source = dir.path().join("session.NDJSON");
    let first = lines(0, 12_000);
    fs::write(&source, &first).unwrap();
    let before = capture_source_file(input(&data, &source)).unwrap();
    let mut rewritten = lines(90_000, 12_000);
    rewritten.extend(lines(200_000, 100));
    fs::write(&source, &rewritten).unwrap();
    let after = capture_source_file(input(&data, &source)).unwrap();
    assert_ne!(before.source_content_blake3, after.source_content_blake3);
    assert_eq!(read_capture(&data, &before), first);
    assert_eq!(read_capture(&data, &after), rewritten);
}

#[test]
fn gh522_non_transcript_policy_and_old_whole_captures_remain_readable() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let source = dir.path().join("session.jsonl");
    let content = lines(0, 12_000);
    fs::write(&source, &content).unwrap();
    let legacy = store::capture_source_file(input(&data, &source)).unwrap();
    assert_eq!(legacy.storage_kind, "whole_blob_v1");
    let chunked = capture_source_file(input(&data, &source)).unwrap();
    assert_eq!(chunked.storage_kind, "fixed_chunks_v1");
    assert_eq!(read_capture(&data, &legacy), content);
    assert_eq!(read_capture(&data, &chunked), content);
    let container = dir.path().join("session.json");
    fs::write(&container, &content).unwrap();
    assert_eq!(capture_source_file(input(&data, &container)).unwrap().storage_kind, "whole_blob_v1");
}
