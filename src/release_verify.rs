//! Release / distribution-channel verification.
//!
//! Bead: coding_agent_session_search-guided-ops-repro-trust-5u82n.8
//! ("Verify release distribution channels and installer parity").
//!
//! CASS ships through several install paths (GitHub release assets, a Homebrew
//! tap, a Scoop bucket, crates.io, and the curl|bash installer script). A
//! release feels *broken* to a user if any one channel lags, serves the wrong
//! binary, or has a mismatched checksum — and the recurring failure mode is the
//! `release.yml` Homebrew/Scoop "notify" jobs being no-ops, so those channels
//! silently stay on the prior version (see bead `3jxkk`).
//!
//! This module is the **evaluation core**: pure, offline functions that turn
//! per-channel observations (gathered by a caller — networked checks live in the
//! CLI layer and are always explicit) into a structured, robot-safe report with
//! a stable JSON contract and an explicit failure taxonomy. Keeping the decision
//! logic pure makes every release scenario unit-testable with fixtures/test
//! doubles and keeps the wire contract pinned independent of live network state.
//!
//! # Robot-safe usage
//!
//! Callers feed a [`ReleaseVerifyRequest`] (the expected version plus one
//! [`ChannelObservationInput`] per channel) and get a
//! [`ReleaseVerificationReport`] back; [`verify_from_json`] is the
//! string-in/struct-out entry for scripts, CI, and agents. Live network probing
//! (GitHub release API, Homebrew tap, Scoop bucket, crates.io, installer smoke)
//! belongs in the caller that fills the observations — it is always explicit, so
//! a network-less run reports `network_unavailable` per channel rather than
//! silently passing.
//!
//! ## Pre-release use
//!
//! After cutting a tag and before announcing it, gather observations for every
//! channel and treat [`ReleaseVerificationReport::overall_ready`] as the
//! go/no-go gate. Any `stale`/`missing`/`checksum_mismatch`/`installer_failed`
//! channel blocks the announcement; each carries a `manual_next_action` (e.g.
//! "manually dispatch the homebrew update workflow for version X") so the
//! release owner knows exactly what to run. A `network_unavailable` channel is
//! never treated as ready — re-run with connectivity.
//!
//! ## Post-release use
//!
//! Re-run periodically after publishing to confirm the dispatch-driven channels
//! (Homebrew/Scoop) actually propagated — the recurring `release.yml` no-op bug
//! leaves them `stale`/`missing` until someone dispatches the notify workflow.
//! [`ReleaseVerificationReport::manual_actions`] lists exactly the channels that
//! still need a manual push.

use semver::Version;
use serde::{Deserialize, Serialize};

/// Stable schema version for the release-verification wire format. Bump only on
/// a breaking change to the field set or enum string values.
pub const RELEASE_VERIFY_SCHEMA_VERSION: u32 = 1;

/// An install/distribution channel a user might receive a release through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseChannel {
    /// GitHub release assets (the source of truth other channels mirror).
    GithubRelease,
    /// Homebrew tap formula.
    Homebrew,
    /// Scoop bucket manifest.
    Scoop,
    /// crates.io published crate.
    CratesIo,
    /// The curl|bash installer script's resolved binary.
    InstallerScript,
}

impl ReleaseChannel {
    /// Stable snake_case wire label.
    pub fn as_str(self) -> &'static str {
        match self {
            ReleaseChannel::GithubRelease => "github_release",
            ReleaseChannel::Homebrew => "homebrew",
            ReleaseChannel::Scoop => "scoop",
            ReleaseChannel::CratesIo => "crates_io",
            ReleaseChannel::InstallerScript => "installer_script",
        }
    }

    /// Whether this channel is updated by a `workflow_dispatch` notify job that
    /// has historically been a no-op (Homebrew tap / Scoop bucket). These need
    /// an explicit dispatch each release and so report a manual next action when
    /// they lag.
    pub fn is_dispatch_driven(self) -> bool {
        matches!(self, ReleaseChannel::Homebrew | ReleaseChannel::Scoop)
    }
}

/// Outcome state for a single channel — the failure taxonomy. Only `UpToDate`
/// and `NotConfigured` are considered ready.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelState {
    /// Channel publishes exactly (or ahead of) the expected version.
    UpToDate,
    /// Channel is reachable but serves an older version (lagging release).
    Stale,
    /// Channel has no published artifact / its dispatch never ran.
    Missing,
    /// A release asset's checksum did not match the expected digest.
    ChecksumMismatch,
    /// The installer script ran but failed or produced the wrong binary.
    InstallerFailed,
    /// Channel could not be checked because the network was unavailable.
    NetworkUnavailable,
    /// Channel is not configured/applicable for this release.
    NotConfigured,
}

impl ChannelState {
    /// Stable snake_case wire label.
    pub fn as_str(self) -> &'static str {
        match self {
            ChannelState::UpToDate => "up_to_date",
            ChannelState::Stale => "stale",
            ChannelState::Missing => "missing",
            ChannelState::ChecksumMismatch => "checksum_mismatch",
            ChannelState::InstallerFailed => "installer_failed",
            ChannelState::NetworkUnavailable => "network_unavailable",
            ChannelState::NotConfigured => "not_configured",
        }
    }

    /// Whether this state represents a release-ready channel (nothing to do).
    pub fn is_ready(self) -> bool {
        matches!(self, ChannelState::UpToDate | ChannelState::NotConfigured)
    }
}

/// Raw per-channel observation gathered by the caller. Networked probing lives
/// in the CLI layer; this struct is the offline-testable boundary so every
/// release scenario can be exercised with fixtures/test doubles.
#[derive(Debug, Clone, Default)]
pub struct ChannelObservation {
    /// Whether this channel applies to the release under test.
    pub configured: bool,
    /// Whether the channel could be contacted at all. `false` => the check
    /// could not run (network unavailable).
    pub reachable: bool,
    /// Version string the channel currently serves, if it could be determined.
    pub observed_version: Option<String>,
    /// Whether the release asset's checksum matched, when a checksum was
    /// checked (asset channels only). `None` when not applicable/checked.
    pub checksum_ok: Option<bool>,
    /// For dispatch-driven channels (Homebrew/Scoop): whether the notify
    /// `workflow_dispatch` actually ran for this release. `Some(false)` is the
    /// classic no-op-job failure.
    pub dispatch_ran: Option<bool>,
    /// For the installer script: whether running it produced the expected
    /// working binary. `None` when not applicable.
    pub installer_ok: Option<bool>,
}

impl ChannelObservation {
    /// A configured, reachable channel serving `version` with a matching
    /// checksum and (where relevant) a successful dispatch/installer run.
    pub fn healthy(version: &str) -> Self {
        Self {
            configured: true,
            reachable: true,
            observed_version: Some(version.to_string()),
            checksum_ok: Some(true),
            dispatch_ran: Some(true),
            installer_ok: Some(true),
        }
    }
}

/// Per-channel verification result with the observed-vs-expected outcome and a
/// manual remediation when the channel cannot be auto-updated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelReport {
    /// Which channel.
    pub channel: ReleaseChannel,
    /// Outcome state.
    pub state: ChannelState,
    /// Expected (release-under-test) version.
    pub expected_version: String,
    /// Version observed on the channel, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_version: Option<String>,
    /// Whether the asset checksum matched, when checked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checksum_ok: Option<bool>,
    /// Structured, prose-light detail of what was observed.
    pub detail: String,
    /// Manual remediation when the channel can't update automatically (e.g.
    /// dispatch the Homebrew/Scoop notify workflow). `None` when ready.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manual_next_action: Option<String>,
}

/// Parse a version string tolerating a leading `v` and trailing build/dirty
/// suffixes (e.g. `v0.6.13`, `0.6.13-dirty`). Mirrors the fleet version-skew
/// parser so the two surfaces agree on comparison semantics.
fn parse_release_version(raw: &str) -> Option<Version> {
    let trimmed = raw.trim().trim_start_matches('v').trim();
    // Compare on the release core (major.minor.patch), ignoring any pre-release
    // or build suffix such as `-dirty` or `+meta`: for distribution purposes a
    // channel serving `0.6.13-dirty` is at `0.6.13`. (Semver precedence would
    // otherwise rank a pre-release *below* the release and falsely flag it
    // stale.)
    let core = trimmed.split(['-', '+']).next().unwrap_or(trimmed).trim();
    Version::parse(core).ok()
}

/// Evaluate a single channel observation against the expected version, deciding
/// its [`ChannelState`] and any manual remediation. Pure and offline.
pub fn evaluate_channel(
    channel: ReleaseChannel,
    expected_version: &str,
    obs: &ChannelObservation,
) -> ChannelReport {
    let mut report = ChannelReport {
        channel,
        state: ChannelState::UpToDate,
        expected_version: expected_version.to_string(),
        observed_version: obs.observed_version.clone(),
        checksum_ok: obs.checksum_ok,
        detail: String::new(),
        manual_next_action: None,
    };

    // Resolution order is deliberate: applicability, then reachability, then
    // hard artifact faults, then version comparison.
    if !obs.configured {
        report.state = ChannelState::NotConfigured;
        report.detail = format!("{} is not configured for this release", channel.as_str());
        return report;
    }

    if !obs.reachable {
        report.state = ChannelState::NetworkUnavailable;
        report.detail = format!(
            "{} could not be checked: network unavailable",
            channel.as_str()
        );
        report.manual_next_action =
            Some("re-run release verification with network access".to_string());
        return report;
    }

    if obs.checksum_ok == Some(false) {
        report.state = ChannelState::ChecksumMismatch;
        report.detail = format!(
            "{} asset checksum did not match the expected digest for {expected_version}",
            channel.as_str()
        );
        report.manual_next_action = Some(
            "re-upload the release asset and regenerate checksums; do not advertise this release until it matches".to_string(),
        );
        return report;
    }

    if channel == ReleaseChannel::InstallerScript && obs.installer_ok == Some(false) {
        report.state = ChannelState::InstallerFailed;
        report.detail =
            "installer script ran but did not produce a working expected-version binary"
                .to_string();
        report.manual_next_action =
            Some("fix the installer script's asset URL/version resolution and re-test".to_string());
        return report;
    }

    // Dispatch-driven channels (Homebrew tap / Scoop bucket): a notify job that
    // never ran leaves the channel with no/old artifact for this release.
    if channel.is_dispatch_driven() && obs.dispatch_ran == Some(false) {
        report.state = ChannelState::Missing;
        report.detail = format!(
            "{} notify workflow_dispatch did not run for {expected_version}",
            channel.as_str()
        );
        report.manual_next_action = Some(format!(
            "manually dispatch the {} update workflow for version {expected_version}",
            channel.as_str()
        ));
        return report;
    }

    match obs.observed_version.as_deref() {
        None => {
            report.state = ChannelState::Missing;
            report.detail = format!(
                "{} did not publish a discoverable version for {expected_version}",
                channel.as_str()
            );
            report.manual_next_action = Some(format!(
                "publish/refresh {} for version {expected_version}",
                channel.as_str()
            ));
        }
        Some(observed) => match (
            parse_release_version(expected_version),
            parse_release_version(observed),
        ) {
            (Some(want), Some(have)) if have < want => {
                report.state = ChannelState::Stale;
                report.detail = format!(
                    "{} serves {observed}, behind expected {expected_version}",
                    channel.as_str()
                );
                report.manual_next_action = if channel.is_dispatch_driven() {
                    Some(format!(
                        "manually dispatch the {} update workflow for version {expected_version}",
                        channel.as_str()
                    ))
                } else {
                    Some(format!(
                        "re-publish {} for version {expected_version}",
                        channel.as_str()
                    ))
                };
            }
            (Some(_), Some(_)) => {
                // Equal or ahead: the channel has at least the expected release.
                report.state = ChannelState::UpToDate;
                report.detail = format!("{} serves {observed}", channel.as_str());
            }
            _ => {
                // Unparseable version on one side: do not guess parity. Treat as
                // stale and require a manual check rather than claiming ready.
                report.state = ChannelState::Stale;
                report.detail = format!(
                    "{} version {observed} could not be compared to expected {expected_version}",
                    channel.as_str()
                );
                report.manual_next_action = Some(format!(
                    "manually confirm {} is at version {expected_version}",
                    channel.as_str()
                ));
            }
        },
    }

    report
}

/// Aggregate counts over the per-channel reports. Lagging/missing channels are
/// counted in their own buckets so a release summary cannot look ready while a
/// channel quietly trails.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ReleaseVerifySummary {
    /// Total channels evaluated.
    pub total: usize,
    /// Channels ready (up-to-date or not configured).
    pub ready: usize,
    /// Channels serving an older/uncomparable version.
    pub stale: usize,
    /// Channels with no published artifact / un-run dispatch.
    pub missing: usize,
    /// Channels with a checksum mismatch.
    pub checksum_mismatch: usize,
    /// Channels where the installer failed.
    pub installer_failed: usize,
    /// Channels that could not be checked (no network).
    pub network_unavailable: usize,
}

/// The top-level release-verification report: every channel plus a rollup and a
/// single overall-ready verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseVerificationReport {
    /// Mirrors [`RELEASE_VERIFY_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// The release version under test.
    pub expected_version: String,
    /// Per-channel results, in stable channel order.
    pub channels: Vec<ChannelReport>,
    /// Aggregate rollup.
    pub summary: ReleaseVerifySummary,
    /// `true` only when every channel is ready.
    pub overall_ready: bool,
}

impl ReleaseVerificationReport {
    /// Build a report from per-channel observations. The summary and the
    /// overall-ready verdict are pure functions of the channel reports.
    pub fn build(
        expected_version: &str,
        observations: &[(ReleaseChannel, ChannelObservation)],
    ) -> Self {
        let channels: Vec<ChannelReport> = observations
            .iter()
            .map(|(channel, obs)| evaluate_channel(*channel, expected_version, obs))
            .collect();

        let mut summary = ReleaseVerifySummary {
            total: channels.len(),
            ..Default::default()
        };
        for report in &channels {
            match report.state {
                ChannelState::UpToDate | ChannelState::NotConfigured => summary.ready += 1,
                ChannelState::Stale => summary.stale += 1,
                ChannelState::Missing => summary.missing += 1,
                ChannelState::ChecksumMismatch => summary.checksum_mismatch += 1,
                ChannelState::InstallerFailed => summary.installer_failed += 1,
                ChannelState::NetworkUnavailable => summary.network_unavailable += 1,
            }
        }
        let overall_ready = channels.iter().all(|c| c.state.is_ready());

        Self {
            schema_version: RELEASE_VERIFY_SCHEMA_VERSION,
            expected_version: expected_version.to_string(),
            channels,
            summary,
            overall_ready,
        }
    }

    /// The channels that need a manual remediation, in order.
    pub fn manual_actions(&self) -> Vec<&ChannelReport> {
        self.channels
            .iter()
            .filter(|c| c.manual_next_action.is_some())
            .collect()
    }
}

/// A single channel's observation in the robot-safe request payload. Mirrors
/// [`ChannelObservation`] but is `Deserialize` and carries its channel id so a
/// caller can submit a flat JSON array.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelObservationInput {
    /// Which channel this observation is for.
    pub channel: ReleaseChannel,
    /// Whether this channel applies to the release under test.
    #[serde(default)]
    pub configured: bool,
    /// Whether the channel could be contacted at all.
    #[serde(default)]
    pub reachable: bool,
    /// Version string the channel serves, if determined.
    #[serde(default)]
    pub observed_version: Option<String>,
    /// Whether the asset checksum matched, when checked.
    #[serde(default)]
    pub checksum_ok: Option<bool>,
    /// Whether the dispatch-driven notify workflow ran (Homebrew/Scoop).
    #[serde(default)]
    pub dispatch_ran: Option<bool>,
    /// Whether the installer script produced a working expected-version binary.
    #[serde(default)]
    pub installer_ok: Option<bool>,
}

impl ChannelObservationInput {
    fn into_pair(self) -> (ReleaseChannel, ChannelObservation) {
        (
            self.channel,
            ChannelObservation {
                configured: self.configured,
                reachable: self.reachable,
                observed_version: self.observed_version,
                checksum_ok: self.checksum_ok,
                dispatch_ran: self.dispatch_ran,
                installer_ok: self.installer_ok,
            },
        )
    }
}

/// Robot-safe request payload: the expected release version plus one
/// observation per channel. This is the stable JSON input contract for scripts,
/// CI, and agents (the producing side fills observations from explicit, live
/// checks).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseVerifyRequest {
    /// The release version under test.
    pub expected_version: String,
    /// Per-channel observations.
    pub channels: Vec<ChannelObservationInput>,
}

/// Evaluate a deserialized request into a report.
pub fn verify_request(request: ReleaseVerifyRequest) -> ReleaseVerificationReport {
    let observations: Vec<(ReleaseChannel, ChannelObservation)> = request
        .channels
        .into_iter()
        .map(ChannelObservationInput::into_pair)
        .collect();
    ReleaseVerificationReport::build(&request.expected_version, &observations)
}

/// Robot-safe entry: parse a [`ReleaseVerifyRequest`] JSON string and return the
/// verification report. The CLI layer wraps this after gathering live, explicit
/// per-channel observations; CI and tests can drive it with recorded doubles.
pub fn verify_from_json(input: &str) -> Result<ReleaseVerificationReport, serde_json::Error> {
    let request: ReleaseVerifyRequest = serde_json::from_str(input)?;
    Ok(verify_request(request))
}

#[cfg(test)]
mod tests {
    use super::*;

    const V: &str = "0.6.13";

    fn all_channels(
        make: impl Fn(ReleaseChannel) -> ChannelObservation,
    ) -> Vec<(ReleaseChannel, ChannelObservation)> {
        [
            ReleaseChannel::GithubRelease,
            ReleaseChannel::Homebrew,
            ReleaseChannel::Scoop,
            ReleaseChannel::CratesIo,
            ReleaseChannel::InstallerScript,
        ]
        .into_iter()
        .map(|c| (c, make(c)))
        .collect()
    }

    #[test]
    fn complete_release_is_overall_ready() {
        let obs = all_channels(|_| ChannelObservation::healthy(V));
        let report = ReleaseVerificationReport::build(V, &obs);
        assert!(
            report.overall_ready,
            "all-healthy release should be ready: {report:?}"
        );
        assert_eq!(report.summary.total, 5);
        assert_eq!(report.summary.ready, 5);
        assert!(report.manual_actions().is_empty());
    }

    #[test]
    fn missing_homebrew_dispatch_is_flagged_with_manual_action() {
        let obs = all_channels(|c| {
            let mut o = ChannelObservation::healthy(V);
            if c == ReleaseChannel::Homebrew {
                o.dispatch_ran = Some(false);
                o.observed_version = Some("0.6.12".to_string());
            }
            o
        });
        let report = ReleaseVerificationReport::build(V, &obs);
        assert!(!report.overall_ready);
        assert_eq!(report.summary.missing, 1);
        let brew = report
            .channels
            .iter()
            .find(|c| c.channel == ReleaseChannel::Homebrew)
            .unwrap();
        assert_eq!(brew.state, ChannelState::Missing);
        assert!(
            brew.manual_next_action
                .as_deref()
                .unwrap()
                .contains("dispatch")
        );
    }

    #[test]
    fn missing_scoop_dispatch_is_flagged() {
        let obs = all_channels(|c| {
            let mut o = ChannelObservation::healthy(V);
            if c == ReleaseChannel::Scoop {
                o.dispatch_ran = Some(false);
                o.observed_version = None;
            }
            o
        });
        let report = ReleaseVerificationReport::build(V, &obs);
        let scoop = report
            .channels
            .iter()
            .find(|c| c.channel == ReleaseChannel::Scoop)
            .unwrap();
        assert_eq!(scoop.state, ChannelState::Missing);
        assert!(!report.overall_ready);
    }

    #[test]
    fn checksum_mismatch_blocks_release() {
        let obs = all_channels(|c| {
            let mut o = ChannelObservation::healthy(V);
            if c == ReleaseChannel::GithubRelease {
                o.checksum_ok = Some(false);
            }
            o
        });
        let report = ReleaseVerificationReport::build(V, &obs);
        let gh = report
            .channels
            .iter()
            .find(|c| c.channel == ReleaseChannel::GithubRelease)
            .unwrap();
        assert_eq!(gh.state, ChannelState::ChecksumMismatch);
        assert_eq!(gh.checksum_ok, Some(false));
        assert_eq!(report.summary.checksum_mismatch, 1);
        assert!(!report.overall_ready);
    }

    #[test]
    fn stale_binary_version_is_detected() {
        let obs = all_channels(|c| {
            let mut o = ChannelObservation::healthy(V);
            if c == ReleaseChannel::CratesIo {
                o.observed_version = Some("0.6.11".to_string());
            }
            o
        });
        let report = ReleaseVerificationReport::build(V, &obs);
        let crates = report
            .channels
            .iter()
            .find(|c| c.channel == ReleaseChannel::CratesIo)
            .unwrap();
        assert_eq!(crates.state, ChannelState::Stale);
        assert!(crates.detail.contains("0.6.11"));
        assert_eq!(report.summary.stale, 1);
    }

    #[test]
    fn installer_script_failure_is_distinct() {
        let obs = all_channels(|c| {
            let mut o = ChannelObservation::healthy(V);
            if c == ReleaseChannel::InstallerScript {
                o.installer_ok = Some(false);
            }
            o
        });
        let report = ReleaseVerificationReport::build(V, &obs);
        let inst = report
            .channels
            .iter()
            .find(|c| c.channel == ReleaseChannel::InstallerScript)
            .unwrap();
        assert_eq!(inst.state, ChannelState::InstallerFailed);
        assert_eq!(report.summary.installer_failed, 1);
        assert!(!report.overall_ready);
    }

    #[test]
    fn network_unavailable_is_reported_not_assumed_ready() {
        let obs = all_channels(|c| {
            let mut o = ChannelObservation::healthy(V);
            if c == ReleaseChannel::Homebrew {
                o.reachable = false;
            }
            o
        });
        let report = ReleaseVerificationReport::build(V, &obs);
        let brew = report
            .channels
            .iter()
            .find(|c| c.channel == ReleaseChannel::Homebrew)
            .unwrap();
        assert_eq!(brew.state, ChannelState::NetworkUnavailable);
        assert_eq!(report.summary.network_unavailable, 1);
        // A channel we could not check must never be silently treated as ready.
        assert!(!report.overall_ready);
        assert!(brew.manual_next_action.is_some());
    }

    #[test]
    fn ahead_or_not_configured_channels_are_ready() {
        let obs = vec![
            (
                ReleaseChannel::GithubRelease,
                ChannelObservation::healthy("0.6.14"),
            ),
            (
                ReleaseChannel::CratesIo,
                ChannelObservation {
                    configured: false,
                    ..Default::default()
                },
            ),
        ];
        let report = ReleaseVerificationReport::build(V, &obs);
        assert!(
            report.overall_ready,
            "ahead + not-configured are both ready: {report:?}"
        );
        assert_eq!(report.summary.ready, 2);
        assert_eq!(report.channels[1].state, ChannelState::NotConfigured);
    }

    #[test]
    fn json_contract_is_stable_and_round_trips() {
        let obs = all_channels(|_| ChannelObservation::healthy(V));
        let report = ReleaseVerificationReport::build(V, &obs);
        let value = serde_json::to_value(&report).expect("serialize");
        assert_eq!(value["schema_version"], RELEASE_VERIFY_SCHEMA_VERSION);
        assert_eq!(value["expected_version"], V);
        assert_eq!(value["overall_ready"], true);
        assert_eq!(value["channels"][0]["channel"], "github_release");
        assert_eq!(value["channels"][0]["state"], "up_to_date");
        let back: ReleaseVerificationReport = serde_json::from_value(value).expect("deserialize");
        assert_eq!(back, report);
    }

    #[test]
    fn verify_from_json_drives_a_complete_release_fixture() {
        let input = r#"{
            "expected_version": "0.6.13",
            "channels": [
                {"channel":"github_release","configured":true,"reachable":true,"observed_version":"0.6.13","checksum_ok":true},
                {"channel":"homebrew","configured":true,"reachable":true,"observed_version":"0.6.13","dispatch_ran":true},
                {"channel":"crates_io","configured":true,"reachable":true,"observed_version":"0.6.13"}
            ]
        }"#;
        let report = verify_from_json(input).expect("parse request");
        assert!(
            report.overall_ready,
            "complete-release fixture should be ready: {report:?}"
        );
        assert_eq!(report.summary.total, 3);
        assert_eq!(report.expected_version, "0.6.13");
    }

    #[test]
    fn verify_from_json_flags_lagging_dispatch_channel() {
        let input = r#"{
            "expected_version": "0.6.13",
            "channels": [
                {"channel":"github_release","configured":true,"reachable":true,"observed_version":"0.6.13","checksum_ok":true},
                {"channel":"scoop","configured":true,"reachable":true,"observed_version":"0.6.12","dispatch_ran":false}
            ]
        }"#;
        let report = verify_from_json(input).expect("parse request");
        assert!(!report.overall_ready);
        let scoop = report
            .channels
            .iter()
            .find(|c| c.channel == ReleaseChannel::Scoop)
            .unwrap();
        assert_eq!(scoop.state, ChannelState::Missing);
        assert!(scoop.manual_next_action.is_some());
    }

    #[test]
    fn verify_request_round_trips_through_its_json_contract() {
        let request = ReleaseVerifyRequest {
            expected_version: "0.6.13".to_string(),
            channels: vec![ChannelObservationInput {
                channel: ReleaseChannel::GithubRelease,
                configured: true,
                reachable: true,
                observed_version: Some("0.6.13".to_string()),
                checksum_ok: Some(true),
                dispatch_ran: None,
                installer_ok: None,
            }],
        };
        let json = serde_json::to_string(&request).unwrap();
        let back: ReleaseVerifyRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, request);
    }

    #[test]
    fn version_parser_tolerates_v_prefix_and_suffix() {
        assert_eq!(
            parse_release_version("v0.6.13"),
            Version::parse("0.6.13").ok()
        );
        assert_eq!(
            parse_release_version("0.6.13-dirty"),
            Version::parse("0.6.13").ok()
        );
        assert!(parse_release_version("not-a-version").is_none());
    }
}
