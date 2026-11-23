//! GH #573: ZCode's verified CLI 0.17.1 SQLite schema through the production
//! connector registry and the real CASS index/search pipeline. These are
//! generated, synthetic fixtures based on the issue's published DDL and JSON;
//! they are not a copy of a user's private ZCode database.

use anyhow::{Context, Result, ensure};
use coding_agent_search::connectors::{
    Connector, NormalizedConversation, ScanContext, ScanRoot, get_connector_factories,
};
use coding_agent_search::franken_sync::Connection;
use coding_agent_search::franken_sync::compat::{ConnectionExt, RowExt};
use coding_agent_search::franken_sync::params;
use coding_agent_search::raw_mirror::{
    RawMirrorCaptureInput, capture_source_file, storage_summary,
};
use coding_agent_search::storage::sqlite::SqliteStorage;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

const AT: i64 = 1_767_225_600_000;
const SECRET: &str = "zcodesecretneedle";
type SourceBundle = Vec<Option<(Vec<u8>, SystemTime)>>;
type CanonicalRow = (i64, i64, i64, String, String);

fn connector() -> Box<dyn Connector + Send> {
    let (_, factory) = get_connector_factories()
        .into_iter()
        .find(|(name, _)| *name == "zcode")
        .expect("ordinary CASS builds must include the native ZCode connector");
    factory()
}

fn context(home: &Path, root: &Path) -> ScanContext {
    ScanContext::with_roots(
        home.join("cass-data"),
        vec![ScanRoot::local(root.to_path_buf())],
        None,
    )
}

fn database(path: &Path) -> Result<Connection> {
    fs::create_dir_all(path.parent().context("fixture database parent")?)?;
    let conn = Connection::open(path.to_string_lossy().as_ref())?;
    conn.execute_batch(
        "PRAGMA journal_mode=DELETE;
         CREATE TABLE session (
           id TEXT PRIMARY KEY, project_id TEXT NOT NULL, workspace_id TEXT,
           parent_id TEXT, slug TEXT NOT NULL, directory TEXT NOT NULL, path TEXT,
           title TEXT NOT NULL, version TEXT NOT NULL, share_url TEXT,
           summary_additions INTEGER, summary_deletions INTEGER,
           summary_files INTEGER, summary_diffs TEXT, revert TEXT, permission TEXT,
           time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL,
           time_compacting INTEGER, time_archived INTEGER,
           task_type TEXT NOT NULL DEFAULT 'interactive',
           title_source TEXT NOT NULL DEFAULT 'first_input'
             CHECK(title_source IN ('default','first_input','generated','custom')),
           title_message_id TEXT, time_title_updated INTEGER, trace_id TEXT,
           time_deleted INTEGER, last_outcome TEXT);
         CREATE TABLE message (
           id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES session(id)
             ON DELETE CASCADE, time_created INTEGER NOT NULL,
           time_updated INTEGER NOT NULL, data TEXT NOT NULL, sequence INTEGER);
         CREATE TABLE part (
           id TEXT PRIMARY KEY, message_id TEXT NOT NULL REFERENCES message(id)
             ON DELETE CASCADE, session_id TEXT NOT NULL,
           time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL,
           data TEXT NOT NULL, sequence INTEGER);
         CREATE TABLE session_entry (
           id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES session(id)
             ON DELETE CASCADE, type TEXT NOT NULL, time_created INTEGER NOT NULL,
           time_updated INTEGER NOT NULL, data TEXT NOT NULL);
         CREATE TABLE turn_usage (
           session_id TEXT NOT NULL REFERENCES session(id) ON DELETE CASCADE,
           turn_id TEXT NOT NULL, trace_id TEXT, user_message_id TEXT,
           status TEXT NOT NULL CHECK(status IN ('running','completed','error','cancelled')),
           started_at INTEGER NOT NULL, first_model_start_at INTEGER,
           first_token_at INTEGER, completed_at INTEGER, duration_ms INTEGER,
           time_to_first_token_ms INTEGER, model_request_count INTEGER NOT NULL DEFAULT 0,
           model_retry_count INTEGER NOT NULL DEFAULT 0,
           tool_call_count INTEGER NOT NULL DEFAULT 0,
           tool_error_count INTEGER NOT NULL DEFAULT 0,
           input_tokens INTEGER NOT NULL DEFAULT 0,
           output_tokens INTEGER NOT NULL DEFAULT 0,
           reasoning_tokens INTEGER NOT NULL DEFAULT 0,
           cache_creation_input_tokens INTEGER NOT NULL DEFAULT 0,
           cache_read_input_tokens INTEGER NOT NULL DEFAULT 0,
           computed_total_tokens INTEGER NOT NULL DEFAULT 0,
           retryable INTEGER NOT NULL DEFAULT 0,
           cancelled_by_user INTEGER NOT NULL DEFAULT 0,
           context_exceeded INTEGER NOT NULL DEFAULT 0, error_type TEXT,
           error_code TEXT, PRIMARY KEY(session_id,turn_id));
         CREATE TABLE zcode_database_identity (id TEXT PRIMARY KEY);
         CREATE TABLE local_setting (key TEXT PRIMARY KEY, value TEXT);",
    )?;
    // The issue specifies the identity table's existence, not its private
    // columns. Its payload must never be needed to recognise this format.
    conn.execute("INSERT INTO zcode_database_identity VALUES ('fixture')")?;
    session(&conn, "main")?;
    session(&conn, "child")?;
    session(&conn, "deleted")?;
    conn.execute("UPDATE session SET parent_id='main' WHERE id='child'")?;
    conn.execute_compat(
        "UPDATE session SET time_deleted=?1 WHERE id='deleted'",
        params![AT + 300],
    )?;
    conn.execute_compat(
        "UPDATE session SET time_archived=?1 WHERE id='main'",
        params![AT + 400],
    )?;

    // Insertion order, IDs and timestamps all disagree with sequence order.
    message(
        &conn,
        "main",
        "a-final",
        9,
        json!({"role":"assistant", "time":{"created":AT + 10},
            "parentID":"z-user", "modelId":"fixture-model", "providerId":"fixture-provider",
            "tokens":{"total":137,"input":100,"output":37,"reasoning":10,
                      "cache":{"read":7,"write":3}},
            "semantics":{"origin":"agent_runtime","kind":"assistant_response",
                         "uiVisibility":"visible","transcriptVisibility":"visible"}}),
    )?;
    part(
        &conn,
        "main",
        "a-final",
        "final-text",
        0,
        json!({"type":"text","text":"zcodeanswerneedle finished 東京 🚀"}),
    )?;
    message(
        &conn,
        "main",
        "z-user",
        0,
        json!({"role":"user","time":{"created":AT + 200},
            "anchor":{"turnId":"turn-main","origin":"realUser"},
            "semantics":{"origin":"real_user","kind":"user_prompt",
                         "uiVisibility":"visible","transcriptVisibility":"visible"}}),
    )?;
    part(
        &conn,
        "main",
        "z-user",
        "prompt",
        0,
        json!({"type":"text","text":"zcodepromptneedle inspect this project"}),
    )?;
    message(
        &conn,
        "main",
        "m-tools",
        7,
        json!({"role":"assistant","time":{"created":AT + 100},
            "parentID":"z-user", "modelId":"fixture-model", "providerId":"fixture-provider",
            "tokens":{"total":63,"input":50,"output":13,"reasoning":3,
                      "cache":{"read":3,"write":2}},
            "semantics":{"origin":"agent_runtime","kind":"assistant_response",
                         "uiVisibility":"visible","transcriptVisibility":"visible"}}),
    )?;
    for (id, sequence, value) in [
        (
            "a-after",
            2,
            json!({"type":"text","text":"zcodeaftertoolneedle"}),
        ),
        (
            "m-tool",
            1,
            json!({"type":"tool","callID":"call-read","tool":"Read",
            "state":{"status":"completed",
              "input":{"file_path":"/work/zcode-probe/zcodetoolinputneedle.rs"},
              "output":"zcodetooloutputneedle fn main() {}"}}),
        ),
        (
            "z-reason",
            0,
            json!({"type":"reasoning","text":"zcodereasoningneedle inspect first"}),
        ),
        (
            "hidden-synthetic",
            3,
            json!({"type":"text","text":SECRET,"synthetic":true}),
        ),
        (
            "hidden-ignored",
            4,
            json!({"type":"text","text":SECRET,"ignored":true}),
        ),
        (
            "attachment",
            5,
            json!({"type":"file","mime":"image/png","filename":"shot.png",
            "url":format!("data:image/png;base64,{SECRET}")}),
        ),
        (
            "step",
            6,
            json!({"type":"step-finish","reason":"stop",
            "tokens":{"input":999999,"output":999999}}),
        ),
        ("snapshot", 7, json!({"type":"snapshot","snapshot":SECRET})),
    ] {
        part(&conn, "main", "m-tools", id, sequence, value)?;
    }
    // Independent discriminators matter: a reader cannot require all flags.
    for (index, flags) in [
        json!({"synthetic":true}),
        json!({"visibility":"model-only"}),
        json!({"semantics":{"origin":"agent_runtime","uiVisibility":"hidden"}}),
        json!({"semantics":{"transcriptVisibility":"hidden"}}),
        json!({"semantics":{"origin":"agent_runtime","kind":"system_reminder"}}),
        json!({"metadata":{"visibility":"model-only"}}),
    ]
    .into_iter()
    .enumerate()
    {
        let id = format!("injected-{index}");
        let mut value = flags;
        value["role"] = json!("user");
        message(&conn, "main", &id, index as i64 + 1, value)?;
        part(
            &conn,
            "main",
            &id,
            &id,
            0,
            json!({"type":"text","text":SECRET}),
        )?;
    }
    text_session(&conn, "child", "zcodechildneedle")?;
    text_session(&conn, "deleted", "zcodedeletedneedle")?;
    conn.execute_compat(
        "INSERT INTO turn_usage (session_id,turn_id,user_message_id,status,started_at,
          completed_at,input_tokens,output_tokens,reasoning_tokens,
          cache_creation_input_tokens,cache_read_input_tokens,computed_total_tokens,tool_call_count)
         VALUES ('main','turn-main','z-user','completed',?1,?2,150,50,13,5,10,200,1)",
        params![AT, AT + 300],
    )?;
    conn.execute_compat(
        "INSERT INTO session_entry VALUES ('private','main','runtime/prompt_snapshot',?1,?1,?2)",
        params![AT, SECRET],
    )?;
    conn.execute_compat(
        "INSERT INTO local_setting VALUES ('credential',?1)",
        params![SECRET],
    )?;
    Ok(conn)
}

fn session(conn: &Connection, id: &str) -> Result<()> {
    conn.execute_compat(
        "INSERT INTO session (id,project_id,slug,directory,title,version,time_created,time_updated)
         VALUES (?1,'project',?1,'/work/zcode-probe',?2,'0.17.1',?3,?4)",
        params![id, format!("ZCode {id} fixture"), AT, AT + 300],
    )?;
    Ok(())
}

fn message(conn: &Connection, session: &str, id: &str, sequence: i64, data: Value) -> Result<()> {
    let created = data["time"]["created"].as_i64().unwrap_or(AT);
    conn.execute_compat(
        "INSERT INTO message VALUES (?1,?2,?3,?3,?4,?5)",
        params![id, session, created, data.to_string(), sequence],
    )?;
    Ok(())
}

fn part(
    conn: &Connection,
    session: &str,
    message: &str,
    id: &str,
    sequence: i64,
    data: Value,
) -> Result<()> {
    conn.execute_compat(
        "INSERT INTO part VALUES (?1,?2,?3,?4,?4,?5,?6)",
        params![
            id,
            message,
            session,
            AT + 100 - sequence,
            data.to_string(),
            sequence
        ],
    )?;
    Ok(())
}

fn text_session(conn: &Connection, session: &str, text: &str) -> Result<()> {
    let id = format!("user-{session}");
    message(
        conn,
        session,
        &id,
        0,
        json!({"role":"user",
        "semantics":{"origin":"real_user","uiVisibility":"visible"}}),
    )?;
    part(
        conn,
        session,
        &id,
        &id,
        0,
        json!({"type":"text","text":text}),
    )
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

fn bundle(database: &Path) -> Result<SourceBundle> {
    ["", "-wal", "-shm", "-journal"]
        .into_iter()
        .map(|suffix| {
            let path = sidecar(database, suffix);
            match fs::metadata(&path) {
                Ok(metadata) => Ok(Some((fs::read(path)?, metadata.modified()?))),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error.into()),
            }
        })
        .collect()
}

fn command(home: &Path) -> Command {
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
        .env("CASS_DATA_DIR", home.join("cass-data"))
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("CASS_AUTO_REFRESH", "0")
        .env("CASS_INLINE_ANALYTICS_UPDATES", "1")
        .env("CASS_RAW_MIRROR", "1")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("RUST_MIN_STACK", "134217728")
        .current_dir(home)
        .args(["--color", "never"]);
    if let Ok(system_root) = dotenvy::var("SystemRoot") {
        command.env("SystemRoot", system_root);
    }
    command
}

fn run_json(command: Command) -> Result<Value> {
    let output = assert_cmd::Command::from_std(command)
        .timeout(Duration::from_secs(180))
        .output()?;
    ensure!(output.status.success(), "CASS failed: {output:?}");
    serde_json::from_slice(&output.stdout).context("CASS JSON response")
}

fn index(home: &Path, full: bool) -> Result<Value> {
    let mut cmd = command(home);
    cmd.args(["index", "--json"]);
    if full {
        cmd.arg("--full");
    }
    run_json(cmd)
}

fn search(home: &Path, query: &str, agent: Option<&str>) -> Result<Value> {
    let mut cmd = command(home);
    cmd.args([
        "search",
        query,
        "--mode",
        "lexical",
        "--json",
        "--no-maintenance",
    ]);
    if let Some(agent) = agent {
        cmd.args(["--agent", agent]);
    }
    let value = run_json(cmd)?;
    ensure!(value["hits"].is_array(), "missing search hits: {value}");
    ensure!(
        value.pointer("/budget/timed_out").and_then(Value::as_bool) != Some(true),
        "a timed out search cannot establish absence: {value}"
    );
    Ok(value)
}

fn canonical(home: &Path) -> Result<Vec<CanonicalRow>> {
    let archive = SqliteStorage::open_readonly(&home.join("cass-data/agent_search.db"))?;
    archive
        .raw()
        .query(
            "SELECT c.id,m.id,m.idx,c.title,m.content FROM conversations c
         JOIN agents a ON a.id=c.agent_id JOIN messages m ON m.conversation_id=c.id
         WHERE a.slug='zcode' ORDER BY c.title,m.idx",
        )?
        .into_iter()
        .map(|row| {
            Ok((
                row.get_typed(0)?,
                row.get_typed(1)?,
                row.get_typed(2)?,
                row.get_typed(3)?,
                row.get_typed(4)?,
            ))
        })
        .collect()
}

fn main_session(conversations: &[NormalizedConversation]) -> &NormalizedConversation {
    conversations
        .iter()
        .find(|session| session.external_id.as_deref() == Some("main"))
        .expect("the live parent session must survive filtering")
}

#[test]
fn zcode_registry_reconstructs_sequence_tools_and_filters_private_content_without_mutation()
-> Result<()> {
    let home = tempfile::tempdir()?;
    let path = home.path().join(".zcode/cli/db/db.sqlite");
    database(&path)?.close()?;
    let before = bundle(&path)?;
    let conversations = connector().scan(&context(home.path(), &path))?;
    assert_eq!(conversations.len(), 2, "deleted sessions must be absent");
    let main = main_session(&conversations);
    assert_eq!(main.agent_slug, "zcode");
    assert_eq!(
        main.workspace.as_deref(),
        Some(Path::new("/work/zcode-probe"))
    );
    assert_eq!(
        main.messages.len(),
        3,
        "only one genuine user and two visible assistants"
    );
    assert_eq!(
        main.messages.iter().map(|m| m.idx).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert_eq!(main.messages[0].role, "user");
    assert_eq!(main.messages[0].created_at, Some(AT + 200));
    assert_eq!(main.messages[1].created_at, Some(AT + 100));
    assert_eq!(main.messages[2].created_at, Some(AT + 10));
    assert!(main.messages[0].content.contains("zcodepromptneedle"));
    assert!(main.messages[2].content.contains("zcodeanswerneedle"));
    let content = &main.messages[1].content;
    let reasoning = content
        .find("zcodereasoningneedle")
        .context("visible reasoning")?;
    let output = content
        .find("zcodetooloutputneedle")
        .context("completed tool output")?;
    let after = content
        .find("zcodeaftertoolneedle")
        .context("text after tool")?;
    assert!(
        reasoning < output && output < after,
        "part sequence was not retained: {content}"
    );
    let invocation = &main.messages[1].invocations[0];
    assert_eq!(invocation.name, "Read");
    assert_eq!(invocation.call_id.as_deref(), Some("call-read"));
    assert_eq!(
        invocation
            .arguments
            .as_ref()
            .map(|value| &value["file_path"]),
        Some(&json!("/work/zcode-probe/zcodetoolinputneedle.rs"))
    );
    let serialized = serde_json::to_string(&conversations)?;
    assert!(
        !serialized.contains(SECRET),
        "filtered payload leaked into normalized metadata"
    );
    assert!(
        !serialized.contains("data:image/"),
        "embedded attachment survived sanitation"
    );
    assert_eq!(
        bundle(&path)?,
        before,
        "provider database or sidecars were modified"
    );
    Ok(())
}

#[test]
fn zcode_raw_mirror_refuses_every_source_container_before_creating_storage() -> Result<()> {
    let home = tempfile::tempdir()?;
    let data = home.path().join("capture-data");
    for name in ["arbitrary-name.sqlite3", "opencode.db"] {
        let path = home.path().join(name);
        database(&path)?.close()?;
        let before = bundle(&path)?;
        // Refusing the correct provider label is insufficient: an unhinted
        // explicit root can reach the legacy capture fallback under OpenCode
        // after its schema-aware source discovery correctly refuses ZCode.
        for provider in ["zcode", "opencode"] {
            for origin in ["local", "remote"] {
                for suffix in ["", "-wal", "-shm", "-journal"] {
                    let source = sidecar(&path, suffix);
                    let error = capture_source_file(RawMirrorCaptureInput {
                        data_dir: &data,
                        provider,
                        source_id: "fixture",
                        origin_kind: origin,
                        origin_host: None,
                        source_path: &source,
                        db_links: &[],
                    })
                    .expect_err("ZCode containers hold credentials and runtime prompt snapshots");
                    if provider == "zcode" || source.is_file() {
                        assert!(
                            error.to_string().contains("disabled_sensitive_container"),
                            "{provider}/{name}{suffix}: {error:#}"
                        );
                    }
                    // An absent sidecar under a different provider label is
                    // rejected at the stat boundary. Existing sidecars and
                    // future schemas are covered by the store identity tests.
                    assert!(!data.exists());
                }
            }
        }
        assert_eq!(bundle(&path)?, before);
    }
    assert!(
        !data.exists(),
        "privacy refusal must precede mirror initialization"
    );
    Ok(())
}

#[test]
fn zcode_unhinted_explicit_source_cannot_be_mirrored_as_opencode() -> Result<()> {
    use coding_agent_search::sources::config::{SourceDefinition, SourcesConfig};

    let home = tempfile::tempdir()?;
    let path = home.path().join("provider/arbitrary.sqlite3");
    database(&path)?.close()?;
    let before = bundle(&path)?;
    let mut source = SourceDefinition::local("imported-history");
    source.paths = vec![path.to_string_lossy().into_owned()];
    let config = SourcesConfig {
        sources: vec![source],
        disabled_agents: get_connector_factories()
            .into_iter()
            .map(|(name, _)| name.to_string())
            .filter(|name| !matches!(name.as_str(), "zcode" | "opencode"))
            .collect(),
    };
    let config_path = home.path().join(".config/cass/sources.toml");
    fs::create_dir_all(config_path.parent().context("sources config parent")?)?;
    fs::write(&config_path, toml::to_string(&config)?)?;
    let mut cmd = command(home.path());
    cmd.env_remove("CASS_IGNORE_SOURCES_CONFIG")
        .args(["index", "--full", "--json"]);
    run_json(cmd)?;
    let result = search(home.path(), "zcodepromptneedle", Some("zcode"))?;
    assert_eq!(result["hits"].as_array().map(Vec::len), Some(1), "{result}");
    let leaked = search(home.path(), SECRET, None)?;
    assert_eq!(leaked["hits"].as_array().map(Vec::len), Some(0), "{leaked}");
    let mirror = storage_summary(&home.path().join("cass-data"));
    assert_eq!(
        mirror.manifest_count, 0,
        "raw fallback relabelled ZCode as OpenCode"
    );
    assert_eq!(mirror.unique_blob_count, 0);
    assert_eq!(bundle(&path)?, before);
    Ok(())
}

#[test]
fn zcode_broad_local_root_keeps_private_rollout_out_of_other_connectors() -> Result<()> {
    use coding_agent_search::sources::config::{SourceDefinition, SourcesConfig};

    let home = tempfile::tempdir()?;
    let database_path = home.path().join(".zcode/cli/db/db.sqlite");
    database(&database_path)?.close()?;
    let before = bundle(&database_path)?;
    let mut private_sources = Vec::new();
    for relative in [
        ".zcode/cli/rollout/model-io-private.jsonl",
        ".zcode/v2/credentials.json",
        ".zcode/v2/provider_config.json",
    ] {
        let path = home.path().join(relative);
        fs::create_dir_all(path.parent().context("private source parent")?)?;
        // A generic JSONL reader can parse this as a Claude session. A broad
        // explicit root must still preserve ZCode's container ownership.
        let bytes = format!(
            "{}\n",
            json!({
                "type": "user",
                "uuid": "private-zcode-prompt",
                "sessionId": "private-zcode-rollout",
                "cwd": "/work/zcode-probe",
                "timestamp": "2026-01-01T00:00:00Z",
                "message": {"role": "user", "content": SECRET}
            })
        )
        .into_bytes();
        fs::write(&path, &bytes)?;
        private_sources.push((path, bytes));
    }
    let mut source = SourceDefinition::local("broad-home");
    source.paths = vec![home.path().to_string_lossy().into_owned()];
    let config = SourcesConfig {
        sources: vec![source],
        disabled_agents: get_connector_factories()
            .into_iter()
            .map(|(name, _)| name.to_string())
            .filter(|name| !matches!(name.as_str(), "zcode" | "claude"))
            .collect(),
    };
    let config_path = home.path().join(".config/cass/sources.toml");
    fs::create_dir_all(config_path.parent().context("sources config parent")?)?;
    fs::write(config_path, toml::to_string(&config)?)?;
    let mut cmd = command(home.path());
    cmd.env_remove("CASS_IGNORE_SOURCES_CONFIG")
        .args(["index", "--full", "--json"]);
    let summary = run_json(cmd)?;
    assert_eq!(summary["success"], true, "{summary}");
    let result = search(home.path(), "zcodepromptneedle", Some("zcode"))?;
    assert_eq!(result["hits"].as_array().map(Vec::len), Some(1), "{result}");
    let private = search(home.path(), SECRET, None)?;
    assert_eq!(
        private["hits"].as_array().map(Vec::len),
        Some(0),
        "{private}"
    );
    let mirror = storage_summary(&home.path().join("cass-data"));
    assert_eq!(mirror.manifest_count, 0);
    assert_eq!(mirror.unique_blob_count, 0);
    assert_eq!(bundle(&database_path)?, before);
    for (path, bytes) in private_sources {
        assert_eq!(fs::read(path)?, bytes);
    }
    Ok(())
}

#[test]
fn zcode_cli_indexes_searches_and_preserves_api_usage_without_raw_mirroring() -> Result<()> {
    let home = tempfile::tempdir()?;
    let path = home.path().join(".zcode/cli/db/db.sqlite");
    database(&path)?.close()?;
    for relative in [
        ".zcode/cli/rollout/model-io-private.jsonl",
        ".zcode/v2/credentials.json",
        ".zcode/v2/provider_config.json",
        ".zcode/v2/logs/private.log",
        ".zcode/v2/certs/private.pem",
    ] {
        let path = home.path().join(relative);
        fs::create_dir_all(path.parent().context("private fixture parent")?)?;
        fs::write(path, json!({"role":"user","content":SECRET}).to_string())?;
    }
    let before = bundle(&path)?;
    let mut capabilities = command(home.path());
    capabilities.args(["capabilities", "--json"]);
    let capabilities = run_json(capabilities)?;
    assert!(
        capabilities["connectors"]
            .as_array()
            .context("connector capabilities")?
            .iter()
            .any(|value| value == "zcode")
    );
    let summary = index(home.path(), true)?;
    assert_eq!(summary["success"], true, "{summary}");
    for query in [
        "zcodepromptneedle",
        "zcodeanswerneedle",
        "zcodereasoningneedle",
        "zcodetoolinputneedle",
        "zcodetooloutputneedle",
        "zcodechildneedle",
    ] {
        let result = search(home.path(), query, Some("zcode"))?;
        assert_eq!(
            result["hits"].as_array().map(Vec::len),
            Some(1),
            "{query}: {result}"
        );
        assert_eq!(result["hits"][0]["agent"], "zcode");
        assert_eq!(result["hits"][0]["workspace"], "/work/zcode-probe");
    }
    for query in [SECRET, "zcodedeletedneedle"] {
        let result = search(home.path(), query, None)?;
        assert_eq!(
            result["hits"].as_array().map(Vec::len),
            Some(0),
            "{query}: {result}"
        );
    }
    let wrong_agent = search(home.path(), "zcodepromptneedle", Some("opencode"))?;
    assert_eq!(
        wrong_agent["hits"].as_array().map(Vec::len),
        Some(0),
        "{wrong_agent}"
    );

    let archive = SqliteStorage::open_readonly(&home.path().join("cass-data/agent_search.db"))?;
    let rows = archive.raw().query(
        "SELECT SUM(t.input_tokens),SUM(t.output_tokens),SUM(t.thinking_tokens),
          SUM(t.cache_read_tokens),SUM(t.cache_creation_tokens),COUNT(*)
         FROM token_usage t JOIN conversations c ON c.id=t.conversation_id
         JOIN agents a ON a.id=t.agent_id
         WHERE a.slug='zcode' AND c.title='ZCode main fixture' AND t.data_source='api'",
    )?;
    let values: Vec<i64> = (0..6)
        .map(|i| rows[0].get_typed(i))
        .collect::<std::result::Result<_, _>>()?;
    assert_eq!(
        values,
        vec![150, 50, 13, 10, 5, 2],
        "message usage must be counted once despite duplicate turn totals and step-finish parts"
    );
    drop(archive);
    let original = canonical(home.path())?;
    assert_eq!(
        original.len(),
        4,
        "hidden messages must not reach canonical storage"
    );
    index(home.path(), true)?;
    assert_eq!(
        canonical(home.path())?,
        original,
        "reindex replaced stable message identities"
    );
    let mirror = storage_summary(&home.path().join("cass-data"));
    assert_eq!(
        mirror.manifest_count, 0,
        "sensitive provider store was raw-mirrored"
    );
    assert_eq!(mirror.unique_blob_count, 0);
    assert_eq!(
        bundle(&path)?,
        before,
        "indexing mutated the source database"
    );
    Ok(())
}

#[test]
fn zcode_cli_honors_database_and_storage_overrides_and_subagent_policy() -> Result<()> {
    for mode in [
        "ZCODE_SESSION_DB_PATH",
        "ZCODE_SESSION_DB",
        "ZCODE_STORAGE_DIR",
        "config",
    ] {
        let home = tempfile::tempdir()?;
        let storage = home.path().join("relocated-storage");
        let path = if mode == "ZCODE_STORAGE_DIR" {
            storage.join("cli/db/db.sqlite")
        } else {
            home.path().join("custom-history.sqlite3")
        };
        database(&path)?.close()?;
        let mut cmd = command(home.path());
        match mode {
            "ZCODE_STORAGE_DIR" => {
                cmd.env(mode, &storage);
            }
            "config" => {
                let config = home.path().join(".zcode/cli/config.json");
                fs::create_dir_all(config.parent().context("config parent")?)?;
                fs::write(
                    config,
                    json!({"storage":{"sessionDbPath":path}}).to_string(),
                )?;
            }
            _ => {
                cmd.env(mode, &path);
            }
        }
        let before = bundle(&path)?;
        cmd.env("CASS_SKIP_SUBAGENTS", "1")
            .args(["index", "--full", "--json"]);
        let summary = run_json(cmd)?;
        assert_eq!(summary["success"], true, "{mode}: {summary}");
        let result = search(home.path(), "zcodepromptneedle", Some("zcode"))?;
        assert_eq!(
            result["hits"].as_array().map(Vec::len),
            Some(1),
            "{mode}: {result}"
        );
        let children = search(home.path(), "zcodechildneedle", Some("zcode"))?;
        assert_eq!(
            children["hits"].as_array().map(Vec::len),
            Some(0),
            "{mode}: {children}"
        );
        assert_eq!(
            canonical(home.path())?.len(),
            3,
            "{mode}: only parent messages are admitted"
        );
        assert_eq!(
            storage_summary(&home.path().join("cass-data")).unique_blob_count,
            0
        );
        assert_eq!(bundle(&path)?, before, "{mode}: source mutated");
    }
    Ok(())
}

#[test]
fn zcode_turn_usage_fills_missing_message_metrics_once_and_incremental_additions_keep_identity()
-> Result<()> {
    let home = tempfile::tempdir()?;
    let path = home.path().join(".zcode/cli/db/db.sqlite");
    let writer = database(&path)?;
    session(&writer, "fallback")?;
    text_session(&writer, "fallback", "zcodefallbackpromptneedle")?;
    message(
        &writer,
        "fallback",
        "fallback-answer",
        1,
        json!({"role":"assistant","parentID":"user-fallback",
            "modelId":"fallback-model","providerId":"fixture-provider"}),
    )?;
    part(
        &writer,
        "fallback",
        "fallback-answer",
        "fallback-text",
        0,
        json!({"type":"text","text":"zcodefallbackanswerneedle"}),
    )?;
    writer.execute_compat(
        "INSERT INTO turn_usage (session_id,turn_id,user_message_id,status,started_at,
          input_tokens,output_tokens,reasoning_tokens,cache_read_input_tokens,
          cache_creation_input_tokens,computed_total_tokens)
         VALUES ('fallback','turn-fallback','user-fallback','completed',?1,80,20,4,6,2,100)",
        params![AT],
    )?;
    writer.close()?;
    index(home.path(), true)?;
    let original = canonical(home.path())?;
    let archive = SqliteStorage::open_readonly(&home.path().join("cass-data/agent_search.db"))?;
    let rows = archive.raw().query(
        "SELECT t.input_tokens,t.output_tokens,t.thinking_tokens,t.cache_read_tokens,
           t.cache_creation_tokens FROM token_usage t
         JOIN conversations c ON c.id=t.conversation_id
         WHERE c.title='ZCode fallback fixture' AND t.data_source='api'",
    )?;
    assert_eq!(
        rows.len(),
        1,
        "aggregate turn usage must attach to exactly one message"
    );
    let counts: Vec<i64> = (0..5)
        .map(|i| rows[0].get_typed(i))
        .collect::<std::result::Result<_, _>>()?;
    assert_eq!(counts, vec![80, 20, 4, 6, 2]);
    drop(archive);

    let writer = Connection::open(path.to_string_lossy().as_ref())?;
    message(
        &writer,
        "main",
        "appended",
        10,
        json!({"role":"assistant",
        "parentID":"z-user","time":{"created":AT + 500}}),
    )?;
    part(
        &writer,
        "main",
        "appended",
        "appended-text",
        0,
        json!({"type":"text","text":"zcodeincrementalneedle"}),
    )?;
    writer.execute_compat(
        "UPDATE session SET time_updated=?1 WHERE id='main'",
        params![AT + 500],
    )?;
    writer.close()?;
    let before = bundle(&path)?;
    index(home.path(), false)?;
    let result = search(home.path(), "zcodeincrementalneedle", Some("zcode"))?;
    assert_eq!(result["hits"].as_array().map(Vec::len), Some(1), "{result}");
    let current = canonical(home.path())?;
    assert_eq!(current.len(), original.len() + 1);
    for old in &original {
        assert!(
            current.contains(old),
            "incremental scan replaced existing identity/content: {old:?}"
        );
    }
    assert_eq!(
        bundle(&path)?,
        before,
        "incremental scan modified provider source"
    );
    Ok(())
}

#[test]
fn zcode_watch_indexes_wal_only_custom_database_commits_with_old_timestamps() -> Result<()> {
    struct WatchChild(Child);
    impl Drop for WatchChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let home = tempfile::tempdir()?;
    let path = home.path().join("arbitrary-history.sqlite3");
    let writer = database(&path)?;
    writer.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")?;
    let stdout = home.path().join("watch.stdout");
    let stderr = home.path().join("watch.stderr");
    let logs = || {
        format!(
            "stdout:\n{}\nstderr:\n{}",
            fs::read_to_string(&stdout).unwrap_or_default(),
            fs::read_to_string(&stderr).unwrap_or_default()
        )
    };
    let mut watch = WatchChild(
        command(home.path())
            .env("ZCODE_SESSION_DB_PATH", &path)
            .env("CASS_SKIP_SUBAGENTS", "1")
            .env("RUST_LOG", "info")
            .arg("--verbose")
            .args(["index", "--watch", "--watch-interval", "1", "--json"])
            .stdout(Stdio::from(fs::File::create(&stdout)?))
            .stderr(Stdio::from(fs::File::create(&stderr)?))
            .spawn()?,
    );
    let startup = Instant::now() + Duration::from_secs(90);
    while !logs().contains("watch mode: minimum interval between scan cycles") {
        ensure!(watch.0.try_wait()?.is_none(), "watcher exited: {}", logs());
        ensure!(
            Instant::now() < startup,
            "watch startup deadline: {}",
            logs()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let original = canonical(home.path())?;
    assert_eq!(original.len(), 3, "initial scan must exclude subagents");
    let main_before = fs::read(&path)?;
    writer.execute("BEGIN")?;
    // These rows retain January timestamps, even though they arrive after the
    // watcher's successful full scan. A file event must not filter them using
    // the current wall-clock watermark.
    session(&writer, "wal-added")?;
    text_session(&writer, "wal-added", "zcodewalneedle")?;
    session(&writer, "wal-child")?;
    text_session(&writer, "wal-child", "zcodewalchildneedle")?;
    writer.execute("UPDATE session SET parent_id='main' WHERE id='wal-child'")?;
    writer.execute("COMMIT")?;
    assert_eq!(
        fs::read(&path)?,
        main_before,
        "fixture change must live solely in WAL"
    );
    ensure!(
        fs::metadata(sidecar(&path, "-wal"))?.len() > 32,
        "fixture has no WAL payload"
    );
    let committed = bundle(&path)?;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        ensure!(watch.0.try_wait()?.is_none(), "watcher exited: {}", logs());
        ensure!(
            Instant::now() < deadline,
            "custom WAL update was not indexed: {}",
            logs()
        );
        let mut query = command(home.path());
        query.args([
            "search",
            "zcodewalneedle",
            "--agent",
            "zcode",
            "--mode",
            "lexical",
            "--json",
            "--no-maintenance",
            "--timeout",
            "3000",
        ]);
        let output = assert_cmd::Command::from_std(query)
            .timeout(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_secs(10)),
            )
            .output()?;
        if output.status.success() {
            let result: Value = serde_json::from_slice(&output.stdout)?;
            ensure!(
                result["hits"].is_array(),
                "invalid watch search response: {result}"
            );
            if result["hits"].as_array().map(Vec::len) == Some(1) {
                assert_eq!(result["hits"][0]["agent"], "zcode");
                break;
            }
        } else {
            assert_eq!(
                output.status.code(),
                Some(7),
                "unexpected watch search error: {output:?}"
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let current = canonical(home.path())?;
    assert_eq!(current.len(), 4);
    for old in &original {
        assert!(
            current.contains(old),
            "watch replaced prior identity/content: {old:?}"
        );
    }
    for excluded in ["zcodechildneedle", "zcodewalchildneedle", SECRET] {
        let result = search(home.path(), excluded, None)?;
        assert_eq!(result["hits"].as_array().map(Vec::len), Some(0), "{result}");
    }
    assert_eq!(
        bundle(&path)?,
        committed,
        "watch reader modified source DB/WAL/sidecars"
    );
    assert_eq!(
        storage_summary(&home.path().join("cass-data")).manifest_count,
        0
    );
    Ok(())
}
