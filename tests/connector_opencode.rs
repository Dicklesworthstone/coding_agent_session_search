//! Tests for the OpenCode connector (JSON file-based storage)

use coding_agent_search::connectors::opencode::OpenCodeConnector;
use coding_agent_search::connectors::{Connector, ScanContext};
use coding_agent_search::franken_sync::Connection;
use coding_agent_search::franken_sync::compat::ConnectionExt;
use coding_agent_search::franken_sync::params;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// Helper to create a JSON-based OpenCode storage structure
fn create_test_storage(dir: &std::path::Path, sessions: &[TestSession]) -> std::io::Result<()> {
    // Create directories
    fs::create_dir_all(dir.join("session"))?;
    fs::create_dir_all(dir.join("message"))?;
    fs::create_dir_all(dir.join("part"))?;

    for session in sessions {
        // Create project dir
        let project_dir = dir.join("session").join(&session.project_id);
        fs::create_dir_all(&project_dir)?;

        // Write session file
        let session_json = serde_json::json!({
            "id": session.id,
            "title": session.title,
            "directory": session.directory,
            "projectID": session.project_id,
            "time": {
                "created": session.created,
                "updated": session.updated
            }
        });
        fs::write(
            project_dir.join(format!("{}.json", session.id)),
            serde_json::to_string_pretty(&session_json)?,
        )?;

        // Create message directory for this session
        let msg_dir = dir.join("message").join(&session.id);
        fs::create_dir_all(&msg_dir)?;

        for msg in &session.messages {
            // Write message file
            let msg_json = serde_json::json!({
                "id": msg.id,
                "sessionID": session.id,
                "role": msg.role,
                "modelID": msg.model_id,
                "time": {
                    "created": msg.created
                }
            });
            fs::write(
                msg_dir.join(format!("{}.json", msg.id)),
                serde_json::to_string_pretty(&msg_json)?,
            )?;

            // Create part directory and write parts
            let part_dir = dir.join("part").join(&msg.id);
            fs::create_dir_all(&part_dir)?;

            for (i, part) in msg.parts.iter().enumerate() {
                let part_json = serde_json::json!({
                    "id": format!("part{}", i),
                    "messageID": msg.id,
                    "type": part.part_type,
                    "text": part.text,
                    "state": part.state.as_ref().map(|s| serde_json::json!({
                        "output": s
                    }))
                });
                fs::write(
                    part_dir.join(format!("part{}.json", i)),
                    serde_json::to_string_pretty(&part_json)?,
                )?;
            }
        }
    }

    Ok(())
}

struct TestSession {
    id: String,
    project_id: String,
    title: Option<String>,
    directory: Option<String>,
    created: Option<i64>,
    updated: Option<i64>,
    messages: Vec<TestMessage>,
}

struct TestMessage {
    id: String,
    role: String,
    model_id: Option<String>,
    created: Option<i64>,
    parts: Vec<TestPart>,
}

struct TestPart {
    part_type: String,
    text: Option<String>,
    state: Option<String>,
}

fn create_drizzle_opencode_db(path: &Path) -> Connection {
    let conn = Connection::open(path.to_string_lossy().as_ref()).expect("open opencode db");
    conn.execute_batch(
        "CREATE TABLE session (
            id TEXT PRIMARY KEY,
            project_id TEXT NOT NULL,
            slug TEXT NOT NULL,
            directory TEXT NOT NULL,
            title TEXT NOT NULL,
            version TEXT NOT NULL,
            time_created INTEGER NOT NULL,
            time_updated INTEGER NOT NULL
        );
        CREATE TABLE message (
            id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL,
            time_created INTEGER NOT NULL,
            time_updated INTEGER NOT NULL,
            data TEXT NOT NULL
        );
        CREATE TABLE part (
            id TEXT PRIMARY KEY,
            message_id TEXT NOT NULL,
            session_id TEXT NOT NULL,
            time_created INTEGER NOT NULL,
            time_updated INTEGER NOT NULL,
            data TEXT NOT NULL
        );",
    )
    .expect("create opencode drizzle tables");
    conn
}

#[test]
fn opencode_parses_json_fixture() {
    let fixture_root = PathBuf::from("tests/fixtures/opencode_json");
    let conn = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_dir: fixture_root.clone(),
        scan_roots: Vec::new(),
        since_ts: None,
        progress_tick: None,
    };
    let convs = conn.scan(&ctx).expect("scan");
    assert_eq!(convs.len(), 1);
    let c = &convs[0];
    assert_eq!(c.title.as_deref(), Some("OpenCode JSON Session"));
    assert_eq!(c.messages.len(), 2);
    assert_eq!(c.workspace, Some(PathBuf::from("/tmp/test-project")));
}

#[test]
fn opencode_parses_drizzle_sqlite_schema() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("opencode.db");
    let conn = create_drizzle_opencode_db(&db_path);

    conn.execute_compat(
        "INSERT INTO session (
            id, project_id, slug, directory, title, version, time_created, time_updated
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            "ses_drizzle_1",
            "proj_drizzle",
            "drizzle-session",
            "/work/opencode",
            "Generate data-lib AGENTS.md",
            "1.14.46",
            1_769_920_193_873_i64,
            1_769_920_194_000_i64
        ],
    )
    .expect("insert session");
    conn.execute_compat(
        "INSERT INTO message (id, session_id, time_created, time_updated, data)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            "msg_user_1",
            "ses_drizzle_1",
            1_769_920_193_900_i64,
            1_769_920_193_950_i64,
            r#"{"role":"user","path":{"cwd":"/work/opencode","root":"/work/opencode"}}"#
        ],
    )
    .expect("insert message");
    conn.execute_compat(
        "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            "part_text_1",
            "msg_user_1",
            "ses_drizzle_1",
            1_769_920_193_910_i64,
            1_769_920_193_910_i64,
            r#"{"id":"part_text_1","type":"text","text":"Explain the current OpenCode Drizzle schema"}"#
        ],
    )
    .expect("insert part");
    drop(conn);

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
        progress_tick: None,
    };
    let convs = connector
        .scan(&ctx)
        .expect("opencode drizzle sqlite scan should succeed");

    assert_eq!(convs.len(), 1);
    let conv = &convs[0];
    assert_eq!(conv.agent_slug, "opencode");
    assert_eq!(conv.external_id.as_deref(), Some("ses_drizzle_1"));
    assert_eq!(conv.title.as_deref(), Some("Generate data-lib AGENTS.md"));
    assert_eq!(conv.workspace, Some(PathBuf::from("/work/opencode")));
    assert_eq!(conv.messages.len(), 1);
    assert_eq!(conv.messages[0].role, "user");
    assert!(conv.messages[0].content.contains("OpenCode Drizzle schema"));
}

/// GH #504: OpenCode 2.0.x keeps new sessions only in `session_v2` /
/// `session_message` (next to the 1.x tables); a 1.x session migrated to 2.x
/// keeps its id in both. Both must come out of a real connector scan, once each.
#[test]
fn opencode_scan_indexes_v2_only_and_migrated_sessions_once() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("opencode.db");
    let conn = create_drizzle_opencode_db(&db_path);
    conn.execute_batch(
        "CREATE TABLE session_v2 (
            id TEXT PRIMARY KEY,
            project_id TEXT NOT NULL,
            parent_id TEXT,
            slug TEXT NOT NULL,
            directory TEXT NOT NULL,
            title TEXT,
            version TEXT NOT NULL,
            time_created INTEGER NOT NULL,
            time_updated INTEGER NOT NULL,
            time_archived INTEGER
        );
        CREATE TABLE session_message (
            id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL,
            type TEXT NOT NULL,
            seq INTEGER NOT NULL,
            time_created INTEGER NOT NULL,
            time_updated INTEGER NOT NULL,
            data TEXT NOT NULL
        );",
    )
    .expect("create opencode 2.x tables");

    // The migrated session: 1.x rows ...
    conn.execute_compat(
        "INSERT INTO session (id, project_id, slug, directory, title, version, time_created, time_updated)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            "ses_migrated",
            "proj_v2",
            "migrated",
            "/work/opencode",
            "1.x title",
            "1.14.46",
            1_769_920_193_873_i64,
            1_769_920_194_000_i64
        ],
    )
    .expect("insert 1.x session");
    conn.execute_compat(
        "INSERT INTO message (id, session_id, time_created, time_updated, data)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            "msg_v1",
            "ses_migrated",
            1_769_920_193_900_i64,
            1_769_920_193_900_i64,
            r#"{"role":"user"}"#
        ],
    )
    .expect("insert 1.x message");
    conn.execute_compat(
        "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            "part_v1",
            "msg_v1",
            "ses_migrated",
            1_769_920_193_910_i64,
            1_769_920_193_910_i64,
            r#"{"type":"text","text":"copy left behind in the 1.x tables"}"#
        ],
    )
    .expect("insert 1.x part");
    // ... and its 2.x copy, plus a session that exists only in 2.x.
    for (id, title) in [
        ("ses_migrated", "2.x title"),
        ("ses_v2only", "Started on 2.x"),
    ] {
        conn.execute_compat(
            "INSERT INTO session_v2 (id, project_id, slug, directory, title, version, time_created, time_updated)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                id,
                "proj_v2",
                id,
                "/work/opencode",
                title,
                "2.0.16",
                1_790_000_000_000_i64,
                1_790_000_010_000_i64
            ],
        )
        .expect("insert 2.x session");
    }
    let rows = [
        (
            "msg_m1",
            "ses_migrated",
            "user",
            1,
            r#"{"text":"migrated prompt in 2.x"}"#,
        ),
        (
            "msg_s0",
            "ses_v2only",
            "agent-switched",
            0,
            r#"{"agent":"build"}"#,
        ),
        (
            "msg_s1",
            "ses_v2only",
            "user",
            1,
            r#"{"text":"unmistakable v2 only phrase"}"#,
        ),
        (
            "msg_s2",
            "ses_v2only",
            "assistant",
            2,
            r#"{"model":{"id":"gpt-5","providerID":"openai"},"content":[{"type":"text","text":"answered on 2.x"}]}"#,
        ),
        (
            "msg_s3",
            "ses_v2only",
            "idle",
            3,
            r#"{"outcome":"succeeded"}"#,
        ),
    ];
    for (id, session_id, kind, seq, data) in rows {
        conn.execute_compat(
            "INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                id,
                session_id,
                kind,
                seq,
                1_790_000_000_000_i64 + seq,
                1_790_000_000_000_i64 + seq,
                data
            ],
        )
        .expect("insert 2.x message");
    }
    drop(conn);

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
        progress_tick: None,
    };
    let mut convs = connector
        .scan(&ctx)
        .expect("opencode 2.x scan should succeed");
    convs.sort_by(|a, b| a.external_id.cmp(&b.external_id));

    let ids: Vec<Option<&str>> = convs.iter().map(|c| c.external_id.as_deref()).collect();
    assert_eq!(ids, vec![Some("ses_migrated"), Some("ses_v2only")]);

    let migrated = &convs[0];
    assert_eq!(migrated.title.as_deref(), Some("2.x title"));
    let migrated_text: Vec<&str> = migrated
        .messages
        .iter()
        .map(|m| m.content.as_str())
        .collect();
    assert_eq!(migrated_text, vec!["migrated prompt in 2.x"]);

    let v2_only = &convs[1];
    assert_eq!(v2_only.title.as_deref(), Some("Started on 2.x"));
    assert_eq!(v2_only.workspace, Some(PathBuf::from("/work/opencode")));
    let turns: Vec<(&str, &str)> = v2_only
        .messages
        .iter()
        .map(|m| (m.role.as_str(), m.content.as_str()))
        .collect();
    assert_eq!(
        turns,
        vec![
            ("user", "unmistakable v2 only phrase"),
            ("assistant", "answered on 2.x")
        ]
    );
    assert_eq!(v2_only.messages[1].author.as_deref(), Some("gpt-5"));
}

#[test]
fn opencode_parses_created_storage() {
    let dir = TempDir::new().unwrap();
    create_test_storage(
        dir.path(),
        &[TestSession {
            id: "test-session-1".into(),
            project_id: "proj1".into(),
            title: Some("My Test Session".into()),
            directory: Some("/home/user/project".into()),
            created: Some(1000),
            updated: Some(5000),
            messages: vec![
                TestMessage {
                    id: "msg1".into(),
                    role: "user".into(),
                    model_id: None,
                    created: Some(1000),
                    parts: vec![TestPart {
                        part_type: "text".into(),
                        text: Some("Hello world".into()),
                        state: None,
                    }],
                },
                TestMessage {
                    id: "msg2".into(),
                    role: "assistant".into(),
                    model_id: Some("claude-3".into()),
                    created: Some(2000),
                    parts: vec![TestPart {
                        part_type: "text".into(),
                        text: Some("Hi there!".into()),
                        state: None,
                    }],
                },
            ],
        }],
    )
    .unwrap();

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
        progress_tick: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    assert_eq!(c.title, Some("My Test Session".to_string()));
    assert_eq!(c.workspace, Some(PathBuf::from("/home/user/project")));
    assert_eq!(c.messages.len(), 2);
    assert_eq!(c.messages[0].content, "Hello world");
    assert_eq!(c.messages[1].content, "Hi there!");
    assert_eq!(c.messages[1].author, Some("claude-3".to_string()));
}

#[test]
fn opencode_handles_multiple_sessions() {
    let dir = TempDir::new().unwrap();
    create_test_storage(
        dir.path(),
        &[
            TestSession {
                id: "session-a".into(),
                project_id: "proj1".into(),
                title: Some("Session A".into()),
                directory: None,
                created: Some(1000),
                updated: None,
                messages: vec![TestMessage {
                    id: "msg-a1".into(),
                    role: "user".into(),
                    model_id: None,
                    created: Some(1000),
                    parts: vec![TestPart {
                        part_type: "text".into(),
                        text: Some("Message A".into()),
                        state: None,
                    }],
                }],
            },
            TestSession {
                id: "session-b".into(),
                project_id: "proj2".into(),
                title: Some("Session B".into()),
                directory: None,
                created: Some(2000),
                updated: None,
                messages: vec![TestMessage {
                    id: "msg-b1".into(),
                    role: "user".into(),
                    model_id: None,
                    created: Some(2000),
                    parts: vec![TestPart {
                        part_type: "text".into(),
                        text: Some("Message B".into()),
                        state: None,
                    }],
                }],
            },
        ],
    )
    .unwrap();

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
        progress_tick: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 2);

    let titles: Vec<_> = convs.iter().filter_map(|c| c.title.as_deref()).collect();
    assert!(titles.contains(&"Session A"));
    assert!(titles.contains(&"Session B"));
}

#[test]
fn opencode_handles_tool_parts() {
    let dir = TempDir::new().unwrap();
    create_test_storage(
        dir.path(),
        &[TestSession {
            id: "tool-session".into(),
            project_id: "proj1".into(),
            title: Some("Tool Session".into()),
            directory: None,
            created: Some(1000),
            updated: None,
            messages: vec![TestMessage {
                id: "tool-msg".into(),
                role: "assistant".into(),
                model_id: None,
                created: Some(1000),
                parts: vec![
                    TestPart {
                        part_type: "text".into(),
                        text: Some("Let me check that file.".into()),
                        state: None,
                    },
                    TestPart {
                        part_type: "tool".into(),
                        text: None,
                        state: Some("file contents here".into()),
                    },
                ],
            }],
        }],
    )
    .unwrap();

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
        progress_tick: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let content = &convs[0].messages[0].content;
    assert!(content.contains("Let me check that file."));
    assert!(content.contains("[Tool Output]"));
    assert!(content.contains("file contents here"));
}

#[test]
fn opencode_handles_reasoning_parts() {
    let dir = TempDir::new().unwrap();
    create_test_storage(
        dir.path(),
        &[TestSession {
            id: "reasoning-session".into(),
            project_id: "proj1".into(),
            title: Some("Reasoning Session".into()),
            directory: None,
            created: Some(1000),
            updated: None,
            messages: vec![TestMessage {
                id: "reasoning-msg".into(),
                role: "assistant".into(),
                model_id: None,
                created: Some(1000),
                parts: vec![
                    TestPart {
                        part_type: "reasoning".into(),
                        text: Some("I need to think about this...".into()),
                        state: None,
                    },
                    TestPart {
                        part_type: "text".into(),
                        text: Some("The answer is 42.".into()),
                        state: None,
                    },
                ],
            }],
        }],
    )
    .unwrap();

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
        progress_tick: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let content = &convs[0].messages[0].content;
    assert!(content.contains("[Reasoning]"));
    assert!(content.contains("I need to think about this..."));
    assert!(content.contains("The answer is 42."));
}

#[test]
fn opencode_sets_correct_agent_slug() {
    let dir = TempDir::new().unwrap();
    create_test_storage(
        dir.path(),
        &[TestSession {
            id: "slug-test".into(),
            project_id: "proj1".into(),
            title: Some("Test".into()),
            directory: None,
            created: Some(1000),
            updated: None,
            messages: vec![TestMessage {
                id: "msg".into(),
                role: "user".into(),
                model_id: None,
                created: Some(1000),
                parts: vec![TestPart {
                    part_type: "text".into(),
                    text: Some("test".into()),
                    state: None,
                }],
            }],
        }],
    )
    .unwrap();

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
        progress_tick: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].agent_slug, "opencode");
}

#[test]
fn opencode_handles_empty_storage() {
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join("session")).unwrap();
    fs::create_dir_all(dir.path().join("message")).unwrap();
    fs::create_dir_all(dir.path().join("part")).unwrap();

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
        progress_tick: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert!(convs.is_empty());
}

#[test]
fn opencode_handles_missing_storage() {
    let dir = TempDir::new().unwrap();
    // Don't create any directories

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
        progress_tick: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert!(convs.is_empty());
}

#[test]
fn opencode_orders_messages_by_timestamp() {
    let dir = TempDir::new().unwrap();
    create_test_storage(
        dir.path(),
        &[TestSession {
            id: "order-session".into(),
            project_id: "proj1".into(),
            title: Some("Order Test".into()),
            directory: None,
            created: Some(1000),
            updated: None,
            messages: vec![
                TestMessage {
                    id: "msg-late".into(),
                    role: "user".into(),
                    model_id: None,
                    created: Some(3000),
                    parts: vec![TestPart {
                        part_type: "text".into(),
                        text: Some("third".into()),
                        state: None,
                    }],
                },
                TestMessage {
                    id: "msg-early".into(),
                    role: "user".into(),
                    model_id: None,
                    created: Some(1000),
                    parts: vec![TestPart {
                        part_type: "text".into(),
                        text: Some("first".into()),
                        state: None,
                    }],
                },
                TestMessage {
                    id: "msg-middle".into(),
                    role: "user".into(),
                    model_id: None,
                    created: Some(2000),
                    parts: vec![TestPart {
                        part_type: "text".into(),
                        text: Some("second".into()),
                        state: None,
                    }],
                },
            ],
        }],
    )
    .unwrap();

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
        progress_tick: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let msgs = &convs[0].messages;
    assert_eq!(msgs[0].content, "first");
    assert_eq!(msgs[1].content, "second");
    assert_eq!(msgs[2].content, "third");
}

#[test]
fn opencode_assigns_sequential_indices() {
    let dir = TempDir::new().unwrap();
    create_test_storage(
        dir.path(),
        &[TestSession {
            id: "idx-session".into(),
            project_id: "proj1".into(),
            title: Some("Index Test".into()),
            directory: None,
            created: Some(1000),
            updated: None,
            messages: vec![
                TestMessage {
                    id: "m0".into(),
                    role: "user".into(),
                    model_id: None,
                    created: Some(1000),
                    parts: vec![TestPart {
                        part_type: "text".into(),
                        text: Some("first".into()),
                        state: None,
                    }],
                },
                TestMessage {
                    id: "m1".into(),
                    role: "assistant".into(),
                    model_id: None,
                    created: Some(2000),
                    parts: vec![TestPart {
                        part_type: "text".into(),
                        text: Some("second".into()),
                        state: None,
                    }],
                },
                TestMessage {
                    id: "m2".into(),
                    role: "user".into(),
                    model_id: None,
                    created: Some(3000),
                    parts: vec![TestPart {
                        part_type: "text".into(),
                        text: Some("third".into()),
                        state: None,
                    }],
                },
            ],
        }],
    )
    .unwrap();

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
        progress_tick: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let msgs = &convs[0].messages;
    for (i, msg) in msgs.iter().enumerate() {
        assert_eq!(msg.idx, i as i64);
    }
}

#[test]
fn opencode_title_fallback_to_first_message() {
    let dir = TempDir::new().unwrap();
    create_test_storage(
        dir.path(),
        &[TestSession {
            id: "no-title".into(),
            project_id: "proj1".into(),
            title: None, // No title
            directory: None,
            created: Some(1000),
            updated: None,
            messages: vec![TestMessage {
                id: "msg".into(),
                role: "user".into(),
                model_id: None,
                created: Some(1000),
                parts: vec![TestPart {
                    part_type: "text".into(),
                    text: Some("This is the first message content".into()),
                    state: None,
                }],
            }],
        }],
    )
    .unwrap();

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
        progress_tick: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    // Title should fall back to first line of first message
    assert_eq!(
        convs[0].title,
        Some("This is the first message content".to_string())
    );
}

#[test]
fn opencode_computes_started_ended_at() {
    let dir = TempDir::new().unwrap();
    create_test_storage(
        dir.path(),
        &[TestSession {
            id: "time-session".into(),
            project_id: "proj1".into(),
            title: Some("Time Test".into()),
            directory: None,
            created: Some(500),  // Session created at 500
            updated: Some(4000), // Session updated at 4000
            messages: vec![
                TestMessage {
                    id: "m0".into(),
                    role: "user".into(),
                    model_id: None,
                    created: Some(1000),
                    parts: vec![TestPart {
                        part_type: "text".into(),
                        text: Some("first".into()),
                        state: None,
                    }],
                },
                TestMessage {
                    id: "m1".into(),
                    role: "assistant".into(),
                    model_id: None,
                    created: Some(3000),
                    parts: vec![TestPart {
                        part_type: "text".into(),
                        text: Some("last".into()),
                        state: None,
                    }],
                },
            ],
        }],
    )
    .unwrap();

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
        progress_tick: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    // started_at comes from session time.created
    assert_eq!(convs[0].started_at, Some(500));
    // ended_at comes from session time.updated
    assert_eq!(convs[0].ended_at, Some(4000));
}

#[test]
fn opencode_skips_sessions_without_messages() {
    let dir = TempDir::new().unwrap();

    // Create session dir but no messages
    let project_dir = dir.path().join("session").join("proj1");
    fs::create_dir_all(&project_dir).unwrap();

    let session_json = serde_json::json!({
        "id": "empty-session",
        "title": "Empty Session",
        "projectID": "proj1"
    });
    fs::write(
        project_dir.join("empty-session.json"),
        serde_json::to_string_pretty(&session_json).unwrap(),
    )
    .unwrap();

    // Create empty message directory
    fs::create_dir_all(dir.path().join("message").join("empty-session")).unwrap();
    fs::create_dir_all(dir.path().join("part")).unwrap();

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
        progress_tick: None,
    };
    let convs = connector.scan(&ctx).unwrap();

    // Should skip sessions without messages
    assert!(convs.is_empty());
}

#[test]
fn opencode_metadata_contains_session_id() {
    let dir = TempDir::new().unwrap();
    create_test_storage(
        dir.path(),
        &[TestSession {
            id: "meta-test".into(),
            project_id: "proj1".into(),
            title: Some("Meta Test".into()),
            directory: None,
            created: Some(1000),
            updated: None,
            messages: vec![TestMessage {
                id: "msg".into(),
                role: "user".into(),
                model_id: None,
                created: Some(1000),
                parts: vec![TestPart {
                    part_type: "text".into(),
                    text: Some("test".into()),
                    state: None,
                }],
            }],
        }],
    )
    .unwrap();

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
        progress_tick: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let metadata = &convs[0].metadata;
    assert_eq!(
        metadata.get("session_id").and_then(|v| v.as_str()),
        Some("meta-test")
    );
    assert_eq!(
        metadata.get("project_id").and_then(|v| v.as_str()),
        Some("proj1")
    );
}

#[test]
fn opencode_external_id_is_session_id() {
    let dir = TempDir::new().unwrap();
    create_test_storage(
        dir.path(),
        &[TestSession {
            id: "external-id-test".into(),
            project_id: "proj1".into(),
            title: Some("External ID Test".into()),
            directory: None,
            created: Some(1000),
            updated: None,
            messages: vec![TestMessage {
                id: "msg".into(),
                role: "user".into(),
                model_id: None,
                created: Some(1000),
                parts: vec![TestPart {
                    part_type: "text".into(),
                    text: Some("test".into()),
                    state: None,
                }],
            }],
        }],
    )
    .unwrap();

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
        progress_tick: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    assert_eq!(convs[0].external_id.as_deref(), Some("external-id-test"));
}

// =============================================================================
// Edge Case Tests (TST.CON)
// =============================================================================

#[test]
fn opencode_handles_corrupted_session_json() {
    let dir = TempDir::new().unwrap();

    // Create session dir with corrupted JSON
    let project_dir = dir.path().join("session").join("proj1");
    fs::create_dir_all(&project_dir).unwrap();
    fs::create_dir_all(dir.path().join("message")).unwrap();
    fs::create_dir_all(dir.path().join("part")).unwrap();

    // Write corrupted JSON (not valid JSON)
    fs::write(
        project_dir.join("corrupted-session.json"),
        "{ this is not valid json at all",
    )
    .unwrap();

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
        progress_tick: None,
    };
    // Should not panic, just skip the corrupted file
    let convs = connector.scan(&ctx).unwrap();
    assert!(convs.is_empty());
}

#[test]
fn opencode_handles_partial_session_data() {
    let dir = TempDir::new().unwrap();

    // Create session with minimal required fields only
    let project_dir = dir.path().join("session").join("proj1");
    fs::create_dir_all(&project_dir).unwrap();
    fs::create_dir_all(dir.path().join("message")).unwrap();
    fs::create_dir_all(dir.path().join("part")).unwrap();

    // Session JSON with only id and projectID (no title, no directory, no time)
    let session_json = serde_json::json!({
        "id": "minimal-session",
        "projectID": "proj1"
    });
    fs::write(
        project_dir.join("minimal-session.json"),
        serde_json::to_string_pretty(&session_json).unwrap(),
    )
    .unwrap();

    // Add a message
    let msg_dir = dir.path().join("message").join("minimal-session");
    fs::create_dir_all(&msg_dir).unwrap();
    let msg_json = serde_json::json!({
        "id": "msg1",
        "sessionID": "minimal-session",
        "role": "user"
    });
    fs::write(
        msg_dir.join("msg1.json"),
        serde_json::to_string_pretty(&msg_json).unwrap(),
    )
    .unwrap();

    // Add a part
    let part_dir = dir.path().join("part").join("msg1");
    fs::create_dir_all(&part_dir).unwrap();
    let part_json = serde_json::json!({
        "id": "part1",
        "messageID": "msg1",
        "type": "text",
        "text": "Hello from partial session"
    });
    fs::write(
        part_dir.join("part1.json"),
        serde_json::to_string_pretty(&part_json).unwrap(),
    )
    .unwrap();

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
        progress_tick: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    // Bead 7k7pl: assert the TITLE FALLBACK content, not just its
    // presence. The test name (`..._falls_back_to_first_message...`)
    // + the inline comment state a clear contract: title equals a
    // prefix of the first message content when no explicit title is
    // set. A regression that fell back to a placeholder like
    // `"(untitled)"` or empty-string would slip past `.is_some()`
    // but fires here.
    let title = c
        .title
        .as_deref()
        .expect("opencode must fall back to first-message content as title");
    assert!(
        title.contains("Hello from partial session"),
        "title should contain (a prefix of) the first message content; \
         got title={title:?}"
    );
    // Workspace should be None since directory wasn't provided
    assert!(c.workspace.is_none());
    assert_eq!(c.messages.len(), 1);
    assert!(c.messages[0].content.contains("Hello from partial session"));
}

#[test]
fn opencode_handles_unicode_content() {
    let dir = TempDir::new().unwrap();
    create_test_storage(
        dir.path(),
        &[TestSession {
            id: "unicode-session".into(),
            project_id: "proj1".into(),
            title: Some("Unicode Test 你好".into()),
            directory: Some("/home/用户/项目".into()),
            created: Some(1000),
            updated: None,
            messages: vec![
                TestMessage {
                    id: "msg1".into(),
                    role: "user".into(),
                    model_id: None,
                    created: Some(1000),
                    parts: vec![TestPart {
                        part_type: "text".into(),
                        text: Some("Hello 世界! 🚀 émojis and Ümlauts café".into()),
                        state: None,
                    }],
                },
                TestMessage {
                    id: "msg2".into(),
                    role: "assistant".into(),
                    model_id: Some("claude-3".into()),
                    created: Some(2000),
                    parts: vec![TestPart {
                        part_type: "text".into(),
                        text: Some("日本語 한국어 ภาษาไทย العربية".into()),
                        state: None,
                    }],
                },
            ],
        }],
    )
    .unwrap();

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
        progress_tick: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    // Title should preserve Unicode
    assert!(c.title.as_ref().unwrap().contains("你好"));
    // Workspace path should preserve Unicode
    assert!(
        c.workspace
            .as_ref()
            .unwrap()
            .to_str()
            .unwrap()
            .contains("用户")
    );
    // Messages should preserve Unicode
    assert!(c.messages[0].content.contains("世界"));
    assert!(c.messages[0].content.contains("🚀"));
    assert!(c.messages[0].content.contains("café"));
    assert!(c.messages[1].content.contains("日本語"));
    assert!(c.messages[1].content.contains("العربية"));
}

#[test]
fn opencode_handles_very_long_session() {
    let dir = TempDir::new().unwrap();

    // Create a session with many messages to test performance
    let mut messages = Vec::new();
    for i in 0..200 {
        messages.push(TestMessage {
            id: format!("msg{}", i),
            role: if i % 2 == 0 {
                "user".into()
            } else {
                "assistant".into()
            },
            model_id: if i % 2 == 1 {
                Some("claude-3".into())
            } else {
                None
            },
            created: Some(1000 + i as i64),
            parts: vec![TestPart {
                part_type: "text".into(),
                text: Some(format!(
                    "Message number {} with some content to make it realistic",
                    i
                )),
                state: None,
            }],
        });
    }

    create_test_storage(
        dir.path(),
        &[TestSession {
            id: "long-session".into(),
            project_id: "proj1".into(),
            title: Some("Long Session Test".into()),
            directory: None,
            created: Some(1000),
            updated: Some(2000),
            messages,
        }],
    )
    .unwrap();

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
        progress_tick: None,
    };

    let start = std::time::Instant::now();
    let convs = connector.scan(&ctx).unwrap();
    let elapsed = start.elapsed();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages.len(), 200);

    // Verify indices are sequential
    for (i, msg) in convs[0].messages.iter().enumerate() {
        assert_eq!(msg.idx, i as i64);
    }

    // Should complete in reasonable time (< 5 seconds)
    assert!(
        elapsed.as_secs() < 5,
        "Parsing 200 messages took too long: {:?}",
        elapsed
    );
}

#[test]
fn opencode_handles_empty_message_parts() {
    let dir = TempDir::new().unwrap();
    create_test_storage(
        dir.path(),
        &[TestSession {
            id: "empty-parts-session".into(),
            project_id: "proj1".into(),
            title: Some("Empty Parts Test".into()),
            directory: None,
            created: Some(1000),
            updated: None,
            messages: vec![
                TestMessage {
                    id: "valid-msg".into(),
                    role: "user".into(),
                    model_id: None,
                    created: Some(1000),
                    parts: vec![TestPart {
                        part_type: "text".into(),
                        text: Some("Valid message".into()),
                        state: None,
                    }],
                },
                TestMessage {
                    id: "empty-parts-msg".into(),
                    role: "assistant".into(),
                    model_id: None,
                    created: Some(2000),
                    parts: vec![], // No parts
                },
            ],
        }],
    )
    .unwrap();

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
        progress_tick: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    // Should have at least the valid message
    assert!(!convs[0].messages.is_empty());
    assert!(
        convs[0]
            .messages
            .iter()
            .any(|m| m.content.contains("Valid message"))
    );
}

#[test]
fn opencode_handles_null_text_parts() {
    let dir = TempDir::new().unwrap();
    create_test_storage(
        dir.path(),
        &[TestSession {
            id: "null-text-session".into(),
            project_id: "proj1".into(),
            title: Some("Null Text Test".into()),
            directory: None,
            created: Some(1000),
            updated: None,
            messages: vec![TestMessage {
                id: "null-text-msg".into(),
                role: "assistant".into(),
                model_id: None,
                created: Some(1000),
                parts: vec![
                    TestPart {
                        part_type: "text".into(),
                        text: None, // Null text
                        state: None,
                    },
                    TestPart {
                        part_type: "text".into(),
                        text: Some("Valid text".into()),
                        state: None,
                    },
                ],
            }],
        }],
    )
    .unwrap();

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
        progress_tick: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    // Should have the message with valid text
    assert!(!convs[0].messages.is_empty());
    assert!(convs[0].messages[0].content.contains("Valid text"));
}

#[test]
fn opencode_handles_deeply_nested_project_dirs() {
    let dir = TempDir::new().unwrap();

    // Create deeply nested project structure
    let deep_project_id = "very/deeply/nested/project";
    let project_dir = dir.path().join("session").join(deep_project_id);
    fs::create_dir_all(&project_dir).unwrap();
    fs::create_dir_all(dir.path().join("message")).unwrap();
    fs::create_dir_all(dir.path().join("part")).unwrap();

    let session_json = serde_json::json!({
        "id": "nested-session",
        "title": "Nested Project Session",
        "projectID": deep_project_id,
        "directory": "/home/user/nested/project"
    });
    fs::write(
        project_dir.join("nested-session.json"),
        serde_json::to_string_pretty(&session_json).unwrap(),
    )
    .unwrap();

    // Add message
    let msg_dir = dir.path().join("message").join("nested-session");
    fs::create_dir_all(&msg_dir).unwrap();
    let msg_json = serde_json::json!({
        "id": "msg1",
        "sessionID": "nested-session",
        "role": "user"
    });
    fs::write(
        msg_dir.join("msg1.json"),
        serde_json::to_string_pretty(&msg_json).unwrap(),
    )
    .unwrap();

    // Add part
    let part_dir = dir.path().join("part").join("msg1");
    fs::create_dir_all(&part_dir).unwrap();
    let part_json = serde_json::json!({
        "id": "part1",
        "messageID": "msg1",
        "type": "text",
        "text": "Content from nested project"
    });
    fs::write(
        part_dir.join("part1.json"),
        serde_json::to_string_pretty(&part_json).unwrap(),
    )
    .unwrap();

    let connector = OpenCodeConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().to_path_buf(),
        scan_roots: Vec::new(),
        since_ts: None,
        progress_tick: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    assert_eq!(c.title, Some("Nested Project Session".to_string()));
    assert!(
        c.messages[0]
            .content
            .contains("Content from nested project")
    );
}
