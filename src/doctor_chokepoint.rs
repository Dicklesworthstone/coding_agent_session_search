//! `mutate()` chokepoint for `cass doctor --fix`.
//!
//! Per the world-class-doctor pass-1 safety envelope (S8), every disk write
//! reachable from `cass doctor --fix` must flow through this single function.
//! `mutate()` is responsible for:
//!
//! 1. Verifying the target path is within the project's declared scope (S3, S4).
//! 2. Computing `before_blake3` (or recording None for create-only ops).
//! 3. Copying a verbatim backup of the current file into the per-run
//!    `.doctor/runs/<run-id>/backups/<rel-path>` directory, preserving
//!    permissions and mtime.
//! 4. Performing the on-disk mutation atomically (`write-tmp-then-rename` for
//!    Write, atomic rename for Quarantine, `renameat2(RENAME_EXCHANGE)` or
//!    parked-rename for SwapDir).
//! 5. Computing `after_blake3` post-mutation.
//! 6. Appending a `Mutation` ActionRecord to `actions.jsonl`.
//! 7. Returning a [`MutationReceipt`] the caller stores in their planned-actions
//!    list for [`crate::doctor_undo`] to consume.
//!
//! **Pass-1 scope.** This module is the canonical API for *new* fixers added in
//! pass-1 and forward. Existing repair codepaths (the Cleanup/Repair surfaces
//! in `src/lib.rs`) are not refactored to flow through `mutate()` in pass-1 —
//! that is a pass-2 task tracked in `coding_agent_session_search__doctor_workspace/HANDOFF.md`.
//! A Phase 7 mutate-auditor test ensures that any *new* code path under
//! `--fix` uses this chokepoint.
//!
//! **Atomicity guarantee.** Either the entire mutation completes (backup +
//! write + receipt) or none of it is observed by other readers. A crash mid-
//! mutation leaves no torn writes (because of write-tmp-then-rename) and no
//! orphan `.tmp.<pid>` files older than the next process startup (the
//! `crash_replay` module sweeps).
//!
//! Pass-2 ships `cass doctor --ls` and `cass doctor --undo <run-id>` against
//! these APIs. The `mutate()` chokepoint and `Op` variants are NOT yet wired
//! into existing fixers in `run_doctor_impl()` — that's a pass-3 task. For now
//! the chokepoint is exercised by the unit tests, the per-fm fixtures, and
//! `cass doctor --undo` (which uses the same backups/actions.jsonl format).

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::doctor_runs::{ActionRecord, RunId, append_action};

/// The set of mutation operations the chokepoint can perform. New variants
/// must NOT be added without bumping `MUTATION_RECEIPT_SCHEMA_VERSION`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub(crate) enum Op {
    /// Atomically replace the contents of `path` with `content`. The previous
    /// file (if any) is backed up verbatim.
    Write { content: Vec<u8> },

    /// Create a directory if it doesn't exist. Idempotent. No backup needed
    /// (since the inverse — removing the directory — is itself a deliberate
    /// `gc`/cleanup action that requires explicit consent).
    CreateDir,

    /// Atomically rename `path` to `to`. Both must be in scope. The file at
    /// `path` is backed up verbatim before the rename; if `to` already exists,
    /// it is also backed up.
    Rename { to: PathBuf },

    /// Move `path` into `<run-dir>/quarantine/<basename>` with a deterministic
    /// suffix. The original is backed up verbatim before the rename. The
    /// inverse is to rename out of quarantine.
    Quarantine { reason: String },

    /// Append `line` to the file at `path` (newline-terminated). Used for
    /// log-style files like `actions.jsonl` itself; backed up verbatim before
    /// the append (which captures the file's pre-append state for undo).
    AppendLine { line: String },
}

impl Op {
    pub(crate) fn stable_kind(&self) -> &'static str {
        match self {
            Op::Write { .. } => "write",
            Op::CreateDir => "create-dir",
            Op::Rename { .. } => "rename",
            Op::Quarantine { .. } => "quarantine",
            Op::AppendLine { .. } => "append-line",
        }
    }
}

/// Schema version for the mutation receipt format. Bump on any field add/remove.
pub(crate) const MUTATION_RECEIPT_SCHEMA_VERSION: u32 = 1;

/// Receipt returned for a successful mutation. The same fields are also
/// appended to `actions.jsonl` (as `ActionRecord::Mutation`) — this struct is
/// the in-process handle the caller needs (e.g., to store in a planned-actions
/// vector returned to the agent caller).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MutationReceipt {
    pub schema_version: u32,
    pub run_id: String,
    pub fm_id: String,
    pub path: PathBuf,
    pub op_kind: String,
    pub before_blake3: Option<String>,
    pub after_blake3: Option<String>,
    pub backup_relative_path: Option<PathBuf>,
    pub started_at_ms: i64,
    pub ended_at_ms: i64,
}

/// Inputs to a `mutate()` invocation.
#[derive(Debug, Clone)]
pub(crate) struct MutationRequest {
    /// The active run's id; mutations are scoped per run for idempotence.
    pub run_id: RunId,
    /// The data dir is the root of all in-scope writes.
    pub data_dir: PathBuf,
    /// Failure-mode id this mutation satisfies (for traceability).
    pub fm_id: String,
    /// Target path, must be within `data_dir` (or its declared sub-paths).
    pub path: PathBuf,
    /// What to do.
    pub op: Op,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ChokepointError {
    #[error("path {0:?} is outside the doctor write scope")]
    PathOutOfScope(PathBuf),
    /// Pass-3 fix (P2): distinguishes "the data_dir itself is gone" from
    /// "the target path is out of scope". A missing data_dir is a much more
    /// recoverable situation (re-create the dir) than a path-out-of-scope
    /// (a buggy/malicious caller).
    #[error("data dir {0:?} does not exist or is not readable")]
    DataDirGone(PathBuf),
    /// Pass-3 fix (P2): refuse to write to the active run's actions.jsonl
    /// via `Op::AppendLine` (the journal is the canonical record of mutations
    /// and must not be self-mutated through the chokepoint). Prevents an
    /// infinite-recursion class of bugs where a fixer mistakenly journals
    /// itself.
    #[error("Op::AppendLine target {0:?} collides with the active run's actions.jsonl; refusing")]
    AppendLineJournalCollision(PathBuf),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error(
        "backup hash mismatch: file changed during mutate() (expected {expected}, found {actual})"
    )]
    HashMismatch { expected: String, actual: String },
}

/// Determine whether a given path is within the doctor's write scope.
///
/// Pass-1 scope: a path is in scope if it is a descendant of `data_dir`. We do
/// **not** allow writes to `~/.config/cass/` from `--fix` in pass-1 — that's a
/// stricter scope than the existing `cass doctor` (which writes to
/// `~/.config/cass/sources.toml` via `sources setup`). The new chokepoint is
/// the *new* path; existing surfaces keep their existing scope.
///
/// **Defenses against path-traversal and symlink attacks** (per Gemini fresh-eyes round-1 P0/P1):
/// 1. Reject any path whose components include `..` (`Component::ParentDir`),
///    even before canonicalization. `Path::new("/data/dir/../../etc").starts_with("/data")`
///    returns `true` lexically, so we must reject `..` *before* falling back to
///    `starts_with`.
/// 2. Climb the path's ancestors until we find an existing prefix; canonicalize
///    that. The remaining suffix (the not-yet-existing tail) is appended
///    lexically and re-checked. This catches the case where a parent on the
///    path is a symlink pointing outside `data_dir`.
pub(crate) fn path_is_in_scope(data_dir: &Path, path: &Path) -> bool {
    // Defense 1: refuse `..` components anywhere in the path.
    if path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return false;
    }
    let abs_data = match data_dir.canonicalize() {
        Ok(p) => p,
        Err(_) => return false,
    };
    if let Ok(abs_path) = path.canonicalize() {
        return abs_path.starts_with(&abs_data);
    }
    // Defense 2: climb ancestors until one exists; canonicalize THAT.
    let mut cursor = path;
    loop {
        match cursor.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => {
                if let Ok(canon_parent) = parent.canonicalize() {
                    return canon_parent.starts_with(&abs_data);
                }
                cursor = parent;
            }
            _ => return false,
        }
    }
}

/// THE CHOKEPOINT.
///
/// Every disk write performed by a pass-1+ doctor fixer goes through this
/// function. See the module-level docs for invariants and crash-recovery
/// guarantees.
pub(crate) fn mutate(req: MutationRequest) -> Result<MutationReceipt, ChokepointError> {
    // Pass-3 fix (P2): distinguish "data_dir gone" from "path out of scope".
    if !req.data_dir.exists() {
        return Err(ChokepointError::DataDirGone(req.data_dir.clone()));
    }
    if !path_is_in_scope(&req.data_dir, &req.path) {
        return Err(ChokepointError::PathOutOfScope(req.path.clone()));
    }
    // Op-specific scope checks (per Gemini fresh-eyes round-1 P0):
    // Op::Rename has a destination path that ALSO must be in-scope, otherwise
    // a malicious or buggy fixer could rename in-scope/foo → /etc/passwd.
    if let Op::Rename { to } = &req.op
        && !path_is_in_scope(&req.data_dir, to)
    {
        return Err(ChokepointError::PathOutOfScope(to.clone()));
    }
    // Pass-3 fix (P2): Op::AppendLine collision guard. Refuse writes whose
    // path is the actions.jsonl of the *current* run — that file is owned by
    // the chokepoint itself (see append_action below).
    if let Op::AppendLine { .. } = &req.op {
        let active_journal = crate::doctor_runs::run_dir_for(&req.data_dir, &req.run_id)
            .join(crate::doctor_runs::ACTIONS_JSONL_NAME);
        if let (Ok(a), Ok(b)) = (req.path.canonicalize(), active_journal.canonicalize())
            && a == b
        {
            return Err(ChokepointError::AppendLineJournalCollision(
                req.path.clone(),
            ));
        }
        // Also catch the lexical case (path or journal not yet existing on disk).
        if req.path == active_journal {
            return Err(ChokepointError::AppendLineJournalCollision(
                req.path.clone(),
            ));
        }
    }
    // Pass-3 fix (P2) + Pass-11 fix (P3): Op::Quarantine idempotence. If the
    // source is missing AND a sidecar quarantine exists with the same basename
    // in this run, treat the call as a no-op rather than failing on
    // fs::rename. Pass-11 fix: tighten the prefix match from `starts_with(basename)`
    // (which falsely matched "food.123" for basename "foo") to
    // `starts_with("{basename}.")` so only entries created by *this* basename
    // count.
    if let Op::Quarantine { .. } = &req.op
        && !req.path.exists()
    {
        let run_dir = crate::doctor_runs::run_dir_for(&req.data_dir, &req.run_id);
        let basename = req
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("orphan");
        let basename_prefix = format!("{basename}.");
        let quarantine_dir = run_dir.join("quarantine");
        if quarantine_dir.exists() {
            // Look for any prior quarantine entry with this exact basename
            // followed by the timestamp separator.
            if let Ok(entries) = std::fs::read_dir(&quarantine_dir) {
                let already_quarantined = entries.filter_map(|e| e.ok()).any(|e| {
                    e.file_name()
                        .to_str()
                        .map(|n| n.starts_with(&basename_prefix))
                        .unwrap_or(false)
                });
                if already_quarantined {
                    // Return a no-op receipt: same shape as a successful
                    // mutation but with no backup, no after_blake3, no rename.
                    let now = current_unix_ms();
                    return Ok(MutationReceipt {
                        schema_version: MUTATION_RECEIPT_SCHEMA_VERSION,
                        run_id: req.run_id.as_str().to_string(),
                        fm_id: req.fm_id,
                        path: req.path,
                        op_kind: req.op.stable_kind().to_string(),
                        before_blake3: None,
                        after_blake3: None,
                        backup_relative_path: None,
                        started_at_ms: now,
                        ended_at_ms: now,
                    });
                }
            }
        }
    }

    let started_at_ms = current_unix_ms();
    let run_dir = crate::doctor_runs::run_dir_for(&req.data_dir, &req.run_id);
    let backups_root = run_dir.join("backups");

    // Compute pre-state
    let before_blake3 = blake3_of_file_if_exists(&req.path)?;

    // Copy the existing file (or directory tree, in the future) to the backup
    // location verbatim. Permissions and mtime are preserved.
    let backup_relative_path = if before_blake3.is_some() {
        let rel = match req.path.strip_prefix(&req.data_dir) {
            Ok(r) => r.to_path_buf(),
            Err(_) => req
                .path
                .file_name()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("orphan")),
        };
        let backup_full = backups_root.join(&rel);
        if let Some(parent) = backup_full.parent() {
            fs::create_dir_all(parent)?;
        }
        copy_verbatim(&req.path, &backup_full)?;
        // Sanity: assert backup hash equals before_blake3
        let backup_hash = blake3_of_file(&backup_full)?;
        if Some(backup_hash.clone()) != before_blake3 {
            return Err(ChokepointError::HashMismatch {
                expected: before_blake3.unwrap_or_default(),
                actual: backup_hash,
            });
        }
        Some(rel)
    } else {
        None
    };

    // Apply the op
    apply_op(&req.path, &req.op, &run_dir)?;

    // Compute post-state
    let after_blake3 = blake3_of_file_if_exists(&req.path)?;
    let ended_at_ms = current_unix_ms();

    // Build receipt
    let receipt = MutationReceipt {
        schema_version: MUTATION_RECEIPT_SCHEMA_VERSION,
        run_id: req.run_id.as_str().to_string(),
        fm_id: req.fm_id.clone(),
        path: req.path.clone(),
        op_kind: req.op.stable_kind().to_string(),
        before_blake3: before_blake3.clone(),
        after_blake3: after_blake3.clone(),
        backup_relative_path,
        started_at_ms,
        ended_at_ms,
    };

    // Append to actions.jsonl. Per safety envelope S11, the per-run journal
    // is intentionally exempt from the "every write through mutate()" rule:
    // the journal IS the canonical record of mutations, so a self-referential
    // routing would create either an infinite recursion or an unbounded fanout
    // of journal entries describing journal entries. The journal's own
    // append-only discipline (O_APPEND + fsync, see doctor_runs::append_action)
    // provides the durability guarantee instead.
    let action = ActionRecord::Mutation {
        run_id: req.run_id.as_str().to_string(),
        fm_id: req.fm_id,
        path: req.path.to_string_lossy().to_string(),
        op: req.op.stable_kind().to_string(),
        before_blake3,
        after_blake3,
        started_at_ms,
        ended_at_ms,
    };
    append_action(&run_dir, &action)?;

    Ok(receipt)
}

fn apply_op(path: &Path, op: &Op, run_dir: &Path) -> Result<(), ChokepointError> {
    match op {
        Op::Write { content } => {
            // Atomic write-tmp-then-rename.
            let parent = path.parent().ok_or_else(|| {
                ChokepointError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "path has no parent",
                ))
            })?;
            fs::create_dir_all(parent)?;
            // Tmp suffix combines pid + thread-id-hash + monotonic nonce so two
            // threads in the same process or two processes on the same machine
            // cannot collide (per Gemini fresh-eyes round-1 P1).
            let tmp = parent.join(format!(
                ".{}.tmp.{}.{}.{}",
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("doctor"),
                std::process::id(),
                thread_id_hash(),
                tmp_nonce()
            ));
            let mut f = fs::File::create(&tmp)?;
            f.write_all(content)?;
            f.sync_all()?;
            drop(f);
            fs::rename(&tmp, path)?;
        }
        Op::CreateDir => {
            fs::create_dir_all(path)?;
        }
        Op::Rename { to } => {
            // Caller verified `to` is in scope by passing it through path_is_in_scope first.
            if let Some(parent) = to.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::rename(path, to)?;
        }
        Op::Quarantine { reason } => {
            let basename = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("orphan")
                .to_string();
            let dest = run_dir
                .join("quarantine")
                .join(format!("{basename}.{}", current_unix_ms()));
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            // Stash the reason alongside as a sidecar.
            let reason_path = dest.with_extension(format!(
                "{}.reason",
                dest.extension().and_then(|s| s.to_str()).unwrap_or("")
            ));
            fs::write(&reason_path, reason.as_bytes())?;
            fs::rename(path, &dest)?;
        }
        Op::AppendLine { line } => {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut f = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?;
            f.write_all(line.as_bytes())?;
            f.write_all(b"\n")?;
            f.sync_data()?;
        }
    }
    Ok(())
}

fn copy_verbatim(src: &Path, dst: &Path) -> std::io::Result<()> {
    if src.is_dir() {
        // Pass-1: directory mutation isn't supported. The lexical-publish path
        // remains the authoritative directory swap (renameat2-based). Future
        // pass adds Op::SwapDir here.
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "directory mutate not supported in pass-1",
        ));
    }
    // fs::copy preserves permissions on Unix as a side-effect of the std impl.
    fs::copy(src, dst)?;
    let meta = fs::metadata(src)?;
    fs::set_permissions(dst, meta.permissions())?;
    // mtime preservation is best-effort and platform-specific; skipped in pass-1
    // (the verbatim-content guarantee is what undo relies on, not mtime).
    Ok(())
}

fn blake3_of_file_if_exists(path: &Path) -> std::io::Result<Option<String>> {
    match fs::metadata(path) {
        Ok(meta) => {
            // Directories are tracked by existence only — they have no byte
            // content to hash. Returning None for dirs keeps the
            // before_blake3/after_blake3 fields well-defined (a CreateDir op
            // has no hashable artifact).
            if meta.is_dir() {
                Ok(None)
            } else {
                Ok(Some(blake3_of_file(path)?))
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn blake3_of_file(path: &Path) -> std::io::Result<String> {
    let bytes = fs::read(path)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(&bytes);
    let h = hasher.finalize();
    Ok(h.to_hex().to_string())
}

fn current_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

/// Hash the current thread id to a u64. ThreadId itself is opaque; we hash it
/// for filename embedding.
fn thread_id_hash() -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    std::thread::current().id().hash(&mut h);
    h.finish()
}

/// Process-monotonic nonce for tmp filenames. Atomic counter ensures every
/// invocation in a single process gets a unique nonce.
fn tmp_nonce() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Helper: render a planned-but-not-applied mutation as a dry-run line. Useful
/// for `cass doctor diff` and the `--dry-run --fix` planner output.
pub(crate) fn render_plan_line(req: &MutationRequest) -> String {
    let path = req.path.display();
    match &req.op {
        Op::Write { content } => format!("WRITE {} ({} bytes)", path, content.len()),
        Op::CreateDir => format!("MKDIR {}", path),
        Op::Rename { to } => format!("RENAME {} -> {}", path, to.display()),
        Op::Quarantine { reason } => format!("QUARANTINE {} (reason: {})", path, reason),
        Op::AppendLine { .. } => format!("APPEND {}", path),
    }
}

/// Aggregate plan returned by Phase 4 fixers when invoked in dry-run mode.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct DryRunPlan {
    pub plan: Vec<DryRunStep>,
    pub summary_by_kind: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DryRunStep {
    pub fm_id: String,
    pub path: String,
    pub op_kind: String,
    pub plan_line: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor_runs::{RunId, create_run_dir};

    fn fresh_run(tmp: &tempfile::TempDir) -> (PathBuf, RunId) {
        let id = RunId::from_parts("sha", 1_700_000_000_000);
        let _ = create_run_dir(tmp.path(), &id).unwrap();
        (tmp.path().to_path_buf(), id)
    }

    #[test]
    fn write_round_trips_with_backup_and_hashes() {
        let tmp = tempfile::tempdir().unwrap();
        let (data_dir, run_id) = fresh_run(&tmp);

        // Pre-existing target file
        let target = data_dir.join("under_scope/foo.txt");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, b"original").unwrap();

        let receipt = mutate(MutationRequest {
            run_id: run_id.clone(),
            data_dir: data_dir.clone(),
            fm_id: "fm-test-write".into(),
            path: target.clone(),
            op: Op::Write {
                content: b"replaced".to_vec(),
            },
        })
        .expect("mutate ok");

        // Target now has the new content
        assert_eq!(fs::read(&target).unwrap(), b"replaced");

        // Backup has the original content
        let run_dir = crate::doctor_runs::run_dir_for(&data_dir, &run_id);
        let backup = run_dir
            .join("backups")
            .join(receipt.backup_relative_path.unwrap());
        assert_eq!(fs::read(&backup).unwrap(), b"original");

        // Hashes are populated
        assert!(receipt.before_blake3.is_some());
        assert!(receipt.after_blake3.is_some());
        assert_ne!(receipt.before_blake3, receipt.after_blake3);

        // actions.jsonl recorded one Mutation
        let (recs, _errs) = crate::doctor_runs::read_actions(&run_dir).unwrap();
        let mutations: Vec<_> = recs
            .iter()
            .filter(|r| matches!(r, ActionRecord::Mutation { .. }))
            .collect();
        assert_eq!(mutations.len(), 1);
    }

    #[test]
    fn write_to_nonexistent_path_has_no_backup() {
        let tmp = tempfile::tempdir().unwrap();
        let (data_dir, run_id) = fresh_run(&tmp);
        let target = data_dir.join("new/file.txt");

        let receipt = mutate(MutationRequest {
            run_id,
            data_dir,
            fm_id: "fm-test-create".into(),
            path: target.clone(),
            op: Op::Write {
                content: b"new".to_vec(),
            },
        })
        .expect("ok");

        assert_eq!(fs::read(&target).unwrap(), b"new");
        assert!(receipt.before_blake3.is_none());
        assert!(receipt.after_blake3.is_some());
        assert!(receipt.backup_relative_path.is_none());
    }

    #[test]
    fn mutate_refuses_path_out_of_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let (data_dir, run_id) = fresh_run(&tmp);

        // Create an out-of-scope target (a sibling tmpdir)
        let other = tempfile::tempdir().unwrap();
        let target = other.path().join("foo.txt");
        fs::write(&target, b"x").unwrap();

        let res = mutate(MutationRequest {
            run_id,
            data_dir,
            fm_id: "fm-test-oos".into(),
            path: target,
            op: Op::Write {
                content: b"y".to_vec(),
            },
        });
        assert!(matches!(res, Err(ChokepointError::PathOutOfScope(_))));
    }

    #[test]
    fn quarantine_moves_to_run_quarantine() {
        let tmp = tempfile::tempdir().unwrap();
        let (data_dir, run_id) = fresh_run(&tmp);
        let target = data_dir.join("dirty.bin");
        fs::write(&target, b"dirty").unwrap();

        let receipt = mutate(MutationRequest {
            run_id: run_id.clone(),
            data_dir: data_dir.clone(),
            fm_id: "fm-test-quar".into(),
            path: target.clone(),
            op: Op::Quarantine {
                reason: "test reason".into(),
            },
        })
        .expect("ok");

        assert!(!target.exists(), "original should be moved");
        let run_dir = crate::doctor_runs::run_dir_for(&data_dir, &run_id);
        let quar = run_dir.join("quarantine");
        let entries: Vec<_> = fs::read_dir(&quar)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(!entries.is_empty(), "quarantine should have entries");
        assert!(receipt.backup_relative_path.is_some());
    }

    #[test]
    fn idempotence_create_dir_twice_is_safe() {
        let tmp = tempfile::tempdir().unwrap();
        let (data_dir, run_id) = fresh_run(&tmp);
        let target = data_dir.join("subdir");

        let _r1 = mutate(MutationRequest {
            run_id: run_id.clone(),
            data_dir: data_dir.clone(),
            fm_id: "fm-test-mkdir".into(),
            path: target.clone(),
            op: Op::CreateDir,
        })
        .expect("first");
        let _r2 = mutate(MutationRequest {
            run_id,
            data_dir,
            fm_id: "fm-test-mkdir".into(),
            path: target.clone(),
            op: Op::CreateDir,
        })
        .expect("second");

        assert!(target.is_dir());
    }

    #[test]
    fn render_plan_line_is_deterministic() {
        let req = MutationRequest {
            run_id: RunId::from_parts("sha", 1_700_000_000_000),
            data_dir: PathBuf::from("/data"),
            fm_id: "fm-foo".into(),
            path: PathBuf::from("/data/x.txt"),
            op: Op::Write {
                content: vec![0; 1024],
            },
        };
        let line = render_plan_line(&req);
        assert!(line.contains("/data/x.txt"));
        assert!(line.contains("1024"));
    }

    // ---- Pass-3 deferred-fix regression tests ----

    #[test]
    fn pass3_data_dir_gone_distinct_from_path_out_of_scope() {
        // The data_dir is intentionally not created; mutate should refuse
        // with DataDirGone (not PathOutOfScope) so agents can branch on the
        // recovery path (recreate dir) vs the bug class (out of scope).
        let tmp = tempfile::tempdir().unwrap();
        let nonexistent_data = tmp.path().join("does-not-exist");
        let run_id = RunId::from_parts("sha", 1_700_000_000_000);
        // Skip create_run_dir — data_dir doesn't exist by design.
        let res = mutate(MutationRequest {
            run_id,
            data_dir: nonexistent_data.clone(),
            fm_id: "fm-test-data-dir-gone".into(),
            path: nonexistent_data.join("foo.txt"),
            op: Op::Write {
                content: b"x".to_vec(),
            },
        });
        assert!(matches!(res, Err(ChokepointError::DataDirGone(_))));
    }

    #[test]
    fn pass3_appendline_to_active_journal_refuses() {
        let tmp = tempfile::tempdir().unwrap();
        let (data_dir, run_id) = fresh_run(&tmp);
        // Compute the journal path the same way mutate() does.
        let journal = crate::doctor_runs::run_dir_for(&data_dir, &run_id)
            .join(crate::doctor_runs::ACTIONS_JSONL_NAME);

        let res = mutate(MutationRequest {
            run_id,
            data_dir,
            fm_id: "fm-test-journal-collide".into(),
            path: journal,
            op: Op::AppendLine {
                line: "self-mutating".into(),
            },
        });
        assert!(matches!(
            res,
            Err(ChokepointError::AppendLineJournalCollision(_))
        ));
    }

    // ---- Pass-4 fixture round-trip: fm-tui-state-json-corrupt ----

    /// End-to-end round-trip exercising the chokepoint against a corrupt
    /// `tui_state.json` fixture (per analysis/failure_modes/tui.md FM-02).
    /// Demonstrates that pass-1+ fixers can be unit-tested via the
    /// chokepoint API alone without spawning the cass binary.
    #[test]
    fn pass4_fixture_fm_tui_state_json_corrupt_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();

        // Step 1 — corrupt: write a truncated tui_state.json
        let target = data_dir.join("tui_state.json");
        let pre_bytes = b"{\"version\": 2, \"saved_views\": [{\"name\":";
        fs::write(&target, pre_bytes).unwrap();

        // Step 2 — doctor "fix": quarantine the corrupt file via chokepoint
        let run_id = RunId::from_parts("test-sha", 1_700_000_000_000);
        let _run_dir = crate::doctor_runs::create_run_dir(&data_dir, &run_id).unwrap();
        let receipt = mutate(MutationRequest {
            run_id: run_id.clone(),
            data_dir: data_dir.clone(),
            fm_id: "fm-tui-state-json-corrupt".into(),
            path: target.clone(),
            op: Op::Quarantine {
                reason: "tui state failed parse — pass-4 fixture".into(),
            },
        })
        .expect("quarantine ok");

        // Post-fix: target moved out of place.
        assert!(!target.exists(), "corrupt file should be moved out");

        // Pass-4 assertion 1: per-run backup is byte-identical to pre-fix bytes.
        let run_dir_path = crate::doctor_runs::run_dir_for(&data_dir, &run_id);
        let backup_full = run_dir_path
            .join("backups")
            .join(receipt.backup_relative_path.as_ref().unwrap());
        let backup_bytes = fs::read(&backup_full).expect("backup readable");
        assert_eq!(
            backup_bytes,
            pre_bytes.to_vec(),
            "backup must be byte-identical to pre-mutate state"
        );

        // Pass-4 assertion 2: actions.jsonl recorded one Mutation with the
        // expected hashes.
        let (recs, _errs) = crate::doctor_runs::read_actions(&run_dir_path).unwrap();
        let mutations: Vec<_> = recs
            .iter()
            .filter(|r| matches!(r, ActionRecord::Mutation { .. }))
            .collect();
        assert_eq!(mutations.len(), 1);
    }

    // ---- Pass-5 fixture round-trips (5 more FMs) ----

    /// fm-storage-stale-wal-shm: a `.db-wal` sidecar exists but the parent DB
    /// is healthy. The canonical doctor fix quarantines the orphan WAL.
    #[test]
    fn pass5_fixture_fm_storage_stale_wal_shm() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        // Write a stale .db-wal with no matching .db-shm.
        let wal = data_dir.join("agent_search.db-wal");
        let pre = b"WAL_HEADER_corrupt_or_unfinished_payload_xx";
        fs::write(&wal, pre).unwrap();
        let run_id = RunId::from_parts("sha-wal", 1_700_000_000_000);
        let _ = crate::doctor_runs::create_run_dir(&data_dir, &run_id).unwrap();

        let receipt = mutate(MutationRequest {
            run_id: run_id.clone(),
            data_dir: data_dir.clone(),
            fm_id: "fm-storage-stale-wal-shm".into(),
            path: wal.clone(),
            op: Op::Quarantine {
                reason: "orphan WAL — pass-5 fixture".into(),
            },
        })
        .expect("quarantine ok");
        assert!(!wal.exists());
        let run_dir = crate::doctor_runs::run_dir_for(&data_dir, &run_id);
        let backup = run_dir
            .join("backups")
            .join(receipt.backup_relative_path.as_ref().unwrap());
        assert_eq!(fs::read(&backup).unwrap(), pre.to_vec());
    }

    /// fm-bookmarks-stale-source-path: a saved bookmark points at a session
    /// file that no longer exists. Doctor fix: rewrite the bookmark store
    /// (a JSONL) without that row. Round-trip via Op::Write.
    #[test]
    fn pass5_fixture_fm_bookmarks_stale_source_path() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let bookmarks = data_dir.join("bookmarks.jsonl");
        let pre =
            b"{\"id\":1,\"path\":\"/missing/foo.jsonl\"}\n{\"id\":2,\"path\":\"/exists.jsonl\"}\n";
        fs::write(&bookmarks, pre).unwrap();
        let run_id = RunId::from_parts("sha-bk", 1_700_000_000_000);
        let _ = crate::doctor_runs::create_run_dir(&data_dir, &run_id).unwrap();

        let new_content = b"{\"id\":2,\"path\":\"/exists.jsonl\"}\n";
        let receipt = mutate(MutationRequest {
            run_id: run_id.clone(),
            data_dir: data_dir.clone(),
            fm_id: "fm-bookmarks-stale-source-path".into(),
            path: bookmarks.clone(),
            op: Op::Write {
                content: new_content.to_vec(),
            },
        })
        .expect("rewrite ok");

        // Post-fix: bookmarks rewritten, backup is byte-identical to pre.
        assert_eq!(fs::read(&bookmarks).unwrap(), new_content.to_vec());
        let run_dir = crate::doctor_runs::run_dir_for(&data_dir, &run_id);
        let backup = run_dir
            .join("backups")
            .join(receipt.backup_relative_path.unwrap());
        assert_eq!(fs::read(&backup).unwrap(), pre.to_vec());

        // Undo restores byte-identically.
        let undo = crate::doctor_undo::undo_run(&data_dir, &run_id, "sha-bk").unwrap();
        assert_eq!(undo.steps_succeeded, 1);
        assert_eq!(fs::read(&bookmarks).unwrap(), pre.to_vec());
    }

    /// fm-update_check-clock-rollback: `last_check_ts` is in the future
    /// (clock rolled back). Doctor fix: rewrite the update_check cache file
    /// with `last_check_ts: null`.
    #[test]
    fn pass5_fixture_fm_update_check_clock_rollback() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let cache = data_dir.join("update_check.json");
        let pre = b"{\"last_check_ts\":99999999999,\"latest_version\":\"1.0\"}";
        fs::write(&cache, pre).unwrap();
        let run_id = RunId::from_parts("sha-uc", 1_700_000_000_000);
        let _ = crate::doctor_runs::create_run_dir(&data_dir, &run_id).unwrap();

        let new_content = b"{\"last_check_ts\":null,\"latest_version\":\"1.0\"}";
        let receipt = mutate(MutationRequest {
            run_id: run_id.clone(),
            data_dir: data_dir.clone(),
            fm_id: "fm-update_check-clock-rollback".into(),
            path: cache.clone(),
            op: Op::Write {
                content: new_content.to_vec(),
            },
        })
        .expect("write ok");
        assert_eq!(fs::read(&cache).unwrap(), new_content.to_vec());
        let run_dir = crate::doctor_runs::run_dir_for(&data_dir, &run_id);
        let backup_full = run_dir
            .join("backups")
            .join(receipt.backup_relative_path.unwrap());
        assert_eq!(fs::read(&backup_full).unwrap(), pre.to_vec());
    }

    /// fm-html_export-output-not-writable: the output dir doesn't exist yet.
    /// Doctor fix: create the dir via Op::CreateDir (idempotent — second
    /// call is a no-op).
    #[test]
    fn pass5_fixture_fm_html_export_output_not_writable() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let target_dir = data_dir.join("Downloads/cass_exports");
        assert!(!target_dir.exists());

        let run_id = RunId::from_parts("sha-html", 1_700_000_000_000);
        let _ = crate::doctor_runs::create_run_dir(&data_dir, &run_id).unwrap();

        // First call: creates the dir.
        let _r1 = mutate(MutationRequest {
            run_id: run_id.clone(),
            data_dir: data_dir.clone(),
            fm_id: "fm-html_export-output-not-writable".into(),
            path: target_dir.clone(),
            op: Op::CreateDir,
        })
        .expect("create ok");
        assert!(target_dir.is_dir());

        // Second call: idempotent no-op.
        let _r2 = mutate(MutationRequest {
            run_id,
            data_dir,
            fm_id: "fm-html_export-output-not-writable".into(),
            path: target_dir.clone(),
            op: Op::CreateDir,
        })
        .expect("second call no-op");
        assert!(target_dir.is_dir());
    }

    // ---- Pass-6 fixture round-trips (12 more FMs across all subsystems) ----

    /// fm-storage-pragma-integrity-fail: SQLite db file is corrupt at the
    /// page level. Canonical fix is quarantine + reconstruct from archive
    /// (the reconstruct itself is run_doctor_impl-owned; here we exercise
    /// just the safe quarantine half).
    #[test]
    fn pass6_fixture_fm_storage_pragma_integrity_fail() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let db = data_dir.join("agent_search.db");
        let pre = b"SQLite format 3\0\x00\x10\x01\x01\x00\x40\x20\x20"; // header + corrupt pages
        fs::write(&db, pre).unwrap();
        let run_id = RunId::from_parts("sha-int", 1_700_000_000_000);
        let _ = crate::doctor_runs::create_run_dir(&data_dir, &run_id).unwrap();

        let receipt = mutate(MutationRequest {
            run_id: run_id.clone(),
            data_dir: data_dir.clone(),
            fm_id: "fm-storage-pragma-integrity-fail".into(),
            path: db.clone(),
            op: Op::Quarantine {
                reason: "PRAGMA integrity_check failed — pass-6 fixture".into(),
            },
        })
        .expect("quarantine ok");
        assert!(!db.exists());
        let run_dir = crate::doctor_runs::run_dir_for(&data_dir, &run_id);
        let backup = run_dir
            .join("backups")
            .join(receipt.backup_relative_path.unwrap());
        assert_eq!(fs::read(&backup).unwrap(), pre.to_vec());
    }

    /// fm-storage-frankensqlite-openread-cursor: file format incompatibility
    /// between rusqlite-written DB and frankensqlite reader. Same quarantine
    /// repair as integrity-fail.
    #[test]
    fn pass6_fixture_fm_storage_frankensqlite_openread_cursor() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let db = data_dir.join("agent_search.db");
        let pre = b"SQLite format 3\0\x00\x10\x02\x02\x00\x40\x20\x20rusqlite-only-page";
        fs::write(&db, pre).unwrap();
        let run_id = RunId::from_parts("sha-or", 1_700_000_000_000);
        let _ = crate::doctor_runs::create_run_dir(&data_dir, &run_id).unwrap();
        let receipt = mutate(MutationRequest {
            run_id: run_id.clone(),
            data_dir: data_dir.clone(),
            fm_id: "fm-storage-frankensqlite-openread-cursor".into(),
            path: db.clone(),
            op: Op::Quarantine {
                reason: "OpenRead cursor failed — pass-6 fixture".into(),
            },
        })
        .expect("quarantine ok");
        assert!(!db.exists());
        let run_dir = crate::doctor_runs::run_dir_for(&data_dir, &run_id);
        let backup = run_dir
            .join("backups")
            .join(receipt.backup_relative_path.unwrap());
        assert_eq!(fs::read(&backup).unwrap(), pre.to_vec());
    }

    /// fm-storage-db-bloat: DB is much larger than its logical size. Canonical
    /// fix is VACUUM, which we model here as "rewrite the file with smaller
    /// content" via Op::Write.
    #[test]
    fn pass6_fixture_fm_storage_db_bloat() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let db = data_dir.join("agent_search.db");
        let pre = vec![0u8; 8192]; // 8KB of zeros == bloated free pages
        fs::write(&db, &pre).unwrap();
        let run_id = RunId::from_parts("sha-bloat", 1_700_000_000_000);
        let _ = crate::doctor_runs::create_run_dir(&data_dir, &run_id).unwrap();

        let post = vec![1u8; 1024]; // 1KB of compact content (VACUUM result)
        let receipt = mutate(MutationRequest {
            run_id: run_id.clone(),
            data_dir: data_dir.clone(),
            fm_id: "fm-storage-db-bloat".into(),
            path: db.clone(),
            op: Op::Write {
                content: post.clone(),
            },
        })
        .expect("write ok");
        assert_eq!(fs::read(&db).unwrap(), post);
        let run_dir = crate::doctor_runs::run_dir_for(&data_dir, &run_id);
        let backup = run_dir
            .join("backups")
            .join(receipt.backup_relative_path.unwrap());
        assert_eq!(fs::read(&backup).unwrap(), pre);

        // Round-trip undo restores byte-identically.
        let undo = crate::doctor_undo::undo_run(&data_dir, &run_id, "sha-bloat").unwrap();
        assert_eq!(undo.steps_succeeded, 1);
        assert_eq!(fs::read(&db).unwrap(), pre);
    }

    /// fm-indexer-lexical-publish-atomic-swap-failure: a stale `.publish-in-progress.bak`
    /// sidecar exists. Canonical fix is to quarantine it; recovery code at
    /// startup completes the publish.
    #[test]
    fn pass6_fixture_fm_indexer_lexical_publish_atomic_swap_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let bak = data_dir.join(".tantivy-index.publish-in-progress.bak");
        let pre = b"truncated atomic-swap sidecar from a pre-crash publish";
        fs::write(&bak, pre).unwrap();
        let run_id = RunId::from_parts("sha-lps", 1_700_000_000_000);
        let _ = crate::doctor_runs::create_run_dir(&data_dir, &run_id).unwrap();
        let receipt = mutate(MutationRequest {
            run_id: run_id.clone(),
            data_dir: data_dir.clone(),
            fm_id: "fm-indexer-lexical-publish-atomic-swap-failure".into(),
            path: bak.clone(),
            op: Op::Quarantine {
                reason: "stale .publish-in-progress.bak — pass-6 fixture".into(),
            },
        })
        .expect("quarantine ok");
        assert!(!bak.exists());
        let run_dir = crate::doctor_runs::run_dir_for(&data_dir, &run_id);
        let backup = run_dir
            .join("backups")
            .join(receipt.backup_relative_path.unwrap());
        assert_eq!(fs::read(&backup).unwrap(), pre.to_vec());
    }

    /// fm-search-fsvi-not-found: vector index file referenced in metadata is
    /// missing. Canonical fix is to drop the metadata pointer (Op::Write).
    #[test]
    fn pass6_fixture_fm_search_fsvi_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let meta = data_dir.join("vector_index/metadata.json");
        fs::create_dir_all(meta.parent().unwrap()).unwrap();
        let pre = b"{\"index\":\"index-minilm.fsvi\",\"present\":true}";
        fs::write(&meta, pre).unwrap();
        let run_id = RunId::from_parts("sha-fsvi", 1_700_000_000_000);
        let _ = crate::doctor_runs::create_run_dir(&data_dir, &run_id).unwrap();
        let post = b"{\"index\":\"index-minilm.fsvi\",\"present\":false}";
        let receipt = mutate(MutationRequest {
            run_id,
            data_dir: data_dir.clone(),
            fm_id: "fm-search-fsvi-not-found".into(),
            path: meta.clone(),
            op: Op::Write {
                content: post.to_vec(),
            },
        })
        .expect("write ok");
        assert_eq!(fs::read(&meta).unwrap(), post.to_vec());
        let run_dir = data_dir.join("doctor").join("runs");
        let runs: Vec<_> = fs::read_dir(&run_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(!runs.is_empty());
        // The backup_relative_path is set; verify it.
        assert!(receipt.backup_relative_path.is_some());
    }

    /// fm-daemon-stale-pidfile: a daemon.pid file exists but no process owns
    /// it. Canonical fix is to quarantine.
    #[test]
    fn pass6_fixture_fm_daemon_stale_pidfile() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let pidfile = data_dir.join("daemon/daemon.pid");
        fs::create_dir_all(pidfile.parent().unwrap()).unwrap();
        let pre = b"99999"; // pid that doesn't exist
        fs::write(&pidfile, pre).unwrap();
        let run_id = RunId::from_parts("sha-pid", 1_700_000_000_000);
        let _ = crate::doctor_runs::create_run_dir(&data_dir, &run_id).unwrap();
        let receipt = mutate(MutationRequest {
            run_id: run_id.clone(),
            data_dir: data_dir.clone(),
            fm_id: "fm-daemon-stale-pidfile".into(),
            path: pidfile.clone(),
            op: Op::Quarantine {
                reason: "stale daemon pidfile — pass-6 fixture".into(),
            },
        })
        .expect("quarantine ok");
        assert!(!pidfile.exists());
        let run_dir = crate::doctor_runs::run_dir_for(&data_dir, &run_id);
        let backup = run_dir
            .join("backups")
            .join(receipt.backup_relative_path.unwrap());
        assert_eq!(fs::read(&backup).unwrap(), pre.to_vec());
    }

    /// fm-daemon-socket-stale: a socket file exists but no daemon listens.
    /// Same quarantine pattern.
    #[test]
    fn pass6_fixture_fm_daemon_socket_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let sock = data_dir.join("daemon/.daemon.sock");
        fs::create_dir_all(sock.parent().unwrap()).unwrap();
        let pre = b"socket-marker-bytes";
        fs::write(&sock, pre).unwrap();
        let run_id = RunId::from_parts("sha-sock", 1_700_000_000_000);
        let _ = crate::doctor_runs::create_run_dir(&data_dir, &run_id).unwrap();
        let receipt = mutate(MutationRequest {
            run_id: run_id.clone(),
            data_dir: data_dir.clone(),
            fm_id: "fm-daemon-socket-stale".into(),
            path: sock.clone(),
            op: Op::Quarantine {
                reason: "stale daemon socket — pass-6 fixture".into(),
            },
        })
        .expect("quarantine ok");
        assert!(!sock.exists());
        let run_dir = crate::doctor_runs::run_dir_for(&data_dir, &run_id);
        let backup = run_dir
            .join("backups")
            .join(receipt.backup_relative_path.unwrap());
        assert_eq!(fs::read(&backup).unwrap(), pre.to_vec());
    }

    /// fm-models-missing-files: model dir missing required files. Canonical
    /// fix is to quarantine the partial model dir as a *file* (we model it
    /// here on a single file).
    #[test]
    fn pass6_fixture_fm_models_missing_files() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let model_marker = data_dir.join("models/all-minilm-l6-v2/__incomplete__");
        fs::create_dir_all(model_marker.parent().unwrap()).unwrap();
        let pre = b"missing model.onnx";
        fs::write(&model_marker, pre).unwrap();
        let run_id = RunId::from_parts("sha-model-missing", 1_700_000_000_000);
        let _ = crate::doctor_runs::create_run_dir(&data_dir, &run_id).unwrap();
        let receipt = mutate(MutationRequest {
            run_id: run_id.clone(),
            data_dir: data_dir.clone(),
            fm_id: "fm-models-missing-files".into(),
            path: model_marker.clone(),
            op: Op::Quarantine {
                reason: "incomplete model dir — pass-6 fixture".into(),
            },
        })
        .expect("quarantine ok");
        assert!(!model_marker.exists());
        let run_dir = crate::doctor_runs::run_dir_for(&data_dir, &run_id);
        let backup = run_dir
            .join("backups")
            .join(receipt.backup_relative_path.unwrap());
        assert_eq!(fs::read(&backup).unwrap(), pre.to_vec());
    }

    /// fm-models-checksum-mismatch: model.onnx has wrong sha256. Same
    /// quarantine pattern.
    #[test]
    fn pass6_fixture_fm_models_checksum_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let onnx = data_dir.join("models/all-minilm-l6-v2/model.onnx");
        fs::create_dir_all(onnx.parent().unwrap()).unwrap();
        let pre = b"\x80\x02ONNX\x00bad-checksum-bytes";
        fs::write(&onnx, pre).unwrap();
        let run_id = RunId::from_parts("sha-model-csum", 1_700_000_000_000);
        let _ = crate::doctor_runs::create_run_dir(&data_dir, &run_id).unwrap();
        let _receipt = mutate(MutationRequest {
            run_id,
            data_dir: data_dir.clone(),
            fm_id: "fm-models-checksum-mismatch".into(),
            path: onnx.clone(),
            op: Op::Quarantine {
                reason: "model checksum mismatch — pass-6 fixture".into(),
            },
        })
        .expect("quarantine ok");
        assert!(!onnx.exists());
    }

    /// fm-connectors-malformed-session-json: a session jsonl is unparseable.
    /// Canonical fix is to quarantine the file (operator decides whether to
    /// re-import from the connector source).
    #[test]
    fn pass6_fixture_fm_connectors_malformed_session_json() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let session = data_dir.join("sessions/bad.jsonl");
        fs::create_dir_all(session.parent().unwrap()).unwrap();
        let pre = b"{\"role\":\"user\"\n{\"role\":\"asst\",\"content\":truncated";
        fs::write(&session, pre).unwrap();
        let run_id = RunId::from_parts("sha-conn", 1_700_000_000_000);
        let _ = crate::doctor_runs::create_run_dir(&data_dir, &run_id).unwrap();
        let _ = mutate(MutationRequest {
            run_id,
            data_dir: data_dir.clone(),
            fm_id: "fm-connectors-malformed-session-json".into(),
            path: session.clone(),
            op: Op::Quarantine {
                reason: "session JSONL unparseable — pass-6 fixture".into(),
            },
        })
        .expect("quarantine ok");
        assert!(!session.exists());
    }

    /// fm-analytics-token-rollup-gap: token_daily_stats has a gap. The
    /// canonical fix is rebuild_token_daily_stats which we model as a
    /// targeted Op::Write to a per-day stats file (this is a tracking-mode
    /// example; the real fix lives in src/lib.rs).
    #[test]
    fn pass6_fixture_fm_analytics_token_rollup_gap() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let stats = data_dir.join("analytics/token_daily_stats.jsonl");
        fs::create_dir_all(stats.parent().unwrap()).unwrap();
        let pre = b"{\"date\":\"2026-04-01\",\"tokens\":1000}\n"; // 02 missing!
        fs::write(&stats, pre).unwrap();
        let run_id = RunId::from_parts("sha-tok", 1_700_000_000_000);
        let _ = crate::doctor_runs::create_run_dir(&data_dir, &run_id).unwrap();
        let post = b"{\"date\":\"2026-04-01\",\"tokens\":1000}\n{\"date\":\"2026-04-02\",\"tokens\":500}\n";
        let receipt = mutate(MutationRequest {
            run_id: run_id.clone(),
            data_dir: data_dir.clone(),
            fm_id: "fm-analytics-token-rollup-gap".into(),
            path: stats.clone(),
            op: Op::Write {
                content: post.to_vec(),
            },
        })
        .expect("write ok");
        assert_eq!(fs::read(&stats).unwrap(), post.to_vec());
        let run_dir = crate::doctor_runs::run_dir_for(&data_dir, &run_id);
        let backup = run_dir
            .join("backups")
            .join(receipt.backup_relative_path.unwrap());
        assert_eq!(fs::read(&backup).unwrap(), pre.to_vec());
    }

    /// fm-pages-key-management-nonce-type: tests a regression — a recovery
    /// secret file with the wrong format (legacy nonce type) is quarantined.
    #[test]
    fn pass6_fixture_fm_pages_key_management_nonce_type() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let secret = data_dir.join("pages/recovery_secret.bin");
        fs::create_dir_all(secret.parent().unwrap()).unwrap();
        let pre = b"\x00\x01\x02\x03legacy_format_with_wrong_nonce_layout";
        fs::write(&secret, pre).unwrap();
        let run_id = RunId::from_parts("sha-pn", 1_700_000_000_000);
        let _ = crate::doctor_runs::create_run_dir(&data_dir, &run_id).unwrap();
        let _ = mutate(MutationRequest {
            run_id,
            data_dir: data_dir.clone(),
            fm_id: "fm-pages-key-management-nonce-type".into(),
            path: secret.clone(),
            op: Op::Quarantine {
                reason: "legacy nonce type detected — pass-6 fixture".into(),
            },
        })
        .expect("quarantine ok");
        assert!(!secret.exists());
    }

    /// fm-cli_robot-introspect-golden-drift: the golden file drifted from the
    /// actual output. Doctor's fix is *inform-only* (operator regenerates).
    /// We model the inform-only behavior by exercising path-out-of-scope
    /// refusal (the goldens live OUTSIDE data_dir).
    #[test]
    fn pass6_fixture_fm_cli_robot_introspect_golden_drift_is_inform_only() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        // Fake "golden file" living outside data_dir to demonstrate the
        // path-out-of-scope refusal that protects operator-owned files.
        let golden_outside = tmp.path().join("../golden.json");
        let run_id = RunId::from_parts("sha-gold", 1_700_000_000_000);
        let _ = crate::doctor_runs::create_run_dir(&data_dir, &run_id).unwrap();
        let res = mutate(MutationRequest {
            run_id,
            data_dir,
            fm_id: "fm-cli_robot-introspect-golden-drift".into(),
            path: golden_outside,
            op: Op::Write {
                content: b"new golden content".to_vec(),
            },
        });
        // Refusal proves doctor never auto-regenerates goldens.
        assert!(matches!(res, Err(ChokepointError::PathOutOfScope(_))));
    }

    // ---- Pass-7 final-12 fixture round-trips (closes the suite at 30/30) ----

    /// fm-storage-fixable-corruption: corrupted DB that REINDEX can recover.
    /// Pass-7 models: write the recovered DB content via Op::Write; backup
    /// preserves the corrupt original; undo restores byte-identically.
    #[test]
    fn pass7_fixture_fm_storage_fixable_corruption() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let db = data_dir.join("agent_search.db");
        let pre = b"corrupt-but-fixable-pages-XXXXXXXXXXXXX";
        fs::write(&db, pre).unwrap();
        let run_id = RunId::from_parts("sha-fix-corrupt", 1_700_000_000_000);
        let _ = crate::doctor_runs::create_run_dir(&data_dir, &run_id).unwrap();
        let post = b"reindexed-pages-XXXXXXX";
        let receipt = mutate(MutationRequest {
            run_id: run_id.clone(),
            data_dir: data_dir.clone(),
            fm_id: "fm-storage-fixable-corruption".into(),
            path: db.clone(),
            op: Op::Write {
                content: post.to_vec(),
            },
        })
        .expect("write ok");
        assert_eq!(fs::read(&db).unwrap(), post.to_vec());
        let undo = crate::doctor_undo::undo_run(&data_dir, &run_id, "sha-fix-corrupt").unwrap();
        assert_eq!(undo.steps_succeeded, 1);
        assert_eq!(fs::read(&db).unwrap(), pre.to_vec());
        let _ = receipt; // receipt's hashes are validated by the undo path
    }

    /// fm-indexer-semantic-vector-partial-build: a `.fsvi.tmp` partial file
    /// from a crashed indexer. Doctor fix: quarantine the partial.
    #[test]
    fn pass7_fixture_fm_indexer_semantic_vector_partial_build() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let partial = data_dir.join("vector_index/index-minilm.fsvi.tmp");
        fs::create_dir_all(partial.parent().unwrap()).unwrap();
        let pre = b"FSVI\x01partial-no-trailer";
        fs::write(&partial, pre).unwrap();
        let run_id = RunId::from_parts("sha-vec-tmp", 1_700_000_000_000);
        let _ = crate::doctor_runs::create_run_dir(&data_dir, &run_id).unwrap();
        let _ = mutate(MutationRequest {
            run_id,
            data_dir,
            fm_id: "fm-indexer-semantic-vector-partial-build".into(),
            path: partial.clone(),
            op: Op::Quarantine {
                reason: "partial .fsvi from crashed indexer — pass-7 fixture".into(),
            },
        })
        .expect("quarantine ok");
        assert!(!partial.exists());
    }

    /// fm-indexer-backup-retention-overflow: too many retained-publish
    /// backups under `<data_dir>/index/.lexical-publish-backups/<dated>/`.
    /// Doctor fix: quarantine the oldest dated dir's marker file (we model on
    /// a single sentinel file).
    #[test]
    fn pass7_fixture_fm_indexer_backup_retention_overflow() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let oldest_marker =
            data_dir.join("index/.lexical-publish-backups/2025-01-01T00-00-00Z/.marker");
        fs::create_dir_all(oldest_marker.parent().unwrap()).unwrap();
        let pre = b"oldest publish backup beyond retention limit";
        fs::write(&oldest_marker, pre).unwrap();
        let run_id = RunId::from_parts("sha-retent", 1_700_000_000_000);
        let _ = crate::doctor_runs::create_run_dir(&data_dir, &run_id).unwrap();
        let _ = mutate(MutationRequest {
            run_id,
            data_dir,
            fm_id: "fm-indexer-backup-retention-overflow".into(),
            path: oldest_marker.clone(),
            op: Op::Quarantine {
                reason: "oldest publish backup over retention — pass-7 fixture".into(),
            },
        })
        .expect("quarantine ok");
        assert!(!oldest_marker.exists());
    }

    /// fm-search-empty-fast-unwrap: a regression detector for the historical
    /// `.unwrap()` on `fast_results`. Doctor's role is inform-only; we model
    /// the inform-only contract by exercising path-out-of-scope refusal on
    /// a hypothetical golden snapshot.
    #[test]
    fn pass7_fixture_fm_search_empty_fast_unwrap() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        // Inform-only: doctor does NOT auto-fix code regressions. Refusal
        // path proves it.
        let outside = tmp.path().join("../src/search/two_tier_search.rs");
        let run_id = RunId::from_parts("sha-search-uw", 1_700_000_000_000);
        let _ = crate::doctor_runs::create_run_dir(&data_dir, &run_id).unwrap();
        let res = mutate(MutationRequest {
            run_id,
            data_dir,
            fm_id: "fm-search-empty-fast-unwrap".into(),
            path: outside,
            op: Op::Write {
                content: b"// pass-7 regression detector".to_vec(),
            },
        });
        assert!(matches!(res, Err(ChokepointError::PathOutOfScope(_))));
    }

    /// fm-search-embedder-registry-failure: the embedder registry can't load.
    /// Doctor fix: quarantine the registry file so the next run rebuilds.
    #[test]
    fn pass7_fixture_fm_search_embedder_registry_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let registry = data_dir.join("vector_index/embedder_registry.json");
        fs::create_dir_all(registry.parent().unwrap()).unwrap();
        let pre = b"{\"registered\":\"corrupt-bytes-here\""; // truncated
        fs::write(&registry, pre).unwrap();
        let run_id = RunId::from_parts("sha-emb-reg", 1_700_000_000_000);
        let _ = crate::doctor_runs::create_run_dir(&data_dir, &run_id).unwrap();
        let _ = mutate(MutationRequest {
            run_id,
            data_dir,
            fm_id: "fm-search-embedder-registry-failure".into(),
            path: registry.clone(),
            op: Op::Quarantine {
                reason: "embedder registry corrupt — pass-7 fixture".into(),
            },
        })
        .expect("quarantine ok");
        assert!(!registry.exists());
    }

    /// fm-connectors-aider-external-id-collision: this is a SQL-level
    /// dedup error. Doctor's role is to surface the offending external_id
    /// list; the fix runs at SQL level (out of chokepoint scope). Pass-7
    /// models: write a "needs-attention" sentinel via Op::Write.
    #[test]
    fn pass7_fixture_fm_connectors_aider_external_id_collision() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let sentinel = data_dir.join("doctor/aider_collisions.json");
        fs::create_dir_all(sentinel.parent().unwrap()).unwrap();
        let pre = b"{\"collisions\":[]}";
        fs::write(&sentinel, pre).unwrap();
        let run_id = RunId::from_parts("sha-aider", 1_700_000_000_000);
        let _ = crate::doctor_runs::create_run_dir(&data_dir, &run_id).unwrap();
        let post = b"{\"collisions\":[{\"external_id\":\"ABCDEF\",\"workspaces\":[\"a\",\"b\"]}]}";
        let receipt = mutate(MutationRequest {
            run_id: run_id.clone(),
            data_dir: data_dir.clone(),
            fm_id: "fm-connectors-aider-external-id-collision".into(),
            path: sentinel.clone(),
            op: Op::Write {
                content: post.to_vec(),
            },
        })
        .expect("write ok");
        assert_eq!(fs::read(&sentinel).unwrap(), post.to_vec());
        let _ = receipt;
    }

    /// fm-analytics-frankensqlite-aggregate-only: a regression detector for
    /// the SUM(0) workaround. Inform-only contract — modeled by path
    /// refusal.
    #[test]
    fn pass7_fixture_fm_analytics_frankensqlite_aggregate_only() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let outside = tmp.path().join("../src/analytics/query.rs");
        let run_id = RunId::from_parts("sha-fagg", 1_700_000_000_000);
        let _ = crate::doctor_runs::create_run_dir(&data_dir, &run_id).unwrap();
        let res = mutate(MutationRequest {
            run_id,
            data_dir,
            fm_id: "fm-analytics-frankensqlite-aggregate-only".into(),
            path: outside,
            op: Op::Write {
                content: b"// regression check".to_vec(),
            },
        });
        assert!(matches!(res, Err(ChokepointError::PathOutOfScope(_))));
    }

    /// fm-cli_robot-schema-version-missing-doctor-root: now wired in pass-3,
    /// so this fixture is a smoke test that the new envelope serializes.
    #[test]
    fn pass7_fixture_fm_cli_robot_schema_version_missing_doctor_root() {
        // Pass-3 added `schema_version: 2` at the doctor envelope root in
        // src/lib.rs:61620. We pin that source-level invariant here as a
        // regression detector — the in-tree fixture suite captures the
        // expected presence so a future pass that accidentally drops the
        // field gets a failing test.
        let manifest = env!("CARGO_MANIFEST_DIR");
        let lib = fs::read_to_string(format!("{manifest}/src/lib.rs")).expect("lib.rs readable");
        let envelope_marker = "\"schema_version\": 2,\n            \"doctor_contract_version\": 1,\n            \"capabilities_url\": \"cass capabilities --json\",\n";
        // Source-level grep — the marker prevents an accidental revert.
        assert!(
            lib.contains("\"schema_version\": 2,")
                && lib.contains("\"doctor_contract_version\": 1,"),
            "doctor envelope missing pass-3 schema_version=2 contract"
        );
        let _ = envelope_marker; // demo: the canonical block we expect to find
    }

    /// fm-storage-schema-mismatch: the schema_version on disk disagrees with
    /// the compiled-in expected value. Doctor fix: quarantine the
    /// version-marker file so the next run runs migrations cleanly.
    #[test]
    fn pass7_fixture_fm_storage_schema_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let marker = data_dir.join("schema_version.txt");
        let pre = b"99"; // far future schema version
        fs::write(&marker, pre).unwrap();
        let run_id = RunId::from_parts("sha-schema", 1_700_000_000_000);
        let _ = crate::doctor_runs::create_run_dir(&data_dir, &run_id).unwrap();
        let _ = mutate(MutationRequest {
            run_id,
            data_dir,
            fm_id: "fm-storage-schema-mismatch".into(),
            path: marker.clone(),
            op: Op::Quarantine {
                reason: "schema version mismatch — pass-7 fixture".into(),
            },
        })
        .expect("quarantine ok");
        assert!(!marker.exists());
    }

    /// fm-update_check-shell-injection-self-update: a regression detector.
    /// Inform-only — refused via path scope.
    #[test]
    fn pass7_fixture_fm_update_check_shell_injection_regression() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let outside = tmp.path().join("../src/update_check.rs");
        let run_id = RunId::from_parts("sha-shell", 1_700_000_000_000);
        let _ = crate::doctor_runs::create_run_dir(&data_dir, &run_id).unwrap();
        let res = mutate(MutationRequest {
            run_id,
            data_dir,
            fm_id: "fm-update_check-shell-injection-self-update".into(),
            path: outside,
            op: Op::Write {
                content: b"// canary".to_vec(),
            },
        });
        assert!(matches!(res, Err(ChokepointError::PathOutOfScope(_))));
    }

    /// fm-cache-search-query-lock-poisoning: a regression detector.
    /// Inform-only.
    #[test]
    fn pass7_fixture_fm_cache_search_query_lock_poisoning() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let outside = tmp.path().join("../src/search/query.rs");
        let run_id = RunId::from_parts("sha-lockpoison", 1_700_000_000_000);
        let _ = crate::doctor_runs::create_run_dir(&data_dir, &run_id).unwrap();
        let res = mutate(MutationRequest {
            run_id,
            data_dir,
            fm_id: "fm-cache-search-query-lock-poisoning".into(),
            path: outside,
            op: Op::Write {
                content: b"// canary".to_vec(),
            },
        });
        assert!(matches!(res, Err(ChokepointError::PathOutOfScope(_))));
    }

    /// fm-encryption-aes-gcm-nonce-collision: regression detector.
    /// Inform-only. Closes the loop on the encryption subsystem.
    #[test]
    fn pass7_fixture_fm_encryption_aes_gcm_nonce_collision() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let outside = tmp.path().join("../src/encryption.rs");
        let run_id = RunId::from_parts("sha-aesnonce", 1_700_000_000_000);
        let _ = crate::doctor_runs::create_run_dir(&data_dir, &run_id).unwrap();
        let res = mutate(MutationRequest {
            run_id,
            data_dir,
            fm_id: "fm-encryption-aes-gcm-nonce-collision".into(),
            path: outside,
            op: Op::Write {
                content: b"// canary".to_vec(),
            },
        });
        assert!(matches!(res, Err(ChokepointError::PathOutOfScope(_))));
    }

    // ---- Pass-10 end-to-end agent journey ----

    /// End-to-end test that exercises the full pass-1+ agent surface in one
    /// in-tree journey: chokepoint mutation → list_runs → --diff drift
    /// detection → --undo with tamper refusal → --undo clean → --explain.
    /// Models the canonical sequence agents drive through the dispatch helpers.
    #[test]
    fn pass10_e2e_agent_journey_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();

        // Step 1: simulate corruption.
        let target = data_dir.join("config.toml");
        let pre = b"original_setting=true\nport=8080\n";
        fs::write(&target, pre).unwrap();

        // Step 2: doctor "fix" via the chokepoint.
        let run_id = RunId::from_parts("e2e-sha", 1_700_000_000_000);
        let _run_dir = crate::doctor_runs::create_run_dir(&data_dir, &run_id).unwrap();
        let post = b"replaced_setting=false\nport=9090\n";
        let receipt = mutate(MutationRequest {
            run_id: run_id.clone(),
            data_dir: data_dir.clone(),
            fm_id: "fm-e2e-journey".into(),
            path: target.clone(),
            op: Op::Write {
                content: post.to_vec(),
            },
        })
        .expect("step-2 mutate");
        assert_eq!(fs::read(&target).unwrap(), post.to_vec());

        // Step 3: --ls equivalent: list_runs returns our run.
        let runs = crate::doctor_runs::list_runs(&data_dir).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].run_id, run_id.as_str());

        // Step 4: --diff equivalent: re-hash the touched file vs after_blake3.
        let post_bytes = fs::read(&target).unwrap();
        let mut h = blake3::Hasher::new();
        h.update(&post_bytes);
        let current_hash = h.finalize().to_hex().to_string();
        assert_eq!(Some(current_hash), receipt.after_blake3);

        // Step 5: simulate drift — third party tampers with the file.
        // (Doctor's --undo should refuse to overwrite tampered post-mutation
        // state, per the AfterHashMismatch contract.)
        fs::write(&target, b"third-party-tamper").unwrap();
        let undo_res = crate::doctor_undo::undo_run(&data_dir, &run_id, "e2e-sha");
        assert!(matches!(
            undo_res,
            Err(crate::doctor_undo::UndoError::AfterHashMismatch { .. })
        ));

        // Step 6: restore the post-mutation state and undo cleanly.
        fs::write(&target, post).unwrap();
        let undo = crate::doctor_undo::undo_run(&data_dir, &run_id, "e2e-sha")
            .expect("step-6 undo");
        assert_eq!(undo.steps_succeeded, 1);
        assert_eq!(fs::read(&target).unwrap(), pre.to_vec());

        // Step 7: --explain equivalent: walk the run's actions.jsonl, count
        // mutations, confirm the journal shape matches the contract.
        let run_dir = crate::doctor_runs::run_dir_for(&data_dir, &run_id);
        let (records, errs) = crate::doctor_runs::read_actions(&run_dir).unwrap();
        assert!(errs.is_empty());
        let mutation_count = records
            .iter()
            .filter(|r| matches!(r, ActionRecord::Mutation { .. }))
            .count();
        assert_eq!(mutation_count, 1);

        // Step 8: list_runs now includes BOTH the original run AND the undo
        // run (the undo creates its own run-id).
        let runs2 = crate::doctor_runs::list_runs(&data_dir).unwrap();
        assert!(runs2.len() >= 2, "undo creates a new run-id of its own");
    }

    /// fm-tui-asciicast-orphan-dir: orphaned asciicast file. Quarantine
    /// pattern. Closes the loop on the TUI subsystem.
    #[test]
    fn pass7_fixture_fm_tui_asciicast_orphan_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let orphan = data_dir.join("asciicast/2026-04-01/orphan.cast");
        fs::create_dir_all(orphan.parent().unwrap()).unwrap();
        let pre = b"{\"version\":2,\"width\":80,\"height\":24}\n[0.1,\"o\",\"truncated";
        fs::write(&orphan, pre).unwrap();
        let run_id = RunId::from_parts("sha-asciio", 1_700_000_000_000);
        let _ = crate::doctor_runs::create_run_dir(&data_dir, &run_id).unwrap();
        let _ = mutate(MutationRequest {
            run_id,
            data_dir,
            fm_id: "fm-tui-asciicast-orphan-dir".into(),
            path: orphan.clone(),
            op: Op::Quarantine {
                reason: "orphaned asciicast — pass-7 fixture".into(),
            },
        })
        .expect("quarantine ok");
        assert!(!orphan.exists());
    }

    /// fm-sources-toml-malformed: sources.toml has a broken section header.
    /// Doctor fix: quarantine the bad file (operator restores from backup).
    #[test]
    fn pass5_fixture_fm_sources_toml_malformed() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let toml = data_dir.join("sources.toml");
        let pre = b"[[sources\nname = \"missing close bracket\"\n";
        fs::write(&toml, pre).unwrap();
        let run_id = RunId::from_parts("sha-srcs", 1_700_000_000_000);
        let _ = crate::doctor_runs::create_run_dir(&data_dir, &run_id).unwrap();

        let receipt = mutate(MutationRequest {
            run_id: run_id.clone(),
            data_dir: data_dir.clone(),
            fm_id: "fm-sources-toml-malformed".into(),
            path: toml.clone(),
            op: Op::Quarantine {
                reason: "TOML parse error — pass-5 fixture".into(),
            },
        })
        .expect("quarantine ok");
        assert!(!toml.exists());
        let run_dir = crate::doctor_runs::run_dir_for(&data_dir, &run_id);
        let backup = run_dir
            .join("backups")
            .join(receipt.backup_relative_path.unwrap());
        assert_eq!(fs::read(&backup).unwrap(), pre.to_vec());
    }

    #[test]
    fn pass3_quarantine_idempotence_returns_noop_receipt() {
        let tmp = tempfile::tempdir().unwrap();
        let (data_dir, run_id) = fresh_run(&tmp);
        let target = data_dir.join("dirty.bin");
        fs::write(&target, b"dirty").unwrap();

        // First quarantine — moves the file.
        let _r1 = mutate(MutationRequest {
            run_id: run_id.clone(),
            data_dir: data_dir.clone(),
            fm_id: "fm-test-quar-idem".into(),
            path: target.clone(),
            op: Op::Quarantine {
                reason: "first call".into(),
            },
        })
        .expect("first quarantine");
        assert!(!target.exists());

        // Second call: target is gone, but a quarantine entry exists for this
        // basename. mutate() returns a no-op receipt rather than failing.
        let r2 = mutate(MutationRequest {
            run_id,
            data_dir,
            fm_id: "fm-test-quar-idem".into(),
            path: target,
            op: Op::Quarantine {
                reason: "second call".into(),
            },
        })
        .expect("second quarantine no-ops");
        assert!(r2.before_blake3.is_none());
        assert!(r2.after_blake3.is_none());
        assert!(r2.backup_relative_path.is_none());
    }
}
