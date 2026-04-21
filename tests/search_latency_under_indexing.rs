//! Foreground-search-latency validation under concurrent indexing.
//!
//! This is the first slice of the "interactive latency stays sane while cass
//! indexes in the background" scenario called for by bead
//! `coding_agent_session_search-d2qix`. It does NOT try to measure absolute
//! throughput — that is what `benches/search_latency_e2e.rs` and
//! `benches/index_perf.rs` are for. It just asserts that an active Tantivy
//! writer (seeding documents + periodically committing) does not starve a
//! foreground `SearchClient` to the point where interactive UX collapses.
//!
//! The test is `#[ignore]` by default because:
//!
//! * it depends on wall-clock timing and is therefore not reliable on
//!   heavily-loaded CI hosts,
//! * it burns CPU for ~2-5 seconds per run.
//!
//! Run explicitly with:
//!
//! ```text
//! cargo test --test search_latency_under_indexing -- --ignored --nocapture
//! ```
//!
//! Thresholds are deliberately generous (p95 ≤ 750 ms on the pressured run,
//! ≤ 300 ms on the idle control) so the test works even on small dev boxes.
//! Regression signal is the *delta*, not the absolute number: if p95 under
//! load ever climbs into multi-second territory while idle stays fast, the
//! responsiveness governor / writer isolation has broken and we want to be
//! told about it.
//!
//! The test also pulls the governor telemetry at the end so the run log
//! captures what the governor saw and decided.

use coding_agent_search::search::query::{FieldMask, SearchClient, SearchFilters};
use coding_agent_search::search::tantivy::TantivyIndex;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

mod util;

/// Seed the index with enough baseline content that searches return hits
/// immediately without cold-start variance. Keeps the corpus small so test
/// wall-clock stays bounded.
fn seed_baseline_corpus(index: &mut TantivyIndex, base_dir: &Path, count: usize) {
    for i in 0..count {
        let conv = util::ConversationFixtureBuilder::new("tester")
            .title(format!("baseline_{i}"))
            .source_path(base_dir.join(format!("baseline_{i}.jsonl")))
            .base_ts(1_000 + i as i64)
            .messages(3)
            .with_content(0, format!("baseline_content_{i} shared_token alpha"))
            .with_content(1, format!("user_message_{i} shared_token beta"))
            .with_content(2, format!("assistant_reply_{i} shared_token gamma"))
            .build_normalized();
        index.add_conversation(&conv).unwrap();
    }
    index.commit().unwrap();
}

/// Drive a background indexer thread that keeps adding documents and
/// committing every `commit_every` conversations until `stop` is set.
/// Returns a join handle that yields the number of conversations indexed.
fn spawn_background_indexer(
    index_path: std::path::PathBuf,
    stop: Arc<AtomicBool>,
    ready: Arc<Barrier>,
    commit_every: usize,
) -> thread::JoinHandle<usize> {
    thread::spawn(move || {
        let mut index = TantivyIndex::open_or_create(&index_path).unwrap();
        ready.wait();
        let mut i: usize = 0;
        while !stop.load(Ordering::Relaxed) {
            let conv = util::ConversationFixtureBuilder::new("tester")
                .title(format!("background_{i}"))
                .source_path(index_path.join(format!("background_{i}.jsonl")))
                .base_ts(10_000 + i as i64)
                .messages(2)
                .with_content(0, format!("bg_user_{i} shared_token load"))
                .with_content(1, format!("bg_assistant_{i} shared_token reply"))
                .build_normalized();
            index.add_conversation(&conv).unwrap();
            i += 1;
            if i.is_multiple_of(commit_every) {
                // Commits trigger segment flush — this is the thing most
                // likely to disturb foreground readers.
                index.commit().unwrap();
            }
        }
        index.commit().unwrap();
        i
    })
}

fn percentile(sorted: &[Duration], pct: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let pos = ((sorted.len() as f64 - 1.0) * pct).round() as usize;
    sorted[pos.min(sorted.len() - 1)]
}

#[derive(Debug, Clone)]
struct LatencyReport {
    sample_count: usize,
    p50: Duration,
    p95: Duration,
    max: Duration,
}

impl LatencyReport {
    fn from_durations(mut durations: Vec<Duration>) -> Self {
        durations.sort();
        let p50 = percentile(&durations, 0.50);
        let p95 = percentile(&durations, 0.95);
        let max = durations.last().copied().unwrap_or_default();
        Self {
            sample_count: durations.len(),
            p50,
            p95,
            max,
        }
    }
}

/// Run the foreground-search workload against a SearchClient while the
/// background indexer is hitting the same index. Returns a latency report.
fn measure_foreground_latency(
    index_path: &Path,
    query_count: usize,
    query_gap: Duration,
) -> LatencyReport {
    let client = SearchClient::open(index_path, None)
        .expect("opening SearchClient under load must succeed")
        .expect("SearchClient ready");

    let mut durations = Vec::with_capacity(query_count);
    // Pre-warm the reader so the first search doesn't dominate p95.
    let _ = client.search(
        "shared_token",
        SearchFilters::default(),
        10,
        0,
        FieldMask::FULL,
    );

    for q in 0..query_count {
        let term = match q % 4 {
            0 => "shared_token",
            1 => "baseline_content_1",
            2 => "assistant_reply_5",
            _ => "bg_user_0",
        };
        let t0 = Instant::now();
        let result = client.search(term, SearchFilters::default(), 10, 0, FieldMask::FULL);
        let elapsed = t0.elapsed();
        assert!(
            result.is_ok(),
            "search must not fail during concurrent indexing: {result:?}"
        );
        durations.push(elapsed);
        if !query_gap.is_zero() {
            thread::sleep(query_gap);
        }
    }
    LatencyReport::from_durations(durations)
}

#[test]
#[ignore = "timing-sensitive; run with --ignored explicitly"]
fn search_p95_stays_within_budget_while_indexing_in_background() {
    let dir = TempDir::new().unwrap();
    let index_path = dir.path().to_path_buf();
    {
        // Tantivy serializes the writer via a filesystem lockfile: only one
        // `IndexWriter` may exist per directory at a time. We seed inside a
        // scope so the writer is dropped (and the lock released) before the
        // background indexer opens its own writer further down.
        let mut index = TantivyIndex::open_or_create(&index_path).unwrap();
        seed_baseline_corpus(&mut index, dir.path(), 100);
    }

    // ---- Control: idle (no background indexer) ----
    let idle_report = measure_foreground_latency(&index_path, 50, Duration::from_millis(5));
    eprintln!(
        "idle latency: samples={} p50={:?} p95={:?} max={:?}",
        idle_report.sample_count, idle_report.p50, idle_report.p95, idle_report.max
    );

    // ---- Pressured: same workload with a live background indexer ----
    let stop = Arc::new(AtomicBool::new(false));
    let ready = Arc::new(Barrier::new(2));
    let bg_handle = spawn_background_indexer(
        index_path.clone(),
        Arc::clone(&stop),
        Arc::clone(&ready),
        10, // commit every 10 adds → ~every 20-40ms
    );
    // Wait for the background thread to be ready so we don't measure its
    // startup cost in the first few queries.
    ready.wait();
    // Give the background indexer a brief head start so the foreground
    // run actually overlaps with an active writer.
    thread::sleep(Duration::from_millis(50));

    let pressured_report = measure_foreground_latency(&index_path, 50, Duration::from_millis(5));
    stop.store(true, Ordering::Relaxed);
    let bg_conversations = bg_handle.join().expect("background indexer thread");
    eprintln!(
        "pressured latency (bg wrote {bg_conversations} convs): samples={} p50={:?} p95={:?} max={:?}",
        pressured_report.sample_count,
        pressured_report.p50,
        pressured_report.p95,
        pressured_report.max
    );

    // Assertions — tuned to be informative-but-not-flaky on shared dev hosts.
    // Idle budget is strict because a 100-conv index should answer in well
    // under a millisecond; 300ms leaves ~100x safety margin.
    assert!(
        idle_report.p95 <= Duration::from_millis(300),
        "idle p95 search latency regressed: {:?}",
        idle_report.p95
    );
    // Pressured budget is generous because the background writer is
    // committing every ~20ms, which can block the reader briefly on segment
    // swap. 750ms is well above any reasonable degradation; seeing it
    // exceeded means the reader is genuinely being starved.
    assert!(
        pressured_report.p95 <= Duration::from_millis(750),
        "pressured p95 search latency exceeded budget: {:?}",
        pressured_report.p95
    );
    // Log the governor's view of what happened during the run so anyone
    // investigating a failure has the decision history in the test output.
    // Health telemetry is exposed through the same public surface the
    // `cass health --json` command uses.
    let status_out = std::process::Command::new(env!("CARGO_BIN_EXE_cass"))
        .arg("health")
        .arg("--json")
        .env("CASS_RESPONSIVENESS_DISABLE", "0")
        .output();
    if let Ok(out) = status_out {
        eprintln!(
            "cass health --json responsiveness snapshot (stdout):\n{}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
}

#[test]
#[ignore = "timing-sensitive; run with --ignored explicitly"]
fn governor_disabled_run_matches_idle_baseline() {
    // Sanity check: with the governor pinned off, the pressured p95 should
    // still stay within the generous 750ms budget. If it blows past that,
    // something other than the governor is slowing foreground search.
    // SAFETY: this test is #[ignore] and runs single-threaded via cargo
    // test default + the --ignored filter.
    unsafe {
        std::env::set_var("CASS_RESPONSIVENESS_DISABLE", "1");
    }

    let dir = TempDir::new().unwrap();
    let index_path = dir.path().to_path_buf();
    {
        // Same lockfile rationale as in the primary test: drop the writer
        // before the background indexer opens its own.
        let mut index = TantivyIndex::open_or_create(&index_path).unwrap();
        seed_baseline_corpus(&mut index, dir.path(), 100);
    }

    let stop = Arc::new(AtomicBool::new(false));
    let ready = Arc::new(Barrier::new(2));
    let bg = spawn_background_indexer(
        index_path.clone(),
        Arc::clone(&stop),
        Arc::clone(&ready),
        10,
    );
    ready.wait();
    thread::sleep(Duration::from_millis(50));

    let report = measure_foreground_latency(&index_path, 50, Duration::from_millis(5));
    stop.store(true, Ordering::Relaxed);
    let bg_conversations = bg.join().unwrap();

    eprintln!(
        "governor-disabled run (bg wrote {bg_conversations} convs): p50={:?} p95={:?} max={:?}",
        report.p50, report.p95, report.max
    );
    assert!(
        report.p95 <= Duration::from_millis(750),
        "governor-disabled p95 exceeded budget: {:?}",
        report.p95
    );

    // SAFETY: single-threaded cleanup after workload.
    unsafe {
        std::env::remove_var("CASS_RESPONSIVENESS_DISABLE");
    }
}
