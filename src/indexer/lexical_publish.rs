//! Phase-specific diagnostics for rebuild finalization (GH #494).
//!
//! Consuming the canonical cursor and publishing a generation are different
//! milestones. Never turn a failed finalization step into a successful outcome.
//! The best-effort last-failure record is history, not a readiness verdict. A
//! successful swap can move it with the prior live generation into its backup.
//! An off-live candidate also keeps bounded first-failure history that travels
//! with a later quarantine, without attaching post-swap errors to its backup.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

mod failure_evidence;

const LAST_FAILURE_FILE: &str = ".lexical-rebuild-last-failure.json";

/// Retain a failed off-live candidate before replaying from zero. The caller
/// holds the index-run lock and must never pass the currently served directory.
/// Quarantine names are deliberately outside the orphan-staging GC allowlist.
pub(super) fn quarantine_incomplete_candidate(candidate: &Path) -> Result<PathBuf> {
    let metadata = std::fs::symlink_metadata(candidate).with_context(|| {
        format!(
            "inspecting failed lexical candidate {}",
            candidate.display()
        )
    })?;
    anyhow::ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "refusing to quarantine a non-directory or symlink candidate: {}",
        candidate.display()
    );
    let parent = candidate
        .parent()
        .context("lexical candidate has no parent")?;
    // Keep BEFORE the rename: unwinding after any subsequent I/O failure must
    // never let a TempDir destructor delete the only retained failure evidence.
    let quarantine = tempfile::Builder::new()
        .prefix(".lexical-rebuild-quarantine-")
        .tempdir_in(parent)
        .context("reserving a lexical rebuild quarantine directory")?
        .keep();
    std::fs::rename(candidate, quarantine.join("index")).with_context(|| {
        format!(
            "retaining failed lexical candidate {} in {}",
            candidate.display(),
            quarantine.display()
        )
    })?;
    #[cfg(unix)]
    for directory in [quarantine.as_path(), parent] {
        std::fs::File::open(directory)
            .and_then(|file| file.sync_all())
            .with_context(|| format!("syncing lexical quarantine {}", directory.display()))?;
    }
    Ok(quarantine)
}

pub(super) struct Finalization<'a> {
    live: &'a Path,
    candidate: &'a Path,
    indexed_docs: usize,
    processed_conversations: usize,
    // Bind the original off-live candidate once, not a prior-live backup left
    // at the same staging path by a successful publication.
    candidate_evidence: Result<Option<failure_evidence::Candidate>>,
}

impl<'a> Finalization<'a> {
    pub(super) fn new(
        live: &'a Path,
        candidate: &'a Path,
        indexed_docs: usize,
        processed_conversations: usize,
    ) -> Self {
        Self {
            live,
            candidate,
            indexed_docs,
            processed_conversations,
            candidate_evidence: failure_evidence::Candidate::capture(live, candidate),
        }
    }

    // json! panics if an interpolated Serialize fails, including non-Unicode
    // filesystem paths. Both diagnostic writers must encode these fallibly,
    // before any write, rather than lose the original error or record a lossy
    // spelling that could name a different directory.
    fn diagnostic_paths(&self) -> Result<(serde_json::Value, serde_json::Value)> {
        Ok((
            serde_json::to_value(self.candidate).context("encoding lexical candidate path")?,
            serde_json::to_value(self.live).context("encoding live lexical index path")?,
        ))
    }

    // Keep the report in the existing live directory, on the same filesystem
    // as its temporary file. Do not create a missing live index merely to log
    // an error: doing so would change the next publication's recovery inputs.
    fn persist_failure(&self, phase: &str, cause: &str) -> Result<PathBuf> {
        let at_ms = u64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .context("reading lexical failure timestamp")?
                .as_millis(),
        )
        .context("converting lexical failure timestamp")?;
        let (build_path, live_path) = self.diagnostic_paths()?;
        let report = serde_json::json!({
            "version": 1,
            "at_ms": at_ms,
            "process_id": std::process::id(),
            "phase": phase,
            "build_path": build_path,
            "live_path": live_path,
            "expected_docs": self.indexed_docs,
            "processed_conversations": self.processed_conversations,
            "error_chain": cause,
        });
        let report_path = self.live.join(LAST_FAILURE_FILE);
        let mut temporary = tempfile::Builder::new()
            .prefix(".lexical-rebuild-failure-")
            .tempfile_in(self.live)
            .context("creating lexical failure diagnostic in the live directory")?;
        serde_json::to_writer_pretty(&mut temporary, &report)
            .context("serializing lexical failure diagnostic")?;
        temporary
            .as_file()
            .sync_all()
            .context("syncing lexical failure diagnostic")?;
        temporary
            .persist(&report_path)
            .map_err(|error| error.error)
            .context("atomically publishing lexical failure diagnostic")?;
        #[cfg(unix)]
        std::fs::File::open(self.live)
            .and_then(|directory| directory.sync_all())
            .context("syncing lexical failure diagnostic directory")?;
        Ok(report_path)
    }

    pub(super) fn run<T>(
        &self,
        phase: &'static str,
        operation: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let started = Instant::now();
        tracing::info!(
            phase,
            live_index_path = %self.live.display(),
            candidate_index_path = %self.candidate.display(),
            indexed_docs = self.indexed_docs,
            processed_conversations = self.processed_conversations,
            "lexical rebuild finalization step started"
        );
        match operation() {
            Ok(value) => {
                tracing::info!(
                    phase,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "lexical rebuild finalization step completed"
                );
                Ok(value)
            }
            Err(error) => {
                let cause = format!("{error:#}");
                tracing::error!(
                    phase,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    live_index_path = %self.live.display(),
                    candidate_index_path = %self.candidate.display(),
                    indexed_docs = self.indexed_docs,
                    processed_conversations = self.processed_conversations,
                    error = %cause,
                    "lexical rebuild finalization failed"
                );
                failure_evidence::record(self, phase, &cause);
                match self.persist_failure(phase, &cause) {
                    Ok(path) => tracing::info!(
                        phase,
                        path = %path.display(),
                        "persisted lexical rebuild last-failure diagnostic"
                    ),
                    Err(report_error) => tracing::warn!(
                        phase,
                        path = %self.live.join(LAST_FAILURE_FILE).display(),
                        error = %format!("{report_error:#}"),
                        "could not persist lexical finalization diagnostic; preserving the original error"
                    ),
                }
                // Some CLI consumers display only the outer error. Keep
                // the full cause there AND retain its typed source chain.
                // Do not say "not published": a late failure can occur
                // after the directory swap changed the live generation.
                let diagnostic = format!(
                    "lexical rebuild finalization failed during {phase} \
                     (live={}, candidate={}, indexed_docs={}, processed_conversations={}): {cause}",
                    self.live.display(),
                    self.candidate.display(),
                    self.indexed_docs,
                    self.processed_conversations,
                );
                Err(error.context(diagnostic))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::io;

    #[test]
    fn gh494_quarantine_retains_candidate_bytes_and_never_replaces_live() {
        let tmp = tempfile::tempdir().unwrap();
        let candidate = tmp.path().join("candidate");
        let live = tmp.path().join("live");
        std::fs::create_dir(&candidate).unwrap();
        std::fs::create_dir(&live).unwrap();
        std::fs::write(candidate.join("MANIFEST"), b"damaged evidence").unwrap();
        std::fs::write(live.join("MANIFEST"), b"prior publication").unwrap();
        let first = quarantine_incomplete_candidate(&candidate).unwrap();
        assert_eq!(
            std::fs::read(first.join("index/MANIFEST")).unwrap(),
            b"damaged evidence"
        );
        assert!(!candidate.exists());
        assert_eq!(
            std::fs::read(live.join("MANIFEST")).unwrap(),
            b"prior publication"
        );
        std::fs::create_dir(&candidate).unwrap();
        std::fs::write(candidate.join("MANIFEST"), b"next failure").unwrap();
        let second = quarantine_incomplete_candidate(&candidate).unwrap();
        assert_ne!(first, second);
        assert_eq!(
            std::fs::read(first.join("index/MANIFEST")).unwrap(),
            b"damaged evidence"
        );
        assert_eq!(
            std::fs::read(second.join("index/MANIFEST")).unwrap(),
            b"next failure"
        );
    }

    #[test]
    fn gh494_quarantine_refuses_missing_or_non_directory_candidates() {
        let tmp = tempfile::tempdir().unwrap();
        let candidate = tmp.path().join("candidate");
        let error = quarantine_incomplete_candidate(&candidate).unwrap_err();
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
        std::fs::write(&candidate, b"not a directory").unwrap();
        assert!(quarantine_incomplete_candidate(&candidate).is_err());
        assert_eq!(std::fs::read(candidate).unwrap(), b"not a directory");
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn gh494_quarantine_refuses_a_candidate_symlink_without_following_it() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target");
        let candidate = tmp.path().join("candidate");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("evidence"), b"keep").unwrap();
        std::os::unix::fs::symlink(&target, &candidate).unwrap();
        assert!(quarantine_incomplete_candidate(&candidate).is_err());
        assert!(
            std::fs::symlink_metadata(candidate)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(target.join("evidence")).unwrap(), b"keep");
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 2);
    }

    fn read_report(live: &Path) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(live.join(LAST_FAILURE_FILE)).unwrap()).unwrap()
    }

    #[test]
    fn gh494_finalization_preserves_the_success_value_and_runs_once() {
        let tmp = tempfile::tempdir().unwrap();
        let candidate = tmp.path().join("candidate");
        let calls = Cell::new(0);
        let context = Finalization::new(tmp.path(), &candidate, 4, 2);
        let value = context
            .run("commit_candidate", || {
                calls.set(calls.get() + 1);
                Ok(42)
            })
            .unwrap();
        assert_eq!(value, 42);
        assert_eq!(calls.get(), 1);
        assert!(!tmp.path().join(LAST_FAILURE_FILE).exists());
    }

    #[test]
    fn gh494_finalization_display_includes_phase_paths_counts_and_original_cause() {
        let tmp = tempfile::tempdir().unwrap();
        let candidate = tmp.path().join("candidate");
        let context = Finalization::new(tmp.path(), &candidate, 4, 2);
        let error = context
            .run::<()>("publish_staged_generation", || {
                Err(anyhow::Error::new(io::Error::from_raw_os_error(13))
                    .context("rename of validated candidate refused"))
            })
            .unwrap_err();
        let message = error.to_string();
        for expected in [
            "publish_staged_generation".to_owned(),
            format!("live={}", tmp.path().display()),
            format!("candidate={}", candidate.display()),
            "indexed_docs=4".to_owned(),
            "processed_conversations=2".to_owned(),
            "rename of validated candidate refused".to_owned(),
        ] {
            assert!(message.contains(&expected), "missing {expected}: {message}");
        }
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().raw_os_error(),
            Some(13)
        );
        let report = read_report(tmp.path());
        assert_eq!(report["version"], 1);
        assert!(report["at_ms"].as_u64().unwrap() > 0);
        assert_eq!(report["process_id"], std::process::id());
        assert_eq!(report["phase"], "publish_staged_generation");
        assert_eq!(report["build_path"], serde_json::json!(candidate));
        assert_eq!(report["live_path"], serde_json::json!(tmp.path()));
        assert_eq!(report["expected_docs"], 4);
        assert_eq!(report["processed_conversations"], 2);
        let chain = report["error_chain"].as_str().unwrap();
        assert!(chain.contains("rename of validated candidate refused"));
        assert!(chain.contains(&io::Error::from_raw_os_error(13).to_string()));
    }

    #[test]
    fn gh494_late_failure_does_not_claim_the_live_generation_was_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let candidate = tmp.path().join("candidate");
        let context = Finalization::new(tmp.path(), &candidate, 4, 2);
        let error = context
            .run::<()>("persist_completed_checkpoint", || {
                anyhow::bail!("checkpoint directory is not writable")
            })
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("checkpoint directory is not writable")
        );
        assert!(!error.to_string().contains("not published"));
        assert!(!error.to_string().contains("unchanged"));
        assert_eq!(
            read_report(tmp.path())["phase"],
            "persist_completed_checkpoint"
        );
    }

    #[test]
    fn gh494_diagnostic_write_failure_preserves_the_original_typed_error() {
        let tmp = tempfile::tempdir().unwrap();
        let live = tmp.path().join("not-a-directory");
        std::fs::write(&live, "existing evidence").unwrap();
        let context = Finalization::new(&live, tmp.path(), 4, 2);
        let error = context
            .run::<()>("publish_staged_generation", || {
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "original swap error").into())
            })
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(error.to_string().contains("original swap error"));
        assert_eq!(std::fs::read_to_string(&live).unwrap(), "existing evidence");
    }

    #[test]
    fn gh494_recording_failure_must_not_create_a_missing_live_index() {
        let tmp = tempfile::tempdir().unwrap();
        let live = tmp.path().join("missing-live-index");
        let context = Finalization::new(&live, tmp.path(), 4, 2);
        let error = context
            .run::<()>("publish_staged_generation", || {
                anyhow::bail!("initial publish refused")
            })
            .unwrap_err();
        assert!(error.to_string().contains("initial publish refused"));
        assert!(
            !live.exists(),
            "diagnostics must not manufacture a live tree"
        );
    }

    #[test]
    fn gh494_last_failure_is_replaced_atomically_but_not_cleared_by_success() {
        let tmp = tempfile::tempdir().unwrap();
        let candidate = tmp.path().join("candidate");
        let context = Finalization::new(tmp.path(), &candidate, 4, 2);
        for phase in ["validate_candidate", "publish_staged_generation"] {
            let error = context
                .run::<()>(phase, || anyhow::bail!("refused {phase}"))
                .unwrap_err();
            assert!(error.to_string().contains(phase));
            let report = read_report(tmp.path());
            assert_eq!(report["phase"], phase);
            assert_eq!(report["error_chain"], format!("refused {phase}"));
            assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 1);
        }
        let report_path = tmp.path().join(LAST_FAILURE_FILE);
        let before = std::fs::read(&report_path).unwrap();
        context.run("publish_staged_generation", || Ok(())).unwrap();
        assert_eq!(std::fs::read(report_path).unwrap(), before);
    }

    #[test]
    fn gh494_report_replace_failure_does_not_destroy_existing_evidence() {
        let tmp = tempfile::tempdir().unwrap();
        let report_path = tmp.path().join(LAST_FAILURE_FILE);
        std::fs::create_dir(&report_path).unwrap();
        let evidence = report_path.join("evidence");
        std::fs::write(&evidence, "keep").unwrap();
        let context = Finalization::new(tmp.path(), tmp.path(), 4, 2);
        let error = context
            .run::<()>("publish_staged_generation", || {
                anyhow::bail!("original publication refusal")
            })
            .unwrap_err();
        assert!(error.to_string().contains("original publication refusal"));
        assert_eq!(std::fs::read_to_string(evidence).unwrap(), "keep");
    }
}

#[cfg(all(test, unix))]
mod path_encoding_tests {
    use super::*;
    use std::cell::Cell;
    use std::ffi::OsString;
    use std::fs;
    use std::io;
    use std::os::unix::ffi::OsStringExt;

    const CHECKPOINT: &str = ".lexical-rebuild-state.json";
    const PRIOR_REPORT: &[u8] = b"previous diagnostic evidence";

    fn fixture() -> (tempfile::TempDir, fs::File) {
        let root = tempfile::tempdir().unwrap();
        let lock = fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(root.path().join("index-run.lock"))
            .unwrap();
        fs2::FileExt::try_lock_exclusive(&lock).unwrap();
        (root, lock)
    }

    fn path(root: &Path, name: &str, invalid: bool) -> PathBuf {
        let mut bytes = name.as_bytes().to_vec();
        if invalid {
            bytes.push(0xff);
        }
        let path = root.join(OsString::from_vec(bytes));
        assert_eq!(path.to_str().is_none(), invalid);
        path
    }

    fn seed(directory: &Path) {
        fs::create_dir(directory).unwrap();
        fs::write(directory.join("MANIFEST"), b"candidate index bytes").unwrap();
        fs::write(directory.join(CHECKPOINT), b"incomplete checkpoint").unwrap();
    }

    fn unchanged(directory: &Path, prior_report: bool) {
        assert_eq!(
            fs::read(directory.join("MANIFEST")).unwrap(),
            b"candidate index bytes"
        );
        assert_eq!(
            fs::read(directory.join(CHECKPOINT)).unwrap(),
            b"incomplete checkpoint"
        );
        if prior_report {
            assert_eq!(
                fs::read(directory.join(LAST_FAILURE_FILE)).unwrap(),
                PRIOR_REPORT
            );
        }
        // No failed diagnostic may leave a temporary or partially encoded record.
        assert_eq!(
            fs::read_dir(directory).unwrap().count(),
            2 + usize::from(prior_report)
        );
    }

    fn original_error_survives(context: &Finalization<'_>) {
        let calls = Cell::new(0);
        let error = context
            .run::<()>("validate_candidate", || {
                calls.set(calls.get() + 1);
                Err(anyhow::Error::new(io::Error::from_raw_os_error(libc::EIO))
                    .context("candidate certification refused"))
            })
            .unwrap_err();
        assert_eq!(calls.get(), 1);
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().raw_os_error(),
            Some(libc::EIO)
        );
        assert!(error.to_string().contains("validate_candidate"));
        assert!(error.to_string().contains("candidate certification refused"));
    }

    #[test]
    fn live_report_path_encoding_is_fallible_and_preserves_existing_evidence() {
        for (bad_live, bad_candidate) in [(false, true), (true, false)] {
            let (root, _lock) = fixture();
            let live = path(root.path(), "live", bad_live);
            let candidate = path(root.path(), "candidate", bad_candidate);
            seed(&live);
            seed(&candidate);
            fs::write(live.join(LAST_FAILURE_FILE), PRIOR_REPORT).unwrap();
            let context = Finalization::new(&live, &candidate, 2, 1);
            let error = context
                .persist_failure("validate_candidate", "original failure")
                .unwrap_err();
            assert!(error.downcast_ref::<serde_json::Error>().is_some());
            unchanged(&live, true);
            unchanged(&candidate, false);
        }
    }

    #[test]
    fn off_live_encoding_failures_preserve_typed_errors_and_quarantinable_bytes() {
        for (bad_live, bad_candidate) in [(false, true), (true, false), (true, true)] {
            let (root, _lock) = fixture();
            let live = path(root.path(), "live", bad_live);
            let candidate = path(root.path(), "candidate", bad_candidate);
            seed(&live);
            seed(&candidate);
            fs::write(live.join(LAST_FAILURE_FILE), PRIOR_REPORT).unwrap();
            let context = Finalization::new(&live, &candidate, 2, 1);
            // Exercise the candidate writer as well as the independent live writer.
            assert!(context.candidate_evidence.as_ref().unwrap().is_some());
            original_error_survives(&context);
            unchanged(&candidate, false);
            unchanged(&live, true);
            drop(context);
            let quarantine = quarantine_incomplete_candidate(&candidate).unwrap();
            assert!(!candidate.exists());
            unchanged(&quarantine.join("index"), false);
            unchanged(&live, true);
        }
    }

    #[test]
    fn in_place_encoding_failure_preserves_the_error_and_success_stays_a_noop() {
        let (root, _lock) = fixture();
        let live = path(root.path(), "live", true);
        seed(&live);
        fs::write(live.join(LAST_FAILURE_FILE), PRIOR_REPORT).unwrap();
        let context = Finalization::new(&live, &live, 2, 1);
        assert!(context.candidate_evidence.as_ref().unwrap().is_none());
        original_error_survives(&context);
        unchanged(&live, true);
        assert_eq!(context.run("commit_candidate", || Ok(37)).unwrap(), 37);
        unchanged(&live, true);
    }

    #[test]
    fn unencodable_paths_do_not_manufacture_a_missing_live_index() {
        for (bad_live, bad_candidate) in [(false, true), (true, false)] {
            let (root, _lock) = fixture();
            let live = path(root.path(), "missing-live", bad_live);
            let candidate = path(root.path(), "candidate", bad_candidate);
            seed(&candidate);
            let context = Finalization::new(&live, &candidate, 2, 1);
            assert!(context.candidate_evidence.as_ref().unwrap().is_some());
            original_error_survives(&context);
            assert!(!live.exists());
            unchanged(&candidate, false);
            assert_eq!(fs::read_dir(root.path()).unwrap().count(), 2);
        }
    }

    #[test]
    fn unicode_and_control_char_paths_round_trip_in_both_existing_report_schemas() {
        let (root, _lock) = fixture();
        let live = root.path().join("live-界\n");
        let candidate = root.path().join("candidate-é\t");
        seed(&live);
        seed(&candidate);
        original_error_survives(&Finalization::new(&live, &candidate, 2, 1));
        for (directory, is_candidate) in [(&live, false), (&candidate, true)] {
            let record: serde_json::Value =
                serde_json::from_slice(&fs::read(directory.join(LAST_FAILURE_FILE)).unwrap())
                    .unwrap();
            assert_eq!(record["version"], 1);
            assert_eq!(record["build_path"].as_str(), candidate.to_str());
            assert_eq!(record["live_path"].as_str(), live.to_str());
            assert_eq!(record["expected_docs"], 2);
            assert_eq!(record["processed_conversations"], 1);
            assert_eq!(record["phase"], "validate_candidate");
            assert!(
                record["error_chain"]
                    .as_str()
                    .unwrap()
                    .contains("candidate certification refused")
            );
            if is_candidate {
                assert_eq!(record["record_scope"], "candidate_first_failure");
                assert_eq!(record["error_chain_truncated"], false);
                assert_eq!(record.as_object().unwrap().len(), 12);
            } else {
                assert!(record.get("record_scope").is_none());
                assert_eq!(record.as_object().unwrap().len(), 9);
            }
            assert_eq!(fs::read_dir(directory).unwrap().count(), 3);
        }
    }
}
