//! Per-host CASS version-skew detection and upgrade recommendations.
//!
//! Bead: coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.6.3
//! ("Report version skew and upgrade recommendations per host").
//!
//! Builds on the fleet-doctor schema (bead `9.1`/`6.1`): given a
//! [`HostDoctorReport`] and the controller's current repo version, decide
//! whether the host's `cass` binary is behind, how large the capability gap is,
//! whether the binary must be upgraded *before* any repair is attempted (an old
//! binary may lack the repair/doctor surfaces entirely), and what
//! platform-specific install hint to surface — **without assuming every host can
//! self-update**. macOS and other hosts frequently need a manual installer path.
//!
//! The fleet evidence that motivated this: `csd` and `mac-mini-max` ran 0.4.1,
//! `ts2` ran 0.6.10, and only some hosts exposed modern doctor/status fields.
//!
//! This is pure, side-effect-free logic over the schema; producers populate the
//! `HostDoctorReport`, this module turns it into an actionable
//! [`VersionAssessment`]. Dependent `6.6` builds upgrade rehearsal on top.

use crate::fleet_doctor_schema::{CapabilityTier, HostDoctorReport, HostOs, HostProbeStatus};
use semver::Version;
use serde::{Deserialize, Serialize};

/// Stable schema version for the version-assessment wire format.
pub const VERSION_SKEW_SCHEMA_VERSION: u32 = 1;

/// How far behind the host binary is, in capability terms (not just numeric
/// distance). Ordered none→missing so a fleet rollup can take a `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CapabilityGap {
    /// Host is at the current version (or ahead) with full capabilities.
    None,
    /// Patch/minor behind; safe to keep operating, upgrade recommended.
    Minor,
    /// Major behind (or missing modern doctor/status surfaces); features the
    /// controller relies on may be absent.
    Major,
    /// The host's version could not be parsed/compared.
    Unknown,
    /// `cass` is not installed / not on PATH on the host.
    BinaryMissing,
}

/// Whether and how the host can be upgraded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UpgradeMethod {
    /// The host's binary supports `cass self-update` (or is recent enough to).
    SelfUpdate,
    /// A platform package manager / installer is the recommended path.
    ManualInstaller,
    /// No supported upgrade path for this platform/tooling — operator must
    /// intervene.
    Unsupported,
}

/// Platform-specific, prose-light guidance for getting the host current.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallHint {
    /// OS the hint targets.
    pub platform: HostOs,
    /// Recommended upgrade method.
    pub method: UpgradeMethod,
    /// The single best command to run, when one exists (e.g.
    /// `"cass self-update"`, `"brew upgrade cass"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Manual fallback steps (installer URL check, package manager), for hosts
    /// that cannot or should not self-update.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub manual_steps: Vec<String>,
}

/// The per-host version assessment: observed vs current, the capability gap, and
/// an actionable upgrade recommendation. Serializes with stable snake_case
/// fields and an embedded [`schema_version`](Self::schema_version).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionAssessment {
    /// Mirrors [`VERSION_SKEW_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Host identity, copied from the report so the assessment stands alone.
    pub host_alias: String,
    /// The version string the host reported, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_version: Option<String>,
    /// The controller's current repo / latest-known version.
    pub current_repo_version: String,
    /// Capability gap classification.
    pub capability_gap: CapabilityGap,
    /// Whether an upgrade is needed at all.
    pub upgrade_needed: bool,
    /// Whether the upgrade must happen BEFORE any repair is attempted (old
    /// binaries may lack the repair surfaces, so repairing first is unsafe).
    pub upgrade_before_repair: bool,
    /// Platform-specific install/upgrade guidance.
    pub install_hint: InstallHint,
}

/// Parse a version string into a comparable [`Version`], tolerating a leading
/// `v` and trailing build/pre-release noise beyond `x.y.z`.
fn parse_version(raw: &str) -> Option<Version> {
    let trimmed = raw.trim().trim_start_matches('v').trim_start_matches('V');
    if let Ok(version) = Version::parse(trimmed) {
        // Normalize to the major.minor.patch core: for upgrade/skew decisions a
        // `-dirty`/`-rc1` build is treated as its release version, so pre-release
        // ordering can't surprise us (e.g. flag "0.7.0-beta" as behind "0.7.0").
        return Some(Version::new(version.major, version.minor, version.patch));
    }
    // Fall back to the leading `x.y.z` triple if the full string is not strict
    // semver (e.g. "0.6.10-dirty" without a proper pre-release tag).
    let mut parts = trimmed.split(['.', '-', '+', ' ']);
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().unwrap_or(0);
    let patch = parts.next().unwrap_or("0").parse().unwrap_or(0);
    Some(Version::new(major, minor, patch))
}

/// Build the platform-specific install hint for a host that needs attention.
fn install_hint_for(platform: HostOs, method: UpgradeMethod) -> InstallHint {
    match platform {
        HostOs::Linux => InstallHint {
            platform,
            method,
            command: match method {
                UpgradeMethod::SelfUpdate => Some("cass self-update".to_string()),
                UpgradeMethod::ManualInstaller => {
                    Some("curl -fsSL https://cass.sh/install.sh | bash".to_string())
                }
                UpgradeMethod::Unsupported => None,
            },
            manual_steps: vec![
                "verify installed cass on PATH: `command -v cass && cass --version`".to_string(),
                "re-run the official installer if self-update is unavailable".to_string(),
            ],
        },
        HostOs::MacOs => InstallHint {
            platform,
            // macOS hosts often cannot self-update cleanly; prefer the package
            // manager / installer path.
            method: if method == UpgradeMethod::SelfUpdate {
                UpgradeMethod::ManualInstaller
            } else {
                method
            },
            command: Some("brew upgrade cass || brew install cass".to_string()),
            manual_steps: vec![
                "if Homebrew is unavailable, run the official macOS installer".to_string(),
                "check for BSD vs GNU tool differences (rsync/coreutils) before repair".to_string(),
            ],
        },
        HostOs::Windows => InstallHint {
            platform,
            method: UpgradeMethod::ManualInstaller,
            command: Some("scoop update cass".to_string()),
            manual_steps: vec![
                "if Scoop is unavailable, download the latest Windows release manually".to_string(),
            ],
        },
        HostOs::Other => InstallHint {
            platform,
            method: UpgradeMethod::Unsupported,
            command: None,
            manual_steps: vec![
                "no supported installer for this platform; build from source or upgrade manually"
                    .to_string(),
            ],
        },
    }
}

/// Assess a single host's version skew against the controller's
/// `current_repo_version`. Pure: depends only on the report and the target
/// version string.
pub fn assess_host(host: &HostDoctorReport, current_repo_version: &str) -> VersionAssessment {
    let platform = host.platform.os;
    let current = parse_version(current_repo_version);

    // 1) Binary missing / unreachable-as-no-binary: must install before anything.
    let binary_absent = host.status == HostProbeStatus::CommandNotFound
        || (host.cass_version.is_none() && host.status != HostProbeStatus::Unreachable);
    if binary_absent {
        let method = match platform {
            HostOs::Other => UpgradeMethod::Unsupported,
            _ => UpgradeMethod::ManualInstaller,
        };
        return VersionAssessment {
            schema_version: VERSION_SKEW_SCHEMA_VERSION,
            host_alias: host.host_alias.clone(),
            observed_version: None,
            current_repo_version: current_repo_version.to_string(),
            capability_gap: CapabilityGap::BinaryMissing,
            upgrade_needed: true,
            upgrade_before_repair: true,
            install_hint: install_hint_for(platform, method),
        };
    }

    let observed = host.cass_version.as_deref().and_then(parse_version);

    // 2) Could not parse either side → Unknown, recommend a manual check but do
    // not force upgrade-before-repair on a guess.
    let (Some(observed_ver), Some(current_ver)) = (observed, current) else {
        return VersionAssessment {
            schema_version: VERSION_SKEW_SCHEMA_VERSION,
            host_alias: host.host_alias.clone(),
            observed_version: host.cass_version.clone(),
            current_repo_version: current_repo_version.to_string(),
            capability_gap: CapabilityGap::Unknown,
            upgrade_needed: false,
            upgrade_before_repair: false,
            install_hint: install_hint_for(platform, UpgradeMethod::ManualInstaller),
        };
    };

    // 3) Numeric comparison, refined by the reported capability tier.
    let minimal_tier = matches!(host.capability_tier, Some(CapabilityTier::Minimal));
    let gap = if observed_ver >= current_ver {
        if minimal_tier {
            // Same version string but a minimal-tier build still lacks features.
            CapabilityGap::Minor
        } else {
            CapabilityGap::None
        }
    } else if observed_ver.major < current_ver.major
        || minimal_tier
        // Under 0.x semver a minor bump IS a breaking change, so a pre-1.0 host
        // that is a minor version behind (e.g. 0.4.x vs 0.6.x — the csd /
        // mac-mini-max fleet case this module was built for) may genuinely lack
        // the repair/doctor surfaces. Classify it as a Major gap so it takes the
        // deliberate installer path and upgrades before repair, rather than a
        // blind `self-update` from a binary too old to trust. A pure patch gap
        // within the same 0.x minor (e.g. 0.6.10 vs 0.6.13) stays Minor.
        || (current_ver.major == 0 && observed_ver.minor < current_ver.minor)
    {
        CapabilityGap::Major
    } else {
        CapabilityGap::Minor
    };

    let upgrade_needed = gap != CapabilityGap::None;
    // A major gap (or a known status of old-binary skew) means the binary may
    // lack the repair surfaces; upgrade must precede repair.
    let upgrade_before_repair =
        gap == CapabilityGap::Major || host.status == HostProbeStatus::OldBinarySkew;

    // Big jumps (or known old-binary skew) take a deliberate installer path
    // rather than a blind self-update from a very old binary; otherwise the
    // self-update path is the recommendation (and is informational when no
    // upgrade is needed).
    let method = if upgrade_before_repair {
        UpgradeMethod::ManualInstaller
    } else {
        UpgradeMethod::SelfUpdate
    };

    VersionAssessment {
        schema_version: VERSION_SKEW_SCHEMA_VERSION,
        host_alias: host.host_alias.clone(),
        observed_version: host.cass_version.clone(),
        current_repo_version: current_repo_version.to_string(),
        capability_gap: gap,
        upgrade_needed,
        upgrade_before_repair,
        install_hint: install_hint_for(platform, method),
    }
}

/// Assess every host in a fleet against `current_repo_version`.
pub fn assess_fleet(
    hosts: &[HostDoctorReport],
    current_repo_version: &str,
) -> Vec<VersionAssessment> {
    hosts
        .iter()
        .map(|host| assess_host(host, current_repo_version))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet_doctor_schema::{CapabilityTier, Platform};

    const CURRENT: &str = "0.6.13";

    fn host_with(alias: &str, version: Option<&str>, status: HostProbeStatus) -> HostDoctorReport {
        let mut h = HostDoctorReport::skeleton(alias, Platform::linux_x86_64(), status, 50);
        h.cass_version = version.map(str::to_string);
        h
    }

    #[test]
    fn current_binary_has_no_gap_and_no_upgrade() {
        let host = host_with("local", Some("0.6.13"), HostProbeStatus::Ok);
        let a = assess_host(&host, CURRENT);
        assert_eq!(a.capability_gap, CapabilityGap::None);
        assert!(!a.upgrade_needed);
        assert!(!a.upgrade_before_repair);
    }

    #[test]
    fn newer_binary_is_not_flagged() {
        let host = host_with("ahead", Some("0.7.0"), HostProbeStatus::Ok);
        let a = assess_host(&host, CURRENT);
        assert_eq!(a.capability_gap, CapabilityGap::None);
        assert!(!a.upgrade_needed);
    }

    #[test]
    fn mid_version_binary_is_minor_gap_and_can_self_update() {
        // ts2 ran 0.6.10 vs current 0.6.13.
        let host = host_with("ts2", Some("0.6.10"), HostProbeStatus::Ok);
        let a = assess_host(&host, CURRENT);
        assert_eq!(a.capability_gap, CapabilityGap::Minor);
        assert!(a.upgrade_needed);
        assert!(
            !a.upgrade_before_repair,
            "minor gap can be repaired without upgrading first"
        );
        assert_eq!(a.install_hint.method, UpgradeMethod::SelfUpdate);
        assert_eq!(a.install_hint.command.as_deref(), Some("cass self-update"));
    }

    #[test]
    fn old_major_binary_requires_upgrade_before_repair() {
        // csd / mac-mini-max ran 0.4.1 vs current 0.6.13.
        let host = host_with("csd", Some("0.4.1"), HostProbeStatus::Ok);
        let a = assess_host(&host, CURRENT);
        // 0.4 vs 0.6: same major (0) but treated as a large jump via minor.
        assert!(a.upgrade_needed);
        // Make the old-binary skew explicit and assert upgrade-before-repair.
        let mut skewed = host;
        skewed.status = HostProbeStatus::OldBinarySkew;
        let a2 = assess_host(&skewed, CURRENT);
        assert!(
            a2.upgrade_before_repair,
            "old-binary-skew must upgrade before repair"
        );
        assert_eq!(a2.install_hint.method, UpgradeMethod::ManualInstaller);
    }

    #[test]
    fn major_version_jump_is_major_gap() {
        let host = host_with("ancient", Some("0.4.1"), HostProbeStatus::Ok);
        // Compare against a different major to exercise the major branch.
        let a = assess_host(&host, "1.0.0");
        assert_eq!(a.capability_gap, CapabilityGap::Major);
        assert!(a.upgrade_before_repair);
        assert_eq!(a.install_hint.method, UpgradeMethod::ManualInstaller);
    }

    #[test]
    fn cass_missing_forces_install_before_repair() {
        let host = host_with("ts2", None, HostProbeStatus::CommandNotFound);
        let a = assess_host(&host, CURRENT);
        assert_eq!(a.capability_gap, CapabilityGap::BinaryMissing);
        assert!(a.upgrade_needed);
        assert!(a.upgrade_before_repair);
        assert!(a.observed_version.is_none());
        assert_eq!(a.install_hint.method, UpgradeMethod::ManualInstaller);
    }

    #[test]
    fn minimal_capability_tier_forces_at_least_minor_even_at_current_version() {
        let mut host = host_with("legacy-tier", Some("0.6.13"), HostProbeStatus::Ok);
        host.capability_tier = Some(CapabilityTier::Minimal);
        let a = assess_host(&host, CURRENT);
        assert_eq!(a.capability_gap, CapabilityGap::Minor);
        assert!(a.upgrade_needed);
    }

    #[test]
    fn unsupported_platform_has_no_self_update_path() {
        let mut host = HostDoctorReport::skeleton(
            "exotic",
            Platform {
                os: HostOs::Other,
                arch: "riscv64".to_string(),
                path_style: crate::fleet_doctor_schema::PathStyle::Posix,
                tool_notes: vec![],
            },
            HostProbeStatus::CommandNotFound,
            10,
        );
        host.cass_version = None;
        let a = assess_host(&host, CURRENT);
        assert_eq!(a.capability_gap, CapabilityGap::BinaryMissing);
        assert_eq!(a.install_hint.method, UpgradeMethod::Unsupported);
        assert!(a.install_hint.command.is_none());
        assert!(!a.install_hint.manual_steps.is_empty());
    }

    #[test]
    fn macos_prefers_installer_over_self_update() {
        let mut host = HostDoctorReport::skeleton(
            "mac-mini-max",
            Platform {
                os: HostOs::MacOs,
                arch: "aarch64".to_string(),
                path_style: crate::fleet_doctor_schema::PathStyle::Posix,
                tool_notes: vec![],
            },
            HostProbeStatus::Ok,
            60,
        );
        host.cass_version = Some("0.6.10".to_string()); // minor gap → would be self-update on Linux
        let a = assess_host(&host, CURRENT);
        assert!(a.upgrade_needed);
        assert_eq!(
            a.install_hint.method,
            UpgradeMethod::ManualInstaller,
            "macOS should prefer the installer path even for a minor gap"
        );
        assert!(a.install_hint.command.as_deref().unwrap().contains("brew"));
    }

    #[test]
    fn unparseable_version_is_unknown_without_forcing_upgrade() {
        let host = host_with("weird", Some("garbage"), HostProbeStatus::Ok);
        // "garbage" → parse_version falls back: first token "garbage" fails u64 → None.
        let a = assess_host(&host, CURRENT);
        assert_eq!(a.capability_gap, CapabilityGap::Unknown);
        assert!(!a.upgrade_before_repair);
        assert_eq!(a.observed_version.as_deref(), Some("garbage"));
    }

    #[test]
    fn parse_version_tolerates_v_prefix_and_suffix() {
        assert_eq!(parse_version("v0.6.13"), Some(Version::new(0, 6, 13)));
        assert_eq!(parse_version("0.6.10-dirty"), Some(Version::new(0, 6, 10)));
        assert_eq!(parse_version("1.2"), Some(Version::new(1, 2, 0)));
        assert!(parse_version("garbage").is_none());
    }

    #[test]
    fn assessment_serializes_with_stable_fields() {
        let host = host_with("ts2", Some("0.6.10"), HostProbeStatus::Ok);
        let a = assess_host(&host, CURRENT);
        let value = serde_json::to_value(&a).unwrap();
        assert_eq!(value["host_alias"], "ts2");
        assert_eq!(value["observed_version"], "0.6.10");
        assert_eq!(value["current_repo_version"], "0.6.13");
        assert_eq!(value["capability_gap"], "minor");
        assert_eq!(value["upgrade_needed"], true);
        assert_eq!(value["upgrade_before_repair"], false);
        assert_eq!(value["install_hint"]["method"], "self-update");
        let back: VersionAssessment = serde_json::from_value(value).unwrap();
        assert_eq!(back, a);
    }

    #[test]
    fn assess_fleet_maps_every_host() {
        let hosts = vec![
            host_with("local", Some("0.6.13"), HostProbeStatus::Ok),
            host_with("ts2", Some("0.6.10"), HostProbeStatus::Ok),
            host_with("csd", Some("0.4.1"), HostProbeStatus::OldBinarySkew),
            host_with("gone", None, HostProbeStatus::CommandNotFound),
        ];
        let assessments = assess_fleet(&hosts, CURRENT);
        assert_eq!(assessments.len(), 4);
        assert_eq!(assessments[0].capability_gap, CapabilityGap::None);
        assert_eq!(assessments[1].capability_gap, CapabilityGap::Minor);
        assert!(assessments[2].upgrade_before_repair);
        assert_eq!(assessments[3].capability_gap, CapabilityGap::BinaryMissing);
    }
}
