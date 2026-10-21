use super::*;
use std::fs;
use std::path::PathBuf;

fn bundle(root: &Path, suffix: &str, pending: bool) -> PathBuf {
    let prefix = if pending {
        ".lexical-rebuild-reclaim-"
    } else {
        ".lexical-rebuild-quarantine-"
    };
    let path = root.join(format!("{prefix}{suffix}"));
    let content = if pending {
        path.join("quarantine")
    } else {
        path.clone()
    };
    fs::create_dir_all(content.join("index")).unwrap();
    fs::write(
        content.join("index/.lexical-rebuild-last-failure.json"),
        b"private failure",
    )
    .unwrap();
    path
}

#[test]
fn both_roots_and_interrupted_evidence_share_one_read_only_report() {
    let root = tempfile::tempdir().unwrap();
    bundle(root.path(), "000001", false);
    bundle(&root.path().join("index"), "000002", false);
    bundle(&root.path().join("index"), "000003", true);
    let report = collect(root.path());
    assert!(report.complete, "{:?}", report.warnings);
    assert!(report.repeated_failure_evidence);
    assert!(!report.cleanup_apply_allowed);
    let inventory = report.inventory.as_ref().unwrap();
    assert_eq!(inventory.artifacts.len(), 3);
    assert_eq!(inventory.total_bytes, 3 * b"private failure".len() as u64);
    assert_eq!(inventory.eligible_bytes, 0);
    assert!(
        inventory
            .artifacts
            .iter()
            .all(|artifact| artifact.age_seconds.is_some())
    );
    let encoded = serde_json::to_string(&report).unwrap();
    assert!(encoded.contains("finalization_failure"));
    assert!(!encoded.contains("private failure"));
    assert!(!root.path().join("index-run.lock").exists());
}

#[test]
fn missing_directory_is_incomplete_not_a_successful_empty_inventory() {
    let root = tempfile::tempdir().unwrap();
    let missing = root.path().join("missing");
    let report = collect(&missing);
    assert!(!report.complete);
    assert!(report.inventory.is_none());
    assert!(!report.cleanup_apply_allowed);
    assert!(!report.warnings.is_empty());
    assert!(!missing.exists());
}

#[test]
fn complete_empty_directory_is_distinct_from_an_inventory_failure() {
    let root = tempfile::tempdir().unwrap();
    let report = collect(root.path());
    assert!(report.complete, "{:?}", report.warnings);
    assert!(report.inventory.unwrap().artifacts.is_empty());
    assert!(!report.repeated_failure_evidence);
}

#[test]
fn preview_arguments_preserve_spaces_and_shell_characters_without_shell_execution() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("archive with spaces; dollar$");
    fs::create_dir(&data).unwrap();
    let report = collect(&data);
    let argv = report.next_safe_argv.unwrap();
    assert_eq!(
        argv,
        vec![
            "cass",
            "lexical-quarantine",
            "--data-dir",
            data.to_str().unwrap(),
            "--plan",
            "--json"
        ]
    );
    assert!(!report.cleanup_apply_allowed);
}

#[cfg(unix)]
#[test]
fn non_utf8_inventory_paths_degrade_to_serializable_incomplete_report() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let root = tempfile::tempdir().unwrap();
    let mut bytes = b"cass-data-".to_vec();
    bytes.push(0xff);
    let data = root.path().join(OsString::from_vec(bytes));
    fs::create_dir(&data).unwrap();
    bundle(&data, "000001", false);

    let report = collect(&data);
    assert!(!report.complete);
    assert!(report.inventory.is_none());
    assert!(report.next_safe_argv.is_none());
    assert!(!report.cleanup_apply_allowed);
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("cannot be represented in structured diagnostics"))
    );
    let encoded = serde_json::to_string(&report).unwrap();
    assert!(encoded.contains("\"complete\":false"));
    assert!(!encoded.contains("private failure"));
    let text = report.human_lines().join("\n");
    assert!(text.contains("cannot be represented in structured diagnostics"));
    assert!(!data.join("index-run.lock").exists());
}

#[test]
fn legacy_generation_walk_does_not_enter_rebuild_or_interrupted_namespaces() {
    let root = tempfile::tempdir().unwrap();
    for name in [
        ".lexical-rebuild-quarantine-000001",
        ".lexical-rebuild-reclaim-000002",
        ".lexical-rebuild-quarantine-unexpected-name",
        "generation-published",
    ] {
        fs::create_dir_all(root.path().join(name).join("index")).unwrap();
        fs::write(
            root.path()
                .join(name)
                .join("index/lexical-generation-manifest.json"),
            b"{private malformed manifest",
        )
        .unwrap();
    }
    let observed: Vec<_> = walkdir::WalkDir::new(root.path())
        .follow_links(false)
        .follow_root_links(false)
        .into_iter()
        .filter_entry(generation_walk_entry)
        .map(|entry| entry.unwrap().into_path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name == "lexical-generation-manifest.json")
        })
        .collect();
    assert_eq!(
        observed,
        vec![
            root.path()
                .join("generation-published/index/lexical-generation-manifest.json")
        ]
    );
    assert!(
        root.path()
            .join(".lexical-rebuild-quarantine-000001/index/lexical-generation-manifest.json")
            .is_file()
    );
}

#[cfg(unix)]
#[test]
fn ambiguous_tree_is_reported_without_following_links_or_claiming_zero_size() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("sentinel"), b"outside evidence").unwrap();
    let path = bundle(root.path(), "000001", false);
    std::os::unix::fs::symlink(outside.path(), path.join("index/escape")).unwrap();
    let report = collect(root.path());
    assert!(!report.complete);
    assert!(!report.cleanup_apply_allowed);
    let inventory = report.inventory.as_ref().unwrap();
    assert_eq!(inventory.eligible_bytes, 0);
    assert!(!inventory.artifacts[0].size_complete);
    let text = report.human_lines().join("\n");
    assert!(text.contains("unknown (scan incomplete)"));
    assert!(text.contains("absence is not established"));
    assert_eq!(
        fs::read(outside.path().join("sentinel")).unwrap(),
        b"outside evidence"
    );
}

#[cfg(unix)]
#[test]
fn reporting_works_while_index_run_lock_is_held_without_rewriting_heartbeat() {
    let root = tempfile::tempdir().unwrap();
    bundle(root.path(), "000001", false);
    let path = root.path().join("index-run.lock");
    fs::write(&path, b"active writer heartbeat").unwrap();
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    fs2::FileExt::try_lock_exclusive(&lock).unwrap();
    let report = collect(root.path());
    assert!(report.complete, "{:?}", report.warnings);
    assert!(!report.cleanup_apply_allowed);
    assert_eq!(fs::read(&path).unwrap(), b"active writer heartbeat");
}
