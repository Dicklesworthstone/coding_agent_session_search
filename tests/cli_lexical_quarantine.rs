//! Real-binary inventory contracts; no production archive or model is required.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn cass(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cass"));
    command
        .current_dir(home)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("CASS_LEXICAL_QUARANTINE_RETENTION", "1")
        .env_remove("CASS_OUTPUT_FORMAT")
        .env_remove("TOON_DEFAULT_FORMAT");
    command
}

fn inspect(home: &Path, data: &Path) -> std::io::Result<Output> {
    cass(home)
        .args(["lexical-quarantine", "--data-dir"])
        .arg(data)
        .arg("--json")
        .output()
}

fn bundle(root: &Path, suffix: &str) -> std::io::Result<PathBuf> {
    let path = root.join(format!(".lexical-rebuild-quarantine-{suffix}"));
    fs::create_dir_all(path.join("index"))?;
    fs::write(
        path.join("index/MANIFEST"),
        b"unreadable derived index is still evidence",
    )?;
    fs::write(
        path.join(".lexical-rebuild-state.json"),
        b"unreadable checkpoint is still evidence",
    )?;
    Ok(path)
}

fn files(root: &Path) -> Result<BTreeMap<PathBuf, Vec<u8>>, Box<dyn std::error::Error>> {
    let mut result = BTreeMap::new();
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = entry?;
        if entry.file_type().is_file() {
            result.insert(
                entry.path().strip_prefix(root)?.to_owned(),
                fs::read(entry.path())?,
            );
        }
    }
    Ok(result)
}

#[test]
fn actual_command_inventories_both_roots_without_opening_a_broken_archive() -> TestResult {
    let home = tempfile::tempdir()?;
    let data = home.path().join("archive with spaces");
    bundle(&data, "000001")?;
    bundle(&data.join("index"), "000002")?;
    fs::write(data.join("agent_search.db"), b"not a SQLite database")?;
    fs::create_dir_all(data.join("index/v9-quill"))?;
    fs::write(data.join("index/v9-quill/MANIFEST"), b"live sentinel")?;
    let before = files(&data)?;
    let output = inspect(home.path(), &data)?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["complete"], true);
    assert_eq!(json["eligible_bytes"], 0);
    let artifacts = json["artifacts"].as_array().ok_or("missing inventory")?;
    assert_eq!(artifacts.len(), 2);
    assert_eq!(
        artifacts
            .iter()
            .filter(|a| a["disposition"] == "retain_newest")
            .count(),
        1
    );
    assert_eq!(
        artifacts
            .iter()
            .filter(|a| a["disposition"] == "retain_recent")
            .count(),
        1
    );
    assert!(
        artifacts
            .iter()
            .all(|a| a["size_complete"] == true && a["size_bytes"].as_u64().unwrap_or(0) > 0)
    );
    assert_eq!(before, files(&data)?);
    assert!(!data.join("index-run.lock").exists());
    assert!(!data.join("agent_search.db-wal").exists());
    assert!(!data.join("agent_search.db-shm").exists());
    Ok(())
}

#[test]
fn incomplete_inventory_emits_complete_json_and_nonzero_exit_without_deletion() -> TestResult {
    let home = tempfile::tempdir()?;
    let data = home.path().join("archive");
    let first = bundle(&data.join("index"), "000001")?;
    bundle(&data.join("index"), "000002")?;
    fs::write(
        first.join("operator-notes"),
        b"unrecognized evidence must be retained",
    )?;
    let before = files(&data)?;
    let output = inspect(home.path(), &data)?;
    assert_eq!(
        output.status.code(),
        Some(8),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(json["complete"], false);
    assert_eq!(json["eligible_bytes"], 0);
    assert!(
        json["artifacts"]
            .as_array()
            .ok_or("missing inventory")?
            .iter()
            .all(|a| a["disposition"] == "inspect")
    );
    assert_eq!(before, files(&data)?);
    Ok(())
}

#[test]
fn explicit_scope_and_read_only_cli_flags_are_enforced_by_the_real_parser() -> TestResult {
    let home = tempfile::tempdir()?;
    let missing = home.path().join("not-created");
    let output = inspect(home.path(), &missing)?;
    assert_eq!(output.status.code(), Some(3));
    assert!(output.stdout.is_empty());
    assert!(!missing.exists());
    for args in [
        vec!["lexical-quarantine"],
        vec!["lexical-quarantine", "--data-dir", ".", "--apply"],
    ] {
        let output = cass(home.path()).args(args).output()?;
        assert_eq!(output.status.code(), Some(2));
    }
    let output = cass(home.path())
        .args(["lexical-quarantine", "--help"])
        .output()?;
    assert!(output.status.success());
    assert!(String::from_utf8(output.stdout)?.contains("--data-dir"));
    assert!(!home.path().join("agent_search.db").exists());
    Ok(())
}

#[test]
fn robot_alias_reports_operator_retention_opt_out_without_changes() -> TestResult {
    let home = tempfile::tempdir()?;
    let data = home.path().join("archive");
    bundle(&data, "000001")?;
    bundle(&data, "000002")?;
    let before = files(&data)?;
    let output = cass(home.path())
        .env("CASS_LEXICAL_QUARANTINE_RETENTION", "0")
        .args(["lexical-quarantine", "--data-dir"])
        .arg(&data)
        .arg("--robot")
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(json["complete"], true);
    assert_eq!(json["automatic_retention_enabled"], false);
    assert!(
        json["artifacts"]
            .as_array()
            .ok_or("missing inventory")?
            .iter()
            .all(|a| a["disposition"] == "retain_disabled")
    );
    assert_eq!(before, files(&data)?);
    Ok(())
}
