//! Parallel-WAL shadow observer (Card 1, `§15.4 Silo/Aether` in the alien
//! graveyard). **This module does NOT change commit semantics.** Per the
//! design in `tests/artifacts/perf/2026-04-21-profile-run/ALIEN-ARTIFACT-CARD1-SPEC.md`
//! §5.7, the shadow-run is the mandatory first rollout stage: we run the
//! existing `persist_conversations_batched_begin_concurrent` path
//! unchanged, but instrument it so we can see what an epoch-ordered
//! group-commit path *would* do on the same workload.
//!
//! The goal at this stage is pure telemetry:
//!
//! * record when each writer chunk starts, ends, and how long it takes;
//! * note the "would-have-coalesced" boundaries where a parallel-WAL
//!   coordinator would have issued one combined epoch fsync instead of N;
//! * publish the numbers via `ParallelWalShadowTelemetry` so an
//!   operator can inspect them through `cass health --json`.
//!
//! Once we have 100+ consecutive shadow runs with stable numbers and no
//! surprises, the committing path can be written *on top of* this
//! observer — exactly the Shadow → Canary → Ramp → Default rollout the
//! spec demands. Until then, enabling this module costs only the shadow
//! counters' ~100 ns per chunk.
//!
//! Activation:
//! ```text
//! (unset)                             # DEFAULT: shadow observer ON
//! CASS_INDEXER_PARALLEL_WAL=shadow    # explicit shadow mode (same as default)
//! CASS_INDEXER_PARALLEL_WAL=off       # disable observer (zero overhead)
//! ```
//!
//! Any other value (including `on` / `commit`) is **rejected** at this
//! revision — the committing path is deliberately not exposed yet.

use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// One shadow-recorded chunk. Matches what the real parallel-WAL
/// coordinator would need to know: which chunk, who processed it, how
/// long it took, and whether any retry happened.
#[derive(Clone, Debug, serde::Serialize)]
pub(crate) struct ShadowChunkRecord {
    pub chunk_idx: usize,
    pub base_conv_idx: usize,
    pub convs_in_chunk: usize,
    pub wall_micros: u64,
    pub succeeded: bool,
}

/// Aggregate shadow telemetry. This is the payload we expose to
/// operators via `cass health --json.responsiveness.parallel_wal_shadow`.
#[derive(Clone, Debug, serde::Serialize)]
pub(crate) struct ParallelWalShadowTelemetry {
    /// Most-recent run's chunk records (FIFO, bounded at
    /// `MAX_SHADOW_RECORDS` so the struct stays small enough for a
    /// health payload).
    pub recent_chunks: Vec<ShadowChunkRecord>,
    /// Monotone: total number of shadow chunks observed since startup.
    pub chunks_observed: u64,
    /// Monotone: total wall-clock across observed chunks, in
    /// microseconds.
    pub cumulative_wall_micros: u64,
    /// Monotone: chunks that returned an error (observed but didn't
    /// commit in the current code path).
    pub chunk_errors: u64,
    /// Whether shadow mode is currently active.
    pub active: bool,
}

const MAX_SHADOW_RECORDS: usize = 64;

struct ShadowObserverState {
    recent_chunks: std::collections::VecDeque<ShadowChunkRecord>,
    chunks_observed: u64,
    cumulative_wall_micros: u64,
    chunk_errors: u64,
}

impl ShadowObserverState {
    fn new() -> Self {
        Self {
            recent_chunks: std::collections::VecDeque::with_capacity(MAX_SHADOW_RECORDS),
            chunks_observed: 0,
            cumulative_wall_micros: 0,
            chunk_errors: 0,
        }
    }

    fn record(&mut self, record: ShadowChunkRecord) {
        if self.recent_chunks.len() >= MAX_SHADOW_RECORDS {
            self.recent_chunks.pop_front();
        }
        self.cumulative_wall_micros = self
            .cumulative_wall_micros
            .saturating_add(record.wall_micros);
        if !record.succeeded {
            self.chunk_errors = self.chunk_errors.saturating_add(1);
        }
        self.recent_chunks.push_back(record);
        self.chunks_observed = self.chunks_observed.saturating_add(1);
    }
}

static OBSERVER: std::sync::LazyLock<Mutex<ShadowObserverState>> =
    std::sync::LazyLock::new(|| Mutex::new(ShadowObserverState::new()));

/// Parse the env var. Only three values are accepted; anything else is
/// treated as `off` (including `on`/`commit` which are intentionally NOT
/// wired up yet — the spec mandates shadow precedes commit).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ShadowMode {
    /// Observer is disabled; hot path is untouched.
    Off,
    /// Observer runs; per-chunk records are captured but no commit
    /// semantics change.
    Shadow,
}

pub(crate) fn mode_from_env() -> ShadowMode {
    // Default (env unset): Shadow — observer runs but no commit semantics
    // change. Explicit `off` disables it. `on` / `commit` are reserved
    // for a future revision that ships the committing path; they fall
    // back to Shadow so we never silently activate unbuilt code.
    match dotenvy::var("CASS_INDEXER_PARALLEL_WAL")
        .ok()
        .as_deref()
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("off" | "0" | "false" | "no" | "disable" | "disabled") => ShadowMode::Off,
        _ => ShadowMode::Shadow,
    }
}

/// Per-chunk guard returned by [`start_chunk`]. Records wall-clock on
/// drop; caller reports success via [`finish_ok`]/[`finish_err`] before
/// dropping for clearer telemetry.
pub(crate) struct ShadowChunkGuard {
    chunk_idx: usize,
    base_conv_idx: usize,
    convs_in_chunk: usize,
    started_at: Instant,
    succeeded: Option<bool>,
}

impl ShadowChunkGuard {
    pub fn finish_ok(mut self) {
        self.succeeded = Some(true);
    }

    pub fn finish_err(mut self) {
        self.succeeded = Some(false);
    }
}

impl Drop for ShadowChunkGuard {
    fn drop(&mut self) {
        let wall = self.started_at.elapsed();
        let record = ShadowChunkRecord {
            chunk_idx: self.chunk_idx,
            base_conv_idx: self.base_conv_idx,
            convs_in_chunk: self.convs_in_chunk,
            wall_micros: wall.as_micros().min(u64::MAX as u128) as u64,
            succeeded: self.succeeded.unwrap_or(false),
        };
        let mut state = OBSERVER.lock().unwrap_or_else(PoisonError::into_inner);
        state.record(record);
    }
}

/// Start a shadow chunk measurement. Cheap (one `Instant::now` +
/// struct init), and a no-op at the observer level when mode is `Off`.
pub(crate) fn start_chunk(
    chunk_idx: usize,
    base_conv_idx: usize,
    convs_in_chunk: usize,
) -> Option<ShadowChunkGuard> {
    if mode_from_env() == ShadowMode::Off {
        return None;
    }
    Some(ShadowChunkGuard {
        chunk_idx,
        base_conv_idx,
        convs_in_chunk,
        started_at: Instant::now(),
        succeeded: None,
    })
}

/// Snapshot the current shadow telemetry. Clones the bounded ring
/// buffer under the observer lock. Safe to call from any thread.
pub(crate) fn telemetry_snapshot() -> ParallelWalShadowTelemetry {
    let state = OBSERVER.lock().unwrap_or_else(PoisonError::into_inner);
    let active = mode_from_env() == ShadowMode::Shadow;
    ParallelWalShadowTelemetry {
        recent_chunks: state.recent_chunks.iter().cloned().collect(),
        chunks_observed: state.chunks_observed,
        cumulative_wall_micros: state.cumulative_wall_micros,
        chunk_errors: state.chunk_errors,
        active,
    }
}

/// Mean wall-clock per chunk in the recent window; returns `None` when
/// fewer than 2 samples have been recorded so the caller can decide
/// whether the number is meaningful yet.
///
/// Currently unused in production. Kept as part of the public surface
/// because the Card 1 commit-path implementation (next session) will
/// feed it into the controller that decides whether to attempt the
/// group-commit coalescing. Removing and re-adding would just be churn.
#[allow(dead_code)]
pub(crate) fn mean_chunk_wall() -> Option<Duration> {
    let state = OBSERVER.lock().unwrap_or_else(PoisonError::into_inner);
    if state.recent_chunks.len() < 2 {
        return None;
    }
    let sum_us: u128 = state
        .recent_chunks
        .iter()
        .map(|r| r.wall_micros as u128)
        .sum();
    let mean_us = sum_us / state.recent_chunks.len() as u128;
    Some(Duration::from_micros(mean_us as u64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn reset_observer() {
        let mut state = OBSERVER.lock().unwrap_or_else(PoisonError::into_inner);
        *state = ShadowObserverState::new();
    }

    #[test]
    #[serial]
    fn mode_parses_shadow_and_off() {
        // SAFETY: test-local env mutation; restored at end.
        let prior = std::env::var("CASS_INDEXER_PARALLEL_WAL").ok();
        // Explicit shadow
        unsafe {
            std::env::set_var("CASS_INDEXER_PARALLEL_WAL", "shadow");
        }
        assert_eq!(mode_from_env(), ShadowMode::Shadow);
        unsafe {
            std::env::set_var("CASS_INDEXER_PARALLEL_WAL", "SHADOW");
        }
        assert_eq!(mode_from_env(), ShadowMode::Shadow);
        // Explicit off — multiple forms all recognised.
        for off_form in ["off", "0", "false", "no", "OFF", "Disable"] {
            unsafe {
                std::env::set_var("CASS_INDEXER_PARALLEL_WAL", off_form);
            }
            assert_eq!(
                mode_from_env(),
                ShadowMode::Off,
                "`{off_form}` should disable the observer"
            );
        }
        // `on` / `commit` are reserved — current revision has no
        // committing path, so they fall through to Shadow rather than
        // silently activating unbuilt code.
        unsafe {
            std::env::set_var("CASS_INDEXER_PARALLEL_WAL", "on");
        }
        assert_eq!(mode_from_env(), ShadowMode::Shadow);
        unsafe {
            std::env::set_var("CASS_INDEXER_PARALLEL_WAL", "commit");
        }
        assert_eq!(mode_from_env(), ShadowMode::Shadow);
        // Unset == default Shadow (post-flip).
        unsafe {
            std::env::remove_var("CASS_INDEXER_PARALLEL_WAL");
        }
        assert_eq!(mode_from_env(), ShadowMode::Shadow);
        if let Some(v) = prior {
            unsafe {
                std::env::set_var("CASS_INDEXER_PARALLEL_WAL", v);
            }
        }
    }

    #[test]
    #[serial]
    fn start_chunk_returns_some_by_default_post_flip() {
        let prior = std::env::var("CASS_INDEXER_PARALLEL_WAL").ok();
        // SAFETY: test-local env mutation.
        unsafe {
            std::env::remove_var("CASS_INDEXER_PARALLEL_WAL");
        }
        // After the default flip, an unset env = shadow mode on = guard
        // returned. Explicit off disables the observer and returns None.
        let guard = start_chunk(0, 0, 1);
        assert!(guard.is_some(), "unset env must default to shadow on");
        drop(guard);
        unsafe {
            std::env::set_var("CASS_INDEXER_PARALLEL_WAL", "off");
        }
        assert!(start_chunk(0, 0, 1).is_none());
        unsafe {
            std::env::remove_var("CASS_INDEXER_PARALLEL_WAL");
        }
        if let Some(v) = prior {
            unsafe {
                std::env::set_var("CASS_INDEXER_PARALLEL_WAL", v);
            }
        }
    }

    #[test]
    #[serial]
    fn start_chunk_records_on_drop_in_shadow_mode() {
        let prior = std::env::var("CASS_INDEXER_PARALLEL_WAL").ok();
        reset_observer();
        // SAFETY: test-local env mutation.
        unsafe {
            std::env::set_var("CASS_INDEXER_PARALLEL_WAL", "shadow");
        }
        {
            let guard = start_chunk(0, 0, 10).expect("guard returned in shadow mode");
            // Simulate a little work.
            std::thread::sleep(Duration::from_micros(50));
            guard.finish_ok();
        }
        let tele = telemetry_snapshot();
        assert!(tele.active);
        assert_eq!(tele.chunks_observed, 1);
        assert_eq!(tele.recent_chunks.len(), 1);
        assert!(tele.recent_chunks[0].succeeded);
        assert!(tele.recent_chunks[0].wall_micros > 0);
        unsafe {
            std::env::remove_var("CASS_INDEXER_PARALLEL_WAL");
        }
        if let Some(v) = prior {
            unsafe {
                std::env::set_var("CASS_INDEXER_PARALLEL_WAL", v);
            }
        }
    }

    #[test]
    #[serial]
    fn ring_buffer_bounded_at_max_shadow_records() {
        let prior = std::env::var("CASS_INDEXER_PARALLEL_WAL").ok();
        reset_observer();
        // SAFETY: test-local env mutation.
        unsafe {
            std::env::set_var("CASS_INDEXER_PARALLEL_WAL", "shadow");
        }
        for i in 0..(MAX_SHADOW_RECORDS + 20) {
            let g = start_chunk(i, i * 5, 5).unwrap();
            g.finish_ok();
        }
        let tele = telemetry_snapshot();
        assert_eq!(tele.recent_chunks.len(), MAX_SHADOW_RECORDS);
        assert_eq!(tele.chunks_observed, (MAX_SHADOW_RECORDS + 20) as u64);
        unsafe {
            std::env::remove_var("CASS_INDEXER_PARALLEL_WAL");
        }
        if let Some(v) = prior {
            unsafe {
                std::env::set_var("CASS_INDEXER_PARALLEL_WAL", v);
            }
        }
    }

    #[test]
    #[serial]
    fn telemetry_serializes_to_json_with_expected_keys() {
        let prior = std::env::var("CASS_INDEXER_PARALLEL_WAL").ok();
        reset_observer();
        unsafe {
            std::env::set_var("CASS_INDEXER_PARALLEL_WAL", "shadow");
        }
        let g = start_chunk(7, 100, 32).unwrap();
        g.finish_err();
        let tele = telemetry_snapshot();
        let json = serde_json::to_string(&tele).unwrap();
        for key in [
            "recent_chunks",
            "chunks_observed",
            "cumulative_wall_micros",
            "chunk_errors",
            "active",
            "chunk_idx",
            "convs_in_chunk",
            "succeeded",
        ] {
            assert!(
                json.contains(key),
                "expected JSON to contain `{key}`: {json}"
            );
        }
        assert_eq!(tele.chunk_errors, 1);
        unsafe {
            std::env::remove_var("CASS_INDEXER_PARALLEL_WAL");
        }
        if let Some(v) = prior {
            unsafe {
                std::env::set_var("CASS_INDEXER_PARALLEL_WAL", v);
            }
        }
    }
}
