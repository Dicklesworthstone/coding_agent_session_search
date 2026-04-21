//! Machine-responsiveness governor for the indexing pipeline.
//!
//! The indexer can easily saturate every core of a big host. That is normally
//! fine on a dedicated build box, but on a shared workstation it makes
//! interactive shells, editors, and a foreground `cass search` feel dead
//! while a rebuild runs. This module provides a lightweight, hysteresis-aware
//! governor that publishes a *capacity factor* (an integer percentage in
//! `[min_capacity_pct, 100]`). Callers consult [`effective_worker_count`]
//! before committing to a worker fan-out, and get back a bounded count that
//! respects the current system load.
//!
//! Design goals (aligned with bead `coding_agent_session_search-d2qix`):
//!
//! * **Conservative by default.** Defaults never grow past caller-requested
//!   fan-out; they only shrink when the box is already under pressure.
//! * **Explainable.** Thresholds live in named env vars and the full decision
//!   history is queryable via [`telemetry_snapshot`].
//! * **Non-oscillating.** Shrink is immediate so responsiveness recovers
//!   fast, but growth back to full capacity requires multiple consecutive
//!   healthy ticks (hysteresis) so flapping loads do not flap the worker count.
//! * **Opt-out.** `CASS_RESPONSIVENESS_DISABLE=1` pins capacity to 100% for
//!   before/after comparison runs and sandboxed environments.
//!
//! Signals read on Linux:
//!
//! * `/proc/loadavg` first field (1-minute load average), compared against
//!   the number of logical CPUs.
//! * `/proc/pressure/cpu` `some avg10` (percent of wall-time some task was
//!   delayed on CPU in the last 10s). This is the best single "how
//!   unresponsive does the machine feel" signal available from the kernel.
//!
//! On non-Linux platforms the reader always reports healthy, so the governor
//! reduces to a no-op.

use std::collections::VecDeque;
use std::sync::{
    Arc, LazyLock, Mutex,
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

/// Lower bound for the published capacity, as a percentage of the caller's
/// desired fan-out. Never shrinks below this regardless of signals.
const DEFAULT_MIN_CAPACITY_PCT: u32 = 25;

/// Loadavg / ncpu threshold above which the governor shrinks by one step.
const DEFAULT_MAX_LOAD_PER_CORE: f32 = 1.25;

/// Loadavg / ncpu threshold above which the governor shrinks *hard* to the
/// floor.
const DEFAULT_SEVERE_LOAD_PER_CORE: f32 = 1.75;

/// PSI cpu `some avg10` threshold above which the governor shrinks by one step.
const DEFAULT_MAX_PSI_AVG10: f32 = 20.0;

/// PSI cpu `some avg10` threshold above which the governor shrinks to the
/// floor.
const DEFAULT_SEVERE_PSI_AVG10: f32 = 40.0;

/// Number of consecutive healthy ticks required before capacity is grown
/// back toward 100%. Prevents flapping under bursty load.
const DEFAULT_GROWTH_CONSECUTIVE_HEALTHY_TICKS: u32 = 3;

/// Background sampling interval. Shorter = more responsive throttling, but
/// more wasted wakeups on an idle box.
const DEFAULT_TICK_SECS: u64 = 2;

/// Maximum number of decisions retained in the telemetry ring buffer.
/// Sized so the structure stays under 16 KB and covers ~4 minutes of
/// history at the default 2-second tick.
const TELEMETRY_DECISION_HISTORY: usize = 128;

#[derive(Clone, Copy, Debug)]
pub(crate) struct GovernorConfig {
    pub min_capacity_pct: u32,
    pub max_load_per_core: f32,
    pub severe_load_per_core: f32,
    pub max_psi_avg10: f32,
    pub severe_psi_avg10: f32,
    pub growth_consecutive_healthy_ticks: u32,
    pub tick: Duration,
    pub disabled: bool,
}

impl GovernorConfig {
    pub fn from_env() -> Self {
        let min_capacity_pct = env_u32("CASS_RESPONSIVENESS_MIN_CAPACITY_PCT")
            .map(|v| v.clamp(10, 100))
            .unwrap_or(DEFAULT_MIN_CAPACITY_PCT);
        let max_load_per_core =
            env_f32("CASS_RESPONSIVENESS_MAX_LOAD_PER_CORE").unwrap_or(DEFAULT_MAX_LOAD_PER_CORE);
        let severe_load_per_core = env_f32("CASS_RESPONSIVENESS_SEVERE_LOAD_PER_CORE")
            .unwrap_or(DEFAULT_SEVERE_LOAD_PER_CORE);
        let max_psi_avg10 =
            env_f32("CASS_RESPONSIVENESS_MAX_PSI_AVG10").unwrap_or(DEFAULT_MAX_PSI_AVG10);
        let severe_psi_avg10 =
            env_f32("CASS_RESPONSIVENESS_SEVERE_PSI_AVG10").unwrap_or(DEFAULT_SEVERE_PSI_AVG10);
        let growth_consecutive_healthy_ticks = env_u32("CASS_RESPONSIVENESS_GROWTH_TICKS")
            .unwrap_or(DEFAULT_GROWTH_CONSECUTIVE_HEALTHY_TICKS);
        let tick_secs = env_u32("CASS_RESPONSIVENESS_TICK_SECS")
            .map(|v| v as u64)
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_TICK_SECS);
        let disabled = env_bool_truthy("CASS_RESPONSIVENESS_DISABLE");
        Self {
            min_capacity_pct,
            max_load_per_core,
            severe_load_per_core,
            max_psi_avg10,
            severe_psi_avg10,
            growth_consecutive_healthy_ticks,
            tick: Duration::from_secs(tick_secs),
            disabled,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
pub(crate) struct HealthSnapshot {
    /// Load average (1-minute) divided by the number of CPUs. `None` on
    /// platforms where the signal is unavailable.
    pub load_per_core: Option<f32>,
    /// PSI `some avg10` for cpu. `None` when `/proc/pressure/cpu` isn't
    /// readable (older kernels, non-Linux).
    pub psi_cpu_some_avg10: Option<f32>,
}

impl HealthSnapshot {
    /// Returns true when either signal is above the severe threshold.
    pub fn is_severe(&self, cfg: &GovernorConfig) -> bool {
        self.load_per_core
            .is_some_and(|v| v > cfg.severe_load_per_core)
            || self
                .psi_cpu_some_avg10
                .is_some_and(|v| v > cfg.severe_psi_avg10)
    }

    /// Returns true when either signal is above the "step down" threshold.
    pub fn is_pressured(&self, cfg: &GovernorConfig) -> bool {
        self.load_per_core
            .is_some_and(|v| v > cfg.max_load_per_core)
            || self
                .psi_cpu_some_avg10
                .is_some_and(|v| v > cfg.max_psi_avg10)
    }
}

/// Classification of why the governor chose a given next-capacity value.
/// Serialized with snake_case tags so robot-mode consumers can switch on a
/// stable string vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GovernorDecisionReason {
    /// Governor was disabled via config; capacity is pinned at 100%.
    Disabled,
    /// Severe pressure observed; capacity dropped straight to the floor.
    Severe,
    /// Moderate pressure observed; capacity stepped down by 25pp.
    Pressured,
    /// Pressure present but capacity already at the floor; held.
    PressuredFloorHold,
    /// Sample healthy, healthy-streak not yet long enough to grow; held.
    HealthyHold,
    /// Sample healthy, streak threshold reached, capacity grew by 25pp.
    HealthyGrow,
    /// Sample healthy, streak threshold reached but already at 100%; held.
    HealthyCeilingHold,
}

/// Decide what the new published capacity should be given the latest signal
/// snapshot, the previous capacity, and an internal "healthy streak" counter.
///
/// Returns `(next_capacity_pct, next_healthy_streak, reason)`. The `reason`
/// lets callers record *why* a decision was made, not just what the decision
/// was. This is the core input to the telemetry surface that bead
/// `coding_agent_session_search-d2qix` asks for.
pub(crate) fn next_capacity(
    prev_capacity_pct: u32,
    healthy_streak: u32,
    snapshot: &HealthSnapshot,
    cfg: &GovernorConfig,
) -> (u32, u32, GovernorDecisionReason) {
    if cfg.disabled {
        return (100, 0, GovernorDecisionReason::Disabled);
    }

    if snapshot.is_severe(cfg) {
        // Severe pressure: drop straight to the floor, reset healthy streak.
        return (cfg.min_capacity_pct, 0, GovernorDecisionReason::Severe);
    }

    if snapshot.is_pressured(cfg) {
        // Moderate pressure: take a 25pp step down, but never below floor.
        let step_down = prev_capacity_pct
            .saturating_sub(25)
            .max(cfg.min_capacity_pct);
        let reason = if step_down == prev_capacity_pct {
            GovernorDecisionReason::PressuredFloorHold
        } else {
            GovernorDecisionReason::Pressured
        };
        return (step_down, 0, reason);
    }

    // Healthy sample. Require N consecutive healthy ticks before growing back.
    let new_streak = healthy_streak.saturating_add(1);
    if new_streak >= cfg.growth_consecutive_healthy_ticks {
        let grown = prev_capacity_pct.saturating_add(25).min(100);
        if grown > prev_capacity_pct {
            // Reset streak after a successful growth step so each step
            // requires a fresh N-tick run of healthy samples.
            return (grown, 0, GovernorDecisionReason::HealthyGrow);
        }
        // Already at the ceiling; hold capacity and keep the streak at its
        // current value so we don't keep incrementing an unbounded counter.
        (
            grown,
            new_streak,
            GovernorDecisionReason::HealthyCeilingHold,
        )
    } else {
        (
            prev_capacity_pct,
            new_streak,
            GovernorDecisionReason::HealthyHold,
        )
    }
}

/// Scale a caller-requested worker count by the current capacity. Always
/// returns at least 1 to keep the pipeline moving.
pub(crate) fn scale_worker_count(desired: usize, capacity_pct: u32) -> usize {
    if desired == 0 {
        return 0;
    }
    let capacity = capacity_pct.clamp(1, 100) as usize;
    let scaled = desired.saturating_mul(capacity) / 100;
    scaled.max(1)
}

/// Reader abstraction for the health signals. Stubbed in tests so the
/// hysteresis policy can be exercised without touching /proc.
pub(crate) trait HealthReader: Send + Sync {
    fn snapshot(&self) -> HealthSnapshot;
}

pub(crate) struct ProcHealthReader {
    ncpu: usize,
}

impl ProcHealthReader {
    pub fn new() -> Self {
        Self {
            ncpu: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1),
        }
    }
}

impl HealthReader for ProcHealthReader {
    #[cfg(target_os = "linux")]
    fn snapshot(&self) -> HealthSnapshot {
        let load_per_core = read_loadavg().map(|l1| {
            if self.ncpu == 0 {
                l1
            } else {
                l1 / self.ncpu as f32
            }
        });
        let psi_cpu_some_avg10 = read_psi_cpu_some_avg10();
        HealthSnapshot {
            load_per_core,
            psi_cpu_some_avg10,
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn snapshot(&self) -> HealthSnapshot {
        HealthSnapshot {
            load_per_core: None,
            psi_cpu_some_avg10: None,
        }
    }
}

#[cfg(target_os = "linux")]
fn read_loadavg() -> Option<f32> {
    let raw = std::fs::read_to_string("/proc/loadavg").ok()?;
    let first = raw.split_whitespace().next()?;
    first.parse::<f32>().ok()
}

#[cfg(target_os = "linux")]
fn read_psi_cpu_some_avg10() -> Option<f32> {
    let raw = std::fs::read_to_string("/proc/pressure/cpu").ok()?;
    // Expected format (first line):
    //   some avg10=0.00 avg60=0.00 avg300=0.00 total=0
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("some ") {
            for token in rest.split_whitespace() {
                if let Some(v) = token.strip_prefix("avg10=") {
                    return v.parse::<f32>().ok();
                }
            }
        }
    }
    None
}

/// One recorded decision, suitable for inclusion in the robot telemetry
/// surface. Kept deliberately small (a few tens of bytes) so the ring
/// buffer's memory footprint stays bounded.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
pub(crate) struct GovernorDecision {
    /// Time since governor startup, in milliseconds.
    pub at_elapsed_ms: u64,
    pub prev_capacity_pct: u32,
    pub next_capacity_pct: u32,
    pub reason: GovernorDecisionReason,
    pub snapshot: HealthSnapshot,
}

/// Telemetry snapshot returned by [`telemetry_snapshot`]. Operators and
/// automated callers can render this as JSON to understand why the governor
/// chose the currently-published capacity.
#[derive(Clone, Debug, serde::Serialize)]
pub(crate) struct GovernorTelemetry {
    pub current_capacity_pct: u32,
    pub healthy_streak: u32,
    pub shrink_count: u64,
    pub grow_count: u64,
    pub ticks_total: u64,
    pub disabled_via_env: bool,
    pub last_snapshot: Option<HealthSnapshot>,
    pub last_reason: Option<GovernorDecisionReason>,
    /// Oldest → newest. Bounded at [`TELEMETRY_DECISION_HISTORY`].
    pub recent_decisions: Vec<GovernorDecision>,
}

struct GovernorRuntimeState {
    recent_decisions: VecDeque<GovernorDecision>,
    last_snapshot: Option<HealthSnapshot>,
    last_reason: Option<GovernorDecisionReason>,
}

impl GovernorRuntimeState {
    fn new() -> Self {
        Self {
            recent_decisions: VecDeque::with_capacity(TELEMETRY_DECISION_HISTORY),
            last_snapshot: None,
            last_reason: None,
        }
    }
}

struct Governor {
    cfg: GovernorConfig,
    current_capacity: AtomicU32,
    healthy_streak: AtomicU32,
    shrink_count: AtomicU64,
    grow_count: AtomicU64,
    ticks_total: AtomicU64,
    started: AtomicBool,
    reader: Arc<dyn HealthReader>,
    runtime: Mutex<GovernorRuntimeState>,
    started_at: Instant,
}

impl Governor {
    fn new(cfg: GovernorConfig, reader: Arc<dyn HealthReader>) -> Self {
        Self {
            cfg,
            current_capacity: AtomicU32::new(100),
            healthy_streak: AtomicU32::new(0),
            shrink_count: AtomicU64::new(0),
            grow_count: AtomicU64::new(0),
            ticks_total: AtomicU64::new(0),
            started: AtomicBool::new(false),
            reader,
            runtime: Mutex::new(GovernorRuntimeState::new()),
            started_at: Instant::now(),
        }
    }

    fn ensure_started(self: &Arc<Self>) {
        if self.cfg.disabled {
            return;
        }
        if self
            .started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            // Another thread already claimed the spawn slot.
            return;
        }

        let me = Arc::clone(self);
        // Background sampler. One long-lived daemon thread per process.
        let spawn_result = thread::Builder::new()
            .name("cass-responsiveness-governor".into())
            .spawn(move || me.run());

        if let Err(err) = spawn_result {
            // Spawn failed (usually RLIMIT_NPROC). Roll back the started flag
            // so a later caller can retry when resource pressure eases, and
            // leave `current_capacity` pinned at its initial 100. We
            // deliberately do not panic: the indexer must keep making progress
            // even when the governor can't.
            self.started.store(false, Ordering::Release);
            tracing::warn!(
                error = %err,
                "failed to spawn cass responsiveness governor thread; capacity pinned at 100% until a later start succeeds"
            );
        }
    }

    fn run(&self) {
        loop {
            self.step_once();
            thread::sleep(self.cfg.tick);
        }
    }

    /// Apply one sampling tick. Split out from `run()` so unit tests can
    /// drive deterministic sequences through the decision machinery without
    /// spawning a background thread or sleeping.
    fn step_once(&self) {
        let snapshot = self.reader.snapshot();
        let prev = self.current_capacity.load(Ordering::Relaxed);
        let streak = self.healthy_streak.load(Ordering::Relaxed);
        let (next, next_streak, reason) = next_capacity(prev, streak, &snapshot, &self.cfg);

        if next < prev {
            self.shrink_count.fetch_add(1, Ordering::Relaxed);
        } else if next > prev {
            self.grow_count.fetch_add(1, Ordering::Relaxed);
        }
        self.ticks_total.fetch_add(1, Ordering::Relaxed);
        self.current_capacity.store(next, Ordering::Relaxed);
        self.healthy_streak.store(next_streak, Ordering::Relaxed);

        // Only retain decisions that describe meaningful events: a capacity
        // change, or a pressure signal (even while already pinned at the
        // floor). "Healthy hold" and "healthy ceiling hold" ticks are the
        // vast majority on an idle box and would otherwise flood the ring
        // buffer with useless rows.
        let record_this_tick = next != prev
            || matches!(
                reason,
                GovernorDecisionReason::Severe
                    | GovernorDecisionReason::Pressured
                    | GovernorDecisionReason::PressuredFloorHold
            );

        if let Ok(mut runtime) = self.runtime.lock() {
            runtime.last_snapshot = Some(snapshot);
            runtime.last_reason = Some(reason);
            if record_this_tick {
                if runtime.recent_decisions.len() >= TELEMETRY_DECISION_HISTORY {
                    runtime.recent_decisions.pop_front();
                }
                runtime.recent_decisions.push_back(GovernorDecision {
                    at_elapsed_ms: self.started_at.elapsed().as_millis() as u64,
                    prev_capacity_pct: prev,
                    next_capacity_pct: next,
                    reason,
                    snapshot,
                });
            }
        }

        if next != prev {
            tracing::info!(
                prev_capacity_pct = prev,
                next_capacity_pct = next,
                reason = ?reason,
                load_per_core = ?snapshot.load_per_core,
                psi_cpu_some_avg10 = ?snapshot.psi_cpu_some_avg10,
                "cass responsiveness governor updated capacity"
            );
        }
    }

    fn telemetry(&self) -> GovernorTelemetry {
        let (recent, last_snapshot, last_reason) = match self.runtime.lock() {
            Ok(runtime) => (
                runtime.recent_decisions.iter().copied().collect::<Vec<_>>(),
                runtime.last_snapshot,
                runtime.last_reason,
            ),
            // Poison is unexpected but non-fatal for telemetry: surface an
            // empty history rather than panicking. The poisoned runtime state
            // will still be updated by the next sampler tick.
            Err(_) => (Vec::new(), None, None),
        };
        let disabled = env_bool_truthy("CASS_RESPONSIVENESS_DISABLE") || self.cfg.disabled;
        // When the governor is disabled via env or config, the effective
        // capacity that every caller of `effective_worker_count` /
        // `current_capacity_pct` observes is pinned at 100. Reporting the
        // raw atomic (which may still hold a stale shrunken value from a
        // pre-disable sampler tick) would leave robot consumers with two
        // different "current" values for the same process. Align the
        // telemetry with what the rest of the module reports.
        let current = if disabled {
            100
        } else {
            self.current_capacity.load(Ordering::Relaxed)
        };
        GovernorTelemetry {
            current_capacity_pct: current,
            healthy_streak: self.healthy_streak.load(Ordering::Relaxed),
            shrink_count: self.shrink_count.load(Ordering::Relaxed),
            grow_count: self.grow_count.load(Ordering::Relaxed),
            ticks_total: self.ticks_total.load(Ordering::Relaxed),
            disabled_via_env: disabled,
            last_snapshot,
            last_reason,
            recent_decisions: recent,
        }
    }
}

static GOVERNOR: LazyLock<Arc<Governor>> = LazyLock::new(|| {
    Arc::new(Governor::new(
        GovernorConfig::from_env(),
        Arc::new(ProcHealthReader::new()),
    ))
});

/// Read the currently published capacity percentage. Starts the background
/// sampler on first call. Safe to call from any thread.
///
/// When `CASS_RESPONSIVENESS_DISABLE` is truthy, returns 100 unconditionally
/// and skips starting the sampler thread. This check happens on every read
/// (not just at init) so tests, benchmarks, and long-running daemons can
/// toggle the governor live without fighting `LazyLock` init order — the
/// static `GOVERNOR` is constructed at most once per process, but the
/// disable signal is honored at every read site.
pub(crate) fn current_capacity_pct() -> u32 {
    if env_bool_truthy("CASS_RESPONSIVENESS_DISABLE") {
        return 100;
    }
    let g = GOVERNOR.clone();
    g.ensure_started();
    g.current_capacity.load(Ordering::Relaxed)
}

/// Scale a caller-requested worker count by the current governor capacity.
/// Callers pass the *maximum* fan-out they would like (e.g. CPU count minus
/// reserved cores); the governor returns a bounded count that respects the
/// current machine responsiveness policy. Always returns at least 1.
pub(crate) fn effective_worker_count(desired: usize) -> usize {
    scale_worker_count(desired, current_capacity_pct())
}

/// Return a full telemetry snapshot of the process-wide governor. Starts
/// the background sampler on first call (same as [`current_capacity_pct`]).
/// Cheap enough to call repeatedly from status commands and diagnostic
/// loops. The returned value derives `serde::Serialize`, so robot callers
/// can render it with `serde_json::to_string_pretty` directly.
pub(crate) fn telemetry_snapshot() -> GovernorTelemetry {
    let g = GOVERNOR.clone();
    g.ensure_started();
    g.telemetry()
}

fn env_u32(key: &str) -> Option<u32> {
    dotenvy::var(key).ok().and_then(|v| v.trim().parse().ok())
}

fn env_f32(key: &str) -> Option<f32> {
    dotenvy::var(key).ok().and_then(|v| v.trim().parse().ok())
}

fn env_bool_truthy(key: &str) -> bool {
    match dotenvy::var(key) {
        Ok(v) => matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> GovernorConfig {
        GovernorConfig {
            min_capacity_pct: DEFAULT_MIN_CAPACITY_PCT,
            max_load_per_core: DEFAULT_MAX_LOAD_PER_CORE,
            severe_load_per_core: DEFAULT_SEVERE_LOAD_PER_CORE,
            max_psi_avg10: DEFAULT_MAX_PSI_AVG10,
            severe_psi_avg10: DEFAULT_SEVERE_PSI_AVG10,
            growth_consecutive_healthy_ticks: DEFAULT_GROWTH_CONSECUTIVE_HEALTHY_TICKS,
            tick: Duration::from_millis(1),
            disabled: false,
        }
    }

    fn healthy() -> HealthSnapshot {
        HealthSnapshot {
            load_per_core: Some(0.1),
            psi_cpu_some_avg10: Some(0.0),
        }
    }

    fn pressured() -> HealthSnapshot {
        HealthSnapshot {
            load_per_core: Some(1.5),
            psi_cpu_some_avg10: Some(5.0),
        }
    }

    fn severe() -> HealthSnapshot {
        HealthSnapshot {
            load_per_core: Some(3.0),
            psi_cpu_some_avg10: Some(80.0),
        }
    }

    /// A test-only `HealthReader` that returns a scripted sequence of
    /// snapshots. Once the script is exhausted, the last entry is
    /// repeated so callers that run the sampler for extra ticks see a
    /// stable tail.
    struct ScriptedReader {
        snapshots: std::sync::Mutex<std::collections::VecDeque<HealthSnapshot>>,
        fallback: HealthSnapshot,
    }

    impl ScriptedReader {
        fn new(script: Vec<HealthSnapshot>) -> Self {
            let fallback = *script.last().unwrap_or(&HealthSnapshot {
                load_per_core: None,
                psi_cpu_some_avg10: None,
            });
            Self {
                snapshots: std::sync::Mutex::new(script.into()),
                fallback,
            }
        }
    }

    impl HealthReader for ScriptedReader {
        fn snapshot(&self) -> HealthSnapshot {
            let mut guard = self.snapshots.lock().expect("scripted reader mutex");
            guard.pop_front().unwrap_or(self.fallback)
        }
    }

    /// Build a test-only governor that never spawns a thread; caller drives
    /// it via `step_once()`.
    fn build_test_governor(cfg: GovernorConfig, script: Vec<HealthSnapshot>) -> Governor {
        Governor::new(cfg, Arc::new(ScriptedReader::new(script)))
    }

    #[test]
    fn disabled_config_always_returns_full_capacity() {
        let mut c = cfg();
        c.disabled = true;
        let snap = HealthSnapshot {
            load_per_core: Some(10.0),
            psi_cpu_some_avg10: Some(90.0),
        };
        let (next, streak, reason) = next_capacity(50, 0, &snap, &c);
        assert_eq!(next, 100);
        assert_eq!(streak, 0);
        assert_eq!(reason, GovernorDecisionReason::Disabled);
    }

    #[test]
    fn healthy_snapshot_does_not_grow_before_streak_threshold() {
        let c = cfg();
        let h = healthy();
        let (next, streak, reason) = next_capacity(50, 0, &h, &c);
        assert_eq!(next, 50);
        assert_eq!(streak, 1);
        assert_eq!(reason, GovernorDecisionReason::HealthyHold);

        let (next, streak, reason) = next_capacity(next, streak, &h, &c);
        assert_eq!(next, 50);
        assert_eq!(streak, 2);
        assert_eq!(reason, GovernorDecisionReason::HealthyHold);

        let (next, streak, reason) = next_capacity(next, streak, &h, &c);
        assert_eq!(next, 75);
        assert_eq!(streak, 0, "streak must reset after a growth step");
        assert_eq!(reason, GovernorDecisionReason::HealthyGrow);
    }

    #[test]
    fn healthy_at_ceiling_is_classified_as_ceiling_hold() {
        let c = cfg();
        let h = healthy();
        let (next, streak, reason) = next_capacity(100, 2, &h, &c);
        // Third healthy tick fires, capacity is already 100 so we hold.
        assert_eq!(next, 100);
        assert_eq!(reason, GovernorDecisionReason::HealthyCeilingHold);
        assert_eq!(streak, 3, "ceiling hold keeps streak rather than resetting");
    }

    #[test]
    fn moderate_pressure_shrinks_immediately() {
        let c = cfg();
        let p = pressured();
        let (next, streak, reason) = next_capacity(100, 2, &p, &c);
        assert_eq!(next, 75);
        assert_eq!(streak, 0);
        assert_eq!(reason, GovernorDecisionReason::Pressured);

        let (next, streak, reason) = next_capacity(next, streak, &p, &c);
        assert_eq!(next, 50);
        assert_eq!(streak, 0);
        assert_eq!(reason, GovernorDecisionReason::Pressured);

        // Floor holds even if pressure persists, and the hold is classified.
        let (next, _, reason) = next_capacity(DEFAULT_MIN_CAPACITY_PCT, 0, &p, &c);
        assert_eq!(next, DEFAULT_MIN_CAPACITY_PCT);
        assert_eq!(reason, GovernorDecisionReason::PressuredFloorHold);
    }

    #[test]
    fn severe_pressure_drops_straight_to_floor() {
        let c = cfg();
        let s = severe();
        let (next, streak, reason) = next_capacity(100, 2, &s, &c);
        assert_eq!(next, DEFAULT_MIN_CAPACITY_PCT);
        assert_eq!(streak, 0);
        assert_eq!(reason, GovernorDecisionReason::Severe);
    }

    #[test]
    fn scale_worker_count_never_below_one_and_never_above_desired() {
        assert_eq!(scale_worker_count(0, 100), 0);
        assert_eq!(scale_worker_count(16, 100), 16);
        assert_eq!(scale_worker_count(16, 50), 8);
        assert_eq!(scale_worker_count(16, 25), 4);
        assert_eq!(scale_worker_count(1, 1), 1);
        assert!(scale_worker_count(4, 100) <= 4);
    }

    #[test]
    fn env_disable_signal_is_truthy_aware() {
        let probe = "__CASS_RESP_DISABLE_PARSE_PROBE__";
        let prior = std::env::var(probe).ok();
        for truthy in ["1", "true", "True", "YES", "on"] {
            // SAFETY: test-scoped env mutation with a unique sentinel key.
            unsafe {
                std::env::set_var(probe, truthy);
            }
            assert!(
                env_bool_truthy(probe),
                "expected `{truthy}` to be recognized as truthy"
            );
        }
        for falsy in ["0", "false", "No", "off", ""] {
            // SAFETY: test-scoped env mutation with a unique sentinel key.
            unsafe {
                std::env::set_var(probe, falsy);
            }
            assert!(
                !env_bool_truthy(probe),
                "expected `{falsy}` to be recognized as falsy"
            );
        }
        // SAFETY: test-scoped env cleanup.
        unsafe {
            std::env::remove_var(probe);
        }
        assert!(!env_bool_truthy(probe), "absent env var must be falsy");
        if let Some(v) = prior {
            // SAFETY: test-scoped env restore.
            unsafe {
                std::env::set_var(probe, v);
            }
        }
    }

    #[test]
    fn snapshot_classification_tolerates_missing_signals() {
        let c = cfg();
        let no_signals = HealthSnapshot {
            load_per_core: None,
            psi_cpu_some_avg10: None,
        };
        assert!(!no_signals.is_severe(&c));
        assert!(!no_signals.is_pressured(&c));
        let (next, streak, reason) = next_capacity(80, 0, &no_signals, &c);
        assert_eq!(next, 80);
        assert_eq!(streak, 1);
        assert_eq!(reason, GovernorDecisionReason::HealthyHold);
    }

    #[test]
    fn telemetry_counts_shrink_and_grow_events() {
        // Script: 1 severe (shrink to floor), then enough healthies to grow
        // all the way back. Default floor is 25 → need (100-25)/25 = 3 grow
        // steps, each requiring 3 healthy ticks = 9 ticks. Plus the 1 severe.
        let mut script = vec![severe()];
        script.extend(std::iter::repeat_n(healthy(), 9));
        let gov = build_test_governor(cfg(), script);

        for _ in 0..10 {
            gov.step_once();
        }

        let tele = gov.telemetry();
        assert_eq!(
            tele.current_capacity_pct, 100,
            "should have recovered to ceiling after 9 healthy ticks"
        );
        assert_eq!(tele.shrink_count, 1, "one severe drop = one shrink");
        assert_eq!(
            tele.grow_count, 3,
            "recovery from 25 to 100 in 25pp steps = 3 grow events"
        );
        assert_eq!(tele.ticks_total, 10);

        // The ring buffer should contain the severe drop plus the three
        // grow events (healthy-hold ticks are deliberately not recorded).
        let reasons: Vec<GovernorDecisionReason> =
            tele.recent_decisions.iter().map(|d| d.reason).collect();
        assert_eq!(
            reasons,
            vec![
                GovernorDecisionReason::Severe,
                GovernorDecisionReason::HealthyGrow,
                GovernorDecisionReason::HealthyGrow,
                GovernorDecisionReason::HealthyGrow,
            ]
        );
    }

    #[test]
    fn telemetry_ring_buffer_is_bounded() {
        // Feed more than TELEMETRY_DECISION_HISTORY pressured ticks so the
        // buffer wraps. All ticks are "pressured" (either real step-down or
        // floor-hold) so every tick is recorded.
        let count = TELEMETRY_DECISION_HISTORY + 50;
        let script = std::iter::repeat_n(pressured(), count).collect::<Vec<_>>();
        let gov = build_test_governor(cfg(), script);
        for _ in 0..count {
            gov.step_once();
        }

        let tele = gov.telemetry();
        assert_eq!(
            tele.recent_decisions.len(),
            TELEMETRY_DECISION_HISTORY,
            "ring buffer must saturate at its cap"
        );
        assert_eq!(tele.ticks_total, count as u64);
        // The newest entry should be the most-recent tick (elapsed_ms
        // monotonically increases).
        let last = tele.recent_decisions.last().unwrap();
        let first = tele.recent_decisions.first().unwrap();
        assert!(
            last.at_elapsed_ms >= first.at_elapsed_ms,
            "ring buffer must preserve chronological order"
        );
    }

    #[test]
    fn telemetry_skips_healthy_hold_ticks() {
        // A long run of healthy-hold ticks below the growth threshold should
        // NOT accumulate buffer entries.
        let script = std::iter::repeat_n(healthy(), 2).collect::<Vec<_>>();
        let gov = build_test_governor(cfg(), script);
        for _ in 0..2 {
            gov.step_once();
        }
        let tele = gov.telemetry();
        assert_eq!(
            tele.recent_decisions.len(),
            0,
            "healthy-hold ticks should not pollute the ring buffer"
        );
        assert_eq!(tele.current_capacity_pct, 100);
    }

    #[test]
    fn telemetry_serializes_to_json_with_expected_keys() {
        let gov = build_test_governor(cfg(), vec![severe(), pressured()]);
        gov.step_once();
        gov.step_once();
        let tele = gov.telemetry();
        let json = serde_json::to_string(&tele).expect("telemetry serializes");
        for key in [
            "current_capacity_pct",
            "shrink_count",
            "grow_count",
            "ticks_total",
            "disabled_via_env",
            "last_snapshot",
            "last_reason",
            "recent_decisions",
            "healthy_streak",
        ] {
            assert!(
                json.contains(key),
                "expected JSON to contain `{key}`, got: {json}"
            );
        }
        // Spot-check that reason serializes as a snake_case string.
        assert!(
            json.contains("\"severe\"") || json.contains("\"pressured\""),
            "expected snake_case reason tag in JSON: {json}"
        );
    }

    // -----------------------------------------------------------------
    // Anti-oscillation stress tests (bead d2qix anti-flap hardening)
    // -----------------------------------------------------------------

    fn run_script_and_trace(
        cfg: GovernorConfig,
        script: Vec<HealthSnapshot>,
    ) -> (Governor, Vec<u32>) {
        let tick_count = script.len();
        let gov = build_test_governor(cfg, script);
        let mut capacities = Vec::with_capacity(tick_count);
        for _ in 0..tick_count {
            gov.step_once();
            capacities.push(gov.current_capacity.load(Ordering::Relaxed));
        }
        (gov, capacities)
    }

    fn transitions(capacities: &[u32]) -> usize {
        capacities
            .windows(2)
            .filter(|pair| pair[0] != pair[1])
            .count()
    }

    #[test]
    fn anti_flap_alternating_pressured_healthy_never_grows() {
        // Alternate pressured/healthy for 100 ticks. Each healthy tick
        // must reset the growth streak (because it always follows a
        // pressured tick which reset it to 0), so capacity should never
        // grow back. The floor absorbs repeated pressure; only the first
        // pressured tick actually shrinks because we start at 100%.
        let mut script = Vec::with_capacity(100);
        for i in 0..100 {
            script.push(if i % 2 == 0 { pressured() } else { healthy() });
        }
        let (gov, capacities) = run_script_and_trace(cfg(), script);

        let tele = gov.telemetry();
        assert_eq!(
            tele.grow_count, 0,
            "alternating flap must never produce a grow event"
        );
        // Over 100 ticks, shrinks happen each pressured tick until we hit
        // the floor (100 → 75 → 50 → 25 = 3 shrinks). After that, pressure
        // samples hit the PressuredFloorHold branch with no shrink.
        assert_eq!(tele.shrink_count, 3, "flap shrinks until floor, then holds");
        let t = transitions(&capacities);
        assert!(
            t <= 3,
            "alternating flap must not oscillate capacity; saw {t} transitions over {} ticks",
            capacities.len()
        );
    }

    #[test]
    fn anti_flap_near_threshold_jitter_does_not_oscillate() {
        // Jitter around the pressured threshold. With max_load_per_core=1.25,
        // load samples of 1.24 are healthy-hold, 1.26 are pressured.
        let mut script = Vec::with_capacity(60);
        for i in 0..60 {
            script.push(HealthSnapshot {
                load_per_core: Some(if i % 2 == 0 { 1.24 } else { 1.26 }),
                psi_cpu_some_avg10: Some(1.0),
            });
        }
        let (_gov, capacities) = run_script_and_trace(cfg(), script);
        let t = transitions(&capacities);
        // Shrink on each pressured tick up to the floor (3 shrinks), then
        // no growth because each healthy tick follows a pressured tick
        // which just reset the streak.
        assert!(
            t <= 3,
            "threshold jitter must not cause capacity oscillation; saw {t} transitions"
        );
    }

    #[test]
    fn anti_flap_burst_recovery_respects_hysteresis() {
        // Alternate blocks of severe pressure and recovery windows. After
        // each severe burst, growth requires exactly growth_ticks healthy
        // samples per 25pp step.
        let mut script = Vec::new();
        for _ in 0..3 {
            for _ in 0..5 {
                script.push(severe());
            }
            for _ in 0..9 {
                script.push(healthy());
            }
        }
        let (gov, capacities) = run_script_and_trace(cfg(), script);

        let tele = gov.telemetry();
        // Three severe bursts each drop from wherever we are straight to
        // the floor; but the FIRST burst starts at 100% so it drops to 25%
        // (one shrink event). Subsequent bursts start at 100% too (after
        // recovery), so they each produce one shrink event. 3 shrinks.
        assert_eq!(tele.shrink_count, 3, "one shrink per severe burst");
        // Each recovery window has 9 healthy ticks = 3 growth steps = 3
        // grow events per burst, × 3 bursts = 9 grow events.
        assert_eq!(
            tele.grow_count, 9,
            "each 9-tick healthy window produces 3 grow steps"
        );
        // `transitions()` only counts pairs in `capacities`, i.e. it compares
        // post-tick values. The initial 100 → 25 shrink of the *first* burst
        // happens before any capacity has been sampled, so it doesn't appear
        // as a transition between adjacent elements. So:
        //   burst 1: 3 grow transitions (the initial shrink is invisible)
        //   burst 2: 1 shrink + 3 grow = 4 transitions
        //   burst 3: 1 shrink + 3 grow = 4 transitions
        // Total = 11. This is consistent with `shrink_count=3` and
        // `grow_count=9` (which the Governor tracks against its starting
        // capacity, not against the capacities vec).
        let t = transitions(&capacities);
        assert_eq!(t, 11);
        assert!(
            (t as f64) / (capacities.len() as f64) <= 1.0 / 3.0,
            "transition rate must respect the 3-tick hysteresis"
        );
    }

    #[test]
    fn anti_flap_transition_rate_upper_bound() {
        // Property-style guard: for any interleaving, transitions per K
        // ticks must never exceed `ceil(K / growth_consecutive_healthy_ticks) + K/growth_ticks + shrink_budget`.
        // Concretely we pick a pathological worst-case where growth fires as
        // fast as possible (3 healthy, grow; 3 healthy, grow; ...). That's
        // one transition every 3 ticks for grow, plus shrink-on-every-
        // pressured. Even then the rate is bounded.
        let growth_ticks = DEFAULT_GROWTH_CONSECUTIVE_HEALTHY_TICKS as usize;
        // 120 ticks: alternate windows of 3 healthy + 1 severe.
        let mut script = Vec::with_capacity(120);
        while script.len() < 120 {
            for _ in 0..growth_ticks {
                script.push(healthy());
            }
            script.push(severe());
        }
        script.truncate(120);
        let tick_count = script.len();
        let (_gov, capacities) = run_script_and_trace(cfg(), script);
        let t = transitions(&capacities);
        // Per 4-tick window: one severe drop (100 → 25 if previously at 100,
        // else same) and one grow (25 → 50). That's at most 2 transitions
        // per 4 ticks = 0.5 per tick.
        let rate = t as f64 / tick_count as f64;
        assert!(
            rate <= 0.55,
            "worst-case transition rate must stay bounded; saw {rate} over {tick_count} ticks"
        );
    }
}
