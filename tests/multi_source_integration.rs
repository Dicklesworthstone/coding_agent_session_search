//! P7.3 Integration tests for multi-source indexing
//!
//! These tests verify the full indexing pipeline handles multiple sources correctly,
//! including provenance attribution and source-based filtering.

use std::path::PathBuf;

use coding_agent_search::franken_sync::compat::{ConnectionExt, RowExt};
use coding_agent_search::indexer::persist;
use coding_agent_search::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
use coding_agent_search::search::tantivy::TantivyIndex;
use coding_agent_search::sources::provenance::Source;
use coding_agent_search::storage::sqlite::SqliteStorage;
use serde_json::json;
use tempfile::TempDir;

mod util;

fn sample_agent() -> Agent {
    Agent {
        id: None,
        slug: "tester".into(),
        name: "Tester".into(),
        version: Some("1.0".into()),
        kind: AgentKind::Cli,
    }
}

fn msg(idx: i64, created_at: i64, content: &str) -> Message {
    Message {
        id: None,
        idx,
        role: MessageRole::User,
        author: Some("user".into()),
        created_at: Some(created_at),
        content: content.to_string(),
        extra_json: json!({}),
        snippets: vec![],
    }
}

fn conv_with_source(
    external_id: &str,
    source_id: &str,
    origin_host: Option<&str>,
    started_at: i64,
    messages: Vec<Message>,
) -> Conversation {
    Conversation {
        id: None,
        agent_slug: "tester".into(),
        workspace: Some(PathBuf::from("/workspace/demo")),
        external_id: Some(external_id.to_string()),
        title: Some(format!("Conv from {}", source_id)),
        source_path: PathBuf::from(format!("/logs/{}.jsonl", external_id)),
        started_at: Some(started_at),
        ended_at: Some(started_at + 100),
        approx_tokens: Some(42),
        metadata_json: json!({}),
        messages,
        source_id: source_id.to_string(),
        origin_host: origin_host.map(String::from),
    }
}

/// Create a NormalizedConversation with provenance metadata for persist testing
fn norm_conv_with_provenance(
    external_id: &str,
    source_id: &str,
    origin_host: Option<&str>,
    started_at: i64,
    messages: Vec<coding_agent_search::connectors::NormalizedMessage>,
) -> coding_agent_search::connectors::NormalizedConversation {
    let metadata = json!({
        "cass": {
            "origin": {
                "source_id": source_id,
                "kind": if source_id == "local" { "local" } else { "ssh" },
                "host": origin_host
            }
        }
    });

    coding_agent_search::connectors::NormalizedConversation {
        agent_slug: "tester".into(),
        external_id: Some(external_id.to_string()),
        title: Some(format!("Conv from {}", source_id)),
        workspace: Some(PathBuf::from("/workspace/demo")),
        source_path: PathBuf::from(format!("/logs/{}.jsonl", external_id)),
        started_at: Some(started_at),
        ended_at: Some(started_at + 100),
        metadata,
        messages,
    }
}

fn norm_msg(
    idx: i64,
    created_at: i64,
    content: &str,
) -> coding_agent_search::connectors::NormalizedMessage {
    coding_agent_search::connectors::NormalizedMessage {
        idx,
        role: "user".into(),
        author: Some("user".into()),
        created_at: Some(created_at),
        content: content.to_string(),
        extra: json!({}),
        snippets: vec![],
        invocations: Vec::new(),
    }
}

// =============================================================================
// Multi-Source Indexing Tests
// =============================================================================

/// P7.3: Verify that indexing conversations from multiple sources preserves provenance
#[test]
fn index_local_and_remote_sources_preserves_provenance() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("multi_source.db");
    let storage = SqliteStorage::open(&db_path).expect("open db");

    // Setup sources
    storage
        .upsert_source(&Source::local())
        .expect("local source");
    storage
        .upsert_source(&Source::remote("laptop", "user@laptop.local"))
        .expect("remote source");
    storage
        .upsert_source(&Source::remote(
            "workstation",
            "dev@workstation.example.com",
        ))
        .expect("workstation source");

    let agent_id = storage.ensure_agent(&sample_agent()).unwrap();
    let ws_id = storage
        .ensure_workspace(PathBuf::from("/workspace/demo").as_path(), Some("Demo"))
        .unwrap();

    let now = 1700000000i64;

    // Insert local conversations
    for i in 0..3 {
        storage
            .insert_conversation_tree(
                agent_id,
                Some(ws_id),
                &conv_with_source(
                    &format!("local-{}", i),
                    "local",
                    None,
                    now + i * 1000,
                    vec![msg(0, now + i * 1000, &format!("Local message {}", i))],
                ),
            )
            .unwrap();
    }

    // Insert laptop conversations (remote)
    for i in 0..2 {
        storage
            .insert_conversation_tree(
                agent_id,
                Some(ws_id),
                &conv_with_source(
                    &format!("laptop-{}", i),
                    "laptop",
                    Some("user@laptop.local"),
                    now + 10000 + i * 1000,
                    vec![msg(
                        0,
                        now + 10000 + i * 1000,
                        &format!("Laptop message {}", i),
                    )],
                ),
            )
            .unwrap();
    }

    // Insert workstation conversations (remote)
    for i in 0..3 {
        storage
            .insert_conversation_tree(
                agent_id,
                Some(ws_id),
                &conv_with_source(
                    &format!("workstation-{}", i),
                    "workstation",
                    Some("dev@workstation.example.com"),
                    now + 20000 + i * 1000,
                    vec![msg(
                        0,
                        now + 20000 + i * 1000,
                        &format!("Workstation message {}", i),
                    )],
                ),
            )
            .unwrap();
    }

    // Verify total count
    let total: i64 = storage
        .raw()
        .query_row_map("SELECT COUNT(*) FROM conversations", &[], |r| {
            r.get_typed(0)
        })
        .unwrap();
    assert_eq!(total, 8, "should have 8 total conversations");

    // Verify local count
    let local_count: i64 = storage
        .raw()
        .query_row_map(
            "SELECT COUNT(*) FROM conversations WHERE source_id = 'local'",
            &[],
            |r| r.get_typed(0),
        )
        .unwrap();
    assert_eq!(local_count, 3, "should have 3 local conversations");

    // Verify remote count (all non-local)
    let remote_count: i64 = storage
        .raw()
        .query_row_map(
            "SELECT COUNT(*) FROM conversations WHERE source_id != 'local'",
            &[],
            |r| r.get_typed(0),
        )
        .unwrap();
    assert_eq!(remote_count, 5, "should have 5 remote conversations");

    // Verify specific source counts
    let laptop_count: i64 = storage
        .raw()
        .query_row_map(
            "SELECT COUNT(*) FROM conversations WHERE source_id = 'laptop'",
            &[],
            |r| r.get_typed(0),
        )
        .unwrap();
    assert_eq!(laptop_count, 2, "should have 2 laptop conversations");

    let workstation_count: i64 = storage
        .raw()
        .query_row_map(
            "SELECT COUNT(*) FROM conversations WHERE source_id = 'workstation'",
            &[],
            |r| r.get_typed(0),
        )
        .unwrap();
    assert_eq!(
        workstation_count, 3,
        "should have 3 workstation conversations"
    );

    // Verify origin_host is preserved for remote conversations
    let remote_with_host: Vec<(String, Option<String>)> = storage
        .raw()
        .query_map_collect(
            "SELECT source_id, origin_host FROM conversations WHERE source_id != 'local'",
            &[],
            |r| Ok((r.get_typed(0)?, r.get_typed(1)?)),
        )
        .unwrap();

    for (source_id, origin_host) in remote_with_host {
        assert!(
            origin_host.is_some(),
            "Remote source {} should have origin_host",
            source_id
        );
    }
}

/// P7.3: Verify persist::persist_conversation extracts provenance from metadata
#[test]
fn persist_conversation_extracts_provenance_from_metadata() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let db_path = data_dir.join("provenance.db");
    let storage = SqliteStorage::open(&db_path).expect("open db");

    // Setup sources
    storage
        .upsert_source(&Source::local())
        .expect("local source");
    storage
        .upsert_source(&Source::remote("laptop", "user@laptop.local"))
        .expect("remote source");

    let index_dir = data_dir.join("index");
    std::fs::create_dir_all(&index_dir).unwrap();
    let mut t_index = TantivyIndex::open_or_create(&index_dir).expect("create index");

    let now = 1700000000i64;

    // Persist a local conversation
    let local_conv = norm_conv_with_provenance(
        "local-conv",
        "local",
        None,
        now,
        vec![norm_msg(0, now, "Local test message")],
    );
    persist::persist_conversation(&storage, &mut t_index, &local_conv).unwrap();

    // Persist a remote conversation
    let remote_conv = norm_conv_with_provenance(
        "remote-conv",
        "laptop",
        Some("user@laptop.local"),
        now + 1000,
        vec![norm_msg(0, now + 1000, "Remote test message")],
    );
    persist::persist_conversation(&storage, &mut t_index, &remote_conv).unwrap();
    t_index.commit().unwrap();

    // Verify provenance was extracted correctly
    let results: Vec<(String, String, Option<String>)> = storage
        .raw()
        .query_map_collect(
            "SELECT external_id, source_id, origin_host FROM conversations ORDER BY external_id",
            &[],
            |r| Ok((r.get_typed(0)?, r.get_typed(1)?, r.get_typed(2)?)),
        )
        .unwrap();

    assert_eq!(results.len(), 2);

    let local = results
        .iter()
        .find(|(id, _, _)| id == "local-conv")
        .unwrap();
    assert_eq!(local.1, "local");
    assert!(local.2.is_none());

    let remote = results
        .iter()
        .find(|(id, _, _)| id == "remote-conv")
        .unwrap();
    assert_eq!(remote.1, "laptop");
    assert_eq!(remote.2.as_deref(), Some("user@laptop.local"));
}

// =============================================================================
// Source Filtering Tests
// =============================================================================

/// P7.3: Verify filtering conversations by source_id = 'local'
#[test]
fn filter_conversations_local_only() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("filter_local.db");
    let storage = SqliteStorage::open(&db_path).expect("open db");

    storage
        .upsert_source(&Source::local())
        .expect("local source");
    storage
        .upsert_source(&Source::remote("remote1", "host1.local"))
        .expect("remote1");
    storage
        .upsert_source(&Source::remote("remote2", "host2.local"))
        .expect("remote2");

    let agent_id = storage.ensure_agent(&sample_agent()).unwrap();
    let ws_id = storage
        .ensure_workspace(PathBuf::from("/workspace/demo").as_path(), Some("Demo"))
        .unwrap();

    let now = 1700000000i64;

    // Insert mixed conversations
    storage
        .insert_conversation_tree(
            agent_id,
            Some(ws_id),
            &conv_with_source("c1", "local", None, now, vec![msg(0, now, "test local")]),
        )
        .unwrap();
    storage
        .insert_conversation_tree(
            agent_id,
            Some(ws_id),
            &conv_with_source(
                "c2",
                "remote1",
                Some("host1.local"),
                now + 1000,
                vec![msg(0, now + 1000, "test remote1")],
            ),
        )
        .unwrap();
    storage
        .insert_conversation_tree(
            agent_id,
            Some(ws_id),
            &conv_with_source(
                "c3",
                "local",
                None,
                now + 2000,
                vec![msg(0, now + 2000, "test local 2")],
            ),
        )
        .unwrap();
    storage
        .insert_conversation_tree(
            agent_id,
            Some(ws_id),
            &conv_with_source(
                "c4",
                "remote2",
                Some("host2.local"),
                now + 3000,
                vec![msg(0, now + 3000, "test remote2")],
            ),
        )
        .unwrap();

    // Query local only
    let local_results: Vec<String> = storage
        .raw()
        .query_map_collect(
            "SELECT external_id FROM conversations WHERE source_id = 'local' ORDER BY external_id",
            &[],
            |r| r.get_typed(0),
        )
        .unwrap();

    assert_eq!(local_results.len(), 2);
    assert!(local_results.contains(&"c1".to_string()));
    assert!(local_results.contains(&"c3".to_string()));
}

/// P7.3: Verify filtering conversations by source_id != 'local' (remote)
#[test]
fn filter_conversations_remote_only() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("filter_remote.db");
    let storage = SqliteStorage::open(&db_path).expect("open db");

    storage
        .upsert_source(&Source::local())
        .expect("local source");
    storage
        .upsert_source(&Source::remote("remote1", "host1.local"))
        .expect("remote1");
    storage
        .upsert_source(&Source::remote("remote2", "host2.local"))
        .expect("remote2");

    let agent_id = storage.ensure_agent(&sample_agent()).unwrap();
    let ws_id = storage
        .ensure_workspace(PathBuf::from("/workspace/demo").as_path(), Some("Demo"))
        .unwrap();

    let now = 1700000000i64;

    // Insert mixed conversations
    storage
        .insert_conversation_tree(
            agent_id,
            Some(ws_id),
            &conv_with_source("c1", "local", None, now, vec![msg(0, now, "test local")]),
        )
        .unwrap();
    storage
        .insert_conversation_tree(
            agent_id,
            Some(ws_id),
            &conv_with_source(
                "c2",
                "remote1",
                Some("host1.local"),
                now + 1000,
                vec![msg(0, now + 1000, "test remote1")],
            ),
        )
        .unwrap();
    storage
        .insert_conversation_tree(
            agent_id,
            Some(ws_id),
            &conv_with_source(
                "c3",
                "local",
                None,
                now + 2000,
                vec![msg(0, now + 2000, "test local 2")],
            ),
        )
        .unwrap();
    storage
        .insert_conversation_tree(
            agent_id,
            Some(ws_id),
            &conv_with_source(
                "c4",
                "remote2",
                Some("host2.local"),
                now + 3000,
                vec![msg(0, now + 3000, "test remote2")],
            ),
        )
        .unwrap();

    // Query remote only (source_id != 'local')
    let remote_results: Vec<String> = storage
        .raw()
        .query_map_collect(
            "SELECT external_id FROM conversations WHERE source_id != 'local' ORDER BY external_id",
            &[],
            |r| r.get_typed(0),
        )
        .unwrap();

    assert_eq!(remote_results.len(), 2);
    assert!(remote_results.contains(&"c2".to_string()));
    assert!(remote_results.contains(&"c4".to_string()));
}

/// P7.3: Verify filtering by specific source_id
#[test]
fn filter_conversations_specific_source() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("filter_specific.db");
    let storage = SqliteStorage::open(&db_path).expect("open db");

    storage
        .upsert_source(&Source::local())
        .expect("local source");
    storage
        .upsert_source(&Source::remote("laptop", "user@laptop"))
        .expect("laptop");
    storage
        .upsert_source(&Source::remote("server", "admin@server"))
        .expect("server");

    let agent_id = storage.ensure_agent(&sample_agent()).unwrap();
    let ws_id = storage
        .ensure_workspace(PathBuf::from("/workspace/demo").as_path(), Some("Demo"))
        .unwrap();

    let now = 1700000000i64;

    // Insert conversations from different sources
    storage
        .insert_conversation_tree(
            agent_id,
            Some(ws_id),
            &conv_with_source("c1", "local", None, now, vec![msg(0, now, "local")]),
        )
        .unwrap();
    storage
        .insert_conversation_tree(
            agent_id,
            Some(ws_id),
            &conv_with_source(
                "c2",
                "laptop",
                Some("user@laptop"),
                now + 1000,
                vec![msg(0, now + 1000, "laptop1")],
            ),
        )
        .unwrap();
    storage
        .insert_conversation_tree(
            agent_id,
            Some(ws_id),
            &conv_with_source(
                "c3",
                "server",
                Some("admin@server"),
                now + 2000,
                vec![msg(0, now + 2000, "server1")],
            ),
        )
        .unwrap();
    storage
        .insert_conversation_tree(
            agent_id,
            Some(ws_id),
            &conv_with_source(
                "c4",
                "laptop",
                Some("user@laptop"),
                now + 3000,
                vec![msg(0, now + 3000, "laptop2")],
            ),
        )
        .unwrap();

    // Query laptop only
    let laptop_results: Vec<String> = storage
        .raw()
        .query_map_collect(
            "SELECT external_id FROM conversations WHERE source_id = 'laptop' ORDER BY external_id",
            &[],
            |r| r.get_typed(0),
        )
        .unwrap();

    assert_eq!(laptop_results.len(), 2);
    assert!(laptop_results.contains(&"c2".to_string()));
    assert!(laptop_results.contains(&"c4".to_string()));

    // Query server only
    let server_results: Vec<String> = storage
        .raw()
        .query_map_collect(
            "SELECT external_id FROM conversations WHERE source_id = 'server' ORDER BY external_id",
            &[],
            |r| r.get_typed(0),
        )
        .unwrap();

    assert_eq!(server_results.len(), 1);
    assert!(server_results.contains(&"c3".to_string()));
}

// =============================================================================
// Incremental Indexing Tests
// =============================================================================

/// P7.3: Verify incremental indexing adds new sources correctly
#[test]
fn incremental_index_new_remote_source() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let db_path = data_dir.join("incremental.db");
    let storage = SqliteStorage::open(&db_path).expect("open db");

    let index_dir = data_dir.join("index");
    std::fs::create_dir_all(&index_dir).unwrap();
    let mut t_index = TantivyIndex::open_or_create(&index_dir).expect("create index");

    // Setup sources
    storage
        .upsert_source(&Source::local())
        .expect("local source");

    let now = 1700000000i64;

    // Initial indexing: local conversations only
    let local_conv1 = norm_conv_with_provenance(
        "local-1",
        "local",
        None,
        now,
        vec![norm_msg(0, now, "Local message 1")],
    );
    let local_conv2 = norm_conv_with_provenance(
        "local-2",
        "local",
        None,
        now + 1000,
        vec![norm_msg(0, now + 1000, "Local message 2")],
    );

    persist::persist_conversation(&storage, &mut t_index, &local_conv1).unwrap();
    persist::persist_conversation(&storage, &mut t_index, &local_conv2).unwrap();
    t_index.commit().unwrap();

    let initial_count: i64 = storage
        .raw()
        .query_row_map("SELECT COUNT(*) FROM conversations", &[], |r| {
            r.get_typed(0)
        })
        .unwrap();
    assert_eq!(initial_count, 2, "should have 2 initial conversations");

    // Add a new remote source
    storage
        .upsert_source(&Source::remote("laptop", "user@laptop.local"))
        .expect("add remote source");

    // Incremental indexing: add remote conversations
    let remote_conv1 = norm_conv_with_provenance(
        "laptop-1",
        "laptop",
        Some("user@laptop.local"),
        now + 10000,
        vec![norm_msg(0, now + 10000, "Laptop message 1")],
    );
    let remote_conv2 = norm_conv_with_provenance(
        "laptop-2",
        "laptop",
        Some("user@laptop.local"),
        now + 11000,
        vec![norm_msg(0, now + 11000, "Laptop message 2")],
    );

    persist::persist_conversation(&storage, &mut t_index, &remote_conv1).unwrap();
    persist::persist_conversation(&storage, &mut t_index, &remote_conv2).unwrap();
    t_index.commit().unwrap();

    let final_count: i64 = storage
        .raw()
        .query_row_map("SELECT COUNT(*) FROM conversations", &[], |r| {
            r.get_typed(0)
        })
        .unwrap();
    assert_eq!(
        final_count,
        initial_count + 2,
        "should have 4 conversations after incremental add"
    );

    // Verify source distribution
    let local_count: i64 = storage
        .raw()
        .query_row_map(
            "SELECT COUNT(*) FROM conversations WHERE source_id = 'local'",
            &[],
            |r| r.get_typed(0),
        )
        .unwrap();
    let laptop_count: i64 = storage
        .raw()
        .query_row_map(
            "SELECT COUNT(*) FROM conversations WHERE source_id = 'laptop'",
            &[],
            |r| r.get_typed(0),
        )
        .unwrap();

    assert_eq!(local_count, 2, "local count should remain 2");
    assert_eq!(laptop_count, 2, "laptop count should be 2");
}

/// P7.3: Verify appending messages to existing remote conversation works
#[test]
fn incremental_append_to_remote_conversation() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let db_path = data_dir.join("append.db");
    let storage = SqliteStorage::open(&db_path).expect("open db");

    let index_dir = data_dir.join("index");
    std::fs::create_dir_all(&index_dir).unwrap();
    let mut t_index = TantivyIndex::open_or_create(&index_dir).expect("create index");

    storage
        .upsert_source(&Source::local())
        .expect("local source");
    storage
        .upsert_source(&Source::remote("laptop", "user@laptop.local"))
        .expect("remote source");

    let now = 1700000000i64;

    // First sync: conversation with 2 messages
    let conv_v1 = norm_conv_with_provenance(
        "remote-conv",
        "laptop",
        Some("user@laptop.local"),
        now,
        vec![
            norm_msg(0, now, "First message"),
            norm_msg(1, now + 100, "Second message"),
        ],
    );
    persist::persist_conversation(&storage, &mut t_index, &conv_v1).unwrap();
    t_index.commit().unwrap();

    let initial_msg_count: i64 = storage
        .raw()
        .query_row_map("SELECT COUNT(*) FROM messages", &[], |r| r.get_typed(0))
        .unwrap();
    assert_eq!(initial_msg_count, 2, "should have 2 initial messages");

    // Second sync: same conversation with 1 new message
    let conv_v2 = norm_conv_with_provenance(
        "remote-conv",
        "laptop",
        Some("user@laptop.local"),
        now,
        vec![
            norm_msg(0, now, "First message"),
            norm_msg(1, now + 100, "Second message"),
            norm_msg(2, now + 200, "Third message"),
        ],
    );
    persist::persist_conversation(&storage, &mut t_index, &conv_v2).unwrap();
    t_index.commit().unwrap();

    let final_msg_count: i64 = storage
        .raw()
        .query_row_map("SELECT COUNT(*) FROM messages", &[], |r| r.get_typed(0))
        .unwrap();
    assert_eq!(final_msg_count, 3, "should have 3 messages after append");

    // Verify conversation count didn't change (still 1)
    let conv_count: i64 = storage
        .raw()
        .query_row_map("SELECT COUNT(*) FROM conversations", &[], |r| {
            r.get_typed(0)
        })
        .unwrap();
    assert_eq!(conv_count, 1, "should still have 1 conversation");

    // Verify provenance is preserved
    let (source_id, origin_host): (String, Option<String>) = storage
        .raw()
        .query_row_map(
            "SELECT source_id, origin_host FROM conversations WHERE external_id = 'remote-conv'",
            &[],
            |r| Ok((r.get_typed(0)?, r.get_typed(1)?)),
        )
        .unwrap();
    assert_eq!(source_id, "laptop");
    assert_eq!(origin_host.as_deref(), Some("user@laptop.local"));
}

// =============================================================================
// Stats and Distribution Tests
// =============================================================================

/// P7.3: Verify stats reflect source distribution
#[test]
fn stats_reflect_source_distribution() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("stats.db");
    let storage = SqliteStorage::open(&db_path).expect("open db");

    // Setup multiple sources
    storage
        .upsert_source(&Source::local())
        .expect("local source");
    storage
        .upsert_source(&Source::remote("laptop", "user@laptop"))
        .expect("laptop");
    storage
        .upsert_source(&Source::remote("server", "admin@server"))
        .expect("server");

    let agent_id = storage.ensure_agent(&sample_agent()).unwrap();
    let ws_id = storage
        .ensure_workspace(PathBuf::from("/workspace/demo").as_path(), Some("Demo"))
        .unwrap();

    let now = 1700000000i64;

    // Insert conversations with distribution: 5 local, 3 laptop, 2 server
    for i in 0..5 {
        storage
            .insert_conversation_tree(
                agent_id,
                Some(ws_id),
                &conv_with_source(
                    &format!("local-{}", i),
                    "local",
                    None,
                    now + i * 1000,
                    vec![msg(0, now + i * 1000, &format!("local {}", i))],
                ),
            )
            .unwrap();
    }
    for i in 0..3 {
        storage
            .insert_conversation_tree(
                agent_id,
                Some(ws_id),
                &conv_with_source(
                    &format!("laptop-{}", i),
                    "laptop",
                    Some("user@laptop"),
                    now + 10000 + i * 1000,
                    vec![msg(0, now + 10000 + i * 1000, &format!("laptop {}", i))],
                ),
            )
            .unwrap();
    }
    for i in 0..2 {
        storage
            .insert_conversation_tree(
                agent_id,
                Some(ws_id),
                &conv_with_source(
                    &format!("server-{}", i),
                    "server",
                    Some("admin@server"),
                    now + 20000 + i * 1000,
                    vec![msg(0, now + 20000 + i * 1000, &format!("server {}", i))],
                ),
            )
            .unwrap();
    }

    // Query source distribution stats
    let distribution: Vec<(String, i64)> = storage
        .raw()
        .query_map_collect(
            "SELECT source_id, COUNT(*) as count FROM conversations GROUP BY source_id ORDER BY source_id",
            &[],
            |r| Ok((r.get_typed(0)?, r.get_typed(1)?)),
        )
        .unwrap();

    assert_eq!(distribution.len(), 3, "should have 3 sources");

    let local = distribution.iter().find(|(s, _)| s == "local").unwrap();
    let laptop = distribution.iter().find(|(s, _)| s == "laptop").unwrap();
    let server = distribution.iter().find(|(s, _)| s == "server").unwrap();

    assert_eq!(local.1, 5, "local should have 5 conversations");
    assert_eq!(laptop.1, 3, "laptop should have 3 conversations");
    assert_eq!(server.1, 2, "server should have 2 conversations");

    // Verify total
    let total: i64 = distribution.iter().map(|(_, c)| c).sum();
    assert_eq!(total, 10, "total should be 10");

    // Verify local vs remote split
    let local_total: i64 = storage
        .raw()
        .query_row_map(
            "SELECT COUNT(*) FROM conversations WHERE source_id = 'local'",
            &[],
            |r| r.get_typed(0),
        )
        .unwrap();
    let remote_total: i64 = storage
        .raw()
        .query_row_map(
            "SELECT COUNT(*) FROM conversations WHERE source_id != 'local'",
            &[],
            |r| r.get_typed(0),
        )
        .unwrap();

    assert_eq!(local_total, 5, "local total should be 5");
    assert_eq!(remote_total, 5, "remote total should be 5");
}

/// P7.3: Verify origin_kind can be retrieved via JOIN with sources table
#[test]
fn source_kind_available_via_join() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("kind_join.db");
    let storage = SqliteStorage::open(&db_path).expect("open db");

    storage
        .upsert_source(&Source::local())
        .expect("local source");
    storage
        .upsert_source(&Source::remote("laptop", "user@laptop"))
        .expect("laptop");

    let agent_id = storage.ensure_agent(&sample_agent()).unwrap();

    let now = 1700000000i64;
    storage
        .insert_conversation_tree(
            agent_id,
            None,
            &conv_with_source("c1", "local", None, now, vec![msg(0, now, "local")]),
        )
        .unwrap();
    storage
        .insert_conversation_tree(
            agent_id,
            None,
            &conv_with_source(
                "c2",
                "laptop",
                Some("user@laptop"),
                now + 1000,
                vec![msg(0, now + 1000, "remote")],
            ),
        )
        .unwrap();

    // Query with JOIN to get source kind
    let results: Vec<(String, String, String)> = storage
        .raw()
        .query_map_collect(
            "SELECT c.external_id, c.source_id, s.kind
             FROM conversations c
             LEFT JOIN sources s ON c.source_id = s.id
             ORDER BY c.external_id",
            &[],
            |r| Ok((r.get_typed(0)?, r.get_typed(1)?, r.get_typed(2)?)),
        )
        .unwrap();

    assert_eq!(results.len(), 2);

    let c1 = results.iter().find(|(id, _, _)| id == "c1").unwrap();
    assert_eq!(c1.1, "local");
    assert_eq!(c1.2, "local");

    let c2 = results.iter().find(|(id, _, _)| id == "c2").unwrap();
    assert_eq!(c2.1, "laptop");
    assert_eq!(c2.2, "ssh");
}

// =============================================================================
// P7.4: Collision and Deduplication Tests
// Tests for edge cases where the same session might appear from multiple sources
// or where session IDs collide across sources.
// =============================================================================

/// P7.4: Verify that re-indexing the same conversation updates it (doesn't duplicate)
#[test]
fn resync_same_conversation_updates_not_duplicates() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let db_path = data_dir.join("resync.db");
    let storage = SqliteStorage::open(&db_path).expect("open db");

    let index_dir = data_dir.join("index");
    std::fs::create_dir_all(&index_dir).unwrap();
    let mut t_index = TantivyIndex::open_or_create(&index_dir).expect("create index");

    storage
        .upsert_source(&Source::local())
        .expect("local source");
    storage
        .upsert_source(&Source::remote("laptop", "user@laptop.local"))
        .expect("remote source");

    let now = 1700000000i64;

    // First sync from laptop
    let conv_v1 = norm_conv_with_provenance(
        "conv-abc123",
        "laptop",
        Some("user@laptop.local"),
        now,
        vec![
            norm_msg(0, now, "First message from laptop"),
            norm_msg(1, now + 100, "Second message"),
        ],
    );
    persist::persist_conversation(&storage, &mut t_index, &conv_v1).unwrap();
    t_index.commit().unwrap();

    let count_after_first: i64 = storage
        .raw()
        .query_row_map("SELECT COUNT(*) FROM conversations", &[], |r| {
            r.get_typed(0)
        })
        .unwrap();
    assert_eq!(
        count_after_first, 1,
        "should have 1 conversation after first sync"
    );

    // Second sync (same conversation, simulating re-sync with updated content)
    let conv_v2 = norm_conv_with_provenance(
        "conv-abc123",
        "laptop",
        Some("user@laptop.local"),
        now,
        vec![
            norm_msg(0, now, "First message from laptop"),
            norm_msg(1, now + 100, "Second message"),
            norm_msg(2, now + 200, "Third message (new)"),
        ],
    );
    persist::persist_conversation(&storage, &mut t_index, &conv_v2).unwrap();
    t_index.commit().unwrap();

    let count_after_second: i64 = storage
        .raw()
        .query_row_map("SELECT COUNT(*) FROM conversations", &[], |r| {
            r.get_typed(0)
        })
        .unwrap();
    assert_eq!(
        count_after_second, 1,
        "should still have 1 conversation after re-sync (not duplicated)"
    );

    // Verify messages were appended
    let msg_count: i64 = storage
        .raw()
        .query_row_map("SELECT COUNT(*) FROM messages", &[], |r| r.get_typed(0))
        .unwrap();
    assert_eq!(msg_count, 3, "should have 3 messages after update");
}

/// P7.4: Verify that same external_id from different sources creates distinct entries
#[test]
fn same_id_different_sources_are_distinct() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("collision.db");
    let storage = SqliteStorage::open(&db_path).expect("open db");

    storage
        .upsert_source(&Source::local())
        .expect("local source");
    storage
        .upsert_source(&Source::remote("laptop", "user@laptop.local"))
        .expect("laptop source");
    storage
        .upsert_source(&Source::remote("server", "admin@server.local"))
        .expect("server source");

    let agent_id = storage.ensure_agent(&sample_agent()).unwrap();

    let now = 1700000000i64;

    // Same external_id "session-001" from three different sources
    // This could happen with sequential IDs or if different machines happen to generate same UUID
    storage
        .insert_conversation_tree(
            agent_id,
            None,
            &conv_with_source(
                "session-001",
                "local",
                None,
                now,
                vec![msg(0, now, "Local version of session")],
            ),
        )
        .unwrap();

    storage
        .insert_conversation_tree(
            agent_id,
            None,
            &conv_with_source(
                "session-001",
                "laptop",
                Some("user@laptop.local"),
                now + 1000,
                vec![msg(0, now + 1000, "Laptop version of session")],
            ),
        )
        .unwrap();

    storage
        .insert_conversation_tree(
            agent_id,
            None,
            &conv_with_source(
                "session-001",
                "server",
                Some("admin@server.local"),
                now + 2000,
                vec![msg(0, now + 2000, "Server version of session")],
            ),
        )
        .unwrap();

    // Should have THREE entries (distinguished by source_id)
    let total: i64 = storage
        .raw()
        .query_row_map(
            "SELECT COUNT(*) FROM conversations WHERE external_id = 'session-001'",
            &[],
            |r| r.get_typed(0),
        )
        .unwrap();
    assert_eq!(
        total, 3,
        "should have 3 conversations with same external_id"
    );

    // Verify each source has one entry
    let by_source: Vec<(String, i64)> = storage
        .raw()
        .query_map_collect(
            "SELECT source_id, COUNT(*) FROM conversations
             WHERE external_id = 'session-001'
             GROUP BY source_id
             ORDER BY source_id",
            &[],
            |r| Ok((r.get_typed(0)?, r.get_typed(1)?)),
        )
        .unwrap();

    assert_eq!(by_source.len(), 3, "should have 3 sources");
    for (source_id, count) in by_source {
        assert_eq!(count, 1, "source {} should have exactly 1 entry", source_id);
    }
}

/// P7.4: Verify deduplication works within the same source
#[test]
fn dedup_within_source_not_across() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let db_path = data_dir.join("dedup.db");
    let storage = SqliteStorage::open(&db_path).expect("open db");

    let index_dir = data_dir.join("index");
    std::fs::create_dir_all(&index_dir).unwrap();
    let mut t_index = TantivyIndex::open_or_create(&index_dir).expect("create index");

    storage
        .upsert_source(&Source::local())
        .expect("local source");
    storage
        .upsert_source(&Source::remote("laptop", "user@laptop.local"))
        .expect("remote source");

    let now = 1700000000i64;

    // Create 3 conversations from laptop
    for i in 0..3 {
        let conv = norm_conv_with_provenance(
            &format!("laptop-conv-{}", i),
            "laptop",
            Some("user@laptop.local"),
            now + i * 1000,
            vec![norm_msg(
                0,
                now + i * 1000,
                &format!("Laptop message {}", i),
            )],
        );
        persist::persist_conversation(&storage, &mut t_index, &conv).unwrap();
    }
    t_index.commit().unwrap();

    let initial_count: i64 = storage
        .raw()
        .query_row_map("SELECT COUNT(*) FROM conversations", &[], |r| {
            r.get_typed(0)
        })
        .unwrap();
    assert_eq!(initial_count, 3, "should have 3 conversations initially");

    // Re-sync same 3 conversations (simulating re-indexing)
    for i in 0..3 {
        let conv = norm_conv_with_provenance(
            &format!("laptop-conv-{}", i),
            "laptop",
            Some("user@laptop.local"),
            now + i * 1000,
            vec![norm_msg(
                0,
                now + i * 1000,
                &format!("Laptop message {}", i),
            )],
        );
        persist::persist_conversation(&storage, &mut t_index, &conv).unwrap();
    }
    t_index.commit().unwrap();

    // Should still have same count (deduplicated within source)
    let final_count: i64 = storage
        .raw()
        .query_row_map("SELECT COUNT(*) FROM conversations", &[], |r| {
            r.get_typed(0)
        })
        .unwrap();
    assert_eq!(
        final_count, initial_count,
        "count should remain same after re-sync (deduplicated)"
    );
}

/// P7.4: Verify composite key (source_id, agent_id, external_id) is unique constraint
#[test]
fn composite_key_unique_constraint() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("unique.db");
    let storage = SqliteStorage::open(&db_path).expect("open db");

    storage
        .upsert_source(&Source::local())
        .expect("local source");
    storage
        .upsert_source(&Source::remote("laptop", "user@laptop.local"))
        .expect("remote source");

    let agent_id = storage.ensure_agent(&sample_agent()).unwrap();

    let now = 1700000000i64;

    // Insert first conversation
    storage
        .insert_conversation_tree(
            agent_id,
            None,
            &conv_with_source(
                "unique-test",
                "local",
                None,
                now,
                vec![msg(0, now, "First message")],
            ),
        )
        .unwrap();

    // Insert same external_id from different source - should succeed
    storage
        .insert_conversation_tree(
            agent_id,
            None,
            &conv_with_source(
                "unique-test",
                "laptop",
                Some("user@laptop.local"),
                now + 1000,
                vec![msg(0, now + 1000, "Laptop message")],
            ),
        )
        .unwrap();

    // Verify both exist
    let count: i64 = storage
        .raw()
        .query_row_map(
            "SELECT COUNT(*) FROM conversations WHERE external_id = 'unique-test'",
            &[],
            |r| r.get_typed(0),
        )
        .unwrap();
    assert_eq!(
        count, 2,
        "should have 2 conversations with same external_id from different sources"
    );

    // Verify composite uniqueness via SQL
    let unique_pairs: Vec<(String, String, String)> = storage
        .raw()
        .query_map_collect(
            "SELECT source_id, agent_id, external_id FROM conversations
             WHERE external_id = 'unique-test'
             ORDER BY source_id",
            &[],
            |r| {
                Ok((
                    r.get_typed::<String>(0)?,
                    r.get_typed::<i64>(1)?.to_string(),
                    r.get_typed::<String>(2)?,
                ))
            },
        )
        .unwrap();

    assert_eq!(unique_pairs.len(), 2);
    // Local and laptop should both have unique-test
    assert!(unique_pairs.iter().any(|(s, _, _)| s == "local"));
    assert!(unique_pairs.iter().any(|(s, _, _)| s == "laptop"));
}

/// P7.4: Verify updating conversation from same source preserves ended_at
#[test]
fn update_conversation_preserves_metadata() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let db_path = data_dir.join("metadata.db");
    let storage = SqliteStorage::open(&db_path).expect("open db");

    let index_dir = data_dir.join("index");
    std::fs::create_dir_all(&index_dir).unwrap();
    let mut t_index = TantivyIndex::open_or_create(&index_dir).expect("create index");

    storage
        .upsert_source(&Source::local())
        .expect("local source");
    storage
        .upsert_source(&Source::remote("laptop", "user@laptop.local"))
        .expect("remote source");

    let now = 1700000000i64;

    // First version with 2 messages
    let conv_v1 = norm_conv_with_provenance(
        "meta-test",
        "laptop",
        Some("user@laptop.local"),
        now,
        vec![
            norm_msg(0, now, "Message 1"),
            norm_msg(1, now + 100, "Message 2"),
        ],
    );
    persist::persist_conversation(&storage, &mut t_index, &conv_v1).unwrap();
    t_index.commit().unwrap();

    // Get initial ended_at
    let initial_ended_at: i64 = storage
        .raw()
        .query_row_map(
            "SELECT ended_at FROM conversations WHERE external_id = 'meta-test'",
            &[],
            |r| r.get_typed(0),
        )
        .unwrap();
    assert_eq!(
        initial_ended_at,
        now + 100,
        "ended_at should be time of last message"
    );

    // Update with new message
    let conv_v2 = norm_conv_with_provenance(
        "meta-test",
        "laptop",
        Some("user@laptop.local"),
        now,
        vec![
            norm_msg(0, now, "Message 1"),
            norm_msg(1, now + 100, "Message 2"),
            norm_msg(2, now + 200, "Message 3 (new)"),
        ],
    );
    persist::persist_conversation(&storage, &mut t_index, &conv_v2).unwrap();
    t_index.commit().unwrap();

    // Verify ended_at was updated
    let final_ended_at: i64 = storage
        .raw()
        .query_row_map(
            "SELECT ended_at FROM conversations WHERE external_id = 'meta-test'",
            &[],
            |r| r.get_typed(0),
        )
        .unwrap();
    assert_eq!(
        final_ended_at,
        now + 200,
        "ended_at should be updated to time of new last message"
    );

    // Verify provenance is still correct
    let (source_id, origin_host): (String, Option<String>) = storage
        .raw()
        .query_row_map(
            "SELECT source_id, origin_host FROM conversations WHERE external_id = 'meta-test'",
            &[],
            |r| Ok((r.get_typed(0)?, r.get_typed(1)?)),
        )
        .unwrap();
    assert_eq!(source_id, "laptop");
    assert_eq!(origin_host.as_deref(), Some("user@laptop.local"));
}
