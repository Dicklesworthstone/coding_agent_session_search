//! Real storage and Codex APIs, with process-isolated environment settings.

use super::*;
use crate::connectors::codex::CodexConnector;
use crate::raw_mirror::{
    RawMirrorCaptureInput, capture_source_file, capture_source_file_with_chunk_policy,
};
use franken_agent_detection::{Connector, ScanContext, ScanRoot};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

const CHILD_ROOT: &str = "CASS_RAW_EXCLUSIONS_ROOT";
const CHILD_MODE: &str = "CASS_RAW_EXCLUSIONS_MODE";

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

fn snapshot(root: &Path) -> anyhow::Result<BTreeMap<PathBuf, Vec<u8>>> {
    let mut files = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        if path.is_dir() {
            for entry in fs::read_dir(path)? {
                pending.push(entry?.path());
            }
        } else if path.is_file() {
            files.insert(path.strip_prefix(root)?.to_path_buf(), fs::read(path)?);
        }
    }
    Ok(files)
}

fn run_child(root: &Path, mode: &str, exclusions: &str) -> anyhow::Result<()> {
    let output = Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "raw_mirror::exclusions::tests::child",
            "--nocapture",
        ])
        .current_dir(root)
        .env(CHILD_ROOT, root)
        .env(CHILD_MODE, mode)
        .env("CODEX_HOME", root.join(".codex"))
        .env("CASS_EXCLUDE_PATHS", exclusions)
        .output()?;
    assert!(
        output.status.success(),
        "{mode}:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

fn fixture() -> anyhow::Result<tempfile::TempDir> {
    let root = tempfile::tempdir()?;
    for path in [
        ".codex/sessions/private/rollout-hidden.jsonl",
        ".codex/sessions/private-copy/rollout-public.jsonl",
        ".codex/archived_sessions/rollout-hidden.json",
    ] {
        let path = root.path().join(path);
        fs::create_dir_all(path.parent().unwrap())?;
        let body = if path.extension().unwrap() == "json" {
            "{\"items\":[{\"role\":\"user\",\"content\":\"private fixture\"}]}\n"
        } else {
            "{\"type\":\"response_item\",\"payload\":{\"role\":\"user\",\"content\":\"fixture message\"}}\n"
        };
        fs::write(path, body)?;
    }
    Ok(root)
}

#[test]
fn all_excluded_codex_inventory_cannot_be_recaptured_by_fallback() -> anyhow::Result<()> {
    let root = fixture()?;
    let excluded = root.path().join(".codex");
    run_child(root.path(), "fallback", excluded.to_str().unwrap())
}

#[test]
fn file_and_directory_exclusions_preserve_prefix_siblings() -> anyhow::Result<()> {
    let root = fixture()?;
    let value = format!(
        " , {} ,\r\n {} \n, ",
        root.path().join(".codex/sessions/private").display(),
        root.path()
            .join(".codex/archived_sessions/rollout-hidden.json")
            .display(),
    );
    run_child(root.path(), "mixed", &value)
}

#[test]
fn excluded_capture_cannot_reuse_warm_cache_or_modify_existing_storage() -> anyhow::Result<()> {
    let root = fixture()?;
    let excluded = root.path().join(".codex");
    run_child(root.path(), "cached", excluded.to_str().unwrap())
}

#[test]
fn exclusion_precedes_source_io_and_mirror_initialization() -> anyhow::Result<()> {
    let root = fixture()?;
    let missing = root.path().join("not-present/rollout-missing.jsonl");
    run_child(root.path(), "missing", missing.to_str().unwrap())
}

#[test]
fn empty_and_unrelated_exclusions_preserve_capture() -> anyhow::Result<()> {
    let root = fixture()?;
    for value in ["", " , \r\n , ", "not-the-source"] {
        run_child(root.path(), "allowed", value)?;
    }
    Ok(())
}

#[test]
fn child() -> anyhow::Result<()> {
    let Some(root) = std::env::var_os(CHILD_ROOT) else {
        return Ok(());
    };
    let root = PathBuf::from(root);
    let mode = dotenvy::var(CHILD_MODE)?;
    let data = root.join("cass-data");
    let private = root.join(".codex/sessions/private/rollout-hidden.jsonl");
    let archived = root.join(".codex/archived_sessions/rollout-hidden.json");
    let public = root.join(".codex/sessions/private-copy/rollout-public.jsonl");
    let original = snapshot(&root.join(".codex"))?;
    if mode == "cached" {
        // Seed the private storage engine, not the guarded public entry point,
        // to model captures/cache entries created before a policy change.
        // The production module is private, so external callers cannot use it
        // to bypass admission. No process-global environment mutation is used.
        super::super::store::capture_source_file(input(&data, &private, "codex"))?;
        let before = snapshot(&data)?;
        for _ in 0..2 {
            assert_excluded(&data, &private)?;
        }
        assert_eq!(snapshot(&data)?, before);
    } else if mode == "fallback" {
        let connector = CodexConnector::new();
        for roots in [vec![], vec![ScanRoot::local(root.join(".codex"))]] {
            let ctx = ScanContext::with_roots(data.clone(), roots, None);
            assert!(connector.discover_source_files(&ctx)?.is_empty());
            assert!(connector.scan(&ctx)?.is_empty());
        }
        // The production indexer falls back to explicit source capture when
        // discovery returns no files. Exercise the very same capture API;
        // an empty filtered inventory must never grant capture permission.
        for source in [&private, &archived, &public] {
            assert_excluded(&data, source)?;
        }
        assert!(!data.exists());
    } else if mode == "mixed" {
        for source in [&private, &archived] {
            assert_excluded(&data, source)?;
        }
        assert!(!data.exists());
        let captured = capture_source_file(input(&data, &public, "codex"))?;
        assert_eq!(captured.source_size_bytes, fs::metadata(&public)?.len());
        let captured = capture_source_file_with_chunk_policy(input(&data, &public, "codex"), 1, 7)?;
        assert!(captured.chunk_count > 1);
    } else if mode == "missing" {
        assert_excluded(&data, &root.join("not-present/rollout-missing.jsonl"))?;
        assert!(!data.exists());
    } else {
        assert_eq!(mode, "allowed");
        let capture = capture_source_file(input(&data, &private, "codex"))?;
        assert_eq!(capture.source_size_bytes, fs::metadata(&private)?.len());
    }
    assert_eq!(snapshot(&root.join(".codex"))?, original);
    Ok(())
}

fn assert_excluded(data: &Path, source: &Path) -> anyhow::Result<()> {
    for provider in ["codex", "claude"] {
        let error = capture_source_file(input(data, source, provider)).unwrap_err();
        assert!(error.downcast_ref::<RawMirrorSourceExcluded>().is_some());
        assert!(
            !error
                .to_string()
                .contains(source.to_string_lossy().as_ref())
        );
        let error =
            capture_source_file_with_chunk_policy(input(data, source, provider), 1, 7).unwrap_err();
        assert!(error.downcast_ref::<RawMirrorSourceExcluded>().is_some());
    }
    Ok(())
}
