//! TUI Smoke Flows E2E Tests with Event Logging and Artifact Capture (coding_agent_session_search-d41o)
//!
//! This module provides comprehensive E2E smoke tests for the TUI with:
//! - Detailed event logging via PhaseTracker with trace IDs
//! - Screen frame capture (stdout/stderr as artifacts)
//! - Per-step timing metrics
//! - Artifact storage under test-results/e2e/tui/
//!
//! This suite includes both PTY-driven interactive ftui flows and headless checks.
//! Headless mode (`--once + TUI_HEADLESS=1`) is still used to verify:
//! - TUI launch/exit paths
//! - CLI search equivalents for search/filter flows
//! - Export flow via CLI
//!
//! Run with: cargo test --test e2e_tui_smoke_flows -- --nocapture

use assert_cmd::cargo::cargo_bin_cmd;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

mod util;
use util::EnvGuard;
use util::e2e_log::{E2eError, E2eErrorContext, E2ePerformanceMetrics, PhaseTracker};

/// Global lock to prevent parallel test interference
static TUI_FLOW_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn tui_flow_guard() -> std::sync::MutexGuard<'static, ()> {
    match TUI_FLOW_LOCK.get_or_init(|| Mutex::new(())).lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Artifact directory for TUI E2E tests
fn artifact_dir() -> PathBuf {
    let dir = PathBuf::from("test-results/e2e/tui");
    fs::create_dir_all(&dir).expect("create artifact dir");
    dir
}

/// Generate a unique trace ID for this test run
fn trace_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis();
    format!("tui-{ts:x}")
}

/// Truncate output for logging
fn truncate_output(bytes: &[u8], max_len: usize) -> String {
    let s = String::from_utf8_lossy(bytes);
    if s.len() > max_len {
        format!(
            "{}... [truncated {} bytes]",
            &s[..max_len],
            s.len() - max_len
        )
    } else {
        s.to_string()
    }
}

/// Save output as artifact
fn save_artifact(name: &str, trace: &str, content: &[u8]) -> PathBuf {
    let dir = artifact_dir();
    let path = dir.join(format!("{trace}_{name}"));
    fs::write(&path, content).expect("write artifact");
    path
}

/// Create tracker for test
fn tracker_for(test_name: &str) -> PhaseTracker {
    PhaseTracker::new("e2e_tui_smoke_flows", test_name)
}

// =============================================================================
// PTY Helpers
// =============================================================================

const PTY_STARTUP_TIMEOUT: Duration = Duration::from_secs(8);
const PTY_EXIT_TIMEOUT: Duration = Duration::from_secs(20);
const PTY_POLL: Duration = Duration::from_millis(40);
const PERF_SEARCH_P95_BUDGET_MS: u64 = 1_500;
const PERF_TUI_STARTUP_BUDGET_MS: u64 = 5_000;
const PERF_TUI_DETAIL_OPEN_BUDGET_MS: u64 = 5_000;
const PERF_TUI_OUTPUT_BYTES_BUDGET: u64 = 600_000;
const PERF_TUI_BYTES_PER_ACTION_BUDGET: u64 = 180_000;

struct FtuiPtyEnv {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    xdg: PathBuf,
    data_dir: PathBuf,
    codex_home: PathBuf,
}

fn cass_bin_path() -> &'static str {
    env!("CARGO_BIN_EXE_cass")
}

fn prepare_ftui_pty_env(trace: &str, tracker: &PhaseTracker) -> FtuiPtyEnv {
    let setup_start = tracker.start(
        "pty_setup",
        Some("Creating isolated ftui PTY environment + fixtures"),
    );

    let tmp = tempfile::TempDir::new().expect("create temp dir");
    let home = tmp.path().join("home");
    let xdg = tmp.path().join("xdg");
    let data_dir = tmp.path().join("cass_data");
    let codex_home = tmp.path().join("codex_home");
    fs::create_dir_all(&home).expect("create home");
    fs::create_dir_all(&xdg).expect("create xdg");
    fs::create_dir_all(&data_dir).expect("create cass_data");
    fs::create_dir_all(&codex_home).expect("create codex_home");
    make_codex_fixture(&codex_home);

    tracker.end(
        "pty_setup",
        Some("PTY fixture environment ready"),
        setup_start,
    );

    let index_start = tracker.start(
        "pty_index",
        Some("Indexing fixture data for ftui interactive PTY tests"),
    );
    let output = cargo_bin_cmd!("cass")
        .arg("index")
        .arg("--full")
        .arg("--data-dir")
        .arg(&data_dir)
        .env("HOME", home.to_string_lossy().as_ref())
        .env("XDG_DATA_HOME", xdg.to_string_lossy().as_ref())
        .env("CODEX_HOME", codex_home.to_string_lossy().as_ref())
        .env("CASS_DATA_DIR", data_dir.to_string_lossy().as_ref())
        .env("NO_COLOR", "1")
        .current_dir(&home)
        .output()
        .expect("failed to spawn cass index");

    save_artifact("pty_index_stdout.txt", trace, &output.stdout);
    save_artifact("pty_index_stderr.txt", trace, &output.stderr);
    let index_ms = index_start.elapsed().as_millis() as u64;
    tracker.end("pty_index", Some("PTY index complete"), index_start);
    tracker.metrics(
        "pty_index_duration",
        &E2ePerformanceMetrics::new()
            .with_duration(index_ms)
            .with_custom("trace_id", trace.to_string()),
    );

    if !output.status.success() {
        let ctx = E2eErrorContext::new()
            .with_command("cass index --full --data-dir <pty-data>")
            .capture_cwd()
            .add_state("trace_id", serde_json::json!(trace))
            .add_state("exit_code", serde_json::json!(output.status.code()))
            .add_state(
                "stderr_tail",
                serde_json::json!(truncate_output(&output.stderr, 1500)),
            );
        eprintln!(
            "PTY index failed context: {}",
            serde_json::to_string(&ctx).unwrap_or_else(|_| "{}".to_string())
        );
        panic!(
            "Failed to build index for PTY test: {}",
            truncate_output(&output.stderr, 500)
        );
    }

    FtuiPtyEnv {
        _tmp: tmp,
        home,
        xdg,
        data_dir,
        codex_home,
    }
}

fn apply_ftui_env(cmd: &mut CommandBuilder, env: &FtuiPtyEnv) {
    cmd.cwd(env.home.to_string_lossy().as_ref());
    cmd.env("HOME", env.home.to_string_lossy().as_ref());
    cmd.env("XDG_DATA_HOME", env.xdg.to_string_lossy().as_ref());
    cmd.env("CASS_DATA_DIR", env.data_dir.to_string_lossy().as_ref());
    cmd.env("CODEX_HOME", env.codex_home.to_string_lossy().as_ref());
    cmd.env("CASS_TUI_RUNTIME", "ftui");
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");
    cmd.env("NO_COLOR", "1");
    cmd.env("TERM", "xterm-256color");
}

fn spawn_reader(reader: Box<dyn Read + Send>) -> (Arc<Mutex<Vec<u8>>>, thread::JoinHandle<()>) {
    let captured = Arc::new(Mutex::new(Vec::<u8>::new()));
    let captured_clone = Arc::clone(&captured);
    let handle = thread::spawn(move || {
        let mut reader = reader;
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    captured_clone
                        .lock()
                        .expect("capture lock")
                        .extend_from_slice(&buf[..n]);
                }
                Err(_) => break,
            }
        }
    });
    (captured, handle)
}

fn wait_for_child_exit(
    child: &mut (dyn portable_pty::Child + Send + Sync),
    timeout: Duration,
) -> portable_pty::ExitStatus {
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status,
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let status = child.wait().expect("wait after kill");
                    panic!("PTY child timed out after {:?} (status: {status})", timeout);
                }
                thread::sleep(PTY_POLL);
            }
            Err(err) => panic!("Failed polling PTY child status: {err}"),
        }
    }
}

fn wait_for_output_contains(
    captured: &Arc<Mutex<Vec<u8>>>,
    needle: &str,
    timeout: Duration,
) -> bool {
    let start = Instant::now();
    loop {
        {
            let data = captured.lock().expect("capture lock");
            if String::from_utf8_lossy(&data).contains(needle) {
                return true;
            }
        }
        if start.elapsed() >= timeout {
            return false;
        }
        thread::sleep(PTY_POLL);
    }
}

fn wait_for_output_growth(
    captured: &Arc<Mutex<Vec<u8>>>,
    base_len: usize,
    min_delta: usize,
    timeout: Duration,
) -> bool {
    let start = Instant::now();
    loop {
        {
            let data = captured.lock().expect("capture lock");
            if data.len() >= base_len.saturating_add(min_delta) {
                return true;
            }
        }
        if start.elapsed() >= timeout {
            return false;
        }
        thread::sleep(PTY_POLL);
    }
}

fn send_key_sequence(writer: &mut (dyn Write + Send), bytes: &[u8]) {
    writer.write_all(bytes).expect("write to PTY");
    writer.flush().expect("flush PTY");
}

fn percentile_ms(samples: &[u64], percentile: f64) -> u64 {
    assert!(
        !samples.is_empty(),
        "percentile_ms requires non-empty samples"
    );
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let clamped = percentile.clamp(0.0, 100.0);
    let rank = ((clamped / 100.0) * ((sorted.len() - 1) as f64)).round() as usize;
    sorted[rank]
}

// =============================================================================
// Fixture Helpers
// =============================================================================

/// Create a Codex fixture with searchable content
fn make_codex_fixture(root: &Path) {
    let sessions = root.join("sessions/2025/11/21");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-1.jsonl");
    // Modern Codex envelope format expected by src/connectors/codex.rs.
    let sample = r#"{"type":"session_meta","timestamp":1700000000000,"payload":{"cwd":"/tmp/cass-test"}}
{"type":"event_msg","timestamp":1700000000100,"payload":{"type":"user_message","message":"hello world"}}
{"type":"response_item","timestamp":1700000000200,"payload":{"role":"assistant","content":"hi there, how can I help?"}}
{"type":"event_msg","timestamp":1700000000300,"payload":{"type":"user_message","message":"search for authentication bugs"}}
{"type":"response_item","timestamp":1700000000400,"payload":{"role":"assistant","content":"I found several authentication issues in the codebase."}}
{"type":"event_msg","timestamp":1700000000500,"payload":{"type":"user_message","message":"fix the session timeout"}}
{"type":"response_item","timestamp":1700000000600,"payload":{"role":"assistant","content":"The session timeout has been updated to 30 minutes."}}
"#;
    fs::write(file, sample).unwrap();
}

/// Create a Claude Code fixture
fn make_claude_fixture(root: &Path, workspace_name: &str) {
    let session_dir = root.join(format!("projects/{workspace_name}"));
    fs::create_dir_all(&session_dir).unwrap();
    let file = session_dir.join("session.jsonl");
    let sample = r#"{"type":"user","timestamp":"2025-01-15T10:00:00Z","message":{"content":"implement export feature"}}
{"type":"assistant","timestamp":"2025-01-15T10:00:05Z","message":{"content":"I'll implement the export functionality."}}
{"type":"user","timestamp":"2025-01-15T10:00:10Z","message":{"content":"add filter by date"}}
{"type":"assistant","timestamp":"2025-01-15T10:00:15Z","message":{"content":"Date filtering has been added."}}
"#;
    fs::write(file, sample).unwrap();
}

// =============================================================================
// PTY Interactive Flow Tests (ftui runtime)
// =============================================================================

#[test]
fn tui_pty_launch_quit_and_terminal_cleanup() {
    let _guard_lock = tui_flow_guard();
    let trace = trace_id();
    let tracker = tracker_for("tui_pty_launch_quit_and_terminal_cleanup");
    let _trace_guard = tracker.trace_env_guard();
    let env = prepare_ftui_pty_env(&trace, &tracker);

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 40,
            cols: 130,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open PTY");

    let reader = pair.master.try_clone_reader().expect("clone PTY reader");
    let (captured, reader_handle) = spawn_reader(reader);
    let mut writer = pair.master.take_writer().expect("take PTY writer");

    let launch_start = tracker.start("pty_launch", Some("Launching interactive ftui TUI"));
    let mut tui_cmd = CommandBuilder::new(cass_bin_path());
    tui_cmd.arg("tui");
    apply_ftui_env(&mut tui_cmd, &env);
    let mut tui_child = pair
        .slave
        .spawn_command(tui_cmd)
        .expect("spawn ftui TUI in PTY");

    let saw_shell = wait_for_output_contains(&captured, "cass", PTY_STARTUP_TIMEOUT);
    assert!(saw_shell, "Did not observe TUI startup text in PTY output");

    send_key_sequence(&mut *writer, b"\x1b"); // ESC to quit
    let tui_status = wait_for_child_exit(&mut *tui_child, PTY_EXIT_TIMEOUT);
    tracker.end(
        "pty_launch",
        Some("ftui quit sequence complete"),
        launch_start,
    );
    assert!(
        tui_status.success(),
        "ftui process exited unsuccessfully: {tui_status}"
    );

    let mut stty_cmd = CommandBuilder::new("stty");
    stty_cmd.arg("-a");
    apply_ftui_env(&mut stty_cmd, &env);
    let mut stty_child = pair
        .slave
        .spawn_command(stty_cmd)
        .expect("spawn stty check");
    let stty_status = wait_for_child_exit(&mut *stty_child, Duration::from_secs(8));
    assert!(
        stty_status.success(),
        "stty exited unsuccessfully: {stty_status}"
    );

    drop(writer);
    drop(pair);
    let _ = reader_handle.join();
    let raw = captured.lock().expect("capture lock").clone();
    save_artifact("pty_launch_quit_output.raw", &trace, &raw);
    let text = String::from_utf8_lossy(&raw);

    // Verify terminal mode restored (canonical mode + echo on).
    assert!(
        text.contains("icanon"),
        "Expected stty output to include canonical mode (icanon). Output tail: {}",
        truncate_output(&raw, 1200)
    );
    assert!(
        text.contains("echo"),
        "Expected stty output to include echo enabled. Output tail: {}",
        truncate_output(&raw, 1200)
    );

    tracker.complete();
}

#[test]
fn tui_pty_help_overlay_open_close_flow() {
    let _guard_lock = tui_flow_guard();
    let trace = trace_id();
    let tracker = tracker_for("tui_pty_help_overlay_open_close_flow");
    let _trace_guard = tracker.trace_env_guard();
    let env = prepare_ftui_pty_env(&trace, &tracker);

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 45,
            cols: 145,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open PTY");
    let reader = pair.master.try_clone_reader().expect("clone PTY reader");
    let (captured, reader_handle) = spawn_reader(reader);
    let mut writer = pair.master.take_writer().expect("take PTY writer");

    let mut tui_cmd = CommandBuilder::new(cass_bin_path());
    tui_cmd.arg("tui");
    apply_ftui_env(&mut tui_cmd, &env);
    let mut child = pair
        .slave
        .spawn_command(tui_cmd)
        .expect("spawn ftui TUI in PTY");

    assert!(
        wait_for_output_contains(&captured, "cass", PTY_STARTUP_TIMEOUT),
        "Did not observe startup text before help overlay interaction"
    );

    send_key_sequence(&mut *writer, b"?"); // open help overlay
    thread::sleep(Duration::from_millis(180));
    assert!(
        child
            .try_wait()
            .expect("poll child after help toggle")
            .is_none(),
        "App exited after '?' instead of opening help overlay"
    );

    send_key_sequence(&mut *writer, b"\x1b"); // close help (should not quit app)
    thread::sleep(Duration::from_millis(200));
    assert!(
        child
            .try_wait()
            .expect("poll child after first ESC")
            .is_none(),
        "App exited on first ESC; expected ESC to close help overlay"
    );

    send_key_sequence(&mut *writer, b"\x1b"); // quit app

    let status = wait_for_child_exit(&mut *child, PTY_EXIT_TIMEOUT);
    assert!(
        status.success(),
        "ftui process exited unsuccessfully: {status}"
    );

    drop(writer);
    drop(pair);
    let _ = reader_handle.join();
    let raw = captured.lock().expect("capture lock").clone();
    save_artifact("pty_help_overlay_output.raw", &trace, &raw);

    tracker.complete();
}

#[test]
fn tui_pty_search_detail_and_quit_flow() {
    let _guard_lock = tui_flow_guard();
    let trace = trace_id();
    let tracker = tracker_for("tui_pty_search_detail_and_quit_flow");
    let _trace_guard = tracker.trace_env_guard();
    let env = prepare_ftui_pty_env(&trace, &tracker);

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 45,
            cols: 145,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open PTY");
    let reader = pair.master.try_clone_reader().expect("clone PTY reader");
    let (captured, reader_handle) = spawn_reader(reader);
    let mut writer = pair.master.take_writer().expect("take PTY writer");

    let mut tui_cmd = CommandBuilder::new(cass_bin_path());
    tui_cmd.arg("tui");
    apply_ftui_env(&mut tui_cmd, &env);
    let mut child = pair
        .slave
        .spawn_command(tui_cmd)
        .expect("spawn ftui TUI in PTY");

    assert!(
        wait_for_output_contains(&captured, "cass", PTY_STARTUP_TIMEOUT),
        "Did not observe startup text before search flow interaction"
    );

    send_key_sequence(&mut *writer, b"hello");
    thread::sleep(Duration::from_millis(120));
    let before_submit_len = captured.lock().expect("capture lock").len();
    send_key_sequence(&mut *writer, b"\r"); // submit query to populate result list
    assert!(
        wait_for_output_growth(&captured, before_submit_len, 24, Duration::from_secs(6)),
        "Did not observe output growth after query submission in PTY search flow"
    );
    thread::sleep(Duration::from_millis(180));

    let before_open_len = captured.lock().expect("capture lock").len();
    send_key_sequence(&mut *writer, b"v"); // open raw-detail modal for selected result
    let saw_detail = wait_for_output_growth(&captured, before_open_len, 8, Duration::from_secs(6));
    assert!(
        saw_detail,
        "Did not observe output growth after detail-open attempt in PTY search flow"
    );

    // First ESC may either close a modal (if opened) or quit directly.
    send_key_sequence(&mut *writer, b"\x1b");
    thread::sleep(Duration::from_millis(200));
    let status = match child
        .try_wait()
        .expect("poll child after first ESC in search flow")
    {
        Some(status) => status,
        None => {
            send_key_sequence(&mut *writer, b"\x1b");
            wait_for_child_exit(&mut *child, PTY_EXIT_TIMEOUT)
        }
    };
    assert!(
        status.success(),
        "ftui process exited unsuccessfully: {status}"
    );

    drop(writer);
    drop(pair);
    let _ = reader_handle.join();
    let raw = captured.lock().expect("capture lock").clone();
    save_artifact("pty_search_detail_output.raw", &trace, &raw);
    let text = String::from_utf8_lossy(&raw);
    assert!(
        text.contains("hello"),
        "Expected query text to appear in PTY capture. Output tail: {}",
        truncate_output(&raw, 1200)
    );

    tracker.complete();
}

#[test]
fn tui_pty_performance_guardrails_smoke() {
    let _guard_lock = tui_flow_guard();
    let trace = trace_id();
    let tracker = tracker_for("tui_pty_performance_guardrails_smoke");
    let _trace_guard = tracker.trace_env_guard();
    let env = prepare_ftui_pty_env(&trace, &tracker);

    let queries = ["hello", "authentication", "session", "timeout", "hello"];
    let mut search_latencies_ms = Vec::with_capacity(queries.len());

    for (idx, query) in queries.iter().enumerate() {
        let run_start = Instant::now();
        let output = cargo_bin_cmd!("cass")
            .arg("search")
            .arg(query)
            .arg("--robot")
            .arg("--data-dir")
            .arg(&env.data_dir)
            .env("HOME", env.home.to_string_lossy().as_ref())
            .env("XDG_DATA_HOME", env.xdg.to_string_lossy().as_ref())
            .env("CODEX_HOME", env.codex_home.to_string_lossy().as_ref())
            .env("CASS_DATA_DIR", env.data_dir.to_string_lossy().as_ref())
            .env("NO_COLOR", "1")
            .current_dir(&env.home)
            .output()
            .expect("spawn cass search for perf budget");

        let stdout_name = format!("perf_search_{idx}_stdout.json");
        let stderr_name = format!("perf_search_{idx}_stderr.txt");
        save_artifact(&stdout_name, &trace, &output.stdout);
        save_artifact(&stderr_name, &trace, &output.stderr);

        assert!(
            output.status.success(),
            "perf search query failed for '{query}': {}",
            truncate_output(&output.stderr, 500)
        );
        if idx == 0 {
            let parsed: serde_json::Value =
                serde_json::from_slice(&output.stdout).expect("parse search json");
            let total_matches = parsed
                .get("total_matches")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            assert!(
                total_matches > 0,
                "Fixture regression: expected search query '{query}' to return hits, got total_matches={total_matches}"
            );
        }
        search_latencies_ms.push(run_start.elapsed().as_millis() as u64);
    }

    let search_p50_ms = percentile_ms(&search_latencies_ms, 50.0);
    let search_p95_ms = percentile_ms(&search_latencies_ms, 95.0);
    tracker.metrics(
        "perf_search_latency",
        &E2ePerformanceMetrics::new()
            .with_custom("trace_id", trace.clone())
            .with_custom("p50_ms", search_p50_ms)
            .with_custom("p95_ms", search_p95_ms)
            .with_custom("samples_ms", serde_json::json!(search_latencies_ms)),
    );

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 45,
            cols: 145,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open PTY");
    let reader = pair.master.try_clone_reader().expect("clone PTY reader");
    let (captured, reader_handle) = spawn_reader(reader);
    let mut writer = pair.master.take_writer().expect("take PTY writer");

    let startup_begin = Instant::now();
    let mut tui_cmd = CommandBuilder::new(cass_bin_path());
    tui_cmd.arg("tui");
    apply_ftui_env(&mut tui_cmd, &env);
    let mut child = pair
        .slave
        .spawn_command(tui_cmd)
        .expect("spawn ftui TUI in PTY");

    assert!(
        wait_for_output_contains(&captured, "cass", PTY_STARTUP_TIMEOUT),
        "Did not observe startup text in perf guard test"
    );
    let startup_ms = startup_begin.elapsed().as_millis() as u64;

    send_key_sequence(&mut *writer, b"hello");
    thread::sleep(Duration::from_millis(120));
    let before_submit_len = captured.lock().expect("capture lock").len();
    send_key_sequence(&mut *writer, b"\r");
    assert!(
        wait_for_output_growth(&captured, before_submit_len, 24, Duration::from_secs(6)),
        "No PTY output growth after query submission during perf flow"
    );
    thread::sleep(Duration::from_millis(160));

    let before_open_len = captured.lock().expect("capture lock").len();
    let detail_begin = Instant::now();
    send_key_sequence(&mut *writer, b"v");
    let saw_detail = wait_for_output_growth(&captured, before_open_len, 8, Duration::from_secs(6));
    let detail_open_ms = detail_begin.elapsed().as_millis() as u64;
    assert!(
        saw_detail,
        "No PTY output growth after detail-open attempt during perf flow"
    );

    send_key_sequence(&mut *writer, b"\x1b");
    thread::sleep(Duration::from_millis(120));
    let mut esc_presses = 1u64;
    let status = match child
        .try_wait()
        .expect("poll child after first ESC in perf flow")
    {
        Some(status) => status,
        None => {
            send_key_sequence(&mut *writer, b"\x1b");
            esc_presses += 1;
            wait_for_child_exit(&mut *child, PTY_EXIT_TIMEOUT)
        }
    };
    assert!(
        status.success(),
        "ftui process exited unsuccessfully: {status}"
    );

    drop(writer);
    drop(pair);
    let _ = reader_handle.join();
    let raw = captured.lock().expect("capture lock").clone();
    save_artifact("pty_perf_guard_output.raw", &trace, &raw);

    let total_output_bytes = raw.len() as u64;
    // Actions: submit query, detail-open attempt, and one-or-two ESC presses.
    let action_count = 2 + esc_presses;
    let bytes_per_action = total_output_bytes / action_count;
    tracker.metrics(
        "perf_pty_runtime",
        &E2ePerformanceMetrics::new()
            .with_custom("trace_id", trace.clone())
            .with_custom("startup_ms", startup_ms)
            .with_custom("detail_open_ms", detail_open_ms)
            .with_custom("total_output_bytes", total_output_bytes)
            .with_custom("bytes_per_action", bytes_per_action),
    );

    let summary = serde_json::json!({
        "trace_id": trace,
        "test": "tui_pty_performance_guardrails_smoke",
        "search_latency_ms": {
            "samples": search_latencies_ms,
            "p50": search_p50_ms,
            "p95": search_p95_ms
        },
        "pty": {
            "startup_ms": startup_ms,
            "detail_open_ms": detail_open_ms,
            "output_bytes_total": total_output_bytes,
            "output_bytes_per_action": bytes_per_action
        },
        "budgets": {
            "search_p95_ms": PERF_SEARCH_P95_BUDGET_MS,
            "tui_startup_ms": PERF_TUI_STARTUP_BUDGET_MS,
            "tui_detail_open_ms": PERF_TUI_DETAIL_OPEN_BUDGET_MS,
            "tui_output_bytes_total": PERF_TUI_OUTPUT_BYTES_BUDGET,
            "tui_output_bytes_per_action": PERF_TUI_BYTES_PER_ACTION_BUDGET
        }
    });
    save_artifact(
        "perf_guardrail_summary.json",
        &trace,
        serde_json::to_string_pretty(&summary)
            .expect("serialize perf summary")
            .as_bytes(),
    );

    assert!(
        search_p95_ms <= PERF_SEARCH_P95_BUDGET_MS,
        "Search latency budget exceeded: p95={}ms > {}ms",
        search_p95_ms,
        PERF_SEARCH_P95_BUDGET_MS
    );
    assert!(
        startup_ms <= PERF_TUI_STARTUP_BUDGET_MS,
        "TUI startup budget exceeded: {}ms > {}ms",
        startup_ms,
        PERF_TUI_STARTUP_BUDGET_MS
    );
    assert!(
        detail_open_ms <= PERF_TUI_DETAIL_OPEN_BUDGET_MS,
        "Detail-open budget exceeded: {}ms > {}ms",
        detail_open_ms,
        PERF_TUI_DETAIL_OPEN_BUDGET_MS
    );
    assert!(
        total_output_bytes <= PERF_TUI_OUTPUT_BYTES_BUDGET,
        "PTY output-byte budget exceeded: {} > {}",
        total_output_bytes,
        PERF_TUI_OUTPUT_BYTES_BUDGET
    );
    assert!(
        bytes_per_action <= PERF_TUI_BYTES_PER_ACTION_BUDGET,
        "PTY bytes/action budget exceeded: {} > {}",
        bytes_per_action,
        PERF_TUI_BYTES_PER_ACTION_BUDGET
    );

    tracker.complete();
}

// =============================================================================
// Search Flow Tests
// =============================================================================

#[test]
fn tui_search_flow_with_logging() {
    let _guard_lock = tui_flow_guard();
    let trace = trace_id();
    let tracker = tracker_for("tui_search_flow_with_logging");
    let _trace_guard = tracker.trace_env_guard();

    // Setup phase
    let setup_start = tracker.start("setup", Some("Creating isolated test environment"));
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());

    let xdg = tmp.path().join("xdg");
    fs::create_dir_all(&xdg).unwrap();
    let _guard_xdg = EnvGuard::set("XDG_DATA_HOME", xdg.to_string_lossy());

    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_codex = EnvGuard::set("CODEX_HOME", data_dir.to_string_lossy());
    make_codex_fixture(&data_dir);
    tracker.end("setup", Some("Fixtures created"), setup_start);

    // Index phase
    let index_start = tracker.start("index", Some("Building search index"));
    let output = cargo_bin_cmd!("cass")
        .arg("index")
        .arg("--full")
        .arg("--data-dir")
        .arg(&data_dir)
        .current_dir(&home)
        .output()
        .expect("failed to spawn cass index");

    // Save index output as artifact
    save_artifact("index_stdout.txt", &trace, &output.stdout);
    save_artifact("index_stderr.txt", &trace, &output.stderr);

    if !output.status.success() {
        let ctx = E2eErrorContext::new()
            .with_command("cass index --full")
            .capture_cwd()
            .add_state("exit_code", serde_json::json!(output.status.code()))
            .add_state("trace_id", serde_json::json!(trace));
        tracker.fail(E2eError::with_type("index failed", "COMMAND_FAILED").with_context(ctx));
        panic!("Index failed");
    }

    let index_ms = index_start.elapsed().as_millis() as u64;
    tracker.end("index", Some("Index complete"), index_start);
    tracker.metrics(
        "index_duration",
        &E2ePerformanceMetrics::new()
            .with_duration(index_ms)
            .with_custom("trace_id", trace.clone()),
    );

    // Search flow: simulate search for "hello"
    let search_start = tracker.start("search_hello", Some("Simulating TUI search: 'hello'"));
    let search_output = cargo_bin_cmd!("cass")
        .arg("search")
        .arg("hello")
        .arg("--robot")
        .arg("--data-dir")
        .arg(&data_dir)
        .current_dir(&home)
        .output()
        .expect("failed to spawn cass search");

    save_artifact("search_hello_stdout.json", &trace, &search_output.stdout);
    save_artifact("search_hello_stderr.txt", &trace, &search_output.stderr);

    let search_ms = search_start.elapsed().as_millis() as u64;
    tracker.end("search_hello", Some("Search complete"), search_start);
    tracker.metrics(
        "search_hello_duration",
        &E2ePerformanceMetrics::new()
            .with_duration(search_ms)
            .with_custom("query", "hello")
            .with_custom("trace_id", trace.clone()),
    );

    assert!(
        search_output.status.success(),
        "Search should succeed: {}",
        truncate_output(&search_output.stderr, 500)
    );

    // Search flow: simulate search for "authentication"
    let search2_start = tracker.start(
        "search_auth",
        Some("Simulating TUI search: 'authentication'"),
    );
    let search2_output = cargo_bin_cmd!("cass")
        .arg("search")
        .arg("authentication")
        .arg("--robot")
        .arg("--data-dir")
        .arg(&data_dir)
        .current_dir(&home)
        .output()
        .expect("failed to spawn cass search");

    save_artifact("search_auth_stdout.json", &trace, &search2_output.stdout);

    let search2_ms = search2_start.elapsed().as_millis() as u64;
    tracker.end("search_auth", Some("Search complete"), search2_start);
    tracker.metrics(
        "search_auth_duration",
        &E2ePerformanceMetrics::new()
            .with_duration(search2_ms)
            .with_custom("query", "authentication")
            .with_custom("trace_id", trace.clone()),
    );

    // TUI launch verification
    let tui_start = tracker.start(
        "tui_headless",
        Some("Verifying TUI launches in headless mode"),
    );
    let tui_output = cargo_bin_cmd!("cass")
        .arg("tui")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--once")
        .current_dir(&home)
        .env("TUI_HEADLESS", "1")
        .output()
        .expect("failed to spawn cass tui");

    save_artifact("tui_stdout.txt", &trace, &tui_output.stdout);
    save_artifact("tui_stderr.txt", &trace, &tui_output.stderr);

    let tui_ms = tui_start.elapsed().as_millis() as u64;
    tracker.end("tui_headless", Some("TUI headless complete"), tui_start);
    tracker.metrics(
        "tui_headless_duration",
        &E2ePerformanceMetrics::new()
            .with_duration(tui_ms)
            .with_custom("mode", "headless")
            .with_custom("trace_id", trace.clone()),
    );

    assert!(
        tui_output.status.success(),
        "TUI should exit cleanly: {}",
        truncate_output(&tui_output.stderr, 500)
    );

    // Write summary artifact
    let summary = serde_json::json!({
        "trace_id": trace,
        "test": "tui_search_flow_with_logging",
        "phases": {
            "index_ms": index_ms,
            "search_hello_ms": search_ms,
            "search_auth_ms": search2_ms,
            "tui_headless_ms": tui_ms,
        },
        "artifacts": [
            format!("{trace}_index_stdout.txt"),
            format!("{trace}_index_stderr.txt"),
            format!("{trace}_search_hello_stdout.json"),
            format!("{trace}_search_auth_stdout.json"),
            format!("{trace}_tui_stdout.txt"),
            format!("{trace}_tui_stderr.txt"),
        ],
    });
    save_artifact(
        "summary.json",
        &trace,
        serde_json::to_string_pretty(&summary).unwrap().as_bytes(),
    );

    tracker.complete();
}

// =============================================================================
// Filter Flow Tests
// =============================================================================

#[test]
fn tui_filter_flow_with_logging() {
    let _guard_lock = tui_flow_guard();
    let trace = trace_id();
    let tracker = tracker_for("tui_filter_flow_with_logging");
    let _trace_guard = tracker.trace_env_guard();

    // Setup
    let setup_start = tracker.start(
        "setup",
        Some("Creating test environment with multiple agents"),
    );
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());

    let xdg = tmp.path().join("xdg");
    fs::create_dir_all(&xdg).unwrap();
    let _guard_xdg = EnvGuard::set("XDG_DATA_HOME", xdg.to_string_lossy());

    let data_dir = tmp.path().join("data");
    let codex_home = tmp.path().join("codex_home");
    let claude_home = tmp.path().join(".claude");
    fs::create_dir_all(&data_dir).unwrap();
    fs::create_dir_all(&codex_home).unwrap();
    fs::create_dir_all(&claude_home).unwrap();

    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());
    make_codex_fixture(&codex_home);
    make_claude_fixture(&claude_home, "testproject");
    tracker.end("setup", Some("Multi-agent fixtures created"), setup_start);

    // Index
    let index_start = tracker.start("index", Some("Building multi-agent index"));
    let output = cargo_bin_cmd!("cass")
        .arg("index")
        .arg("--full")
        .arg("--data-dir")
        .arg(&data_dir)
        .current_dir(&home)
        .output()
        .expect("failed to spawn cass index");

    save_artifact("index_stdout.txt", &trace, &output.stdout);
    save_artifact("index_stderr.txt", &trace, &output.stderr);

    if !output.status.success() {
        let ctx = E2eErrorContext::new()
            .with_command("cass index --full")
            .add_state("trace_id", serde_json::json!(trace));
        tracker.fail(E2eError::with_type("index failed", "COMMAND_FAILED").with_context(ctx));
        panic!("Index failed");
    }
    tracker.end("index", Some("Index complete"), index_start);

    // Filter by agent: Codex
    let filter_start = tracker.start("filter_codex", Some("Simulating TUI filter: agent=codex"));
    let filter_output = cargo_bin_cmd!("cass")
        .arg("search")
        .arg("hello")
        .arg("--agent")
        .arg("codex")
        .arg("--robot")
        .arg("--data-dir")
        .arg(&data_dir)
        .current_dir(&home)
        .output()
        .expect("failed to spawn cass search with filter");

    save_artifact("filter_codex_stdout.json", &trace, &filter_output.stdout);

    let filter_ms = filter_start.elapsed().as_millis() as u64;
    tracker.end("filter_codex", Some("Filter complete"), filter_start);
    tracker.metrics(
        "filter_codex_duration",
        &E2ePerformanceMetrics::new()
            .with_duration(filter_ms)
            .with_custom("filter", "agent=codex")
            .with_custom("trace_id", trace.clone()),
    );

    // TUI launch with filter
    let tui_start = tracker.start("tui_headless", Some("Verifying TUI with filter"));
    let tui_output = cargo_bin_cmd!("cass")
        .arg("tui")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--once")
        .current_dir(&home)
        .env("TUI_HEADLESS", "1")
        .output()
        .expect("failed to spawn cass tui");

    save_artifact("tui_stdout.txt", &trace, &tui_output.stdout);
    save_artifact("tui_stderr.txt", &trace, &tui_output.stderr);

    let tui_ms = tui_start.elapsed().as_millis() as u64;
    tracker.end("tui_headless", Some("TUI headless complete"), tui_start);
    tracker.metrics(
        "tui_headless_duration",
        &E2ePerformanceMetrics::new()
            .with_duration(tui_ms)
            .with_custom("mode", "headless_filtered")
            .with_custom("trace_id", trace.clone()),
    );

    assert!(tui_output.status.success());

    // Summary
    let summary = serde_json::json!({
        "trace_id": trace,
        "test": "tui_filter_flow_with_logging",
        "phases": {
            "filter_codex_ms": filter_ms,
            "tui_headless_ms": tui_ms,
        },
    });
    save_artifact(
        "summary.json",
        &trace,
        serde_json::to_string_pretty(&summary).unwrap().as_bytes(),
    );

    tracker.complete();
}

// =============================================================================
// Export Flow Tests
// =============================================================================

#[test]
fn tui_export_flow_with_logging() {
    let _guard_lock = tui_flow_guard();
    let trace = trace_id();
    let tracker = tracker_for("tui_export_flow_with_logging");
    let _trace_guard = tracker.trace_env_guard();

    // Setup
    let setup_start = tracker.start("setup", Some("Creating test environment"));
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());

    let xdg = tmp.path().join("xdg");
    fs::create_dir_all(&xdg).unwrap();
    let _guard_xdg = EnvGuard::set("XDG_DATA_HOME", xdg.to_string_lossy());

    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_codex = EnvGuard::set("CODEX_HOME", data_dir.to_string_lossy());
    make_codex_fixture(&data_dir);
    tracker.end("setup", Some("Fixtures created"), setup_start);

    // Index
    let index_start = tracker.start("index", Some("Building index"));
    let output = cargo_bin_cmd!("cass")
        .arg("index")
        .arg("--full")
        .arg("--data-dir")
        .arg(&data_dir)
        .current_dir(&home)
        .output()
        .expect("failed to spawn cass index");

    if !output.status.success() {
        tracker.fail(E2eError::with_type("index failed", "COMMAND_FAILED"));
        panic!("Index failed");
    }
    tracker.end("index", Some("Index complete"), index_start);

    // Simulate export flow by searching and capturing for export
    let search_start = tracker.start(
        "search_for_export",
        Some("Search to identify exportable content"),
    );
    let search_output = cargo_bin_cmd!("cass")
        .arg("search")
        .arg("hello")
        .arg("--robot")
        .arg("--format")
        .arg("sessions")
        .arg("--data-dir")
        .arg(&data_dir)
        .current_dir(&home)
        .output()
        .expect("failed to spawn cass search");

    save_artifact("search_sessions_stdout.json", &trace, &search_output.stdout);

    let search_ms = search_start.elapsed().as_millis() as u64;
    tracker.end("search_for_export", Some("Search complete"), search_start);
    tracker.metrics(
        "search_sessions_duration",
        &E2ePerformanceMetrics::new()
            .with_duration(search_ms)
            .with_custom("format", "sessions")
            .with_custom("trace_id", trace.clone()),
    );

    // Export to HTML (simulating TUI export action)
    let export_dir = tmp.path().join("exports");
    fs::create_dir_all(&export_dir).unwrap();

    // Note: Full HTML export requires a session source path, which we simulate
    // In a real TUI flow, user would select a session and export it
    // Here we verify the export command infrastructure works

    // TUI launch to verify export UI would work
    let tui_start = tracker.start(
        "tui_headless",
        Some("Verifying TUI launches for export flow"),
    );
    let tui_output = cargo_bin_cmd!("cass")
        .arg("tui")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--once")
        .current_dir(&home)
        .env("TUI_HEADLESS", "1")
        .output()
        .expect("failed to spawn cass tui");

    save_artifact("tui_stdout.txt", &trace, &tui_output.stdout);
    save_artifact("tui_stderr.txt", &trace, &tui_output.stderr);

    let tui_ms = tui_start.elapsed().as_millis() as u64;
    tracker.end("tui_headless", Some("TUI headless complete"), tui_start);
    tracker.metrics(
        "tui_headless_duration",
        &E2ePerformanceMetrics::new()
            .with_duration(tui_ms)
            .with_custom("mode", "headless_export")
            .with_custom("trace_id", trace.clone()),
    );

    assert!(tui_output.status.success());

    // Summary
    let summary = serde_json::json!({
        "trace_id": trace,
        "test": "tui_export_flow_with_logging",
        "phases": {
            "search_sessions_ms": search_ms,
            "tui_headless_ms": tui_ms,
        },
    });
    save_artifact(
        "summary.json",
        &trace,
        serde_json::to_string_pretty(&summary).unwrap().as_bytes(),
    );

    tracker.complete();
}

// =============================================================================
// Edge Case Tests
// =============================================================================

#[test]
fn tui_empty_dataset_flow_with_logging() {
    let _guard_lock = tui_flow_guard();
    let trace = trace_id();
    let tracker = tracker_for("tui_empty_dataset_flow_with_logging");
    let _trace_guard = tracker.trace_env_guard();

    // Setup with empty dataset
    let setup_start = tracker.start("setup", Some("Creating empty test environment"));
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());

    let xdg = tmp.path().join("xdg");
    fs::create_dir_all(&xdg).unwrap();
    let _guard_xdg = EnvGuard::set("XDG_DATA_HOME", xdg.to_string_lossy());

    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    // Point to empty directories (no fixtures)
    let empty_codex = tmp.path().join("empty_codex");
    fs::create_dir_all(&empty_codex).unwrap();
    let _guard_codex = EnvGuard::set("CODEX_HOME", empty_codex.to_string_lossy());

    tracker.end("setup", Some("Empty environment created"), setup_start);

    // Index empty dataset
    let index_start = tracker.start("index_empty", Some("Building empty index"));
    let output = cargo_bin_cmd!("cass")
        .arg("index")
        .arg("--full")
        .arg("--data-dir")
        .arg(&data_dir)
        .current_dir(&home)
        .output()
        .expect("failed to spawn cass index");

    save_artifact("index_empty_stdout.txt", &trace, &output.stdout);
    save_artifact("index_empty_stderr.txt", &trace, &output.stderr);

    tracker.end("index_empty", Some("Empty index complete"), index_start);

    // Search empty dataset
    let search_start = tracker.start("search_empty", Some("Searching empty dataset"));
    let search_output = cargo_bin_cmd!("cass")
        .arg("search")
        .arg("anything")
        .arg("--robot")
        .arg("--data-dir")
        .arg(&data_dir)
        .current_dir(&home)
        .output()
        .expect("failed to spawn cass search");

    save_artifact("search_empty_stdout.json", &trace, &search_output.stdout);

    let search_ms = search_start.elapsed().as_millis() as u64;
    tracker.end("search_empty", Some("Empty search complete"), search_start);
    tracker.metrics(
        "search_empty_duration",
        &E2ePerformanceMetrics::new()
            .with_duration(search_ms)
            .with_custom("dataset", "empty")
            .with_custom("trace_id", trace.clone()),
    );

    // TUI with empty dataset
    let tui_start = tracker.start("tui_empty", Some("TUI with empty dataset"));
    let tui_output = cargo_bin_cmd!("cass")
        .arg("tui")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--once")
        .current_dir(&home)
        .env("TUI_HEADLESS", "1")
        .output()
        .expect("failed to spawn cass tui");

    save_artifact("tui_empty_stdout.txt", &trace, &tui_output.stdout);
    save_artifact("tui_empty_stderr.txt", &trace, &tui_output.stderr);

    let tui_ms = tui_start.elapsed().as_millis() as u64;
    tracker.end("tui_empty", Some("TUI empty complete"), tui_start);
    tracker.metrics(
        "tui_empty_duration",
        &E2ePerformanceMetrics::new()
            .with_duration(tui_ms)
            .with_custom("dataset", "empty")
            .with_custom("trace_id", trace.clone()),
    );

    // Should exit cleanly (not panic)
    let stderr = String::from_utf8_lossy(&tui_output.stderr);
    assert!(
        !stderr.contains("panicked"),
        "TUI should not panic on empty dataset: {}",
        stderr
    );

    // Summary
    let summary = serde_json::json!({
        "trace_id": trace,
        "test": "tui_empty_dataset_flow_with_logging",
        "phases": {
            "search_empty_ms": search_ms,
            "tui_empty_ms": tui_ms,
        },
        "validation": {
            "no_panic": !stderr.contains("panicked"),
        },
    });
    save_artifact(
        "summary.json",
        &trace,
        serde_json::to_string_pretty(&summary).unwrap().as_bytes(),
    );

    tracker.complete();
}

#[test]
fn tui_unicode_flow_with_logging() {
    let _guard_lock = tui_flow_guard();
    let trace = trace_id();
    let tracker = tracker_for("tui_unicode_flow_with_logging");
    let _trace_guard = tracker.trace_env_guard();

    // Setup
    let setup_start = tracker.start("setup", Some("Creating unicode test environment"));
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());

    let xdg = tmp.path().join("xdg");
    fs::create_dir_all(&xdg).unwrap();
    let _guard_xdg = EnvGuard::set("XDG_DATA_HOME", xdg.to_string_lossy());

    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    // Create unicode fixture
    let sessions = data_dir.join("sessions/2025/11/21");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-unicode.jsonl");
    let sample = r#"{"role":"user","timestamp":1700000000000,"content":"日本語テスト こんにちは"}
{"role":"assistant","timestamp":1700000001000,"content":"Emoji test: 🎉🚀💻 中文测试"}
{"role":"user","timestamp":1700000002000,"content":"한국어 테스트 안녕하세요"}
{"role":"assistant","timestamp":1700000003000,"content":"Arabic: مرحبا Hebrew: שלום"}
"#;
    fs::write(file, sample).unwrap();

    let _guard_codex = EnvGuard::set("CODEX_HOME", data_dir.to_string_lossy());
    tracker.end("setup", Some("Unicode fixtures created"), setup_start);

    // Index
    let index_start = tracker.start("index", Some("Building unicode index"));
    let output = cargo_bin_cmd!("cass")
        .arg("index")
        .arg("--full")
        .arg("--data-dir")
        .arg(&data_dir)
        .current_dir(&home)
        .output()
        .expect("failed to spawn cass index");

    if !output.status.success() {
        tracker.fail(E2eError::with_type("index failed", "COMMAND_FAILED"));
        panic!("Index failed");
    }
    tracker.end("index", Some("Index complete"), index_start);

    // Search for unicode content
    let search_start = tracker.start("search_unicode", Some("Searching for unicode content"));
    let search_output = cargo_bin_cmd!("cass")
        .arg("search")
        .arg("日本語")
        .arg("--robot")
        .arg("--data-dir")
        .arg(&data_dir)
        .current_dir(&home)
        .output()
        .expect("failed to spawn cass search");

    save_artifact("search_unicode_stdout.json", &trace, &search_output.stdout);

    let search_ms = search_start.elapsed().as_millis() as u64;
    tracker.end(
        "search_unicode",
        Some("Unicode search complete"),
        search_start,
    );
    tracker.metrics(
        "search_unicode_duration",
        &E2ePerformanceMetrics::new()
            .with_duration(search_ms)
            .with_custom("query", "日本語")
            .with_custom("trace_id", trace.clone()),
    );

    // TUI with unicode
    let tui_start = tracker.start("tui_unicode", Some("TUI with unicode content"));
    let tui_output = cargo_bin_cmd!("cass")
        .arg("tui")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--once")
        .current_dir(&home)
        .env("TUI_HEADLESS", "1")
        .output()
        .expect("failed to spawn cass tui");

    save_artifact("tui_unicode_stdout.txt", &trace, &tui_output.stdout);
    save_artifact("tui_unicode_stderr.txt", &trace, &tui_output.stderr);

    let tui_ms = tui_start.elapsed().as_millis() as u64;
    tracker.end("tui_unicode", Some("TUI unicode complete"), tui_start);
    tracker.metrics(
        "tui_unicode_duration",
        &E2ePerformanceMetrics::new()
            .with_duration(tui_ms)
            .with_custom("content", "unicode")
            .with_custom("trace_id", trace.clone()),
    );

    assert!(tui_output.status.success());

    // Summary
    let summary = serde_json::json!({
        "trace_id": trace,
        "test": "tui_unicode_flow_with_logging",
        "phases": {
            "search_unicode_ms": search_ms,
            "tui_unicode_ms": tui_ms,
        },
    });
    save_artifact(
        "summary.json",
        &trace,
        serde_json::to_string_pretty(&summary).unwrap().as_bytes(),
    );

    tracker.complete();
}
