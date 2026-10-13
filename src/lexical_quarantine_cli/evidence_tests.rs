use super::*;
use std::fs;

#[test]
fn human_inventory_shows_age_failure_evidence_and_repeated_failure_warning() {
    let root = tempfile::tempdir().unwrap();
    for suffix in ["000001", "000002"] {
        let bundle = root.path().join(format!(".lexical-rebuild-quarantine-{suffix}"));
        fs::create_dir_all(bundle.join("index")).unwrap();
        fs::write(bundle.join("index/.lexical-rebuild-last-failure.json"), b"private cause").unwrap();
    }
    let inventory = lexical_quarantine::inspect(root.path()).unwrap();
    let mut output = Vec::new();
    render(&inventory, false, &mut output).unwrap();
    let text = String::from_utf8(output).unwrap();
    assert!(text.contains("age="));
    assert!(text.contains("FinalizationFailure"));
    assert!(text.contains(".lexical-rebuild-last-failure.json"));
    assert!(text.contains("contents not inspected"));
    assert!(text.contains("Repeated failed rebuild evidence"));
    assert!(!text.contains("private cause"));
    assert!(!root.path().join("index-run.lock").exists());
}

#[test]
fn json_inventory_contains_metadata_not_diagnostic_contents() {
    let root = tempfile::tempdir().unwrap();
    let bundle = root.path().join(".lexical-rebuild-quarantine-000001");
    fs::create_dir(&bundle).unwrap();
    fs::write(bundle.join(".lexical-rebuild-state.json"), b"private checkpoint").unwrap();
    let inventory = lexical_quarantine::inspect(root.path()).unwrap();
    let mut output = Vec::new();
    render(&inventory, true, &mut output).unwrap();
    let report: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(report["artifacts"][0]["failure_evidence"][0]["kind"], "rebuild_checkpoint");
    assert_eq!(report["artifacts"][0]["failure_evidence"][0]["size_bytes"], 18);
    assert!(!String::from_utf8(output).unwrap().contains("private checkpoint"));
}

#[cfg(unix)]
#[test]
fn incomplete_and_control_character_paths_are_unambiguous_in_human_output() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data\x1b[31m\n");
    let bundle = data.join(".lexical-rebuild-quarantine-000001");
    fs::create_dir_all(bundle.join("index")).unwrap();
    std::os::unix::fs::symlink(root.path(), bundle.join("index/link")).unwrap();
    let inventory = lexical_quarantine::inspect(&data).unwrap();
    assert!(!inventory.complete);
    let mut output = Vec::new();
    render(&inventory, false, &mut output).unwrap();
    let text = String::from_utf8(output).unwrap();
    assert!(text.contains("not a total"));
    assert!(text.contains("unknown (scan incomplete)"));
    assert!(text.contains("absence is not established"));
    assert!(!text.contains('\x1b'));
    assert!(!text.contains("data\x1b[31m\n"));
}
