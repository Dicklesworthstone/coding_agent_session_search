//! Optional raw-mirror maintenance and bounded capture reporting for indexing.
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{RawMirrorCaptureInput, RawMirrorCapturePaused, RawMirrorCaptureRecord};
use crate::search::semantic_manifest::{persist_named_temp, sync_parent_directory};

const CAPTURE_REPORT_FILE: &str = "raw-mirror-capture.json";
const CAPTURE_REPORT_MAX_BYTES: u64 = 8192;
const CAPTURE_REPORT_INTERVAL: Duration = Duration::from_secs(30);
const CAPTURE_SOURCE_LIMIT: usize = 65_536;
const CAPTURE_HINT: &str = "Sessions can still be indexed without a new raw copy. Review CASS_RAW_MIRROR_MAX_BYTES / CASS_RAW_MIRROR_MIN_FREE_BYTES, free space, or inspect cass mirror prune (dry-run first). Requested reservation bytes count refused attempts, including retries; they are not source-file bytes or unique missing-copy bytes.";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CaptureBudget {
    used_bytes: u64,
    max_bytes: u64,
    available_bytes: u64,
    min_free_bytes: u64,
    requested_bytes: u64,
}

impl From<RawMirrorCapturePaused> for CaptureBudget {
    fn from(paused: RawMirrorCapturePaused) -> Self {
        Self {
            used_bytes: paused.used_bytes,
            max_bytes: paused.max_bytes,
            available_bytes: paused.available_bytes,
            min_free_bytes: paused.min_free_bytes,
            requested_bytes: paused.requested_bytes,
        }
    }
}

/// A small observation receipt, never an assertion about current admission or
/// missing-copy coverage. `completed` means this accounting guard was finalized;
/// it does not imply that indexing succeeded. A killed process leaves the most
/// recent checkpoint, which may omit the final thirty seconds of refusals.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CaptureReceipt {
    schema_version: u32,
    run_id: String,
    started_at_ms: i64,
    updated_at_ms: i64,
    finished_at_ms: Option<i64>,
    completed: bool,
    capture_enabled: bool,
    max_bytes: Option<u64>,
    min_free_bytes: Option<u64>,
    capture_paused: bool,
    pause_attempts: u64,
    skipped_sources: u64,
    skipped_sources_exact: bool,
    requested_reservation_bytes: u64,
    counters_exact: bool,
    last_pause: Option<CaptureBudget>,
}

impl CaptureReceipt {
    fn valid(&self) -> bool {
        self.schema_version == 1
            && !self.run_id.is_empty()
            && self.run_id.len() <= 96
            && self
                .run_id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte == b'-')
            && self.started_at_ms >= 0
            && self.updated_at_ms >= self.started_at_ms
            && self.completed == self.finished_at_ms.is_some()
            && self
                .finished_at_ms
                .is_none_or(|finished| finished >= self.updated_at_ms)
            && self.max_bytes.is_some() == self.min_free_bytes.is_some()
            && self.capture_paused == (self.pause_attempts > 0)
            && self.last_pause.is_some() == self.capture_paused
            && (self.capture_enabled || !self.capture_paused)
            && self.skipped_sources <= self.pause_attempts
            && self.skipped_sources <= CAPTURE_SOURCE_LIMIT as u64
            && (self.capture_paused == (self.skipped_sources > 0))
            && (self.capture_paused || self.requested_reservation_bytes == 0)
    }
}

struct CaptureRunState {
    receipt: CaptureReceipt,
    // Only digests live in memory. Neither source identities nor their hashes
    // belong in the on-disk receipt or in stderr diagnostics.
    sources: HashSet<[u8; 32]>,
    last_write: Option<Instant>,
    write_failure_reported: bool,
}

impl CaptureRunState {
    fn new() -> Self {
        static NEXT_RUN: AtomicU64 = AtomicU64::new(0);
        let now = chrono::Utc::now().timestamp_millis();
        let policy = super::RawMirrorRetentionPolicy::from_env().ok();
        Self {
            receipt: CaptureReceipt {
                schema_version: 1,
                run_id: format!(
                    "{now}-{}-{}",
                    std::process::id(),
                    NEXT_RUN.fetch_add(1, Ordering::Relaxed)
                ),
                started_at_ms: now,
                updated_at_ms: now,
                finished_at_ms: None,
                completed: false,
                capture_enabled: super::capture_enabled(),
                max_bytes: policy.map(|policy| policy.max_bytes),
                min_free_bytes: policy.map(|policy| policy.min_free_bytes),
                capture_paused: false,
                pause_attempts: 0,
                skipped_sources: 0,
                skipped_sources_exact: true,
                requested_reservation_bytes: 0,
                counters_exact: true,
                last_pause: None,
            },
            sources: HashSet::new(),
            last_write: None,
            write_failure_reported: false,
        }
    }

    fn observe(&mut self, identity: [u8; 32], paused: RawMirrorCapturePaused) {
        let receipt = &mut self.receipt;
        receipt.capture_paused = true;
        receipt.updated_at_ms = chrono::Utc::now()
            .timestamp_millis()
            .max(receipt.updated_at_ms);
        receipt.pause_attempts = match receipt.pause_attempts.checked_add(1) {
            Some(value) => value,
            None => {
                receipt.counters_exact = false;
                u64::MAX
            }
        };
        receipt.requested_reservation_bytes = match receipt
            .requested_reservation_bytes
            .checked_add(paused.requested_bytes)
        {
            Some(value) => value,
            None => {
                receipt.counters_exact = false;
                u64::MAX
            }
        };
        if !self.sources.contains(&identity) {
            if self.sources.len() < CAPTURE_SOURCE_LIMIT {
                self.sources.insert(identity);
                receipt.skipped_sources = self.sources.len() as u64;
            } else {
                receipt.skipped_sources_exact = false;
            }
        }
        receipt.last_pause = Some(paused.into());
    }

    fn persist(&mut self, data_dir: &Path) {
        // Failed writes are throttled too: an unavailable report must not turn
        // a refused source into thousands of attempted writes and warnings.
        self.last_write = Some(Instant::now());
        if write_capture_receipt(data_dir, &self.receipt).is_err() && !self.write_failure_reported {
            self.write_failure_reported = true;
            emit_capture_event(json!({
                "event": "raw_mirror_capture_report_unavailable",
                "reason": "capture receipt could not be persisted; status may show an older or unavailable observation",
                "hint": "Indexing continues; inspect the data directory's permissions and free space."
            }));
        }
    }
}

type CaptureRun = Arc<Mutex<CaptureRunState>>;
type CaptureRuns = Mutex<HashMap<PathBuf, Weak<Mutex<CaptureRunState>>>>;

fn capture_runs() -> &'static CaptureRuns {
    static RUNS: OnceLock<CaptureRuns> = OnceLock::new();
    RUNS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn capture_run_key(data_dir: &Path) -> PathBuf {
    if data_dir.is_absolute() {
        data_dir.to_path_buf()
    } else {
        std::env::current_dir().map_or_else(|_| data_dir.to_path_buf(), |cwd| cwd.join(data_dir))
    }
}

fn active_capture_run(data_dir: &Path) -> Option<CaptureRun> {
    capture_runs()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&capture_run_key(data_dir))
        .and_then(Weak::upgrade)
}

/// Holds one index run's bounded accounting. Create this only after acquiring
/// the archive's index lease so a competing process cannot reset its receipt.
pub(crate) struct CaptureRunGuard {
    data_dir: PathBuf,
    state: Option<CaptureRun>,
}

pub(crate) fn begin_capture_run(data_dir: &Path) -> CaptureRunGuard {
    let key = capture_run_key(data_dir);
    let mut runs = capture_runs()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // A nested caller joins the existing observer; only the original owner
    // finalizes it. Normally the index lease already prevents this situation.
    if runs.get(&key).and_then(Weak::upgrade).is_some() {
        return CaptureRunGuard {
            data_dir: key,
            state: None,
        };
    }
    runs.retain(|_, state| state.strong_count() > 0);
    let state = Arc::new(Mutex::new(CaptureRunState::new()));
    runs.insert(key.clone(), Arc::downgrade(&state));
    drop(runs);
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .persist(&key);
    CaptureRunGuard {
        data_dir: key,
        state: Some(state),
    }
}

fn emit_capture_event(event: Value) {
    // Reporting remains optional even when stderr was closed by its reader.
    let _ = writeln!(std::io::stderr().lock(), "{event}");
}

impl Drop for CaptureRunGuard {
    fn drop(&mut self) {
        let Some(state) = self.state.take() else {
            return;
        };
        let mut runs = capture_runs()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if runs
            .get(&self.data_dir)
            .and_then(Weak::upgrade)
            .is_some_and(|current| Arc::ptr_eq(&state, &current))
        {
            runs.remove(&self.data_dir);
        }
        drop(runs);
        let mut state = state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.receipt.updated_at_ms = chrono::Utc::now()
            .timestamp_millis()
            .max(state.receipt.updated_at_ms);
        state.receipt.finished_at_ms = Some(state.receipt.updated_at_ms);
        state.receipt.completed = true;
        state.persist(&self.data_dir);
        if state.receipt.capture_paused {
            emit_capture_event(json!({
                "event": "raw_mirror_capture_summary",
                "raw_mirror_capture": state.receipt,
                "hint": CAPTURE_HINT
            }));
        }
    }
}

pub(super) fn observe_capture_result(
    input: &RawMirrorCaptureInput<'_>,
    result: &anyhow::Result<RawMirrorCaptureRecord>,
) {
    let Some(paused) = result
        .as_ref()
        .err()
        .and_then(|error| error.downcast_ref::<RawMirrorCapturePaused>())
    else {
        return;
    };
    let Some(state) = active_capture_run(input.data_dir) else {
        return;
    };
    let mut identity = blake3::Hasher::new();
    for component in [
        input.provider.as_bytes(),
        input.source_id.as_bytes(),
        input.source_path.as_os_str().as_encoded_bytes(),
    ] {
        identity.update(&(component.len() as u64).to_le_bytes());
        identity.update(component);
    }
    let mut state = state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if state.receipt.completed {
        return;
    }
    let first = !state.receipt.capture_paused;
    state.observe(*identity.finalize().as_bytes(), *paused);
    if first
        || state
            .last_write
            .is_none_or(|last| last.elapsed() >= CAPTURE_REPORT_INTERVAL)
    {
        state.persist(&capture_run_key(input.data_dir));
    }
    if first {
        emit_capture_event(json!({
            "event": "raw_mirror_capture_paused",
            "raw_mirror_capture": state.receipt,
            "hint": CAPTURE_HINT
        }));
    }
}

/// Suppress only a budget refusal already owned by this run's observer.
/// Standalone capture operations and other failures retain their diagnostics.
pub(crate) fn capture_pause_is_summarized(data_dir: &Path, error: &anyhow::Error) -> bool {
    error.downcast_ref::<RawMirrorCapturePaused>().is_some()
        && active_capture_run(data_dir).is_some_and(|state| {
            let state = state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.receipt.capture_paused && !state.receipt.completed
        })
}

fn write_capture_receipt(data_dir: &Path, receipt: &CaptureReceipt) -> std::io::Result<()> {
    let path = data_dir.join(CAPTURE_REPORT_FILE);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if !metadata.is_file() => return Err(std::io::ErrorKind::InvalidData.into()),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    // The index owner has already created the data directory. Do not create a
    // mirror tree, and keep this receipt outside the raw-mirror byte budget.
    let mut temporary = tempfile::NamedTempFile::new_in(data_dir)?;
    serde_json::to_writer(&mut temporary, receipt).map_err(std::io::Error::other)?;
    temporary.write_all(b"\n")?;
    temporary.as_file().sync_all()?;
    let published = persist_named_temp(temporary, &path, true)?;
    published.sync_all()?;
    sync_parent_directory(&path)
}

fn read_capture_receipt(data_dir: &Path) -> Result<CaptureReceipt, &'static str> {
    let path = data_dir.join(CAPTURE_REPORT_FILE);
    let before = fs::symlink_metadata(&path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            "missing"
        } else {
            "unreadable"
        }
    })?;
    if !before.is_file() || before.len() > CAPTURE_REPORT_MAX_BYTES {
        return Err("invalid");
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    let file = options.open(&path).map_err(|_| "unreadable")?;
    let metadata = file.metadata().map_err(|_| "unreadable")?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > CAPTURE_REPORT_MAX_BYTES
    {
        return Err("invalid");
    }
    let mut bytes = Vec::new();
    file.take(CAPTURE_REPORT_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "unreadable")?;
    if bytes.len() as u64 > CAPTURE_REPORT_MAX_BYTES {
        return Err("invalid");
    }
    let receipt: CaptureReceipt = serde_json::from_slice(&bytes).map_err(|_| "invalid")?;
    if !receipt.valid() {
        return Err("invalid");
    }
    Ok(receipt)
}

/// Cheap read-only observation for status/health: one small receipt, no mirror
/// inventory, database opening, free-space probe, or filesystem mutation.
pub(crate) fn capture_status(data_dir: &Path) -> Value {
    let capture_enabled = super::capture_enabled();
    let policy = super::RawMirrorRetentionPolicy::from_env().ok();
    let (receipt_status, receipt) = match read_capture_receipt(data_dir) {
        Ok(receipt) => ("recorded", Some(receipt)),
        Err(reason) => (reason, None),
    };
    let status = if !capture_enabled {
        "capture_disabled"
    } else if policy.is_none() {
        "configuration_invalid"
    } else {
        match receipt.as_ref() {
            None => "unknown",
            Some(receipt) if receipt.capture_paused => "last_run_paused",
            Some(receipt) if !receipt.capture_enabled || receipt.max_bytes.is_none() => "unknown",
            Some(receipt) if !receipt.completed => "last_run_incomplete",
            Some(_) => "last_run_no_pause_observed",
        }
    };
    json!({
        "status": status,
        "capture_enabled": capture_enabled,
        "configuration": {
            "status": if policy.is_some() { "valid" } else { "invalid" },
            "max_bytes": policy.map(|policy| policy.max_bytes),
            "min_free_bytes": policy.map(|policy| policy.min_free_bytes)
        },
        "receipt_status": receipt_status,
        "last_run": receipt
    })
}

pub(crate) fn capture_status_warning(status: &Value) -> Option<String> {
    if status["capture_enabled"] == false {
        return Some(
            "Raw-mirror capture is disabled (CASS_RAW_MIRROR); new source copies are not retained."
                .to_owned(),
        );
    }
    if status["configuration"]["status"] == "invalid" {
        return Some("Raw-mirror capture configuration is invalid; check CASS_RAW_MIRROR_MAX_BYTES and CASS_RAW_MIRROR_MIN_FREE_BYTES.".to_owned());
    }
    let receipt = &status["last_run"];
    if receipt["capture_paused"] != true {
        return None;
    }
    let qualifier = if receipt["skipped_sources_exact"] != true || receipt["completed"] != true {
        "at least "
    } else {
        ""
    };
    let counter_qualifier = if receipt["counters_exact"] != true || receipt["completed"] != true {
        "at least "
    } else {
        ""
    };
    let incomplete = if receipt["completed"] != true {
        " (last observation is incomplete)"
    } else {
        ""
    };
    Some(format!(
        "Raw-mirror capture paused in the last recorded run{incomplete}: {qualifier}{} sources, {counter_qualifier}{} refused attempts, {counter_qualifier}{} requested reservation bytes. Current admission was not probed. Review CASS_RAW_MIRROR_MAX_BYTES / CASS_RAW_MIRROR_MIN_FREE_BYTES.",
        receipt["skipped_sources"],
        receipt["pause_attempts"],
        receipt["requested_reservation_bytes"]
    ))
}

fn validate_maintenance_archive(data_dir: &Path, db_path: &Path) -> anyhow::Result<()> {
    // The existing keep-tag query intentionally addresses agent_search.db.
    // Do not consult a different archive's pins for a custom database basename.
    let absolute = |path: &Path| -> std::io::Result<PathBuf> {
        if path.is_absolute() {
            Ok(path.to_path_buf())
        } else {
            Ok(std::env::current_dir()?.join(path))
        }
    };
    let actual = absolute(db_path)?;
    let expected = absolute(&data_dir.join("agent_search.db"))?;
    let same = actual == expected
        || (super::store::raw_mirror_io("archive_exists", &actual, || actual.try_exists())?
            && super::store::raw_mirror_io("archive_exists", &expected, || expected.try_exists())?
            && super::store::raw_mirror_io("canonicalize_archive", &actual, || {
                std::fs::canonicalize(&actual)
            })? == super::store::raw_mirror_io("canonicalize_archive", &expected, || {
                std::fs::canonicalize(&expected)
            })?);
    if !same {
        anyhow::bail!(
            "automatic raw-mirror retention skipped: a custom --db requires explicit mirror pruning so keep-tag pins are not read from the wrong archive"
        );
    }
    Ok(())
}

/// Called by the index owner after tracing, progress observation and its
/// watchdog have started. The concrete index-run lease prevents a second
/// acquisition of the same lock while preserving index-before-mirror ordering.
/// Optional failure never blocks indexing; the caller still checks cancellation
/// after return.
pub(crate) fn maintain_for_index(
    data_dir: &Path,
    db_path: &Path,
    index_lease: &crate::indexer::IndexRunLockGuard,
) {
    let result = validate_maintenance_archive(data_dir, db_path)
        .and_then(|()| super::store::policy::maintain_while_index_owned(data_dir, index_lease));
    // JSON on stderr keeps robot stdout data-only during preflight.
    match result {
        Ok(report) if report.retired_manifests > 0 || report.capture_paused => {
            eprintln!(
                "{}",
                serde_json::json!({
                    "raw_mirror_maintenance": report,
                    "hint": "Indexing retains its normal disk-safety checks. If capture remains paused, free space or review cass mirror prune (dry-run first); protected evidence and hold-downs are never overridden."
                })
            );
        }
        Ok(_) => {}
        Err(error) => {
            eprintln!(
                "{}",
                serde_json::json!({
                    "raw_mirror_maintenance": {"status": "skipped", "reason": error.to_string()},
                    "hint": "Indexing will still attempt its normal disk preflight. Review cass mirror prune and CASS_RAW_MIRROR_MAX_BYTES / CASS_RAW_MIRROR_MIN_FREE_BYTES; no protected evidence was intentionally discarded."
                })
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paused(requested_bytes: u64) -> RawMirrorCapturePaused {
        RawMirrorCapturePaused {
            used_bytes: 8_000,
            max_bytes: 8_192,
            available_bytes: 10_000,
            min_free_bytes: 100,
            requested_bytes,
        }
    }

    fn refuse(data: &Path, provider: &str, source_id: &str, source: &Path, bytes: u64) {
        let input = RawMirrorCaptureInput {
            data_dir: data,
            provider,
            source_id,
            origin_kind: "local",
            origin_host: None,
            source_path: source,
            db_links: &[],
        };
        observe_capture_result(&input, &Err(paused(bytes).into()));
    }

    #[test]
    fn gh542_receipts_count_distinct_sources_and_attempts_without_disclosing_identities() {
        let data = tempfile::tempdir().unwrap();
        let source = data.path().join("private-source-marker.jsonl");
        let error = anyhow::Error::new(paused(7));
        assert!(!capture_pause_is_summarized(data.path(), &error));
        let guard = begin_capture_run(data.path());
        let initial = read_capture_receipt(data.path()).unwrap();
        assert!(!initial.completed);
        assert_eq!(initial.pause_attempts, 0);
        assert!(!capture_pause_is_summarized(data.path(), &error));
        refuse(
            data.path(),
            "private-provider",
            "private-origin",
            &source,
            7,
        );
        let first = read_capture_receipt(data.path()).unwrap();
        assert_eq!(
            first.pause_attempts, 1,
            "the first refusal is durable immediately"
        );
        assert!(!first.completed);
        assert!(capture_pause_is_summarized(data.path(), &error));
        assert!(!capture_pause_is_summarized(
            data.path(),
            &anyhow::anyhow!("unrelated failure")
        ));
        // The same physical source may be retried or belong to a different
        // provider/origin. Only retries of the complete identity are deduped.
        refuse(
            data.path(),
            "private-provider",
            "private-origin",
            &source,
            7,
        );
        refuse(
            data.path(),
            "private-provider-other",
            "private-origin",
            &source,
            11,
        );
        refuse(
            data.path(),
            "private-provider",
            "private-origin-other",
            &source,
            13,
        );
        refuse(
            data.path(),
            "private-provider",
            "private-origin",
            &source.with_extension("json"),
            17,
        );
        assert_eq!(
            read_capture_receipt(data.path()).unwrap().pause_attempts,
            1,
            "ordinary updates are throttled"
        );
        drop(guard);
        assert!(!capture_pause_is_summarized(data.path(), &error));
        let last = read_capture_receipt(data.path()).unwrap();
        assert!(last.completed);
        assert!(last.capture_paused);
        assert!(last.skipped_sources_exact && last.counters_exact);
        assert_eq!(last.pause_attempts, 5);
        assert_eq!(last.skipped_sources, 4);
        assert_eq!(last.requested_reservation_bytes, 55);
        assert_eq!(last.last_pause.unwrap().requested_bytes, 17);
        let bytes = fs::read_to_string(data.path().join(CAPTURE_REPORT_FILE)).unwrap();
        assert!(
            !bytes.contains("private-"),
            "source identities must not enter the receipt"
        );
        assert!(!data.path().join("raw-mirror").exists());
        let status = capture_status(data.path());
        assert_eq!(status["receipt_status"], "recorded");
        assert_eq!(status["last_run"]["skipped_sources"], 4);
        assert!(
            capture_status_warning(&status)
                .unwrap()
                .contains("requested reservation bytes")
        );
    }

    #[test]
    fn gh542_new_run_replaces_prior_observation_and_nested_guard_does_not_reset_it() {
        let data = tempfile::tempdir().unwrap();
        let source = data.path().join("source");
        let first = begin_capture_run(data.path());
        refuse(data.path(), "provider", "local", &source, 5);
        let nested = begin_capture_run(data.path());
        drop(nested);
        assert!(active_capture_run(data.path()).is_some());
        assert_eq!(read_capture_receipt(data.path()).unwrap().pause_attempts, 1);
        drop(first);
        let previous = read_capture_receipt(data.path()).unwrap();
        let next = begin_capture_run(data.path());
        let reset = read_capture_receipt(data.path()).unwrap();
        assert_ne!(reset.run_id, previous.run_id);
        assert!(!reset.completed);
        assert_eq!(reset.pause_attempts, 0);
        assert!(!reset.capture_paused);
        drop(next);
        assert!(read_capture_receipt(data.path()).unwrap().completed);
    }

    #[test]
    fn gh542_parallel_refusals_share_one_bounded_source_counter() {
        let data = tempfile::tempdir().unwrap();
        let guard = begin_capture_run(data.path());
        std::thread::scope(|scope| {
            for worker in 0..8 {
                let directory = data.path();
                scope.spawn(move || {
                    for _ in 0..20 {
                        refuse(
                            directory,
                            "provider",
                            "local",
                            &directory.join(format!("source-{worker}")),
                            3,
                        );
                    }
                });
            }
        });
        drop(guard);
        let receipt = read_capture_receipt(data.path()).unwrap();
        assert_eq!(receipt.pause_attempts, 160);
        assert_eq!(receipt.skipped_sources, 8);
        assert_eq!(receipt.requested_reservation_bytes, 480);
    }

    #[test]
    fn gh542_distinct_source_cap_reports_a_lower_bound_without_growing() {
        let mut state = CaptureRunState::new();
        for index in 0..CAPTURE_SOURCE_LIMIT + 3 {
            let mut identity = [0; 32];
            identity[..8].copy_from_slice(&(index as u64).to_le_bytes());
            state.observe(identity, paused(1));
        }
        assert_eq!(state.sources.len(), CAPTURE_SOURCE_LIMIT);
        assert_eq!(state.receipt.skipped_sources, CAPTURE_SOURCE_LIMIT as u64);
        assert!(!state.receipt.skipped_sources_exact);
        assert_eq!(
            state.receipt.pause_attempts,
            CAPTURE_SOURCE_LIMIT as u64 + 3
        );
        assert!(state.receipt.counters_exact);
        let encoded = serde_json::to_vec(&state.receipt).unwrap();
        assert!(
            encoded.len() < 1024,
            "receipt size is independent of source count"
        );
        state.receipt.requested_reservation_bytes = u64::MAX;
        state.observe([0; 32], paused(1));
        assert_eq!(state.receipt.requested_reservation_bytes, u64::MAX);
        assert!(!state.receipt.counters_exact);
    }

    #[test]
    fn gh542_status_is_read_only_and_missing_invalid_or_oversized_receipts_are_unknown() {
        let data = tempfile::tempdir().unwrap();
        let missing = data.path().join("not-created");
        let status = capture_status(&missing);
        assert_eq!(status["receipt_status"], "missing");
        assert!(status["last_run"].is_null());
        assert!(!missing.exists());
        let report = data.path().join(CAPTURE_REPORT_FILE);
        for invalid in [
            b"{}".to_vec(),
            vec![b' '; CAPTURE_REPORT_MAX_BYTES as usize + 1],
        ] {
            fs::write(&report, &invalid).unwrap();
            let status = capture_status(data.path());
            assert_eq!(status["receipt_status"], "invalid");
            assert!(status["last_run"].is_null());
            assert_eq!(fs::read(&report).unwrap(), invalid);
        }
    }

    #[test]
    fn gh542_receipt_write_failure_keeps_accounting_and_other_errors_visible() {
        let data = tempfile::tempdir().unwrap();
        fs::create_dir(data.path().join(CAPTURE_REPORT_FILE)).unwrap();
        let guard = begin_capture_run(data.path());
        let source = data.path().join("source");
        refuse(data.path(), "provider", "local", &source, 4);
        assert!(capture_pause_is_summarized(data.path(), &paused(4).into()));
        assert!(!capture_pause_is_summarized(
            data.path(),
            &super::super::RawMirrorCaptureDisabled.into()
        ));
        let state = active_capture_run(data.path()).unwrap();
        let state = state.lock().unwrap();
        assert!(state.write_failure_reported);
        assert_eq!(state.receipt.pause_attempts, 1);
        drop(state);
        drop(guard);
        assert_eq!(capture_status(data.path())["receipt_status"], "invalid");
    }

    #[cfg(unix)]
    #[test]
    fn gh542_status_refuses_symlink_and_fifo_receipts_without_opening_them() {
        let data = tempfile::tempdir().unwrap();
        let target = data.path().join("private-target");
        fs::write(&target, "private marker").unwrap();
        let symlink_dir = data.path().join("symlink");
        fs::create_dir(&symlink_dir).unwrap();
        std::os::unix::fs::symlink(&target, symlink_dir.join(CAPTURE_REPORT_FILE)).unwrap();
        assert_eq!(capture_status(&symlink_dir)["receipt_status"], "invalid");
        assert_eq!(fs::read_to_string(&target).unwrap(), "private marker");
        let fifo_dir = data.path().join("fifo");
        fs::create_dir(&fifo_dir).unwrap();
        let created = std::process::Command::new("mkfifo")
            .arg(fifo_dir.join(CAPTURE_REPORT_FILE))
            .status()
            .unwrap();
        assert!(created.success());
        assert_eq!(capture_status(&fifo_dir)["receipt_status"], "invalid");
    }

    #[test]
    fn explicit_data_directory_is_used_without_touching_the_default_archive() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("isolated");
        let db_path = directory.join("agent_search.db");
        validate_maintenance_archive(&directory, &db_path).unwrap();
        super::super::maintain_before_index(&directory).unwrap();
        assert!(
            !directory.exists(),
            "an absent mirror needs no maintenance writes"
        );
    }

    #[test]
    fn custom_database_name_cannot_read_pins_from_a_neighboring_archive() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("scratch.db");
        assert!(validate_maintenance_archive(temp.path(), &path).is_err());
        assert!(std::fs::read_dir(temp.path()).unwrap().next().is_none());
        let path = temp.path().join("agent_search.db");
        validate_maintenance_archive(temp.path(), &path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn archive_alias_is_allowed_only_when_it_resolves_to_the_same_pin_database() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("agent_search.db");
        let alias = temp.path().join("archive-alias.db");
        std::fs::write(&archive, []).unwrap();
        std::os::unix::fs::symlink(&archive, &alias).unwrap();
        validate_maintenance_archive(temp.path(), &alias).unwrap();
        let other = temp.path().join("another.db");
        std::fs::write(&other, []).unwrap();
        assert!(validate_maintenance_archive(temp.path(), &other).is_err());
    }
}
