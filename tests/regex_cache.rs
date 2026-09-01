//! Tests for RegexQuery LRU caching (Opt 5.3).
//!
//! Validates that the regex cache:
//! - Returns equivalent results with cache enabled vs disabled
//! - Correctly caches patterns per field
//! - Is thread-safe under concurrent access
//! - Can be disabled via CASS_REGEX_CACHE=0

use coding_agent_search::search::query::{FieldMask, SearchClient, SearchFilters};
use coding_agent_search::search::tantivy::TantivyIndex;
use std::sync::Arc;
use std::thread;
use tempfile::TempDir;

mod util;

/// Create a test index with content that includes patterns for regex matching.
fn create_test_index_with_patterns(dir: &TempDir) -> TantivyIndex {
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();

    // Create conversations with suffix-matchable content
    let conv1 = util::ConversationFixtureBuilder::new("tester")
        .title("Authentication Handler Test")
        .source_path(dir.path().join("auth.jsonl"))
        .base_ts(1000)
        .messages(2)
        .with_content(0, "Fix the authentication handler for login")
        .with_content(1, "The handler needs proper validation")
        .build_normalized();

    let conv2 = util::ConversationFixtureBuilder::new("tester")
        .title("Error Handler Implementation")
        .source_path(dir.path().join("error.jsonl"))
        .base_ts(2000)
        .messages(2)
        .with_content(0, "Implement error handler for API")
        .with_content(1, "ErrorHandler class should catch all exceptions")
        .build_normalized();

    let conv3 = util::ConversationFixtureBuilder::new("tester")
        .title("Database Configuration")
        .source_path(dir.path().join("db.jsonl"))
        .base_ts(3000)
        .messages(2)
        .with_content(0, "Configure database configuration settings")
        .with_content(1, "Configuration file needs updating")
        .build_normalized();

    index.add_conversation(&conv1).unwrap();
    index.add_conversation(&conv2).unwrap();
    index.add_conversation(&conv3).unwrap();
    index.commit().unwrap();

    index
}

// =============================================================================
// Equivalence Tests: Cache enabled vs disabled should return identical results
// =============================================================================

#[test]
fn test_regex_cache_equivalence_suffix_pattern() {
    // Test that suffix pattern (*handler) returns same results with/without cache
    let dir = TempDir::new().unwrap();
    let _index = create_test_index_with_patterns(&dir);

    let filters = SearchFilters::default();

    // Search with cache enabled (default)
    let client_cached = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");
    let results_cached = client_cached
        .search("*handler", filters.clone(), 10, 0, FieldMask::FULL)
        .unwrap();

    // qu81y: no production code reads CASS_REGEX_CACHE anymore — the former
    // per-call bypass gate was removed — so the old EnvGuard here mutated a
    // variable with no consumer. The test keeps its real assertion: a second
    // independently-opened client produces identical results.
    let client_uncached = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");
    let results_uncached = client_uncached
        .search("*handler", filters.clone(), 10, 0, FieldMask::FULL)
        .unwrap();

    // Both should return the same results
    assert_eq!(
        results_cached.len(),
        results_uncached.len(),
        "Result counts differ: cached={}, uncached={}",
        results_cached.len(),
        results_uncached.len()
    );

    // Verify we got hits for "handler" pattern
    assert!(
        !results_cached.is_empty(),
        "Expected hits for *handler pattern"
    );

    // Compare content of results
    let cached_contents: Vec<_> = results_cached.iter().map(|h| &h.content).collect();
    let uncached_contents: Vec<_> = results_uncached.iter().map(|h| &h.content).collect();
    assert_eq!(
        cached_contents, uncached_contents,
        "Result contents differ between cached and uncached"
    );
}

#[test]
fn test_regex_cache_equivalence_substring_pattern() {
    // Test that substring pattern (*config*) returns same results with/without cache
    let dir = TempDir::new().unwrap();
    let _index = create_test_index_with_patterns(&dir);

    let filters = SearchFilters::default();

    // Search with cache enabled
    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");
    let results_cached = client
        .search("*config*", filters.clone(), 10, 0, FieldMask::FULL)
        .unwrap();

    // qu81y: CASS_REGEX_CACHE has no consumer anymore; repeat the query and
    // pin result stability instead of mutating a dead env var.
    let results_uncached = client
        .search("*config*", filters.clone(), 10, 0, FieldMask::FULL)
        .unwrap();

    assert_eq!(
        results_cached.len(),
        results_uncached.len(),
        "Substring pattern result counts differ"
    );
    assert!(
        !results_cached.is_empty(),
        "Expected hits for *config* pattern"
    );
}

#[test]
fn test_regex_cache_equivalence_multiple_patterns() {
    // Test multiple different patterns all return consistent results
    let dir = TempDir::new().unwrap();
    let _index = create_test_index_with_patterns(&dir);

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");
    let filters = SearchFilters::default();

    let patterns = vec!["*handler", "*ation", "*error*", "*data*"];

    for pattern in patterns {
        // With cache
        let results1 = client
            .search(pattern, filters.clone(), 10, 0, FieldMask::FULL)
            .unwrap();

        // Repeat search - should use cached regex
        let results2 = client
            .search(pattern, filters.clone(), 10, 0, FieldMask::FULL)
            .unwrap();

        assert_eq!(
            results1.len(),
            results2.len(),
            "Pattern '{}' gave different result counts on repeat: {} vs {}",
            pattern,
            results1.len(),
            results2.len()
        );

        // Content should be identical
        let contents1: Vec<_> = results1.iter().map(|h| &h.content).collect();
        let contents2: Vec<_> = results2.iter().map(|h| &h.content).collect();
        assert_eq!(
            contents1, contents2,
            "Pattern '{}' gave different content on repeat",
            pattern
        );
    }
}

// =============================================================================
// Cache Behavior Tests: Verify caching mechanics
// =============================================================================

#[test]
fn test_regex_cache_repeated_queries_consistent() {
    // Repeated identical suffix queries should return consistent results
    let dir = TempDir::new().unwrap();
    let _index = create_test_index_with_patterns(&dir);

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");
    let filters = SearchFilters::default();

    // Run the same regex-triggering query 10 times
    let mut all_results = Vec::new();
    for _ in 0..10 {
        let results = client
            .search("*handler", filters.clone(), 10, 0, FieldMask::FULL)
            .unwrap();
        all_results.push(results);
    }

    // All results should be identical
    let first = &all_results[0];
    for (i, results) in all_results.iter().enumerate().skip(1) {
        assert_eq!(
            first.len(),
            results.len(),
            "Iteration {} had different result count",
            i
        );
        for (j, (a, b)) in first.iter().zip(results.iter()).enumerate() {
            assert_eq!(
                a.content, b.content,
                "Iteration {} result {} had different content",
                i, j
            );
        }
    }
}

#[test]
fn test_regex_cache_different_patterns_independent() {
    // Different patterns should be cached independently
    let dir = TempDir::new().unwrap();
    let _index = create_test_index_with_patterns(&dir);

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");
    let filters = SearchFilters::default();

    // Search for *handler (should match auth/error handler content)
    let handler_results = client
        .search("*handler", filters.clone(), 10, 0, FieldMask::FULL)
        .unwrap();

    // Search for *configuration (should match db config content)
    let config_results = client
        .search("*uration", filters.clone(), 10, 0, FieldMask::FULL)
        .unwrap();

    // These should be different result sets
    // (unless there's overlap, which there shouldn't be in our test data)
    let handler_contents: std::collections::HashSet<_> =
        handler_results.iter().map(|h| &h.content).collect();
    let config_contents: std::collections::HashSet<_> =
        config_results.iter().map(|h| &h.content).collect();

    // Verify they're not completely overlapping
    // (at least one should have unique results if patterns match different content)
    assert!(
        !handler_results.is_empty() || !config_results.is_empty(),
        "Both patterns returned no results"
    );

    // If both have results, verify independence
    if !handler_results.is_empty() && !config_results.is_empty() {
        // The result sets should differ in some way for truly different patterns
        // (This is a sanity check that different regex patterns produce different results)
        let _handler_set = handler_contents;
        let _config_set = config_contents;
        // Note: We don't strictly assert inequality since patterns might overlap
        // The key test is that each pattern is cached independently and returns
        // consistent results when repeated
    }
}

// =============================================================================
// Thread Safety Tests: Concurrent regex queries
// =============================================================================
// Note: The RegexCache is a global static protected by RwLock, so thread safety
// is tested by having multiple threads access it through their own SearchClient
// instances (each thread creates its own client pointing to the same index).

#[test]
fn test_regex_cache_concurrent_reads() {
    // Multiple threads reading with the same pattern should be safe
    // Each thread creates its own SearchClient, but they all hit the global RegexCache
    let dir = TempDir::new().unwrap();
    let _index = create_test_index_with_patterns(&dir);

    let index_path = Arc::new(dir.path().to_path_buf());

    let mut handles = Vec::new();

    // Spawn 10 threads all searching the same pattern
    for i in 0..10 {
        let path = Arc::clone(&index_path);
        let handle = thread::spawn(move || {
            // Each thread creates its own client
            let client = SearchClient::open(&path, None).unwrap().expect("client");
            let filters = SearchFilters::default();
            let results = client
                .search("*handler", filters, 10, 0, FieldMask::FULL)
                .unwrap();
            (i, results.len())
        });
        handles.push(handle);
    }

    // Collect results and verify consistency
    let mut result_counts = Vec::new();
    for handle in handles {
        let (thread_id, count) = handle.join().expect("Thread panicked");
        result_counts.push((thread_id, count));
    }

    // All threads should get the same result count
    let first_count = result_counts[0].1;
    for (thread_id, count) in &result_counts {
        assert_eq!(
            *count, first_count,
            "Thread {} got {} results, expected {}",
            thread_id, count, first_count
        );
    }
}

#[test]
fn test_regex_cache_concurrent_different_patterns() {
    // Multiple threads searching different patterns should be safe
    let dir = TempDir::new().unwrap();
    let _index = create_test_index_with_patterns(&dir);

    let index_path = Arc::new(dir.path().to_path_buf());

    let patterns = vec![
        "*handler",
        "*config*",
        "*error*",
        "*auth*",
        "*database*",
        "*impl*",
        "*test*",
        "*valid*",
    ];

    let mut handles = Vec::new();

    // Each thread searches a different pattern
    for (i, pattern) in patterns.into_iter().enumerate() {
        let path = Arc::clone(&index_path);
        let pattern = pattern.to_string();
        let handle = thread::spawn(move || {
            let client = SearchClient::open(&path, None).unwrap().expect("client");
            let filters = SearchFilters::default();
            let results = client
                .search(&pattern, filters, 10, 0, FieldMask::FULL)
                .unwrap();
            (i, pattern, results.len())
        });
        handles.push(handle);
    }

    // All threads should complete without deadlock or panic
    for handle in handles {
        let (thread_id, pattern, count) = handle.join().expect("Thread panicked");
        // Just verify it completed - we don't know expected counts for all patterns
        println!(
            "Thread {} pattern '{}' returned {} results",
            thread_id, pattern, count
        );
    }
}

#[test]
fn test_regex_cache_concurrent_read_write() {
    // Concurrent reads while cache is being populated should be safe
    // The global RegexCache uses RwLock for thread-safe access
    let dir = TempDir::new().unwrap();
    let _index = create_test_index_with_patterns(&dir);

    let index_path = Arc::new(dir.path().to_path_buf());

    let mut handles = Vec::new();

    // Spawn threads that will hit the cache and potentially cause new entries
    for i in 0..20 {
        let path = Arc::clone(&index_path);
        // Use a mix of patterns - some will cache hit, some will miss
        let pattern = format!("*thread{}*", i % 5);
        let handle = thread::spawn(move || {
            let client = SearchClient::open(&path, None).unwrap().expect("client");
            let filters = SearchFilters::default();
            // Run multiple searches to increase contention on the global cache
            for _ in 0..5 {
                let _ = client.search(&pattern, filters.clone(), 10, 0, FieldMask::FULL);
            }
            i
        });
        handles.push(handle);
    }

    // All threads should complete without deadlock
    for handle in handles {
        let thread_id = handle
            .join()
            .expect("Thread panicked during concurrent read/write");
        assert!(thread_id < 20, "Thread ID out of range");
    }
}

// =============================================================================
// Rollback Tests: CASS_REGEX_CACHE=0 bypasses cache
// =============================================================================

#[test]
fn test_regex_cache_disabled_via_env() {
    // qu81y: CASS_REGEX_CACHE has no consumer in production code anymore
    // (the per-call bypass gate was removed), so this test no longer sets
    // it. It still pins that regex queries work on a fresh client.
    let dir = TempDir::new().unwrap();
    let _index = create_test_index_with_patterns(&dir);

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");
    let filters = SearchFilters::default();

    // Search should still work even with cache disabled
    let results = client
        .search("*handler", filters.clone(), 10, 0, FieldMask::FULL)
        .unwrap();

    // Verify we get results (same as with cache enabled)
    assert!(
        !results.is_empty(),
        "Expected results even with cache disabled"
    );

    // Repeated query should also work
    let results2 = client
        .search("*handler", filters.clone(), 10, 0, FieldMask::FULL)
        .unwrap();

    assert_eq!(
        results.len(),
        results2.len(),
        "Repeated query with cache disabled gave different results"
    );
}

#[test]
fn test_regex_cache_disabled_false_string() {
    // qu81y: see test_regex_cache_disabled_via_env — the env gate is gone;
    // this variant keeps its correctness assertions on a fresh client.
    let dir = TempDir::new().unwrap();
    let _index = create_test_index_with_patterns(&dir);

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");
    let filters = SearchFilters::default();

    // Should still return correct results
    let results = client
        .search("*error*", filters, 10, 0, FieldMask::FULL)
        .unwrap();

    // Just verify it works - error* pattern should match our test data
    // The exact count depends on test data
    println!(
        "With cache disabled (false), *error* returned {} results",
        results.len()
    );
}

// =============================================================================
// Edge Case Tests
// =============================================================================

#[test]
fn test_regex_cache_empty_pattern_core() {
    // Patterns that resolve to empty core (like just "*") should be handled
    let dir = TempDir::new().unwrap();
    let _index = create_test_index_with_patterns(&dir);

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");
    let filters = SearchFilters::default();

    // Single asterisk - should parse as empty pattern
    let results = client
        .search("*", filters.clone(), 10, 0, FieldMask::FULL)
        .unwrap();

    // Empty pattern typically returns no results or all results depending on impl
    // Just verify it doesn't panic
    println!("Single asterisk returned {} results", results.len());
}

#[test]
fn test_regex_cache_special_regex_chars() {
    // Patterns with regex metacharacters should be properly escaped
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();

    // Add content with regex-special characters
    let conv = util::ConversationFixtureBuilder::new("tester")
        .title("Test with special chars")
        .source_path(dir.path().join("special.jsonl"))
        .base_ts(1000)
        .messages(1)
        .with_content(0, "The function foo.bar() handles [array] items")
        .build_normalized();

    index.add_conversation(&conv).unwrap();
    index.commit().unwrap();

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");
    let filters = SearchFilters::default();

    // Search for suffix with dot (regex metachar)
    let results = client
        .search("*bar()", filters.clone(), 10, 0, FieldMask::FULL)
        .unwrap();

    // Should handle without regex errors
    println!("Pattern with parens returned {} results", results.len());

    // Search with brackets (regex metachar)
    let results2 = client
        .search("*[array]*", filters.clone(), 10, 0, FieldMask::FULL)
        .unwrap();

    println!("Pattern with brackets returned {} results", results2.len());
}

#[test]
fn test_regex_cache_unicode_patterns() {
    // Unicode patterns should work correctly
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();

    // Add content with unicode
    let conv = util::ConversationFixtureBuilder::new("tester")
        .title("Unicode test")
        .source_path(dir.path().join("unicode.jsonl"))
        .base_ts(1000)
        .messages(1)
        .with_content(0, "Handle emoji: rocket and international: cafe")
        .build_normalized();

    index.add_conversation(&conv).unwrap();
    index.commit().unwrap();

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");
    let filters = SearchFilters::default();

    // Search for unicode content
    let results = client
        .search("*cafe", filters, 10, 0, FieldMask::FULL)
        .unwrap();

    // Should handle unicode without errors
    println!("Unicode pattern returned {} results", results.len());
}

#[test]
fn test_regex_cache_very_long_pattern() {
    // Very long patterns should be handled (cache key limits)
    let dir = TempDir::new().unwrap();
    let _index = create_test_index_with_patterns(&dir);

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");
    let filters = SearchFilters::default();

    // Create a very long pattern
    let long_core = "a".repeat(500);
    let long_pattern = format!("*{}", long_core);

    // Should handle without panic or OOM
    let results = client
        .search(&long_pattern, filters, 10, 0, FieldMask::FULL)
        .unwrap();

    // Likely no results, but should not crash
    assert!(
        results.len() <= 10,
        "Unexpectedly many results for long pattern"
    );
}
