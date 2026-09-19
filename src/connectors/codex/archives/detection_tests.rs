//! Detect-to-scan integration with an isolated real environment.

use super::*;
use crate::connectors::codex::CodexConnector;
use std::process::Command;

const CHILD_HOME: &str = "CASS_ARCHIVE_DETECTION_HOME";
const CHILD_COUNT: &str = "CASS_ARCHIVE_DETECTION_COUNT";

#[test]
fn detection_roots_cover_both_collections_and_archive_only_profiles() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let home = temp.path().join("custom Codex home");
    let sessions = home.join("sessions");
    let archive = home.join(ARCHIVE);
    fs::create_dir_all(&sessions)?;
    fs::create_dir_all(&archive)?;
    let modern = concat!(
        "{\"type\":\"response_item\",\"payload\":",
        "{\"role\":\"user\",\"content\":\"active\"}}\n"
    );
    let legacy = "{\"items\":[{\"role\":\"user\",\"content\":\"archived\"}]}";
    fs::write(sessions.join("rollout-active.jsonl"), modern)?;
    fs::write(archive.join("rollout-archived.json"), legacy)?;
    for expected in [2, 1, 0] {
        if expected == 1 {
            fs::rename(&sessions, temp.path().join("retained-active"))?;
        }
        if expected == 0 {
            fs::rename(&archive, temp.path().join("retained-archive"))?;
        }
        let output = Command::new(std::env::current_exe()?)
            .args(["codex::archives::detection_tests::child", "--nocapture"])
            .current_dir(temp.path())
            .env(CHILD_HOME, &home)
            .env(CHILD_COUNT, expected.to_string())
            .env("CODEX_HOME", format!("  {}  ", home.display()))
            .env("CASS_EXCLUDE_PATHS", "")
            .output()?;
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert_eq!(
        fs::read_to_string(temp.path().join("retained-active/rollout-active.jsonl"))?,
        modern
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("retained-archive/rollout-archived.json"))?,
        legacy
    );
    Ok(())
}

#[test]
fn child() -> Result<()> {
    let Some(home) = std::env::var_os(CHILD_HOME) else {
        return Ok(());
    };
    let home = PathBuf::from(home);
    let expected: usize = dotenvy::var(CHILD_COUNT)?.parse()?;
    let connector = CodexConnector::new();
    let detection = connector.detect();
    if expected == 0 {
        assert!(!detection.detected);
        return Ok(());
    }
    assert!(detection.detected);
    assert!(detection.root_paths.contains(&home));
    let roots = detection
        .root_paths
        .into_iter()
        .map(ScanRoot::local)
        .collect();
    let mut ctx = ScanContext::with_roots(home.join("cass"), roots, None);
    assert_eq!(connector.discover_source_files(&ctx)?.len(), expected);
    let conversations = connector.scan(&ctx)?;
    assert_eq!(conversations.len(), expected);
    ctx.since_ts = Some(i64::MAX);
    let archived = connector.scan(&ctx)?;
    assert_eq!(archived.len(), 1);
    assert_eq!(archived[0].messages[0].content, "archived");
    assert!(archived[0].source_path.starts_with(home.join(ARCHIVE)));
    Ok(())
}
