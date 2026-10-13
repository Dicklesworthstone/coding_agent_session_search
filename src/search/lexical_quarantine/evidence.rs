//! Metadata-only pointers to known rebuild diagnostics. Never parse a candidate
//! checkpoint or failure report: it may be malformed, huge, or contain secrets.
//! Paths are relative to the bundle, including an interrupted cleanup wrapper.

use super::*;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    RebuildCheckpoint,
    FinalizationFailure,
    GenerationManifest,
    IndexManifest,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FailureEvidence {
    pub kind: EvidenceKind,
    /// An observed regular file under this bundle, not an external source path.
    pub relative_path: PathBuf,
    pub size_bytes: u64,
    pub modified_at_ms: Option<u64>,
}

/// Called only after observe() has checked the entry's type, link count,
/// filesystem and layout. This does not add any paths to the cleanup allowlist.
/// There are at most six matching paths per bundle; no second walk or read.
pub(super) fn observe_file(
    relative: &Path,
    pending: bool,
    metadata: &fs::Metadata,
) -> Option<FailureEvidence> {
    if !metadata.is_file() {
        return None;
    }
    let path = if pending {
        relative.strip_prefix("quarantine").ok()?
    } else {
        relative
    };
    let kind = if path == Path::new(CHECKPOINT)
        || path == Path::new("index/.lexical-rebuild-state.json")
    {
        EvidenceKind::RebuildCheckpoint
    } else if path == Path::new("index/.lexical-rebuild-last-failure.json") {
        EvidenceKind::FinalizationFailure
    } else if path == Path::new("index/lexical-generation-manifest.json") {
        EvidenceKind::GenerationManifest
    } else if path == Path::new("index/MANIFEST") || path == Path::new("index/meta.json") {
        EvidenceKind::IndexManifest
    } else {
        return None;
    };
    let modified_at_ms = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .and_then(|age| u64::try_from(age.as_millis()).ok());
    Some(FailureEvidence {
        kind,
        relative_path: relative.to_owned(),
        size_bytes: metadata.len(),
        modified_at_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inventory(root: &Path) -> Inventory {
        inspect_at(root, SystemTime::now(), true, Limits::default()).unwrap()
    }

    #[test]
    fn known_diagnostics_are_enumerated_without_parsing_or_leaking_payloads() {
        for pending in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let prefix = if pending { RECLAIM_PREFIX } else { QUARANTINE_PREFIX };
            let bundle = root.path().join(format!("{prefix}000001"));
            let layout = if pending { bundle.join("quarantine") } else { bundle.clone() };
            fs::create_dir_all(layout.join("index")).unwrap();
            let paths = [
                (CHECKPOINT, EvidenceKind::RebuildCheckpoint),
                ("index/.lexical-rebuild-state.json", EvidenceKind::RebuildCheckpoint),
                ("index/.lexical-rebuild-last-failure.json", EvidenceKind::FinalizationFailure),
                ("index/lexical-generation-manifest.json", EvidenceKind::GenerationManifest),
                ("index/MANIFEST", EvidenceKind::IndexManifest),
                ("index/meta.json", EvidenceKind::IndexManifest),
            ];
            for (path, _) in paths {
                fs::write(layout.join(path), b"{malformed private diagnostic").unwrap();
            }
            fs::write(layout.join("index/segment-data"), b"private indexed content").unwrap();
            // An unrelated live failure is not evidence for this candidate.
            fs::write(root.path().join(".lexical-rebuild-last-failure.json"), b"other failure").unwrap();
            let report = inventory(root.path());
            assert!(report.complete, "{:?}", report.warnings);
            let artifact = &report.artifacts[0];
            assert_eq!(artifact.failure_evidence.len(), paths.len());
            for (path, kind) in paths {
                let relative = if pending { Path::new("quarantine").join(path) } else { PathBuf::from(path) };
                let evidence = artifact.failure_evidence.iter().find(|item| item.relative_path == relative).unwrap();
                assert_eq!(evidence.kind, kind);
                assert_eq!(evidence.size_bytes, b"{malformed private diagnostic".len() as u64);
                assert!(evidence.modified_at_ms.is_some());
                assert_eq!(fs::read(bundle.join(&evidence.relative_path)).unwrap(), b"{malformed private diagnostic");
            }
            assert!(artifact.failure_evidence.windows(2).all(|pair| pair[0].relative_path < pair[1].relative_path));
            let json = serde_json::to_string(&report).unwrap();
            assert!(!json.contains("private diagnostic"));
            assert!(!json.contains("private indexed content"));
            assert!(!json.contains("other failure"));
            assert!(!root.path().join("index-run.lock").exists());
        }
    }

    #[test]
    fn oversized_failure_report_is_only_statted_and_nested_impostors_are_not_reported() {
        let root = tempfile::tempdir().unwrap();
        let bundle = root.path().join(format!("{QUARANTINE_PREFIX}000001"));
        fs::create_dir_all(bundle.join("index/nested")).unwrap();
        let failure = bundle.join("index/.lexical-rebuild-last-failure.json");
        let file = fs::File::create(&failure).unwrap();
        file.set_len(32 * 1024 * 1024).unwrap();
        fs::write(bundle.join("index/nested/.lexical-rebuild-last-failure.json"), b"not a root diagnostic").unwrap();
        let report = inventory(root.path());
        assert!(report.complete);
        assert_eq!(report.artifacts[0].failure_evidence.len(), 1);
        assert_eq!(report.artifacts[0].failure_evidence[0].size_bytes, 32 * 1024 * 1024);
        assert_eq!(file.metadata().unwrap().len(), 32 * 1024 * 1024);
    }

    #[test]
    fn incomplete_scan_never_reports_a_complete_evidence_catalog() {
        let root = tempfile::tempdir().unwrap();
        let bundle = root.path().join(format!("{QUARANTINE_PREFIX}000001"));
        fs::create_dir_all(bundle.join("index")).unwrap();
        fs::write(bundle.join("index/MANIFEST"), b"damaged").unwrap();
        let report = inspect_at(root.path(), SystemTime::now(), true,
            Limits { tree_entries: 1, ..Default::default() }).unwrap();
        assert!(!report.complete);
        assert!(!report.artifacts[0].size_complete);
        assert!(report.artifacts[0].failure_evidence.is_empty());
        assert_eq!(report.artifacts[0].disposition, Disposition::Inspect);
        assert_eq!(report.eligible_bytes, 0);
    }

    #[cfg(unix)]
    #[test]
    fn diagnostic_symlinks_are_not_followed_or_mistaken_for_failure_evidence() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let sentinel = outside.path().join("secret");
        fs::write(&sentinel, b"outside sentinel").unwrap();
        let bundle = root.path().join(format!("{QUARANTINE_PREFIX}000001"));
        fs::create_dir_all(bundle.join("index")).unwrap();
        std::os::unix::fs::symlink(&sentinel, bundle.join("index/.lexical-rebuild-last-failure.json")).unwrap();
        let report = inventory(root.path());
        assert!(!report.complete);
        assert!(report.artifacts[0].failure_evidence.is_empty());
        assert_eq!(report.eligible_bytes, 0);
        assert_eq!(fs::read(sentinel).unwrap(), b"outside sentinel");
    }
}
