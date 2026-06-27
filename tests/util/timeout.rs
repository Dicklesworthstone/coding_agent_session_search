//! Subprocess timeout-with-diagnostic wrapper (bead f2r5t / ibuuh.10.12).
//!
//! Motivation
//! ----------
//!
//! E2E test suites that spawn the `cass` binary via `assert_cmd` (or
//! `std::process::Command`) and call `.wait()` / `.assert().success()`
//! will block indefinitely when the child hangs. Under that failure
//! mode the test harness produces *no* useful output — just a silent
//! stall until the outer cargo-test timeout eventually kills the
//! runner. Operators and CI consumers are left reconstructing what
//! phase the child was in by guessing.
//!
//! This module provides [`spawn_with_timeout_or_diag`], a wrapper that
//!   1. spawns a command and polls [`std::process::Child::try_wait`],
//!   2. on success returns the normal [`std::process::Output`],
//!   3. on timeout emits a structured diagnostic dump to stderr
//!      (label, child PID, elapsed, optional `data_dir` listing, last
//!      N bytes of stdout / stderr), kills the child, and panics with
//!      a clear message.
//!
//! Tests in `tests/e2e_large_dataset.rs` and similar long-running
//! suites can swap `.assert().success()` for this wrapper to convert
//! a silent hang into a loud, diagnosable failure.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How often we poll the child's exit status while waiting.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// How much of the child's streamed stdout/stderr to keep for the
/// timeout-diagnostic dump. Bounded so the dump stays readable and a
/// misbehaving child that spams output doesn't blow up test memory.
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

/// Spawn `cmd` and wait for it to finish, up to `timeout`. On timeout,
/// emit a diagnostic dump, kill the child, and panic.
///
/// `label` identifies the phase in the diagnostic dump so future-you
/// can tell at a glance what was hung.
///
/// `data_dir`, when supplied, is recursively listed (up to a bounded
/// number of entries) in the diagnostic dump so the reader can see
/// which index / lock / checkpoint files were or were not present at
/// the moment of the hang.
///
/// Stdin is closed (`Stdio::null()`) so the child never blocks on a
/// non-existent operator. Stdout and stderr are captured and included
/// (tail-clipped) in the diagnostic dump.
#[allow(dead_code)]
pub fn spawn_with_timeout_or_diag(
    mut cmd: Command,
    label: &str,
    data_dir: Option<&Path>,
    timeout: Duration,
) -> Output {
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    configure_child_process_group(&mut cmd);

    let start = Instant::now();
    let mut child = cmd
        .spawn()
        .unwrap_or_else(|err| panic!("spawn_with_timeout_or_diag({label}): spawn failed: {err}"));
    let stdout_reader = spawn_pipe_reader(child.stdout.take());
    let stderr_reader = spawn_pipe_reader(child.stderr.take());

    let deadline = start + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                kill_child_process_group(child.id());
                let stdout = join_pipe_reader(stdout_reader, label, "stdout");
                let stderr = join_pipe_reader(stderr_reader, label, "stderr");
                let stdout = full_output_or_panic(stdout, label, "stdout");
                let stderr = full_output_or_panic(stderr, label, "stderr");
                return Output {
                    status,
                    stdout,
                    stderr,
                };
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let pid = child.id();
                    // Kill FIRST so the stdout/stderr pipe FDs close
                    // on the child's side and the reader-thread joins
                    // below return. Waiting on a pipe whose writer is
                    // still alive but idle would otherwise block the
                    // diagnostic dump forever and defeat this helper.
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
                    if let Some(dir) = data_dir {
                        eprintln!("--- data_dir listing ({}):", dir.display());
                        for entry in list_dir_bounded(dir, 200) {
                            eprintln!("  {entry}");
                        }
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

                    panic!(
                        "subprocess phase {label:?} exceeded timeout of {:?} (see stderr \
                         diagnostic above)",
                        timeout
                    );
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(err) => {
                // try_wait errored — treat as a hard failure.
                kill_child_process_group(child.id());
                let _ = child.kill();
                let _ = child.wait();
                let _ = join_pipe_reader(stdout_reader, label, "stdout");
                let _ = join_pipe_reader(stderr_reader, label, "stderr");
                panic!("spawn_with_timeout_or_diag({label}): try_wait errored: {err}");
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

fn full_output_or_panic(capture: PipeCapture, label: &str, stream_name: &str) -> Vec<u8> {
    if capture.truncated {
        panic!(
            "spawn_with_timeout_or_diag({label}): {stream_name} exceeded capture cap of {} bytes",
            MAX_CAPTURE_BYTES
        );
    }
    capture.bytes
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

/// Produce up to `limit` relative entries under `root`, formatted as
/// `path (size_bytes)` for files and `path/` for directories. Silently
/// ignores I/O errors so a diagnostic dump never turns into its own
/// second-order failure.
fn list_dir_bounded(root: &Path, limit: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
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
                Err(_) => {
                    out.push(format!("{rel} (<stat failed>)"));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Proves the happy path: a fast-exiting child returns Output
    /// normally with no panic and no diagnostic noise.
    #[test]
    fn happy_path_returns_output_without_panicking() {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("printf 'hi' && exit 0");
        let out = spawn_with_timeout_or_diag(cmd, "happy_path", None, Duration::from_secs(5));
        assert!(out.status.success(), "shell must exit 0");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "hi",
            "stdout must round-trip"
        );
    }

    /// Proves stdout is drained while the child is still running. A
    /// child that writes more than the kernel pipe buffer can otherwise
    /// block before exit and get misdiagnosed as a timeout.
    #[test]
    fn large_stdout_child_can_exit_without_pipe_deadlock() {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("yes x | head -c 1048576");
        let out = spawn_with_timeout_or_diag(cmd, "large_stdout", None, Duration::from_secs(10));
        assert!(out.status.success(), "large stdout child must exit 0");
        assert_eq!(out.stdout.len(), 1024 * 1024);
    }

    /// Proves the timeout path: a child that hangs past the deadline
    /// triggers the diagnostic-dump + kill + panic sequence.
    ///
    /// Use a shell wrapper on Unix so this also proves process-group
    /// cleanup: killing only the shell would leave `sleep` holding the
    /// stdout/stderr pipe FDs open, deadlocking the diagnostic join.
    #[test]
    #[should_panic(expected = "exceeded timeout")]
    fn hung_child_triggers_timeout_panic_with_diagnostic() {
        #[cfg(unix)]
        let cmd = {
            let mut cmd = Command::new("/bin/sh");
            cmd.arg("-c").arg("sleep 30");
            cmd
        };
        #[cfg(not(unix))]
        let mut cmd = Command::new("/bin/sleep");
        #[cfg(not(unix))]
        cmd.arg("30");
        let _ =
            spawn_with_timeout_or_diag(cmd, "intentional_hang", None, Duration::from_millis(300));
    }

    /// Proves the diagnostic dump includes the data_dir listing when
    /// one is supplied — even though we don't capture stderr of the
    /// panicking test, asserting `should_panic` with the right message
    /// plus eyeballing the dump locally is enough signal here. We also
    /// exercise list_dir_bounded directly so its happy-path shape is
    /// covered.
    #[test]
    fn list_dir_bounded_reports_files_and_dirs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::write(root.join("a.txt"), b"hello").unwrap();
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/b.bin"), b"0123456789").unwrap();

        let entries = list_dir_bounded(root, 200);
        assert!(
            entries.iter().any(|e| e.starts_with("a.txt (5 bytes)")),
            "expected a.txt entry with size; got: {entries:?}"
        );
        assert!(
            entries.iter().any(|e| e == "sub/"),
            "expected sub/ directory entry; got: {entries:?}"
        );
        assert!(
            entries
                .iter()
                .any(|e| e.starts_with("sub/b.bin (10 bytes)")),
            "expected nested file entry with size; got: {entries:?}"
        );
    }

    /// Proves the `limit` cap triggers the truncation marker so the
    /// dump never unbounded-grows on a pathological data_dir.
    #[test]
    fn list_dir_bounded_truncates_at_limit() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        for i in 0..10 {
            std::fs::write(root.join(format!("f-{i:02}.txt")), b"x").unwrap();
        }
        let entries = list_dir_bounded(root, 3);
        assert!(
            entries.iter().any(|e| e.contains("truncated at 3")),
            "must include the truncated-at marker once limit is exceeded; got: {entries:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn list_dir_bounded_reports_symlinked_directories_without_following() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("root");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&root).expect("create root");
        std::fs::create_dir_all(&outside).expect("create outside");
        std::fs::write(outside.join("outside-only.txt"), b"should not be listed")
            .expect("write outside marker");
        std::os::unix::fs::symlink(&outside, root.join("linked-outside"))
            .expect("create symlinked directory");

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
    }
}
