//! Cheap, bounded fleet probes that never let one host block the report.
//!
//! Bead: coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.6.2
//! ("Implement cheap bounded probes that never block the fleet report").
//!
//! A fleet sweep contacts many heterogeneous hosts; any one of them can be slow,
//! unreachable, or running an old binary. The rule is that **no single host
//! blocks the report**: each host gets a per-host time budget, a host that
//! exceeds it becomes an explicit `timed-out` state (preserving whatever partial
//! facts were gathered), and the fleet report is always produced. This module is
//! the pure aggregation/budget-enforcement core over the bead-6.1 schema — the
//! caller runs the actual probes (with real timeouts) and feeds outcomes in, so
//! the policy is deterministic and unit-testable. No mutation, no source-log
//! writes happen here; it is inert data.

use crate::fleet_doctor_schema::{
    FleetDoctorReport, HostDoctorReport, HostProbeStatus,
};
use serde::{Deserialize, Serialize};

/// Per-host and overall time budgets for a bounded fleet sweep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeBudget {
    /// Max wall-clock any single host may take before it is forced to a
    /// `timed-out` state so it cannot block the report.
    pub per_host_ms: u64,
    /// Soft overall budget; once exceeded, remaining un-probed hosts should be
    /// recorded as skipped rather than probed (see [`should_stop_sweep`]).
    pub total_ms: u64,
}

impl Default for ProbeBudget {
    fn default() -> Self {
        Self {
            per_host_ms: 8_000,
            total_ms: 60_000,
        }
    }
}

/// The raw outcome of probing one host: whatever [`HostDoctorReport`] was
/// assembled (possibly partial), the wall-clock it took, and a compact captured
/// error/stderr summary (never a raw dump).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostProbeOutcome {
    pub report: HostDoctorReport,
    pub elapsed_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Enforce a host's per-host budget on its (possibly partial) report. A host that
/// overran its budget — and that is not already a hard failure
/// (unreachable / command-not-found) — is downgraded to `timed-out` with its
/// partial facts preserved and a recommended next probe, so it never blocks the
/// fleet report. The host's `elapsed_ms` is always recorded.
pub fn finalize_host(mut report: HostDoctorReport, elapsed_ms: u64, per_host_ms: u64) -> HostDoctorReport {
    report.elapsed_ms = elapsed_ms;
    let overran = per_host_ms > 0 && elapsed_ms >= per_host_ms;
    if overran && !report.status.is_hard_failure() && report.status != HostProbeStatus::TimedOut {
        report.status = HostProbeStatus::TimedOut;
        report.timed_out = true;
        if !report
            .skipped_sections
            .iter()
            .any(|s| s == "deep-probe")
        {
            report.skipped_sections.push("deep-probe".to_string());
        }
        if report.recommended_action.is_none() {
            report.recommended_action =
                Some("host exceeded its probe budget; retry with a higher --per-host-budget or run `cass status` on that host".to_string());
        }
    }
    report
}

/// Whether the sweep should stop probing further hosts because the overall
/// budget is spent (remaining hosts get recorded as skipped rather than blocking).
pub fn should_stop_sweep(elapsed_ms: u64, budget: ProbeBudget) -> bool {
    budget.total_ms > 0 && elapsed_ms >= budget.total_ms
}

/// Assemble a bounded [`FleetDoctorReport`] from per-host probe outcomes,
/// enforcing the per-host budget on each. Every host appears in the report
/// (identity preserved); no single slow/unreachable host blocks it.
pub fn assemble_fleet(outcomes: Vec<HostProbeOutcome>, budget: ProbeBudget) -> FleetDoctorReport {
    let hosts: Vec<HostDoctorReport> = outcomes
        .into_iter()
        .map(|o| finalize_host(o.report, o.elapsed_ms, budget.per_host_ms))
        .collect();
    FleetDoctorReport::from_hosts(hosts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet_doctor_schema::{ArchiveRisk, HostOs, Platform, ReadinessState};

    fn host(alias: &str, status: HostProbeStatus) -> HostDoctorReport {
        HostDoctorReport::skeleton(alias, Platform::linux_x86_64(), status, 0)
    }

    #[test]
    fn fast_host_with_status_timeout_keeps_partial_facts() {
        // ts2: health was fast, but the deep status probe overran the per-host
        // budget. The host must become timed-out yet keep the facts it gathered.
        let mut r = host("ts2", HostProbeStatus::Ok);
        r.readiness = Some(ReadinessState::Ready); // a fact gathered before the overrun
        let finalized = finalize_host(r, 9_000, 8_000);
        assert_eq!(finalized.status, HostProbeStatus::TimedOut);
        assert!(finalized.timed_out);
        assert_eq!(finalized.elapsed_ms, 9_000);
        assert!(finalized.skipped_sections.iter().any(|s| s == "deep-probe"));
        assert_eq!(finalized.readiness, Some(ReadinessState::Ready), "partial facts preserved");
        assert!(finalized.recommended_action.is_some(), "next command provided");
    }

    #[test]
    fn unreachable_host_is_not_downgraded_to_timeout() {
        // mac-mini-old: SSH timeout => already Unreachable; budget overrun must not
        // mask the hard failure.
        let finalized = finalize_host(host("mac-mini-old", HostProbeStatus::Unreachable), 12_000, 8_000);
        assert_eq!(finalized.status, HostProbeStatus::Unreachable);
        assert!(finalized.unreachable);
    }

    #[test]
    fn old_binary_within_budget_is_preserved() {
        // csd: old binary, probed quickly => OldBinarySkew stays as-is.
        let finalized = finalize_host(host("csd", HostProbeStatus::OldBinarySkew), 200, 8_000);
        assert_eq!(finalized.status, HostProbeStatus::OldBinarySkew);
        assert!(!finalized.timed_out);
    }

    #[test]
    fn healthy_and_macos_hosts_within_budget_stay_ok() {
        let healthy = finalize_host(host("local", HostProbeStatus::Ok), 50, 8_000);
        assert_eq!(healthy.status, HostProbeStatus::Ok);

        // mac-mini-max: BSD/GNU tool difference noted in platform, probed fast.
        let mut mac = HostDoctorReport::skeleton(
            "mac-mini-max",
            Platform {
                os: HostOs::MacOs,
                arch: "aarch64".to_string(),
                path_style: crate::fleet_doctor_schema::PathStyle::Posix,
                tool_notes: vec!["date=bsd".to_string()],
            },
            HostProbeStatus::Ok,
            0,
        );
        mac.archive_risk = ArchiveRisk::Low;
        let mac = finalize_host(mac, 120, 8_000);
        assert_eq!(mac.status, HostProbeStatus::Ok);
        assert_eq!(mac.platform.os, HostOs::MacOs);
        assert!(mac.platform.tool_notes.iter().any(|n| n == "date=bsd"));
    }

    #[test]
    fn assemble_fleet_includes_every_host_and_no_host_blocks() {
        let outcomes = vec![
            HostProbeOutcome { report: host("local", HostProbeStatus::Ok), elapsed_ms: 40, error: None },
            HostProbeOutcome {
                report: { let mut r = host("ts2", HostProbeStatus::Ok); r.readiness = Some(ReadinessState::Ready); r },
                elapsed_ms: 9_000, // overruns -> timed-out
                error: None,
            },
            HostProbeOutcome {
                report: host("mac-mini-old", HostProbeStatus::Unreachable),
                elapsed_ms: 12_000,
                error: Some("ssh: connect timeout".to_string()),
            },
            HostProbeOutcome { report: host("csd", HostProbeStatus::OldBinarySkew), elapsed_ms: 200, error: None },
        ];
        let report = assemble_fleet(outcomes, ProbeBudget { per_host_ms: 8_000, total_ms: 60_000 });
        assert_eq!(report.hosts.len(), 4, "every host appears; none is dropped");
        assert_eq!(report.summary.total_hosts, 4);
        // ts2 was forced to timed-out by the per-host budget; identity + facts kept.
        let ts2 = report.hosts.iter().find(|h| h.host_alias == "ts2").unwrap();
        assert_eq!(ts2.status, HostProbeStatus::TimedOut);
        assert_eq!(ts2.readiness, Some(ReadinessState::Ready));
        // unreachable stays unreachable.
        let old = report.hosts.iter().find(|h| h.host_alias == "mac-mini-old").unwrap();
        assert_eq!(old.status, HostProbeStatus::Unreachable);
        assert_eq!(report.summary.unreachable, 1);
        assert_eq!(report.summary.timed_out, 1);
        assert_eq!(report.summary.ok, 1);
        // Whole report round-trips.
        let value = serde_json::to_value(&report).unwrap();
        let back: FleetDoctorReport = serde_json::from_value(value).unwrap();
        assert_eq!(back, report);
    }

    #[test]
    fn should_stop_sweep_respects_total_budget() {
        let b = ProbeBudget { per_host_ms: 8_000, total_ms: 60_000 };
        assert!(!should_stop_sweep(30_000, b));
        assert!(should_stop_sweep(60_000, b));
        assert!(should_stop_sweep(120_000, b));
    }

    #[test]
    fn zero_per_host_budget_does_not_force_timeout() {
        // A disabled per-host budget (0) must not spuriously time out a host.
        let finalized = finalize_host(host("local", HostProbeStatus::Ok), 999_999, 0);
        assert_eq!(finalized.status, HostProbeStatus::Ok);
        assert_eq!(finalized.elapsed_ms, 999_999);
    }

    #[test]
    fn default_budget_is_bounded() {
        let b = ProbeBudget::default();
        assert!(b.per_host_ms > 0 && b.total_ms >= b.per_host_ms);
    }
}
