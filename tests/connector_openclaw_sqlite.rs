//! GH #487: the CASS consumer must include native transcripts in normal builds.
//! No feature gate: disabling the dependency feature must fail these tests.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;
use coding_agent_search::connectors::{Connector, ScanContext, ScanRoot, openclaw::OpenClawConnector};
use coding_agent_search::franken_sync::Connection;
use coding_agent_search::franken_sync::compat::{ConnectionExt, ParamValue};
use franken_agent_detection::{DiscoveredSourceRole, NormalizedConversation};
use serde_json::{Value, json};

const OLD: i64 = 1_780_000_000_000;
const NEW: i64 = OLD + 60_000;

fn database(root: &Path, agent: &str) -> Result<(PathBuf, Connection)> {
    let path = root.join(".openclaw/agents").join(agent).join("agent/openclaw-agent.sqlite");
    fs::create_dir_all(path.parent().unwrap())?;
    let conn = Connection::open(path.to_string_lossy().into_owned())?;
    conn.execute("PRAGMA journal_mode = DELETE")?;
    conn.execute("CREATE TABLE transcript_events (session_id TEXT NOT NULL, seq INTEGER NOT NULL, event_json TEXT NOT NULL, created_at INTEGER NOT NULL, PRIMARY KEY (session_id, seq))")?;
    Ok((path, conn))
}

fn event(conn: &Connection, session: &str, seq: i64, at: i64, body: Value) -> Result<()> {
    let body = body.to_string();
    conn.execute_compat(
        "INSERT INTO transcript_events (session_id, seq, event_json, created_at) VALUES (?1, ?2, ?3, ?4)",
        &[ParamValue::from(session), ParamValue::from(seq), ParamValue::from(body.as_str()), ParamValue::from(at)],
    )?;
    Ok(())
}

fn message(role: &str, text: &str) -> Value {
    json!({"type":"message", "message":{"role":role, "content":[{"type":"text", "text":text}]}})
}

fn seed(conn: &Connection, session: &str, text: &str) -> Result<()> {
    // Insert out of sequence: the reader must use seq, not physical row order.
    event(conn, session, 2, NEW, message("assistant", "native answer"))?;
    event(conn, session, 0, OLD, json!({"type":"session", "id":session, "cwd":"/workspace/native"}))?;
    event(conn, session, 1, OLD + 1_000, message("user", text))
}

fn context(root: &Path, since: Option<i64>) -> ScanContext {
    ScanContext::with_roots(root.join("cass-data"), vec![ScanRoot::local(root.to_path_buf())], since)
}

fn by_id(conversations: &[NormalizedConversation]) -> BTreeMap<String, Value> {
    let mut result = BTreeMap::new();
    for conversation in conversations {
        let id = conversation.external_id.clone().expect("stable session identity");
        assert!(result.insert(id, serde_json::to_value(conversation).unwrap()).is_none(), "duplicate session identity");
    }
    result
}

fn snapshot(path: &Path) -> Result<(Vec<u8>, std::time::SystemTime)> {
    Ok((fs::read(path)?, fs::metadata(path)?.modified()?))
}

#[test]
fn native_databases_are_discovered_and_scanned_by_the_cass_connector() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (path, conn) = database(temp.path(), "main")?;
    seed(&conn, "native-session", "native question")?;
    conn.close()?;
    let before = snapshot(&path)?;
    let mut wal_name = path.as_os_str().to_os_string();
    wal_name.push("-wal");
    let wal = PathBuf::from(wal_name);
    let wal_before = wal.is_file().then(|| snapshot(&wal)).transpose()?;
    let connector = OpenClawConnector::new();
    let ctx = context(temp.path(), None);
    let sources = connector.discover_source_files(&ctx)?;
    assert_eq!(
        sources
            .iter()
            .filter(|source| source.role == DiscoveredSourceRole::SqliteDatabase)
            .count(),
        1,
        "normal CASS build must discover exactly one native database"
    );
    assert!(sources.iter().any(|source| {
        source.source_path == path && source.role == DiscoveredSourceRole::SqliteDatabase
    }));
    assert_eq!(sources.len(), 1 + usize::from(wal_before.is_some()));
    if wal_before.is_some() {
        assert!(sources.iter().any(|source| {
            source.source_path == wal
                && source.role == DiscoveredSourceRole::MetadataSidecar
                && source.required_for_reconstruction
        }));
    }
    let conversations = connector.scan(&ctx)?;
    assert_eq!(conversations.len(), 1);
    let conversation = &conversations[0];
    assert_eq!(conversation.agent_slug, "openclaw/main");
    assert_eq!(conversation.external_id.as_deref(), Some("main/native-session"));
    assert_eq!(conversation.source_path, path);
    assert_eq!(conversation.title.as_deref(), Some("native question"));
    assert_eq!(conversation.workspace.as_deref(), Some(Path::new("/workspace/native")));
    assert_eq!(conversation.metadata["storage"], "sqlite");
    assert_eq!(conversation.metadata["last_event_seq"], 2);
    assert_eq!(conversation.messages.len(), 2);
    assert_eq!(conversation.messages[0].content, "native question");
    assert_eq!(conversation.messages[0].created_at, Some(OLD + 1_000));
    assert_eq!(conversation.messages[1].content, "native answer");
    assert_eq!(conversation.messages[1].idx, 1);
    let mut streamed = Vec::new();
    connector.scan_with_callback(&ctx, &mut |conversation| { streamed.push(conversation); Ok(()) })?;
    assert_eq!(by_id(&streamed), by_id(&conversations));
    assert_eq!(snapshot(&path)?, before, "scanning must not rewrite the source database");
    assert_eq!(
        wal.is_file().then(|| snapshot(&wal)).transpose()?,
        wal_before
    );
    Ok(())
}

#[test]
fn incremental_native_scan_returns_all_events_of_fresh_sessions() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (path, conn) = database(temp.path(), "main")?;
    seed(&conn, "fresh", "old prefix remains")?;
    event(&conn, "stale", 0, OLD, message("user", "stale session"))?;
    conn.close()?;
    let connector = OpenClawConnector::new();
    let conversations = connector.scan(&context(&path, Some(NEW - 1_000)))?;
    assert_eq!(conversations.len(), 1);
    assert_eq!(conversations[0].external_id.as_deref(), Some("main/fresh"));
    assert_eq!(conversations[0].messages.len(), 2, "freshness selects sessions, not partial message tails");
    assert_eq!(conversations[0].messages[0].content, "old prefix remains");
    Ok(())
}

#[test]
fn native_and_legacy_history_coexist_without_migration_duplicates() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (native, conn) = database(temp.path(), "main")?;
    seed(&conn, "shared", "native authoritative copy")?;
    conn.close()?;
    let sessions = temp.path().join(".openclaw/agents/main/sessions");
    fs::create_dir_all(&sessions)?;
    let shared = sessions.join("shared.jsonl");
    let legacy = sessions.join("legacy-only.jsonl");
    fs::write(&shared, format!("{}\n", message("user", "obsolete legacy copy")))?;
    fs::write(&legacy, format!("{}\n", message("user", "legacy question")))?;
    let before = [snapshot(&native)?, snapshot(&shared)?, snapshot(&legacy)?];
    let conversations = OpenClawConnector::new().scan(&context(temp.path(), None))?;
    let ids = by_id(&conversations);
    assert_eq!(ids.len(), 2);
    assert_eq!(ids["main/shared"]["messages"][0]["content"], "native authoritative copy");
    assert_eq!(ids["main/legacy-only"]["messages"][0]["content"], "legacy question");
    assert_eq!([snapshot(&native)?, snapshot(&shared)?, snapshot(&legacy)?], before);
    Ok(())
}

#[test]
fn native_agent_identity_and_explicit_database_scope_are_preserved() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (alpha, conn) = database(temp.path(), "alpha")?;
    seed(&conn, "same-session", "alpha question")?;
    conn.close()?;
    let (beta, conn) = database(temp.path(), "beta")?;
    seed(&conn, "same-session", "beta question")?;
    conn.close()?;
    let connector = OpenClawConnector::new();
    let ids = by_id(&connector.scan(&context(temp.path(), None))?);
    assert_eq!(ids.len(), 2);
    assert!(ids.contains_key("alpha/same-session"));
    assert!(ids.contains_key("beta/same-session"));
    for path in [&alpha, &beta] {
        let conversations = connector.scan(&context(path, None))?;
        assert_eq!(conversations.len(), 1);
        assert_eq!(&conversations[0].source_path, path);
    }
    Ok(())
}

#[test]
fn native_tool_calls_keep_arguments_and_call_identity() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (path, conn) = database(temp.path(), "main")?;
    event(&conn, "tools", 0, OLD, json!({"type":"message", "message":{
        "role":"assistant", "model":"fixture-model", "content":[
            {"type":"toolCall", "name":"read", "id":"call-1", "arguments":{"path":"日本語.rs"}}
        ]
    }}))?;
    conn.close()?;
    let conversations = OpenClawConnector::new().scan(&context(&path, None))?;
    assert_eq!(conversations.len(), 1);
    let message = &conversations[0].messages[0];
    assert_eq!(message.author.as_deref(), Some("fixture-model"));
    assert_eq!(message.invocations.len(), 1);
    assert_eq!(message.invocations[0].call_id.as_deref(), Some("call-1"));
    assert_eq!(message.invocations[0].arguments, Some(json!({"path":"日本語.rs"})));
    Ok(())
}

#[test]
fn native_schema_admission_does_not_execute_views_or_create_missing_stores() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (path, conn) = database(temp.path(), "main")?;
    conn.execute("ALTER TABLE transcript_events RENAME TO unrelated_events")?;
    conn.execute("CREATE VIEW transcript_events AS SELECT * FROM unrelated_events")?;
    conn.close()?;
    let before = snapshot(&path)?;
    let connector = OpenClawConnector::new();
    assert!(connector.scan(&context(&path, None))?.is_empty());
    assert_eq!(snapshot(&path)?, before);
    let missing = temp.path().join("missing/agent/openclaw-agent.sqlite");
    assert!(connector.scan(&context(&missing, None))?.is_empty());
    assert!(!missing.exists());
    Ok(())
}

#[test]
fn native_only_profile_is_detected_in_an_isolated_home() -> Result<()> {
    const CHILD: &str = "CASS_OPENCLAW_NATIVE_TEST_HOME";
    if let Some(home) = std::env::var_os(CHILD) {
        let home = PathBuf::from(home);
        let connector = OpenClawConnector::new();
        let detected = connector.detect();
        assert!(detected.detected);
        assert_eq!(detected.root_paths.len(), 1);
        let ctx = ScanContext::with_roots(home.join("cass-data"), detected.root_paths.into_iter().map(ScanRoot::local).collect(), None);
        assert_eq!(connector.scan(&ctx)?.len(), 1);
        assert_eq!(connector.scan(&ScanContext::local_default(home.join("cass-data"), None))?.len(), 1);
        // Explicit roots must stay narrow even when a configured profile exists.
        assert!(connector.scan(&context(&home.join("missing-store"), None))?.is_empty());
        return Ok(());
    }
    let temp = tempfile::tempdir()?;
    let (_, conn) = database(temp.path(), "main")?;
    seed(&conn, "native-only", "detected question")?;
    conn.close()?;
    let result = Command::new(std::env::current_exe()?)
        .args(["--exact", "native_only_profile_is_detected_in_an_isolated_home", "--nocapture"])
        .current_dir(temp.path()).env(CHILD, temp.path())
        .env("HOME", temp.path()).env("USERPROFILE", temp.path())
        .env("OPENCLAW_STATE_DIR", temp.path().join(".openclaw")).output()?;
    assert!(result.status.success(), "{}\n{}", String::from_utf8_lossy(&result.stdout), String::from_utf8_lossy(&result.stderr));
    Ok(())
}

#[test]
fn committed_wal_transcripts_are_read_without_checkpointing_the_source() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (path, conn) = database(temp.path(), "main")?;
    conn.close()?;
    let writer = Connection::open(path.to_string_lossy().into_owned())?;
    writer.execute("PRAGMA journal_mode = WAL")?;
    seed(&writer, "wal-session", "committed WAL question")?;
    writer.close_without_checkpoint()?;
    let wal = PathBuf::from(format!("{}-wal", path.display()));
    assert!(fs::metadata(&wal)?.len() > 32, "fixture must retain real WAL frames");
    let before = (snapshot(&path)?, snapshot(&wal)?);
    let conversations = OpenClawConnector::new().scan(&context(&path, None))?;
    assert_eq!(conversations.len(), 1);
    assert_eq!(conversations[0].messages[0].content, "committed WAL question");
    assert_eq!((snapshot(&path)?, snapshot(&wal)?), before);
    Ok(())
}

mod registry_and_cli {
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
}
