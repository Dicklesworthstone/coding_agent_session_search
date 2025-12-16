//! P7.3 Integration tests for multi-source indexing
//!
//! These tests verify the full indexing pipeline handles multiple sources correctly,
//! including provenance attribution and source-based filtering.

use std::path::PathBuf;

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

fn norm_msg(idx: i64, created_at: i64, content: &str) -> coding_agent_search::connectors::NormalizedMessage {
    coding_agent_search::connectors::NormalizedMessage {
        idx,
        role: "user".into(),
        author: Some("user".into()),
        created_at: Some(created_at),
        content: content.to_string(),
        extra: json!({}),
        snippets: vec![],
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
    let mut storage = SqliteStorage::open(&db_path).expect("open db");

    // Setup sources
    storage.upsert_source(&Source::local()).expect("local source");
    storage
        .upsert_source(&Source::remote("laptop", "user@laptop.local"))
        .expect("remote source");
    storage
        .upsert_source(&Source::remote("workstation", "dev@workstation.example.com"))
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
                    vec![msg(0, now + 10000 + i * 1000, &format!("Laptop message {}", i))],
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
                    vec![msg(0, now + 20000 + i * 1000, &format!("Workstation message {}", i))],
                ),
            )
            .unwrap();
    }

    // Verify total count
    let total: i64 = storage
        .raw()
        .query_row("SELECT COUNT(*) FROM conversations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(total, 8, "should have 8 total conversations");

    // Verify local count
    let local_count: i64 = storage
        .raw()
        .query_row(
            "SELECT COUNT(*) FROM conversations WHERE source_id = 'local'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(local_count, 3, "should have 3 local conversations");

    // Verify remote count (all non-local)
    let remote_count: i64 = storage
        .raw()
        .query_row(
            "SELECT COUNT(*) FROM conversations WHERE source_id != 'local'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(remote_count, 5, "should have 5 remote conversations");

    // Verify specific source counts
    let laptop_count: i64 = storage
        .raw()
        .query_row(
            "SELECT COUNT(*) FROM conversations WHERE source_id = 'laptop'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(laptop_count, 2, "should have 2 laptop conversations");

    let workstation_count: i64 = storage
        .raw()
        .query_row(
            "SELECT COUNT(*) FROM conversations WHERE source_id = 'workstation'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(workstation_count, 3, "should have 3 workstation conversations");

    // Verify origin_host is preserved for remote conversations
    let remote_with_host: Vec<(String, Option<String>)> = storage
        .raw()
        .prepare("SELECT source_id, origin_host FROM conversations WHERE source_id != 'local'")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

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
    let mut storage = SqliteStorage::open(&db_path).expect("open db");

    // Setup sources
    storage.upsert_source(&Source::local()).expect("local source");
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
    persist::persist_conversation(&mut storage, &mut t_index, &local_conv).unwrap();

    // Persist a remote conversation
    let remote_conv = norm_conv_with_provenance(
        "remote-conv",
        "laptop",
        Some("user@laptop.local"),
        now + 1000,
        vec![norm_msg(0, now + 1000, "Remote test message")],
    );
    persist::persist_conversation(&mut storage, &mut t_index, &remote_conv).unwrap();
    t_index.commit().unwrap();

    // Verify provenance was extracted correctly
    let results: Vec<(String, String, Option<String>)> = storage
        .raw()
        .prepare("SELECT external_id, source_id, origin_host FROM conversations ORDER BY external_id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    assert_eq!(results.len(), 2);

    let local = results.iter().find(|(id, _, _)| id == "local-conv").unwrap();
    assert_eq!(local.1, "local");
    assert!(local.2.is_none());

    let remote = results.iter().find(|(id, _, _)| id == "remote-conv").unwrap();
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
    let mut storage = SqliteStorage::open(&db_path).expect("open db");

    storage.upsert_source(&Source::local()).expect("local source");
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
    storage.insert_conversation_tree(agent_id, Some(ws_id),
        &conv_with_source("c1", "local", None, now, vec![msg(0, now, "test local")])).unwrap();
    storage.insert_conversation_tree(agent_id, Some(ws_id),
        &conv_with_source("c2", "remote1", Some("host1.local"), now + 1000, vec![msg(0, now + 1000, "test remote1")])).unwrap();
    storage.insert_conversation_tree(agent_id, Some(ws_id),
        &conv_with_source("c3", "local", None, now + 2000, vec![msg(0, now + 2000, "test local 2")])).unwrap();
    storage.insert_conversation_tree(agent_id, Some(ws_id),
        &conv_with_source("c4", "remote2", Some("host2.local"), now + 3000, vec![msg(0, now + 3000, "test remote2")])).unwrap();

    // Query local only
    let local_results: Vec<String> = storage
        .raw()
        .prepare("SELECT external_id FROM conversations WHERE source_id = 'local' ORDER BY external_id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    assert_eq!(local_results.len(), 2);
    assert!(local_results.contains(&"c1".to_string()));
    assert!(local_results.contains(&"c3".to_string()));
}

/// P7.3: Verify filtering conversations by source_id != 'local' (remote)
#[test]
fn filter_conversations_remote_only() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("filter_remote.db");
    let mut storage = SqliteStorage::open(&db_path).expect("open db");

    storage.upsert_source(&Source::local()).expect("local source");
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
    storage.insert_conversation_tree(agent_id, Some(ws_id),
        &conv_with_source("c1", "local", None, now, vec![msg(0, now, "test local")])).unwrap();
    storage.insert_conversation_tree(agent_id, Some(ws_id),
        &conv_with_source("c2", "remote1", Some("host1.local"), now + 1000, vec![msg(0, now + 1000, "test remote1")])).unwrap();
    storage.insert_conversation_tree(agent_id, Some(ws_id),
        &conv_with_source("c3", "local", None, now + 2000, vec![msg(0, now + 2000, "test local 2")])).unwrap();
    storage.insert_conversation_tree(agent_id, Some(ws_id),
        &conv_with_source("c4", "remote2", Some("host2.local"), now + 3000, vec![msg(0, now + 3000, "test remote2")])).unwrap();

    // Query remote only (source_id != 'local')
    let remote_results: Vec<String> = storage
        .raw()
        .prepare("SELECT external_id FROM conversations WHERE source_id != 'local' ORDER BY external_id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    assert_eq!(remote_results.len(), 2);
    assert!(remote_results.contains(&"c2".to_string()));
    assert!(remote_results.contains(&"c4".to_string()));
}

/// P7.3: Verify filtering by specific source_id
#[test]
fn filter_conversations_specific_source() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("filter_specific.db");
    let mut storage = SqliteStorage::open(&db_path).expect("open db");

    storage.upsert_source(&Source::local()).expect("local source");
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
    storage.insert_conversation_tree(agent_id, Some(ws_id),
        &conv_with_source("c1", "local", None, now, vec![msg(0, now, "local")])).unwrap();
    storage.insert_conversation_tree(agent_id, Some(ws_id),
        &conv_with_source("c2", "laptop", Some("user@laptop"), now + 1000, vec![msg(0, now + 1000, "laptop1")])).unwrap();
    storage.insert_conversation_tree(agent_id, Some(ws_id),
        &conv_with_source("c3", "server", Some("admin@server"), now + 2000, vec![msg(0, now + 2000, "server1")])).unwrap();
    storage.insert_conversation_tree(agent_id, Some(ws_id),
        &conv_with_source("c4", "laptop", Some("user@laptop"), now + 3000, vec![msg(0, now + 3000, "laptop2")])).unwrap();

    // Query laptop only
    let laptop_results: Vec<String> = storage
        .raw()
        .prepare("SELECT external_id FROM conversations WHERE source_id = 'laptop' ORDER BY external_id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    assert_eq!(laptop_results.len(), 2);
    assert!(laptop_results.contains(&"c2".to_string()));
    assert!(laptop_results.contains(&"c4".to_string()));

    // Query server only
    let server_results: Vec<String> = storage
        .raw()
        .prepare("SELECT external_id FROM conversations WHERE source_id = 'server' ORDER BY external_id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

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
    let mut storage = SqliteStorage::open(&db_path).expect("open db");

    let index_dir = data_dir.join("index");
    std::fs::create_dir_all(&index_dir).unwrap();
    let mut t_index = TantivyIndex::open_or_create(&index_dir).expect("create index");

    // Setup sources
    storage.upsert_source(&Source::local()).expect("local source");

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

    persist::persist_conversation(&mut storage, &mut t_index, &local_conv1).unwrap();
    persist::persist_conversation(&mut storage, &mut t_index, &local_conv2).unwrap();
    t_index.commit().unwrap();

    let initial_count: i64 = storage
        .raw()
        .query_row("SELECT COUNT(*) FROM conversations", [], |r| r.get(0))
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

    persist::persist_conversation(&mut storage, &mut t_index, &remote_conv1).unwrap();
    persist::persist_conversation(&mut storage, &mut t_index, &remote_conv2).unwrap();
    t_index.commit().unwrap();

    let final_count: i64 = storage
        .raw()
        .query_row("SELECT COUNT(*) FROM conversations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        final_count,
        initial_count + 2,
        "should have 4 conversations after incremental add"
    );

    // Verify source distribution
    let local_count: i64 = storage
        .raw()
        .query_row(
            "SELECT COUNT(*) FROM conversations WHERE source_id = 'local'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let laptop_count: i64 = storage
        .raw()
        .query_row(
            "SELECT COUNT(*) FROM conversations WHERE source_id = 'laptop'",
            [],
            |r| r.get(0),
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
    let mut storage = SqliteStorage::open(&db_path).expect("open db");

    let index_dir = data_dir.join("index");
    std::fs::create_dir_all(&index_dir).unwrap();
    let mut t_index = TantivyIndex::open_or_create(&index_dir).expect("create index");

    storage.upsert_source(&Source::local()).expect("local source");
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
    persist::persist_conversation(&mut storage, &mut t_index, &conv_v1).unwrap();
    t_index.commit().unwrap();

    let initial_msg_count: i64 = storage
        .raw()
        .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
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
    persist::persist_conversation(&mut storage, &mut t_index, &conv_v2).unwrap();
    t_index.commit().unwrap();

    let final_msg_count: i64 = storage
        .raw()
        .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(final_msg_count, 3, "should have 3 messages after append");

    // Verify conversation count didn't change (still 1)
    let conv_count: i64 = storage
        .raw()
        .query_row("SELECT COUNT(*) FROM conversations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(conv_count, 1, "should still have 1 conversation");

    // Verify provenance is preserved
    let (source_id, origin_host): (String, Option<String>) = storage
        .raw()
        .query_row(
            "SELECT source_id, origin_host FROM conversations WHERE external_id = 'remote-conv'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
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
    let mut storage = SqliteStorage::open(&db_path).expect("open db");

    // Setup multiple sources
    storage.upsert_source(&Source::local()).expect("local source");
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
        storage.insert_conversation_tree(agent_id, Some(ws_id),
            &conv_with_source(&format!("local-{}", i), "local", None, now + i * 1000,
                vec![msg(0, now + i * 1000, &format!("local {}", i))])).unwrap();
    }
    for i in 0..3 {
        storage.insert_conversation_tree(agent_id, Some(ws_id),
            &conv_with_source(&format!("laptop-{}", i), "laptop", Some("user@laptop"), now + 10000 + i * 1000,
                vec![msg(0, now + 10000 + i * 1000, &format!("laptop {}", i))])).unwrap();
    }
    for i in 0..2 {
        storage.insert_conversation_tree(agent_id, Some(ws_id),
            &conv_with_source(&format!("server-{}", i), "server", Some("admin@server"), now + 20000 + i * 1000,
                vec![msg(0, now + 20000 + i * 1000, &format!("server {}", i))])).unwrap();
    }

    // Query source distribution stats
    let distribution: Vec<(String, i64)> = storage
        .raw()
        .prepare("SELECT source_id, COUNT(*) as count FROM conversations GROUP BY source_id ORDER BY source_id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

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
        .query_row(
            "SELECT COUNT(*) FROM conversations WHERE source_id = 'local'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let remote_total: i64 = storage
        .raw()
        .query_row(
            "SELECT COUNT(*) FROM conversations WHERE source_id != 'local'",
            [],
            |r| r.get(0),
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
    let mut storage = SqliteStorage::open(&db_path).expect("open db");

    storage.upsert_source(&Source::local()).expect("local source");
    storage
        .upsert_source(&Source::remote("laptop", "user@laptop"))
        .expect("laptop");

    let agent_id = storage.ensure_agent(&sample_agent()).unwrap();

    let now = 1700000000i64;
    storage.insert_conversation_tree(agent_id, None,
        &conv_with_source("c1", "local", None, now, vec![msg(0, now, "local")])).unwrap();
    storage.insert_conversation_tree(agent_id, None,
        &conv_with_source("c2", "laptop", Some("user@laptop"), now + 1000, vec![msg(0, now + 1000, "remote")])).unwrap();

    // Query with JOIN to get source kind
    let results: Vec<(String, String, String)> = storage
        .raw()
        .prepare(
            "SELECT c.external_id, c.source_id, s.kind
             FROM conversations c
             LEFT JOIN sources s ON c.source_id = s.id
             ORDER BY c.external_id",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    assert_eq!(results.len(), 2);

    let c1 = results.iter().find(|(id, _, _)| id == "c1").unwrap();
    assert_eq!(c1.1, "local");
    assert_eq!(c1.2, "local");

    let c2 = results.iter().find(|(id, _, _)| id == "c2").unwrap();
    assert_eq!(c2.1, "laptop");
    assert_eq!(c2.2, "ssh");
}
