//! Pure classification of daemon runtime artifacts and searcher-cache outcomes.
//!
//! Bead: coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.15.2
//! ("Add daemon socket pidfile cache and stale-searcher recovery coverage").
//!
//! The report flagged daemon stale pidfile/socket behavior, an FD leak on a
//! `try_clone` failure, and a stale `SearcherManager` cache after an atomic
//! lexical publish. The common failure shape is the same: cass *looks* healthy
//! while it is actually attached to a ghost process or serving an old index
//! generation. The two truths this module pins:
//!
//! 1. **Daemon runtime artifacts are disposable runtime state, never archive
//!    data.** A stale lock file, a stale socket, or a hung daemon must be safe
//!    to reclaim/restart — the recommended action is always a runtime action
//!    (restart the daemon, clean the stale socket, reconnect), never a rebuild
//!    over, or any mutation of, the canonical archive. [`DaemonRuntimeState`]
//!    classifies the observed artifacts and [`safe_recovery`] yields that
//!    non-destructive action.
//! 2. **A searcher cache must reload before serving a newer generation.** After
//!    an atomic lexical publish the index generation changes; serving from the
//!    old cached generation is stale. [`SearcherCacheOutcome`] is the metric
//!    taxonomy that makes the difference between a real cache hit, a
//!    stale-generation miss that forced a reload, a reload failure, and a
//!    degraded fallback observable instead of all collapsing into one
//!    "cache_miss" counter.
//!
//! The classification itself is pure, side-effect-free logic over recorded
//! observations. The bounded read-only probe gathers those facts and the live
//! status/doctor surfaces serialize this contract without spawning a daemon or
//! mutating archive state.

use serde::{Deserialize, Serialize};

/// Stable schema version for the daemon-runtime-state wire format.
pub const DAEMON_RUNTIME_STATE_SCHEMA_VERSION: u32 = 1;

/// Classified state of a daemon's runtime artifacts (run-lock + socket +
/// liveness + served generation). Ordered `Ok`→worst so a fleet/rollup can take
/// the most-severe state with `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DaemonRuntimeState {
    /// Run-lock held by a live owner, socket connectable and responsive, and the
    /// served index generation is current.
    Ok,
    /// No run-lock and no socket: the daemon simply is not running. Not an
    /// error — an on-demand client may spawn one.
    NotRunning,
    /// The daemon is alive and responsive but serving an older index generation
    /// than the one published — the stale-searcher case after an atomic publish.
    GenerationSkew,
    /// A socket file/symlink exists but no live owner holds the run-lock: a
    /// leftover bind from a crashed daemon (disposable runtime state).
    StaleSocket,
    /// The run-lock file exists but is not held by a live process: a crash left
    /// it behind and it can be safely reclaimed (disposable runtime state).
    StaleLock,
    /// The socket connects but the daemon does not answer a liveness ping (or
    /// answers wrongly): a ghost/hung process bound to the socket.
    GhostProcess,
    /// A connect attempt exceeded its bound: the daemon is unresponsive (slow or
    /// hung); restart it.
    Unresponsive,
    /// The runtime state could not be determined (probe incomplete/skipped).
    Unknown,
}

impl DaemonRuntimeState {
    /// Stable kebab-case wire value (single source of truth; a unit test pins
    /// serde output to this).
    pub const fn as_str(self) -> &'static str {
        match self {
            DaemonRuntimeState::Ok => "ok",
            DaemonRuntimeState::NotRunning => "not-running",
            DaemonRuntimeState::GenerationSkew => "generation-skew",
            DaemonRuntimeState::StaleSocket => "stale-socket",
            DaemonRuntimeState::StaleLock => "stale-lock",
            DaemonRuntimeState::GhostProcess => "ghost-process",
            DaemonRuntimeState::Unresponsive => "unresponsive",
            DaemonRuntimeState::Unknown => "unknown",
        }
    }

    /// Whether the state reflects a leftover *runtime artifact* that is safe to
    /// reclaim/replace (a stale lock, stale socket, or a ghost/unresponsive
    /// process) — as opposed to a healthy daemon, an absent daemon, or a
    /// generation skew that a reload fixes. This is the linchpin distinction:
    /// runtime artifacts are disposable; archive data is never touched to clear
    /// one.
    pub const fn is_disposable_runtime_artifact(self) -> bool {
        matches!(
            self,
            DaemonRuntimeState::StaleSocket
                | DaemonRuntimeState::StaleLock
                | DaemonRuntimeState::GhostProcess
                | DaemonRuntimeState::Unresponsive
        )
    }

    /// Whether a client should be able to use the daemon as-is (only `Ok`).
    pub const fn is_usable(self) -> bool {
        matches!(self, DaemonRuntimeState::Ok)
    }
}

/// The facts a caller gathers about a daemon's runtime artifacts. All optional /
/// boolean so a partial probe is honest; the classifier never performs I/O.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DaemonRuntimeObservation {
    /// The daemon's unix socket path (for the diagnostic surface; not parsed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket_path: Option<String>,
    /// The daemon data dir, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_dir: Option<String>,
    /// Whether the run-lock file is present on disk.
    pub run_lock_present: bool,
    /// Whether the run-lock could be acquired (`Some(true)` = acquirable, so NO
    /// live owner holds it → stale; `Some(false)` = held by a live owner;
    /// `None` = not probed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_lock_acquirable: Option<bool>,
    /// Whether the socket file/symlink exists.
    pub socket_present: bool,
    /// Whether a connect to the socket succeeded (`None` = not attempted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket_connectable: Option<bool>,
    /// Whether the connect attempt hit its bound (timed out).
    pub connect_timed_out: bool,
    /// Whether the daemon answered a protocol liveness ping (`None` = not asked).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub responded_to_ping: Option<bool>,
    /// The index generation the daemon reports serving, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_generation: Option<u64>,
    /// The current published lexical index generation, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_generation: Option<u64>,
    /// The owning process id, when discoverable (advisory; for the diagnostic).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_pid: Option<u32>,
    /// Wall-clock heartbeat written by the daemon into its run-lock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_heartbeat_unix_ms: Option<u64>,
    /// Age of the daemon's last heartbeat in ms, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_heartbeat_age_ms: Option<u64>,
    /// The connection error string, when a connect failed (evidence).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connect_error: Option<String>,
    /// `true` when the probe was bounded out before completing — the classifier
    /// then refuses to over-claim and returns [`DaemonRuntimeState::Unknown`].
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub probe_incomplete: bool,
}

impl DaemonRuntimeObservation {
    /// Whether the run-lock is held by a live owner (not acquirable).
    fn lock_held_by_live(&self) -> bool {
        matches!(self.run_lock_acquirable, Some(false))
    }

    /// Whether the run-lock is present but stale (acquirable → no live owner).
    fn lock_is_stale(&self) -> bool {
        self.run_lock_present && matches!(self.run_lock_acquirable, Some(true))
    }

    /// Whether the served generation differs from the published one.
    ///
    /// Generation identifiers are opaque identities on some backends (for
    /// example, a digest of atomically-published metadata), so ordering them
    /// would miss stale readers whenever the newer digest happened to be
    /// numerically smaller.
    fn generation_is_stale(&self) -> bool {
        matches!(
            (self.daemon_generation, self.published_generation),
            (Some(served), Some(published)) if served != published
        )
    }
}

/// Classify a daemon's runtime artifacts into a [`DaemonRuntimeState`]. Pure;
/// precedence is deliberate and safety-first: an incomplete probe never claims
/// health, an unresponsive/ghost daemon outranks a "connected" reading, and a
/// generation skew is only reported for an otherwise-live, responsive daemon.
pub fn classify(obs: &DaemonRuntimeObservation) -> DaemonRuntimeState {
    if obs.probe_incomplete {
        return DaemonRuntimeState::Unknown;
    }

    // A connect that exceeded its bound: the daemon is unresponsive (slow/hung).
    // This outranks any other socket reading.
    if obs.connect_timed_out {
        return DaemonRuntimeState::Unresponsive;
    }

    let connected = matches!(obs.socket_connectable, Some(true));

    // Connected to a socket but the process does not answer a liveness ping (or
    // answers wrong): a ghost bound to the socket.
    if connected && matches!(obs.responded_to_ping, Some(false)) {
        return DaemonRuntimeState::GhostProcess;
    }

    // Connected, responsive, but serving an older generation than published: the
    // stale-searcher case a reload fixes.
    if connected && !matches!(obs.responded_to_ping, Some(false)) && obs.generation_is_stale() {
        return DaemonRuntimeState::GenerationSkew;
    }

    // Connected and responsive (or liveness not probed) and not behind: healthy.
    if connected && obs.lock_held_by_live() {
        return DaemonRuntimeState::Ok;
    }
    // Connected and responsive but lock-liveness not probed — still usable.
    if connected && obs.run_lock_acquirable.is_none() {
        return DaemonRuntimeState::Ok;
    }

    // Not connected from here on. A socket exists but no live owner holds the
    // run-lock: a leftover bind from a crashed daemon.
    if obs.socket_present && !obs.lock_held_by_live() && !connected {
        return DaemonRuntimeState::StaleSocket;
    }

    // A run-lock file is present but stale (no live owner).
    if obs.lock_is_stale() {
        return DaemonRuntimeState::StaleLock;
    }

    // No socket and no (live) lock: the daemon is simply not running.
    if !obs.socket_present && !obs.lock_held_by_live() {
        return DaemonRuntimeState::NotRunning;
    }

    DaemonRuntimeState::Unknown
}

/// A non-destructive recovery action for a daemon runtime state. The command is
/// always a runtime action — restart/reconnect/clean a stale socket — and never
/// rebuilds or mutates the canonical archive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonRecovery {
    /// Whether any action is needed (false for `Ok`/`NotRunning`).
    pub action_needed: bool,
    /// Whether the issue is a disposable runtime artifact (safe to reclaim).
    pub disposable_runtime_artifact: bool,
    /// The single safe next command, when one applies. Always a `cass` runtime
    /// command; never a rebuild/cleanup of the archive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Why this is the safe action.
    pub why: String,
}

/// The curated set of safe daemon-runtime commands a recovery may surface.
/// `cass daemon` starts a fresh daemon (cleaning a stale socket/lock on bind);
/// the status/health checks are read-only. None rebuilds or mutates the
/// canonical archive. The recovery-safety test pins every emitted command to
/// this allow-list.
#[cfg(test)]
const SAFE_DAEMON_COMMANDS: &[&str] = &["cass daemon", "cass status --json", "cass health --json"];

/// The safe recovery action for a classified daemon runtime state. A *stale*
/// socket/lock (no live owner) is cleaned by binding a fresh `cass daemon`; a
/// *live* but unresponsive/ghost daemon holds the lock, so there is no command
/// that force-replaces it — it idle-shuts-down or is terminated manually, then a
/// fresh daemon respawns on demand. No action ever touches the archive.
pub fn safe_recovery(state: DaemonRuntimeState) -> DaemonRecovery {
    match state {
        DaemonRuntimeState::Ok => DaemonRecovery {
            action_needed: false,
            disposable_runtime_artifact: false,
            command: None,
            why: "daemon is live, responsive, and serving the current generation".to_string(),
        },
        DaemonRuntimeState::NotRunning => DaemonRecovery {
            action_needed: false,
            disposable_runtime_artifact: false,
            command: None,
            why: "no daemon is running; an on-demand client will spawn one when needed".to_string(),
        },
        DaemonRuntimeState::GenerationSkew => DaemonRecovery {
            action_needed: true,
            disposable_runtime_artifact: false,
            command: Some("cass status --json".to_string()),
            why: "daemon is serving an older index generation than published; the searcher reloads \
                  to the published generation on its next bounded refresh — the canonical archive \
                  is untouched, so re-check that it caught up"
                .to_string(),
        },
        DaemonRuntimeState::StaleSocket => DaemonRecovery {
            action_needed: true,
            disposable_runtime_artifact: true,
            command: Some("cass daemon".to_string()),
            why: "a stale socket from a crashed daemon remains (disposable runtime state); a fresh \
                  `cass daemon` re-binds and cleans it on startup without touching any archive data \
                  (the on-demand client also auto-cleans it on next use)"
                .to_string(),
        },
        DaemonRuntimeState::StaleLock => DaemonRecovery {
            action_needed: true,
            disposable_runtime_artifact: true,
            command: Some("cass daemon".to_string()),
            why: "a stale run-lock from a crashed daemon remains (disposable runtime state); a fresh \
                  `cass daemon` reclaims it on startup, no archive change"
                .to_string(),
        },
        DaemonRuntimeState::GhostProcess => DaemonRecovery {
            action_needed: true,
            disposable_runtime_artifact: true,
            command: None,
            why: "a live process holds the socket but is not answering; it auto-shuts-down after \
                  its idle timeout (or can be terminated manually), then the next semantic query \
                  respawns a fresh daemon — runtime state only, the archive is untouched"
                .to_string(),
        },
        DaemonRuntimeState::Unresponsive => DaemonRecovery {
            action_needed: true,
            disposable_runtime_artifact: true,
            command: None,
            why: "the daemon did not answer within the bound; it idle-shuts-down or can be \
                  terminated, then a fresh daemon respawns on demand (no archive change)"
                .to_string(),
        },
        DaemonRuntimeState::Unknown => DaemonRecovery {
            action_needed: false,
            disposable_runtime_artifact: false,
            command: Some("cass status --json".to_string()),
            why: "the daemon probe did not complete; re-check status before acting".to_string(),
        },
    }
}

/// A fully classified daemon-runtime diagnostic: the observation, its state, and
/// the safe recovery. Serializes with stable snake_case fields + a schema
/// version, ready for the eventual status/doctor/fleet surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonRuntimeDiagnostic {
    pub schema_version: u32,
    pub state: DaemonRuntimeState,
    pub observation: DaemonRuntimeObservation,
    pub recovery: DaemonRecovery,
}

impl DaemonRuntimeDiagnostic {
    /// Classify an observation into a full diagnostic.
    pub fn from_observation(observation: DaemonRuntimeObservation) -> Self {
        let state = classify(&observation);
        let recovery = safe_recovery(state);
        Self {
            schema_version: DAEMON_RUNTIME_STATE_SCHEMA_VERSION,
            state,
            observation,
            recovery,
        }
    }
}

/// The outcome of a single searcher-cache lookup against the current published
/// generation. This is the metric taxonomy that keeps a stale-generation miss
/// distinct from an ordinary cold miss and a reload failure — so "serving stale
/// segments after an atomic publish" is observable, not hidden in one counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SearcherCacheOutcome {
    /// Served from cache; the cached generation matched the current one.
    Hit,
    /// An ordinary miss: nothing cached for the key, generation unchanged.
    ColdMiss,
    /// The published generation changed, so the cache was invalidated and this
    /// lookup missed because of the generation change (not a cold key).
    StaleGenerationMiss,
    /// A generation change forced a reader/cache reload, which then succeeded.
    ForcedReload,
    /// A reload was attempted but failed; the searcher must fall back rather than
    /// serve stale segments.
    ReloadFailure,
    /// Served in a degraded fallback mode after a cache/reload problem (e.g.
    /// lexical-only while semantic catches up).
    Fallback,
}

impl SearcherCacheOutcome {
    /// Stable kebab-case wire value.
    pub const fn as_str(self) -> &'static str {
        match self {
            SearcherCacheOutcome::Hit => "hit",
            SearcherCacheOutcome::ColdMiss => "cold-miss",
            SearcherCacheOutcome::StaleGenerationMiss => "stale-generation-miss",
            SearcherCacheOutcome::ForcedReload => "forced-reload",
            SearcherCacheOutcome::ReloadFailure => "reload-failure",
            SearcherCacheOutcome::Fallback => "fallback",
        }
    }

    /// Whether the lookup served correct, current results (hit or a successful
    /// forced reload). A stale-generation miss, reload failure, or fallback is
    /// not a trustworthy current-generation serve.
    pub const fn served_current(self) -> bool {
        matches!(
            self,
            SearcherCacheOutcome::Hit | SearcherCacheOutcome::ForcedReload
        )
    }
}

/// Inputs to the searcher-cache outcome decision, recorded by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheLookup {
    /// Whether the key was present in the cache.
    pub cache_hit: bool,
    /// The generation the cached entry was built for (if cached).
    pub cached_generation: Option<u64>,
    /// The current published generation at lookup time.
    pub current_generation: u64,
    /// Whether a reload was attempted because of a generation change.
    pub reload_attempted: bool,
    /// Whether that reload succeeded.
    pub reload_succeeded: bool,
    /// Whether the result was served in a degraded fallback mode.
    pub served_fallback: bool,
}

/// Classify a searcher-cache lookup into a [`SearcherCacheOutcome`]. Pure;
/// precedence: reload failure / fallback (degraded) outrank a nominal hit, a
/// generation change is a stale-generation miss (not a cold miss), and a
/// successful forced reload is reported distinctly from a plain hit.
pub fn classify_cache_outcome(lookup: &CacheLookup) -> SearcherCacheOutcome {
    // A reload that was attempted and failed must never read as a hit: it would
    // mean serving stale segments.
    if lookup.reload_attempted && !lookup.reload_succeeded {
        return SearcherCacheOutcome::ReloadFailure;
    }
    if lookup.served_fallback {
        return SearcherCacheOutcome::Fallback;
    }
    // A successful reload triggered by a generation change.
    if lookup.reload_attempted && lookup.reload_succeeded {
        return SearcherCacheOutcome::ForcedReload;
    }
    // A cache hit whose generation matches the current one.
    if lookup.cache_hit && lookup.cached_generation == Some(lookup.current_generation) {
        return SearcherCacheOutcome::Hit;
    }
    // A cached entry exists but for an older generation → stale-generation miss.
    if lookup.cache_hit
        && matches!(lookup.cached_generation, Some(g) if g != lookup.current_generation)
    {
        return SearcherCacheOutcome::StaleGenerationMiss;
    }
    // Nothing cached for this key.
    SearcherCacheOutcome::ColdMiss
}

/// Serializable per-outcome searcher-cache metric counters. Replaces a single
/// opaque "cache_miss" with the full outcome breakdown so an operator can see a
/// stale-generation-miss spike after a publish.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SearcherCacheMetrics {
    pub hit: u64,
    pub cold_miss: u64,
    pub stale_generation_miss: u64,
    pub forced_reload: u64,
    pub reload_failure: u64,
    pub fallback: u64,
}

impl SearcherCacheMetrics {
    /// Record one classified outcome.
    pub fn record(&mut self, outcome: SearcherCacheOutcome) {
        match outcome {
            SearcherCacheOutcome::Hit => self.hit += 1,
            SearcherCacheOutcome::ColdMiss => self.cold_miss += 1,
            SearcherCacheOutcome::StaleGenerationMiss => self.stale_generation_miss += 1,
            SearcherCacheOutcome::ForcedReload => self.forced_reload += 1,
            SearcherCacheOutcome::ReloadFailure => self.reload_failure += 1,
            SearcherCacheOutcome::Fallback => self.fallback += 1,
        }
    }

    /// Total lookups recorded.
    pub fn total(&self) -> u64 {
        self.hit
            + self.cold_miss
            + self.stale_generation_miss
            + self.forced_reload
            + self.reload_failure
            + self.fallback
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live_responsive(generation: u64) -> DaemonRuntimeObservation {
        DaemonRuntimeObservation {
            socket_path: Some("/tmp/cass-semantic.sock".to_string()),
            run_lock_present: true,
            run_lock_acquirable: Some(false), // held by a live owner
            socket_present: true,
            socket_connectable: Some(true),
            connect_timed_out: false,
            responded_to_ping: Some(true),
            daemon_generation: Some(generation),
            published_generation: Some(generation),
            ..Default::default()
        }
    }

    // --- daemon state classification --------------------------------------

    #[test]
    fn live_responsive_current_generation_is_ok() {
        let state = classify(&live_responsive(7));
        assert_eq!(state, DaemonRuntimeState::Ok);
        assert!(state.is_usable());
        assert!(!state.is_disposable_runtime_artifact());
        assert!(!safe_recovery(state).action_needed);
    }

    #[test]
    fn no_socket_no_lock_is_not_running_not_an_error() {
        let obs = DaemonRuntimeObservation {
            run_lock_present: false,
            run_lock_acquirable: Some(true),
            socket_present: false,
            socket_connectable: Some(false),
            ..Default::default()
        };
        let state = classify(&obs);
        assert_eq!(state, DaemonRuntimeState::NotRunning);
        let recovery = safe_recovery(state);
        assert!(!recovery.action_needed, "absent daemon is not an error");
        assert!(recovery.command.is_none());
    }

    #[test]
    fn socket_present_but_no_live_owner_is_stale_socket() {
        // Bind left behind by a crashed daemon: socket file exists, lock
        // acquirable (no live owner), connect fails.
        let obs = DaemonRuntimeObservation {
            run_lock_present: true,
            run_lock_acquirable: Some(true),
            socket_present: true,
            socket_connectable: Some(false),
            connect_error: Some("Connection refused".to_string()),
            ..Default::default()
        };
        let state = classify(&obs);
        assert_eq!(state, DaemonRuntimeState::StaleSocket);
        assert!(state.is_disposable_runtime_artifact());
    }

    #[test]
    fn lock_present_stale_without_socket_is_stale_lock() {
        let obs = DaemonRuntimeObservation {
            run_lock_present: true,
            run_lock_acquirable: Some(true),
            socket_present: false,
            socket_connectable: Some(false),
            ..Default::default()
        };
        let state = classify(&obs);
        assert_eq!(state, DaemonRuntimeState::StaleLock);
        assert!(state.is_disposable_runtime_artifact());
    }

    #[test]
    fn connected_but_unanswered_ping_is_ghost_process() {
        let obs = DaemonRuntimeObservation {
            run_lock_present: true,
            run_lock_acquirable: Some(false),
            socket_present: true,
            socket_connectable: Some(true),
            responded_to_ping: Some(false),
            ..Default::default()
        };
        let state = classify(&obs);
        assert_eq!(state, DaemonRuntimeState::GhostProcess);
        assert!(state.is_disposable_runtime_artifact());
    }

    #[test]
    fn connect_timeout_is_unresponsive_outranks_everything() {
        let obs = DaemonRuntimeObservation {
            run_lock_present: true,
            run_lock_acquirable: Some(false),
            socket_present: true,
            socket_connectable: Some(false),
            connect_timed_out: true,
            ..Default::default()
        };
        assert_eq!(classify(&obs), DaemonRuntimeState::Unresponsive);
    }

    #[test]
    fn daemon_behind_published_generation_is_generation_skew() {
        let mut obs = live_responsive(5);
        obs.published_generation = Some(9); // newer index published since the daemon loaded
        let state = classify(&obs);
        assert_eq!(state, DaemonRuntimeState::GenerationSkew);
        // The recovery restarts the daemon (reloads derived assets) and is
        // explicit that the archive is untouched.
        let recovery = safe_recovery(state);
        assert!(recovery.action_needed);
        assert!(!recovery.disposable_runtime_artifact);
        assert!(recovery.why.contains("archive is untouched"));
    }

    #[test]
    fn b7tb0_opaque_generation_mismatch_is_skew_even_when_new_identity_is_smaller() {
        let mut obs = live_responsive(9);
        obs.published_generation = Some(5);
        assert_eq!(classify(&obs), DaemonRuntimeState::GenerationSkew);
    }

    #[test]
    fn incomplete_probe_never_claims_health() {
        let mut obs = live_responsive(3);
        obs.probe_incomplete = true;
        assert_eq!(classify(&obs), DaemonRuntimeState::Unknown);
    }

    #[test]
    fn connected_without_lock_probe_is_usable() {
        let mut obs = live_responsive(4);
        obs.run_lock_acquirable = None; // lock liveness not probed
        assert_eq!(classify(&obs), DaemonRuntimeState::Ok);
    }

    // --- safety invariant: no recovery ever touches the archive -----------

    #[test]
    fn no_recovery_command_ever_mutates_the_archive() {
        let states = [
            DaemonRuntimeState::Ok,
            DaemonRuntimeState::NotRunning,
            DaemonRuntimeState::GenerationSkew,
            DaemonRuntimeState::StaleSocket,
            DaemonRuntimeState::StaleLock,
            DaemonRuntimeState::GhostProcess,
            DaemonRuntimeState::Unresponsive,
            DaemonRuntimeState::Unknown,
        ];
        // Forbidden: anything that rebuilds/deletes archive or derived assets, or
        // any destructive cleanup. Daemon recovery is runtime-only.
        let forbidden = [
            "index --full",
            "rebuild",
            "rm ",
            "--delete",
            "--purge",
            "reset",
            "drop",
            "models backfill",
        ];
        for state in states {
            let recovery = safe_recovery(state);
            if let Some(cmd) = &recovery.command {
                for bad in forbidden {
                    assert!(
                        !cmd.contains(bad),
                        "{}: recovery command {cmd:?} references a non-runtime/destructive op {bad:?}",
                        state.as_str()
                    );
                }
                assert!(
                    SAFE_DAEMON_COMMANDS.contains(&cmd.as_str()),
                    "{}: recovery command {cmd:?} is not on the safe daemon command allow-list",
                    state.as_str()
                );
            }
        }
    }

    #[test]
    fn disposable_runtime_artifacts_are_exactly_the_reclaimable_states() {
        assert!(DaemonRuntimeState::StaleSocket.is_disposable_runtime_artifact());
        assert!(DaemonRuntimeState::StaleLock.is_disposable_runtime_artifact());
        assert!(DaemonRuntimeState::GhostProcess.is_disposable_runtime_artifact());
        assert!(DaemonRuntimeState::Unresponsive.is_disposable_runtime_artifact());
        // Generation skew is NOT a disposable artifact (a reload fixes it), and
        // a healthy/absent daemon obviously is not.
        assert!(!DaemonRuntimeState::GenerationSkew.is_disposable_runtime_artifact());
        assert!(!DaemonRuntimeState::Ok.is_disposable_runtime_artifact());
        assert!(!DaemonRuntimeState::NotRunning.is_disposable_runtime_artifact());
    }

    // --- searcher cache outcome -------------------------------------------

    fn lookup() -> CacheLookup {
        CacheLookup {
            cache_hit: false,
            cached_generation: None,
            current_generation: 10,
            reload_attempted: false,
            reload_succeeded: false,
            served_fallback: false,
        }
    }

    #[test]
    fn cache_hit_current_generation_is_hit() {
        let mut l = lookup();
        l.cache_hit = true;
        l.cached_generation = Some(10);
        let outcome = classify_cache_outcome(&l);
        assert_eq!(outcome, SearcherCacheOutcome::Hit);
        assert!(outcome.served_current());
    }

    #[test]
    fn cached_older_generation_is_stale_generation_miss_not_cold() {
        let mut l = lookup();
        l.cache_hit = true;
        l.cached_generation = Some(7); // older than current 10
        let outcome = classify_cache_outcome(&l);
        assert_eq!(outcome, SearcherCacheOutcome::StaleGenerationMiss);
        assert!(
            !outcome.served_current(),
            "a stale-generation hit must not read as a current serve"
        );
    }

    #[test]
    fn nothing_cached_is_cold_miss() {
        assert_eq!(
            classify_cache_outcome(&lookup()),
            SearcherCacheOutcome::ColdMiss
        );
    }

    #[test]
    fn generation_change_forced_reload_is_reported_distinctly() {
        let mut l = lookup();
        l.reload_attempted = true;
        l.reload_succeeded = true;
        let outcome = classify_cache_outcome(&l);
        assert_eq!(outcome, SearcherCacheOutcome::ForcedReload);
        assert!(outcome.served_current());
    }

    #[test]
    fn reload_failure_never_reads_as_a_serve() {
        let mut l = lookup();
        l.reload_attempted = true;
        l.reload_succeeded = false;
        l.cache_hit = true; // even with a cached entry present
        l.cached_generation = Some(10);
        let outcome = classify_cache_outcome(&l);
        assert_eq!(outcome, SearcherCacheOutcome::ReloadFailure);
        assert!(
            !outcome.served_current(),
            "a failed reload must not serve stale segments as current"
        );
    }

    #[test]
    fn degraded_fallback_outranks_a_nominal_hit() {
        let mut l = lookup();
        l.cache_hit = true;
        l.cached_generation = Some(10);
        l.served_fallback = true;
        assert_eq!(classify_cache_outcome(&l), SearcherCacheOutcome::Fallback);
    }

    #[test]
    fn metrics_record_every_outcome_distinctly() {
        let mut m = SearcherCacheMetrics::default();
        for outcome in [
            SearcherCacheOutcome::Hit,
            SearcherCacheOutcome::Hit,
            SearcherCacheOutcome::ColdMiss,
            SearcherCacheOutcome::StaleGenerationMiss,
            SearcherCacheOutcome::ForcedReload,
            SearcherCacheOutcome::ReloadFailure,
            SearcherCacheOutcome::Fallback,
        ] {
            m.record(outcome);
        }
        assert_eq!(m.hit, 2);
        assert_eq!(m.cold_miss, 1);
        assert_eq!(m.stale_generation_miss, 1);
        assert_eq!(m.forced_reload, 1);
        assert_eq!(m.reload_failure, 1);
        assert_eq!(m.fallback, 1);
        assert_eq!(m.total(), 7);
    }

    // --- serialization stability ------------------------------------------

    #[test]
    fn diagnostic_serializes_with_stable_fields_and_round_trips() {
        let diag = DaemonRuntimeDiagnostic::from_observation(live_responsive(12));
        let value = serde_json::to_value(&diag).expect("to_value");
        assert_eq!(value["schema_version"], DAEMON_RUNTIME_STATE_SCHEMA_VERSION);
        assert_eq!(value["state"], "ok");
        assert_eq!(value["recovery"]["action_needed"], false);
        assert_eq!(value["observation"]["socket_present"], true);
        let back: DaemonRuntimeDiagnostic = serde_json::from_value(value).expect("round-trip");
        assert_eq!(back, diag);
    }

    #[test]
    fn metrics_serialize_with_stable_snake_case_keys() {
        let mut m = SearcherCacheMetrics::default();
        m.record(SearcherCacheOutcome::StaleGenerationMiss);
        let value = serde_json::to_value(m).expect("to_value");
        assert_eq!(value["stale_generation_miss"], 1);
        assert_eq!(value["hit"], 0);
    }

    #[test]
    fn wire_labels_are_stable_kebab() {
        for (s, w) in [
            (DaemonRuntimeState::Ok, "ok"),
            (DaemonRuntimeState::NotRunning, "not-running"),
            (DaemonRuntimeState::GenerationSkew, "generation-skew"),
            (DaemonRuntimeState::StaleSocket, "stale-socket"),
            (DaemonRuntimeState::StaleLock, "stale-lock"),
            (DaemonRuntimeState::GhostProcess, "ghost-process"),
            (DaemonRuntimeState::Unresponsive, "unresponsive"),
            (DaemonRuntimeState::Unknown, "unknown"),
        ] {
            assert_eq!(serde_json::to_string(&s).expect("ser"), format!("\"{w}\""));
            assert_eq!(s.as_str(), w);
        }
        for (o, w) in [
            (SearcherCacheOutcome::Hit, "hit"),
            (SearcherCacheOutcome::ColdMiss, "cold-miss"),
            (
                SearcherCacheOutcome::StaleGenerationMiss,
                "stale-generation-miss",
            ),
            (SearcherCacheOutcome::ForcedReload, "forced-reload"),
            (SearcherCacheOutcome::ReloadFailure, "reload-failure"),
            (SearcherCacheOutcome::Fallback, "fallback"),
        ] {
            assert_eq!(serde_json::to_string(&o).expect("ser"), format!("\"{w}\""));
            assert_eq!(o.as_str(), w);
        }
    }

    #[test]
    fn state_ordering_is_ok_first_for_max_rollup() {
        // Ok is the floor of concern; a rollup `max` surfaces the worst state.
        assert!(DaemonRuntimeState::Ok < DaemonRuntimeState::StaleSocket);
        assert!(DaemonRuntimeState::Ok < DaemonRuntimeState::GhostProcess);
        let worst = [
            DaemonRuntimeState::Ok,
            DaemonRuntimeState::GenerationSkew,
            DaemonRuntimeState::Unresponsive,
        ]
        .into_iter()
        .max()
        .expect("non-empty");
        assert_eq!(worst, DaemonRuntimeState::Unresponsive);
    }
}
