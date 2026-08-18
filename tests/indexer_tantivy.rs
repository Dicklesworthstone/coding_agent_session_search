use coding_agent_search::connectors::{NormalizedConversation, NormalizedMessage};
use coding_agent_search::search::tantivy::{SCHEMA_HASH, TantivyIndex};
use serde_json::json;
use std::fs;
use tempfile::TempDir;

fn read_schema_hash(dir: &TempDir) -> String {
    let path = dir.path().join("schema_hash.json");
    fs::read_to_string(path).expect("schema_hash.json should exist")
}

#[test]
fn open_or_create_writes_schema_hash() {
    let dir = TempDir::new().unwrap();

    let mut index = TantivyIndex::open_or_create(dir.path()).expect("create index");
    index.commit().unwrap();

    let schema_file = read_schema_hash(&dir);
    assert!(
        schema_file.contains(SCHEMA_HASH),
        "schema_hash.json should contain current schema hash"
    );

    // The published manifest is what marks the directory as a real index;
    // `meta.json` was the Tantivy-era equivalent.
    assert!(
        dir.path()
            .join(coding_agent_search::search::quill_bridge::QUILL_INDEX_MARKER)
            .exists(),
        "the published Quill manifest should be present"
    );
}

#[test]
fn open_or_create_reuses_when_hash_matches() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).expect("create index");
    index.commit().unwrap();

    let sentinel = dir.path().join("sentinel.txt");
    fs::write(&sentinel, b"keep").unwrap();
    drop(index); // release writer lock before reopening

    // Second open with matching schema hash should not delete existing files.
    let mut index_again = TantivyIndex::open_or_create(dir.path()).expect("reopen index");
    index_again.commit().unwrap();

    assert!(
        sentinel.exists(),
        "sentinel file should remain when schema hash matches (no rebuild)"
    );
}

#[test]
fn open_or_create_rebuilds_on_schema_mismatch() {
    let dir = TempDir::new().unwrap();

    // Seed directory with mismatched schema hash and a sentinel.
    fs::write(
        dir.path().join("schema_hash.json"),
        r#"{"schema_hash":"old-hash"}"#,
    )
    .unwrap();
    let sentinel = dir.path().join("sentinel.txt");
    fs::write(&sentinel, b"remove-me").unwrap();

    let mut index = TantivyIndex::open_or_create(dir.path()).expect("recreate index");
    index.commit().unwrap();

    // Directory was rebuilt, so sentinel should be gone and schema hash should be updated.
    assert!(
        !sentinel.exists(),
        "sentinel should be removed when index is rebuilt due to hash mismatch"
    );
    let schema_file = read_schema_hash(&dir);
    assert!(
        schema_file.contains(SCHEMA_HASH),
        "schema hash should be refreshed after rebuild"
    );
}

#[test]
fn optimize_if_idle_triggers_when_segment_threshold_met() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).expect("create index");

    // Create multiple tiny conversations and commit after each to generate multiple segments.
    for i in 0..5 {
        let conv = NormalizedConversation {
            agent_slug: "codex".into(),
            external_id: Some(format!("conv-{i}")),
            title: Some(format!("Conv {i}")),
            workspace: None,
            source_path: dir.path().join(format!("conv-{i}.jsonl")),
            started_at: Some(i),
            ended_at: Some(i),
            metadata: json!({}),
            messages: vec![NormalizedMessage {
                idx: 0,
                role: "user".into(),
                author: Some("user".into()),
                created_at: Some(i),
                content: format!("hello-{i}"),
                extra: json!({}),
                snippets: Vec::new(),
                invocations: Vec::new(),
            }],
        };
        index.add_conversation(&conv).expect("add conv");
        index.commit().expect("commit");
    }

    let pre_segments = index.segment_count();

    // Tantivy's merge policy may eagerly merge; only require trigger when threshold is met.
    let merged = index.optimize_if_idle().expect("optimize_if_idle");
    if pre_segments >= 4 {
        assert!(
            merged,
            "optimize_if_idle should trigger when segment threshold is met"
        );
    } else {
        assert!(
            !merged,
            "optimize_if_idle may skip when segments below threshold; got {pre_segments}"
        );
    }
}

#[test]
fn incremental_commit_preserves_existing_docs() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).expect("create index");

    // First conversation
    let conv_a = NormalizedConversation {
        agent_slug: "codex".into(),
        external_id: Some("a".into()),
        title: Some("First".into()),
        workspace: None,
        source_path: dir.path().join("a.jsonl"),
        started_at: Some(1),
        ended_at: Some(1),
        metadata: json!({}),
        messages: vec![NormalizedMessage {
            idx: 0,
            role: "user".into(),
            author: Some("u".into()),
            created_at: Some(1),
            content: "first message".into(),
            extra: json!({}),
            snippets: Vec::new(),
            invocations: Vec::new(),
        }],
    };
    index.add_conversation(&conv_a).expect("add conv a");
    index.commit().expect("commit a");

    let reader = index.reader().expect("reader");
    let initial_docs = reader.doc_count().expect("doc count");
    assert_eq!(initial_docs, 1, "one doc after first commit");

    // Second conversation: incremental add, new commit should preserve prior doc
    let conv_b = NormalizedConversation {
        agent_slug: "codex".into(),
        external_id: Some("b".into()),
        title: Some("Second".into()),
        workspace: None,
        source_path: dir.path().join("b.jsonl"),
        started_at: Some(2),
        ended_at: Some(2),
        metadata: json!({}),
        messages: vec![NormalizedMessage {
            idx: 0,
            role: "assistant".into(),
            author: Some("u".into()),
            created_at: Some(2),
            content: "second message".into(),
            extra: json!({}),
            snippets: Vec::new(),
            invocations: Vec::new(),
        }],
    };
    index.add_conversation(&conv_b).expect("add conv b");
    index.commit().expect("commit b");

    let reader2 = index.reader().expect("reader2");
    let docs_after = reader2.doc_count().expect("doc count");
    assert_eq!(
        docs_after, 2,
        "incremental commit should retain existing docs and add new ones"
    );
}
