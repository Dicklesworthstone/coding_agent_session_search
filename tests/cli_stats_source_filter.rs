use assert_cmd::cargo::cargo_bin_cmd;
use coding_agent_search::franken_sync::Connection as FrankenConnection;
use coding_agent_search::franken_sync::compat::ConnectionExt;
use serde_json::Value;
use tempfile::TempDir;

#[test]
fn stats_source_filter_preserves_date_range() {
    let tmp = TempDir::new().expect("tempdir");
    let data_dir = tmp.path();
    let db_path = data_dir.join("agent_search.db");

    // Minimal schema required by `cass stats` queries.
    let conn = FrankenConnection::open(db_path.to_string_lossy().into_owned()).expect("open db");
    conn.execute("CREATE TABLE agents (id INTEGER PRIMARY KEY, slug TEXT NOT NULL)")
        .expect("create agents");
    conn.execute("CREATE TABLE workspaces (id INTEGER PRIMARY KEY, path TEXT NOT NULL)")
        .expect("create workspaces");
    conn.execute(
        "CREATE TABLE conversations (
            id INTEGER PRIMARY KEY,
            agent_id INTEGER NOT NULL,
            workspace_id INTEGER,
            source_id TEXT NOT NULL,
            started_at INTEGER
        )",
    )
    .expect("create conversations");
    conn.execute(
        "CREATE TABLE messages (id INTEGER PRIMARY KEY, conversation_id INTEGER NOT NULL)",
    )
    .expect("create messages");

    conn.execute("INSERT INTO agents (id, slug) VALUES (1, 'codex')")
        .expect("insert agent");
    conn.execute("INSERT INTO workspaces (id, path) VALUES (1, '/tmp/ws')")
        .expect("insert workspace");

    let ts = 1_700_000_000_000i64;
    conn.execute_compat(
        "INSERT INTO conversations (id, agent_id, workspace_id, source_id, started_at)
         VALUES (1, 1, 1, 'local', ?1)",
        coding_agent_search::franken_sync::params![ts],
    )
    .expect("insert conversation");
    conn.execute("INSERT INTO messages (id, conversation_id) VALUES (1, 1)")
        .expect("insert message");

    let out = cargo_bin_cmd!("cass")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .arg("stats")
        .arg("--json")
        .arg("--source")
        .arg("local")
        .arg("--data-dir")
        .arg(data_dir)
        .assert()
        .success()
        .get_output()
        .clone();

    let json: Value = serde_json::from_slice(&out.stdout).expect("valid json");
    assert!(
        json.get("date_range")
            .and_then(|d| d.get("oldest"))
            .is_some_and(|v| v.is_string()),
        "expected date_range.oldest to be a string, got: {json}"
    );
    assert!(
        json.get("date_range")
            .and_then(|d| d.get("newest"))
            .is_some_and(|v| v.is_string()),
        "expected date_range.newest to be a string, got: {json}"
    );
}

fn stats_test_archive(omit: Option<&str>) -> TempDir {
    let tmp = TempDir::new().expect("stats archive");
    let conn = FrankenConnection::open(
        tmp.path()
            .join("agent_search.db")
            .to_string_lossy()
            .into_owned(),
    )
    .expect("create archive");
    conn.execute("CREATE TABLE agents (id INTEGER PRIMARY KEY, slug TEXT NOT NULL)")
        .expect("agents");
    conn.execute("CREATE TABLE workspaces (id INTEGER PRIMARY KEY, path TEXT NOT NULL)")
        .expect("workspaces");
    if omit != Some("conversations") {
        let date_column = if omit == Some("dates") {
            ""
        } else {
            ", started_at INTEGER"
        };
        conn.execute(&format!("CREATE TABLE conversations (id INTEGER PRIMARY KEY, agent_id INTEGER, workspace_id INTEGER, source_id TEXT, origin_host TEXT{date_column})")).expect("conversations");
    }
    if omit != Some("messages") {
        conn.execute("CREATE TABLE messages (id INTEGER PRIMARY KEY, conversation_id INTEGER)")
            .expect("messages");
    }
    conn.close().expect("close archive");
    tmp
}

#[test]
fn stats_storage_errors_never_report_successful_zero_counts() {
    for (omitted, operation) in [
        ("conversations", "count conversations"),
        ("messages", "count messages"),
        ("dates", "read conversation date range"),
    ] {
        let tmp = stats_test_archive(Some(omitted));
        let output = cargo_bin_cmd!("cass")
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .args(["stats", "--json", "--data-dir"])
            .arg(tmp.path())
            .assert()
            .code(5)
            .get_output()
            .clone();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(operation),
            "missing operation context: {stderr}"
        );
        assert!(
            stderr.contains("cass doctor --json"),
            "missing diagnostic hint: {stderr}"
        );
        if let Ok(json) = serde_json::from_slice::<Value>(&output.stdout) {
            assert!(
                json.get("conversations").is_none(),
                "error must not emit success counts: {json}"
            );
        }
    }
}

#[test]
fn stats_empty_archive_and_unmatched_source_are_real_zeros() {
    let tmp = stats_test_archive(None);
    for source in [None, Some("absent")] {
        let mut command = cargo_bin_cmd!("cass");
        command
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .args(["stats", "--json", "--data-dir"])
            .arg(tmp.path());
        if let Some(source) = source {
            command.args(["--source", source]);
        }
        let output = command.assert().success().get_output().clone();
        let json: Value = serde_json::from_slice(&output.stdout).expect("stats JSON");
        assert_eq!(json["conversations"], 0);
        assert_eq!(json["messages"], 0);
        assert_eq!(json["by_agent"], serde_json::json!([]));
        assert_eq!(json["top_workspaces"], serde_json::json!([]));
        assert!(json["date_range"]["oldest"].is_null());
        assert!(json["date_range"]["newest"].is_null());
    }
}

#[test]
fn stats_refuses_invalid_fts_schema_even_when_canonical_counts_are_empty() {
    let tmp = stats_test_archive(None);
    let conn = FrankenConnection::open(
        tmp.path()
            .join("agent_search.db")
            .to_string_lossy()
            .into_owned(),
    )
    .expect("open archive");
    // A malformed ordinary table occupying the derived FTS name must not
    // bypass the existing required-shadow checks merely because LIMIT 0 works.
    conn.execute("CREATE TABLE fts_messages (content TEXT)")
        .expect("plant invalid FTS schema");
    conn.close().expect("publish malformed derived object");
    let output = cargo_bin_cmd!("cass")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .args(["stats", "--json", "--data-dir"])
        .arg(tmp.path())
        .assert()
        .code(5)
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("fts_messages"),
        "missing FTS diagnosis: {stderr}"
    );
    assert!(
        stderr.contains("shadow"),
        "missing shadow integrity detail: {stderr}"
    );
}

#[test]
fn stats_workspace_ties_filter_orphans_before_top_ten() {
    let tmp = stats_test_archive(None);
    let conn = FrankenConnection::open(
        tmp.path()
            .join("agent_search.db")
            .to_string_lossy()
            .into_owned(),
    )
    .expect("open archive");
    conn.execute("INSERT INTO agents VALUES (1, 'codex')")
        .expect("agent");
    // IDs determine ties, independently of insertion order or path spelling.
    for id in (1..=12_i64).rev() {
        conn.execute_compat(
            "INSERT INTO workspaces VALUES (?1, ?2)",
            coding_agent_search::franken_sync::params![id, format!("/workspace/{id:02}")],
        )
        .expect("workspace");
        conn.execute_compat("INSERT INTO conversations (id, agent_id, workspace_id, source_id) VALUES (?1, 1, ?1, 'local')",
            coding_agent_search::franken_sync::params![id]).expect("conversation");
    }
    conn.execute("INSERT INTO conversations (id, agent_id, workspace_id, source_id) VALUES (20, NULL, 0, 'local'), (21, NULL, 0, 'local'), (22, NULL, NULL, 'local')").expect("orphan and null groups");
    conn.execute("INSERT INTO messages VALUES (1, 1)")
        .expect("message");
    conn.close().expect("publish archive");
    let output = cargo_bin_cmd!("cass")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .args(["stats", "--json", "--data-dir"])
        .arg(tmp.path())
        .assert()
        .success()
        .get_output()
        .clone();
    let json: Value = serde_json::from_slice(&output.stdout).expect("stats JSON");
    assert_eq!(json["conversations"], 15);
    assert_eq!(json["messages"], 1);
    assert_eq!(
        json["by_agent"],
        serde_json::json!([
            {"agent": "codex", "count": 12}, {"agent": "unknown", "count": 3}
        ])
    );
    let expected: Vec<Value> = (1..=10)
        .map(|id| {
            serde_json::json!({
                "workspace": format!("/workspace/{id:02}"), "count": 1
            })
        })
        .collect();
    assert_eq!(json["top_workspaces"], serde_json::json!(expected));
    let unmatched = cargo_bin_cmd!("cass")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .args(["stats", "--json", "--source", "absent", "--data-dir"])
        .arg(tmp.path())
        .assert()
        .success()
        .get_output()
        .clone();
    let absent: Value = serde_json::from_slice(&unmatched.stdout).expect("unmatched JSON");
    assert_eq!(absent["conversations"], 0);
    assert_eq!(absent["messages"], 0);
    assert_eq!(absent["top_workspaces"], serde_json::json!([]));
}
