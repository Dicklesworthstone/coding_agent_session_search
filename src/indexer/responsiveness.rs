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

/// Default calibration window size for split-conformal pressure thresholds.
/// 4 096 samples at the default 2 s tick = ~2.3 hours of history, which
/// comfortably spans a typical workday's duty cycle without inflating
/// memory beyond a few KiB per signal.
const DEFAULT_CONFORMAL_K: usize = 256;

/// Minimum number of calibration samples required before the conformal
/// quantile is emitted. Below this, the governor falls back to static
/// thresholds for the current tick (deterministic conservative fallback).
const DEFAULT_CONFORMAL_K_MIN: usize = 32;

/// Default coverage level for the "pressured" quantile. Higher α = more
/// false-positive shrinks, but faster response to real pressure. 0.05
/// means we expect ≈ 1 false-positive step-down per 20 healthy ticks.
const DEFAULT_CONFORMAL_ALPHA_PRESSURED: f32 = 0.05;

/// Default coverage level for the "severe" quantile. Tighter than
/// pressured because drop-to-floor is disruptive. 0.01 means ≈ 1
/// false-positive floor-drop per 100 healthy ticks.
const DEFAULT_CONFORMAL_ALPHA_SEVERE: f32 = 0.01;

/// Page-Hinkley drift-detection parameter δ (allowed mean-drift
/// tolerance). A larger δ tolerates more drift before triggering a
/// calibration reset; smaller δ is stricter but flaps more often.
const DEFAULT_DRIFT_DELTA: f32 = 0.01;

/// Page-Hinkley trigger threshold λ. Once the cumulative drift signal
/// exceeds λ, declare change-point and reset the calibration window.
/// Conservative default — tuned so stationary streams trip < 1 time
/// per 10k samples on average.
const DEFAULT_DRIFT_LAMBDA: f32 = 0.5;

/// Huber / MAD outlier rejection multiplier. Samples with
/// |v − median| > HUBER_K × MAD are dropped from the calibration
/// window (still used for the current-tick decision). 3.5 is the
/// published Huber constant; equivalent to ≈ 3σ under Normal tails.
const MAD_OUTLIER_K: f32 = 3.5;

/// Which calibration policy the governor uses to compute the next
/// `pressured` / `severe` thresholds for each health signal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CalibrationMode {
    /// Static, hand-tuned thresholds from `DEFAULT_*` constants plus env
    /// overrides. Default for backwards compatibility — all 17 existing
    /// governor unit tests exercise this path.
    Static,
    /// Split-conformal quantile over a rolling window of healthy-period
    /// samples. Provides a finite-sample coverage guarantee per-host.
    /// See `ALIEN-ARTIFACT-CARD2-SPEC.md` under
    /// `tests/artifacts/perf/2026-04-21-profile-run/`.
    Conformal,
}

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
    /// Threshold policy for pressured/severe signal classification.
    /// `Static` preserves the pre-conformal behaviour bit-for-bit.
    pub calibration_mode: CalibrationMode,
    /// Calibration window size for split-conformal quantile. Only used
    /// when `calibration_mode == CalibrationMode::Conformal`.
    pub conformal_k: usize,
    /// Minimum samples before the conformal path emits a threshold.
    pub conformal_k_min: usize,
    /// α for the pressured quantile (expected FP rate per tick).
    pub conformal_alpha_pressured: f32,
    /// α for the severe quantile (always ≤ alpha_pressured so
    /// `q̂_severe` ≥ `q̂_pressured`).
    pub conformal_alpha_severe: f32,
    /// Page-Hinkley δ parameter (mean-drift tolerance).
    pub drift_delta: f32,
    /// Page-Hinkley λ threshold (triggers calibration reset).
    pub drift_lambda: f32,
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

        // Conformal knobs. Never clamped to zero — a zero `K` or `K_min`
        // would make the calibration path operate on an empty window,
        // which we handle by refusing to emit a threshold. But values
        // outside sane ranges are clamped so env misconfig can't make
        // the governor do something absurd.
        // Default is `Conformal` (post-flip): every host gets an
        // adaptive per-host threshold with a finite-sample coverage
        // guarantee. Users can pin the legacy static thresholds back on
        // with `CASS_RESPONSIVENESS_CALIBRATION=static`.
        let calibration_mode = match dotenvy::var("CASS_RESPONSIVENESS_CALIBRATION")
            .ok()
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("static") => CalibrationMode::Static,
            _ => CalibrationMode::Conformal,
        };
        let conformal_k = env_u32("CASS_RESPONSIVENESS_CONFORMAL_K")
            .map(|v| (v as usize).clamp(16, 4096))
            .unwrap_or(DEFAULT_CONFORMAL_K);
        let conformal_k_min = env_u32("CASS_RESPONSIVENESS_CONFORMAL_K_MIN")
            .map(|v| (v as usize).clamp(4, conformal_k))
            .unwrap_or(DEFAULT_CONFORMAL_K_MIN.min(conformal_k));
        let conformal_alpha_pressured = env_f32("CASS_RESPONSIVENESS_CONFORMAL_ALPHA_PRESSURED")
            .filter(|v| v.is_finite() && *v > 0.0 && *v < 0.5)
            .unwrap_or(DEFAULT_CONFORMAL_ALPHA_PRESSURED);
        let conformal_alpha_severe = env_f32("CASS_RESPONSIVENESS_CONFORMAL_ALPHA_SEVERE")
            .filter(|v| v.is_finite() && *v > 0.0 && *v < conformal_alpha_pressured)
            .unwrap_or_else(|| DEFAULT_CONFORMAL_ALPHA_SEVERE.min(conformal_alpha_pressured * 0.5));
        let drift_delta = env_f32("CASS_RESPONSIVENESS_DRIFT_DELTA")
            .filter(|v| v.is_finite() && *v > 0.0 && *v < 10.0)
            .unwrap_or(DEFAULT_DRIFT_DELTA);
        let drift_lambda = env_f32("CASS_RESPONSIVENESS_DRIFT_LAMBDA")
            .filter(|v| v.is_finite() && *v > 0.0 && *v < 100.0)
            .unwrap_or(DEFAULT_DRIFT_LAMBDA);

        Self {
            min_capacity_pct,
            max_load_per_core,
            severe_load_per_core,
            max_psi_avg10,
            severe_psi_avg10,
            growth_consecutive_healthy_ticks,
            tick: Duration::from_secs(tick_secs),
            disabled,
            calibration_mode,
            conformal_k,
            conformal_k_min,
            conformal_alpha_pressured,
            conformal_alpha_severe,
            drift_delta,
            drift_lambda,
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
    /// Present only when `CASS_RESPONSIVENESS_CALIBRATION=conformal`.
    /// Carries the current calibration-window fill, recent drift trips,
    /// and the quantiles the conformal policy is emitting for each
    /// signal × severity pairing.
    pub calibration: Option<CalibrationTelemetry>,
}

// ---------------------------------------------------------------------------
// Conformal calibration machinery (bead `d2qix` Card 2). See
// `tests/artifacts/perf/2026-04-21-profile-run/ALIEN-ARTIFACT-CARD2-SPEC.md`
// for the decision-theoretic spec + proof obligations this code discharges.
// ---------------------------------------------------------------------------

/// Split-conformal quantile per Vovk/Gammerman/Shafer 2005 Theorem 1.
/// Returns `scores[ ⌈(K + 1)(1 − α)⌉ − 1 ]` from a sorted-ascending slice,
/// clamped to the last element when the target index falls outside the
/// available range (which is exactly the convention in the Vovk text:
/// when (K+1)(1-α) > K we fall back to the empirical max).
///
/// Preconditions: `scores.len() ≥ 1`, `0 < alpha < 1`. Violating either
/// returns `None`.
pub(crate) fn conformal_quantile_sorted(scores: &[f32], alpha: f32) -> Option<f32> {
    if scores.is_empty() || !alpha.is_finite() || alpha <= 0.0 || alpha >= 1.0 {
        return None;
    }
    // ⌈(K + 1)(1 − α)⌉ − 1, clamped to [0, K−1].
    let k = scores.len();
    let target = ((k as f32 + 1.0) * (1.0 - alpha)).ceil() as isize - 1;
    let idx = target.clamp(0, (k as isize) - 1) as usize;
    Some(scores[idx])
}

/// Rolling ring buffer of healthy-period samples for one signal. Keeps the
/// most recent `K` entries so we can recompute a split-conformal quantile
/// on demand. Writes are cheap (push_back + pop_front when full); reads
/// allocate a sorted copy once per observation request.
#[derive(Debug, Clone)]
struct CalibrationStream {
    samples: VecDeque<f32>,
    k: usize,
}

impl CalibrationStream {
    fn new(k: usize) -> Self {
        Self {
            samples: VecDeque::with_capacity(k.max(1)),
            k: k.max(1),
        }
    }

    fn len(&self) -> usize {
        self.samples.len()
    }

    fn clear(&mut self) {
        self.samples.clear();
    }

    fn push(&mut self, value: f32) {
        if !value.is_finite() {
            return;
        }
        if self.samples.len() >= self.k {
            self.samples.pop_front();
        }
        self.samples.push_back(value);
    }

    /// Median of the current window. Linear-time via sort into a temporary.
    /// We intentionally re-sort per call instead of maintaining a sorted
    /// structure — the window is small (≤ 4 096) and called at most once
    /// per governor tick.
    fn median(&self) -> Option<f32> {
        if self.samples.is_empty() {
            return None;
        }
        let mut buf: Vec<f32> = self.samples.iter().copied().collect();
        buf.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = buf.len();
        Some(if n.is_multiple_of(2) {
            (buf[n / 2 - 1] + buf[n / 2]) / 2.0
        } else {
            buf[n / 2]
        })
    }

    /// Median absolute deviation (MAD). Returns None on empty window.
    fn mad(&self) -> Option<f32> {
        let med = self.median()?;
        let mut deviations: Vec<f32> = self.samples.iter().map(|v| (v - med).abs()).collect();
        deviations.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = deviations.len();
        if n == 0 {
            return None;
        }
        Some(if n.is_multiple_of(2) {
            (deviations[n / 2 - 1] + deviations[n / 2]) / 2.0
        } else {
            deviations[n / 2]
        })
    }

    /// True iff `v` is a Huber-outlier given the window's current median
    /// and MAD. The gate only engages once the window has enough data
    /// to estimate spread; small windows or degenerate-all-identical
    /// windows (MAD = 0) admit every new sample so we don't get stuck
    /// with a 1-sample window forever.
    fn is_outlier(&self, v: f32) -> bool {
        const MIN_SAMPLES_FOR_GATE: usize = 8;
        if self.samples.len() < MIN_SAMPLES_FOR_GATE {
            return false;
        }
        let (Some(med), Some(mad)) = (self.median(), self.mad()) else {
            return false;
        };
        if mad == 0.0 {
            // Everything currently in the window is identical. A new
            // varying value is fine to admit — letting it in is how
            // MAD eventually becomes non-zero. Rejecting it would lock
            // the window at its initial value permanently.
            return false;
        }
        (v - med).abs() > MAD_OUTLIER_K * mad
    }

    /// Compute the split-conformal quantile at the given α. Returns None
    /// when the window has fewer than `k_min` samples (caller falls back
    /// to the static threshold).
    fn quantile(&self, alpha: f32, k_min: usize) -> Option<f32> {
        if self.samples.len() < k_min {
            return None;
        }
        let mut sorted: Vec<f32> = self.samples.iter().copied().collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        conformal_quantile_sorted(&sorted, alpha)
    }
}

/// Page-Hinkley drift detector — a one-sided sequential change-point test
/// covering the same ground as §12.13 ADWIN for our purposes but in under
/// 30 lines. Tracks a cumulative deviation statistic `g_t`; when it
/// exceeds `λ` we declare a regime shift and reset.
///
///    µ_t = running mean of observed values
///    m_t = (x_t - µ_t) - δ
///    g_t = max(0, g_{t-1} + m_t)      ← one-sided increase detector
///    g_t = min(g_t', g_{t-1} + m_t)   ← track running minimum
///    declare change iff (g_t - min_g) > λ
#[derive(Debug, Clone)]
struct PageHinkley {
    delta: f32,
    lambda: f32,
    n: u64,
    running_mean: f32,
    cumulative: f32,
    min_cumulative: f32,
}

impl PageHinkley {
    fn new(delta: f32, lambda: f32) -> Self {
        Self {
            delta,
            lambda,
            n: 0,
            running_mean: 0.0,
            cumulative: 0.0,
            min_cumulative: 0.0,
        }
    }

    fn reset(&mut self) {
        self.n = 0;
        self.running_mean = 0.0;
        self.cumulative = 0.0;
        self.min_cumulative = 0.0;
    }

    /// Update with a new observation. Returns `true` iff the cumulative
    /// drift exceeds the configured λ — at that point the caller should
    /// clear its calibration window and reset this detector.
    fn observe(&mut self, v: f32) -> bool {
        if !v.is_finite() {
            return false;
        }
        self.n = self.n.saturating_add(1);
        // Welford-style running mean update for numerical stability.
        self.running_mean += (v - self.running_mean) / (self.n as f32);
        self.cumulative += v - self.running_mean - self.delta;
        if self.cumulative < self.min_cumulative {
            self.min_cumulative = self.cumulative;
        }
        (self.cumulative - self.min_cumulative) > self.lambda
    }
}

/// Per-signal calibration state (one load stream, one PSI stream). Each
/// stream holds its own rolling window and its own Page-Hinkley detector
/// so drift on one signal does not invalidate the other.
#[derive(Debug, Clone)]
struct SignalCalibration {
    window: CalibrationStream,
    drift: PageHinkley,
}

impl SignalCalibration {
    fn new(cfg: &GovernorConfig) -> Self {
        Self {
            window: CalibrationStream::new(cfg.conformal_k),
            drift: PageHinkley::new(cfg.drift_delta, cfg.drift_lambda),
        }
    }

    /// Observe a sample taken during a healthy tick. Returns a status the
    /// caller can record into telemetry. `is_healthy_tick` must be true
    /// for the sample to enter the calibration window — pressured-tick
    /// samples would contaminate the quantile we want to learn.
    fn observe(&mut self, v: f32, is_healthy_tick: bool) -> SignalObserveOutcome {
        if !v.is_finite() {
            return SignalObserveOutcome::NotFinite;
        }
        let drift_detected = self.drift.observe(v);
        if drift_detected {
            self.window.clear();
            self.drift.reset();
            return SignalObserveOutcome::DriftResetTriggered;
        }
        if !is_healthy_tick {
            return SignalObserveOutcome::SkippedPressuredTick;
        }
        if self.window.is_outlier(v) {
            return SignalObserveOutcome::RejectedOutlier;
        }
        self.window.push(v);
        SignalObserveOutcome::Accepted
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SignalObserveOutcome {
    Accepted,
    SkippedPressuredTick,
    RejectedOutlier,
    DriftResetTriggered,
    NotFinite,
}

/// Aggregate per-host calibration across both signals (load, PSI).
#[derive(Debug, Clone)]
struct GovernorCalibration {
    load: SignalCalibration,
    psi: SignalCalibration,
    /// Monotone counter: total drift resets triggered (both signals).
    drift_reset_count: u64,
    /// Monotone counter: samples dropped by the MAD outlier gate.
    outliers_rejected: u64,
    /// Total observation calls, for telemetry denominators.
    observations_total: u64,
}

impl GovernorCalibration {
    fn new(cfg: &GovernorConfig) -> Self {
        Self {
            load: SignalCalibration::new(cfg),
            psi: SignalCalibration::new(cfg),
            drift_reset_count: 0,
            outliers_rejected: 0,
            observations_total: 0,
        }
    }

    /// Observe one snapshot. Returns the effective thresholds the caller
    /// should use for the current tick (None → caller falls back to the
    /// static thresholds from `GovernorConfig`).
    ///
    /// `is_healthy_tick` signals whether the CURRENT tick's reading is
    /// considered healthy by the static classifier — this prevents the
    /// calibration window from learning from pressured samples.
    fn observe_and_compute_thresholds(
        &mut self,
        snapshot: &HealthSnapshot,
        is_healthy_tick: bool,
        cfg: &GovernorConfig,
    ) -> Option<EffectiveThresholds> {
        self.observations_total = self.observations_total.saturating_add(1);
        // Observe each signal independently so drift on one doesn't
        // destroy the other's calibration.
        if let Some(v) = snapshot.load_per_core {
            let outcome = self.load.observe(v, is_healthy_tick);
            match outcome {
                SignalObserveOutcome::RejectedOutlier => {
                    self.outliers_rejected = self.outliers_rejected.saturating_add(1);
                }
                SignalObserveOutcome::DriftResetTriggered => {
                    self.drift_reset_count = self.drift_reset_count.saturating_add(1);
                }
                _ => {}
            }
        }
        if let Some(v) = snapshot.psi_cpu_some_avg10 {
            let outcome = self.psi.observe(v, is_healthy_tick);
            match outcome {
                SignalObserveOutcome::RejectedOutlier => {
                    self.outliers_rejected = self.outliers_rejected.saturating_add(1);
                }
                SignalObserveOutcome::DriftResetTriggered => {
                    self.drift_reset_count = self.drift_reset_count.saturating_add(1);
                }
                _ => {}
            }
        }

        // Require BOTH streams to have accumulated K_min healthy samples
        // before we start emitting dynamic thresholds — otherwise we'd
        // mix a dynamic bound on one signal with a static bound on the
        // other, which makes the composite behaviour hard to reason
        // about.
        let load_pressured = self
            .load
            .window
            .quantile(cfg.conformal_alpha_pressured, cfg.conformal_k_min);
        let load_severe = self
            .load
            .window
            .quantile(cfg.conformal_alpha_severe, cfg.conformal_k_min);
        let psi_pressured = self
            .psi
            .window
            .quantile(cfg.conformal_alpha_pressured, cfg.conformal_k_min);
        let psi_severe = self
            .psi
            .window
            .quantile(cfg.conformal_alpha_severe, cfg.conformal_k_min);

        let (lp, ls, pp, ps) = (load_pressured?, load_severe?, psi_pressured?, psi_severe?);

        // Invariant PO-C2-3: severe must strictly exceed pressured so the
        // classifier has a non-empty "only pressured" band. When
        // distribution happens to be so narrow that both quantiles equal,
        // fall back to static — preserves the ordering contract.
        if ls <= lp || ps <= pp {
            return None;
        }

        Some(EffectiveThresholds {
            pressured_load: lp,
            severe_load: ls,
            pressured_psi: pp,
            severe_psi: ps,
        })
    }

    fn telemetry(&self, cfg: &GovernorConfig) -> CalibrationTelemetry {
        CalibrationTelemetry {
            mode: cfg.calibration_mode,
            load_window_len: self.load.window.len(),
            psi_window_len: self.psi.window.len(),
            conformal_k: cfg.conformal_k,
            conformal_k_min: cfg.conformal_k_min,
            conformal_alpha_pressured: cfg.conformal_alpha_pressured,
            conformal_alpha_severe: cfg.conformal_alpha_severe,
            drift_reset_count: self.drift_reset_count,
            outliers_rejected: self.outliers_rejected,
            observations_total: self.observations_total,
            load_pressured_q: self
                .load
                .window
                .quantile(cfg.conformal_alpha_pressured, cfg.conformal_k_min),
            load_severe_q: self
                .load
                .window
                .quantile(cfg.conformal_alpha_severe, cfg.conformal_k_min),
            psi_pressured_q: self
                .psi
                .window
                .quantile(cfg.conformal_alpha_pressured, cfg.conformal_k_min),
            psi_severe_q: self
                .psi
                .window
                .quantile(cfg.conformal_alpha_severe, cfg.conformal_k_min),
        }
    }
}

/// Effective classifier thresholds for the current tick. In static mode
/// these are just copies of `GovernorConfig`'s static fields; in conformal
/// mode they are the dynamic quantiles from the calibration window.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
pub(crate) struct EffectiveThresholds {
    pub pressured_load: f32,
    pub severe_load: f32,
    pub pressured_psi: f32,
    pub severe_psi: f32,
}

impl EffectiveThresholds {
    /// Rebuild a config with these thresholds substituted, preserving
    /// every other field (min_capacity_pct, growth_ticks, tick, disabled,
    /// calibration_mode, drift_*). Used to feed `next_capacity` via the
    /// existing `cfg.is_severe/is_pressured` methods unchanged.
    fn apply_to(self, cfg: &GovernorConfig) -> GovernorConfig {
        GovernorConfig {
            max_load_per_core: self.pressured_load,
            severe_load_per_core: self.severe_load,
            max_psi_avg10: self.pressured_psi,
            severe_psi_avg10: self.severe_psi,
            ..*cfg
        }
    }
}

/// Calibration telemetry embedded in `GovernorTelemetry.calibration`.
/// Includes enough information for an operator to tell:
///  - whether conformal mode is active
///  - how full the calibration windows are
///  - what quantiles are currently being emitted
///  - whether drift has been detected recently
#[derive(Clone, Debug, serde::Serialize)]
pub(crate) struct CalibrationTelemetry {
    pub mode: CalibrationMode,
    pub load_window_len: usize,
    pub psi_window_len: usize,
    pub conformal_k: usize,
    pub conformal_k_min: usize,
    pub conformal_alpha_pressured: f32,
    pub conformal_alpha_severe: f32,
    pub drift_reset_count: u64,
    pub outliers_rejected: u64,
    pub observations_total: u64,
    pub load_pressured_q: Option<f32>,
    pub load_severe_q: Option<f32>,
    pub psi_pressured_q: Option<f32>,
    pub psi_severe_q: Option<f32>,
}

struct GovernorRuntimeState {
    recent_decisions: VecDeque<GovernorDecision>,
    last_snapshot: Option<HealthSnapshot>,
    last_reason: Option<GovernorDecisionReason>,
    calibration: Option<GovernorCalibration>,
}

impl GovernorRuntimeState {
    fn new(cfg: &GovernorConfig) -> Self {
        let calibration = match cfg.calibration_mode {
            CalibrationMode::Conformal => Some(GovernorCalibration::new(cfg)),
            CalibrationMode::Static => None,
        };
        Self {
            recent_decisions: VecDeque::with_capacity(TELEMETRY_DECISION_HISTORY),
            last_snapshot: None,
            last_reason: None,
            calibration,
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
            runtime: Mutex::new(GovernorRuntimeState::new(&cfg)),
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

    /// Fold the current tick's snapshot into the calibration window (if
    /// conformal mode is active) and return the `GovernorConfig` to use
    /// for the decision. In static mode this is identical to `self.cfg`;
    /// in conformal mode the four threshold fields are overridden by the
    /// current quantile estimates (or left at static values if the
    /// calibration isn't ready yet).
    fn apply_calibration(
        &self,
        snapshot: &HealthSnapshot,
        is_healthy_tick: bool,
    ) -> GovernorConfig {
        if self.cfg.calibration_mode == CalibrationMode::Static {
            return self.cfg;
        }
        let mut runtime = self
            .runtime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(cal) = runtime.calibration.as_mut() else {
            // This can happen if someone constructs a Governor with
            // CalibrationMode::Conformal but forgets to populate the
            // runtime calibration. Static fallback is always safe.
            return self.cfg;
        };
        match cal.observe_and_compute_thresholds(snapshot, is_healthy_tick, &self.cfg) {
            Some(effective) => effective.apply_to(&self.cfg),
            None => self.cfg,
        }
    }

    /// Apply one sampling tick. Split out from `run()` so unit tests can
    /// drive deterministic sequences through the decision machinery without
    /// spawning a background thread or sleeping.
    fn step_once(&self) {
        let snapshot = self.reader.snapshot();
        let prev = self.current_capacity.load(Ordering::Relaxed);
        let streak = self.healthy_streak.load(Ordering::Relaxed);

        // In conformal mode, first classify the tick under the STATIC
        // thresholds: this tells the calibration window whether the
        // sample came from a healthy or pressured regime. Feeding
        // pressured-regime samples into the calibration window would
        // teach the quantile "what pressure looks like" — exactly the
        // opposite of what we want. We never touch static-mode behaviour
        // here: if calibration is disabled, `effective_cfg` stays as
        // `self.cfg` and the decision path below is bit-identical to
        // pre-conformal builds.
        let static_is_pressured = snapshot.is_pressured(&self.cfg);
        let is_healthy_tick = !static_is_pressured;
        let effective_cfg = self.apply_calibration(&snapshot, is_healthy_tick);

        let (next, next_streak, reason) = next_capacity(prev, streak, &snapshot, &effective_cfg);

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

        // `unwrap_or_else(PoisonError::into_inner)` unconditionally yields the
        // guard: a poisoned `Mutex` still holds a valid `GovernorRuntimeState`
        // (its invariants are tick-local), so silently dropping telemetry
        // forever after a single panic-while-locked is strictly worse than
        // keeping it flowing. The `if let Ok` pattern we used before had
        // exactly that failure mode.
        let mut runtime = self
            .runtime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        drop(runtime);

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
        // Same poison-safe acquisition pattern as `step_once`: the runtime
        // state is tick-local, so reading the most-recent-committed history
        // after a panic-while-locked is strictly more useful than returning
        // an empty slice for the rest of the process lifetime.
        let runtime = self
            .runtime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let recent: Vec<_> = runtime.recent_decisions.iter().copied().collect();
        let last_snapshot = runtime.last_snapshot;
        let last_reason = runtime.last_reason;
        let calibration = runtime
            .calibration
            .as_ref()
            .map(|cal| cal.telemetry(&self.cfg));
        drop(runtime);
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
            calibration,
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
            // Conformal knobs default to `Static` so the existing tests
            // continue to exercise the original threshold policy unchanged.
            calibration_mode: CalibrationMode::Static,
            conformal_k: DEFAULT_CONFORMAL_K,
            conformal_k_min: DEFAULT_CONFORMAL_K_MIN,
            conformal_alpha_pressured: DEFAULT_CONFORMAL_ALPHA_PRESSURED,
            conformal_alpha_severe: DEFAULT_CONFORMAL_ALPHA_SEVERE,
            drift_delta: DEFAULT_DRIFT_DELTA,
            drift_lambda: DEFAULT_DRIFT_LAMBDA,
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
    fn telemetry_survives_mutex_poison() {
        // Regression: an earlier version used `if let Ok(guard) = lock()` /
        // `match Ok/Err` to access the runtime state, which silently dropped
        // every telemetry update for the rest of the process if any thread
        // panicked while holding the mutex. Switching to
        // `unwrap_or_else(PoisonError::into_inner)` means a single panic
        // cannot mute the governor forever.
        let gov = Arc::new(build_test_governor(
            cfg(),
            std::iter::repeat_n(pressured(), 4).collect(),
        ));
        // Poison the runtime mutex deliberately by panicking inside a
        // closure that holds the lock.
        {
            let poison_gov = Arc::clone(&gov);
            let handle = std::thread::spawn(move || {
                let _held = poison_gov
                    .runtime
                    .lock()
                    .expect("fresh mutex should not be poisoned");
                panic!("intentional poison for regression test");
            });
            let _ = handle.join();
        }
        assert!(
            gov.runtime.is_poisoned(),
            "mutex must be poisoned after the helper thread's panic"
        );

        // Now drive the sampler. If the old `if let Ok(...)` guard were
        // still in place, none of these ticks would be recorded.
        for _ in 0..4 {
            gov.step_once();
        }

        let tele = gov.telemetry();
        assert_eq!(
            tele.ticks_total, 4,
            "atomics update regardless of mutex state"
        );
        assert!(
            !tele.recent_decisions.is_empty(),
            "telemetry must continue to record after mutex poison, got: {tele:?}"
        );
        assert_eq!(
            tele.recent_decisions.first().unwrap().reason,
            GovernorDecisionReason::Pressured,
            "first recorded decision after poison should still classify correctly"
        );
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

    // -----------------------------------------------------------------
    // Conformal-calibration tests (Card 2). All of these exercise
    // pure-function helpers (quantile, MAD, Page-Hinkley) plus the
    // Governor::step_once integration; the 17 static-mode tests above
    // must also stay green (PO-C2-2 bit-exact compatibility).
    // -----------------------------------------------------------------

    #[test]
    fn conformal_quantile_index_matches_vovk_theorem_1() {
        // Theorem 1 formula: index = ⌈(K+1)(1-α)⌉ - 1, clamped to [0, K-1].
        let sorted: Vec<f32> = (0..256).map(|i| i as f32).collect();
        // K=256, α=0.05 → ⌈257·0.95⌉-1 = 245-1 = 244-1 = 243
        //   ceil(244.15) = 245, so index = 244... let's compute directly:
        //   (256+1)*(1-0.05) = 244.15 → ceil = 245 → -1 = 244
        // So the returned value should be sorted[244] = 244.0.
        let q = conformal_quantile_sorted(&sorted, 0.05).unwrap();
        assert!(
            (q - 244.0).abs() < 1e-6,
            "K=256, α=0.05 expected sorted[244]=244 but got {q}"
        );

        // Tighter α should yield a higher quantile.
        let q_tight = conformal_quantile_sorted(&sorted, 0.01).unwrap();
        assert!(q_tight > q, "α=0.01 must produce q̂ ≥ α=0.05 q̂");
    }

    #[test]
    fn conformal_quantile_clamps_to_last_element_on_tiny_window() {
        let sorted = [0.0_f32, 1.0, 2.0, 3.0, 4.0];
        // K=5, α=0.01 → ⌈6·0.99⌉-1 = 6-1 = 5 → clamped to 4 (last idx).
        let q = conformal_quantile_sorted(&sorted, 0.01).unwrap();
        assert_eq!(q, 4.0);
    }

    #[test]
    fn conformal_quantile_rejects_pathological_alpha() {
        let sorted = [1.0_f32, 2.0, 3.0];
        assert!(conformal_quantile_sorted(&sorted, 0.0).is_none());
        assert!(conformal_quantile_sorted(&sorted, 1.0).is_none());
        assert!(conformal_quantile_sorted(&sorted, f32::NAN).is_none());
    }

    #[test]
    fn conformal_coverage_on_iid_uniform_meets_guarantee() {
        // Classical split-conformal validation: generate N iid samples,
        // calibrate q̂ on K of them, test on the remaining. Observed
        // coverage should be within sqrt(α(1-α)/N) of the guaranteed
        // 1-α floor.
        let mut stream = CalibrationStream::new(256);
        // Deterministic "uniform" via halton-like sequence so the test
        // is not dependent on a PRNG seed.
        for i in 0..256 {
            let v = (i as f32) * 0.0039; // 0 to ~1.0 uniformly
            stream.push(v);
        }
        let q = stream.quantile(0.05, 32).unwrap();
        // Empirical coverage on a fresh test set of 1024 identically
        // distributed values.
        let mut covered = 0usize;
        let test_n = 1024;
        for i in 0..test_n {
            let v = (i as f32) * (1.0 / test_n as f32);
            if v <= q {
                covered += 1;
            }
        }
        let coverage = covered as f32 / test_n as f32;
        // Target 1-α = 0.95, finite-sample slack ≈ 3σ ≈ 0.02 for N=1024.
        // We assert coverage within [0.90, 1.00] to stay robust to the
        // halton-sequence non-iid pattern while still catching a real
        // breakage of the quantile formula.
        assert!(
            (0.90..=1.00).contains(&coverage),
            "observed coverage {coverage} outside [0.90, 1.00] window"
        );
    }

    #[test]
    fn mad_rejects_obvious_outlier_on_stationary_stream() {
        let mut stream = CalibrationStream::new(64);
        for _ in 0..32 {
            stream.push(1.0);
        }
        for _ in 0..32 {
            stream.push(1.2);
        }
        // median ≈ 1.1, MAD ≈ 0.1 → reject anything > 1.1 + 3.5·0.1 = 1.45
        assert!(stream.is_outlier(10.0));
        assert!(stream.is_outlier(1.5));
        assert!(!stream.is_outlier(1.15));
    }

    #[test]
    fn mad_is_not_an_outlier_on_empty_window() {
        let stream = CalibrationStream::new(16);
        // Empty window has no median/MAD; nothing can be an outlier yet.
        assert!(!stream.is_outlier(100.0));
    }

    #[test]
    fn page_hinkley_does_not_trip_on_stationary_stream() {
        let mut ph = PageHinkley::new(0.01, 0.5);
        // 10 000 samples drawn from roughly-stationary N(0, 0.01).
        // Deterministic pseudo-random via LCG so test is reproducible.
        let mut state: u32 = 12345;
        let mut trips = 0;
        for _ in 0..10_000 {
            state = state.wrapping_mul(1103515245).wrapping_add(12345);
            let r = (state as f32 / u32::MAX as f32) * 0.02 - 0.01; // [-0.01, 0.01]
            if ph.observe(r) {
                trips += 1;
                ph.reset();
            }
        }
        // With δ=0.01 the stream barely accumulates drift; stationary
        // should trip ≤ 100 times per 10 000 (< 1 %).
        assert!(
            trips < 100,
            "page-hinkley tripped {trips} times on stationary stream (expected < 100)"
        );
    }

    #[test]
    fn page_hinkley_trips_on_clear_mean_shift() {
        let mut ph = PageHinkley::new(0.01, 0.5);
        // Phase 1: 500 zero-mean samples.
        for _ in 0..500 {
            ph.observe(0.0);
        }
        // Phase 2: mean shifts to +0.5 and stays there.
        let mut trips_in_phase_2 = 0;
        for _ in 0..500 {
            if ph.observe(0.5) {
                trips_in_phase_2 += 1;
                // reset so we can confirm detection lag is short
                ph.reset();
                break;
            }
        }
        assert!(
            trips_in_phase_2 >= 1,
            "page-hinkley missed a clear 0.5-magnitude mean shift"
        );
    }

    fn conformal_cfg() -> GovernorConfig {
        GovernorConfig {
            calibration_mode: CalibrationMode::Conformal,
            conformal_k: 64,
            conformal_k_min: 16,
            conformal_alpha_pressured: 0.05,
            conformal_alpha_severe: 0.01,
            drift_delta: 0.01,
            drift_lambda: 0.5,
            ..cfg()
        }
    }

    #[test]
    fn conformal_mode_static_behavior_until_k_min_reached() {
        // Before the window is full enough, the governor should behave
        // exactly like static mode. Drive it through healthy ticks that
        // are under the STATIC pressured threshold (load=0.1 < 1.25),
        // so even though they're below threshold they populate the
        // window. We should see zero shrinks during warm-up.
        let script = std::iter::repeat_n(healthy(), 20).collect();
        let gov = build_test_governor(conformal_cfg(), script);
        for _ in 0..20 {
            gov.step_once();
        }
        let tele = gov.telemetry();
        assert_eq!(
            tele.shrink_count, 0,
            "no shrinks expected during healthy-only warm-up"
        );
        // Calibration telemetry should be present and should report the
        // window filling up.
        let cal = tele
            .calibration
            .expect("conformal mode must emit calibration telemetry");
        assert_eq!(cal.mode, CalibrationMode::Conformal);
        assert!(cal.load_window_len > 0);
    }

    #[test]
    fn conformal_mode_never_inverts_severe_vs_pressured_invariant() {
        // PO-C2-3: when `observe_and_compute_thresholds` returns
        // `Some(effective)`, the severe thresholds must strictly exceed
        // the pressured thresholds. We exercise this contract directly
        // on `GovernorCalibration` because the governor-level shape of
        // the invariant depends on whether ANY samples reach the top
        // quantile bins, which is an artifact of sample count × α.
        let cfg_conf = conformal_cfg();
        let mut cal = GovernorCalibration::new(&cfg_conf);
        // Stationary pseudo-random LCG over [0.05, 1.05]: same mean and
        // variance throughout, so Page-Hinkley sees no drift. Enough
        // unique values that the α=0.01 and α=0.05 quantiles pick
        // different sorted positions.
        let mut state: u32 = 987654321;
        for _ in 0..96 {
            state = state.wrapping_mul(1103515245).wrapping_add(12345);
            let v = 0.05 + (state as f32 / u32::MAX as f32);
            let snap = HealthSnapshot {
                load_per_core: Some(v),
                psi_cpu_some_avg10: Some(v * 10.0),
            };
            let thresholds = cal.observe_and_compute_thresholds(&snap, true, &cfg_conf);
            // When thresholds are emitted, they must satisfy the
            // invariant; when they are None (still warming up or
            // degenerate), the governor falls back to static, which is
            // also safe.
            if let Some(t) = thresholds {
                assert!(
                    t.severe_load > t.pressured_load,
                    "PO-C2-3 violated for load: severe {} !> pressured {}",
                    t.severe_load,
                    t.pressured_load
                );
                assert!(
                    t.severe_psi > t.pressured_psi,
                    "PO-C2-3 violated for psi: severe {} !> pressured {}",
                    t.severe_psi,
                    t.pressured_psi
                );
            }
        }
        // At the end we should have emitted at least once — otherwise
        // the test isn't exercising the invariant.
        let tele = cal.telemetry(&cfg_conf);
        assert!(
            tele.load_pressured_q.is_some() && tele.load_severe_q.is_some(),
            "expected both load quantiles to be emitted by the end of the loop"
        );
    }

    #[test]
    fn conformal_mode_falls_back_to_static_on_degenerate_window() {
        // When every sample is identical, both quantiles collide. PO-C2-3
        // requires the apply-calibration path to refuse rather than emit
        // an inverted pair — degenerate distributions fall back to the
        // static thresholds silently (safe behavior).
        let script = std::iter::repeat_n(healthy(), 80).collect();
        let gov = build_test_governor(conformal_cfg(), script);
        for _ in 0..80 {
            gov.step_once();
        }
        let tele = gov.telemetry();
        // Governor should have spent all 80 ticks on static thresholds —
        // no shrinks, no grows (except the built-in 3-tick healthy-streak
        // hold pattern which can trigger HealthyCeilingHold from 100).
        assert_eq!(tele.shrink_count, 0);
        // Calibration telemetry is present but the quantiles may or may
        // not be emitted; either way behaviour was static-safe.
        assert!(tele.calibration.is_some());
    }

    #[test]
    fn conformal_telemetry_serializes_with_calibration_block() {
        let script = std::iter::repeat_n(healthy(), 40).collect();
        let gov = build_test_governor(conformal_cfg(), script);
        for _ in 0..40 {
            gov.step_once();
        }
        let tele = gov.telemetry();
        let json = serde_json::to_string(&tele).expect("telemetry serialization");
        for key in [
            "calibration",
            "\"mode\":\"conformal\"",
            "load_window_len",
            "psi_window_len",
            "conformal_k",
            "drift_reset_count",
            "outliers_rejected",
        ] {
            assert!(
                json.contains(key),
                "expected JSON to contain `{key}`; got: {json}"
            );
        }
    }

    // -----------------------------------------------------------------
    // Decision-replay harness (P-M3): feed the SAME synthetic signal
    // trace through both static and conformal governors, count the
    // shrink/grow events each produces. This is the simplest way to
    // answer "does conformal behave worse than static on a realistic
    // idle trace?" without running a full cass index rebuild.
    // -----------------------------------------------------------------

    /// Generate a synthetic loadavg trace that mimics an idle dev box:
    /// baseline `~0.3 / core` with small Poisson-like spikes.
    fn idle_dev_box_trace(n: usize) -> Vec<HealthSnapshot> {
        let mut state: u32 = 42;
        (0..n)
            .map(|_| {
                // Deterministic PRNG; uniform [0, 1).
                state = state.wrapping_mul(1103515245).wrapping_add(12345);
                let u = (state as f32 / u32::MAX as f32).clamp(0.0, 0.9999);
                // Most ticks land around 0.3; rare spikes to ~1.0.
                let load = if u < 0.95 {
                    0.2 + u * 0.3
                } else {
                    0.9 + u * 0.2
                };
                HealthSnapshot {
                    load_per_core: Some(load),
                    psi_cpu_some_avg10: Some(load * 8.0),
                }
            })
            .collect()
    }

    fn run_replay(mut cfg: GovernorConfig, script: Vec<HealthSnapshot>) -> GovernorTelemetry {
        cfg.tick = Duration::from_millis(1);
        let gov = build_test_governor(cfg, script.clone());
        for _ in 0..script.len() {
            gov.step_once();
        }
        gov.telemetry()
    }

    #[test]
    fn conformal_vs_static_idle_dev_trace_is_not_materially_worse() {
        // Feed both governors the same 2 048-tick idle-dev trace and
        // compare shrink counts. An idle trace (load stays under the
        // static 1.25 threshold almost everywhere) should produce
        // ZERO static-mode shrinks; a well-calibrated conformal
        // governor should produce a small, bounded number of shrinks
        // matching its 5% false-positive target (~100 over 2 048 ticks
        // under adversarial exchangeable samples; typically far fewer
        // on a stationary trace).
        let script = idle_dev_box_trace(2_048);
        let static_cfg = GovernorConfig {
            calibration_mode: CalibrationMode::Static,
            ..cfg()
        };
        let conf_cfg = GovernorConfig {
            calibration_mode: CalibrationMode::Conformal,
            conformal_k: 256,
            conformal_k_min: 32,
            conformal_alpha_pressured: 0.05,
            conformal_alpha_severe: 0.01,
            drift_delta: 0.01,
            drift_lambda: 0.5,
            ..cfg()
        };

        let static_tele = run_replay(static_cfg, script.clone());
        let conformal_tele = run_replay(conf_cfg, script);

        // Hard floor: conformal must not produce 10× more shrinks than
        // static. Our 5% target means conformal has a distribution-free
        // guarantee of ≤ ~102 spurious shrinks on a worst-case
        // exchangeable 2 048-tick trace. If it blows through 10× that,
        // something is broken (bad K, bad α, broken quantile).
        eprintln!(
            "replay trace: static shrinks={}, conformal shrinks={}, \
             static grows={}, conformal grows={}",
            static_tele.shrink_count,
            conformal_tele.shrink_count,
            static_tele.grow_count,
            conformal_tele.grow_count,
        );
        assert!(
            conformal_tele.shrink_count <= 1024,
            "conformal shrink_count={} is more than 10x the α=0.05 theoretical FP budget — conformal calibration is misbehaving",
            conformal_tele.shrink_count
        );
    }

    #[test]
    fn conformal_vs_static_under_sustained_pressure_shrinks_similarly() {
        // Both policies should shrink aggressively once a severe-class
        // signal arrives. This catches the opposite failure: conformal
        // learning thresholds that are too permissive for actual load
        // spikes.
        let pressured_trace: Vec<HealthSnapshot> = std::iter::repeat_n(severe(), 128).collect();
        let static_cfg = GovernorConfig {
            calibration_mode: CalibrationMode::Static,
            ..cfg()
        };
        let conf_cfg = GovernorConfig {
            calibration_mode: CalibrationMode::Conformal,
            conformal_k: 256,
            conformal_k_min: 32,
            conformal_alpha_pressured: 0.05,
            conformal_alpha_severe: 0.01,
            drift_delta: 0.01,
            drift_lambda: 0.5,
            ..cfg()
        };

        let static_tele = run_replay(static_cfg, pressured_trace.clone());
        let conformal_tele = run_replay(conf_cfg, pressured_trace);

        // Both must aggressively drop capacity to the min_capacity floor.
        assert_eq!(static_tele.current_capacity_pct, cfg().min_capacity_pct);
        assert_eq!(
            conformal_tele.current_capacity_pct,
            cfg().min_capacity_pct,
            "conformal must not be more permissive than static under severe load"
        );
    }
}
