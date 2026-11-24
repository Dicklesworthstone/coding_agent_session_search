//! GH #559: raw capture must preserve the admitted bytes, not a later live file.
use super::*;
use crate::connectors::codex::StablePrefixReceipt;
use anyhow::Result;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn input<'a>(data: &'a Path, source: &'a Path) -> RawMirrorCaptureInput<'a> {
    RawMirrorCaptureInput {
        data_dir: data,
        provider: "codex",
        source_id: "local",
        origin_kind: "local",
        origin_host: None,
        source_path: source,
        db_links: &[],
    }
}

fn receipt(source: &Path, boundary: usize) -> Result<StablePrefixReceipt> {
    let bytes = fs::read(source)?;
    let metadata = fs::metadata(source)?;
    assert!(boundary > 0 && boundary <= bytes.len());
    assert_eq!(bytes[boundary - 1], b'\n');
    Ok(serde_json::from_value(serde_json::json!({
        "schema_version": 1,
        "source_path": source,
        "device": metadata.dev(),
        "inode": metadata.ino(),
        "admitted_bytes": bytes.len() as u64,
        "committed_bytes": boundary as u64,
        "blake3": blake3::hash(&bytes[..boundary]).to_hex().to_string(),
        "partial": boundary < bytes.len(),
        "message_count": 0,
        "messages_blake3": blake3::hash(b"[]").to_hex().to_string(),
    }))?)
}

#[test]
fn exact_prefix_mirror_preserves_original_identity_and_cannot_poison_full_capture_cache()
-> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path().join("data");
    let source = temp.path().join("rollout-live.jsonl");
    let mut prefix = Vec::new();
    for sequence in 0_u64..8_000 {
        writeln!(
            prefix,
            "{{\"sequence\":{sequence},\"text\":\"{}\"}}",
            blake3::hash(&sequence.to_le_bytes()).to_hex()
        )?;
    }
    assert!(prefix.len() > TRANSCRIPT_CHUNK_BYTES);
    let mut admitted = prefix.clone();
    admitted.extend_from_slice(b"{\"unfinished\":");
    fs::write(&source, &admitted)?;
    let proof = receipt(&source, prefix.len())?;
    OpenOptions::new()
        .append(true)
        .open(&source)?
        .write_all(b"true}\n{\"later\":true}\n")?;
    let full = fs::read(&source)?;
    let captured = capture_codex_prefix(input(&data, &source), &proof)?;
    assert_eq!(captured.source_size_bytes, prefix.len() as u64);
    assert_eq!(captured.source_content_blake3, proof.blake3);
    assert_eq!(super::append_tests::read_capture(&data, &captured), prefix);
    let manifest: serde_json::Value = serde_json::from_slice(&fs::read(
        data.join("raw-mirror/v1")
            .join(&captured.manifest_relative_path),
    )?)?;
    assert_eq!(manifest["original_path"], source.to_string_lossy().as_ref());
    assert_eq!(manifest["source_size_bytes"], prefix.len() as u64);
    assert_eq!(manifest["provider"], "codex");
    let repeated = capture_codex_prefix(input(&data, &source), &proof)?;
    assert_eq!(repeated.manifest_id, captured.manifest_id);
    assert!(repeated.already_present);
    let complete = capture_source_file(input(&data, &source))?;
    assert_ne!(complete.manifest_id, captured.manifest_id);
    assert_eq!(complete.source_size_bytes, full.len() as u64);
    assert_eq!(super::append_tests::read_capture(&data, &complete), full);
    assert_eq!(super::append_tests::read_capture(&data, &captured), prefix);
    let summary = storage_summary(&data);
    assert_eq!(summary.manifest_count, 2);
    assert_eq!(summary.missing_blob_count, 0);
    assert_eq!(summary.invalid_manifest_count, 0);
    assert_eq!(summary.orphan_blob_count, 0);
    Ok(())
}

#[test]
fn prefix_capture_rejects_rewrites_truncation_and_same_bytes_replacement_before_mirroring()
-> Result<()> {
    for mutation in ["rewrite", "append-and-rewrite", "truncate", "replace"] {
        let temp = tempfile::tempdir()?;
        let data = temp.path().join("data");
        let source = temp.path().join("rollout-live.jsonl");
        let original = b"{\"text\":\"original\"}\n{\"second\":true}\n";
        fs::write(&source, original)?;
        let proof = receipt(&source, original.len())?;
        let modified = fs::metadata(&source)?.modified()?;
        match mutation {
            "rewrite" | "append-and-rewrite" => {
                let replacement = b"{\"text\":\"tampered\"}\n{\"second\":true}\n";
                assert_eq!(replacement.len(), original.len());
                fs::write(&source, replacement)?;
                if mutation == "append-and-rewrite" {
                    OpenOptions::new()
                        .append(true)
                        .open(&source)?
                        .write_all(b"{\"later\":true}\n")?;
                }
            }
            "truncate" => OpenOptions::new()
                .write(true)
                .open(&source)?
                .set_len(original.len() as u64 - 1)?,
            "replace" => {
                fs::rename(&source, source.with_extension("retained"))?;
                fs::write(&source, original)?;
            }
            _ => unreachable!(),
        }
        File::open(&source)?.set_modified(modified)?;
        assert!(
            capture_codex_prefix(input(&data, &source), &proof).is_err(),
            "{mutation}"
        );
        assert!(!data.exists(), "refused {mutation} initialized a mirror");
    }
    Ok(())
}

#[test]
fn immutable_view_bytes_must_match_receipt_before_any_blob_is_published() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path().join("data");
    let source = temp.path().join("rollout-source.jsonl");
    let view = temp.path().join("rollout-unrelated.jsonl");
    let original = b"{\"text\":\"original\"}\n";
    fs::write(&source, original)?;
    fs::write(&view, b"{\"text\":\"tampered\"}\n")?;
    let proof = receipt(&source, original.len())?;
    let error = store::policy::capture_codex_prefix(
        input(&data, &source),
        &proof,
        &view,
        TRANSCRIPT_CHUNK_BYTES as u64,
        TRANSCRIPT_CHUNK_BYTES,
        RawMirrorRetentionPolicy {
            max_bytes: u64::MAX,
            min_free_bytes: 0,
        },
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("do not match the parsed source receipt")
    );
    let summary = storage_summary(&data);
    assert_eq!(summary.manifest_count, 0);
    assert_eq!(summary.unique_blob_count, 0);
    assert_eq!(summary.orphan_blob_count, 0);
    Ok(())
}

#[test]
fn source_mutation_after_prefix_copy_is_rejected_before_raw_publication() -> Result<()> {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, mpsc};

    let temp = tempfile::tempdir()?;
    let data = temp.path().join("data");
    let source = temp.path().join("rollout-racing.jsonl");
    let original = b"{\"text\":\"original\"}\n";
    let replacement = b"{\"text\":\"tampered\"}\n";
    assert_eq!(original.len(), replacement.len());
    fs::write(&source, original)?;
    let proof = receipt(&source, original.len())?;
    let modified = fs::metadata(&source)?.modified()?;
    let (request, requested) = mpsc::channel();
    let (finished, completed) = mpsc::channel();
    let writer_path = source.clone();
    let writer = std::thread::spawn(move || {
        requested
            .recv_timeout(Duration::from_secs(5))
            .expect("raw policy began");
        fs::write(&writer_path, replacement).expect("concurrent rewrite");
        File::open(&writer_path)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        finished.send(()).unwrap();
    });
    let completed = Mutex::new(completed);
    let triggered = Arc::new(AtomicBool::new(false));
    let trigger = Arc::clone(&triggered);
    // Raw policy I/O starts only after capture_view has made and verified its
    // immutable copy. Coordinate a real writer at that boundary, rather than
    // relying on a scheduler race or changing the private parser input.
    let scope = observe_raw_mirror_io(
        Arc::new(move || {
            if !trigger.swap(true, Ordering::SeqCst) {
                request.send(()).unwrap();
                completed
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .expect("concurrent writer completed");
            }
        }),
        Arc::new(|| false),
    );
    let result = capture_codex_prefix(input(&data, &source), &proof);
    drop(scope);
    writer.join().expect("writer joined");
    assert!(triggered.load(Ordering::SeqCst));
    let error = result.expect_err("a valid private copy cannot authorize a rewritten source");
    assert_eq!(
        error.downcast_ref::<std::io::Error>().unwrap().kind(),
        std::io::ErrorKind::Interrupted
    );
    let summary = storage_summary(&data);
    assert_eq!(summary.manifest_count, 0);
    assert_eq!(summary.unique_blob_count, 0);
    assert_eq!(fs::read(source)?, replacement);
    Ok(())
}

const CHILD_ROOT: &str = "CASS_PREFIX_MIRROR_TEST_ROOT";
const CHILD_CASE: &str = "CASS_PREFIX_MIRROR_TEST_CASE";
const CHILD_TEST: &str = "raw_mirror::prefix_tests::prefix_capture_policy_child";

#[test]
fn prefix_capture_preserves_off_exclusion_and_budget_policies() -> Result<()> {
    for case in ["off", "excluded", "budget"] {
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("rollout-policy.jsonl");
        fs::write(&source, b"{\"source\":true}\n")?;
        let mut command = Command::new(std::env::current_exe()?);
        command
            .args(["--exact", CHILD_TEST, "--nocapture"])
            .current_dir(temp.path())
            .env(CHILD_ROOT, temp.path())
            .env(CHILD_CASE, case)
            .env("CASS_RAW_MIRROR", "on")
            .env("CASS_RAW_MIRROR_MAX_BYTES", u64::MAX.to_string())
            .env("CASS_RAW_MIRROR_MIN_FREE_BYTES", "0")
            .env_remove("CASS_EXCLUDE_PATHS")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        match case {
            "off" => {
                command.env("CASS_RAW_MIRROR", "off");
            }
            "excluded" => {
                command.env("CASS_EXCLUDE_PATHS", &source);
            }
            "budget" => {
                command.env("CASS_RAW_MIRROR_MAX_BYTES", "0");
            }
            _ => unreachable!(),
        }
        let mut child = command.spawn()?;
        let deadline = Instant::now() + Duration::from_secs(60);
        while child.try_wait()?.is_none() {
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("prefix capture policy regression exceeded 60 seconds");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = child.wait_with_output()?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{case}: {stdout}\n{stderr}");
        assert!(stdout.contains("test result: ok. 1 passed"), "{stdout}");
    }
    Ok(())
}

#[test]
fn prefix_capture_policy_child() -> Result<()> {
    let Ok(root) = dotenvy::var(CHILD_ROOT) else {
        return Ok(());
    };
    let root = PathBuf::from(root);
    let case = dotenvy::var(CHILD_CASE)?;
    let data = root.join("data");
    let source = root.join("rollout-policy.jsonl");
    let bytes = fs::read(&source)?;
    let proof = receipt(&source, bytes.len())?;
    if case != "budget" {
        // Missing source proves these policy refusals happen before capture_view
        // opens or validates the original file, not merely before publication.
        fs::rename(&source, source.with_extension("retained"))?;
    }
    let error = capture_codex_prefix(input(&data, &source), &proof).unwrap_err();
    match case.as_str() {
        "off" => assert!(error.is::<RawMirrorCaptureDisabled>(), "{error:#}"),
        "excluded" => assert!(error.is::<RawMirrorSourceExcluded>(), "{error:#}"),
        "budget" => assert!(error.is::<RawMirrorCapturePaused>(), "{error:#}"),
        _ => anyhow::bail!("unknown prefix policy test case"),
    }
    if case == "budget" {
        let summary = storage_summary(&data);
        assert_eq!(summary.manifest_count, 0);
        assert_eq!(summary.unique_blob_count, 0);
        assert_eq!(fs::read(source)?, bytes);
    } else {
        assert!(!data.exists(), "policy refusal initialized the mirror");
    }
    Ok(())
}

#[test]
fn prefix_capture_cannot_relabel_a_foreign_source() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path().join("data");
    let source = temp.path().join("rollout-source.jsonl");
    let foreign = temp.path().join("rollout-foreign.jsonl");
    let bytes = b"{\"source\":true}\n";
    fs::write(&source, bytes)?;
    fs::write(&foreign, bytes)?;
    let proof = receipt(&source, bytes.len())?;
    assert!(capture_codex_prefix(input(&data, &foreign), &proof).is_err());
    let mut relabeled = input(&data, &source);
    relabeled.provider = "claude_code";
    assert!(capture_codex_prefix(relabeled, &proof).is_err());
    assert!(!data.exists());
    Ok(())
}

#[test]
fn prefix_capture_cannot_copy_a_sensitive_store_under_a_codex_label() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path().join("data");
    let directory = temp.path().join(".zcode");
    fs::create_dir(&directory)?;
    let source = directory.join("rollout-private.jsonl");
    let bytes = b"{\"private_store_setting\":true}\n";
    fs::write(&source, bytes)?;
    let proof = receipt(&source, bytes.len())?;
    // The physical store's ownership must reject before a prefix view tries
    // to reopen the source, even when all of its receipt fields are plausible.
    fs::rename(&source, source.with_extension("retained"))?;
    let error = capture_codex_prefix(input(&data, &source), &proof).unwrap_err();
    assert!(error.to_string().contains("disabled_sensitive_container"));
    assert!(!data.exists());
    Ok(())
}

#[test]
fn append_after_raw_admission_keeps_the_admitted_prefix() -> Result<()> {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    for with_unfinished_tail in [false, true] {
        let temp = tempfile::tempdir()?;
        let data = temp.path().join("archive");
        let source = temp.path().join("rollout-appending.jsonl");
        let complete = b"{\"text\":\"admitted\"}\n";
        let mut original = complete.to_vec();
        if with_unfinished_tail {
            original.extend_from_slice(b"{\"unfinished\":");
        }
        fs::write(&source, &original)?;
        let proof = receipt(&source, complete.len())?;
        assert_eq!(proof.partial, with_unfinished_tail);
        let appended = Arc::new(AtomicBool::new(false));
        let observer = observe_raw_mirror_io(
            Arc::new({
                let source = source.clone();
                let data = data.clone();
                let appended = Arc::clone(&appended);
                move || {
                    // Initialization follows original-source privacy admission.
                    // Exercise the same lock-wait window as the rotation guard.
                    if data.is_dir() && !appended.swap(true, Ordering::SeqCst) {
                        let suffix = if with_unfinished_tail {
                            &b"true}\n{\"later\":true}\n"[..]
                        } else {
                            &b"{\"later\":true}\n"[..]
                        };
                        OpenOptions::new()
                            .append(true)
                            .open(&source)
                            .unwrap()
                            .write_all(suffix)
                            .unwrap();
                    }
                }
            }),
            Arc::new(|| false),
        );
        let captured = capture_codex_prefix(input(&data, &source), &proof)?;
        drop(observer);
        assert!(appended.load(Ordering::SeqCst));
        assert!(fs::metadata(&source)?.len() > proof.admitted_bytes);
        assert_eq!(captured.source_size_bytes, proof.committed_bytes);
        assert_eq!(captured.source_content_blake3, proof.blake3);
        assert_eq!(super::append_tests::read_capture(&data, &captured), complete);
        let full = capture_source_file(input(&data, &source))?;
        assert_ne!(full.manifest_id, captured.manifest_id);
        assert_eq!(super::append_tests::read_capture(&data, &full), fs::read(&source)?);
        assert_eq!(storage_summary(&data).manifest_count, 2);
    }
    Ok(())
}
