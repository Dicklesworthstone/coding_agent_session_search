use super::*;
use crate::connectors::codex::CodexConnector;
use franken_agent_detection::{Origin, Platform, SourceCompletion, SourceScanHooks};
use std::process::Command;

const ACTIVE: &str = "rollout-2026-09-18T12-00-00-aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa.jsonl";
const ARCHIVED: &str = "rollout-2026-09-17T12-00-00-bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb.jsonl";
const CHILD_HOME: &str = "CASS_ARCHIVE_TEST_HOME";
const CHILD_EXPECTED: &str = "CASS_ARCHIVE_TEST_EXPECTED";

fn message(text: &str) -> String {
    let payload = serde_json::json!({"role": "user", "content": text});
    let record = serde_json::json!({"type": "response_item", "payload": payload});
    format!("{record}\n")
}

fn fixture() -> Result<(tempfile::TempDir, PathBuf, Vec<PathBuf>)> {
    let temp = tempfile::tempdir()?;
    let home = temp.path().join(".codex");
    let active = home.join("sessions/2026/09/18").join(ACTIVE);
    let archived = home.join(ARCHIVE).join(ARCHIVED);
    let inputs = [(&active, "active history"), (&archived, "archived history")];
    for (path, text) in inputs {
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(path, message(text))?;
    }
    Ok((temp, home, vec![active, archived]))
}

fn assert_routes(ctx: &ScanContext, expected: &[PathBuf]) -> Result<()> {
    let connector = CodexConnector::new();
    let inventory = connector.discover_source_files(ctx)?;
    let mut paths: Vec<_> = inventory
        .iter()
        .map(|source| source.source_path.clone())
        .collect();
    let mut expected = expected.to_vec();
    paths.sort();
    expected.sort();
    assert_eq!(paths, expected);
    let collected = connector.scan(ctx)?;
    let mut streamed = Vec::new();
    connector.scan_with_callback(ctx, &mut |item| {
        streamed.push(item);
        Ok(())
    })?;
    assert_eq!(
        serde_json::to_value(&collected)?,
        serde_json::to_value(&streamed)?
    );
    let mut completed = Vec::new();
    let mut visited = Vec::new();
    let mut before = |source: &DiscoveredSourceFile| {
        assert!(inventory.contains(source));
        visited.push(source.source_path.clone());
        true
    };
    let mut complete = |done: &SourceCompletion| {
        assert!(inventory.contains(&done.source));
        assert_eq!(done.conversations_emitted, 1);
        completed.push(done.source.source_path.clone());
        Ok(())
    };
    let mut hooks = SourceScanHooks {
        should_scan_source: Some(&mut before),
        on_source_complete: Some(&mut complete),
    };
    let mut bounded = Vec::new();
    connector.scan_with_source_boundaries(ctx, &mut hooks, &mut |item| {
        bounded.push(item);
        Ok(())
    })?;
    assert_eq!(
        serde_json::to_value(&collected)?,
        serde_json::to_value(&bounded)?
    );
    paths = collected.into_iter().map(|item| item.source_path).collect();
    paths.sort();
    visited.sort();
    completed.sort();
    assert_eq!(paths, expected);
    assert_eq!(visited, expected);
    assert_eq!(completed, expected);
    Ok(())
}

#[test]
fn active_and_archived_collections_share_all_public_routes() -> Result<()> {
    let (temp, home, paths) = fixture()?;
    for roots in [
        vec![home.clone()],
        vec![temp.path().to_path_buf()],
        vec![home.clone(), home.join(ARCHIVE), paths[1].clone()],
    ] {
        let ctx = ScanContext::with_roots(
            temp.path().join("cass"),
            roots.into_iter().map(ScanRoot::local).collect(),
            None,
        );
        assert_routes(&ctx, &paths)?;
    }
    for (root, selected) in [
        (home.join("sessions"), vec![paths[0].clone()]),
        (paths[0].clone(), vec![paths[0].clone()]),
        (home.join(ARCHIVE), vec![paths[1].clone()]),
        (paths[1].clone(), vec![paths[1].clone()]),
    ] {
        let ctx =
            ScanContext::with_roots(temp.path().join("cass"), vec![ScanRoot::local(root)], None);
        assert_routes(&ctx, &selected)?;
    }
    Ok(())
}

#[test]
fn archive_moves_keep_full_home_ids_and_old_files_remain_eligible() -> Result<()> {
    let (temp, home, paths) = fixture()?;
    let mut ctx = ScanContext::with_roots(
        temp.path().join("cass"),
        vec![ScanRoot::local(home.clone())],
        None,
    );
    let connector = CodexConnector::new();
    let before = connector.scan(&ctx)?;
    let original = before
        .iter()
        .find(|item| item.source_path == paths[0])
        .unwrap();
    let moved = home.join(ARCHIVE).join(ACTIVE);
    fs::rename(&paths[0], &moved)?;
    ctx.since_ts = Some(i64::MAX);
    let archived = connector.scan(&ctx)?;
    let after = archived
        .iter()
        .find(|item| item.source_path == moved)
        .unwrap();
    assert_eq!(after.external_id, original.external_id);
    assert_eq!(
        serde_json::to_value(&after.messages)?,
        serde_json::to_value(&original.messages)?
    );
    assert_eq!(archived.len(), 2);
    assert_routes(&ctx, &[moved.clone(), paths[1].clone()])?;
    fs::rename(&moved, &paths[0])?;
    ctx.since_ts = None;
    let restored = connector.scan(&ctx)?;
    assert_eq!(
        serde_json::to_value(&restored)?,
        serde_json::to_value(&before)?
    );
    Ok(())
}

#[test]
fn archive_only_home_has_the_same_identity_as_a_home_with_active_sessions() -> Result<()> {
    let (temp, home, paths) = fixture()?;
    let ctx = ScanContext::with_roots(
        temp.path().join("cass"),
        vec![ScanRoot::local(home.clone())],
        None,
    );
    let connector = CodexConnector::new();
    let both = connector.scan(&ctx)?;
    let expected = both
        .iter()
        .find(|item| item.source_path == paths[1])
        .unwrap();
    fs::rename(home.join("sessions"), temp.path().join("retained-active"))?;
    let archive_only = connector.scan(&ctx)?;
    assert_eq!(archive_only.len(), 1);
    assert_eq!(archive_only[0].external_id, expected.external_id);
    assert_routes(&ctx, &[paths[1].clone()])?;
    Ok(())
}

#[test]
fn remote_archive_provenance_and_scoped_ids_are_preserved() -> Result<()> {
    let (temp, home, paths) = fixture()?;
    let root = ScanRoot::remote(home, Origin::remote("laptop"), Some(Platform::Linux))
        .with_rewrite("/remote", "/local");
    let ctx = ScanContext::with_roots(temp.path().join("cass"), vec![root], None);
    let connector = CodexConnector::new();
    let inventory = connector.discover_source_files(&ctx)?;
    assert_eq!(inventory.len(), 2);
    assert!(
        inventory
            .iter()
            .all(|source| source.origin == Origin::remote("laptop"))
    );
    assert_routes(&ctx, &paths)?;
    let narrow = ScanContext::with_roots(
        ctx.data_dir.clone(),
        vec![ScanRoot::local(paths[1].clone())],
        None,
    );
    let native = franken_agent_detection::CodexConnector::new().scan(&narrow)?;
    assert_eq!(
        connector.scan(&narrow)?[0].external_id,
        native[0].external_id
    );
    Ok(())
}

#[test]
fn unfinished_archive_does_not_hide_active_or_other_archived_history() -> Result<()> {
    let (temp, home, paths) = fixture()?;
    let broken = home.join(ARCHIVE).join("rollout-incomplete.jsonl");
    let bytes = format!("{}{{", message("unfinished archive"));
    fs::write(&broken, &bytes)?;
    let ctx = ScanContext::with_roots(temp.path().join("cass"), vec![ScanRoot::local(home)], None);
    let mut delivered = Vec::new();
    let error = CodexConnector::new()
        .scan_with_callback(&ctx, &mut |item| {
            delivered.push(item.source_path);
            Ok(())
        })
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<io::Error>().unwrap().kind(),
        io::ErrorKind::UnexpectedEof
    );
    delivered.sort();
    let mut expected = paths;
    expected.sort();
    assert_eq!(delivered, expected);
    assert_eq!(fs::read_to_string(broken)?, bytes);
    Ok(())
}

#[test]
fn invalid_native_archive_names_do_not_get_invented_active_ids() {
    for name in [
        "rollout-private.jsonl",
        "rollout-2026-02-31T12-00-00-aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa.jsonl",
        "rollout-2026-09-18T12-00-00-not-a-uuid.jsonl",
        "rollout-日本語.jsonl",
    ] {
        assert!(native_session_id(Path::new(name)).is_none());
    }
}

#[test]
fn archive_exclusions_and_default_codex_home_are_isolated() -> Result<()> {
    let (temp, home, paths) = fixture()?;
    let scenarios = [
        (String::new(), paths.clone()),
        (home.display().to_string(), vec![]),
        (
            home.join("sessions").display().to_string(),
            vec![paths[1].clone()],
        ),
        (
            home.join(ARCHIVE).display().to_string(),
            vec![paths[0].clone()],
        ),
        (paths[1].display().to_string(), vec![paths[0].clone()]),
    ];
    for (excluded, expected) in scenarios {
        let output = Command::new(std::env::current_exe()?)
            .args(["codex::archives::tests::archives_child", "--nocapture"])
            .current_dir(temp.path())
            .env(CHILD_HOME, &home)
            .env(CHILD_EXPECTED, serde_json::to_string(&expected)?)
            .env("CODEX_HOME", &home)
            .env("CASS_EXCLUDE_PATHS", &excluded)
            .output()?;
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

#[test]
fn archives_child() -> Result<()> {
    let Some(home) = std::env::var_os(CHILD_HOME) else {
        return Ok(());
    };
    let home = PathBuf::from(home);
    let expected: Vec<PathBuf> = serde_json::from_str(&dotenvy::var(CHILD_EXPECTED)?)?;
    let mut shapes = vec![vec![], vec![home.clone()]];
    if home.file_name().is_some_and(|name| name == ".codex") {
        shapes.push(vec![home.parent().unwrap().to_path_buf()]);
    }
    for roots in shapes {
        let mut ctx = ScanContext::with_roots(
            home.join("cass"),
            roots.into_iter().map(ScanRoot::local).collect(),
            None,
        );
        assert_routes(&ctx, &expected)?;
        ctx.since_ts = Some(i64::MAX);
        let archived: Vec<_> = expected
            .iter()
            .filter(|path| path.starts_with(home.join(ARCHIVE)))
            .cloned()
            .collect();
        assert_routes(&ctx, &archived)?;
    }
    Ok(())
}

#[test]
fn custom_codex_home_uses_the_same_trimmed_path_as_active_discovery() -> Result<()> {
    let (temp, home, paths) = fixture()?;
    let custom = temp.path().join("custom Codex home");
    let expected: Vec<_> = paths
        .iter()
        .map(|path| custom.join(path.strip_prefix(&home).unwrap()))
        .collect();
    fs::rename(&home, &custom)?;
    let output = Command::new(std::env::current_exe()?)
        .args(["codex::archives::tests::archives_child", "--nocapture"])
        .current_dir(temp.path())
        .env(CHILD_HOME, &custom)
        .env(CHILD_EXPECTED, serde_json::to_string(&expected)?)
        .env("CODEX_HOME", format!("  {}  ", custom.display()))
        .env("CASS_EXCLUDE_PATHS", "")
        .output()?;
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[test]
fn archive_inventory_reuses_completed_sources_and_admits_new_old_mtime_moves() -> Result<()> {
    use std::cell::RefCell;
    use std::collections::HashMap;

    let (temp, home, paths) = fixture()?;
    let mut ctx = ScanContext::with_roots(
        temp.path().join("cass"),
        vec![ScanRoot::local(home.clone())],
        None,
    );
    let connector = CodexConnector::new();
    let ledger = RefCell::new(HashMap::<PathBuf, DiscoveredSourceFile>::new());
    let moved = home.join(ARCHIVE).join(ACTIVE);
    let original_mtime = fs::metadata(&paths[0])?.modified()?;
    let mut active_id = None;
    for phase in 0..4 {
        if phase > 0 {
            ctx.since_ts = Some(i64::MAX);
        }
        if phase == 2 {
            fs::rename(&paths[0], &moved)?;
            assert_eq!(fs::metadata(&moved)?.modified()?, original_mtime);
        }
        let mut admitted = Vec::new();
        let mut predicate = |source: &DiscoveredSourceFile| {
            if ledger.borrow().get(&source.source_path) == Some(source) {
                return false;
            }
            admitted.push(source.source_path.clone());
            true
        };
        let mut complete = |done: &SourceCompletion| {
            ledger
                .borrow_mut()
                .insert(done.source.source_path.clone(), done.source.clone());
            Ok(())
        };
        let mut hooks = SourceScanHooks {
            should_scan_source: Some(&mut predicate),
            on_source_complete: Some(&mut complete),
        };
        let mut delivered = Vec::new();
        connector.scan_with_source_boundaries(&ctx, &mut hooks, &mut |item| {
            delivered.push(item);
            Ok(())
        })?;
        match phase {
            0 => {
                assert_eq!(admitted, paths);
                assert_eq!(delivered.len(), 2);
                active_id.clone_from(&delivered[0].external_id);
            }
            2 => {
                assert_eq!(admitted.as_slice(), std::slice::from_ref(&moved));
                assert_eq!(delivered.len(), 1);
                assert_eq!(delivered[0].external_id, active_id);
            }
            _ => {
                assert!(admitted.is_empty());
                assert!(delivered.is_empty());
            }
        }
    }
    Ok(())
}
