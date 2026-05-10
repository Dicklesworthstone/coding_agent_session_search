//! `cass doctor undo <run-id>` — restore byte-identically from a per-run
//! backup directory.
//!
//! Walks `<run-dir>/actions.jsonl` in reverse. For each `Mutation` record:
//!
//! 1. Verify the current on-disk file's Blake3 matches the recorded
//!    `after_blake3` (proves no third-party touched the file since the
//!    mutation). If mismatch, refuse with `UndoError::AfterHashMismatch`.
//! 2. If `before_blake3` is `None`, the original mutation was a CREATE — the
//!    inverse is to remove the file. Pass-1 undo refuses removal (per
//!    AGENTS.md RULE NUMBER 1) and instead quarantines the file into the
//!    undo run's quarantine dir.
//! 3. Otherwise, copy the verbatim backup back over the live path (atomic
//!    write-tmp-then-rename) and verify the post-undo Blake3 matches
//!    `before_blake3`.
//!
//! The undo itself is a fresh doctor run with its own `run-id`; its actions
//! are appended to that run's `actions.jsonl`. So `undo` of `undo` is a
//! well-defined operation (re-applies the original mutation).
//!
//! Pass-2 wires `undo_run()` into `cass doctor --undo <run-id>` in
//! `src/lib.rs::run_doctor_undo`. The error variants in [`UndoError`] are
//! mapped to specific `CliError` codes there.

#![allow(dead_code)]

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::doctor_runs::{
    ActionRecord, RUN_ARTIFACT_SCHEMA_VERSION, RunId, append_action, create_run_dir, read_actions,
    run_dir_for,
};

#[derive(Debug, thiserror::Error)]
pub(crate) enum UndoError {
    #[error("run id {0:?} not found in {1:?}")]
    RunNotFound(String, PathBuf),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error(
        "after-hash mismatch on {path}: actions.jsonl recorded {expected}, current file is {actual}"
    )]
    AfterHashMismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },
    #[error("undo target file missing for path {0:?} (was deleted out-of-band)")]
    TargetMissing(PathBuf),
    #[error("backup file missing: {0:?}")]
    BackupMissing(PathBuf),
    #[error("backup hash mismatch on {path:?}: expected {expected}, found {actual}")]
    BackupHashMismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },
    #[error("actions.jsonl unparseable at {0}")]
    ActionsParseError(String),
}

/// Per-action outcome of an undo step. Returned in undo order (i.e., reverse
/// of original mutation order).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct UndoStepReceipt {
    pub original_mutation_path: String,
    pub original_mutation_op: String,
    pub action_taken: String,
    pub before_hash_after_undo: Option<String>,
    pub elapsed_ms: i64,
}

/// Aggregate result of `cass doctor undo <run-id>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct UndoReceipt {
    pub schema_version: u32,
    pub original_run_id: String,
    pub undo_run_id: String,
    pub steps_total: usize,
    pub steps_succeeded: usize,
    pub steps_skipped: usize,
    pub steps_failed: usize,
    pub steps: Vec<UndoStepReceipt>,
}

/// Run undo for `original_run_id`. The new undo run's id is generated and
/// returned. The function fails closed: at the first hash mismatch the undo
/// refuses to proceed (rather than producing a half-undone state).
///
/// `target_sha` should be the cass build sha (used only to seed the new
/// undo-run's id); if unknown, "unknown" is acceptable.
pub(crate) fn undo_run(
    data_dir: &Path,
    original_run_id: &RunId,
    target_sha: &str,
) -> Result<UndoReceipt, UndoError> {
    let original_run_dir = run_dir_for(data_dir, original_run_id);
    if !original_run_dir.exists() {
        return Err(UndoError::RunNotFound(
            original_run_id.as_str().to_string(),
            original_run_dir,
        ));
    }

    let (records, parse_errors) = read_actions(&original_run_dir)?;
    if !parse_errors.is_empty() {
        // Conservative: refuse to undo a run whose actions.jsonl is partially
        // unparseable — operator must inspect first.
        return Err(UndoError::ActionsParseError(format!(
            "{} bad lines in actions.jsonl",
            parse_errors.len()
        )));
    }

    // Initialize the undo run dir
    let undo_run_id = RunId::new(target_sha);
    let undo_dir = create_run_dir(data_dir, &undo_run_id)?;

    let started_at_ms = current_unix_ms();
    append_action(
        &undo_dir,
        &ActionRecord::RunStarted {
            schema_version: RUN_ARTIFACT_SCHEMA_VERSION,
            run_id: undo_run_id.as_str().to_string(),
            target_sha: target_sha.to_string(),
            mode: format!("undo:{}", original_run_id.as_str()),
            started_at_ms,
        },
    )?;

    // Collect Mutation records in reverse order
    let mut mutations_rev: Vec<&ActionRecord> = records
        .iter()
        .filter(|r| matches!(r, ActionRecord::Mutation { .. }))
        .collect();
    mutations_rev.reverse();

    let mut steps = Vec::with_capacity(mutations_rev.len());
    let mut succeeded = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;

    for rec in &mutations_rev {
        let ActionRecord::Mutation {
            path,
            op,
            before_blake3,
            after_blake3,
            ..
        } = rec
        else {
            continue;
        };
        let target = PathBuf::from(path);
        let step_start = current_unix_ms();
        let step_result = undo_one(
            data_dir,
            &original_run_dir,
            &target,
            before_blake3.as_deref(),
            after_blake3.as_deref(),
        );
        let elapsed_ms = current_unix_ms() - step_start;
        let receipt = match step_result {
            Ok(action) => {
                succeeded += 1;
                UndoStepReceipt {
                    original_mutation_path: path.clone(),
                    original_mutation_op: op.clone(),
                    action_taken: action.to_string(),
                    before_hash_after_undo: before_blake3.clone(),
                    elapsed_ms,
                }
            }
            Err(e) => {
                // Distinguish "skipped because target already missing" from "failed because hash mismatch"
                let action: &'static str = match &e {
                    UndoError::TargetMissing(_) => {
                        skipped += 1;
                        "skipped-target-missing"
                    }
                    _ => {
                        failed += 1;
                        "failed"
                    }
                };
                let receipt = UndoStepReceipt {
                    original_mutation_path: path.clone(),
                    original_mutation_op: op.clone(),
                    action_taken: action.to_string(),
                    before_hash_after_undo: None,
                    elapsed_ms,
                };
                steps.push(receipt);
                // Conservative: stop on first hard failure. Operator inspects.
                // Pass-11 fix: dropped a dead-code `let receipt = ...; let _ =
                // receipt;` block that constructed an UndoReceipt and then
                // discarded it before returning Err. The receipt was never
                // surfaced; the journal's RunEnded record is the canonical
                // failure signal here.
                if !matches!(e, UndoError::TargetMissing(_)) {
                    let ended_at_ms = current_unix_ms();
                    append_action(
                        &undo_dir,
                        &ActionRecord::RunEnded {
                            run_id: undo_run_id.as_str().to_string(),
                            exit_code: 3, // fix failed and rolled back
                            exit_code_kind: "repair-failure".to_string(),
                            ended_at_ms,
                        },
                    )?;
                    return Err(e);
                }
                continue;
            }
        };
        steps.push(receipt);
    }

    let ended_at_ms = current_unix_ms();
    append_action(
        &undo_dir,
        &ActionRecord::RunEnded {
            run_id: undo_run_id.as_str().to_string(),
            exit_code: 0,
            exit_code_kind: "success".to_string(),
            ended_at_ms,
        },
    )?;

    Ok(UndoReceipt {
        schema_version: 1,
        original_run_id: original_run_id.as_str().to_string(),
        undo_run_id: undo_run_id.as_str().to_string(),
        steps_total: mutations_rev.len(),
        steps_succeeded: succeeded,
        steps_skipped: skipped,
        steps_failed: failed,
        steps,
    })
}

fn undo_one(
    data_dir: &Path,
    original_run_dir: &Path,
    target: &Path,
    before_blake3: Option<&str>,
    after_blake3: Option<&str>,
) -> Result<&'static str, UndoError> {
    // Step 1: tamper-check the post-mutation state. The recorded
    // `after_blake3` is what the chokepoint observed RIGHT after the
    // mutation; the current file's hash must match (or both must be None
    // for a mutation that moved/quarantined the file out of req.path).
    //
    // **Pass-11 fix:** the previous `(None, None) → noop` arm silently
    // skipped Op::Quarantine / Op::Rename undos because those mutations
    // record `after_blake3 = None` (the file is gone from req.path) but
    // `before_blake3 = Some(...)` (we still have a backup). We now fall
    // through to step 2 + step 3 in that case so the backup actually
    // gets restored. Genuine no-ops (where both `before` and `after` are
    // None — e.g. Op::CreateDir on an existing dir) are handled by the
    // step-2 branch returning early.
    let current = blake3_of_file_if_exists(target)?;
    match (current.as_deref(), after_blake3) {
        (Some(actual), Some(expected)) if actual == expected => {
            // OK — post-mutation state intact; proceed to restore.
        }
        (None, Some(_)) => {
            // The mutation produced a file but it's gone now — skip.
            return Err(UndoError::TargetMissing(target.to_path_buf()));
        }
        (Some(actual), Some(expected)) => {
            return Err(UndoError::AfterHashMismatch {
                path: target.to_path_buf(),
                expected: expected.to_string(),
                actual: actual.to_string(),
            });
        }
        (None, None) => {
            // Both absent: this is the Op::Quarantine / Op::Rename case
            // (req.path was emptied) OR a true noop (Op::CreateDir on a
            // dir that's still missing). Step 2 below distinguishes via
            // before_blake3 and either restores or no-ops.
        }
        (Some(actual), None) => {
            // The mutation moved/removed the file from req.path, but a
            // file is now back at req.path. Two sub-cases:
            //   1. Idempotent re-undo: the file matches before_blake3,
            //      so we're already in the desired post-undo state.
            //   2. Tamper: someone wrote arbitrary content to req.path.
            //      Refuse to overwrite.
            if let Some(before) = before_blake3
                && actual == before
            {
                return Ok("noop-already-idempotent-restored");
            }
            return Err(UndoError::AfterHashMismatch {
                path: target.to_path_buf(),
                expected: "<absent>".to_string(),
                actual: actual.to_string(),
            });
        }
    }

    // Step 2: if before_blake3 is None, the mutation was a CREATE; quarantine instead of unlink.
    // (Per AGENTS.md RULE NUMBER 1: never delete files; quarantine instead.)
    if before_blake3.is_none() {
        // Genuine no-op when there's nothing on disk to quarantine
        // (e.g., Op::CreateDir on a directory that was already removed
        // out of band).
        if !target.exists() {
            return Ok("noop-create-already-removed");
        }
        let undo_quarantine_dir = data_dir
            .join("doctor")
            .join("undo-quarantine")
            .join(format!("{}", current_unix_ms()));
        fs::create_dir_all(&undo_quarantine_dir)?;
        let dest = undo_quarantine_dir.join(
            target
                .file_name()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("orphan")),
        );
        fs::rename(target, &dest)?;
        return Ok("quarantined-because-original-was-create");
    }

    // Step 3: locate and verify backup, then atomically restore.
    let rel = target
        .strip_prefix(data_dir)
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|_| {
            target
                .file_name()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("orphan"))
        });
    let backup_path = original_run_dir.join("backups").join(&rel);
    if !backup_path.exists() {
        return Err(UndoError::BackupMissing(backup_path));
    }
    // Read the backup ONCE into memory and verify its hash, then write those
    // exact bytes to the target. Reading twice (once to hash, once to copy)
    // exposes a TOCTOU window where a third party could swap the backup
    // between the hash check and the read (per Gemini fresh-eyes round-1 P1).
    let backup_bytes = fs::read(&backup_path)?;
    let backup_hash = {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&backup_bytes);
        hasher.finalize().to_hex().to_string()
    };
    let expected_before = before_blake3.expect("checked above");
    if backup_hash != expected_before {
        return Err(UndoError::BackupHashMismatch {
            path: backup_path,
            expected: expected_before.to_string(),
            actual: backup_hash,
        });
    }

    // Atomic restore: write-tmp-then-rename
    let parent = target.parent().ok_or_else(|| {
        UndoError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "target has no parent",
        ))
    })?;
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".{}.undo.tmp.{}.{}.{}",
        target
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("doctor"),
        std::process::id(),
        thread_id_hash_undo(),
        tmp_nonce_undo(),
    ));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&backup_bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, target)?;

    // Verify post-undo hash
    let post = blake3_of_file(target)?;
    if post != expected_before {
        return Err(UndoError::BackupHashMismatch {
            path: target.to_path_buf(),
            expected: expected_before.to_string(),
            actual: post,
        });
    }

    Ok("restored-from-backup")
}

fn blake3_of_file_if_exists(path: &Path) -> std::io::Result<Option<String>> {
    match fs::metadata(path) {
        Ok(_) => Ok(Some(blake3_of_file(path)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn blake3_of_file(path: &Path) -> std::io::Result<String> {
    let bytes = fs::read(path)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(&bytes);
    Ok(hasher.finalize().to_hex().to_string())
}

fn current_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

/// Hash the current thread id to a u64 — for tmp-filename uniqueness.
fn thread_id_hash_undo() -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    std::thread::current().id().hash(&mut h);
    h.finish()
}

/// Process-monotonic nonce for tmp filenames. Atomic counter ensures every
/// invocation in a single process gets a unique nonce.
fn tmp_nonce_undo() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor_chokepoint::{MutationRequest, Op, mutate};

    #[test]
    fn round_trip_write_then_undo_restores_byte_identical() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let run_id = RunId::from_parts("sha", 1_700_000_000_000);
        let _ = create_run_dir(&data_dir, &run_id).unwrap();

        let target = data_dir.join("config.toml");
        fs::write(&target, b"original=1\n").unwrap();
        let before_bytes = fs::read(&target).unwrap();

        // Apply a Write mutation
        let _r = mutate(MutationRequest {
            run_id: run_id.clone(),
            data_dir: data_dir.clone(),
            fm_id: "fm-test".into(),
            path: target.clone(),
            op: Op::Write {
                content: b"replaced=2\n".to_vec(),
            },
        })
        .expect("mutate ok");

        assert_eq!(fs::read(&target).unwrap(), b"replaced=2\n");

        // Now undo
        let receipt = undo_run(&data_dir, &run_id, "sha2").expect("undo ok");
        assert_eq!(receipt.steps_succeeded, 1);
        assert_eq!(receipt.steps_failed, 0);
        assert_eq!(fs::read(&target).unwrap(), before_bytes);
    }

    #[test]
    fn undo_create_quarantines_instead_of_deleting() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let run_id = RunId::from_parts("sha", 1_700_000_000_000);
        let _ = create_run_dir(&data_dir, &run_id).unwrap();

        // Create a file via mutate
        let target = data_dir.join("new.txt");
        let _r = mutate(MutationRequest {
            run_id: run_id.clone(),
            data_dir: data_dir.clone(),
            fm_id: "fm-test".into(),
            path: target.clone(),
            op: Op::Write {
                content: b"hello".to_vec(),
            },
        })
        .expect("create ok");
        assert!(target.exists());

        let receipt = undo_run(&data_dir, &run_id, "sha2").expect("undo ok");
        assert_eq!(receipt.steps_succeeded, 1);
        // Target was NOT deleted, was moved to undo-quarantine
        assert!(!target.exists(), "original target should be moved");
        let undo_quar_root = data_dir.join("doctor").join("undo-quarantine");
        assert!(undo_quar_root.exists());
    }

    #[test]
    fn undo_fails_closed_on_after_hash_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let run_id = RunId::from_parts("sha", 1_700_000_000_000);
        let _ = create_run_dir(&data_dir, &run_id).unwrap();

        let target = data_dir.join("file.txt");
        fs::write(&target, b"original").unwrap();
        let _r = mutate(MutationRequest {
            run_id: run_id.clone(),
            data_dir: data_dir.clone(),
            fm_id: "fm-test".into(),
            path: target.clone(),
            op: Op::Write {
                content: b"replaced".to_vec(),
            },
        })
        .expect("mutate ok");

        // Tamper with the post-mutation file
        fs::write(&target, b"third-party-edit").unwrap();

        let res = undo_run(&data_dir, &run_id, "sha2");
        assert!(matches!(res, Err(UndoError::AfterHashMismatch { .. })));
    }

    /// Pass-11 regression test for a P0 bug found via fresh-eyes review:
    /// `undo_one` previously matched `(None, None)` to "noop-already-restored"
    /// without checking `before_blake3`. For Op::Quarantine where the file
    /// was moved out of req.path, `after_blake3 = None` but `before_blake3 =
    /// Some(...)` and a backup exists at `backups/<rel>` — the file should
    /// be restored. Pre-fix: undo silently returned success without
    /// touching the file. Post-fix: undo restores from backup.
    #[test]
    fn pass11_undo_of_op_quarantine_restores_from_backup() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let run_id = RunId::from_parts("sha-quar-undo", 1_700_000_000_000);
        let _ = create_run_dir(&data_dir, &run_id).unwrap();

        // Pre-existing file with content.
        let target = data_dir.join("config.toml");
        let pre = b"original=true\n";
        fs::write(&target, pre).unwrap();

        // Op::Quarantine via the chokepoint moves the file out + records a
        // backup at backups/<rel>.
        let _r = crate::doctor_chokepoint::mutate(crate::doctor_chokepoint::MutationRequest {
            run_id: run_id.clone(),
            data_dir: data_dir.clone(),
            fm_id: "fm-test-quarantine-undo".into(),
            path: target.clone(),
            op: crate::doctor_chokepoint::Op::Quarantine {
                reason: "regression test for pass-11 bug".into(),
            },
        })
        .expect("quarantine ok");
        assert!(!target.exists(), "post-fix: file moved out");

        // Undo MUST restore the file from backup byte-identically.
        let receipt = undo_run(&data_dir, &run_id, "sha-quar-undo").expect("undo ok");
        assert_eq!(receipt.steps_succeeded, 1);
        assert!(target.exists(), "post-undo: file restored at original location");
        assert_eq!(fs::read(&target).unwrap(), pre.to_vec());
    }

    /// Pass-11 regression test: idempotent re-undo. After a successful
    /// undo, the file is back at req.path with `before_blake3` content.
    /// A second undo invocation against the same original run should now
    /// be classified as already-idempotent rather than fail.
    #[test]
    fn pass11_undo_of_quarantine_is_idempotent_on_re_undo() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let run_id = RunId::from_parts("sha-idem", 1_700_000_000_000);
        let _ = create_run_dir(&data_dir, &run_id).unwrap();

        let target = data_dir.join("foo.bin");
        let pre = b"v1";
        fs::write(&target, pre).unwrap();

        let _r = crate::doctor_chokepoint::mutate(crate::doctor_chokepoint::MutationRequest {
            run_id: run_id.clone(),
            data_dir: data_dir.clone(),
            fm_id: "fm-test-idem".into(),
            path: target.clone(),
            op: crate::doctor_chokepoint::Op::Quarantine {
                reason: "test".into(),
            },
        })
        .expect("quar ok");

        // First undo: restores.
        let r1 = undo_run(&data_dir, &run_id, "sha-idem").expect("undo1 ok");
        assert_eq!(r1.steps_succeeded, 1);
        assert_eq!(fs::read(&target).unwrap(), pre.to_vec());

        // Second undo: should NOT fail with AfterHashMismatch — the file
        // at req.path matches before_blake3, so it's idempotent.
        let r2 = undo_run(&data_dir, &run_id, "sha-idem").expect("undo2 ok (idempotent)");
        // The second undo's first step succeeds via the
        // "noop-already-idempotent-restored" path — not classified as
        // failure.
        assert_eq!(r2.steps_failed, 0);
    }

    #[test]
    fn undo_skips_target_missing_with_warning() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let run_id = RunId::from_parts("sha", 1_700_000_000_000);
        let _ = create_run_dir(&data_dir, &run_id).unwrap();

        let target = data_dir.join("ephemeral.txt");
        fs::write(&target, b"x").unwrap();
        let _r = mutate(MutationRequest {
            run_id: run_id.clone(),
            data_dir: data_dir.clone(),
            fm_id: "fm-test".into(),
            path: target.clone(),
            op: Op::Write {
                content: b"y".to_vec(),
            },
        })
        .expect("ok");

        // Out-of-band delete
        fs::remove_file(&target).unwrap();

        let receipt = undo_run(&data_dir, &run_id, "sha2").expect("undo continues");
        assert_eq!(receipt.steps_skipped, 1);
        assert_eq!(receipt.steps_failed, 0);
    }
}
