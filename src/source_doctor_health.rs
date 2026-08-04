//! Source/fleet doctor reachability and sync-health classification.
//!
//! Bead: coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.8.2
//! ("Add source/fleet doctor reachability and sync-health output").
//!
//! `cass sources doctor` / `fleet doctor` must report, per source, an explicit
//! health state covering the full diagnostic taxonomy — host reachability, the
//! remote `cass` binary, the source roots and data dir, sync/mirror/archive
//! state, and index health — and emit a *safe* next command, **without ever
//! mutating** anything during the diagnosis. Unreachable hosts must stay visible
//! as evidence gaps rather than dropping out of the report.
//!
//! This module is the **pure classification core**: it turns a
//! [`SourceDoctorObservation`] (gathered read-only by the caller — networked SSH
//! probing lives in the command layer and is always explicit) into an explicit
//! [`SourceDoctorState`] plus a preservation-safe next command, with a stable
//! JSON contract. Keeping it pure makes every scenario unit-testable with
//! fixtures and guarantees the diagnosis itself performs no I/O or mutation.
//!
//! It composes the host-reachability taxonomy from [`crate::fleet_doctor_schema`]
//! (bead 8.3 / 6.1) rather than re-deriving it, and follows the additive,
//! preservation-first remediation discipline from the remote-sync diagnostics
//! (bead 8.4): a suggested next command is never destructive.
//!
//! The human per-source projection deliberately remains native to this remote
//! source model. A remote probe can establish reachability, binary skew, and
//! sync/mirror state, but it cannot establish the controller's local SQLite,
//! lexical, or semantic readiness. Projecting a remote host into the local
//! derived-asset truth table would therefore invent facts. Human output instead
//! uses [`project_source_human_summary`] to render the same source and host state
//! codes, reason, and preservation-safe command carried by the robot report.

use crate::fleet_doctor_schema::{HostDoctorReport, HostProbeStatus, classify_connection_failure};
use serde::{Deserialize, Serialize};

/// Stable schema version for the source-doctor health wire format.
pub const SOURCE_DOCTOR_SCHEMA_VERSION: u32 = 1;

/// The explicit per-source doctor state. Covers the full diagnostic taxonomy the
/// bead enumerates; exactly one state is reported per source (the most severe
/// issue found), so a source is never silently shown as healthy when a deeper
/// problem exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceDoctorState {
    /// Reachable and healthy: host answered, binary present and current, source
    /// roots readable, index healthy, sync/mirror consistent.
    Reachable,
    /// Host could not be contacted at all (transport failure).
    Unreachable,
    /// Probe exceeded its time budget.
    Timeout,
    /// SSH authentication was denied.
    AuthDenied,
    /// Host reachable but the remote `cass` binary was not found.
    CassMissing,
    /// Remote `cass` is behind the required version.
    OldCass,
    /// Source root path could not be read on the host.
    SourceRootUnreadable,
    /// The remote source path was pruned/removed (preserve local evidence).
    RemotePruned,
    /// Local index exists but is stale relative to the source.
    StaleIndex,
    /// Lexical index metadata is missing (search degraded to fallback).
    MissingLexicalMetadata,
    /// Local archive/mirror is ahead of the remote (local has more).
    MirrorAhead,
    /// Remote is ahead of the local mirror (a sync would add coverage).
    MirrorBehind,
}

impl SourceDoctorState {
    /// Stable snake_case wire label.
    pub fn as_str(self) -> &'static str {
        match self {
            SourceDoctorState::Reachable => "reachable",
            SourceDoctorState::Unreachable => "unreachable",
            SourceDoctorState::Timeout => "timeout",
            SourceDoctorState::AuthDenied => "auth_denied",
            SourceDoctorState::CassMissing => "cass_missing",
            SourceDoctorState::OldCass => "old_cass",
            SourceDoctorState::SourceRootUnreadable => "source_root_unreadable",
            SourceDoctorState::RemotePruned => "remote_pruned",
            SourceDoctorState::StaleIndex => "stale_index",
            SourceDoctorState::MissingLexicalMetadata => "missing_lexical_metadata",
            SourceDoctorState::MirrorAhead => "mirror_ahead",
            SourceDoctorState::MirrorBehind => "mirror_behind",
        }
    }

    /// Whether the host answered at all (only the transport-failure states are
    /// "unreached"; every other state implies the host was contacted).
    pub fn host_reached(self) -> bool {
        !matches!(
            self,
            SourceDoctorState::Unreachable
                | SourceDoctorState::Timeout
                | SourceDoctorState::AuthDenied
        )
    }

    /// Whether this state is healthy (nothing to do).
    pub fn is_healthy(self) -> bool {
        matches!(self, SourceDoctorState::Reachable)
    }
}

/// Read-only observation of a single source, gathered by the command layer. All
/// fields are facts the diagnosis observed; no field implies a mutation.
#[derive(Debug, Clone, Default)]
pub struct SourceDoctorObservation {
    /// Source identifier (alias).
    pub source_id: String,
    /// SSH host alias, when this is a remote source.
    pub host: Option<String>,
    /// Whether the host could be contacted at all.
    pub host_reachable: bool,
    /// Connection error text when the host probe failed (classified for the
    /// transport taxonomy).
    pub connection_error: Option<String>,
    /// Whether the remote `cass` binary was found (when the host was reached).
    pub cass_present: Option<bool>,
    /// Whether the remote `cass` is at/above the required version.
    pub cass_current: Option<bool>,
    /// Whether the configured source root path was readable.
    pub source_root_readable: Option<bool>,
    /// Whether the remote source path was pruned/removed.
    pub remote_pruned: bool,
    /// Whether the local index is stale relative to the source.
    pub index_stale: bool,
    /// Whether lexical index metadata is present.
    pub lexical_metadata_present: Option<bool>,
    /// Local archive/mirror is ahead of the remote (local has more coverage).
    pub mirror_ahead: bool,
    /// Remote is ahead of the local mirror (a sync would add coverage).
    pub mirror_behind: bool,
}

/// Read-only outcome of the bounded remote `cass` binary probe for one source
/// (bead 8.7). The command layer runs a single bounded SSH round trip
/// (`uname` + `cass --version`) only against an already-reachable host; this
/// carries the resulting capability gap so the pure mapping never re-parses
/// version strings. A local source is the current binary by definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteBinaryOutcome {
    /// `cass` was not found on the host's PATH.
    Missing,
    /// `cass` is present and at/above the controller version (or only a minor
    /// patch behind — still operational; not flagged as old).
    Current,
    /// `cass` is present but a major capability gap behind — repair surfaces may
    /// be absent, so it is reported as old.
    Old,
    /// `cass` is present but its version could not be parsed/compared; presence
    /// is recorded, currency is left unknown rather than guessed.
    PresentUnknownVersion,
}

impl RemoteBinaryOutcome {
    /// Map a [`crate::fleet_version_skew::CapabilityGap`] (the existing pure
    /// version-skew assessment of the remote binary) onto the doctor outcome.
    pub fn from_capability_gap(gap: crate::fleet_version_skew::CapabilityGap) -> Self {
        use crate::fleet_version_skew::CapabilityGap;
        match gap {
            CapabilityGap::BinaryMissing => RemoteBinaryOutcome::Missing,
            // A minor/patch gap is still operational; only a major gap (missing
            // repair/doctor surfaces) is reported as `old_cass`.
            CapabilityGap::None | CapabilityGap::Minor => RemoteBinaryOutcome::Current,
            CapabilityGap::Major => RemoteBinaryOutcome::Old,
            CapabilityGap::Unknown => RemoteBinaryOutcome::PresentUnknownVersion,
        }
    }
}

/// Apply the remote-binary probe outcome onto an observation's `cass_present` /
/// `cass_current` fields (bead 8.7). Pure; never overclaims currency on an
/// unparseable version.
pub fn apply_remote_binary(obs: &mut SourceDoctorObservation, outcome: RemoteBinaryOutcome) {
    match outcome {
        RemoteBinaryOutcome::Missing => {
            obs.cass_present = Some(false);
        }
        RemoteBinaryOutcome::Current => {
            obs.cass_present = Some(true);
            obs.cass_current = Some(true);
        }
        RemoteBinaryOutcome::Old => {
            obs.cass_present = Some(true);
            obs.cass_current = Some(false);
        }
        RemoteBinaryOutcome::PresentUnknownVersion => {
            obs.cass_present = Some(true);
            // Currency unknown — leave `cass_current` at None so the classifier
            // does not report `old_cass` on a guess.
        }
    }
}

/// Read-only sync/mirror/index evidence for one source, gathered from
/// cass-owned LOCAL state only (the remote-path probe result already collected
/// for reachability, `sync_status.json`, the local mirror dir, and the archive
/// DB mtime). It never opens a fresh SSH session or mutates anything — it is the
/// pure input to [`apply_sync_evidence`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SourceSyncEvidence {
    /// The remote source path probe reported a real "does not exist" (a pruned
    /// path), distinct from an SSH transport failure.
    pub remote_path_missing: bool,
    /// The remote source path is reachable but now empty (data cleared upstream).
    pub remote_path_empty: bool,
    /// The local mirror/archive for this source still holds data.
    pub local_mirror_nonempty: bool,
    /// A sync has completed at least once (a `sync_status.json` record exists).
    pub has_sync_record: bool,
    /// The last recorded sync did not fully succeed (remote may hold newer data
    /// we could not pull).
    pub last_sync_incomplete: bool,
    /// The most recent sync finished after the archive index was last written
    /// (sessions synced but not yet indexed).
    pub index_behind_sync: bool,
}

/// Apply local sync/mirror/index evidence onto an observation's mirror, index,
/// and prune fields (bead 8.7). Pure and preservation-first: a vanished remote
/// path whose data we still hold locally is classified as `remote_pruned` (never
/// a local fault, never something to rebuild away).
pub fn apply_sync_evidence(obs: &mut SourceDoctorObservation, ev: &SourceSyncEvidence) {
    // Preservation signal: the remote source is gone but we retain its data.
    obs.remote_pruned = ev.remote_path_missing && ev.local_mirror_nonempty;
    // Local holds data the remote no longer does (remote emptied, mirror kept).
    obs.mirror_ahead = ev.remote_path_empty && ev.local_mirror_nonempty && !obs.remote_pruned;
    // An incomplete last sync means the remote likely still holds un-pulled data;
    // an additive sync would add coverage. Never reported over a prune.
    obs.mirror_behind = ev.last_sync_incomplete && !obs.remote_pruned;
    // Synced-but-not-indexed: the local index trails the last sync.
    obs.index_stale = ev.index_behind_sync;
}

/// Classify the host transport failure into one of the reachability states,
/// reusing the bead-8.3 connection classifier so the two surfaces agree.
fn reachability_state_from_error(error: &str) -> SourceDoctorState {
    match classify_connection_failure(error).0 {
        HostProbeStatus::TimedOut => SourceDoctorState::Timeout,
        HostProbeStatus::Unreachable => {
            // Distinguish auth denial from a pure transport failure for a more
            // actionable report (the classifier maps auth -> Unreachable).
            let lower = error.to_ascii_lowercase();
            if lower.contains("permission denied")
                || lower.contains("publickey")
                || lower.contains("authentication")
            {
                SourceDoctorState::AuthDenied
            } else {
                SourceDoctorState::Unreachable
            }
        }
        // Other host statuses are not transport failures at this layer.
        _ => SourceDoctorState::Unreachable,
    }
}

/// Classify a source observation into its most severe explicit doctor state.
/// Pure and side-effect free — the diagnosis never mutates. Precedence runs
/// from hard host failures down to soft coverage/index drift.
pub fn classify_source_doctor_state(obs: &SourceDoctorObservation) -> SourceDoctorState {
    // 1) Host transport: unreachable / timeout / auth denied win first.
    if !obs.host_reachable {
        return match obs.connection_error.as_deref() {
            Some(error) => reachability_state_from_error(error),
            None => SourceDoctorState::Unreachable,
        };
    }

    // 2) Remote binary problems.
    if obs.cass_present == Some(false) {
        return SourceDoctorState::CassMissing;
    }
    if obs.cass_current == Some(false) {
        return SourceDoctorState::OldCass;
    }

    // 3) Source path problems. A confirmed prune with locally retained data is
    // more specific than the generic unreadable-path signal produced by the
    // same missing-path probe, so preservation evidence wins.
    if obs.remote_pruned {
        return SourceDoctorState::RemotePruned;
    }
    if obs.source_root_readable == Some(false) {
        return SourceDoctorState::SourceRootUnreadable;
    }

    // 4) Index health.
    if obs.lexical_metadata_present == Some(false) {
        return SourceDoctorState::MissingLexicalMetadata;
    }
    if obs.index_stale {
        return SourceDoctorState::StaleIndex;
    }

    // 5) Coverage drift (least severe — additive sync resolves "behind").
    if obs.mirror_ahead {
        return SourceDoctorState::MirrorAhead;
    }
    if obs.mirror_behind {
        return SourceDoctorState::MirrorBehind;
    }

    SourceDoctorState::Reachable
}

/// The preservation-safe next command for a state. Per the additive-only
/// contract (bead 8.4), this is NEVER a destructive operation — no `--delete`,
/// no prune, no source-log mutation. `None` when the source is healthy.
pub fn safe_next_command(state: SourceDoctorState, source_id: &str) -> Option<String> {
    let cmd = match state {
        SourceDoctorState::Reachable => return None,
        SourceDoctorState::Unreachable | SourceDoctorState::Timeout => {
            format!(
                "cass sources doctor --source {source_id} --json   # retry when the host is reachable"
            )
        }
        SourceDoctorState::AuthDenied => {
            "ssh-add your key (or fix the identity file), then re-run cass sources doctor --json"
                .to_string()
        }
        SourceDoctorState::CassMissing => {
            format!("cass sources setup --source {source_id}   # install cass on the remote")
        }
        SourceDoctorState::OldCass => {
            format!(
                "cass sources setup --source {source_id} --upgrade   # bring the remote binary current"
            )
        }
        SourceDoctorState::SourceRootUnreadable => {
            format!(
                "verify the configured paths for '{source_id}' (cass sources list --json); fix permissions on the host"
            )
        }
        SourceDoctorState::RemotePruned => {
            "preserve the local archive/mirror; do NOT rebuild from the now-missing remote source"
                .to_string()
        }
        SourceDoctorState::StaleIndex => {
            "cass index --json   # refresh the local index".to_string()
        }
        SourceDoctorState::MissingLexicalMetadata => {
            "cass index --rebuild-lexical --json   # restore lexical metadata".to_string()
        }
        SourceDoctorState::MirrorAhead => {
            "local archive is ahead; inspect before any remote-backed rebuild (additive only)"
                .to_string()
        }
        SourceDoctorState::MirrorBehind => {
            format!(
                "cass sources sync --source {source_id} --json   # additive sync to add coverage"
            )
        }
    };
    Some(cmd)
}

/// A single source's doctor entry: identity, explicit state, host reachability
/// detail, and a safe next command. Always carries identity, even when the
/// source is unreachable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceDoctorEntry {
    /// Source identifier.
    pub source_id: String,
    /// SSH host alias, when remote.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Explicit doctor state.
    pub state: SourceDoctorState,
    /// Whether the host answered at all.
    pub host_reached: bool,
    /// Connection error text when the host probe failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connection_error: Option<String>,
    /// Preservation-safe next command, when action is needed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub safe_next_command: Option<String>,
}

impl SourceDoctorEntry {
    /// Project a read-only observation into a doctor entry.
    pub fn from_observation(obs: &SourceDoctorObservation) -> Self {
        let state = classify_source_doctor_state(obs);
        SourceDoctorEntry {
            source_id: obs.source_id.clone(),
            host: obs.host.clone(),
            state,
            host_reached: state.host_reached(),
            connection_error: obs.connection_error.clone(),
            safe_next_command: safe_next_command(state, &obs.source_id),
        }
    }
}

/// Bounded human projection of one source-doctor entry and its corresponding
/// fleet host report. This is intentionally not serialized: robot consumers
/// continue to receive the stable [`SourceDoctorReport`] plus diagnostics
/// schema, while the human surface gets a faithful, fixed-vocabulary view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceDoctorHumanSummary {
    /// `ready` only for the native healthy state; otherwise attention is needed.
    pub readiness: &'static str,
    /// Short, fixed-vocabulary description of the native source state.
    pub headline: &'static str,
    /// Whether the remote host answered the source probe.
    pub host_reached: bool,
    /// Fixed explanation for why the native state was selected.
    pub reason: &'static str,
    /// Stable source and host codes, kept together for human/robot parity.
    pub state_codes: String,
    /// The exact preservation-safe command carried by the robot source entry.
    pub safe_next_command: Option<String>,
}

impl SourceDoctorHumanSummary {
    /// Render at most six stable lines. No local search-readiness claim appears
    /// here because a remote source probe does not establish that fact.
    pub fn render_lines(&self) -> Vec<String> {
        let mut lines = vec![
            format!("Readiness: {}", self.readiness),
            format!("Source state: {}", self.headline),
            format!(
                "Host reached: {}",
                if self.host_reached { "yes" } else { "no" }
            ),
            format!("Why: {}", self.reason),
            format!("State codes: {}", self.state_codes),
        ];
        if let Some(command) = &self.safe_next_command {
            lines.push(format!("Next safe command: {command}"));
        }
        lines
    }
}

fn source_human_copy(state: SourceDoctorState) -> (&'static str, &'static str) {
    match state {
        SourceDoctorState::Reachable => (
            "no classified source issue",
            "the bounded probe found no required repair; optional maintenance and unobserved axes remain unknown",
        ),
        SourceDoctorState::Unreachable => (
            "source host unreachable",
            "the host could not be contacted; deeper state is unknown",
        ),
        SourceDoctorState::Timeout => (
            "source probe timed out",
            "the host did not answer within the bounded probe window",
        ),
        SourceDoctorState::AuthDenied => (
            "source authentication denied",
            "SSH authentication failed before source state could be inspected",
        ),
        SourceDoctorState::CassMissing => (
            "remote cass binary missing",
            "the host answered but cass was not found on its PATH",
        ),
        SourceDoctorState::OldCass => (
            "remote cass binary is too old",
            "the reported binary lacks the controller's required contract",
        ),
        SourceDoctorState::SourceRootUnreadable => (
            "source root unreadable",
            "the configured remote source path could not be read",
        ),
        SourceDoctorState::RemotePruned => (
            "remote source pruned",
            "the remote path is gone while local mirror evidence remains",
        ),
        SourceDoctorState::StaleIndex => (
            "local index trails the source",
            "newer sync evidence exists than the local archive index",
        ),
        SourceDoctorState::MissingLexicalMetadata => (
            "lexical metadata missing",
            "the source is reachable but its derived lexical metadata is absent",
        ),
        SourceDoctorState::MirrorAhead => (
            "local mirror ahead",
            "local archive evidence exceeds the current remote source",
        ),
        SourceDoctorState::MirrorBehind => (
            "local mirror behind",
            "the last sync was incomplete and the remote may hold more evidence",
        ),
    }
}

/// Project a source entry and its same-source host diagnostic into the bounded
/// human vocabulary. The exact robot safe command is preserved verbatim.
pub fn project_source_human_summary(
    entry: &SourceDoctorEntry,
    host: &HostDoctorReport,
) -> SourceDoctorHumanSummary {
    let (headline, reason) = source_human_copy(entry.state);
    SourceDoctorHumanSummary {
        readiness: if entry.state.is_healthy() {
            "no-action-observed"
        } else {
            "attention-required"
        },
        headline,
        host_reached: entry.host_reached,
        reason,
        state_codes: format!(
            "source={} host={}",
            entry.state.as_str(),
            host.status.as_str()
        ),
        safe_next_command: entry.safe_next_command.clone(),
    }
}

/// Aggregate rollup over the source entries. Unreachable sources are counted in
/// their own bucket and never folded into "healthy".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceDoctorSummary {
    /// Total sources diagnosed.
    pub total: usize,
    /// Healthy (reachable, nothing to do).
    pub healthy: usize,
    /// Reached but unhealthy (binary/index/source/coverage issues).
    pub unhealthy: usize,
    /// Not reached at all (unreachable/timeout/auth denied).
    pub unreached: usize,
}

/// The source/fleet doctor health report: every source plus a rollup. Carries
/// an explicit `mutation_free` marker: the diagnosis is pure observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceDoctorReport {
    /// Mirrors [`SOURCE_DOCTOR_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Per-source entries, in input order.
    pub sources: Vec<SourceDoctorEntry>,
    /// Aggregate rollup.
    pub summary: SourceDoctorSummary,
    /// Always `true`: producing this report performs no mutation.
    pub mutation_free: bool,
}

impl SourceDoctorReport {
    /// Build a report from read-only observations. The summary is a pure
    /// function of the entries; no I/O or mutation occurs.
    pub fn build(observations: &[SourceDoctorObservation]) -> Self {
        let sources: Vec<SourceDoctorEntry> = observations
            .iter()
            .map(SourceDoctorEntry::from_observation)
            .collect();

        let mut summary = SourceDoctorSummary {
            total: sources.len(),
            ..Default::default()
        };
        for entry in &sources {
            if !entry.state.host_reached() {
                summary.unreached += 1;
            } else if entry.state.is_healthy() {
                summary.healthy += 1;
            } else {
                summary.unhealthy += 1;
            }
        }

        Self {
            schema_version: SOURCE_DOCTOR_SCHEMA_VERSION,
            sources,
            summary,
            mutation_free: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reachable_healthy(id: &str) -> SourceDoctorObservation {
        SourceDoctorObservation {
            source_id: id.to_string(),
            host: Some(format!("user@{id}")),
            host_reachable: true,
            cass_present: Some(true),
            cass_current: Some(true),
            source_root_readable: Some(true),
            lexical_metadata_present: Some(true),
            ..Default::default()
        }
    }

    #[test]
    fn classifies_every_scenario_in_the_taxonomy() {
        // reachable / healthy
        assert_eq!(
            classify_source_doctor_state(&reachable_healthy("ok")),
            SourceDoctorState::Reachable
        );

        // unreachable (transport)
        let mut o = reachable_healthy("x");
        o.host_reachable = false;
        o.connection_error = Some("ssh: connect to host x: No route to host".to_string());
        assert_eq!(
            classify_source_doctor_state(&o),
            SourceDoctorState::Unreachable
        );

        // timeout
        let mut o = reachable_healthy("x");
        o.host_reachable = false;
        o.connection_error =
            Some("ssh: connect to host x port 22: Connection timed out".to_string());
        assert_eq!(classify_source_doctor_state(&o), SourceDoctorState::Timeout);

        // auth denied
        let mut o = reachable_healthy("x");
        o.host_reachable = false;
        o.connection_error = Some("Permission denied (publickey).".to_string());
        assert_eq!(
            classify_source_doctor_state(&o),
            SourceDoctorState::AuthDenied
        );

        // cass missing
        let mut o = reachable_healthy("x");
        o.cass_present = Some(false);
        assert_eq!(
            classify_source_doctor_state(&o),
            SourceDoctorState::CassMissing
        );

        // old cass
        let mut o = reachable_healthy("x");
        o.cass_current = Some(false);
        assert_eq!(classify_source_doctor_state(&o), SourceDoctorState::OldCass);

        // source root unreadable
        let mut o = reachable_healthy("x");
        o.source_root_readable = Some(false);
        assert_eq!(
            classify_source_doctor_state(&o),
            SourceDoctorState::SourceRootUnreadable
        );

        // remote pruned
        let mut o = reachable_healthy("x");
        o.remote_pruned = true;
        assert_eq!(
            classify_source_doctor_state(&o),
            SourceDoctorState::RemotePruned
        );

        // missing lexical metadata
        let mut o = reachable_healthy("x");
        o.lexical_metadata_present = Some(false);
        assert_eq!(
            classify_source_doctor_state(&o),
            SourceDoctorState::MissingLexicalMetadata
        );

        // stale index
        let mut o = reachable_healthy("x");
        o.index_stale = true;
        assert_eq!(
            classify_source_doctor_state(&o),
            SourceDoctorState::StaleIndex
        );

        // mirror ahead
        let mut o = reachable_healthy("x");
        o.mirror_ahead = true;
        assert_eq!(
            classify_source_doctor_state(&o),
            SourceDoctorState::MirrorAhead
        );

        // mirror behind
        let mut o = reachable_healthy("x");
        o.mirror_behind = true;
        assert_eq!(
            classify_source_doctor_state(&o),
            SourceDoctorState::MirrorBehind
        );
    }

    #[test]
    fn unreachable_host_severity_wins_over_other_signals() {
        // Even with index/mirror signals set, an unreachable host reports the
        // transport failure (deeper state is untrustworthy).
        let mut o = reachable_healthy("x");
        o.host_reachable = false;
        o.connection_error = Some("Connection timed out".to_string());
        o.index_stale = true;
        o.mirror_behind = true;
        assert_eq!(classify_source_doctor_state(&o), SourceDoctorState::Timeout);
    }

    #[test]
    fn safe_next_command_is_never_destructive() {
        let states = [
            SourceDoctorState::Reachable,
            SourceDoctorState::Unreachable,
            SourceDoctorState::Timeout,
            SourceDoctorState::AuthDenied,
            SourceDoctorState::CassMissing,
            SourceDoctorState::OldCass,
            SourceDoctorState::SourceRootUnreadable,
            SourceDoctorState::RemotePruned,
            SourceDoctorState::StaleIndex,
            SourceDoctorState::MissingLexicalMetadata,
            SourceDoctorState::MirrorAhead,
            SourceDoctorState::MirrorBehind,
        ];
        for state in states {
            if let Some(cmd) = safe_next_command(state, "laptop") {
                let lower = cmd.to_ascii_lowercase();
                for needle in [
                    "--delete",
                    "rm -rf",
                    "rm -r ",
                    "--remove-source-files",
                    "prune",
                    "shred",
                ] {
                    assert!(
                        !lower.contains(needle),
                        "state {state:?} suggested a destructive command: {cmd:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn report_counts_unreached_separately_and_is_mutation_free() {
        let mut pruned = reachable_healthy("retired");
        pruned.remote_pruned = true;
        let mut down = reachable_healthy("offline");
        down.host_reachable = false;
        down.connection_error = Some("No route to host".to_string());

        let report = SourceDoctorReport::build(&[reachable_healthy("good"), pruned, down]);
        assert!(report.mutation_free);
        assert_eq!(report.summary.total, 3);
        assert_eq!(report.summary.healthy, 1);
        assert_eq!(report.summary.unhealthy, 1); // pruned: reached but unhealthy
        assert_eq!(report.summary.unreached, 1); // offline
        // Unreachable never folds into healthy.
        assert_ne!(report.summary.healthy, report.summary.total);
    }

    #[test]
    fn entry_preserves_identity_for_unreachable_source() {
        let mut o = reachable_healthy("mac-mini-old");
        o.host_reachable = false;
        o.connection_error = Some(
            "ssh: Could not resolve hostname mac-mini-old: Name or service not known".to_string(),
        );
        let entry = SourceDoctorEntry::from_observation(&o);
        assert_eq!(entry.source_id, "mac-mini-old");
        assert_eq!(entry.host.as_deref(), Some("user@mac-mini-old"));
        assert!(!entry.host_reached);
        assert!(entry.connection_error.is_some());
        assert!(entry.safe_next_command.is_some());
        // DNS failure classifies as Unreachable at this layer.
        assert_eq!(entry.state, SourceDoctorState::Unreachable);
    }

    #[test]
    fn sources_doctor_human_summary_is_bounded_and_preserves_native_state_codes() {
        let mut old = reachable_healthy("legacy-node");
        old.cass_current = Some(false);
        let entry = SourceDoctorEntry::from_observation(&old);
        let host = HostDoctorReport::skeleton(
            "legacy-node",
            crate::fleet_doctor_schema::Platform::linux_x86_64(),
            HostProbeStatus::OldBinarySkew,
            12,
        );

        let summary = project_source_human_summary(&entry, &host);
        let lines = summary.render_lines();
        assert_eq!(summary.readiness, "attention-required");
        assert_eq!(summary.state_codes, "source=old_cass host=old-binary-skew");
        assert_eq!(summary.safe_next_command, entry.safe_next_command);
        assert_eq!(lines.len(), 6, "human source summary must stay bounded");
        assert_eq!(
            lines.last().map(String::as_str),
            Some(
                "Next safe command: cass sources setup --source legacy-node --upgrade   # bring the remote binary current"
            )
        );
        assert!(
            lines.iter().all(|line| !line.contains("Search usable now")),
            "remote source state must not fabricate local search readiness"
        );

        let mut unreachable = reachable_healthy("offline-node");
        unreachable.host_reachable = false;
        unreachable.connection_error = Some("No route to host".to_string());
        let entry = SourceDoctorEntry::from_observation(&unreachable);
        let host = HostDoctorReport::skeleton(
            "offline-node",
            crate::fleet_doctor_schema::Platform::linux_x86_64(),
            HostProbeStatus::Unreachable,
            50,
        );
        let summary = project_source_human_summary(&entry, &host);
        assert!(!summary.host_reached);
        assert_eq!(summary.state_codes, "source=unreachable host=unreachable");
        assert!(summary.render_lines().len() <= 6);

        let mut unknown_version = reachable_healthy("unknown-version");
        unknown_version.cass_current = None;
        let entry = SourceDoctorEntry::from_observation(&unknown_version);
        let host = HostDoctorReport::skeleton(
            "unknown-version",
            crate::fleet_doctor_schema::Platform::linux_x86_64(),
            HostProbeStatus::Ok,
            5,
        );
        let summary = project_source_human_summary(&entry, &host);
        assert_eq!(summary.readiness, "no-action-observed");
        assert_eq!(summary.headline, "no classified source issue");
        assert!(summary.reason.contains("no required repair"));
        assert!(summary.reason.contains("optional maintenance"));
        assert!(summary.reason.contains("unobserved axes remain unknown"));
        assert!(summary.safe_next_command.is_none());
        assert_eq!(summary.render_lines().len(), 5);

        let mut minor_gap = reachable_healthy("minor-gap");
        minor_gap.cass_present = None;
        minor_gap.cass_current = None;
        apply_remote_binary(
            &mut minor_gap,
            RemoteBinaryOutcome::from_capability_gap(
                crate::fleet_version_skew::CapabilityGap::Minor,
            ),
        );
        let entry = SourceDoctorEntry::from_observation(&minor_gap);
        let host = HostDoctorReport::skeleton(
            "minor-gap",
            crate::fleet_doctor_schema::Platform::linux_x86_64(),
            HostProbeStatus::Ok,
            5,
        );
        let summary = project_source_human_summary(&entry, &host);
        assert_eq!(summary.readiness, "no-action-observed");
        assert!(summary.reason.contains("optional maintenance"));
        assert!(summary.safe_next_command.is_none());
    }

    #[test]
    fn remote_binary_outcome_maps_capability_gap_without_overclaiming() {
        use crate::fleet_version_skew::CapabilityGap;
        assert_eq!(
            RemoteBinaryOutcome::from_capability_gap(CapabilityGap::BinaryMissing),
            RemoteBinaryOutcome::Missing
        );
        assert_eq!(
            RemoteBinaryOutcome::from_capability_gap(CapabilityGap::None),
            RemoteBinaryOutcome::Current
        );
        // A minor patch gap is still operational — not reported as old.
        assert_eq!(
            RemoteBinaryOutcome::from_capability_gap(CapabilityGap::Minor),
            RemoteBinaryOutcome::Current
        );
        assert_eq!(
            RemoteBinaryOutcome::from_capability_gap(CapabilityGap::Major),
            RemoteBinaryOutcome::Old
        );
        assert_eq!(
            RemoteBinaryOutcome::from_capability_gap(CapabilityGap::Unknown),
            RemoteBinaryOutcome::PresentUnknownVersion
        );
    }

    #[test]
    fn apply_remote_binary_drives_cass_missing_and_old_states() {
        // Missing binary -> cass_missing.
        let mut o = reachable_healthy("h");
        o.cass_present = None;
        o.cass_current = None;
        apply_remote_binary(&mut o, RemoteBinaryOutcome::Missing);
        assert_eq!(o.cass_present, Some(false));
        assert_eq!(
            classify_source_doctor_state(&o),
            SourceDoctorState::CassMissing
        );

        // Old binary -> old_cass.
        let mut o = reachable_healthy("h");
        apply_remote_binary(&mut o, RemoteBinaryOutcome::Old);
        assert_eq!(o.cass_present, Some(true));
        assert_eq!(o.cass_current, Some(false));
        assert_eq!(classify_source_doctor_state(&o), SourceDoctorState::OldCass);

        // Current binary -> healthy on that axis.
        let mut o = reachable_healthy("h");
        o.cass_present = None;
        o.cass_current = None;
        apply_remote_binary(&mut o, RemoteBinaryOutcome::Current);
        assert_eq!(o.cass_present, Some(true));
        assert_eq!(o.cass_current, Some(true));
        assert_eq!(
            classify_source_doctor_state(&o),
            SourceDoctorState::Reachable
        );

        // Unparseable version -> presence recorded, currency left unknown (no
        // false old_cass).
        let mut o = reachable_healthy("h");
        o.cass_present = None;
        o.cass_current = None;
        apply_remote_binary(&mut o, RemoteBinaryOutcome::PresentUnknownVersion);
        assert_eq!(o.cass_present, Some(true));
        assert_eq!(o.cass_current, None);
        assert_eq!(
            classify_source_doctor_state(&o),
            SourceDoctorState::Reachable
        );
    }

    #[test]
    fn apply_sync_evidence_prefers_preservation_for_pruned_remote() {
        // Remote path gone but local mirror retained -> remote_pruned, and it
        // wins over both the generic unreadable-path projection and any
        // mirror_behind signal (preservation first).
        let mut o = reachable_healthy("retired");
        o.source_root_readable = Some(false);
        apply_sync_evidence(
            &mut o,
            &SourceSyncEvidence {
                remote_path_missing: true,
                local_mirror_nonempty: true,
                last_sync_incomplete: true,
                ..Default::default()
            },
        );
        assert!(o.remote_pruned);
        assert!(!o.mirror_behind, "prune must suppress mirror_behind");
        assert!(!o.mirror_ahead);
        assert_eq!(
            classify_source_doctor_state(&o),
            SourceDoctorState::RemotePruned
        );

        // A vanished remote with no retained local data is NOT a prune we own.
        let mut o = reachable_healthy("h");
        apply_sync_evidence(
            &mut o,
            &SourceSyncEvidence {
                remote_path_missing: true,
                local_mirror_nonempty: false,
                ..Default::default()
            },
        );
        assert!(!o.remote_pruned);
    }

    #[test]
    fn apply_sync_evidence_drives_mirror_and_index_states() {
        // Remote emptied, local retains -> mirror_ahead.
        let mut o = reachable_healthy("h");
        apply_sync_evidence(
            &mut o,
            &SourceSyncEvidence {
                remote_path_empty: true,
                local_mirror_nonempty: true,
                ..Default::default()
            },
        );
        assert!(o.mirror_ahead);
        assert_eq!(
            classify_source_doctor_state(&o),
            SourceDoctorState::MirrorAhead
        );

        // Incomplete last sync -> mirror_behind.
        let mut o = reachable_healthy("h");
        apply_sync_evidence(
            &mut o,
            &SourceSyncEvidence {
                last_sync_incomplete: true,
                has_sync_record: true,
                ..Default::default()
            },
        );
        assert!(o.mirror_behind);
        assert_eq!(
            classify_source_doctor_state(&o),
            SourceDoctorState::MirrorBehind
        );

        // Index trails the last sync -> stale_index.
        let mut o = reachable_healthy("h");
        apply_sync_evidence(
            &mut o,
            &SourceSyncEvidence {
                index_behind_sync: true,
                has_sync_record: true,
                ..Default::default()
            },
        );
        assert!(o.index_stale);
        assert_eq!(
            classify_source_doctor_state(&o),
            SourceDoctorState::StaleIndex
        );

        // No drift signals -> nothing flipped.
        let mut o = reachable_healthy("h");
        apply_sync_evidence(&mut o, &SourceSyncEvidence::default());
        assert!(!o.mirror_ahead && !o.mirror_behind && !o.index_stale && !o.remote_pruned);
        assert_eq!(
            classify_source_doctor_state(&o),
            SourceDoctorState::Reachable
        );
    }

    #[test]
    fn json_contract_is_stable_and_round_trips() {
        let report = SourceDoctorReport::build(&[reachable_healthy("good")]);
        let value = serde_json::to_value(&report).expect("serialize");
        assert_eq!(value["schema_version"], SOURCE_DOCTOR_SCHEMA_VERSION);
        assert_eq!(value["mutation_free"], true);
        assert_eq!(value["sources"][0]["state"], "reachable");
        assert_eq!(value["sources"][0]["host_reached"], true);
        let back: SourceDoctorReport = serde_json::from_value(value).expect("deserialize");
        assert_eq!(back, report);
    }
}
