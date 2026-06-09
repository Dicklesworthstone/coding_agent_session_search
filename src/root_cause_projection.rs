//! Project a likely root-cause family from observed evidence signals.
//!
//! Bead: coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.9.2
//! ("Project likely root-cause family in status doctor and fleet outputs").
//!
//! Bead `9.1` defines the attribution *contract* ([`RootCauseAttribution`],
//! [`RootCauseFamily`], [`EvidenceRef`], [`AttributionConfidence`]). This module
//! is the pure **classifier** that fills it: it maps a read-only
//! [`ProjectionSignals`] snapshot (the structured facts status/doctor/fleet and
//! incident mining already observe) to the single most likely family, with a
//! confidence that reflects how dominant and unambiguous the evidence is, the
//! supporting [`EvidenceRef`]s, and the cheap next probe.
//!
//! It deliberately honors each family's documented false-positive guidance:
//! lexical fail-open returning results is *designed degradation*, not a search
//! fault; a cold/never-indexed data dir is *not* derived-state corruption;
//! abundant free space rules out disk pressure. Signals that only describe
//! graceful behavior never, on their own, drive an attribution.

use crate::root_cause_taxonomy::{
    AttributionConfidence, EvidenceRef, RootCauseAttribution, RootCauseFamily,
};

/// Read-only evidence snapshot. Every field is a fact a diagnostic already
/// gathered; presence (not prose) drives attribution. Defaults are "no signal".
#[derive(Debug, Clone, Default)]
pub struct ProjectionSignals {
    // --- frankensqlite / storage ---
    /// A structured fsqlite error code was observed (direct evidence).
    pub fsqlite_error_code: Option<String>,
    /// An OpenRead / FTS read failure occurred.
    pub open_read_failure: bool,

    // --- frankensearch / search stack ---
    /// A search-stack error (fusion panic, tantivy segment corruption).
    pub frankensearch_error: bool,
    /// Lexical fail-open returned results (designed degradation — NOT a fault
    /// by itself; never drives an attribution alone).
    pub lexical_fail_open: bool,

    // --- cass derived state ---
    /// Two CASS-owned facts disagree (e.g. index count vs. truth table). A cold
    /// or empty data dir is NOT this; only a real mismatch counts.
    pub derived_truth_table_mismatch: bool,

    // --- asupersync runtime ---
    /// Work stalled with no underlying I/O progress, or cancellation failed.
    pub runtime_task_stall: bool,

    // --- remote transport / auth ---
    /// Remote authentication failed (publickey/permission denied).
    pub transport_auth_failure: bool,
    /// Remote ssh/transport non-zero exit, when known.
    pub transport_ssh_exit_code: Option<i32>,
    /// Remote connect timed out.
    pub transport_connect_timeout: bool,

    // --- semantic assets ---
    /// Semantic mode was requested/enabled but its assets are missing/partial
    /// (disabled-by-config is NOT a fault).
    pub semantic_requested_but_missing: bool,

    // --- workspace provenance ---
    /// Configured provenance disagrees with on-disk reality (moved/stale data
    /// dir, misclassified source).
    pub workspace_provenance_mismatch: bool,

    // --- host disk pressure ---
    /// Free-space percentage, when measured. Below ~5% is pressure.
    pub host_disk_free_pct: Option<f64>,

    // --- host OOM / load ---
    /// An OOM kill was recorded.
    pub host_oom_kill: bool,
    /// Load average vs. core count, when measured (ratio > ~4 is pressure).
    pub host_load_ratio: Option<f64>,

    // --- old binary / contract skew ---
    /// The running binary's contract/api version is behind on-disk/fleet need.
    pub binary_behind_contract: bool,
}

const DISK_PRESSURE_PCT: f64 = 5.0;
const LOAD_PRESSURE_RATIO: f64 = 4.0;

/// One scored family hit, with the evidence backing it.
struct FamilyHit {
    family: RootCauseFamily,
    /// 2 = direct/unambiguous, 1 = circumstantial.
    score: u32,
    evidence: Vec<EvidenceRef>,
    /// Direct evidence makes a single-family attribution `Confirmed`.
    direct: bool,
}

/// Collect the families implicated by the signals, each with score + evidence.
fn collect_hits(s: &ProjectionSignals) -> Vec<FamilyHit> {
    let mut hits: Vec<FamilyHit> = Vec::new();

    // frankensqlite storage — direct on an explicit error code / OpenRead.
    {
        let mut ev = Vec::new();
        let mut direct = false;
        if let Some(code) = &s.fsqlite_error_code {
            ev.push(EvidenceRef::new("fsqlite.error_code", "diag").with_detail(code.clone()));
            direct = true;
        }
        if s.open_read_failure {
            ev.push(EvidenceRef::new("fsqlite.open_read_failure", "diag"));
            direct = true;
        }
        if !ev.is_empty() {
            hits.push(FamilyHit {
                family: RootCauseFamily::FrankensqliteStorage,
                score: if direct { 2 } else { 1 },
                evidence: ev,
                direct,
            });
        }
    }

    // frankensearch — only on a real error; lexical fail-open alone is NOT a fault.
    if s.frankensearch_error {
        hits.push(FamilyHit {
            family: RootCauseFamily::FrankensearchSearch,
            score: 2,
            evidence: vec![EvidenceRef::new("frankensearch.fusion_error", "search")],
            direct: true,
        });
    }

    // cass derived state — only on a concrete mismatch.
    if s.derived_truth_table_mismatch {
        hits.push(FamilyHit {
            family: RootCauseFamily::CassDerivedState,
            score: 2,
            evidence: vec![EvidenceRef::new(
                "cass.derived_asset.truth_table_mismatch",
                "doctor",
            )],
            direct: true,
        });
    }

    // asupersync runtime.
    if s.runtime_task_stall {
        hits.push(FamilyHit {
            family: RootCauseFamily::AsupersyncRuntime,
            score: 1,
            evidence: vec![EvidenceRef::new("asupersync.task_stall_ms", "status")],
            direct: false,
        });
    }

    // remote transport / auth.
    {
        let mut ev = Vec::new();
        let mut direct = false;
        if s.transport_auth_failure {
            ev.push(EvidenceRef::new("transport.auth_failure", "sources"));
            direct = true;
        }
        if let Some(code) = s.transport_ssh_exit_code {
            ev.push(
                EvidenceRef::new("transport.ssh_exit_code", "sources").with_detail(code.to_string()),
            );
        }
        if s.transport_connect_timeout {
            ev.push(EvidenceRef::new("transport.connect_timeout_ms", "sources"));
        }
        if !ev.is_empty() {
            hits.push(FamilyHit {
                family: RootCauseFamily::RemoteTransportAuth,
                score: if direct { 2 } else { 1 },
                evidence: ev,
                direct,
            });
        }
    }

    // semantic assets — requested-but-missing only.
    if s.semantic_requested_but_missing {
        hits.push(FamilyHit {
            family: RootCauseFamily::SemanticAssets,
            score: 1,
            evidence: vec![EvidenceRef::new("semantic.vector_index_built", "diag")
                .with_detail("false".to_string())],
            direct: false,
        });
    }

    // workspace provenance.
    if s.workspace_provenance_mismatch {
        hits.push(FamilyHit {
            family: RootCauseFamily::WorkspaceProvenance,
            score: 1,
            evidence: vec![EvidenceRef::new("config.data_dir", "status")],
            direct: false,
        });
    }

    // host disk pressure — only below threshold (abundant free space rules out).
    if let Some(pct) = s.host_disk_free_pct
        && pct < DISK_PRESSURE_PCT
    {
        hits.push(FamilyHit {
            family: RootCauseFamily::HostDiskPressure,
            score: 2,
            evidence: vec![
                EvidenceRef::new("host.disk_free_pct", "host").with_detail(format!("{pct:.1}")),
            ],
            direct: true,
        });
    }

    // host OOM / load.
    {
        let mut ev = Vec::new();
        let mut direct = false;
        if s.host_oom_kill {
            ev.push(EvidenceRef::new("host.oom_kill_count", "host").with_detail("1+".to_string()));
            direct = true;
        }
        if let Some(ratio) = s.host_load_ratio
            && ratio > LOAD_PRESSURE_RATIO
        {
            ev.push(
                EvidenceRef::new("host.load_avg_1m", "host").with_detail(format!("{ratio:.1}x")),
            );
        }
        if !ev.is_empty() {
            hits.push(FamilyHit {
                family: RootCauseFamily::HostOomLoad,
                score: if direct { 2 } else { 1 },
                evidence: ev,
                direct,
            });
        }
    }

    // old binary / contract skew.
    if s.binary_behind_contract {
        hits.push(FamilyHit {
            family: RootCauseFamily::OldBinarySkew,
            score: 2,
            evidence: vec![EvidenceRef::new("binary.contract_version", "api-version")],
            direct: true,
        });
    }

    hits
}

/// Project the most likely root-cause attribution from the evidence. Pure; no
/// I/O or mutation. With no signals, returns the explicit unattributed record.
///
/// Confidence:
/// - `Confirmed`: exactly one family implicated, by direct evidence.
/// - `Probable`: one family clearly dominates (strictly higher score).
/// - `Possible`: the top family ties with another (mixed evidence).
/// - `Unknown`: no signals.
pub fn project_root_cause(signals: &ProjectionSignals) -> RootCauseAttribution {
    let mut hits = collect_hits(signals);
    if hits.is_empty() {
        return RootCauseAttribution::unattributed(
            "no evidence signals pointed at any root-cause family".to_string(),
        );
    }

    // Highest score wins; ties broken by the stable family label for determinism.
    hits.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.family.as_str().cmp(b.family.as_str()))
    });

    let top_score = hits[0].score;
    let contenders = hits.iter().filter(|h| h.score == top_score).count();
    let only_one_family = hits.len() == 1;

    let confidence = if only_one_family && hits[0].direct {
        AttributionConfidence::Confirmed
    } else if contenders == 1 {
        AttributionConfidence::Probable
    } else {
        AttributionConfidence::Possible
    };

    let chosen = &hits[0];
    let competing: Vec<&str> = hits
        .iter()
        .skip(1)
        .filter(|h| h.score == top_score)
        .map(|h| h.family.as_str())
        .collect();
    let summary = if competing.is_empty() {
        format!(
            "evidence points at {}; confirm with the family's bounded probe",
            chosen.family.as_str()
        )
    } else {
        format!(
            "evidence favors {} but {} is not excluded; gather more before acting",
            chosen.family.as_str(),
            competing.join(", ")
        )
    };

    RootCauseAttribution::new(chosen.family, confidence, summary)
        .with_evidence(chosen.evidence.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::root_cause_taxonomy::FaultLocus;

    #[test]
    fn storage_dominated_corpus_attributes_frankensqlite() {
        // csd-like: storage-dominated.
        let s = ProjectionSignals {
            fsqlite_error_code: Some("SQLITE_BUSY".to_string()),
            open_read_failure: true,
            ..Default::default()
        };
        let a = project_root_cause(&s);
        assert_eq!(a.family, RootCauseFamily::FrankensqliteStorage);
        assert_eq!(a.locus, FaultLocus::Dependency);
        assert_eq!(a.confidence, AttributionConfidence::Confirmed);
        assert!(a.evidence_refs.iter().any(|e| e.kind == "fsqlite.error_code"));
        assert!(a.recommended_next_probe.is_some());
    }

    #[test]
    fn dependency_search_corpus_attributes_frankensearch() {
        // css-like: dependency/search-dominated (a real search-stack error).
        let s = ProjectionSignals {
            frankensearch_error: true,
            ..Default::default()
        };
        let a = project_root_cause(&s);
        assert_eq!(a.family, RootCauseFamily::FrankensearchSearch);
        assert_eq!(a.locus, FaultLocus::Dependency);
        assert_eq!(a.confidence, AttributionConfidence::Confirmed);
    }

    #[test]
    fn lexical_fail_open_alone_is_not_a_search_fault() {
        // Designed graceful degradation must NOT attribute a fault.
        let s = ProjectionSignals {
            lexical_fail_open: true,
            ..Default::default()
        };
        let a = project_root_cause(&s);
        assert_eq!(a.family, RootCauseFamily::Unknown);
        assert_eq!(a.confidence, AttributionConfidence::Unknown);
    }

    #[test]
    fn stale_derived_state_without_host_pressure_attributes_cass_not_host() {
        let s = ProjectionSignals {
            derived_truth_table_mismatch: true,
            // No disk/oom signals at all.
            ..Default::default()
        };
        let a = project_root_cause(&s);
        assert_eq!(a.family, RootCauseFamily::CassDerivedState);
        assert_eq!(a.locus, FaultLocus::Cass);
        assert_ne!(a.family, RootCauseFamily::HostDiskPressure);
        assert_ne!(a.family, RootCauseFamily::HostOomLoad);
    }

    #[test]
    fn auth_failure_attributes_remote_transport() {
        let s = ProjectionSignals {
            transport_auth_failure: true,
            transport_ssh_exit_code: Some(255),
            ..Default::default()
        };
        let a = project_root_cause(&s);
        assert_eq!(a.family, RootCauseFamily::RemoteTransportAuth);
        assert_eq!(a.confidence, AttributionConfidence::Confirmed);
        assert!(a.evidence_refs.iter().any(|e| e.kind == "transport.auth_failure"));
    }

    #[test]
    fn disk_pressure_only_below_threshold() {
        // Abundant free space rules it out.
        let plenty = ProjectionSignals {
            host_disk_free_pct: Some(60.0),
            ..Default::default()
        };
        assert_eq!(project_root_cause(&plenty).family, RootCauseFamily::Unknown);
        // Below threshold attributes host disk pressure.
        let tight = ProjectionSignals {
            host_disk_free_pct: Some(2.0),
            ..Default::default()
        };
        let a = project_root_cause(&tight);
        assert_eq!(a.family, RootCauseFamily::HostDiskPressure);
        assert_eq!(a.locus, FaultLocus::Host);
    }

    #[test]
    fn mixed_evidence_picks_dominant_with_possible_confidence() {
        // Two direct families implicated => top is Possible, competitor named.
        let s = ProjectionSignals {
            open_read_failure: true,        // FrankensqliteStorage (direct, score 2)
            derived_truth_table_mismatch: true, // CassDerivedState (direct, score 2)
            ..Default::default()
        };
        let a = project_root_cause(&s);
        assert_eq!(a.confidence, AttributionConfidence::Possible);
        // Deterministic tie-break by taxonomy rank; both must be representable.
        assert!(matches!(
            a.family,
            RootCauseFamily::FrankensqliteStorage | RootCauseFamily::CassDerivedState
        ));
        assert!(a.summary.contains("not excluded"));
    }

    #[test]
    fn dominant_direct_over_circumstantial_is_probable() {
        // One direct (score 2) + one circumstantial (score 1) => Probable, no tie.
        let s = ProjectionSignals {
            binary_behind_contract: true, // OldBinarySkew, direct, score 2
            workspace_provenance_mismatch: true, // WorkspaceProvenance, score 1
            ..Default::default()
        };
        let a = project_root_cause(&s);
        assert_eq!(a.family, RootCauseFamily::OldBinarySkew);
        assert_eq!(a.confidence, AttributionConfidence::Probable);
    }

    #[test]
    fn no_signals_is_explicit_unknown() {
        let a = project_root_cause(&ProjectionSignals::default());
        assert_eq!(a.family, RootCauseFamily::Unknown);
        assert_eq!(a.confidence, AttributionConfidence::Unknown);
        assert!(a.evidence_refs.is_empty());
    }

    #[test]
    fn attribution_json_contract_round_trips() {
        let s = ProjectionSignals {
            host_oom_kill: true,
            ..Default::default()
        };
        let a = project_root_cause(&s);
        assert_eq!(a.family, RootCauseFamily::HostOomLoad);
        let value = serde_json::to_value(&a).unwrap();
        assert_eq!(value["family"], "host-oom-load");
        assert_eq!(value["confidence"], "confirmed");
        let back: RootCauseAttribution = serde_json::from_value(value).unwrap();
        assert_eq!(back, a);
    }
}
