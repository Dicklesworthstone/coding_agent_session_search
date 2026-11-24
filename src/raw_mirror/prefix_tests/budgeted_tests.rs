//! A prefix raw capture must use only budgeted staging. A regular file in
//! TMPDIR makes any second full-prefix tempfile fail deterministically.

use super::*;

const NO_TEMP_ROOT: &str = "CASS_PREFIX_NO_TEMP_TEST_ROOT";
const NO_TEMP_CASE: &str = "CASS_PREFIX_NO_TEMP_TEST_CASE";

fn run_without_global_temp(
    root: &Path,
    case: &str,
    max_bytes: u64,
    min_free_bytes: u64,
) -> Result<()> {
    let (_, module) = module_path!()
        .split_once("::")
        .ok_or_else(|| anyhow::anyhow!("prefix budget test has no module prefix"))?;
    let child_test = format!("{module}::prefix_without_global_temp_child");
    let mut child = Command::new(std::env::current_exe()?)
        .args(["--exact", &child_test, "--nocapture", "--test-threads=1"])
        .env(NO_TEMP_ROOT, root)
        .env(NO_TEMP_CASE, case)
        .env("TMPDIR", root.join("global-temp-is-a-file"))
        .env("CASS_RAW_MIRROR", "on")
        .env("CASS_RAW_MIRROR_MAX_BYTES", max_bytes.to_string())
        .env("CASS_RAW_MIRROR_MIN_FREE_BYTES", min_free_bytes.to_string())
        .env_remove("CASS_EXCLUDE_PATHS")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(60);
    while child.try_wait()?.is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("prefix budget child exceeded 60 seconds");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{case}: {stdout}\n{stderr}");
    assert!(
        stdout.contains(&format!("test {child_test} ... ok")),
        "{stdout}"
    );
    assert!(stdout.contains("test result: ok. 1 passed"), "{stdout}");
    assert_eq!(
        fs::read(root.join(format!("finished-{case}")))?,
        case.as_bytes(),
        "a zero-test child cannot satisfy this regression"
    );
    Ok(())
}

#[test]
fn prefix_capture_pauses_or_reuses_chunks_without_a_second_full_temp_copy() -> Result<()> {
    for case in ["retention", "free-space", "allowed"] {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        let source = root.join("rollout-no-temp.jsonl");
        let mut prefix = Vec::new();
        for fill in [b'a', b'b'] {
            let header = b"{\"text\":\"";
            let suffix = b"\"}\n";
            prefix.extend_from_slice(header);
            prefix.extend(std::iter::repeat_n(
                fill,
                TRANSCRIPT_CHUNK_BYTES - header.len() - suffix.len(),
            ));
            prefix.extend_from_slice(suffix);
        }
        assert_eq!(prefix.len(), TRANSCRIPT_CHUNK_BYTES * 2);
        let mut original = prefix.clone();
        original.extend_from_slice(b"{\"pending\":");
        fs::write(&source, &original)?;
        let proof = receipt(&source, prefix.len())?;
        fs::write(root.join("receipt.json"), serde_json::to_vec(&proof)?)?;
        fs::write(root.join("expected-prefix"), &prefix)?;
        fs::write(root.join("global-temp-is-a-file"), b"not a directory")?;

        match case {
            "retention" => run_without_global_temp(root, case, 0, 0)?,
            "free-space" => run_without_global_temp(root, case, u64::MAX, u64::MAX)?,
            "allowed" => {
                run_without_global_temp(root, case, u64::MAX, 0)?;
                // Every content-addressed chunk and the manifest already
                // exist. Read-only reuse needs no new retention/free-space
                // reservation, even after another append to the live source.
                run_without_global_temp(root, "reuse", 0, u64::MAX)?;
            }
            _ => unreachable!(),
        }
    }
    Ok(())
}

#[test]
fn prefix_without_global_temp_child() -> Result<()> {
    let Ok(root) = dotenvy::var(NO_TEMP_ROOT) else {
        return Ok(());
    };
    let root = PathBuf::from(root);
    let case = dotenvy::var(NO_TEMP_CASE)?;
    assert_eq!(std::env::temp_dir(), root.join("global-temp-is-a-file"));
    assert!(fs::metadata(std::env::temp_dir())?.is_file());

    // The parent made a real object/hash receipt before launching with an
    // unusable TMPDIR. Setup does not invoke tempfile or prefix capture.
    let source = root.join("rollout-no-temp.jsonl");
    let proof: StablePrefixReceipt = serde_json::from_slice(&fs::read(root.join("receipt.json"))?)?;
    let expected = fs::read(root.join("expected-prefix"))?;
    OpenOptions::new()
        .append(true)
        .open(&source)?
        .write_all(b"true}\n{\"later\":true}\n")?;
    let before = fs::read(&source)?;
    let data = root.join("data");
    let result = capture_codex_prefix(input(&data, &source), &proof);
    if matches!(case.as_str(), "retention" | "free-space") {
        let error = result.expect_err("a new raw capture must respect its admission policy");
        assert!(error.is::<RawMirrorCapturePaused>(), "{case}: {error:#}");
        let summary = storage_summary(&data);
        assert_eq!(summary.manifest_count, 0);
        assert_eq!(summary.unique_blob_count, 0);
        assert_eq!(summary.orphan_blob_count, 0);
    } else {
        assert!(matches!(case.as_str(), "allowed" | "reuse"));
        let captured = result?;
        assert_eq!(captured.source_size_bytes, proof.committed_bytes);
        assert_eq!(captured.source_content_blake3, proof.blake3);
        assert_eq!(
            super::super::append_tests::read_capture(&data, &captured),
            expected
        );
        assert_eq!(captured.already_present, case == "reuse");
        assert_eq!(captured.chunk_count, 2);
        let summary = storage_summary(&data);
        assert_eq!(summary.manifest_count, 1);
        assert_eq!(summary.missing_blob_count, 0);
        assert_eq!(summary.invalid_manifest_count, 0);
        assert_eq!(summary.orphan_blob_count, 0);
    }
    assert_eq!(fs::read(&source)?, before);
    let staging = data.join("raw-mirror/v1/tmp");
    if staging.exists() {
        assert_eq!(fs::read_dir(staging)?.count(), 0);
    }
    fs::write(root.join(format!("finished-{case}")), case.as_bytes())?;
    Ok(())
}
