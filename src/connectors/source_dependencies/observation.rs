//! Filesystem evidence for source-ledger reuse. Independent of connector discovery
//! so the exact production matcher can be tested without the search/UI runtime.

use serde_json::Value;
use std::path::Path;

pub(crate) fn file_observation(path: &Path) -> Option<Value> {
    match std::fs::metadata(path) {
        Ok(metadata) => {
            let observation = serde_json::json!({
                "path": path, "size": metadata.len(),
                "mtime_ns": metadata.modified().ok()?.duration_since(std::time::UNIX_EPOCH)
                    .ok()?.as_nanos().to_string(),
            });
            #[cfg(unix)]
            let observation = {
                use std::os::unix::fs::MetadataExt;
                let mut observation = observation;
                observation["device"] = serde_json::json!(metadata.dev());
                observation["inode"] = serde_json::json!(metadata.ino());
                observation["ctime"] = serde_json::json!([metadata.ctime(), metadata.ctime_nsec()]);
                observation
            };
            Some(observation)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Some(serde_json::json!({"path": path, "absent": true}))
        }
        Err(_) => None,
    }
}

pub(crate) fn ledger_matches(
    observation: &str,
    source_path: &Path,
    observes_parent_directory: bool,
    producer_contract: &str,
) -> bool {
    let Ok(saved) = serde_json::from_str::<Value>(observation) else {
        return false;
    };
    if saved["producer_contract"].as_str() != Some(producer_contract) {
        return false;
    }
    // Missing or unreadable primaries cannot prove a completed parse. Do not
    // turn an observation error into JSON null (which a corrupt row can match).
    let Some(primary) = file_observation(source_path) else {
        return false;
    };
    if primary["absent"].as_bool() == Some(true) || saved["primary"] != primary {
        return false;
    }
    let Some(files) = saved["dependencies"].as_array() else {
        return false;
    };
    if observes_parent_directory
        && !files.iter().any(|file| {
            file["path"]
                .as_str()
                .is_some_and(|path| Some(Path::new(path)) == source_path.parent())
        })
    {
        // A sidecar-aware parse is not reusable without its directory evidence,
        // even if all explicitly listed files (or an empty list) still match.
        return false;
    }
    // Old rows retain their exact shape. Only a self-contained connector's
    // implicit immediate parent is irrelevant; explicit files remain evidence,
    // including absence observations for sidecars that have not appeared yet.
    let ignored_parent = (!observes_parent_directory)
        .then(|| source_path.parent())
        .flatten();
    files.iter().all(|file| {
        file["path"].as_str().is_some_and(|path| {
            Some(Path::new(path)) == ignored_parent
                || file_observation(Path::new(path)).as_ref() == Some(file)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::hint::black_box;
    use std::time::Instant;
    use tempfile::TempDir;

    const CONTRACT: &str = "fixture-producer-contract";

    fn saved(path: &Path, dependencies: Vec<Value>) -> String {
        serde_json::json!({"primary": file_observation(path),
            "producer_contract": CONTRACT, "dependencies": dependencies})
        .to_string()
    }

    fn grow(path: &Path, prefix: &str) {
        let before = file_observation(path).unwrap();
        for index in 0..1024 {
            fs::write(path.join(format!("{prefix}-{index}")), b"new sibling").unwrap();
            if file_observation(path).unwrap() != before {
                return;
            }
        }
        panic!("fixture could not produce an observable directory change");
    }

    #[test]
    fn legacy_parent_is_ignored_only_when_connector_declares_it_irrelevant() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("primary");
        fs::write(&path, b"transcript").unwrap();
        let row = saved(&path, vec![file_observation(temp.path()).unwrap()]);
        assert!(ledger_matches(&row, &path, true, CONTRACT));
        grow(temp.path(), "sibling");
        assert!(ledger_matches(&row, &path, false, CONTRACT));
        assert!(!ledger_matches(&row, &path, true, CONTRACT));
        fs::write(&path, b"new primary message").unwrap();
        assert!(!ledger_matches(&row, &path, false, CONTRACT));
    }

    #[test]
    fn explicit_absence_and_existing_sidecars_remain_dependencies() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("primary");
        let sidecar = temp.path().join("optional-sidecar");
        fs::write(&path, b"transcript").unwrap();
        let row = saved(&path, vec![file_observation(&sidecar).unwrap()]);
        assert!(ledger_matches(&row, &path, false, CONTRACT));
        fs::write(&sidecar, b"appeared").unwrap();
        assert!(!ledger_matches(&row, &path, false, CONTRACT));
        let row = saved(&path, vec![file_observation(&sidecar).unwrap()]);
        fs::write(&sidecar, b"changed content").unwrap();
        assert!(!ledger_matches(&row, &path, false, CONTRACT));
    }

    #[test]
    fn missing_or_unreadable_primary_never_matches_a_malformed_row() {
        let temp = TempDir::new().unwrap();
        let missing = temp.path().join("missing");
        let row = saved(&missing, vec![]);
        assert!(!ledger_matches(&row, &missing, false, CONTRACT));
        // NUL is refused by the filesystem API without privileged chmod tests.
        let unreadable = Path::new("invalid\0path");
        assert!(file_observation(unreadable).is_none());
        let row = serde_json::json!({"producer_contract": CONTRACT,
            "primary": null, "dependencies": []})
        .to_string();
        assert!(!ledger_matches(&row, unreadable, false, CONTRACT));
    }

    #[test]
    fn malformed_dependencies_and_producer_changes_fail_closed() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("primary");
        fs::write(&path, b"transcript").unwrap();
        let row = saved(&path, vec![]);
        assert!(!ledger_matches(&row, &path, false, "other-producer"));
        for malformed in ["null", "{}", "{", "[]"] {
            assert!(!ledger_matches(malformed, &path, false, CONTRACT));
        }
        let row = saved(&path, vec![serde_json::json!({"path": 7})]);
        assert!(!ledger_matches(&row, &path, false, CONTRACT));
    }

    #[test]
    fn sidecar_aware_reuse_requires_the_parent_observation() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("primary");
        let sidecar = temp.path().join("metadata");
        fs::write(&path, b"transcript").unwrap();
        fs::write(&sidecar, b"metadata").unwrap();
        for dependencies in [vec![], vec![file_observation(&sidecar).unwrap()]] {
            let row = saved(&path, dependencies);
            assert!(ledger_matches(&row, &path, false, CONTRACT));
            assert!(!ledger_matches(&row, &path, true, CONTRACT));
            // Even unchanged explicitly listed files cannot prove that some
            // previously absent optional file has not appeared in the folder.
            grow(temp.path(), "new-optional-dependency");
            assert!(!ledger_matches(&row, &path, true, CONTRACT));
        }
        let row = saved(&path, vec![file_observation(temp.path()).unwrap()]);
        assert!(ledger_matches(&row, &path, true, CONTRACT));
    }

    #[test]
    fn removing_a_previously_certified_primary_invalidates_every_policy() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("primary");
        fs::write(&path, b"transcript").unwrap();
        let row = saved(&path, vec![file_observation(temp.path()).unwrap()]);
        fs::remove_file(&path).unwrap();
        for observes_parent in [false, true] {
            assert!(!ledger_matches(&row, &path, observes_parent, CONTRACT));
        }
    }

    // Pre-GH512 decision logic, kept live in the SAME benchmark invocation.
    // Only the input adapter differs: it receives the source path directly.
    fn incumbent(observation: &str, path: &Path) -> bool {
        let Ok(saved) = serde_json::from_str::<Value>(observation) else {
            return false;
        };
        if saved["producer_contract"].as_str() != Some(CONTRACT) {
            return false;
        }
        if saved["primary"] != file_observation(path).unwrap_or_default() {
            return false;
        }
        let Some(files) = saved["dependencies"].as_array() else {
            return false;
        };
        files.iter().all(|file| {
            file["path"]
                .as_str()
                .is_some_and(|path| file_observation(Path::new(path)).as_ref() == Some(file))
        })
    }

    #[test]
    #[ignore = "native workload benchmark; explicit --ignored --nocapture"]
    fn gh512_benchmark_4159_sources_10_folders() {
        const SOURCES: usize = 4159;
        const FOLDERS: usize = 10;
        const ROUNDS: usize = 7;
        let temp = TempDir::new().unwrap();
        let folders: Vec<_> = (0..FOLDERS)
            .map(|index| {
                let path = temp.path().join(format!("subagents-{index}"));
                fs::create_dir(&path).unwrap();
                path
            })
            .collect();
        let paths: Vec<_> = (0..SOURCES)
            .map(|index| {
                let path = folders[index % FOLDERS].join(format!("agent-{index}.jsonl"));
                fs::write(
                    &path,
                    b"{\"type\":\"user\",\"message\":{\"content\":\"benchmark\"}}\n",
                )
                .unwrap();
                path
            })
            .collect();
        let rows: Vec<_> = paths
            .iter()
            .map(|path| {
                saved(
                    path,
                    vec![file_observation(path.parent().unwrap()).unwrap()],
                )
            })
            .collect();
        for (path, row) in paths.iter().zip(&rows) {
            assert!(incumbent(row, path));
            assert!(ledger_matches(row, path, false, CONTRACT));
        }
        for folder in &folders {
            grow(folder, "new-transcript");
        }
        let mut old_times = Vec::new();
        let mut new_times = Vec::new();
        let mut old_counts = Vec::new();
        let mut new_counts = Vec::new();
        for round in 0..ROUNDS {
            for old in [round % 2 == 0, round % 2 != 0] {
                let start = Instant::now();
                let reused = paths
                    .iter()
                    .zip(&rows)
                    .filter(|(path, row)| {
                        if old {
                            incumbent(black_box(row), black_box(path))
                        } else {
                            ledger_matches(black_box(row), black_box(path), false, CONTRACT)
                        }
                    })
                    .count();
                let elapsed = start.elapsed().as_nanos();
                if old {
                    old_times.push(elapsed);
                    old_counts.push(reused);
                    assert_eq!(reused, 0);
                } else {
                    new_times.push(elapsed);
                    new_counts.push(reused);
                    assert_eq!(reused, SOURCES);
                }
            }
        }
        assert!(
            paths
                .iter()
                .zip(&rows)
                .all(|(path, row)| !ledger_matches(row, path, true, CONTRACT))
        );
        old_times.sort_unstable();
        new_times.sort_unstable();
        println!(
            "GH512_NATIVE_MATCHER_BENCH {}",
            serde_json::json!({
                "sources": SOURCES, "growing_folders": FOLDERS, "rounds": ROUNDS,
                "incumbent_reuse_counts": old_counts, "candidate_reuse_counts": new_counts,
                "incumbent_median_ns": old_times[ROUNDS / 2],
                "candidate_median_ns": new_times[ROUNDS / 2],
                "incumbent_samples_ns": old_times, "candidate_samples_ns": new_times,
                "debug_assertions": cfg!(debug_assertions),
                "scope": "production filesystem matcher; excludes discovery, registry, storage and parsing",
            })
        );
    }
}
