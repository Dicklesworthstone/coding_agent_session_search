//! Database performance benchmarks for cass.
//!
//! Benchmarks for:
//! - SQLite open/close operations
//! - FTS5 query performance
//! - Conversation/message insertion
//! - Result pagination
//! - Daily statistics queries
//!
//! Run with:
//!   cargo bench --bench db_perf
//!
//! Performance targets:
//! | Operation | Target | Corpus |
//! |-----------|--------|--------|
//! | DB open | < 100ms | Any |
//! | FTS search | < 100ms | 10K+ rows |
//! | Insert conversation | < 10ms | Per conversation |
//! | Pagination (100 results) | < 50ms | 10K+ results |

use coding_agent_search::connectors::{NormalizedConversation, NormalizedMessage};
use coding_agent_search::indexer::persist::persist_conversation;
use coding_agent_search::search::tantivy::{index_dir, TantivyIndex};
use coding_agent_search::storage::sqlite::SqliteStorage;
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::hint::black_box;
use std::path::PathBuf;
use tempfile::TempDir;

// =============================================================================
// Test Data Generation
// =============================================================================

/// Generate a test conversation with specified message count.
fn generate_conversation(conv_id: i64, msg_count: i64) -> NormalizedConversation {
    let base_ts = 1_700_000_000_000 + conv_id * 100_000;
    let messages: Vec<NormalizedMessage> = (0..msg_count)
        .map(|m| NormalizedMessage {
            idx: m,
            role: if m % 2 == 0 { "user" } else { "agent" }.into(),
            author: Some(format!("model-{}", conv_id % 5)),
            created_at: Some(base_ts + m * 1000),
            content: format!(
                "Conversation {} message {}: Lorem ipsum dolor sit amet, \
                 consectetur adipiscing elit. Testing database performance \
                 with various search terms like rust, python, javascript.",
                conv_id, m
            ),
            extra: serde_json::json!({ "bench": true }),
            snippets: Vec::new(),
        })
        .collect();

    NormalizedConversation {
        agent_slug: format!("bench-agent-{}", conv_id % 10),
        external_id: Some(format!("bench-conv-{}", conv_id)),
        title: Some(format!("Benchmark Conversation {}", conv_id)),
        workspace: Some(PathBuf::from(format!(
            "/workspace/project-{}",
            conv_id % 20
        ))),
        source_path: PathBuf::from(format!("/tmp/bench/conv-{}.jsonl", conv_id)),
        started_at: Some(base_ts),
        ended_at: Some(base_ts + msg_count * 1000),
        metadata: serde_json::json!({ "bench": true }),
        messages,
    }
}

/// Set up a test database with specified conversation count.
fn setup_test_db(conv_count: i64, msgs_per_conv: i64) -> (TempDir, SqliteStorage) {
    let temp = TempDir::new().expect("create tempdir");
    let db_path = temp.path().join("bench.db");
    let index_path = index_dir(temp.path()).expect("index path");

    let mut storage = SqliteStorage::open(&db_path).expect("open db");
    let mut t_index = TantivyIndex::open_or_create(&index_path).unwrap();

    for i in 0..conv_count {
        let conv = generate_conversation(i, msgs_per_conv);
        persist_conversation(&mut storage, &mut t_index, &conv).expect("persist");
    }
    t_index.commit().unwrap();

    (temp, storage)
}

// =============================================================================
// Database Open/Close Benchmarks
// =============================================================================

/// Benchmark database open time.
fn bench_db_open(c: &mut Criterion) {
    let temp = TempDir::new().expect("create tempdir");
    let db_path = temp.path().join("bench_open.db");

    // Create initial database
    {
        let storage = SqliteStorage::open(&db_path).expect("open db");
        drop(storage);
    }

    c.bench_function("db_open", |b| {
        b.iter(|| {
            let storage = SqliteStorage::open(&db_path).expect("open db");
            black_box(storage)
        })
    });
}

/// Benchmark database open with data.
fn bench_db_open_with_data(c: &mut Criterion) {
    let (temp, _storage) = setup_test_db(1000, 10);
    let db_path = temp.path().join("bench.db");

    c.bench_function("db_open_with_1k_convs", |b| {
        b.iter(|| {
            let storage = SqliteStorage::open(&db_path).expect("open db");
            black_box(storage)
        })
    });
}

/// Benchmark readonly database open.
fn bench_db_open_readonly(c: &mut Criterion) {
    let (temp, _storage) = setup_test_db(1000, 10);
    let db_path = temp.path().join("bench.db");

    c.bench_function("db_open_readonly", |b| {
        b.iter(|| {
            let storage = SqliteStorage::open_readonly(&db_path).expect("open db");
            black_box(storage)
        })
    });
}

// =============================================================================
// Insertion Benchmarks
// =============================================================================

/// Benchmark single conversation insertion.
fn bench_insert_conversation(c: &mut Criterion) {
    let mut group = c.benchmark_group("insert_conversation");

    for &msg_count in &[5i64, 20, 50, 100] {
        group.throughput(Throughput::Elements(msg_count as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{}_msgs", msg_count)),
            &msg_count,
            |b, &msg_count| {
                let temp = TempDir::new().expect("create tempdir");
                let db_path = temp.path().join("bench.db");
                let index_path = index_dir(temp.path()).expect("index path");
                let mut storage = SqliteStorage::open(&db_path).expect("open db");
                let mut t_index = TantivyIndex::open_or_create(&index_path).unwrap();
                let mut conv_id = 0i64;

                b.iter(|| {
                    let conv = generate_conversation(conv_id, msg_count);
                    persist_conversation(&mut storage, &mut t_index, &conv).expect("persist");
                    conv_id += 1;
                })
            },
        );
    }

    group.finish();
}

/// Benchmark batch conversation insertion.
fn bench_insert_batch(c: &mut Criterion) {
    let mut group = c.benchmark_group("insert_batch");
    group.sample_size(20);

    for &batch_size in &[10usize, 50, 100] {
        group.throughput(Throughput::Elements(batch_size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{}_convs", batch_size)),
            &batch_size,
            |b, &batch_size| {
                let temp = TempDir::new().expect("create tempdir");
                let db_path = temp.path().join("bench.db");
                let index_path = index_dir(temp.path()).expect("index path");
                let mut storage = SqliteStorage::open(&db_path).expect("open db");
                let mut t_index = TantivyIndex::open_or_create(&index_path).unwrap();
                let mut base_id = 0i64;

                b.iter(|| {
                    for i in 0..batch_size as i64 {
                        let conv = generate_conversation(base_id + i, 10);
                        persist_conversation(&mut storage, &mut t_index, &conv).expect("persist");
                    }
                    t_index.commit().unwrap();
                    base_id += batch_size as i64;
                })
            },
        );
    }

    group.finish();
}

// =============================================================================
// Query Benchmarks
// =============================================================================

/// Benchmark listing conversations with pagination.
fn bench_list_conversations(c: &mut Criterion) {
    let (temp, storage) = setup_test_db(5000, 10);

    let mut group = c.benchmark_group("list_conversations");

    for &limit in &[25i64, 100, 500] {
        group.throughput(Throughput::Elements(limit as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("limit_{}", limit)),
            &limit,
            |b, &limit| {
                b.iter(|| {
                    let results = storage.list_conversations(limit, 0).expect("list");
                    black_box(results)
                })
            },
        );
    }

    // Test pagination (offset performance)
    for &offset in &[0i64, 1000, 4000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("offset_{}", offset)),
            &offset,
            |b, &offset| {
                b.iter(|| {
                    let results = storage.list_conversations(100, offset).expect("list");
                    black_box(results)
                })
            },
        );
    }

    group.finish();
    drop(temp);
}

/// Benchmark fetching messages for a conversation.
fn bench_fetch_messages(c: &mut Criterion) {
    // Create conversations with different message counts
    let temp = TempDir::new().expect("create tempdir");
    let db_path = temp.path().join("bench.db");
    let index_path = index_dir(temp.path()).expect("index path");
    let mut storage = SqliteStorage::open(&db_path).expect("open db");
    let mut t_index = TantivyIndex::open_or_create(&index_path).unwrap();

    // Create conversations with varying message counts
    let msg_counts = [10i64, 50, 100, 500];
    for (i, &msg_count) in msg_counts.iter().enumerate() {
        let conv = generate_conversation(i as i64, msg_count);
        persist_conversation(&mut storage, &mut t_index, &conv).expect("persist");
    }
    t_index.commit().unwrap();

    let mut group = c.benchmark_group("fetch_messages");

    for (conv_id, &msg_count) in msg_counts.iter().enumerate() {
        group.throughput(Throughput::Elements(msg_count as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{}_msgs", msg_count)),
            &conv_id,
            |b, &conv_id| {
                b.iter(|| {
                    let messages = storage
                        .fetch_messages((conv_id + 1) as i64) // SQLite IDs start at 1
                        .expect("fetch");
                    black_box(messages)
                })
            },
        );
    }

    group.finish();
}

/// Benchmark listing agents.
fn bench_list_agents(c: &mut Criterion) {
    let (temp, storage) = setup_test_db(1000, 10);

    c.bench_function("list_agents", |b| {
        b.iter(|| {
            let agents = storage.list_agents().expect("list");
            black_box(agents)
        })
    });

    drop(temp);
}

/// Benchmark listing workspaces.
fn bench_list_workspaces(c: &mut Criterion) {
    let (temp, storage) = setup_test_db(1000, 10);

    c.bench_function("list_workspaces", |b| {
        b.iter(|| {
            let workspaces = storage.list_workspaces().expect("list");
            black_box(workspaces)
        })
    });

    drop(temp);
}

// =============================================================================
// FTS Benchmarks
// =============================================================================

/// Benchmark FTS rebuild.
fn bench_fts_rebuild(c: &mut Criterion) {
    let mut group = c.benchmark_group("fts_rebuild");
    group.sample_size(10);

    for &conv_count in &[100i64, 500, 1000] {
        let (temp, mut storage) = setup_test_db(conv_count, 10);

        group.throughput(Throughput::Elements(conv_count as u64 * 10)); // total messages
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{}_convs", conv_count)),
            &conv_count,
            |b, _| {
                b.iter(|| {
                    storage.rebuild_fts().expect("rebuild");
                })
            },
        );

        drop(temp);
    }

    group.finish();
}

// =============================================================================
// Statistics Benchmarks
// =============================================================================

/// Benchmark daily histogram query.
fn bench_daily_histogram(c: &mut Criterion) {
    let (temp, storage) = setup_test_db(2000, 10);

    c.bench_function("daily_histogram_30_days", |b| {
        // Query for last 30 days
        let end_ts = 1_700_000_000_000i64 + 2000 * 100_000;
        let start_ts = end_ts - (30 * 24 * 60 * 60 * 1000);

        b.iter(|| {
            let histogram = storage
                .get_daily_histogram(start_ts, end_ts, None, None)
                .expect("histogram");
            black_box(histogram)
        })
    });

    drop(temp);
}

/// Benchmark session count in range.
fn bench_session_count_range(c: &mut Criterion) {
    let (temp, storage) = setup_test_db(2000, 10);

    c.bench_function("session_count_range", |b| {
        let end_ts = 1_700_000_000_000i64 + 2000 * 100_000;
        let start_ts = end_ts - (30 * 24 * 60 * 60 * 1000);

        b.iter(|| {
            let count = storage
                .count_sessions_in_range(Some(start_ts), Some(end_ts), None, None)
                .expect("count");
            black_box(count)
        })
    });

    drop(temp);
}

// =============================================================================
// Scaling Benchmarks
// =============================================================================

/// Benchmark database performance scaling with corpus size.
fn bench_db_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("db_scaling");
    group.sample_size(20);

    for &conv_count in &[100i64, 500, 1000, 2500] {
        let (temp, storage) = setup_test_db(conv_count, 10);

        group.throughput(Throughput::Elements(conv_count as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{}_convs", conv_count)),
            &conv_count,
            |b, _| {
                b.iter(|| {
                    // Combined operation: list + fetch
                    let convs = storage.list_conversations(25, 0).expect("list");
                    if let Some(conv) = convs.first()
                        && let Some(id) = conv.id
                    {
                        let _ = storage.fetch_messages(id);
                    }
                    black_box(convs)
                })
            },
        );

        drop(temp);
    }

    group.finish();
}

// =============================================================================
// Criterion Configuration
// =============================================================================

criterion_group!(
    open_benches,
    bench_db_open,
    bench_db_open_with_data,
    bench_db_open_readonly
);

criterion_group!(
    insert_benches,
    bench_insert_conversation,
    bench_insert_batch
);

criterion_group!(
    query_benches,
    bench_list_conversations,
    bench_fetch_messages,
    bench_list_agents,
    bench_list_workspaces
);

criterion_group!(fts_benches, bench_fts_rebuild);

criterion_group!(
    stats_benches,
    bench_daily_histogram,
    bench_session_count_range
);

criterion_group!(scaling_benches, bench_db_scaling);

criterion_main!(
    open_benches,
    insert_benches,
    query_benches,
    fts_benches,
    stats_benches,
    scaling_benches
);
