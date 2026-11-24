//! The activity probe runs before connector admission. A nonregular pathname
//! must therefore not block it even when the connector's opener is safe.

use super::*;
use std::fs::File;
use std::io;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn advisory_probe_preserves_regular_file_locks_and_symlinks() -> io::Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("rollout-regular.jsonl");
    let bytes = b"{\"complete\":\"source must remain unchanged\"}\n";
    fs::write(&path, bytes)?;
    let alias = root.path().join("rollout-alias.jsonl");
    std::os::unix::fs::symlink(&path, &alias)?;
    let modified = fs::metadata(&path)?.modified()?;
    let owner = File::open(&path)?;

    assert!(!source_file_has_active_advisory_lock(&path));
    assert!(!source_file_has_active_advisory_lock(&alias));
    owner.lock()?;
    assert!(source_file_has_active_advisory_lock(&path));
    assert!(source_file_has_active_advisory_lock(&alias));
    owner.unlock()?;
    assert!(!source_file_has_active_advisory_lock(&path));
    assert!(!source_file_has_active_advisory_lock(&alias));
    assert!(!source_file_has_active_advisory_lock(root.path()));
    assert!(!source_file_has_active_advisory_lock(
        &root.path().join("rollout-missing.jsonl")
    ));
    assert_eq!(fs::read(&path)?, bytes);
    assert_eq!(fs::metadata(&path)?.modified()?, modified);
    Ok(())
}

#[test]
fn advisory_probe_does_not_wait_for_fifo_writers() -> io::Result<()> {
    let root = tempfile::tempdir()?;
    let (_, module) = module_path!()
        .split_once("::")
        .ok_or_else(|| io::Error::other("advisory probe test has no module prefix"))?;
    let child_test = format!("{module}::advisory_probe_fifo_child");
    let mut child = Command::new(std::env::current_exe()?)
        .args(["--exact", &child_test, "--nocapture", "--test-threads=1"])
        .env("CASS_TEST_ADVISORY_FIFO_ROOT", root.path())
        .stdin(Stdio::null())
        .spawn()?;
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < Duration::from_secs(10) => {
                std::thread::sleep(Duration::from_millis(10));
            }
            outcome => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(match outcome {
                    Err(error) => error,
                    _ => io::Error::new(
                        io::ErrorKind::TimedOut,
                        "activity probe waited for a FIFO writer before connector admission",
                    ),
                });
            }
        }
    };
    assert!(status.success(), "advisory probe FIFO child failed");
    // A zero-test child run must not satisfy the boundedness regression.
    assert_eq!(
        fs::read(root.path().join("advisory-fifo-receipt"))?,
        b"direct,symlink,replacement,activity-filter,retry"
    );
    Ok(())
}

#[test]
fn advisory_probe_fifo_child() -> io::Result<()> {
    let Ok(root) = dotenvy::var("CASS_TEST_ADVISORY_FIFO_ROOT") else {
        return Ok(());
    };
    let root = PathBuf::from(root);
    let make_fifo = |path: &Path| -> io::Result<()> {
        if !Command::new("mkfifo").arg(path).status()?.success() {
            return Err(io::Error::other("create advisory-probe source FIFO"));
        }
        Ok(())
    };
    let fifo = root.join("rollout-no-writer.jsonl");
    make_fifo(&fifo)?;
    assert!(!source_file_has_active_advisory_lock(&fifo));
    let alias = root.join("rollout-fifo-alias.jsonl");
    std::os::unix::fs::symlink(&fifo, &alias)?;
    assert!(!source_file_has_active_advisory_lock(&alias));

    let replaced = root.join("rollout-replaced.jsonl");
    let original = b"{\"original\":\"retained\"}\n";
    fs::write(&replaced, original)?;
    assert!(fs::metadata(&replaced)?.is_file());
    let retained = root.join("original-retained.jsonl");
    fs::rename(&replaced, &retained)?;
    make_fifo(&replaced)?;
    assert!(!source_file_has_active_advisory_lock(&replaced));
    let probe = SourceWriterProbe::default();
    for path in [&fifo, &alias, &replaced] {
        assert!(probe.active_writer_reason(path, None).is_none());
        assert!(!source_can_recover_incomplete_tail(path));
    }
    assert_eq!(fs::read(&retained)?, original);

    // Retain both generations and verify that the same pathname can recover.
    fs::rename(&replaced, root.join("retained-fifo"))?;
    fs::rename(&retained, &replaced)?;
    assert!(!source_file_has_active_advisory_lock(&replaced));
    let owner = File::open(&replaced)?;
    owner.lock()?;
    assert!(source_file_has_active_advisory_lock(&replaced));
    owner.unlock()?;
    assert!(!source_file_has_active_advisory_lock(&replaced));
    assert_eq!(fs::read(&replaced)?, original);
    fs::write(
        root.join("advisory-fifo-receipt"),
        b"direct,symlink,replacement,activity-filter,retry",
    )?;
    Ok(())
}
