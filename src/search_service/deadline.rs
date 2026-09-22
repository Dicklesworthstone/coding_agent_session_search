//! Process-level deadlines for the read-only service, including blocked I/O.
//!
//! A timed-out native call must not survive in a detached thread while the
//! caller starts another query. The watchdog therefore terminates this entire
//! worker, without taking an output lock or pretending to return a tool error.
//! There is at most one watchdog per serial request; completion joins it before
//! admitting another request. Idle time before the first frame byte is excluded.

use std::io::{self, BufRead};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub(super) const DEFAULT_TIMEOUT_MS: u64 = 30_000;
pub(super) const MAX_TIMEOUT_MS: u64 = 300_000;
pub(super) const TIMEOUT_EXIT_CODE: i32 = 124;
pub(super) const WATCHDOG_FAILURE_EXIT_CODE: i32 = 125;

pub(super) fn parse_timeout_ms(value: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .ok()
        .filter(|ms| (1..=MAX_TIMEOUT_MS).contains(ms))
        .ok_or_else(|| format!("request timeout must be between 1 and {MAX_TIMEOUT_MS} milliseconds"))
}

pub(super) struct Deadline {
    completed: Sender<Instant>,
    watchdog: Option<JoinHandle<()>>,
}

impl Deadline {
    pub(super) fn start(timeout: Duration) -> io::Result<Self> {
        if timeout.is_zero() || timeout > Duration::from_millis(MAX_TIMEOUT_MS) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "request deadline must be positive and at most five minutes",
            ));
        }
        // Anchor on the caller, not on when the watchdog happens to be scheduled.
        let expires = Instant::now().checked_add(timeout).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "request deadline overflow")
        })?;
        let (completed, receiver) = mpsc::channel();
        let watchdog = thread::Builder::new()
            .name("cass-request-deadline".into())
            .spawn(move || {
                match receiver.recv_timeout(expires.saturating_duration_since(Instant::now())) {
                    // A timely completion stays timely even if this thread is
                    // scheduled late. Conversely, a late completion cannot
                    // erase a deadline merely by winning the channel race.
                    Ok(finished) if finished < expires => {}
                    Ok(_) | Err(RecvTimeoutError::Timeout) => {
                        // Do not log, flush, unwind or lock stdout/stderr here:
                        // the request can be blocked while holding those locks.
                        // OS teardown releases all worker threads and handles.
                        std::process::exit(TIMEOUT_EXIT_CODE);
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        std::process::exit(WATCHDOG_FAILURE_EXIT_CODE);
                    }
                }
            })?;
        Ok(Self {
            completed,
            watchdog: Some(watchdog),
        })
    }

    pub(super) fn for_frame(
        input: &mut impl BufRead,
        timeout: Duration,
    ) -> io::Result<Option<Self>> {
        // Waiting for a new request is intentionally not work. Once any byte is
        // available, the guard covers the rest of framing AND response flushing.
        if input.fill_buf()?.is_empty() {
            return Ok(None);
        }
        Self::start(timeout).map(Some)
    }
}

impl Drop for Deadline {
    fn drop(&mut self) {
        let signalled = self.completed.send(Instant::now()).is_ok();
        let joined = self.watchdog.take().is_some_and(|watchdog| watchdog.join().is_ok());
        if !signalled || !joined {
            // A failed supervisor must not leave later native work unprotected.
            std::process::exit(WATCHDOG_FAILURE_EXIT_CODE);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::process::{Command, Stdio};

    #[test]
    fn timeout_configuration_refuses_unbounded_and_overflow_values() {
        for value in ["0", "-1", "300001", "18446744073709551616", "", "x"] {
            assert!(parse_timeout_ms(value).is_err(), "{value}");
        }
        assert_eq!(parse_timeout_ms("1").unwrap(), 1);
        assert_eq!(parse_timeout_ms("300000").unwrap(), MAX_TIMEOUT_MS);
        assert_eq!(parse_timeout_ms("30000").unwrap(), DEFAULT_TIMEOUT_MS);
        assert!(Deadline::start(Duration::ZERO).is_err());
        assert!(Deadline::start(Duration::from_secs(301)).is_err());
    }

    #[test]
    fn framing_leaves_idle_eof_unarmed_and_does_not_consume_first_byte() -> io::Result<()> {
        let timeout = Duration::from_secs(5);
        assert!(Deadline::for_frame(&mut Cursor::new(b""), timeout)?.is_none());
        let mut input = Cursor::new(b"partial frame");
        let guard = Deadline::for_frame(&mut input, timeout)?;
        assert!(guard.is_some());
        assert_eq!(input.position(), 0);
        drop(guard);
        Ok(())
    }

    #[test]
    fn completed_guards_join_without_leaving_a_stale_timer() -> io::Result<()> {
        for _ in 0..16 {
            drop(Deadline::start(Duration::from_secs(1))?);
        }
        thread::sleep(Duration::from_millis(1100));
        drop(Deadline::start(Duration::from_secs(1))?);
        Ok(())
    }

    fn expect_timeout(fixture: &str) -> io::Result<()> {
        // The same exact production module is compiled both in CASS and by the
        // small native supervisor gate. Strip only the crate name from the
        // harness's test path, retaining all nested module components.
        let module = module_path!().split_once("::").unwrap().1;
        let mut child = Command::new(std::env::current_exe()?)
            .args(["--exact", &format!("{module}::{fixture}"), "--ignored", "--nocapture"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let started = Instant::now();
        loop {
            if let Some(status) = child.try_wait()? {
                assert_eq!(status.code(), Some(TIMEOUT_EXIT_CODE));
                return Ok(());
            }
            if started.elapsed() > Duration::from_secs(10) {
                let _ = child.kill();
                let _ = child.wait();
                panic!("watchdog failed to terminate {fixture}");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn stalled_native_work_is_terminated_not_detached() -> io::Result<()> {
        expect_timeout("stalled_native_fixture")
    }

    #[test]
    fn held_stdio_locks_do_not_block_deadline_termination() -> io::Result<()> {
        expect_timeout("held_stdio_fixture")
    }

    #[test]
    #[ignore = "subprocess fixture: intentionally terminates its process"]
    fn stalled_native_fixture() {
        let _guard = Deadline::start(Duration::from_millis(100)).unwrap();
        thread::sleep(Duration::from_secs(30));
        panic!("watchdog did not terminate stalled work");
    }

    #[test]
    #[ignore = "subprocess fixture: intentionally terminates its process"]
    fn held_stdio_fixture() {
        let stdout = io::stdout();
        let stderr = io::stderr();
        let _out = stdout.lock();
        let _err = stderr.lock();
        let _guard = Deadline::start(Duration::from_millis(100)).unwrap();
        thread::sleep(Duration::from_secs(30));
        panic!("watchdog waited for stdio locks");
    }
}
