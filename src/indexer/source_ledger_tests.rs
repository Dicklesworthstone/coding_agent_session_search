//! GH512: use the production match and completion helpers, including a live
//! pre-fix comparator in the workload-shaped ignored benchmark.

use super::*;
use std::hint::black_box;
use std::sync::{Arc, Barrier};
use tempfile::TempDir;

fn discovered(path: &Path, slug: &str) -> DiscoveredSourceFile {
    DiscoveredSourceFile::new(
        slug,
        &ScanRoot::local(path.parent().unwrap().to_path_buf()),
        path.to_path_buf(),
        crate::connectors::DiscoveredSourceRole::PrimarySessionLog,
        true,
    )
    .with_fs_metadata()
}

fn saved_row(source: &DiscoveredSourceFile, dependencies: Vec<serde_json::Value>) -> String {
    serde_json::json!({
        "primary": source_file_observation(&source.source_path).unwrap(),
        "producer_contract": env!("CASS_SOURCE_INGEST_CONTRACT"),
        "dependencies": dependencies,
    })
    .to_string()
}

fn legacy_row(source: &DiscoveredSourceFile) -> String {
    saved_row(
        source,
        vec![source_file_observation(source.source_path.parent().unwrap()).unwrap()],
    )
}

/// No timestamp sleeps: require an observed directory change before testing
/// invalidation. Unique files also force directory growth on coarse clocks.
fn grow_directory(path: &Path, prefix: &str) {
    let before = source_file_observation(path).unwrap();
    for index in 0..1024 {
        fs::write(path.join(format!("{prefix}-{index}.jsonl")), b"new sibling").unwrap();
        if source_file_observation(path).unwrap() != before {
            return;
        }
    }
    panic!("fixture could not produce observable directory growth");
}

#[test]
fn gh512_legacy_rows_reuse_immediately_without_ignoring_explicit_dependencies() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("agent-a.jsonl");
    fs::write(&path, b"primary").unwrap();
    let rows: Vec<_> = ["claude_code", "codex", "shelley", "unknown-provider"]
        .into_iter()
        .map(|slug| {
            let source = discovered(&path, slug);
            let row = legacy_row(&source);
            assert!(source_ledger_matches(&row, &source));
            (source, row)
        })
        .collect();
    grow_directory(temp.path(), "sibling");
    for (source, row) in rows {
        assert_eq!(
            source_ledger_matches(&row, &source),
            matches!(source.provider_slug.as_str(), "claude_code" | "codex"),
            "{}",
            source.provider_slug,
        );
    }
    let source = discovered(&path, "claude_code");
    let sidecar = temp.path().join("explicit-metadata");
    fs::write(&sidecar, b"metadata").unwrap();
    let row = saved_row(&source, vec![source_file_observation(&sidecar).unwrap()]);
    fs::write(&sidecar, b"changed metadata").unwrap();
    assert!(!source_ledger_matches(&row, &source));
    let absent = temp.path().join("future-sidecar");
    let row = saved_row(&source, vec![source_file_observation(&absent).unwrap()]);
    fs::write(&absent, b"appeared").unwrap();
    assert!(!source_ledger_matches(&row, &source));
}

#[test]
fn gh512_primary_mutation_rejects_both_reuse_and_completion() {
    for slug in ["claude_code", "codex", "shelley"] {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("session.jsonl");
        fs::write(&path, b"original").unwrap();
        let source = discovered(&path, slug);
        let primary = source_file_observation(&path).unwrap();
        let parent = source_parent_observation(&source);
        let legacy = legacy_row(&source);
        let modern = saved_row(&source, Vec::new());
        fs::write(&path, b"changed primary transcript").unwrap();
        assert!(!source_ledger_matches(&legacy, &source));
        assert!(!source_ledger_matches(&modern, &source));
        assert!(
            source_observations_after_scan(
                &source,
                &[],
                Some(&primary),
                parent.as_ref(),
                &HashMap::new()
            )
            .is_none()
        );
    }
}

#[cfg(unix)]
#[test]
fn gh512_primary_replacement_with_same_size_and_mtime_is_not_a_sibling_change() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("session.jsonl");
    fs::write(&path, b"original").unwrap();
    let source = discovered(&path, "codex");
    let row = legacy_row(&source);
    let original_mtime = fs::metadata(&path).unwrap().modified().unwrap();
    let replacement = temp.path().join("replacement");
    fs::write(&replacement, b"replaced").unwrap();
    File::open(&replacement)
        .unwrap()
        .set_modified(original_mtime)
        .unwrap();
    fs::rename(&replacement, &path).unwrap();
    assert!(!source_ledger_matches(&row, &source));
}

#[test]
fn gh512_new_optional_sidecar_invalidates_even_when_previous_completion_had_none() {
    for slug in ["shelley", "kiro", "grok", "unknown-provider"] {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("sessions.sqlite3");
        fs::write(&path, b"database").unwrap();
        let source = discovered(&path, slug);
        let primary = source_file_observation(&path).unwrap();
        let parent = source_parent_observation(&source);
        let (_, dependencies) = source_observations_after_scan(
            &source,
            &[],
            Some(&primary),
            parent.as_ref(),
            &HashMap::new(),
        )
        .unwrap();
        assert_eq!(dependencies.len(), 1);
        let row = saved_row(&source, dependencies);
        fs::write(temp.path().join("sessions.sqlite3-wal"), b"new WAL").unwrap();
        grow_directory(temp.path(), "directory-clock");
        assert!(!source_ledger_matches(&row, &source));
        assert!(
            source_observations_after_scan(
                &source,
                &[],
                Some(&primary),
                parent.as_ref(),
                &HashMap::new()
            )
            .is_none()
        );
    }
}

#[test]
fn gh512_declared_sidecar_mutation_withholds_completion_for_every_policy() {
    for slug in ["claude_code", "codex", "shelley"] {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("primary");
        let sidecar_path = temp.path().join("sidecar");
        fs::write(&path, b"primary").unwrap();
        fs::write(&sidecar_path, b"sidecar").unwrap();
        let source = discovered(&path, slug);
        let sidecar = discovered(&sidecar_path, slug);
        let primary = source_file_observation(&path).unwrap();
        let parent = source_parent_observation(&source);
        let dependencies_before = HashMap::from([(
            sidecar_path.clone(),
            source_file_observation(&sidecar_path).unwrap(),
        )]);
        fs::write(&sidecar_path, b"mutated sidecar content").unwrap();
        assert!(
            source_observations_after_scan(
                &source,
                &[sidecar],
                Some(&primary),
                parent.as_ref(),
                &dependencies_before
            )
            .is_none()
        );
    }
}

#[test]
fn gh512_unobserved_sidecars_cannot_certify_a_completion() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("primary");
    let sidecar_path = temp.path().join("sidecar");
    fs::write(&path, b"primary").unwrap();
    fs::write(&sidecar_path, b"sidecar").unwrap();
    let source = discovered(&path, "claude_code");
    let sidecar = discovered(&sidecar_path, "claude_code");
    let primary = source_file_observation(&path).unwrap();
    assert!(
        source_observations_after_scan(
            &source,
            std::slice::from_ref(&sidecar),
            Some(&primary),
            None,
            &HashMap::new()
        )
        .is_none()
    );
    let dependencies_before = HashMap::from([(
        sidecar_path.clone(),
        source_file_observation(&sidecar_path).unwrap(),
    )]);
    let (_, dependencies) = source_observations_after_scan(
        &source,
        &[sidecar],
        Some(&primary),
        None,
        &dependencies_before,
    )
    .unwrap();
    assert_eq!(
        dependencies,
        vec![source_file_observation(&sidecar_path).unwrap()]
    );
}

#[test]
fn gh512_concurrent_folder_growth_preserves_only_self_contained_completions() {
    for slug in ["claude_code", "codex", "shelley", "unknown-provider"] {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("primary");
        fs::write(&path, b"unchanged source").unwrap();
        let source = discovered(&path, slug);
        let primary = source_file_observation(&path).unwrap();
        let parent = source_parent_observation(&source);
        let barrier = Arc::new(Barrier::new(2));
        let writer_barrier = Arc::clone(&barrier);
        let directory = temp.path().to_path_buf();
        let writer = thread::spawn(move || {
            writer_barrier.wait();
            grow_directory(&directory, "concurrent");
            writer_barrier.wait();
        });
        barrier.wait();
        barrier.wait();
        let observations = source_observations_after_scan(
            &source,
            &[],
            Some(&primary),
            parent.as_ref(),
            &HashMap::new(),
        );
        writer.join().unwrap();
        if matches!(slug, "claude_code" | "codex") {
            assert!(
                parent.is_none(),
                "self-contained scans must not stat the parent"
            );
            let (_, dependencies) =
                observations.expect("folder growth is unrelated to this source");
            assert!(dependencies.is_empty());
            let row = saved_row(&source, dependencies);
            grow_directory(temp.path(), "later");
            assert!(source_ledger_matches(&row, &source));
        } else {
            assert!(
                observations.is_none(),
                "an optional sidecar may have appeared"
            );
        }
    }
}

#[test]
fn gh512_legacy_handling_keeps_producer_and_malformed_row_guards() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("primary");
    fs::write(&path, b"primary").unwrap();
    let source = discovered(&path, "claude_code");
    let row = legacy_row(&source);
    let mut value: serde_json::Value = serde_json::from_str(&row).unwrap();
    value["producer_contract"] = serde_json::json!("unrelated-parser-contract");
    assert!(!source_ledger_matches(&value.to_string(), &source));
    for malformed in ["{}", "null", "{", r#"{"dependencies":[]}"#] {
        assert!(!source_ledger_matches(malformed, &source));
    }
    value = serde_json::from_str(&row).unwrap();
    value["dependencies"] = serde_json::json!([{"path": 7}]);
    assert!(!source_ledger_matches(&value.to_string(), &source));
}

#[test]
#[ignore = "GH512 workload-shaped native reuse benchmark; run explicitly with --ignored --nocapture"]
fn gh512_benchmark_4159_unchanged_sources_in_10_growing_folders() {
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
    let sources: Vec<_> = (0..SOURCES)
        .map(|index| {
            let path = folders[index % FOLDERS].join(format!("agent-{index}.jsonl"));
            fs::write(
                &path,
                b"{\"type\":\"user\",\"message\":{\"content\":\"benchmark\"}}\n",
            )
            .unwrap();
            discovered(
                &path,
                if index % 2 == 0 {
                    "claude_code"
                } else {
                    "codex"
                },
            )
        })
        .collect();
    let rows: Vec<_> = sources.iter().map(legacy_row).collect();
    for (source, row) in sources.iter().zip(&rows) {
        assert!(gh512_incumbent_source_ledger_matches(row, source));
        assert!(source_ledger_matches(row, source));
    }
    for folder in &folders {
        grow_directory(folder, "new-nightly-source");
    }
    let mut incumbent_times = Vec::new();
    let mut candidate_times = Vec::new();
    for round in 0..ROUNDS {
        // Alternate order so the same implementation does not always go last.
        for incumbent in [round % 2 == 0, round % 2 != 0] {
            let begin = Instant::now();
            let mut reused = 0;
            for (source, row) in sources.iter().zip(&rows) {
                let matches = if incumbent {
                    gh512_incumbent_source_ledger_matches(black_box(row), black_box(source))
                } else {
                    source_ledger_matches(black_box(row), black_box(source))
                };
                reused += usize::from(black_box(matches));
            }
            let nanos = begin.elapsed().as_nanos();
            if incumbent {
                assert_eq!(reused, 0);
                incumbent_times.push(nanos);
            } else {
                assert_eq!(reused, SOURCES);
                candidate_times.push(nanos);
            }
        }
    }
    incumbent_times.sort_unstable();
    candidate_times.sort_unstable();
    println!(
        "GH512_BENCH {}",
        serde_json::json!({
            "sources": SOURCES, "growing_folders": FOLDERS, "rounds": ROUNDS,
            "incumbent_reused": 0, "candidate_reused": SOURCES,
            "incumbent_forced_reparses": SOURCES, "candidate_forced_reparses": 0,
            "incumbent_median_ns": incumbent_times[ROUNDS / 2],
            "candidate_median_ns": candidate_times[ROUNDS / 2],
            "incumbent_samples_ns": incumbent_times,
            "candidate_samples_ns": candidate_times,
            "scope": "native ledger matching only; not a full nightly index benchmark",
            "debug_assertions": cfg!(debug_assertions),
        })
    );
}

// Verbatim pre-fix matcher for the same-invocation benchmark.
fn gh512_incumbent_source_ledger_matches(observation: &str, source: &DiscoveredSourceFile) -> bool {
    let Ok(saved) = serde_json::from_str::<serde_json::Value>(observation) else {
        return false;
    };
    if saved["producer_contract"].as_str() != Some(env!("CASS_SOURCE_INGEST_CONTRACT")) {
        return false;
    }
    if saved["primary"] != source_file_observation(&source.source_path).unwrap_or_default() {
        return false;
    }
    let Some(files) = saved["dependencies"].as_array() else {
        return false;
    };
    files.iter().all(|file| {
        file["path"]
            .as_str()
            .is_some_and(|path| source_file_observation(Path::new(path)).as_ref() == Some(file))
    })
}
