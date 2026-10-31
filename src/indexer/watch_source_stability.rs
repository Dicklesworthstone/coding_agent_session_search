//! Distinguish a source changing during parsing from unavailable receipt evidence.

use super::{DiscoveredSourceFile, source_file_observation, source_folder_is_dependency};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

fn observation_changed(path: &Path, before: Option<&Value>) -> bool {
    // No earlier observation means that a comparison is unavailable, including
    // native paths the JSON receipt cannot encode. Once evidence was observed,
    // losing it (or observing an absent file) cannot certify the same input.
    before.is_some_and(|before| source_file_observation(path).as_ref() != Some(before))
}

fn discovery_metadata_changed(source: &DiscoveredSourceFile) -> bool {
    // FAD compares against None even when discovery never obtained metadata,
    // and reports lookup failures as changed. Require a successful discovery
    // snapshot first; otherwise a stable unsupported input would retry forever.
    source.size_bytes.is_some() && source.fs_metadata_changed()
}

pub(super) fn observed_source_changed(
    source: &DiscoveredSourceFile,
    sidecars: &[DiscoveredSourceFile],
    primary_before: Option<&Value>,
    parent_before: Option<&Value>,
    dependencies_before: &HashMap<PathBuf, Value>,
) -> bool {
    if discovery_metadata_changed(source)
        || observation_changed(&source.source_path, primary_before)
    {
        return true;
    }
    if source_folder_is_dependency(source)
        && source
            .source_path
            .parent()
            .is_some_and(|parent| observation_changed(parent, parent_before))
    {
        return true;
    }
    // Explicit sidecars remain dependencies even for a normally self-contained
    // provider. An unobserved sidecar with no metadata does not prove mutation.
    sidecars.iter().any(|sidecar| {
        discovery_metadata_changed(sidecar)
            || observation_changed(
                &sidecar.source_path,
                dependencies_before.get(&sidecar.source_path),
            )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::{DiscoveredSourceRole, ScanRoot};
    use std::fs;
    use tempfile::TempDir;

    fn source(path: &Path, slug: &str) -> DiscoveredSourceFile {
        DiscoveredSourceFile::new(
            slug,
            &ScanRoot::local(path.parent().unwrap().to_path_buf()),
            path.to_path_buf(),
            DiscoveredSourceRole::PrimarySessionLog,
            true,
        )
    }

    fn grow_directory(path: &Path) {
        let before = source_file_observation(path).unwrap();
        for index in 0..1024 {
            fs::write(path.join(format!("new-sibling-{index}")), b"new input").unwrap();
            if source_file_observation(path).as_ref() != Some(&before) {
                return;
            }
        }
        panic!("fixture did not produce an observable directory change");
    }

    #[test]
    fn gh538_stable_primary_and_sidecars_do_not_request_retry() {
        let temp = TempDir::new().unwrap();
        let primary_path = temp.path().join("primary");
        let sidecar_path = temp.path().join("sidecar");
        fs::write(&primary_path, b"transcript").unwrap();
        fs::write(&sidecar_path, b"metadata").unwrap();
        let primary = source_file_observation(&primary_path);
        let parent = source_file_observation(temp.path());
        let dependencies = HashMap::from([(
            sidecar_path.clone(),
            source_file_observation(&sidecar_path).unwrap(),
        )]);
        for slug in ["claude_code", "codex", "shelley", "unknown-provider"] {
            assert!(!observed_source_changed(
                &source(&primary_path, slug).with_fs_metadata(),
                &[source(&sidecar_path, slug).with_fs_metadata()],
                primary.as_ref(),
                parent.as_ref(),
                &dependencies,
            ));
        }
    }

    #[test]
    fn gh538_primary_mutation_and_disappearance_request_retry() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("primary");
        fs::write(&path, b"original").unwrap();
        let original = source(&path, "codex").with_fs_metadata();
        let before = source_file_observation(&path);
        fs::write(&path, b"original plus appended content").unwrap();
        assert!(observed_source_changed(
            &original,
            &[],
            before.as_ref(),
            None,
            &HashMap::new(),
        ));

        let appended = source(&path, "codex").with_fs_metadata();
        let before = source_file_observation(&path);
        fs::rename(&path, temp.path().join("moved-primary")).unwrap();
        assert!(observed_source_changed(
            &appended,
            &[],
            before.as_ref(),
            None,
            &HashMap::new(),
        ));
    }

    #[cfg(unix)]
    #[test]
    fn gh538_replacement_is_changed_even_when_size_and_mtime_match() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("primary");
        fs::write(&path, b"original").unwrap();
        let original = source(&path, "codex").with_fs_metadata();
        let before = source_file_observation(&path);
        let mtime = fs::metadata(&path).unwrap().modified().unwrap();
        let replacement = temp.path().join("replacement");
        fs::write(&replacement, b"replaced").unwrap();
        fs::File::open(&replacement)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        fs::rename(&replacement, &path).unwrap();
        assert!(!original.fs_metadata_changed());
        assert!(observed_source_changed(
            &original,
            &[],
            before.as_ref(),
            None,
            &HashMap::new(),
        ));
    }

    #[test]
    fn gh538_parent_changes_only_matter_for_directory_dependency_policies() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("primary");
        fs::write(&path, b"stable transcript").unwrap();
        let primary = source_file_observation(&path);
        let parent = source_file_observation(temp.path());
        let sources: Vec<_> = ["claude_code", "codex", "shelley", "unknown-provider"]
            .into_iter()
            .map(|slug| source(&path, slug).with_fs_metadata())
            .collect();
        grow_directory(temp.path());
        for source in sources {
            assert_eq!(
                observed_source_changed(
                    &source,
                    &[],
                    primary.as_ref(),
                    parent.as_ref(),
                    &HashMap::new(),
                ),
                source_folder_is_dependency(&source),
                "{}",
                source.provider_slug,
            );
        }
    }

    #[test]
    fn gh538_declared_sidecar_mutation_and_appearance_request_retry() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("primary");
        let sidecar_path = temp.path().join("sidecar");
        fs::write(&path, b"stable transcript").unwrap();
        fs::write(&sidecar_path, b"metadata").unwrap();
        let primary = source_file_observation(&path);
        let parent = source_file_observation(temp.path());
        let dependencies = HashMap::from([(
            sidecar_path.clone(),
            source_file_observation(&sidecar_path).unwrap(),
        )]);
        let sidecar = source(&sidecar_path, "codex").with_fs_metadata();
        fs::write(&sidecar_path, b"changed reconstruction metadata").unwrap();
        assert!(observed_source_changed(
            &source(&path, "codex").with_fs_metadata(),
            &[sidecar],
            primary.as_ref(),
            parent.as_ref(),
            &dependencies,
        ));

        let future = temp.path().join("future-sidecar");
        let absent = source(&future, "codex").with_fs_metadata();
        let dependencies =
            HashMap::from([(future.clone(), source_file_observation(&future).unwrap())]);
        assert!(absent.size_bytes.is_none());
        fs::write(&future, b"new optional sidecar").unwrap();
        assert!(observed_source_changed(
            &source(&path, "codex").with_fs_metadata(),
            &[absent],
            primary.as_ref(),
            parent.as_ref(),
            &dependencies,
        ));
    }

    #[test]
    fn gh538_unavailable_evidence_alone_does_not_request_retry() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("primary");
        let sidecar_path = temp.path().join("unobserved-sidecar");
        fs::write(&path, b"transcript").unwrap();
        fs::write(&sidecar_path, b"metadata").unwrap();
        let unobserved = source(&path, "unknown-provider");
        assert!(unobserved.fs_metadata_changed());
        assert!(!observed_source_changed(
            &unobserved,
            &[source(&sidecar_path, "unknown-provider")],
            None,
            None,
            &HashMap::new(),
        ));
        assert!(!observed_source_changed(
            &source(&temp.path().join("absent-primary"), "unknown-provider").with_fs_metadata(),
            &[source(&temp.path().join("absent-sidecar"), "unknown-provider").with_fs_metadata()],
            None,
            None,
            &HashMap::new(),
        ));
    }

    #[cfg(unix)]
    #[test]
    fn gh538_losing_a_previously_available_observation_requests_retry() {
        let temp = TempDir::new().unwrap();
        let directory = temp.path().join("source-directory");
        fs::create_dir(&directory).unwrap();
        let path = directory.join("primary");
        fs::write(&path, b"transcript").unwrap();
        let before = source_file_observation(&path);
        assert!(before.is_some());
        // No discovery metadata: this specifically exercises loss of the
        // already-recorded JSON observation, rather than FAD's metadata check.
        let unobserved = source(&path, "codex");
        fs::rename(&directory, temp.path().join("moved-directory")).unwrap();
        fs::write(&directory, b"directory replaced by a file").unwrap();
        assert!(source_file_observation(&path).is_none());
        assert!(observed_source_changed(
            &unobserved,
            &[],
            before.as_ref(),
            None,
            &HashMap::new(),
        ));
    }

    #[cfg(unix)]
    #[test]
    fn gh538_non_unicode_sources_retry_only_for_observed_metadata_changes() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let temp = TempDir::new().unwrap();
        let path = temp
            .path()
            .join(OsString::from_vec(b"primary-\xff".to_vec()));
        let sidecar_path = temp
            .path()
            .join(OsString::from_vec(b"sidecar-\xfe".to_vec()));
        fs::write(&path, b"transcript").unwrap();
        fs::write(&sidecar_path, b"metadata").unwrap();
        let original = source(&path, "codex").with_fs_metadata();
        let sidecar = source(&sidecar_path, "codex").with_fs_metadata();
        assert!(source_file_observation(&path).is_none());
        assert!(source_file_observation(&sidecar_path).is_none());
        assert!(!observed_source_changed(
            &original,
            std::slice::from_ref(&sidecar),
            None,
            None,
            &HashMap::new(),
        ));
        fs::write(&path, b"transcript with appended content").unwrap();
        assert!(observed_source_changed(
            &original,
            std::slice::from_ref(&sidecar),
            None,
            None,
            &HashMap::new(),
        ));
        let current = source(&path, "codex").with_fs_metadata();
        assert!(!observed_source_changed(
            &current,
            std::slice::from_ref(&sidecar),
            None,
            None,
            &HashMap::new(),
        ));
        fs::write(&sidecar_path, b"changed metadata").unwrap();
        assert!(observed_source_changed(
            &current,
            &[sidecar],
            None,
            None,
            &HashMap::new(),
        ));
    }
}
