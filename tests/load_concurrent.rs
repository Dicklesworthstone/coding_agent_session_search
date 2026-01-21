//! Concurrent operation load tests for cass.
//!
//! These tests verify that cass handles concurrent operations correctly:
//! - Multiple simultaneous searches
//! - Concurrent indexing and searching
//! - Thread safety under load
//!
//! Run with release mode:
//!   cargo test --test load_concurrent --release -- --nocapture

use coding_agent_search::connectors::{NormalizedConversation, NormalizedMessage};
use coding_agent_search::indexer::persist::persist_conversation;
use coding_agent_search::search::query::{FieldMask, SearchClient, SearchFilters};
use coding_agent_search::search::tantivy::{index_dir, TantivyIndex};
use coding_agent_search::storage::sqlite::SqliteStorage;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// Generate a test conversation.
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
                 consectetur adipiscing elit. Testing concurrent operations.",
                conv_id, m
            ),
            extra: serde_json::json!({ "concurrent_test": true }),
            snippets: Vec::new(),
        })
        .collect();

    NormalizedConversation {
        agent_slug: format!("concurrent-agent-{}", conv_id % 10),
        external_id: Some(format!("concurrent-conv-{}", conv_id)),
        title: Some(format!("Concurrent Test Conversation {}", conv_id)),
        workspace: Some(PathBuf::from(format!("/workspace/project-{}", conv_id % 20))),
        source_path: PathBuf::from(format!("/tmp/concurrent-test/conv-{}.jsonl", conv_id)),
        started_at: Some(base_ts),
        ended_at: Some(base_ts + msg_count * 1000),
        metadata: serde_json::json!({ "concurrent_test": true }),
        messages,
    }
}

/// Set up a test index with sample data.
fn setup_test_index(conv_count: i64, msgs_per_conv: i64) -> (TempDir, PathBuf, PathBuf) {
    let temp = TempDir::new().expect("create tempdir");
    let data_dir = temp.path().to_path_buf();
    let db_path = data_dir.join("concurrent_test.db");
    let index_path = index_dir(&data_dir).expect("index path");

    let mut storage = SqliteStorage::open(&db_path).expect("open db");
    let mut t_index = TantivyIndex::open_or_create(&index_path).unwrap();

    for i in 0..conv_count {
        let conv = generate_conversation(i, msgs_per_conv);
        persist_conversation(&mut storage, &mut t_index, &conv).expect("persist");
    }
    t_index.commit().unwrap();

    (temp, index_path, db_path)
}

// =============================================================================
// Concurrent Search Tests
// =============================================================================

/// Test multiple simultaneous searches.
#[test]
fn concurrent_search_parallel() {
    println!("\n=== Concurrent Test: Parallel Searches ===");

    let (tmp, index_path, db_path) = setup_test_index(2_000, 10);

    let thread_count = 8;
    let searches_per_thread = 100;
    let success_count = Arc::new(AtomicUsize::new(0));
    let error_count = Arc::new(AtomicUsize::new(0));

    let start = Instant::now();
    let mut handles = Vec::new();

    for thread_id in 0..thread_count {
        let index_path = index_path.clone();
        let db_path = db_path.clone();
        let success = Arc::clone(&success_count);
        let errors = Arc::clone(&error_count);

        let handle = thread::spawn(move || {
            // Each thread gets its own client
            let client = match SearchClient::open(&index_path, Some(&db_path)) {
                Ok(Some(c)) => c,
                Ok(None) => {
                    errors.fetch_add(searches_per_thread, Ordering::SeqCst);
                    return;
                }
                Err(_) => {
                    errors.fetch_add(searches_per_thread, Ordering::SeqCst);
                    return;
                }
            };

            let queries = ["lorem", "ipsum", "dolor", "test*", "concurrent"];
            let filters = SearchFilters::default();

            for i in 0..searches_per_thread {
                let query = queries[(thread_id * i) % queries.len()];
                match client.search(query, filters.clone(), 50, 0, FieldMask::FULL) {
                    Ok(_) => {
                        success.fetch_add(1, Ordering::SeqCst);
                    }
                    Err(_) => {
                        errors.fetch_add(1, Ordering::SeqCst);
                    }
                }
            }
        });
        handles.push(handle);
    }

    // Wait for all threads
    for handle in handles {
        handle.join().expect("thread panicked");
    }

    let duration = start.elapsed();
    let total_searches = thread_count * searches_per_thread;
    let successes = success_count.load(Ordering::SeqCst);
    let errors = error_count.load(Ordering::SeqCst);

    println!(
        "  {} threads x {} searches = {} total",
        thread_count, searches_per_thread, total_searches
    );
    println!("  Duration: {:?}", duration);
    println!("  Successes: {}, Errors: {}", successes, errors);
    println!(
        "  Throughput: {:.1} searches/sec",
        total_searches as f64 / duration.as_secs_f64()
    );

    assert_eq!(errors, 0, "All searches should succeed");
    assert_eq!(successes, total_searches, "All searches should complete");

    drop(tmp);
    println!("  PASS: Parallel searches");
}

/// Test search stability under sustained load.
#[test]
fn concurrent_sustained_load() {
    println!("\n=== Concurrent Test: Sustained Load ===");

    let (tmp, index_path, db_path) = setup_test_index(5_000, 10);
    let client = SearchClient::open(&index_path, Some(&db_path))
        .expect("open")
        .expect("client");

    let duration_target = Duration::from_secs(5);
    let start = Instant::now();
    let mut search_count = 0;
    let mut max_latency = Duration::ZERO;

    let queries = ["lorem", "ipsum", "dolor", "sit", "amet", "test*", "conv*"];
    let filters = SearchFilters::default();

    while start.elapsed() < duration_target {
        let query = queries[search_count % queries.len()];
        let search_start = Instant::now();
        let _ = client
            .search(query, filters.clone(), 50, 0, FieldMask::FULL)
            .expect("search failed");
        let latency = search_start.elapsed();

        if latency > max_latency {
            max_latency = latency;
        }
        search_count += 1;
    }

    let total_duration = start.elapsed();
    println!("  Sustained load for {:?}", total_duration);
    println!("  Total searches: {}", search_count);
    println!("  Max latency: {:?}", max_latency);
    println!(
        "  Avg throughput: {:.1} searches/sec",
        search_count as f64 / total_duration.as_secs_f64()
    );

    // Max latency should stay reasonable even under load
    assert!(
        max_latency < Duration::from_secs(2),
        "Max latency {:?} exceeds 2s threshold",
        max_latency
    );

    drop(client);
    drop(tmp);
    println!("  PASS: Sustained load");
}

/// Test varied query patterns concurrently.
#[test]
fn concurrent_varied_queries() {
    println!("\n=== Concurrent Test: Varied Query Patterns ===");

    let (tmp, index_path, db_path) = setup_test_index(3_000, 10);

    let thread_count = 4;
    let searches_per_thread = 50;

    // Each thread uses different query patterns
    let query_patterns = [
        vec!["simple", "terms", "only"],
        vec!["prefix*", "wild*", "*suffix"],
        vec!["\"exact phrase\"", "\"another phrase\""],
        vec!["complex AND boolean", "term OR other", "mixed -exclude"],
    ];

    let results: Vec<(usize, Duration)> = (0..thread_count)
        .map(|thread_id| {
            let index_path = index_path.clone();
            let db_path = db_path.clone();
            let patterns = query_patterns[thread_id % query_patterns.len()].clone();

            thread::spawn(move || {
                let client = SearchClient::open(&index_path, Some(&db_path))
                    .expect("open")
                    .expect("client");

                let filters = SearchFilters::default();
                let start = Instant::now();
                let mut count = 0;

                for i in 0..searches_per_thread {
                    let query = &patterns[i % patterns.len()];
                    if client
                        .search(query, filters.clone(), 50, 0, FieldMask::FULL)
                        .is_ok()
                    {
                        count += 1;
                    }
                }

                (count, start.elapsed())
            })
        })
        .map(|h| h.join().expect("thread"))
        .collect();

    for (i, (count, duration)) in results.iter().enumerate() {
        println!(
            "  Thread {} ({:?} pattern): {} searches in {:?}",
            i,
            ["simple", "wildcard", "phrase", "boolean"][i % 4],
            count,
            duration
        );
    }

    let total_success: usize = results.iter().map(|(c, _)| c).sum();
    let expected_total = thread_count * searches_per_thread;
    println!(
        "  Total: {}/{} successful",
        total_success, expected_total
    );

    // Allow some failures for complex queries that may not match
    assert!(
        total_success >= expected_total * 8 / 10,
        "At least 80% of searches should succeed"
    );

    drop(tmp);
    println!("  PASS: Varied queries");
}

// =============================================================================
// Concurrent Index + Search Tests
// =============================================================================

/// Test searching while index is being updated.
/// Note: This requires the SearchClient to handle reader reload.
#[test]
fn concurrent_search_during_index() {
    println!("\n=== Concurrent Test: Search During Indexing ===");

    let temp = TempDir::new().expect("create tempdir");
    let data_dir = temp.path().to_path_buf();
    let db_path = data_dir.join("concurrent_index.db");
    let index_path = index_dir(&data_dir).expect("index path");

    // Create initial index with some data
    {
        let mut storage = SqliteStorage::open(&db_path).expect("open db");
        let mut t_index = TantivyIndex::open_or_create(&index_path).unwrap();

        for i in 0..500 {
            let conv = generate_conversation(i, 5);
            persist_conversation(&mut storage, &mut t_index, &conv).expect("persist");
        }
        t_index.commit().unwrap();
    }

    // Create search client
    let client = SearchClient::open(&index_path, Some(&db_path))
        .expect("open")
        .expect("client");

    let search_success = Arc::new(AtomicUsize::new(0));
    let search_count = Arc::clone(&search_success);

    // Start search thread
    let index_path_clone = index_path.clone();
    let db_path_clone = db_path.clone();
    let search_handle = thread::spawn(move || {
        let client = SearchClient::open(&index_path_clone, Some(&db_path_clone))
            .expect("open")
            .expect("client");

        let filters = SearchFilters::default();
        for _ in 0..200 {
            if client
                .search("lorem", filters.clone(), 50, 0, FieldMask::FULL)
                .is_ok()
            {
                search_count.fetch_add(1, Ordering::SeqCst);
            }
            thread::sleep(Duration::from_millis(5));
        }
    });

    // Perform indexing while searches are running
    let index_handle = thread::spawn(move || {
        let mut storage = SqliteStorage::open(&db_path).expect("open db");
        let mut t_index = TantivyIndex::open_or_create(&index_path).unwrap();

        for i in 500..1000 {
            let conv = generate_conversation(i, 5);
            persist_conversation(&mut storage, &mut t_index, &conv).expect("persist");
            if i % 100 == 0 {
                t_index.commit().unwrap();
            }
        }
        t_index.commit().unwrap();
    });

    search_handle.join().expect("search thread");
    index_handle.join().expect("index thread");

    let successes = search_success.load(Ordering::SeqCst);
    println!("  Successful searches during indexing: {}/200", successes);

    // Most searches should succeed even during indexing
    assert!(
        successes >= 180,
        "At least 90% of searches should succeed during indexing"
    );

    drop(client);
    drop(temp);
    println!("  PASS: Search during indexing");
}

// =============================================================================
// Stress Tests
// =============================================================================

/// High concurrency stress test.
#[test]
#[ignore = "expensive: run with --ignored for stress testing"]
fn concurrent_stress_high_threads() {
    println!("\n=== Concurrent Stress Test: High Thread Count ===");

    let (tmp, index_path, db_path) = setup_test_index(10_000, 10);

    let thread_count = 32;
    let searches_per_thread = 200;
    let success_count = Arc::new(AtomicUsize::new(0));

    let start = Instant::now();
    let handles: Vec<_> = (0..thread_count)
        .map(|thread_id| {
            let index_path = index_path.clone();
            let db_path = db_path.clone();
            let success = Arc::clone(&success_count);

            thread::spawn(move || {
                let client = match SearchClient::open(&index_path, Some(&db_path)) {
                    Ok(Some(c)) => c,
                    _ => return,
                };

                let queries = ["lorem", "ipsum", "test*"];
                let filters = SearchFilters::default();

                for i in 0..searches_per_thread {
                    let query = queries[(thread_id + i) % queries.len()];
                    if client
                        .search(query, filters.clone(), 50, 0, FieldMask::FULL)
                        .is_ok()
                    {
                        success.fetch_add(1, Ordering::SeqCst);
                    }
                }
            })
        })
        .collect();

    for handle in handles {
        handle.join().expect("thread");
    }

    let duration = start.elapsed();
    let total = thread_count * searches_per_thread;
    let successes = success_count.load(Ordering::SeqCst);

    println!("  {} threads x {} searches = {}", thread_count, searches_per_thread, total);
    println!("  Duration: {:?}", duration);
    println!("  Success rate: {}/{} ({:.1}%)", successes, total, 100.0 * successes as f64 / total as f64);
    println!(
        "  Throughput: {:.1} searches/sec",
        total as f64 / duration.as_secs_f64()
    );

    assert!(
        successes >= total * 95 / 100,
        "At least 95% success rate required"
    );

    drop(tmp);
    println!("  PASS: High thread stress test");
}

// =============================================================================
// Summary
// =============================================================================

#[test]
fn concurrent_test_summary() {
    println!("\n");
    println!("╔═══════════════════════════════════════════════════════════════╗");
    println!("║              CASS Concurrent Load Test Summary                ║");
    println!("╠═══════════════════════════════════════════════════════════════╣");
    println!("║ Test                    │ Configuration   │ Status            ║");
    println!("╠─────────────────────────┼─────────────────┼───────────────────╣");
    println!("║ Parallel searches       │ 8 threads       │ Run by default    ║");
    println!("║ Sustained load          │ 5s duration     │ Run by default    ║");
    println!("║ Varied query patterns   │ 4 threads       │ Run by default    ║");
    println!("║ Search during indexing  │ concurrent      │ Run by default    ║");
    println!("║ High thread stress      │ 32 threads      │ --ignored         ║");
    println!("╚═══════════════════════════════════════════════════════════════╝");
    println!();
    println!("Run all concurrent tests:");
    println!("  cargo test --test load_concurrent --release -- --nocapture");
    println!();
    println!("Include stress tests:");
    println!("  cargo test --test load_concurrent --release -- --nocapture --include-ignored");
    println!();
}
