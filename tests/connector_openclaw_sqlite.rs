//! Native OpenClaw consumer coverage, including the actual registry and CLI.
//! Generated transcript_events fixtures use the published FAD 0.3.0 schema.
//! These tests must not be feature-gated: disabling SQLite support in an ordinary
//! CASS build must fail positive discovery/retrieval assertions, not skip them.

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
        .find(|(name, _)| *name == "openclaw")
        .expect("OpenClaw must be registered in standard CASS builds");
    factory()
}

fn context(home: &Path, root: &Path) -> ScanContext {
    ScanContext::with_roots(
        home.join("cass-data"),
        vec![ScanRoot::local(root.to_path_buf())],
        None,
    )
}

fn append(conn: &Connection, session: &str, seq: i64, event: Value) -> Result<()> {
    let event = event.to_string();
    conn.execute_compat(
        "INSERT INTO transcript_events (session_id, seq, event_json, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        &[
            ParamValue::from(session),
            ParamValue::from(seq),
            ParamValue::from(event.as_str()),
            ParamValue::from(AT + seq * 1_000),
        ],
    )?;
    Ok(())
}

fn fixture(home: &Path, agent: &str, session: &str, needle: &str) -> Result<PathBuf> {
    let directory = home.join(".openclaw/agents").join(agent).join("agent");
    fs::create_dir_all(&directory)?;
    let path = directory.join("openclaw-agent.sqlite");
    let conn = Connection::open(path.to_string_lossy().as_ref())?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA wal_autocheckpoint = 0;
         CREATE TABLE transcript_events (
             session_id TEXT NOT NULL, seq INTEGER NOT NULL,
             event_json TEXT NOT NULL, created_at INTEGER NOT NULL,
             PRIMARY KEY (session_id, seq));",
    )?;
    // Deliberately insert out of sequence: event order comes from seq, not row
    // insertion order, and indices in normalized messages must be contiguous.
    append(&conn, session, 5, json!({
        "type": "message", "message": {
            "role": "assistant", "model": "fixture-model",
            "content": [{"type": "text", "text": "Answer with Unicode: 東京 🚀"},
                        {"type": "toolCall", "name": "read_file", "id": "call-5",
                         "arguments": {"path": "src/lib.rs"}}]
        }
    }))?;
    append(&conn, session, 0, json!({
        "type": "session", "cwd": "/work/native-history"
    }))?;
    append(&conn, session, 1, json!({
        "type": "message", "message": {"role": "user", "content": needle}
    }))?;
    conn.close_without_checkpoint()?;
    let wal = sidecar(&path, "-wal");
    ensure!(fs::metadata(wal)?.len() > 32, "fixture needs committed WAL rows");
    Ok(path)
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

fn bundle(path: &Path) -> Result<SourceBundle> {
    ["", "-wal", "-shm"]
        .into_iter()
        .map(|suffix| {
            let path = sidecar(path, suffix);
            match fs::metadata(&path) {
                Ok(metadata) => Ok(Some((fs::read(path)?, metadata.modified()?))),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error.into()),
            }
        })
        .collect()
}

#[test]
fn standard_factory_discovers_native_database_and_reconstruction_wal() -> Result<()> {
    let home = tempfile::tempdir()?;
    let path = fixture(home.path(), "openclaw", "native", "nativeclawneedle")?;
    let before = bundle(&path)?;
    let sources = connector().discover_source_files(&context(home.path(), home.path()))?;
    assert_eq!(
        sources.iter().filter(|s| s.source_path == path).count(),
        1,
        "native database must be discovered exactly once"
    );
    assert!(sources.iter().any(|s| {
        s.source_path == path && s.role == DiscoveredSourceRole::SqliteDatabase
    }));
    assert!(sources.iter().any(|s| {
        s.source_path == sidecar(&path, "-wal")
            && s.role == DiscoveredSourceRole::MetadataSidecar
            && s.required_for_reconstruction
    }));
    assert_eq!(bundle(&path)?, before, "discovery mutated the provider store");
    Ok(())
}

#[test]
fn native_scan_preserves_order_workspace_identity_and_invocations() -> Result<()> {
    let home = tempfile::tempdir()?;
    let path = fixture(home.path(), "研究-agent", "session-🚀", "nativeclawneedle")?;
    let before = bundle(&path)?;
    let conversations = connector().scan(&context(home.path(), home.path()))?;
    assert_eq!(conversations.len(), 1);
    let conversation = &conversations[0];
    assert_eq!(conversation.agent_slug, "openclaw/研究-agent");
    assert_eq!(conversation.external_id.as_deref(), Some("研究-agent/session-🚀"));
    assert_eq!(conversation.source_path, path);
    assert_eq!(conversation.workspace.as_deref(), Some(Path::new("/work/native-history")));
    assert_eq!(conversation.metadata["storage"], "sqlite");
    assert_eq!(conversation.metadata["session_id"], "session-🚀");
    assert_eq!(conversation.metadata["last_event_seq"], 5);
    assert_eq!(conversation.messages.len(), 2);
    assert_eq!(conversation.messages[0].idx, 0);
    assert_eq!(conversation.messages[0].role, "user");
    assert_eq!(conversation.messages[0].content, "nativeclawneedle");
    assert_eq!(conversation.messages[0].created_at, Some(AT + 1_000));
    assert_eq!(conversation.messages[1].idx, 1);
    assert!(conversation.messages[1].content.contains("東京 🚀"));
    let call = &conversation.messages[1].invocations[0];
    assert_eq!(call.name, "read_file");
    assert_eq!(call.call_id.as_deref(), Some("call-5"));
    assert_eq!(bundle(&path)?, before, "scan changed database/WAL bytes or mtimes");
    Ok(())
}

#[test]
fn mixed_migration_prefers_native_history_without_losing_legacy_only_sessions() -> Result<()> {
    let home = tempfile::tempdir()?;
    let database = fixture(home.path(), "openclaw", "shared", "nativeclawneedle")?;
    let sessions = home.path().join(".openclaw/agents/openclaw/sessions");
    fs::create_dir_all(&sessions)?;
    for id in ["shared", "legacy-only"] {
        fs::write(sessions.join(format!("{id}.jsonl")), format!("{}\n", json!({
            "type": "message", "message": {"role": "user", "content": format!("legacy {id}")}
        })))?;
    }
    let conversations = connector().scan(&context(home.path(), home.path()))?;
    assert_eq!(conversations.len(), 2);
    let native = conversations.iter()
        .find(|c| c.external_id.as_deref() == Some("shared")).unwrap();
    assert_eq!(native.source_path, database);
    assert_eq!(native.messages[0].content, "nativeclawneedle");
    assert!(conversations.iter().any(|c| {
        c.external_id.as_deref() == Some("legacy-only")
            && c.messages[0].content == "legacy legacy-only"
    }));
    Ok(())
}

#[test]
fn explicit_native_database_root_does_not_scan_a_sibling_agent() -> Result<()> {
    let home = tempfile::tempdir()?;
    let selected = fixture(home.path(), "openclaw", "selected", "selectedneedle")?;
    fixture(home.path(), "other", "hidden", "outside-scope")?;
    let conversations = connector().scan(&context(home.path(), &selected))?;
    assert_eq!(conversations.len(), 1);
    assert_eq!(conversations[0].source_path, selected);
    assert_eq!(conversations[0].external_id.as_deref(), Some("selected"));
    Ok(())
}

fn command(home: &Path, data: &Path) -> Command {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    command.env_clear()
        .env("HOME", home).env("USERPROFILE", home).env("PATH", "")
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
fn standard_cli_indexes_and_retrieves_native_sqlite_history() -> Result<()> {
    let home = tempfile::tempdir()?;
    let database = fixture(home.path(), "openclaw", "native", "nativeclawneedle")?;
    let before = bundle(&database)?;
    let data = home.path().join("cass-data");
    assert_cmd::Command::from_std(command(home.path(), &data))
        .args(["index", "--full", "--json"])
        .timeout(Duration::from_secs(120))
        .assert().success();
    let output = assert_cmd::Command::from_std(command(home.path(), &data))
        .args(["search", "nativeclawneedle", "--mode", "lexical", "--json",
               "--no-maintenance", "--limit", "10"])
        .timeout(Duration::from_secs(30))
        .output()?;
    ensure!(output.status.success(), "lexical retrieval failed: {output:?}");
    let result: Value = serde_json::from_slice(&output.stdout)?;
    let hits = result["hits"].as_array().expect("search must return a hits array");
    assert!(!hits.is_empty(), "a successful empty query does not prove ingestion: {result}");
    assert!(hits.iter().any(|hit| {
        hit["agent"].as_str() == Some("openclaw")
            && hit["source_path"].as_str() == database.to_str()
    }), "native source provenance must survive indexing and retrieval: {result}");
    assert_eq!(bundle(&database)?, before, "CLI mutated the native provider store");
    Ok(())
}
