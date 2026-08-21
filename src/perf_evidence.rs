//! Stable evidence records for performance experiments and control-plane decisions.
//!
//! These types are intentionally data-only. Runtime controllers can consume ledgers
//! from benchmarks, replay harnesses, or production diagnostics without depending on
//! benchmark-specific structs or ad hoc JSON.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

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

#[derive(Debug)]
pub enum PerfEvidenceIoError {
    Io(io::Error),
    Json(serde_json::Error),
    Validation(PerfEvidenceValidationError),
}

impl fmt::Display for PerfEvidenceIoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "perf evidence I/O failed: {err}"),
            Self::Json(err) => write!(f, "perf evidence JSON failed: {err}"),
            Self::Validation(err) => write!(f, "perf evidence validation failed: {err}"),
        }
    }
}

impl Error for PerfEvidenceIoError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::Json(err) => Some(err),
            Self::Validation(err) => Some(err),
        }
    }
}

impl From<io::Error> for PerfEvidenceIoError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<serde_json::Error> for PerfEvidenceIoError {
    fn from(err: serde_json::Error) -> Self {
        Self::Json(err)
    }
}

impl From<PerfEvidenceValidationError> for PerfEvidenceIoError {
    fn from(err: PerfEvidenceValidationError) -> Self {
        Self::Validation(err)
    }
}

pub fn read_perf_evidence_ledger(
    path: impl AsRef<Path>,
) -> Result<PerfEvidenceLedger, PerfEvidenceIoError> {
    let bytes = fs::read(path.as_ref())?;
    let ledger: PerfEvidenceLedger = serde_json::from_slice(&bytes)?;
    ledger.validate()?;
    Ok(ledger)
}

pub fn write_perf_evidence_ledger(
    ledger: &PerfEvidenceLedger,
    path: impl AsRef<Path>,
) -> Result<PerfArtifactRef, PerfEvidenceIoError> {
    ledger.validate()?;
    let path = path.as_ref();
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(ledger)?;
    fs::write(path, &bytes)?;
    Ok(PerfArtifactRef {
        label: "perf-evidence-ledger".to_string(),
        path: path.display().to_string(),
        kind: "json".to_string(),
        sha256: Some(sha256_hex(&bytes)),
    })
}

#[derive(Debug)]
pub enum PerfEvidenceRecorderError {
    ActivePhaseAlreadyRunning { active_phase: String },
    NoActivePhase,
    Validation(PerfEvidenceValidationError),
}

impl fmt::Display for PerfEvidenceRecorderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ActivePhaseAlreadyRunning { active_phase } => {
                write!(f, "perf evidence phase {active_phase:?} is already active")
            }
            Self::NoActivePhase => write!(f, "no perf evidence phase is active"),
            Self::Validation(err) => {
                write!(f, "perf evidence recorder produced invalid data: {err}")
            }
        }
    }
}

impl Error for PerfEvidenceRecorderError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Validation(err) => Some(err),
            _ => None,
        }
    }
}

impl From<PerfEvidenceValidationError> for PerfEvidenceRecorderError {
    fn from(err: PerfEvidenceValidationError) -> Self {
        Self::Validation(err)
    }
}

#[derive(Debug)]
struct ActivePerfPhase {
    name: String,
    kind: PerfPhaseKind,
    started_at: Instant,
}

/// Incrementally records a [`PerfEvidenceLedger`] without coupling callers to
/// benchmark-only structs.
///
/// The recorder is intentionally small: callers provide workload identity and
/// optional snapshots, then append explicit phases or time `begin_phase` /
/// `finish_phase` spans. It never reads global process configuration.
#[derive(Debug)]
pub struct PerfEvidenceRecorder {
    ledger: PerfEvidenceLedger,
    active_phase: Option<ActivePerfPhase>,
}

impl PerfEvidenceRecorder {
    pub fn new(run_id: impl Into<String>, workload: PerfWorkload, recorded_at_ms: i64) -> Self {
        Self {
            ledger: PerfEvidenceLedger::new(run_id, workload, recorded_at_ms),
            active_phase: None,
        }
    }

    pub fn start(run_id: impl Into<String>, workload: PerfWorkload) -> Self {
        Self::new(run_id, workload, now_unix_ms())
    }

    pub fn ledger(&self) -> &PerfEvidenceLedger {
        &self.ledger
    }

    pub fn machine(&mut self, machine: PerfMachineProfile) -> &mut Self {
        self.ledger.machine = machine;
        self
    }

    pub fn resource_snapshot(&mut self, resources: PerfResourceSnapshot) -> &mut Self {
        self.ledger.resources = resources;
        self
    }

    pub fn cache_snapshot(&mut self, cache: PerfCacheSnapshot) -> &mut Self {
        self.ledger.cache = Some(cache);
        self
    }

    pub fn search_snapshot(&mut self, search: PerfSearchSnapshot) -> &mut Self {
        self.ledger.search = Some(search);
        self
    }

    pub fn rebuild_snapshot(&mut self, rebuild: PerfRebuildSnapshot) -> &mut Self {
        self.ledger.rebuild = Some(rebuild);
        self
    }

    pub fn proof_summary(&mut self, proof: PerfProofSummary) -> &mut Self {
        self.ledger.proof = proof;
        self
    }

    pub fn env_kv(&mut self, key: impl Into<String>, value: impl Into<String>) -> &mut Self {
        self.ledger.env.insert(key.into(), value.into());
        self
    }

    pub fn artifact(&mut self, artifact: PerfArtifactRef) -> &mut Self {
        self.ledger.artifacts.push(artifact);
        self
    }

    pub fn record_phase(
        &mut self,
        phase: PerfPhaseTiming,
    ) -> Result<&mut Self, PerfEvidenceRecorderError> {
        validate_phase(&phase, self.ledger.phases.len())?;
        self.ledger.phases.push(phase);
        Ok(self)
    }

    pub fn begin_phase(
        &mut self,
        name: impl Into<String>,
        kind: PerfPhaseKind,
    ) -> Result<&mut Self, PerfEvidenceRecorderError> {
        if let Some(active) = &self.active_phase {
            return Err(PerfEvidenceRecorderError::ActivePhaseAlreadyRunning {
                active_phase: active.name.clone(),
            });
        }
        self.active_phase = Some(ActivePerfPhase {
            name: name.into(),
            kind,
            started_at: Instant::now(),
        });
        Ok(self)
    }

    pub fn finish_phase(&mut self) -> Result<&mut Self, PerfEvidenceRecorderError> {
        let Some(active) = self.active_phase.take() else {
            return Err(PerfEvidenceRecorderError::NoActivePhase);
        };
        let elapsed_ms = active
            .started_at
            .elapsed()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;
        self.record_phase(PerfPhaseTiming::new(active.name, active.kind, elapsed_ms))
    }

    pub fn finish(mut self) -> Result<PerfEvidenceLedger, PerfEvidenceRecorderError> {
        if self.active_phase.is_some() {
            self.finish_phase()?;
        }
        self.ledger.validate()?;
        Ok(self.ledger)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PerfReplayVerdict {
    Clean,
    Warning,
    Failure,
}

impl PerfReplayVerdict {
    pub fn should_fail_build(self) -> bool {
        matches!(self, Self::Failure)
    }

    fn max(self, other: Self) -> Self {
        match (self, other) {
            (Self::Failure, _) | (_, Self::Failure) => Self::Failure,
            (Self::Warning, _) | (_, Self::Warning) => Self::Warning,
            _ => Self::Clean,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PerfReplayMetric {
    Validation,
    MeasurementCoverage,
    ProofStatus,
    ProofP99Regression,
    ComposedP99,
    TotalElapsed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PerfReplayFinding {
    pub verdict: PerfReplayVerdict,
    pub metric: PerfReplayMetric,
    pub message: String,
    #[serde(default)]
    pub baseline_value: Option<i64>,
    #[serde(default)]
    pub current_value: Option<i64>,
    #[serde(default)]
    pub delta_basis_points: Option<i64>,
    #[serde(default)]
    pub threshold_basis_points: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PerfReplayLogEvent {
    pub level: String,
    pub message: String,
    #[serde(default)]
    pub artifact_path: Option<String>,
    pub run_id: String,
    #[serde(default)]
    pub command_args: Vec<String>,
    #[serde(default)]
    pub failure_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PerfReplayReport {
    pub current_run_id: String,
    #[serde(default)]
    pub baseline_run_id: Option<String>,
    pub verdict: PerfReplayVerdict,
    #[serde(default)]
    pub findings: Vec<PerfReplayFinding>,
    #[serde(default)]
    pub logs: Vec<PerfReplayLogEvent>,
}

impl PerfReplayReport {
    pub fn should_fail_build(&self) -> bool {
        self.verdict.should_fail_build()
    }

    fn new(current: &PerfEvidenceLedger, baseline: Option<&PerfEvidenceLedger>) -> Self {
        Self {
            current_run_id: current.run_id.clone(),
            baseline_run_id: baseline.map(|ledger| ledger.run_id.clone()),
            verdict: PerfReplayVerdict::Clean,
            findings: Vec::new(),
            logs: Vec::new(),
        }
    }

    fn add_finding(&mut self, finding: PerfReplayFinding) {
        self.verdict = self.verdict.max(finding.verdict);
        self.findings.push(finding);
    }

    fn log(
        &mut self,
        level: &str,
        message: &str,
        current: &PerfEvidenceLedger,
        artifact_path: Option<&Path>,
        failure_reason: Option<String>,
    ) {
        self.logs.push(PerfReplayLogEvent {
            level: level.to_string(),
            message: message.to_string(),
            artifact_path: artifact_path.map(|path| path.display().to_string()),
            run_id: current.run_id.clone(),
            command_args: current.workload.command_args.clone(),
            failure_reason,
        });
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PerfReplayThresholds {
    pub warning_p99_regression_basis_points: i64,
    pub failure_p99_regression_basis_points: i64,
    pub warning_elapsed_regression_basis_points: i64,
    pub failure_elapsed_regression_basis_points: i64,
}

impl PerfReplayThresholds {
    pub fn defaults() -> Self {
        Self {
            warning_p99_regression_basis_points: 1_000,
            failure_p99_regression_basis_points: 2_500,
            warning_elapsed_regression_basis_points: 1_500,
            failure_elapsed_regression_basis_points: 3_000,
        }
    }

    pub fn try_new(
        warning_p99_regression_basis_points: i64,
        failure_p99_regression_basis_points: i64,
        warning_elapsed_regression_basis_points: i64,
        failure_elapsed_regression_basis_points: i64,
    ) -> Result<Self, &'static str> {
        validate_threshold_pair(
            warning_p99_regression_basis_points,
            failure_p99_regression_basis_points,
            "p99",
        )?;
        validate_threshold_pair(
            warning_elapsed_regression_basis_points,
            failure_elapsed_regression_basis_points,
            "elapsed",
        )?;
        Ok(Self {
            warning_p99_regression_basis_points,
            failure_p99_regression_basis_points,
            warning_elapsed_regression_basis_points,
            failure_elapsed_regression_basis_points,
        })
    }
}

impl Default for PerfReplayThresholds {
    fn default() -> Self {
        Self::defaults()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerfReplayGate {
    thresholds: PerfReplayThresholds,
}

impl PerfReplayGate {
    pub fn new(thresholds: PerfReplayThresholds) -> Self {
        Self { thresholds }
    }

    pub fn replay(
        &self,
        current: &PerfEvidenceLedger,
        baseline: Option<&PerfEvidenceLedger>,
    ) -> PerfReplayReport {
        self.replay_with_artifact(current, baseline, None)
    }

    pub fn replay_with_artifact(
        &self,
        current: &PerfEvidenceLedger,
        baseline: Option<&PerfEvidenceLedger>,
        current_artifact_path: Option<&Path>,
    ) -> PerfReplayReport {
        let mut report = PerfReplayReport::new(current, baseline);
        report.log(
            "info",
            "perf evidence replay started",
            current,
            current_artifact_path,
            None,
        );

        if let Err(err) = current.validate() {
            let failure_reason = err.to_string();
            report.add_finding(PerfReplayFinding {
                verdict: PerfReplayVerdict::Failure,
                metric: PerfReplayMetric::Validation,
                message: "current perf evidence ledger failed validation".to_string(),
                baseline_value: None,
                current_value: None,
                delta_basis_points: None,
                threshold_basis_points: None,
            });
            report.log(
                "error",
                "perf evidence replay failed",
                current,
                current_artifact_path,
                Some(failure_reason),
            );
            return report;
        }

        if let Some(baseline) = baseline
            && let Err(err) = baseline.validate()
        {
            let failure_reason = err.to_string();
            report.add_finding(PerfReplayFinding {
                verdict: PerfReplayVerdict::Failure,
                metric: PerfReplayMetric::Validation,
                message: "baseline perf evidence ledger failed validation".to_string(),
                baseline_value: None,
                current_value: None,
                delta_basis_points: None,
                threshold_basis_points: None,
            });
            report.log(
                "error",
                "perf evidence replay failed",
                current,
                current_artifact_path,
                Some(failure_reason),
            );
            return report;
        }

        self.evaluate_measurement_coverage(current, baseline, &mut report);
        self.evaluate_proof_status(current, &mut report);
        self.evaluate_proof_p99(current, &mut report);
        if let Some(baseline) = baseline {
            self.evaluate_composed_p99(current, baseline, &mut report);
            self.evaluate_total_elapsed(current, baseline, &mut report);
        } else {
            report.log(
                "info",
                "perf evidence replay had no baseline; validated current ledger only",
                current,
                current_artifact_path,
                None,
            );
        }

        if report.verdict.should_fail_build() {
            let reason = report
                .findings
                .iter()
                .find(|finding| finding.verdict == PerfReplayVerdict::Failure)
                .map(|finding| finding.message.clone())
                .unwrap_or_else(|| "perf evidence replay failed".to_string());
            report.log(
                "error",
                "perf evidence replay failed",
                current,
                current_artifact_path,
                Some(reason),
            );
        } else if report.verdict == PerfReplayVerdict::Warning {
            report.log(
                "warn",
                "perf evidence replay produced warnings",
                current,
                current_artifact_path,
                None,
            );
        } else {
            report.log(
                "info",
                "perf evidence replay passed",
                current,
                current_artifact_path,
                None,
            );
        }

        report
    }

    pub fn replay_files<P>(
        &self,
        current_path: P,
        baseline_path: Option<P>,
    ) -> Result<PerfReplayReport, PerfEvidenceIoError>
    where
        P: AsRef<Path>,
    {
        let current_path = current_path.as_ref();
        let current = read_perf_evidence_ledger(current_path)?;
        let baseline = match baseline_path {
            Some(path) => Some(read_perf_evidence_ledger(path.as_ref())?),
            None => None,
        };
        Ok(self.replay_with_artifact(&current, baseline.as_ref(), Some(current_path)))
    }

    fn evaluate_measurement_coverage(
        &self,
        current: &PerfEvidenceLedger,
        baseline: Option<&PerfEvidenceLedger>,
        report: &mut PerfReplayReport,
    ) {
        let current_has_phase_timings = !current.phases.is_empty();
        let current_has_proof = current.proof.status != PerfProofStatus::NotMeasured
            || current.proof.p99_regression_basis_points.is_some();
        if !current_has_phase_timings && !current_has_proof {
            report.add_finding(PerfReplayFinding {
                verdict: PerfReplayVerdict::Warning,
                metric: PerfReplayMetric::MeasurementCoverage,
                message: "current perf evidence ledger has no phase timings or proof summary"
                    .to_string(),
                baseline_value: None,
                current_value: None,
                delta_basis_points: None,
                threshold_basis_points: None,
            });
        }

        if baseline.is_some_and(|ledger| ledger.phases.is_empty()) {
            report.add_finding(PerfReplayFinding {
                verdict: PerfReplayVerdict::Warning,
                metric: PerfReplayMetric::MeasurementCoverage,
                message:
                    "baseline perf evidence ledger has no phase timings; timing comparisons skipped"
                        .to_string(),
                baseline_value: None,
                current_value: None,
                delta_basis_points: None,
                threshold_basis_points: None,
            });
        }
    }

    fn evaluate_proof_status(&self, current: &PerfEvidenceLedger, report: &mut PerfReplayReport) {
        match current.proof.status {
            PerfProofStatus::Failed => report.add_finding(PerfReplayFinding {
                verdict: PerfReplayVerdict::Failure,
                metric: PerfReplayMetric::ProofStatus,
                message: "perf evidence proof status is failed".to_string(),
                baseline_value: None,
                current_value: None,
                delta_basis_points: None,
                threshold_basis_points: None,
            }),
            PerfProofStatus::Inconclusive => report.add_finding(PerfReplayFinding {
                verdict: PerfReplayVerdict::Warning,
                metric: PerfReplayMetric::ProofStatus,
                message: "perf evidence proof status is inconclusive".to_string(),
                baseline_value: None,
                current_value: None,
                delta_basis_points: None,
                threshold_basis_points: None,
            }),
            PerfProofStatus::NotMeasured | PerfProofStatus::Passed => {}
        }
    }

    fn evaluate_proof_p99(&self, current: &PerfEvidenceLedger, report: &mut PerfReplayReport) {
        let Some(delta_basis_points) = current.proof.p99_regression_basis_points else {
            return;
        };
        self.add_threshold_finding(
            report,
            PerfReplayMetric::ProofP99Regression,
            "proof-reported p99 regression",
            None,
            None,
            delta_basis_points,
            self.thresholds.warning_p99_regression_basis_points,
            self.thresholds.failure_p99_regression_basis_points,
        );
    }

    fn evaluate_composed_p99(
        &self,
        current: &PerfEvidenceLedger,
        baseline: &PerfEvidenceLedger,
        report: &mut PerfReplayReport,
    ) {
        let Some(baseline_p99) = composed_p99_ms(baseline) else {
            return;
        };
        let Some(current_p99) = composed_p99_ms(current) else {
            return;
        };
        let Some(delta_basis_points) = basis_points_delta(baseline_p99, current_p99) else {
            return;
        };
        self.add_threshold_finding(
            report,
            PerfReplayMetric::ComposedP99,
            "composed phase p99 regression",
            Some(baseline_p99),
            Some(current_p99),
            delta_basis_points,
            self.thresholds.warning_p99_regression_basis_points,
            self.thresholds.failure_p99_regression_basis_points,
        );
    }

    fn evaluate_total_elapsed(
        &self,
        current: &PerfEvidenceLedger,
        baseline: &PerfEvidenceLedger,
        report: &mut PerfReplayReport,
    ) {
        let baseline_elapsed = total_elapsed_ms(baseline);
        let current_elapsed = total_elapsed_ms(current);
        let Some(delta_basis_points) = basis_points_delta(baseline_elapsed, current_elapsed) else {
            return;
        };
        self.add_threshold_finding(
            report,
            PerfReplayMetric::TotalElapsed,
            "total elapsed phase time regression",
            Some(baseline_elapsed),
            Some(current_elapsed),
            delta_basis_points,
            self.thresholds.warning_elapsed_regression_basis_points,
            self.thresholds.failure_elapsed_regression_basis_points,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn add_threshold_finding(
        &self,
        report: &mut PerfReplayReport,
        metric: PerfReplayMetric,
        label: &str,
        baseline_value: Option<i64>,
        current_value: Option<i64>,
        delta_basis_points: i64,
        warning_basis_points: i64,
        failure_basis_points: i64,
    ) {
        if delta_basis_points < warning_basis_points {
            return;
        }
        let (verdict, threshold_basis_points) = if delta_basis_points >= failure_basis_points {
            (PerfReplayVerdict::Failure, failure_basis_points)
        } else {
            (PerfReplayVerdict::Warning, warning_basis_points)
        };
        report.add_finding(PerfReplayFinding {
            verdict,
            metric,
            message: format!("{label}: +{delta_basis_points} bps"),
            baseline_value,
            current_value,
            delta_basis_points: Some(delta_basis_points),
            threshold_basis_points: Some(threshold_basis_points),
        });
    }
}

fn validate_phase(
    phase: &PerfPhaseTiming,
    index: usize,
) -> Result<(), PerfEvidenceValidationError> {
    if phase.name.trim().is_empty() {
        return Err(PerfEvidenceValidationError::EmptyPhaseName { index });
    }
    if quantile_order_violated(phase.p50_ms, phase.p95_ms)
        || quantile_order_violated(phase.p95_ms, phase.p99_ms)
        || quantile_order_violated(phase.p50_ms, phase.p99_ms)
    {
        return Err(PerfEvidenceValidationError::PhaseQuantilesOutOfOrder { index });
    }
    Ok(())
}

fn composed_p99_ms(ledger: &PerfEvidenceLedger) -> Option<i64> {
    let mut total = 0u64;
    let mut saw_phase = false;
    for phase in &ledger.phases {
        total = total.checked_add(phase.p99_ms?)?;
        saw_phase = true;
    }
    saw_phase.then_some(total.min(i64::MAX as u64) as i64)
}

fn total_elapsed_ms(ledger: &PerfEvidenceLedger) -> i64 {
    ledger
        .phases
        .iter()
        .map(|phase| phase.elapsed_ms)
        .fold(0u64, u64::saturating_add)
        .min(i64::MAX as u64) as i64
}

fn basis_points_delta(baseline: i64, current: i64) -> Option<i64> {
    if baseline <= 0 {
        return None;
    }
    let delta = i128::from(current) - i128::from(baseline);
    let scaled = delta.checked_mul(10_000)?;
    let rounded = if delta >= 0 {
        scaled.checked_add(i128::from(baseline / 2))?
    } else {
        scaled.checked_sub(i128::from(baseline / 2))?
    };
    let basis_points = rounded.checked_div(i128::from(baseline))?;
    i64::try_from(basis_points).ok()
}

fn validate_threshold_pair(
    warning_basis_points: i64,
    failure_basis_points: i64,
    metric: &'static str,
) -> Result<(), &'static str> {
    if warning_basis_points < 0 || failure_basis_points < 0 {
        return Err("perf replay thresholds must be non-negative basis points");
    }
    if warning_basis_points >= failure_basis_points {
        return match metric {
            "p99" => Err(
                "warning_p99_regression_basis_points must be less than failure_p99_regression_basis_points",
            ),
            "elapsed" => Err(
                "warning_elapsed_regression_basis_points must be less than failure_elapsed_regression_basis_points",
            ),
            _ => Err("warning threshold must be less than failure threshold"),
        };
    }
    Ok(())
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};

    hex::encode(Sha256::digest(bytes))
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
    fn recorder_accumulates_phases_snapshots_and_artifacts() {
        let mut recorder = PerfEvidenceRecorder::new(
            "recorder-run",
            PerfWorkload {
                kind: PerfWorkloadKind::WatchOnce,
                name: "watch-once-ingest".to_string(),
                description: None,
                command_args: vec![
                    "cass".to_string(),
                    "index".to_string(),
                    "--watch-once".to_string(),
                    "/tmp/session.jsonl".to_string(),
                    "--json".to_string(),
                ],
                input_count: Some(PerfCount {
                    value: 64,
                    precision: PerfCountPrecision::Exact,
                }),
            },
            42,
        );

        recorder
            .machine(PerfMachineProfile {
                logical_cpus: Some(64),
                reserved_cores: Some(4),
                available_memory_bytes: Some(256 * 1024 * 1024 * 1024),
                topology_class: Some("many_core".to_string()),
            })
            .env_kv("CASS_WATCH_ONCE_INGEST_CHUNK_CONVERSATIONS", "64")
            .cache_snapshot(PerfCacheSnapshot {
                result_cache_hits: 7,
                result_cache_misses: 2,
                eviction_count: 1,
                approx_bytes: Some(1_024),
                byte_cap: Some(2_048),
            })
            .artifact(PerfArtifactRef {
                label: "trace".to_string(),
                path: "tests/artifacts/perf/trace.json".to_string(),
                kind: "json".to_string(),
                sha256: None,
            });
        recorder
            .record_phase(phase("queue", PerfPhaseKind::Queueing, 3, 1, 2, 3))
            .unwrap()
            .begin_phase("emit-json", PerfPhaseKind::Output)
            .unwrap()
            .finish_phase()
            .unwrap();

        let ledger = recorder.finish().unwrap();

        ledger.validate().unwrap();
        assert_eq!(ledger.run_id, "recorder-run");
        assert_eq!(
            ledger.env["CASS_WATCH_ONCE_INGEST_CHUNK_CONVERSATIONS"],
            "64"
        );
        assert_eq!(ledger.phases.len(), 2);
        assert_eq!(ledger.phases[0].kind, PerfPhaseKind::Queueing);
        assert_eq!(ledger.phases[1].name, "emit-json");
        assert_eq!(ledger.artifacts[0].label, "trace");
    }

    #[test]
    fn recorder_rejects_overlapping_or_missing_active_phase() {
        let mut recorder = PerfEvidenceRecorder::new(
            "active-phase-run",
            PerfWorkload::new(PerfWorkloadKind::Search, "search"),
            1,
        );

        assert_eq!(
            recorder.finish_phase().unwrap_err().to_string(),
            "no perf evidence phase is active"
        );

        recorder
            .begin_phase("service", PerfPhaseKind::Service)
            .unwrap();
        let err = recorder
            .begin_phase("io", PerfPhaseKind::Io)
            .unwrap_err()
            .to_string();
        assert!(err.contains("service"), "{err}");
    }

    #[test]
    fn replay_gate_detects_p99_and_elapsed_regressions() {
        let baseline = representative_ledger();
        let mut current = representative_ledger();
        current.run_id = "current-regressed".to_string();
        current.phases = vec![
            phase("admission", PerfPhaseKind::Queueing, 4, 2, 3, 5),
            phase("bm25", PerfPhaseKind::Service, 30, 20, 24, 30),
            phase("semantic", PerfPhaseKind::Io, 45, 30, 40, 45),
            phase("merge", PerfPhaseKind::Synchronization, 12, 7, 10, 12),
            phase("retry-budget", PerfPhaseKind::Retries, 2, 1, 2, 2),
            phase("hydrate", PerfPhaseKind::Hydration, 18, 10, 15, 18),
            phase("emit-json", PerfPhaseKind::Output, 6, 3, 5, 6),
        ];

        let gate =
            PerfReplayGate::new(PerfReplayThresholds::try_new(500, 1_000, 500, 1_000).unwrap());
        let report = gate.replay(&current, Some(&baseline));

        assert_eq!(report.verdict, PerfReplayVerdict::Failure);
        assert!(report.should_fail_build());
        assert!(
            report
                .findings
                .iter()
                .any(|finding| finding.metric == PerfReplayMetric::ComposedP99
                    && finding.verdict == PerfReplayVerdict::Failure),
            "{report:#?}"
        );
        assert!(
            report
                .findings
                .iter()
                .any(|finding| finding.metric == PerfReplayMetric::TotalElapsed),
            "{report:#?}"
        );
    }

    #[test]
    fn replay_gate_warns_on_inconclusive_proof_and_fails_on_failed_proof() {
        let mut current = representative_ledger();
        current.proof.status = PerfProofStatus::Inconclusive;

        let gate = PerfReplayGate::new(PerfReplayThresholds::defaults());
        let report = gate.replay(&current, None);

        assert_eq!(report.verdict, PerfReplayVerdict::Warning);
        assert!(
            report
                .findings
                .iter()
                .any(|finding| finding.metric == PerfReplayMetric::ProofStatus)
        );

        current.proof.status = PerfProofStatus::Failed;
        let report = gate.replay(&current, None);

        assert_eq!(report.verdict, PerfReplayVerdict::Failure);
        assert!(
            report
                .logs
                .iter()
                .any(|event| event.failure_reason.as_deref()
                    == Some("perf evidence proof status is failed")),
            "{report:#?}"
        );
    }

    #[test]
    fn replay_gate_uses_proof_reported_p99_without_baseline() {
        let mut current = representative_ledger();
        current.proof.p99_regression_basis_points = Some(1_500);

        let gate =
            PerfReplayGate::new(PerfReplayThresholds::try_new(500, 1_000, 500, 1_000).unwrap());
        let report = gate.replay(&current, None);

        assert_eq!(report.verdict, PerfReplayVerdict::Failure);
        assert!(
            report.findings.iter().any(|finding| finding.metric
                == PerfReplayMetric::ProofP99Regression
                && finding.delta_basis_points == Some(1_500)),
            "{report:#?}"
        );
    }

    #[test]
    fn replay_gate_warns_when_current_ledger_has_no_measurements() {
        let current = PerfEvidenceLedger::new(
            "empty-measurement-run",
            PerfWorkload::new(PerfWorkloadKind::Search, "empty-measurement"),
            1,
        );

        let gate = PerfReplayGate::new(PerfReplayThresholds::defaults());
        let report = gate.replay(&current, None);

        assert_eq!(report.verdict, PerfReplayVerdict::Warning);
        assert!(
            report
                .findings
                .iter()
                .any(|finding| finding.metric == PerfReplayMetric::MeasurementCoverage),
            "{report:#?}"
        );
    }

    #[test]
    fn replay_thresholds_reject_unreachable_warning_bands() {
        assert_eq!(
            PerfReplayThresholds::try_new(1_000, 1_000, 500, 1_000),
            Err(
                "warning_p99_regression_basis_points must be less than failure_p99_regression_basis_points"
            )
        );
        assert_eq!(
            PerfReplayThresholds::try_new(500, 1_000, -1, 1_000),
            Err("perf replay thresholds must be non-negative basis points")
        );
    }

    #[test]
    fn replay_log_events_include_command_shape_and_artifact_context() {
        let baseline = representative_ledger();
        let mut current = representative_ledger();
        current.run_id = "artifact-context".to_string();
        current.proof.status = PerfProofStatus::Failed;

        let gate = PerfReplayGate::new(PerfReplayThresholds::defaults());
        let report = gate.replay_with_artifact(
            &current,
            Some(&baseline),
            Some(Path::new("tests/artifacts/perf/current.json")),
        );

        let failure_log = report
            .logs
            .iter()
            .find(|event| event.level == "error")
            .expect("error log");
        assert_eq!(failure_log.run_id, "artifact-context");
        assert_eq!(
            failure_log.artifact_path.as_deref(),
            Some("tests/artifacts/perf/current.json")
        );
        assert_eq!(
            failure_log.command_args,
            ["cass", "search", "wal conflict", "--json"]
        );
        assert_eq!(
            failure_log.failure_reason.as_deref(),
            Some("perf evidence proof status is failed")
        );
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
