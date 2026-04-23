//! Metamorphic Search Tests (tst.srch.meta)
//!
//! Metamorphic testing verifies input-output *relationships* rather than
//! exact expected values.  Each test encodes a metamorphic relation (MR)
//! that must hold for every corpus; a violation IS a bug.
//!
//! MR1 – Search idempotence
//! MR2 – Limit prefix monotonicity
//! MR3 – Agent-filter union completeness
//! MR4 – Reindex idempotence (doc counts stable across double-index)
//! MR5 – Days-filter subset ordering
//! MR6 – Case invariance

use std::collections::HashSet;

use coding_agent_search::search::query::{FieldMask, SearchClient, SearchFilters};
use coding_agent_search::search::tantivy::TantivyIndex;
use tempfile::TempDir;

mod util;

// ---------------------------------------------------------------------------
// Corpus construction
// ---------------------------------------------------------------------------

/// Agents used in the test corpus.  Distinct slugs so agent-filter union
/// test can enumerate all of them.
const AGENTS: &[&str] = &["claude", "codex", "amp"];

/// Build a deterministic multi-agent corpus in `index`.
///
/// Layout:
///   - 3 agents × 5 conversations each = 15 conversations
///   - Each conversation has 3 messages containing the agent name + unique terms
///   - Timestamps span a 60-day window ending at `now_ms`
///   - A shared keyword "metamorphic_sentinel" appears in every conversation
///     so we always have a non-empty result set to reason about.
fn seed_corpus(index: &mut TantivyIndex, dir: &std::path::Path, now_ms: i64) {
    let day_ms: i64 = 86_400_000;

    for (agent_idx, &agent) in AGENTS.iter().enumerate() {
        for conv_idx in 0..5 {
            let age_days = (agent_idx * 5 + conv_idx) as i64 * 4; // 0,4,8,...56 days ago
            let ts = now_ms - age_days * day_ms;
            let unique = format!("{agent}_conv{conv_idx}");

            let conv = util::ConversationFixtureBuilder::new(agent)
                .title(format!("{agent} session {conv_idx}"))
                .source_path(dir.join(format!("{agent}/session_{conv_idx}.jsonl")))
                .base_ts(ts)
                .messages(3)
                .with_content(
                    0,
                    format!(
                        "metamorphic_sentinel {unique} async function alpha beta"
                    ),
                )
                .with_content(
                    1,
                    format!(
                        "metamorphic_sentinel {unique} refactoring the search layer gamma delta"
                    ),
                )
                .with_content(
                    2,
                    format!(
                        "metamorphic_sentinel {unique} debugging epsilon zeta"
                    ),
                )
                .build_normalized();

            index.add_conversation(&conv).unwrap();
        }
    }
    index.commit().unwrap();
}

/// Stable "now" timestamp (ms) used across tests so age-based filters are
/// deterministic regardless of wall-clock time.
fn fixed_now_ms() -> i64 {
    // 2026-01-15 00:00:00 UTC
    1_768_435_200_000
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Canonical identity of a search hit for set comparisons.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct HitKey {
    source_path: String,
    line_number: Option<usize>,
}

impl From<&coding_agent_search::search::query::SearchHit> for HitKey {
    fn from(h: &coding_agent_search::search::query::SearchHit) -> Self {
        Self {
            source_path: h.source_path.clone(),
            line_number: h.line_number,
        }
    }
}

fn hit_keys(
    hits: &[coding_agent_search::search::query::SearchHit],
) -> Vec<HitKey> {
    hits.iter().map(HitKey::from).collect()
}

fn hit_key_set(
    hits: &[coding_agent_search::search::query::SearchHit],
) -> HashSet<HitKey> {
    hits.iter().map(HitKey::from).collect()
}

// ---------------------------------------------------------------------------
// MR1 – Search idempotence
// ---------------------------------------------------------------------------

/// Running the same query twice on an unchanged index must return identical
/// result tuples (source_path, line_number) in the same order and with the
/// same total count.
#[test]
fn mr1_search_idempotence() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();
    seed_corpus(&mut index, dir.path(), fixed_now_ms());

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("search client");

    let queries = &[
        "metamorphic_sentinel",
        "async",
        "refactoring",
        "claude_conv0",
    ];

    for &q in queries {
        let run1 = client
            .search(q, SearchFilters::default(), 50, 0, FieldMask::FULL)
            .unwrap();
        let run2 = client
            .search(q, SearchFilters::default(), 50, 0, FieldMask::FULL)
            .unwrap();

        let keys1 = hit_keys(&run1);
        let keys2 = hit_keys(&run2);

        assert_eq!(
            keys1, keys2,
            "MR1 violated: query {q:?} returned different hit tuples on second run"
        );
        assert_eq!(
            run1.len(),
            run2.len(),
            "MR1 violated: query {q:?} total count changed ({} vs {})",
            run1.len(),
            run2.len()
        );
    }
}

// ---------------------------------------------------------------------------
// MR2 – Limit prefix monotonicity
// ---------------------------------------------------------------------------

/// `search(q, limit=N)` must be a prefix of `search(q, limit=M)` when M > N,
/// both ranked by the same scoring function.  In other words, expanding the
/// limit must not reorder or drop earlier results.
#[test]
fn mr2_limit_prefix() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();
    seed_corpus(&mut index, dir.path(), fixed_now_ms());

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("search client");

    let queries = &["metamorphic_sentinel", "async", "debugging"];

    for &q in queries {
        let small = client
            .search(q, SearchFilters::default(), 5, 0, FieldMask::FULL)
            .unwrap();
        let large = client
            .search(q, SearchFilters::default(), 20, 0, FieldMask::FULL)
            .unwrap();

        // The small result set must be a prefix of the large one.
        assert!(
            small.len() <= large.len(),
            "MR2 violated: limit=5 returned {} hits but limit=20 returned {} for {q:?}",
            small.len(),
            large.len()
        );

        let small_keys = hit_keys(&small);
        let large_keys = hit_keys(&large);

        for (i, sk) in small_keys.iter().enumerate() {
            assert_eq!(
                sk, &large_keys[i],
                "MR2 violated: hit at position {i} differs between limit=5 and limit=20 for {q:?}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// MR3 – Agent-filter union completeness
// ---------------------------------------------------------------------------

/// The union of per-agent filtered results must equal the unfiltered result
/// set.  This catches bugs where a filter silently drops documents or where
/// the unfiltered path includes ghosts from a different agent namespace.
#[test]
fn mr3_agent_filter_union() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();
    seed_corpus(&mut index, dir.path(), fixed_now_ms());

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("search client");

    let q = "metamorphic_sentinel";
    let limit = 100; // large enough to capture everything

    // Unfiltered
    let all_hits = client
        .search(q, SearchFilters::default(), limit, 0, FieldMask::FULL)
        .unwrap();
    let all_set = hit_key_set(&all_hits);

    // Per-agent
    let mut union_set: HashSet<HitKey> = HashSet::new();
    for &agent in AGENTS {
        let mut filters = SearchFilters::default();
        filters.agents.insert(agent.to_string());
        let agent_hits = client
            .search(q, filters, limit, 0, FieldMask::FULL)
            .unwrap();

        // Every hit from this agent-filtered search should also be in the unfiltered set
        for hk in hit_key_set(&agent_hits) {
            assert!(
                all_set.contains(&hk),
                "MR3 violated: agent={agent} returned hit {hk:?} absent from unfiltered results"
            );
        }

        // Also verify that each returned hit's agent field matches the filter
        for hit in &agent_hits {
            assert_eq!(
                hit.agent, agent,
                "MR3 violated: agent filter {agent} returned hit with agent={:?}",
                hit.agent
            );
        }

        union_set.extend(hit_key_set(&agent_hits));
    }

    // The union should cover the unfiltered set exactly
    let missing: Vec<_> = all_set.difference(&union_set).collect();
    assert!(
        missing.is_empty(),
        "MR3 violated: {} hits in unfiltered results are missing from the per-agent union: {missing:?}",
        missing.len()
    );
}

// ---------------------------------------------------------------------------
// MR4 – Reindex idempotence
// ---------------------------------------------------------------------------

/// Indexing the same corpus twice (delete-all + re-add) must produce the same
/// document count.  This catches off-by-one bugs in the indexer's commit/merge
/// pipeline.
#[test]
fn mr4_reindex_idempotence() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();

    // First index
    seed_corpus(&mut index, dir.path(), fixed_now_ms());

    let client1 = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client after first index");
    let hits1 = client1
        .search(
            "metamorphic_sentinel",
            SearchFilters::default(),
            200,
            0,
            FieldMask::FULL,
        )
        .unwrap();
    let count1 = hits1.len();
    let keys1 = hit_key_set(&hits1);
    drop(client1);

    // Delete all and reindex the same corpus
    index.delete_all().unwrap();
    index.commit().unwrap();
    seed_corpus(&mut index, dir.path(), fixed_now_ms());

    let client2 = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client after reindex");
    let hits2 = client2
        .search(
            "metamorphic_sentinel",
            SearchFilters::default(),
            200,
            0,
            FieldMask::FULL,
        )
        .unwrap();
    let count2 = hits2.len();
    let keys2 = hit_key_set(&hits2);

    assert_eq!(
        count1, count2,
        "MR4 violated: document count changed after reindex ({count1} vs {count2})"
    );
    assert_eq!(
        keys1, keys2,
        "MR4 violated: hit key set changed after reindex"
    );
}

// ---------------------------------------------------------------------------
// MR5 – Days-filter subset ordering
// ---------------------------------------------------------------------------

/// `search(q, days=7) ⊆ search(q, days=30) ⊆ search(q)` (no days filter).
/// Widening the time window must only add results, never remove them.
#[test]
fn mr5_days_filter_subset() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();
    let now = fixed_now_ms();
    seed_corpus(&mut index, dir.path(), now);

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("search client");

    let q = "metamorphic_sentinel";
    let limit = 200;

    let day_ms: i64 = 86_400_000;

    // No time filter
    let all = client
        .search(q, SearchFilters::default(), limit, 0, FieldMask::FULL)
        .unwrap();
    let all_set = hit_key_set(&all);

    // 30-day window
    let mut filters_30 = SearchFilters::default();
    filters_30.created_from = Some(now - 30 * day_ms);
    let hits_30 = client
        .search(q, filters_30, limit, 0, FieldMask::FULL)
        .unwrap();
    let set_30 = hit_key_set(&hits_30);

    // 7-day window
    let mut filters_7 = SearchFilters::default();
    filters_7.created_from = Some(now - 7 * day_ms);
    let hits_7 = client
        .search(q, filters_7, limit, 0, FieldMask::FULL)
        .unwrap();
    let set_7 = hit_key_set(&hits_7);

    // 7-day ⊆ 30-day
    let leaked_from_7: Vec<_> = set_7.difference(&set_30).collect();
    assert!(
        leaked_from_7.is_empty(),
        "MR5 violated: {} hits in 7-day results are missing from 30-day results: {leaked_from_7:?}",
        leaked_from_7.len()
    );

    // 30-day ⊆ all
    let leaked_from_30: Vec<_> = set_30.difference(&all_set).collect();
    assert!(
        leaked_from_30.is_empty(),
        "MR5 violated: {} hits in 30-day results are missing from unfiltered results: {leaked_from_30:?}",
        leaked_from_30.len()
    );

    // Monotonicity: narrower window should have fewer or equal results
    assert!(
        set_7.len() <= set_30.len(),
        "MR5 violated: 7-day ({}) has more results than 30-day ({})",
        set_7.len(),
        set_30.len()
    );
    assert!(
        set_30.len() <= all_set.len(),
        "MR5 violated: 30-day ({}) has more results than unfiltered ({})",
        set_30.len(),
        all_set.len()
    );
}

// ---------------------------------------------------------------------------
// MR6 – Case invariance
// ---------------------------------------------------------------------------

/// Tantivy's default tokenizer lowercases terms during both indexing and
/// querying.  Therefore `search("async")` and `search("ASYNC")` and
/// `search("Async")` must return the same result set (modulo score ordering
/// if the scoring function is case-sensitive, which it shouldn't be).
#[test]
fn mr6_case_invariance() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();
    seed_corpus(&mut index, dir.path(), fixed_now_ms());

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("search client");

    let limit = 100;

    let cases = &[
        ("async", "ASYNC"),
        ("async", "Async"),
        ("refactoring", "REFACTORING"),
        ("debugging", "DEBUGGING"),
        ("metamorphic_sentinel", "METAMORPHIC_SENTINEL"),
    ];

    for &(lower, upper) in cases {
        let hits_lower = client
            .search(
                lower,
                SearchFilters::default(),
                limit,
                0,
                FieldMask::FULL,
            )
            .unwrap();
        let hits_upper = client
            .search(
                upper,
                SearchFilters::default(),
                limit,
                0,
                FieldMask::FULL,
            )
            .unwrap();

        let set_lower = hit_key_set(&hits_lower);
        let set_upper = hit_key_set(&hits_upper);

        assert_eq!(
            set_lower, set_upper,
            "MR6 violated: {lower:?} vs {upper:?} returned different result sets \
             ({} vs {} hits)",
            set_lower.len(),
            set_upper.len()
        );
    }
}

// ---------------------------------------------------------------------------
// MR7 – Offset pagination consistency
// ---------------------------------------------------------------------------

/// Paginating through results via offset must yield the same hits as fetching
/// all at once.  `search(q, limit=5, offset=0) ++ search(q, limit=5, offset=5)`
/// must equal `search(q, limit=10, offset=0)`.
#[test]
fn mr7_offset_pagination_consistency() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();
    seed_corpus(&mut index, dir.path(), fixed_now_ms());

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("search client");

    let q = "metamorphic_sentinel";

    let page1 = client
        .search(q, SearchFilters::default(), 5, 0, FieldMask::FULL)
        .unwrap();
    let page2 = client
        .search(q, SearchFilters::default(), 5, 5, FieldMask::FULL)
        .unwrap();
    let all = client
        .search(q, SearchFilters::default(), 10, 0, FieldMask::FULL)
        .unwrap();

    let mut paginated_keys: Vec<HitKey> = hit_keys(&page1);
    paginated_keys.extend(hit_keys(&page2));

    let all_keys = hit_keys(&all);

    assert_eq!(
        paginated_keys.len(),
        all_keys.len(),
        "MR7 violated: paginated ({}) vs bulk ({}) hit count differs",
        paginated_keys.len(),
        all_keys.len()
    );

    assert_eq!(
        paginated_keys, all_keys,
        "MR7 violated: paginated results differ from bulk fetch"
    );
}

// ---------------------------------------------------------------------------
// MR8 – Agent filter exclusivity
// ---------------------------------------------------------------------------

/// Filtering by one agent must return a disjoint set from filtering by a
/// different agent.  No hit should appear under two agent slugs.
#[test]
fn mr8_agent_filter_exclusivity() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();
    seed_corpus(&mut index, dir.path(), fixed_now_ms());

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("search client");

    let q = "metamorphic_sentinel";
    let limit = 100;

    let mut per_agent: Vec<(String, HashSet<HitKey>)> = Vec::new();

    for &agent in AGENTS {
        let mut filters = SearchFilters::default();
        filters.agents.insert(agent.to_string());
        let hits = client
            .search(q, filters, limit, 0, FieldMask::FULL)
            .unwrap();
        per_agent.push((agent.to_string(), hit_key_set(&hits)));
    }

    for i in 0..per_agent.len() {
        for j in (i + 1)..per_agent.len() {
            let overlap: Vec<_> = per_agent[i]
                .1
                .intersection(&per_agent[j].1)
                .collect();
            assert!(
                overlap.is_empty(),
                "MR8 violated: agents {:?} and {:?} share {} hits: {overlap:?}",
                per_agent[i].0,
                per_agent[j].0,
                overlap.len()
            );
        }
    }
}
