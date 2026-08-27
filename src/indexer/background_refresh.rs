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
}

impl AutoRefreshOutcome {
    pub fn spawned(&self) -> bool {
        matches!(self, Self::Spawned { .. })
    }
}

/// Durable record of the last auto-spawn, used for the cooldown and surfaced
/// by `cass status`/`schedule status`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AutoRefreshState {
    pub last_spawn_ms: i64,
    pub last_pid: u32,
    pub last_reason: String,
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
    // ubs:ignore — `binary` is always `std::env::current_exe()` (the running
    // cass), never user-supplied input; same pattern as daemon auto-spawn.
    let mut cmd = Command::new(binary);
    cmd.args(background_index_args(data_dir, db_path, full));
    cmd.env("CASS_INDEX_NO_PROGRESS_EVENTS", "1");
    // The child never searches, but be explicit: a catch-up must not spawn
    // catch-ups.
    cmd.env("CASS_AUTO_REFRESH", "0");
    cmd.stdin(Stdio::null());
    match File::create(log_path(data_dir)) {
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
) -> AutoRefreshOutcome {
    maybe_spawn_with_policy(
        data_dir,
        db_path,
        reason,
        false,
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
    maybe_spawn_with_policy(
        data_dir,
        db_path,
        reason,
        true,
        AutoRefreshPolicy::from_env(),
    )
}

pub fn maybe_spawn_with_policy(
    data_dir: &Path,
    db_path: &Path,
    reason: &str,
    full: bool,
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
    let state = load_state(data_dir);
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
            let new_state = AutoRefreshState {
                last_spawn_ms: now,
                last_pid: pid,
                last_reason: reason.to_string(),
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
