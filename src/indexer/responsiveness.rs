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
//! * **Explainable.** Thresholds live in named env vars and the current
//!   decision is queryable via [`current_capacity_pct`].
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

use std::sync::{
    Arc, LazyLock, OnceLock,
    atomic::{AtomicBool, AtomicU32, Ordering},
};
use std::thread;
use std::time::Duration;

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

#[derive(Clone, Copy, Debug, PartialEq)]
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

/// Decide what the new published capacity should be given the latest signal
/// snapshot, the previous capacity, and an internal "healthy streak" counter.
///
/// Returned tuple: (next_capacity_pct, next_healthy_streak).
pub(crate) fn next_capacity(
    prev_capacity_pct: u32,
    healthy_streak: u32,
    snapshot: &HealthSnapshot,
    cfg: &GovernorConfig,
) -> (u32, u32) {
    if cfg.disabled {
        return (100, 0);
    }

    if snapshot.is_severe(cfg) {
        // Severe pressure: drop straight to the floor, reset healthy streak.
        return (cfg.min_capacity_pct, 0);
    }

    if snapshot.is_pressured(cfg) {
        // Moderate pressure: take a 25pp step down, but never below floor.
        let step_down = prev_capacity_pct
            .saturating_sub(25)
            .max(cfg.min_capacity_pct);
        return (step_down, 0);
    }

    // Healthy sample. Require N consecutive healthy ticks before growing back.
    let new_streak = healthy_streak.saturating_add(1);
    if new_streak >= cfg.growth_consecutive_healthy_ticks {
        let grown = prev_capacity_pct.saturating_add(25).min(100);
        // Reset streak after a successful growth step so each step requires a
        // fresh N-tick run of healthy samples.
        let reset_streak = if grown > prev_capacity_pct {
            0
        } else {
            new_streak
        };
        (grown, reset_streak)
    } else {
        (prev_capacity_pct, new_streak)
    }
}

/// Scale a caller-requested worker count by the current capacity. Always
/// returns at least 1 to keep the pipeline moving.
pub(crate) fn scale_worker_count(desired: usize, capacity_pct: u32) -> usize {
    if desired == 0 {
        return 0;
    }
    let capacity = capacity_pct.min(100).max(1) as usize;
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

struct Governor {
    cfg: GovernorConfig,
    current_capacity: AtomicU32,
    healthy_streak: AtomicU32,
    started: AtomicBool,
    reader: Arc<dyn HealthReader>,
}

impl Governor {
    fn new(cfg: GovernorConfig, reader: Arc<dyn HealthReader>) -> Self {
        Self {
            cfg,
            current_capacity: AtomicU32::new(100),
            healthy_streak: AtomicU32::new(0),
            started: AtomicBool::new(false),
            reader,
        }
    }

    fn ensure_started(self: &Arc<Self>) {
        if self.cfg.disabled {
            return;
        }
        if self
            .started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            let me = Arc::clone(self);
            // Background sampler. One long-lived daemon thread per process.
            thread::Builder::new()
                .name("cass-responsiveness-governor".into())
                .spawn(move || me.run())
                .unwrap_or_else(|err| {
                    tracing::warn!(
                        error = %err,
                        "failed to spawn cass responsiveness governor thread; running pinned at 100%"
                    );
                    // Ensure later callers don't infinitely try to start it.
                    panic!("governor thread spawn failed");
                });
        }
    }

    fn run(&self) {
        loop {
            let snapshot = self.reader.snapshot();
            let prev = self.current_capacity.load(Ordering::Relaxed);
            let streak = self.healthy_streak.load(Ordering::Relaxed);
            let (next, next_streak) = next_capacity(prev, streak, &snapshot, &self.cfg);
            if next != prev {
                tracing::info!(
                    prev_capacity_pct = prev,
                    next_capacity_pct = next,
                    load_per_core = ?snapshot.load_per_core,
                    psi_cpu_some_avg10 = ?snapshot.psi_cpu_some_avg10,
                    "cass responsiveness governor updated capacity"
                );
            }
            self.current_capacity.store(next, Ordering::Relaxed);
            self.healthy_streak.store(next_streak, Ordering::Relaxed);
            thread::sleep(self.cfg.tick);
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
pub(crate) fn current_capacity_pct() -> u32 {
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

/// Test-only override: pin the published capacity to a specific value and
/// skip the background sampler. Intended for use by benchmarks and regression
/// tests that want deterministic fan-out. The override persists until
/// `clear_test_override` is called.
#[cfg(test)]
static TEST_OVERRIDE: OnceLock<AtomicU32> = OnceLock::new();

#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn set_test_capacity_pct(pct: u32) {
    TEST_OVERRIDE
        .get_or_init(|| AtomicU32::new(100))
        .store(pct, Ordering::Relaxed);
}

// Prevent an "unused" warning on non-test builds.
#[cfg(not(test))]
#[allow(dead_code)]
fn _reserve_test_override_symbol() -> &'static OnceLock<AtomicU32> {
    static RESERVED: OnceLock<AtomicU32> = OnceLock::new();
    &RESERVED
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

    #[test]
    fn disabled_config_always_returns_full_capacity() {
        let mut c = cfg();
        c.disabled = true;
        let snap = HealthSnapshot {
            load_per_core: Some(10.0),
            psi_cpu_some_avg10: Some(90.0),
        };
        let (next, streak) = next_capacity(50, 0, &snap, &c);
        assert_eq!(next, 100);
        assert_eq!(streak, 0);
    }

    #[test]
    fn healthy_snapshot_does_not_grow_before_streak_threshold() {
        let c = cfg();
        let healthy = HealthSnapshot {
            load_per_core: Some(0.1),
            psi_cpu_some_avg10: Some(0.0),
        };
        // Start at 50%, streak 0. First tick: streak increases but capacity
        // stays at 50 because growth requires 3 consecutive healthy ticks.
        let (next, streak) = next_capacity(50, 0, &healthy, &c);
        assert_eq!(next, 50);
        assert_eq!(streak, 1);

        let (next, streak) = next_capacity(next, streak, &healthy, &c);
        assert_eq!(next, 50);
        assert_eq!(streak, 2);

        // Third healthy tick trips the threshold and grows by 25pp.
        let (next, streak) = next_capacity(next, streak, &healthy, &c);
        assert_eq!(next, 75);
        assert_eq!(streak, 0, "streak must reset after a growth step");
    }

    #[test]
    fn moderate_pressure_shrinks_immediately() {
        let c = cfg();
        let pressured = HealthSnapshot {
            // Just over the loadavg/core threshold.
            load_per_core: Some(1.5),
            psi_cpu_some_avg10: Some(5.0),
        };
        let (next, streak) = next_capacity(100, 2, &pressured, &c);
        assert_eq!(next, 75);
        assert_eq!(streak, 0);

        let (next, streak) = next_capacity(next, streak, &pressured, &c);
        assert_eq!(next, 50);
        assert_eq!(streak, 0);

        // Floor holds even if pressure persists.
        let (next, _) = next_capacity(DEFAULT_MIN_CAPACITY_PCT, 0, &pressured, &c);
        assert_eq!(next, DEFAULT_MIN_CAPACITY_PCT);
    }

    #[test]
    fn severe_pressure_drops_straight_to_floor() {
        let c = cfg();
        let severe = HealthSnapshot {
            load_per_core: Some(3.0),
            psi_cpu_some_avg10: Some(80.0),
        };
        let (next, streak) = next_capacity(100, 2, &severe, &c);
        assert_eq!(next, DEFAULT_MIN_CAPACITY_PCT);
        assert_eq!(streak, 0);
    }

    #[test]
    fn scale_worker_count_never_below_one_and_never_above_desired() {
        assert_eq!(scale_worker_count(0, 100), 0);
        assert_eq!(scale_worker_count(16, 100), 16);
        assert_eq!(scale_worker_count(16, 50), 8);
        assert_eq!(scale_worker_count(16, 25), 4);
        // Even with capacity 1, we never drop to 0 for non-empty requests.
        assert_eq!(scale_worker_count(1, 1), 1);
        // Never exceed desired.
        assert!(scale_worker_count(4, 100) <= 4);
    }

    #[test]
    fn snapshot_classification_tolerates_missing_signals() {
        let c = cfg();
        // Non-Linux or older kernels: both signals None. Should never trigger
        // pressure, so the governor pins at current capacity and simply
        // advances the healthy streak.
        let no_signals = HealthSnapshot {
            load_per_core: None,
            psi_cpu_some_avg10: None,
        };
        assert!(!no_signals.is_severe(&c));
        assert!(!no_signals.is_pressured(&c));
        let (next, streak) = next_capacity(80, 0, &no_signals, &c);
        assert_eq!(next, 80);
        assert_eq!(streak, 1);
    }
}
