use assert_cmd::cargo::cargo_bin_cmd;
use std::fs;
use std::path::Path;

mod util;
use util::EnvGuard;
use util::e2e_log::{E2eError, E2eErrorContext, E2ePerformanceMetrics, PhaseTracker};

fn truncate_output(bytes: &[u8], max_len: usize) -> String {
    let s = String::from_utf8_lossy(bytes);
    if s.len() > max_len {
        format!("{}... [truncated {} bytes]", &s[..max_len], s.len() - max_len)
    } else {
        s.to_string()
    }
}

fn tracker_for(test_name: &str) -> PhaseTracker {
    PhaseTracker::new("e2e_index_tui", test_name)
}

fn make_codex_fixture(root: &Path) {
    let sessions = root.join("sessions/2025/11/21");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-1.jsonl");
    let sample = r#"{"role":"user","timestamp":1700000000000,"content":"hello"}
{"role":"assistant","timestamp":1700000001000,"content":"hi"}
"#;
    fs::write(file, sample).unwrap();
}

#[test]
fn index_then_tui_once_headless() {
    let tracker = tracker_for("index_then_tui_once_headless");

    // Phase: setup fixtures
    let setup_start = tracker.start("setup_fixtures", Some("Creating isolated test environment"));
    let tmp = tempfile::TempDir::new().unwrap();
    // Isolate from the developer machine's real session dirs (HOME-based connectors).
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());

    let xdg = tmp.path().join("xdg");
    fs::create_dir_all(&xdg).unwrap();
    let _guard_xdg = EnvGuard::set("XDG_DATA_HOME", xdg.to_string_lossy());

    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    // Codex fixture under CODEX_HOME to satisfy detection.
    let _guard_codex = EnvGuard::set("CODEX_HOME", data_dir.to_string_lossy());
    make_codex_fixture(&data_dir);
    tracker.end("setup_fixtures", Some("Test environment ready"), setup_start);

    // Phase: run index
    let index_start = tracker.start("run_index", Some("Running cass index --full"));
    let output = cargo_bin_cmd!("cass")
        .arg("index")
        .arg("--full")
        .arg("--data-dir")
        .arg(&data_dir)
        .current_dir(&home)
        .output()
        .expect("failed to spawn cass index");
    if !output.status.success() {
        let ctx = E2eErrorContext::new()
            .with_command("cass index --full")
            .capture_cwd()
            .add_state("exit_code", serde_json::json!(output.status.code()))
            .add_state("stdout_tail", serde_json::json!(truncate_output(&output.stdout, 1000)))
            .add_state("stderr_tail", serde_json::json!(truncate_output(&output.stderr, 1000)));
        tracker.fail(E2eError::with_type("cass index --full failed", "COMMAND_FAILED").with_context(ctx));
        panic!("cass index --full failed (exit {:?}): {}", output.status.code(), truncate_output(&output.stderr, 500));
    }
    let index_ms = index_start.elapsed().as_millis() as u64;
    tracker.end("run_index", Some("Index complete"), index_start);
    tracker.metrics(
        "index_duration",
        &E2ePerformanceMetrics::new()
            .with_duration(index_ms)
            .with_throughput(1, index_ms)
            .with_custom("operation", "full_index"),
    );

    // Phase: run TUI in headless mode
    let tui_start = tracker.start("run_tui", Some("Smoke-testing TUI in headless mode"));
    let output = cargo_bin_cmd!("cass")
        .arg("tui")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--once")
        .current_dir(&home)
        .env("TUI_HEADLESS", "1")
        .output()
        .expect("failed to spawn cass tui");
    if !output.status.success() {
        let ctx = E2eErrorContext::new()
            .with_command("cass tui --once")
            .capture_cwd()
            .add_state("exit_code", serde_json::json!(output.status.code()))
            .add_state("stdout_tail", serde_json::json!(truncate_output(&output.stdout, 1000)))
            .add_state("stderr_tail", serde_json::json!(truncate_output(&output.stderr, 1000)));
        tracker.fail(E2eError::with_type("cass tui --once failed", "COMMAND_FAILED").with_context(ctx));
        panic!("cass tui --once failed (exit {:?}): {}", output.status.code(), truncate_output(&output.stderr, 500));
    }
    let tui_ms = tui_start.elapsed().as_millis() as u64;
    tracker.end("run_tui", Some("TUI headless smoke test passed"), tui_start);
    tracker.metrics(
        "tui_duration",
        &E2ePerformanceMetrics::new()
            .with_duration(tui_ms)
            .with_custom("mode", "headless"),
    );

    // Phase: verify artifacts
    let verify_start = tracker.start("verify_artifacts", Some("Checking index artifacts exist"));
    assert!(data_dir.join("agent_search.db").exists());
    assert!(data_dir.join("index/v6").exists());
    let verify_ms = verify_start.elapsed().as_millis() as u64;
    tracker.end(
        "verify_artifacts",
        Some("All artifacts verified"),
        verify_start,
    );
    tracker.metrics(
        "verify_duration",
        &E2ePerformanceMetrics::new()
            .with_duration(verify_ms)
            .with_custom("operation", "artifact_check"),
    );

    tracker.complete();
}
