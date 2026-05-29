//! Deterministic crash/replay harness for state-machine proof tests.
//!
//! The harness is intentionally small and data-only: production code exposes
//! named checkpoints, tests simulate a crash at each checkpoint, then restart
//! and verify invariants. The resulting report can be saved as a JSON artifact
//! for later replay or review.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub const CRASH_REPLAY_SCHEMA_VERSION: &str = "1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrashReplayCheckpoint {
    pub id: String,
    pub ordinal: u32,
    pub description: String,
}

impl CrashReplayCheckpoint {
    pub fn new(ordinal: u32, id: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            ordinal,
            description: description.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrashReplayPhase {
    AdvanceToCheckpoint,
    InjectCrash,
    Restart,
    CheckInvariants,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrashReplayEvent {
    pub checkpoint_id: String,
    pub phase: CrashReplayPhase,
    pub ok: bool,
    pub detail: String,
}

impl CrashReplayEvent {
    fn ok(
        checkpoint: &CrashReplayCheckpoint,
        phase: CrashReplayPhase,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            checkpoint_id: checkpoint.id.clone(),
            phase,
            ok: true,
            detail: detail.into(),
        }
    }

    fn failed(
        checkpoint: &CrashReplayCheckpoint,
        phase: CrashReplayPhase,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            checkpoint_id: checkpoint.id.clone(),
            phase,
            ok: false,
            detail: detail.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrashReplayInvariant {
    pub checkpoint_id: String,
    pub name: String,
    pub passed: bool,
    pub detail: String,
}

impl CrashReplayInvariant {
    pub fn passed(
        checkpoint: &CrashReplayCheckpoint,
        name: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            checkpoint_id: checkpoint.id.clone(),
            name: name.into(),
            passed: true,
            detail: detail.into(),
        }
    }

    pub fn failed(
        checkpoint: &CrashReplayCheckpoint,
        name: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            checkpoint_id: checkpoint.id.clone(),
            name: name.into(),
            passed: false,
            detail: detail.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrashReplayVerdict {
    Clean,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrashReplayReport {
    pub schema_version: String,
    pub scenario_id: String,
    pub state_machine: String,
    pub verdict: CrashReplayVerdict,
    pub checkpoints: Vec<CrashReplayCheckpoint>,
    pub events: Vec<CrashReplayEvent>,
    pub invariants: Vec<CrashReplayInvariant>,
}

impl CrashReplayReport {
    pub fn validate(&self) -> Result<(), CrashReplayValidationError> {
        if self.schema_version != CRASH_REPLAY_SCHEMA_VERSION {
            return Err(CrashReplayValidationError::UnsupportedSchemaVersion {
                expected: CRASH_REPLAY_SCHEMA_VERSION,
                actual: self.schema_version.clone(),
            });
        }
        if self.scenario_id.trim().is_empty() {
            return Err(CrashReplayValidationError::EmptyScenarioId);
        }
        if self.state_machine.trim().is_empty() {
            return Err(CrashReplayValidationError::EmptyStateMachine);
        }
        if self.checkpoints.is_empty() {
            return Err(CrashReplayValidationError::NoCheckpoints);
        }
        if self.verdict == CrashReplayVerdict::Clean && self.invariants.is_empty() {
            return Err(CrashReplayValidationError::CleanReportWithoutInvariants);
        }

        let mut checkpoint_ids = BTreeSet::new();
        let mut previous_ordinal = None;
        for (index, checkpoint) in self.checkpoints.iter().enumerate() {
            if checkpoint.id.trim().is_empty() {
                return Err(CrashReplayValidationError::EmptyCheckpointId { index });
            }
            if checkpoint.description.trim().is_empty() {
                return Err(CrashReplayValidationError::EmptyCheckpointDescription { index });
            }
            if let Some(previous) = previous_ordinal
                && checkpoint.ordinal <= previous
            {
                return Err(CrashReplayValidationError::NonMonotoneCheckpointOrdinal {
                    index,
                    previous,
                    current: checkpoint.ordinal,
                });
            }
            if !checkpoint_ids.insert(checkpoint.id.as_str()) {
                return Err(CrashReplayValidationError::DuplicateCheckpointId {
                    index,
                    checkpoint_id: checkpoint.id.clone(),
                });
            }
            previous_ordinal = Some(checkpoint.ordinal);
        }

        let mut checked_checkpoints = BTreeSet::new();
        for (index, event) in self.events.iter().enumerate() {
            if event.checkpoint_id.trim().is_empty() {
                return Err(CrashReplayValidationError::EmptyEventCheckpointId { index });
            }
            if !checkpoint_ids.contains(event.checkpoint_id.as_str()) {
                return Err(CrashReplayValidationError::UnknownEventCheckpoint {
                    index,
                    checkpoint_id: event.checkpoint_id.clone(),
                });
            }
            if event.detail.trim().is_empty() {
                return Err(CrashReplayValidationError::EmptyEventDetail { index });
            }
            if event.ok && event.phase == CrashReplayPhase::CheckInvariants {
                checked_checkpoints.insert(event.checkpoint_id.as_str());
            }
        }

        let mut invariant_checkpoints = BTreeSet::new();
        for (index, invariant) in self.invariants.iter().enumerate() {
            if invariant.checkpoint_id.trim().is_empty() {
                return Err(CrashReplayValidationError::EmptyInvariantCheckpointId { index });
            }
            if !checkpoint_ids.contains(invariant.checkpoint_id.as_str()) {
                return Err(CrashReplayValidationError::UnknownInvariantCheckpoint {
                    index,
                    checkpoint_id: invariant.checkpoint_id.clone(),
                });
            }
            if invariant.name.trim().is_empty() {
                return Err(CrashReplayValidationError::EmptyInvariantName { index });
            }
            if invariant.detail.trim().is_empty() {
                return Err(CrashReplayValidationError::EmptyInvariantDetail { index });
            }
            if invariant.passed {
                invariant_checkpoints.insert(invariant.checkpoint_id.as_str());
            }
        }
        if self.verdict == CrashReplayVerdict::Clean
            && (self.events.iter().any(|event| !event.ok)
                || self.invariants.iter().any(|invariant| !invariant.passed))
        {
            return Err(CrashReplayValidationError::CleanReportContainsFailure);
        }
        if self.verdict == CrashReplayVerdict::Clean {
            if self.events.is_empty() {
                return Err(CrashReplayValidationError::CleanReportWithoutEvents);
            }
            for checkpoint in &self.checkpoints {
                if !checked_checkpoints.contains(checkpoint.id.as_str()) {
                    return Err(
                        CrashReplayValidationError::CleanReportMissingCheckpointEvent {
                            checkpoint_id: checkpoint.id.clone(),
                        },
                    );
                }
                if !invariant_checkpoints.contains(checkpoint.id.as_str()) {
                    return Err(
                        CrashReplayValidationError::CleanReportMissingCheckpointInvariant {
                            checkpoint_id: checkpoint.id.clone(),
                        },
                    );
                }
            }
        }

        Ok(())
    }

    pub fn save_json(&self, path: &Path) -> Result<(), CrashReplayIoError> {
        self.validate()?;
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(self)?;
        let temp_path = write_crash_replay_json_temp_file(path, &json)?;
        replace_crash_replay_json_from_temp(&temp_path, path)?;
        Ok(())
    }

    pub fn load_json(path: &Path) -> Result<Self, CrashReplayIoError> {
        let bytes = fs::read(path)?;
        let report: Self = serde_json::from_slice(&bytes)?;
        report.validate()?;
        Ok(report)
    }
}

fn write_crash_replay_json_temp_file(path: &Path, contents: &[u8]) -> io::Result<PathBuf> {
    for _ in 0..100 {
        let temp_path = unique_crash_replay_json_temp_path(path)?;
        match write_crash_replay_json_temp_file_at(&temp_path, contents) {
            Ok(()) => return Ok(temp_path),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!(
            "failed to allocate unique crash replay temp path for {}",
            path.display()
        ),
    ))
}

fn write_crash_replay_json_temp_file_at(path: &Path, contents: &[u8]) -> io::Result<()> {
    use std::io::Write;

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(contents)?;
    file.sync_all()
}

fn replace_crash_replay_json_from_temp(temp_path: &Path, final_path: &Path) -> io::Result<()> {
    fs::rename(temp_path, final_path)?;
    sync_parent_directory(final_path)
}

#[cfg(not(windows))]
fn sync_parent_directory(path: &Path) -> io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    fs::File::open(parent)?.sync_all()
}

#[cfg(windows)]
fn sync_parent_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn unique_crash_replay_json_temp_path(path: &Path) -> io::Result<PathBuf> {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let nonce = crash_replay_temp_path_nonce()?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("crash-replay-report.json");

    Ok(path.with_file_name(format!(".{file_name}.tmp.{timestamp}.{nonce:016x}")))
}

fn crash_replay_temp_path_nonce() -> io::Result<u64> {
    use ring::rand::SecureRandom;

    let mut random_bytes = [0u8; 8];
    ring::rand::SystemRandom::new()
        .fill(&mut random_bytes)
        .map_err(|_| io::Error::other("failed to generate crash replay temp path nonce"))?;
    Ok(u64::from_le_bytes(random_bytes))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrashReplayError {
    pub action: String,
    pub detail: String,
}

impl CrashReplayError {
    pub fn new(action: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            action: action.into(),
            detail: detail.into(),
        }
    }

    pub fn from_error(action: impl Into<String>, error: impl fmt::Display) -> Self {
        Self::new(action, error.to_string())
    }
}

impl fmt::Display for CrashReplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.action, self.detail)
    }
}

impl Error for CrashReplayError {}

#[derive(Debug)]
pub enum CrashReplayValidationError {
    UnsupportedSchemaVersion {
        expected: &'static str,
        actual: String,
    },
    EmptyScenarioId,
    EmptyStateMachine,
    NoCheckpoints,
    EmptyCheckpointId {
        index: usize,
    },
    EmptyCheckpointDescription {
        index: usize,
    },
    DuplicateCheckpointId {
        index: usize,
        checkpoint_id: String,
    },
    NonMonotoneCheckpointOrdinal {
        index: usize,
        previous: u32,
        current: u32,
    },
    CleanReportWithoutInvariants,
    CleanReportWithoutEvents,
    CleanReportContainsFailure,
    CleanReportMissingCheckpointEvent {
        checkpoint_id: String,
    },
    CleanReportMissingCheckpointInvariant {
        checkpoint_id: String,
    },
    EmptyEventCheckpointId {
        index: usize,
    },
    UnknownEventCheckpoint {
        index: usize,
        checkpoint_id: String,
    },
    EmptyEventDetail {
        index: usize,
    },
    EmptyInvariantCheckpointId {
        index: usize,
    },
    UnknownInvariantCheckpoint {
        index: usize,
        checkpoint_id: String,
    },
    EmptyInvariantName {
        index: usize,
    },
    EmptyInvariantDetail {
        index: usize,
    },
}

impl fmt::Display for CrashReplayValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedSchemaVersion { expected, actual } => {
                write!(
                    f,
                    "unsupported crash replay schema version {actual}; expected {expected}"
                )
            }
            Self::EmptyScenarioId => write!(f, "crash replay scenario_id cannot be empty"),
            Self::EmptyStateMachine => write!(f, "crash replay state_machine cannot be empty"),
            Self::NoCheckpoints => write!(f, "crash replay report must include checkpoints"),
            Self::EmptyCheckpointId { index } => {
                write!(f, "crash replay checkpoint #{index} has an empty id")
            }
            Self::EmptyCheckpointDescription { index } => write!(
                f,
                "crash replay checkpoint #{index} has an empty description"
            ),
            Self::DuplicateCheckpointId {
                index,
                checkpoint_id,
            } => write!(
                f,
                "crash replay checkpoint #{index} duplicates checkpoint id {checkpoint_id}"
            ),
            Self::NonMonotoneCheckpointOrdinal {
                index,
                previous,
                current,
            } => write!(
                f,
                "crash replay checkpoint #{index} ordinal {current} must be greater than previous ordinal {previous}"
            ),
            Self::CleanReportWithoutInvariants => {
                write!(f, "clean crash replay report must include invariants")
            }
            Self::CleanReportWithoutEvents => {
                write!(f, "clean crash replay report must include events")
            }
            Self::CleanReportContainsFailure => {
                write!(
                    f,
                    "clean crash replay report contains failed events or invariants"
                )
            }
            Self::CleanReportMissingCheckpointEvent { checkpoint_id } => write!(
                f,
                "clean crash replay report has no successful invariant-check event for checkpoint {checkpoint_id}"
            ),
            Self::CleanReportMissingCheckpointInvariant { checkpoint_id } => write!(
                f,
                "clean crash replay report has no passing invariant for checkpoint {checkpoint_id}"
            ),
            Self::EmptyEventCheckpointId { index } => {
                write!(f, "crash replay event #{index} has an empty checkpoint id")
            }
            Self::UnknownEventCheckpoint {
                index,
                checkpoint_id,
            } => write!(
                f,
                "crash replay event #{index} references unknown checkpoint {checkpoint_id}"
            ),
            Self::EmptyEventDetail { index } => {
                write!(f, "crash replay event #{index} has an empty detail")
            }
            Self::EmptyInvariantCheckpointId { index } => write!(
                f,
                "crash replay invariant #{index} has an empty checkpoint id"
            ),
            Self::UnknownInvariantCheckpoint {
                index,
                checkpoint_id,
            } => write!(
                f,
                "crash replay invariant #{index} references unknown checkpoint {checkpoint_id}"
            ),
            Self::EmptyInvariantName { index } => {
                write!(f, "crash replay invariant #{index} has an empty name")
            }
            Self::EmptyInvariantDetail { index } => {
                write!(f, "crash replay invariant #{index} has an empty detail")
            }
        }
    }
}

impl Error for CrashReplayValidationError {}

#[derive(Debug)]
pub enum CrashReplayIoError {
    Io(io::Error),
    Json(serde_json::Error),
    Validation(CrashReplayValidationError),
}

impl fmt::Display for CrashReplayIoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "crash replay I/O error: {err}"),
            Self::Json(err) => write!(f, "crash replay JSON error: {err}"),
            Self::Validation(err) => write!(f, "crash replay validation error: {err}"),
        }
    }
}

impl Error for CrashReplayIoError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::Json(err) => Some(err),
            Self::Validation(err) => Some(err),
        }
    }
}

impl From<io::Error> for CrashReplayIoError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<serde_json::Error> for CrashReplayIoError {
    fn from(err: serde_json::Error) -> Self {
        Self::Json(err)
    }
}

impl From<CrashReplayValidationError> for CrashReplayIoError {
    fn from(err: CrashReplayValidationError) -> Self {
        Self::Validation(err)
    }
}

pub fn replay_named_checkpoints<S, MakeState, Advance, Restart, Check>(
    scenario_id: impl Into<String>,
    state_machine: impl Into<String>,
    mut checkpoints: Vec<CrashReplayCheckpoint>,
    mut make_state: MakeState,
    mut advance_to_checkpoint: Advance,
    mut restart: Restart,
    mut check_invariants: Check,
) -> CrashReplayReport
where
    MakeState: FnMut() -> Result<S, CrashReplayError>,
    Advance: FnMut(&mut S, &CrashReplayCheckpoint) -> Result<(), CrashReplayError>,
    Restart: FnMut(&mut S) -> Result<(), CrashReplayError>,
    Check: FnMut(&S, &CrashReplayCheckpoint) -> Vec<CrashReplayInvariant>,
{
    checkpoints.sort_by_key(|checkpoint| checkpoint.ordinal);
    let mut report = CrashReplayReport {
        schema_version: CRASH_REPLAY_SCHEMA_VERSION.to_string(),
        scenario_id: scenario_id.into(),
        state_machine: state_machine.into(),
        verdict: CrashReplayVerdict::Clean,
        checkpoints: checkpoints.clone(),
        events: Vec::new(),
        invariants: Vec::new(),
    };

    if checkpoints.is_empty() {
        report.verdict = CrashReplayVerdict::Failed;
        return report;
    }

    for checkpoint in checkpoints {
        let mut state = match make_state() {
            Ok(state) => state,
            Err(err) => {
                report.verdict = CrashReplayVerdict::Failed;
                report.events.push(CrashReplayEvent::failed(
                    &checkpoint,
                    CrashReplayPhase::AdvanceToCheckpoint,
                    format!("failed creating fresh state: {err}"),
                ));
                continue;
            }
        };

        match advance_to_checkpoint(&mut state, &checkpoint) {
            Ok(()) => report.events.push(CrashReplayEvent::ok(
                &checkpoint,
                CrashReplayPhase::AdvanceToCheckpoint,
                "advanced to checkpoint",
            )),
            Err(err) => {
                report.verdict = CrashReplayVerdict::Failed;
                report.events.push(CrashReplayEvent::failed(
                    &checkpoint,
                    CrashReplayPhase::AdvanceToCheckpoint,
                    err.to_string(),
                ));
                continue;
            }
        }

        report.events.push(CrashReplayEvent::ok(
            &checkpoint,
            CrashReplayPhase::InjectCrash,
            "simulated process stop at named checkpoint",
        ));

        match restart(&mut state) {
            Ok(()) => report.events.push(CrashReplayEvent::ok(
                &checkpoint,
                CrashReplayPhase::Restart,
                "restart action completed",
            )),
            Err(err) => {
                report.verdict = CrashReplayVerdict::Failed;
                report.events.push(CrashReplayEvent::failed(
                    &checkpoint,
                    CrashReplayPhase::Restart,
                    err.to_string(),
                ));
                continue;
            }
        }

        let invariants = check_invariants(&state, &checkpoint);
        if invariants.is_empty() {
            report.verdict = CrashReplayVerdict::Failed;
            report.events.push(CrashReplayEvent::failed(
                &checkpoint,
                CrashReplayPhase::CheckInvariants,
                "checkpoint produced no invariants",
            ));
            continue;
        }

        let failed = invariants.iter().any(|invariant| !invariant.passed);
        if failed {
            report.verdict = CrashReplayVerdict::Failed;
        }
        report.events.push(CrashReplayEvent {
            checkpoint_id: checkpoint.id.clone(),
            phase: CrashReplayPhase::CheckInvariants,
            ok: !failed,
            detail: format!("{} invariant(s) evaluated", invariants.len()),
        });
        report.invariants.extend(invariants);
    }

    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy_registry::{
        PolicyControllerStatus, PolicyFallbackState, policy_registry_snapshot,
    };
    use crate::search::policy::{
        CHUNKING_STRATEGY_VERSION, SEMANTIC_SCHEMA_VERSION, SemanticPolicy,
    };
    use crate::search::semantic_manifest::{
        ArtifactRecord, BuildCheckpoint, SemanticManifest, TierKind,
    };
    use serde_json::{Value, json};
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[derive(Debug)]
    struct SemanticReplayState {
        temp_dir: TempDir,
        loaded: Option<SemanticManifest>,
    }

    impl SemanticReplayState {
        fn data_dir(&self) -> &Path {
            self.temp_dir.path()
        }
    }

    fn semantic_checkpoint() -> BuildCheckpoint {
        BuildCheckpoint {
            tier: TierKind::Fast,
            embedder_id: "fnv1a-384".to_string(),
            last_offset: 8,
            docs_embedded: 13,
            conversations_processed: 2,
            total_conversations: 5,
            db_fingerprint: "semantic-fp".to_string(),
            schema_version: SEMANTIC_SCHEMA_VERSION,
            chunking_version: CHUNKING_STRATEGY_VERSION,
            saved_at_ms: 1_700_000_000_000,
            last_message_id: None,
        }
    }

    fn semantic_artifact() -> ArtifactRecord {
        ArtifactRecord {
            tier: TierKind::Fast,
            embedder_id: "fnv1a-384".to_string(),
            model_revision: "hash".to_string(),
            schema_version: SEMANTIC_SCHEMA_VERSION,
            chunking_version: CHUNKING_STRATEGY_VERSION,
            dimension: 384,
            doc_count: 13,
            conversation_count: 5,
            db_fingerprint: "semantic-fp".to_string(),
            index_path: "vector_index/fast.fsvi".to_string(),
            size_bytes: 4096,
            started_at_ms: 1_700_000_000_000,
            completed_at_ms: 1_700_000_060_000,
            ready: true,
        }
    }

    #[test]
    fn semantic_manifest_state_machine_replays_checkpoint_and_publish_crashes() {
        let checkpoints = vec![
            CrashReplayCheckpoint::new(
                10,
                "semantic_checkpoint_saved",
                "semantic checkpoint persisted before artifact publish",
            ),
            CrashReplayCheckpoint::new(
                20,
                "semantic_artifact_published",
                "semantic artifact published and checkpoint cleared",
            ),
        ];

        let report =
            replay_named_checkpoints(
                "semantic-manifest-save-restart",
                "semantic_manifest",
                checkpoints,
                || {
                    Ok(SemanticReplayState {
                        temp_dir: tempfile::tempdir()
                            .map_err(|err| CrashReplayError::from_error("create tempdir", err))?,
                        loaded: None,
                    })
                },
                |state, checkpoint| {
                    let mut manifest = SemanticManifest::default();
                    manifest.refresh_backlog(5, "semantic-fp");
                    manifest.save_checkpoint(semantic_checkpoint());
                    if checkpoint.id == "semantic_artifact_published" {
                        manifest.publish_artifact(semantic_artifact());
                    }
                    manifest
                        .save(state.data_dir())
                        .map_err(|err| CrashReplayError::from_error("save semantic manifest", err))
                },
                |state| {
                    state.loaded = SemanticManifest::load(state.data_dir()).map_err(|err| {
                        CrashReplayError::from_error("load semantic manifest", err)
                    })?;
                    Ok(())
                },
                |state, checkpoint| {
                    let mut invariants = Vec::new();
                    let Some(manifest) = &state.loaded else {
                        return vec![CrashReplayInvariant::failed(
                            checkpoint,
                            "semantic_manifest_loaded",
                            "manifest did not load after restart",
                        )];
                    };

                    invariants.push(CrashReplayInvariant::passed(
                        checkpoint,
                        "semantic_manifest_loaded",
                        "manifest loaded after restart",
                    ));
                    match checkpoint.id.as_str() {
                        "semantic_checkpoint_saved" => {
                            invariants.push(if manifest.checkpoint.is_some()
                            && manifest.fast_tier.is_none()
                        {
                            CrashReplayInvariant::passed(
                                checkpoint,
                                "checkpoint_without_torn_artifact",
                                "restart sees resumable checkpoint and no half-published artifact",
                            )
                        } else {
                            CrashReplayInvariant::failed(
                                checkpoint,
                                "checkpoint_without_torn_artifact",
                                format!(
                                    "checkpoint={:?} fast_tier={:?}",
                                    manifest.checkpoint, manifest.fast_tier
                                ),
                            )
                        });
                        }
                        "semantic_artifact_published" => {
                            invariants.push(if manifest.checkpoint.is_none()
                            && manifest.fast_tier.as_ref().is_some_and(|artifact| artifact.ready)
                        {
                            CrashReplayInvariant::passed(
                                checkpoint,
                                "published_artifact_clears_checkpoint",
                                "restart sees ready artifact and no stale matching checkpoint",
                            )
                        } else {
                            CrashReplayInvariant::failed(
                                checkpoint,
                                "published_artifact_clears_checkpoint",
                                format!(
                                    "checkpoint={:?} fast_tier={:?}",
                                    manifest.checkpoint, manifest.fast_tier
                                ),
                            )
                        });
                        }
                        _ => invariants.push(CrashReplayInvariant::failed(
                            checkpoint,
                            "known_checkpoint",
                            "unexpected semantic checkpoint",
                        )),
                    }
                    invariants
                },
            );

        assert_eq!(report.verdict, CrashReplayVerdict::Clean);
        assert_eq!(report.checkpoints.len(), 2);
        assert_eq!(report.invariants.len(), 4);
        assert!(
            report.validate().is_ok(),
            "semantic replay report should validate: {report:?}"
        );
    }

    #[derive(Debug)]
    struct PolicyReplayState {
        pipeline: Value,
        semantic_available: bool,
        semantic_fallback_mode: Option<&'static str>,
        snapshot_statuses: Vec<(String, PolicyControllerStatus, PolicyFallbackState)>,
    }

    fn policy_pipeline_fixture(mode: &str, reason: &str) -> Value {
        json!({
            "pipeline_channel_size": 128,
            "pipeline_max_message_bytes_in_flight": 1048576,
            "page_prep_workers": 12,
            "staged_merge_workers": 4,
            "staged_shard_builders": 8,
            "controller_mode": "auto",
            "controller_restore_clear_samples": 3,
            "controller_restore_hold_ms": 5000,
            "controller_loadavg_high_watermark_1m": 1.75,
            "controller_loadavg_low_watermark_1m": 0.75,
            "runtime": {
                "controller_mode": mode,
                "controller_reason": reason
            }
        })
    }

    #[test]
    fn policy_registry_state_machine_replays_deterministic_controller_snapshots() {
        let checkpoints = vec![
            CrashReplayCheckpoint::new(
                10,
                "semantic_fallback_snapshot",
                "semantic controller reports lexical fallback",
            ),
            CrashReplayCheckpoint::new(
                20,
                "lexical_throttle_snapshot",
                "lexical rebuild controller reports pressure fallback",
            ),
        ];

        let report = replay_named_checkpoints(
            "policy-registry-recompute-restart",
            "policy_registry",
            checkpoints,
            || {
                Ok(PolicyReplayState {
                    pipeline: policy_pipeline_fixture("steady", "pipeline settings active"),
                    semantic_available: true,
                    semantic_fallback_mode: None,
                    snapshot_statuses: Vec::new(),
                })
            },
            |state, checkpoint| {
                match checkpoint.id.as_str() {
                    "semantic_fallback_snapshot" => {
                        state.semantic_available = false;
                        state.semantic_fallback_mode = Some("lexical");
                    }
                    "lexical_throttle_snapshot" => {
                        state.pipeline =
                            policy_pipeline_fixture("throttled", "load pressure reduced workers");
                    }
                    _ => {
                        return Err(CrashReplayError::new(
                            "advance policy checkpoint",
                            "unknown checkpoint",
                        ));
                    }
                }
                Ok(())
            },
            |state| {
                let policy = SemanticPolicy::compiled_defaults();
                let snapshot = policy_registry_snapshot(
                    &policy,
                    state.semantic_available,
                    state.semantic_fallback_mode,
                    &state.pipeline,
                );
                state.snapshot_statuses = snapshot
                    .controllers
                    .into_iter()
                    .map(|controller| {
                        (
                            controller.controller_id,
                            controller.status,
                            controller.fallback_state,
                        )
                    })
                    .collect();
                Ok(())
            },
            |state, checkpoint| {
                let ids: Vec<_> = state
                    .snapshot_statuses
                    .iter()
                    .map(|(id, _, _)| id.as_str())
                    .collect();
                let mut invariants =
                    vec![if ids == ["lexical_rebuild_pipeline", "semantic_search"] {
                        CrashReplayInvariant::passed(
                            checkpoint,
                            "controller_ids_sorted",
                            "controller ids are deterministic and sorted",
                        )
                    } else {
                        CrashReplayInvariant::failed(
                            checkpoint,
                            "controller_ids_sorted",
                            format!("unexpected controller ids: {ids:?}"),
                        )
                    }];

                let expected_controller = match checkpoint.id.as_str() {
                    "semantic_fallback_snapshot" => "semantic_search",
                    "lexical_throttle_snapshot" => "lexical_rebuild_pipeline",
                    _ => "unknown",
                };
                let controller = state
                    .snapshot_statuses
                    .iter()
                    .find(|(id, _, _)| id == expected_controller);
                invariants.push(match controller {
                    Some((
                        _id,
                        PolicyControllerStatus::Fallback,
                        PolicyFallbackState::Conservative,
                    )) => CrashReplayInvariant::passed(
                        checkpoint,
                        "conservative_fallback_reported",
                        "checkpoint recompute reports conservative fallback",
                    ),
                    other => CrashReplayInvariant::failed(
                        checkpoint,
                        "conservative_fallback_reported",
                        format!("unexpected controller status: {other:?}"),
                    ),
                });
                invariants
            },
        );

        assert_eq!(report.verdict, CrashReplayVerdict::Clean);
        assert!(
            report.validate().is_ok(),
            "policy replay report should validate: {report:?}"
        );
    }

    #[derive(Debug)]
    struct LexicalPublishFixtureState {
        temp_dir: TempDir,
        live_path: PathBuf,
        staged_path: PathBuf,
        backup_path: PathBuf,
    }

    impl LexicalPublishFixtureState {
        fn new() -> Result<Self, CrashReplayError> {
            let temp_dir = tempfile::tempdir()
                .map_err(|err| CrashReplayError::from_error("create tempdir", err))?;
            let live_path = temp_dir.path().join("live-generation.txt");
            let staged_path = temp_dir.path().join("staged-generation.txt");
            let backup_path = temp_dir.path().join("live-generation.bak");
            fs::write(&live_path, "old-generation")
                .map_err(|err| CrashReplayError::from_error("seed live generation", err))?;
            Ok(Self {
                temp_dir,
                live_path,
                staged_path,
                backup_path,
            })
        }

        fn write_staged(&self) -> Result<(), CrashReplayError> {
            fs::write(&self.staged_path, "new-generation")
                .map_err(|err| CrashReplayError::from_error("write staged generation", err))
        }

        fn park_live(&self) -> Result<(), CrashReplayError> {
            fs::rename(&self.live_path, &self.backup_path)
                .map_err(|err| CrashReplayError::from_error("park live generation", err))
        }

        fn publish_staged(&self) -> Result<(), CrashReplayError> {
            fs::rename(&self.staged_path, &self.live_path)
                .map_err(|err| CrashReplayError::from_error("publish staged generation", err))
        }
    }

    #[test]
    fn lexical_publish_fixture_replays_park_and_swap_crash_windows() {
        let checkpoints = vec![
            CrashReplayCheckpoint::new(
                10,
                "staged_written",
                "staged generation exists before live path is touched",
            ),
            CrashReplayCheckpoint::new(
                20,
                "live_parked",
                "live generation has been parked but staged is not yet live",
            ),
            CrashReplayCheckpoint::new(
                30,
                "staged_published",
                "staged generation has been promoted to live",
            ),
        ];

        let report = replay_named_checkpoints(
            "lexical-publish-fixture-restart",
            "lexical_publish",
            checkpoints,
            LexicalPublishFixtureState::new,
            |state, checkpoint| {
                state.write_staged()?;
                match checkpoint.id.as_str() {
                    "staged_written" => {}
                    "live_parked" => {
                        state.park_live()?;
                    }
                    "staged_published" => {
                        state.park_live()?;
                        state.publish_staged()?;
                    }
                    _ => {
                        return Err(CrashReplayError::new(
                            "advance lexical publish checkpoint",
                            "unknown checkpoint",
                        ));
                    }
                }
                Ok(())
            },
            |state| {
                if !state.live_path.exists() && state.backup_path.exists() {
                    fs::rename(&state.backup_path, &state.live_path)
                        .map_err(|err| CrashReplayError::from_error("restore parked live", err))?;
                }
                Ok(())
            },
            |state, checkpoint| {
                let live = fs::read_to_string(&state.live_path).ok();
                let expected = match checkpoint.id.as_str() {
                    "staged_written" | "live_parked" => "old-generation",
                    "staged_published" => "new-generation",
                    _ => "unknown",
                };

                vec![
                    if state.temp_dir.path().exists() {
                        CrashReplayInvariant::passed(
                            checkpoint,
                            "fixture_root_retained",
                            "fixture root remains available for artifact inspection",
                        )
                    } else {
                        CrashReplayInvariant::failed(
                            checkpoint,
                            "fixture_root_retained",
                            "fixture root disappeared before invariant checks",
                        )
                    },
                    if live.as_deref() == Some(expected) {
                        CrashReplayInvariant::passed(
                            checkpoint,
                            "live_generation_is_old_or_new",
                            format!("live generation recovered as {expected}"),
                        )
                    } else {
                        CrashReplayInvariant::failed(
                            checkpoint,
                            "live_generation_is_old_or_new",
                            format!("expected {expected}, got {live:?}"),
                        )
                    },
                ]
            },
        );

        assert_eq!(report.verdict, CrashReplayVerdict::Clean);
        assert!(
            report.validate().is_ok(),
            "lexical publish replay report should validate: {report:?}"
        );
    }

    #[derive(Debug)]
    struct BackupRecoveryFixtureState {
        temp_dir: TempDir,
        canonical_db: PathBuf,
        backup_dir: PathBuf,
        manifest: Option<Value>,
    }

    impl BackupRecoveryFixtureState {
        fn new() -> Result<Self, CrashReplayError> {
            let temp_dir = tempfile::tempdir()
                .map_err(|err| CrashReplayError::from_error("create tempdir", err))?;
            let canonical_db = temp_dir.path().join("cass.db");
            let backup_dir = temp_dir.path().join("backup");
            fs::write(&canonical_db, "canonical-main")
                .map_err(|err| CrashReplayError::from_error("seed canonical db", err))?;
            fs::write(temp_dir.path().join("cass.db-wal"), "canonical-wal")
                .map_err(|err| CrashReplayError::from_error("seed canonical wal", err))?;
            fs::create_dir_all(&backup_dir)
                .map_err(|err| CrashReplayError::from_error("create backup dir", err))?;
            Ok(Self {
                temp_dir,
                canonical_db,
                backup_dir,
                manifest: None,
            })
        }

        fn copy_main(&self) -> Result<(), CrashReplayError> {
            fs::copy(&self.canonical_db, self.backup_dir.join("cass.db"))
                .map(|_| ())
                .map_err(|err| CrashReplayError::from_error("copy backup main", err))
        }

        fn copy_wal_and_manifest(&self) -> Result<(), CrashReplayError> {
            fs::copy(
                self.temp_dir.path().join("cass.db-wal"),
                self.backup_dir.join("cass.db-wal"),
            )
            .map_err(|err| CrashReplayError::from_error("copy backup wal", err))?;
            let manifest = json!({
                "schema_version": 1,
                "complete": true,
                "files": ["cass.db", "cass.db-wal"],
            });
            let bytes = serde_json::to_vec_pretty(&manifest)
                .map_err(|err| CrashReplayError::from_error("encode backup manifest", err))?;
            fs::write(self.backup_dir.join("manifest.json"), bytes)
                .map_err(|err| CrashReplayError::from_error("write backup manifest", err))
        }
    }

    #[test]
    fn backup_recovery_fixture_replays_incomplete_and_complete_bundle_crashes() {
        let checkpoints = vec![
            CrashReplayCheckpoint::new(
                10,
                "backup_main_copied",
                "backup main file copied before bundle manifest exists",
            ),
            CrashReplayCheckpoint::new(
                20,
                "backup_manifest_written",
                "backup sidecars and manifest mark the bundle complete",
            ),
        ];

        let report = replay_named_checkpoints(
            "backup-recovery-fixture-restart",
            "backup_recovery",
            checkpoints,
            BackupRecoveryFixtureState::new,
            |state, checkpoint| {
                state.copy_main()?;
                match checkpoint.id.as_str() {
                    "backup_main_copied" => {}
                    "backup_manifest_written" => {
                        state.copy_wal_and_manifest()?;
                    }
                    _ => {
                        return Err(CrashReplayError::new(
                            "advance backup recovery checkpoint",
                            "unknown checkpoint",
                        ));
                    }
                }
                Ok(())
            },
            |state| {
                let manifest_path = state.backup_dir.join("manifest.json");
                state.manifest = if manifest_path.exists() {
                    let bytes = fs::read(&manifest_path)
                        .map_err(|err| CrashReplayError::from_error("read backup manifest", err))?;
                    Some(serde_json::from_slice(&bytes).map_err(|err| {
                        CrashReplayError::from_error("parse backup manifest", err)
                    })?)
                } else {
                    None
                };
                Ok(())
            },
            |state, checkpoint| {
                let canonical = fs::read_to_string(&state.canonical_db).ok();
                let mut invariants = vec![if canonical.as_deref() == Some("canonical-main") {
                    CrashReplayInvariant::passed(
                        checkpoint,
                        "canonical_db_preserved",
                        "restart did not replace the canonical DB from an incomplete backup",
                    )
                } else {
                    CrashReplayInvariant::failed(
                        checkpoint,
                        "canonical_db_preserved",
                        format!("unexpected canonical DB content: {canonical:?}"),
                    )
                }];

                match checkpoint.id.as_str() {
                    "backup_main_copied" => {
                        invariants.push(if state.manifest.is_none() {
                            CrashReplayInvariant::passed(
                                checkpoint,
                                "partial_backup_not_marked_complete",
                                "main-only backup has no manifest and is not advertised recoverable",
                            )
                        } else {
                            CrashReplayInvariant::failed(
                                checkpoint,
                                "partial_backup_not_marked_complete",
                                format!("unexpected manifest: {:?}", state.manifest),
                            )
                        });
                    }
                    "backup_manifest_written" => {
                        let complete = state
                            .manifest
                            .as_ref()
                            .and_then(|manifest| manifest.get("complete"))
                            .and_then(Value::as_bool)
                            == Some(true);
                        let files_match = state
                            .manifest
                            .as_ref()
                            .and_then(|manifest| manifest.get("files"))
                            .and_then(Value::as_array)
                            .map(|files| {
                                let mut names = files.iter().filter_map(Value::as_str);
                                matches!(
                                    (names.next(), names.next(), names.next()),
                                    (Some("cass.db"), Some("cass.db-wal"), None)
                                )
                            })
                            == Some(true);
                        let wal_exists = state.backup_dir.join("cass.db-wal").exists();
                        invariants.push(if complete && files_match && wal_exists {
                            CrashReplayInvariant::passed(
                                checkpoint,
                                "complete_backup_manifest_matches_sidecars",
                                "complete manifest is present only with expected sidecars",
                            )
                        } else {
                            CrashReplayInvariant::failed(
                                checkpoint,
                                "complete_backup_manifest_matches_sidecars",
                                format!(
                                    "complete={complete} files_match={files_match} wal_exists={wal_exists}"
                                ),
                            )
                        });
                    }
                    _ => invariants.push(CrashReplayInvariant::failed(
                        checkpoint,
                        "known_backup_checkpoint",
                        "unexpected backup checkpoint",
                    )),
                }
                invariants
            },
        );

        assert_eq!(report.verdict, CrashReplayVerdict::Clean);
        assert!(
            report.validate().is_ok(),
            "backup recovery replay report should validate: {report:?}"
        );
    }

    #[test]
    fn crash_replay_report_round_trips_as_artifact_manifest()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = tempfile::tempdir()?;
        let path = temp_dir
            .path()
            .join("artifacts/crash-replay/crash-replay-report.json");
        let checkpoints = vec![CrashReplayCheckpoint::new(
            1,
            "only_checkpoint",
            "single checkpoint for artifact round-trip",
        )];
        let report = replay_named_checkpoints(
            "artifact-round-trip",
            "harness",
            checkpoints,
            || Ok(()),
            |_state, _checkpoint| Ok(()),
            |_state| Ok(()),
            |_state, checkpoint| {
                vec![CrashReplayInvariant::passed(
                    checkpoint,
                    "round_trip_invariant",
                    "round-trip invariant passed",
                )]
            },
        );

        report.save_json(&path)?;
        let loaded = CrashReplayReport::load_json(&path)?;

        assert_eq!(loaded, report);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn crash_replay_report_save_json_replaces_existing_symlink_without_following()
    -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::symlink;

        let temp_dir = tempfile::tempdir()?;
        let outside_dir = tempfile::tempdir()?;
        let report_dir = temp_dir.path().join("artifacts/crash-replay");
        fs::create_dir_all(&report_dir)?;
        let path = report_dir.join("crash-replay-report.json");
        let protected_target = outside_dir.path().join("protected-report.json");
        fs::write(&protected_target, "untouched")?;
        symlink(&protected_target, &path)?;

        let checkpoints = vec![CrashReplayCheckpoint::new(
            1,
            "only_checkpoint",
            "single checkpoint for symlink replacement",
        )];
        let report = replay_named_checkpoints(
            "symlink-replacement",
            "harness",
            checkpoints,
            || Ok(()),
            |_state, _checkpoint| Ok(()),
            |_state| Ok(()),
            |_state, checkpoint| {
                vec![CrashReplayInvariant::passed(
                    checkpoint,
                    "symlink_invariant",
                    "symlink replacement invariant passed",
                )]
            },
        );

        report.save_json(&path)?;

        assert_eq!(
            fs::read_to_string(&protected_target)?,
            "untouched",
            "save_json must replace the report-path symlink, not follow it"
        );
        assert!(
            !fs::symlink_metadata(&path)?.file_type().is_symlink(),
            "report path should become a regular JSON file"
        );
        assert_eq!(CrashReplayReport::load_json(&path)?, report);
        Ok(())
    }

    #[test]
    fn crash_replay_validation_rejects_untrustworthy_clean_reports() {
        let checkpoint = CrashReplayCheckpoint::new(1, "checkpoint", "validation checkpoint");
        let report = CrashReplayReport {
            schema_version: CRASH_REPLAY_SCHEMA_VERSION.to_string(),
            scenario_id: "bad-clean-report".to_string(),
            state_machine: "harness".to_string(),
            verdict: CrashReplayVerdict::Clean,
            checkpoints: vec![checkpoint.clone()],
            events: vec![CrashReplayEvent {
                checkpoint_id: checkpoint.id.clone(),
                phase: CrashReplayPhase::CheckInvariants,
                ok: true,
                detail: "checked".to_string(),
            }],
            invariants: vec![CrashReplayInvariant::failed(
                &checkpoint,
                "must_not_fail",
                "intentional validation failure",
            )],
        };

        assert!(matches!(
            report.validate(),
            Err(CrashReplayValidationError::CleanReportContainsFailure)
        ));

        let duplicate_checkpoint = CrashReplayCheckpoint {
            ordinal: 2,
            ..checkpoint.clone()
        };
        let duplicate_report = CrashReplayReport {
            checkpoints: vec![checkpoint.clone(), duplicate_checkpoint],
            ..report.clone()
        };
        assert!(matches!(
            duplicate_report.validate(),
            Err(CrashReplayValidationError::DuplicateCheckpointId { .. })
        ));

        let missing_check_event_report = CrashReplayReport {
            events: vec![CrashReplayEvent {
                checkpoint_id: checkpoint.id.clone(),
                phase: CrashReplayPhase::AdvanceToCheckpoint,
                ok: true,
                detail: "advanced".to_string(),
            }],
            invariants: vec![CrashReplayInvariant::passed(
                &checkpoint,
                "passing_but_unchecked",
                "invariant exists but no check event proves it ran",
            )],
            ..report
        };
        assert!(matches!(
            missing_check_event_report.validate(),
            Err(CrashReplayValidationError::CleanReportMissingCheckpointEvent { .. })
        ));
    }
}
