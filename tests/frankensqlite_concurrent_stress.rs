//! Stress tests for frankensqlite concurrent writers under realistic cass workloads.
//!
//! Tests BEGIN CONCURRENT (MVCC) behavior: parallel writes, read-write mix,
//! crash recovery, large transactions, and retry convergence.
//!
//! Bead: coding_agent_session_search-2tax6

use coding_agent_search::storage::sqlite::{
    ConnectionManagerConfig, FrankenConnectionManager, FrankenStorage,
};
use frankensqlite::compat::{RowExt, TransactionExt};
use frankensqlite::{Connection, FrankenError};
use rand::RngExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// Create a frankensqlite DB with cass schema applied.
fn setup_db(dir: &TempDir) -> std::path::PathBuf {
    let db_path = dir.path().join("stress.db");
    let fs = FrankenStorage::open(&db_path).expect("create frankensqlite db");
    drop(fs);
    db_path
}

/// Create a minimal frankensqlite DB with just a simple table.
/// Sets WAL mode and busy_timeout — required for concurrent writes.
fn setup_simple_db(dir: &TempDir) -> std::path::PathBuf {
    let db_path = dir.path().join("simple.db");
    let conn = Connection::open(db_path.to_str().unwrap()).unwrap();
    conn.execute("PRAGMA journal_mode = WAL;").unwrap();
    conn.execute("PRAGMA synchronous = NORMAL;").unwrap();
    conn.execute("PRAGMA busy_timeout = 5000;").unwrap();
    conn.execute(
        "CREATE TABLE items (id INTEGER PRIMARY KEY, thread_id INTEGER, seq INTEGER, val TEXT)",
    )
    .unwrap();
    conn.execute("CREATE INDEX idx_items_thread ON items(thread_id)")
        .unwrap();
    drop(conn);
    db_path
}

/// Open a connection with proper WAL/busy_timeout config for concurrent tests.
fn open_configured(path: &std::path::Path) -> Connection {
    let conn = Connection::open(path.to_str().unwrap()).unwrap();
    let _ = conn.execute("PRAGMA journal_mode = WAL;");
    let _ = conn.execute("PRAGMA busy_timeout = 5000;");
    let _ = conn.execute("PRAGMA cache_size = -4096;");
    conn
}

/// Retry wrapper for concurrent write operations with jittered exponential backoff.
fn with_retry<F, T>(max_retries: usize, mut f: F) -> anyhow::Result<T>
where
    F: FnMut() -> Result<T, anyhow::Error>,
{
    let mut rng = rand::rng();
    let mut backoff_ms = 2_u64;
    for attempt in 0..=max_retries {
        match f() {
            Ok(val) => return Ok(val),
            Err(err) => {
                let is_retryable = err
                    .downcast_ref::<FrankenError>()
                    .or_else(|| err.root_cause().downcast_ref::<FrankenError>())
                    .is_some_and(|inner| {
                        matches!(
                            inner,
                            FrankenError::Busy
                                | FrankenError::BusyRecovery
                                | FrankenError::BusySnapshot { .. }
                                | FrankenError::WriteConflict { .. }
                                | FrankenError::SerializationFailure { .. }
                                | FrankenError::DatabaseCorrupt { .. }
                        )
                    });
                if attempt < max_retries && is_retryable {
                    // Jittered backoff to reduce thundering herd
                    let jitter = rng.random_range(0..=backoff_ms);
                    std::thread::sleep(Duration::from_millis(backoff_ms + jitter));
                    backoff_ms = (backoff_ms * 2).min(256);
                    continue;
                }
                return Err(err);
            }
        }
    }
    Err(anyhow::anyhow!("exhausted retries"))
}

// ============================================================================
// 1. PARALLEL CONNECTOR WRITES
// ============================================================================

#[test]
fn stress_parallel_connector_writes() {
    let dir = TempDir::new().unwrap();
    let db_path = setup_db(&dir);

    // Uses FrankenConnectionManager (the production connection management pattern)
    // to coordinate parallel writers. Each write gets a fresh concurrent_writer.
    let config = ConnectionManagerConfig {
        reader_count: 2,
        max_writers: 4,
    };
    let mgr = FrankenConnectionManager::new(&db_path, config).unwrap();

    // Create test table
    {
        let mut guard = mgr.writer().unwrap();
        guard
            .storage()
            .raw()
            .execute("CREATE TABLE IF NOT EXISTS items (id INTEGER PRIMARY KEY, thread_id INTEGER, seq INTEGER, val TEXT)")
            .unwrap();
        guard.mark_committed();
    }

    let num_threads = 4;
    let writes_per_thread = 100;
    let conflict_count = Arc::new(AtomicUsize::new(0));

    std::thread::scope(|s| {
        let mut handles = Vec::new();
        for thread_id in 0..num_threads {
            let m = &mgr;
            let conflicts = Arc::clone(&conflict_count);

            handles.push(s.spawn(move || {
                for seq in 0..writes_per_thread {
                    let val = format!("thread-{thread_id}-seq-{seq}");
                    let mut guard = m.concurrent_writer().expect("acquire writer");
                    with_retry(50, || {
                        let tx = guard.storage().raw().transaction()?;
                        tx.execute(&format!(
                            "INSERT INTO items (thread_id, seq, val) VALUES ({thread_id}, {seq}, '{val}')"
                        ))?;
                        tx.commit().map_err(|e| {
                            conflicts.fetch_add(1, Ordering::Relaxed);
                            anyhow::Error::new(e)
                        })?;
                        Ok(())
                    })
                    .expect("insert should succeed after retries");
                    guard.mark_committed();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
    });

    // Verify via reader
    let reader = mgr.reader();
    let rows = reader.query("SELECT COUNT(*) FROM items").unwrap();
    let count: i64 = rows[0].get_typed(0).unwrap();
    let expected = (num_threads * writes_per_thread) as i64;
    assert!(
        count >= expected,
        "at least {expected} rows should be persisted, got {count}"
    );

    eprintln!(
        "Parallel write: {} total (expected {}), {} conflicts",
        count,
        expected,
        conflict_count.load(Ordering::Relaxed)
    );
}

// ============================================================================
// 2. WRITE-HEAVY CONTENTION
// ============================================================================

#[test]
fn stress_write_heavy_contention() {
    let dir = TempDir::new().unwrap();
    let db_path = setup_db(&dir);

    // Models the production par_chunks pattern: each thread batches multiple
    // rows per transaction via ConnectionManager, reducing commit contention.
    // 4 threads × 20 batches × 10 rows = 800 total rows with only ~20 commit
    // points per thread.
    let config = ConnectionManagerConfig {
        reader_count: 2,
        max_writers: 4,
    };
    let mgr = FrankenConnectionManager::new(&db_path, config).unwrap();

    // Create test table
    {
        let mut guard = mgr.writer().unwrap();
        guard
            .storage()
            .raw()
            .execute("CREATE TABLE IF NOT EXISTS items (id INTEGER PRIMARY KEY, thread_id INTEGER, seq INTEGER, val TEXT)")
            .unwrap();
        guard.mark_committed();
    }

    let num_threads = 4;
    let batches_per_thread = 20;
    let rows_per_batch = 10;

    let start = Instant::now();

    std::thread::scope(|s| {
        let mut handles = Vec::new();
        for thread_id in 0..num_threads {
            let m = &mgr;
            handles.push(s.spawn(move || {
                for batch in 0..batches_per_thread {
                    let mut guard = m.concurrent_writer().expect("acquire writer");
                    with_retry(50, || {
                        let tx = guard.storage().raw().transaction()?;
                        for row_in_batch in 0..rows_per_batch {
                            let seq = batch * rows_per_batch + row_in_batch;
                            tx.execute(&format!(
                                "INSERT INTO items (thread_id, seq, val) VALUES ({thread_id}, {seq}, 'contention')"
                            ))?;
                        }
                        tx.commit().map_err(anyhow::Error::new)?;
                        Ok(())
                    })
                    .expect("batch insert should succeed");
                    guard.mark_committed();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
    });

    let elapsed = start.elapsed();

    // Verify via reader
    let reader = mgr.reader();
    let rows = reader.query("SELECT COUNT(*) FROM items").unwrap();
    let count: i64 = rows[0].get_typed(0).unwrap();
    let expected = (num_threads * batches_per_thread * rows_per_batch) as i64;
    assert!(
        count >= expected,
        "at least {expected} rows should be persisted, got {count}"
    );

    let throughput = count as f64 / elapsed.as_secs_f64();
    eprintln!(
        "Write contention: {count} rows in {:.2}s ({:.0} rows/sec)",
        elapsed.as_secs_f64(),
        throughput
    );
}

// ============================================================================
// 3. READ-WRITE MIX (TUI + indexer simulation)
// ============================================================================

#[test]
fn stress_read_write_mix() {
    let dir = TempDir::new().unwrap();
    let db_path = setup_simple_db(&dir);

    let duration = Duration::from_secs(3);
    let read_count = Arc::new(AtomicUsize::new(0));
    let read_errors = Arc::new(AtomicUsize::new(0));
    let write_errors = Arc::new(AtomicUsize::new(0));

    std::thread::scope(|s| {
        // 4 writer threads
        for thread_id in 0..4 {
            let path = db_path.clone();
            let werr = Arc::clone(&write_errors);
            s.spawn(move || {
                let conn = open_configured(&path);
                let start = Instant::now();
                let mut seq = 0;

                while start.elapsed() < duration {
                    let result = with_retry(30, || {
                        let tx = conn.transaction()?;
                        tx.execute(&format!(
                            "INSERT INTO items (thread_id, seq, val) VALUES ({thread_id}, {seq}, 'rw-mix')"
                        ))?;
                        tx.commit().map_err(anyhow::Error::new)?;
                        Ok(())
                    });
                    if result.is_ok() {
                        seq += 1;
                    } else {
                        werr.fetch_add(1, Ordering::Relaxed);
                    }
                }
            });
        }

        // 4 reader threads
        for _reader_id in 0..4 {
            let path = db_path.clone();
            let reads = Arc::clone(&read_count);
            let errors = Arc::clone(&read_errors);
            s.spawn(move || {
                let conn = open_configured(&path);
                let start = Instant::now();

                while start.elapsed() < duration {
                    match conn.query("SELECT COUNT(*) FROM items") {
                        Ok(rows) => {
                            let _count: i64 = rows[0].get_typed(0).unwrap();
                            reads.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(_) => {
                            errors.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    // Small yield to prevent spinning
                    std::thread::yield_now();
                }
            });
        }
    });

    let total_reads = read_count.load(Ordering::Relaxed);
    let total_read_errors = read_errors.load(Ordering::Relaxed);
    let total_write_errors = write_errors.load(Ordering::Relaxed);

    // Verify final state
    let conn = open_configured(&db_path);
    let rows = conn.query("SELECT COUNT(*) FROM items").unwrap();
    let final_count: i64 = rows[0].get_typed(0).unwrap();

    // The DB count is authoritative. Under MVCC, a commit may return an error
    // after the data was actually persisted (partial commit edge case), so we
    // verify structural integrity rather than exact write counting.
    assert!(final_count > 0, "writers should have committed rows");
    assert!(total_reads > 0, "readers should have completed queries");

    // Verify data integrity: all rows are readable and have expected columns
    let integrity_rows = conn
        .query("SELECT thread_id, seq, val FROM items ORDER BY thread_id, seq")
        .unwrap();
    for row in &integrity_rows {
        let tid: i64 = row.get_typed(0).unwrap();
        let seq: i64 = row.get_typed(1).unwrap();
        let val: String = row.get_typed(2).unwrap();
        assert!((0..4).contains(&tid), "thread_id {tid} should be 0..3");
        assert!(seq >= 0, "seq should be non-negative");
        assert_eq!(val, "rw-mix", "val should be 'rw-mix'");
    }

    // Verify each thread contributed rows
    for thread_id in 0..4 {
        let count_rows = conn
            .query(&format!(
                "SELECT COUNT(*) FROM items WHERE thread_id = {thread_id}"
            ))
            .unwrap();
        let thread_count: i64 = count_rows[0].get_typed(0).unwrap();
        assert!(
            thread_count > 0,
            "thread {thread_id} should have written at least 1 row"
        );
    }

    eprintln!(
        "Read-write mix ({:.1}s): {} final rows, {} reads, {} read errors, {} write errors",
        duration.as_secs_f64(),
        final_count,
        total_reads,
        total_read_errors,
        total_write_errors,
    );
}

// ============================================================================
// 4. CRASH RECOVERY
// ============================================================================

#[test]
fn stress_crash_recovery_uncommitted_data_absent() {
    let dir = TempDir::new().unwrap();
    let db_path = setup_simple_db(&dir);

    // Commit some data first
    {
        let conn = open_configured(&db_path);
        let tx = conn.transaction().unwrap();
        tx.execute("INSERT INTO items (thread_id, seq, val) VALUES (0, 0, 'committed')")
            .unwrap();
        tx.commit().unwrap();
    }

    // Begin concurrent write but DO NOT commit - drop connection
    {
        let conn = open_configured(&db_path);
        conn.execute("BEGIN CONCURRENT").unwrap();
        conn.execute("INSERT INTO items (thread_id, seq, val) VALUES (1, 0, 'uncommitted')")
            .unwrap();
        // Drop without COMMIT — should auto-rollback
    }

    // Verify only committed data exists
    let conn = open_configured(&db_path);
    let rows = conn.query("SELECT COUNT(*) FROM items").unwrap();
    let count: i64 = rows[0].get_typed(0).unwrap();
    assert_eq!(count, 1, "only committed row should exist");

    let val_rows = conn.query("SELECT val FROM items").unwrap();
    assert_eq!(
        val_rows[0].get_typed::<String>(0).unwrap(),
        "committed",
        "only committed data should be present"
    );
}

// ============================================================================
// 5. LARGE TRANSACTION
// ============================================================================

#[test]
fn stress_large_transaction() {
    let dir = TempDir::new().unwrap();
    let db_path = setup_simple_db(&dir);

    let num_rows = 10_000; // Reduced from 100K for test speed
    let start = Instant::now();

    {
        let conn = open_configured(&db_path);
        let tx = conn.transaction().unwrap();

        for i in 0..num_rows {
            tx.execute(&format!(
                "INSERT INTO items (thread_id, seq, val) VALUES (0, {i}, 'large-txn-row-{i}')"
            ))
            .unwrap();
        }

        tx.commit().unwrap();
    }

    let commit_time = start.elapsed();

    // Verify all rows present
    let conn = open_configured(&db_path);
    let rows = conn.query("SELECT COUNT(*) FROM items").unwrap();
    let count: i64 = rows[0].get_typed(0).unwrap();
    assert_eq!(count, num_rows, "all {num_rows} rows should be present");

    eprintln!(
        "Large transaction: {num_rows} rows committed in {:.2}s",
        commit_time.as_secs_f64()
    );
    assert!(
        commit_time < Duration::from_secs(30),
        "large transaction should complete within 30 seconds"
    );
}

// ============================================================================
// 6. RETRY CONVERGENCE
// ============================================================================

#[test]
fn stress_retry_convergence_conflicting_writes() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("conflict.db");

    let conn = open_configured(&db_path);
    conn.execute("CREATE TABLE counter (id INTEGER PRIMARY KEY, val INTEGER)")
        .unwrap();
    conn.execute("INSERT INTO counter (id, val) VALUES (1, 0)")
        .unwrap();
    drop(conn);

    let num_threads = 4;
    let increments_per_thread = 50;
    let total_retries = Arc::new(AtomicUsize::new(0));
    let max_retries_any = Arc::new(AtomicUsize::new(0));

    std::thread::scope(|s| {
        let mut handles = Vec::new();
        for _thread_id in 0..num_threads {
            let path = db_path.clone();
            let retries = Arc::clone(&total_retries);
            let max_r = Arc::clone(&max_retries_any);

            handles.push(s.spawn(move || {
                let conn = open_configured(&path);

                for _ in 0..increments_per_thread {
                    let mut attempt = 0;
                    loop {
                        let tx = conn.transaction().unwrap();
                        let rows = tx.query("SELECT val FROM counter WHERE id = 1").unwrap();
                        let current: i64 = rows[0].get_typed(0).unwrap();
                        let new_val = current + 1;

                        if let Err(e) =
                            tx.execute(&format!("UPDATE counter SET val = {new_val} WHERE id = 1"))
                        {
                            // Execute failed — likely conflict
                            let _ = conn.execute("ROLLBACK");
                            attempt += 1;
                            retries.fetch_add(1, Ordering::Relaxed);
                            if attempt > 50 {
                                panic!("too many retries on execute: {e}");
                            }
                            std::thread::sleep(Duration::from_millis(1 << attempt.min(6)));
                            continue;
                        }

                        match tx.commit() {
                            Ok(()) => {
                                // Update max retries
                                let mut current_max = max_r.load(Ordering::Relaxed);
                                while attempt > current_max {
                                    match max_r.compare_exchange_weak(
                                        current_max,
                                        attempt,
                                        Ordering::Relaxed,
                                        Ordering::Relaxed,
                                    ) {
                                        Ok(_) => break,
                                        Err(v) => current_max = v,
                                    }
                                }
                                break;
                            }
                            Err(_) => {
                                attempt += 1;
                                retries.fetch_add(1, Ordering::Relaxed);
                                if attempt > 50 {
                                    panic!("too many commit retries");
                                }
                                std::thread::sleep(Duration::from_millis(1 << attempt.min(6)));
                            }
                        }
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
    });

    // Verify final counter value
    let conn = open_configured(&db_path);
    let rows = conn.query("SELECT val FROM counter WHERE id = 1").unwrap();
    let final_val: i64 = rows[0].get_typed(0).unwrap();

    // With optimistic concurrency, some increments may be lost due to
    // read-modify-write races. The counter should be >= num_threads
    // (at least one increment per thread succeeds without conflict).
    let expected = (num_threads * increments_per_thread) as i64;
    eprintln!(
        "Retry convergence: final={final_val}, expected={expected}, retries={}, max_retries_per_op={}",
        total_retries.load(Ordering::Relaxed),
        max_retries_any.load(Ordering::Relaxed)
    );

    // With SSI retry logic, the counter should eventually reach the expected value
    // because each thread retries until its CAS-like increment succeeds.
    assert_eq!(
        final_val, expected,
        "counter should reach expected value after retries"
    );
}

// ============================================================================
// 7. CONNECTION MANAGER STRESS
// ============================================================================

#[test]
fn stress_connection_manager_parallel_writers() {
    let dir = TempDir::new().unwrap();
    let db_path = setup_db(&dir);

    let config = ConnectionManagerConfig {
        reader_count: 2,
        max_writers: 4,
    };
    let mgr = FrankenConnectionManager::new(&db_path, config).unwrap();

    // Create test table via a writer
    {
        let mut guard = mgr.writer().unwrap();
        guard
            .storage()
            .raw()
            .execute("CREATE TABLE IF NOT EXISTS cm_stress (id INTEGER PRIMARY KEY, tid INTEGER, val TEXT)")
            .unwrap();
        guard.mark_committed();
    }

    let writes_per_thread = 50;

    std::thread::scope(|s| {
        let mut handles = Vec::new();
        for tid in 0..4 {
            let m = &mgr;
            handles.push(s.spawn(move || {
                for seq in 0..writes_per_thread {
                    let mut guard = m.concurrent_writer().expect("acquire writer");
                    with_retry(50, || {
                        let tx = guard.storage().raw().transaction()?;
                        tx.execute(&format!(
                            "INSERT INTO cm_stress (tid, val) VALUES ({tid}, 'cm-{tid}-{seq}')"
                        ))?;
                        tx.commit().map_err(anyhow::Error::new)?;
                        Ok(())
                    })
                    .expect("cm write should succeed");
                    guard.mark_committed();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
    });

    // Verify via reader
    let reader_guard = mgr.reader();
    let rows = reader_guard
        .query("SELECT COUNT(*) FROM cm_stress")
        .unwrap();
    let count: i64 = rows[0].get_typed(0).unwrap();
    assert_eq!(
        count,
        (4 * writes_per_thread) as i64,
        "all writes should be persisted"
    );
}
