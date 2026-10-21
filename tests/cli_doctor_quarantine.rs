//! Real CLI journeys for the doctor entry to shared lexical quarantine policy.
//! Only isolated temporary archives are touched; the malformed DB is a sentinel.

use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use serde_json::Value;
use wait_timeout::ChildExt;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn command(home: &Path, data: &Path, doctor: bool) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cass"));
    command
        .current_dir(home)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_LEXICAL_QUARANTINE_RETENTION", "1")
        .env_remove("CASS_OUTPUT_FORMAT")
        .env_remove("TOON_DEFAULT_FORMAT")
        .env_remove("CASS_DB_PATH");
    if doctor {
        command.args(["doctor", "quarantine"]);
    } else {
        command.arg("lexical-quarantine");
    }
    command.arg("--data-dir").arg(data).arg("--json");
    command
}

fn run(command: &mut Command) -> io::Result<Output> {
    let mut child = command.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    match child.wait_timeout(Duration::from_secs(20)) {
        Ok(Some(_)) => child.wait_with_output(),
        outcome => {
            let _ = child.kill();
            let _ = child.wait();
            Err(match outcome {
                Err(error) => error,
                _ => io::Error::new(io::ErrorKind::TimedOut, "doctor quarantine fixture timed out"),
            })
        }
    }
}

fn json(output: &Output, code: i32) -> Value {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("one complete JSON result")
}

fn bundle(root: &Path, suffix: &str, pending: bool) -> io::Result<PathBuf> {
    let prefix = if pending {
        ".lexical-rebuild-reclaim-"
    } else {
        ".lexical-rebuild-quarantine-"
    };
    let path = root.join(format!("{prefix}{suffix}"));
    let evidence = if pending {
        path.join("quarantine/index")
    } else {
        path.join("index")
    };
    fs::create_dir_all(&evidence)?;
    fs::write(evidence.join("MANIFEST"), b"retained candidate")?;
    fs::write(evidence.join(".lexical-rebuild-last-failure.json"), b"private invalid diagnostic")?;
    Ok(path)
}

fn preview(home: &Path, data: &Path, doctor: bool) -> Value {
    json(&run(command(home, data, doctor).arg("--plan")).unwrap(), 0)
}

fn approval(plan: &Value) -> &str {
    plan["approval_fingerprint"].as_str().expect("complete enabled preview")
}

#[cfg(unix)]
fn age(path: &Path, hours: u64) -> io::Result<()> {
    let past = std::time::SystemTime::now() - Duration::from_secs(hours * 60 * 60);
    for entry in walkdir::WalkDir::new(path).contents_first(true) {
        let entry = entry.map_err(io::Error::other)?;
        fs::File::open(entry.path())?.set_times(fs::FileTimes::new().set_modified(past))?;
    }
    Ok(())
}

#[test]
fn doctor_inventory_reports_evidence_without_opening_or_changing_the_archive() -> TestResult {
    let home = tempfile::tempdir()?;
    let data = home.path().join("archive with spaces");
    let path = bundle(&data.join("index"), "000001", false)?;
    fs::write(data.join("agent_search.db"), b"not a database")?;
    let output = run(&mut command(home.path(), &data, true))?;
    let report = json(&output, 0);
    assert_eq!(report["complete"], true);
    assert_eq!(report["artifacts"].as_array().unwrap().len(), 1);
    let entry = &report["artifacts"][0];
    assert_eq!(entry["path"], serde_json::json!(path));
    assert_eq!(entry["size_complete"], true);
    assert!(entry["size_bytes"].as_u64().unwrap() > 0);
    assert!(entry["age_seconds"].as_u64().is_some());
    assert_eq!(entry["disposition"], "retain_newest");
    assert!(
        entry["failure_evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["kind"] == "finalization_failure")
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("private invalid diagnostic"));
    assert_eq!(fs::read(data.join("agent_search.db"))?, b"not a database");
    assert!(!data.join("agent_search.db-wal").exists());
    assert!(!data.join("index-run.lock").exists());
    assert!(!data.join("raw-mirror").exists());
    assert!(!data.join("index/v9-quill").exists());
    Ok(())
}

#[test]
fn doctor_and_standalone_share_preview_approval_and_noop_execution() -> TestResult {
    let home = tempfile::tempdir()?;
    let data = home.path().join("archive");
    fs::create_dir(&data)?;
    fs::write(data.join("agent_search.db"), b"malformed archive")?;
    let doctor = preview(home.path(), &data, true);
    let standalone = preview(home.path(), &data, false);
    assert_eq!(approval(&doctor), approval(&standalone));
    assert!(!data.join("index-run.lock").exists());
    let report = json(
        &run(command(home.path(), &data, true).args(["--apply", approval(&standalone)]))?,
        0,
    );
    assert_eq!(report["attempted_bundles"], 0);
    assert_eq!(report["removed_bundles"], 0);
    assert_eq!(report["complete"], true);
    assert_eq!(fs::read(data.join("agent_search.db"))?, b"malformed archive");
    Ok(())
}

#[test]
fn doctor_apply_refuses_active_indexer_without_modifying_heartbeat_or_evidence() -> TestResult {
    let home = tempfile::tempdir()?;
    let data = home.path().join("archive");
    let path = bundle(&data, "000001", false)?;
    let lock_path = data.join("index-run.lock");
    fs::write(&lock_path, b"active indexer heartbeat")?;
    let lock = OpenOptions::new().read(true).write(true).open(&lock_path)?;
    fs2::FileExt::try_lock_exclusive(&lock)?;
    let plan = preview(home.path(), &data, true);
    let output = run(command(home.path(), &data, true).args(["--apply", approval(&plan)]))?;
    assert_eq!(output.status.code(), Some(7));
    assert!(output.stdout.is_empty());
    assert_eq!(fs::read(&lock_path)?, b"active indexer heartbeat");
    assert_eq!(fs::read(path.join("index/MANIFEST"))?, b"retained candidate");
    fs2::FileExt::unlock(&lock)?;
    let report = json(
        &run(command(home.path(), &data, true).args(["--apply", approval(&plan)]))?,
        0,
    );
    assert_eq!(report["removed_bundles"], 0);
    Ok(())
}

#[test]
fn doctor_reports_incomplete_and_disabled_inventory_without_granting_approval() -> TestResult {
    let home = tempfile::tempdir()?;
    let data = home.path().join("archive");
    let path = bundle(&data, "000001", false)?;
    let plan = preview(home.path(), &data, true);
    let disabled = json(&run(command(home.path(), &data, true)
        .env("CASS_LEXICAL_QUARANTINE_RETENTION", "0").arg("--plan"))?, 0);
    assert!(disabled["approval_fingerprint"].is_null());
    let output = run(command(home.path(), &data, true)
        .env("CASS_LEXICAL_QUARANTINE_RETENTION", "0").args(["--apply", approval(&plan)]))?;
    assert_eq!(output.status.code(), Some(2));
    assert!(!data.join("index-run.lock").exists());
    fs::write(path.join("operator-notes"), b"unrecognized evidence")?;
    let incomplete = json(&run(command(home.path(), &data, true).arg("--plan"))?, 8);
    assert_eq!(incomplete["inventory"]["complete"], false);
    assert!(incomplete["approval_fingerprint"].is_null());
    assert_eq!(incomplete["inventory"]["eligible_bytes"], 0);
    let output = run(command(home.path(), &data, true).args(["--apply", approval(&plan)]))?;
    assert_eq!(output.status.code(), Some(8));
    assert_eq!(fs::read(path.join("operator-notes"))?, b"unrecognized evidence");
    Ok(())
}

#[test]
fn doctor_help_and_invalid_controls_do_not_create_a_data_directory() -> TestResult {
    let home = tempfile::tempdir()?;
    let data = home.path().join("must-not-exist");
    let output = run(command(home.path(), &data, true).arg("--help"))?;
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("cass doctor quarantine"));
    assert!(text.contains("--plan"));
    assert!(text.contains("--apply"));
    let fake = format!("lq-cleanup-v1-{}", "a".repeat(64));
    for args in [
        vec!["--apply"],
        vec!["--apply", "yes"],
        vec!["--plan", "--apply", fake.as_str()],
        vec!["--fix"],
        vec!["--yes"],
    ] {
        let output = run(command(home.path(), &data, true).args(args))?;
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert!(!data.exists());
    }
    let output = run(command(home.path(), &data, true).arg("--plan"))?;
    assert_eq!(output.status.code(), Some(3));
    assert!(!data.exists());
    Ok(())
}

#[cfg(unix)]
#[test]
fn doctor_cleanup_keeps_newest_and_recent_while_reclaiming_old_and_interrupted() -> TestResult {
    let home = tempfile::tempdir()?;
    let data = home.path().join("archive");
    let old = bundle(&data, "000001", false)?;
    let pending = bundle(&data.join("index"), "000002", true)?;
    let newest = bundle(&data.join("index"), "000003", false)?;
    let recent = bundle(&data, "000004", false)?;
    for (path, hours) in [(&old, 24), (&pending, 23), (&newest, 1), (&recent, 3)] {
        age(path, hours)?;
    }
    fs::write(data.join("agent_search.db"), b"archive sentinel")?;
    fs::create_dir_all(data.join("index/v9-quill"))?;
    fs::write(data.join("index/v9-quill/MANIFEST"), b"live sentinel")?;
    let plan = preview(home.path(), &data, false);
    let report = json(
        &run(command(home.path(), &data, true).args(["--apply", approval(&plan)]))?,
        0,
    );
    assert_eq!(report["removed_bundles"], 2);
    assert_eq!(report["retained_bundles"], 2);
    assert!(!old.exists());
    assert!(!pending.exists());
    for path in [&newest, &recent] {
        assert_eq!(fs::read(path.join("index/MANIFEST"))?, b"retained candidate");
    }
    assert_eq!(fs::read(data.join("agent_search.db"))?, b"archive sentinel");
    assert_eq!(fs::read(data.join("index/v9-quill/MANIFEST"))?, b"live sentinel");
    let stale = run(command(home.path(), &data, true).args(["--apply", approval(&plan)]))?;
    assert_eq!(stale.status.code(), Some(6));
    assert!(stale.stdout.is_empty());
    Ok(())
}

#[cfg(unix)]
#[test]
fn doctor_refuses_symlink_evidence_without_touching_the_outside_target() -> TestResult {
    let home = tempfile::tempdir()?;
    let data = home.path().join("archive");
    let path = bundle(&data, "000001", false)?;
    let outside = home.path().join("outside");
    fs::write(&outside, b"external sentinel")?;
    std::os::unix::fs::symlink(&outside, path.join("index/link"))?;
    let report = json(&run(command(home.path(), &data, true).arg("--plan"))?, 8);
    assert_eq!(report["inventory"]["complete"], false);
    assert!(report["approval_fingerprint"].is_null());
    assert_eq!(fs::read(outside)?, b"external sentinel");
    assert!(fs::symlink_metadata(path.join("index/link"))?.file_type().is_symlink());
    assert!(!data.join("index-run.lock").exists());
    Ok(())
}
