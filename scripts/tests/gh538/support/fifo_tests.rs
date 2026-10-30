//! Supplemental subprocess proof. This is deliberately not a cass watcher or
//! SQL/checkpoint test; the selected production watchdog/observer/exit helper
//! executes against real blocking FIFO operations without weakening its code.
use super::*;
use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

const CHILD_MODE: &str = "CASS_538_EXACT_FIFO_CHILD";
const CHILD_ROOT: &str = "CASS_538_EXACT_FIFO_ROOT";
const TEST_NAME: &str = "fifo_tests::real_fifo_diagnostics_cannot_hide_watchdog_abort";

struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn child_probe(mode: &str, root: &Path) {
    if mode == "synchronous-negative-control" {
        println!("entering-production-diagnostic-collection");
        std::io::stdout().flush().expect("flush entry marker");
        // This same collection call used to run synchronously on the watchdog
        // thread. A real FIFO read must demonstrate that this trigger blocks.
        let _ = collect_stall_diagnostics(root);
        panic!("unpaired FIFO collection unexpectedly returned");
    }
    assert_eq!(mode, "asynchronous-watchdog");
    let source = root.join("source.fifo");
    let progress = Arc::new(indexer::IndexingProgress::default());
    progress.phase.store(indexer::INDEX_PHASE_LEXICAL_INDEXING, Ordering::Relaxed);
    progress.total.store(1, Ordering::Relaxed);
    let worker_progress = Arc::clone(&progress);
    let _blocked_worker = std::thread::spawn(move || {
        raw_mirror::harness_open_fifo(&source, &worker_progress)
            .expect("real FIFO open remains blocked until process exit");
    });
    let entered_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if raw_mirror::raw_mirror_io_snapshot()
            .as_array()
            .is_some_and(|ops| !ops.is_empty())
        {
            break;
        }
        assert!(Instant::now() < entered_deadline, "source FIFO was not observed");
        std::thread::sleep(Duration::from_millis(5));
    }
    let mut watchdog = IndexStallWatchdog::aborting_lexical_phases(
        root.to_path_buf(),
        Duration::from_millis(50),
    )
    .watch_aware(true);
    // cfg(test) pins this sampler to None for deterministic unit tests. Restore
    // the exact real production sampler for this subprocess integration probe.
    watchdog.io_probe = process_block_io_bytes;
    assert_eq!(watchdog.threshold, Some(Duration::from_millis(1050)));
    assert_eq!(watchdog.abort_threshold, Some(Duration::from_secs(3)));
    let started = Instant::now();
    loop {
        if let Some(payload) = watchdog.observe(&progress, started.elapsed().as_millis()) {
            println!("{payload}");
            std::io::stdout().flush().expect("flush watchdog event");
            if let Some(envelope) = index_stall_abort_envelope(&payload) {
                assert_eq!(payload["diagnostics"]["filesystem_probe"]["status"], "pending");
                let operations = payload["diagnostics"]["raw_mirror_io"].as_array().unwrap();
                assert_eq!(operations.len(), 1);
                assert_eq!(operations[0]["operation"], "harness_open_source_fifo");
                assert_eq!(operations[0]["path"], root.join("source.fifo").to_string_lossy().as_ref());
                assert!(operations[0]["elapsed_ms"].as_u64().is_some_and(|ms| ms >= 3000));
                assert_eq!(progress.current.load(Ordering::Relaxed), 0);
                assert_eq!(progress.activity.load(Ordering::Relaxed), 0);
                eprintln!("{envelope}");
                // Canonical WAL checkpointing is explicitly outside this
                // harness. Use the real exit helper, not a fabricated result.
                exit_without_this_threads_tls_destructors(70);
            }
        }
        assert!(started.elapsed() < Duration::from_secs(10), "watchdog missed its real deadline");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn real_fifo_diagnostics_cannot_hide_watchdog_abort() -> anyhow::Result<()> {
    if let Ok(mode) = dotenvy::var(CHILD_MODE) {
        child_probe(&mode, &PathBuf::from(dotenvy::var(CHILD_ROOT)?));
        return Ok(());
    }
    let temp = tempfile::TempDir::new()?;
    let root = temp.path();
    std::fs::create_dir_all(root.join("index"))?;
    for path in [root.join("source.fifo"), root.join("index/lexical_rebuild_state.json")] {
        assert!(Command::new("mkfifo").arg(path).status()?.success());
    }
    for mode in ["synchronous-negative-control", "asynchronous-watchdog"] {
        let stdout_path = root.join(format!("{mode}.stdout"));
        let stderr_path = root.join(format!("{mode}.stderr"));
        let mut child = OwnedChild(
            Command::new(std::env::current_exe()?)
                .args(["--exact", TEST_NAME, "--nocapture", "--test-threads=1"])
                .env(CHILD_MODE, mode)
                .env(CHILD_ROOT, root)
                .env("CASS_INDEX_STALL_DETECT_SECS", "1")
                .env("CASS_INDEX_STALL_ABORT_SECS", "3")
                .env("CASS_INDEX_FINALIZE_ABORT_SECS", "3")
                .env_remove("CASS_INDEX_STALL_ABORT_ALL_PHASES")
                .stdin(Stdio::null())
                .stdout(std::fs::File::create(&stdout_path)?)
                .stderr(std::fs::File::create(&stderr_path)?)
                .spawn()?,
        );
        let started = Instant::now();
        if mode == "synchronous-negative-control" {
            let deadline = started + Duration::from_secs(5);
            while !std::fs::read_to_string(&stdout_path)?.contains("entering-production-diagnostic-collection") {
                assert!(child.0.try_wait()?.is_none());
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            }
            std::thread::sleep(Duration::from_millis(500));
            assert!(child.0.try_wait()?.is_none(), "FIFO did not actually block collection");
            child.0.kill()?;
            child.0.wait()?;
            eprintln!("negative control: exact synchronous collection blocked in FIFO open; killed and reaped");
            continue;
        }
        let status = loop {
            if let Some(status) = child.0.try_wait()? {
                break status;
            }
            assert!(started.elapsed() < Duration::from_secs(15), "child missed watchdog bound");
            std::thread::sleep(Duration::from_millis(50));
        };
        let stdout = std::fs::read_to_string(&stdout_path)?;
        let stderr = std::fs::read_to_string(&stderr_path)?;
        eprintln!("FIFO probe wall_ms={}\n{stdout}\n{stderr}", started.elapsed().as_millis());
        assert_eq!(status.code(), Some(70), "stdout={stdout}\nstderr={stderr}");
        let events: Vec<serde_json::Value> = stdout.lines()
            // libtest prints the test-name prefix without a newline before
            // the child's first production JSON payload.
            .filter_map(|line| line.find('{').and_then(|start| serde_json::from_str(&line[start..]).ok()))
            .collect();
        assert!(events.iter().any(|event| event["event"] == "stall_detected"));
        let abort = events.iter().find(|event| event["event"] == "stall_aborting")
            .expect("actual abort event");
        assert_eq!(abort["abort_threshold_secs"], 3);
        assert_eq!(abort["diagnostics"]["filesystem_probe"]["status"], "pending");
        let envelope = stderr.lines().filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find(|value| value["kind"] == "index-stalled").expect("actual exit envelope");
        assert_eq!(envelope["code"], 70);
        assert_eq!(envelope["retryable"], true);
    }
    Ok(())
}
