//! GH#521: filter conversation metadata before counting messages.
//! These exercise the real CLI/FrankenSQLite reader, not a stock SQLite model.
use assert_cmd::Command;
use coding_agent_search::franken_sync::compat::ConnectionExt;
use coding_agent_search::franken_sync::{Connection, params};
use serde_json::{Value, json};
use std::fs;
use std::path::Path;
use std::time::Duration;
use tempfile::TempDir;

const START: i64 = 1_705_276_800_000;
const END: i64 = START + 60_000;

fn archive(root: &Path) -> Connection {
    let db = root.join("agent_search.db");
    let conn = Connection::open(db.to_string_lossy().into_owned()).unwrap();
    // The UNIQUE constraint is important: production streams the canonical
    // sqlite_autoindex_messages_1 covering index, not idx_messages_conv_idx.
    conn.execute_batch(
        "CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT);
         INSERT INTO meta VALUES('schema_version', '20');
         CREATE TABLE agents(id INTEGER PRIMARY KEY, slug TEXT);
         INSERT INTO agents VALUES(1, 'claude_code'), (2, 'codex');
         CREATE TABLE sources(id TEXT PRIMARY KEY, kind TEXT);
         INSERT INTO sources VALUES('local', 'local'), ('backup', 'local'), ('laptop', 'ssh');
         CREATE TABLE workspaces(id INTEGER PRIMARY KEY, path TEXT);
         CREATE TABLE conversations(
             id INTEGER PRIMARY KEY, agent_id INTEGER, workspace_id INTEGER,
             title TEXT, started_at INTEGER, ended_at INTEGER, source_path TEXT,
             source_id TEXT, origin_host TEXT
         );
         CREATE INDEX idx_conversations_started_at ON conversations(started_at);
         CREATE TABLE messages(
             id INTEGER PRIMARY KEY, conversation_id INTEGER NOT NULL,
             idx INTEGER NOT NULL, role TEXT, content TEXT, created_at INTEGER,
             UNIQUE(conversation_id, idx)
         );
         CREATE INDEX idx_messages_conv_idx ON messages(conversation_id, idx);",
    )
    .unwrap();
    conn
}

fn conversation(
    conn: &Connection,
    id: i64,
    agent: Option<i64>,
    started: Option<i64>,
    source: Option<&str>,
    host: Option<&str>,
    count: i64,
) {
    conn.execute_compat(
        "INSERT INTO conversations
         (id, agent_id, title, started_at, ended_at, source_path, source_id, origin_host)
         VALUES(?1, ?2, NULL, ?3, ?4, ?5, ?6, ?7)",
        params![
            id,
            agent,
            started,
            started.map(|t| t + 20_000),
            format!("/sessions/{id}.jsonl"),
            source,
            host
        ],
    )
    .unwrap();
    for idx in 0..count {
        // Message timestamps deliberately lie OUTSIDE the session-start
        // window: timeline promises lifetime message counts.
        conn.execute_compat(
            "INSERT INTO messages(conversation_id, idx, role, content, created_at)
             VALUES(?1, ?2, 'user', 'small message', 0)",
            params![id, idx],
        )
        .unwrap();
    }
}

fn finish(conn: Connection) {
    conn.execute("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
    conn.close().unwrap();
}

fn command(root: &Path) -> std::process::Command {
    let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    cmd.env("HOME", root)
        .env("XDG_DATA_HOME", root.join("xdg-data"))
        .env("XDG_CONFIG_HOME", root.join("xdg-config"))
        .env("CASS_DATA_DIR", root)
        .env("CASS_AUTO_REFRESH", "0")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env_remove("CASS_DB_PATH")
        .env_remove("CASS_ROBOT_FORMAT")
        .env_remove("CASS_FORMAT")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("NO_COLOR", "1")
        .env("RAYON_NUM_THREADS", "2")
        .env("RUST_LOG", "error")
        .args(["timeline", "--data-dir"])
        .arg(root)
        .args([
            "--since",
            "2024-01-15T00:00:00Z",
            "--until",
            "2024-01-15T00:01:00Z",
        ]);
    cmd
}

fn entries(payload: &Value) -> Vec<&Value> {
    assert_eq!(payload["range"], json!({"start": START, "end": END}));
    let rows: Vec<&Value> = if let Some(rows) = payload["sessions"].as_array() {
        rows.iter().collect()
    } else {
        payload["groups"]
            .as_object()
            .expect("grouped timeline")
            .values()
            .flat_map(|group| group.as_array().expect("group array"))
            .collect()
    };
    assert_eq!(
        payload["total_sessions"].as_u64().unwrap() as usize,
        rows.len()
    );
    // This fixture's selected sessions belong to one minute, so grouping
    // must preserve the same descending order as the flat result.
    assert!(rows.windows(2).all(|w| {
        w[0]["started_at"].as_i64().unwrap() >= w[1]["started_at"].as_i64().unwrap()
    }));
    rows
}

#[test]
fn gh521_filters_counts_nulls_and_grouping_preserve_the_timeline_contract() {
    let tmp = TempDir::new().unwrap();
    let conn = archive(tmp.path());
    conn.execute("BEGIN IMMEDIATE").unwrap();
    conversation(&conn, 1, Some(1), Some(START - 1), Some("local"), None, 3);
    conversation(&conn, 2, Some(1), Some(START), Some("local"), None, 2);
    conversation(&conn, 3, Some(2), Some(START + 1_000), Some("local"), None, 0);
    conversation(
        &conn,
        4,
        Some(1),
        Some(START + 2_000),
        Some("backup"),
        Some("user@backup"),
        4,
    );
    conversation(
        &conn,
        5,
        Some(1),
        Some(START + 3_000),
        Some("laptop"),
        Some("user@laptop"),
        1,
    );
    conversation(
        &conn,
        6,
        Some(2),
        Some(START + 4_000),
        Some("   "),
        Some("user@remote"),
        3,
    );
    conversation(&conn, 7, Some(1), Some(END), Some("local"), None, 1);
    conversation(&conn, 8, Some(1), Some(END + 1), Some("local"), None, 2);
    conversation(&conn, 9, Some(1), None, Some("local"), None, 1);
    conversation(&conn, 10, None, Some(START + 5_000), None, None, 0);
    conn.execute("COMMIT").unwrap();
    finish(conn);
    let before = fs::read(tmp.path().join("agent_search.db")).unwrap();

    let cases: &[(&[&str], &[i64])] = &[
        (&[], &[7, 10, 6, 5, 4, 3, 2]),
        (&["--agent", "claude_code"], &[7, 5, 4, 2]),
        (&["--agent", "codex"], &[6, 3]),
        (
            &["--agent", "claude_code", "--agent", "codex"],
            &[7, 6, 5, 4, 3, 2],
        ),
        (&["--source", "local"], &[7, 10, 4, 3, 2]),
        (&["--source", "remote"], &[6, 5]),
        (&["--source", "backup"], &[4]),
        (&["--source", "user@remote"], &[6]),
        (&["--agent", "claude_code", "--source", "local"], &[7, 4, 2]),
        (&["--agent", "codex", "--source", "backup"], &[]),
        (&["--agent", "does-not-exist"], &[]),
    ];
    for grouping in ["none", "hour", "day"] {
        for (flags, expected) in cases {
            let output = Command::from(command(tmp.path()))
                .timeout(Duration::from_secs(30))
                .args(["--json", "--group-by", grouping])
                .args(*flags)
                .assert()
                .success()
                .get_output()
                .clone();
            let payload: Value = serde_json::from_slice(&output.stdout).unwrap();
            let rows = entries(&payload);
            let ids: Vec<i64> = rows.iter().map(|r| r["id"].as_i64().unwrap()).collect();
            assert_eq!(ids.as_slice(), *expected, "grouping={grouping} flags={flags:?}");
            for row in rows {
                let id = row["id"].as_i64().unwrap();
                let count = match id {
                    2 => 2,
                    3 | 10 => 0,
                    4 => 4,
                    5 | 7 => 1,
                    6 => 3,
                    _ => unreachable!(),
                };
                assert_eq!(row["message_count"], count, "session {id}");
                assert!(row["title"].is_null());
                assert_eq!(row["source_path"], format!("/sessions/{id}.jsonl"));
                if grouping == "none" {
                    assert_eq!(row["duration_seconds"], 20);
                } else {
                    assert!(row.get("duration_seconds").is_none());
                }
                if id == 10 {
                    assert_eq!(row["agent"], "unknown");
                }
                if id == 4 {
                    assert_eq!(row["source_id"], "backup");
                    assert_eq!(row["origin_kind"], "local");
                }
                if id == 6 {
                    assert_eq!(row["source_id"], "user@remote");
                    assert_eq!(row["origin_kind"], "remote");
                }
            }
        }
    }
    assert_eq!(
        fs::read(tmp.path().join("agent_search.db")).unwrap(),
        before,
        "timeline must not migrate or rewrite an archive"
    );
}

#[test]
fn gh521_filters_json_and_compact_preserve_escaping_and_nullable_metadata() {
    let tmp = TempDir::new().unwrap();
    let conn = archive(tmp.path());
    conversation(&conn, 1, Some(1), Some(START), Some("local"), None, 2);
    let title = "quote \" and slash \\ and newline\n Unicode λ\0";
    conn.execute_compat(
        "UPDATE conversations SET title = ?1, ended_at = NULL WHERE id = 1",
        params![title],
    )
    .unwrap();
    finish(conn);
    for grouping in ["none", "hour", "day"] {
        let mut payloads = Vec::new();
        for format in ["json", "compact"] {
            let output = Command::from(command(tmp.path()))
                .args(["--robot-format", format, "--group-by", grouping])
                .timeout(Duration::from_secs(30))
                .assert()
                .success()
                .get_output()
                .clone();
            assert!(output.stdout.ends_with(b"\n"));
            let value: Value = serde_json::from_slice(&output.stdout).unwrap();
            let rows = entries(&value);
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0]["title"], title);
            assert!(rows[0]["ended_at"].is_null());
            if grouping == "none" {
                assert!(rows[0]["duration_seconds"].is_null());
            } else {
                assert!(rows[0].get("duration_seconds").is_none());
            }
            payloads.push(value);
        }
        assert_eq!(payloads[0], payloads[1], "grouping={grouping}");
    }
}

#[cfg(target_os = "linux")]
fn bounded_timeline(root: &Path, flags: &[&str], expected: &[(i64, i64)]) {
    use std::process::Stdio;
    use std::time::Instant;

    let out = root.join("timeline.stdout");
    let err = root.join("timeline.stderr");
    let mut child_cmd = command(root);
    child_cmd
        .args(["--json", "--group-by", "none"])
        .args(flags)
        .stdout(Stdio::from(fs::File::create(&out).unwrap()))
        .stderr(Stdio::from(fs::File::create(&err).unwrap()));
    let mut child = child_cmd.spawn().unwrap();
    let started = Instant::now();
    let mut peak_kib = 0_u64;
    let status = loop {
        if let Ok(text) = fs::read_to_string(format!("/proc/{}/status", child.id())) {
            for line in text
                .lines()
                .filter(|s| s.starts_with("VmHWM:") || s.starts_with("VmRSS:"))
            {
                if let Some(kib) = line
                    .split_whitespace()
                    .nth(1)
                    .and_then(|s| s.parse::<u64>().ok())
                {
                    peak_kib = peak_kib.max(kib);
                }
            }
        }
        if peak_kib > 384 * 1024 || started.elapsed() > Duration::from_secs(60) {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "GH521 flags={flags:?} peak_kib={peak_kib} elapsed={:?}; stderr={}",
                started.elapsed(),
                fs::read_to_string(&err).unwrap_or_default()
            );
        }
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(
        status.success(),
        "{status}: {}",
        fs::read_to_string(&err).unwrap()
    );
    assert!(
        peak_kib > 0,
        "RSS probe observed no process samples; cannot claim a memory pass"
    );
    assert!(
        fs::metadata(&out).unwrap().len() < 16 * 1024,
        "narrow output must be small"
    );
    let payload: Value = serde_json::from_slice(&fs::read(&out).unwrap()).unwrap();
    let actual: Vec<(i64, i64)> = entries(&payload)
        .into_iter()
        .map(|row| {
            (
                row["id"].as_i64().unwrap(),
                row["message_count"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(actual.as_slice(), expected, "flags={flags:?}");
    eprintln!(
        "GH521 flags={flags:?} peak_kib={peak_kib} elapsed={:?}",
        started.elapsed()
    );
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "GH521 archive-scale RSS regression; run explicitly with --ignored"]
fn gh521_narrow_window_does_not_materialize_unrelated_message_payloads() {
    let tmp = TempDir::new().unwrap();
    let conn = archive(tmp.path());
    conversation(&conn, 1, Some(1), Some(START - 1), Some("local"), None, 0);
    conversation(&conn, 2, Some(1), Some(START), Some("local"), None, 2);
    // Match the reporter's conversation cardinality without retaining a tree
    // of conversations/messages in the fixture builder.
    for batch in 0..127_i64 {
        conn.execute("BEGIN IMMEDIATE").unwrap();
        for id in (3 + batch * 100)..(3 + (batch + 1) * 100).min(12_668) {
            conversation(
                &conn,
                id,
                Some(2),
                Some(START - 86_400_000 - id),
                Some("local"),
                None,
                0,
            );
        }
        conn.execute("COMMIT").unwrap();
    }
    // More than 512 MiB of payload. Only one reusable buffer and 64-row
    // transactions are held while constructing it.
    let payload = "x".repeat(64 * 1024);
    for batch in 0..128_i64 {
        conn.execute("BEGIN IMMEDIATE").unwrap();
        for offset in 0..64_i64 {
            conn.execute_compat(
                "INSERT INTO messages(conversation_id, idx, role, content, created_at)
                 VALUES(1, ?1, 'user', ?2, 0)",
                params![batch * 64 + offset, payload.as_str()],
            )
            .unwrap();
        }
        conn.execute("COMMIT").unwrap();
    }
    finish(conn);
    assert!(
        fs::metadata(tmp.path().join("agent_search.db"))
            .unwrap()
            .len()
            > 512 * 1024 * 1024
    );
    for flags in [
        vec![],
        vec!["--agent", "claude_code"],
        vec!["--source", "local"],
    ] {
        bounded_timeline(tmp.path(), &flags, &[(2, 2)]);
    }
    bounded_timeline(
        tmp.path(),
        &["--agent", "codex", "--source", "remote"],
        &[],
    );

    // Now select the huge conversation too. Counting its turns must remain
    // bounded even when the time/agent/source predicates all match it.
    let conn = Connection::open(
        tmp.path()
            .join("agent_search.db")
            .to_string_lossy()
            .into_owned(),
    )
    .unwrap();
    conn.execute_compat(
        "UPDATE conversations SET started_at = ?1, ended_at = ?2 WHERE id = 1",
        params![START + 1, START + 20_001],
    )
    .unwrap();
    finish(conn);
    bounded_timeline(
        tmp.path(),
        &["--agent", "claude_code", "--source", "local"],
        &[(1, 8192), (2, 2)],
    );
}
