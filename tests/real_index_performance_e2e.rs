//! Opt-in real-index performance gates for the operator's local cass archive.
//!
//! These tests intentionally use the real on-disk data dir when explicitly
//! enabled:
//!
//! ```text
//! CASS_REAL_INDEX_E2E=1 \
//! CASS_REAL_INDEX_E2E_BIN=/home/ubuntu/.local/bin/cass \
//! cargo test --test real_index_performance_e2e -- --nocapture
//! ```
//!
//! Operator-archive checks are skipped by default because CI and clean developer machines do not
//! have the multi-GB TRJ archive. When enabled, the suite fails hard on hangs,
//! non-JSON output, toy data dirs, and latency regressions.
//! A separate synthetic-history regression below uses temporary sources only.

use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const DEFAULT_REAL_DATA_DIR: &str = "/home/ubuntu/.local/share/coding-agent-search";
const DEFAULT_MIN_DB_BYTES: u64 = 1_000_000_000;
const DEFAULT_HEALTH_MAX_MS: u128 = 1_500;
const DEFAULT_STATUS_MAX_MS: u128 = 3_000;
const DEFAULT_SEARCH_MAX_MS: u128 = 3_000;
const DEFAULT_INDEX_MAX_MS: u128 = 30_000;
const DEFAULT_TIMEOUT_SECS: u64 = 15;
const DEFAULT_INDEX_TIMEOUT_SECS: u64 = 45;
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const DIAG_STREAM_TAIL_BYTES: usize = 16 * 1024;
const MAX_CAPTURE_BYTES: usize = 64 * 1024 * 1024;

struct PipeCapture {
    bytes: Vec<u8>,
    truncated: bool,
}

impl PipeCapture {
    fn empty() -> Self {
        Self {
            bytes: Vec::new(),
            truncated: false,
        }
    }
}

struct RealIndexHarness {
    bin: String,
    data_dir: PathBuf,
    db_path: PathBuf,
}

struct TimedOutput {
    output: Output,
    elapsed: Duration,
}

fn env_truthy(key: &str) -> bool {
    dotenvy::var(key)
        .ok()
        .is_some_and(|value| matches!(value.trim(), "1" | "true" | "TRUE" | "yes" | "YES"))
}

fn env_u64(key: &str, default: u64) -> u64 {
    dotenvy::var(key)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(default)
}

fn env_u128(key: &str, default: u128) -> u128 {
    dotenvy::var(key)
        .ok()
        .and_then(|value| value.trim().parse::<u128>().ok())
        .unwrap_or(default)
}

fn real_index_harness() -> Result<Option<RealIndexHarness>> {
    if !env_truthy("CASS_REAL_INDEX_E2E") {
        eprintln!("skipping real-index E2E: set CASS_REAL_INDEX_E2E=1 to enable");
        return Ok(None);
    }

    let data_dir = dotenvy::var("CASS_REAL_INDEX_E2E_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_REAL_DATA_DIR));
    let db_path = data_dir.join("agent_search.db");
    let min_db_bytes = env_u64("CASS_REAL_INDEX_E2E_MIN_DB_BYTES", DEFAULT_MIN_DB_BYTES);
    let db_bytes = db_path
        .metadata()
        .with_context(|| {
            format!(
                "real-index E2E enabled but DB is not readable at {}",
                db_path.display()
            )
        })?
        .len();
    ensure!(
        db_bytes >= min_db_bytes,
        "real-index E2E must run against a large real archive, not a toy DB: {} has {} bytes, expected at least {}",
        db_path.display(),
        db_bytes,
        min_db_bytes
    );

    let index_dir = data_dir.join("index");
    ensure!(
        index_dir.exists(),
        "real-index E2E enabled but index dir is missing at {}",
        index_dir.display()
    );

    let bin = dotenvy::var("CASS_REAL_INDEX_E2E_BIN").unwrap_or_else(|_| cass_bin());
    Ok(Some(RealIndexHarness {
        bin,
        data_dir,
        db_path,
    }))
}

fn cass_bin() -> String {
    dotenvy::var("CARGO_BIN_EXE_cass")
        .ok()
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_cass").to_string())
}

fn real_cass_command(harness: &RealIndexHarness) -> Command {
    let mut cmd = Command::new(&harness.bin);
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");
    cmd.env("CASS_INDEX_NO_PROGRESS_EVENTS", "1");
    cmd.env("CASS_RAW_MIRROR_FSYNC", "0");
    cmd.env("RUST_BACKTRACE", "1");
    cmd
}

fn run_real_cass(
    harness: &RealIndexHarness,
    label: &str,
    args: &[&str],
    timeout: Duration,
) -> Result<TimedOutput> {
    let mut cmd = real_cass_command(harness);
    cmd.args(args);
    let start = Instant::now();
    let output = spawn_with_timeout_or_diag(cmd, label, &harness.data_dir, timeout)?;
    Ok(TimedOutput {
        output,
        elapsed: start.elapsed(),
    })
}

fn spawn_with_timeout_or_diag(
    mut cmd: Command,
    label: &str,
    data_dir: &Path,
    timeout: Duration,
) -> Result<Output> {
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    configure_child_process_group(&mut cmd);

    let start = Instant::now();
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawn_with_timeout_or_diag({label}): spawn failed"))?;
    let stdout_reader = spawn_pipe_reader(child.stdout.take());
    let stderr_reader = spawn_pipe_reader(child.stderr.take());
    let deadline = start + timeout;

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                kill_child_process_group(child.id());
                let stdout = join_pipe_reader(stdout_reader, label, "stdout");
                let stderr = join_pipe_reader(stderr_reader, label, "stderr");
                let stdout = full_output_or_error(stdout, label, "stdout")?;
                let stderr = full_output_or_error(stderr, label, "stderr")?;
                return Ok(Output {
                    status,
                    stdout,
                    stderr,
                });
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let pid = child.id();
                    kill_child_process_group(pid);
                    let _ = child.kill();
                    let _ = child.wait();
                    let stdout = join_pipe_reader(stdout_reader, label, "stdout");
                    let stderr = join_pipe_reader(stderr_reader, label, "stderr");
                    let stdout_tail = stream_tail(&stdout.bytes);
                    let stderr_tail = stream_tail(&stderr.bytes);

                    eprintln!();
                    eprintln!("================================================================");
                    eprintln!(
                        "TIMEOUT DIAGNOSTIC: phase={label:?} pid={pid} elapsed_ms={} timeout_ms={}",
                        start.elapsed().as_millis(),
                        timeout.as_millis(),
                    );
                    eprintln!("================================================================");
                    eprintln!("--- data_dir listing ({}):", data_dir.display());
                    for entry in list_dir_bounded(data_dir, 200) {
                        eprintln!("  {entry}");
                    }
                    if stdout.truncated {
                        eprintln!(
                            "--- child stdout exceeded capture cap ({} bytes); retained latest bytes only",
                            MAX_CAPTURE_BYTES
                        );
                    }
                    eprintln!(
                        "--- child stdout tail ({} bytes of up to {}):",
                        stdout_tail.len(),
                        DIAG_STREAM_TAIL_BYTES
                    );
                    eprintln!("{}", String::from_utf8_lossy(&stdout_tail));
                    if stderr.truncated {
                        eprintln!(
                            "--- child stderr exceeded capture cap ({} bytes); retained latest bytes only",
                            MAX_CAPTURE_BYTES
                        );
                    }
                    eprintln!(
                        "--- child stderr tail ({} bytes of up to {}):",
                        stderr_tail.len(),
                        DIAG_STREAM_TAIL_BYTES
                    );
                    eprintln!("{}", String::from_utf8_lossy(&stderr_tail));
                    eprintln!("================================================================");

                    bail!(
                        "subprocess phase {label:?} exceeded timeout of {:?} (see stderr diagnostic above)",
                        timeout
                    );
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(err) => {
                kill_child_process_group(child.id());
                let _ = child.kill();
                let _ = child.wait();
                let _ = join_pipe_reader(stdout_reader, label, "stdout");
                let _ = join_pipe_reader(stderr_reader, label, "stderr");
                bail!("spawn_with_timeout_or_diag({label}): try_wait errored: {err}");
            }
        }
    }
}

#[cfg(unix)]
fn configure_child_process_group(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;

    cmd.process_group(0);
}

#[cfg(not(unix))]
fn configure_child_process_group(_cmd: &mut Command) {}

#[cfg(unix)]
fn kill_child_process_group(pid: u32) {
    let process_group = format!("-{pid}");
    let _ = Command::new("/bin/kill")
        .args(["-KILL", &process_group])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(not(unix))]
fn kill_child_process_group(_pid: u32) {}

fn spawn_pipe_reader<R>(handle: Option<R>) -> JoinHandle<PipeCapture>
where
    R: Read + Send + 'static,
{
    std::thread::spawn(move || {
        let Some(mut handle) = handle else {
            return PipeCapture::empty();
        };
        read_pipe_to_capture(&mut handle)
    })
}

fn join_pipe_reader(
    reader: JoinHandle<PipeCapture>,
    label: &str,
    stream_name: &str,
) -> PipeCapture {
    match reader.join() {
        Ok(capture) => capture,
        Err(_) => {
            eprintln!("{label}: {stream_name} reader thread panicked");
            PipeCapture::empty()
        }
    }
}

fn full_output_or_error(capture: PipeCapture, label: &str, stream_name: &str) -> Result<Vec<u8>> {
    if capture.truncated {
        bail!(
            "spawn_with_timeout_or_diag({label}): {stream_name} exceeded capture cap of {} bytes",
            MAX_CAPTURE_BYTES
        );
    }
    Ok(capture.bytes)
}

fn read_pipe_to_capture<R: Read>(handle: &mut R) -> PipeCapture {
    let mut bytes = Vec::new();
    let mut scratch = [0_u8; 8192];
    let mut write_pos = 0_usize;
    let mut truncated = false;

    loop {
        let read = match handle.read(&mut scratch) {
            Ok(0) => break,
            Ok(read) => read,
            Err(_) => break,
        };

        if bytes.len() < MAX_CAPTURE_BYTES {
            let remaining = MAX_CAPTURE_BYTES - bytes.len();
            let keep = read.min(remaining);
            bytes.extend_from_slice(&scratch[..keep]);
            if keep == read {
                continue;
            }
            truncated = true;
            for &byte in &scratch[keep..read] {
                bytes[write_pos] = byte;
                write_pos = (write_pos + 1) % MAX_CAPTURE_BYTES;
            }
        } else {
            truncated = true;
            for &byte in &scratch[..read] {
                bytes[write_pos] = byte;
                write_pos = (write_pos + 1) % MAX_CAPTURE_BYTES;
            }
        }
    }

    if truncated && write_pos != 0 {
        let mut ordered = Vec::with_capacity(bytes.len());
        ordered.extend_from_slice(&bytes[write_pos..]);
        ordered.extend_from_slice(&bytes[..write_pos]);
        bytes = ordered;
    }

    PipeCapture { bytes, truncated }
}

fn stream_tail(buf: &[u8]) -> Vec<u8> {
    if buf.len() > DIAG_STREAM_TAIL_BYTES {
        let tail_start = buf.len() - DIAG_STREAM_TAIL_BYTES;
        buf[tail_start..].to_vec()
    } else {
        buf.to_vec()
    }
}

fn list_dir_bounded(root: &Path, limit: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.flatten() {
            if out.len() >= limit {
                out.push(format!("  ... (truncated at {limit} entries)"));
                return out;
            }
            let path = entry.path();
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            match std::fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    out.push(format!("{rel} (symlink)"));
                }
                Ok(metadata) if metadata.is_dir() => {
                    out.push(format!("{rel}/"));
                    stack.push(path);
                }
                Ok(metadata) => out.push(format!("{rel} ({} bytes)", metadata.len())),
                Err(_) => out.push(format!("{rel} (metadata unavailable)")),
            }
        }
    }
    out
}

#[cfg(unix)]
#[test]
fn list_dir_bounded_reports_symlinked_directories_without_following() -> Result<()> {
    let tmp = tempfile::TempDir::new().context("create temp dir")?;
    let root = tmp.path().join("root");
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(&root).context("create root")?;
    std::fs::create_dir_all(&outside).context("create outside")?;
    std::fs::write(outside.join("outside-only.txt"), b"should not be listed")
        .context("write outside marker")?;
    std::os::unix::fs::symlink(&outside, root.join("linked-outside"))
        .context("create symlinked directory")?;

    let entries = list_dir_bounded(&root, 20);

    assert!(
        entries
            .iter()
            .any(|entry| entry == "linked-outside (symlink)"),
        "diagnostic listing must identify the symlink itself: {entries:?}"
    );
    assert!(
        entries
            .iter()
            .all(|entry| !entry.contains("outside-only.txt")),
        "diagnostic listing must not follow symlinked directories outside the data dir: {entries:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn timeout_kills_shell_grandchild_pipe_holders() -> Result<()> {
    let tmp = tempfile::TempDir::new().context("create temp dir")?;
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c").arg("sleep 30");

    let start = Instant::now();
    let error = spawn_with_timeout_or_diag(
        cmd,
        "intentional_shell_hang",
        tmp.path(),
        Duration::from_millis(300),
    )
    .expect_err("hung shell command should time out");

    ensure!(
        start.elapsed() < Duration::from_secs(5),
        "timeout helper must not wait for the shell grandchild to finish"
    );
    ensure!(
        error.to_string().contains("exceeded timeout"),
        "unexpected timeout error: {error:#}"
    );
    Ok(())
}

fn parse_json_stdout(label: &str, output: &Output) -> Result<Value> {
    serde_json::from_slice(&output.stdout).with_context(|| {
        format!(
            "{label} did not emit valid JSON on stdout\nstatus: {:?}\nstdout:\n{}\nstderr:\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn assert_elapsed(label: &str, elapsed: Duration, max_ms: u128, output: &Output) -> Result<()> {
    let elapsed_ms = elapsed.as_millis();
    ensure!(
        elapsed_ms <= max_ms,
        "{label} was too slow: elapsed_ms={elapsed_ms}, max_ms={max_ms}\nstatus: {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

fn assert_success(label: &str, output: &Output) -> Result<()> {
    ensure!(
        output.status.success(),
        "{label} failed: status={:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

fn data_dir_arg(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[test]
fn real_health_and_status_are_bounded_on_huge_archive() -> Result<()> {
    let Some(harness) = real_index_harness()? else {
        return Ok(());
    };
    eprintln!(
        "real-index E2E using bin={} data_dir={} db={}",
        harness.bin,
        harness.data_dir.display(),
        harness.db_path.display()
    );

    let data_dir = data_dir_arg(&harness.data_dir);
    let timeout = Duration::from_secs(env_u64(
        "CASS_REAL_INDEX_E2E_TIMEOUT_SECS",
        DEFAULT_TIMEOUT_SECS,
    ));

    let health = run_real_cass(
        &harness,
        "real_health_json",
        &["health", "--json", "--data-dir", &data_dir],
        timeout,
    )?;
    let health_json = parse_json_stdout("real_health_json", &health.output)?;
    ensure!(
        health.output.status.success() || health.output.status.code() == Some(1),
        "health may report not-ready with exit 1, but it must not crash: status={:?}\njson={health_json:#}",
        health.output.status.code()
    );
    assert_elapsed(
        "real_health_json",
        health.elapsed,
        env_u128("CASS_REAL_INDEX_E2E_HEALTH_MAX_MS", DEFAULT_HEALTH_MAX_MS),
        &health.output,
    )?;

    let status = run_real_cass(
        &harness,
        "real_status_json",
        &["status", "--json", "--data-dir", &data_dir],
        timeout,
    )?;
    let status_json = parse_json_stdout("real_status_json", &status.output)?;
    ensure!(
        status_json.get("coverage_risk").is_some(),
        "status JSON must include the bounded coverage summary: {status_json:#}"
    );
    assert_elapsed(
        "real_status_json",
        status.elapsed,
        env_u128("CASS_REAL_INDEX_E2E_STATUS_MAX_MS", DEFAULT_STATUS_MAX_MS),
        &status.output,
    )?;
    Ok(())
}

#[test]
fn real_search_modes_are_bounded_on_huge_archive() -> Result<()> {
    let Some(harness) = real_index_harness()? else {
        return Ok(());
    };
    let data_dir = data_dir_arg(&harness.data_dir);
    let timeout = Duration::from_secs(env_u64(
        "CASS_REAL_INDEX_E2E_TIMEOUT_SECS",
        DEFAULT_TIMEOUT_SECS,
    ));
    let max_ms = env_u128("CASS_REAL_INDEX_E2E_SEARCH_MAX_MS", DEFAULT_SEARCH_MAX_MS);

    let cases: [(&str, Vec<&str>); 5] = [
        (
            "real_search_robot_meta",
            vec![
                "search",
                "authentication error",
                "--robot",
                "--robot-meta",
                "--fields",
                "minimal",
                "--limit",
                "5",
                "--data-dir",
                &data_dir,
            ],
        ),
        (
            "real_search_explicit_lexical",
            vec![
                "search",
                "cass status",
                "--robot",
                "--mode",
                "lexical",
                "--fields",
                "minimal",
                "--limit",
                "5",
                "--data-dir",
                &data_dir,
            ],
        ),
        (
            "real_search_workspace_filter",
            vec![
                "search",
                "README",
                "--robot",
                "--workspace",
                "/data/projects/coding_agent_session_search",
                "--fields",
                "minimal",
                "--limit",
                "5",
                "--data-dir",
                &data_dir,
            ],
        ),
        (
            "real_search_aggregate",
            vec![
                "search",
                "index",
                "--robot",
                "--aggregate",
                "agent,date",
                "--limit",
                "5",
                "--data-dir",
                &data_dir,
            ],
        ),
        (
            "real_search_project_name",
            vec![
                "search",
                "Dicklesworthstone",
                "--robot",
                "--fields",
                "minimal",
                "--limit",
                "5",
                "--data-dir",
                &data_dir,
            ],
        ),
    ];

    for (label, args) in cases {
        let timed = run_real_cass(&harness, label, &args, timeout)?;
        assert_success(label, &timed.output)?;
        let json = parse_json_stdout(label, &timed.output)?;
        ensure!(
            json.is_object() || json.is_array(),
            "{label} must emit structured robot JSON: {json:#}"
        );
        assert_elapsed(label, timed.elapsed, max_ms, &timed.output)?;
    }
    Ok(())
}

#[test]
fn real_incremental_index_startup_is_bounded_on_huge_archive() -> Result<()> {
    let Some(harness) = real_index_harness()? else {
        return Ok(());
    };
    let data_dir = data_dir_arg(&harness.data_dir);
    let timeout = Duration::from_secs(env_u64(
        "CASS_REAL_INDEX_E2E_INDEX_TIMEOUT_SECS",
        DEFAULT_INDEX_TIMEOUT_SECS,
    ));
    let max_ms = env_u128("CASS_REAL_INDEX_E2E_INDEX_MAX_MS", DEFAULT_INDEX_MAX_MS);

    let timed = run_real_cass(
        &harness,
        "real_incremental_index_json",
        &[
            "index",
            "--json",
            "--no-progress-events",
            "--data-dir",
            &data_dir,
        ],
        timeout,
    )?;
    let json = parse_json_stdout("real_incremental_index_json", &timed.output)?;
    ensure!(
        timed.output.status.success(),
        "incremental index must finish successfully on the real archive; json={json:#}\nstderr:\n{}",
        String::from_utf8_lossy(&timed.output.stderr)
    );
    assert_elapsed(
        "real_incremental_index_json",
        timed.elapsed,
        max_ms,
        &timed.output,
    )?;
    Ok(())
}

/// GH537: exercise actual provider parsing and canonical/lexical indexing.
/// The default fixture protects work counts; the installation-sized run is
/// explicit because its canonical/lexical assets require several GiB of space.
mod synthetic_history {
    use super::*;
    use coding_agent_search::franken_sync::Connection;
    use coding_agent_search::franken_sync::compat::{ConnectionExt, RowExt};
    use coding_agent_search::franken_sync::params;
    use coding_agent_search::indexer::{
        IndexOptions, IndexingProgress, run_index_with_local_connector_roots,
    };
    use coding_agent_search::storage::sqlite::FrankenStorage;
    use std::collections::HashMap;
    use std::fs::{self, File};
    use std::io::{BufWriter, Write};
    use std::sync::Arc;

    const CHILD: &str = "synthetic_history::gh537_index_child";
    const START_MS: i64 = 1_778_544_000_000;

    fn messages_in_session(index: usize, conversations: usize, messages: usize) -> usize {
        messages / conversations + usize::from(index < messages % conversations)
    }

    fn open_store(path: &Path, schema: &str) -> Result<Connection> {
        fs::create_dir_all(path.parent().context("store parent")?)?;
        let connection = Connection::open(path.to_string_lossy().as_ref())?;
        connection.execute_batch("PRAGMA journal_mode=WAL;")?;
        connection.execute_batch(schema)?;
        Ok(connection)
    }

    fn make_corpus(root: &Path, counts: [usize; 4], total_messages: usize) -> Result<()> {
        let conversations: usize = counts.iter().sum();
        ensure!(counts.iter().all(|count| *count > 0) && total_messages >= conversations);
        let opencode = open_store(
            &root.join("sources/opencode/opencode.db"),
            "CREATE TABLE session_v2(id TEXT PRIMARY KEY, project_id TEXT, directory TEXT,
                title TEXT, time_created INTEGER, time_updated INTEGER);
             CREATE TABLE session_message(id TEXT PRIMARY KEY, session_id TEXT, type TEXT,
                seq INTEGER, time_created INTEGER, time_updated INTEGER, data TEXT);
             CREATE INDEX session_message_session ON session_message(session_id);",
        )?;
        let devin = open_store(
            &root.join("sources/devin/sessions.db"),
            "CREATE TABLE sessions(id TEXT PRIMARY KEY, title TEXT, working_directory TEXT,
                model TEXT, agent_mode TEXT, created_at INTEGER, last_activity_at INTEGER,
                main_chain_id INTEGER, hidden INTEGER DEFAULT 0);
             CREATE TABLE message_nodes(node_id INTEGER PRIMARY KEY, session_id TEXT,
                parent_node_id INTEGER, chat_message TEXT, created_at INTEGER);
             CREATE INDEX message_nodes_session ON message_nodes(session_id);",
        )?;
        let mut global = 0usize;
        let mut node_id = 0i64;
        for (provider, count) in counts.into_iter().enumerate() {
            for session in 0..count {
                let messages = messages_in_session(global, conversations, total_messages);
                let id = format!("history-{provider}-{session}");
                let workspace = format!("/work/project-{}", session % 16);
                let start = START_MS + global as i64 * 1_000;
                if provider < 2 {
                    let path = if provider == 0 {
                        root.join(format!(
                            "sources/claude/projects/project-{}/{}.jsonl",
                            session % 16,
                            id
                        ))
                    } else {
                        root.join(format!(
                            "sources/codex/sessions/2026/05/{:02}/rollout-{id}.jsonl",
                            session % 28 + 1
                        ))
                    };
                    fs::create_dir_all(path.parent().context("transcript parent")?)?;
                    let mut file = BufWriter::new(File::create(&path)?);
                    if provider == 1 {
                        writeln!(
                            file,
                            "{}",
                            serde_json::json!({"type":"session_meta", "timestamp":start,
                            "payload":{"id":id,"cwd":workspace}})
                        )?;
                    }
                    for message in 0..messages {
                        let role = if message.is_multiple_of(2) {
                            "user"
                        } else {
                            "assistant"
                        };
                        let content = format!("history probe session {global} message {message}");
                        let timestamp = start + message as i64;
                        let row = if provider == 0 {
                            serde_json::json!({"type":role,"sessionId":id,
                                "uuid":format!("{id}-{message}"),"cwd":workspace,
                                "timestamp":timestamp,"message":{"role":role,"content":content}})
                        } else {
                            serde_json::json!({"type":"response_item","timestamp":timestamp,
                                "payload":{"type":"message","role":role,"content":[{
                                    "type":if role == "user" {"input_text"} else {"output_text"},
                                    "text":content}]}})
                        };
                        writeln!(file, "{row}")?;
                    }
                    file.flush()?;
                    file.get_ref().set_modified(
                        std::time::UNIX_EPOCH + Duration::from_millis(start as u64),
                    )?;
                } else if provider == 2 {
                    opencode.execute_batch("BEGIN;")?;
                    opencode.execute_compat(
                        "INSERT INTO session_v2 VALUES(?1,'history',?2,?1,?3,?4)",
                        params![
                            id.as_str(),
                            workspace.as_str(),
                            start,
                            start + messages as i64
                        ],
                    )?;
                    for message in 0..messages {
                        let role = if message.is_multiple_of(2) {
                            "user"
                        } else {
                            "assistant"
                        };
                        let data = if role == "user" {
                            serde_json::json!({"text":format!("history probe session {global} message {message}")})
                        } else {
                            serde_json::json!({"content":[{"type":"text",
                                "text":format!("history probe session {global} message {message}")}]})
                        };
                        opencode.execute_compat(
                            "INSERT INTO session_message VALUES(?1,?2,?3,?4,?5,?5,?6)",
                            params![
                                format!("{id}-{message}"),
                                id.as_str(),
                                role,
                                message as i64,
                                start + message as i64,
                                data.to_string()
                            ],
                        )?;
                    }
                    opencode.execute_batch("COMMIT;")?;
                } else {
                    devin.execute_batch("BEGIN;")?;
                    let mut parent: Option<i64> = None;
                    for message in 0..messages {
                        node_id += 1;
                        let data = serde_json::json!({"message_id":format!("{id}-{message}"),
                            "role":if message.is_multiple_of(2) {"user"} else {"assistant"},
                            "content":format!("history probe session {global} message {message}")});
                        devin.execute_compat(
                            "INSERT INTO message_nodes VALUES(?1,?2,?3,?4,?5)",
                            params![
                                node_id,
                                id.as_str(),
                                parent,
                                data.to_string(),
                                start / 1_000 + message as i64
                            ],
                        )?;
                        parent = Some(node_id);
                    }
                    devin.execute_compat(
                        "INSERT INTO sessions VALUES(?1,?1,?2,'synthetic','default',?3,?4,?5,0)",
                        params![
                            id.as_str(),
                            workspace.as_str(),
                            start / 1_000,
                            start / 1_000 + messages as i64,
                            parent
                        ],
                    )?;
                    devin.execute_batch("COMMIT;")?;
                }
                global += 1;
            }
        }
        opencode.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        devin.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        fs::write(
            root.join("shape.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "provider_counts":counts,"conversations":conversations,"messages":total_messages,
                "payload":"short synthetic text; preserves session/message counts, not the reported 49.5 GB"
            }))?,
        )?;
        Ok(())
    }

    fn append_shared_sibling(root: &Path) -> Result<()> {
        let conn = Connection::open(
            root.join("sources/opencode/opencode.db")
                .to_string_lossy()
                .as_ref(),
        )?;
        let now = FrankenStorage::now_millis();
        conn.execute_batch("BEGIN;")?;
        conn.execute_compat(
            "UPDATE session_v2 SET time_updated=?1 WHERE id='history-2-0'",
            params![now],
        )?;
        conn.execute_compat("INSERT INTO session_message VALUES('new-sibling-message','history-2-0','user',99999,?1,?1,?2)",
            params![now,r#"{"text":"new sibling message after the unchanged passes"}"#])?;
        conn.execute_batch("COMMIT; PRAGMA wal_checkpoint(TRUNCATE);")?;
        Ok(())
    }

    fn run_pass(root: &Path, full: bool, raw_mirror: bool, label: &str) -> Result<Value> {
        let mut command = Command::new(std::env::current_exe()?);
        command
            .args(["--exact", CHILD, "--ignored", "--nocapture"])
            .env("CASS_HISTORY_PROBE_ROOT", root)
            .env("CASS_HISTORY_PROBE_FULL", if full { "1" } else { "0" })
            .env("CASS_IGNORE_SOURCES_CONFIG", "1")
            .env("CASS_EXCLUDE_PATHS", "")
            .env("CASS_EXCLUDE_WORKSPACES", "")
            .env("CASS_RAW_MIRROR", if raw_mirror { "1" } else { "0" })
            .env("CASS_AUTO_REFRESH", "0")
            .env("CASS_STREAMING_INDEX", "1")
            .env("RUST_MIN_STACK", "134217728");
        let output = spawn_with_timeout_or_diag(
            command,
            label,
            &root.join("archive"),
            Duration::from_secs(env_u64("CASS_HISTORY_PROBE_TIMEOUT_SECS", 1_800)),
        )?;
        ensure!(
            output.status.success(),
            "{label}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout)?;
        let line = stdout
            .lines()
            // The single-threaded test harness may prefix the test name.
            .find_map(|line| line.split_once("GH537_PASS ").map(|(_, report)| report))
            .context("history probe child emitted no measurement")?;
        let report: Value = serde_json::from_str(line)?;
        println!(
            "GH537_HISTORY {}",
            serde_json::json!({"phase":label,"measurement":report})
        );
        Ok(report)
    }

    fn run_case(counts: [usize; 4], messages: usize, raw_mirror: bool) -> Result<()> {
        let root = tempfile::tempdir()?;
        make_corpus(root.path(), counts, messages)?;
        let expected_conversations: usize = counts.iter().sum();
        let initial = run_pass(root.path(), true, raw_mirror, "initial-full")?;
        assert_eq!(initial["canonical_conversations"], expected_conversations);
        assert_eq!(initial["canonical_messages"], messages);
        for pass in 0..3 {
            let unchanged = run_pass(root.path(), false, raw_mirror, &format!("unchanged-{pass}"))?;
            assert_eq!(
                unchanged["delivered_conversations"], 0,
                "unchanged history must not be delivered to canonical or lexical ingestion"
            );
            assert_eq!(unchanged["delivered_messages"], 0);
            assert_eq!(unchanged["canonical_messages"], messages);
        }
        append_shared_sibling(root.path())?;
        let changed = run_pass(root.path(), false, raw_mirror, "one-shared-sibling-changed")?;
        assert_eq!(
            changed["delivered_conversations"], 1,
            "a shared DB change must not replay its old siblings"
        );
        assert_eq!(changed["canonical_conversations"], expected_conversations);
        assert_eq!(changed["canonical_messages"], messages + 1);
        let settled = run_pass(root.path(), false, raw_mirror, "unchanged-after-sibling")?;
        // OpenCode deliberately overlaps the timestamp cutoff by one second.
        // An immediate pass may revisit that one recently changed session;
        // the old siblings must stay out regardless of machine speed.
        let changed_session_messages =
            messages_in_session(counts[0] + counts[1], expected_conversations, messages) + 1;
        assert!(settled["delivered_conversations"].as_u64().unwrap() <= 1);
        assert!(settled["delivered_messages"].as_u64().unwrap() <= changed_session_messages as u64);
        assert_eq!(settled["canonical_messages"], messages + 1);
        Ok(())
    }

    #[test]
    fn gh537_unchanged_history_and_shared_sibling_work_counts() -> Result<()> {
        run_case([4, 4, 4, 4], 48, false)
    }

    #[test]
    #[ignore = "GH537 report-sized 31,361-session/1.71M-message index diagnostic; requires several GiB of free disk"]
    fn gh537_report_sized_incremental_history() -> Result<()> {
        // The four reported provider counts sum to 31,360. Attribute the one
        // unspecified session to Claude and record the exact shape in output.
        let raw_mirror = dotenvy::var("CASS_HISTORY_PROBE_RAW_MIRROR")
            .map(|value| value != "0")
            .unwrap_or(true);
        run_case([12_968, 9_505, 7_126, 1_762], 1_710_000, raw_mirror)
    }

    fn read_io_counters() -> Option<[u64; 2]> {
        let text = fs::read_to_string("/proc/self/io").ok()?;
        let value = |key: &str| {
            text.lines()
                .find_map(|line| line.strip_prefix(key)?.trim().parse::<u64>().ok())
        };
        Some([value("rchar:")?, value("read_bytes:")?])
    }

    #[test]
    #[ignore = "fresh-process worker invoked by GH537 workload tests"]
    fn gh537_index_child() -> Result<()> {
        let root = PathBuf::from(
            dotenvy::var("CASS_HISTORY_PROBE_ROOT").context("run via GH537 workload driver")?,
        );
        let data_dir = root.join("archive");
        let db_path = data_dir.join("agent_search.db");
        let progress = Arc::new(IndexingProgress::default());
        let options = IndexOptions {
            full: dotenvy::var("CASS_HISTORY_PROBE_FULL").is_ok_and(|value| value == "1"),
            force_rebuild: false,
            watch: false,
            watch_once_paths: None,
            db_path: db_path.clone(),
            data_dir,
            semantic: false,
            build_hnsw: false,
            embedder: "hash".to_string(),
            progress: Some(Arc::clone(&progress)),
            watch_interval_secs: 30,
        };
        let roots = HashMap::from([
            ("claude".to_string(), vec![root.join("sources/claude")]),
            ("codex".to_string(), vec![root.join("sources/codex")]),
            (
                "opencode".to_string(),
                vec![root.join("sources/opencode/opencode.db")],
            ),
            (
                "devin".to_string(),
                vec![root.join("sources/devin/sessions.db")],
            ),
        ]);
        let io_before = read_io_counters();
        let start = Instant::now();
        run_index_with_local_connector_roots(options, roots, None)?;
        let wall_ms = start.elapsed().as_millis();
        let io_delta = io_before.zip(read_io_counters()).map(|(before, after)| {
            serde_json::json!({
                "logical_read_bytes":after[0].saturating_sub(before[0]),
                "physical_read_bytes":after[1].saturating_sub(before[1]),
            })
        });
        let stats = progress
            .stats
            .lock()
            .map_err(|_| anyhow::anyhow!("progress stats poisoned"))?
            .clone();
        ensure!(
            !stats.scan_had_errors,
            "synthetic source scan had errors: {:?}",
            stats.connector_diagnostics
        );
        let storage = FrankenStorage::open_readonly(&db_path)?;
        let conversations: i64 = storage.raw().query_row_map(
            "SELECT COUNT(*) FROM conversations",
            params![],
            |row| row.get_typed(0),
        )?;
        let messages: i64 =
            storage
                .raw()
                .query_row_map("SELECT COUNT(*) FROM messages", params![], |row| {
                    row.get_typed(0)
                })?;
        let rss_kib = fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|status| {
                status.lines().find_map(|line| {
                    line.strip_prefix("VmHWM:")?
                        .split_whitespace()
                        .next()?
                        .parse::<u64>()
                        .ok()
                })
            });
        println!(
            "GH537_PASS {}",
            serde_json::json!({
                "wall_ms":wall_ms,"process_peak_rss_kib":rss_kib,"process_io":io_delta,
                "delivered_conversations":stats.connectors.iter().map(|c|c.conversations).sum::<usize>(),
                "delivered_messages":stats.connectors.iter().map(|c|c.messages).sum::<usize>(),
                "counter_scope":"normalized sessions delivered to ingestion, not internal parser decoding",
                "canonical_conversations":conversations,"canonical_messages":messages,
                "statistics":stats,"debug_assertions":cfg!(debug_assertions),
                "raw_mirror_enabled":coding_agent_search::raw_mirror::capture_enabled(),
                "shape":serde_json::from_slice::<Value>(&fs::read(root.join("shape.json"))?)?,
            })
        );
        Ok(())
    }
}
