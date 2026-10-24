use super::*;
use crate::indexer::lexical_publish::quarantine_incomplete_candidate;
use crate::search::lexical_quarantine::{self, EvidenceKind};
use serde_json::Value;

const CHECKPOINT: &str = ".lexical-rebuild-state.json";

fn seed(candidate: &Path) {
    fs::create_dir(candidate).unwrap();
    fs::write(candidate.join("MANIFEST"), b"candidate manifest").unwrap();
    fs::write(candidate.join(CHECKPOINT), b"incomplete checkpoint").unwrap();
}

fn failure(publication: &Finalization<'_>, phase: &'static str, cause: &str) {
    let result: Result<()> = publication.run(phase, || {
        Err(io::Error::new(io::ErrorKind::InvalidData, cause.to_owned()).into())
    });
    let error = result.unwrap_err();
    let original = error.downcast_ref::<io::Error>().unwrap();
    assert_eq!(original.kind(), io::ErrorKind::InvalidData);
    assert_eq!(original.to_string(), cause);
    assert!(error.to_string().contains(phase));
}

fn report(directory: &Path) -> Value {
    serde_json::from_slice(&fs::read(directory.join(LAST_FAILURE_FILE)).unwrap()).unwrap()
}

fn unchanged_payload(candidate: &Path) {
    assert_eq!(
        fs::read(candidate.join("MANIFEST")).unwrap(),
        b"candidate manifest"
    );
    assert_eq!(
        fs::read(candidate.join(CHECKPOINT)).unwrap(),
        b"incomplete checkpoint"
    );
}

#[test]
fn repeated_failed_rebuilds_keep_distinct_evidence_through_real_quarantine_and_inventory() {
    let root = tempfile::tempdir().unwrap();
    let live = root.path().join("live");
    fs::create_dir(&live).unwrap();
    fs::write(live.join("MANIFEST"), b"prior live").unwrap();
    let candidate = root.path().join("candidate");
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.path().join("index-run.lock"))
        .unwrap();
    fs2::FileExt::try_lock_exclusive(&lock).unwrap();
    let failures = [
        ("commit_candidate", "commit refused"),
        ("validate_candidate", "count mismatch"),
    ];
    let mut quarantines = Vec::new();
    for (phase, cause) in failures {
        seed(&candidate);
        let publication = Finalization::new(&live, &candidate, 41, 7);
        failure(&publication, phase, cause);
        drop(publication);
        unchanged_payload(&candidate);
        quarantines.push(quarantine_incomplete_candidate(&candidate).unwrap());
    }
    for (quarantine, (phase, cause)) in quarantines.iter().zip(failures) {
        let evidence = quarantine.join("index");
        let record = report(&evidence);
        assert_eq!(record["phase"], phase);
        assert_eq!(record["error_chain"], cause);
        assert_eq!(record["record_scope"], "candidate_first_failure");
        assert_eq!(record["expected_docs"], 41);
        assert_eq!(record["processed_conversations"], 7);
        assert_eq!(record["error_chain_truncated"], false);
        unchanged_payload(&evidence);
    }
    assert_eq!(report(&live)["error_chain"], "count mismatch");
    assert_eq!(fs::read(live.join("MANIFEST")).unwrap(), b"prior live");
    let inventory = lexical_quarantine::inspect(root.path()).unwrap();
    assert!(inventory.complete);
    assert_eq!(inventory.artifacts.len(), 2);
    assert_eq!(
        inventory.eligible_bytes, 0,
        "new failure evidence is recent"
    );
    for artifact in &inventory.artifacts {
        assert!(artifact.failure_evidence.iter().any(|evidence| {
            evidence.kind == EvidenceKind::FinalizationFailure
                && evidence.relative_path == Path::new("index").join(LAST_FAILURE_FILE)
                && evidence.size_bytes > 0
        }));
    }
    let json = serde_json::to_string(&inventory).unwrap();
    for (_, cause) in failures {
        assert!(!json.contains(cause), "inventory must remain metadata-only");
    }
}

#[test]
fn first_build_without_live_index_still_keeps_candidate_evidence() {
    let root = tempfile::tempdir().unwrap();
    let live = root.path().join("missing-live");
    let candidate = root.path().join("candidate");
    seed(&candidate);
    failure(
        &Finalization::new(&live, &candidate, 3, 2),
        "validate_candidate",
        "unreadable candidate",
    );
    assert!(
        !live.exists(),
        "diagnostics must not manufacture a live generation"
    );
    assert_eq!(report(&candidate)["error_chain"], "unreadable candidate");
    unchanged_payload(&candidate);
}

#[test]
fn successful_phases_leave_no_report_or_temporary_file() {
    let root = tempfile::tempdir().unwrap();
    let live = root.path().join("missing-live");
    let candidate = root.path().join("candidate");
    seed(&candidate);
    let publication = Finalization::new(&live, &candidate, 0, 0);
    assert_eq!(
        publication.run("validate_candidate", || Ok(123)).unwrap(),
        123
    );
    assert!(!live.exists());
    assert_eq!(fs::read_dir(&candidate).unwrap().count(), 2);
    unchanged_payload(&candidate);
}

#[test]
fn same_finalization_keeps_first_candidate_failure_but_live_report_remains_latest() {
    let root = tempfile::tempdir().unwrap();
    let live = root.path().join("live");
    fs::create_dir(&live).unwrap();
    let candidate = root.path().join("candidate");
    seed(&candidate);
    let publication = Finalization::new(&live, &candidate, 2, 1);
    failure(&publication, "commit_candidate", "first error");
    let original = fs::read(candidate.join(LAST_FAILURE_FILE)).unwrap();
    publication.run("checkpoint_after_fold", || Ok(())).unwrap();
    failure(&publication, "validate_candidate", "second error");
    assert_eq!(
        fs::read(candidate.join(LAST_FAILURE_FILE)).unwrap(),
        original
    );
    assert_eq!(report(&live)["error_chain"], "second error");
    unchanged_payload(&candidate);
}

#[test]
fn post_swap_failure_never_writes_candidate_evidence_into_prior_live_backup() {
    let root = tempfile::tempdir().unwrap();
    let live = root.path().join("live");
    let candidate = root.path().join("candidate");
    let intermediate = root.path().join("swap-intermediate");
    fs::create_dir(&live).unwrap();
    fs::write(live.join("MANIFEST"), b"prior live").unwrap();
    seed(&candidate);
    let publication = Finalization::new(&live, &candidate, 4, 2);
    publication
        .run("publish_staged_generation", || {
            fs::rename(&live, &intermediate)?;
            fs::rename(&candidate, &live)?;
            fs::rename(&intermediate, &candidate)?;
            Ok(())
        })
        .unwrap();
    failure(
        &publication,
        "validate_published_generation",
        "late verification failure",
    );
    assert_eq!(fs::read(candidate.join("MANIFEST")).unwrap(), b"prior live");
    assert_eq!(fs::read_dir(&candidate).unwrap().count(), 1);
    assert_eq!(report(&live)["error_chain"], "late verification failure");
    assert!(
        report(&live).get("record_scope").is_none(),
        "live diagnostic schema is unchanged"
    );
    unchanged_payload(&live);
}

#[test]
fn replacement_candidate_during_failure_does_not_inherit_the_original_cause() {
    let root = tempfile::tempdir().unwrap();
    let live = root.path().join("live");
    fs::create_dir(&live).unwrap();
    let candidate = root.path().join("candidate");
    let saved = root.path().join("operator-saved");
    seed(&candidate);
    let publication = Finalization::new(&live, &candidate, 0, 0);
    let error = publication
        .run::<()>("validate_candidate", || {
            fs::rename(&candidate, &saved)?;
            seed(&candidate);
            Err(io::Error::new(io::ErrorKind::InvalidData, "original candidate failed").into())
        })
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<io::Error>().unwrap().to_string(),
        "original candidate failed"
    );
    for path in [&candidate, &saved] {
        assert_eq!(fs::read_dir(path).unwrap().count(), 2);
        unchanged_payload(path);
    }
}

#[test]
fn directory_created_after_capture_is_not_admitted_retroactively() {
    let root = tempfile::tempdir().unwrap();
    let live = root.path().join("live");
    fs::create_dir(&live).unwrap();
    let candidate = root.path().join("candidate");
    let publication = Finalization::new(&live, &candidate, 0, 0);
    seed(&candidate);
    failure(&publication, "validate_candidate", "unowned replacement");
    assert_eq!(fs::read_dir(&candidate).unwrap().count(), 2);
}

#[test]
fn existing_record_and_directory_obstructions_are_preserved_without_temporary_files() {
    for is_directory in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("live");
        fs::create_dir(&live).unwrap();
        let candidate = root.path().join("candidate");
        seed(&candidate);
        let destination = candidate.join(LAST_FAILURE_FILE);
        if is_directory {
            fs::create_dir(&destination).unwrap();
            fs::write(destination.join("sentinel"), b"keep").unwrap();
        } else {
            fs::write(&destination, b"malformed original evidence").unwrap();
        }
        failure(
            &Finalization::new(&live, &candidate, 0, 0),
            "commit_candidate",
            "later cause",
        );
        if is_directory {
            assert_eq!(fs::read(destination.join("sentinel")).unwrap(), b"keep");
        } else {
            assert_eq!(
                fs::read(destination).unwrap(),
                b"malformed original evidence"
            );
        }
        assert_eq!(fs::read_dir(&candidate).unwrap().count(), 3);
    }
}

#[test]
fn unicode_and_escaped_causes_are_bounded_without_changing_the_typed_error() {
    for cause in [
        "界".repeat(MAX_CAUSE_BYTES),
        "\0".repeat(MAX_CAUSE_BYTES * 2),
    ] {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("missing-live");
        let candidate = root.path().join("candidate");
        seed(&candidate);
        failure(
            &Finalization::new(&live, &candidate, 0, 0),
            "commit_candidate",
            &cause,
        );
        let record = report(&candidate);
        let saved = record["error_chain"].as_str().unwrap();
        assert!(saved.len() <= MAX_CAUSE_BYTES);
        assert!(cause.starts_with(saved));
        assert_eq!(record["error_chain_truncated"], true);
        assert_eq!(
            record["error_chain_original_bytes"].as_u64(),
            Some(cause.len() as u64)
        );
        assert!(
            fs::metadata(candidate.join(LAST_FAILURE_FILE))
                .unwrap()
                .len()
                <= MAX_RECORD_BYTES as u64
        );
        unchanged_payload(&candidate);
    }
}

#[test]
fn oversized_record_is_refused_before_any_candidate_write() {
    let root = tempfile::tempdir().unwrap();
    let live = root.path().join("live");
    let candidate = root.path().join("candidate");
    seed(&candidate);
    let publication = Finalization::new(&live, &candidate, 0, 0);
    let captured = publication
        .candidate_evidence
        .as_ref()
        .unwrap()
        .as_ref()
        .unwrap();
    assert!(
        captured
            .persist(&publication, &"x".repeat(MAX_RECORD_BYTES), "cause")
            .is_err()
    );
    assert_eq!(fs::read_dir(&candidate).unwrap().count(), 2);
    assert!(!live.exists());
}

#[test]
fn in_place_and_overlapping_paths_do_not_admit_a_candidate_report() {
    let root = tempfile::tempdir().unwrap();
    let live = root.path().join("live");
    seed(&live);
    assert!(Candidate::capture(&live, &live).unwrap().is_none());
    let nested = live.join("nested");
    seed(&nested);
    assert!(Candidate::capture(&live, &nested).is_err());
    assert!(Candidate::capture(&nested, &live).is_err());
    failure(
        &Finalization::new(&live, &live, 0, 0),
        "late_phase",
        "in place",
    );
    assert!(report(&live).get("record_scope").is_none());
    assert!(!nested.join(LAST_FAILURE_FILE).exists());
}

#[cfg(unix)]
#[test]
fn symlinked_candidates_parents_and_diagnostic_targets_are_not_followed() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("sentinel"), b"outside").unwrap();
    let live = root.path().join("live");
    fs::create_dir(&live).unwrap();
    let candidate = root.path().join("candidate");
    seed(&candidate);
    let alias = root.path().join("alias");
    symlink(&candidate, &alias).unwrap();
    assert!(Candidate::capture(&live, &alias).is_err());
    let alias_parent = root.path().join("alias-parent");
    symlink(root.path(), &alias_parent).unwrap();
    assert!(Candidate::capture(&live, &alias_parent.join("candidate")).is_err());
    symlink(
        outside.path().join("sentinel"),
        candidate.join(LAST_FAILURE_FILE),
    )
    .unwrap();
    failure(
        &Finalization::new(&live, &candidate, 0, 0),
        "commit_candidate",
        "target symlink",
    );
    assert!(
        fs::symlink_metadata(candidate.join(LAST_FAILURE_FILE))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        fs::read(outside.path().join("sentinel")).unwrap(),
        b"outside"
    );
    assert_eq!(fs::read_dir(&candidate).unwrap().count(), 3);
}

#[cfg(unix)]
#[test]
fn symlink_replacement_after_capture_is_refused_and_original_error_survives() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let live = root.path().join("live");
    fs::create_dir(&live).unwrap();
    let candidate = root.path().join("candidate");
    let saved = root.path().join("saved");
    seed(&candidate);
    let publication = Finalization::new(&live, &candidate, 0, 0);
    fs::rename(&candidate, &saved).unwrap();
    std::os::unix::fs::symlink(outside.path(), &candidate).unwrap();
    failure(
        &publication,
        "validate_candidate",
        "refused original candidate",
    );
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    assert!(!saved.join(LAST_FAILURE_FILE).exists());
    unchanged_payload(&saved);
}

#[test]
fn competing_diagnostics_preserve_one_whole_first_record() {
    let root = tempfile::tempdir().unwrap();
    let live = root.path().join("live");
    let candidate = root.path().join("candidate");
    fs::create_dir(&live).unwrap();
    seed(&candidate);
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        for cause in ["first contender", "second contender"] {
            let live = &live;
            let candidate = &candidate;
            let barrier = &barrier;
            scope.spawn(move || {
                let publication = Finalization::new(live, candidate, 0, 0);
                barrier.wait();
                failure(&publication, "commit_candidate", cause);
            });
        }
    });
    let record = report(&candidate);
    assert!(matches!(
        record["error_chain"].as_str(),
        Some("first contender" | "second contender")
    ));
    assert_eq!(record["record_scope"], "candidate_first_failure");
    assert_eq!(fs::read_dir(&candidate).unwrap().count(), 3);
    unchanged_payload(&candidate);
}
