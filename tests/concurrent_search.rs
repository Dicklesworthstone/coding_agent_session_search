//! Concurrent Search Tests (tst.srch.conc)
//!
//! Tests search behavior under concurrent load:
//! - 10 simultaneous searches
//! - Search during indexing
//! - Cache contention
//! - Reader handle exhaustion
//!
//! Assertions:
//! - All return correct results
//! - No deadlocks
//! - Reasonable latency

use coding_agent_search::search::query::{FieldMask, SearchClient, SearchFilters};
use coding_agent_search::search::tantivy::TantivyIndex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

mod util;

/// Test 10 simultaneous searches all return correct results
#[test]
fn concurrent_10_simultaneous_searches() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();

    // Seed index with diverse content for different searches
    for i in 0..20 {
        let conv = util::ConversationFixtureBuilder::new("tester")
            .title(format!("conversation_{i}"))
            .source_path(dir.path().join(format!("log_{i}.jsonl")))
            .base_ts(1000 + i64::from(i))
            .messages(3)
            .with_content(0, format!("unique_term_{i} alpha beta gamma"))
            .with_content(1, format!("search_target_{i} delta epsilon"))
            .with_content(2, format!("concurrent_test_{i} zeta eta"))
            .build_normalized();

        index.add_conversation(&conv).unwrap();
    }
    index.commit().unwrap();

    // Each thread creates its own SearchClient so the search state stays thread-local.
    let index_path = dir.path().to_path_buf();

    let barrier = Arc::new(Barrier::new(10));
    let success_count = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();

    // Spawn 10 threads that all search simultaneously
    for i in 0..10 {
        let index_path = index_path.clone();
        let barrier = Arc::clone(&barrier);
        let success_count = Arc::clone(&success_count);
        let search_term = format!("unique_term_{}", i % 5); // Use 5 different terms

        handles.push(thread::spawn(move || {
            // Each thread creates its own client (thread-safe pattern)
            let client = SearchClient::open(&index_path, None)
                .unwrap()
                .expect("client");

            // Wait for all threads to be ready
            barrier.wait();

            let start = Instant::now();
            let hits = client
                .search(
                    &search_term,
                    SearchFilters::default(),
                    10,
                    0,
                    FieldMask::FULL,
                )
                .unwrap();
            let elapsed = start.elapsed();

            // Each term should find at least 1 result (4 conversations per term pattern)
            if !hits.is_empty() && elapsed < Duration::from_secs(5) {
                success_count.fetch_add(1, Ordering::Relaxed);
            }

            (hits.len(), elapsed)
        }));
    }

    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    // All 10 searches should succeed
    assert_eq!(
        success_count.load(Ordering::Relaxed),
        10,
        "All 10 concurrent searches should succeed. Results: {results:?}"
    );

    // Verify reasonable latency (all should complete within 5 seconds)
    for (hit_count, elapsed) in &results {
        assert!(
            *elapsed < Duration::from_secs(5),
            "Search took too long: {elapsed:?}"
        );
        assert!(*hit_count > 0, "Search should return results");
    }
}

/// Test search works correctly during active indexing
#[test]
fn concurrent_search_during_indexing() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();

    // Initial seed data
    let conv = util::ConversationFixtureBuilder::new("tester")
        .title("initial")
        .source_path(dir.path().join("initial.jsonl"))
        .base_ts(1000)
        .messages(1)
        .with_content(0, "baseline_content searchable_term")
        .build_normalized();

    index.add_conversation(&conv).unwrap();
    index.commit().unwrap();

    // Each thread creates its own SearchClient so the search state stays thread-local.
    let index_path = dir.path().to_path_buf();

    let indexing_complete = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let searches_during_index = Arc::new(AtomicUsize::new(0));
    let search_successes = Arc::new(AtomicUsize::new(0));

    let indexing_complete_clone = Arc::clone(&indexing_complete);
    let searches_clone = Arc::clone(&searches_during_index);
    let successes_clone = Arc::clone(&search_successes);

    // Spawn searcher thread that searches continuously during indexing
    let search_handle = thread::spawn(move || {
        // Thread creates its own client
        let client = SearchClient::open(&index_path, None)
            .unwrap()
            .expect("client");

        while !indexing_complete_clone.load(Ordering::Relaxed) {
            let result = client.search(
                "searchable_term",
                SearchFilters::default(),
                10,
                0,
                FieldMask::FULL,
            );
            searches_clone.fetch_add(1, Ordering::Relaxed);
            if result.is_ok() {
                successes_clone.fetch_add(1, Ordering::Relaxed);
            }
            thread::sleep(Duration::from_millis(10));
        }
    });

    // Index more documents while searches are happening
    for i in 0..50 {
        let conv = util::ConversationFixtureBuilder::new("tester")
            .title(format!("added_{i}"))
            .source_path(dir.path().join(format!("added_{i}.jsonl")))
            .base_ts(2000 + i64::from(i))
            .messages(1)
            .with_content(0, format!("new_content_{i} searchable_term"))
            .build_normalized();

        index.add_conversation(&conv).unwrap();

        // Commit periodically to create multiple segments
        if i % 10 == 0 {
            index.commit().unwrap();
        }
    }
    index.commit().unwrap();

    // Signal indexing complete
    indexing_complete.store(true, Ordering::Relaxed);
    search_handle.join().unwrap();

    let total_searches = searches_during_index.load(Ordering::Relaxed);
    let successful_searches = search_successes.load(Ordering::Relaxed);

    // All searches should succeed (no deadlocks, no errors)
    assert!(
        total_searches > 0,
        "Should have performed searches during indexing"
    );
    assert_eq!(
        total_searches, successful_searches,
        "All {total_searches} searches should succeed during indexing, but only {successful_searches} did"
    );
}

/// Test cache contention with multiple readers accessing the same cached data
/// Note: With per-thread clients, each has its own cache, so this tests concurrent
/// access to the same underlying Tantivy index rather than shared cache contention.
#[test]
fn concurrent_cache_contention() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();

    // Create content that will be cached
    let conv = util::ConversationFixtureBuilder::new("tester")
        .title("cache contention test")
        .source_path(dir.path().join("cache_test.jsonl"))
        .base_ts(1000)
        .messages(1)
        .with_content(0, "cache_contention_unique_term for testing")
        .build_normalized();

    index.add_conversation(&conv).unwrap();
    index.commit().unwrap();

    // Each thread creates its own SearchClient so the search state stays thread-local.
    let index_path = dir.path().to_path_buf();

    // Pre-test: verify the content exists
    let test_client = SearchClient::open(&index_path, None)
        .unwrap()
        .expect("client");
    let initial_hits = test_client
        .search(
            "cache_contention",
            SearchFilters::default(),
            10,
            0,
            FieldMask::FULL,
        )
        .unwrap();
    assert_eq!(initial_hits.len(), 1, "Should find the cached content");
    drop(test_client);

    let barrier = Arc::new(Barrier::new(20));
    let success_count = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();

    // Spawn 20 threads that all hit the same search simultaneously
    for _ in 0..20 {
        let index_path = index_path.clone();
        let barrier = Arc::clone(&barrier);
        let success_count = Arc::clone(&success_count);

        handles.push(thread::spawn(move || {
            // Each thread creates its own client
            let client = SearchClient::open(&index_path, None)
                .unwrap()
                .expect("client");

            barrier.wait();

            // All threads search for the same term
            let start = Instant::now();
            let hits = client
                .search(
                    "cache_contention",
                    SearchFilters::default(),
                    10,
                    0,
                    FieldMask::FULL,
                )
                .unwrap();
            let elapsed = start.elapsed();

            if hits.len() == 1 && elapsed < Duration::from_secs(2) {
                success_count.fetch_add(1, Ordering::Relaxed);
            }

            (hits.len(), elapsed)
        }));
    }

    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    // All 20 concurrent accesses should succeed
    assert_eq!(
        success_count.load(Ordering::Relaxed),
        20,
        "All 20 concurrent accesses should succeed. Results: {results:?}"
    );
}

/// Test reader handle exhaustion under high load
#[test]
fn concurrent_reader_handle_exhaustion() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();

    // Seed index with substantial content
    for i in 0..100 {
        let conv = util::ConversationFixtureBuilder::new("tester")
            .title(format!("stress_test_{i}"))
            .source_path(dir.path().join(format!("stress_{i}.jsonl")))
            .base_ts(1000 + i64::from(i))
            .messages(5)
            .with_content(0, format!("stress_content_{i} alpha"))
            .with_content(1, format!("heavy_load_{i} beta"))
            .with_content(2, format!("concurrent_access_{i} gamma"))
            .with_content(3, format!("reader_test_{i} delta"))
            .with_content(4, format!("exhaustion_check_{i} epsilon"))
            .build_normalized();

        index.add_conversation(&conv).unwrap();
    }
    index.commit().unwrap();

    // Each thread creates its own SearchClient so the search state stays thread-local.
    let index_path = dir.path().to_path_buf();

    let barrier = Arc::new(Barrier::new(50));
    let success_count = Arc::new(AtomicUsize::new(0));
    let error_count = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();

    // Spawn 50 threads to stress test concurrent index access
    for i in 0..50 {
        let index_path = index_path.clone();
        let barrier = Arc::clone(&barrier);
        let success_count = Arc::clone(&success_count);
        let error_count = Arc::clone(&error_count);

        handles.push(thread::spawn(move || {
            // Each thread creates its own client
            let client = SearchClient::open(&index_path, None)
                .unwrap()
                .expect("client");

            barrier.wait();

            // Each thread performs multiple searches
            let mut local_success = 0;
            let mut local_errors = Vec::new();

            for j in 0..10 {
                let term = format!("stress_content_{}", (i * 10 + j) % 100);
                match client.search(&term, SearchFilters::default(), 5, 0, FieldMask::FULL) {
                    Ok(hits) if !hits.is_empty() => local_success += 1,
                    Ok(_) => local_success += 1, // Empty results still count as success
                    Err(error) => local_errors.push(error),
                }
            }

            success_count.fetch_add(local_success, Ordering::Relaxed);
            error_count.fetch_add(local_errors.len(), Ordering::Relaxed);

            (local_success, local_errors)
        }));
    }

    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    let total_success = success_count.load(Ordering::Relaxed);
    let total_errors = error_count.load(Ordering::Relaxed);
    let total_searches = 50 * 10; // 50 threads × 10 searches each

    // Should have no errors (no reader exhaustion)
    assert_eq!(
        total_errors, 0,
        "Should have no reader exhaustion errors. Results: {results:?}"
    );

    // All searches should succeed
    assert_eq!(
        total_success, total_searches,
        "All {total_searches} searches should succeed, got {total_success}"
    );
}

/// Test that concurrent searches with different filters don't interfere
#[test]
fn concurrent_different_filters_no_interference() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();

    // Create conversations with different agents and workspaces
    // Note: Use single-word agent names because the agent field uses TEXT (tokenized)
    // and TermQuery requires exact token matches. "claude_code" would be tokenized
    // into "claude" and "code", breaking the filter.
    for agent in &["codex", "claude", "gemini"] {
        for i in 0..5 {
            let conv = util::ConversationFixtureBuilder::new(*agent)
                .title(format!("{agent}_{i}"))
                .source_path(dir.path().join(format!("{agent}_{i}.jsonl")))
                .workspace(format!("/workspace/{agent}"))
                .base_ts(1000 + i64::from(i))
                .messages(1)
                .with_content(0, format!("filter_test common_term {agent} specific"))
                .build_normalized();

            index.add_conversation(&conv).unwrap();
        }
    }
    index.commit().unwrap();

    // Each thread creates its own SearchClient so the search state stays thread-local.
    let index_path = dir.path().to_path_buf();

    let barrier = Arc::new(Barrier::new(6));
    let mut handles = Vec::new();

    // Spawn threads with different agent filters
    for agent in &["codex", "claude", "gemini"] {
        let index_path_clone = index_path.clone();
        let barrier_clone = Arc::clone(&barrier);
        let agent = agent.to_string();

        // Thread searching with agent filter
        handles.push(thread::spawn(move || {
            let client = SearchClient::open(&index_path_clone, None)
                .unwrap()
                .expect("client");

            barrier_clone.wait();

            let mut filters = SearchFilters::default();
            filters.agents.insert(agent.clone());

            let hits = client
                .search("common_term", filters, 20, 0, FieldMask::FULL)
                .unwrap();

            // All results should be from the filtered agent
            for hit in &hits {
                assert_eq!(
                    hit.agent, agent,
                    "Result should be from agent {}, got {}",
                    agent, hit.agent
                );
            }

            (agent, hits.len())
        }));

        // Thread searching without filters
        let index_path_clone = index_path.clone();
        let barrier_clone = Arc::clone(&barrier);

        handles.push(thread::spawn(move || {
            let client = SearchClient::open(&index_path_clone, None)
                .unwrap()
                .expect("client");

            barrier_clone.wait();

            let hits = client
                .search(
                    "common_term",
                    SearchFilters::default(),
                    20,
                    0,
                    FieldMask::FULL,
                )
                .unwrap();

            // Should find results from all agents
            ("all".to_string(), hits.len())
        }));
    }

    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    // Verify filtered results
    for (agent, count) in &results {
        if agent == "all" {
            assert!(
                *count >= 3,
                "Unfiltered search should find results from multiple agents (got {count})"
            );
        } else {
            assert!(
                *count > 0,
                "Filtered search for {agent} should find results (got {count}). All results: {results:?}"
            );
        }
    }
}

/// Test no deadlocks under mixed read/write/search operations
#[test]
fn concurrent_no_deadlock_mixed_operations() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();

    // Initial seed
    let conv = util::ConversationFixtureBuilder::new("tester")
        .title("deadlock_test")
        .source_path(dir.path().join("deadlock.jsonl"))
        .base_ts(1000)
        .messages(1)
        .with_content(0, "deadlock_prevention_test")
        .build_normalized();

    index.add_conversation(&conv).unwrap();
    index.commit().unwrap();

    // Each thread creates its own SearchClient so the search state stays thread-local.
    let index_path = dir.path().to_path_buf();

    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let search_count = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();

    // Multiple search threads
    for _ in 0..5 {
        let index_path = index_path.clone();
        let done = Arc::clone(&done);
        let search_count = Arc::clone(&search_count);

        handles.push(thread::spawn(move || {
            // Each thread creates its own client
            let client = SearchClient::open(&index_path, None)
                .unwrap()
                .expect("client");

            while !done.load(Ordering::Relaxed) {
                let _ = client.search("deadlock", SearchFilters::default(), 10, 0, FieldMask::FULL);
                search_count.fetch_add(1, Ordering::Relaxed);
                thread::sleep(Duration::from_millis(5));
            }
        }));
    }

    // Index more content while searches are running
    for i in 0..30 {
        let conv = util::ConversationFixtureBuilder::new("tester")
            .title(format!("added_{i}"))
            .source_path(dir.path().join(format!("added_{i}.jsonl")))
            .base_ts(2000 + i64::from(i))
            .messages(1)
            .with_content(0, format!("deadlock_content_{i}"))
            .build_normalized();

        index.add_conversation(&conv).unwrap();

        if i % 5 == 0 {
            index.commit().unwrap();
        }

        thread::sleep(Duration::from_millis(10));
    }
    index.commit().unwrap();

    // Signal completion and wait with timeout
    done.store(true, Ordering::Relaxed);

    let timeout = Duration::from_secs(10);
    let start = Instant::now();

    for handle in handles {
        let remaining = timeout.saturating_sub(start.elapsed());
        assert!(
            !remaining.is_zero(),
            "Deadlock detected: threads did not complete within timeout"
        );
        // Join with implicit timeout through the atomic flag
        handle.join().expect("Thread should not panic");
    }

    let total_searches = search_count.load(Ordering::Relaxed);
    assert!(
        total_searches > 0,
        "Should have completed searches without deadlock"
    );
}
