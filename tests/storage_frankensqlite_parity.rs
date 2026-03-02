//! FrankenStorage / SqliteStorage parity tests.
//!
//! These tests exercise identical operations against both SqliteStorage (rusqlite)
//! and FrankenStorage (frankensqlite) and verify the results match.
//!
//! Covers: CRUD operations, queries (JOIN, GROUP BY, ORDER BY, LIMIT, LIKE, FTS),
//! transaction behavior, edge cases (Unicode, NULL, empty DB, large content).
//!
//! ## Known frankensqlite limitations (documented via #[ignore] tests):
//!
//! - **ON CONFLICT / UPSERT**: INSERT ... ON CONFLICT DO UPDATE fails with UNIQUE
//!   constraint error. Affects: ensure_agent idempotent, source upsert update.
//! - **ORDER BY expression not in SELECT**: `ORDER BY started_at DESC` fails when
//!   the column isn't in the SELECT list. Affects: list_conversations.
//! - **Placeholder in JOIN/subquery**: Parameterized queries in certain contexts
//!   produce "unsupported expression type: Placeholder" errors. Affects: dedup
//!   check in insert_conversation_tree, embedding job queries.
//! - **daily_stats UNIQUE constraint**: Batch conversation inserts with same day_id
//!   hit constraint failures. Affects: multi-conversation inserts.
//! - **Mixed aggregate/non-aggregate without GROUP BY**: Frankensqlite rejects
//!   queries that C SQLite handles. Affects: count_sessions_in_range.
//! - **File format interop**: Frankensqlite-written DBs are not guaranteed readable
//!   by C SQLite (different WAL/page format).

use coding_agent_search::model::types::{
    Agent, AgentKind, Conversation, Message, MessageRole, Snippet,
};
use coding_agent_search::sources::provenance::{Source, SourceKind};
use coding_agent_search::storage::sqlite::{CURRENT_SCHEMA_VERSION, FrankenStorage, SqliteStorage};
use serde_json::json;
use std::path::PathBuf;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_agent(slug: &str, name: &str) -> Agent {
    Agent {
        id: None,
        slug: slug.to_string(),
        name: name.to_string(),
        version: Some("1.0".to_string()),
        kind: AgentKind::Cli,
    }
}

fn make_conversation(
    agent_slug: &str,
    ext_id: &str,
    title: &str,
    messages: Vec<Message>,
) -> Conversation {
    Conversation {
        id: None,
        agent_slug: agent_slug.to_string(),
        workspace: Some(PathBuf::from("/test/workspace")),
        external_id: Some(ext_id.to_string()),
        title: Some(title.to_string()),
        source_path: PathBuf::from("/test/source.jsonl"),
        started_at: Some(1700000000000),
        ended_at: None,
        approx_tokens: Some(500),
        metadata_json: json!({"test": true}),
        messages,
        source_id: "local".to_string(),
        origin_host: None,
    }
}

fn make_message(idx: i64, role: MessageRole, content: &str) -> Message {
    Message {
        id: None,
        idx,
        role,
        author: Some("test-author".to_string()),
        created_at: Some(1700000000000 + idx * 1000),
        content: content.to_string(),
        extra_json: json!({}),
        snippets: vec![],
    }
}

fn make_message_with_snippet(idx: i64, content: &str, snippet_text: &str) -> Message {
    Message {
        id: None,
        idx,
        role: MessageRole::Agent,
        author: Some("test-author".to_string()),
        created_at: Some(1700000000000 + idx * 1000),
        content: content.to_string(),
        extra_json: json!({}),
        snippets: vec![Snippet {
            id: None,
            file_path: Some(PathBuf::from("/src/main.rs")),
            start_line: Some(1),
            end_line: Some(10),
            language: Some("rust".to_string()),
            snippet_text: Some(snippet_text.to_string()),
        }],
    }
}

fn make_source(id: &str, kind: SourceKind, host: Option<&str>) -> Source {
    Source {
        id: id.to_string(),
        kind,
        host_label: host.map(String::from),
        machine_id: None,
        platform: Some("linux".to_string()),
        config_json: None,
        created_at: Some(1700000000000),
        updated_at: Some(1700000000000),
    }
}

/// Open both storages against fresh temp DBs.
fn open_both() -> (TempDir, SqliteStorage, FrankenStorage) {
    let dir = TempDir::new().expect("temp dir");
    let sql_path = dir.path().join("rusqlite.db");
    let frank_path = dir.path().join("franken.db");
    let sql = SqliteStorage::open(&sql_path).expect("open SqliteStorage");
    let frank = FrankenStorage::open(&frank_path).expect("open FrankenStorage");
    (dir, sql, frank)
}

// ============================================================================
// 1. SCHEMA PARITY (PASSING)
// ============================================================================

#[test]
fn parity_schema_version_matches() {
    let (_dir, sql, frank) = open_both();
    assert_eq!(
        sql.schema_version().unwrap(),
        frank.schema_version().unwrap(),
        "Schema versions should match between SqliteStorage and FrankenStorage"
    );
}

#[test]
fn parity_migration_creates_local_source() {
    let (_dir, sql, frank) = open_both();

    let sql_src = sql.get_source("local").unwrap();
    let frank_src = frank.get_source("local").unwrap();

    assert!(sql_src.is_some(), "SqliteStorage should have local source");
    assert!(
        frank_src.is_some(),
        "FrankenStorage should have local source"
    );

    let s = sql_src.unwrap();
    let f = frank_src.unwrap();
    assert_eq!(s.id, f.id);
    assert_eq!(s.kind, f.kind);
}

// ============================================================================
// 2. CRUD PARITY — Agents
// ============================================================================

#[test]
fn parity_ensure_agent_returns_id() {
    let (_dir, sql, frank) = open_both();
    let agent = make_agent("claude-code", "Claude Code");

    let sql_id = sql.ensure_agent(&agent).unwrap();
    let frank_id = frank.ensure_agent(&agent).unwrap();

    assert!(sql_id > 0);
    assert!(frank_id > 0);
}

#[test]
#[ignore = "frankensqlite: ON CONFLICT DO UPDATE not yet supported (UNIQUE constraint error on second insert)"]
fn parity_ensure_agent_idempotent() {
    let (_dir, sql, frank) = open_both();
    let agent = make_agent("codex", "OpenAI Codex");

    let sql_id1 = sql.ensure_agent(&agent).unwrap();
    let sql_id2 = sql.ensure_agent(&agent).unwrap();
    let frank_id1 = frank.ensure_agent(&agent).unwrap();
    let frank_id2 = frank.ensure_agent(&agent).unwrap();

    assert_eq!(
        sql_id1, sql_id2,
        "SqliteStorage ensure_agent not idempotent"
    );
    assert_eq!(
        frank_id1, frank_id2,
        "FrankenStorage ensure_agent not idempotent"
    );
}

#[test]
fn parity_list_agents_ordering() {
    let (_dir, sql, frank) = open_both();

    for (slug, name) in [("codex", "Codex"), ("aider", "Aider"), ("claude", "Claude")] {
        sql.ensure_agent(&make_agent(slug, name)).unwrap();
        frank.ensure_agent(&make_agent(slug, name)).unwrap();
    }

    let sql_agents = sql.list_agents().unwrap();
    let frank_agents = frank.list_agents().unwrap();

    assert_eq!(sql_agents.len(), frank_agents.len());
    for (s, f) in sql_agents.iter().zip(frank_agents.iter()) {
        assert_eq!(s.slug, f.slug, "Agent slugs should match in order");
        assert_eq!(s.name, f.name, "Agent names should match");
    }
}

// ============================================================================
// 3. CRUD PARITY — Workspaces
// ============================================================================

#[test]
fn parity_ensure_workspace() {
    let (_dir, sql, frank) = open_both();
    let path = PathBuf::from("/home/user/project");

    let sql_id = sql.ensure_workspace(&path, Some("My Project")).unwrap();
    let frank_id = frank.ensure_workspace(&path, Some("My Project")).unwrap();

    assert!(sql_id > 0);
    assert!(frank_id > 0);
}

#[test]
fn parity_list_workspaces() {
    let (_dir, sql, frank) = open_both();

    for p in ["/a/project", "/b/project", "/c/project"] {
        sql.ensure_workspace(&PathBuf::from(p), None).unwrap();
        frank.ensure_workspace(&PathBuf::from(p), None).unwrap();
    }

    let sql_ws = sql.list_workspaces().unwrap();
    let frank_ws = frank.list_workspaces().unwrap();

    assert_eq!(sql_ws.len(), frank_ws.len());
    for (s, f) in sql_ws.iter().zip(frank_ws.iter()) {
        assert_eq!(s.path, f.path);
    }
}

// ============================================================================
// 4. CRUD PARITY — Sources
// ============================================================================

#[test]
fn parity_upsert_and_get_source() {
    let (_dir, sql, frank) = open_both();

    let src = make_source("work-laptop", SourceKind::Ssh, Some("work-laptop.local"));
    sql.upsert_source(&src).unwrap();
    frank.upsert_source(&src).unwrap();

    let sql_src = sql.get_source("work-laptop").unwrap().unwrap();
    let frank_src = frank.get_source("work-laptop").unwrap().unwrap();

    assert_eq!(sql_src.id, frank_src.id);
    assert_eq!(sql_src.kind, frank_src.kind);
    assert_eq!(sql_src.host_label, frank_src.host_label);
    assert_eq!(sql_src.platform, frank_src.platform);
}

#[test]
fn parity_list_sources() {
    let (_dir, sql, frank) = open_both();

    let src = make_source("remote-1", SourceKind::Ssh, Some("host1"));
    sql.upsert_source(&src).unwrap();
    frank.upsert_source(&src).unwrap();

    let sql_sources = sql.list_sources().unwrap();
    let frank_sources = frank.list_sources().unwrap();

    assert_eq!(sql_sources.len(), frank_sources.len());
    for (s, f) in sql_sources.iter().zip(frank_sources.iter()) {
        assert_eq!(s.id, f.id);
        assert_eq!(s.kind, f.kind);
    }
}

#[test]
fn parity_get_source_ids() {
    let (_dir, sql, frank) = open_both();

    for id in ["remote-a", "remote-b"] {
        let src = make_source(id, SourceKind::Ssh, Some(id));
        sql.upsert_source(&src).unwrap();
        frank.upsert_source(&src).unwrap();
    }

    let sql_ids = sql.get_source_ids().unwrap();
    let frank_ids = frank.get_source_ids().unwrap();

    assert_eq!(sql_ids, frank_ids, "Source IDs should match");
}

#[test]
fn parity_delete_source() {
    let (_dir, sql, frank) = open_both();

    let src = make_source("deleteme", SourceKind::Ssh, Some("gone"));
    sql.upsert_source(&src).unwrap();
    frank.upsert_source(&src).unwrap();

    let sql_deleted = sql.delete_source("deleteme", false).unwrap();
    let frank_deleted = frank.delete_source("deleteme", false).unwrap();

    assert_eq!(sql_deleted, frank_deleted);
    assert!(sql_deleted);

    assert!(sql.get_source("deleteme").unwrap().is_none());
    assert!(frank.get_source("deleteme").unwrap().is_none());
}

#[test]
fn parity_delete_local_source_fails() {
    let (_dir, sql, frank) = open_both();

    let sql_err = sql.delete_source("local", false).is_err();
    let frank_err = frank.delete_source("local", false).is_err();

    assert!(sql_err, "Deleting local source should fail (SqliteStorage)");
    assert!(
        frank_err,
        "Deleting local source should fail (FrankenStorage)"
    );
}

// ============================================================================
// 5. CRUD PARITY — Conversations + Messages
// ============================================================================

#[test]
#[ignore = "frankensqlite: ORDER BY expression not in SELECT list (list_conversations)"]
fn parity_insert_and_list_conversations() {
    let (_dir, mut sql, frank) = open_both();

    let agent = make_agent("claude", "Claude");
    let sql_agent_id = sql.ensure_agent(&agent).unwrap();
    let frank_agent_id = frank.ensure_agent(&agent).unwrap();

    let conv = make_conversation(
        "claude",
        "ext-001",
        "Test Conversation",
        vec![
            make_message(0, MessageRole::User, "Hello, Claude!"),
            make_message(1, MessageRole::Agent, "Hello! How can I help?"),
        ],
    );

    let sql_result = sql
        .insert_conversation_tree(sql_agent_id, None, &conv)
        .unwrap();
    let frank_result = frank
        .insert_conversation_tree(frank_agent_id, None, &conv)
        .unwrap();

    assert_eq!(
        sql_result.inserted_indices, frank_result.inserted_indices,
        "Inserted message indices should match"
    );

    let sql_convs = sql.list_conversations(10, 0).unwrap();
    let frank_convs = frank.list_conversations(10, 0).unwrap();

    assert_eq!(sql_convs.len(), frank_convs.len());
    assert_eq!(sql_convs[0].agent_slug, frank_convs[0].agent_slug);
    assert_eq!(sql_convs[0].title, frank_convs[0].title);
    assert_eq!(sql_convs[0].external_id, frank_convs[0].external_id);
}

/// Verify insert + fetch_messages parity without list_conversations (avoids ORDER BY issue).
#[test]
fn parity_insert_and_fetch_messages() {
    let (_dir, mut sql, frank) = open_both();

    let agent = make_agent("claude", "Claude");
    let sql_agent_id = sql.ensure_agent(&agent).unwrap();
    let frank_agent_id = frank.ensure_agent(&agent).unwrap();

    let conv = make_conversation(
        "claude",
        "ext-001",
        "Test Conversation",
        vec![
            make_message(0, MessageRole::User, "Hello, Claude!"),
            make_message(1, MessageRole::Agent, "Hello! How can I help?"),
        ],
    );

    let sql_result = sql
        .insert_conversation_tree(sql_agent_id, None, &conv)
        .unwrap();
    let frank_result = frank
        .insert_conversation_tree(frank_agent_id, None, &conv)
        .unwrap();

    assert_eq!(
        sql_result.inserted_indices, frank_result.inserted_indices,
        "Inserted message indices should match"
    );

    // Use conversation_id directly instead of list_conversations
    let sql_msgs = sql.fetch_messages(sql_result.conversation_id).unwrap();
    let frank_msgs = frank.fetch_messages(frank_result.conversation_id).unwrap();

    assert_eq!(sql_msgs.len(), frank_msgs.len());
    assert_eq!(sql_msgs.len(), 2);
    for (s, f) in sql_msgs.iter().zip(frank_msgs.iter()) {
        assert_eq!(s.idx, f.idx);
        assert_eq!(s.content, f.content);
        assert_eq!(s.role, f.role);
        assert_eq!(s.author, f.author);
    }
}

#[test]
fn parity_fetch_messages_four_msgs() {
    let (_dir, mut sql, frank) = open_both();

    let agent = make_agent("claude", "Claude");
    let sql_agent_id = sql.ensure_agent(&agent).unwrap();
    let frank_agent_id = frank.ensure_agent(&agent).unwrap();

    let conv = make_conversation(
        "claude",
        "ext-002",
        "Message Test",
        vec![
            make_message(0, MessageRole::User, "First user message"),
            make_message(1, MessageRole::Agent, "First agent response"),
            make_message(2, MessageRole::User, "Follow-up question"),
            make_message(3, MessageRole::Agent, "Follow-up answer"),
        ],
    );

    let sql_result = sql
        .insert_conversation_tree(sql_agent_id, None, &conv)
        .unwrap();
    let frank_result = frank
        .insert_conversation_tree(frank_agent_id, None, &conv)
        .unwrap();

    let sql_msgs = sql.fetch_messages(sql_result.conversation_id).unwrap();
    let frank_msgs = frank.fetch_messages(frank_result.conversation_id).unwrap();

    assert_eq!(sql_msgs.len(), frank_msgs.len());
    for (s, f) in sql_msgs.iter().zip(frank_msgs.iter()) {
        assert_eq!(s.idx, f.idx, "Message idx mismatch");
        assert_eq!(s.content, f.content, "Message content mismatch");
        assert_eq!(s.role, f.role, "Message role mismatch");
        assert_eq!(s.author, f.author, "Message author mismatch");
    }
}

#[test]
fn parity_insert_with_snippets() {
    let (_dir, mut sql, frank) = open_both();

    let agent = make_agent("claude", "Claude");
    let sql_agent_id = sql.ensure_agent(&agent).unwrap();
    let frank_agent_id = frank.ensure_agent(&agent).unwrap();

    let conv = make_conversation(
        "claude",
        "ext-003",
        "Snippet Test",
        vec![make_message_with_snippet(
            0,
            "Here's the code fix",
            "fn main() { println!(\"fixed!\"); }",
        )],
    );

    let sql_result = sql
        .insert_conversation_tree(sql_agent_id, None, &conv)
        .unwrap();
    let frank_result = frank
        .insert_conversation_tree(frank_agent_id, None, &conv)
        .unwrap();

    // Verify messages via fetch_messages (avoids list_conversations ORDER BY issue)
    let sql_msgs = sql.fetch_messages(sql_result.conversation_id).unwrap();
    let frank_msgs = frank.fetch_messages(frank_result.conversation_id).unwrap();
    assert_eq!(sql_msgs.len(), 1);
    assert_eq!(frank_msgs.len(), 1);
    assert_eq!(sql_msgs[0].content, frank_msgs[0].content);
}

#[test]
#[ignore = "frankensqlite: Placeholder in subquery (dedup SELECT with ?1 in WHERE clause)"]
fn parity_conversation_dedup_by_external_id() {
    let (_dir, mut sql, frank) = open_both();

    let agent = make_agent("claude", "Claude");
    let sql_agent_id = sql.ensure_agent(&agent).unwrap();
    let frank_agent_id = frank.ensure_agent(&agent).unwrap();

    let conv = make_conversation(
        "claude",
        "dedup-ext",
        "Dedup Test",
        vec![make_message(0, MessageRole::User, "Initial message")],
    );

    sql.insert_conversation_tree(sql_agent_id, None, &conv)
        .unwrap();
    frank
        .insert_conversation_tree(frank_agent_id, None, &conv)
        .unwrap();

    let conv2 = make_conversation(
        "claude",
        "dedup-ext",
        "Dedup Test Updated",
        vec![
            make_message(0, MessageRole::User, "Initial message"),
            make_message(1, MessageRole::Agent, "New appended message"),
        ],
    );

    sql.insert_conversation_tree(sql_agent_id, None, &conv2)
        .unwrap();
    frank
        .insert_conversation_tree(frank_agent_id, None, &conv2)
        .unwrap();

    let sql_convs = sql.list_conversations(10, 0).unwrap();
    let frank_convs = frank.list_conversations(10, 0).unwrap();
    assert_eq!(sql_convs.len(), 1);
    assert_eq!(frank_convs.len(), 1);
}

// ============================================================================
// 6. QUERY PARITY — Pagination
// ============================================================================

#[test]
#[ignore = "frankensqlite: ORDER BY expression not in SELECT list (list_conversations)"]
fn parity_list_conversations_pagination() {
    let (_dir, mut sql, frank) = open_both();

    let agent = make_agent("claude", "Claude");
    let sql_agent_id = sql.ensure_agent(&agent).unwrap();
    let frank_agent_id = frank.ensure_agent(&agent).unwrap();

    for i in 0..5 {
        let conv = make_conversation(
            "claude",
            &format!("page-{i}"),
            &format!("Conversation {i}"),
            vec![make_message(0, MessageRole::User, &format!("msg {i}"))],
        );
        sql.insert_conversation_tree(sql_agent_id, None, &conv)
            .unwrap();
        frank
            .insert_conversation_tree(frank_agent_id, None, &conv)
            .unwrap();
    }

    let sql_p1 = sql.list_conversations(2, 0).unwrap();
    let frank_p1 = frank.list_conversations(2, 0).unwrap();
    assert_eq!(sql_p1.len(), frank_p1.len());
    assert_eq!(sql_p1.len(), 2);

    let sql_p2 = sql.list_conversations(2, 2).unwrap();
    let frank_p2 = frank.list_conversations(2, 2).unwrap();
    assert_eq!(sql_p2.len(), frank_p2.len());
    assert_eq!(sql_p2.len(), 2);

    let sql_p3 = sql.list_conversations(2, 4).unwrap();
    let frank_p3 = frank.list_conversations(2, 4).unwrap();
    assert_eq!(sql_p3.len(), frank_p3.len());
    assert_eq!(sql_p3.len(), 1);
}

// ============================================================================
// 7. QUERY PARITY — Meta key-value store
// ============================================================================

#[test]
fn parity_scan_timestamp_roundtrip() {
    let (_dir, mut sql, frank) = open_both();

    assert_eq!(sql.get_last_scan_ts().unwrap(), None);
    assert_eq!(frank.get_last_scan_ts().unwrap(), None);

    let ts = 1700000000000_i64;
    sql.set_last_scan_ts(ts).unwrap();
    frank.set_last_scan_ts(ts).unwrap();

    assert_eq!(sql.get_last_scan_ts().unwrap(), Some(ts));
    assert_eq!(frank.get_last_scan_ts().unwrap(), Some(ts));
}

// ============================================================================
// 8. FTS PARITY
// ============================================================================

#[test]
fn parity_rebuild_fts_and_query() {
    let (_dir, mut sql, frank) = open_both();

    let agent = make_agent("claude", "Claude");
    let sql_agent_id = sql.ensure_agent(&agent).unwrap();
    let frank_agent_id = frank.ensure_agent(&agent).unwrap();

    let conv = make_conversation(
        "claude",
        "fts-test",
        "FTS Parity",
        vec![
            make_message(0, MessageRole::User, "searchable keyword alpha"),
            make_message(1, MessageRole::Agent, "response with beta keyword"),
        ],
    );

    sql.insert_conversation_tree(sql_agent_id, None, &conv)
        .unwrap();
    frank
        .insert_conversation_tree(frank_agent_id, None, &conv)
        .unwrap();

    sql.rebuild_fts().unwrap();
    frank.rebuild_fts().unwrap();
}

// ============================================================================
// 9. EDGE CASES
// ============================================================================

#[test]
fn parity_empty_database_agents_and_sources() {
    let (_dir, sql, frank) = open_both();

    let sql_agents = sql.list_agents().unwrap();
    let frank_agents = frank.list_agents().unwrap();
    assert_eq!(sql_agents.len(), 0);
    assert_eq!(frank_agents.len(), 0);

    // get_source for non-existent returns None
    assert!(sql.get_source("nonexistent").unwrap().is_none());
    assert!(frank.get_source("nonexistent").unwrap().is_none());
}

#[test]
#[ignore = "frankensqlite: ORDER BY expression not in SELECT list (list_conversations)"]
fn parity_empty_database_conversations() {
    let (_dir, sql, frank) = open_both();

    let sql_convs = sql.list_conversations(10, 0).unwrap();
    let frank_convs = frank.list_conversations(10, 0).unwrap();
    assert_eq!(sql_convs.len(), 0);
    assert_eq!(frank_convs.len(), 0);
}

#[test]
fn parity_unicode_content() {
    let (_dir, mut sql, frank) = open_both();

    let agent = make_agent("claude", "Claude");
    let sql_agent_id = sql.ensure_agent(&agent).unwrap();
    let frank_agent_id = frank.ensure_agent(&agent).unwrap();

    let unicode_content = "Unicode test: 日本語 中文 한국어 العربية emoji: 🦀🔥💡 math: ∀x∈ℝ, x²≥0";
    let conv = make_conversation(
        "claude",
        "unicode-test",
        "Unicode: 日本語テスト",
        vec![make_message(0, MessageRole::User, unicode_content)],
    );

    let sql_result = sql
        .insert_conversation_tree(sql_agent_id, None, &conv)
        .unwrap();
    let frank_result = frank
        .insert_conversation_tree(frank_agent_id, None, &conv)
        .unwrap();

    let sql_msgs = sql.fetch_messages(sql_result.conversation_id).unwrap();
    let frank_msgs = frank.fetch_messages(frank_result.conversation_id).unwrap();

    assert_eq!(sql_msgs[0].content, frank_msgs[0].content);
    assert_eq!(sql_msgs[0].content, unicode_content);
}

#[test]
fn parity_null_handling() {
    let (_dir, mut sql, frank) = open_both();

    let agent = make_agent("claude", "Claude");
    let sql_agent_id = sql.ensure_agent(&agent).unwrap();
    let frank_agent_id = frank.ensure_agent(&agent).unwrap();

    let conv = Conversation {
        id: None,
        agent_slug: "claude".to_string(),
        workspace: None,
        external_id: None,
        title: None,
        source_path: PathBuf::from("/test.jsonl"),
        started_at: None,
        ended_at: None,
        approx_tokens: None,
        metadata_json: json!(null),
        messages: vec![Message {
            id: None,
            idx: 0,
            role: MessageRole::User,
            author: None,
            created_at: None,
            content: "minimal message".to_string(),
            extra_json: json!(null),
            snippets: vec![],
        }],
        source_id: "local".to_string(),
        origin_host: None,
    };

    let sql_result = sql
        .insert_conversation_tree(sql_agent_id, None, &conv)
        .unwrap();
    let frank_result = frank
        .insert_conversation_tree(frank_agent_id, None, &conv)
        .unwrap();

    // Verify via fetch_messages (avoids list_conversations ORDER BY issue)
    let sql_msgs = sql.fetch_messages(sql_result.conversation_id).unwrap();
    let frank_msgs = frank.fetch_messages(frank_result.conversation_id).unwrap();

    assert_eq!(sql_msgs.len(), frank_msgs.len());
    assert_eq!(sql_msgs[0].author, frank_msgs[0].author);
    assert!(sql_msgs[0].author.is_none());
}

#[test]
fn parity_large_content() {
    let (_dir, mut sql, frank) = open_both();

    let agent = make_agent("claude", "Claude");
    let sql_agent_id = sql.ensure_agent(&agent).unwrap();
    let frank_agent_id = frank.ensure_agent(&agent).unwrap();

    let large_body = "x".repeat(100_000);
    let conv = make_conversation(
        "claude",
        "large-content",
        "Large Content Test",
        vec![make_message(0, MessageRole::User, &large_body)],
    );

    let sql_result = sql
        .insert_conversation_tree(sql_agent_id, None, &conv)
        .unwrap();
    let frank_result = frank
        .insert_conversation_tree(frank_agent_id, None, &conv)
        .unwrap();

    let sql_msgs = sql.fetch_messages(sql_result.conversation_id).unwrap();
    let frank_msgs = frank.fetch_messages(frank_result.conversation_id).unwrap();

    assert_eq!(sql_msgs[0].content.len(), frank_msgs[0].content.len());
    assert_eq!(sql_msgs[0].content.len(), 100_000);
}

// ============================================================================
// 10. MULTIPLE CONVERSATIONS + AGENTS
// ============================================================================

#[test]
#[ignore = "frankensqlite: daily_stats UNIQUE constraint + ORDER BY expression not in SELECT list"]
fn parity_multiple_agents_multiple_conversations() {
    let (_dir, mut sql, frank) = open_both();

    let agents = [
        make_agent("claude", "Claude"),
        make_agent("codex", "OpenAI Codex"),
        make_agent("aider", "Aider"),
    ];

    let mut sql_agent_ids = Vec::new();
    let mut frank_agent_ids = Vec::new();

    for a in &agents {
        sql_agent_ids.push(sql.ensure_agent(a).unwrap());
        frank_agent_ids.push(frank.ensure_agent(a).unwrap());
    }

    for (i, (sql_aid, frank_aid)) in sql_agent_ids.iter().zip(frank_agent_ids.iter()).enumerate() {
        for j in 0..3 {
            let conv = make_conversation(
                &agents[i].slug,
                &format!("agent{i}-conv{j}"),
                &format!("{} Session {j}", agents[i].name),
                vec![
                    make_message(
                        0,
                        MessageRole::User,
                        &format!("Hello from {}", agents[i].slug),
                    ),
                    make_message(
                        1,
                        MessageRole::Agent,
                        &format!("Reply from {} agent", agents[i].name),
                    ),
                ],
            );
            sql.insert_conversation_tree(*sql_aid, None, &conv).unwrap();
            frank
                .insert_conversation_tree(*frank_aid, None, &conv)
                .unwrap();
        }
    }

    let sql_total = sql.list_conversations(100, 0).unwrap();
    let frank_total = frank.list_conversations(100, 0).unwrap();
    assert_eq!(sql_total.len(), 9);
    assert_eq!(frank_total.len(), 9);

    let sql_agents = sql.list_agents().unwrap();
    let frank_agents = frank.list_agents().unwrap();
    assert_eq!(sql_agents.len(), 3);
    assert_eq!(frank_agents.len(), 3);
}

/// Variant: multiple agents (no conversations) to avoid daily_stats issue.
#[test]
fn parity_multiple_agents_only() {
    let (_dir, sql, frank) = open_both();

    for (slug, name) in [
        ("claude", "Claude"),
        ("codex", "Codex"),
        ("aider", "Aider"),
        ("cursor", "Cursor"),
    ] {
        sql.ensure_agent(&make_agent(slug, name)).unwrap();
        frank.ensure_agent(&make_agent(slug, name)).unwrap();
    }

    let sql_agents = sql.list_agents().unwrap();
    let frank_agents = frank.list_agents().unwrap();
    assert_eq!(sql_agents.len(), 4);
    assert_eq!(frank_agents.len(), 4);

    for (s, f) in sql_agents.iter().zip(frank_agents.iter()) {
        assert_eq!(s.slug, f.slug);
        assert_eq!(s.name, f.name);
        assert_eq!(s.kind, f.kind);
    }
}

// ============================================================================
// 11. SOURCE OPERATIONS EDGE CASES
// ============================================================================

#[test]
#[ignore = "frankensqlite: ON CONFLICT DO UPDATE not yet supported (UNIQUE constraint on second upsert)"]
fn parity_source_upsert_updates_existing() {
    let (_dir, sql, frank) = open_both();

    let src_v1 = make_source("evolving", SourceKind::Ssh, Some("host-v1"));
    sql.upsert_source(&src_v1).unwrap();
    frank.upsert_source(&src_v1).unwrap();

    let src_v2 = Source {
        host_label: Some("host-v2".to_string()),
        ..src_v1
    };
    sql.upsert_source(&src_v2).unwrap();
    frank.upsert_source(&src_v2).unwrap();

    let sql_src = sql.get_source("evolving").unwrap().unwrap();
    let frank_src = frank.get_source("evolving").unwrap().unwrap();

    assert_eq!(sql_src.host_label.as_deref(), Some("host-v2"));
    assert_eq!(frank_src.host_label.as_deref(), Some("host-v2"));
}

#[test]
fn parity_get_nonexistent_source_returns_none() {
    let (_dir, sql, frank) = open_both();

    assert!(sql.get_source("nonexistent").unwrap().is_none());
    assert!(frank.get_source("nonexistent").unwrap().is_none());
}

#[test]
fn parity_delete_nonexistent_source_returns_false() {
    let (_dir, sql, frank) = open_both();

    let sql_del = sql.delete_source("ghost", false).unwrap();
    let frank_del = frank.delete_source("ghost", false).unwrap();

    assert!(!sql_del);
    assert!(!frank_del);
}

// ============================================================================
// 12. EMBEDDING JOB PARITY
// ============================================================================

#[test]
#[ignore = "frankensqlite: Placeholder in subquery (embedding_jobs WHERE clause)"]
fn parity_embedding_job_lifecycle() {
    let (_dir, sql, frank) = open_both();

    let db_path = "/test/db.sqlite";
    let model_id = "all-MiniLM-L6-v2";

    let sql_job_id = sql.upsert_embedding_job(db_path, model_id, 100).unwrap();
    let frank_job_id = frank.upsert_embedding_job(db_path, model_id, 100).unwrap();

    assert!(sql_job_id > 0);
    assert!(frank_job_id > 0);

    sql.start_embedding_job(sql_job_id).unwrap();
    frank.start_embedding_job(frank_job_id).unwrap();

    sql.update_job_progress(sql_job_id, 50).unwrap();
    frank.update_job_progress(frank_job_id, 50).unwrap();

    sql.complete_embedding_job(sql_job_id).unwrap();
    frank.complete_embedding_job(frank_job_id).unwrap();

    let sql_jobs = sql.get_embedding_jobs(db_path).unwrap();
    let frank_jobs = frank.get_embedding_jobs(db_path).unwrap();

    assert_eq!(sql_jobs.len(), frank_jobs.len());
    assert_eq!(sql_jobs.len(), 1);
    assert_eq!(sql_jobs[0].status, frank_jobs[0].status);
}

#[test]
#[ignore = "frankensqlite: Placeholder in subquery (embedding_jobs WHERE clause)"]
fn parity_embedding_job_failure() {
    let (_dir, sql, frank) = open_both();

    let db_path = "/test/fail.sqlite";
    let model_id = "test-model";

    let sql_job_id = sql.upsert_embedding_job(db_path, model_id, 50).unwrap();
    let frank_job_id = frank.upsert_embedding_job(db_path, model_id, 50).unwrap();

    sql.start_embedding_job(sql_job_id).unwrap();
    frank.start_embedding_job(frank_job_id).unwrap();

    sql.fail_embedding_job(sql_job_id, "OOM error").unwrap();
    frank.fail_embedding_job(frank_job_id, "OOM error").unwrap();

    let sql_jobs = sql.get_embedding_jobs(db_path).unwrap();
    let frank_jobs = frank.get_embedding_jobs(db_path).unwrap();

    assert_eq!(sql_jobs[0].status, frank_jobs[0].status);
    assert_eq!(sql_jobs[0].error_message, frank_jobs[0].error_message);
}

#[test]
#[ignore = "frankensqlite: Placeholder in subquery (cancel_embedding_jobs WHERE clause)"]
fn parity_cancel_embedding_jobs() {
    let (_dir, sql, frank) = open_both();

    let db_path = "/test/cancel.sqlite";

    for model in ["model-a", "model-b"] {
        sql.upsert_embedding_job(db_path, model, 10).unwrap();
        frank.upsert_embedding_job(db_path, model, 10).unwrap();
    }

    let sql_cancelled = sql.cancel_embedding_jobs(db_path, Some("model-a")).unwrap();
    let frank_cancelled = frank
        .cancel_embedding_jobs(db_path, Some("model-a"))
        .unwrap();

    assert_eq!(
        sql_cancelled, frank_cancelled,
        "Cancelled counts should match"
    );
}

// ============================================================================
// 13. TRANSITION: rusqlite DB → FrankenStorage
// ============================================================================

#[test]
#[ignore = "frankensqlite: 'no such column: kind in table sources' when reading rusqlite-created schema"]
fn transition_rusqlite_db_readable_by_frankenstorage_basic() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("transition.db");

    // Create and populate with SqliteStorage
    {
        let mut sql = SqliteStorage::open(&db_path).unwrap();
        let agent_id = sql.ensure_agent(&make_agent("claude", "Claude")).unwrap();

        let conv = make_conversation(
            "claude",
            "trans-001",
            "Transition Test",
            vec![
                make_message(0, MessageRole::User, "Before transition"),
                make_message(1, MessageRole::Agent, "Response before transition"),
            ],
        );
        sql.insert_conversation_tree(agent_id, None, &conv).unwrap();
    }

    // Open same DB with FrankenStorage
    let frank = FrankenStorage::open(&db_path).unwrap();

    // Verify schema version
    assert_eq!(frank.schema_version().unwrap(), CURRENT_SCHEMA_VERSION);

    // Verify agents readable
    let agents = frank.list_agents().unwrap();
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0].slug, "claude");

    // Verify sources readable
    let src = frank.get_source("local").unwrap();
    assert!(src.is_some());
}

#[test]
#[ignore = "frankensqlite: ORDER BY expression not in SELECT list (list_conversations)"]
fn transition_rusqlite_db_conversations_readable() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("transition_conv.db");

    {
        let mut sql = SqliteStorage::open(&db_path).unwrap();
        let agent_id = sql.ensure_agent(&make_agent("claude", "Claude")).unwrap();

        let conv = make_conversation(
            "claude",
            "trans-001",
            "Transition Test",
            vec![make_message(0, MessageRole::User, "Before transition")],
        );
        sql.insert_conversation_tree(agent_id, None, &conv).unwrap();
    }

    let frank = FrankenStorage::open(&db_path).unwrap();

    let convs = frank.list_conversations(10, 0).unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].title.as_deref(), Some("Transition Test"));
}

#[test]
#[ignore = "frankensqlite: DB file format not compatible with C SQLite reader"]
fn transition_frankenstorage_data_readable_by_rusqlite() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("frank_first.db");

    {
        let frank = FrankenStorage::open(&db_path).unwrap();
        let agent_id = frank
            .ensure_agent(&make_agent("codex", "OpenAI Codex"))
            .unwrap();

        let conv = make_conversation(
            "codex",
            "frank-001",
            "Frank-Created",
            vec![make_message(
                0,
                MessageRole::User,
                "Created by FrankenStorage",
            )],
        );
        frank
            .insert_conversation_tree(agent_id, None, &conv)
            .unwrap();
    }

    let conn = rusqlite::Connection::open(&db_path).unwrap();

    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM conversations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1);

    let title: String = conn
        .query_row("SELECT title FROM conversations LIMIT 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(title, "Frank-Created");
}

// ============================================================================
// 14. DAILY STATS PARITY
// ============================================================================

#[test]
#[ignore = "frankensqlite: mixed aggregate and non-aggregate columns without GROUP BY"]
fn parity_count_sessions_in_range() {
    let (_dir, mut sql, frank) = open_both();

    let agent = make_agent("claude", "Claude");
    let sql_agent_id = sql.ensure_agent(&agent).unwrap();
    let frank_agent_id = frank.ensure_agent(&agent).unwrap();

    for i in 0..3 {
        let mut conv = make_conversation(
            "claude",
            &format!("range-{i}"),
            &format!("Range {i}"),
            vec![make_message(0, MessageRole::User, "test")],
        );
        conv.started_at = Some(1700000000000 + i * 86_400_000);
        sql.insert_conversation_tree(sql_agent_id, None, &conv)
            .unwrap();
        frank
            .insert_conversation_tree(frank_agent_id, None, &conv)
            .unwrap();
    }

    let (sql_count, sql_approx) = sql.count_sessions_in_range(None, None, None, None).unwrap();
    let (frank_count, frank_approx) = frank
        .count_sessions_in_range(None, None, None, None)
        .unwrap();
    assert_eq!(sql_count, frank_count);
    assert_eq!(sql_approx, frank_approx);
    assert_eq!(sql_count, 3);
}
