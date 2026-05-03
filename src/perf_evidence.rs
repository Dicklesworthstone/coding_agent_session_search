//! Stable evidence records for performance experiments and control-plane decisions.
//!
//! These types are intentionally data-only. Runtime controllers can consume ledgers
//! from benchmarks, replay harnesses, or production diagnostics without depending on
//! benchmark-specific structs or ad hoc JSON.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

pub const PERF_EVIDENCE_SCHEMA_VERSION: &str = "1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PerfWorkloadKind {
    Search,
    WatchOnce,
    FullRebuild,
    SemanticBackfill,
    SourceSync,
    DoctorRepair,
    CacheWarm,
    #[default]
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PerfPhaseKind {
    Queueing,
    Service,
    Io,
    Synchronization,
    Retries,
    Hydration,
    Output,
    #[default]
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PerfProofStatus {
    #[default]
    NotMeasured,
    Passed,
    Failed,
    Inconclusive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PerfCountPrecision {
    #[default]
    Exact,
    LowerBound,
    Estimated,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PerfEvidenceLedger {
    pub schema_version: String,
    pub run_id: String,
    pub recorded_at_ms: i64,
    pub workload: PerfWorkload,
    #[serde(default)]
    pub machine: PerfMachineProfile,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub phases: Vec<PerfPhaseTiming>,
    #[serde(default)]
    pub resources: PerfResourceSnapshot,
    #[serde(default)]
    pub cache: Option<PerfCacheSnapshot>,
    #[serde(default)]
    pub search: Option<PerfSearchSnapshot>,
    #[serde(default)]
    pub rebuild: Option<PerfRebuildSnapshot>,
    #[serde(default)]
    pub proof: PerfProofSummary,
    #[serde(default)]
    pub artifacts: Vec<PerfArtifactRef>,
}

impl PerfEvidenceLedger {
    pub fn new(run_id: impl Into<String>, workload: PerfWorkload, recorded_at_ms: i64) -> Self {
        Self {
            schema_version: PERF_EVIDENCE_SCHEMA_VERSION.to_string(),
            run_id: run_id.into(),
            recorded_at_ms,
            workload,
            machine: PerfMachineProfile::default(),
            env: BTreeMap::new(),
            phases: Vec::new(),
            resources: PerfResourceSnapshot::default(),
            cache: None,
            search: None,
            rebuild: None,
            proof: PerfProofSummary::default(),
            artifacts: Vec::new(),
        }
    }

    pub fn validate(&self) -> Result<(), PerfEvidenceValidationError> {
        if self.schema_version != PERF_EVIDENCE_SCHEMA_VERSION {
            return Err(PerfEvidenceValidationError::UnsupportedSchemaVersion {
                expected: PERF_EVIDENCE_SCHEMA_VERSION,
                actual: self.schema_version.clone(),
            });
        }

        if self.run_id.trim().is_empty() {
            return Err(PerfEvidenceValidationError::EmptyRunId);
        }

        if self.recorded_at_ms < 0 {
            return Err(PerfEvidenceValidationError::NegativeRecordedAtMs {
                recorded_at_ms: self.recorded_at_ms,
            });
        }

        if self.workload.name.trim().is_empty() {
            return Err(PerfEvidenceValidationError::EmptyWorkloadName);
        }

        if let Some(search) = &self.search {
            if search.query_hash.trim().is_empty() {
                return Err(PerfEvidenceValidationError::EmptySearchQueryHash);
            }

            if search.requested_mode.trim().is_empty() {
                return Err(PerfEvidenceValidationError::EmptySearchRequestedMode);
            }

            if search.realized_mode.trim().is_empty() {
                return Err(PerfEvidenceValidationError::EmptySearchRealizedMode);
            }
        }

        if let Some(rebuild) = &self.rebuild {
            if rebuild.execution_mode.trim().is_empty() {
                return Err(PerfEvidenceValidationError::EmptyRebuildExecutionMode);
            }

            if rebuild.workers == 0 {
                return Err(PerfEvidenceValidationError::ZeroRebuildWorkers);
            }
        }

        for (index, phase) in self.phases.iter().enumerate() {
            if phase.name.trim().is_empty() {
                return Err(PerfEvidenceValidationError::EmptyPhaseName { index });
            }

            if quantile_order_violated(phase.p50_ms, phase.p95_ms)
                || quantile_order_violated(phase.p95_ms, phase.p99_ms)
                || quantile_order_violated(phase.p50_ms, phase.p99_ms)
            {
                return Err(PerfEvidenceValidationError::PhaseQuantilesOutOfOrder { index });
            }
        }

        for (index, artifact) in self.artifacts.iter().enumerate() {
            if artifact.label.trim().is_empty() {
                return Err(PerfEvidenceValidationError::EmptyArtifactLabel { index });
            }

            if artifact.path.trim().is_empty() {
                return Err(PerfEvidenceValidationError::EmptyArtifactPath { index });
            }

            if artifact.kind.trim().is_empty() {
                return Err(PerfEvidenceValidationError::EmptyArtifactKind { index });
            }
        }

        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PerfWorkload {
    pub kind: PerfWorkloadKind,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub command_args: Vec<String>,
    #[serde(default)]
    pub input_count: Option<PerfCount>,
}

impl PerfWorkload {
    pub fn new(kind: PerfWorkloadKind, name: impl Into<String>) -> Self {
        Self {
            kind,
            name: name.into(),
            description: None,
            command_args: Vec::new(),
            input_count: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PerfCount {
    pub value: u64,
    #[serde(default)]
    pub precision: PerfCountPrecision,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PerfMachineProfile {
    #[serde(default)]
    pub logical_cpus: Option<u32>,
    #[serde(default)]
    pub reserved_cores: Option<u32>,
    #[serde(default)]
    pub available_memory_bytes: Option<u64>,
    #[serde(default)]
    pub topology_class: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PerfPhaseTiming {
    pub name: String,
    pub kind: PerfPhaseKind,
    pub elapsed_ms: u64,
    #[serde(default)]
    pub p50_ms: Option<u64>,
    #[serde(default)]
    pub p95_ms: Option<u64>,
    #[serde(default)]
    pub p99_ms: Option<u64>,
    #[serde(default)]
    pub samples: Option<PerfCount>,
}

impl PerfPhaseTiming {
    pub fn new(name: impl Into<String>, kind: PerfPhaseKind, elapsed_ms: u64) -> Self {
        Self {
            name: name.into(),
            kind,
            elapsed_ms,
            p50_ms: None,
            p95_ms: None,
            p99_ms: None,
            samples: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PerfResourceSnapshot {
    #[serde(default)]
    pub peak_rss_bytes: Option<u64>,
    #[serde(default)]
    pub avg_cpu_utilization_pct_x100: Option<u32>,
    #[serde(default)]
    pub max_inflight_bytes: Option<u64>,
    #[serde(default)]
    pub disk_read_bytes: Option<u64>,
    #[serde(default)]
    pub disk_write_bytes: Option<u64>,
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PerfCacheSnapshot {
    #[serde(default)]
    pub result_cache_hits: u64,
    #[serde(default)]
    pub result_cache_misses: u64,
    #[serde(default)]
    pub eviction_count: u64,
    #[serde(default)]
    pub approx_bytes: Option<u64>,
    #[serde(default)]
    pub byte_cap: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PerfSearchSnapshot {
    pub query_hash: String,
    pub limit: u32,
    #[serde(default)]
    pub matched_count: Option<PerfCount>,
    pub returned_hits: u32,
    pub requested_mode: String,
    pub realized_mode: String,
    #[serde(default)]
    pub fallback_tier: Option<String>,
    #[serde(default)]
    pub timed_out: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PerfRebuildSnapshot {
    pub execution_mode: String,
    pub workers: u32,
    #[serde(default)]
    pub shard_count: Option<u32>,
    #[serde(default)]
    pub queued_items: Option<PerfCount>,
    #[serde(default)]
    pub indexed_items: Option<PerfCount>,
    #[serde(default)]
    pub checkpoint_count: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PerfProofSummary {
    #[serde(default)]
    pub status: PerfProofStatus,
    #[serde(default)]
    pub baseline_artifact: Option<String>,
    #[serde(default)]
    pub comparison_artifact: Option<String>,
    #[serde(default)]
    pub p99_regression_basis_points: Option<i64>,
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PerfArtifactRef {
    pub label: String,
    pub path: String,
    pub kind: String,
    #[serde(default)]
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PerfEvidenceValidationError {
    UnsupportedSchemaVersion {
        expected: &'static str,
        actual: String,
    },
    EmptyRunId,
    NegativeRecordedAtMs {
        recorded_at_ms: i64,
    },
    EmptyWorkloadName,
    EmptySearchQueryHash,
    EmptySearchRequestedMode,
    EmptySearchRealizedMode,
    EmptyRebuildExecutionMode,
    ZeroRebuildWorkers,
    EmptyPhaseName {
        index: usize,
    },
    PhaseQuantilesOutOfOrder {
        index: usize,
    },
    EmptyArtifactLabel {
        index: usize,
    },
    EmptyArtifactPath {
        index: usize,
    },
    EmptyArtifactKind {
        index: usize,
    },
}

impl fmt::Display for PerfEvidenceValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedSchemaVersion { expected, actual } => {
                write!(
                    f,
                    "unsupported perf evidence schema version {actual:?}; expected {expected:?}"
                )
            }
            Self::EmptyRunId => write!(f, "perf evidence run_id cannot be empty"),
            Self::NegativeRecordedAtMs { recorded_at_ms } => {
                write!(
                    f,
                    "perf evidence recorded_at_ms cannot be negative: {recorded_at_ms}"
                )
            }
            Self::EmptyWorkloadName => write!(f, "perf evidence workload.name cannot be empty"),
            Self::EmptySearchQueryHash => {
                write!(f, "perf evidence search.query_hash cannot be empty")
            }
            Self::EmptySearchRequestedMode => {
                write!(f, "perf evidence search.requested_mode cannot be empty")
            }
            Self::EmptySearchRealizedMode => {
                write!(f, "perf evidence search.realized_mode cannot be empty")
            }
            Self::EmptyRebuildExecutionMode => {
                write!(f, "perf evidence rebuild.execution_mode cannot be empty")
            }
            Self::ZeroRebuildWorkers => {
                write!(f, "perf evidence rebuild.workers must be greater than zero")
            }
            Self::EmptyPhaseName { index } => {
                write!(f, "perf evidence phase at index {index} has an empty name")
            }
            Self::PhaseQuantilesOutOfOrder { index } => {
                write!(
                    f,
                    "perf evidence phase at index {index} has out-of-order quantiles"
                )
            }
            Self::EmptyArtifactLabel { index } => {
                write!(
                    f,
                    "perf evidence artifact at index {index} has an empty label"
                )
            }
            Self::EmptyArtifactPath { index } => {
                write!(
                    f,
                    "perf evidence artifact at index {index} has an empty path"
                )
            }
            Self::EmptyArtifactKind { index } => {
                write!(
                    f,
                    "perf evidence artifact at index {index} has an empty kind"
                )
            }
        }
    }
}

impl Error for PerfEvidenceValidationError {}

fn quantile_order_violated(lower: Option<u64>, upper: Option<u64>) -> bool {
    matches!((lower, upper), (Some(lower), Some(upper)) if lower > upper)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn representative_ledger() -> PerfEvidenceLedger {
        let mut ledger = PerfEvidenceLedger::new(
            "run-search-p99-001",
            PerfWorkload {
                kind: PerfWorkloadKind::Search,
                name: "hybrid-search-tail-latency".to_string(),
                description: Some("Representative hybrid search p99 probe".to_string()),
                command_args: vec![
                    "cass".to_string(),
                    "search".to_string(),
                    "wal conflict".to_string(),
                    "--json".to_string(),
                ],
                input_count: Some(PerfCount {
                    value: 1_000_000,
                    precision: PerfCountPrecision::LowerBound,
                }),
            },
            1_779_999_999_000,
        );

        ledger.machine = PerfMachineProfile {
            logical_cpus: Some(64),
            reserved_cores: Some(8),
            available_memory_bytes: Some(256 * 1024 * 1024 * 1024),
            topology_class: Some("single_host_many_core".to_string()),
        };
        ledger.env = BTreeMap::from([("CASS_SEARCH_MODE".to_string(), "hybrid".to_string())]);
        ledger.phases = vec![
            phase("admission", PerfPhaseKind::Queueing, 2, 1, 2, 3),
            phase("bm25", PerfPhaseKind::Service, 18, 12, 16, 18),
            phase("semantic", PerfPhaseKind::Io, 35, 22, 31, 35),
            phase("merge", PerfPhaseKind::Synchronization, 7, 4, 6, 7),
            phase("retry-budget", PerfPhaseKind::Retries, 1, 0, 1, 1),
            phase("hydrate", PerfPhaseKind::Hydration, 9, 5, 8, 9),
            phase("emit-json", PerfPhaseKind::Output, 3, 2, 3, 3),
        ];
        ledger.resources = PerfResourceSnapshot {
            peak_rss_bytes: Some(2_147_483_648),
            avg_cpu_utilization_pct_x100: Some(5_250),
            max_inflight_bytes: Some(268_435_456),
            disk_read_bytes: Some(41_943_040),
            disk_write_bytes: Some(0),
            notes: vec!["warm lexical index".to_string()],
        };
        ledger.cache = Some(PerfCacheSnapshot {
            result_cache_hits: 42,
            result_cache_misses: 3,
            eviction_count: 1,
            approx_bytes: Some(64 * 1024 * 1024),
            byte_cap: Some(512 * 1024 * 1024),
        });
        ledger.search = Some(PerfSearchSnapshot {
            query_hash: "blake3:abc123".to_string(),
            limit: 20,
            matched_count: Some(PerfCount {
                value: 482,
                precision: PerfCountPrecision::Exact,
            }),
            returned_hits: 20,
            requested_mode: "hybrid".to_string(),
            realized_mode: "hybrid".to_string(),
            fallback_tier: None,
            timed_out: false,
        });
        ledger.proof = PerfProofSummary {
            status: PerfProofStatus::Passed,
            baseline_artifact: Some("tests/artifacts/perf/baseline.json".to_string()),
            comparison_artifact: Some("tests/artifacts/perf/candidate.json".to_string()),
            p99_regression_basis_points: Some(-250),
            notes: vec!["p99 improved by 2.5%".to_string()],
        };
        ledger.artifacts = vec![PerfArtifactRef {
            label: "candidate-ledger".to_string(),
            path: "tests/artifacts/perf/candidate.json".to_string(),
            kind: "json".to_string(),
            sha256: Some("0123456789abcdef".to_string()),
        }];

        ledger
    }

    fn phase(
        name: &str,
        kind: PerfPhaseKind,
        elapsed_ms: u64,
        p50_ms: u64,
        p95_ms: u64,
        p99_ms: u64,
    ) -> PerfPhaseTiming {
        PerfPhaseTiming {
            name: name.to_string(),
            kind,
            elapsed_ms,
            p50_ms: Some(p50_ms),
            p95_ms: Some(p95_ms),
            p99_ms: Some(p99_ms),
            samples: Some(PerfCount {
                value: 100,
                precision: PerfCountPrecision::Exact,
            }),
        }
    }

    #[test]
    fn representative_ledger_validates_and_round_trips_json() {
        let ledger = representative_ledger();

        ledger.validate().unwrap();

        let encoded = serde_json::to_value(&ledger).unwrap();
        assert_eq!(encoded["schema_version"], PERF_EVIDENCE_SCHEMA_VERSION);
        assert_eq!(encoded["workload"]["kind"], "search");
        assert_eq!(encoded["phases"][0]["kind"], "queueing");
        assert_eq!(
            encoded["workload"]["input_count"]["precision"],
            "lower_bound"
        );

        let decoded: PerfEvidenceLedger = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, ledger);
    }

    #[test]
    fn future_top_level_fields_are_ignored_by_old_readers() {
        let encoded = json!({
            "schema_version": PERF_EVIDENCE_SCHEMA_VERSION,
            "run_id": "run-with-future",
            "recorded_at_ms": 1,
            "workload": {
                "kind": "search",
                "name": "future-field-probe"
            },
            "future_controller_hint": {
                "new_field": true
            }
        });

        let decoded: PerfEvidenceLedger = serde_json::from_value(encoded).unwrap();

        assert_eq!(decoded.run_id, "run-with-future");
        decoded.validate().unwrap();
    }

    #[test]
    fn validation_rejects_missing_identity_fields() {
        let mut ledger = representative_ledger();
        ledger.run_id = "  ".to_string();

        assert_eq!(
            ledger.validate(),
            Err(PerfEvidenceValidationError::EmptyRunId)
        );

        ledger = representative_ledger();
        ledger.workload.name.clear();
        assert_eq!(
            ledger.validate(),
            Err(PerfEvidenceValidationError::EmptyWorkloadName)
        );
    }

    #[test]
    fn validation_rejects_unsupported_schema_and_negative_time() {
        let mut ledger = representative_ledger();
        ledger.schema_version = "2".to_string();

        assert_eq!(
            ledger.validate(),
            Err(PerfEvidenceValidationError::UnsupportedSchemaVersion {
                expected: PERF_EVIDENCE_SCHEMA_VERSION,
                actual: "2".to_string(),
            })
        );

        ledger = representative_ledger();
        ledger.recorded_at_ms = -1;
        assert_eq!(
            ledger.validate(),
            Err(PerfEvidenceValidationError::NegativeRecordedAtMs { recorded_at_ms: -1 })
        );
    }

    #[test]
    fn validation_rejects_bad_phase_and_artifact_entries() {
        let mut ledger = representative_ledger();
        ledger.phases[0].name.clear();

        assert_eq!(
            ledger.validate(),
            Err(PerfEvidenceValidationError::EmptyPhaseName { index: 0 })
        );

        ledger = representative_ledger();
        ledger.phases[0].p50_ms = Some(10);
        ledger.phases[0].p95_ms = Some(5);
        assert_eq!(
            ledger.validate(),
            Err(PerfEvidenceValidationError::PhaseQuantilesOutOfOrder { index: 0 })
        );

        ledger = representative_ledger();
        ledger.artifacts[0].label.clear();
        assert_eq!(
            ledger.validate(),
            Err(PerfEvidenceValidationError::EmptyArtifactLabel { index: 0 })
        );

        ledger = representative_ledger();
        ledger.artifacts[0].path = " ".to_string();
        assert_eq!(
            ledger.validate(),
            Err(PerfEvidenceValidationError::EmptyArtifactPath { index: 0 })
        );

        ledger = representative_ledger();
        ledger.artifacts[0].kind.clear();
        assert_eq!(
            ledger.validate(),
            Err(PerfEvidenceValidationError::EmptyArtifactKind { index: 0 })
        );
    }

    #[test]
    fn validation_rejects_empty_nested_snapshot_fields() {
        let mut ledger = representative_ledger();
        ledger.search.as_mut().unwrap().query_hash.clear();

        assert_eq!(
            ledger.validate(),
            Err(PerfEvidenceValidationError::EmptySearchQueryHash)
        );

        ledger = representative_ledger();
        ledger.search.as_mut().unwrap().requested_mode = " ".to_string();
        assert_eq!(
            ledger.validate(),
            Err(PerfEvidenceValidationError::EmptySearchRequestedMode)
        );

        ledger = representative_ledger();
        ledger.search.as_mut().unwrap().realized_mode.clear();
        assert_eq!(
            ledger.validate(),
            Err(PerfEvidenceValidationError::EmptySearchRealizedMode)
        );

        ledger = representative_ledger();
        ledger.rebuild = Some(PerfRebuildSnapshot {
            execution_mode: " ".to_string(),
            workers: 1,
            shard_count: None,
            queued_items: None,
            indexed_items: None,
            checkpoint_count: None,
        });
        assert_eq!(
            ledger.validate(),
            Err(PerfEvidenceValidationError::EmptyRebuildExecutionMode)
        );

        ledger = representative_ledger();
        ledger.rebuild = Some(PerfRebuildSnapshot {
            execution_mode: "flat_combining".to_string(),
            workers: 0,
            shard_count: None,
            queued_items: None,
            indexed_items: None,
            checkpoint_count: None,
        });
        assert_eq!(
            ledger.validate(),
            Err(PerfEvidenceValidationError::ZeroRebuildWorkers)
        );
    }

    #[test]
    fn representative_ledger_covers_tail_decomposition_phase_kinds() {
        let ledger = representative_ledger();
        let phase_kinds = ledger
            .phases
            .iter()
            .map(|phase| phase.kind)
            .collect::<Vec<_>>();

        for required in [
            PerfPhaseKind::Queueing,
            PerfPhaseKind::Service,
            PerfPhaseKind::Io,
            PerfPhaseKind::Synchronization,
            PerfPhaseKind::Retries,
            PerfPhaseKind::Hydration,
            PerfPhaseKind::Output,
        ] {
            assert!(
                phase_kinds.contains(&required),
                "missing required phase kind {required:?}"
            );
        }
    }

    #[test]
    fn enum_serialization_is_stable_snake_case() {
        let encoded = serde_json::to_value(PerfEvidenceLedger {
            schema_version: PERF_EVIDENCE_SCHEMA_VERSION.to_string(),
            run_id: "enum-stability".to_string(),
            recorded_at_ms: 1,
            workload: PerfWorkload::new(PerfWorkloadKind::CacheWarm, "cache-warm"),
            machine: PerfMachineProfile::default(),
            env: BTreeMap::new(),
            phases: vec![PerfPhaseTiming::new("output", PerfPhaseKind::Output, 1)],
            resources: PerfResourceSnapshot::default(),
            cache: None,
            search: None,
            rebuild: None,
            proof: PerfProofSummary {
                status: PerfProofStatus::Inconclusive,
                ..PerfProofSummary::default()
            },
            artifacts: Vec::new(),
        })
        .unwrap();

        assert_eq!(encoded["workload"]["kind"], "cache_warm");
        assert_eq!(encoded["phases"][0]["kind"], "output");
        assert_eq!(encoded["proof"]["status"], "inconclusive");

        let precision: Value = serde_json::to_value(PerfCountPrecision::Unavailable).unwrap();
        assert_eq!(precision, "unavailable");
    }
}
