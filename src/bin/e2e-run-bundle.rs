//! Strict, run-scoped E2E execution and immutable evidence verification.
//!
//! This binary is intentionally separate from the shipped `cass` CLI. RCH runs
//! it on one explicitly pinned worker; it launches the real Cargo E2E targets,
//! captures bounded stdout/stderr, validates only that run's JSONL, and seals a
//! digest manifest. The repository acceptance script independently verifies the
//! retrieved bytes with shell, jq, and SHA-256 before publishing a report; the
//! large remote debug binary is not part of the evidence bundle.

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use clap::{Args, Parser, Subcommand};
use coding_agent_search::proof_artifact::{
    E2E_RUN_BUNDLE_SCHEMA_VERSION, E2eRunBundleExpectation, E2eRunBundleMetadata, E2eRunGateResult,
    finalize_e2e_run_bundle, validate_e2e_run_id, validate_e2e_source_identity,
    validate_rch_worker_id, verify_e2e_run_bundle,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

const DEFAULT_TIMEOUT_SECONDS: u64 = 7_200;
const DEFAULT_STREAM_CAP_BYTES: u64 = 16 * 1024 * 1024;
const MAX_STREAM_CAP_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Parser)]
#[command(
    name = "e2e-run-bundle",
    about = "Run or verify one immutable strict-RCH E2E evidence bundle"
)]
struct Cli {
    #[command(subcommand)]
    command: BundleCommand,
}

#[derive(Debug, Subcommand)]
enum BundleCommand {
    /// Execute real Cargo E2E targets and seal their remote artifacts.
    Run(RunArgs),
    /// Verify a retrieved bundle's complete file set, hashes, and provenance.
    Verify(VerifyArgs),
}

#[derive(Debug, Args)]
struct RunArgs {
    #[arg(long)]
    run_id: String,
    #[arg(long)]
    worker_id: String,
    #[arg(long)]
    source_sha: String,
    #[arg(long)]
    source_diff_sha256: String,
    #[arg(long, default_value_t = DEFAULT_TIMEOUT_SECONDS)]
    timeout_seconds: u64,
    #[arg(long, default_value_t = DEFAULT_STREAM_CAP_BYTES)]
    max_stream_bytes: u64,
    /// Arguments passed to `cargo test` (after a literal `--`).
    #[arg(last = true, required = true, allow_hyphen_values = true)]
    cargo_args: Vec<String>,
}

#[derive(Debug, Args)]
struct VerifyArgs {
    #[arg(long)]
    bundle: PathBuf,
    #[arg(long)]
    run_id: Option<String>,
    #[arg(long)]
    worker_id: Option<String>,
    #[arg(long)]
    worker_hostname: Option<String>,
    #[arg(long)]
    source_sha: Option<String>,
    #[arg(long)]
    source_diff_sha256: Option<String>,
    /// Check byte integrity and provenance while permitting a recorded test failure.
    #[arg(long)]
    integrity_only: bool,
}

#[derive(Debug, Clone, Copy)]
struct CaptureStats {
    observed_bytes: u64,
    persisted_bytes: u64,
    truncated: bool,
}

#[derive(Debug)]
struct CommandResult {
    exit_code: i32,
    timed_out: bool,
    elapsed_ms: u64,
    stdout: CaptureStats,
    stderr: CaptureStats,
}

#[derive(Debug, Serialize)]
struct RunStartedSummary<'a> {
    schema_version: u32,
    kind: &'static str,
    run_id: &'a str,
    worker_id: &'a str,
    worker_hostname: &'a str,
    source_sha: &'a str,
    source_diff_sha256: &'a str,
    run_root: String,
}

#[derive(Debug, Serialize)]
struct RunSummary<'a> {
    schema_version: u32,
    kind: &'static str,
    run_id: &'a str,
    worker_id: &'a str,
    worker_hostname: &'a str,
    cargo_exit_code: i32,
    schema_exit_code: i32,
    acceptance_pass: bool,
    artifact_files: u64,
    artifact_bytes: u64,
    artifact_events: u64,
    trace_files: u64,
    trace_events: u64,
    manifest: String,
}

#[derive(Debug, Serialize)]
struct VerifySummary<'a> {
    schema_version: u32,
    run_id: &'a str,
    worker_id: &'a str,
    worker_hostname: &'a str,
    source_sha: &'a str,
    source_diff_sha256: &'a str,
    cargo_exit_code: i32,
    acceptance_pass: bool,
    files: u64,
    bytes: u64,
    events: u64,
    trace_files: u64,
    trace_events: u64,
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn emit_json_receipt(value: &impl Serialize) -> Result<()> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    serde_json::to_writer(&mut stdout, value).context("serialize runner receipt")?;
    stdout
        .write_all(b"\n")
        .context("terminate runner receipt")?;
    stdout.flush().context("flush runner receipt")
}

fn e2e_timestamp() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn write_run_start(path: &Path, args: &RunArgs) -> Result<()> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .with_context(|| format!("create run envelope {}", path.display()))?;
    let event = serde_json::json!({
        "ts": e2e_timestamp(),
        "event": "run_start",
        "run_id": args.run_id,
        "runner": "rust",
        "env": {
            "git_sha": args.source_sha,
            "worker_id": args.worker_id,
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "cass_version": env!("CARGO_PKG_VERSION"),
        },
        "config": {
            "explicit_test_targets": args.cargo_args.iter()
                .filter(|argument| argument.as_str() == "--test")
                .count(),
            "parallel": false,
            "fail_fast": false,
        },
    });
    serde_json::to_writer(&mut file, &event).context("serialize run_start")?;
    file.write_all(b"\n").context("terminate run_start")?;
    file.sync_all().context("sync run_start")
}

fn write_run_end(
    path: &Path,
    args: &RunArgs,
    cargo_result: &CommandResult,
    duration_ms: u64,
) -> Result<()> {
    let explicit_targets = args
        .cargo_args
        .iter()
        .filter(|argument| argument.as_str() == "--test")
        .count();
    let success = cargo_result.exit_code == 0 && !cargo_result.timed_out;
    let mut file = OpenOptions::new()
        .append(true)
        .open(path)
        .with_context(|| format!("append run envelope {}", path.display()))?;
    let event = serde_json::json!({
        "ts": e2e_timestamp(),
        "event": "run_end",
        "run_id": args.run_id,
        "runner": "rust",
        "summary": {
            "total": explicit_targets,
            "passed": if success { explicit_targets } else { 0 },
            "failed": if success { 0 } else { 1 },
            "skipped": 0,
            "flaky": 0,
            "duration_ms": duration_ms,
            "timed_out": cargo_result.timed_out,
        },
        "exit_code": cargo_result.exit_code,
    });
    serde_json::to_writer(&mut file, &event).context("serialize run_end")?;
    file.write_all(b"\n").context("terminate run_end")?;
    file.sync_all().context("sync run_end")
}

fn worker_hostname() -> Result<String> {
    let output = Command::new("hostname")
        .stdin(Stdio::null())
        .output()
        .context("run hostname")?;
    ensure!(output.status.success(), "hostname exited {}", output.status);
    let hostname = String::from_utf8(output.stdout).context("hostname was not UTF-8")?;
    let hostname = hostname.trim().to_string();
    ensure!(!hostname.is_empty(), "hostname returned an empty value");
    Ok(hostname)
}

fn validate_run_args(args: &RunArgs) -> Result<()> {
    validate_e2e_run_id(&args.run_id)?;
    validate_rch_worker_id(&args.worker_id)?;
    validate_e2e_source_identity(&args.source_sha, &args.source_diff_sha256)?;
    ensure!(
        (1..=14_400).contains(&args.timeout_seconds),
        "timeout_seconds must be between 1 and 14400"
    );
    ensure!(
        (1..=MAX_STREAM_CAP_BYTES).contains(&args.max_stream_bytes),
        "max_stream_bytes must be between 1 and {MAX_STREAM_CAP_BYTES}"
    );
    ensure!(
        args.cargo_args
            .iter()
            .any(|argument| argument == "--locked"),
        "the wrapped cargo test command must use --locked"
    );
    ensure!(
        args.cargo_args.iter().any(|argument| argument == "--test"),
        "the wrapped cargo test command must name explicit --test targets"
    );
    ensure!(
        !args
            .cargo_args
            .iter()
            .any(|argument| argument == "--no-run"),
        "--no-run cannot produce E2E acceptance evidence"
    );
    Ok(())
}

fn git_output(project_root: &Path, arguments: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(project_root)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("run git {}", arguments.join(" ")))?;
    ensure!(
        output.status.success(),
        "git {} exited {}: {}",
        arguments.join(" "),
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output.stdout)
}

fn verify_worker_source(project_root: &Path, args: &RunArgs) -> Result<()> {
    let head = git_output(project_root, &["rev-parse", "HEAD"])?;
    let head = String::from_utf8(head).context("worker git SHA was not UTF-8")?;
    ensure!(
        head.trim() == args.source_sha,
        "worker source SHA mismatch: expected {}, found {}",
        args.source_sha,
        head.trim()
    );

    let diff = git_output(
        project_root,
        &["diff", "--binary", "HEAD", "--", ".", ":(exclude).beads/**"],
    )?;
    let diff_sha256 = hex::encode(Sha256::digest(&diff));
    ensure!(
        diff_sha256.cmp(&args.source_diff_sha256).is_eq(),
        "worker source diff SHA-256 mismatch: expected {}, found {}",
        args.source_diff_sha256,
        diff_sha256
    );

    let untracked = git_output(
        project_root,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )?;
    for raw_path in untracked
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let path = std::str::from_utf8(raw_path).context("worker untracked path was not UTF-8")?;
        ensure!(
            path.starts_with(".beads/") || path.starts_with("test-results/"),
            "worker contains unreceipted source path `{path}`"
        );
    }
    Ok(())
}

fn create_capture_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .with_context(|| format!("create bounded capture {}", path.display()))
}

fn drain_bounded(
    mut reader: impl Read,
    mut destination: File,
    cap: u64,
) -> io::Result<CaptureStats> {
    let mut buffer = [0_u8; 32 * 1024];
    let mut observed = 0_u64;
    let mut persisted = 0_u64;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        observed = observed.saturating_add(read as u64);
        if persisted < cap {
            let remaining = usize::try_from(cap.saturating_sub(persisted)).unwrap_or(usize::MAX);
            let to_write = read.min(remaining);
            let chunk = buffer
                .get(..to_write)
                .ok_or_else(|| io::Error::other("capture length exceeded read buffer"))?;
            destination.write_all(chunk)?;
            persisted = persisted.saturating_add(to_write as u64);
        }
    }
    destination.sync_all()?;
    Ok(CaptureStats {
        observed_bytes: observed,
        persisted_bytes: persisted,
        truncated: observed > persisted,
    })
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(not(unix))]
fn configure_process_group(_command: &mut Command) {}

#[cfg(unix)]
fn kill_process_group(pid: u32) {
    let group = format!("-{pid}");
    let _ = Command::new("/bin/kill")
        .args(["-KILL", &group])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(not(unix))]
fn kill_process_group(_pid: u32) {}

fn status_exit_code(status: ExitStatus, timed_out: bool) -> i32 {
    if timed_out {
        return 124;
    }
    status.code().unwrap_or(1)
}

fn run_bounded_command(
    command: &mut Command,
    stdout_path: &Path,
    stderr_path: &Path,
    timeout: Duration,
    cap: u64,
) -> Result<CommandResult> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_process_group(command);
    let start = Instant::now();
    let mut child = command.spawn().context("spawn bounded command")?;
    let pid = child.id();
    let stdout = child.stdout.take().context("capture child stdout")?;
    let stderr = child.stderr.take().context("capture child stderr")?;
    let stdout_file = create_capture_file(stdout_path)?;
    let stderr_file = create_capture_file(stderr_path)?;
    let stdout_thread = thread::spawn(move || drain_bounded(stdout, stdout_file, cap));
    let stderr_thread = thread::spawn(move || drain_bounded(stderr, stderr_file, cap));

    let deadline = start + timeout;
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait().context("poll bounded command")? {
            break status;
        }
        if Instant::now() >= deadline {
            timed_out = true;
            kill_process_group(pid);
            let _ = child.kill();
            break child.wait().context("wait after bounded timeout")?;
        }
        thread::sleep(Duration::from_millis(25));
    };
    let stdout = stdout_thread
        .join()
        .map_err(|_| anyhow::anyhow!("stdout capture thread panicked"))?
        .context("write bounded stdout")?;
    let stderr = stderr_thread
        .join()
        .map_err(|_| anyhow::anyhow!("stderr capture thread panicked"))?
        .context("write bounded stderr")?;
    Ok(CommandResult {
        exit_code: status_exit_code(status, timed_out),
        timed_out,
        elapsed_ms: start.elapsed().as_millis() as u64,
        stdout,
        stderr,
    })
}

fn validator_inputs(root: &Path) -> Result<Vec<PathBuf>> {
    let mut inputs = Vec::new();
    for entry in WalkDir::new(root).min_depth(1).follow_links(false) {
        let entry = entry.context("walk run directory for JSONL validator")?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(OsStr::to_str) == Some("jsonl")
            || path.file_name().and_then(OsStr::to_str) == Some("cass.log")
        {
            let relative = path
                .strip_prefix(root)
                .context("validator input escaped the run directory")?;
            inputs.push(root.join(relative));
        }
    }
    inputs.sort();
    Ok(inputs)
}

fn run_schema_validator(
    project_root: &Path,
    run_root: &Path,
    timeout: Duration,
    cap: u64,
) -> Result<CommandResult> {
    let inputs = validator_inputs(run_root)?;
    let stdout_path = run_root.join("schema.stdout");
    let stderr_path = run_root.join("schema.stderr");
    if inputs.is_empty() {
        let diagnostic = b"no run-scoped JSONL or cass.log artifacts were produced\n";
        fs::write(&stderr_path, diagnostic)
            .with_context(|| format!("write {}", stderr_path.display()))?;
        fs::write(&stdout_path, b"").with_context(|| format!("write {}", stdout_path.display()))?;
        return Ok(CommandResult {
            exit_code: 2,
            timed_out: false,
            elapsed_ms: 0,
            stdout: CaptureStats {
                observed_bytes: 0,
                persisted_bytes: 0,
                truncated: false,
            },
            stderr: CaptureStats {
                observed_bytes: diagnostic.len() as u64,
                persisted_bytes: diagnostic.len() as u64,
                truncated: false,
            },
        });
    }
    let mut command = Command::new(project_root.join("scripts/validate-e2e-jsonl.sh"));
    command.args(inputs);
    run_bounded_command(&mut command, &stdout_path, &stderr_path, timeout, cap)
}

fn capture_detail(label: &str, result: &CommandResult) -> String {
    format!(
        "{label}: exit={} timeout={} elapsed_ms={} stdout={}/{}{} stderr={}/{}{}",
        result.exit_code,
        result.timed_out,
        result.elapsed_ms,
        result.stdout.persisted_bytes,
        result.stdout.observed_bytes,
        if result.stdout.truncated {
            " truncated"
        } else {
            ""
        },
        result.stderr.persisted_bytes,
        result.stderr.observed_bytes,
        if result.stderr.truncated {
            " truncated"
        } else {
            ""
        }
    )
}

fn run_bundle(args: RunArgs) -> Result<i32> {
    validate_run_args(&args)?;
    let project_root = std::env::current_dir().context("read current project directory")?;
    verify_worker_source(&project_root, &args)?;
    let hostname = worker_hostname()?;
    let runs_dir = project_root.join("test-results/e2e/runs");
    fs::create_dir_all(&runs_dir)
        .with_context(|| format!("create run parent {}", runs_dir.display()))?;
    let run_root = runs_dir.join(&args.run_id);
    fs::create_dir(&run_root).with_context(|| {
        format!(
            "create absent run directory {}; run ids are never reused",
            run_root.display()
        )
    })?;

    let started_at_unix_ms = unix_ms();
    let run_envelope_path = run_root.join("run.jsonl");
    write_run_start(&run_envelope_path, &args)?;
    emit_json_receipt(&RunStartedSummary {
        schema_version: E2E_RUN_BUNDLE_SCHEMA_VERSION,
        kind: "e2e-run-started",
        run_id: &args.run_id,
        worker_id: &args.worker_id,
        worker_hostname: &hostname,
        source_sha: &args.source_sha,
        source_diff_sha256: &args.source_diff_sha256,
        run_root: run_root.display().to_string(),
    })?;
    let timeout = Duration::from_secs(args.timeout_seconds);
    let mut cargo = Command::new("cargo");
    cargo.arg("test").args(&args.cargo_args);
    cargo.current_dir(&project_root);
    cargo.env("CASS_E2E_RUN_ID", &args.run_id);
    cargo.env("E2E_LOG", "1");
    let cargo_result = run_bounded_command(
        &mut cargo,
        &run_root.join("cargo.stdout"),
        &run_root.join("cargo.stderr"),
        timeout,
        args.max_stream_bytes,
    )?;
    write_run_end(
        &run_envelope_path,
        &args,
        &cargo_result,
        unix_ms().saturating_sub(started_at_unix_ms),
    )?;

    let schema_result =
        run_schema_validator(&project_root, &run_root, timeout, args.max_stream_bytes)?;
    let ended_at_unix_ms = unix_ms();
    let command = std::iter::once("cargo".to_string())
        .chain(std::iter::once("test".to_string()))
        .chain(args.cargo_args.iter().cloned())
        .collect::<Vec<_>>();
    let capture_complete = !cargo_result.stdout.truncated
        && !cargo_result.stderr.truncated
        && !schema_result.stdout.truncated
        && !schema_result.stderr.truncated;
    let metadata = E2eRunBundleMetadata {
        run_id: args.run_id.clone(),
        source_sha: args.source_sha,
        source_diff_sha256: args.source_diff_sha256,
        worker_id: args.worker_id.clone(),
        worker_hostname: hostname.clone(),
        command,
        cargo_exit_code: cargo_result.exit_code,
        started_at_unix_ms,
        ended_at_unix_ms,
        gates: vec![
            E2eRunGateResult {
                name: "cargo-tests".to_string(),
                passed: cargo_result.exit_code == 0 && !cargo_result.timed_out,
                detail: capture_detail("cargo", &cargo_result),
            },
            E2eRunGateResult {
                name: "jsonl-schema".to_string(),
                passed: schema_result.exit_code == 0 && !schema_result.timed_out,
                detail: capture_detail("schema", &schema_result),
            },
            E2eRunGateResult {
                name: "bounded-capture-complete".to_string(),
                passed: capture_complete,
                detail: format!(
                    "cap={} cargo_stdout_truncated={} cargo_stderr_truncated={} schema_stdout_truncated={} schema_stderr_truncated={}",
                    args.max_stream_bytes,
                    cargo_result.stdout.truncated,
                    cargo_result.stderr.truncated,
                    schema_result.stdout.truncated,
                    schema_result.stderr.truncated
                ),
            },
        ],
    };
    let manifest = finalize_e2e_run_bundle(&run_root, metadata)?;
    let summary = RunSummary {
        schema_version: E2E_RUN_BUNDLE_SCHEMA_VERSION,
        kind: "e2e-run-finalized",
        run_id: &manifest.run_id,
        worker_id: &manifest.worker_id,
        worker_hostname: &manifest.worker_hostname,
        cargo_exit_code: cargo_result.exit_code,
        schema_exit_code: schema_result.exit_code,
        acceptance_pass: manifest.is_acceptance_pass(),
        artifact_files: manifest.aggregates.files,
        artifact_bytes: manifest.aggregates.bytes,
        artifact_events: manifest.aggregates.events,
        trace_files: manifest.aggregates.trace_files,
        trace_events: manifest.aggregates.trace_events,
        manifest: run_root.join("manifest.json").display().to_string(),
    };
    emit_json_receipt(&summary)?;

    if manifest.is_acceptance_pass() {
        Ok(0)
    } else if cargo_result.exit_code != 0 {
        Ok(cargo_result.exit_code)
    } else if schema_result.exit_code != 0 {
        Ok(schema_result.exit_code)
    } else {
        Ok(1)
    }
}

fn verify_bundle(args: VerifyArgs) -> Result<i32> {
    if let Some(run_id) = &args.run_id {
        validate_e2e_run_id(run_id)?;
    }
    if let Some(worker_id) = &args.worker_id {
        validate_rch_worker_id(worker_id)?;
    }
    let manifest = verify_e2e_run_bundle(
        &args.bundle,
        &E2eRunBundleExpectation {
            run_id: args.run_id,
            source_sha: args.source_sha,
            source_diff_sha256: args.source_diff_sha256,
            worker_id: args.worker_id,
            worker_hostname: args.worker_hostname,
        },
    )?;
    let summary = VerifySummary {
        schema_version: manifest.schema_version,
        run_id: &manifest.run_id,
        worker_id: &manifest.worker_id,
        worker_hostname: &manifest.worker_hostname,
        source_sha: &manifest.source_sha,
        source_diff_sha256: &manifest.source_diff_sha256,
        cargo_exit_code: manifest.cargo_exit_code,
        acceptance_pass: manifest.is_acceptance_pass(),
        files: manifest.aggregates.files,
        bytes: manifest.aggregates.bytes,
        events: manifest.aggregates.events,
        trace_files: manifest.aggregates.trace_files,
        trace_events: manifest.aggregates.trace_events,
    };
    println!("{}", serde_json::to_string(&summary)?);
    if !args.integrity_only && !manifest.is_acceptance_pass() {
        bail!(
            "bundle integrity is valid, but one or more acceptance gates failed (cargo exit {})",
            manifest.cargo_exit_code
        );
    }
    Ok(0)
}

fn main() -> Result<()> {
    let exit_code = match Cli::parse().command {
        BundleCommand::Run(args) => run_bundle(args)?,
        BundleCommand::Verify(args) => verify_bundle(args)?,
    };
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}
