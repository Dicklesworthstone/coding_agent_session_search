// Included inside indexer::tests: use the real persistence path and fixtures.
mod gh473_preflight {
    use super::*;
    use std::process::{Command, Stdio};

    fn scalar(storage: &FrankenStorage, sql: &str) -> i64 {
        storage.raw().query(sql).unwrap()[0].get_typed::<i64>(0).unwrap()
    }

    fn prime_cold_writer(storage: &FrankenStorage) {
        assert!(!storage.ephemeral_writer_preflight_verified());
        let (writer, reusable) = storage.acquire_cached_ephemeral_writer().unwrap();
        assert!(reusable, "the regression must exercise the cached writer path");
        // Force the actual connection and its cached policy to a stale long wait.
        writer.raw().execute("PRAGMA busy_timeout = 60000").unwrap();
        writer.mark_index_writer_busy_timeout_ms(60_000);
        storage.release_cached_ephemeral_writer(writer);
    }

    fn preflight_contention(expected: u64, release_after_failure: bool) {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("preflight.db");
        let storage = FrankenStorage::open(&path).unwrap();
        assert!(!storage.bulk_single_connection_enabled());
        prime_cold_writer(&storage);
        let holder = crate::franken_sync::Connection::open_existing_schema_only(
            path.to_string_lossy().into_owned(),
        ).unwrap();
        holder.execute("BEGIN IMMEDIATE").unwrap();
        let mut attempts = 0;
        let mut bodies = 0;
        let started = Instant::now();
        let result = persist::with_concurrent_retry(2, || {
            attempts += 1;
            let result = persist::with_ephemeral_writer(&storage, false, "GH473 preflight", |writer| {
                bodies += 1;
                assert_eq!(scalar(writer, "PRAGMA busy_timeout"), expected as i64);
                Ok(())
            });
            if attempts == 1 {
                let err = result.as_ref().expect_err("a held writer must block preflight");
                assert!(
                    err.to_string().contains("ephemeral writer preflight write failed"),
                    "the failure must reach preflight, not a different open path: {err:#}"
                );
                assert!(anyhow_chain_indicates_retryable_storage_contention(err));
                assert!(!storage.ephemeral_writer_preflight_verified());
                assert_eq!(bodies, 0, "the body must not run before preflight succeeds");
                if release_after_failure {
                    holder.execute("ROLLBACK").unwrap();
                }
            }
            result
        });
        if release_after_failure {
            result.unwrap();
            assert_eq!((attempts, bodies), (2, 1));
            assert!(storage.ephemeral_writer_preflight_verified());
        } else {
            let err = result.expect_err("exhaustion must not turn contention into success");
            assert!(anyhow_chain_indicates_retryable_storage_contention(&err));
            assert_eq!((attempts, bodies), (3, 0));
            assert!(!storage.ephemeral_writer_preflight_verified());
            holder.execute("ROLLBACK").unwrap();
        }
        assert!(started.elapsed() < Duration::from_secs(5), "preflight retained a long inner wait");
        assert_eq!(scalar(&storage, "SELECT COUNT(*) FROM conversations"), 0);
        assert_eq!(scalar(&storage, "SELECT COUNT(*) FROM messages"), 0);
        assert!(storage.source_ingest_ledger_entries().unwrap().is_empty());
        holder.close_without_checkpoint().unwrap();
        storage.close().unwrap();
    }

    fn permanent_error_is_not_retried() {
        let tmp = TempDir::new().unwrap();
        let storage = FrankenStorage::open(&tmp.path().join("permanent.db")).unwrap();
        let mut attempts = 0;
        let result = persist::with_concurrent_retry(2, || {
            attempts += 1;
            persist::with_ephemeral_writer(&storage, false, "GH473 permanent error", |writer| {
                writer.raw().execute("INSERT INTO gh473_table_that_does_not_exist VALUES (1)")
                    .map(|_| ()).map_err(anyhow::Error::new)
            })
        });
        let err = result.expect_err("a permanent SQL failure must propagate");
        assert!(!anyhow_chain_indicates_retryable_storage_contention(&err));
        assert_eq!(attempts, 1);
        assert!(storage.source_ingest_ledger_entries().unwrap().is_empty());
        storage.close().unwrap();
    }

    fn canonical_write_and_replay_with_pinned_reader(expected: u64) {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("canonical.db");
        let storage = FrankenStorage::open(&path).unwrap();
        prime_cold_writer(&storage);
        let reader = FrankenStorage::open_readonly(&path).unwrap();
        reader.raw().execute("BEGIN").unwrap();
        assert_eq!(scalar(&reader, "SELECT COUNT(*) FROM messages"), 0);
        let conversation = norm_conv(Some("gh473-pinned-reader"), vec![norm_msg(0, 100)]);
        let completion = crate::storage::sqlite::SourceIngestLedgerEntry {
            key: "source_ingest_v1:gh473-pinned-reader".into(),
            observation: "{\"complete\":true,\"generation\":1}".into(),
        };
        for pass in 0..3 {
            let started = Instant::now();
            persist::persist_conversations_batched_inner(
                &storage, None, std::slice::from_ref(&conversation),
                LexicalPopulationStrategy::DeferredAuthoritativeDbRebuild,
                false, false, None, persist::PersistHeartbeat::NONE, Some(&completion),
            ).unwrap();
            assert!(started.elapsed() < Duration::from_secs(10), "canonical pass {pass} stalled");
            assert_eq!(scalar(&storage, "SELECT COUNT(*) FROM conversations"), 1);
            assert_eq!(scalar(&storage, "SELECT COUNT(*) FROM messages"), 1);
            let ledger = storage.source_ingest_ledger_entries().unwrap();
            assert_eq!(ledger.len(), 1);
            assert_eq!(ledger.get(&completion.key), Some(&completion.observation));
            persist::with_ephemeral_writer(&storage, false, "GH473 reused policy", |writer| {
                assert_eq!(scalar(writer, "PRAGMA busy_timeout"), expected as i64);
                Ok(())
            }).unwrap();
        }
        assert_eq!(scalar(&reader, "SELECT COUNT(*) FROM messages"), 0, "the snapshot stays pinned");
        reader.raw().execute("ROLLBACK").unwrap();
        reader.close_without_checkpoint().unwrap();
        storage.close().unwrap();
        let reopened = FrankenStorage::open_readonly(&path).unwrap();
        assert_eq!(scalar(&reopened, "SELECT COUNT(*) FROM conversations"), 1);
        assert_eq!(scalar(&reopened, "SELECT COUNT(*) FROM messages"), 1);
        let ledger = reopened.source_ingest_ledger_entries().unwrap();
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger.get(&completion.key), Some(&completion.observation));
        reopened.close_without_checkpoint().unwrap();
    }

    #[test]
    fn writer_preflight_contention_regression() {
        const CHILD: &str = "CASS_TEST_GH473_PREFLIGHT_CHILD";
        if let Ok(expected) = dotenvy::var(CHILD) {
            let expected = expected.parse::<u64>().unwrap();
            preflight_contention(expected, true);
            preflight_contention(expected, false);
            permanent_error_is_not_retried();
            canonical_write_and_replay_with_pinned_reader(expected);
            return;
        }
        // No unsafe global environment mutation; isolate ambient .env files too.
        for (value, expected) in [
            (None, "10"), (Some("0"), "10"), (Some("invalid"), "10"),
            (Some("-1"), "10"), (Some("18446744073709551616"), "10"), (Some("37"), "37"),
        ] {
            let dir = TempDir::new().unwrap();
            let stdout_path = dir.path().join("stdout.log");
            let stderr_path = dir.path().join("stderr.log");
            let mut command = Command::new(std::env::current_exe().unwrap());
            command.args([
                "--exact", "indexer::tests::gh473_preflight::writer_preflight_contention_regression",
                "--nocapture",
            ]).current_dir(dir.path()).env(CHILD, expected)
                .env_remove("CASS_INDEX_WRITER_BUSY_TIMEOUT_MS")
                .stdout(Stdio::from(std::fs::File::create(&stdout_path).unwrap()))
                .stderr(Stdio::from(std::fs::File::create(&stderr_path).unwrap()));
            if let Some(value) = value {
                command.env("CASS_INDEX_WRITER_BUSY_TIMEOUT_MS", value);
            }
            let mut child = command.spawn().unwrap();
            let started = Instant::now();
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() { break status; }
                if started.elapsed() > Duration::from_secs(40) {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("GH473 child policy {value:?} exceeded 40s (possible long inner wait)");
                }
                std::thread::sleep(Duration::from_millis(20));
            };
            let stdout = std::fs::read_to_string(stdout_path).unwrap();
            let stderr = std::fs::read_to_string(stderr_path).unwrap();
            assert!(status.success(), "policy {value:?}: {stdout}\n{stderr}");
            assert!(stdout.contains("1 passed; 0 failed"), "child did not run: {stdout}");
        }
    }
}
