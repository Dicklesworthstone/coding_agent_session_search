//! cass-level integration tests for the Antigravity (agy) connector.
//!
//! These exercise the connector through cass's adapter
//! (`coding_agent_search::connectors::antigravity`) against a checked-in fixture
//! that mirrors agy's on-disk layout
//! (`<base>/brain/<uuid>/.system_generated/logs/transcript.jsonl` plus the
//! sibling `<base>/conversations/<uuid>.db`). The normalized conversations these
//! produce are exactly what cass's indexer + FTS search consume, so verifying the
//! scan here proves the index/search path covers agy. A companion check confirms
//! the legacy Gemini CLI connector is unaffected (bd-47kjh.3.3).
//!
//! NOTE: the agy connector defaults to the REAL `~/.gemini/antigravity-cli` when
//! `scan_roots` is empty, so every test passes an explicit `ScanRoot` at the
//! fixture base — never relying on default detection.

use coding_agent_search::connectors::antigravity::AntigravityConnector;
use coding_agent_search::connectors::gemini::GeminiConnector;
use coding_agent_search::connectors::{
    Connector, DiscoveredSourceRole, ScanContext, ScanRoot, get_connector_factories,
};
use franken_agent_detection::connectors::SourceScanHooks;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const FIXTURE_UUID: &str = "aaaa1111-bbbb-2222-cccc-333344445555";

fn fixture_base() -> PathBuf {
    PathBuf::from("tests/fixtures/antigravity")
}

/// A scan context rooted explicitly at the fixture (never the real ~/.gemini).
fn fixture_ctx() -> ScanContext {
    let base = fixture_base();
    ScanContext {
        data_dir: base.clone(),
        scan_roots: vec![ScanRoot::local(base)],
        since_ts: None,
        progress_tick: None,
    }
}

fn only_conversation() -> coding_agent_search::connectors::NormalizedConversation {
    let convs = AntigravityConnector::new()
        .scan(&fixture_ctx())
        .expect("scan");
    assert_eq!(convs.len(), 1, "fixture is a single agy conversation");
    convs.into_iter().next().unwrap()
}

#[test]
fn antigravity_scans_fixture_into_one_conversation() {
    let c = only_conversation();
    assert_eq!(c.agent_slug, "antigravity");
    assert_eq!(c.external_id.as_deref(), Some(FIXTURE_UUID));
    assert!(
        c.source_path.to_string_lossy().contains("transcript.jsonl"),
        "source_path should be the transcript we parse"
    );
}

#[test]
fn antigravity_pins_model_from_settings_change() {
    let c = only_conversation();
    assert_eq!(
        c.metadata.get("model").and_then(|v| v.as_str()),
        Some("Gemini 3.1 Pro (High)"),
        "model must be extracted from the <USER_SETTINGS_CHANGE> wrapper"
    );
    assert_eq!(
        c.metadata.get("source").and_then(|v| v.as_str()),
        Some("antigravity")
    );
}

#[test]
fn antigravity_maps_all_roles() {
    let c = only_conversation();
    assert!(c.messages.iter().any(|m| m.role == "user"));
    assert!(c.messages.iter().any(|m| m.role == "assistant"));
    assert!(c.messages.iter().any(|m| m.role == "tool"));
    assert!(c.messages.iter().any(|m| m.role == "system"));
    // CONVERSATION_HISTORY (null content) is dropped; the other 7 records map.
    assert_eq!(c.messages.len(), 7);
    for (i, m) in c.messages.iter().enumerate() {
        assert_eq!(m.idx, i64::try_from(i).unwrap());
    }
}

#[test]
fn antigravity_unwraps_user_request_and_keeps_settings() {
    let c = only_conversation();
    let user = c.messages.iter().find(|m| m.role == "user").unwrap();
    assert!(user.content.contains("FLYWHEEL"));
    assert!(!user.content.contains("USER_REQUEST"));
    assert!(
        user.extra
            .get("settings_change")
            .and_then(|v| v.as_str())
            .is_some_and(|s| s.contains("Gemini 3.1 Pro (High)"))
    );
}

#[test]
fn antigravity_preserves_thinking_on_planner_response() {
    let c = only_conversation();
    let planner = c
        .messages
        .iter()
        .find(|m| m.role == "assistant" && m.extra.get("thinking").is_some())
        .expect("a planner response carrying thinking");
    assert!(
        planner.extra["thinking"]
            .as_str()
            .unwrap()
            .contains("grep_search")
    );
}

#[test]
fn antigravity_tool_steps_carry_invocations() {
    let c = only_conversation();
    // VIEW_FILE -> synthesized "view_file" invocation, result content present.
    let view = c
        .messages
        .iter()
        .find(|m| m.extra.get("agy_type").and_then(|v| v.as_str()) == Some("VIEW_FILE"))
        .expect("VIEW_FILE message");
    assert_eq!(view.role, "tool");
    assert_eq!(view.invocations[0].name, "view_file");
    assert!(view.content.contains("FLYWHEEL"));

    // RUN_COMMAND -> structured grep_search invocation with its query argument.
    let run = c
        .messages
        .iter()
        .find(|m| m.invocations.iter().any(|i| i.name == "grep_search"))
        .expect("grep_search invocation");
    assert_eq!(run.role, "tool");
    assert_eq!(
        run.invocations[0]
            .arguments
            .as_ref()
            .and_then(|a| a.get("query"))
            .and_then(|v| v.as_str()),
        Some("FLYWHEEL")
    );
}

/// The content cass's FTS would index must contain the searchable marker — this
/// is the scan-level proxy for "search a real agy conversation". The generic
/// index+search machinery covers any registered connector, so a present,
/// correctly-roled marker here means it is findable end-to-end.
#[test]
fn antigravity_indexable_content_contains_searchable_marker() {
    let c = only_conversation();
    let haystack: String = c
        .messages
        .iter()
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        haystack.contains("FLYWHEEL") && haystack.contains("42"),
        "indexable conversation text must contain the searchable FLYWHEEL/42 marker"
    );
}

#[test]
fn antigravity_discovers_transcript_and_db_sources() {
    let discovered = AntigravityConnector::new()
        .discover_source_files(&fixture_ctx())
        .expect("discover");
    assert!(discovered.iter().all(|d| d.provider_slug == "antigravity"));
    assert!(
        discovered
            .iter()
            .any(|d| d.source_path.ends_with("transcript.jsonl")
                && d.role == DiscoveredSourceRole::PrimarySessionLog
                && d.required_for_reconstruction)
    );
    assert!(
        discovered
            .iter()
            .any(|d| d.role == DiscoveredSourceRole::SqliteDatabase
                && d.source_path.extension().is_some_and(|e| e == "db")),
        "the conversations/<uuid>.db must be discovered for archival"
    );
}

#[test]
fn antigravity_ignores_legacy_gemini_layout_under_shared_dot_gemini() {
    // Both agy and the legacy Gemini CLI live under ~/.gemini. Rooted at the
    // shared parent containing BOTH layouts, the agy connector must resolve only
    // the agy conversation.
    let tmp = tempfile::TempDir::new().unwrap();
    let dot_gemini = tmp.path().join(".gemini");

    let chats = dot_gemini.join("tmp").join("deadbeef").join("chats");
    fs::create_dir_all(&chats).unwrap();
    fs::write(chats.join("session-1.json"), "{\"messages\":[]}").unwrap();

    let logs = dot_gemini
        .join("antigravity-cli")
        .join("brain")
        .join("99990000-1111-2222-3333-444455556666")
        .join(".system_generated")
        .join("logs");
    fs::create_dir_all(&logs).unwrap();
    fs::write(
        logs.join("transcript.jsonl"),
        "{\"step_index\":0,\"source\":\"USER_EXPLICIT\",\"type\":\"USER_INPUT\",\"status\":\"DONE\",\"created_at\":\"2026-06-11T20:14:42Z\",\"content\":\"<USER_REQUEST>\\nhi\\n</USER_REQUEST>\"}\n",
    )
    .unwrap();

    let base = dot_gemini.clone();
    let ctx = ScanContext {
        data_dir: base.clone(),
        scan_roots: vec![ScanRoot::local(base)],
        since_ts: None,
        progress_tick: None,
    };
    let convs = AntigravityConnector::new().scan(&ctx).expect("scan");
    assert_eq!(convs.len(), 1, "only the agy conversation should be found");
    assert_eq!(convs[0].agent_slug, "antigravity");
}

/// The migration must not regress the legacy Gemini CLI connector: its own
/// fixture still scans into conversations labeled "gemini".
#[test]
fn legacy_gemini_connector_still_indexes() {
    let ctx = ScanContext {
        data_dir: PathBuf::from("tests/fixtures/gemini"),
        scan_roots: Vec::new(),
        since_ts: None,
        progress_tick: None,
    };
    let convs = GeminiConnector::new().scan(&ctx).expect("gemini scan");
    assert!(!convs.is_empty(), "legacy gemini fixture must still index");
    assert!(convs.iter().all(|c| c.agent_slug == "gemini"));
}

fn registered_antigravity() -> Box<dyn Connector + Send> {
    let (_, factory) = get_connector_factories()
        .into_iter()
        .find(|(name, _)| *name == "antigravity")
        .expect("Antigravity must be available to the production indexer");
    factory()
}

fn transcript_context(path: &Path) -> ScanContext {
    ScanContext::with_roots(
        path.to_path_buf(),
        vec![ScanRoot::local(path.to_path_buf())],
        None,
    )
}

fn write_transcript(base: &Path, uuid: &str, marker: &str) -> PathBuf {
    let transcript = base
        .join("brain")
        .join(uuid)
        .join(".system_generated/logs/transcript.jsonl");
    fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    let record = serde_json::json!({
        "step_index": 0,
        "source": "USER_EXPLICIT",
        "type": "USER_INPUT",
        "status": "DONE",
        "created_at": "2026-06-11T20:14:42Z",
        "content": marker,
    });
    fs::write(&transcript, format!("{record}\n")).unwrap();
    fs::File::open(&transcript)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(7200))
        .unwrap();
    transcript
}

#[test]
fn antigravity_explicit_primary_matches_directory_through_every_scan_route() {
    let transcript = fixture_base()
        .join("brain")
        .join(FIXTURE_UUID)
        .join(".system_generated/logs/transcript.jsonl");
    let ctx = transcript_context(&transcript);
    let expected = serde_json::to_value(only_conversation()).unwrap();

    for connector in [
        Box::new(AntigravityConnector::new()) as Box<dyn Connector + Send>,
        registered_antigravity(),
    ] {
        let scanned = connector.scan(&ctx).expect("exact primary scan");
        assert_eq!(scanned.len(), 1);
        assert_eq!(serde_json::to_value(&scanned[0]).unwrap(), expected);
        let mut streamed = Vec::new();
        connector
            .scan_with_callback(&ctx, &mut |conversation| {
                streamed.push(conversation);
                Ok(())
            })
            .unwrap();
        assert_eq!(
            serde_json::to_value(&streamed).unwrap(),
            serde_json::to_value(&scanned).unwrap()
        );

        let mut boundary_scanned = Vec::new();
        connector
            .scan_with_source_boundaries(
                &ctx,
                &mut SourceScanHooks::default(),
                &mut |conversation| {
                    boundary_scanned.push(conversation);
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(
            serde_json::to_value(boundary_scanned).unwrap(),
            serde_json::to_value(scanned).unwrap()
        );
        let discovered = connector.discover_source_files(&ctx).unwrap();
        assert!(discovered.iter().any(|source| {
            source.source_path == transcript
                && source.role == DiscoveredSourceRole::PrimarySessionLog
                && source.required_for_reconstruction
        }));

        let upstream = franken_agent_detection::AntigravityConnector::new();
        assert_eq!(
            connector.supports_streaming_scan(),
            upstream.supports_streaming_scan()
        );
        assert_eq!(
            connector.supports_source_boundaries(),
            upstream.supports_source_boundaries()
        );
        let error = connector
            .scan_with_callback(&ctx, &mut |_| anyhow::bail!("consumer stopped"))
            .expect_err("the native callback error must reach the caller");
        assert!(format!("{error:#}").contains("consumer stopped"));

        let mut future = ctx.clone();
        future.since_ts = Some(i64::MAX);
        assert!(connector.scan(&future).unwrap().is_empty());
        assert!(connector.discover_source_files(&future).unwrap().is_empty());
    }
}

#[test]
fn antigravity_explicit_primary_preserves_store_identity_and_excludes_siblings() {
    let temp = tempfile::tempdir().unwrap();
    for (store, external_id) in [
        ("antigravity", format!("ide/{FIXTURE_UUID}")),
        ("antigravity-cli", FIXTURE_UUID.to_string()),
    ] {
        let base = temp.path().join(".gemini").join(store);
        let selected = write_transcript(&base, FIXTURE_UUID, "selected conversation");
        let sibling = write_transcript(&base, "unselected", "unselected conversation");
        let before = fs::read(&selected).unwrap();
        let connector = registered_antigravity();
        let ctx = transcript_context(&selected);
        let conversations = connector.scan(&ctx).unwrap();
        assert_eq!(conversations.len(), 1);
        assert_eq!(
            conversations[0].external_id.as_deref(),
            Some(external_id.as_str())
        );
        assert_eq!(conversations[0].source_path, selected);
        assert_eq!(
            conversations[0].messages[0].content,
            "selected conversation"
        );
        let discovered = connector.discover_source_files(&ctx).unwrap();
        assert_eq!(
            discovered.len(),
            1,
            "only the selected transcript exists in this source bundle"
        );
        assert_eq!(discovered[0].source_path, selected);
        assert_ne!(discovered[0].source_path, sibling);
        assert_eq!(fs::read(&selected).unwrap(), before);
    }
}

#[test]
fn antigravity_nonprimary_or_unrecognized_file_never_selects_adjacent_history() {
    let temp = tempfile::tempdir().unwrap();
    let primary = write_transcript(temp.path(), FIXTURE_UUID, "must remain unselected");
    let logs = primary.parent().unwrap();
    let other = temp.path().join("transcript.jsonl");
    let fuller = logs.join("transcript_full.jsonl");
    let database = temp
        .path()
        .join("conversations")
        .join(format!("{FIXTURE_UUID}.db"));
    fs::create_dir_all(database.parent().unwrap()).unwrap();
    for path in [&other, &fuller, &database] {
        fs::copy(&primary, path).unwrap();
    }
    let missing = logs.join("missing/transcript.jsonl");
    let connector = registered_antigravity();
    for path in [other, fuller, database, missing] {
        let ctx = transcript_context(&path);
        assert!(
            connector.scan(&ctx).unwrap().is_empty(),
            "{}",
            path.display()
        );
        assert!(
            connector.discover_source_files(&ctx).unwrap().is_empty(),
            "{}",
            path.display()
        );
    }
}

fn antigravity_cli(root: &Path, data: &Path, source: &Path) -> assert_cmd::Command {
    let mut command = assert_cmd::Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    command
        .env_clear()
        .env("PATH", "")
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("CASS_ANTIGRAVITY_DATA_ROOT", source)
        .env("CASS_DATA_DIR", data)
        .env("CASS_AUTO_REFRESH", "0")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("RUST_MIN_STACK", "134217728")
        .env("RAYON_NUM_THREADS", "2")
        .current_dir(root)
        .timeout(Duration::from_secs(120));
    if let Ok(system_root) = dotenvy::var("SystemRoot") {
        command.env("SystemRoot", system_root);
    }
    command
}

fn antigravity_canonical_rows(data: &Path) -> anyhow::Result<Vec<(i64, i64, i64, String)>> {
    use coding_agent_search::franken_sync::compat::{ConnectionExt, RowExt};
    use coding_agent_search::storage::sqlite::FrankenStorage;

    let storage = FrankenStorage::open_readonly(&data.join("agent_search.db"))?;
    let rows = storage.raw().query_map_collect(
        "SELECT c.id, m.id, m.idx, m.content FROM conversations c
         JOIN messages m ON m.conversation_id = c.id ORDER BY m.id",
        &[],
        |row| {
            Ok((
                row.get_typed(0)?,
                row.get_typed(1)?,
                row.get_typed(2)?,
                row.get_typed(3)?,
            ))
        },
    )?;
    storage.close_without_checkpoint()?;
    Ok(rows)
}

#[test]
fn antigravity_cli_watch_once_primary_indexes_only_selected_history_and_appends_once()
-> anyhow::Result<()> {
    use coding_agent_search::sources::config::SourcesConfig;
    use std::io::Write;

    let temp = tempfile::tempdir()?;
    let base = temp.path().join(".gemini/antigravity-cli");
    let selected = write_transcript(&base, FIXTURE_UUID, "placeholder");
    let fixture = fixture_base()
        .join("brain")
        .join(FIXTURE_UUID)
        .join(".system_generated/logs/transcript.jsonl");
    fs::copy(fixture, &selected)?;
    fs::File::open(&selected)?.set_modified(SystemTime::now() - Duration::from_secs(7200))?;
    let sibling = write_transcript(&base, "unselected", "unselectedsiblingmarker");
    let original_bytes = fs::read(&selected)?;
    let sibling_bytes = fs::read(&sibling)?;
    let config = SourcesConfig {
        sources: Vec::new(),
        disabled_agents: get_connector_factories()
            .into_iter()
            .filter(|(name, _)| *name != "antigravity")
            .map(|(name, _)| name.to_string())
            .collect(),
    };
    let config_path = temp.path().join("config/cass/sources.toml");
    fs::create_dir_all(config_path.parent().unwrap())?;
    fs::write(config_path, toml::to_string(&config)?)?;
    let data = temp.path().join("archive");
    let index = || {
        let output = antigravity_cli(temp.path(), &data, &base)
            .args(["index", "--watch-once"])
            .arg(&selected)
            .args(["--json", "--no-progress-events"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        serde_json::from_slice::<serde_json::Value>(&output)
    };
    let summary = index()?;
    assert_eq!(summary["conversations"], 1, "{summary}");
    let original = antigravity_canonical_rows(&data)?;
    assert_eq!(
        original.len(),
        7,
        "the whole selected fixture must be stored"
    );
    index()?;
    assert_eq!(
        antigravity_canonical_rows(&data)?,
        original,
        "repeat indexing must preserve message IDs"
    );
    assert_eq!(
        fs::read(&selected)?,
        original_bytes,
        "indexing mutated the provider transcript"
    );

    let append = serde_json::json!({
        "step_index": 8,
        "source": "MODEL",
        "type": "PLANNER_RESPONSE",
        "status": "DONE",
        "created_at": "2026-06-11T20:16:42Z",
        "content": "selectedappendmarker",
    });
    {
        let mut writer = fs::OpenOptions::new().append(true).open(&selected)?;
        writeln!(writer, "{append}")?;
    }
    fs::File::open(&selected)?.set_modified(SystemTime::now() - Duration::from_secs(7200))?;
    index()?;
    let after = antigravity_canonical_rows(&data)?;
    assert_eq!(after.len(), original.len() + 1);
    assert_eq!(&after[..original.len()], original.as_slice());
    assert_eq!(after.last().unwrap().0, original[0].0);
    assert_eq!(after.last().unwrap().3, "selectedappendmarker");
    index()?;
    assert_eq!(antigravity_canonical_rows(&data)?, after);

    for (needle, expected_hits) in [("selectedappendmarker", 1), ("unselectedsiblingmarker", 0)] {
        let output = antigravity_cli(temp.path(), &data, &base)
            .args([
                "search",
                needle,
                "--mode",
                "lexical",
                "--json",
                "--no-maintenance",
                "--limit",
                "10",
            ])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let search: serde_json::Value = serde_json::from_slice(&output)?;
        let hits = search["hits"].as_array().expect("search hits array");
        assert_eq!(hits.len(), expected_hits, "{search}");
        if let Some(hit) = hits.first() {
            let path = hit["source_path"].as_str().expect("search source path");
            assert_eq!(PathBuf::from(path), dunce::canonicalize(&selected)?);
            assert_eq!(hit["agent"], "antigravity");
        }
    }
    assert_eq!(fs::read(&sibling)?, sibling_bytes);
    Ok(())
}
