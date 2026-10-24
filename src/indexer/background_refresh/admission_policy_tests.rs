//! The real refresh path must reject disabled requests before archive access.
use super::*;
use std::cell::Cell;

fn blocked_state(spawn_ms: i64) -> AutoRefreshState {
    AutoRefreshState {
        last_spawn_ms: spawn_ms,
        last_pid: 4242,
        last_reason: "index-stale".into(),
        consecutive_failures: FAILURE_TRIP_THRESHOLD,
        failure_counted_for_spawn_ms: spawn_ms,
        last_failure_detected_ms: spawn_ms,
        last_failure: Some("preserve this failure".into()),
    }
}

#[test]
fn disabled_refresh_never_reads_the_archive_or_creates_runtime_directories() {
    let dir = tempfile::tempdir().unwrap();
    let absent = dir.path().join("must-not-be-created");
    let outcome = refresh_with_watermark_reader(
        &absent,
        &absent.join("agent_search.db"),
        "index-stale",
        Some(1),
        AutoRefreshPolicy {
            enabled: false,
            cooldown: Duration::ZERO,
        },
        |_| panic!("disabled auto-refresh must not open the archive"),
    );
    assert_eq!(outcome, AutoRefreshOutcome::Disabled);
    assert!(!absent.exists());
}

#[test]
fn disabled_admission_does_not_probe_even_an_unusable_data_dir() {
    let dir = tempfile::tempdir().unwrap();
    let not_a_directory = dir.path().join("ordinary-file");
    std::fs::write(&not_a_directory, b"unchanged").unwrap();
    let outcome = refresh_with_watermark_reader(
        &not_a_directory,
        &not_a_directory.join("agent_search.db"),
        "source-scan-overdue",
        None,
        AutoRefreshPolicy {
            enabled: false,
            cooldown: Duration::ZERO,
        },
        |_| panic!("disabled path must be independent of database availability"),
    );
    assert_eq!(outcome, AutoRefreshOutcome::Disabled);
    assert_eq!(std::fs::read(&not_a_directory).unwrap(), b"unchanged");
}

#[test]
fn an_unavailable_scan_watermark_does_not_reopen_a_tripped_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let state = blocked_state(now_ms() - 10_000);
    save_state(dir.path(), &state).unwrap();
    let reads = Cell::new(0);
    let db_path = dir.path().join("agent_search.db");
    let outcome = refresh_with_watermark_reader(
        dir.path(),
        &db_path,
        "index-stale",
        None,
        AutoRefreshPolicy {
            enabled: true,
            cooldown: Duration::ZERO,
        },
        |requested| {
            assert_eq!(requested, db_path.as_path());
            reads.set(reads.get() + 1);
            None
        },
    );
    assert_eq!(reads.get(), 1);
    assert!(matches!(outcome, AutoRefreshOutcome::Tripped { .. }));
    assert_eq!(load_state(dir.path()).unwrap(), state);
    assert!(!log_path(dir.path()).exists());
    assert!(!db_path.exists());
}

#[test]
fn a_successful_no_op_scan_still_resets_the_breaker_without_a_new_lexical_publish() {
    let dir = tempfile::tempdir().unwrap();
    let spawn_ms = now_ms() - 1_000;
    let state = blocked_state(spawn_ms);
    save_state(dir.path(), &state).unwrap();
    let outcome = refresh_with_watermark_reader(
        dir.path(),
        &dir.path().join("agent_search.db"),
        "index-stale",
        Some(spawn_ms - 1),
        AutoRefreshPolicy {
            enabled: true,
            cooldown: Duration::from_secs(86_400),
        },
        |_| Some(spawn_ms + 1),
    );
    // A long cooldown ensures this test never launches the test executable.
    assert!(matches!(outcome, AutoRefreshOutcome::Cooldown { .. }));
    let reset = load_state(dir.path()).unwrap();
    assert_eq!(reset.consecutive_failures, 0);
    assert_eq!(reset.failure_counted_for_spawn_ms, spawn_ms);
    assert!(reset.last_failure.is_none());
    assert!(!log_path(dir.path()).exists());
}

#[test]
fn sentinel_scan_metadata_does_not_replace_real_lexical_success() {
    for scan_watermark in [None, Some(0), Some(-1), Some(i64::MIN)] {
        let dir = tempfile::tempdir().unwrap();
        let spawn_ms = now_ms() - 1_000;
        save_state(dir.path(), &blocked_state(spawn_ms)).unwrap();
        let outcome = refresh_with_watermark_reader(
            dir.path(),
            &dir.path().join("agent_search.db"),
            "index-stale",
            Some(spawn_ms),
            AutoRefreshPolicy {
                enabled: true,
                cooldown: Duration::from_secs(86_400),
            },
            |_| scan_watermark,
        );
        assert!(matches!(outcome, AutoRefreshOutcome::Cooldown { .. }));
        assert_eq!(load_state(dir.path()).unwrap().consecutive_failures, 0);
        assert!(!log_path(dir.path()).exists());
    }
}
