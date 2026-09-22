//! Sampled resident-memory termination for the entire read-only worker lifetime.
//!
//! This is not an OS allocation limit: a process can overshoot between samples,
//! and swapped-out pages are not resident memory. No archive/model/other process
//! is opened by the probe. It never writes to potentially blocked output pipes.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

pub(super) const MIB: u64 = 1024 * 1024;
pub(super) const DEFAULT_MAX_RESIDENT_MIB: u64 = 4096;
pub(super) const MAX_RESIDENT_MIB: u64 = 1024 * 1024;
pub(super) const SAMPLE_INTERVAL_MS: u64 = 100;
pub(super) const MEMORY_EXIT_CODE: i32 = 126;
pub(super) const MONITOR_FAILURE_EXIT_CODE: i32 = 125;

pub(super) fn parse_max_resident_mib(value: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .ok()
        .filter(|mib| (1..=MAX_RESIDENT_MIB).contains(mib))
        .ok_or_else(|| format!("resident-memory threshold must be 1..={MAX_RESIDENT_MIB} MiB"))
}

/// Fresh state prevents a failed refresh from reusing an old, low memory value.
/// Query only this PID and explicitly exclude Linux task enumeration. Do not
/// call System::new_all or gather command lines, environments or other archives.
fn resident_bytes() -> io::Result<u64> {
    let pid = Pid::from_u32(std::process::id());
    let mut system = System::new();
    let refreshed = system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing().without_tasks().with_memory(),
    );
    let bytes = system.process(pid).map(|process| process.memory());
    match (refreshed, bytes) {
        (1, Some(bytes)) if bytes > 0 => Ok(bytes),
        _ => Err(io::Error::other(
            "cannot obtain a fresh resident-memory sample for this worker",
        )),
    }
}

pub(super) struct Observation {
    limit_bytes: u64,
    started: Instant,
    current_bytes: AtomicU64,
    peak_bytes: AtomicU64,
    sampled_at_ms: AtomicU64,
    samples: AtomicU64,
}

pub(super) struct Sample {
    pub limit_bytes: u64,
    pub current_bytes: u64,
    pub peak_bytes: u64,
    pub age_ms: u64,
    pub samples: u64,
}

impl Observation {
    fn elapsed_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn observe(&self, bytes: u64) {
        self.peak_bytes.fetch_max(bytes, Ordering::Relaxed);
        self.current_bytes.store(bytes, Ordering::Relaxed);
        self.sampled_at_ms
            .store(self.elapsed_ms(), Ordering::Release);
        self.samples.fetch_add(1, Ordering::Release);
    }

    /// Status reads atomics only; it never waits for an OS probe or performs I/O.
    /// The numbers are monitoring observations, not an allocation certificate.
    pub(super) fn sample(&self) -> Sample {
        let sampled_at = self.sampled_at_ms.load(Ordering::Acquire);
        let current_bytes = self.current_bytes.load(Ordering::Relaxed);
        Sample {
            limit_bytes: self.limit_bytes,
            current_bytes,
            peak_bytes: self.peak_bytes.load(Ordering::Relaxed).max(current_bytes),
            age_ms: self.elapsed_ms().saturating_sub(sampled_at),
            samples: self.samples.load(Ordering::Acquire),
        }
    }
}

pub(super) struct Guard {
    observation: Arc<Observation>,
    stop: Sender<()>,
    monitor: Option<JoinHandle<()>>,
}

impl Guard {
    pub(super) fn start(max_resident_mib: u64) -> io::Result<Self> {
        if !(1..=MAX_RESIDENT_MIB).contains(&max_resident_mib) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid resident-memory threshold",
            ));
        }
        let limit_bytes = max_resident_mib.checked_mul(MIB).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "resident-memory threshold overflow",
            )
        })?;
        let bytes = resident_bytes()?;
        if bytes > limit_bytes {
            std::process::exit(MEMORY_EXIT_CODE);
        }
        let observation = Arc::new(Observation {
            limit_bytes,
            started: Instant::now(),
            current_bytes: AtomicU64::new(bytes),
            peak_bytes: AtomicU64::new(bytes),
            sampled_at_ms: AtomicU64::new(0),
            samples: AtomicU64::new(1),
        });
        let observed = Arc::clone(&observation);
        let (stop, receiver) = mpsc::channel();
        let monitor = thread::Builder::new()
            .name("cass-resident-memory".into())
            .spawn(move || {
                loop {
                    match receiver.recv_timeout(Duration::from_millis(SAMPLE_INTERVAL_MS)) {
                        Ok(()) => return,
                        Err(RecvTimeoutError::Disconnected) => {
                            std::process::exit(MONITOR_FAILURE_EXIT_CODE);
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                    }
                    let bytes = resident_bytes().unwrap_or_else(|_| {
                        std::process::exit(MONITOR_FAILURE_EXIT_CODE);
                    });
                    observed.observe(bytes);
                    if bytes > limit_bytes {
                        // Never log, flush or wait for the thread running the query.
                        // The kernel also releases any reader-pool lease on exit.
                        std::process::exit(MEMORY_EXIT_CODE);
                    }
                }
            })?;
        Ok(Self {
            observation,
            stop,
            monitor: Some(monitor),
        })
    }

    pub(super) fn observation(&self) -> Arc<Observation> {
        Arc::clone(&self.observation)
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        let signalled = self.stop.send(()).is_ok();
        let joined = self
            .monitor
            .take()
            .is_some_and(|monitor| monitor.join().is_ok());
        if !signalled || !joined {
            std::process::exit(MONITOR_FAILURE_EXIT_CODE);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::admission::Pool;
    use super::*;
    use std::process::{Child, Command, Stdio};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn configuration_refuses_disabled_unbounded_and_overflow_limits() {
        for value in ["0", "-1", "1048577", "18446744073709551616", "", "nan"] {
            assert!(parse_max_resident_mib(value).is_err(), "{value}");
        }
        assert_eq!(
            parse_max_resident_mib("4096").unwrap(),
            DEFAULT_MAX_RESIDENT_MIB
        );
        assert_eq!(parse_max_resident_mib("1048576").unwrap(), MAX_RESIDENT_MIB);
        assert!(Guard::start(0).is_err());
        assert!(Guard::start(MAX_RESIDENT_MIB + 1).is_err());
        assert!(Guard::start(u64::MAX).is_err());
    }

    #[test]
    fn own_process_resident_measurement_is_fresh_and_nonzero() -> io::Result<()> {
        assert!(resident_bytes()? > 0);
        assert!(resident_bytes()? > 0);
        Ok(())
    }

    #[test]
    fn live_monitor_reports_samples_and_joins_without_killing_idle_process() -> io::Result<()> {
        let baseline = resident_bytes()?;
        let guard = Guard::start(baseline.div_ceil(MIB) + 256)?;
        let observation = guard.observation();
        let deadline = Instant::now() + Duration::from_secs(10);
        while observation.sample().samples < 2 {
            assert!(
                Instant::now() < deadline,
                "memory monitor must keep sampling while idle"
            );
            thread::sleep(Duration::from_millis(10));
        }
        let sample = observation.sample();
        assert_eq!(sample.limit_bytes, (baseline.div_ceil(MIB) + 256) * MIB);
        assert!(sample.current_bytes > 0);
        assert!(sample.peak_bytes >= sample.current_bytes);
        assert!(sample.age_ms < 10_000);
        assert!(sample.samples >= 2);
        drop(guard);
        // No stale timer after stopping the observer. The final values remain
        // available to code releasing its own reference, without another probe.
        let stopped = observation.sample().current_bytes;
        thread::sleep(Duration::from_millis(SAMPLE_INTERVAL_MS * 2));
        assert_eq!(observation.sample().current_bytes, stopped);
        Ok(())
    }

    struct OwnedChild(Child);
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn memory_growth_terminates_with_held_stdio_and_releases_kernel_lease() -> io::Result<()> {
        let path = std::env::temp_dir().join(format!(
            "cass-memory-pool-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let module = module_path!().split_once("::").unwrap().1;
        let mut child = OwnedChild(
            Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    &format!("{module}::growing_process_fixture"),
                    "--ignored",
                    "--nocapture",
                ])
                .env("CASS_MEMORY_TEST_POOL", &path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?,
        );
        let outer_deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = child.0.try_wait()? {
                break status;
            }
            assert!(
                Instant::now() < outer_deadline,
                "memory monitor failed to terminate growing worker"
            );
            thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(status.code(), Some(MEMORY_EXIT_CODE));
        assert!(
            path.join("ready").is_file(),
            "fixture must acquire lease before memory growth"
        );
        let pool = Pool::new(path.clone(), 1)?;
        let _lease = pool.acquire().map_err(io::Error::other)?;
        assert_eq!(
            std::fs::read(path.join("policy-v1"))?,
            b"CASS-READER-POOL-1\nslots=1\n"
        );
        Ok(())
    }

    #[test]
    #[ignore = "subprocess fixture: allocates real resident pages and terminates"]
    fn growing_process_fixture() {
        let path = std::path::PathBuf::from(std::env::var_os("CASS_MEMORY_TEST_POOL").unwrap());
        let pool = Pool::new(path.clone(), 1).unwrap();
        let _lease = pool.acquire().unwrap();
        let baseline = resident_bytes().unwrap();
        let _guard = Guard::start(baseline.div_ceil(MIB) + 16).unwrap();
        let stdout = io::stdout();
        let stderr = io::stderr();
        let _out = stdout.lock();
        let _err = stderr.lock();
        std::fs::write(path.join("ready"), b"lease acquired before allocation").unwrap();
        // Actually touch every byte, not merely reserve virtual address space.
        // Hold the allocation live and make pages different, including on macOS.
        let mut pages = vec![0_u8; 64 * MIB as usize];
        let mut state = 0x9e3779b9_u32;
        for chunk in pages.chunks_mut(4) {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            chunk.copy_from_slice(&state.to_ne_bytes());
        }
        std::hint::black_box(&pages);
        thread::sleep(Duration::from_secs(30));
        std::hint::black_box(pages);
        panic!("growing worker survived its resident-memory threshold");
    }
}
