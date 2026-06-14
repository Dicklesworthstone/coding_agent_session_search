//! Bounded fleet-doctor JSON contract.
//!
//! Bead: coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.6.1
//! ("Define bounded fleet doctor probe schema").
//!
//! This module defines the wire contract for `cass`'s fleet doctor *before* the
//! probe implementation lands, so the producers (`6.2` cheap bounded probes,
//! `8.2` reachability/sync health) and consumers (`6.3` version skew, `6.4`
//! archive coverage, `9.2` root-cause projection, `10.4` per-host incident
//! rollups) all agree on one shape.
//!
//! The defining constraint is **boundedness without loss of host identity**: a
//! fleet sweep contacts many hosts, any of which may be slow, unreachable, or
//! running an old binary. The schema must therefore represent every outcome —
//! success, partial, timeout, old-binary skew, command-not-found, unreachable
//! SSH, macOS path/tool differences, and high archive risk — while *always*
//! preserving who the host is ([`HostDoctorReport::host_alias`] and
//! [`HostDoctorReport::platform`] are non-optional and survive every failure
//! mode). Deep state that could not be probed is `None`/empty and the omission
//! is recorded in [`HostDoctorReport::skipped_sections`], never silently dropped.
//!
//! Every field a diagnostic needs is structured and prose-free; a coarse,
//! optional [`RootCauseFamily`] hint composes with the attribution taxonomy from
//! bead `9.1` so `9.2` can project a likely root cause without changing this
//! contract.

use std::collections::BTreeMap;

use crate::root_cause_taxonomy::{
    AttributionConfidence, EvidenceRef, RootCauseAttribution, RootCauseFamily,
};
use serde::{Deserialize, Serialize};

/// Stable schema version for the fleet-doctor wire format. Bump only on a
/// breaking change to the field set or enum string values.
pub const FLEET_DOCTOR_SCHEMA_VERSION: u32 = 1;

/// The overall outcome of probing a single host. This is the rich discriminant;
/// the scalar [`HostDoctorReport::timed_out`] / [`HostDoctorReport::unreachable`]
/// flags mirror the timeout/unreachable cases for consumers that only branch on
/// booleans.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum HostProbeStatus {
    /// Every requested section was probed and the host is healthy.
    Ok,
    /// Some sections were probed; others were skipped or degraded (see
    /// [`HostDoctorReport::skipped_sections`]). Host identity and partial facts
    /// are present.
    Partial,
    /// The probe exceeded its time budget. Identity is present; deep state is
    /// whatever completed before the deadline.
    TimedOut,
    /// The host responded but its `cass` binary is behind the required
    /// contract/api version. [`HostDoctorReport::cass_version`] is populated.
    OldBinarySkew,
    /// The host was reachable but `cass` (or a required tool) was not found on
    /// `PATH`.
    CommandNotFound,
    /// The host could not be contacted at all (SSH/transport failure).
    Unreachable,
    /// The host was fully probed but is unhealthy (e.g. DB not ready, high
    /// archive risk) without a hard failure.
    Degraded,
}

impl HostProbeStatus {
    /// Stable kebab-case wire value (single source of truth; a unit test pins
    /// serde output to this).
    pub const fn as_str(self) -> &'static str {
        match self {
            HostProbeStatus::Ok => "ok",
            HostProbeStatus::Partial => "partial",
            HostProbeStatus::TimedOut => "timed-out",
            HostProbeStatus::OldBinarySkew => "old-binary-skew",
            HostProbeStatus::CommandNotFound => "command-not-found",
            HostProbeStatus::Unreachable => "unreachable",
            HostProbeStatus::Degraded => "degraded",
        }
    }

    /// `true` when the host yielded no deep state (unreachable / command-not-found):
    /// consumers should expect only identity fields to be populated.
    pub const fn is_hard_failure(self) -> bool {
        matches!(
            self,
            HostProbeStatus::Unreachable | HostProbeStatus::CommandNotFound
        )
    }
}

/// Host operating system family. Distinguishes macOS so consumers can account
/// for path and tooling differences (the recurring fleet heterogeneity issue).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum HostOs {
    Linux,
    /// Pinned to `"macos"` (not kebab `"mac-os"`) to match `std::env::consts::OS`
    /// and rustc target conventions.
    #[serde(rename = "macos")]
    MacOs,
    Windows,
    Other,
}

/// Filesystem path convention, so a Linux controller can correctly interpret a
/// macOS/Windows host's paths.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum PathStyle {
    Posix,
    Windows,
}

/// Stable host identity and environment shape. Always present, even for an
/// unreachable host (the controller knows *who* it failed to reach).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Platform {
    /// OS family.
    pub os: HostOs,
    /// CPU architecture (e.g. `"x86_64"`, `"aarch64"`). Free-form because the
    /// set is open; empty string when unknown.
    pub arch: String,
    /// Path convention for interpreting this host's paths.
    pub path_style: PathStyle,
    /// Structured notes about path/tool divergences from the controller's
    /// platform (e.g. `"rsync=bsd"`, `"coreutils=bsd"`, `"data_dir=~/Library"`).
    /// Prose-free key=value-ish tokens, not sentences.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_notes: Vec<String>,
}

impl Platform {
    /// A Linux/x86_64 POSIX host with no tool divergences — the common case.
    pub fn linux_x86_64() -> Self {
        Self {
            os: HostOs::Linux,
            arch: "x86_64".to_string(),
            path_style: PathStyle::Posix,
            tool_notes: Vec::new(),
        }
    }
}

/// What the host's `cass` binary can do, used to gate which probes are even
/// meaningful and to surface version/feature skew across the fleet.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum CapabilityTier {
    /// All features present (semantic, remote sync, robot envelopes, …).
    Full,
    /// Core search/index present; some optional features absent.
    Standard,
    /// Minimal/legacy binary; only basic commands available.
    Minimal,
    /// Could not be determined.
    Unknown,
}

/// Coarse DB / readiness state for the host's CASS store.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum ReadinessState {
    Ready,
    Degraded,
    NotReady,
    Unknown,
}

/// Semantic-search asset/state on the host.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum SemanticState {
    Enabled,
    Disabled,
    /// Enabled/requested but assets are missing or partial.
    AssetsMissing,
    Unknown,
}

/// Remote source-sync state for the host's mirrors.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum RemoteSyncState {
    Synced,
    Stale,
    NeverSynced,
    Failed,
    NotConfigured,
    Unknown,
}

/// Risk that derived/archive state is unrecoverable or diverging — drives the
/// "back this up / re-archive" recommendation. Ordered low→high so
/// [`FleetSummary::highest_archive_risk`] can take a `max`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum ArchiveRisk {
    Unknown,
    Low,
    Medium,
    High,
}

/// A configured source root on the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRoot {
    /// Path as it exists on the host (interpret via [`Platform::path_style`]).
    pub path: String,
    /// Detected agent kind for the root, if known (e.g. `"claude"`, `"codex"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Whether this root is backed by an archive (vs. live-only).
    pub archived: bool,
}

/// Aggregate source counts for the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SourceCounts {
    /// Number of configured source roots.
    pub roots: u64,
    /// Total sessions discovered across roots.
    pub sessions: u64,
    /// Sessions that are indexed/derived.
    pub indexed_sessions: u64,
}

/// Quarantine state for the host's store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct QuarantineState {
    /// Number of quarantined items.
    pub quarantined: u64,
    /// Of those, how many are eligible for automatic recovery.
    pub recoverable: u64,
}

/// The per-host fleet-doctor record. Identity fields ([`Self::host_alias`],
/// [`Self::platform`]) and the bounded scalars ([`Self::status`],
/// [`Self::elapsed_ms`], [`Self::timed_out`], [`Self::unreachable`],
/// [`Self::archive_risk`]) are always present; deep state is optional and absent
/// when not probed, with the omission named in [`Self::skipped_sections`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostDoctorReport {
    /// Mirrors [`FLEET_DOCTOR_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Stable host identity. ALWAYS present, including for unreachable hosts.
    pub host_alias: String,
    /// Host platform/identity. ALWAYS present.
    pub platform: Platform,
    /// Rich probe outcome.
    pub status: HostProbeStatus,
    /// Wall-clock the probe took, bounded by the host time budget.
    pub elapsed_ms: u64,
    /// `true` if the probe hit its deadline (mirrors [`HostProbeStatus::TimedOut`]).
    pub timed_out: bool,
    /// `true` if the host could not be contacted (mirrors
    /// [`HostProbeStatus::Unreachable`]).
    pub unreachable: bool,
    /// `true` if the probe was cancelled by the operator before it completed.
    /// A cancelled probe keeps [`Self::status`] = [`HostProbeStatus::Partial`]
    /// (incomplete, not a host fault) but is distinguishable via this flag.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cancelled: bool,
    /// `true` when the surfaced deep-state fields are last-known (stale) evidence
    /// carried from a prior successful probe rather than current truth — set when
    /// an intermittent host fails now but was reachable before.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stale_data: bool,
    /// Highest archive risk observed for this host. `Unknown` when not assessed.
    pub archive_risk: ArchiveRisk,
    /// Sections deliberately not probed or that failed to complete, by stable
    /// name (e.g. `"semantic"`, `"remote_sync"`). Makes partial/timeout results
    /// honest instead of indistinguishable from "all clear".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped_sections: Vec<String>,

    /// Running `cass` version string, when the binary answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cass_version: Option<String>,
    /// Binary capability tier, when determined.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability_tier: Option<CapabilityTier>,
    /// Resolved data dir on the host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_dir: Option<String>,
    /// Configured source roots.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_roots: Vec<SourceRoot>,
    /// Aggregate source counts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_counts: Option<SourceCounts>,
    /// DB / readiness state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readiness: Option<ReadinessState>,
    /// Semantic-asset state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic: Option<SemanticState>,
    /// Quarantine state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quarantine: Option<QuarantineState>,
    /// Remote sync state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_sync: Option<RemoteSyncState>,

    /// Optional coarse root-cause hint, composing with the bead-9.1 taxonomy.
    /// Populated by `9.2`; absent here keeps the two contracts decoupled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub likely_root_cause: Option<RootCauseFamily>,
    /// Full root-cause attribution for this host (bead 9.5 fleet slice):
    /// family + confidence + structured evidence + summary + next probe, in the
    /// same vocabulary status/doctor emit. Present whenever `likely_root_cause`
    /// is, so fleet consumers get the rich attribution, not just the family.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_cause: Option<RootCauseAttribution>,
    /// Recommended next action for an operator/agent (single, action-oriented).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommended_action: Option<String>,
    /// Specific connection/transport error when the probe failed (e.g. DNS
    /// resolution, auth rejection, connection refused, timeout, banner-exchange
    /// timeout). Preserved as evidence; host identity still stands regardless.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_error: Option<String>,
    /// Actionable retry hint for a failed or cancelled probe, or `None` when the
    /// host was reached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_hint: Option<String>,
}

impl HostDoctorReport {
    /// Construct an identity-only skeleton with the given status. All deep state
    /// is absent; callers fill in what they probe. Use this so host identity is
    /// established first and can never be lost on a later failure.
    pub fn skeleton(
        host_alias: impl Into<String>,
        platform: Platform,
        status: HostProbeStatus,
        elapsed_ms: u64,
    ) -> Self {
        Self {
            schema_version: FLEET_DOCTOR_SCHEMA_VERSION,
            host_alias: host_alias.into(),
            platform,
            status,
            elapsed_ms,
            timed_out: status == HostProbeStatus::TimedOut,
            unreachable: status == HostProbeStatus::Unreachable,
            cancelled: false,
            stale_data: false,
            archive_risk: ArchiveRisk::Unknown,
            skipped_sections: Vec::new(),
            cass_version: None,
            capability_tier: None,
            data_dir: None,
            source_roots: Vec::new(),
            source_counts: None,
            readiness: None,
            semantic: None,
            quarantine: None,
            remote_sync: None,
            likely_root_cause: None,
            root_cause: None,
            recommended_action: None,
            connection_error: None,
            retry_hint: None,
        }
    }

    /// Identity-only record for a host that could not be contacted. Preserves
    /// who the host is and records the recommended remediation.
    pub fn unreachable(
        host_alias: impl Into<String>,
        platform: Platform,
        elapsed_ms: u64,
        recommended_action: impl Into<String>,
    ) -> Self {
        let mut report =
            Self::skeleton(host_alias, platform, HostProbeStatus::Unreachable, elapsed_ms);
        report.likely_root_cause = Some(RootCauseFamily::RemoteTransportAuth);
        report.root_cause = Some(transport_root_cause_attribution(report.status, None));
        report.recommended_action = Some(recommended_action.into());
        report
    }

    /// Identity-only record for a host whose probe failed, classifying the
    /// connection error into an explicit [`HostProbeStatus`] (timeout vs.
    /// unreachable) and preserving the error text plus a retry hint. A DNS,
    /// auth, connection-refused, or host-key failure classifies as
    /// [`HostProbeStatus::Unreachable`]; a connect/banner timeout classifies as
    /// [`HostProbeStatus::TimedOut`]. Host identity always survives.
    pub fn failed(
        host_alias: impl Into<String>,
        platform: Platform,
        elapsed_ms: u64,
        connection_error: impl Into<String>,
    ) -> Self {
        let connection_error = connection_error.into();
        let (status, retry_hint) = classify_connection_failure(&connection_error);
        let mut report = Self::skeleton(host_alias, platform, status, elapsed_ms);
        report.likely_root_cause = Some(RootCauseFamily::RemoteTransportAuth);
        report.root_cause = Some(transport_root_cause_attribution(
            report.status,
            Some(&connection_error),
        ));
        report.recommended_action = Some(retry_hint.to_string());
        report.retry_hint = Some(retry_hint.to_string());
        report.connection_error = Some(connection_error);
        report
    }

    /// Identity-only record for a probe the operator cancelled before it
    /// completed. This is a human action, not a host fault, so it keeps
    /// [`HostProbeStatus::Partial`] (incomplete) and is flagged via
    /// [`Self::cancelled`] rather than counted as a transport failure.
    pub fn cancelled(host_alias: impl Into<String>, platform: Platform, elapsed_ms: u64) -> Self {
        let mut report =
            Self::skeleton(host_alias, platform, HostProbeStatus::Partial, elapsed_ms);
        report.cancelled = true;
        report.connection_error = Some("probe cancelled by operator".to_string());
        report.retry_hint = Some("rerun the fleet probe to collect fresh evidence".to_string());
        report.recommended_action = report.retry_hint.clone();
        report
    }

    /// Carry last-known deep-state evidence from a prior successful probe into
    /// this failed/partial record so an intermittent host still surfaces stale
    /// facts (version, data dir, source roots/counts) instead of looking empty,
    /// marking [`Self::stale_data`]. No-op when this host is `Ok` or when a
    /// field is already populated.
    #[must_use]
    pub fn with_last_known(mut self, previous: &HostDoctorReport) -> Self {
        if self.status == HostProbeStatus::Ok {
            return self;
        }
        let mut carried = false;
        if self.cass_version.is_none() && previous.cass_version.is_some() {
            self.cass_version = previous.cass_version.clone();
            carried = true;
        }
        if self.capability_tier.is_none() && previous.capability_tier.is_some() {
            self.capability_tier = previous.capability_tier;
            carried = true;
        }
        if self.data_dir.is_none() && previous.data_dir.is_some() {
            self.data_dir = previous.data_dir.clone();
            carried = true;
        }
        if self.source_roots.is_empty() && !previous.source_roots.is_empty() {
            self.source_roots = previous.source_roots.clone();
            carried = true;
        }
        if self.source_counts.is_none() && previous.source_counts.is_some() {
            self.source_counts = previous.source_counts;
            carried = true;
        }
        if carried {
            self.stale_data = true;
        }
        self
    }
}

/// Classify a probe connection error into an explicit [`HostProbeStatus`] and a
/// short, actionable retry hint. Pure and offline-testable. DNS/auth/refused/
/// host-key failures are [`HostProbeStatus::Unreachable`]; connect and
/// banner-exchange timeouts are [`HostProbeStatus::TimedOut`].
pub fn classify_connection_failure(error: &str) -> (HostProbeStatus, &'static str) {
    let lower = error.to_ascii_lowercase();
    if lower.contains("could not resolve")
        || lower.contains("name or service not known")
        || lower.contains("nodename nor servname")
        || lower.contains("temporary failure in name resolution")
    {
        (
            HostProbeStatus::Unreachable,
            "hostname did not resolve; verify the SSH host alias and DNS, then retry",
        )
    } else if lower.contains("permission denied")
        || lower.contains("publickey")
        || lower.contains("authentication failed")
        || lower.contains("no valid authentication")
        || lower.contains("too many authentication failures")
    {
        (
            HostProbeStatus::Unreachable,
            "SSH authentication was rejected; load the key into ssh-agent or fix the identity file, then retry",
        )
    } else if lower.contains("host key verification")
        || lower.contains("remote host identification has changed")
    {
        (
            HostProbeStatus::Unreachable,
            "host key verification failed; resolve the known_hosts entry (possible key change) before retrying",
        )
    } else if lower.contains("connection refused") {
        (
            HostProbeStatus::Unreachable,
            "remote refused the connection; confirm sshd is running on the expected port, then retry",
        )
    } else if lower.contains("banner exchange") {
        (
            HostProbeStatus::TimedOut,
            "SSH banner exchange timed out (slow or overloaded host); retry when the host is responsive",
        )
    } else if lower.contains("timed out") || lower.contains("timeout") {
        (
            HostProbeStatus::TimedOut,
            "host did not answer within the probe budget; retry when it is online or raise the timeout",
        )
    } else if lower.contains("no route to host") || lower.contains("network is unreachable") {
        (
            HostProbeStatus::Unreachable,
            "no network route to the host; check connectivity and retry from a reachable fleet node",
        )
    } else {
        (
            HostProbeStatus::Unreachable,
            "host was unreachable; check the transport and retry from a reachable fleet node",
        )
    }
}

/// Fleet-wide rollup over the per-host reports.
// No longer `Copy`: the bead-10.4 `root_cause_distribution` BTreeMap is heap-backed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetSummary {
    /// Total hosts in the sweep.
    pub total_hosts: usize,
    /// Hosts reporting [`HostProbeStatus::Ok`].
    pub ok: usize,
    /// Hosts that were probed but degraded/partial/old/timed-out (soft issues).
    pub degraded: usize,
    /// Hosts that timed out.
    pub timed_out: usize,
    /// Hosts that were unreachable or command-not-found (hard failures).
    pub unreachable: usize,
    /// Probes the operator cancelled (also included in `degraded` by status).
    pub cancelled: usize,
    /// Hosts surfacing last-known (stale) evidence while failing/incomplete.
    pub stale_data: usize,
    /// The worst archive risk seen across all hosts.
    pub highest_archive_risk: ArchiveRisk,
    /// Attribution-aware fleet rollup (bead 10.4): per-family host counts keyed
    /// by kebab-case root-cause family. Hosts with no attribution are omitted.
    /// Keeps dependency/transport dominance (`remote-transport-auth`) distinct
    /// from cass-runtime faults (`cass-derived-state`, `frankensqlite-storage`)
    /// and host pressure (`host-disk-pressure`) instead of flattening everything
    /// into one generic failure count.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub root_cause_distribution: BTreeMap<RootCauseFamily, usize>,
    /// The single most common attributed family across the fleet, or `None` when
    /// no host carried an attribution. Ties are broken deterministically toward
    /// the lowest family in taxonomy order so rollups are reproducible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dominant_root_cause: Option<RootCauseFamily>,
}

/// The top-level fleet-doctor report: every host plus a rollup summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetDoctorReport {
    /// Mirrors [`FLEET_DOCTOR_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Per-host records, identity-preserving.
    pub hosts: Vec<HostDoctorReport>,
    /// Aggregate rollup, derived from `hosts`.
    pub summary: FleetSummary,
}

impl FleetDoctorReport {
    /// Build a report and derive the summary from the hosts. The summary is a
    /// pure function of the host records, so this is the only correct way to
    /// construct one.
    pub fn from_hosts(hosts: Vec<HostDoctorReport>) -> Self {
        let total_hosts = hosts.len();
        let mut ok = 0;
        let mut degraded = 0;
        let mut timed_out = 0;
        let mut unreachable = 0;
        let mut cancelled = 0;
        let mut stale_data = 0;
        let mut highest_archive_risk = ArchiveRisk::Unknown;
        // bead 10.4: attribution-aware rollup over each host's likely_root_cause.
        let mut root_cause_distribution: BTreeMap<RootCauseFamily, usize> = BTreeMap::new();

        for host in &hosts {
            // Count the host's attributed family (when any). Unattributed hosts
            // (likely_root_cause = None) are intentionally omitted so the
            // distribution reflects only diagnosed faults.
            if let Some(family) = host.likely_root_cause {
                *root_cause_distribution.entry(family).or_default() += 1;
            }
            match host.status {
                HostProbeStatus::Ok => ok += 1,
                HostProbeStatus::TimedOut => timed_out += 1,
                HostProbeStatus::Unreachable | HostProbeStatus::CommandNotFound => {
                    unreachable += 1;
                }
                HostProbeStatus::Partial
                | HostProbeStatus::OldBinarySkew
                | HostProbeStatus::Degraded => degraded += 1,
            }
            // Cancelled/stale are cross-cutting flags counted independently of
            // the status bucket so an operator-cancelled or stale-evidence host
            // stays visible without being miscounted as a transport failure.
            if host.cancelled {
                cancelled += 1;
            }
            if host.stale_data {
                stale_data += 1;
            }
            // ArchiveRisk derives Ord low→high (Unknown is the floor).
            if host.archive_risk > highest_archive_risk {
                highest_archive_risk = host.archive_risk;
            }
        }

        // Dominant family = highest host count; ties resolve toward the lowest
        // family in taxonomy order (`then_with(|| fb.cmp(fa))` makes the smaller
        // family compare greater) so the rollup is deterministic.
        let dominant_root_cause = root_cause_distribution
            .iter()
            .max_by(|(fa, ca), (fb, cb)| ca.cmp(cb).then_with(|| fb.cmp(fa)))
            .map(|(family, _)| *family);

        Self {
            schema_version: FLEET_DOCTOR_SCHEMA_VERSION,
            hosts,
            summary: FleetSummary {
                total_hosts,
                ok,
                degraded,
                timed_out,
                unreachable,
                cancelled,
                stale_data,
                highest_archive_risk,
                root_cause_distribution,
                dominant_root_cause,
            },
        }
    }
}

/// A single source/fleet-doctor check outcome, decoupled from the CLI's internal
/// `DiagnosticCheck` so the mapping below stays pure and unit-testable. `status`
/// is `"pass"` / `"warn"` / `"fail"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceCheck {
    pub name: String,
    pub status: String,
    pub remediation: Option<String>,
}

impl SourceCheck {
    pub fn new(
        name: impl Into<String>,
        status: impl Into<String>,
        remediation: Option<String>,
    ) -> Self {
        Self {
            name: name.into(),
            status: status.into(),
            remediation: remediation.into(),
        }
    }

    fn is_fail(&self) -> bool {
        self.status.eq_ignore_ascii_case("fail")
    }

    fn matches(&self, needle: &str) -> bool {
        self.name.to_ascii_lowercase().contains(needle)
    }
}

/// Assemble a bounded [`HostDoctorReport`] from a source's doctor checks, deriving
/// the reachability/transport [`HostProbeStatus`] and a safe next command
/// (bead uojcg.8.2, reachability/transport slice). Pure and unit-testable; the
/// CLI maps its `DiagnosticCheck`s into [`SourceCheck`]s and calls this. Identity
/// (`host_alias`, `platform`) is always preserved, even when unreachable.
///
/// Status derivation, in priority order: an SSH/connectivity `fail` →
/// `Unreachable` (root cause `RemoteTransportAuth`); else an rsync/transport
/// `fail` → `CommandNotFound`; else any `fail` → `Degraded`; else any `warn` →
/// `Partial`; else `Ok`. The recommended action is the first failing (then
/// warning) check's remediation.
/// Build the full transport/auth root-cause attribution for a fleet host whose
/// reachability/transport probe failed (bead uojcg.9.5 fleet slice). Confidence
/// tracks the probe status: a hard unreachable/command-not-found is Confirmed, a
/// timeout is Probable, anything softer is Possible. Built directly from the
/// bead-9.1 taxonomy so it matches the status/doctor attribution vocabulary
/// without coupling this schema module to the projector.
fn transport_root_cause_attribution(
    status: HostProbeStatus,
    detail: Option<&str>,
) -> RootCauseAttribution {
    let confidence = match status {
        HostProbeStatus::Unreachable | HostProbeStatus::CommandNotFound => {
            AttributionConfidence::Confirmed
        }
        HostProbeStatus::TimedOut => AttributionConfidence::Probable,
        _ => AttributionConfidence::Possible,
    };
    let mut evidence = vec![EvidenceRef::new("transport.probe_status", "sources")];
    if let Some(detail) = detail {
        evidence.push(
            EvidenceRef::new("transport.connection_error", "sources")
                .with_detail(detail.to_string()),
        );
    }
    RootCauseAttribution::new(
        RootCauseFamily::RemoteTransportAuth,
        confidence,
        "remote transport/auth failure; confirm host reachability before trusting host state",
    )
    .with_evidence(evidence)
}

pub fn host_report_from_checks(
    host_alias: &str,
    platform: Platform,
    elapsed_ms: u64,
    checks: &[SourceCheck],
) -> HostDoctorReport {
    let ssh_failed = checks
        .iter()
        .any(|c| (c.matches("ssh") || c.matches("connect")) && c.is_fail());
    let rsync_failed = checks
        .iter()
        .any(|c| (c.matches("rsync") || c.matches("transport")) && c.is_fail());
    let any_fail = checks.iter().any(SourceCheck::is_fail);
    let any_warn = checks.iter().any(|c| c.status.eq_ignore_ascii_case("warn"));

    let status = if ssh_failed {
        HostProbeStatus::Unreachable
    } else if rsync_failed {
        HostProbeStatus::CommandNotFound
    } else if any_fail {
        HostProbeStatus::Degraded
    } else if any_warn {
        HostProbeStatus::Partial
    } else {
        HostProbeStatus::Ok
    };

    let recommended_action = checks
        .iter()
        .find(|c| c.is_fail())
        .or_else(|| checks.iter().find(|c| c.status.eq_ignore_ascii_case("warn")))
        .and_then(|c| c.remediation.clone());

    let mut report = HostDoctorReport::skeleton(host_alias, platform, status, elapsed_ms);
    report.recommended_action = recommended_action;
    if matches!(status, HostProbeStatus::Unreachable | HostProbeStatus::CommandNotFound) {
        report.likely_root_cause = Some(RootCauseFamily::RemoteTransportAuth);
        report.root_cause = Some(transport_root_cause_attribution(status, None));
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chk(name: &str, status: &str) -> SourceCheck {
        SourceCheck::new(name, status, Some(format!("fix {name}")))
    }

    #[test]
    fn host_report_ok_when_all_checks_pass() {
        let checks = [chk("SSH connectivity", "pass"), chk("rsync available", "pass")];
        let r = host_report_from_checks("ts1", Platform::linux_x86_64(), 30, &checks);
        assert_eq!(r.status, HostProbeStatus::Ok);
        assert_eq!(r.host_alias, "ts1");
        assert!(!r.unreachable);
        assert!(r.recommended_action.is_none());
        assert!(r.likely_root_cause.is_none());
    }

    #[test]
    fn ssh_fail_marks_unreachable_with_transport_root_cause() {
        let checks = [chk("SSH connectivity", "fail"), chk("rsync available", "pass")];
        let r = host_report_from_checks("mac-mini-old", Platform::linux_x86_64(), 5000, &checks);
        assert_eq!(r.status, HostProbeStatus::Unreachable);
        assert!(r.unreachable, "unreachable flag must mirror status");
        assert_eq!(r.likely_root_cause, Some(RootCauseFamily::RemoteTransportAuth));
        // bead 9.5 fleet slice: the full attribution rides alongside the family.
        let rc = r.root_cause.as_ref().expect("unreachable host carries root_cause");
        assert_eq!(rc.family, RootCauseFamily::RemoteTransportAuth);
        assert_eq!(rc.confidence, AttributionConfidence::Confirmed);
        assert!(rc.recommended_next_probe.is_some());
        assert!(rc.evidence_refs.iter().any(|e| e.kind == "transport.probe_status"));
        assert_eq!(r.recommended_action.as_deref(), Some("fix SSH connectivity"));
        assert_eq!(r.host_alias, "mac-mini-old", "identity preserved when unreachable");
    }

    #[test]
    fn failed_host_carries_full_transport_attribution_with_connection_detail() {
        let r = HostDoctorReport::failed(
            "css",
            Platform::linux_x86_64(),
            1200,
            "ssh: connect to host css port 22: Connection timed out",
        );
        let rc = r.root_cause.as_ref().expect("failed host carries root_cause");
        assert_eq!(rc.family, RootCauseFamily::RemoteTransportAuth);
        // A timeout is Probable, not a Confirmed hard failure.
        assert_eq!(rc.confidence, AttributionConfidence::Probable);
        assert!(
            rc.evidence_refs
                .iter()
                .any(|e| e.kind == "transport.connection_error"
                    && e.detail.as_deref().is_some_and(|d| d.contains("Connection timed out"))),
            "connection error must be preserved as evidence: {rc:?}"
        );
    }

    #[test]
    fn rsync_fail_marks_command_not_found() {
        let checks = [chk("SSH connectivity", "pass"), chk("rsync available", "fail")];
        let r = host_report_from_checks("ts2", Platform::linux_x86_64(), 80, &checks);
        assert_eq!(r.status, HostProbeStatus::CommandNotFound);
        assert!(r.status.is_hard_failure());
        assert_eq!(r.likely_root_cause, Some(RootCauseFamily::RemoteTransportAuth));
    }

    #[test]
    fn other_fail_is_degraded_and_warn_is_partial() {
        let degraded = [chk("SSH connectivity", "pass"), chk("Remote Path: paths[0]", "fail")];
        let r = host_report_from_checks("css", Platform::linux_x86_64(), 50, &degraded);
        assert_eq!(r.status, HostProbeStatus::Degraded);
        assert_eq!(r.recommended_action.as_deref(), Some("fix Remote Path: paths[0]"));

        let warned = [chk("SSH connectivity", "pass"), chk("storage", "warn")];
        let r2 = host_report_from_checks("csd", Platform::linux_x86_64(), 50, &warned);
        assert_eq!(r2.status, HostProbeStatus::Partial);
        assert_eq!(r2.recommended_action.as_deref(), Some("fix storage"));
    }

    #[test]
    fn host_report_serializes_with_status_and_identity() {
        let checks = [chk("SSH connectivity", "fail")];
        let r = host_report_from_checks("mac-mini-old", Platform::linux_x86_64(), 5000, &checks);
        let value = serde_json::to_value(&r).unwrap();
        assert_eq!(value["status"], "unreachable");
        assert_eq!(value["unreachable"], true);
        assert_eq!(value["host_alias"], "mac-mini-old");
        assert_eq!(value["likely_root_cause"], "remote-transport-auth");
        let back: HostDoctorReport = serde_json::from_value(value).unwrap();
        assert_eq!(back, r);
    }

    fn populated_ok_host() -> HostDoctorReport {
        let mut h = HostDoctorReport::skeleton(
            "ts1",
            Platform::linux_x86_64(),
            HostProbeStatus::Ok,
            42,
        );
        h.cass_version = Some("0.6.13".to_string());
        h.capability_tier = Some(CapabilityTier::Full);
        h.data_dir = Some("/home/ubuntu/.cass".to_string());
        h.source_roots = vec![SourceRoot {
            path: "/home/ubuntu/.claude".to_string(),
            agent: Some("claude".to_string()),
            archived: true,
        }];
        h.source_counts = Some(SourceCounts {
            roots: 1,
            sessions: 500,
            indexed_sessions: 500,
        });
        h.readiness = Some(ReadinessState::Ready);
        h.semantic = Some(SemanticState::Enabled);
        h.quarantine = Some(QuarantineState {
            quarantined: 0,
            recoverable: 0,
        });
        h.remote_sync = Some(RemoteSyncState::Synced);
        h.archive_risk = ArchiveRisk::Low;
        h
    }

    #[test]
    fn success_scenario_round_trips_with_all_fields() {
        let host = populated_ok_host();
        let value = serde_json::to_value(&host).expect("serialize");
        assert_eq!(value["status"], "ok");
        assert_eq!(value["host_alias"], "ts1");
        assert_eq!(value["platform"]["os"], "linux");
        assert_eq!(value["cass_version"], "0.6.13");
        assert_eq!(value["readiness"], "ready");
        assert_eq!(value["archive_risk"], "low");
        let back: HostDoctorReport = serde_json::from_value(value).expect("deserialize");
        assert_eq!(back, host);
    }

    #[test]
    fn partial_scenario_names_skipped_sections() {
        let mut h = HostDoctorReport::skeleton(
            "css",
            Platform::linux_x86_64(),
            HostProbeStatus::Partial,
            7_900,
        );
        h.readiness = Some(ReadinessState::Ready);
        h.skipped_sections = vec!["semantic".to_string(), "remote_sync".to_string()];
        let value = serde_json::to_value(&h).unwrap();
        assert_eq!(value["status"], "partial");
        assert_eq!(value["skipped_sections"][0], "semantic");
        // Deep, unprobed fields are omitted entirely (not null noise).
        assert!(value.get("semantic").is_none());
        assert_eq!(serde_json::from_value::<HostDoctorReport>(value).unwrap(), h);
    }

    #[test]
    fn timeout_scenario_sets_flag_and_keeps_identity() {
        let h = HostDoctorReport::skeleton(
            "csd",
            Platform::linux_x86_64(),
            HostProbeStatus::TimedOut,
            8_000,
        );
        assert!(h.timed_out, "TimedOut status must set the scalar flag");
        assert!(!h.unreachable);
        let value = serde_json::to_value(&h).unwrap();
        assert_eq!(value["status"], "timed-out");
        assert_eq!(value["timed_out"], true);
        assert_eq!(value["host_alias"], "csd", "identity survives timeout");
        assert!(value.get("readiness").is_none(), "deep state absent on timeout");
    }

    #[test]
    fn old_binary_scenario_carries_version_and_action() {
        let mut h = HostDoctorReport::skeleton(
            "mac-mini-max",
            Platform {
                os: HostOs::MacOs,
                arch: "aarch64".to_string(),
                path_style: PathStyle::Posix,
                tool_notes: vec![],
            },
            HostProbeStatus::OldBinarySkew,
            120,
        );
        h.cass_version = Some("0.5.0".to_string());
        h.capability_tier = Some(CapabilityTier::Standard);
        h.likely_root_cause = Some(RootCauseFamily::OldBinarySkew);
        h.recommended_action = Some("upgrade cass to 0.6.13".to_string());
        let value = serde_json::to_value(&h).unwrap();
        assert_eq!(value["status"], "old-binary-skew");
        assert_eq!(value["cass_version"], "0.5.0");
        assert_eq!(value["likely_root_cause"], "old-binary-skew");
        assert_eq!(serde_json::from_value::<HostDoctorReport>(value).unwrap(), h);
    }

    #[test]
    fn command_not_found_scenario_is_hard_failure() {
        let h = HostDoctorReport::skeleton(
            "ts2",
            Platform::linux_x86_64(),
            HostProbeStatus::CommandNotFound,
            55,
        );
        assert!(h.status.is_hard_failure());
        let value = serde_json::to_value(&h).unwrap();
        assert_eq!(value["status"], "command-not-found");
        assert_eq!(value["host_alias"], "ts2");
    }

    #[test]
    fn unreachable_ssh_scenario_preserves_identity_and_attributes_transport() {
        let h = HostDoctorReport::unreachable(
            "mac-mini-old",
            Platform {
                os: HostOs::MacOs,
                arch: "x86_64".to_string(),
                path_style: PathStyle::Posix,
                tool_notes: vec![],
            },
            5_000,
            "check SSH reachability and host key for mac-mini-old",
        );
        assert!(h.unreachable);
        assert_eq!(h.status, HostProbeStatus::Unreachable);
        assert_eq!(h.likely_root_cause, Some(RootCauseFamily::RemoteTransportAuth));
        let value = serde_json::to_value(&h).unwrap();
        assert_eq!(value["status"], "unreachable");
        assert_eq!(value["unreachable"], true);
        assert_eq!(value["host_alias"], "mac-mini-old", "identity survives unreachable");
        assert!(value["recommended_action"].is_string());
        // No deep state leaked.
        assert!(value.get("readiness").is_none());
    }

    #[test]
    fn classify_connection_failure_covers_probe_taxonomy() {
        // SSH connect timeout → timed-out.
        assert_eq!(
            classify_connection_failure("ssh: connect to host ts2 port 22: Connection timed out").0,
            HostProbeStatus::TimedOut
        );
        // Banner-exchange timeout (intermittent host mid-handshake) → timed-out.
        assert_eq!(
            classify_connection_failure(
                "kex_exchange_identification: read: Connection timed out during banner exchange",
            )
            .0,
            HostProbeStatus::TimedOut
        );
        // DNS failure → unreachable.
        assert_eq!(
            classify_connection_failure(
                "ssh: Could not resolve hostname mac-mini-old: Name or service not known",
            )
            .0,
            HostProbeStatus::Unreachable
        );
        // Auth failure → unreachable.
        assert_eq!(
            classify_connection_failure("Permission denied (publickey).").0,
            HostProbeStatus::Unreachable
        );
        // Connection refused and host-key change also stay explicit unreachable.
        assert_eq!(
            classify_connection_failure("ssh: connect to host h: Connection refused").0,
            HostProbeStatus::Unreachable
        );
        assert_eq!(
            classify_connection_failure("Host key verification failed.").0,
            HostProbeStatus::Unreachable
        );
        // Every classification carries a non-empty retry hint.
        for err in [
            "Connection timed out",
            "Could not resolve hostname",
            "Permission denied",
            "Connection refused",
            "banner exchange",
            "No route to host",
            "something unexpected",
        ] {
            assert!(!classify_connection_failure(err).1.is_empty());
        }
    }

    #[test]
    fn failed_probe_preserves_identity_error_and_retry_hint() {
        // DNS failure scenario.
        let dns = HostDoctorReport::failed(
            "mac-mini-old",
            Platform::linux_x86_64(),
            3_200,
            "ssh: Could not resolve hostname mac-mini-old: Name or service not known",
        );
        assert_eq!(dns.status, HostProbeStatus::Unreachable);
        assert!(dns.unreachable);
        assert!(!dns.timed_out);
        assert_eq!(dns.elapsed_ms, 3_200);
        let value = serde_json::to_value(&dns).unwrap();
        assert_eq!(value["status"], "unreachable");
        assert_eq!(value["host_alias"], "mac-mini-old", "identity survives failure");
        assert!(value["connection_error"].as_str().unwrap().contains("resolve"));
        assert!(value["retry_hint"].is_string());
        assert_eq!(serde_json::from_value::<HostDoctorReport>(value).unwrap(), dns);

        // Auth failure scenario.
        let auth = HostDoctorReport::failed(
            "ts2",
            Platform::linux_x86_64(),
            800,
            "Permission denied (publickey).",
        );
        assert_eq!(auth.status, HostProbeStatus::Unreachable);
        assert!(auth.connection_error.as_deref().unwrap().contains("Permission denied"));

        // Banner-exchange timeout scenario → timed-out, scalar flag mirrors.
        let banner = HostDoctorReport::failed(
            "ts2",
            Platform::linux_x86_64(),
            9_500,
            "kex_exchange_identification: read: Connection timed out during banner exchange",
        );
        assert_eq!(banner.status, HostProbeStatus::TimedOut);
        assert!(banner.timed_out);
        assert!(!banner.unreachable);
        assert_eq!(banner.elapsed_ms, 9_500);
    }

    #[test]
    fn operator_cancelled_probe_is_distinct_explicit_state() {
        let cancelled = HostDoctorReport::cancelled("ts1", Platform::linux_x86_64(), 120);
        assert!(cancelled.cancelled);
        // Not a transport failure: neither timed_out nor unreachable.
        assert!(!cancelled.timed_out);
        assert!(!cancelled.unreachable);
        assert_eq!(cancelled.status, HostProbeStatus::Partial);
        assert!(
            cancelled
                .connection_error
                .as_deref()
                .unwrap()
                .contains("cancelled")
        );
        let value = serde_json::to_value(&cancelled).unwrap();
        assert_eq!(value["cancelled"], true);
        assert_eq!(value["status"], "partial");
        assert_eq!(serde_json::from_value::<HostDoctorReport>(value).unwrap(), cancelled);
    }

    #[test]
    fn with_last_known_surfaces_stale_evidence_for_intermittent_host() {
        // A host that was healthy before now times out; carry its last-known
        // version/data dir so it stays informative rather than blank.
        let mut healthy = populated_ok_host();
        healthy.cass_version = Some("0.6.10".to_string());
        healthy.data_dir = Some("/home/me/.local/share/cass".to_string());

        let now_failed = HostDoctorReport::failed(
            &healthy.host_alias,
            healthy.platform.clone(),
            9_900,
            "Connection timed out",
        )
        .with_last_known(&healthy);

        assert_eq!(now_failed.status, HostProbeStatus::TimedOut);
        assert!(now_failed.stale_data, "carried last-known evidence marks stale");
        assert_eq!(now_failed.cass_version.as_deref(), Some("0.6.10"));
        assert_eq!(
            now_failed.data_dir.as_deref(),
            Some("/home/me/.local/share/cass")
        );
        let value = serde_json::to_value(&now_failed).unwrap();
        assert_eq!(value["stale_data"], true);

        // No-op for a reachable host (nothing stale to mark).
        let still_ok = populated_ok_host().with_last_known(&healthy);
        assert!(!still_ok.stale_data);
    }

    #[test]
    fn fleet_rollup_counts_unreachable_cancelled_and_stale_separately() {
        let mut healthy = populated_ok_host();
        healthy.cass_version = Some("0.6.10".to_string());

        let hosts = vec![
            populated_ok_host(),
            HostDoctorReport::failed(
                "dns-host",
                Platform::linux_x86_64(),
                1_000,
                "Could not resolve hostname",
            ),
            HostDoctorReport::failed(
                "auth-host",
                Platform::linux_x86_64(),
                800,
                "Permission denied (publickey)",
            ),
            HostDoctorReport::failed(
                "slow-host",
                Platform::linux_x86_64(),
                9_000,
                "Connection timed out",
            ),
            HostDoctorReport::cancelled("cancel-host", Platform::linux_x86_64(), 50),
            HostDoctorReport::failed(
                "intermittent",
                Platform::linux_x86_64(),
                9_900,
                "Connection timed out",
            )
            .with_last_known(&healthy),
        ];
        let report = FleetDoctorReport::from_hosts(hosts);
        let s = report.summary;
        assert_eq!(s.total_hosts, 6);
        assert_eq!(s.ok, 1);
        // dns + auth are hard unreachable; counted separately from healthy.
        assert_eq!(s.unreachable, 2);
        // slow-host + intermittent timed out.
        assert_eq!(s.timed_out, 2);
        // operator-cancelled counted on its own axis (and is not unreachable).
        assert_eq!(s.cancelled, 1);
        // the intermittent host carried stale evidence.
        assert_eq!(s.stale_data, 1);
        // Unreachable never folds into the healthy/ok bucket.
        assert_ne!(s.ok, s.total_hosts);
    }

    #[test]
    fn macos_path_and_tool_differences_are_representable() {
        let platform = Platform {
            os: HostOs::MacOs,
            arch: "aarch64".to_string(),
            path_style: PathStyle::Posix,
            tool_notes: vec!["rsync=bsd".to_string(), "data_dir=~/Library".to_string()],
        };
        let h = HostDoctorReport::skeleton("mac-mini-max", platform, HostProbeStatus::Ok, 80);
        let value = serde_json::to_value(&h).unwrap();
        assert_eq!(value["platform"]["os"], "macos");
        assert_eq!(value["platform"]["tool_notes"][0], "rsync=bsd");
        assert_eq!(serde_json::from_value::<HostDoctorReport>(value).unwrap(), h);
    }

    #[test]
    fn high_archive_risk_is_representable() {
        let mut h = populated_ok_host();
        h.status = HostProbeStatus::Degraded;
        h.archive_risk = ArchiveRisk::High;
        h.recommended_action = Some("back up derived archive before re-index".to_string());
        let value = serde_json::to_value(&h).unwrap();
        assert_eq!(value["archive_risk"], "high");
        assert_eq!(serde_json::from_value::<HostDoctorReport>(value).unwrap(), h);
    }

    #[test]
    fn host_identity_is_present_for_every_status() {
        for status in [
            HostProbeStatus::Ok,
            HostProbeStatus::Partial,
            HostProbeStatus::TimedOut,
            HostProbeStatus::OldBinarySkew,
            HostProbeStatus::CommandNotFound,
            HostProbeStatus::Unreachable,
            HostProbeStatus::Degraded,
        ] {
            let h = HostDoctorReport::skeleton("host-x", Platform::linux_x86_64(), status, 1);
            let value = serde_json::to_value(&h).unwrap();
            assert_eq!(value["host_alias"], "host-x", "{status:?}: lost host alias");
            assert!(value.get("platform").is_some(), "{status:?}: lost platform");
            // Status flags stay consistent with the discriminant.
            assert_eq!(value["timed_out"], status == HostProbeStatus::TimedOut);
            assert_eq!(value["unreachable"], status == HostProbeStatus::Unreachable);
        }
    }

    #[test]
    fn probe_status_wire_values_match_as_str() {
        for status in [
            HostProbeStatus::Ok,
            HostProbeStatus::Partial,
            HostProbeStatus::TimedOut,
            HostProbeStatus::OldBinarySkew,
            HostProbeStatus::CommandNotFound,
            HostProbeStatus::Unreachable,
            HostProbeStatus::Degraded,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            assert_eq!(json, format!("\"{}\"", status.as_str()));
            let back: HostProbeStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(back, status);
        }
    }

    #[test]
    fn archive_risk_orders_low_to_high() {
        assert!(ArchiveRisk::High > ArchiveRisk::Medium);
        assert!(ArchiveRisk::Medium > ArchiveRisk::Low);
        assert!(ArchiveRisk::Low > ArchiveRisk::Unknown);
    }

    #[test]
    fn fleet_summary_is_derived_and_takes_max_archive_risk() {
        let hosts = vec![
            populated_ok_host(),
            HostDoctorReport::skeleton("csd", Platform::linux_x86_64(), HostProbeStatus::TimedOut, 8000),
            HostDoctorReport::unreachable(
                "mac-mini-old",
                Platform::linux_x86_64(),
                5000,
                "check ssh",
            ),
            {
                let mut h = HostDoctorReport::skeleton(
                    "css",
                    Platform::linux_x86_64(),
                    HostProbeStatus::Degraded,
                    100,
                );
                h.archive_risk = ArchiveRisk::High;
                h
            },
        ];
        let report = FleetDoctorReport::from_hosts(hosts);
        assert_eq!(report.summary.total_hosts, 4);
        assert_eq!(report.summary.ok, 1);
        assert_eq!(report.summary.timed_out, 1);
        assert_eq!(report.summary.unreachable, 1);
        assert_eq!(report.summary.degraded, 1);
        assert_eq!(report.summary.highest_archive_risk, ArchiveRisk::High);

        // Whole report round-trips.
        let value = serde_json::to_value(&report).unwrap();
        let back: FleetDoctorReport = serde_json::from_value(value).unwrap();
        assert_eq!(back, report);
    }

    #[test]
    fn command_not_found_counts_as_unreachable_in_rollup() {
        let hosts = vec![HostDoctorReport::skeleton(
            "ts2",
            Platform::linux_x86_64(),
            HostProbeStatus::CommandNotFound,
            10,
        )];
        let report = FleetDoctorReport::from_hosts(hosts);
        assert_eq!(report.summary.unreachable, 1, "command-not-found is a hard failure");
    }

    // --- bead 10.4: attribution-aware fleet rollups over the host matrix ---

    /// Build a reachable-but-faulted host whose deep state attributes a specific
    /// non-transport family (e.g. a remote whose storage probe failed).
    fn host_attributed(alias: &str, family: RootCauseFamily) -> HostDoctorReport {
        let mut h = HostDoctorReport::skeleton(
            alias,
            Platform::linux_x86_64(),
            HostProbeStatus::Degraded,
            40,
        );
        h.likely_root_cause = Some(family);
        h
    }

    #[test]
    fn fleet_rollup_keeps_dependency_and_storage_dominance_distinct() {
        // css-like dependency/transport dominance (one hard-unreachable, one
        // connect-timeout), a csd-like storage host, and a healthy host.
        let hosts = vec![
            HostDoctorReport::unreachable("css1", Platform::linux_x86_64(), 100, "fix ssh"),
            HostDoctorReport::failed(
                "css2",
                Platform::linux_x86_64(),
                120,
                "ssh: connect to host css2 port 22: Connection timed out",
            ),
            host_attributed("csd", RootCauseFamily::FrankensqliteStorage),
            HostDoctorReport::skeleton("ts1", Platform::linux_x86_64(), HostProbeStatus::Ok, 20),
        ];
        let report = FleetDoctorReport::from_hosts(hosts);
        let dist = &report.summary.root_cause_distribution;
        // Dependency/transport dominance is NOT flattened into a generic cass
        // failure, and storage dominance stays its own distinct category.
        assert_eq!(dist.get(&RootCauseFamily::RemoteTransportAuth), Some(&2));
        assert_eq!(dist.get(&RootCauseFamily::FrankensqliteStorage), Some(&1));
        // The healthy host contributes no attribution.
        assert_eq!(dist.values().sum::<usize>(), 3);
        assert_eq!(
            report.summary.dominant_root_cause,
            Some(RootCauseFamily::RemoteTransportAuth)
        );
        // Unreachable / timed-out hosts remain explicit evidence-gap states.
        assert_eq!(report.summary.unreachable, 1, "css1 hard-unreachable");
        assert_eq!(report.summary.timed_out, 1, "css2 connect-timeout");
    }

    #[test]
    fn fleet_rollup_dominant_flips_when_storage_dominates() {
        let hosts = vec![
            host_attributed("csd1", RootCauseFamily::FrankensqliteStorage),
            host_attributed("csd2", RootCauseFamily::FrankensqliteStorage),
            host_attributed("csd3", RootCauseFamily::FrankensqliteStorage),
            HostDoctorReport::unreachable("css", Platform::linux_x86_64(), 90, "fix ssh"),
        ];
        let report = FleetDoctorReport::from_hosts(hosts);
        assert_eq!(
            report.summary.dominant_root_cause,
            Some(RootCauseFamily::FrankensqliteStorage)
        );
        assert_eq!(
            report.summary.root_cause_distribution.get(&RootCauseFamily::FrankensqliteStorage),
            Some(&3)
        );
        assert_eq!(
            report.summary.root_cause_distribution.get(&RootCauseFamily::RemoteTransportAuth),
            Some(&1)
        );
    }

    #[test]
    fn fleet_rollup_dominant_tie_breaks_deterministically_to_lowest_family() {
        // One host each of two families => a tie; the lower family in taxonomy
        // order wins deterministically (CassDerivedState < RemoteTransportAuth).
        let hosts = vec![
            host_attributed("a", RootCauseFamily::CassDerivedState),
            HostDoctorReport::unreachable("b", Platform::linux_x86_64(), 50, "fix"),
        ];
        let report = FleetDoctorReport::from_hosts(hosts);
        assert_eq!(
            report.summary.dominant_root_cause,
            Some(RootCauseFamily::CassDerivedState),
            "ties resolve to the lowest family for reproducibility"
        );
    }

    #[test]
    fn fleet_rollup_unattributed_fleet_has_empty_distribution_and_no_dominant() {
        let hosts = vec![
            HostDoctorReport::skeleton("ts1", Platform::linux_x86_64(), HostProbeStatus::Ok, 10),
            HostDoctorReport::skeleton("ts2", Platform::linux_x86_64(), HostProbeStatus::Partial, 12),
        ];
        let report = FleetDoctorReport::from_hosts(hosts);
        assert!(report.summary.root_cause_distribution.is_empty());
        assert_eq!(report.summary.dominant_root_cause, None);
    }

    #[test]
    fn fleet_rollup_distribution_serializes_with_kebab_family_keys() {
        let report = FleetDoctorReport::from_hosts(vec![HostDoctorReport::unreachable(
            "css",
            Platform::linux_x86_64(),
            50,
            "fix",
        )]);
        let value = serde_json::to_value(&report.summary).unwrap();
        assert_eq!(value["root_cause_distribution"]["remote-transport-auth"], 1);
        assert_eq!(value["dominant_root_cause"], "remote-transport-auth");
        let back: FleetSummary = serde_json::from_value(value).unwrap();
        assert_eq!(back, report.summary);
    }
}
