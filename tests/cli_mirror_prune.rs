//! Subprocess integration tests for `cass mirror prune`.
//!
//! Issue #253 reported that on v0.5.2 every documented invocation of
//! `cass mirror prune` exited 0 with empty stdout and stderr. The
//! `Commands::Mirror(..)` arm of cass's outer dispatcher was listed in the
//! pattern of the Index/Search/Pack/... branch, but the inner match inside
//! that branch had no `Commands::Mirror(..)` arm; the actual dispatch lived
//! in the sibling `_ =>` branch and was therefore unreachable for any
//! mirror invocation. The user-visible symptom was a clean exit with no
//! output for every flag combination.
//!
//! These tests pin the contract that the success path emits a plan/summary,
//! the no-args path errors out with the documented usage message, and the
//! `--json`/`--robot` paths produce a parseable JSON envelope.

use assert_cmd::Command;
use coding_agent_search::raw_mirror::{RawMirrorCaptureInput, capture_source_file};
use predicates::str::contains;
use std::path::Path;
use tempfile::TempDir;

/// Build a `cass` command with an isolated data dir and no update prompts.
fn cmd_in(temp_home: &Path) -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");
    let data_dir = temp_home.join("data");
    std::fs::create_dir_all(&data_dir).ok();
    cmd.env("HOME", temp_home);
    cmd.env("XDG_DATA_HOME", temp_home.join(".local/share"));
    cmd.env("CASS_DATA_DIR", &data_dir);
    cmd.env("XDG_CONFIG_HOME", temp_home.join(".config"));
    cmd.env("NO_COLOR", "1");
    cmd
}

/// The no-args invocation must reject with the documented usage message.
/// Regression guard for issue #253: previously this exited 0 silently.
#[test]
fn mirror_prune_without_retention_predicate_fails_with_usage_error() {
    let temp = TempDir::new().expect("tempdir");
    let mut cmd = cmd_in(temp.path());
    cmd.args(["mirror", "prune"]);
    cmd.assert().failure().code(2).stderr(contains(
        "cass mirror prune needs at least one retention predicate",
    ));
}

/// `--older-than` alone is a valid retention predicate; the dry-run path
/// must emit a human-readable plan to stdout and exit 0.
/// Regression guard for issue #253.
#[test]
fn mirror_prune_dry_run_emits_plan_to_stdout() {
    let temp = TempDir::new().expect("tempdir");
    let mut cmd = cmd_in(temp.path());
    cmd.args(["mirror", "prune", "--older-than", "90d", "--dry-run"]);
    let output = cmd.assert().success();
    let stdout = String::from_utf8_lossy(&output.get_output().stdout).into_owned();
    assert!(
        stdout.contains("Raw-mirror prune (dry-run)"),
        "stdout missing dry-run header; got: {stdout:?}"
    );
    assert!(
        stdout.contains("Manifests:"),
        "stdout missing manifest count line; got: {stdout:?}"
    );
    assert!(
        stdout.contains("Planned manifests:"),
        "stdout missing planned-manifest line; got: {stdout:?}"
    );
}

/// `--json` (or `--robot`) must emit a JSON envelope with a `prune` payload.
/// Regression guard for issue #253: previously the JSON path was also silent.
#[test]
fn mirror_prune_json_emits_envelope_with_prune_payload() {
    let temp = TempDir::new().expect("tempdir");
    let mut cmd = cmd_in(temp.path());
    cmd.args([
        "mirror",
        "prune",
        "--older-than",
        "90d",
        "--dry-run",
        "--json",
    ]);
    let output = cmd.assert().success();
    let stdout = String::from_utf8_lossy(&output.get_output().stdout).into_owned();
    let value: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|err| panic!("stdout not valid JSON: {err}; got {stdout:?}"));
    assert_eq!(
        value.get("success"),
        Some(&serde_json::Value::Bool(true)),
        "envelope missing success=true: {value}"
    );
    let prune = value
        .get("prune")
        .expect("envelope missing `prune` payload");
    assert_eq!(
        prune.get("mode").and_then(|v| v.as_str()),
        Some("dry-run"),
        "expected mode=dry-run in payload; got {prune}"
    );
    assert!(
        prune.get("manifest_count").is_some(),
        "payload missing manifest_count: {prune}"
    );
}

/// GH488: populated previews return candidates without adding persistent audit
/// records. Exercise both an absent ledger and retained historical evidence.
#[test]
fn mirror_prune_repeated_previews_do_not_grow_the_archive() {
    let temp = TempDir::new().expect("tempdir");
    let data_dir = temp.path().join("data");
    for index in 0..32 {
        let source_path = temp.path().join(format!("session-{index}.jsonl"));
        std::fs::write(&source_path, format!("{{\"text\":\"session {index}\"}}\n"))
            .expect("write source");
        capture_source_file(RawMirrorCaptureInput {
            data_dir: &data_dir,
            provider: "codex",
            source_id: "local",
            origin_kind: "local",
            origin_host: None,
            source_path: &source_path,
            db_links: &[],
        })
        .expect("capture real source");
    }
    let root = data_dir.join("raw-mirror/v1");
    let audit_path = root.join("pruned.jsonl");
    let snapshot = || {
        walkdir::WalkDir::new(&root)
            .into_iter()
            .map(|entry| entry.expect("mirror entry"))
            .filter(|entry| entry.file_type().is_file())
            .map(|entry| {
                let bytes = std::fs::read(entry.path()).expect("mirror bytes");
                (entry.into_path(), bytes)
            })
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    for preview in 0..4 {
        if preview == 1 {
            std::fs::write(&audit_path, b"{\"mode\":\"apply\",\"applied\":true}\n")
                .expect("seed historical audit");
        }
        let before = snapshot();
        let mut cmd = cmd_in(temp.path());
        cmd.args([
            "mirror",
            "prune",
            "--max-size",
            "0",
            "--safety-hold-down",
            "0s",
            "--dry-run",
            "--json",
        ]);
        let output = cmd.assert().success();
        let value: serde_json::Value =
            serde_json::from_slice(&output.get_output().stdout).expect("prune JSON");
        let prune = &value["prune"];
        assert_eq!(value["success"], true);
        assert_eq!(prune["mode"], "dry-run");
        assert_eq!(prune["planned_manifest_count"], 32);
        assert_eq!(prune["planned_blob_count"], 32);
        assert_eq!(prune["entries"].as_array().expect("candidates").len(), 64);
        assert_eq!(prune["applied_reclaim_bytes"], 0);
        assert!(prune["audit_log_path"].is_null());
        assert_eq!(snapshot(), before, "preview {preview} changed mirror files");
        if preview == 0 {
            assert!(!audit_path.exists());
        }
    }
}

/// Preview details are bounded, but apply must retain every audit entry.
#[test]
fn mirror_prune_caps_preview_details_without_truncating_apply() {
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path().join("data/raw-mirror/v1");
    let mut expected_bytes = 0_u64;
    for index in 0..1_001 {
        let bytes = format!("orphan capture {index}");
        let digest = blake3::hash(bytes.as_bytes()).to_hex().to_string();
        let directory = root.join("blobs/blake3").join(&digest[..2]);
        std::fs::create_dir_all(&directory).expect("blob directory");
        std::fs::write(directory.join(format!("{digest}.raw")), &bytes).expect("orphan blob");
        expected_bytes += bytes.len() as u64;
    }
    let audit_path = root.join("pruned.jsonl");
    for mode in ["--dry-run", "--dry-run", "--apply"] {
        let mut cmd = cmd_in(temp.path());
        cmd.args([
            "mirror",
            "prune",
            "--max-size",
            "0",
            "--safety-hold-down",
            "0s",
            mode,
            "--json",
        ]);
        let output = cmd.assert().success();
        let value: serde_json::Value =
            serde_json::from_slice(&output.get_output().stdout).expect("prune JSON");
        let report = &value["prune"];
        assert_eq!(report["planned_blob_count"], 1_001);
        assert_eq!(report["planned_reclaim_bytes"], expected_bytes);
        let entries = report["entries"].as_array().expect("entries");
        if mode == "--dry-run" {
            assert_eq!(entries.len(), 1_000);
            assert_eq!(report["omitted_entry_count"], 1);
            assert_eq!(report["applied_blob_count"], 0);
            assert!(!audit_path.exists());
        } else {
            assert_eq!(entries.len(), 1_001);
            assert_eq!(report["omitted_entry_count"], 0);
            assert_eq!(report["applied_blob_count"], 1_001);
            assert_eq!(report["applied_reclaim_bytes"], expected_bytes);
            assert!(entries.iter().all(|entry| entry["applied"] == true));
            let audit = std::fs::read_to_string(&audit_path).expect("applied audit");
            assert_eq!(
                audit.lines().count(),
                2_002,
                "one intent and result per blob"
            );
        }
    }
}

/// `--apply` on an empty data dir must still produce an "apply" summary
/// rather than silently exiting. The actual prune work is a no-op (no
/// captures present in the tempdir), but the user-facing summary is what
/// proves dispatch reached `run_mirror_prune`.
/// Regression guard for issue #253.
#[test]
fn mirror_prune_apply_emits_summary_even_with_empty_mirror() {
    let temp = TempDir::new().expect("tempdir");
    let mut cmd = cmd_in(temp.path());
    cmd.args([
        "mirror",
        "prune",
        "--older-than",
        "90d",
        "--apply",
        "--safety-hold-down",
        "0s",
    ]);
    let output = cmd.assert().success();
    let stdout = String::from_utf8_lossy(&output.get_output().stdout).into_owned();
    assert!(
        stdout.contains("Raw-mirror prune (apply)"),
        "stdout missing apply header; got: {stdout:?}"
    );
    assert!(
        stdout.contains("Applied manifests:"),
        "stdout missing applied-manifest line; got: {stdout:?}"
    );
}
