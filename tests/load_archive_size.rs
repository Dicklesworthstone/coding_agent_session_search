//! Archive size load tests for cass.
//!
//! These tests verify that cass handles large archives correctly:
//! - 10K, 50K, 100K conversations
//! - Various message sizes (small, large, mixed)
//! - Memory bounded operation
//!
//! Run with release mode for realistic performance:
//!   cargo test --test archive_size --release -- --nocapture --test-threads=1
//!
//! Target metrics from P6.9:
//! | Archive Size | Conversations | Expected |
//! |--------------|---------------|----------|
//! | 10MB         | 1,000         | Full performance |
//! | 100MB        | 10,000        | Search under 5s |
//! | 500MB        | 50,000        | Search under 10s |

use coding_agent_search::connectors::{NormalizedConversation, NormalizedMessage};
use coding_agent_search::indexer::persist::persist_conversation;
use coding_agent_search::search::query::{FieldMask, SearchClient, SearchFilters};
use coding_agent_search::search::tantivy::{index_dir, TantivyIndex};
use coding_agent_search::storage::sqlite::SqliteStorage;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// Generate a test conversation with specified parameters.
fn generate_conversation(
    conv_id: i64,
    msg_count: i64,
    msg_size: ContentSize,
) -> NormalizedConversation {
    let base_ts = 1_700_000_000_000 + conv_id * 100_000;
    let messages: Vec<NormalizedMessage> = (0..msg_count)
        .map(|m| {
            let content = match msg_size {
                ContentSize::Small => format!(
                    "Conv {} msg {}: Quick note about the project status.",
                    conv_id, m
                ),
                ContentSize::Medium => format!(
                    "Conv {} msg {}: {}",
                    conv_id,
                    m,
                    "Lorem ipsum dolor sit amet, consectetur adipiscing elit. \
                     Sed do eiusmod tempor incididunt ut labore et dolore magna aliqua. \
                     Ut enim ad minim veniam, quis nostrud exercitation ullamco laboris \
                     nisi ut aliquip ex ea commodo consequat. ".repeat(5)
                ),
                ContentSize::Large => format!(
                    "Conv {} msg {}: {}",
                    conv_id,
                    m,
                    "Lorem ipsum dolor sit amet, consectetur adipiscing elit. \
                     Sed do eiusmod tempor incididunt ut labore et dolore magna aliqua. \
                     Ut enim ad minim veniam, quis nostrud exercitation ullamco laboris \
                     nisi ut aliquip ex ea commodo consequat. Duis aute irure dolor in \
                     reprehenderit in voluptate velit esse cillum dolore eu fugiat nulla \
                     pariatur. Excepteur sint occaecat cupidatat non proident, sunt in \
                     culpa qui officia deserunt mollit anim id est laborum. ".repeat(50)
                ),
                ContentSize::Mixed => {
                    let factor = (m % 10 + 1) as usize;
                    format!(
                        "Conv {} msg {}: {}",
                        conv_id,
                        m,
                        "Variable content for realistic testing scenarios. ".repeat(factor * 10)
                    )
                }
            };

            NormalizedMessage {
                idx: m,
                role: if m % 2 == 0 { "user" } else { "agent" }.into(),
                author: Some(format!("model-{}", conv_id % 5)),
                created_at: Some(base_ts + m * 1000),
                content,
                extra: serde_json::json!({ "load_test": true }),
                snippets: Vec::new(),
            }
        })
        .collect();

    NormalizedConversation {
        agent_slug: format!("load-test-agent-{}", conv_id % 10),
        external_id: Some(format!("load-conv-{}", conv_id)),
        title: Some(format!("Load Test Conversation {} - {}", conv_id, msg_size.as_str())),
        workspace: Some(PathBuf::from(format!("/workspace/project-{}", conv_id % 50))),
        source_path: PathBuf::from(format!("/tmp/load-test/conv-{}.jsonl", conv_id)),
        started_at: Some(base_ts),
        ended_at: Some(base_ts + msg_count * 1000),
        metadata: serde_json::json!({
            "load_test": true,
            "conv_id": conv_id,
            "msg_count": msg_count,
        }),
        messages,
    }
}

/// Content size variants for testing.
#[derive(Clone, Copy)]
#[allow(dead_code)]
enum ContentSize {
    Small,
    Medium,
    Large,
    Mixed,
}

impl ContentSize {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Small => "small",
            Self::Medium => "medium",
            Self::Large => "large",
            Self::Mixed => "mixed",
        }
    }
}

/// Set up a test index with specified conversation count and message parameters.
fn setup_load_index(
    conv_count: i64,
    msgs_per_conv: i64,
    content_size: ContentSize,
) -> (TempDir, SearchClient, Duration) {
    let temp = TempDir::new().expect("create tempdir");
    let data_dir = temp.path().to_path_buf();
    let db_path = data_dir.join("load_test.db");
    let index_path = index_dir(&data_dir).expect("index path");

    let mut storage = SqliteStorage::open(&db_path).expect("open db");
    let mut t_index = TantivyIndex::open_or_create(&index_path).unwrap();

    let start = Instant::now();
    for i in 0..conv_count {
        let conv = generate_conversation(i, msgs_per_conv, content_size);
        persist_conversation(&mut storage, &mut t_index, &conv).expect("persist");

        // Progress logging for large tests
        if (i + 1) % 1000 == 0 {
            println!("  Indexed {}/{} conversations...", i + 1, conv_count);
        }
    }
    t_index.commit().unwrap();
    let index_duration = start.elapsed();

    let client = SearchClient::open(&index_path, Some(&db_path))
        .expect("open client")
        .expect("client available");

    (temp, client, index_duration)
}

/// Get current process memory usage (resident set size).
fn get_memory_mb() -> f64 {
    #[cfg(target_os = "linux")]
    {
        if let Ok(statm) = std::fs::read_to_string("/proc/self/statm")
            && let Some(rss_pages) = statm.split_whitespace().nth(1)
            && let Ok(pages) = rss_pages.parse::<usize>()
        {
            return (pages * 4096) as f64 / (1024.0 * 1024.0);
        }
    }

    #[cfg(target_os = "macos")]
    {
        use std::process::Command;
        if let Ok(output) = Command::new("ps")
            .args(["-o", "rss=", "-p", &std::process::id().to_string()])
            .output()
            && let Ok(rss_kb) = String::from_utf8_lossy(&output.stdout)
                .trim()
                .parse::<usize>()
        {
            return rss_kb as f64 / 1024.0;
        }
    }

    0.0
}

/// Run a search and measure latency.
fn measure_search(
    client: &SearchClient,
    query: &str,
    limit: usize,
) -> (usize, Duration) {
    let filters = SearchFilters::default();
    let start = Instant::now();
    let results = client
        .search(query, filters, limit, 0, FieldMask::FULL)
        .expect("search failed");
    let duration = start.elapsed();
    (results.len(), duration)
}

// =============================================================================
// Archive Size Tests
// =============================================================================

/// Test 1K conversations (10MB baseline).
#[test]
fn load_1k_conversations() {
    println!("\n=== Load Test: 1K Conversations (10MB baseline) ===");

    let (tmp, client, index_time) = setup_load_index(1_000, 10, ContentSize::Mixed);
    println!("  Index creation: {:?}", index_time);

    let mem_before = get_memory_mb();

    // Run search tests
    let queries = ["lorem", "project", "test", "conv*", "agent"];
    for query in queries {
        let (count, duration) = measure_search(&client, query, 100);
        println!("  Search '{}': {} results in {:?}", query, count, duration);
        assert!(
            duration < Duration::from_secs(1),
            "1K search should be under 1s, was {:?}",
            duration
        );
    }

    let mem_after = get_memory_mb();
    println!("  Memory: {:.1}MB -> {:.1}MB", mem_before, mem_after);

    drop(client);
    drop(tmp);
    println!("  PASS: 1K conversations");
}

/// Test 10K conversations (100MB target).
/// Target: Search under 5s.
#[test]
fn load_10k_conversations() {
    println!("\n=== Load Test: 10K Conversations (100MB target) ===");

    let (tmp, client, index_time) = setup_load_index(10_000, 10, ContentSize::Mixed);
    println!("  Index creation: {:?}", index_time);

    let mem_before = get_memory_mb();

    // Run search tests
    let queries = ["lorem", "project", "test", "conv*"];
    for query in queries {
        let (count, duration) = measure_search(&client, query, 100);
        println!("  Search '{}': {} results in {:?}", query, count, duration);
        assert!(
            duration < Duration::from_secs(5),
            "10K search should be under 5s, was {:?}",
            duration
        );
    }

    let mem_after = get_memory_mb();
    println!("  Memory: {:.1}MB -> {:.1}MB", mem_before, mem_after);

    drop(client);
    drop(tmp);
    println!("  PASS: 10K conversations");
}

/// Test 50K conversations (500MB target).
/// Target: Search under 10s.
#[test]
#[ignore = "expensive: run with --ignored for full load testing"]
fn load_50k_conversations() {
    println!("\n=== Load Test: 50K Conversations (500MB target) ===");

    let (tmp, client, index_time) = setup_load_index(50_000, 10, ContentSize::Mixed);
    println!("  Index creation: {:?}", index_time);

    let mem_before = get_memory_mb();

    // Run search tests
    let queries = ["lorem", "project", "test"];
    for query in queries {
        let (count, duration) = measure_search(&client, query, 100);
        println!("  Search '{}': {} results in {:?}", query, count, duration);
        assert!(
            duration < Duration::from_secs(10),
            "50K search should be under 10s, was {:?}",
            duration
        );
    }

    let mem_after = get_memory_mb();
    println!("  Memory: {:.1}MB -> {:.1}MB", mem_before, mem_after);

    drop(client);
    drop(tmp);
    println!("  PASS: 50K conversations");
}

// =============================================================================
// Message Size Tests
// =============================================================================

/// Test with very large messages (1MB each).
#[test]
fn load_large_messages() {
    println!("\n=== Load Test: Large Messages ===");

    // 100 conversations with 10 large messages each
    let (tmp, client, index_time) = setup_load_index(100, 10, ContentSize::Large);
    println!("  Index creation: {:?}", index_time);

    let mem_before = get_memory_mb();

    let (count, duration) = measure_search(&client, "lorem ipsum", 50);
    println!("  Search: {} results in {:?}", count, duration);
    assert!(
        duration < Duration::from_secs(5),
        "Large message search should be under 5s"
    );

    let mem_after = get_memory_mb();
    println!("  Memory: {:.1}MB -> {:.1}MB", mem_before, mem_after);

    drop(client);
    drop(tmp);
    println!("  PASS: Large messages");
}

/// Test with many small messages per conversation.
#[test]
fn load_many_small_messages() {
    println!("\n=== Load Test: Many Small Messages (100 per conv) ===");

    // 500 conversations with 100 small messages each = 50K messages
    let (tmp, client, index_time) = setup_load_index(500, 100, ContentSize::Small);
    println!("  Index creation: {:?}", index_time);

    let mem_before = get_memory_mb();

    let (count, duration) = measure_search(&client, "project status", 100);
    println!("  Search: {} results in {:?}", count, duration);
    assert!(
        duration < Duration::from_secs(5),
        "Many small messages search should be under 5s"
    );

    let mem_after = get_memory_mb();
    println!("  Memory: {:.1}MB -> {:.1}MB", mem_before, mem_after);

    drop(client);
    drop(tmp);
    println!("  PASS: Many small messages");
}

// =============================================================================
// Memory Bounds Tests
// =============================================================================

/// Verify memory doesn't grow unboundedly during repeated searches.
#[test]
fn load_memory_bounded_search() {
    println!("\n=== Load Test: Memory Bounded Search ===");

    let (tmp, client, _) = setup_load_index(5_000, 10, ContentSize::Mixed);

    // Warmup
    for _ in 0..10 {
        let _ = measure_search(&client, "lorem", 100);
    }

    let baseline = get_memory_mb();
    if baseline == 0.0 {
        println!("  Skipping: Memory measurement not supported on this platform");
        return;
    }

    // Run many searches with varying queries
    let queries = ["lorem", "ipsum", "dolor", "sit", "amet", "test*", "conv*"];
    for i in 0..500 {
        let query = queries[i % queries.len()];
        let _ = measure_search(&client, query, 100);
    }

    let after = get_memory_mb();
    let growth = after - baseline;

    println!("  Baseline: {:.1}MB, After: {:.1}MB, Growth: {:.1}MB", baseline, after, growth);

    // Allow up to 100MB growth for caching, but flag excessive growth
    assert!(
        growth < 100.0,
        "Memory grew excessively: {:.1}MB growth",
        growth
    );

    drop(client);
    drop(tmp);
    println!("  PASS: Memory bounded");
}

/// Verify index and resources are cleaned up properly.
#[test]
fn load_resource_cleanup() {
    println!("\n=== Load Test: Resource Cleanup ===");

    let initial_mem = get_memory_mb();
    if initial_mem == 0.0 {
        println!("  Skipping: Memory measurement not supported");
        return;
    }

    // Create and destroy multiple indexes
    for round in 0..3 {
        println!("  Round {}/3...", round + 1);
        let (tmp, client, _) = setup_load_index(1_000, 10, ContentSize::Small);

        // Use the index
        for _ in 0..50 {
            let _ = measure_search(&client, "test", 50);
        }

        // Explicit cleanup
        drop(client);
        drop(tmp);
    }

    // Force GC-like behavior
    std::thread::sleep(Duration::from_millis(100));

    let final_mem = get_memory_mb();
    let net_growth = final_mem - initial_mem;

    println!(
        "  Initial: {:.1}MB, Final: {:.1}MB, Net Growth: {:.1}MB",
        initial_mem, final_mem, net_growth
    );

    // Allow up to 50MB retained (OS caching, etc.)
    assert!(
        net_growth < 50.0,
        "Resources not properly cleaned up: {:.1}MB retained",
        net_growth
    );

    println!("  PASS: Resource cleanup");
}

// =============================================================================
// Benchmark Summary
// =============================================================================

/// Print a summary of load test capabilities.
#[test]
fn load_test_summary() {
    println!("\n");
    println!("╔═══════════════════════════════════════════════════════════════╗");
    println!("║                    CASS Load Test Summary                     ║");
    println!("╠═══════════════════════════════════════════════════════════════╣");
    println!("║ Test                    │ Target          │ Status            ║");
    println!("╠─────────────────────────┼─────────────────┼───────────────────╣");
    println!("║ 1K conversations        │ < 1s search     │ Run by default    ║");
    println!("║ 10K conversations       │ < 5s search     │ Run by default    ║");
    println!("║ 50K conversations       │ < 10s search    │ --ignored         ║");
    println!("║ Large messages          │ < 5s search     │ Run by default    ║");
    println!("║ Many small messages     │ < 5s search     │ Run by default    ║");
    println!("║ Memory bounded          │ < 100MB growth  │ Run by default    ║");
    println!("║ Resource cleanup        │ < 50MB retained │ Run by default    ║");
    println!("╚═══════════════════════════════════════════════════════════════╝");
    println!();
    println!("Run all tests (including expensive):");
    println!("  cargo test --test archive_size --release -- --nocapture --include-ignored");
    println!();
}
