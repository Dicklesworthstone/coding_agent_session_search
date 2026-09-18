//! Request-owned Unix subprocess capture for diagnostics and remote commands.
//!
//! Both pipes are nonblocking and drained fairly by the caller. There are no
//! reader threads to abandon when a deadline, byte limit, or read fails. Keep
//! the direct child unreaped until pipe collection succeeds, so its PID cannot
//! be recycled before failure cleanup signals the process group.

use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::process::{Child, Output};
use std::time::{Duration, Instant};

const READ_BYTES: usize = 8 * 1024;
const READS_PER_TURN: usize = 4;
const IDLE_POLL: Duration = Duration::from_millis(5);

struct ChildOwner {
    child: Child,
    armed: bool,
}

impl Drop for ChildOwner {
    fn drop(&mut self) {
        if self.armed {
            super::kill_child_process_group(self.child.id());
            let _ = self.child.kill();
            // Reap the direct child even when it exited while descendants
            // retained a pipe. OS process teardown itself is not preemptible.
            let _ = self.child.wait();
        }
    }
}

struct Capture<P> {
    pipe: Option<P>,
    bytes: Vec<u8>,
}

impl<P: Read + AsRawFd> Capture<P> {
    fn new(pipe: Option<P>) -> io::Result<Self> {
        if let Some(pipe) = &pipe {
            set_nonblocking(pipe)?;
        }
        Ok(Self {
            pipe,
            bytes: Vec::new(),
        })
    }

    /// A bounded turn prevents a continuously writable stdout from starving
    /// stderr, process completion, or the deadline check.
    fn drain(
        &mut self,
        started: Instant,
        timeout: Duration,
        limit: Option<usize>,
    ) -> io::Result<bool> {
        let mut made_progress = false;
        let mut buffer = [0u8; READ_BYTES];
        for _ in 0..READS_PER_TURN {
            if started.elapsed() >= timeout {
                break;
            }
            let read_len = limit.map_or(buffer.len(), |limit| {
                limit
                    .saturating_sub(self.bytes.len())
                    .saturating_add(1)
                    .min(buffer.len())
            });
            let result = match self.pipe.as_mut() {
                Some(pipe) => pipe.read(&mut buffer[..read_len]),
                None => break,
            };
            match result {
                Ok(0) => {
                    self.pipe = None;
                    return Ok(true);
                }
                Ok(count) => {
                    let total = self.bytes.len().checked_add(count).ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "child output size overflow")
                    })?;
                    if limit.is_some_and(|limit| total > limit) {
                        // Do not drain-and-discard until the execution timeout:
                        // the owner kills/reaps immediately on this error.
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "child output exceeded the configured byte limit",
                        ));
                    }
                    if total > self.bytes.capacity() {
                        let capacity = self
                            .bytes
                            .capacity()
                            .saturating_mul(2)
                            .max(total)
                            .min(limit.unwrap_or(usize::MAX));
                        self.bytes
                            .try_reserve_exact(capacity - self.bytes.len())
                            .map_err(|_| {
                                io::Error::new(
                                    io::ErrorKind::OutOfMemory,
                                    "cannot allocate child output buffer",
                                )
                            })?;
                    }
                    self.bytes.extend_from_slice(&buffer[..count]);
                    made_progress = true;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error),
            }
        }
        Ok(made_progress)
    }
}

/// std does not expose nonblocking mode for ChildStdout/ChildStderr. This FFI
/// only changes flags on a live borrowed descriptor; ownership stays in std.
#[allow(unsafe_code)]
fn set_nonblocking(pipe: &impl AsRawFd) -> io::Result<()> {
    let fd = pipe.as_raw_fd();
    // SAFETY: fd remains owned by `pipe` throughout both calls. F_GETFL takes
    // no variadic argument; F_SETFL takes the returned integer flags. Neither
    // operation transfers ownership or retains a pointer.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(super) fn wait(
    child: Child,
    timeout: Duration,
    limit: Option<usize>,
) -> io::Result<Option<Output>> {
    let started = Instant::now();
    let mut owner = ChildOwner { child, armed: true };
    // Like wait_with_output, finish any stdin still owned by this child
    // handle. Otherwise a command waiting for EOF can deadlock its own wait.
    drop(owner.child.stdin.take());
    if timeout.is_zero() {
        return Ok(None);
    }
    let mut stdout = Capture::new(owner.child.stdout.take())?;
    let mut stderr = Capture::new(owner.child.stderr.take())?;
    loop {
        if started.elapsed() >= timeout {
            return Ok(None);
        }
        let out_progress = stdout.drain(started, timeout, limit)?;
        let err_progress = stderr.drain(started, timeout, limit)?;
        if started.elapsed() >= timeout {
            return Ok(None);
        }
        if stdout.pipe.is_none()
            && stderr.pipe.is_none()
            && let Some(status) = owner.child.try_wait()?
        {
            owner.armed = false;
            return Ok(Some(Output {
                status,
                stdout: stdout.bytes,
                stderr: stderr.bytes,
            }));
        }
        if !out_progress && !err_progress {
            // Subtraction avoids Instant overflow for very long budgets and
            // never renews the budget as more bytes arrive.
            std::thread::sleep(timeout.saturating_sub(started.elapsed()).min(IDLE_POLL));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::process::{Command, Stdio};

    fn child(script: &str) -> Child {
        let mut command = Command::new("sh");
        command
            .args(["-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        crate::sources::configure_child_process_group(&mut command);
        command.spawn().expect("spawn capture child")
    }

    #[test]
    fn preserves_binary_streams_and_nonzero_exit() {
        let output = wait(
            child("printf 'a\\000b'; printf 'c\\000d' >&2; exit 23"),
            Duration::from_secs(5),
            Some(3),
        )
        .unwrap()
        .unwrap();
        assert_eq!(output.status.code(), Some(23));
        assert_eq!(output.stdout, b"a\0b");
        assert_eq!(output.stderr, b"c\0d");
    }

    #[test]
    fn zero_limit_accepts_empty_streams_but_rejects_the_first_byte() {
        assert!(wait(child("exit 0"), Duration::from_secs(5), Some(0))
            .unwrap()
            .unwrap()
            .stdout
            .is_empty());
        for script in ["printf x", "printf x >&2"] {
            assert_eq!(
                wait(child(script), Duration::from_secs(5), Some(0))
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidData
            );
        }
    }

    #[test]
    fn drains_both_streams_beyond_pipe_capacity_without_deadlock() {
        let output = wait(
            child(concat!(
                "(i=0; while [ $i -lt 4096 ]; do ",
                "printf 0123456789abcdef; i=$((i+1)); done) & ",
                "i=0; while [ $i -lt 4096 ]; do ",
                "printf fedcba9876543210 >&2; i=$((i+1)); done; wait"
            )),
            Duration::from_secs(10),
            Some(65536),
        )
        .unwrap()
        .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"0123456789abcdef".repeat(4096));
        assert_eq!(output.stderr, b"fedcba9876543210".repeat(4096));
    }

    #[test]
    fn overflow_aborts_without_waiting_for_a_stalled_sibling_stream() {
        for script in [
            "printf 12345; sleep 30",
            "sleep 30 & printf 12345 >&2; wait",
        ] {
            let started = Instant::now();
            let error = wait(child(script), Duration::from_secs(30), Some(4)).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(started.elapsed() < Duration::from_secs(5));
        }
    }

    #[test]
    fn exited_parent_with_inherited_pipes_still_has_a_deadline() {
        let started = Instant::now();
        let output = wait(
            child("sleep 30 & printf ready; exit 0"),
            Duration::from_millis(100),
            Some(1024),
        )
        .unwrap();
        assert!(output.is_none());
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn closed_pipes_do_not_mistake_a_running_child_for_completion() {
        let output = wait(
            child("exec 1>&- 2>&-; sleep 30"),
            Duration::from_millis(100),
            Some(1024),
        )
        .unwrap();
        assert!(output.is_none());
    }

    #[test]
    fn zero_deadline_cancels_instead_of_granting_an_extra_second() {
        let started = Instant::now();
        assert!(wait(child("sleep 30"), Duration::ZERO, Some(1024))
            .unwrap()
            .is_none());
        assert!(started.elapsed() < Duration::from_millis(900));
    }

    #[test]
    fn very_long_deadline_does_not_overflow_instant() {
        let output = wait(child("printf done"), Duration::MAX, Some(4))
            .unwrap()
            .unwrap();
        assert_eq!(output.stdout, b"done");
    }

    #[test]
    fn closes_owned_stdin_before_waiting_for_eof() {
        let mut command = Command::new("sh");
        command
            .args(["-c", "cat"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        crate::sources::configure_child_process_group(&mut command);
        let mut child = command.spawn().unwrap();
        child.stdin.as_mut().unwrap().write_all(b"input\0bytes").unwrap();
        let output = wait(child, Duration::from_secs(5), Some(1024))
            .unwrap()
            .unwrap();
        assert_eq!(output.stdout, b"input\0bytes");
    }
}
