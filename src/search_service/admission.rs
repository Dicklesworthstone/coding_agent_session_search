//! Nonblocking, cross-process reader leases on an operator-selected local pool.
//!
//! A lease covers expensive reader ownership, not a machine-wide RSS budget.
//! Pool files are coordination metadata, never archive files. Never unlink them:
//! replacing a locked inode would admit a second set of owners under the same name.
//! Kernel locks, not PIDs or wall-clock leases, release slots after process death.

use std::fmt;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

pub(super) const DEFAULT_SLOTS: u32 = 1;
pub(super) const MAX_SLOTS: u32 = 64;

pub(super) fn parse_slots(value: &str) -> Result<u32, String> {
    value
        .parse::<u32>()
        .ok()
        .filter(|slots| (1..=MAX_SLOTS).contains(slots))
        .ok_or_else(|| format!("admission slots must be between 1 and {MAX_SLOTS}"))
}

#[derive(Debug)]
pub(super) enum Refusal {
    Busy,
    Policy,
    Io(io::Error),
}

impl Refusal {
    pub(super) fn kind(&self) -> &'static str {
        match self {
            Self::Busy => "admission_busy",
            Self::Policy => "admission_policy_mismatch",
            Self::Io(_) => "admission_unavailable",
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => f.write_str("reader pool is busy; reuse an admitted worker, unload its reader, or retry later"),
            Self::Policy => f.write_str("reader pool policy is invalid or uses a different slot count; never replace pool files while workers are alive"),
            Self::Io(error) => write!(f, "reader pool could not be admitted: {error}"),
        }
    }
}

impl std::error::Error for Refusal {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for Refusal {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Debug, Clone)]
pub(super) struct Pool {
    path: PathBuf,
    slots: u32,
}

#[derive(Debug)]
pub(super) struct Lease {
    // Keep the sole handle alive for the entire admitted reader lifetime.
    _file: File,
}

impl Pool {
    /// Configuration only: status/invalid requests must not touch the filesystem.
    pub(super) fn new(path: PathBuf, slots: u32) -> io::Result<Self> {
        if !(1..=MAX_SLOTS).contains(&slots) || path.as_os_str().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid reader pool configuration",
            ));
        }
        Ok(Self { path, slots })
    }

    pub(super) fn slots(&self) -> u32 {
        self.slots
    }

    pub(super) fn acquire(&self) -> Result<Lease, Refusal> {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&self.path)?;
        if !fs::symlink_metadata(&self.path)?.file_type().is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "pool must be a real local directory, not a symlink",
            )
            .into());
        }

        // Serialize first initialization and policy checking. The wait for this
        // control is bounded and never waits for a worker: workers hold slots.
        // All cooperating workers must agree on the pool size.
        let mut policy = open_control(&self.path.join("policy-v1"))?;
        lock_control(&policy)?;
        let expected = format!("CASS-READER-POOL-1\nslots={}\n", self.slots);
        let mut actual = String::new();
        (&mut policy).take(513).read_to_string(&mut actual)?;
        if actual.is_empty() {
            policy.write_all(expected.as_bytes())?;
            policy.sync_all()?;
        } else if actual != expected {
            // A crash-partial policy is not repaired by overwriting it. No reader
            // is admitted until the operator resolves the configuration safely.
            return Err(Refusal::Policy);
        }
        for slot in 0..self.slots {
            let file = open_control(&self.path.join(format!("reader-{slot:02}.lock")))?;
            match lock(&file) {
                Ok(()) => return Ok(Lease { _file: file }),
                Err(Refusal::Busy) => continue,
                Err(error) => return Err(error),
            }
        }
        Err(Refusal::Busy)
    }
}

fn lock(file: &File) -> Result<(), Refusal> {
    match file.try_lock() {
        Ok(()) => Ok(()),
        Err(TryLockError::WouldBlock) => Err(Refusal::Busy),
        Err(TryLockError::Error(error)) => Err(Refusal::Io(error)),
    }
}

/// How long `acquire` waits for the policy control (j79k1). That lock covers
/// only reading or initializing `policy-v1` and scanning the slots, so
/// contention means another acquirer is inside that step, or a child process
/// another thread just spawned holds a copy of the descriptor until it execs.
/// Neither means the pool is full, yet a nonblocking attempt reported `Busy`
/// (and hid a `Policy` mismatch behind it). Slot locks stay nonblocking.
const CONTROL_LOCK_WAIT: std::time::Duration = std::time::Duration::from_millis(250);

fn lock_control(file: &File) -> Result<(), Refusal> {
    let deadline = std::time::Instant::now() + CONTROL_LOCK_WAIT;
    loop {
        match lock(file) {
            Err(Refusal::Busy) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            outcome => return outcome,
        }
    }
}

fn open_control(path: &Path) -> io::Result<File> {
    match fs::symlink_metadata(path) {
        Ok(meta) if !meta.file_type().is_file() => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "pool controls must be regular files, not symlinks",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "pool control is not a regular file",
        ));
    }
    // The operator must own/trust the directory and its parents. This preflight
    // is not an atomic security boundary against concurrent path replacement.
    Ok(file)
}

/// Acquire a slot a test has just released (j79k1). A lease is a `flock` on
/// an open file, and while another thread of the same test binary spawns a
/// child, the child holds a copy of every descriptor until it execs
/// (close-on-exec), so a released slot can stay locked for that window
/// (search_service's own tests spawn `cass` and their own binary in parallel).
/// Slots are nonblocking by design, so every test that re-acquires to prove a
/// release uses this: waiting, bounded, keeps the assertion that the release
/// happens without racing a peer's spawn, and a lease that is never released
/// still fails after the deadline. (The policy control's equivalent window is
/// handled in `acquire` itself; see `CONTROL_LOCK_WAIT`.)
#[cfg(test)]
pub(super) fn acquire_after_release(pool: &Pool) -> Result<Lease, Refusal> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match pool.acquire() {
            Err(Refusal::Busy) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            outcome => return outcome,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Command, Stdio};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    fn directory() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "cass-pool-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn invalid_configuration_and_lazy_configuration_do_not_touch_disk() {
        let path = directory();
        for value in ["0", "-1", "65", "4294967296", "", "x"] {
            assert!(parse_slots(value).is_err());
        }
        assert_eq!(parse_slots("1").unwrap(), DEFAULT_SLOTS);
        assert_eq!(parse_slots("64").unwrap(), MAX_SLOTS);
        assert!(Pool::new(path.clone(), 0).is_err());
        assert!(Pool::new(path.clone(), 65).is_err());
        assert_eq!(Pool::new(path.clone(), 2).unwrap().slots(), 2);
        assert!(!path.exists());
    }

    #[test]
    fn exact_slot_count_is_enforced_and_release_reuses_existing_files() -> Result<(), Refusal> {
        let path = directory();
        let pool = Pool::new(path.clone(), 2)?;
        let first = pool.acquire()?;
        let second = pool.acquire()?;
        assert!(matches!(pool.acquire(), Err(Refusal::Busy)));
        let before = fs::read(path.join("policy-v1"))?;
        drop(first);
        let replacement = acquire_after_release(&pool)?;
        assert!(matches!(pool.acquire(), Err(Refusal::Busy)));
        drop(second);
        drop(replacement);
        drop(acquire_after_release(&pool)?);
        assert_eq!(fs::read(path.join("policy-v1"))?, before);
        assert_eq!(fs::read_dir(path)?.count(), 3);
        Ok(())
    }

    /// j79k1: contention on the policy control is transient, so acquire waits
    /// for it, bounded, instead of reporting a full pool; a control held past
    /// the bound still refuses `Busy`.
    #[test]
    fn acquire_waits_briefly_for_the_policy_control_and_refuses_past_the_bound()
    -> Result<(), Refusal> {
        let path = directory();
        let pool = Pool::new(path.clone(), 1)?;
        drop(pool.acquire()?);

        let control = open_control(&path.join("policy-v1"))?;
        control.lock()?;
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let acquirer = {
            let pool = pool.clone();
            thread::spawn(move || {
                started_tx.send(()).ok();
                pool.acquire().map(drop)
            })
        };
        started_rx
            .recv()
            .map_err(|error| Refusal::Io(io::Error::other(error)))?;
        thread::sleep(Duration::from_millis(20));
        drop(control);
        acquirer
            .join()
            .map_err(|_| Refusal::Io(io::Error::other("acquirer panicked")))??;

        // Negative: held past the bound, the control still refuses Busy, and
        // the wait is bounded.
        let control = open_control(&path.join("policy-v1"))?;
        control.lock()?;
        let started = Instant::now();
        assert!(matches!(pool.acquire(), Err(Refusal::Busy)));
        let waited = started.elapsed();
        assert!(
            waited >= CONTROL_LOCK_WAIT && waited < CONTROL_LOCK_WAIT * 8,
            "{waited:?}"
        );
        drop(control);
        drop(acquire_after_release(&pool)?);
        Ok(())
    }

    #[test]
    fn conflicting_policy_cannot_expand_a_live_pool() -> Result<(), Refusal> {
        let path = directory();
        let one = Pool::new(path.clone(), 1)?;
        let _lease = one.acquire()?;
        for slots in [2, 64] {
            let other = Pool::new(path.clone(), slots)?;
            assert!(matches!(other.acquire(), Err(Refusal::Policy)));
        }
        assert!(!path.join("reader-01.lock").exists());
        Ok(())
    }

    #[test]
    fn corrupt_policy_is_refused_without_rewriting_or_admitting() -> io::Result<()> {
        let path = directory();
        fs::create_dir(&path)?;
        let bytes = b"CASS-READER-POOL-1\nslots=";
        fs::write(path.join("policy-v1"), bytes)?;
        let pool = Pool::new(path.clone(), 1)?;
        assert!(matches!(pool.acquire(), Err(Refusal::Policy)));
        assert_eq!(fs::read(path.join("policy-v1"))?, bytes);
        assert!(!path.join("reader-00.lock").exists());
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
    fn kernel_releases_another_process_lease_after_forced_termination() -> Result<(), Refusal> {
        let path = directory();
        let module = module_path!().split_once("::").unwrap().1;
        let mut child = OwnedChild(
            Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    &format!("{module}::lease_holder"),
                    "--ignored",
                    "--nocapture",
                ])
                .env("CASS_POOL_TEST_ROOT", &path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?,
        );
        let started = Instant::now();
        while !path.join("ready").is_file() {
            assert!(
                child.0.try_wait()?.is_none(),
                "lease fixture exited before admission"
            );
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "lease fixture did not become ready"
            );
            thread::sleep(Duration::from_millis(10));
        }
        let pool = Pool::new(path.clone(), 1)?;
        assert!(matches!(pool.acquire(), Err(Refusal::Busy)));
        let before = fs::read(path.join("policy-v1"))?;
        child.0.kill()?;
        child.0.wait()?;
        drop(pool.acquire()?);
        assert_eq!(fs::read(path.join("policy-v1"))?, before);
        Ok(())
    }

    #[test]
    #[ignore = "subprocess fixture: parent kills this admitted worker"]
    fn lease_holder() -> Result<(), Refusal> {
        // Test-only process argument; no production configuration reads env here.
        let path = PathBuf::from(std::env::var_os("CASS_POOL_TEST_ROOT").expect("fixture root"));
        let pool = Pool::new(path.clone(), 1)?;
        let _lease = pool.acquire()?;
        fs::write(path.join("ready"), b"admitted")?;
        thread::sleep(Duration::from_secs(30));
        panic!("parent did not terminate the fixture");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_control_is_refused_without_touching_its_target() -> io::Result<()> {
        let path = directory();
        fs::create_dir(&path)?;
        let sentinel = path.join("sentinel");
        fs::write(&sentinel, b"preserve me")?;
        std::os::unix::fs::symlink(&sentinel, path.join("policy-v1"))?;
        let pool = Pool::new(path, 1)?;
        assert!(matches!(pool.acquire(), Err(Refusal::Io(_))));
        assert_eq!(fs::read(sentinel)?, b"preserve me");
        Ok(())
    }
}
