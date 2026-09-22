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
