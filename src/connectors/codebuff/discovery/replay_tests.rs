//! Metadata events have no reliable transcript mtime. Exercise real FAD reads
//! through the CASS watch callback and inventory, not a replacement parser.

use crate::connectors::codebuff::CodebuffConnector;
use crate::connectors::{Connector, NormalizedConversation, ScanContext, ScanRoot};
use franken_agent_detection::connectors::{SourceCompletion, SourceScanHooks};
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

const CUTOFF: i64 = 4_102_444_800_000;
const OLD_SECONDS: u64 = 1_774_113_351;

fn write_old(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    fs::write(path, bytes)?;
    fs::File::options()
        .write(true)
        .open(path)?
        .set_modified(UNIX_EPOCH + Duration::from_secs(OLD_SECONDS))?;
    Ok(())
}

fn fixture(home: &Path, name: &str) -> anyhow::Result<PathBuf> {
    let chat = home
        .join(name)
        .join("projects/probe/chats/2026-03-21T17-14-03.768Z");
    fs::create_dir_all(&chat)?;
    let primary = chat.join("chat-messages.json");
    write_old(
        &primary,
        &serde_json::to_vec(&json!([{
            "id":"user-1774113351457", "variant":"user",
            "content":"metadata replay proof", "timestamp":"01:15 PM"
        }]))?,
    )?;
    write_old(
        &chat.join("run-state.json"),
        br#"{"sessionState":{"fileContext":{"projectRoot":"/before"}}}"#,
    )?;
    Ok(primary)
}

fn context(home: &Path, selectors: Vec<PathBuf>) -> ScanContext {
    ScanContext::with_roots(
        home.join("data"),
        selectors.into_iter().map(ScanRoot::local).collect(),
        Some(CUTOFF),
    )
}

fn selector(primary: &Path, directory: bool) -> PathBuf {
    if directory {
        primary.parent().unwrap().to_path_buf()
    } else {
        primary.with_file_name("run-state.json")
    }
}

fn collect(
    ctx: &ScanContext,
) -> anyhow::Result<(Vec<NormalizedConversation>, Vec<SourceCompletion>)> {
    let mut conversations = Vec::new();
    let mut completions = Vec::new();
    CodebuffConnector::new().scan_with_source_boundaries(
        ctx,
        &mut SourceScanHooks {
            // Unlike durable admission, ordinary watch callbacks do not
            // already clear the cutoff. This is the previously broken case.
            should_scan_source: None,
            on_source_complete: Some(&mut |completion| {
                completions.push(completion.clone());
                Ok(())
            }),
        },
        &mut |conversation| {
            conversations.push(conversation);
            Ok(())
        },
    )?;
    Ok((conversations, completions))
}

#[test]
fn gh511_metadata_replay_keeps_inventory_and_watch_in_sync() -> anyhow::Result<()> {
    for directory in [false, true] {
        let temp = tempfile::tempdir()?;
        let primary = fixture(temp.path(), "selected")?;
        let state = primary.with_file_name("run-state.json");
        let bytes = fs::read(&primary)?;
        let modified = fs::metadata(&primary)?.modified()?;
        write_old(
            &state,
            br#"{"sessionState":{"fileContext":{"projectRoot":"/after"}}}"#,
        )?;
        let selected = selector(&primary, directory);
        let ctx = context(temp.path(), vec![selected.clone()]);
        // The original narrowing is insufficient: the exact primary still
        // falls below FAD's cutoff, even though its metadata was replaced.
        let mut direct = context(temp.path(), vec![primary.clone()]);
        let fad = franken_agent_detection::CodebuffConnector::new();
        assert!(fad.scan(&direct)?.is_empty());
        direct.since_ts = None;
        let expected = fad.scan(&direct)?;
        assert_eq!(expected.len(), 1);
        assert_eq!(expected[0].workspace.as_deref(), Some(Path::new("/after")));

        let connector = CodebuffConnector::new();
        let inventory = connector.discover_source_files(&ctx)?;
        assert_eq!(inventory.len(), 2);
        assert!(inventory.iter().all(|source| source.scan_root == selected));
        let mut watched = Vec::new();
        connector.scan_with_callback(&ctx, &mut |conversation| {
            watched.push(conversation);
            Ok(())
        })?;
        assert_eq!(
            serde_json::to_value(&watched)?,
            serde_json::to_value(&expected)?
        );
        let (delivered, completed) = collect(&ctx)?;
        assert_eq!(
            serde_json::to_value(&delivered)?,
            serde_json::to_value(&expected)?
        );
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].source, inventory[0]);
        assert_eq!(completed[0].required_sidecars, vec![inventory[1].clone()]);
        assert_eq!(delivered[0].messages[0].created_at, Some(1_774_113_351_457));
        assert_eq!(ctx.since_ts, Some(CUTOFF), "do not mutate the request");
        assert_eq!(fs::read(&primary)?, bytes);
        assert_eq!(fs::metadata(&primary)?.modified()?, modified);
    }
    Ok(())
}

#[test]
fn gh511_metadata_removal_and_return_replay_an_untouched_primary() -> anyhow::Result<()> {
    for directory in [false, true] {
        let temp = tempfile::tempdir()?;
        let primary = fixture(temp.path(), "selected")?;
        let state = primary.with_file_name("run-state.json");
        let saved = primary.with_file_name("saved-run-state.json");
        let primary_bytes = fs::read(&primary)?;
        let state_bytes = fs::read(&state)?;
        let primary_mtime = fs::metadata(&primary)?.modified()?;
        let selected = selector(&primary, directory);
        let ctx = context(temp.path(), vec![selected.clone()]);
        // Rename preserves the fixture while reproducing removal of the
        // consulted pathname. The removal has no remaining file timestamp.
        fs::rename(&state, &saved)?;
        let (without, completed) = collect(&ctx)?;
        assert_eq!(without.len(), 1);
        assert!(without[0].workspace.is_none());
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].source.scan_root, selected);
        assert!(completed[0].required_sidecars.is_empty());
        let identity = without[0].external_id.clone();
        assert!(identity.is_some());
        assert_eq!(
            CodebuffConnector::new().discover_source_files(&ctx)?.len(),
            1
        );

        fs::rename(&saved, &state)?;
        let (restored, completed) = collect(&ctx)?;
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].external_id, identity);
        assert_eq!(restored[0].workspace.as_deref(), Some(Path::new("/before")));
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].required_sidecars.len(), 1);
        assert_eq!(completed[0].required_sidecars[0].source_path, state);
        assert_eq!(restored[0].messages[0].created_at, Some(1_774_113_351_457));
        assert_eq!(fs::read(&primary)?, primary_bytes);
        assert_eq!(fs::read(&state)?, state_bytes);
        assert_eq!(fs::metadata(&primary)?.modified()?, primary_mtime);
        assert_eq!(
            fs::metadata(&state)?.modified()?,
            UNIX_EPOCH + Duration::from_secs(OLD_SECONDS)
        );
    }
    Ok(())
}

#[test]
fn gh511_metadata_replay_does_not_clear_other_roots_cutoffs() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let selected = fixture(temp.path(), "selected")?;
    let other = fixture(temp.path(), "other")?;
    write_old(&other, b"[")?;
    let other_store = temp.path().join("other/projects");
    let connector = CodebuffConnector::new();
    let mut unfiltered = context(temp.path(), vec![other_store.clone()]);
    unfiltered.since_ts = None;
    assert!(
        connector.scan(&unfiltered).is_err(),
        "real malformed-source control"
    );
    for directory in [false, true] {
        for other_root in [&other, &other_store] {
            for replay_first in [false, true] {
                let replay = selector(&selected, directory);
                let roots = if replay_first {
                    vec![replay, other_root.to_path_buf()]
                } else {
                    vec![other_root.to_path_buf(), replay]
                };
                let ctx = context(temp.path(), roots);
                let inventory = connector.discover_source_files(&ctx)?;
                assert_eq!(inventory.len(), 2);
                assert!(inventory.iter().all(|source| source.source_path != other));
                let conversations = connector.scan(&ctx)?;
                assert_eq!(conversations.len(), 1);
                assert_eq!(conversations[0].source_path, selected);
                assert_eq!(ctx.since_ts, Some(CUTOFF));
            }
        }
    }
    // The public primary-file and broad-store timestamp contracts remain intact.
    assert!(
        connector
            .scan(&context(temp.path(), vec![other]))?
            .is_empty()
    );
    assert!(
        connector
            .scan(&context(temp.path(), vec![other_store]))?
            .is_empty()
    );
    Ok(())
}

#[test]
fn gh511_metadata_replay_keeps_parse_errors_and_admission() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let primary = fixture(temp.path(), "selected")?;
    write_old(&primary, b"[")?;
    let ctx = context(temp.path(), vec![primary.with_file_name("run-state.json")]);
    let connector = CodebuffConnector::new();
    let mut delivered = 0;
    let error = connector
        .scan_with_callback(&ctx, &mut |_| {
            delivered += 1;
            Ok(())
        })
        .unwrap_err();
    assert_eq!(delivered, 0);
    assert!(error.chain().any(|cause| cause.is::<serde_json::Error>()));
    assert!(
        error
            .to_string()
            .contains(primary.to_string_lossy().as_ref())
    );

    let mut admitted = 0;
    let mut completed = 0;
    connector.scan_with_source_boundaries(
        &ctx,
        &mut SourceScanHooks {
            should_scan_source: Some(&mut |source| {
                admitted += 1;
                assert_eq!(source.source_path, primary);
                false
            }),
            on_source_complete: Some(&mut |_| {
                completed += 1;
                Ok(())
            }),
        },
        &mut |_| {
            delivered += 1;
            Ok(())
        },
    )?;
    assert_eq!((admitted, delivered, completed), (1, 0, 0));
    assert_eq!(fs::read(&primary)?, b"[");
    Ok(())
}
