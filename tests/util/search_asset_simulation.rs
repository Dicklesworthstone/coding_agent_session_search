#![allow(dead_code)]

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use tempfile::TempDir;

/// Stable logical clock for deterministic simulation logs.
#[derive(Debug, Clone)]
struct DeterministicClock {
    next_ms: i64,
    step_ms: i64,
}

impl Default for DeterministicClock {
    fn default() -> Self {
        Self {
            next_ms: 1_700_000_000_000,
            step_ms: 100,
        }
    }
}

impl DeterministicClock {
    fn tick(&mut self) -> i64 {
        let current = self.next_ms;
        self.next_ms += self.step_ms;
        current
    }
}

/// Coarse scheduler pressure state for deterministic busy/idle/load tests.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LoadTier {
    Idle,
    Busy,
    Loaded,
}

impl LoadTier {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Busy => "busy",
            Self::Loaded => "loaded",
        }
    }
}

/// One deterministic pressure sample consumed by the scheduler harness.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LoadSample {
    pub label: String,
    pub tier: LoadTier,
    pub cpu_pct: u8,
    pub io_pct: u8,
    pub active_foreground_searches: u8,
    pub active_lexical_repairs: u8,
    pub user_active: bool,
}

impl LoadSample {
    pub fn idle(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            tier: LoadTier::Idle,
            cpu_pct: 8,
            io_pct: 5,
            active_foreground_searches: 0,
            active_lexical_repairs: 0,
            user_active: false,
        }
    }

    pub fn busy(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            tier: LoadTier::Busy,
            cpu_pct: 48,
            io_pct: 28,
            active_foreground_searches: 2,
            active_lexical_repairs: 1,
            user_active: true,
        }
    }

    pub fn loaded(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            tier: LoadTier::Loaded,
            cpu_pct: 82,
            io_pct: 71,
            active_foreground_searches: 4,
            active_lexical_repairs: 2,
            user_active: true,
        }
    }
}

/// Deterministic scripted pressure source for scheduler and controller tests.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LoadScript {
    samples: Vec<LoadSample>,
    cursor: usize,
}

impl LoadScript {
    pub fn new(samples: Vec<LoadSample>) -> Self {
        assert!(
            !samples.is_empty(),
            "load script must contain at least one sample"
        );
        Self { samples, cursor: 0 }
    }

    pub fn current(&self) -> &LoadSample {
        let idx = self.cursor.min(self.samples.len().saturating_sub(1));
        &self.samples[idx]
    }

    /// Return the current sample and then advance, saturating at the tail.
    pub fn step(&mut self) -> LoadSample {
        let sample = self.current().clone();
        if self.cursor + 1 < self.samples.len() {
            self.cursor += 1;
        }
        sample
    }

    pub fn reset(&mut self) {
        self.cursor = 0;
    }
}

/// Publish-path crash windows that later generation/promotion beads can target.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum PublishCrashWindow {
    AcquirePublishLock,
    StageScratchGeneration,
    SyncScratchGeneration,
    SwapPublishedGeneration,
    SaveGenerationManifest,
    CleanupSupersededGeneration,
}

impl PublishCrashWindow {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::AcquirePublishLock => "acquire_publish_lock",
            Self::StageScratchGeneration => "stage_scratch_generation",
            Self::SyncScratchGeneration => "sync_scratch_generation",
            Self::SwapPublishedGeneration => "swap_published_generation",
            Self::SaveGenerationManifest => "save_generation_manifest",
            Self::CleanupSupersededGeneration => "cleanup_superseded_generation",
        }
    }
}

/// Staged model-acquisition checkpoints for deterministic interruption tests.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum AcquisitionStage {
    DetectExistingAssets,
    PrepareStagingDir,
    DownloadPayload,
    VerifyChecksum,
    PromoteInstall,
    MarkReady,
}

impl AcquisitionStage {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::DetectExistingAssets => "detect_existing_assets",
            Self::PrepareStagingDir => "prepare_staging_dir",
            Self::DownloadPayload => "download_payload",
            Self::VerifyChecksum => "verify_checksum",
            Self::PromoteInstall => "promote_install",
            Self::MarkReady => "mark_ready",
        }
    }
}

/// Deterministic failpoint targets exposed by the simulation harness.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(tag = "kind", content = "target", rename_all = "snake_case")]
pub enum FailpointId {
    Publish(PublishCrashWindow),
    Acquisition(AcquisitionStage),
}

impl FailpointId {
    pub fn as_str(&self) -> String {
        match self {
            Self::Publish(window) => format!("publish:{}", window.as_str()),
            Self::Acquisition(stage) => format!("acquisition:{}", stage.as_str()),
        }
    }
}

/// One injected failpoint effect. Each action is consumed once.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FailpointEffect {
    CrashOnce,
    ErrorOnce { reason: String },
}

impl FailpointEffect {
    fn label(&self) -> &'static str {
        match self {
            Self::CrashOnce => "crash_once",
            Self::ErrorOnce { .. } => "error_once",
        }
    }
}

/// Failure returned from an injected deterministic crash or staged error.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SimulationFailure {
    Crash {
        failpoint: FailpointId,
    },
    InjectedError {
        failpoint: FailpointId,
        reason: String,
    },
}

impl std::fmt::Display for SimulationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Crash { failpoint } => write!(f, "simulated crash at {}", failpoint.as_str()),
            Self::InjectedError { failpoint, reason } => {
                write!(f, "simulated failure at {}: {reason}", failpoint.as_str())
            }
        }
    }
}

impl std::error::Error for SimulationFailure {}

/// High-level actors that contend in maintenance-orchestration scenarios.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum SimulationActor {
    ForegroundSearch,
    LexicalRepair,
    SemanticAcquire,
    BackgroundSemantic,
}

impl SimulationActor {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ForegroundSearch => "foreground_search",
            Self::LexicalRepair => "lexical_repair",
            Self::SemanticAcquire => "semantic_acquire",
            Self::BackgroundSemantic => "background_semantic",
        }
    }
}

/// One deterministic turn in a multi-actor contention schedule.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContentionTurn {
    pub actor: SimulationActor,
    pub label: String,
}

/// Builder-style contention plan used by orchestration tests.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContentionPlan {
    turns: Vec<ContentionTurn>,
}

impl ContentionPlan {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn turn(mut self, actor: SimulationActor, label: impl Into<String>) -> Self {
        self.turns.push(ContentionTurn {
            actor,
            label: label.into(),
        });
        self
    }

    pub fn turns(&self) -> &[ContentionTurn] {
        &self.turns
    }
}

/// Structured phase log entry with the same JSONL field names as the earlier harness.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SimulationLogEntry {
    pub timestamp_ms: i64,
    pub phase: String,
    pub message: String,
    pub artifacts: BTreeMap<String, String>,
}

/// Marker for an injected crash window or staged-acquisition failure.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FailpointMarker {
    pub timestamp_ms: i64,
    pub failpoint: FailpointId,
    pub effect: String,
    pub detail: Option<String>,
}

/// One actor outcome emitted by the contention simulator.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", content = "detail", rename_all = "snake_case")]
pub enum ActorOutcome {
    Ok,
    Crashed,
    Failed(String),
}

/// Deterministic per-actor trace entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActorTraceEntry {
    pub timestamp_ms: i64,
    pub turn_index: usize,
    pub actor: SimulationActor,
    pub label: String,
    pub load: LoadSample,
    pub outcome: ActorOutcome,
}

/// Stable, path-free summary used for determinism checks across repeated runs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SimulationSummary {
    pub scenario: String,
    pub phase_log: Vec<SimulationLogEntry>,
    pub failpoint_markers: Vec<FailpointMarker>,
    pub actor_traces: Vec<ActorTraceEntry>,
    pub snapshot_digests: BTreeMap<String, String>,
}

/// Paths to the persisted diagnostic artifacts for one simulation run.
#[derive(Debug, Clone)]
pub struct SimulationArtifacts {
    pub root: PathBuf,
    pub phase_log_path: PathBuf,
    pub failpoints_path: PathBuf,
    pub actor_traces_path: PathBuf,
    pub summary_path: PathBuf,
    pub snapshot_dir: PathBuf,
}

/// Reusable deterministic harness for scheduler/load/publish simulation tests.
pub struct SearchAssetSimulationHarness {
    scenario: String,
    dir: TempDir,
    artifact_root: PathBuf,
    snapshot_dir: PathBuf,
    clock: DeterministicClock,
    load_script: LoadScript,
    active_load: Option<LoadSample>,
    phase_log: Vec<SimulationLogEntry>,
    failpoints: BTreeMap<FailpointId, VecDeque<FailpointEffect>>,
    failpoint_markers: Vec<FailpointMarker>,
    actor_traces: Vec<ActorTraceEntry>,
    snapshot_counter: usize,
    snapshot_digests: BTreeMap<String, String>,
}

impl SearchAssetSimulationHarness {
    pub fn new(scenario: impl Into<String>, load_script: LoadScript) -> Self {
        let scenario = scenario.into();
        let dir = TempDir::new().expect("create simulation tempdir");
        let artifact_root = dir.path().join(sanitize_label(&scenario));
        let snapshot_dir = artifact_root.join("snapshots");
        fs::create_dir_all(&snapshot_dir).expect("create simulation snapshot dir");

        let mut harness = Self {
            scenario,
            dir,
            artifact_root,
            snapshot_dir,
            clock: DeterministicClock::default(),
            load_script,
            active_load: None,
            phase_log: Vec::new(),
            failpoints: BTreeMap::new(),
            failpoint_markers: Vec::new(),
            actor_traces: Vec::new(),
            snapshot_counter: 0,
            snapshot_digests: BTreeMap::new(),
        };
        harness.phase("setup", "simulation harness created");
        harness
    }

    pub fn artifact_root(&self) -> &Path {
        &self.artifact_root
    }

    pub fn snapshot_root(&self) -> &Path {
        &self.snapshot_dir
    }

    pub fn current_load(&self) -> Option<&LoadSample> {
        self.active_load.as_ref()
    }

    pub fn phase(&mut self, phase: &str, message: &str) {
        self.phase_with_artifacts(phase, message, BTreeMap::new());
    }

    pub fn phase_with_artifacts(
        &mut self,
        phase: &str,
        message: &str,
        artifacts: BTreeMap<String, String>,
    ) {
        self.phase_log.push(SimulationLogEntry {
            timestamp_ms: self.clock.tick(),
            phase: phase.to_owned(),
            message: message.to_owned(),
            artifacts,
        });
    }

    /// Snapshot the current state of a directory tree (file names + sizes).
    pub fn snapshot_dir(&mut self, phase: &str, dir: &Path) {
        let mut artifacts = BTreeMap::new();
        if let Ok(entries) = fs::read_dir(dir) {
            let mut rows: Vec<_> = entries
                .flatten()
                .map(|entry| {
                    let name = entry.file_name().to_string_lossy().to_string();
                    let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                    (name, format!("{size} bytes"))
                })
                .collect();
            rows.sort_by(|a, b| a.0.cmp(&b.0));
            for (name, size) in rows {
                artifacts.insert(name, size);
            }
        }
        self.phase_with_artifacts(phase, &format!("snapshot of {}", dir.display()), artifacts);
    }

    /// Write a stable JSON snapshot into the artifact directory and record its digest.
    pub fn snapshot_json<T: Serialize>(&mut self, label: &str, value: &T) -> PathBuf {
        self.snapshot_counter += 1;
        let filename = format!(
            "{:03}-{}.json",
            self.snapshot_counter,
            sanitize_label(label)
        );
        let path = self.snapshot_dir.join(filename.clone());
        let json = serde_json::to_vec_pretty(value).expect("serialize simulation snapshot");
        fs::write(&path, &json).expect("write simulation snapshot");
        let digest = blake3::hash(&json).to_hex().to_string();
        self.snapshot_digests
            .insert(filename.clone(), digest.clone());

        let mut artifacts = BTreeMap::new();
        artifacts.insert("snapshot".to_owned(), filename);
        artifacts.insert("digest".to_owned(), digest);
        self.phase_with_artifacts("snapshot", label, artifacts);

        path
    }

    pub fn install_failpoint_once(&mut self, failpoint: FailpointId, effect: FailpointEffect) {
        self.failpoints
            .entry(failpoint.clone())
            .or_default()
            .push_back(effect.clone());

        let mut artifacts = BTreeMap::new();
        artifacts.insert("failpoint".to_owned(), failpoint.as_str());
        artifacts.insert("effect".to_owned(), effect.label().to_owned());
        self.phase_with_artifacts(
            "failpoint_install",
            "installed deterministic failpoint",
            artifacts,
        );
    }

    pub fn trigger_failpoint(&mut self, failpoint: FailpointId) -> Result<(), SimulationFailure> {
        let Some(queue) = self.failpoints.get_mut(&failpoint) else {
            return Ok(());
        };
        let Some(effect) = queue.pop_front() else {
            return Ok(());
        };
        let timestamp_ms = self.clock.tick();
        let (detail, failure) = match effect {
            FailpointEffect::CrashOnce => (
                None,
                SimulationFailure::Crash {
                    failpoint: failpoint.clone(),
                },
            ),
            FailpointEffect::ErrorOnce { ref reason } => (
                Some(reason.clone()),
                SimulationFailure::InjectedError {
                    failpoint: failpoint.clone(),
                    reason: reason.clone(),
                },
            ),
        };
        self.failpoint_markers.push(FailpointMarker {
            timestamp_ms,
            failpoint: failpoint.clone(),
            effect: effect.label().to_owned(),
            detail: detail.clone(),
        });

        let mut artifacts = BTreeMap::new();
        artifacts.insert("failpoint".to_owned(), failpoint.as_str());
        artifacts.insert("effect".to_owned(), effect.label().to_owned());
        if let Some(detail) = detail {
            artifacts.insert("detail".to_owned(), detail);
        }
        self.phase_with_artifacts(
            "failpoint_triggered",
            "deterministic failpoint triggered",
            artifacts,
        );
        Err(failure)
    }

    pub fn run_contention_plan<F>(
        &mut self,
        plan: &ContentionPlan,
        mut callback: F,
    ) -> Vec<Result<(), SimulationFailure>>
    where
        F: FnMut(&ContentionTurn, &mut Self) -> Result<(), SimulationFailure>,
    {
        let mut results = Vec::with_capacity(plan.turns().len());
        for (turn_index, turn) in plan.turns().iter().enumerate() {
            let load = self.load_script.step();
            self.active_load = Some(load.clone());

            let mut artifacts = BTreeMap::new();
            artifacts.insert("actor".to_owned(), turn.actor.as_str().to_owned());
            artifacts.insert("label".to_owned(), turn.label.clone());
            artifacts.insert("load".to_owned(), load.label.clone());
            artifacts.insert("load_tier".to_owned(), load.tier.as_str().to_owned());
            self.phase_with_artifacts(
                "contention_turn",
                "executing deterministic actor turn",
                artifacts,
            );

            let result = callback(turn, self);
            let outcome = match &result {
                Ok(()) => ActorOutcome::Ok,
                Err(SimulationFailure::Crash { .. }) => ActorOutcome::Crashed,
                Err(SimulationFailure::InjectedError { reason, .. }) => {
                    ActorOutcome::Failed(reason.clone())
                }
            };

            self.actor_traces.push(ActorTraceEntry {
                timestamp_ms: self.clock.tick(),
                turn_index,
                actor: turn.actor,
                label: turn.label.clone(),
                load,
                outcome,
            });
            results.push(result);
        }
        results
    }

    pub fn summary(&self) -> SimulationSummary {
        SimulationSummary {
            scenario: self.scenario.clone(),
            phase_log: self.phase_log.clone(),
            failpoint_markers: self.failpoint_markers.clone(),
            actor_traces: self.actor_traces.clone(),
            snapshot_digests: self.snapshot_digests.clone(),
        }
    }

    pub fn phase_log_jsonl(&self) -> String {
        self.phase_log
            .iter()
            .map(|entry| {
                let phase =
                    serde_json::to_string(&entry.phase).unwrap_or_else(|_| "\"\"".to_owned());
                let msg =
                    serde_json::to_string(&entry.message).unwrap_or_else(|_| "\"\"".to_owned());
                let artifacts =
                    serde_json::to_string(&entry.artifacts).unwrap_or_else(|_| "{}".to_owned());
                format!(
                    r#"{{"ts":{},"phase":{},"msg":{},"artifacts":{}}}"#,
                    entry.timestamp_ms, phase, msg, artifacts
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn write_artifacts(&self) -> std::io::Result<SimulationArtifacts> {
        let root = persistent_artifact_root(&self.scenario);
        let snapshot_dir = root.join("snapshots");
        fs::create_dir_all(&snapshot_dir)?;

        for entry in fs::read_dir(&self.snapshot_dir)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if !file_type.is_file() {
                continue;
            }
            fs::copy(entry.path(), snapshot_dir.join(entry.file_name()))?;
        }

        let phase_log_path = root.join("phase-log.jsonl");
        let failpoints_path = root.join("failpoints.json");
        let actor_traces_path = root.join("actor-traces.json");
        let summary_path = root.join("summary.json");

        fs::write(&phase_log_path, self.phase_log_jsonl())?;
        write_pretty_json_file(&failpoints_path, &self.failpoint_markers)?;
        write_pretty_json_file(&actor_traces_path, &self.actor_traces)?;
        write_pretty_json_file(&summary_path, &self.summary())?;

        Ok(SimulationArtifacts {
            root,
            phase_log_path,
            failpoints_path,
            actor_traces_path,
            summary_path,
            snapshot_dir,
        })
    }
}

fn write_pretty_json_file<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    fs::write(
        path,
        serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?,
    )
}

fn sanitize_label(label: &str) -> String {
    let sanitized = label
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => ch,
            _ => '_',
        })
        .collect::<String>();
    let trimmed = sanitized.trim_matches('_');
    if trimmed.is_empty() {
        "scenario".to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn persistent_artifact_root(scenario: &str) -> PathBuf {
    static NEXT_ARTIFACT_ID: AtomicU64 = AtomicU64::new(0);

    let artifact_id = NEXT_ARTIFACT_ID.fetch_add(1, Ordering::Relaxed);
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("test-results")
        .join("search_asset_simulation")
        .join(format!("{:03}-{}", artifact_id, sanitize_label(scenario)))
}
