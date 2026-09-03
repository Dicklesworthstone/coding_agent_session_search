//! Stale-on-read index catch-up: spawn a detached, low-priority incremental
//! `cass index` when a read path (search, pack, TUI launch, daemon tick)
//! notices the index is behind the session files on disk.
//!
//! Design constraints:
//!
//! * **Never block the read.** The caller serves its current (possibly stale)
//!   results immediately; the catch-up runs in a separate process that
//!   outlives the caller. The *next* read is fresh.
//! * **At most one catch-up at a time, machine-wide per data dir.** The
//!   spawned child takes the normal `index-run.lock`, so it cannot race a
//!   foreground `cass index`; this module additionally checks that lock
//!   before spawning and serializes spawners with a tiny flock so N parallel
//!   agent searches spawn one child, not N.
//! * **Bounded churn.** A cooldown (default 5 min) prevents a burst of
//!   searches against a busy session directory from re-spawning the indexer
//!   every time a file changes. The active-writer window inside the indexer
//!   already defers files that are still being written.
//! * **Opt-out.** `CASS_AUTO_REFRESH=0` disables the behaviour entirely.
//! * **A child that dies is not respawned blindly.** The child inherits the
//!   spawner's cgroup (an agent session capped at 16 GB, say) and can be
//!   OOM-killed, stall-aborted, or crash without ever advancing
//!   `last_indexed_at`. On 2026-09-03 that turned every stale read of a
//!   10 GB archive into another hour-long doomed run. So the next evaluation
//!   compares the index watermark with the last spawn: a spawn the index did
//!   not outlive counts as a failure, failures space the next attempt out
//!   (1 h, then 6 h), and three in a row trip the breaker — no more
//!   auto-spawns until *any* run completes and advances the watermark. The
//!   state is surfaced by `search --robot-meta`, `status --json`,
//!   `schedule status`, and `doctor`.
//!
//! The child is `cass index --background --json --no-progress-events`, which
//! applies `nice`/`ionice` to itself before doing any work.

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

/// Default minimum spacing between two auto-spawned catch-up runs.
pub const DEFAULT_COOLDOWN_SECS: u64 = 300;

/// Spacing after an auto-spawned catch-up ended without advancing the index,
/// indexed by `consecutive_failures - 1`, measured from the moment the failure
/// was detected. The last entry also covers any longer streak below the trip
/// threshold.
pub const FAILURE_BACKOFF_SECS: [u64; 2] = [3_600, 6 * 3_600];

/// Consecutive failed catch-ups that trip the breaker: no auto-spawn until a
/// run (foreground `cass index`, scheduled, or a later manual full run)
/// completes and advances `last_indexed_at`.
pub const FAILURE_TRIP_THRESHOLD: u32 = 3;

/// Default nice value applied by `cass index --background`.
pub const DEFAULT_BACKGROUND_NICE: i32 = 15;

/// Default ionice class applied by `cass index --background` (3 = idle).
pub const DEFAULT_BACKGROUND_IONICE_CLASS: u32 = 3;

/// Resolved auto-refresh policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AutoRefreshPolicy {
    pub enabled: bool,
    pub cooldown: Duration,
}

impl Default for AutoRefreshPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            cooldown: Duration::from_secs(DEFAULT_COOLDOWN_SECS),
        }
    }
}

impl AutoRefreshPolicy {
    /// `CASS_AUTO_REFRESH` (default on; `0`/`false`/`no`/`off` disables) and
    /// `CASS_AUTO_REFRESH_COOLDOWN_SECS` (default 300).
    pub fn from_env() -> Self {
        let enabled = dotenvy::var("CASS_AUTO_REFRESH")
            .map(|v| {
                !matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "0" | "false" | "no" | "off"
                )
            })
            .unwrap_or(true);
        let cooldown = dotenvy::var("CASS_AUTO_REFRESH_COOLDOWN_SECS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(DEFAULT_COOLDOWN_SECS));
        Self { enabled, cooldown }
    }
}

/// What happened when a read path asked for a catch-up.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum AutoRefreshOutcome {
    /// A detached `cass index --background` child was started.
    Spawned { pid: u32, reason: String },
    /// `CASS_AUTO_REFRESH=0`.
    Disabled,
    /// Another cass index run already holds the data-dir index lock.
    IndexRunActive,
    /// A catch-up ran too recently.
    Cooldown { remaining_secs: u64 },
    /// Another process is spawning right now.
    GuardBusy,
    /// Could not start the child process.
    SpawnFailed { error: String },
    /// The previous auto-spawned catch-up ended without advancing the index;
    /// the next attempt waits out an escalated spacing.
    BackedOff {
        consecutive_failures: u32,
        remaining_secs: u64,
        detail: String,
    },
    /// [`FAILURE_TRIP_THRESHOLD`] catch-ups in a row failed; auto-spawn stays
    /// off until a run completes and advances `last_indexed_at`.
    Tripped {
        consecutive_failures: u32,
        detail: String,
    },
}

impl AutoRefreshOutcome {
    pub fn spawned(&self) -> bool {
        matches!(self, Self::Spawned { .. })
    }
}

/// Durable record of the last auto-spawn, used for the cooldown and surfaced
/// by `cass status`/`schedule status`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoRefreshState {
    pub last_spawn_ms: i64,
    pub last_pid: u32,
    pub last_reason: String,
    /// Auto-spawned catch-ups in a row that ended without advancing
    /// `last_indexed_at`. Reset to zero as soon as any run advances it.
    #[serde(default)]
    pub consecutive_failures: u32,
    /// The spawn (`last_spawn_ms`) whose failure has already been counted, so
    /// repeated reads between attempts do not inflate the streak.
    #[serde(default)]
    pub failure_counted_for_spawn_ms: i64,
    /// When the most recent failure was detected; the backoff clock starts
    /// here, not at the spawn (the doomed child may itself have run for an
    /// hour).
    #[serde(default)]
    pub last_failure_detected_ms: i64,
    /// What the most recent failure looked like, for `status`/`doctor`.
    #[serde(default)]
    pub last_failure: Option<String>,
}

pub fn state_path(data_dir: &Path) -> PathBuf {
    data_dir.join("auto-refresh-state.json")
}

pub fn guard_path(data_dir: &Path) -> PathBuf {
    data_dir.join("auto-refresh.spawn.lock")
}

/// stdout+stderr of the most recent detached child (truncated per spawn).
pub fn log_path(data_dir: &Path) -> PathBuf {
    data_dir.join("auto-refresh.log")
}

pub fn load_state(data_dir: &Path) -> Option<AutoRefreshState> {
    let raw = std::fs::read_to_string(state_path(data_dir)).ok()?;
    serde_json::from_str(&raw).ok()
}

fn save_state(data_dir: &Path, state: &AutoRefreshState) -> std::io::Result<()> {
    let path = state_path(data_dir);
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_vec_pretty(state).map_err(std::io::Error::other)?;
    let mut file = File::create(&tmp)?;
    file.write_all(&body)?;
    file.sync_all()?;
    std::fs::rename(&tmp, &path)
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn rfc3339(ms: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_else(|| ms.to_string())
}

/// The index watermark carried by a freshness block: `last_indexed_at` is an
/// RFC 3339 string in `cass status`/`--robot-meta` output; a bare millisecond
/// integer is accepted too.
pub fn last_indexed_at_ms_from_freshness(index_freshness: &serde_json::Value) -> Option<i64> {
    let value = index_freshness.get("last_indexed_at")?;
    if let Some(ms) = value.as_i64() {
        return Some(ms);
    }
    chrono::DateTime::parse_from_rfc3339(value.as_str()?)
        .ok()
        .map(|t| t.timestamp_millis())
}

/// What the breaker says about spawning right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BreakerVerdict {
    Closed,
    BackedOff { remaining_secs: u64 },
    Tripped,
}

/// Judge the previous auto-spawn against the index watermark the caller just
/// observed, updating the streak in `state` (persist it when it changed).
///
/// A spawn the watermark did not outlive is a failed catch-up: the child was
/// OOM-killed, stall-aborted, or crashed before `last_indexed_at` was written.
/// Each spawn is counted once. Any watermark at or past the spawn — that child
/// finishing, a foreground `cass index`, the scheduler — resets the streak.
/// `None` for the watermark means the caller cannot judge; nothing changes.
pub fn judge_previous_spawn(
    state: &mut AutoRefreshState,
    last_indexed_at_ms: Option<i64>,
    now_ms: i64,
    child_log: &Path,
) -> BreakerVerdict {
    if state.last_spawn_ms <= 0 {
        return BreakerVerdict::Closed;
    }
    let Some(watermark) = last_indexed_at_ms else {
        return BreakerVerdict::Closed;
    };
    if watermark >= state.last_spawn_ms {
        state.consecutive_failures = 0;
        state.last_failure = None;
        state.last_failure_detected_ms = 0;
        return BreakerVerdict::Closed;
    }
    if state.failure_counted_for_spawn_ms != state.last_spawn_ms {
        state.consecutive_failures = state.consecutive_failures.saturating_add(1);
        state.failure_counted_for_spawn_ms = state.last_spawn_ms;
        state.last_failure_detected_ms = now_ms;
        state.last_failure = Some(format!(
            "background catch-up pid {} spawned {} ({}) ended without advancing the index \
             (last_indexed_at {}); its output is in {}",
            state.last_pid,
            rfc3339(state.last_spawn_ms),
            state.last_reason,
            rfc3339(watermark),
            child_log.display()
        ));
    }
    if state.consecutive_failures >= FAILURE_TRIP_THRESHOLD {
        return BreakerVerdict::Tripped;
    }
    let step = usize::try_from(state.consecutive_failures.saturating_sub(1)).unwrap_or(usize::MAX);
    let backoff_ms = i64::try_from(FAILURE_BACKOFF_SECS[step.min(FAILURE_BACKOFF_SECS.len() - 1)])
        .unwrap_or(i64::MAX)
        .saturating_mul(1000);
    let elapsed_ms = now_ms.saturating_sub(state.last_failure_detected_ms);
    if elapsed_ms >= backoff_ms {
        return BreakerVerdict::Closed;
    }
    let remaining_ms = backoff_ms - elapsed_ms;
    BreakerVerdict::BackedOff {
        remaining_secs: u64::try_from((remaining_ms + 999) / 1000).unwrap_or(u64::MAX),
    }
}

/// Decide whether a freshness snapshot (the `index_freshness` block that
/// `cass search --robot-meta` and `cass status` emit) warrants a catch-up.
/// Returns the reason string that will be recorded, or `None`.
pub fn catch_up_reason(index_freshness: &serde_json::Value) -> Option<&'static str> {
    let flag = |key: &str| {
        index_freshness
            .get(key)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    };
    if !flag("exists") {
        // A missing index is a first-run problem for `cass index --full`,
        // not something to paper over from a read path.
        return None;
    }
    if flag("rebuilding") {
        return None;
    }
    if flag("partial") {
        return Some("index-partial");
    }
    if flag("stale") {
        return Some("index-stale");
    }
    let pending = index_freshness
        .get("pending_sessions")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    if pending > 0 {
        return Some("pending-sessions");
    }
    None
}

/// Cooldown check factored out for tests.
pub fn cooldown_remaining(
    state: Option<&AutoRefreshState>,
    cooldown: Duration,
    now_ms: i64,
) -> Option<u64> {
    let last = state?.last_spawn_ms;
    if last <= 0 {
        return None;
    }
    let elapsed_ms = now_ms.saturating_sub(last);
    let cooldown_ms = i64::try_from(cooldown.as_millis()).unwrap_or(i64::MAX);
    if elapsed_ms >= cooldown_ms {
        None
    } else {
        let remaining_ms = cooldown_ms - elapsed_ms;
        Some(((remaining_ms + 999) / 1000).max(1) as u64)
    }
}

/// The exact argv (after the binary) used for a detached catch-up child.
/// Public so the scheduler and tests can pin the contract.
pub fn background_index_args(data_dir: &Path, db_path: &Path, full: bool) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        OsString::from("--db"),
        db_path.as_os_str().to_os_string(),
        OsString::from("--color=never"),
        OsString::from("index"),
        OsString::from("--background"),
        OsString::from("--json"),
        OsString::from("--no-progress-events"),
        OsString::from("--data-dir"),
        data_dir.as_os_str().to_os_string(),
    ];
    if full {
        args.push(OsString::from("--full"));
    }
    args
}

/// Build the detached child command. stdio goes to `auto-refresh.log`
/// (truncated) so a misbehaving catch-up leaves evidence; the child is placed
/// in its own process group so closing the terminal that ran the original
/// `cass search` does not HUP it.
fn build_command(binary: &Path, data_dir: &Path, db_path: &Path, full: bool) -> Command {
    let mut cmd = build_detached_command(
        binary,
        &background_index_args(data_dir, db_path, full),
        &log_path(data_dir),
    );
    cmd.env("CASS_INDEX_NO_PROGRESS_EVENTS", "1");
    cmd
}

/// Log file for the detached analytics rollup rebuild the TUI spawns (GH #395).
pub fn analytics_rebuild_log_path(data_dir: &Path) -> PathBuf {
    data_dir.join("analytics-rebuild.log")
}

/// The exact argv (after the binary) for a detached analytics rollup rebuild.
///
/// Track A only: that is what the TUI dashboard reads, and Track B's token
/// ledger has its own scheduled path. `--json` keeps the child's stdout a
/// single parseable receipt in the log file.
pub fn background_analytics_rebuild_args(data_dir: &Path, db_path: &Path) -> Vec<OsString> {
    vec![
        OsString::from("--db"),
        db_path.as_os_str().to_os_string(),
        OsString::from("--color=never"),
        OsString::from("analytics"),
        OsString::from("rebuild"),
        OsString::from("--json"),
        OsString::from("--data-dir"),
        data_dir.as_os_str().to_os_string(),
    ]
}

/// Spawn a detached `cass analytics rebuild` for `db_path` (GH #395).
///
/// The TUI used to run the full rollup rebuild in-process on its effects
/// thread when the dashboard found no rollups; on a multi-million-message
/// archive that is a multi-GB, multi-minute allocation storm that froze the
/// UI. The child inherits the same detachment as the index catch-up (own
/// process group, stdio to a log, no nested auto-refresh) and is fire-and-
/// forget: the caller reports "rebuilding in the background" and reloads the
/// dashboard later. Returns the child pid.
///
/// # Errors
///
/// Returns a message when the current executable cannot be resolved or the
/// child cannot be spawned.
pub fn spawn_detached_analytics_rebuild(data_dir: &Path, db_path: &Path) -> Result<u32, String> {
    let binary = std::env::current_exe()
        .map_err(|error| format!("cannot resolve the running cass binary: {error}"))?;
    let mut cmd = build_detached_command(
        &binary,
        &background_analytics_rebuild_args(data_dir, db_path),
        &analytics_rebuild_log_path(data_dir),
    );
    let mut child = cmd
        .spawn()
        .map_err(|error| format!("cannot spawn detached analytics rebuild: {error}"))?;
    let pid = child.id();
    info!(pid, db_path = %db_path.display(), "spawned detached analytics rollup rebuild");
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(pid)
}

/// Shared shape of every detached cass child: stdio to `log`, own process
/// group, and `CASS_AUTO_REFRESH=0` so a child never spawns children.
fn build_detached_command(binary: &Path, args: &[OsString], log: &Path) -> Command {
    // ubs:ignore — `binary` is always `std::env::current_exe()` (the running
    // cass), never user-supplied input; same pattern as daemon auto-spawn.
    let mut cmd = Command::new(binary);
    cmd.args(args);
    // The child never searches, but be explicit: a catch-up must not spawn
    // catch-ups.
    cmd.env("CASS_AUTO_REFRESH", "0");
    cmd.stdin(Stdio::null());
    match File::create(log) {
        Ok(log) => {
            match log.try_clone() {
                Ok(err_log) => {
                    cmd.stderr(Stdio::from(err_log));
                }
                Err(_) => {
                    cmd.stderr(Stdio::null());
                }
            }
            cmd.stdout(Stdio::from(log));
        }
        Err(_) => {
            cmd.stdout(Stdio::null());
            cmd.stderr(Stdio::null());
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    cmd
}

/// Spawn a detached incremental catch-up if policy, locks, and cooldown allow.
///
/// `reason` is recorded in the state file and returned in the outcome so
/// robot callers can see *why* a child was started.
pub fn maybe_spawn_background_index_refresh(
    data_dir: &Path,
    db_path: &Path,
    reason: &str,
    last_indexed_at_ms: Option<i64>,
) -> AutoRefreshOutcome {
    maybe_spawn_with_policy(
        data_dir,
        db_path,
        reason,
        false,
        last_indexed_at_ms,
        AutoRefreshPolicy::from_env(),
    )
}

/// Same as [`maybe_spawn_background_index_refresh`] but for a *full* pass
/// (scheduler nightly job / daemon tick with an explicit request).
pub fn maybe_spawn_background_full_index(
    data_dir: &Path,
    db_path: &Path,
    reason: &str,
) -> AutoRefreshOutcome {
    // The scheduler drives full passes on its own cadence; it does not hand
    // over a watermark, so the breaker cannot (and does not) judge them.
    maybe_spawn_with_policy(
        data_dir,
        db_path,
        reason,
        true,
        None,
        AutoRefreshPolicy::from_env(),
    )
}

pub fn maybe_spawn_with_policy(
    data_dir: &Path,
    db_path: &Path,
    reason: &str,
    full: bool,
    last_indexed_at_ms: Option<i64>,
    policy: AutoRefreshPolicy,
) -> AutoRefreshOutcome {
    if !policy.enabled {
        debug!(reason, "auto-refresh disabled via CASS_AUTO_REFRESH");
        return AutoRefreshOutcome::Disabled;
    }
    if crate::search::asset_state::read_search_maintenance_snapshot(data_dir).active {
        debug!(reason, "auto-refresh skipped: index run already active");
        return AutoRefreshOutcome::IndexRunActive;
    }
    if let Err(error) = std::fs::create_dir_all(data_dir) {
        return AutoRefreshOutcome::SpawnFailed {
            error: format!("cannot create data dir {}: {error}", data_dir.display()),
        };
    }

    let guard = match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(guard_path(data_dir))
    {
        Ok(file) => file,
        Err(error) => {
            return AutoRefreshOutcome::SpawnFailed {
                error: format!("cannot open spawn guard: {error}"),
            };
        }
    };
    if guard.try_lock_exclusive().is_err() {
        debug!(reason, "auto-refresh skipped: another process is spawning");
        return AutoRefreshOutcome::GuardBusy;
    }

    let now = now_ms();
    let mut state = load_state(data_dir);
    if let Some(current) = state.as_mut() {
        let before = current.clone();
        let verdict = judge_previous_spawn(current, last_indexed_at_ms, now, &log_path(data_dir));
        if *current != before
            && let Err(error) = save_state(data_dir, current)
        {
            warn!(error = %error, "failed to persist auto-refresh breaker state");
        }
        let detail = current.last_failure.clone().unwrap_or_default();
        match verdict {
            BreakerVerdict::Closed => {}
            BreakerVerdict::BackedOff { remaining_secs } => {
                warn!(
                    reason,
                    consecutive_failures = current.consecutive_failures,
                    remaining_secs,
                    detail,
                    "auto-refresh backed off: the previous background catch-up did not advance the index"
                );
                let _ = FileExt::unlock(&guard);
                return AutoRefreshOutcome::BackedOff {
                    consecutive_failures: current.consecutive_failures,
                    remaining_secs,
                    detail,
                };
            }
            BreakerVerdict::Tripped => {
                warn!(
                    reason,
                    consecutive_failures = current.consecutive_failures,
                    detail,
                    "auto-refresh tripped: run `cass index` in the foreground; auto-spawn resumes once a run completes"
                );
                let _ = FileExt::unlock(&guard);
                return AutoRefreshOutcome::Tripped {
                    consecutive_failures: current.consecutive_failures,
                    detail,
                };
            }
        }
    }
    if let Some(remaining_secs) = cooldown_remaining(state.as_ref(), policy.cooldown, now) {
        debug!(reason, remaining_secs, "auto-refresh skipped: cooldown");
        let _ = FileExt::unlock(&guard);
        return AutoRefreshOutcome::Cooldown { remaining_secs };
    }

    let binary = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            let _ = FileExt::unlock(&guard);
            return AutoRefreshOutcome::SpawnFailed {
                error: format!("cannot resolve cass binary: {error}"),
            };
        }
    };

    let mut cmd = build_command(&binary, data_dir, db_path, full);
    let outcome = match cmd.spawn() {
        Ok(mut child) => {
            let pid = child.id();
            info!(
                pid,
                reason,
                full,
                data_dir = %data_dir.display(),
                "spawned detached background index catch-up"
            );
            // The streak survives the spawn: a child that dies again is the
            // next consecutive failure, not the first.
            let new_state = AutoRefreshState {
                last_spawn_ms: now,
                last_pid: pid,
                last_reason: reason.to_string(),
                ..state.clone().unwrap_or_default()
            };
            if let Err(error) = save_state(data_dir, &new_state) {
                warn!(error = %error, "failed to persist auto-refresh state");
            }
            // Reap so a long-lived parent (TUI, daemon) never accumulates
            // zombies; a short-lived CLI parent simply exits and init adopts
            // the child.
            // ubs:ignore — detached reaper thread intentionally waits on the
            // spawned child to avoid zombies in long-lived parents.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            AutoRefreshOutcome::Spawned {
                pid,
                reason: reason.to_string(),
            }
        }
        Err(error) => {
            warn!(error = %error, "failed to spawn background index catch-up");
            AutoRefreshOutcome::SpawnFailed {
                error: error.to_string(),
            }
        }
    };
    let _ = FileExt::unlock(&guard);
    outcome
}

/// Lower the *current* process's scheduling priority. Called by
/// `cass index --background` before any work starts. Returns what was
/// applied for logging.
pub fn apply_background_priority() -> BackgroundPriority {
    let nice = dotenvy::var("CASS_BACKGROUND_NICE")
        .ok()
        .and_then(|v| v.trim().parse::<i32>().ok())
        .map(|n| n.clamp(0, 19))
        .unwrap_or(DEFAULT_BACKGROUND_NICE);
    let ionice_class = dotenvy::var("CASS_BACKGROUND_IONICE_CLASS")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .map(|c| c.min(3))
        .unwrap_or(DEFAULT_BACKGROUND_IONICE_CLASS);
    #[cfg(unix)]
    {
        let monitor = crate::daemon::resource::ResourceMonitor::new();
        let nice_applied = monitor.apply_nice(nice);
        let ionice_applied = monitor.apply_ionice(ionice_class);
        BackgroundPriority {
            nice,
            nice_applied,
            ionice_class,
            ionice_applied,
        }
    }
    #[cfg(not(unix))]
    {
        BackgroundPriority {
            nice,
            nice_applied: false,
            ionice_class,
            ionice_applied: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
pub struct BackgroundPriority {
    pub nice: i32,
    pub nice_applied: bool,
    pub ionice_class: u32,
    pub ionice_applied: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_refresh_is_enabled_by_default() {
        assert!(AutoRefreshPolicy::default().enabled);
    }

    #[test]
    fn a_spawn_the_watermark_did_not_outlive_counts_once_and_backs_off() {
        let log = Path::new("/scratch/auto-refresh.log");
        let mut state = AutoRefreshState {
            last_spawn_ms: 1_000_000,
            last_pid: 42,
            last_reason: "index-stale".to_string(),
            ..AutoRefreshState::default()
        };
        let first = judge_previous_spawn(&mut state, Some(500_000), 5_000_000, log);
        assert_eq!(
            first,
            BreakerVerdict::BackedOff {
                remaining_secs: FAILURE_BACKOFF_SECS[0]
            }
        );
        assert_eq!(state.consecutive_failures, 1);
        assert_eq!(state.last_failure_detected_ms, 5_000_000);
        // Read again ten seconds later at the same watermark: the same
        // failure, not a second one.
        let again = judge_previous_spawn(&mut state, Some(500_000), 5_010_000, log);
        assert_eq!(
            again,
            BreakerVerdict::BackedOff {
                remaining_secs: FAILURE_BACKOFF_SECS[0] - 10
            }
        );
        assert_eq!(state.consecutive_failures, 1);
        let detail = state.last_failure.clone().expect("failure detail");
        assert!(
            detail.contains("pid 42")
                && detail.contains("index-stale")
                && detail.contains("auto-refresh.log"),
            "{detail}"
        );
        // Once the backoff elapses a new attempt is allowed; the streak stays
        // so a second dead child counts as failure two.
        let elapsed = 5_000_000 + i64::try_from(FAILURE_BACKOFF_SECS[0]).unwrap() * 1000;
        assert_eq!(
            judge_previous_spawn(&mut state, Some(500_000), elapsed, log),
            BreakerVerdict::Closed
        );
        assert_eq!(state.consecutive_failures, 1);
    }

    #[test]
    fn three_failed_spawns_trip_until_a_run_advances_the_watermark() {
        let log = Path::new("/scratch/auto-refresh.log");
        let mut state = AutoRefreshState::default();
        let mut now = 10_000_000_i64;
        for spawn in 1..=FAILURE_TRIP_THRESHOLD {
            state.last_spawn_ms = now;
            state.last_pid = spawn;
            // The child ran for an hour and died without writing the watermark.
            now += 3_600_000;
            let verdict = judge_previous_spawn(&mut state, Some(1), now, log);
            assert_eq!(state.consecutive_failures, spawn);
            if spawn < FAILURE_TRIP_THRESHOLD {
                assert!(
                    matches!(verdict, BreakerVerdict::BackedOff { .. }),
                    "{verdict:?}"
                );
                now += 7 * 3_600_000;
            } else {
                assert_eq!(verdict, BreakerVerdict::Tripped);
            }
        }
        // Still tripped a week later at the same watermark.
        assert_eq!(
            judge_previous_spawn(&mut state, Some(1), now + 7 * 86_400_000, log),
            BreakerVerdict::Tripped
        );
        // Any completed run resets the breaker.
        assert_eq!(
            judge_previous_spawn(&mut state, Some(now + 1), now + 2, log),
            BreakerVerdict::Closed
        );
        assert_eq!(state.consecutive_failures, 0);
        assert!(state.last_failure.is_none());
        assert_eq!(state.last_failure_detected_ms, 0);
    }

    #[test]
    fn breaker_does_not_judge_without_a_watermark_or_a_prior_spawn() {
        let log = Path::new("/scratch/auto-refresh.log");
        let mut fresh = AutoRefreshState::default();
        assert_eq!(
            judge_previous_spawn(&mut fresh, Some(1), 2, log),
            BreakerVerdict::Closed
        );
        let mut spawned = AutoRefreshState {
            last_spawn_ms: 5,
            ..AutoRefreshState::default()
        };
        assert_eq!(
            judge_previous_spawn(&mut spawned, None, 6, log),
            BreakerVerdict::Closed
        );
        assert_eq!(spawned.consecutive_failures, 0);
    }

    #[test]
    fn state_files_written_before_the_breaker_still_load() {
        let legacy = r#"{"last_spawn_ms": 1788455403951, "last_pid": 897803, "last_reason": "index-stale"}"#;
        let state: AutoRefreshState = serde_json::from_str(legacy).expect("legacy state parses");
        assert_eq!(state.last_pid, 897803);
        assert_eq!(state.consecutive_failures, 0);
        assert_eq!(state.failure_counted_for_spawn_ms, 0);
        assert!(state.last_failure.is_none());
    }

    #[test]
    fn last_indexed_at_ms_accepts_rfc3339_and_millis() {
        let stamp = "2026-08-14T12:00:00+00:00";
        let expected = chrono::DateTime::parse_from_rfc3339(stamp)
            .unwrap()
            .timestamp_millis();
        let rfc = serde_json::json!({ "last_indexed_at": stamp });
        assert_eq!(last_indexed_at_ms_from_freshness(&rfc), Some(expected));
        let millis = serde_json::json!({ "last_indexed_at": expected });
        assert_eq!(last_indexed_at_ms_from_freshness(&millis), Some(expected));
        let missing = serde_json::json!({ "last_indexed_at": null });
        assert_eq!(last_indexed_at_ms_from_freshness(&missing), None);
    }

    #[test]
    fn catch_up_reason_prefers_partial_then_stale_then_pending() {
        let base = serde_json::json!({"exists": true, "rebuilding": false});
        assert_eq!(catch_up_reason(&base), None);

        let partial = serde_json::json!({"exists": true, "partial": true, "stale": true});
        assert_eq!(catch_up_reason(&partial), Some("index-partial"));

        let stale = serde_json::json!({"exists": true, "stale": true, "pending_sessions": 3});
        assert_eq!(catch_up_reason(&stale), Some("index-stale"));

        let pending = serde_json::json!({"exists": true, "stale": false, "pending_sessions": 3});
        assert_eq!(catch_up_reason(&pending), Some("pending-sessions"));
    }

    #[test]
    fn catch_up_reason_never_fires_for_missing_or_rebuilding_index() {
        let missing = serde_json::json!({"exists": false, "stale": true});
        assert_eq!(catch_up_reason(&missing), None);
        let rebuilding = serde_json::json!({"exists": true, "rebuilding": true, "stale": true});
        assert_eq!(catch_up_reason(&rebuilding), None);
    }

    #[test]
    fn cooldown_remaining_rounds_up_and_expires() {
        let cooldown = Duration::from_secs(300);
        assert_eq!(cooldown_remaining(None, cooldown, 1_000_000), None);
        let state = AutoRefreshState {
            last_spawn_ms: 1_000_000,
            last_pid: 1,
            last_reason: "x".into(),
        };
        assert_eq!(
            cooldown_remaining(Some(&state), cooldown, 1_000_000 + 1_500),
            Some(299)
        );
        assert_eq!(
            cooldown_remaining(Some(&state), cooldown, 1_000_000 + 299_999),
            Some(1)
        );
        assert_eq!(
            cooldown_remaining(Some(&state), cooldown, 1_000_000 + 300_000),
            None
        );
        let zero = AutoRefreshState::default();
        assert_eq!(cooldown_remaining(Some(&zero), cooldown, 5), None);
    }

    #[test]
    fn background_index_args_pin_the_child_contract() {
        let args = background_index_args(Path::new("/d"), Path::new("/d/agent_search.db"), false);
        let rendered: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            rendered,
            vec![
                "--db",
                "/d/agent_search.db",
                "--color=never",
                "index",
                "--background",
                "--json",
                "--no-progress-events",
                "--data-dir",
                "/d",
            ]
        );
        let full = background_index_args(Path::new("/d"), Path::new("/d/x.db"), true);
        assert_eq!(full.last().unwrap(), "--full");
    }

    #[test]
    fn disabled_policy_short_circuits_before_touching_disk() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("never-created");
        let outcome = maybe_spawn_with_policy(
            &missing,
            &missing.join("agent_search.db"),
            "test",
            false,
            None,
            AutoRefreshPolicy {
                enabled: false,
                cooldown: Duration::from_secs(1),
            },
        );
        assert_eq!(outcome, AutoRefreshOutcome::Disabled);
        assert!(!missing.exists());
    }

    #[test]
    fn cooldown_blocks_a_second_spawn_without_starting_a_process() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path();
        save_state(
            data_dir,
            &AutoRefreshState {
                last_spawn_ms: now_ms(),
                last_pid: 42,
                last_reason: "seed".into(),
            },
        )
        .unwrap();
        let outcome = maybe_spawn_with_policy(
            data_dir,
            &data_dir.join("agent_search.db"),
            "test",
            false,
            None,
            AutoRefreshPolicy {
                enabled: true,
                cooldown: Duration::from_secs(3600),
            },
        );
        assert!(
            matches!(outcome, AutoRefreshOutcome::Cooldown { .. }),
            "{outcome:?}"
        );
        assert!(
            !log_path(data_dir).exists(),
            "no child must have been spawned"
        );
    }

    #[test]
    fn guard_busy_when_another_spawner_holds_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path();
        let holder = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(guard_path(data_dir))
            .unwrap();
        holder.lock_exclusive().unwrap();
        let outcome = maybe_spawn_with_policy(
            data_dir,
            &data_dir.join("agent_search.db"),
            "test",
            false,
            None,
            AutoRefreshPolicy {
                enabled: true,
                ..AutoRefreshPolicy::default()
            },
        );
        assert_eq!(outcome, AutoRefreshOutcome::GuardBusy);
        let _ = FileExt::unlock(&holder);
    }

    #[test]
    fn state_round_trips_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let state = AutoRefreshState {
            last_spawn_ms: 123,
            last_pid: 7,
            last_reason: "index-stale".into(),
        };
        save_state(dir.path(), &state).unwrap();
        let loaded = load_state(dir.path()).unwrap();
        assert_eq!(loaded.last_spawn_ms, 123);
        assert_eq!(loaded.last_pid, 7);
        assert_eq!(loaded.last_reason, "index-stale");
        assert!(!state_path(dir.path()).with_extension("json.tmp").exists());
    }
}
