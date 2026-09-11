//! GH415 CASS integration using the schema and numeric llm payloads exercised
//! by published franken-agent-detection 0.2.4's Shelley fixture tests.
//! These generated fixtures are not evidence about a user's private database.

use coding_agent_search::connectors::{Connector, ScanContext, ScanRoot};
use coding_agent_search::franken_sync::Connection;
use coding_agent_search::franken_sync::compat::{ConnectionExt, RowExt};
use coding_agent_search::franken_sync::params;
use coding_agent_search::raw_mirror::{
    RawMirrorCaptureInput, capture_source_file, storage_summary,
};
use coding_agent_search::storage::sqlite::SqliteStorage;
use franken_agent_detection::ShelleyConnector;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

type SourceBundle = Vec<Option<(Vec<u8>, std::time::SystemTime)>>;
type CanonicalSession = (i64, String, String, String, Value);

fn fixture(path: &Path) -> Connection {
    let conn = Connection::open(path.to_string_lossy().as_ref()).unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;
         CREATE TABLE migrations (id INTEGER PRIMARY KEY, migration_number INTEGER NOT NULL,
             migration_name TEXT NOT NULL UNIQUE, executed_at DATETIME DEFAULT CURRENT_TIMESTAMP);
         CREATE TABLE conversations (conversation_id TEXT PRIMARY KEY, slug TEXT,
             user_initiated BOOLEAN NOT NULL DEFAULT TRUE,
             created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
             updated_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP, cwd TEXT,
             archived BOOLEAN NOT NULL DEFAULT FALSE, parent_conversation_id TEXT,
             model TEXT, conversation_options TEXT NOT NULL DEFAULT '{}',
             current_generation INTEGER NOT NULL DEFAULT 0, tags TEXT NOT NULL DEFAULT '[]',
             is_draft BOOLEAN NOT NULL DEFAULT FALSE, draft TEXT NOT NULL DEFAULT '',
             queued_messages TEXT NOT NULL DEFAULT '[]');
         CREATE TABLE messages (message_id TEXT PRIMARY KEY, conversation_id TEXT NOT NULL,
             sequence_id INTEGER NOT NULL, type TEXT NOT NULL, llm_data TEXT, user_data TEXT,
             usage_data TEXT, created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
             display_data TEXT, excluded_from_context BOOLEAN NOT NULL DEFAULT FALSE,
             generation INTEGER NOT NULL DEFAULT 0, llm_api_url TEXT, model_name TEXT,
             forked_from_message_id TEXT, user_email TEXT, other_usage_data TEXT);",
    )
    .unwrap();
    for (number, name) in [
        (1, "001-conversations.sql"),
        (2, "002-messages.sql"),
        (3, "003-add-message-sequence.sql"),
    ] {
        conn.execute_compat(
            "INSERT INTO migrations (migration_number,migration_name) VALUES (?,?)",
            params![number, name],
        )
        .unwrap();
    }
    insert_session(&conn, "alpha", "shelleyalphaneedle");
    insert_session(&conn, "beta", "shelleybetaneedle");
    conn
}

fn insert_session(conn: &Connection, id: &str, text: &str) {
    conn.execute_compat(
        "INSERT INTO conversations (conversation_id,slug,cwd,created_at,updated_at)
        VALUES (?,?, '/work/original', '2026-08-01 10:00:00', '2026-08-01 10:00:00')",
        params![id, format!("title-{id}")],
    )
    .unwrap();
    // Preserve a legitimate sequence gap; timestamps deliberately predate the
    // later filesystem event and must not hide a committed WAL-only session.
    conn.execute_compat("INSERT INTO messages
        (message_id,conversation_id,sequence_id,type,llm_data,created_at) VALUES (?,?,7,'user',?,'2026-08-01 10:30:00')",
        params![format!("message-{id}"), id,
            json!({"Role":0,"Content":[{"Type":2,"Text":text}]}).to_string()]).unwrap();
}

fn insert_child(conn: &Connection, id: &str, text: &str) {
    insert_session(conn, id, text);
    conn.execute_compat(
        "UPDATE conversations SET parent_conversation_id='alpha' WHERE conversation_id=?",
        params![id],
    )
    .unwrap();
}

fn bundle(path: &Path) -> SourceBundle {
    ["", "-wal", "-shm"]
        .into_iter()
        .map(|suffix| {
            let path = PathBuf::from(format!("{}{suffix}", path.display()));
            path.exists().then(|| {
                (
                    fs::read(&path).unwrap(),
                    fs::metadata(path).unwrap().modified().unwrap(),
                )
            })
        })
        .collect()
}

fn command(home: &Path, data: &Path, db: &Path) -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    cmd.env_clear()
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("PATH", "")
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("CASS_SHELLEY_DB", db)
        .env("CASS_DATA_DIR", data)
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("CASS_AUTO_REFRESH", "0")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("RUST_MIN_STACK", "134217728")
        .current_dir(home);
    if let Ok(system_root) = dotenvy::var("SystemRoot") {
        cmd.env("SystemRoot", system_root);
    }
    cmd
}

fn search(home: &Path, data: &Path, db: &Path, needle: &str, workspace: Option<&str>) -> Value {
    search_when_published(home, data, db, needle, workspace, Duration::from_secs(10))
        .expect("lexical publication must be complete outside the watch polling loop")
}

fn search_when_published(
    home: &Path,
    data: &Path,
    db: &Path,
    needle: &str,
    workspace: Option<&str>,
    timeout: Duration,
) -> Option<Value> {
    assert!(!timeout.is_zero(), "search publication deadline expired");
    let mut cmd = assert_cmd::Command::from_std(command(home, data, db));
    cmd.args([
        "search",
        needle,
        "--mode",
        "lexical",
        "--json",
        "--no-maintenance",
        "--timeout",
        "3000",
        "--limit",
        "100",
    ]);
    if let Some(workspace) = workspace {
        cmd.args(["--workspace", workspace]);
    }
    let output = cmd.timeout(timeout).output().unwrap();
    if output.status.code() == Some(7) {
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(error["error"]["kind"], "index-busy", "{error}");
        assert_eq!(error["error"]["retryable"], true, "{error}");
        return None;
    }
    assert!(output.status.success(), "search failed: {output:?}");
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(value["hits"].is_array(), "missing hits: {value}");
    assert_ne!(
        value.pointer("/budget/timed_out").and_then(Value::as_bool),
        Some(true),
        "timeout cannot establish absence: {value}"
    );
    Some(value)
}

fn canonical(data: &Path) -> Vec<CanonicalSession> {
    let storage = SqliteStorage::open_readonly(&data.join("agent_search.db")).unwrap();
    storage
        .raw()
        .query(
            "SELECT c.id,c.external_id,c.title,w.path FROM conversations c
        JOIN agents a ON a.id=c.agent_id JOIN workspaces w ON w.id=c.workspace_id
        WHERE a.slug='shelley' ORDER BY c.external_id",
        )
        .unwrap()
        .into_iter()
        .map(|row| {
            let id = row.get_typed::<i64>(0).unwrap();
            (
                id,
                row.get_typed::<String>(1).unwrap(),
                row.get_typed::<String>(2).unwrap(),
                row.get_typed::<String>(3).unwrap(),
                serde_json::to_value(storage.fetch_messages(id).unwrap()).unwrap(),
            )
        })
        .collect()
}

fn analytics_without_workspace(data: &Path) -> Vec<String> {
    let storage = SqliteStorage::open_readonly(&data.join("agent_search.db")).unwrap();
    ["message_metrics", "token_usage"]
        .into_iter()
        .map(|table| {
            let columns: Vec<String> = storage
                .raw()
                .query(&format!("PRAGMA table_info({table})"))
                .unwrap()
                .iter()
                .map(|row| row.get_typed::<String>(1).unwrap())
                .filter(|column| column != "workspace_id")
                .collect();
            let rows = storage
                .raw()
                .query(&format!(
                    "SELECT {} FROM {table} ORDER BY message_id",
                    columns.join(",")
                ))
                .unwrap();
            assert!(
                !rows.is_empty(),
                "{table} must contain real ingestion analytics"
            );
            format!("{rows:?}")
        })
        .collect()
}

fn assert_analytics_workspace_matches_canonical(data: &Path) {
    let storage = SqliteStorage::open_readonly(&data.join("agent_search.db")).unwrap();
    for table in ["message_metrics", "token_usage"] {
        let rows = storage
            .raw()
            .query(&format!(
                "SELECT COUNT(*) FROM {table} t
            JOIN messages m ON m.id=t.message_id JOIN conversations c ON c.id=m.conversation_id
            WHERE t.workspace_id != c.workspace_id"
            ))
            .unwrap();
        assert_eq!(
            rows[0].get_typed::<i64>(0).unwrap(),
            0,
            "{table} kept obsolete workspace"
        );
    }
}

#[test]
fn shelley_local_read_conserves_source_and_sensitive_capture_is_denied() {
    let home = tempfile::tempdir().unwrap();
    let db = home.path().join("arbitrary-name.db");
    let _writer = fixture(&db);
    let before = bundle(&db);
    let connector = ShelleyConnector::new();
    let local = ScanContext::with_roots(
        home.path().join("data"),
        vec![ScanRoot::local(db.clone())],
        None,
    );
    let conversations = connector.scan(&local).unwrap();
    assert_eq!(conversations.len(), 2);
    assert!(
        conversations
            .iter()
            .all(|c| c.agent_slug == "shelley" && c.messages.len() == 1 && c.messages[0].idx == 7)
    );
    assert_eq!(bundle(&db), before, "local reader mutated provider files");
    let remote = ScanContext::with_roots(
        home.path().join("data"),
        vec![ScanRoot::remote(
            db.clone(),
            franken_agent_detection::types::Origin::remote("fixture-host"),
            None,
        )],
        None,
    );
    let error = connector.scan(&remote).unwrap_err();
    assert!(format!("{error:#}").contains("remote Shelley databases are not supported"));
    let data = home.path().join("capture-data");
    for origin in ["local", "remote"] {
        for suffix in ["", "-wal", "-shm"] {
            let source = PathBuf::from(format!("{}{suffix}", db.display()));
            let error = capture_source_file(RawMirrorCaptureInput {
                data_dir: &data,
                provider: "shelley",
                source_id: "fixture",
                origin_kind: origin,
                origin_host: None,
                source_path: &source,
                db_links: &[],
            })
            .unwrap_err();
            assert!(
                error.to_string().contains("disabled_sensitive_container"),
                "{error}"
            );
        }
    }
    assert!(!data.exists(), "denial must precede mirror initialization");
    assert_eq!(bundle(&db), before);
}

#[test]
fn shelley_batch_refreshes_metadata_without_replacing_messages_or_mirroring_database() {
    let home = tempfile::tempdir().unwrap();
    let db = home.path().join("arbitrary-name.db");
    let data = home.path().join("data");
    let writer = fixture(&db);
    let mut original = Vec::new();
    let mut original_analytics = Vec::new();
    for round in 0..3 {
        if round == 1 {
            writer
                .execute_compat(
                    "UPDATE conversations SET slug='renamed-alpha',cwd='/work/renamed',
                archived=1,tags='[\"reviewed\"]' WHERE conversation_id='alpha'",
                    params![],
                )
                .unwrap();
        }
        let before = bundle(&db);
        let mut index = assert_cmd::Command::from_std(command(home.path(), &data, &db));
        if round == 0 {
            // Populate both analytics tracks through the real ingestion path.
            // Subsequent metadata-only refreshes use the default deferred policy
            // and must conserve those existing measurements without rebuilding.
            index.env("CASS_INLINE_ANALYTICS_UPDATES", "1");
        }
        index
            .args(["index", "--full", "--json"])
            .timeout(Duration::from_secs(120))
            .assert()
            .success();
        let rows = canonical(&data);
        assert_eq!(
            rows.len(),
            2,
            "shared database must retain distinct session identities"
        );
        if round == 0 {
            original = rows.clone();
            original_analytics = analytics_without_workspace(&data);
        } else {
            assert_eq!(
                analytics_without_workspace(&data),
                original_analytics,
                "workspace refresh changed stored analytics measurements"
            );
            assert_analytics_workspace_matches_canonical(&data);
            for (old, new) in original.iter().zip(&rows) {
                assert_eq!(
                    (&old.0, &old.1, &old.4),
                    (&new.0, &new.1, &new.4),
                    "metadata refresh replaced canonical identity/messages"
                );
            }
            let alpha = rows.iter().find(|r| r.1.ends_with(":alpha")).unwrap();
            assert_eq!(alpha.2, "renamed-alpha");
            assert_eq!(alpha.3, "/work/renamed");
            let archive = SqliteStorage::open_readonly(&data.join("agent_search.db")).unwrap();
            let saved = archive
                .list_conversations(20, 0)
                .unwrap()
                .into_iter()
                .find(|c| {
                    c.external_id
                        .as_deref()
                        .is_some_and(|id| id.ends_with(":alpha"))
                })
                .unwrap();
            assert_eq!(saved.metadata_json["shelley"]["archived"], true);
            assert_eq!(saved.metadata_json["shelley"]["tags"], json!(["reviewed"]));
            drop(archive);
            assert_eq!(
                search(
                    home.path(),
                    &data,
                    &db,
                    "shelleyalphaneedle",
                    Some("/work/renamed")
                )["hits"]
                    .as_array()
                    .unwrap()
                    .len(),
                1
            );
            assert!(
                search(
                    home.path(),
                    &data,
                    &db,
                    "shelleyalphaneedle",
                    Some("/work/original")
                )["hits"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
        }
        for needle in ["shelleyalphaneedle", "shelleybetaneedle"] {
            let result = search(home.path(), &data, &db, needle, None);
            assert_eq!(result["hits"].as_array().unwrap().len(), 1, "{result}");
            assert_eq!(result["hits"][0]["agent"], "shelley");
        }
        assert_eq!(
            storage_summary(&data).manifest_count,
            0,
            "sensitive container leaked into raw mirror"
        );
        assert_eq!(storage_summary(&data).unique_blob_count, 0);
        assert_eq!(bundle(&db), before);
    }
}

#[test]
fn shelley_live_watch_reads_wal_only_new_session_and_metadata_change() {
    struct WatchChild(Child);
    impl Drop for WatchChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let home = tempfile::tempdir().unwrap();
    let db = home.path().join("custom-store.sqlite3");
    let data = home.path().join("data");
    let writer = fixture(&db);
    insert_child(&writer, "initial-child", "shelleyinitialchildneedle");
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
        command(home.path(), &data, &db)
            .env("RUST_LOG", "info")
            .env("CASS_SKIP_SUBAGENTS", "1")
            .arg("--verbose")
            .args(["index", "--watch", "--watch-interval", "1", "--json"])
            .stdout(Stdio::from(fs::File::create(&stdout).unwrap()))
            .stderr(Stdio::from(fs::File::create(&stderr).unwrap()))
            .spawn()
            .unwrap(),
    );
    let startup = Instant::now() + Duration::from_secs(90);
    while !logs().contains("watch mode: minimum interval between scan cycles") {
        assert!(
            watch.0.try_wait().unwrap().is_none(),
            "watch exited: {}",
            logs()
        );
        assert!(
            Instant::now() < startup,
            "watch startup timeout: {}",
            logs()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let original = canonical(&data);
    assert_eq!(original.len(), 2);
    let main_before = fs::read(&db).unwrap();
    writer.execute_batch("BEGIN;").unwrap();
    insert_session(&writer, "gamma", "shelleygammaneedle");
    insert_child(&writer, "later-child", "shelleylaterchildneedle");
    writer.execute_batch("UPDATE conversations SET slug='live-renamed',cwd='/work/live' WHERE conversation_id='alpha'; COMMIT;").unwrap();
    assert_eq!(
        fs::read(&db).unwrap(),
        main_before,
        "fixture commit must remain WAL-only"
    );
    let committed = bundle(&db);
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        assert!(
            Instant::now() < deadline,
            "WAL-only session/metadata not indexed: {}",
            logs()
        );
        let rows = canonical(&data);
        if rows.len() == 3
            && rows
                .iter()
                .any(|r| r.1.ends_with(":alpha") && r.2 == "live-renamed" && r.3 == "/work/live")
        {
            for old in &original {
                let new = rows.iter().find(|r| r.1 == old.1).unwrap();
                assert_eq!((&old.0, &old.4), (&new.0, &new.4));
            }
            // Canonical commit precedes lexical publication. Wait for both
            // observable query results before certifying the watch cycle.
            let gamma = search_when_published(
                home.path(),
                &data,
                &db,
                "shelleygammaneedle",
                None,
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_secs(10)),
            );
            let alpha = search_when_published(
                home.path(),
                &data,
                &db,
                "shelleyalphaneedle",
                Some("/work/live"),
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_secs(10)),
            );
            if gamma.is_some_and(|v| v["hits"].as_array().unwrap().len() == 1)
                && alpha.is_some_and(|v| v["hits"].as_array().unwrap().len() == 1)
            {
                assert!(Instant::now() < deadline, "publication missed its deadline");
                break;
            }
        }
        assert!(
            watch.0.try_wait().unwrap().is_none(),
            "watch exited: {}",
            logs()
        );
        assert!(
            Instant::now() < deadline,
            "WAL-only session/metadata not indexed: {}",
            logs()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    for needle in [
        "shelleyalphaneedle",
        "shelleybetaneedle",
        "shelleygammaneedle",
    ] {
        assert_eq!(
            search(home.path(), &data, &db, needle, None)["hits"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
    assert_eq!(
        search(
            home.path(),
            &data,
            &db,
            "shelleyalphaneedle",
            Some("/work/live")
        )["hits"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(
        search(
            home.path(),
            &data,
            &db,
            "shelleyalphaneedle",
            Some("/work/original")
        )["hits"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    for needle in ["shelleyinitialchildneedle", "shelleylaterchildneedle"] {
        assert!(
            search(home.path(), &data, &db, needle, None)["hits"]
                .as_array()
                .unwrap()
                .is_empty(),
            "subagent exclusion must apply during initial scan and WAL-triggered scan"
        );
    }
    assert_eq!(
        bundle(&db),
        committed,
        "watch reader mutated provider DB/WAL/SHM"
    );
    assert_eq!(storage_summary(&data).manifest_count, 0);
    assert_eq!(storage_summary(&data).unique_blob_count, 0);
}

#[test]
fn shelley_subagent_toggle_applies_to_batch_and_streaming() {
    for streaming in ["0", "1"] {
        for skip in ["0", "1"] {
            let home = tempfile::tempdir().unwrap();
            let db = home.path().join("provider.sqlite3");
            let data = home.path().join("data");
            let writer = fixture(&db);
            insert_child(&writer, "child", "shelleychildtoggleproof");
            let before = bundle(&db);
            assert_cmd::Command::from_std(command(home.path(), &data, &db))
                .env("CASS_STREAMING_INDEX", streaming)
                .env("CASS_SKIP_SUBAGENTS", skip)
                .args(["index", "--full", "--json"])
                .timeout(Duration::from_secs(120))
                .assert()
                .success();
            let rows = canonical(&data);
            assert_eq!(
                rows.len(),
                if skip == "1" { 2 } else { 3 },
                "streaming={streaming} skip={skip}"
            );
            assert_eq!(
                search(home.path(), &data, &db, "shelleychildtoggleproof", None)["hits"]
                    .as_array()
                    .unwrap()
                    .len(),
                usize::from(skip == "0")
            );
            assert_eq!(
                search(home.path(), &data, &db, "shelleyalphaneedle", None)["hits"]
                    .as_array()
                    .unwrap()
                    .len(),
                1,
                "parent must remain searchable"
            );
            assert_eq!(bundle(&db), before);
        }
    }
}

#[test]
fn shelley_configured_local_file_scans_only_the_requested_database() {
    use coding_agent_search::sources::config::{SourceDefinition, SourcesConfig};
    let home = tempfile::tempdir().unwrap();
    let provider = home.path().join("provider");
    fs::create_dir_all(&provider).unwrap();
    let db = provider.join("chosen.sqlite3");
    let _writer = fixture(&db);
    let neighbor = provider.join("neighbor.sqlite3");
    let _neighbor_writer = fixture(&neighbor);
    let before = bundle(&db);
    let neighbor_before = bundle(&neighbor);
    let data = home.path().join("data");
    let mut source = SourceDefinition::local("shelley-configured");
    source.paths = vec![db.to_string_lossy().into_owned()];
    let config = SourcesConfig {
        sources: vec![source],
        disabled_agents: coding_agent_search::connectors::get_connector_factories()
            .into_iter()
            .map(|(name, _)| name.to_string())
            .filter(|name| name != "shelley")
            .collect(),
    };
    let config_path = home.path().join(".config/cass/sources.toml");
    fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    fs::write(&config_path, toml::to_string(&config).unwrap()).unwrap();
    let config_before = fs::read(&config_path).unwrap();
    let mut cmd = command(home.path(), &data, &db);
    cmd.env_remove("CASS_SHELLEY_DB")
        .env_remove("CASS_IGNORE_SOURCES_CONFIG");
    assert_cmd::Command::from_std(cmd)
        .args(["index", "--full", "--json"])
        .timeout(Duration::from_secs(120))
        .assert()
        .success();
    let rows = canonical(&data);
    assert_eq!(rows.len(), 2);
    for needle in ["shelleyalphaneedle", "shelleybetaneedle"] {
        let result = search(home.path(), &data, &db, needle, None);
        assert_eq!(
            result["hits"].as_array().unwrap().len(),
            1,
            "explicit source must not scan the neighboring valid Shelley database"
        );
        assert_eq!(
            result["hits"][0]["source_path"],
            db.to_string_lossy().as_ref()
        );
    }
    assert_eq!(bundle(&db), before);
    assert_eq!(bundle(&neighbor), neighbor_before);
    assert_eq!(fs::read(&config_path).unwrap(), config_before);
    assert_eq!(storage_summary(&data).manifest_count, 0);
    let mut doctor = command(home.path(), &data, &db);
    doctor
        .env_remove("CASS_SHELLEY_DB")
        .env_remove("CASS_IGNORE_SOURCES_CONFIG");
    let output = assert_cmd::Command::from_std(doctor)
        .args(["doctor", "archive-scan", "--json", "--verbose"])
        .timeout(Duration::from_secs(60))
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let report: Value = serde_json::from_slice(&output).unwrap();
    let receipts = report["raw_mirror_backfill"]["receipts"]
        .as_array()
        .unwrap();
    assert_eq!(
        receipts.len(),
        2,
        "doctor must account for both canonical sessions"
    );
    assert!(
        receipts
            .iter()
            .all(|receipt| receipt["provider"] == "shelley"
                && receipt["action"] == "disabled_sensitive_container"
                && receipt["raw_source_captured"] == false)
    );
    assert_eq!(
        report["raw_mirror_backfill"]["eligible_live_source_count"],
        0
    );
    assert_eq!(bundle(&db), before);
    assert_eq!(bundle(&neighbor), neighbor_before);
    assert_eq!(storage_summary(&data).manifest_count, 0);
}
