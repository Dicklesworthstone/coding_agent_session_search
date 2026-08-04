//! Progress JSONL sink for quality semantic backfill.
//!
//! When `CASS_SEMANTIC_PROGRESS_JSONL=/abs/path/to/file.jsonl` is set,
//! the semantic backfill code path appends one JSON object per transition
//! event to that file. Each event carries a timestamp, a phase + sub-phase,
//! a row/batch counter where applicable, the wall-time delta since the
//! sink was started, and a cheap RSS estimate.
//!
//! Goal — give operators enough proof, during long-running quality semantic
//! backfill runs, to tell whether time is going to selection, packet
//! replay, embedding, staging, checkpoint, or publish; and to distinguish
//! storage-side stalls from model-inference stalls. See cass#257.
//!
//! Env-var family: matches the existing `CASS_SEMANTIC_*` namespace (see
//! `src/search/policy.rs` and `src/indexer/semantic.rs`). The sink itself
//! is silent when the env var is unset, so it has zero cost for normal
//! operation. Writes are best-effort: a failed write is logged at debug
//! and never propagated upward — we never want telemetry to crash a
//! backfill that would otherwise succeed.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// Env var that activates the sink and names the output file.
pub const ENV_PROGRESS_JSONL: &str = "CASS_SEMANTIC_PROGRESS_JSONL";

/// Schema version for the JSONL event stream. Bump on any
/// breaking change to event names or fields.
pub const PROGRESS_JSONL_SCHEMA: &str = "cass.semantic.progress.v1";

/// The 20 named transition events. Strings deliberately mirror the
/// `phase` + `sub_phase` columns in each emitted record so a `jq` user
/// can filter on event name OR phase as they prefer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticProgressEvent {
    /// Backfill is about to materialize the message-selection query.
    SelectionStart,
    /// Backfill is about to resolve the stable canonical conversation total.
    SelectionCountStart,
    /// The stable canonical conversation total is available, either from the
    /// current checkpoint/manifest or from a database aggregate.
    SelectionCountDone,
    /// Backfill is about to run the bounded cursor candidate query.
    SelectionCandidatesStart,
    /// The bounded cursor candidate query returned.
    SelectionCandidatesDone,
    /// Selection finished; downstream knows how many candidate rows
    /// will be considered for this batch.
    SelectionDone,
    /// Canonical packet replay is about to begin (envelope fetch +
    /// per-conversation message materialization + packet build).
    PacketReplayStart,
    /// Periodic per-conversation tick during packet replay so a
    /// stuck conversation does not look like a stuck model.
    PacketReplayProgress,
    /// Packet replay finished — `EmbeddingInput`s are ready.
    PacketReplayDone,
    /// About to call `embedder.embed_batch_sync` for a single batch.
    EmbedBatchStart,
    /// `embedder.embed_batch_sync` returned for this batch.
    EmbedBatchDone,
    /// About to write the embedded vectors into the staging index.
    StagingWriteStart,
    /// Staging write returned.
    StagingWriteDone,
    /// About to fsync the updated manifest with this batch's checkpoint.
    CheckpointSaveStart,
    /// Manifest fsync returned.
    CheckpointSaveDone,
    /// About to atomically rename the staged index into the published
    /// index path (only fires on the batch that completes the tier).
    PublishStart,
    /// Publish rename + fsync done; tier is queryable.
    PublishDone,
    /// Backfill aborted with an error.
    Error,
    /// Backfill cancelled cooperatively (signal, idle-yield, etc).
    Cancelled,
    /// All work finished cleanly (terminal — emitted exactly once per
    /// run, after publish_done or in the no-op path).
    Complete,
}

impl SemanticProgressEvent {
    /// Stable snake_case string for the event field. Used both as the
    /// JSONL `event` value and (with `phase()`) as a discriminator in
    /// downstream consumers.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SelectionStart => "selection_start",
            Self::SelectionCountStart => "selection_count_start",
            Self::SelectionCountDone => "selection_count_done",
            Self::SelectionCandidatesStart => "selection_candidates_start",
            Self::SelectionCandidatesDone => "selection_candidates_done",
            Self::SelectionDone => "selection_done",
            Self::PacketReplayStart => "packet_replay_start",
            Self::PacketReplayProgress => "packet_replay_progress",
            Self::PacketReplayDone => "packet_replay_done",
            Self::EmbedBatchStart => "embed_batch_start",
            Self::EmbedBatchDone => "embed_batch_done",
            Self::StagingWriteStart => "staging_write_start",
            Self::StagingWriteDone => "staging_write_done",
            Self::CheckpointSaveStart => "checkpoint_save_start",
            Self::CheckpointSaveDone => "checkpoint_save_done",
            Self::PublishStart => "publish_start",
            Self::PublishDone => "publish_done",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
            Self::Complete => "complete",
        }
    }

    /// Coarse phase classification, useful to a downstream `jq` consumer
    /// that wants to bucket time across selection / replay / embed /
    /// staging / checkpoint / publish without enumerating every event.
    pub fn phase(self) -> &'static str {
        match self {
            Self::SelectionStart
            | Self::SelectionCountStart
            | Self::SelectionCountDone
            | Self::SelectionCandidatesStart
            | Self::SelectionCandidatesDone
            | Self::SelectionDone => "selection",
            Self::PacketReplayStart | Self::PacketReplayProgress | Self::PacketReplayDone => {
                "packet_replay"
            }
            Self::EmbedBatchStart | Self::EmbedBatchDone => "embed",
            Self::StagingWriteStart | Self::StagingWriteDone => "staging",
            Self::CheckpointSaveStart | Self::CheckpointSaveDone => "checkpoint",
            Self::PublishStart | Self::PublishDone => "publish",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
            Self::Complete => "complete",
        }
    }

    /// `start` / `done` / `progress` / single (sub_phase=`event`).
    pub fn sub_phase(self) -> &'static str {
        match self {
            Self::SelectionStart
            | Self::SelectionCountStart
            | Self::SelectionCandidatesStart
            | Self::PacketReplayStart
            | Self::EmbedBatchStart
            | Self::StagingWriteStart
            | Self::CheckpointSaveStart
            | Self::PublishStart => "start",
            Self::SelectionDone
            | Self::SelectionCountDone
            | Self::SelectionCandidatesDone
            | Self::PacketReplayDone
            | Self::EmbedBatchDone
            | Self::StagingWriteDone
            | Self::CheckpointSaveDone
            | Self::PublishDone => "done",
            Self::PacketReplayProgress => "progress",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
            Self::Complete => "complete",
        }
    }
}

/// Optional counters carried by an event. Every field is `None` when
/// not applicable — JSON serializers should skip nulls so the row stays
/// readable.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SemanticProgressFields {
    /// Batch index within this backfill run, when applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batch_index: Option<u64>,
    /// Rows in the current batch, when applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batch_rows: Option<u64>,
    /// Cumulative rows processed so far, when applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows_processed: Option<u64>,
    /// Total rows expected (best-effort).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows_total: Option<u64>,
    /// Conversation cursor (per-tier semantic) at this event.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_conversation_id: Option<i64>,
    /// Message PK cursor at this event.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_message_id: Option<i64>,
    /// Conversations in the active batch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversations_in_batch: Option<u64>,
    /// Free-form context note. Kept short — long context belongs in a
    /// debug log line, not in a high-frequency JSONL event.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Bytes touched (e.g. content bytes selected, bytes embedded,
    /// staged write size). Lets operators distinguish a stalled query
    /// from a stalled model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    /// Free-form error string when the event is `error`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct EventRecord<'a> {
    schema: &'static str,
    event: &'static str,
    phase: &'static str,
    sub_phase: &'static str,
    /// Unix milliseconds, wall clock.
    ts_ms: i64,
    /// Milliseconds since this sink was opened.
    elapsed_ms: u64,
    /// Tier label (`fast` / `quality` / `unknown`).
    tier: &'a str,
    /// Embedder id (e.g. `minilm-384`, `hash`).
    embedder_id: &'a str,
    /// Cheap RSS estimate in MiB (None if /proc parse fails or the
    /// platform doesn't expose it).
    #[serde(skip_serializing_if = "Option::is_none")]
    rss_mib: Option<u64>,
    #[serde(flatten)]
    fields: &'a SemanticProgressFields,
}

/// Process-pid, used only for cross-correlation when an operator
/// concatenates JSONL files from multiple runs.
fn current_pid() -> u32 {
    std::process::id()
}

/// Wall-clock Unix milliseconds at the moment of the call.
fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

/// Cheap RSS estimate from /proc/self/status (Linux). Returns None on
/// other platforms or any parse failure. Reading /proc/self/status is
/// a cheap pseudo-file read — safe to call inside the embed batch loop.
fn read_rss_mib() -> Option<u64> {
    let bytes = std::fs::read("/proc/self/status").ok()?;
    let text = std::str::from_utf8(&bytes).ok()?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            // Expected format: `VmRSS:    12345 kB`
            let mut parts = rest.split_whitespace();
            let kb_str = parts.next()?;
            let kb: u64 = kb_str.parse().ok()?;
            return Some(kb / 1024);
        }
    }
    None
}

/// Resolve the sink's destination path from the env var.
fn resolve_path() -> Option<PathBuf> {
    let raw = dotenvy::var(ENV_PROGRESS_JSONL).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(PathBuf::from(trimmed))
}

/// Open the sink file (append, create) on first event, cache the
/// handle in a Mutex. We deliberately accept the cost of a Mutex over
/// every event because the JSONL stream is several events per batch,
/// not per row — even a 50ms batch wall-time dwarfs the lock cost.
pub struct SemanticProgressSink {
    inner: Option<Mutex<SinkInner>>,
    tier: String,
    embedder_id: String,
    started: Instant,
}

struct SinkInner {
    file: File,
    /// Cached so we can include it in `complete`/`error` log lines.
    path: PathBuf,
    /// True after we've written at least one record successfully —
    /// lets us suppress repeat "failed to write" warnings.
    healthy: bool,
}

impl SemanticProgressSink {
    /// Open a sink for the given tier+embedder. Returns a no-op sink
    /// when the env var is unset, so callers can always emit events
    /// unconditionally without branching.
    pub fn open(tier: &str, embedder_id: &str) -> Self {
        let path = resolve_path();
        let inner = match path {
            Some(p) => match Self::open_file(&p) {
                Ok(file) => Some(Mutex::new(SinkInner {
                    file,
                    path: p,
                    healthy: false,
                })),
                Err(err) => {
                    tracing::warn!(
                        path = %p.display(),
                        error = %err,
                        "CASS_SEMANTIC_PROGRESS_JSONL: failed to open sink — continuing without progress JSONL",
                    );
                    None
                }
            },
            None => None,
        };
        Self {
            inner,
            tier: tier.to_string(),
            embedder_id: embedder_id.to_string(),
            started: Instant::now(),
        }
    }

    /// Sink that never writes — kept as an explicit factory so callers
    /// can default to a sink without consulting the env var (e.g. tests
    /// that don't care about telemetry).
    pub fn disabled() -> Self {
        Self {
            inner: None,
            tier: "unknown".to_string(),
            embedder_id: "unknown".to_string(),
            started: Instant::now(),
        }
    }

    /// True if the sink is actively writing (env var set + file
    /// opened). Callers can branch on this to skip building expensive
    /// `SemanticProgressFields` when no one will read them.
    pub fn is_active(&self) -> bool {
        self.inner.is_some()
    }

    fn open_file(path: &Path) -> std::io::Result<File> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        OpenOptions::new().create(true).append(true).open(path)
    }

    /// Emit one event. Best-effort: a write failure logs at debug and
    /// returns Ok — telemetry never bubbles errors into the backfill.
    pub fn emit(&self, event: SemanticProgressEvent, fields: SemanticProgressFields) {
        let Some(mutex) = self.inner.as_ref() else {
            return;
        };
        let elapsed_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let rss_mib = read_rss_mib();
        let record = EventRecord {
            schema: PROGRESS_JSONL_SCHEMA,
            event: event.as_str(),
            phase: event.phase(),
            sub_phase: event.sub_phase(),
            ts_ms: now_unix_ms(),
            elapsed_ms,
            tier: self.tier.as_str(),
            embedder_id: self.embedder_id.as_str(),
            rss_mib,
            fields: &fields,
        };
        let mut line = match serde_json::to_string(&record) {
            Ok(s) => s,
            Err(err) => {
                tracing::debug!(
                    ?err,
                    event = event.as_str(),
                    "skip JSONL emit: serialize failed"
                );
                return;
            }
        };
        line.push('\n');
        // Best-effort write under lock. We intentionally do not propagate
        // errors — a backfill that succeeded but couldn't write telemetry
        // is still a successful backfill.
        let mut guard = match mutex.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Err(err) = guard.file.write_all(line.as_bytes()) {
            if guard.healthy {
                // Surface once on transition healthy→sick to help
                // operators notice (e.g. disk full mid-run).
                tracing::warn!(
                    path = %guard.path.display(),
                    error = %err,
                    "CASS_SEMANTIC_PROGRESS_JSONL: write failed after previous successes; continuing without progress JSONL",
                );
                guard.healthy = false;
            } else {
                tracing::debug!(
                    path = %guard.path.display(),
                    error = %err,
                    "CASS_SEMANTIC_PROGRESS_JSONL: write failed",
                );
            }
        } else {
            guard.healthy = true;
            // We do NOT fsync per-event — sync at end is the operator's
            // job (e.g. shutdown drain). Per-event fsync would dominate
            // wall time on a long run. The file is opened append, so
            // partial writes are tolerable to the reader.
        }
    }

    /// Convenience: emit an event with no extra fields.
    pub fn emit_bare(&self, event: SemanticProgressEvent) {
        self.emit(event, SemanticProgressFields::default());
    }

    /// Process-pid for cross-correlation. Stable for the life of the sink.
    pub fn pid(&self) -> u32 {
        current_pid()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::sync::Mutex;
    use tempfile::TempDir;

    // env vars are process-global; serialize tests so concurrent
    // cargo test runs don't trample each other's env state.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn read_lines(path: &Path) -> Vec<String> {
        let f = File::open(path).expect("open jsonl");
        std::io::BufReader::new(f)
            .lines()
            .map_while(Result::ok)
            .collect()
    }

    #[test]
    fn disabled_sink_is_noop() {
        let sink = SemanticProgressSink::disabled();
        assert!(!sink.is_active());
        sink.emit_bare(SemanticProgressEvent::SelectionStart);
        // No panic = pass.
    }

    #[test]
    fn unset_env_is_noop() {
        let _guard = ENV_LOCK.lock().unwrap();
        // SAFETY: tests are serialized via ENV_LOCK; this is the
        // standard pattern in this crate for env-dependent tests.
        unsafe {
            std::env::remove_var(ENV_PROGRESS_JSONL);
        }
        let sink = SemanticProgressSink::open("quality", "minilm-384");
        assert!(!sink.is_active());
        sink.emit_bare(SemanticProgressEvent::SelectionStart);
    }

    #[test]
    fn writes_one_line_per_event() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("progress.jsonl");
        // SAFETY: tests are serialized via ENV_LOCK.
        unsafe {
            std::env::set_var(ENV_PROGRESS_JSONL, &path);
        }
        let sink = SemanticProgressSink::open("quality", "minilm-384");
        assert!(sink.is_active());
        sink.emit_bare(SemanticProgressEvent::SelectionStart);
        sink.emit(
            SemanticProgressEvent::EmbedBatchDone,
            SemanticProgressFields {
                batch_index: Some(3),
                batch_rows: Some(128),
                rows_processed: Some(384),
                ..Default::default()
            },
        );
        sink.emit_bare(SemanticProgressEvent::Complete);
        drop(sink);

        let lines = read_lines(&path);
        assert_eq!(lines.len(), 3, "expected 3 events; got {:?}", lines);
        assert!(
            lines[0].contains("\"event\":\"selection_start\""),
            "line 0: {}",
            lines[0]
        );
        assert!(
            lines[1].contains("\"event\":\"embed_batch_done\""),
            "line 1: {}",
            lines[1]
        );
        assert!(
            lines[1].contains("\"batch_index\":3"),
            "line 1: {}",
            lines[1]
        );
        assert!(
            lines[2].contains("\"event\":\"complete\""),
            "line 2: {}",
            lines[2]
        );
        // SAFETY: tests serialized via ENV_LOCK.
        unsafe {
            std::env::remove_var(ENV_PROGRESS_JSONL);
        }
    }

    #[test]
    fn each_event_has_phase_and_sub_phase() {
        use SemanticProgressEvent::*;
        let all = [
            SelectionStart,
            SelectionCountStart,
            SelectionCountDone,
            SelectionCandidatesStart,
            SelectionCandidatesDone,
            SelectionDone,
            PacketReplayStart,
            PacketReplayProgress,
            PacketReplayDone,
            EmbedBatchStart,
            EmbedBatchDone,
            StagingWriteStart,
            StagingWriteDone,
            CheckpointSaveStart,
            CheckpointSaveDone,
            PublishStart,
            PublishDone,
            Error,
            Cancelled,
            Complete,
        ];
        assert_eq!(all.len(), 20);
        for event in all {
            assert!(!event.as_str().is_empty(), "{:?}", event);
            assert!(!event.phase().is_empty(), "{:?}", event);
            assert!(!event.sub_phase().is_empty(), "{:?}", event);
        }
    }

    #[test]
    fn invalid_env_var_is_safe_noop() {
        let _guard = ENV_LOCK.lock().unwrap();
        // Whitespace-only env var should be treated as unset rather
        // than as an attempt to write to "" (which would fail). The
        // sink should silently degrade to disabled.
        // SAFETY: tests serialized via ENV_LOCK.
        unsafe {
            std::env::set_var(ENV_PROGRESS_JSONL, "   ");
        }
        let sink = SemanticProgressSink::open("quality", "minilm-384");
        assert!(!sink.is_active());
        // SAFETY: tests serialized via ENV_LOCK.
        unsafe {
            std::env::remove_var(ENV_PROGRESS_JSONL);
        }
    }

    // -----------------------------------------------------------------
    // .5.2: agent-facing progress-sink acceptance — ordering, required
    // fields, failure events, schema stability, best-effort write failure.
    // -----------------------------------------------------------------

    /// Emit one record for `event` to a fresh sink and return its parsed
    /// JSON. Serializes env access via `ENV_LOCK`.
    fn one_record(
        event: SemanticProgressEvent,
        fields: SemanticProgressFields,
    ) -> serde_json::Value {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("progress.jsonl");
        // SAFETY: tests serialized via ENV_LOCK.
        unsafe {
            std::env::set_var(ENV_PROGRESS_JSONL, &path);
        }
        let sink = SemanticProgressSink::open("quality", "minilm-384");
        sink.emit(event, fields);
        drop(sink);
        let lines = read_lines(&path);
        // SAFETY: tests serialized via ENV_LOCK.
        unsafe {
            std::env::remove_var(ENV_PROGRESS_JSONL);
        }
        assert_eq!(lines.len(), 1, "expected exactly one record");
        serde_json::from_str(&lines[0]).expect("record is valid JSON")
    }

    #[test]
    fn full_backfill_lifecycle_emits_events_in_phase_order() {
        use SemanticProgressEvent::*;
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("progress.jsonl");
        // SAFETY: tests serialized via ENV_LOCK.
        unsafe {
            std::env::set_var(ENV_PROGRESS_JSONL, &path);
        }
        let sink = SemanticProgressSink::open("quality", "minilm-384");
        // The canonical #257 backfill lifecycle.
        let sequence = [
            SelectionStart,
            SelectionCountStart,
            SelectionCountDone,
            SelectionCandidatesStart,
            SelectionCandidatesDone,
            SelectionDone,
            PacketReplayStart,
            PacketReplayProgress,
            PacketReplayDone,
            EmbedBatchStart,
            EmbedBatchDone,
            StagingWriteStart,
            StagingWriteDone,
            CheckpointSaveStart,
            CheckpointSaveDone,
            PublishStart,
            PublishDone,
            Complete,
        ];
        for ev in sequence {
            sink.emit_bare(ev);
        }
        drop(sink);

        let lines = read_lines(&path);
        // SAFETY: tests serialized via ENV_LOCK.
        unsafe {
            std::env::remove_var(ENV_PROGRESS_JSONL);
        }
        assert_eq!(lines.len(), sequence.len(), "one line per emitted event");
        let emitted: Vec<String> = lines
            .iter()
            .map(|l| {
                serde_json::from_str::<serde_json::Value>(l).unwrap()["event"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        let expected: Vec<String> = sequence.iter().map(|e| e.as_str().to_string()).collect();
        assert_eq!(emitted, expected, "events must persist in emit order");
        assert_eq!(emitted.first().unwrap(), "selection_start");
        assert_eq!(emitted.last().unwrap(), "complete");
    }

    #[test]
    fn every_record_carries_the_required_stable_fields() {
        let v = one_record(
            SemanticProgressEvent::EmbedBatchDone,
            SemanticProgressFields {
                batch_index: Some(1),
                rows_processed: Some(64),
                ..Default::default()
            },
        );
        for key in [
            "schema",
            "event",
            "phase",
            "sub_phase",
            "ts_ms",
            "elapsed_ms",
            "tier",
            "embedder_id",
        ] {
            assert!(
                v.get(key).is_some(),
                "record missing required field {key}: {v}"
            );
        }
        assert_eq!(v["event"], "embed_batch_done");
        assert_eq!(v["phase"], "embed");
        assert_eq!(v["tier"], "quality");
        assert_eq!(v["embedder_id"], "minilm-384");
    }

    #[test]
    fn failure_events_serialize_with_their_detail() {
        let err = one_record(
            SemanticProgressEvent::Error,
            SemanticProgressFields {
                error: Some("embed batch OOM".to_string()),
                ..Default::default()
            },
        );
        assert_eq!(err["event"], "error");
        assert_eq!(err["phase"], "error");
        assert_eq!(err["error"], "embed batch OOM");

        let cancelled = one_record(
            SemanticProgressEvent::Cancelled,
            SemanticProgressFields {
                note: Some("operator interrupt".to_string()),
                ..Default::default()
            },
        );
        assert_eq!(cancelled["event"], "cancelled");
        assert_eq!(cancelled["note"], "operator interrupt");
    }

    #[test]
    fn jsonl_schema_version_is_pinned_and_present_in_records() {
        // Pin the schema string so any wire-format change is a deliberate,
        // reviewed break (the .5.2 "schema remains stable" requirement).
        assert_eq!(PROGRESS_JSONL_SCHEMA, "cass.semantic.progress.v1");
        let v = one_record(
            SemanticProgressEvent::SelectionStart,
            SemanticProgressFields::default(),
        );
        assert_eq!(v["schema"], "cass.semantic.progress.v1");
    }

    #[test]
    fn open_failure_degrades_to_disabled_without_panic() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = TempDir::new().unwrap();
        // Point the sink at a path whose PARENT is a regular file, so
        // create_dir_all (and the open) fail: the sink must degrade to a
        // disabled no-op, and emitting must not panic (best-effort).
        let file_as_parent = dir.path().join("not-a-dir");
        std::fs::write(&file_as_parent, b"x").unwrap();
        let bad_path = file_as_parent.join("progress.jsonl");
        // SAFETY: tests serialized via ENV_LOCK.
        unsafe {
            std::env::set_var(ENV_PROGRESS_JSONL, &bad_path);
        }
        let sink = SemanticProgressSink::open("quality", "minilm-384");
        assert!(!sink.is_active(), "open failure must degrade to disabled");
        // Emitting against the degraded sink is a safe no-op.
        sink.emit_bare(SemanticProgressEvent::SelectionStart);
        sink.emit(
            SemanticProgressEvent::Error,
            SemanticProgressFields {
                error: Some("ignored".to_string()),
                ..Default::default()
            },
        );
        // SAFETY: tests serialized via ENV_LOCK.
        unsafe {
            std::env::remove_var(ENV_PROGRESS_JSONL);
        }
    }
}
