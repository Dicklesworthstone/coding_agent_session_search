use super::*;
use crate::storage::sqlite::FrankenStorage;

#[test]
fn gh514_source_scan_watermark_supersedes_older_index_publication() {
    let indexed = 1_800_000_000_000_i64;
    let scanned = indexed + 240_000;
    let freshness = serde_json::json!({
        "last_indexed_at": indexed,
        "source_scan_completed_at_ms": scanned,
    });
    assert_eq!(last_indexed_at_ms_from_freshness(&freshness), Some(scanned));

    let malformed_scan = serde_json::json!({
        "last_indexed_at": indexed,
        "source_scan_completed_at_ms": "not-a-time",
    });
    assert_eq!(
        last_indexed_at_ms_from_freshness(&malformed_scan),
        Some(indexed)
    );
}

#[test]
fn gh514_noop_scan_resets_source_scan_cadence_without_republishing() {
    let indexed = 1_800_000_000_000_i64;
    let scanned = indexed + 600_000;
    let interval = Duration::from_secs(DEFAULT_SOURCE_SCAN_SECS);
    let freshness = serde_json::json!({
        "exists": true,
        "stale": false,
        "partial": false,
        "pending_sessions": 0,
        "last_indexed_at": indexed,
        "source_scan_completed_at_ms": scanned,
    });

    assert_eq!(
        catch_up_reason_at(&freshness, scanned + 299_999, interval),
        None
    );
    assert_eq!(
        catch_up_reason_at(&freshness, scanned + 300_000, interval),
        Some("source-scan-overdue")
    );
}

#[test]
fn gh514_source_scan_due_handles_missing_boundaries_and_clock_corrections() {
    let now = 1_800_000_000_000_i64;
    let interval = Duration::from_secs(300);
    assert!(source_scan_is_due(None, now, interval));
    assert!(!source_scan_is_due(Some(now), now, interval));
    assert!(!source_scan_is_due(Some(now - 299_999), now, interval));
    assert!(source_scan_is_due(Some(now - 300_000), now, interval));
    assert!(!source_scan_is_due(Some(now + 1), now, interval));
    assert!(!source_scan_is_due(None, now, Duration::ZERO));
}

#[test]
fn gh514_canonical_meta_last_scan_drives_the_same_policy() {
    let temp = tempfile::tempdir().expect("tempdir");
    let db_path = temp.path().join("agent_search.db");
    let storage = FrankenStorage::open(&db_path).expect("open canonical archive");
    assert_eq!(source_scan_completed_at_ms_from_db(&db_path), None);

    let scan = 1_800_000_000_000_i64;
    storage.set_last_scan_ts(scan).expect("record source scan");
    drop(storage);

    assert_eq!(source_scan_completed_at_ms_from_db(&db_path), Some(scan));
    assert!(!source_scan_is_due(
        source_scan_completed_at_ms_from_db(&db_path),
        scan + 299_999,
        Duration::from_secs(300),
    ));
    assert!(source_scan_is_due(
        source_scan_completed_at_ms_from_db(&db_path),
        scan + 300_000,
        Duration::from_secs(300),
    ));
}

#[test]
fn gh514_successful_noop_scan_resets_failed_child_breaker() {
    let spawn = 1_800_000_000_000_i64;
    let scan = spawn + 1_000;
    let freshness = serde_json::json!({
        "last_indexed_at": spawn - 60_000,
        "source_scan_completed_at_ms": scan,
    });
    let watermark = last_indexed_at_ms_from_freshness(&freshness);
    let mut state = AutoRefreshState {
        last_spawn_ms: spawn,
        last_pid: 42,
        last_reason: "source-scan-overdue".to_owned(),
        consecutive_failures: 2,
        last_failure_detected_ms: spawn - 1,
        last_failure: Some("old failure".to_owned()),
        ..AutoRefreshState::default()
    };

    assert_eq!(
        judge_previous_spawn(
            &mut state,
            watermark,
            scan + 10,
            Path::new("/tmp/auto-refresh.log"),
        ),
        BreakerVerdict::Closed
    );
    assert_eq!(state.consecutive_failures, 0);
    assert_eq!(state.last_failure, None);
    assert_eq!(state.last_failure_detected_ms, 0);
}

#[test]
fn gh514_enrich_freshness_with_source_scan_watermark_suppresses_overdue_scan() {
    let temp = tempfile::tempdir().expect("tempdir");
    let db_path = temp.path().join("agent_search.db");
    let storage = FrankenStorage::open(&db_path).expect("open canonical archive");
    let indexed = 1_800_000_000_000_i64;
    let scanned = indexed + 600_000;
    storage
        .set_last_scan_ts(scanned)
        .expect("record source scan");
    drop(storage);

    let interval = Duration::from_secs(DEFAULT_SOURCE_SCAN_SECS);
    let base_freshness = serde_json::json!({
        "exists": true,
        "stale": false,
        "partial": false,
        "pending_sessions": 0,
        "last_indexed_at": indexed,
    });
    assert_eq!(
        catch_up_reason_at(&base_freshness, scanned + 120_000, interval),
        Some("source-scan-overdue"),
        "without DB scan watermark enrichment, old last_indexed_at triggers source-scan-overdue"
    );

    let enriched = enrich_freshness_with_source_scan_watermark(&base_freshness, &db_path)
        .expect("enrich freshness with canonical last_scan_ts");
    assert_eq!(
        catch_up_reason_at(&enriched, scanned + 120_000, interval),
        None,
        "enriched freshness honors recent no-op source scan"
    );
    assert_eq!(
        catch_up_reason_at(&enriched, scanned + 300_000, interval),
        Some("source-scan-overdue"),
        "once canonical last_scan_ts also ages past interval, source-scan-overdue fires"
    );
}
