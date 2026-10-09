//! A terminal deadline for work that must retain its caller's ownership.
//!
//! Database work stays on the caller thread. Only the timer runs elsewhere;
//! completion and unwinding disarm and join it before returning. A timeout
//! ends the process instead of returning while an uninterruptible database
//! future still owns storage resources or could resume writing later.

use std::io::{self, Write};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const DIAGNOSTIC_REPORT_BUDGET: Duration = Duration::from_millis(50);

fn abort_with_diagnostic(timeout_diagnostic: String) -> ! {
    // stderr can itself be held by the blocked owner or connected to a full
    // pipe. Reporting is best-effort after the timeout has already won; this
    // terminal scope never returns or lets its reporter escape the process.
    thread::scope(|scope| {
        let (sent, received) = std::sync::mpsc::sync_channel(1);
        let _reporter = thread::Builder::new()
            .name("cass-checkpoint-timeout-report".into())
            .spawn_scoped(scope, move || {
                let _ = writeln!(io::stderr(), "{timeout_diagnostic}");
                let _ = sent.send(());
            });
        let _ = received.recv_timeout(DIAGNOSTIC_REPORT_BUDGET);
        std::process::exit(70);
    })
}

struct Completion {
    disarmed: Mutex<bool>,
    wake: Condvar,
}

struct Deadline {
    completion: Arc<Completion>,
    thread: Option<JoinHandle<()>>,
}

impl Deadline {
    fn start(timeout: Duration, timeout_diagnostic: String) -> io::Result<Self> {
        let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "checkpoint deadline is out of range",
            )
        })?;
        let completion = Arc::new(Completion {
            disarmed: Mutex::new(false),
            wake: Condvar::new(),
        });
        let observer = Arc::clone(&completion);
        let thread = thread::Builder::new()
            .name("cass-checkpoint-deadline".into())
            .spawn(move || {
                let mut disarmed = observer
                    .disarmed
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                while !*disarmed {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        // Once the timer wins, the caller's Drop joins this
                        // thread and cannot return a late success before exit.
                        drop(disarmed);
                        abort_with_diagnostic(timeout_diagnostic);
                    }
                    let (next, _) = observer
                        .wake
                        .wait_timeout(disarmed, remaining)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    disarmed = next;
                }
            })?;
        Ok(Self {
            completion,
            thread: Some(thread),
        })
    }
}

impl Drop for Deadline {
    fn drop(&mut self) {
        {
            let mut disarmed = self
                .completion
                .disarmed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *disarmed = true;
            self.completion.wake.notify_one();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Run `work` on this thread under a terminal deadline. The closure need not
/// be `Send`: thread-affine connections and their cleanup stay with the owner.
/// Timer creation errors prevent work from starting. The diagnostic must be a
/// preformatted single line; timeout attempts stderr reporting for at most
/// 50 milliseconds, then exits 70 even if diagnostic output is blocked.
pub(super) fn run<T>(
    timeout: Duration,
    timeout_diagnostic: String,
    work: impl FnOnce() -> T,
) -> io::Result<T> {
    let deadline = Deadline::start(timeout, timeout_diagnostic)?;
    let result = work();
    drop(deadline);
    Ok(result)
}
