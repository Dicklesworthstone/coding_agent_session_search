//! One nonblocking, cross-process lease for the complete scheduled job.
//!
//! Index children already use the index lock, but that lock does not serialize
//! preceding source syncs or the scheduler's read/modify/write receipts. Both
//! jobs and `--force` share this lease. Never unlink or truncate its inode:
//! kernel ownership, not a PID file or a timeout, releases it after process death.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::Path;

use super::{JobReport, ScheduleJob, StepReport, now_ms, schedule_dir};

#[derive(Debug)]
pub(super) struct Lease {
    _file: File,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AdmissionError {
    Busy,
    Unavailable,
}

impl AdmissionError {
    pub(super) fn report(self, job: ScheduleJob) -> JobReport {
        let (reason, message) = match self {
            Self::Busy => (
                "schedule_busy",
                "another scheduled run is active; retry on the next cycle",
            ),
            Self::Unavailable => (
                "schedule_lock_unavailable",
                "cannot acquire the scheduled-run lock; no scheduled work was started",
            ),
        };
        let timestamp = now_ms();
        // Do not save this receipt over an active owner's state or append to
        // shared history. The OS may still log the returned failed JSON report
        // on stdout; the existing schedule-job-failed exit status is preserved.
        JobReport {
            job,
            started_ms: timestamp,
            finished_ms: timestamp,
            ok: false,
            skipped_reason: None,
            steps: vec![StepReport {
                name: "schedule-admission".to_string(),
                argv: Vec::new(),
                exit_code: None,
                ok: false,
                duration_ms: 0,
                skipped_reason: (self == Self::Busy).then(|| message.to_string()),
                result: Some(serde_json::json!({
                    "status": if self == Self::Busy { "deferred" } else { "failed" },
                    "reason": reason,
                    "dependent_work_started": false,
                    "persisted": false,
                })),
                stderr_tail: (self == Self::Unavailable).then(|| message.to_string()),
            }],
            pressure: None,
            user_idle: None,
        }
    }
}

pub(super) fn acquire(data_dir: &Path) -> Result<Lease, AdmissionError> {
    let directory = schedule_dir(data_dir);
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(&directory)
        .map_err(|_| AdmissionError::Unavailable)?;
    let metadata = fs::symlink_metadata(&directory).map_err(|_| AdmissionError::Unavailable)?;
    if !metadata.file_type().is_dir() {
        return Err(AdmissionError::Unavailable);
    }
    let path = directory.join("run.lock");
    match fs::symlink_metadata(&path) {
        Ok(metadata) if !metadata.file_type().is_file() => {
            return Err(AdmissionError::Unavailable);
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(AdmissionError::Unavailable),
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(path)
        .map_err(|_| AdmissionError::Unavailable)?;
    if !file
        .metadata()
        .map_err(|_| AdmissionError::Unavailable)?
        .is_file()
    {
        return Err(AdmissionError::Unavailable);
    }
    // As with the index lock, the operator must trust the data directory and
    // its parents. These checks are not a sandbox against concurrent renames.
    match file.try_lock() {
        Ok(()) => Ok(Lease { _file: file }),
        Err(TryLockError::WouldBlock) => Err(AdmissionError::Busy),
        Err(TryLockError::Error(_)) => Err(AdmissionError::Unavailable),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    #[test]
    fn exclusive_lease_releases_on_drop_without_replacing_control_file() {
        let directory = tempfile::tempdir().unwrap();
        let lease = acquire(directory.path()).unwrap();
        let path = schedule_dir(directory.path()).join("run.lock");
        assert_eq!(acquire(directory.path()).unwrap_err(), AdmissionError::Busy);
        assert!(path.is_file());
        assert_eq!(fs::metadata(&path).unwrap().len(), 0);
        drop(lease);
        let next = acquire(directory.path()).unwrap();
        assert!(path.is_file());
        drop(next);
    }

    #[test]
    fn different_archives_do_not_block_each_other() {
        let directory = tempfile::tempdir().unwrap();
        let _first = acquire(&directory.path().join("first")).unwrap();
        let _second = acquire(&directory.path().join("second")).unwrap();
    }

    #[test]
    fn existing_control_contents_are_never_truncated() {
        let directory = tempfile::tempdir().unwrap();
        let path = schedule_dir(directory.path()).join("run.lock");
        fs::create_dir_all(schedule_dir(directory.path())).unwrap();
        fs::write(&path, "preserved coordination metadata").unwrap();
        let lease = acquire(directory.path()).unwrap();
        drop(lease);
        assert_eq!(
            fs::read_to_string(path).unwrap(),
            "preserved coordination metadata"
        );
    }

    #[test]
    fn invalid_lock_path_is_not_claimed_or_repaired() {
        let directory = tempfile::tempdir().unwrap();
        let path = schedule_dir(directory.path()).join("run.lock");
        fs::create_dir_all(&path).unwrap();
        assert_eq!(
            acquire(directory.path()).unwrap_err(),
            AdmissionError::Unavailable
        );
        assert!(path.is_dir());
        assert!(!schedule_dir(directory.path()).join("state.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_control_file_does_not_lock_or_modify_its_target() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("untouched");
        fs::write(&target, "original lock target").unwrap();
        fs::create_dir_all(schedule_dir(directory.path())).unwrap();
        std::os::unix::fs::symlink(&target, schedule_dir(directory.path()).join("run.lock"))
            .unwrap();
        assert_eq!(
            acquire(directory.path()).unwrap_err(),
            AdmissionError::Unavailable
        );
        assert_eq!(fs::read_to_string(target).unwrap(), "original lock target");
    }

    #[test]
    fn refusal_report_never_fabricates_completed_or_persisted_work() {
        for job in [ScheduleJob::Incremental, ScheduleJob::Nightly] {
            for error in [AdmissionError::Busy, AdmissionError::Unavailable] {
                let report = error.report(job);
                assert!(!report.ok);
                assert_eq!(report.steps.len(), 1);
                let step = &report.steps[0];
                assert!(!step.ok);
                assert_eq!(step.exit_code, None);
                assert!(step.argv.is_empty());
                let result = step.result.as_ref().unwrap();
                assert_eq!(result["dependent_work_started"], false);
                assert_eq!(result["persisted"], false);
                let state = super::super::ScheduleState {
                    last_incremental: (job == ScheduleJob::Incremental).then(|| report.clone()),
                    last_nightly: (job == ScheduleJob::Nightly).then(|| report.clone()),
                };
                let detail = super::super::outcomes::unit_detail(job, &state, None).unwrap();
                assert!(detail.starts_with(if error == AdmissionError::Busy {
                    "last run deferred:"
                } else {
                    "last run failed:"
                }));
            }
        }
    }

    fn persistence_fixture(directory: &Path) -> (super::super::RunConfig, JobReport) {
        (
            super::super::RunConfig {
                binary: directory.join("not-executed"),
                data_dir: directory.to_path_buf(),
                db_path: directory.join("untouched.db"),
                semantic: false,
                max_backfill_batches: 1,
            },
            JobReport {
                job: ScheduleJob::Incremental,
                started_ms: 1,
                finished_ms: 2,
                ok: true,
                skipped_reason: None,
                steps: Vec::new(),
                pressure: None,
                user_idle: None,
            },
        )
    }

    #[test]
    fn state_write_failure_is_returned_and_recorded_in_history() {
        let directory = tempfile::tempdir().unwrap();
        let _lease = acquire(directory.path()).unwrap();
        let state_path = super::super::state_path(directory.path());
        fs::create_dir(&state_path).unwrap();
        let (cfg, report) = persistence_fixture(directory.path());
        let report = super::super::persist_job_report(&cfg, report);
        assert!(!report.ok);
        assert_eq!(
            report.steps[0].result.as_ref().unwrap()["reason"],
            "schedule_state_write_failed"
        );
        let raw = fs::read_to_string(super::super::runs_log_path(directory.path())).unwrap();
        let saved: JobReport = serde_json::from_str(raw.trim()).unwrap();
        assert!(!saved.ok);
        assert!(
            state_path.is_dir(),
            "failed publication never deletes evidence"
        );
        assert!(!cfg.db_path.exists());
    }

    #[test]
    fn history_write_failure_corrects_previously_saved_success() {
        let directory = tempfile::tempdir().unwrap();
        let _lease = acquire(directory.path()).unwrap();
        let history_path = super::super::runs_log_path(directory.path());
        fs::create_dir(&history_path).unwrap();
        let (cfg, report) = persistence_fixture(directory.path());
        let report = super::super::persist_job_report(&cfg, report);
        assert!(!report.ok);
        assert_eq!(
            report.steps[0].result.as_ref().unwrap()["reason"],
            "schedule_history_write_failed"
        );
        let state = super::super::load_state(directory.path());
        assert!(!state.last_incremental.unwrap().ok);
        assert!(
            history_path.is_dir(),
            "failed history append never deletes evidence"
        );
        assert!(!cfg.db_path.exists());
    }

    // Invoked only by the cross-process test below; ordinary suite execution
    // returns without touching the filesystem or waiting for input.
    #[test]
    fn lock_child() {
        if !std::env::args().any(|arg| arg == "--exact") {
            return;
        }
        let Ok(directory) = dotenvy::var("CASS_SCHEDULE_LOCK_TEST_DIRECTORY") else {
            return;
        };
        let directory = Path::new(&directory);
        let _lease = acquire(directory).unwrap();
        fs::write(directory.join("child-ready"), b"ready").unwrap();
        // Parent holds stdin open and terminates this test child deliberately.
        let mut byte = [0u8; 1];
        let _ = std::io::Read::read(&mut std::io::stdin(), &mut byte);
    }

    struct ChildGuard(Child);

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn process_death_releases_lease_without_stale_pid_recovery() {
        let directory = tempfile::tempdir().unwrap();
        let mut child = ChildGuard(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "schedule::execution::tests::lock_child",
                    "--nocapture",
                ])
                .env("CASS_SCHEDULE_LOCK_TEST_DIRECTORY", directory.path())
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while !directory.path().join("child-ready").exists() {
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "lock child exited early"
            );
            assert!(Instant::now() < deadline, "lock child readiness timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(acquire(directory.path()).unwrap_err(), AdmissionError::Busy);
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        let _recovered = acquire(directory.path()).unwrap();
        assert!(schedule_dir(directory.path()).join("run.lock").is_file());
    }
}
