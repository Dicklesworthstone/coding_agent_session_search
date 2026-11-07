//! GH #487: the CASS consumer must include native transcripts in normal builds.
//! No feature gate: disabling the dependency feature must fail these tests.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, UNIX_EPOCH};

use anyhow::Result;
use coding_agent_search::connectors::{
    Connector, ScanContext, ScanRoot, openclaw::OpenClawConnector,
};
use coding_agent_search::franken_sync::Connection;
use coding_agent_search::franken_sync::compat::{ConnectionExt, ParamValue};
use franken_agent_detection::{DiscoveredSourceRole, NormalizedConversation};
use serde_json::{Value, json};

const OLD: i64 = 1_780_000_000_000;
const NEW: i64 = OLD + 60_000;

fn database(root: &Path, agent: &str) -> Result<(PathBuf, Connection)> {
    let path = root
        .join(".openclaw/agents")
        .join(agent)
        .join("agent/openclaw-agent.sqlite");
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
    event(
        conn,
        session,
        0,
        OLD,
        json!({"type":"session", "id":session, "cwd":"/workspace/native"}),
    )?;
    event(conn, session, 1, OLD + 1_000, message("user", text))
}

fn context(root: &Path, since: Option<i64>) -> ScanContext {
    ScanContext::with_roots(
        root.join("cass-data"),
        vec![ScanRoot::local(root.to_path_buf())],
        since,
    )
}

fn by_id(conversations: &[NormalizedConversation]) -> BTreeMap<String, Value> {
    let mut result = BTreeMap::new();
    for conversation in conversations {
        let id = conversation
            .external_id
            .clone()
            .expect("stable session identity");
        assert!(
            result
                .insert(id, serde_json::to_value(conversation).unwrap())
                .is_none(),
            "duplicate session identity"
        );
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
    assert_eq!(
        conversation.external_id.as_deref(),
        Some("main/native-session")
    );
    assert_eq!(conversation.source_path, path);
    assert_eq!(conversation.title.as_deref(), Some("native question"));
    assert_eq!(
        conversation.workspace.as_deref(),
        Some(Path::new("/workspace/native"))
    );
    assert_eq!(conversation.metadata["storage"], "sqlite");
    assert_eq!(conversation.metadata["last_event_seq"], 2);
    assert_eq!(conversation.messages.len(), 2);
    assert_eq!(conversation.messages[0].content, "native question");
    assert_eq!(conversation.messages[0].created_at, Some(OLD + 1_000));
    assert_eq!(conversation.messages[1].content, "native answer");
    assert_eq!(conversation.messages[1].idx, 1);
    let mut streamed = Vec::new();
    connector.scan_with_callback(&ctx, &mut |conversation| {
        streamed.push(conversation);
        Ok(())
    })?;
    assert_eq!(by_id(&streamed), by_id(&conversations));
    assert_eq!(
        snapshot(&path)?,
        before,
        "scanning must not rewrite the source database"
    );
    assert_eq!(
        wal.is_file().then(|| snapshot(&wal)).transpose()?,
        wal_before
    );
    Ok(())
}

#[test]
fn incremental_native_scan_uses_source_freshness_without_activity_filtering() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (path, conn) = database(temp.path(), "main")?;
    seed(&conn, "fresh", "old prefix remains")?;
    event(&conn, "historical", 0, OLD, message("user", "historical session"))?;
    conn.close()?;

    let sessions = temp.path().join(".openclaw/agents/main/sessions");
    fs::create_dir_all(&sessions)?;
    let legacy = sessions.join("historical-legacy.jsonl");
    fs::write(
        &legacy,
        format!("{}\n", message("user", "unchanged legacy source")),
    )?;
    fs::File::options()
        .write(true)
        .open(&legacy)?
        .set_modified(UNIX_EPOCH + Duration::from_millis(OLD as u64))?;
    let cutoff = NEW + 10_000;
    fs::File::options()
        .write(true)
        .open(&path)?
        .set_modified(UNIX_EPOCH + Duration::from_millis((cutoff + 10_000) as u64))?;
    let before = (snapshot(&path)?, snapshot(&legacy)?);
    let connector = OpenClawConnector::new();
    let mut previous = None;
    for root in [path.as_path(), temp.path()] {
        let ctx = context(root, Some(cutoff));
        let sources = connector.discover_source_files(&ctx)?;
        assert_eq!(sources.len(), 1, "unchanged JSONL sources remain filtered");
        assert_eq!(sources[0].source_path, path);
        let conversations = connector.scan(&ctx)?;
        let ids = by_id(&conversations);
        assert_eq!(ids.len(), 2, "changed stores retain historical sessions");
        assert_eq!(ids["main/fresh"]["messages"].as_array().unwrap().len(), 2);
        assert_eq!(ids["main/fresh"]["messages"][0]["content"], "old prefix remains");
        assert_eq!(
            ids["main/historical"]["messages"][0]["content"],
            "historical session"
        );
        assert!(conversations.iter().all(|conversation| {
            conversation.source_path == path
                && conversation.metadata["last_event_at"].as_i64().unwrap() < cutoff
        }));
        if let Some(previous) = &previous {
            assert_eq!(&ids, previous, "broad and database roots retain one identity");
        }
        assert_eq!(by_id(&connector.scan(&ctx)?), ids, "repeat is deterministic");
        previous = Some(ids);
        assert_eq!((snapshot(&path)?, snapshot(&legacy)?), before);
    }

    // Preserve the filesystem gate: no changed database or WAL means no parse.
    fs::File::options()
        .write(true)
        .open(&path)?
        .set_modified(UNIX_EPOCH + Duration::from_millis(OLD as u64))?;
    let unchanged = (snapshot(&path)?, snapshot(&legacy)?);
    for root in [path.as_path(), temp.path()] {
        let ctx = context(root, Some(cutoff));
        assert!(connector.discover_source_files(&ctx)?.is_empty());
        assert!(connector.scan(&ctx)?.is_empty());
        assert_eq!((snapshot(&path)?, snapshot(&legacy)?), unchanged);
    }
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
    fs::write(
        &shared,
        format!("{}\n", message("user", "obsolete legacy copy")),
    )?;
    fs::write(&legacy, format!("{}\n", message("user", "legacy question")))?;
    let before = [snapshot(&native)?, snapshot(&shared)?, snapshot(&legacy)?];
    let conversations = OpenClawConnector::new().scan(&context(temp.path(), None))?;
    let ids = by_id(&conversations);
    assert_eq!(ids.len(), 2);
    assert_eq!(
        ids["main/shared"]["messages"][0]["content"],
        "native authoritative copy"
    );
    assert_eq!(
        ids["main/legacy-only"]["messages"][0]["content"],
        "legacy question"
    );
    assert_eq!(
        [snapshot(&native)?, snapshot(&shared)?, snapshot(&legacy)?],
        before
    );
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
    event(
        &conn,
        "tools",
        0,
        OLD,
        json!({"type":"message", "message":{
            "role":"assistant", "model":"fixture-model", "content":[
                {"type":"toolCall", "name":"read", "id":"call-1", "arguments":{"path":"日本語.rs"}}
            ]
        }}),
    )?;
    conn.close()?;
    let conversations = OpenClawConnector::new().scan(&context(&path, None))?;
    assert_eq!(conversations.len(), 1);
    let message = &conversations[0].messages[0];
    assert_eq!(message.author.as_deref(), Some("fixture-model"));
    assert_eq!(message.invocations.len(), 1);
    assert_eq!(message.invocations[0].call_id.as_deref(), Some("call-1"));
    assert_eq!(
        message.invocations[0].arguments,
        Some(json!({"path":"日本語.rs"}))
    );
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
        let ctx = ScanContext::with_roots(
            home.join("cass-data"),
            detected
                .root_paths
                .into_iter()
                .map(ScanRoot::local)
                .collect(),
            None,
        );
        assert_eq!(connector.scan(&ctx)?.len(), 1);
        assert_eq!(
            connector
                .scan(&ScanContext::local_default(home.join("cass-data"), None))?
                .len(),
            1
        );
        // Explicit roots must stay narrow even when a configured profile exists.
        assert!(
            connector
                .scan(&context(&home.join("missing-store"), None))?
                .is_empty()
        );
        return Ok(());
    }
    let temp = tempfile::tempdir()?;
    let (_, conn) = database(temp.path(), "main")?;
    seed(&conn, "native-only", "detected question")?;
    conn.close()?;
    let result = Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "native_only_profile_is_detected_in_an_isolated_home",
            "--nocapture",
        ])
        .current_dir(temp.path())
        .env(CHILD, temp.path())
        .env("HOME", temp.path())
        .env("USERPROFILE", temp.path())
        .env("OPENCLAW_STATE_DIR", temp.path().join(".openclaw"))
        .output()?;
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
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
    assert!(
        fs::metadata(&wal)?.len() > 32,
        "fixture must retain real WAL frames"
    );
    let before = (snapshot(&path)?, snapshot(&wal)?);
    let conversations = OpenClawConnector::new().scan(&context(&path, None))?;
    assert_eq!(conversations.len(), 1);
    assert_eq!(
        conversations[0].messages[0].content,
        "committed WAL question"
    );
    assert_eq!((snapshot(&path)?, snapshot(&wal)?), before);
    Ok(())
}

// The registry and CLI coverage needs the full crate (the application's
// connector registry, assert_cmd, the cass binary) and lives in
// connector_openclaw_registry_cli.rs, because scripts/test_openclaw_contract.py
// compiles this file in a slim crate.
