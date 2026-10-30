//! Positive consumer tests for the legacy VS Code Copilot transcript reader.
//! Do not gate these on a CASS feature: all normal builds must retain histories
//! from ItemTable/interactive.sessions alongside the newer JSON/JSONL stores.

use anyhow::{Result, ensure};
use coding_agent_search::connectors::{
    Connector, DiscoveredSourceRole, ScanContext, ScanRoot, get_connector_factories,
};
use coding_agent_search::franken_sync::Connection;
use coding_agent_search::franken_sync::compat::{ConnectionExt, ParamValue};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

const AT: i64 = 1_780_000_000_000;
type SourceBundle = Vec<Option<(Vec<u8>, SystemTime)>>;

fn connector() -> Box<dyn Connector + Send> {
    let (_, factory) = get_connector_factories()
        .into_iter()
        .find(|(name, _)| *name == "copilot")
        .expect("Copilot must be registered in standard builds");
    factory()
}

fn context(home: &Path, root: &Path) -> ScanContext {
    ScanContext::with_roots(
        home.join("cass-data"),
        vec![ScanRoot::local(root.to_path_buf())],
        None,
    )
}

fn session(id: &str, text: &str) -> Value {
    json!({
        "sessionId": id,
        "creationDate": AT,
        "lastMessageDate": AT + 1_000,
        "responderUsername": "GitHub Copilot",
        "requesterUsername": "fixture-user",
        "customTitle": format!("History {id}"),
        "requests": [{
            "requestId": format!("request-{id}"),
            "timestamp": AT + 1_000,
            "message": {"text": text},
            "response": [
                {"value": "Preserved answer: 東京 🚀"},
                {"kind": "toolInvocationSerialized", "toolId": "read_file",
                 "toolCallId": "call-1", "toolSpecificData": {"rawInput": {"path": "src/lib.rs"}}}
            ]
        }]
    })
}

fn database(directory: &Path, payload: &Value, blob_value: bool) -> Result<PathBuf> {
    fs::create_dir_all(directory)?;
    let path = directory.join("state.vscdb");
    let conn = Connection::open(path.to_string_lossy().as_ref())?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value BLOB);",
    )?;
    let sql = if blob_value {
        "INSERT INTO ItemTable (key, value) VALUES ('interactive.sessions', CAST(?1 AS BLOB))"
    } else {
        "INSERT INTO ItemTable (key, value) VALUES ('interactive.sessions', ?1)"
    };
    let serialized = payload.to_string();
    conn.execute_compat(sql, &[ParamValue::from(serialized.as_str())])?;
    // Other extension state must never become conversation content.
    conn.execute("INSERT INTO ItemTable VALUES ('unrelated.extension', 'not a transcript')")?;
    conn.close()?;
    Ok(path)
}

fn workspace(home: &Path, name: &str) -> Result<PathBuf> {
    let directory = home.join(".config/Code/User/workspaceStorage").join(name);
    fs::create_dir_all(&directory)?;
    fs::write(
        directory.join("workspace.json"),
        r#"{"folder":"file:///work/copilot%20project"}"#,
    )?;
    Ok(directory)
}

fn bundle(database: &Path) -> Result<SourceBundle> {
    ["", "-wal", "-shm"]
        .into_iter()
        .map(|suffix| {
            let mut name = database.as_os_str().to_owned();
            name.push(suffix);
            let path = PathBuf::from(name);
            match fs::metadata(&path) {
                Ok(metadata) => Ok(Some((fs::read(path)?, metadata.modified()?))),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error.into()),
            }
        })
        .collect()
}

#[test]
fn registry_discovers_and_reads_workspace_sqlite_transcripts_without_mutation() -> Result<()> {
    let home = tempfile::tempdir()?;
    let directory = workspace(home.path(), "workspace-a")?;
    let path = database(
        &directory,
        &json!([session("legacy", "copilotlegacyneedle")]),
        false,
    )?;
    let before = bundle(&path)?;
    let ctx = context(home.path(), home.path());
    let connector = connector();
    let sources = connector.discover_source_files(&ctx)?;
    assert!(
        sources
            .iter()
            .any(|s| { s.source_path == path && s.role == DiscoveredSourceRole::SqliteDatabase })
    );
    assert!(
        sources
            .iter()
            .any(|s| s.source_path == directory.join("workspace.json"))
    );
    let conversations = connector.scan(&ctx)?;
    assert_eq!(conversations.len(), 1);
    let conversation = &conversations[0];
    assert_eq!(conversation.agent_slug, "copilot");
    assert_eq!(conversation.external_id.as_deref(), Some("legacy"));
    assert_eq!(conversation.title.as_deref(), Some("History legacy"));
    assert_eq!(
        conversation.workspace.as_deref(),
        Some(Path::new("/work/copilot project"))
    );
    assert_eq!(conversation.source_path, path);
    assert_eq!(conversation.metadata["store"], "vscode-state-db");
    assert_eq!(conversation.messages.len(), 2);
    assert_eq!(conversation.messages[0].content, "copilotlegacyneedle");
    assert_eq!(conversation.messages[0].role, "user");
    assert_eq!(conversation.messages[0].idx, 0);
    assert_eq!(conversation.messages[1].idx, 1);
    assert_eq!(conversation.messages[1].role, "assistant");
    assert_eq!(conversation.messages[1].created_at, Some(AT + 1_000));
    assert!(conversation.messages[1].content.contains("東京 🚀"));
    let invocation = &conversation.messages[1].invocations[0];
    assert_eq!(invocation.name, "read_file");
    assert_eq!(invocation.call_id.as_deref(), Some("call-1"));
    assert_eq!(
        bundle(&path)?,
        before,
        "provider database/WAL/SHM must not change"
    );
    Ok(())
}

#[test]
fn legacy_blob_map_payload_preserves_each_distinct_session() -> Result<()> {
    let home = tempfile::tempdir()?;
    let directory = workspace(home.path(), "workspace-map")?;
    let path = database(
        &directory,
        &json!({
            "one": session("one", "first session"),
            "two": session("two", "second session")
        }),
        true,
    )?;
    let conversations = connector().scan(&context(home.path(), &path))?;
    let mut ids = conversations
        .iter()
        .filter_map(|c| c.external_id.as_deref())
        .collect::<Vec<_>>();
    ids.sort_unstable();
    assert_eq!(ids, ["one", "two"]);
    Ok(())
}

#[test]
fn shared_vscode_database_does_not_misattribute_other_chat_providers() -> Result<()> {
    let home = tempfile::tempdir()?;
    let directory = workspace(home.path(), "workspace-mixed")?;
    let mut other = session("foreign", "other provider content");
    other["responderUsername"] = json!("Other Assistant");
    let mut unknown = session("unknown", "unidentified provider content");
    unknown.as_object_mut().unwrap().remove("responderUsername");
    let path = database(
        &directory,
        &json!([other, unknown, session("copilot", "admitted content")]),
        false,
    )?;
    let conversations = connector().scan(&context(home.path(), &path))?;
    assert_eq!(conversations.len(), 1);
    assert_eq!(conversations[0].external_id.as_deref(), Some("copilot"));
    Ok(())
}

#[test]
fn append_log_then_flat_json_take_precedence_without_losing_legacy_only_history() -> Result<()> {
    let home = tempfile::tempdir()?;
    let directory = workspace(home.path(), "workspace-migration")?;
    database(
        &directory,
        &json!([
            session("shared", "obsolete sqlite copy"),
            session("flat-wins", "obsolete flat predecessor"),
            session("legacy-only", "legacy survives")
        ]),
        false,
    )?;
    let sessions = directory.join("chatSessions");
    fs::create_dir_all(&sessions)?;
    fs::write(
        sessions.join("shared.json"),
        session("shared", "obsolete flat copy").to_string(),
    )?;
    fs::write(
        sessions.join("flat-wins.json"),
        session("flat-wins", "flat winner").to_string(),
    )?;
    let append_log = sessions.join("shared.jsonl");
    fs::write(
        &append_log,
        format!(
            "{}\n",
            json!({
                "kind": 0, "v": session("shared", "append log winner")
            })
        ),
    )?;
    let conversations = connector().scan(&context(home.path(), home.path()))?;
    assert_eq!(conversations.len(), 3);
    let shared = conversations
        .iter()
        .find(|c| c.external_id.as_deref() == Some("shared"))
        .unwrap();
    assert_eq!(shared.source_path, append_log);
    assert_eq!(shared.messages[0].content, "append log winner");
    assert!(conversations.iter().any(|c| {
        c.external_id.as_deref() == Some("flat-wins") && c.messages[0].content == "flat winner"
    }));
    assert!(conversations.iter().any(|c| {
        c.external_id.as_deref() == Some("legacy-only")
            && c.messages[0].content == "legacy survives"
    }));
    Ok(())
}

#[test]
fn global_database_and_explicit_source_scope_remain_distinct() -> Result<()> {
    let home = tempfile::tempdir()?;
    let directory = workspace(home.path(), "workspace-hidden")?;
    database(
        &directory,
        &json!([session("hidden", "outside scope")]),
        false,
    )?;
    let global = home.path().join(".config/Code/User/globalStorage");
    let path = database(
        &global,
        &json!([session("empty-window", "global history")]),
        false,
    )?;
    let conversations = connector().scan(&context(home.path(), &path))?;
    assert_eq!(conversations.len(), 1);
    assert_eq!(
        conversations[0].external_id.as_deref(),
        Some("empty-window")
    );
    assert_eq!(conversations[0].source_path, path);
    assert!(conversations[0].workspace.is_none());
    Ok(())
}

fn command(home: &Path, data: &Path) -> Command {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    command
        .env_clear()
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("PATH", "")
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("APPDATA", home.join("AppData/Roaming"))
        .env("LOCALAPPDATA", home.join("AppData/Local"))
        .env("CASS_DATA_DIR", data)
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("CASS_AUTO_REFRESH", "0")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("RUST_MIN_STACK", "134217728")
        .env("RAYON_NUM_THREADS", "2")
        .current_dir(home);
    if let Ok(system_root) = dotenvy::var("SystemRoot") {
        command.env("SystemRoot", system_root);
    }
    command
}

#[test]
fn standard_cli_indexes_and_searches_copilot_sqlite_history() -> Result<()> {
    let home = tempfile::tempdir()?;
    let directory = workspace(home.path(), "workspace-cli")?;
    let path = database(
        &directory,
        &json!([session("cli", "copilotlegacyneedle")]),
        false,
    )?;
    let before = bundle(&path)?;
    let data = home.path().join("cass-data");
    assert_cmd::Command::from_std(command(home.path(), &data))
        .args(["index", "--full", "--json"])
        .timeout(Duration::from_secs(120))
        .assert()
        .success();
    let output = assert_cmd::Command::from_std(command(home.path(), &data))
        .args([
            "search",
            "copilotlegacyneedle",
            "--mode",
            "lexical",
            "--json",
            "--no-maintenance",
            "--limit",
            "10",
        ])
        .timeout(Duration::from_secs(30))
        .output()?;
    ensure!(
        output.status.success(),
        "lexical retrieval failed: {output:?}"
    );
    let value: Value = serde_json::from_slice(&output.stdout)?;
    let hits = value["hits"].as_array().expect("search must include hits");
    assert!(
        !hits.is_empty(),
        "an empty success does not prove legacy ingestion: {value}"
    );
    assert!(
        hits.iter().any(|hit| {
            hit["agent"].as_str() == Some("copilot") && hit["source_path"].as_str() == path.to_str()
        }),
        "legacy source identity must survive indexing and search: {value}"
    );
    assert_eq!(
        bundle(&path)?,
        before,
        "CLI changed provider bytes or mtimes"
    );
    Ok(())
}

fn captured_fixture_sources(data: &Path, home: &Path) -> Result<Vec<PathBuf>> {
    let manifests = match fs::read_dir(data.join("raw-mirror/v1/manifests")) {
        Ok(manifests) => manifests,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut paths = Vec::new();
    for entry in manifests {
        let value: Value = serde_json::from_slice(&fs::read(entry?.path())?)?;
        let original = value["original_path"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("capture must name its original source"))?;
        let path = PathBuf::from(original);
        if path.starts_with(home) {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

fn chat_free_workspace_then_first_chat(
    streaming: &str,
    initial_payload: Option<&str>,
) -> Result<()> {
    let home = tempfile::tempdir()?;
    let directory = workspace(home.path(), "workspace-without-chat")?;
    let path = directory.join("state.vscdb");
    let conn = Connection::open(path.to_string_lossy().as_ref())?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value BLOB);
         INSERT INTO ItemTable VALUES ('unrelated.extension', 'private workspace state');",
    )?;
    if let Some(payload) = initial_payload {
        conn.execute_compat(
            "INSERT INTO ItemTable VALUES ('interactive.sessions', CAST(?1 AS BLOB))",
            &[ParamValue::from(payload)],
        )?;
    }
    conn.close()?;
    let before = bundle(&path)?;
    let data = home.path().join("cass-data");
    let index = |full: bool| {
        let mut cmd = command(home.path(), &data);
        cmd.env("CASS_STREAMING_INDEX", streaming)
            .args(["index", "--json"]);
        if full {
            cmd.arg("--full");
        }
        assert_cmd::Command::from_std(cmd)
            .timeout(Duration::from_secs(120))
            .assert()
            .success();
    };
    index(true);
    assert!(
        captured_fixture_sources(&data, home.path())?.is_empty(),
        "a workspace without chat must not create raw recovery copies of VS Code state"
    );
    assert_eq!(bundle(&path)?, before, "inspection changed VS Code state");

    let conn = Connection::open(path.to_string_lossy().as_ref())?;
    let payload = json!([session("first-chat", "copilotfirstchatneedle")]).to_string();
    conn.execute_compat(
        "INSERT OR REPLACE INTO ItemTable VALUES ('interactive.sessions', ?1)",
        &[ParamValue::from(payload.as_str())],
    )?;
    conn.close()?;
    let with_chat = bundle(&path)?;
    index(false);
    let mut expected = vec![path.clone(), directory.join("workspace.json")];
    expected.sort();
    assert_eq!(captured_fixture_sources(&data, home.path())?, expected);
    let output = assert_cmd::Command::from_std(command(home.path(), &data))
        .args([
            "search",
            "copilotfirstchatneedle",
            "--mode",
            "lexical",
            "--json",
            "--no-maintenance",
        ])
        .timeout(Duration::from_secs(30))
        .output()?;
    ensure!(
        output.status.success(),
        "first-chat search failed: {output:?}"
    );
    let value: Value = serde_json::from_slice(&output.stdout)?;
    assert!(
        value["hits"].as_array().is_some_and(|hits| {
            hits.iter().any(|hit| {
                hit["agent"].as_str() == Some("copilot")
                    && hit["source_path"].as_str() == path.to_str()
            })
        }),
        "the first real chat must be indexed on the next incremental pass: {value}"
    );
    assert_eq!(bundle(&path)?, with_chat, "capture changed VS Code state");
    Ok(())
}

#[test]
fn chat_free_workspace_is_not_mirrored_and_first_chat_is_captured_buffered() -> Result<()> {
    chat_free_workspace_then_first_chat("0", None)
}

#[test]
fn chat_free_workspace_is_not_mirrored_and_first_chat_is_captured_streaming() -> Result<()> {
    chat_free_workspace_then_first_chat("1", None)
}

#[test]
fn empty_legacy_chat_payload_is_not_mirrored_and_first_chat_is_captured() -> Result<()> {
    for streaming in ["0", "1"] {
        for payload in ["[]", "{}", "[ \n\t\r ]", "{ \n\t\r }"] {
            chat_free_workspace_then_first_chat(streaming, Some(payload))?;
        }
    }
    Ok(())
}

#[test]
fn incremental_index_reads_committed_chat_from_live_wal() -> Result<()> {
    for streaming in ["0", "1"] {
        let home = tempfile::tempdir()?;
        let directory = workspace(home.path(), "workspace-live-wal")?;
        let path = database(
            &directory,
            &json!([session("checkpointed", "copilotcheckpointedneedle")]),
            true,
        )?;
        let writer = Connection::open(path.to_string_lossy().as_ref())?;
        writer.execute("PRAGMA journal_mode = WAL")?;
        writer.execute("PRAGMA wal_autocheckpoint = 0")?;
        let checkpoint_mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(1);
        fs::File::options()
            .write(true)
            .open(&path)?
            .set_times(fs::FileTimes::new().set_modified(checkpoint_mtime))?;
        let data = home.path().join("cass-data");
        let index = |full: bool| {
            let mut cmd = command(home.path(), &data);
            cmd.env("CASS_STREAMING_INDEX", streaming)
                .args(["index", "--json"]);
            if full {
                cmd.arg("--full");
            }
            assert_cmd::Command::from_std(cmd)
                .timeout(Duration::from_secs(120))
                .assert()
                .success();
        };
        let search = |needle: &str| -> Result<Value> {
            let output = assert_cmd::Command::from_std(command(home.path(), &data))
                .args([
                    "search",
                    needle,
                    "--mode",
                    "lexical",
                    "--json",
                    "--no-maintenance",
                ])
                .timeout(Duration::from_secs(30))
                .output()?;
            ensure!(output.status.success(), "WAL search failed: {output:?}");
            Ok(serde_json::from_slice(&output.stdout)?)
        };
        index(true);
        assert_eq!(
            search("copilotcheckpointedneedle")?["hits"]
                .as_array()
                .map(Vec::len),
            Some(1),
            "the initial real CLI run must index the fixture"
        );
        // Keep the primary older than the overlap used by timestamp-based
        // discovery. The next write must advance only the committed WAL.
        std::thread::sleep(Duration::from_millis(1_200));
        let primary_before = bundle(&path)?[0].clone();
        let payload = json!([
            session("checkpointed", "copilotcheckpointedneedle"),
            session("wal-only", "copilotcommittedwalneedle")
        ])
        .to_string();
        writer.execute_compat(
            "UPDATE ItemTable SET value = CAST(?1 AS BLOB) WHERE key = 'interactive.sessions'",
            &[ParamValue::from(payload.as_str())],
        )?;
        let with_chat = bundle(&path)?;
        assert_eq!(with_chat[0], primary_before, "write must stay in the WAL");
        assert!(
            with_chat[1]
                .as_ref()
                .is_some_and(|(bytes, _)| bytes.len() > 32),
            "fixture must contain a committed live WAL"
        );
        assert_eq!(
            connector().scan(&context(home.path(), &path))?.len(),
            2,
            "an unrestricted real provider scan must read both committed chats"
        );
        assert_eq!(
            bundle(&path)?,
            with_chat,
            "provider scan changed source state"
        );
        index(false);
        let value = search("copilotcommittedwalneedle")?;
        assert_eq!(value["hits"].as_array().map(Vec::len), Some(1));
        assert!(
            value["hits"].as_array().is_some_and(|hits| {
                hits.iter().any(|hit| {
                    hit["agent"].as_str() == Some("copilot")
                        && hit["source_path"].as_str() == path.to_str()
                })
            }),
            "incremental indexing must admit a committed WAL-only chat ({streaming}): {value}"
        );
        assert_eq!(bundle(&path)?, with_chat, "indexing changed provider state");
        writer.close()?;
    }
    Ok(())
}

#[test]
fn watch_once_reads_committed_wal_without_scanning_adjacent_workspaces() -> Result<()> {
    for streaming in ["0", "1"] {
        let home = tempfile::tempdir()?;
        let directory = workspace(home.path(), "workspace-targeted-wal")?;
        let path = database(
            &directory,
            &json!([session("checkpointed", "copilottargetedseed")]),
            true,
        )?;
        let writer = Connection::open(path.to_string_lossy().as_ref())?;
        writer.execute("PRAGMA journal_mode = WAL")?;
        writer.execute("PRAGMA wal_autocheckpoint = 0")?;
        let checkpoint_mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(1);
        fs::File::options()
            .write(true)
            .open(&path)?
            .set_times(fs::FileTimes::new().set_modified(checkpoint_mtime))?;
        let data = home.path().join("cass-data");
        let mut initial = command(home.path(), &data);
        initial
            .env("CASS_STREAMING_INDEX", streaming)
            .args(["index", "--full", "--json"]);
        assert_cmd::Command::from_std(initial)
            .timeout(Duration::from_secs(120))
            .assert()
            .success();
        let adjacent = workspace(home.path(), "workspace-adjacent")?;
        let adjacent_path = database(
            &adjacent,
            &json!([session("adjacent", "copilottargetedneighbor")]),
            false,
        )?;
        let adjacent_before = bundle(&adjacent_path)?;
        let checkpoint = bundle(&path)?;
        let search = |needle: &str| -> Result<Value> {
            let output = assert_cmd::Command::from_std(command(home.path(), &data))
                .args([
                    "search",
                    needle,
                    "--mode",
                    "lexical",
                    "--json",
                    "--no-maintenance",
                ])
                .timeout(Duration::from_secs(30))
                .output()?;
            ensure!(
                output.status.success(),
                "targeted WAL search failed: {output:?}"
            );
            Ok(serde_json::from_slice(&output.stdout)?)
        };
        let wal = path.with_file_name("state.vscdb-wal");
        for (trigger, id, needle) in [
            (&path, "primary", "copilottargetedprimary"),
            (&wal, "wal", "copilottargetedwal"),
        ] {
            std::thread::sleep(Duration::from_millis(1_200));
            let payload = json!([
                session("checkpointed", "copilottargetedseed"),
                session(id, needle),
            ])
            .to_string();
            writer.execute_compat(
                "UPDATE ItemTable SET value = ?1 WHERE key = 'interactive.sessions'",
                &[ParamValue::from(payload.as_str())],
            )?;
            let before = bundle(&path)?;
            assert_eq!(before[0], checkpoint[0], "only the WAL may change");
            assert!(
                before[1]
                    .as_ref()
                    .is_some_and(|(bytes, _)| !bytes.is_empty())
            );
            let mut targeted = command(home.path(), &data);
            targeted
                .env("CASS_STREAMING_INDEX", streaming)
                .args(["index", "--watch-once"])
                .arg(trigger)
                .arg("--json");
            assert_cmd::Command::from_std(targeted)
                .timeout(Duration::from_secs(120))
                .assert()
                .success();
            let value = search(needle)?;
            let hits = value["hits"].as_array().expect("search hits");
            assert_eq!(hits.len(), 1, "{streaming}/{id}: {value}");
            assert_eq!(hits[0]["agent"], "copilot");
            assert_eq!(hits[0]["source_id"], "local");
            assert_eq!(hits[0]["source_path"], path.to_string_lossy().as_ref());
            assert_eq!(
                search("copilottargetedneighbor")?["hits"]
                    .as_array()
                    .map(Vec::len),
                Some(0)
            );
            assert_eq!(
                bundle(&path)?,
                before,
                "targeted scan changed provider state"
            );
            assert_eq!(
                bundle(&adjacent_path)?,
                adjacent_before,
                "targeted scan changed neighbor"
            );
        }
        let captures = captured_fixture_sources(&data, home.path())?;
        assert!(
            captures.contains(&path),
            "fixture raw mirroring must be active"
        );
        assert!(
            captures.iter().all(|source| !source.starts_with(&adjacent)),
            "targeted indexing must not mirror the adjacent workspace"
        );
        writer.close()?;
    }
    Ok(())
}

#[test]
fn wal_fallback_preserves_generation_precedence_scope_and_origin() -> Result<()> {
    use coding_agent_search::connectors::Origin;

    let home = tempfile::tempdir()?;
    let directory = workspace(home.path(), "workspace-wal-priority")?;
    let path = database(&directory, &json!([session("seed", "checkpoint")]), false)?;
    let writer = Connection::open(path.to_string_lossy().as_ref())?;
    writer.execute("PRAGMA journal_mode = WAL")?;
    writer.execute("PRAGMA wal_autocheckpoint = 0")?;
    fs::File::options()
        .write(true)
        .open(&path)?
        .set_times(fs::FileTimes::new().set_modified(SystemTime::UNIX_EPOCH))?;
    let payload = json!([
        session("shared", "obsolete sqlite log copy"),
        session("flat-wins", "obsolete sqlite flat copy"),
        session("recent-wins", "older sqlite copy"),
        session("export-wins", "obsolete sqlite export copy"),
        session("wal-only", "unique committed WAL chat")
    ])
    .to_string();
    writer.execute_compat(
        "UPDATE ItemTable SET value = ?1 WHERE key = 'interactive.sessions'",
        &[ParamValue::from(payload.as_str())],
    )?;
    let wal = path.with_file_name("state.vscdb-wal");
    let since = i64::try_from(
        fs::metadata(&wal)?
            .modified()?
            .duration_since(SystemTime::UNIX_EPOCH)?
            .as_millis(),
    )?;
    let sessions = directory.join("chatSessions");
    fs::create_dir_all(&sessions)?;
    fs::write(
        sessions.join("shared.jsonl"),
        format!(
            "{}\n",
            json!({"kind": 0, "v": session("shared", "append log wins")})
        ),
    )?;
    fs::write(
        sessions.join("flat.json"),
        session("flat-wins", "flat JSON wins").to_string(),
    )?;
    let recent = workspace(home.path(), "workspace-newer-primary")?;
    database(
        &recent,
        &json!([session("recent-wins", "recent primary wins")]),
        false,
    )?;
    let exports = home
        .path()
        .join(".config/Code/User/globalStorage/github.copilot-chat");
    fs::create_dir_all(&exports)?;
    fs::write(
        exports.join("conversations.json"),
        json!([{
            "id": "export-wins",
            "turns": [{"request": {"message": "extension export wins"}}]
        }])
        .to_string(),
    )?;
    let before = bundle(&path)?;
    let mut ctx = context(home.path(), home.path());
    ctx.since_ts = Some(since);
    let conversations = connector().scan(&ctx)?;
    assert_eq!(conversations.len(), 5);
    for (id, content) in [
        ("shared", "append log wins"),
        ("flat-wins", "flat JSON wins"),
        ("recent-wins", "recent primary wins"),
        ("export-wins", "extension export wins"),
        ("wal-only", "unique committed WAL chat"),
    ] {
        let matches: Vec<_> = conversations
            .iter()
            .filter(|conversation| conversation.external_id.as_deref() == Some(id))
            .collect();
        assert_eq!(matches.len(), 1, "{id}");
        assert_eq!(matches[0].messages[0].content, content);
    }
    let mut scoped = context(home.path(), &path);
    scoped.since_ts = Some(since);
    let selected = connector().scan(&scoped)?;
    assert_eq!(selected.len(), 5);
    assert!(
        selected
            .iter()
            .all(|conversation| conversation.source_path == path)
    );
    let origin = Origin::remote_with_host("fixture-mirror", "fixture-host");
    ctx.scan_roots = vec![ScanRoot::remote(
        home.path().to_path_buf(),
        origin.clone(),
        None,
    )];
    let sources = connector().discover_source_files(&ctx)?;
    for source_path in [path.clone(), directory.join("workspace.json")] {
        let source = sources
            .iter()
            .find(|source| source.source_path == source_path)
            .expect("WAL source and declared workspace metadata must be discovered");
        assert_eq!(source.origin, origin);
        assert_eq!(source.scan_root, home.path());
    }
    ctx.since_ts = Some(since.saturating_add(60_000));
    assert!(connector().scan(&ctx)?.is_empty());
    assert!(connector().discover_source_files(&ctx)?.is_empty());
    assert_eq!(
        bundle(&path)?,
        before,
        "all probes must preserve provider state"
    );
    writer.close()?;
    Ok(())
}
