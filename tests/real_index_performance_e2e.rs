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
//! They are skipped by default because CI and clean developer machines do not
//! have the multi-GB TRJ archive. When enabled, the suite fails hard on hangs,
//! non-JSON output, toy data dirs, and latency regressions.

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
