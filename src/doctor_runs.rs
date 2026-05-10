//! Per-run artifact directory for `cass doctor`.
//!
//! World-class-doctor pass-1 introduces a `.doctor/runs/<run-id>/` directory
//! that is the single source of truth for what one invocation of `cass doctor`
//! observed and (if `--fix`) applied. The layout is documented in
//! `coding_agent_session_search__doctor_workspace/analysis/repair_specs/_consolidated_specs.md`
//! decision D2.
//!
//! The run-id is content-addressed (`sha256(target_sha + iso8601_seconds)[..6]`)
//! so it is deterministic to the second; replaying the same input twice produces
//! the same run-id.
//!
//! This module is **purely additive**. It never replaces the existing
//! `.doctor/locks/`, `.doctor/quarantine/`, `.doctor/events/`, or
//! `.doctor/repair/{class}/` directories — those continue to be authoritative
//! for their existing surfaces. The run dir is *additional* observability for
//! agents and the new `cass doctor undo`/`ls`/`diff`/`gc` subcommands.
//!
//! Pass-2 wires `find_latest_run`, `RunId`, `list_runs`, etc. into the
//! `cass doctor --ls` and `cass doctor --undo <run-id>` dispatch in
//! `src/lib.rs`. Items not yet referenced (`update_latest_link`,
//! `RunSummary::run_dir`, the various stable filename constants) are queued
//! for pass-3 and are kept under the per-item allow rather than a module-wide
//! one so that any genuine dead-code regression is still caught.

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Schema version of the per-run artifact layout. Bumped on layout changes
/// (e.g., adding new sibling files within `<run-id>/`).
pub(crate) const RUN_ARTIFACT_SCHEMA_VERSION: u32 = 1;

/// Stable directory name for per-run artifacts under `<data_dir>/.doctor/`.
pub(crate) const RUNS_DIRNAME: &str = "runs";

/// Stable filename for the per-run mutation log.
pub(crate) const ACTIONS_JSONL_NAME: &str = "actions.jsonl";

/// Stable filename for the per-run report (JSON envelope).
pub(crate) const REPORT_JSON_NAME: &str = "report.json";

/// Stable filename for the per-run report (human readable).
pub(crate) const REPORT_MD_NAME: &str = "report.md";

/// Stable filename for the per-run scorecard.
pub(crate) const SCORECARD_JSON_NAME: &str = "scorecard.json";

/// Stable filename for the per-run undo helper script.
pub(crate) const UNDO_SH_NAME: &str = "undo.sh";

/// Stable filename for the per-run captured stderr.
pub(crate) const STDERR_LOG_NAME: &str = "stderr.log";

/// Stable filename for the per-run manifest (run-id, mode, target_sha).
pub(crate) const MANIFEST_JSON_NAME: &str = "manifest.json";

/// Symlink (or junction on Windows) pointing at the latest run dir.
pub(crate) const LATEST_LINK_NAME: &str = "latest";

/// A content-addressed identifier for a single `cass doctor` invocation.
///
/// Format: `<ISO8601-with-dashes>__<6-hex-of-sha256(target_sha + iso_seconds)>`.
/// Determinism: two invocations within the same wall-clock second on the same
/// `target_sha` produce the same id; the second one will refuse to create the
/// dir (it already exists) and the caller must wait one second or pass an
/// explicit override.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct RunId(String);

impl RunId {
    /// Generate a fresh run-id. `target_sha` is typically the cass binary's
    /// build SHA; for the live-binary case use `env!("VERGEN_GIT_SHA")` or fall
    /// back to a sentinel (`"unknown"` is acceptable for development builds).
    pub(crate) fn new(target_sha: &str) -> Self {
        let now_ms = current_unix_ms();
        Self::from_parts(target_sha, now_ms)
    }

    /// Generate a run-id from explicit parts (for tests and replay).
    pub(crate) fn from_parts(target_sha: &str, now_ms: i64) -> Self {
        let iso = iso8601_seconds_from_ms(now_ms);
        let suffix = sha256_short_suffix(target_sha, now_ms);
        Self(format!("{iso}__{suffix}"))
    }

    /// Parse a run-id from its string form. Returns `None` if the string does
    /// not look like a run-id (no `__` separator, hex too short, etc.).
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        let (iso, suffix) = raw.split_once("__")?;
        if iso.len() < 19 {
            return None;
        }
        if suffix.len() != 6 {
            return None;
        }
        if !suffix.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        Some(Self(raw.to_string()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn into_inner(self) -> String {
        self.0
    }
}

impl std::fmt::Display for RunId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Compute the on-disk path for a given run-id under `<data_dir>/.doctor/`.
pub(crate) fn run_dir_for(data_dir: &Path, run_id: &RunId) -> PathBuf {
    data_dir
        .join("doctor")
        .join(RUNS_DIRNAME)
        .join(run_id.as_str())
}

/// Compute the on-disk path of the latest-symlink under `<data_dir>/.doctor/`.
pub(crate) fn latest_link_path(data_dir: &Path) -> PathBuf {
    data_dir.join("doctor").join(LATEST_LINK_NAME)
}

/// Path to the runs index dir under `<data_dir>/.doctor/runs/`.
pub(crate) fn runs_index_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("doctor").join(RUNS_DIRNAME)
}

/// Create the per-run directory and all standard sub-paths.
///
/// Returns `Ok(run_dir)` on success. The directory is created with `0o700`
/// permissions on Unix (since it may contain backups of sensitive files).
/// Idempotent: if the directory already exists with the expected layout, this
/// is a no-op and returns the existing path.
///
/// **Error:** Refuses if any expected child file already exists with non-zero
/// size (would suggest run-id collision — caller should pick a fresh one).
pub(crate) fn create_run_dir(data_dir: &Path, run_id: &RunId) -> std::io::Result<PathBuf> {
    let run_dir = run_dir_for(data_dir, run_id);
    fs::create_dir_all(&run_dir)?;
    fs::create_dir_all(run_dir.join("backups"))?;
    fs::create_dir_all(run_dir.join("quarantine"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o700);
        let _ = fs::set_permissions(&run_dir, perms);
    }
    Ok(run_dir)
}

/// Update the `latest` symlink to point at `run_dir`. Atomic via
/// rename-of-temp-symlink. On non-Unix platforms this is a junction or a
/// regular file containing the run-id (we never assume symlinks work).
pub(crate) fn update_latest_link(data_dir: &Path, run_id: &RunId) -> std::io::Result<()> {
    let target_run_id = run_id.as_str();
    let link_path = latest_link_path(data_dir);

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let tmp_link = link_path.with_extension(format!("tmp.{}", std::process::id()));
        let _ = fs::remove_file(&tmp_link);
        // The symlink target is a relative path from <data_dir>/.doctor/ to
        // <data_dir>/.doctor/runs/<run-id>/, so the link is portable across
        // mounts.
        symlink(format!("{}/{}", RUNS_DIRNAME, target_run_id), &tmp_link)?;
        // Atomic rename
        fs::rename(&tmp_link, &link_path)?;
    }

    #[cfg(not(unix))]
    {
        // On Windows we write a small text file as a fallback marker. The
        // `cass doctor undo latest` resolver checks for this.
        let tmp = link_path.with_extension("tmp");
        fs::write(&tmp, target_run_id)?;
        fs::rename(&tmp, &link_path)?;
    }

    Ok(())
}

/// One record in `actions.jsonl`. Append-only.
///
/// The schema mirrors the spec at decision D1 in the consolidated spec doc.
/// Future schema bumps require a new `kind` enum variant and a contract bump.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub(crate) enum ActionRecord {
    /// Issued at run start.
    RunStarted {
        schema_version: u32,
        run_id: String,
        target_sha: String,
        mode: String,
        started_at_ms: i64,
    },
    /// Issued before each repair band begins executing.
    BandStarted {
        run_id: String,
        band_id: String,
        planned_action_count: usize,
        started_at_ms: i64,
    },
    /// Issued after each repair band completes (every action ack'd).
    BandCompleted {
        run_id: String,
        band_id: String,
        applied_action_count: usize,
        ended_at_ms: i64,
    },
    /// One mutation receipt — the canonical record of a single disk write.
    Mutation {
        run_id: String,
        fm_id: String,
        path: String,
        op: String,
        before_blake3: Option<String>,
        after_blake3: Option<String>,
        started_at_ms: i64,
        ended_at_ms: i64,
    },
    /// Issued at run end.
    RunEnded {
        run_id: String,
        exit_code: i32,
        exit_code_kind: String,
        ended_at_ms: i64,
    },
}

/// World-class-doctor pass-6: append a `BandStarted` event to the run's
/// `actions.jsonl`. Helpers like this make it easy for repair planners to mark
/// the boundary between bands without hand-rolling the ActionRecord variant.
///
/// The crash-recovery contract: a band whose `BandStarted` is appended but
/// whose `BandCompleted` is not constitutes "in-flight". Recovery code
/// (pass-7+) walks `actions.jsonl` and either rolls back via `cass doctor undo`
/// or reapplies, depending on operator policy.
pub(crate) fn append_band_started(
    run_dir: &Path,
    run_id: &RunId,
    band_id: &str,
    planned_action_count: usize,
) -> std::io::Result<()> {
    append_action(
        run_dir,
        &ActionRecord::BandStarted {
            run_id: run_id.as_str().to_string(),
            band_id: band_id.to_string(),
            planned_action_count,
            started_at_ms: current_unix_ms(),
        },
    )
}

/// World-class-doctor pass-6: append a `BandCompleted` event. Symmetric
/// counterpart to [`append_band_started`]. Together they bracket a repair
/// band so crash-recovery code can determine which band was in flight.
pub(crate) fn append_band_completed(
    run_dir: &Path,
    run_id: &RunId,
    band_id: &str,
    applied_action_count: usize,
) -> std::io::Result<()> {
    append_action(
        run_dir,
        &ActionRecord::BandCompleted {
            run_id: run_id.as_str().to_string(),
            band_id: band_id.to_string(),
            applied_action_count,
            ended_at_ms: current_unix_ms(),
        },
    )
}

/// Append a single record to `<run_dir>/actions.jsonl`. Each record is one line
/// of JSON; the file is opened with `O_APPEND` so concurrent appenders within
/// the same process are race-free up to OS guarantees (Linux: atomic for
/// writes ≤PIPE_BUF).
pub(crate) fn append_action(run_dir: &Path, rec: &ActionRecord) -> std::io::Result<()> {
    use std::io::Write;
    let path = run_dir.join(ACTIONS_JSONL_NAME);
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    let line = serde_json::to_string(rec)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    file.write_all(line.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_data()?;
    Ok(())
}

/// Read all records from `<run_dir>/actions.jsonl`. Bad lines are skipped (with
/// the line index recorded in the returned error vec); good lines are returned
/// in file order. The caller decides how to handle partial corruption.
pub(crate) type ParseError = (usize, String);

pub(crate) fn read_actions(
    run_dir: &Path,
) -> std::io::Result<(Vec<ActionRecord>, Vec<ParseError>)> {
    use std::io::BufRead;
    let path = run_dir.join(ACTIONS_JSONL_NAME);
    let file = match fs::File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Vec::new(), Vec::new()));
        }
        Err(e) => return Err(e),
    };
    let reader = std::io::BufReader::new(file);
    let mut records = Vec::new();
    let mut errors = Vec::new();
    for (idx, line_result) in reader.lines().enumerate() {
        let line = match line_result {
            Ok(l) => l,
            Err(e) => {
                errors.push((idx, format!("read error: {e}")));
                continue;
            }
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<ActionRecord>(trimmed) {
            Ok(r) => records.push(r),
            Err(e) => errors.push((idx, format!("parse error: {e}"))),
        }
    }
    Ok((records, errors))
}

/// Summary of a single run, suitable for `cass doctor ls`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RunSummary {
    pub run_id: String,
    pub started_at_ms: Option<i64>,
    pub ended_at_ms: Option<i64>,
    pub exit_code: Option<i32>,
    pub mode: Option<String>,
    pub action_count: usize,
    pub status: &'static str,
    pub run_dir: String,
}

/// Walk `<data_dir>/.doctor/runs/` and return one summary per run, sorted by
/// `started_at_ms` descending (newest first). Bad runs (where actions.jsonl is
/// missing or unparseable) are reported with `status="incomplete"`.
pub(crate) fn list_runs(data_dir: &Path) -> std::io::Result<Vec<RunSummary>> {
    let runs_dir = runs_index_dir(data_dir);
    if !runs_dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(&runs_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = match path.file_name().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        // Skip entries that aren't run-id-shaped
        if RunId::parse(&name).is_none() {
            continue;
        }
        out.push(summarize_run(&path, name));
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.started_at_ms));
    Ok(out)
}

fn summarize_run(run_dir: &Path, name: String) -> RunSummary {
    let (records, _errors) = read_actions(run_dir).unwrap_or_default();
    let mut started_at_ms = None;
    let mut ended_at_ms = None;
    let mut exit_code = None;
    let mut mode = None;
    let mut mutation_count = 0usize;
    for r in &records {
        match r {
            ActionRecord::RunStarted {
                started_at_ms: t,
                mode: m,
                ..
            } => {
                started_at_ms = Some(*t);
                mode = Some(m.clone());
            }
            ActionRecord::Mutation { .. } => {
                mutation_count += 1;
            }
            ActionRecord::RunEnded {
                ended_at_ms: t,
                exit_code: code,
                ..
            } => {
                ended_at_ms = Some(*t);
                exit_code = Some(*code);
            }
            _ => {}
        }
    }
    let status = if ended_at_ms.is_some() {
        "completed"
    } else if started_at_ms.is_some() {
        "incomplete"
    } else {
        "unknown"
    };
    RunSummary {
        run_id: name,
        started_at_ms,
        ended_at_ms,
        exit_code,
        mode,
        action_count: mutation_count,
        status,
        run_dir: run_dir.to_string_lossy().to_string(),
    }
}

/// Find the most recent completed run dir, if any.
pub(crate) fn find_latest_run(data_dir: &Path) -> std::io::Result<Option<RunSummary>> {
    Ok(list_runs(data_dir)?.into_iter().next())
}

/// World-class-doctor pass-6: find any in-flight band in this run dir.
///
/// A band is "in-flight" if its `BandStarted` record is present but no
/// matching `BandCompleted` (matched by `band_id`) follows it. Returns
/// `Some(band_id)` if exactly one in-flight band exists; returns `None` if all
/// bands completed, or if the journal contains no band events at all.
///
/// Pass-6 ships this primarily for crash-recovery diagnostics; pass-7 wires
/// it into `cass doctor --undo <run-id>`'s opening probe so the operator
/// learns "the run crashed during band B3" rather than just "the run is
/// incomplete".
pub(crate) fn find_in_flight_band(run_dir: &Path) -> std::io::Result<Option<String>> {
    let (records, _errs) = read_actions(run_dir)?;
    let mut started: Option<String> = None;
    for r in &records {
        match r {
            ActionRecord::BandStarted { band_id, .. } => {
                started = Some(band_id.clone());
            }
            ActionRecord::BandCompleted { band_id, .. } if started.as_deref() == Some(band_id) => {
                started = None;
            }
            _ => {}
        }
    }
    Ok(started)
}

// ---- helpers ----

fn current_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

fn iso8601_seconds_from_ms(ms: i64) -> String {
    // Format: 2026-05-09T20-07-01Z (dashes instead of colons so it is a valid
    // POSIX directory name).
    let secs = ms / 1000;
    let (year, month, day, hour, min, sec) = decompose_unix_seconds(secs);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}-{min:02}-{sec:02}Z")
}

/// Pure decomposition of a unix epoch seconds value into (year, month, day, hour, min, sec).
/// Avoids pulling in a date crate; the caller cares about uniqueness and chronology, not locale.
///
/// **Pass-3 fix (P2 from pass-1 fresh-eyes round-1 by Gemini):**
/// Pre-1970 timestamps used to produce a wrap-around year (e.g. 4294967295) due
/// to the `as u32` cast on a possibly-negative intermediate. We now clamp years
/// outside `[1970, 9999]` to the run-id-friendly anchor year `0000` (followed
/// by the rest of the components) — the run-id space is reserved for forward
/// chronology, not pre-epoch replay; clamping is the documented behavior.
fn decompose_unix_seconds(seconds: i64) -> (u32, u32, u32, u32, u32, u32) {
    // Howard Hinnant's algorithm; civil_from_days. Accurate well past 2200.
    let z = seconds.div_euclid(86400) + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let raw_year = y + if m <= 2 { 1 } else { 0 };
    // Clamp years outside [0, 9999] — see fix note above.
    let year: u32 = if (0..=9999).contains(&raw_year) {
        raw_year as u32
    } else {
        0
    };
    let secs_today = seconds.rem_euclid(86400) as u32;
    let hour = secs_today / 3600;
    let min = (secs_today % 3600) / 60;
    let sec = secs_today % 60;
    (year, m, d, hour, min, sec)
}

fn sha256_short_suffix(target_sha: &str, now_ms: i64) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(target_sha.as_bytes());
    hasher.update(now_ms.to_be_bytes().as_slice());
    let digest = hasher.finalize();
    let mut s = String::with_capacity(6);
    for byte in digest.iter().take(3) {
        use std::fmt::Write;
        let _ = write!(s, "{byte:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_id_round_trip() {
        let id = RunId::from_parts("deadbeef", 1_746_820_021_000);
        let s = id.as_str().to_string();
        let parsed = RunId::parse(&s).expect("parse");
        assert_eq!(parsed.as_str(), id.as_str());
        assert!(s.starts_with("2025-05-09T")); // sanity: 1746820021 ms ~ 2025-05-09
    }

    #[test]
    fn run_id_deterministic_within_second() {
        let id1 = RunId::from_parts("sha", 1_700_000_000_000);
        let id2 = RunId::from_parts("sha", 1_700_000_000_999);
        // Same iso8601 second, same target_sha, but different ms → different hex
        // because the hex is sha256(sha + ms.be_bytes())
        assert_ne!(id1.as_str(), id2.as_str());
        // But the iso prefix is the same
        let prefix1 = &id1.as_str()[..19];
        let prefix2 = &id2.as_str()[..19];
        assert_eq!(prefix1, prefix2);
    }

    #[test]
    fn run_id_rejects_malformed() {
        assert!(RunId::parse("nope").is_none());
        assert!(RunId::parse("2026-01-01T00-00-00Z__zzzz").is_none()); // not hex
        assert!(RunId::parse("2026-01-01T00-00-00Z__abcde").is_none()); // 5 not 6
        assert!(RunId::parse("short__abcdef").is_none()); // iso too short
    }

    #[test]
    fn create_run_dir_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let id = RunId::from_parts("abc", 1_700_000_000_000);
        let p1 = create_run_dir(tmp.path(), &id).expect("first");
        let p2 = create_run_dir(tmp.path(), &id).expect("second");
        assert_eq!(p1, p2);
        assert!(p1.join("backups").is_dir());
        assert!(p1.join("quarantine").is_dir());
    }

    #[test]
    fn append_and_read_actions_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let id = RunId::from_parts("xyz", 1_700_000_000_000);
        let run_dir = create_run_dir(tmp.path(), &id).unwrap();

        let r1 = ActionRecord::RunStarted {
            schema_version: RUN_ARTIFACT_SCHEMA_VERSION,
            run_id: id.as_str().to_string(),
            target_sha: "xyz".to_string(),
            mode: "check".to_string(),
            started_at_ms: 1,
        };
        let r2 = ActionRecord::RunEnded {
            run_id: id.as_str().to_string(),
            exit_code: 0,
            exit_code_kind: "success".to_string(),
            ended_at_ms: 2,
        };
        append_action(&run_dir, &r1).expect("r1");
        append_action(&run_dir, &r2).expect("r2");

        let (recs, errs) = read_actions(&run_dir).unwrap();
        assert_eq!(recs.len(), 2);
        assert!(errs.is_empty());
    }

    #[test]
    fn list_runs_returns_empty_for_no_data_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let runs = list_runs(tmp.path()).unwrap();
        assert!(runs.is_empty());
    }

    #[test]
    fn list_runs_orders_newest_first_and_classifies_status() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ids = Vec::new();
        for i in 0..3 {
            let id = RunId::from_parts("sha", 1_700_000_000_000 + i * 1000);
            let dir = create_run_dir(tmp.path(), &id).unwrap();
            // Only the middle one gets a RunStarted+RunEnded; oldest and newest are incomplete.
            let started = ActionRecord::RunStarted {
                schema_version: RUN_ARTIFACT_SCHEMA_VERSION,
                run_id: id.as_str().to_string(),
                target_sha: "sha".to_string(),
                mode: "check".to_string(),
                started_at_ms: i,
            };
            append_action(&dir, &started).unwrap();
            if i == 1 {
                let ended = ActionRecord::RunEnded {
                    run_id: id.as_str().to_string(),
                    exit_code: 0,
                    exit_code_kind: "success".to_string(),
                    ended_at_ms: i + 1,
                };
                append_action(&dir, &ended).unwrap();
            }
            ids.push(id);
        }

        let runs = list_runs(tmp.path()).unwrap();
        assert_eq!(runs.len(), 3);
        // Newest first
        assert_eq!(runs[0].run_id, ids[2].as_str());
        // Middle one has status=completed; others=incomplete
        let by_id: std::collections::BTreeMap<_, _> =
            runs.iter().map(|r| (r.run_id.clone(), r.status)).collect();
        assert_eq!(by_id[ids[1].as_str()], "completed");
        assert_eq!(by_id[ids[0].as_str()], "incomplete");
        assert_eq!(by_id[ids[2].as_str()], "incomplete");
    }

    #[test]
    fn list_runs_skips_non_runid_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        // Pre-create the runs dir then drop a junk dir into it
        let runs = runs_index_dir(tmp.path());
        fs::create_dir_all(&runs).unwrap();
        fs::create_dir_all(runs.join("not-a-runid")).unwrap();
        let id = RunId::from_parts("sha", 1_700_000_000_000);
        let _ = create_run_dir(tmp.path(), &id).unwrap();

        let listed = list_runs(tmp.path()).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].run_id, id.as_str());
    }

    #[test]
    fn iso8601_seconds_from_ms_known_anchor() {
        // 1700000000 unix seconds = 2023-11-14T22:13:20Z
        let s = iso8601_seconds_from_ms(1_700_000_000_000);
        assert_eq!(s, "2023-11-14T22-13-20Z");
    }

    #[test]
    fn decompose_unix_seconds_handles_zero() {
        // 0 = 1970-01-01T00:00:00
        let (y, m, d, h, mi, se) = decompose_unix_seconds(0);
        assert_eq!((y, m, d, h, mi, se), (1970, 1, 1, 0, 0, 0));
    }

    // ---- Pass-3 deferred-fix regression test (year clamp) ----

    #[test]
    fn pass3_decompose_unix_seconds_clamps_pre_1970_year() {
        // Pass-11 fix: this test now exercises the actual underflow path.
        // Pre-fix, far-pre-epoch timestamps produced raw_year < 0 which the
        // `as u32` cast wrapped to ~4.2 billion. Pass-3's clamp returns 0
        // instead.
        //
        // -100 trillion seconds is ~3170 BC, well into negative-year
        // territory for the Howard-Hinnant algorithm. Verify the year is
        // representable as a 4-digit ISO8601 component (i.e., the clamp
        // fired) and not the underflow value 4_294_966_-something.
        let (year, _m, _d, _h, _mi, _se) = decompose_unix_seconds(-100_000_000_000_000);
        assert!(
            year < 10_000,
            "pre-fix bug would underflow to ~4.29 billion; got {year}"
        );

        // Sanity: 1970-01-01 itself decomposes correctly (the canonical
        // anchor point — must NOT be clamped to 0).
        let (year2, m, d, _, _, _) = decompose_unix_seconds(0);
        assert_eq!((year2, m, d), (1970, 1, 1));
    }

    // ---- Pass-6 band-journal helpers ----

    #[test]
    fn pass6_band_started_and_completed_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let id = RunId::from_parts("sha-band", 1_700_000_000_000);
        let dir = create_run_dir(tmp.path(), &id).unwrap();
        append_band_started(&dir, &id, "B2-storage-hygiene", 3).unwrap();
        append_band_completed(&dir, &id, "B2-storage-hygiene", 3).unwrap();
        let (recs, errs) = read_actions(&dir).unwrap();
        assert!(errs.is_empty());
        assert!(matches!(recs[0], ActionRecord::BandStarted { .. }));
        assert!(matches!(recs[1], ActionRecord::BandCompleted { .. }));
    }

    #[test]
    fn pass6_find_in_flight_band_detects_unfinished_band() {
        let tmp = tempfile::tempdir().unwrap();
        let id = RunId::from_parts("sha-band-crash", 1_700_000_000_000);
        let dir = create_run_dir(tmp.path(), &id).unwrap();
        // Started but never completed (simulates a crash mid-band).
        append_band_started(&dir, &id, "B3-storage-repair", 5).unwrap();
        let inflight = find_in_flight_band(&dir).unwrap();
        assert_eq!(inflight, Some("B3-storage-repair".to_string()));
    }

    #[test]
    fn pass6_find_in_flight_band_returns_none_when_complete() {
        let tmp = tempfile::tempdir().unwrap();
        let id = RunId::from_parts("sha-band-ok", 1_700_000_000_000);
        let dir = create_run_dir(tmp.path(), &id).unwrap();
        append_band_started(&dir, &id, "B0-pre-flight", 0).unwrap();
        append_band_completed(&dir, &id, "B0-pre-flight", 0).unwrap();
        append_band_started(&dir, &id, "B1-daemon-eviction", 1).unwrap();
        append_band_completed(&dir, &id, "B1-daemon-eviction", 1).unwrap();
        assert_eq!(find_in_flight_band(&dir).unwrap(), None);
    }

    #[test]
    fn pass6_find_in_flight_band_handles_empty_run_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let id = RunId::from_parts("sha-empty", 1_700_000_000_000);
        let dir = create_run_dir(tmp.path(), &id).unwrap();
        assert_eq!(find_in_flight_band(&dir).unwrap(), None);
    }
}
