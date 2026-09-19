//! One request owns its socket budget, including delivery of its response.
//!
//! Byte progress never replenishes the budget. All I/O is nonblocking and the
//! caller retains both the socket and cancellation state; no helper thread can
//! survive a timed-out exchange. Native inference and filesystem operations are
//! cooperative boundaries, not forcibly preemptible work.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const POLL_INTERVAL: Duration = Duration::from_millis(5);
const SHUTDOWN_ACK_BUDGET: Duration = Duration::from_millis(250);

pub(super) struct RequestBudget<'a> {
    started: Instant,
    timeout: Duration,
    shutdown: &'a AtomicBool,
}

impl<'a> RequestBudget<'a> {
    pub(super) fn new(timeout: Duration, shutdown: &'a AtomicBool) -> Self {
        Self {
            started: Instant::now(),
            timeout,
            shutdown,
        }
    }

    fn time_remaining(&self) -> Option<Duration> {
        let remaining = self.timeout.saturating_sub(self.started.elapsed());
        (!remaining.is_zero()).then_some(remaining)
    }

    pub(super) fn remaining(&self) -> Option<Duration> {
        if self.shutdown.load(Ordering::Acquire) {
            None
        } else {
            self.time_remaining()
        }
    }

    /// False means EOF, cancellation, or budget exhaustion. Any partial frame
    /// then belongs to the closing connection and must never be dispatched.
    pub(super) fn read_exact(
        &self,
        stream: &mut UnixStream,
        mut bytes: &mut [u8],
    ) -> io::Result<bool> {
        stream.set_nonblocking(true)?;
        loop {
            let Some(remaining) = self.remaining() else {
                return Ok(false);
            };
            if bytes.is_empty() {
                return Ok(true);
            }
            match stream.read(bytes) {
                Ok(0) => return Ok(false),
                Ok(count) => bytes = &mut bytes[count..],
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(remaining.min(POLL_INTERVAL));
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Shutdown itself sets cancellation before replying. Permit that one ACK
    /// only, within both the original request deadline and a short delivery
    /// budget. Every other blocked response observes shutdown immediately.
    pub(super) fn write_all(
        &self,
        stream: &mut UnixStream,
        mut bytes: &[u8],
        shutdown_ack: bool,
    ) -> io::Result<bool> {
        stream.set_nonblocking(true)?;
        let delivery_started = Instant::now();
        loop {
            let remaining = if shutdown_ack {
                self.time_remaining().map(|remaining| {
                    remaining.min(SHUTDOWN_ACK_BUDGET.saturating_sub(delivery_started.elapsed()))
                })
            } else {
                self.remaining()
            };
            let Some(remaining) = remaining.filter(|remaining| !remaining.is_zero()) else {
                return Ok(false);
            };
            if bytes.is_empty() {
                return Ok(true);
            }
            match stream.write(bytes) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "daemon response made no progress",
                    ));
                }
                Ok(count) => bytes = &bytes[count..],
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(remaining.min(POLL_INTERVAL));
                }
                Err(error) => return Err(error),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fill the real socket's send buffer rather than assuming an OS-specific
    // capacity. A non-reading peer then exercises actual write backpressure.
    fn fill_socket(stream: &mut UnixStream) {
        stream.set_nonblocking(true).unwrap();
        let block = [0_u8; 8192];
        for _ in 0..8192 {
            match stream.write(&block) {
                Ok(0) => panic!("socket accepted zero bytes"),
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => panic!("fill socket: {error}"),
            }
        }
        panic!("socket did not apply backpressure within 64 MiB");
    }

    #[test]
    fn partial_progress_cannot_renew_the_read_deadline() {
        let shutdown = AtomicBool::new(false);
        let (mut server, mut peer) = UnixStream::pair().unwrap();
        peer.write_all(&[1]).unwrap();
        std::thread::scope(|scope| {
            let sender = scope.spawn(move || {
                peer.set_nonblocking(true).unwrap();
                for _ in 0..80 {
                    if peer.write(&[1]).is_err() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            });
            let budget = RequestBudget::new(Duration::from_millis(70), &shutdown);
            let started = Instant::now();
            let mut frame = [0; 1024];
            assert!(!budget.read_exact(&mut server, &mut frame).unwrap());
            assert!(frame.contains(&1), "the peer must have made progress");
            assert!(started.elapsed() < Duration::from_millis(500));
            assert!(budget.remaining().is_none());
            drop(server);
            sender.join().unwrap();
        });
    }

    #[test]
    fn prefix_and_payload_share_the_same_budget() {
        let shutdown = AtomicBool::new(false);
        let (mut server, mut peer) = UnixStream::pair().unwrap();
        let mut budget = RequestBudget::new(Duration::from_secs(10), &shutdown);
        peer.write_all(&[0, 0, 0, 1]).unwrap();
        assert!(budget.read_exact(&mut server, &mut [0; 4]).unwrap());
        // Simulate elapsed upload time without depending on a tiny scheduling
        // window; do not construct a new budget for the payload phase.
        budget.started = Instant::now() - Duration::from_secs(11);
        peer.write_all(&[42]).unwrap();
        let mut payload = [0];
        assert!(!budget.read_exact(&mut server, &mut payload).unwrap());
        assert_eq!(
            payload,
            [0],
            "expired payload must not be admitted even when buffered"
        );
    }

    #[test]
    fn write_backpressure_respects_the_request_deadline() {
        let shutdown = AtomicBool::new(false);
        let (mut server, _peer) = UnixStream::pair().unwrap();
        fill_socket(&mut server);
        let budget = RequestBudget::new(Duration::from_millis(30), &shutdown);
        assert!(!budget.write_all(&mut server, &[42], false).unwrap());
        assert!(budget.remaining().is_none());
    }

    #[test]
    fn cancellation_interrupts_a_blocked_writer() {
        let shutdown = AtomicBool::new(false);
        let (mut server, _peer) = UnixStream::pair().unwrap();
        fill_socket(&mut server);
        std::thread::scope(|scope| {
            let budget = RequestBudget::new(Duration::from_secs(10), &shutdown);
            let writer = scope.spawn(move || budget.write_all(&mut server, &[42], false));
            std::thread::sleep(Duration::from_millis(20));
            let started = Instant::now();
            shutdown.store(true, Ordering::Release);
            assert!(!writer.join().unwrap().unwrap());
            assert!(started.elapsed() < Duration::from_secs(1));
        });
    }

    #[test]
    fn shutdown_ack_is_delivered_but_cannot_pin_shutdown() {
        let shutdown = AtomicBool::new(true);
        let (mut server, mut peer) = UnixStream::pair().unwrap();
        let budget = RequestBudget::new(Duration::from_secs(10), &shutdown);
        assert!(!budget.write_all(&mut server, &[1], false).unwrap());
        assert!(budget.write_all(&mut server, &[2], true).unwrap());
        let mut ack = [0];
        peer.read_exact(&mut ack).unwrap();
        assert_eq!(ack, [2]);
        fill_socket(&mut server);
        let started = Instant::now();
        assert!(!budget.write_all(&mut server, &[3], true).unwrap());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn zero_budget_admits_neither_buffered_input_nor_output() {
        let shutdown = AtomicBool::new(false);
        let (mut server, mut peer) = UnixStream::pair().unwrap();
        peer.write_all(&[42]).unwrap();
        let budget = RequestBudget::new(Duration::ZERO, &shutdown);
        assert!(!budget.read_exact(&mut server, &mut [0]).unwrap());
        assert!(!budget.write_all(&mut server, &[42], false).unwrap());
        assert!(!budget.write_all(&mut server, &[42], true).unwrap());
    }

    #[test]
    fn complete_exchanges_keep_one_socket_usable_with_fresh_request_budgets() {
        let shutdown = AtomicBool::new(false);
        let (mut server, mut peer) = UnixStream::pair().unwrap();
        for byte in [1, 2, 3] {
            let budget = RequestBudget::new(Duration::from_secs(1), &shutdown);
            peer.write_all(&[byte]).unwrap();
            let mut input = [0];
            assert!(budget.read_exact(&mut server, &mut input).unwrap());
            assert!(budget.write_all(&mut server, &input, false).unwrap());
            let mut output = [0];
            peer.read_exact(&mut output).unwrap();
            assert_eq!(output, [byte]);
        }
    }

    #[test]
    fn extreme_duration_does_not_overflow_and_eof_closes_the_exchange() {
        let shutdown = AtomicBool::new(false);
        let budget = RequestBudget::new(Duration::MAX, &shutdown);
        assert!(budget.remaining().is_some());
        let (mut server, peer) = UnixStream::pair().unwrap();
        drop(peer);
        assert!(!budget.read_exact(&mut server, &mut [0]).unwrap());
    }
}
