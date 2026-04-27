//! Integration regression benchmarks for cass ingestion/storage paths.
//!
//! `SqliteStorage` is now a compatibility alias for `FrankenStorage`, so this
//! benchmark suite no longer compares rusqlite/legacy SQLite against
//! frankensqlite. Instead it compares two distinct cass-owned paths:
//! - `persist_conversation`: normalized ingestion + lexical index updates
//! - direct `FrankenStorage` calls: pre-resolved IDs + storage-only writes
//!
//! The intent is to quantify cass application-path overheads honestly. Engine-
//! level comparison against legacy C SQLite must use a separate harness.
//!
//! Bead: coding_agent_session_search-9ma8q
//!
//! Run with:
//!   cargo bench --bench integration_regression

mod bench_utils;

use bench_utils::configure_criterion;
use coding_agent_search::connectors::{NormalizedConversation, NormalizedMessage};
use coding_agent_search::indexer::persist::persist_conversation;
use coding_agent_search::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
use coding_agent_search::search::tantivy::{TantivyIndex, index_dir};
use coding_agent_search::storage::sqlite::{
    ConnectionManagerConfig, FrankenConnectionManager, FrankenStorage,
};
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankensqlite::FrankenError;
use std::hint::black_box;
use std::path::PathBuf;
use std::time::Duration;
use tempfile::TempDir;

// =============================================================================
// Test Data Generation
// =============================================================================

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
            invocations: Vec::new(),
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

fn generate_remote_conversation(conv_id: i64, msg_count: i64) -> NormalizedConversation {
    let mut conv = generate_conversation(conv_id, msg_count);
    conv.metadata = serde_json::json!({
        "bench": true,
        "cass": {
            "origin": {
                "source_id": "bench-remote-source",
                "host": "bench-remote-host"
            }
        }
    });
    conv
}

fn make_conversation(conv_id: i64, msg_count: i64) -> Conversation {
    let base_ts = 1_700_000_000_000 + conv_id * 100_000;
    let messages: Vec<Message> = (0..msg_count)
        .map(|m| Message {
            id: None,
            idx: m,
            role: if m % 2 == 0 {
                MessageRole::User
            } else {
                MessageRole::Agent
            },
            author: Some(format!("model-{}", conv_id % 5)),
            created_at: Some(base_ts + m * 1000),
            content: format!(
                "Conversation {} message {}: Lorem ipsum dolor sit amet, \
                 consectetur adipiscing elit.",
                conv_id, m
            ),
            extra_json: serde_json::json!({ "bench": true }),
            snippets: Vec::new(),
        })
        .collect();

    Conversation {
        id: None,
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
        approx_tokens: Some(msg_count * 50),
        metadata_json: serde_json::json!({ "bench": true }),
        messages,
        source_id: "local".into(),
        origin_host: None,
    }
}

fn make_remote_conversation(conv_id: i64, msg_count: i64) -> Conversation {
    let mut conv = make_conversation(conv_id, msg_count);
    conv.source_id = "bench-remote-source".into();
    conv.origin_host = Some("bench-remote-host".into());
    conv
}

fn make_agent(id: i64) -> Agent {
    Agent {
        id: None,
        slug: format!("bench-agent-{}", id % 10),
        name: format!("Bench Agent {}", id % 10),
        version: None,
        kind: AgentKind::Cli,
    }
}

// =============================================================================
// Setup helpers
// =============================================================================

/// Seed a database through the normalized `persist_conversation` pipeline.
fn setup_persist_seeded_db(conv_count: i64, msgs_per_conv: i64) -> (TempDir, FrankenStorage) {
    let temp = TempDir::new().expect("create tempdir");
    let db_path = temp.path().join("persist.db");
    let index_path = index_dir(temp.path()).expect("index path");

    let storage = FrankenStorage::open(&db_path).expect("open persist-seeded db");
    let mut t_index = TantivyIndex::open_or_create(&index_path).unwrap();

    for i in 0..conv_count {
        let conv = generate_conversation(i, msgs_per_conv);
        persist_conversation(&storage, &mut t_index, &conv).expect("persist");
    }
    t_index.commit().unwrap();

    (temp, storage)
}

/// Seed a database through direct `FrankenStorage` calls only.
fn setup_direct_seeded_db(conv_count: i64, msgs_per_conv: i64) -> (TempDir, FrankenStorage) {
    let temp = TempDir::new().expect("create tempdir");
    let db_path = temp.path().join("direct.db");

    let fs = FrankenStorage::open(&db_path).expect("open direct-seeded db");

    // Pre-create all agents and workspaces to avoid ON CONFLICT overhead
    let mut agent_ids = std::collections::HashMap::new();
    let mut ws_ids = std::collections::HashMap::new();

    for i in 0..10i64 {
        let agent = make_agent(i);
        let agent_id = fs.ensure_agent(&agent).expect("ensure agent");
        agent_ids.insert(i, agent_id);
    }
    for i in 0..20i64 {
        let ws_path = PathBuf::from(format!("/workspace/project-{}", i));
        let ws_id = fs
            .ensure_workspace(&ws_path, None)
            .expect("ensure workspace");
        ws_ids.insert(i, ws_id);
    }

    for i in 0..conv_count {
        let agent_id = agent_ids[&(i % 10)];
        let ws_id = ws_ids[&(i % 20)];
        let conv = make_conversation(i, msgs_per_conv);
        fs.insert_conversation_tree(agent_id, Some(ws_id), &conv)
            .expect("insert conversation");
    }

    (temp, fs)
}

// =============================================================================
// 1. DATABASE OPEN BENCHMARKS
// =============================================================================

fn bench_db_open_comparison(c: &mut Criterion) {
    let mut group = c.benchmark_group("db_open");

    // Empty database
    let empty_temp = TempDir::new().unwrap();
    let empty_path = empty_temp.path().join("empty.db");
    {
        FrankenStorage::open(&empty_path).unwrap();
    }

    group.bench_function("empty_db", |b| {
        b.iter(|| black_box(FrankenStorage::open(&empty_path).unwrap()))
    });

    // Databases with 100 conversations (reduced for benchmark setup speed).
    let (persist_data_temp, _) = setup_persist_seeded_db(100, 10);
    let persist_data_path = persist_data_temp.path().join("persist.db");

    let (direct_data_temp, _) = setup_direct_seeded_db(100, 10);
    let direct_data_path = direct_data_temp.path().join("direct.db");

    group.bench_function("persist_seeded_100_convs", |b| {
        b.iter(|| black_box(FrankenStorage::open(&persist_data_path).unwrap()))
    });

    group.bench_function("direct_seeded_100_convs", |b| {
        b.iter(|| black_box(FrankenStorage::open(&direct_data_path).unwrap()))
    });

    group.finish();
}

// =============================================================================
// 2. BULK INSERT BENCHMARKS
// =============================================================================

fn bench_insert_comparison(c: &mut Criterion) {
    let mut group = c.benchmark_group("insert_conversation");
    group.sample_size(20);

    for &msg_count in &[5i64, 20, 50] {
        group.throughput(Throughput::Elements(msg_count as u64));

        // Normalized ingest pipeline: adapter + storage + lexical index update.
        group.bench_with_input(
            BenchmarkId::new("persist", format!("{msg_count}_msgs")),
            &msg_count,
            |b, &msg_count| {
                let temp = TempDir::new().unwrap();
                let db_path = temp.path().join("bench.db");
                let index_path = index_dir(temp.path()).expect("index path");
                let storage = FrankenStorage::open(&db_path).unwrap();
                let mut t_index = TantivyIndex::open_or_create(&index_path).unwrap();
                let mut conv_id = 0i64;

                b.iter(|| {
                    let conv = generate_conversation(conv_id, msg_count);
                    persist_conversation(&storage, &mut t_index, &conv).unwrap();
                    conv_id += 1;
                })
            },
        );

        // Direct storage path: pre-resolved IDs + storage-only write.
        group.bench_with_input(
            BenchmarkId::new("direct", format!("{msg_count}_msgs")),
            &msg_count,
            |b, &msg_count| {
                let temp = TempDir::new().unwrap();
                let db_path = temp.path().join("bench.db");
                let fs = FrankenStorage::open(&db_path).unwrap();

                // Pre-create agents/workspaces (avoid repeated ON CONFLICT)
                let mut agent_ids = Vec::new();
                let mut ws_ids = Vec::new();
                for i in 0..10i64 {
                    agent_ids.push(fs.ensure_agent(&make_agent(i)).unwrap());
                }
                for i in 0..20i64 {
                    let ws_path = PathBuf::from(format!("/workspace/project-{}", i));
                    ws_ids.push(fs.ensure_workspace(&ws_path, None).unwrap());
                }

                let mut conv_id = 0i64;
                b.iter(|| {
                    let agent_id = agent_ids[(conv_id % 10) as usize];
                    let ws_id = ws_ids[(conv_id % 20) as usize];
                    let conv = make_conversation(conv_id, msg_count);
                    black_box(
                        fs.insert_conversation_tree(agent_id, Some(ws_id), &conv)
                            .unwrap(),
                    );
                    conv_id += 1;
                })
            },
        );
    }

    group.finish();
}

fn bench_insert_remote_source_reuse(c: &mut Criterion) {
    let mut group = c.benchmark_group("insert_remote_source_reuse");
    group.sample_size(20);

    for &msg_count in &[5i64, 20, 50] {
        group.throughput(Throughput::Elements(msg_count as u64));

        // Warm each database once so the measured path reflects steady-state
        // reuse of a single remote provenance source across many inserts.
        group.bench_with_input(
            BenchmarkId::new("persist", format!("{msg_count}_msgs")),
            &msg_count,
            |b, &msg_count| {
                let temp = TempDir::new().unwrap();
                let db_path = temp.path().join("bench.db");
                let index_path = index_dir(temp.path()).expect("index path");
                let storage = FrankenStorage::open(&db_path).unwrap();
                let mut t_index = TantivyIndex::open_or_create(&index_path).unwrap();

                persist_conversation(
                    &storage,
                    &mut t_index,
                    &generate_remote_conversation(0, msg_count),
                )
                .unwrap();

                let mut conv_id = 1i64;
                b.iter(|| {
                    let conv = generate_remote_conversation(conv_id, msg_count);
                    persist_conversation(&storage, &mut t_index, &conv).unwrap();
                    conv_id += 1;
                })
            },
        );

        group.bench_with_input(
            BenchmarkId::new("direct", format!("{msg_count}_msgs")),
            &msg_count,
            |b, &msg_count| {
                let temp = TempDir::new().unwrap();
                let db_path = temp.path().join("bench.db");
                let fs = FrankenStorage::open(&db_path).unwrap();

                let mut agent_ids = Vec::new();
                let mut ws_ids = Vec::new();
                for i in 0..10i64 {
                    agent_ids.push(fs.ensure_agent(&make_agent(i)).unwrap());
                }
                for i in 0..20i64 {
                    let ws_path = PathBuf::from(format!("/workspace/project-{}", i));
                    ws_ids.push(fs.ensure_workspace(&ws_path, None).unwrap());
                }

                let warmup = make_remote_conversation(0, msg_count);
                fs.insert_conversation_tree(agent_ids[0], Some(ws_ids[0]), &warmup)
                    .unwrap();

                let mut conv_id = 1i64;
                b.iter(|| {
                    let agent_id = agent_ids[(conv_id % 10) as usize];
                    let ws_id = ws_ids[(conv_id % 20) as usize];
                    let conv = make_remote_conversation(conv_id, msg_count);
                    black_box(
                        fs.insert_conversation_tree(agent_id, Some(ws_id), &conv)
                            .unwrap(),
                    );
                    conv_id += 1;
                })
            },
        );
    }

    group.finish();
}

fn bench_append_remote_source_merge(c: &mut Criterion) {
    let mut group = c.benchmark_group("append_remote_source_merge");
    group.sample_size(20);

    const WORKLOAD_CONVERSATIONS: i64 = 600;

    for &msg_count in &[5i64, 20, 50] {
        group.throughput(Throughput::Elements(msg_count as u64));

        group.bench_with_input(
            BenchmarkId::new("persist", format!("{msg_count}_msgs")),
            &msg_count,
            |b, &msg_count| {
                let temp = TempDir::new().unwrap();
                let db_path = temp.path().join("bench.db");
                let index_path = index_dir(temp.path()).expect("index path");
                let storage = FrankenStorage::open(&db_path).unwrap();
                let mut t_index = TantivyIndex::open_or_create(&index_path).unwrap();

                for conv_id in 0..WORKLOAD_CONVERSATIONS {
                    persist_conversation(
                        &storage,
                        &mut t_index,
                        &generate_remote_conversation(conv_id, msg_count),
                    )
                    .unwrap();
                }
                t_index.commit().unwrap();

                let mut conv_id = 0i64;
                b.iter(|| {
                    let scenario_id = conv_id % WORKLOAD_CONVERSATIONS;
                    let conv = generate_remote_conversation(scenario_id, msg_count * 2);
                    persist_conversation(&storage, &mut t_index, &conv).unwrap();
                    conv_id += 1;
                })
            },
        );

        group.bench_with_input(
            BenchmarkId::new("direct", format!("{msg_count}_msgs")),
            &msg_count,
            |b, &msg_count| {
                let temp = TempDir::new().unwrap();
                let db_path = temp.path().join("bench.db");
                let fs = FrankenStorage::open(&db_path).unwrap();

                let mut agent_ids = Vec::new();
                let mut ws_ids = Vec::new();
                for i in 0..10i64 {
                    agent_ids.push(fs.ensure_agent(&make_agent(i)).unwrap());
                }
                for i in 0..20i64 {
                    let ws_path = PathBuf::from(format!("/workspace/project-{}", i));
                    ws_ids.push(fs.ensure_workspace(&ws_path, None).unwrap());
                }

                for conv_id in 0..WORKLOAD_CONVERSATIONS {
                    let agent_id = agent_ids[(conv_id % 10) as usize];
                    let ws_id = ws_ids[(conv_id % 20) as usize];
                    let base = make_remote_conversation(conv_id, msg_count);
                    fs.insert_conversation_tree(agent_id, Some(ws_id), &base)
                        .unwrap();
                }

                let mut conv_id = 0i64;
                b.iter(|| {
                    let scenario_id = conv_id % WORKLOAD_CONVERSATIONS;
                    let agent_id = agent_ids[(scenario_id % 10) as usize];
                    let ws_id = ws_ids[(scenario_id % 20) as usize];
                    let conv = make_remote_conversation(scenario_id, msg_count * 2);
                    black_box(
                        fs.insert_conversation_tree(agent_id, Some(ws_id), &conv)
                            .unwrap(),
                    );
                    conv_id += 1;
                })
            },
        );
    }

    group.finish();
}

// =============================================================================
// 3. QUERY BENCHMARKS
// =============================================================================

fn bench_query_comparison(c: &mut Criterion) {
    let mut group = c.benchmark_group("query_ops");

    // Setup: both datasets contain 100 conversations × 10 messages, but were
    // produced by different cass-owned ingestion paths.
    let (_persist_temp, persist_storage) = setup_persist_seeded_db(100, 10);
    let (_direct_temp, direct_storage) = setup_direct_seeded_db(100, 10);

    // list_agents
    group.bench_function("persist_seeded_list_agents", |b| {
        b.iter(|| black_box(persist_storage.list_agents().unwrap()))
    });
    group.bench_function("direct_seeded_list_agents", |b| {
        b.iter(|| black_box(direct_storage.list_agents().unwrap()))
    });

    // list_conversations (paginated)
    group.bench_function("persist_seeded_list_convs_50", |b| {
        b.iter(|| black_box(persist_storage.list_conversations(50, 0).unwrap()))
    });
    group.bench_function("direct_seeded_list_convs_50", |b| {
        b.iter(|| black_box(direct_storage.list_conversations(50, 0).unwrap()))
    });

    // fetch_messages for first conversation
    let persist_convs = persist_storage.list_conversations(1, 0).unwrap();
    let direct_conv_id = direct_storage
        .list_conversations(1, 0)
        .unwrap()
        .first()
        .and_then(|conv| conv.id);

    if let Some(sc) = persist_convs.first() {
        let sid = sc.id.unwrap();
        group.bench_function("persist_seeded_fetch_messages", |b| {
            b.iter(|| black_box(persist_storage.fetch_messages(sid).unwrap()))
        });
    }
    if let Some(fid) = direct_conv_id {
        group.bench_function("direct_seeded_fetch_messages", |b| {
            b.iter(|| black_box(direct_storage.fetch_messages(fid).unwrap()))
        });
    }

    // count_sessions_in_range (aggregate) on a persist-seeded dataset.
    group.bench_function("persist_seeded_count_sessions", |b| {
        b.iter(|| {
            black_box(
                persist_storage
                    .count_sessions_in_range(None, None, None, None)
                    .unwrap(),
            )
        })
    });
    // Raw COUNT(*) on a direct-seeded dataset gives a storage-floor reference
    // for the same archive shape without extra aggregate logic above it.
    group.bench_function("direct_seeded_raw_count", |b| {
        b.iter(|| {
            black_box(
                direct_storage
                    .raw()
                    .query("SELECT COUNT(*) FROM conversations")
                    .unwrap(),
            )
        })
    });

    group.finish();
}

// =============================================================================
// Retry helper for concurrent writes
// =============================================================================

fn with_retry<F, T>(max_retries: usize, mut f: F) -> anyhow::Result<T>
where
    F: FnMut() -> Result<T, anyhow::Error>,
{
    let mut backoff_ms = 2_u64;
    for attempt in 0..=max_retries {
        match f() {
            Ok(val) => return Ok(val),
            Err(err) => {
                let is_retryable = err
                    .downcast_ref::<FrankenError>()
                    .or_else(|| err.root_cause().downcast_ref::<FrankenError>())
                    .is_some_and(|inner| {
                        matches!(
                            inner,
                            FrankenError::Busy
                                | FrankenError::BusyRecovery
                                | FrankenError::BusySnapshot { .. }
                                | FrankenError::WriteConflict { .. }
                                | FrankenError::SerializationFailure { .. }
                                | FrankenError::DatabaseCorrupt { .. }
                        )
                    });
                if attempt < max_retries && is_retryable {
                    std::thread::sleep(Duration::from_millis(backoff_ms));
                    backoff_ms = (backoff_ms * 2).min(128);
                    continue;
                }
                return Err(err);
            }
        }
    }
    Err(anyhow::anyhow!("exhausted retries"))
}

// =============================================================================
// 4. CONCURRENT WRITE THROUGHPUT (FrankenConnectionManager)
// =============================================================================

fn bench_concurrent_writes(c: &mut Criterion) {
    use frankensqlite::compat::TransactionExt;

    let mut group = c.benchmark_group("concurrent_writes");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(5));

    // Single-writer baseline (FrankenStorage direct)
    group.throughput(Throughput::Elements(100));
    group.bench_function("single_writer_100_convs", |b| {
        b.iter_with_setup(
            || {
                let temp = TempDir::new().unwrap();
                let db_path = temp.path().join("bench.db");
                let fs = FrankenStorage::open(&db_path).unwrap();

                // Pre-create agents/workspaces
                let mut agent_ids = Vec::new();
                let mut ws_ids = Vec::new();
                for i in 0..10i64 {
                    agent_ids.push(fs.ensure_agent(&make_agent(i)).unwrap());
                }
                for i in 0..20i64 {
                    let ws_path = PathBuf::from(format!("/workspace/project-{}", i));
                    ws_ids.push(fs.ensure_workspace(&ws_path, None).unwrap());
                }

                let convs: Vec<_> = (0..100i64).map(|i| make_conversation(i, 5)).collect();
                (temp, fs, agent_ids, ws_ids, convs)
            },
            |(_temp, fs, agent_ids, ws_ids, convs)| {
                for (i, conv) in convs.iter().enumerate() {
                    let agent_id = agent_ids[i % 10];
                    let ws_id = ws_ids[i % 20];
                    black_box(
                        fs.insert_conversation_tree(agent_id, Some(ws_id), conv)
                            .unwrap(),
                    );
                }
            },
        );
    });

    // ConnectionManager with 4 concurrent writers using raw SQL.
    // Uses raw INSERT + retry to benchmark the MVCC concurrent write path
    // independent of the full insert_conversation_tree complexity.
    group.throughput(Throughput::Elements(400));
    group.bench_function("4_writers_raw_400_rows", |b| {
        b.iter_with_setup(
            || {
                let temp = TempDir::new().unwrap();
                let db_path = temp.path().join("bench.db");
                let fs = FrankenStorage::open(&db_path).unwrap();
                // Create a simple table for raw concurrent writes
                fs.raw()
                    .execute("CREATE TABLE IF NOT EXISTS bench_raw (id INTEGER PRIMARY KEY, tid INTEGER, seq INTEGER, val TEXT)")
                    .unwrap();
                drop(fs);

                let config = ConnectionManagerConfig {
                    reader_count: 2,
                    max_writers: 4,
                };
                let mgr = FrankenConnectionManager::new(&db_path, config).unwrap();
                (temp, mgr)
            },
            |(_temp, mgr)| {
                std::thread::scope(|s| {
                    for tid in 0..4 {
                        let m = &mgr;
                        s.spawn(move || {
                            for seq in 0..100 {
                                let mut guard = m.concurrent_writer().unwrap();
                                with_retry(50, || {
                                    let mut tx = guard.storage().raw().transaction()?;
                                    tx.execute(&format!(
                                        "INSERT INTO bench_raw (tid, seq, val) VALUES ({tid}, {seq}, 'bench-{tid}-{seq}')"
                                    ))?;
                                    tx.commit().map_err(anyhow::Error::new)?;
                                    Ok(())
                                })
                                .expect("concurrent raw insert should succeed");
                                guard.mark_committed();
                            }
                        });
                    }
                });
            },
        );
    });

    group.finish();
}

// =============================================================================
// 5. SCALING BENCHMARKS
// =============================================================================

fn bench_insert_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("insert_scaling");
    group.sample_size(10);

    for &count in &[50usize, 100, 250, 500] {
        group.throughput(Throughput::Elements(count as u64));

        group.bench_with_input(BenchmarkId::new("persist", count), &count, |b, &count| {
            b.iter_with_setup(
                || {
                    let temp = TempDir::new().unwrap();
                    let db_path = temp.path().join("bench.db");
                    let index_path = index_dir(temp.path()).expect("index path");
                    let storage = FrankenStorage::open(&db_path).unwrap();
                    let t_index = TantivyIndex::open_or_create(&index_path).unwrap();
                    let convs: Vec<_> = (0..count as i64)
                        .map(|i| generate_conversation(i, 10))
                        .collect();
                    (temp, storage, t_index, convs)
                },
                |(_temp, storage, mut t_index, convs)| {
                    for conv in &convs {
                        persist_conversation(&storage, &mut t_index, conv).unwrap();
                    }
                    t_index.commit().unwrap();
                },
            );
        });

        group.bench_with_input(BenchmarkId::new("direct", count), &count, |b, &count| {
            b.iter_with_setup(
                || {
                    let temp = TempDir::new().unwrap();
                    let db_path = temp.path().join("bench.db");
                    let fs = FrankenStorage::open(&db_path).unwrap();

                    // Pre-create agents/workspaces
                    let mut agent_ids = Vec::new();
                    let mut ws_ids = Vec::new();
                    for i in 0..10i64 {
                        agent_ids.push(fs.ensure_agent(&make_agent(i)).unwrap());
                    }
                    for i in 0..20i64 {
                        let ws_path = PathBuf::from(format!("/workspace/project-{}", i));
                        ws_ids.push(fs.ensure_workspace(&ws_path, None).unwrap());
                    }

                    let convs: Vec<_> = (0..count as i64)
                        .map(|i| make_conversation(i, 10))
                        .collect();
                    (temp, fs, agent_ids, ws_ids, convs)
                },
                |(_temp, fs, agent_ids, ws_ids, convs)| {
                    for (i, conv) in convs.iter().enumerate() {
                        let agent_id = agent_ids[i % 10];
                        let ws_id = ws_ids[i % 20];
                        black_box(
                            fs.insert_conversation_tree(agent_id, Some(ws_id), conv)
                                .unwrap(),
                        );
                    }
                },
            );
        });
    }

    group.finish();
}

// =============================================================================
// Criterion wiring
// =============================================================================

criterion_group! {
    name = db_regression;
    config = configure_criterion();
    targets =
        bench_db_open_comparison,
        bench_insert_comparison,
        bench_insert_remote_source_reuse,
        bench_append_remote_source_merge,
        bench_query_comparison,
        bench_concurrent_writes,
        bench_insert_scaling,
}

criterion_main!(db_regression);
