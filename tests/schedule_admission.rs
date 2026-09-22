//! Real CLI coverage for source planning and whole-job scheduler admission.
//! No live scheduler is installed, no network source is contacted, and the
//! deliberately invalid archive must never be opened by these refused runs.

use serde_json::Value;
use std::fs::{self, File};
use std::path::PathBuf;
use std::process::{Command, Output};

struct Fixture {
    _temporary: tempfile::TempDir,
    home: PathBuf,
    data: PathBuf,
}

impl Fixture {
    fn new(config: &str, ledger: Option<&str>) -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("home");
        let data = temporary.path().join("data");
        fs::create_dir_all(home.join(".config/cass")).unwrap();
        fs::create_dir_all(&data).unwrap();
        // Stop dotenvy's ancestor search without touching any real .env file.
        File::create_new(home.join(".env")).unwrap();
        fs::write(home.join(".config/cass/sources.toml"), config).unwrap();
        if let Some(ledger) = ledger {
            fs::write(data.join("sync_status.json"), ledger).unwrap();
        }
        fs::write(data.join("agent_search.db"), Self::archive_bytes()).unwrap();
        Self {
            _temporary: temporary,
            home,
            data,
        }
    }

    fn archive_bytes() -> &'static [u8] {
        b"private-archive-sentinel: admission must not open me"
    }

    fn run(&self, job: &str) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cass"));
        command.env_clear();
        for key in ["PATH", "SystemRoot", "WINDIR"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        command
            .current_dir(&self.home)
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_DATA_HOME", self.home.join(".local/share"))
            .env("CLAUDE_CONFIG_DIR", self.home.join(".claude"))
            .env("CODEX_HOME", self.home.join(".codex"))
            .env("RUST_MIN_STACK", "134217728")
            .env("TUI_HEADLESS", "1")
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .env("CASS_RESPONSIVENESS_DISABLE", "1")
            .env("CASS_AUTO_REFRESH", "0")
            .args([
                "schedule",
                "run",
                "--job",
                job,
                "--force",
                "--json",
                "--data-dir",
            ])
            .arg(&self.data)
            .output()
            .expect("execute the actual scheduled-run admission path")
    }

    fn assert_archive_unchanged(&self) {
        assert_eq!(
            fs::read(self.data.join("agent_search.db")).unwrap(),
            Self::archive_bytes()
        );
        for suffix in ["-wal", "-shm", "-journal"] {
            assert!(!self.data.join(format!("agent_search.db{suffix}")).exists());
        }
    }
}

fn refused(output: &Output, job: &str, step: &str, reason: &str) -> Value {
    assert_eq!(
        output.status.code(),
        Some(9),
        "scheduler failure must reach the caller"
    );
    let report: Value = serde_json::from_slice(&output.stdout).expect("exactly one JSON report");
    assert_eq!(report["job"], job);
    assert_eq!(report["ok"], false);
    let steps = report["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 1, "no dependent process may start");
    assert_eq!(steps[0]["name"], step);
    assert_eq!(steps[0]["ok"], false);
    assert!(
        steps[0]["exit_code"].is_null(),
        "there is no child exit status"
    );
    assert!(steps[0]["argv"].as_array().unwrap().is_empty());
    assert_eq!(steps[0]["result"]["reason"], reason);
    assert_eq!(steps[0]["result"]["dependent_work_started"], false);
    for bytes in [&output.stdout, &output.stderr] {
        assert!(!String::from_utf8_lossy(bytes).contains("private-"));
    }
    report
}

fn assert_discovery_failure(config: &str, ledger: Option<&str>, reason: &str) {
    let fixture = Fixture::new(config, ledger);
    for job in ["incremental", "nightly"] {
        refused(&fixture.run(job), job, "sources-discovery", reason);
        fixture.assert_archive_unchanged();
        assert_eq!(
            fs::read_to_string(fixture.home.join(".config/cass/sources.toml")).unwrap(),
            config
        );
        let ledger_path = fixture.data.join("sync_status.json");
        match ledger {
            Some(expected) => assert_eq!(fs::read_to_string(ledger_path).unwrap(), expected),
            None => assert!(!ledger_path.exists()),
        }
        let state: Value =
            serde_json::from_slice(&fs::read(fixture.data.join("schedule/state.json")).unwrap())
                .unwrap();
        assert_eq!(state[format!("last_{job}")]["ok"], false);
    }
    let history = fs::read_to_string(fixture.data.join("schedule/runs.jsonl")).unwrap();
    let reports: Vec<Value> = history
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(reports.len(), 2);
    assert!(reports.iter().all(|report| report["ok"] == false));
}

#[test]
fn malformed_configuration_stops_both_real_jobs_without_disclosing_content() {
    assert_discovery_failure(
        "private-configuration-sentinel = [unterminated",
        None,
        "source_configuration_unavailable",
    );
}

#[test]
fn malformed_sync_history_cannot_reset_backoff_or_start_any_child() {
    assert_discovery_failure(
        "[[sources]]\nname = 'fixture'\ntype = 'ssh'\nhost = 'fixture.invalid'\npaths = ['~/.claude/projects']\nsync_schedule = 'hourly'\n",
        Some("{\"private-ledger-sentinel\":"),
        "source_sync_history_unavailable",
    );
}

#[test]
fn both_forced_jobs_respect_a_cross_process_lease_and_preserve_owner_receipts() {
    let fixture = Fixture::new("private-configuration-sentinel = [unterminated", None);
    let schedule = fixture.data.join("schedule");
    fs::create_dir(&schedule).unwrap();
    let state = br#"{"last_incremental":null,"last_nightly":null}"#;
    let history = b"private-owner-history-sentinel\n";
    fs::write(schedule.join("state.json"), state).unwrap();
    fs::write(schedule.join("runs.jsonl"), history).unwrap();
    let lock = File::create_new(schedule.join("run.lock")).unwrap();
    lock.try_lock().unwrap();
    for job in ["incremental", "nightly"] {
        let report = refused(
            &fixture.run(job),
            job,
            "schedule-admission",
            "schedule_busy",
        );
        assert_eq!(report["steps"][0]["result"]["status"], "deferred");
        assert_eq!(report["steps"][0]["result"]["persisted"], false);
        fixture.assert_archive_unchanged();
        assert_eq!(fs::read(schedule.join("state.json")).unwrap(), state);
        assert_eq!(fs::read(schedule.join("runs.jsonl")).unwrap(), history);
        assert!(!schedule.join(format!("{job}.log")).exists());
        assert!(!schedule.join("state.json.tmp").exists());
    }
    // A later invocation can acquire the same inode and reaches source
    // discovery; no stale-lock cleanup or force override is needed.
    drop(lock);
    refused(
        &fixture.run("incremental"),
        "incremental",
        "sources-discovery",
        "source_configuration_unavailable",
    );
    fixture.assert_archive_unchanged();
    assert!(schedule.join("run.lock").is_file());
}

#[test]
fn invalid_lock_control_is_a_visible_failure_not_an_unlocked_run() {
    let fixture = Fixture::new("private-configuration-sentinel = [unterminated", None);
    let schedule = fixture.data.join("schedule");
    fs::create_dir_all(schedule.join("run.lock")).unwrap();
    for job in ["incremental", "nightly"] {
        let report = refused(
            &fixture.run(job),
            job,
            "schedule-admission",
            "schedule_lock_unavailable",
        );
        assert_eq!(report["steps"][0]["result"]["persisted"], false);
        assert_eq!(report["steps"][0]["result"]["status"], "failed");
        fixture.assert_archive_unchanged();
        assert!(schedule.join("run.lock").is_dir());
        assert!(!schedule.join("state.json").exists());
        assert!(!schedule.join("runs.jsonl").exists());
    }
}
