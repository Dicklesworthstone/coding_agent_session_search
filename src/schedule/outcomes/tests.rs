use super::*;

fn index_step(job: ScheduleJob, exit_code: Option<i32>, ok: bool) -> StepReport {
    StepReport {
        name: match job {
            ScheduleJob::Incremental => "index",
            ScheduleJob::Nightly => "index-full",
        }
        .to_string(),
        argv: Vec::new(),
        exit_code,
        ok,
        duration_ms: 1,
        skipped_reason: None,
        result: None,
        stderr_tail: None,
    }
}

fn report(job: ScheduleJob, step: StepReport, ok: bool) -> JobReport {
    JobReport {
        job,
        started_ms: 1,
        finished_ms: 2,
        ok,
        skipped_reason: None,
        steps: vec![step],
        pressure: None,
        user_idle: None,
    }
}

#[test]
fn legacy_busy_receipts_are_not_successful_for_either_job() {
    let state = ScheduleState {
        last_incremental: Some(report(
            ScheduleJob::Incremental,
            index_step(ScheduleJob::Incremental, Some(7), true),
            true,
        )),
        last_nightly: Some(report(
            ScheduleJob::Nightly,
            index_step(ScheduleJob::Nightly, Some(7), true),
            true,
        )),
    };
    let state = normalize_state(state);
    for report in [state.last_incremental, state.last_nightly]
        .into_iter()
        .flatten()
    {
        assert!(!report.ok);
        assert!(!report.steps[0].ok);
        assert_eq!(report.steps[0].exit_code, Some(7));
    }
}

#[test]
fn normalization_preserves_independent_failures_and_non_index_exit_codes() {
    let mut step = index_step(ScheduleJob::Incremental, Some(7), true);
    step.name = "sources-sync:remote".to_string();
    let state = normalize_state(ScheduleState {
        last_incremental: Some(report(ScheduleJob::Incremental, step, false)),
        last_nightly: None,
    });
    let report = state.last_incremental.unwrap();
    assert!(!report.ok);
    assert!(report.steps[0].ok);
    assert!(state.last_nightly.is_none());
}

#[test]
fn successful_probe_does_not_hide_failed_indexing() {
    let state = ScheduleState {
        last_incremental: Some(report(
            ScheduleJob::Incremental,
            index_step(ScheduleJob::Incremental, Some(70), false),
            false,
        )),
        last_nightly: None,
    };
    assert_eq!(
        unit_detail(ScheduleJob::Incremental, &state, Some("exit 0".into())),
        Some("last run failed: index (exit 70); scheduler probe: exit 0".into())
    );
}

#[test]
fn busy_is_reported_as_deferred_even_before_legacy_normalization() {
    let legacy = report(
        ScheduleJob::Nightly,
        index_step(ScheduleJob::Nightly, Some(7), true),
        true,
    );
    assert_eq!(run_detail(&legacy), "last run deferred: index-full (exit 7)");
}

#[test]
fn real_failure_takes_precedence_over_busy_deferral() {
    let mut report = report(
        ScheduleJob::Nightly,
        index_step(ScheduleJob::Nightly, Some(7), false),
        false,
    );
    let mut sync = index_step(ScheduleJob::Nightly, Some(1), false);
    sync.name = "sources-sync:remote".to_string();
    report.steps.insert(0, sync);
    assert_eq!(
        run_detail(&report),
        "last run failed: sources-sync:remote (exit 1), index-full (exit 7)"
    );
}

#[test]
fn missing_probe_does_not_erase_the_persisted_outcome() {
    let state = ScheduleState {
        last_incremental: None,
        last_nightly: Some(report(
            ScheduleJob::Nightly,
            index_step(ScheduleJob::Nightly, None, false),
            false,
        )),
    };
    assert_eq!(
        unit_detail(ScheduleJob::Nightly, &state, None),
        Some("last run failed: index-full (no exit status)".into())
    );
    assert_eq!(unit_detail(ScheduleJob::Incremental, &state, None), None);
}

#[test]
fn successful_probe_without_a_receipt_does_not_claim_completed_work() {
    assert_eq!(
        unit_detail(
            ScheduleJob::Incremental,
            &ScheduleState::default(),
            Some("exit 0".into())
        ),
        Some("no recorded run; scheduler probe: exit 0".into())
    );
}

#[test]
fn gated_runs_and_completed_runs_remain_distinct() {
    let mut report = report(
        ScheduleJob::Incremental,
        index_step(ScheduleJob::Incremental, Some(0), true),
        true,
    );
    assert_eq!(run_detail(&report), "last run completed");
    report.steps.clear();
    report.skipped_reason = Some("machine under severe load".into());
    assert_eq!(
        run_detail(&report),
        "last run skipped: machine under severe load"
    );
    report.ok = false;
    assert_eq!(run_detail(&report), "last run failed");
}

#[test]
fn reading_legacy_receipts_does_not_rewrite_history() {
    let dir = tempfile::tempdir().unwrap();
    let path = crate::schedule::state_path(dir.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let state = ScheduleState {
        last_incremental: Some(report(
            ScheduleJob::Incremental,
            index_step(ScheduleJob::Incremental, Some(7), true),
            true,
        )),
        last_nightly: None,
    };
    let original = serde_json::to_vec_pretty(&state).unwrap();
    std::fs::write(&path, &original).unwrap();
    let loaded = crate::schedule::load_state(dir.path());
    assert!(!loaded.last_incremental.unwrap().ok);
    assert_eq!(std::fs::read(&path).unwrap(), original);
}

// Run execution tests in a subprocess so changing HOME and source discovery
// configuration never mutates the environment of parallel Rust tests. The child
// executable is a deterministic fixture, not the user's installed cass binary.
#[cfg(unix)]
#[test]
fn indexing_outcomes_control_downstream_work_and_persisted_receipts() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    let dir = tempfile::tempdir().unwrap();
    let fixture = dir.path().join("cass-fixture");
    std::fs::write(
        &fixture,
        r##"#!/bin/sh
printf '%s\n' "$*" >> "$CASS_SCHEDULE_OUTCOME_TEST_TRACE"
case " $* " in
  *" index "*)
    printf '{"success":%s}\n' "$CASS_SCHEDULE_OUTCOME_TEST_SUCCESS"
    exit "$CASS_SCHEDULE_OUTCOME_TEST_EXIT"
    ;;
  *" models status "*) printf '{"installed":true}\n' ;;
  *" models backfill "*)
    printf '{"status":"published","batches_attempted":1,"batches_completed":1,"model_initializations":1}\n'
    ;;
  *) printf 'unexpected fixture invocation\n' >&2; exit 64 ;;
esac
"##,
    )
    .unwrap();
    std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o700)).unwrap();
    for job in ["incremental", "nightly"] {
        for code in [0, 7, 9, 70] {
            let case = dir.path().join(format!("{job}-{code}"));
            let home = case.join("home");
            std::fs::create_dir_all(&home).unwrap();
            let trace = case.join("trace");
            let loader_environment = [
                "LD_LIBRARY_PATH",
                "DYLD_LIBRARY_PATH",
                "DYLD_FALLBACK_LIBRARY_PATH",
            ]
            .into_iter()
            .filter_map(|key| std::env::var_os(key).map(|value| (key, value)));
            let output = Command::new(std::env::current_exe().unwrap())
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .envs(loader_environment)
                .args([
                    "--exact",
                    "schedule::outcomes::tests::indexing_outcome_child",
                    "--nocapture",
                ])
                .env("CASS_SCHEDULE_OUTCOME_TEST_CHILD", &case)
                .env("CASS_SCHEDULE_OUTCOME_TEST_BINARY", &fixture)
                .env("CASS_SCHEDULE_OUTCOME_TEST_JOB", job)
                .env("CASS_SCHEDULE_OUTCOME_TEST_EXIT", code.to_string())
                .env(
                    "CASS_SCHEDULE_OUTCOME_TEST_SUCCESS",
                    if code == 0 { "true" } else { "false" },
                )
                .env("CASS_SCHEDULE_OUTCOME_TEST_TRACE", &trace)
                .env("HOME", &home)
                .env("XDG_CONFIG_HOME", home.join(".config"))
                .env("XDG_DATA_HOME", home.join(".local/share"))
                .env("CASS_DATA_DIR", &case)
                .current_dir(&case)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{job}/{code}: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let calls = std::fs::read_to_string(&trace).unwrap();
            assert!(calls.contains("index"));
            assert_eq!(
                calls.contains("models backfill"),
                job == "nightly" && code == 0,
                "{job}/{code}: {calls}"
            );
            assert_eq!(
                calls.contains("models status"),
                job == "nightly" && code == 0,
                "{job}/{code}: {calls}"
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn indexing_outcome_child() {
    let Some(root) = std::env::var_os("CASS_SCHEDULE_OUTCOME_TEST_CHILD") else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    let job = match std::env::var("CASS_SCHEDULE_OUTCOME_TEST_JOB")
        .unwrap()
        .as_str()
    {
        "incremental" => ScheduleJob::Incremental,
        "nightly" => ScheduleJob::Nightly,
        other => panic!("unexpected fixture job: {other}"),
    };
    let code: i32 = std::env::var("CASS_SCHEDULE_OUTCOME_TEST_EXIT")
        .unwrap()
        .parse()
        .unwrap();
    let cfg = crate::schedule::RunConfig {
        binary: std::env::var_os("CASS_SCHEDULE_OUTCOME_TEST_BINARY")
            .unwrap()
            .into(),
        data_dir: root.clone(),
        db_path: root.join("agent_search.db"),
        semantic: true,
        max_backfill_batches: 2,
    };
    let report = crate::schedule::run_job_unconditionally(job, &cfg);
    assert!(report.skipped_reason.is_none());
    assert_eq!(report.ok, code == 0);
    let index = report
        .steps
        .iter()
        .find(|step| matches!(step.name.as_str(), "index" | "index-full"))
        .unwrap();
    assert_eq!(index.exit_code, Some(code));
    assert_eq!(index.ok, code == 0);
    if code == 7 {
        assert!(index.skipped_reason.is_some());
    }
    let persisted = crate::schedule::load_state(&root);
    let saved = match job {
        ScheduleJob::Incremental => persisted.last_incremental,
        ScheduleJob::Nightly => persisted.last_nightly,
    }
    .unwrap();
    assert_eq!(saved.ok, code == 0);
    let history = std::fs::read_to_string(crate::schedule::runs_log_path(&root)).unwrap();
    let receipts: Vec<JobReport> = history
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].ok, code == 0);
}
