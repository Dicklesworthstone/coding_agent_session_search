//! Golden snapshots for metamorphic search invariants.
//!
//! Freezes the concrete output of the deterministic metamorphic test corpus
//! (3 agents × 5 conversations × 3 messages = 45 indexed messages) so any
//! change to search ranking, tokenization, or filtering surfaces as a golden
//! diff rather than a silent behavioral change.
//!
//! Regenerate:
//!   UPDATE_GOLDENS=1 cargo test --test golden_metamorphic_search
//!
//! Then review:
//!   git diff tests/golden/metamorphic/

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use coding_agent_search::search::query::{FieldMask, SearchClient, SearchFilters};
use coding_agent_search::search::tantivy::TantivyIndex;
use tempfile::TempDir;

mod util;

// ---------------------------------------------------------------------------
// Corpus (must exactly match metamorphic_search.rs to keep goldens coherent)
// ---------------------------------------------------------------------------

const AGENTS: &[&str] = &["claude", "codex", "amp"];

fn fixed_now_ms() -> i64 {
    1_768_435_200_000 // 2026-01-15 00:00:00 UTC
}

fn seed_corpus(index: &mut TantivyIndex, dir: &std::path::Path, now_ms: i64) {
    let day_ms: i64 = 86_400_000;
    for (agent_idx, &agent) in AGENTS.iter().enumerate() {
        for conv_idx in 0..5 {
            let age_days = (agent_idx * 5 + conv_idx) as i64 * 4;
            let ts = now_ms - age_days * day_ms;
            let unique = format!("{agent}_conv{conv_idx}");
            let conv = util::ConversationFixtureBuilder::new(agent)
                .title(format!("{agent} session {conv_idx}"))
                .source_path(dir.join(format!("{agent}/session_{conv_idx}.jsonl")))
                .base_ts(ts)
                .messages(3)
                .with_content(
                    0,
                    format!("metamorphic_sentinel {unique} async function alpha beta"),
                )
                .with_content(
                    1,
                    format!(
                        "metamorphic_sentinel {unique} refactoring the search layer gamma delta"
                    ),
                )
                .with_content(
                    2,
                    format!("metamorphic_sentinel {unique} debugging epsilon zeta"),
                )
                .build_normalized();
            index.add_conversation(&conv).unwrap();
        }
    }
    index.commit().unwrap();
}

// ---------------------------------------------------------------------------
// Golden infrastructure
// ---------------------------------------------------------------------------

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join("metamorphic")
}

fn assert_golden(name: &str, actual: &str) {
    let golden_path = golden_dir().join(name);

    if std::env::var("UPDATE_GOLDENS").is_ok() {
        std::fs::create_dir_all(golden_path.parent().unwrap()).expect("create golden dir");
        std::fs::write(&golden_path, actual).expect("write golden");
        eprintln!("[GOLDEN] Updated: {}", golden_path.display());
        return;
    }

    let expected = std::fs::read_to_string(&golden_path).unwrap_or_else(|err| {
        panic!(
            "Golden file missing: {}\n{err}\n\n\
             Run: UPDATE_GOLDENS=1 cargo test --test golden_metamorphic_search",
            golden_path.display(),
        )
    });

    if actual != expected {
        let actual_path = golden_path.with_extension("json.actual");
        std::fs::write(&actual_path, actual).expect("write .actual");
        panic!(
            "GOLDEN MISMATCH: {name}\n\
             Expected: {}\n\
             Actual:   {}\n\n\
             Regenerate: UPDATE_GOLDENS=1 cargo test --test golden_metamorphic_search",
            golden_path.display(),
            actual_path.display(),
        );
    }
}

// ---------------------------------------------------------------------------
// Golden: corpus shape
// ---------------------------------------------------------------------------

/// Snapshot the total hit count and per-agent breakdown for the sentinel query
/// that matches every document in the corpus.
#[test]
fn golden_corpus_shape() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();
    seed_corpus(&mut index, dir.path(), fixed_now_ms());

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");

    let all = client
        .search(
            "metamorphic_sentinel",
            SearchFilters::default(),
            200,
            0,
            FieldMask::FULL,
        )
        .unwrap();

    let mut per_agent: BTreeMap<String, usize> = BTreeMap::new();
    for hit in &all {
        *per_agent.entry(hit.agent.clone()).or_default() += 1;
    }

    let snapshot = serde_json::json!({
        "query": "metamorphic_sentinel",
        "total_hits": all.len(),
        "per_agent": per_agent,
    });

    assert_golden(
        "corpus_shape.json",
        &serde_json::to_string_pretty(&snapshot).unwrap(),
    );
}

// ---------------------------------------------------------------------------
// Golden: limit-prefix hit ordering
// ---------------------------------------------------------------------------

/// Snapshot the first 5 hits by source_path for limit=5 and limit=20 queries,
/// proving the limit=5 prefix relationship is stable.
#[test]
fn golden_limit_prefix_ordering() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();
    seed_corpus(&mut index, dir.path(), fixed_now_ms());

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");

    let small = client
        .search(
            "metamorphic_sentinel",
            SearchFilters::default(),
            5,
            0,
            FieldMask::FULL,
        )
        .unwrap();

    let large = client
        .search(
            "metamorphic_sentinel",
            SearchFilters::default(),
            20,
            0,
            FieldMask::FULL,
        )
        .unwrap();

    let dir_prefix = dir.path().to_string_lossy().to_string();
    let strip = |p: &str| -> String {
        p.strip_prefix(&dir_prefix)
            .unwrap_or(p)
            .trim_start_matches('/')
            .to_string()
    };

    let extract =
        |hits: &[coding_agent_search::search::query::SearchHit]| -> Vec<serde_json::Value> {
            let mut entries: Vec<_> = hits
                .iter()
                .map(|h| {
                    let rel = strip(&h.source_path);
                    (
                        rel.clone(),
                        h.agent.clone(),
                        h.line_number,
                        serde_json::json!({
                            "source_path": rel,
                            "agent": h.agent,
                            "line_number": h.line_number,
                        }),
                    )
                })
                .collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0).then(a.2.cmp(&b.2)));
            entries.into_iter().map(|e| e.3).collect()
        };

    let snapshot = serde_json::json!({
        "query": "metamorphic_sentinel",
        "limit_5_hits": extract(&small),
        "limit_20_hits": extract(&large),
        "limit_5_count": small.len(),
        "limit_20_count": large.len(),
    });

    assert_golden(
        "limit_prefix_ordering.json",
        &serde_json::to_string_pretty(&snapshot).unwrap(),
    );
}

// ---------------------------------------------------------------------------
// Golden: agent filter breakdown
// ---------------------------------------------------------------------------

/// Snapshot the per-agent filtered hit counts and verify union = total.
#[test]
fn golden_agent_filter_breakdown() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();
    seed_corpus(&mut index, dir.path(), fixed_now_ms());

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");

    let q = "metamorphic_sentinel";
    let limit = 200;

    let all = client
        .search(q, SearchFilters::default(), limit, 0, FieldMask::FULL)
        .unwrap();

    let mut agent_breakdown: BTreeMap<String, usize> = BTreeMap::new();
    let mut union_count = 0usize;

    for &agent in AGENTS {
        let mut filters = SearchFilters::default();
        filters.agents.insert(agent.to_string());
        let hits = client
            .search(q, filters, limit, 0, FieldMask::FULL)
            .unwrap();
        agent_breakdown.insert(agent.to_string(), hits.len());
        union_count += hits.len();
    }

    let snapshot = serde_json::json!({
        "query": q,
        "unfiltered_total": all.len(),
        "per_agent_filtered": agent_breakdown,
        "union_count": union_count,
        "union_equals_total": union_count == all.len(),
    });

    assert_golden(
        "agent_filter_breakdown.json",
        &serde_json::to_string_pretty(&snapshot).unwrap(),
    );
}

// ---------------------------------------------------------------------------
// Golden: days-filter staircase
// ---------------------------------------------------------------------------

/// Snapshot the hit counts for 7-day, 30-day, and unfiltered windows.
#[test]
fn golden_days_filter_staircase() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();
    let now = fixed_now_ms();
    seed_corpus(&mut index, dir.path(), now);

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");

    let q = "metamorphic_sentinel";
    let limit = 200;
    let day_ms: i64 = 86_400_000;

    let all = client
        .search(q, SearchFilters::default(), limit, 0, FieldMask::FULL)
        .unwrap();

    let filters_30 = SearchFilters {
        created_from: Some(now - 30 * day_ms),
        ..Default::default()
    };
    let hits_30 = client
        .search(q, filters_30, limit, 0, FieldMask::FULL)
        .unwrap();

    let filters_7 = SearchFilters {
        created_from: Some(now - 7 * day_ms),
        ..Default::default()
    };
    let hits_7 = client
        .search(q, filters_7, limit, 0, FieldMask::FULL)
        .unwrap();

    let set_7: HashSet<String> = hits_7.iter().map(|h| h.source_path.clone()).collect();
    let set_30: HashSet<String> = hits_30.iter().map(|h| h.source_path.clone()).collect();

    let snapshot = serde_json::json!({
        "query": q,
        "unfiltered_count": all.len(),
        "days_30_count": hits_30.len(),
        "days_7_count": hits_7.len(),
        "days_7_subset_of_30": set_7.is_subset(&set_30),
        "monotonic": hits_7.len() <= hits_30.len() && hits_30.len() <= all.len(),
    });

    assert_golden(
        "days_filter_staircase.json",
        &serde_json::to_string_pretty(&snapshot).unwrap(),
    );
}

// ---------------------------------------------------------------------------
// Golden: case invariance
// ---------------------------------------------------------------------------

/// Snapshot hit counts for lowercase/uppercase variants of several terms.
#[test]
fn golden_case_invariance() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();
    seed_corpus(&mut index, dir.path(), fixed_now_ms());

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");

    let limit = 100;
    let pairs = &[
        ("async", "ASYNC"),
        ("refactoring", "REFACTORING"),
        ("debugging", "DEBUGGING"),
    ];

    let mut results: Vec<serde_json::Value> = Vec::new();

    for &(lower, upper) in pairs {
        let hits_l = client
            .search(lower, SearchFilters::default(), limit, 0, FieldMask::FULL)
            .unwrap();
        let hits_u = client
            .search(upper, SearchFilters::default(), limit, 0, FieldMask::FULL)
            .unwrap();

        let set_l: HashSet<String> = hits_l.iter().map(|h| h.source_path.clone()).collect();
        let set_u: HashSet<String> = hits_u.iter().map(|h| h.source_path.clone()).collect();

        results.push(serde_json::json!({
            "lower": lower,
            "upper": upper,
            "lower_count": hits_l.len(),
            "upper_count": hits_u.len(),
            "sets_equal": set_l == set_u,
        }));
    }

    let snapshot = serde_json::json!({
        "case_pairs": results,
    });

    assert_golden(
        "case_invariance.json",
        &serde_json::to_string_pretty(&snapshot).unwrap(),
    );
}
