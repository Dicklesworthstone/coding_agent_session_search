//! Memory profiling tests for cass.
//!
//! These tests verify that repeated operations don't leak memory.
//!
//! IMPORTANT: Run with --test-threads=1 to avoid measurement interference:
//!   cargo test --test memory_tests --release -- --nocapture --test-threads=1
//!
//! For detailed profiling, use heaptrack:
//!   heaptrack cargo test --test memory_tests --release
//!   heaptrack_gui heaptrack.*.zst

use coding_agent_search::connectors::{NormalizedConversation, NormalizedMessage};
use coding_agent_search::indexer::persist::persist_conversation;
use coding_agent_search::search::query::{FieldMask, SearchClient, SearchFilters};
use coding_agent_search::search::tantivy::{TantivyIndex, index_dir};
use coding_agent_search::storage::sqlite::SqliteStorage;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use tempfile::TempDir;

/// These tests use RSS-based assertions and should not run concurrently.
static MEMORY_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn memory_test_guard() -> std::sync::MutexGuard<'static, ()> {
    MEMORY_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Generate a sample conversation for testing.
fn sample_conv(i: i64, msgs: i64) -> NormalizedConversation {
    let mut messages = Vec::new();
    for m in 0..msgs {
        messages.push(NormalizedMessage {
            idx: m,
            role: if m % 2 == 0 { "user" } else { "agent" }.into(),
            author: None,
            created_at: Some(1_700_000_000_000 + (i * 10 + m)),
            content: format!(
                "conversation {i} message {m} lorem ipsum dolor sit amet \
                 consectetur adipiscing elit sed do eiusmod tempor"
            ),
            extra: serde_json::json!({}),
            snippets: Vec::new(),
            invocations: Vec::new(),
        });
    }
    NormalizedConversation {
        agent_slug: "memory-test-agent".into(),
        external_id: Some(format!("mem-conv-{i}")),
        title: Some(format!("Memory Test Conversation {i}")),
        workspace: Some(PathBuf::from("/tmp/workspace")),
        source_path: PathBuf::from(format!("/tmp/memory-test/conv-{i}.jsonl")),
        started_at: Some(1_700_000_000_000),
        ended_at: Some(1_700_000_000_000 + msgs),
        metadata: serde_json::json!({ "memory_test": true, "i": i }),
        messages,
    }
}

/// Set up a test index with sample data.
fn setup_test_index(conv_count: i64, msgs_per_conv: i64) -> (TempDir, SearchClient) {
    let temp = TempDir::new().expect("create tempdir");
    let data_dir = temp.path().to_path_buf();
    let db_path = data_dir.join("memory_test.db");
    let index_path = index_dir(&data_dir).expect("index path");

    let storage = SqliteStorage::open(&db_path).expect("open db");
    let mut t_index = TantivyIndex::open_or_create(&index_path).unwrap();

    for i in 0..conv_count {
        let conv = sample_conv(i, msgs_per_conv);
        persist_conversation(&storage, &mut t_index, &conv).expect("persist");
    }
    t_index.commit().unwrap();

    let client = SearchClient::open(&index_path, Some(&db_path))
        .expect("open client")
        .expect("client available");

    (temp, client)
}

/// Get current process memory usage (resident set size).
/// Returns 0 on unsupported platforms.
fn get_process_memory_bytes() -> usize {
    #[cfg(target_os = "linux")]
    {
        // Read /proc/self/statm: VmSize VmRSS VmShared ...
        // Second field is RSS in pages
        if let Ok(statm) = std::fs::read_to_string("/proc/self/statm")
            && let Some(rss_pages) = statm.split_whitespace().nth(1)
            && let Ok(pages) = rss_pages.parse::<usize>()
        {
            return pages * 4096; // Assume 4KB pages
        }
        0
    }

    #[cfg(target_os = "macos")]
    {
        use std::process::Command;
        // Use ps to get RSS in KB
        Command::new("ps")
            .args(["-o", "rss=", "-p", &std::process::id().to_string()])
            .output()
            .ok()
            .and_then(|output| {
                String::from_utf8_lossy(&output.stdout)
                    .trim()
                    .parse::<usize>()
                    .ok()
            })
            .map(|rss_kb| rss_kb * 1024)
            .unwrap_or(0)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        0
    }
}

/// Test that repeated searches don't leak memory.
///
/// This test runs many searches and verifies that memory usage doesn't
/// grow unboundedly. Some growth is acceptable due to caching.
#[test]
#[ignore = "RSS profiling test; run manually with `cargo test --release --test memory_tests -- --nocapture --test-threads=1`"]
fn test_search_memory_no_leak() {
    let _guard = memory_test_guard();
    // Create index with 100 conversations
    let (_tmp, client) = setup_test_index(100, 10);
    let filters = SearchFilters::default();

    // Warm up - run a few searches to initialize caches
    for _ in 0..10 {
        let _ = client.search("lorem", filters.clone(), 20, 0, FieldMask::FULL);
    }

    // Get baseline memory after warmup
    let baseline = get_process_memory_bytes();

    // Skip test on platforms where we can't measure memory
    if baseline == 0 {
        println!("Skipping memory test - platform doesn't support memory measurement");
        return;
    }

    // Run many searches
    for i in 0..500 {
        let query = if i % 3 == 0 {
            "lorem"
        } else if i % 3 == 1 {
            "ipsum"
        } else {
            "dolor"
        };
        let _ = client.search(query, filters.clone(), 20, 0, FieldMask::FULL);
    }

    let after = get_process_memory_bytes();
    let growth = after.saturating_sub(baseline);

    // Allow up to 50MB growth (for caches, etc.)
    // This is generous but catches true leaks
    let max_allowed_growth = 50 * 1024 * 1024; // 50MB

    println!(
        "Memory: baseline={:.2}MB, after={:.2}MB, growth={:.2}MB",
        baseline as f64 / 1_048_576.0,
        after as f64 / 1_048_576.0,
        growth as f64 / 1_048_576.0
    );

    assert!(
        growth < max_allowed_growth,
        "Memory grew by {:.2}MB during search loop (max allowed: {:.2}MB). \
         This may indicate a memory leak.",
        growth as f64 / 1_048_576.0,
        max_allowed_growth as f64 / 1_048_576.0
    );
}

/// Test that repeated indexing operations don't leak memory.
#[test]
#[ignore = "RSS profiling test; run manually with `cargo test --release --test memory_tests -- --nocapture --test-threads=1`"]
fn test_indexing_memory_no_leak() {
    let _guard = memory_test_guard();
    let temp = TempDir::new().expect("tempdir");
    let data_dir = temp.path().to_path_buf();
    let db_path = data_dir.join("memory_index_test.db");
    let index_path = index_dir(&data_dir).expect("index path");

    let storage = SqliteStorage::open(&db_path).expect("open db");
    let mut t_index = TantivyIndex::open_or_create(&index_path).unwrap();

    // Warm up
    for i in 0..5 {
        let conv = sample_conv(i, 5);
        persist_conversation(&storage, &mut t_index, &conv).expect("persist");
    }
    t_index.commit().unwrap();

    let baseline = get_process_memory_bytes();

    if baseline == 0 {
        println!("Skipping memory test - platform doesn't support memory measurement");
        return;
    }

    // Index many conversations
    for i in 5..105 {
        let conv = sample_conv(i, 10);
        persist_conversation(&storage, &mut t_index, &conv).expect("persist");

        // Commit periodically
        if i % 20 == 0 {
            t_index.commit().unwrap();
        }
    }
    t_index.commit().unwrap();

    let after = get_process_memory_bytes();
    let growth = after.saturating_sub(baseline);

    // Allow up to 100MB growth for indexing (more data = more legitimate memory use)
    let max_allowed_growth = 100 * 1024 * 1024; // 100MB

    println!(
        "Indexing memory: baseline={:.2}MB, after={:.2}MB, growth={:.2}MB",
        baseline as f64 / 1_048_576.0,
        after as f64 / 1_048_576.0,
        growth as f64 / 1_048_576.0
    );

    assert!(
        growth < max_allowed_growth,
        "Memory grew by {:.2}MB during indexing (max allowed: {:.2}MB). \
         This may indicate a memory leak.",
        growth as f64 / 1_048_576.0,
        max_allowed_growth as f64 / 1_048_576.0
    );
}

/// Test that vector search operations don't leak memory.
#[test]
#[ignore = "RSS profiling test; run manually with `cargo test --release --test memory_tests -- --nocapture --test-threads=1`"]
fn test_vector_search_memory_no_leak() {
    let _guard = memory_test_guard();
    use coding_agent_search::search::vector_index::{Quantization, SemanticDocId, VectorIndex};

    let dimension = 384;
    let count = 10_000;

    fn normalize_in_place(vec: &mut [f32]) {
        let norm_sq: f32 = vec.iter().map(|v| v * v).sum();
        let norm = norm_sq.sqrt();
        if norm > 0.0 {
            for v in vec {
                *v /= norm;
            }
        }
    }

    // Build on-disk index (FSVI) so memory measurements reflect real behavior.
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("mem.fsvi");
    let mut writer = VectorIndex::create_with_revision(
        &path,
        "test-embedder",
        "rev1",
        dimension,
        Quantization::F16,
    )
    .expect("create writer");

    let mut vec_buf = vec![0.0f32; dimension];
    for idx in 0..count {
        for (d, slot) in vec_buf.iter_mut().enumerate() {
            *slot = ((idx + d * 31) % 997) as f32 / 997.0;
        }
        normalize_in_place(&mut vec_buf);
        let doc_id = SemanticDocId {
            message_id: idx as u64,
            chunk_idx: 0,
            agent_id: (idx % 8) as u32,
            workspace_id: 1,
            source_id: 1,
            role: 1,
            created_at_ms: idx as i64,
            content_hash: None,
        }
        .to_doc_id_string();
        writer
            .write_record(&doc_id, &vec_buf)
            .expect("write_record");
    }
    writer.finish().expect("finish");

    let index = VectorIndex::open(&path).expect("open");

    // Generate query vector
    let mut query: Vec<f32> = (0..dimension).map(|d| (d % 17) as f32 / 17.0).collect();
    normalize_in_place(&mut query);

    // Warm up
    for _ in 0..10 {
        let _ = index.search_top_k(&query, 25, None);
    }

    let baseline = get_process_memory_bytes();

    if baseline == 0 {
        println!("Skipping memory test - platform doesn't support memory measurement");
        return;
    }

    // Run many vector searches
    for _ in 0..500 {
        let _ = index.search_top_k(&query, 25, None);
    }

    let after = get_process_memory_bytes();
    let growth = after.saturating_sub(baseline);

    // Allow up to 20MB growth (vector search should be very memory-stable)
    let max_allowed_growth = 20 * 1024 * 1024; // 20MB

    println!(
        "Vector search memory: baseline={:.2}MB, after={:.2}MB, growth={:.2}MB",
        baseline as f64 / 1_048_576.0,
        after as f64 / 1_048_576.0,
        growth as f64 / 1_048_576.0
    );

    assert!(
        growth < max_allowed_growth,
        "Memory grew by {:.2}MB during vector search loop (max allowed: {:.2}MB). \
         This may indicate a memory leak.",
        growth as f64 / 1_048_576.0,
        max_allowed_growth as f64 / 1_048_576.0
    );
}
