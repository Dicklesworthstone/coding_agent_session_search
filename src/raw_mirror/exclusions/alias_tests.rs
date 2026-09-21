//! Fresh-process regressions through real discovery, parsing and capture APIs.

use super::RawMirrorSourceExcluded;
use crate::connectors::codex::CodexConnector;
use crate::raw_mirror::{
    RawMirrorCaptureInput, capture_source_file, capture_source_file_with_chunk_policy,
};
use franken_agent_detection::{Connector, ScanContext, ScanRoot};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const CHILD_ROOT: &str = "CASS_EXCLUSION_ALIAS_TEST_ROOT";
const CHILD_MODE: &str = "CASS_EXCLUSION_ALIAS_TEST_MODE";
const CHILD_TEST: &str = "raw_mirror::exclusions::alias_tests::child";

fn fixture() -> anyhow::Result<tempfile::TempDir> {
    let root = tempfile::tempdir()?;
    for (directory, id) in [("private", 1), ("private-copy", 2)] {
        let source = root.path().join(format!(".codex/sessions/{directory}/rollout-{id}.jsonl"));
        fs::create_dir_all(source.parent().unwrap())?;
        fs::write(
            source,
            format!(
                "{{\"timestamp\":\"2026-01-01T00:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"00000000-0000-0000-0000-{id:012}\",\"cwd\":\"/fixture\"}}}}\n{{\"timestamp\":\"2026-01-01T00:00:01Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\"content\":[{{\"type\":\"input_text\",\"text\":\"alias policy fixture {id}\"}}]}}}}\n"
            ),
        )?;
    }
    Ok(root)
}

fn run_child(root: &Path, mode: &str, exclusions: &OsStr) -> anyhow::Result<()> {
    let mut child = Command::new(std::env::current_exe()?)
        .args(["--exact", CHILD_TEST, "--nocapture"])
        .current_dir(root)
        .env(CHILD_ROOT, root)
        .env(CHILD_MODE, mode)
        .env("CODEX_HOME", root.join(".codex"))
        .env("CASS_EXCLUDE_PATHS", exclusions)
        .env("RUST_LOG", "off")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(60);
    while child.try_wait()?.is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("source exclusion regression exceeded 60 seconds: {mode}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output()?;
    assert!(output.stdout.len() < 1024 * 1024 && output.stderr.len() < 1024 * 1024);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{mode}: {stdout}\n{stderr}");
    assert!(stdout.contains("test result: ok. 1 passed"), "{stdout}");
    Ok(())
}

#[test]
fn relative_policy_blocks_absolute_discovery_and_capture() -> anyhow::Result<()> {
    let root = fixture()?;
    run_child(root.path(), "absolute-source", OsStr::new(".codex/sessions/private"))
}

#[test]
fn absolute_policy_blocks_relative_source_capture() -> anyhow::Result<()> {
    let root = fixture()?;
    run_child(root.path(), "relative-source", root.path().join(".codex/sessions/private").as_os_str())
}

#[test]
fn parent_components_do_not_admit_excluded_sources() -> anyhow::Result<()> {
    let root = fixture()?;
    run_child(root.path(), "parent-source", OsStr::new(".codex/sessions/private"))
}

#[cfg(unix)]
#[test]
fn source_and_policy_symlinks_do_not_admit_excluded_sources() -> anyhow::Result<()> {
    use std::os::unix::fs::symlink;
    for mode in ["source-alias", "policy-alias", "missing-alias"] {
        let root = fixture()?;
        symlink(root.path().join(".codex/sessions/private"), root.path().join("alias"))?;
        let excluded = if mode == "policy-alias" { "alias" } else { ".codex/sessions/private" };
        run_child(root.path(), mode, OsStr::new(excluded))?;
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn non_unicode_policy_cannot_silently_disable_exclusions() -> anyhow::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let root = fixture()?;
    run_child(root.path(), "invalid-policy", OsStr::from_bytes(b"private-\xff"))
}

fn input<'a>(data: &'a Path, source: &'a Path, provider: &'a str) -> RawMirrorCaptureInput<'a> {
    RawMirrorCaptureInput {
        data_dir: data,
        provider,
        source_id: "local",
        origin_kind: "local",
        origin_host: None,
        source_path: source,
        db_links: &[],
    }
}

fn assert_excluded(data: &Path, source: &Path) -> anyhow::Result<()> {
    for provider in ["codex", "claude"] {
        let error = capture_source_file(input(data, source, provider)).unwrap_err();
        assert!(error.is::<RawMirrorSourceExcluded>(), "{error:#}");
        assert!(!error.to_string().contains(source.to_string_lossy().as_ref()));
        let error = capture_source_file_with_chunk_policy(input(data, source, provider), 1, 7)
            .unwrap_err();
        assert!(error.is::<RawMirrorSourceExcluded>(), "{error:#}");
    }
    assert!(!data.exists(), "excluded capture must not initialize mirror storage");
    Ok(())
}

#[test]
fn child() -> anyhow::Result<()> {
    let Some(root) = std::env::var_os(CHILD_ROOT) else {
        return Ok(());
    };
    let root = PathBuf::from(root);
    let mode = std::env::var(CHILD_MODE)?;
    let data = root.join("mirror");
    let private = root.join(".codex/sessions/private/rollout-1.jsonl");
    let public = root.join(".codex/sessions/private-copy/rollout-2.jsonl");
    let before = [fs::read(&private)?, fs::read(&public)?];
    let source = match mode.as_str() {
        "relative-source" => PathBuf::from(".codex/sessions/private/rollout-1.jsonl"),
        "parent-source" => root.join(".codex/sessions/private-copy/../private/rollout-1.jsonl"),
        "source-alias" => root.join("alias/rollout-1.jsonl"),
        "missing-alias" => root.join("alias/not-present.jsonl"),
        "absolute-source" | "policy-alias" | "invalid-policy" => private.clone(),
        _ => anyhow::bail!("unknown alias regression mode"),
    };
    if mode == "invalid-policy" {
        let error = capture_source_file(input(&data, &source, "codex")).unwrap_err();
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::InvalidInput,
        );
        assert!(!data.exists());
    } else {
        assert_excluded(&data, &source)?;
    }
    let connector = CodexConnector::new();
    let ctx = ScanContext::with_roots(
        data.clone(),
        vec![ScanRoot::local(root.join(".codex"))],
        None,
    );
    if mode == "invalid-policy" {
        // Refuse the scan, rather than certifying empty coverage and advancing
        // a watermark that would hide sources after configuration is repaired.
        for error in [
            connector.discover_source_files(&ctx).unwrap_err(),
            connector.scan(&ctx).unwrap_err(),
        ] {
            assert_eq!(
                error.downcast_ref::<std::io::Error>().unwrap().kind(),
                std::io::ErrorKind::InvalidInput,
            );
        }
        assert!(!data.exists());
    } else {
        let discovered = connector.discover_source_files(&ctx)?;
        let conversations = connector.scan(&ctx)?;
        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].source_path, public);
        assert_eq!(conversations.len(), 1);
        assert_eq!(conversations[0].source_path, public);
        assert!(!data.exists());
        let capture = capture_source_file(input(&data, &public, "codex"))?;
        assert_eq!(capture.source_size_bytes, before[1].len() as u64);
    }
    assert_eq!([fs::read(&private)?, fs::read(&public)?], before);
    Ok(())
}
