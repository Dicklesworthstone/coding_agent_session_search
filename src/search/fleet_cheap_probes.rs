// Dead-code tolerated module-wide: this cheap-probe scheduling/classification
// layer lands ahead of the fleet-doctor runner that executes the probes and
// emits the .6.1 HostDoctorReport / FleetDoctorReport.
#![allow(dead_code)]

//! Cheap bounded fleet-doctor probes that never block the report (bead
//! cass-fleet-resilience-20260608-uojcg.6.2).
//!
//! Fleet doctor must run cheap probes first and return useful partial JSON
//! even when a host's status/doctor is slow, the host is unreachable, or it
//! runs an old binary — and **no single host may block the report**. This
//! module is the scheduling + classification layer on top of the `.6.1`
//! schema: it defines the cheap-first probe order, classifies a host's
//! outcome from its probe signals into the shared
//! [`HostProbeStatus`](crate::fleet_doctor_schema::HostProbeStatus) (timeout
//! is a first-class state), records which phases were skipped and the partial
//! facts that survived, names the next command, and assembles per-host
//! outcomes independently so one slow/unreachable host never stalls the rest.
//!
//! No remote mutation and no source-log writes are modeled here — this is a
//! pure, deterministic decision layer (signals in, outcome out). `ProbePhase`
//! serializes snake_case; `HostProbeStatus` keeps its `.6.1` kebab-case form.

use serde::{Deserialize, Serialize};

use crate::fleet_doctor_schema::HostProbeStatus;

/// A fleet-doctor probe phase, cheapest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProbePhase {
    /// Can we reach the host at all? (cheapest)
    Reachability,
    /// Is the `cass` binary present and contract-compatible?
    Version,
    /// A fast/cached health read.
    CheapHealth,
    /// The heavier status probe (deep state).
    DeepStatus,
    /// The heaviest doctor probe.
    DeepDoctor,
}

impl ProbePhase {
    /// Cheap-first order. The runner walks this and stops descending once a
    /// host's budget/health says deeper probes would block.
    pub(crate) fn cheap_first_order() -> &'static [ProbePhase] {
        &[
            ProbePhase::Reachability,
            ProbePhase::Version,
            ProbePhase::CheapHealth,
            ProbePhase::DeepStatus,
            ProbePhase::DeepDoctor,
        ]
    }

    /// Whether this phase is cheap (always attempted before the budget can be
    /// blown by a deep probe).
    pub(crate) fn is_cheap(self) -> bool {
        matches!(self, Self::Reachability | Self::Version | Self::CheapHealth)
    }
}

/// The signals a host's cheap probes produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HostProbeSignals {
    /// The host was reachable (SSH/transport succeeded).
    pub reachable: bool,
    /// `cass` binary contract-compatible. `None` when not yet checked.
    pub version_compatible: Option<bool>,
    /// The cheap/cached health read succeeded.
    pub cheap_health_ok: bool,
    /// The deep status probe exceeded its budget.
    pub deep_status_timed_out: bool,
    /// A BSD/GNU tool difference was observed (macOS heterogeneity).
    pub tool_difference: bool,
    /// Whether the host is macOS (path/tooling differences apply).
    pub is_macos: bool,
}

/// The classified per-host probe outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HostProbeOutcome {
    pub host_alias: String,
    pub status: HostProbeStatus,
    /// Phases skipped (deferred) to keep the host within budget.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped_phases: Vec<ProbePhase>,
    /// Partial facts that survived the budget (compact, no raw logs).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub partial_facts: Vec<String>,
    /// The next command for this host (concrete; never bare/destructive).
    pub next_command: String,
    pub elapsed_ms: u64,
}

/// Classify a host's probe outcome from its signals, cheap-first and
/// non-blocking: a host that can't be probed deeply yields partial facts plus
/// a next command rather than stalling.
pub(crate) fn classify_host(
    host_alias: impl Into<String>,
    signals: HostProbeSignals,
    elapsed_ms: u64,
) -> HostProbeOutcome {
    let host_alias = host_alias.into();
    let deep = vec![ProbePhase::DeepStatus, ProbePhase::DeepDoctor];

    // Unreachable: cheapest probe failed — return identity only, never block.
    if !signals.reachable {
        return HostProbeOutcome {
            host_alias,
            status: HostProbeStatus::Unreachable,
            skipped_phases: vec![
                ProbePhase::Version,
                ProbePhase::CheapHealth,
                ProbePhase::DeepStatus,
                ProbePhase::DeepDoctor,
            ],
            partial_facts: vec!["host identity only; transport failed".to_string()],
            next_command: "cass fleet doctor --json   # retry from a reachable node".to_string(),
            elapsed_ms,
        };
    }

    // Old binary: skip deep probes whose schema the old binary may not honor.
    if signals.version_compatible == Some(false) {
        return HostProbeOutcome {
            host_alias,
            status: HostProbeStatus::OldBinarySkew,
            skipped_phases: deep,
            partial_facts: vec!["reachable; cass version behind the fleet baseline".to_string()],
            next_command: "cass self-update   # upgrade this host before trusting deep state"
                .to_string(),
            elapsed_ms,
        };
    }

    // Reachable + cheap health OK but the deep status probe timed out: return
    // the cheap facts, defer the deep probes (the ts2 fast-health/slow-status
    // case). Timeout is first-class.
    if signals.cheap_health_ok && signals.deep_status_timed_out {
        let mut facts = vec!["cheap health ok; deep status exceeded its budget".to_string()];
        if signals.tool_difference {
            facts.push("BSD/GNU tool difference observed (macOS)".to_string());
        }
        return HostProbeOutcome {
            host_alias,
            status: HostProbeStatus::TimedOut,
            skipped_phases: deep,
            partial_facts: facts,
            next_command: "cass status --json   # re-run with a larger --budget-ms".to_string(),
            elapsed_ms,
        };
    }

    // Reachable, macOS tool difference but otherwise fine: a partial result
    // noting the heterogeneity rather than a failure.
    if signals.tool_difference || signals.is_macos {
        return HostProbeOutcome {
            host_alias,
            status: HostProbeStatus::Partial,
            skipped_phases: Vec::new(),
            partial_facts: vec!["probed with macOS BSD/GNU tool adaptations".to_string()],
            next_command: "cass status --json".to_string(),
            elapsed_ms,
        };
    }

    // Everything cheap + deep succeeded.
    HostProbeOutcome {
        host_alias,
        status: HostProbeStatus::Ok,
        skipped_phases: Vec::new(),
        partial_facts: Vec::new(),
        next_command: "cass status --json".to_string(),
        elapsed_ms,
    }
}

/// The assembled fleet probe report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FleetProbeReport {
    pub hosts: Vec<HostProbeOutcome>,
    /// Number of hosts that returned anything other than `Ok`.
    pub degraded_host_count: usize,
    /// Always false: the contract is that no single host blocks the report.
    pub blocked: bool,
}

/// Assemble per-host outcomes into a fleet report. Each host is independent —
/// one slow/unreachable host never drops or blocks the others.
pub(crate) fn assemble(hosts: Vec<HostProbeOutcome>) -> FleetProbeReport {
    let degraded_host_count = hosts
        .iter()
        .filter(|h| h.status != HostProbeStatus::Ok)
        .count();
    FleetProbeReport {
        hosts,
        degraded_host_count,
        blocked: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn healthy() -> HostProbeSignals {
        HostProbeSignals {
            reachable: true,
            version_compatible: Some(true),
            cheap_health_ok: true,
            deep_status_timed_out: false,
            tool_difference: false,
            is_macos: false,
        }
    }

    fn assert_safe_command(cmd: &str) {
        assert!(
            cmd.starts_with("cass "),
            "must be a concrete cass command: {cmd}"
        );
        for bad in ["rm ", "--force-clean", "delete ", "DROP "] {
            assert!(!cmd.contains(bad), "destructive: {cmd}");
        }
    }

    #[test]
    fn probe_phase_order_is_cheap_first() {
        let order = ProbePhase::cheap_first_order();
        assert_eq!(order[0], ProbePhase::Reachability);
        // All cheap phases precede all deep ones.
        let first_deep = order.iter().position(|p| !p.is_cheap()).unwrap();
        assert!(order[..first_deep].iter().all(|p| p.is_cheap()));
        assert!(order[first_deep..].iter().all(|p| !p.is_cheap()));
    }

    #[test]
    fn healthy_host_is_ok_with_no_skips() {
        let o = classify_host("local", healthy(), 40);
        assert_eq!(o.status, HostProbeStatus::Ok);
        assert!(o.skipped_phases.is_empty());
        assert_safe_command(&o.next_command);
    }

    #[test]
    fn ts2_fast_health_slow_status_is_timed_out_partial() {
        let mut s = healthy();
        s.deep_status_timed_out = true;
        let o = classify_host("ts2", s, 5_000);
        assert_eq!(o.status, HostProbeStatus::TimedOut);
        assert!(o.skipped_phases.contains(&ProbePhase::DeepStatus));
        assert!(!o.partial_facts.is_empty(), "partial facts preserved");
        assert_safe_command(&o.next_command);
    }

    #[test]
    fn unreachable_host_returns_identity_only_and_never_blocks() {
        let mut s = healthy();
        s.reachable = false;
        let o = classify_host("mac-mini-old", s, 30_000);
        assert_eq!(o.status, HostProbeStatus::Unreachable);
        assert!(o.skipped_phases.contains(&ProbePhase::DeepDoctor));
        assert!(o.next_command.contains("reachable node"));
    }

    #[test]
    fn old_binary_skews_and_skips_deep_probes() {
        let mut s = healthy();
        s.version_compatible = Some(false);
        let o = classify_host("csd", s, 80);
        assert_eq!(o.status, HostProbeStatus::OldBinarySkew);
        assert!(o.skipped_phases.contains(&ProbePhase::DeepStatus));
        assert!(o.next_command.contains("self-update"));
    }

    #[test]
    fn macos_tool_difference_is_partial_not_failure() {
        let mut s = healthy();
        s.tool_difference = true;
        s.is_macos = true;
        let o = classify_host("mac-mini-max", s, 120);
        assert_eq!(o.status, HostProbeStatus::Partial);
        assert!(o.partial_facts.iter().any(|f| f.contains("macOS")));
    }

    #[test]
    fn one_unreachable_host_does_not_block_or_drop_the_others() {
        let mut down = healthy();
        down.reachable = false;
        let mut slow = healthy();
        slow.deep_status_timed_out = true;
        let report = assemble(vec![
            classify_host("local", healthy(), 40),
            classify_host("mac-mini-old", down, 30_000),
            classify_host("ts2", slow, 5_000),
        ]);
        // No host dropped; report is never blocked.
        assert_eq!(report.hosts.len(), 3);
        assert!(!report.blocked);
        assert_eq!(report.degraded_host_count, 2);
        // The healthy host is still reported fully.
        assert_eq!(report.hosts[0].status, HostProbeStatus::Ok);
    }

    #[test]
    fn report_round_trips_through_json() {
        let report = assemble(vec![classify_host(
            "ts2",
            {
                let mut s = healthy();
                s.deep_status_timed_out = true;
                s
            },
            5_000,
        )]);
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("\"blocked\":false"));
        assert!(json.contains("\"status\":\"timed-out\""));
        assert!(json.contains("\"skipped_phases\":[\"deep_status\""));
        let parsed: FleetProbeReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, report);
    }
}
