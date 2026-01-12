use coding_agent_search::connectors::{
    NormalizedConversation, NormalizedMessage, NormalizedSnippet,
};
use coding_agent_search::indexer::persist;
use coding_agent_search::search::query::{FieldMask, SearchClient, SearchFilters};
use coding_agent_search::search::tantivy::TantivyIndex;
use coding_agent_search::storage::sqlite::SqliteStorage;
use criterion::{Criterion, criterion_group, criterion_main};
use tempfile::TempDir;

fn build_small_index() -> (TempDir, SearchClient) {
    let dir = TempDir::new().expect("tmp");
    let data_dir = dir.path().to_path_buf();
    let db_path = data_dir.join("agent_search.db");
    let mut storage = SqliteStorage::open(&db_path).expect("storage");
    let mut index = TantivyIndex::open_or_create(
        &coding_agent_search::search::tantivy::index_dir(&data_dir).unwrap(),
    )
    .expect("index");

    let conv = NormalizedConversation {
        agent_slug: "codex".into(),
        external_id: None,
        title: Some("hello".into()),
        workspace: None,
        source_path: data_dir.join("rollout-1.jsonl"),
        started_at: Some(1),
        ended_at: None,
        metadata: serde_json::json!({}),
        messages: vec![
            NormalizedMessage {
                idx: 0,
                role: "user".into(),
                author: None,
                created_at: Some(1),
                content: "alpha beta gamma".into(),
                extra: serde_json::json!({}),
                snippets: vec![NormalizedSnippet {
                    file_path: None,
                    start_line: None,
                    end_line: None,
                    language: None,
                    snippet_text: None,
                }],
            },
            NormalizedMessage {
                idx: 1,
                role: "assistant".into(),
                author: None,
                created_at: Some(2),
                content: "delta epsilon zeta".into(),
                extra: serde_json::json!({}),
                snippets: vec![],
            },
        ],
    };

    persist::persist_conversation(&mut storage, &mut index, &conv).expect("persist");
    index.commit().expect("commit");

    let client = SearchClient::open(
        &coding_agent_search::search::tantivy::index_dir(&data_dir).unwrap(),
        Some(&db_path),
    )
    .expect("open")
    .expect("present");

    (dir, client)
}

fn bench_cache_hits(c: &mut Criterion) {
    let (_dir, client) = build_small_index();
    let filters = SearchFilters::default();

    c.bench_function("cache_prefix_hit", |b| {
        // warm cache
        let _ = client
            .search("alp", filters.clone(), 10, 0, FieldMask::FULL)
            .unwrap();
        b.iter(|| {
            client
                .search("alp", filters.clone(), 10, 0, FieldMask::FULL)
                .expect("search")
        });
    });
}

/// Benchmark simulating rapid forward typing: a → al → alp → alph → alpha
fn bench_typing_forward(c: &mut Criterion) {
    let (_dir, client) = build_small_index();
    let filters = SearchFilters::default();
    let prefixes = ["a", "al", "alp", "alph", "alpha"];

    c.bench_function("typing_forward_5char", |b| {
        b.iter(|| {
            for prefix in &prefixes {
                let _ = client.search(prefix, filters.clone(), 10, 0, FieldMask::FULL);
            }
        });
    });
}

/// Benchmark simulating backspace pattern: alpha → alph → alp → al → a
fn bench_typing_backspace(c: &mut Criterion) {
    let (_dir, client) = build_small_index();
    let filters = SearchFilters::default();
    let prefixes = ["alpha", "alph", "alp", "al", "a"];

    // Warm cache with forward pass first
    for prefix in &["a", "al", "alp", "alph", "alpha"] {
        let _ = client.search(prefix, filters.clone(), 10, 0, FieldMask::FULL);
    }

    c.bench_function("typing_backspace_5char", |b| {
        b.iter(|| {
            for prefix in &prefixes {
                let _ = client.search(prefix, filters.clone(), 10, 0, FieldMask::FULL);
            }
        });
    });
}

/// Benchmark rapid keystroke simulation (mixed typing pattern)
fn bench_rapid_keystroke_mixed(c: &mut Criterion) {
    let (_dir, client) = build_small_index();
    let filters = SearchFilters::default();
    // Simulate: type "del", backspace to "de", continue to "delta"
    let sequence = ["d", "de", "del", "de", "del", "delt", "delta"];

    c.bench_function("rapid_keystroke_mixed_7", |b| {
        b.iter(|| {
            for query in &sequence {
                let _ = client.search(query, filters.clone(), 10, 0, FieldMask::FULL);
            }
        });
    });
}

/// Benchmark cache miss (cold query)
fn bench_cache_miss(c: &mut Criterion) {
    let (_dir, client) = build_small_index();
    let filters = SearchFilters::default();

    c.bench_function("cache_cold_query", |b| {
        let mut counter = 0u64;
        b.iter(|| {
            // Each iteration uses a unique query to avoid cache hits
            counter += 1;
            let query = format!("unique{counter}");
            let _ = client.search(&query, filters.clone(), 10, 0, FieldMask::FULL);
        });
    });
}

/// Benchmark with agent filter applied
fn bench_filtered_search(c: &mut Criterion) {
    let (_dir, client) = build_small_index();
    let mut filters = SearchFilters::default();
    filters.agents.insert("codex".into());

    c.bench_function("search_with_agent_filter", |b| {
        // warm cache
        let _ = client
            .search("alp", filters.clone(), 10, 0, FieldMask::FULL)
            .unwrap();
        b.iter(|| {
            client
                .search("alp", filters.clone(), 10, 0, FieldMask::FULL)
                .expect("search")
        });
    });
}

criterion_group!(
    benches,
    bench_cache_hits,
    bench_typing_forward,
    bench_typing_backspace,
    bench_rapid_keystroke_mixed,
    bench_cache_miss,
    bench_filtered_search
);
criterion_main!(benches);
