//! Fresh-process plan/apply journeys. Fixtures never contain a real archive.
//! Aging/removal cases are Unix-only; parser/no-op/locking checks also compile
//! elsewhere. This target does not qualify actual release binaries by itself.

use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use serde_json::Value;
use wait_timeout::ChildExt;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn cass(home: &Path, data: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cass"));
    command.current_dir(home)
        .env("HOME", home).env("USERPROFILE", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("CASS_LEXICAL_QUARANTINE_RETENTION", "1")
        .env_remove("CASS_OUTPUT_FORMAT").env_remove("TOON_DEFAULT_FORMAT")
        .args(["lexical-quarantine", "--data-dir"]).arg(data).arg("--json");
    command
}

fn run(command: &mut Command) -> io::Result<Output> {
    let mut child = command.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    match child.wait_timeout(Duration::from_secs(20)) {
        Ok(Some(_)) => child.wait_with_output(),
        outcome => {
            // Own and reap this fixture process even on supervision errors.
            let _ = child.kill();
            let _ = child.wait();
            Err(match outcome {
                Err(error) => error,
                _ => io::Error::new(io::ErrorKind::TimedOut, "fixture cass exceeded 20 seconds"),
            })
        }
    }
}

fn plan(home: &Path, data: &Path) -> Result<Value, Box<dyn std::error::Error>> {
    let output = run(cass(home, data).arg("--plan"))?;
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn approval(plan: &Value) -> &str {
    plan["approval_fingerprint"].as_str().expect("complete enabled plan needs approval")
}

fn bundle(root: &Path, suffix: &str) -> io::Result<PathBuf> {
    let path = root.join(format!(".lexical-rebuild-quarantine-{suffix}"));
    fs::create_dir_all(path.join("index"))?;
    fs::write(path.join("index/MANIFEST"), b"derived failure evidence")?;
    Ok(path)
}

#[cfg(unix)]
fn old_bundle(root: &Path, suffix: &str, age_hours: u64) -> io::Result<PathBuf> {
    use std::time::SystemTime;
    let path = bundle(root, suffix)?;
    let past = SystemTime::now() - Duration::from_secs(age_hours * 60 * 60);
    // Explicitly age every entry; policy uses the newest descendant, not just
    // the bundle's root mtime. No production time override is introduced.
    for entry in walkdir::WalkDir::new(&path).contents_first(true) {
        let entry = entry.map_err(io::Error::other)?;
        fs::File::open(entry.path())?.set_times(fs::FileTimes::new().set_modified(past))?;
    }
    Ok(path)
}

#[test]
fn preview_and_empty_apply_work_with_unreadable_archive_without_index_startup() -> TestResult {
    let home = tempfile::tempdir()?;
    let data = home.path().join("archive with spaces");
    fs::create_dir(&data)?;
    fs::write(data.join("agent_search.db"), b"not a database")?;
    let preview = plan(home.path(), &data)?;
    assert_eq!(preview["inventory"]["complete"], true);
    assert_eq!(preview["inventory"]["artifacts"], serde_json::json!([]));
    assert!(!data.join("index-run.lock").exists());
    let output = run(cass(home.path(), &data).args(["--apply", approval(&preview)]))?;
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let report: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(report["complete"], true);
    assert_eq!(report["attempted_bundles"], 0);
    assert_eq!(report["removed_bundles"], 0);
    assert_eq!(fs::read(data.join("agent_search.db"))?, b"not a database");
    assert!(!data.join("agent_search.db-wal").exists());
    assert!(!data.join("index").exists());
    Ok(())
}

#[cfg(unix)]
#[test]
fn approved_cli_cleanup_keeps_newest_and_rejects_replayed_approval() -> TestResult {
    let home = tempfile::tempdir()?;
    let data = home.path().join("archive");
    let a = old_bundle(&data, "000001", 12)?;
    let b = old_bundle(&data.join("index"), "000002", 11)?;
    let newest = old_bundle(&data.join("index"), "000003", 10)?;
    fs::write(data.join("agent_search.db"), b"archive sentinel")?;
    fs::create_dir(data.join("index/v9-quill"))?;
    fs::write(data.join("index/v9-quill/MANIFEST"), b"live")?;
    let preview = plan(home.path(), &data)?;
    assert_eq!(preview["inventory"]["artifacts"].as_array().unwrap().len(), 3);
    let output = run(cass(home.path(), &data).args(["--apply", approval(&preview)]))?;
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let report: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(report["complete"], true);
    assert_eq!(report["removed_bundles"], 2);
    assert_eq!(report["retained_bundles"], 1);
    assert!(!a.exists()); assert!(!b.exists());
    assert_eq!(fs::read(newest.join("index/MANIFEST"))?, b"derived failure evidence");
    assert_eq!(fs::read(data.join("agent_search.db"))?, b"archive sentinel");
    assert_eq!(fs::read(data.join("index/v9-quill/MANIFEST"))?, b"live");
    let repeated = run(cass(home.path(), &data).args(["--apply", approval(&preview)]))?;
    assert_eq!(repeated.status.code(), Some(6));
    assert!(repeated.stdout.is_empty());
    assert!(newest.join("index/MANIFEST").is_file());
    Ok(())
}

#[cfg(unix)]
#[test]
fn changed_evidence_refuses_old_approval_before_removal() -> TestResult {
    let home = tempfile::tempdir()?;
    let data = home.path().join("archive");
    let changed = old_bundle(&data, "000001", 12)?;
    let other = old_bundle(&data, "000002", 11)?;
    let preview = plan(home.path(), &data)?;
    fs::write(changed.join("index/new-evidence"), b"retain")?;
    let output = run(cass(home.path(), &data).args(["--apply", approval(&preview)]))?;
    assert_eq!(output.status.code(), Some(6));
    assert!(output.stdout.is_empty());
    assert_eq!(fs::read(changed.join("index/new-evidence"))?, b"retain");
    assert!(other.join("index/MANIFEST").is_file());
    Ok(())
}

#[test]
fn active_indexer_lock_blocks_apply_but_not_read_only_preview() -> TestResult {
    let home = tempfile::tempdir()?;
    let data = home.path().join("archive");
    bundle(&data, "000001")?;
    let path = data.join("index-run.lock");
    fs::write(&path, b"active heartbeat")?;
    let lock = OpenOptions::new().read(true).write(true).open(&path)?;
    fs2::FileExt::try_lock_exclusive(&lock)?;
    let preview = plan(home.path(), &data)?;
    let output = run(cass(home.path(), &data).args(["--apply", approval(&preview)]))?;
    assert_eq!(output.status.code(), Some(7));
    assert!(output.stdout.is_empty());
    assert_eq!(fs::read(&path)?, b"active heartbeat");
    fs2::FileExt::unlock(&lock)?;
    let output = run(cass(home.path(), &data).args(["--apply", approval(&preview)]))?;
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    Ok(())
}

#[test]
fn partial_and_disabled_previews_cannot_authorize_cleanup() -> TestResult {
    let home = tempfile::tempdir()?;
    let data = home.path().join("archive");
    let path = bundle(&data, "000001")?;
    let preview = plan(home.path(), &data)?;
    let disabled = run(cass(home.path(), &data).env("CASS_LEXICAL_QUARANTINE_RETENTION", "0").arg("--plan"))?;
    assert!(disabled.status.success());
    let disabled_plan: Value = serde_json::from_slice(&disabled.stdout)?;
    assert!(disabled_plan["approval_fingerprint"].is_null());
    let disabled_apply = run(cass(home.path(), &data).env("CASS_LEXICAL_QUARANTINE_RETENTION", "0")
        .args(["--apply", approval(&preview)]))?;
    assert_eq!(disabled_apply.status.code(), Some(2));
    assert!(!data.join("index-run.lock").exists());
    fs::write(path.join("operator-notes"), b"unknown evidence")?;
    let partial = run(cass(home.path(), &data).arg("--plan"))?;
    assert_eq!(partial.status.code(), Some(8));
    let partial_plan: Value = serde_json::from_slice(&partial.stdout)?;
    assert_eq!(partial_plan["inventory"]["complete"], false);
    assert!(partial_plan["approval_fingerprint"].is_null());
    let refused = run(cass(home.path(), &data).args(["--apply", approval(&preview)]))?;
    assert_eq!(refused.status.code(), Some(8));
    assert_eq!(fs::read(path.join("operator-notes"))?, b"unknown evidence");
    assert!(path.join("index/MANIFEST").is_file());
    Ok(())
}

#[test]
fn bare_apply_malformed_approval_and_conflicting_modes_never_create_paths() -> TestResult {
    let home = tempfile::tempdir()?;
    let data = home.path().join("must-not-exist");
    let fake = format!("lq-cleanup-v1-{}", "a".repeat(64));
    for args in [vec!["--apply"], vec!["--apply", "yes"], vec!["--plan", "--apply", fake.as_str()]] {
        let output = run(cass(home.path(), &data).args(args))?;
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert!(!data.exists());
    }
    let missing = run(cass(home.path(), &data).arg("--plan"))?;
    assert_eq!(missing.status.code(), Some(3));
    assert!(!data.exists());
    Ok(())
}
