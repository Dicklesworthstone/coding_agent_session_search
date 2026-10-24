//! GH #523: the real diag/doctor commands expose the bounded rebuild inventory.
use assert_cmd::Command;
use serde_json::Value;
use std::fs;
use std::path::Path;

fn cass(home: &Path) -> Command {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    command
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("CASS_LEXICAL_QUARANTINE_RETENTION", "1")
        .env("HOME", home)
        .env("XDG_DATA_HOME", home)
        .env("XDG_CONFIG_HOME", home)
        .env("CLAUDE_CONFIG_DIR", home.join(".claude"))
        .env("CODEX_HOME", home.join(".codex"))
        .current_dir(home);
    command
}

fn seed(root: &Path, suffix: &str) {
    let index = root.join(format!(".lexical-rebuild-quarantine-{suffix}/index"));
    fs::create_dir_all(&index).unwrap();
    fs::write(
        index.join("lexical-generation-manifest.json"),
        b"{private malformed candidate",
    )
    .unwrap();
    fs::write(
        index.join(".lexical-rebuild-last-failure.json"),
        b"private certification failure",
    )
    .unwrap();
}

fn run_json(home: &Path, data: &Path, command: &str) -> Value {
    let mut cli = cass(home);
    cli.arg(command).arg("--json").arg("--data-dir").arg(data);
    if command == "diag" {
        cli.arg("--quarantine");
    } else {
        cli.arg("--check");
    }
    let output = cli.output().unwrap();
    if command == "diag" {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    // Doctor may report an unhealthy/uninitialized fixture. It must still emit
    // its full diagnostic report; a command error or absent report fails here.
    let payload: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "{command}: {error}; stderr={}; stdout={}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        )
    });
    assert!(
        payload["quarantine"]["lexical_rebuild_quarantine"].is_object(),
        "{payload}"
    );
    payload
}

#[test]
fn diag_and_doctor_report_both_roots_and_do_not_parse_candidate_manifests() {
    let home = tempfile::tempdir().unwrap();
    let data = home.path().join("cass-data");
    seed(&data, "000001");
    seed(&data.join("index"), "000002");
    for command in ["diag", "doctor"] {
        let payload = run_json(home.path(), &data, command);
        let rebuilds = &payload["quarantine"]["lexical_rebuild_quarantine"];
        assert_eq!(rebuilds["complete"], true);
        assert_eq!(rebuilds["cleanup_apply_allowed"], false);
        assert_eq!(rebuilds["repeated_failure_evidence"], true);
        let artifacts = rebuilds["inventory"]["artifacts"].as_array().unwrap();
        assert_eq!(artifacts.len(), 2);
        assert!(
            artifacts
                .iter()
                .all(|artifact| artifact["size_complete"] == true
                    && artifact["age_seconds"].is_u64())
        );
        assert_eq!(
            artifacts
                .iter()
                .filter(|artifact| artifact["disposition"] == "retain_newest")
                .count(),
            1
        );
        assert_eq!(rebuilds["inventory"]["eligible_bytes"], 0);
        assert!(
            artifacts
                .iter()
                .all(|artifact| artifact["failure_evidence"].as_array().unwrap().len() == 2)
        );
        assert_eq!(
            payload["quarantine"]["summary"]["lexical_generation_count"],
            0
        );
        assert!(
            !payload
                .to_string()
                .contains("private certification failure")
        );
        assert!(!payload.to_string().contains("private malformed candidate"));
        assert!(!data.join("index-run.lock").exists());
    }
}

#[test]
fn diag_human_output_names_sizes_age_retention_and_failure_evidence() {
    let home = tempfile::tempdir().unwrap();
    let data = home.path().join("cass-data");
    seed(&data.join("index"), "000001");
    let output = cass(home.path())
        .args(["diag", "--quarantine", "--data-dir"])
        .arg(&data)
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("Lexical rebuild quarantines"));
    assert!(text.contains("age="));
    assert!(text.contains("RetainNewest"));
    assert!(text.contains(".lexical-rebuild-last-failure.json"));
    assert!(!text.contains("private certification failure"));
}

#[test]
fn diag_without_quarantine_does_not_add_the_inventory() {
    let home = tempfile::tempdir().unwrap();
    let data = home.path().join("cass-data");
    seed(&data, "000001");
    let output = cass(home.path())
        .args(["diag", "--json", "--data-dir"])
        .arg(&data)
        .output()
        .unwrap();
    assert!(output.status.success());
    let payload: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(payload.get("quarantine").is_none());
}

#[cfg(unix)]
#[test]
fn symlinked_evidence_blocks_eligibility_without_touching_foreign_files() {
    let home = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("sentinel"), b"foreign file").unwrap();
    let data = home.path().join("cass-data");
    seed(&data, "000001");
    let link = data.join(".lexical-rebuild-quarantine-000001/index/link");
    std::os::unix::fs::symlink(outside.path(), &link).unwrap();
    let payload = run_json(home.path(), &data, "diag");
    let rebuilds = &payload["quarantine"]["lexical_rebuild_quarantine"];
    assert_eq!(rebuilds["complete"], false);
    assert_eq!(rebuilds["inventory"]["eligible_bytes"], 0);
    assert_eq!(
        rebuilds["inventory"]["artifacts"][0]["disposition"],
        "inspect"
    );
    assert_eq!(
        fs::read(outside.path().join("sentinel")).unwrap(),
        b"foreign file"
    );
    assert!(fs::symlink_metadata(link).unwrap().file_type().is_symlink());
}

#[cfg(unix)]
#[test]
fn diag_inventory_does_not_acquire_or_rewrite_the_active_indexer_lock() {
    let home = tempfile::tempdir().unwrap();
    let data = home.path().join("cass-data");
    seed(&data, "000001");
    let lock_path = data.join("index-run.lock");
    fs::write(&lock_path, b"active writer heartbeat").unwrap();
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
        .unwrap();
    fs2::FileExt::try_lock_exclusive(&lock).unwrap();
    let payload = run_json(home.path(), &data, "diag");
    assert_eq!(
        payload["quarantine"]["lexical_rebuild_quarantine"]["complete"],
        true
    );
    assert_eq!(fs::read(&lock_path).unwrap(), b"active writer heartbeat");
}
