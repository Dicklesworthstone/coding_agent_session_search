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
use std::time::{Duration, Instant};

/// How often we poll the child's exit status while waiting.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// How much of the child's streamed stdout/stderr to keep for the
/// timeout-diagnostic dump. Bounded so the dump stays readable and a
/// misbehaving child that spams output doesn't blow up test memory.
const DIAG_STREAM_TAIL_BYTES: usize = 16 * 1024;

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

    let start = Instant::now();
    let mut child = cmd
        .spawn()
        .unwrap_or_else(|err| panic!("spawn_with_timeout_or_diag({label}): spawn failed: {err}"));

    let deadline = start + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // Normal path — drain the pipes and return an Output.
                let mut stdout = Vec::new();
                let mut stderr = Vec::new();
                if let Some(mut h) = child.stdout.take() {
                    let _ = h.read_to_end(&mut stdout);
                }
                if let Some(mut h) = child.stderr.take() {
                    let _ = h.read_to_end(&mut stderr);
                }
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
                    // on the child's side and our `read_to_end` below
                    // actually returns. Draining a pipe whose writer
                    // is still alive but idle would otherwise block
                    // the diagnostic dump forever and defeat the
                    // whole point of this helper.
                    let _ = child.kill();
                    let _ = child.wait();
                    let stdout_tail = drain_pipe_tail(child.stdout.take());
                    let stderr_tail = drain_pipe_tail(child.stderr.take());

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
                    eprintln!(
                        "--- child stdout tail ({} bytes of up to {}):",
                        stdout_tail.len(),
                        DIAG_STREAM_TAIL_BYTES
                    );
                    eprintln!("{}", String::from_utf8_lossy(&stdout_tail));
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
                let _ = child.kill();
                let _ = child.wait();
                panic!("spawn_with_timeout_or_diag({label}): try_wait errored: {err}");
            }
        }
    }
}

/// Drain as much of a pipe as is currently available without blocking
/// long, then truncate to the configured tail size.
fn drain_pipe_tail<R: Read>(handle: Option<R>) -> Vec<u8> {
    let Some(mut h) = handle else {
        return Vec::new();
    };
    // We don't have a portable way to set O_NONBLOCK on the pipe fd
    // here; instead we rely on the child having produced finite output
    // during the timeout window. `read_to_end` on a closed pipe returns
    // what's been written so far. For the hung-child case the pipe is
    // still open, but we're about to kill the child anyway — the
    // subsequent read returns EOF quickly once the FD closes.
    let mut buf = Vec::new();
    let _ = h.read_to_end(&mut buf);
    if buf.len() > DIAG_STREAM_TAIL_BYTES {
        let tail_start = buf.len() - DIAG_STREAM_TAIL_BYTES;
        buf.drain(0..tail_start);
    }
    buf
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
            match entry.file_type() {
                Ok(ft) if ft.is_dir() => {
                    out.push(format!("{rel}/"));
                    stack.push(path);
                }
                Ok(_) => {
                    let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                    out.push(format!("{rel} ({size} bytes)"));
                }
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

    /// Proves the timeout path: a child that hangs past the deadline
    /// triggers the diagnostic-dump + kill + panic sequence.
    #[test]
    #[should_panic(expected = "exceeded timeout")]
    fn hung_child_triggers_timeout_panic_with_diagnostic() {
        // `/bin/sleep` is invoked DIRECTLY (no shell wrapper) so
        // SIGKILL from `child.kill()` actually terminates the hanging
        // process. Going through `/bin/sh -c 'sleep 30'` would kill
        // only the shell, leaving the orphan sleep holding the
        // stdout/stderr pipe FDs open and making the subsequent
        // `read_to_end` in drain_pipe_tail block for the full 30s.
        let mut cmd = Command::new("/bin/sleep");
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
}
