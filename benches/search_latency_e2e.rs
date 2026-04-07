//! End-to-end search latency benchmark for the README sub-60ms claim.
//!
//! This benchmarks the real `SearchClient` execution path against a realistic
//! 1000-conversation / 24k-message corpus.

mod bench_utils;

use anyhow::{Context, Result};
use bench_utils::configure_criterion;
use coding_agent_search::connectors::{NormalizedConversation, NormalizedMessage};
use coding_agent_search::search::query::{
    CacheStats, FieldMask, SearchClient, SearchClientOptions, SearchFilters, SearchHit,
    SearchResult as BackendSearchResult,
};
use coding_agent_search::search::tantivy::{TantivyIndex, index_dir};
use coding_agent_search::ui::app::RankingMode;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::cmp::Ordering;
use std::hint::black_box;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tempfile::TempDir;

const CONVERSATION_COUNT: usize = 1_000;
const SEARCH_LIMIT: usize = 25;
const SPARSE_THRESHOLD: usize = 3;
const MESSAGES_PER_CONVERSATION: usize = 24;
const WARM_SAMPLES: usize = 12;
const PREFIX_SEQUENCES: usize = 6;
const TYPICAL_P95_BUDGET_MS: f64 = 60.0;
const FILTER_OVERHEAD_MAX_RATIO: f64 = 2.0;

const EXACT_QUERY: &str = "frankensqlite write conflict";
const PHRASE_QUERY: &str = "\"distributed tracing handshake\"";
const WILDCARD_QUERY: &str = "*token*";
const PREFIX_SEQUENCE: [&str; 4] = ["a", "au", "aut", "auth"];

const COMPONENTS: &[&str] = &[
    "authentication",
    "indexer",
    "workspace",
    "connector",
    "timeline",
    "export",
    "analytics",
    "search",
];

const OPERATIONS: &[&str] = &[
    "cache invalidation",
    "prefix refinement",
    "vector warming",
    "result formatting",
    "remote sync",
    "query explanation",
    "index rebuild",
    "error recovery",
];

struct FixtureConversation {
    normalized: NormalizedConversation,
}

struct SearchFixture {
    _temp: TempDir,
    _index_path: PathBuf,
    client: SearchClient,
    label: &'static str,
    total_messages: usize,
    filtered_agent: String,
    filtered_workspace: String,
    filtered_from: i64,
    filtered_to: i64,
}

#[derive(Debug, Clone)]
struct LatencySummary {
    p50_ms: f64,
    p95_ms: f64,
    p99_ms: f64,
    max_ms: f64,
    hit_count: usize,
    cache_hits_delta: u64,
    cache_miss_delta: u64,
    cache_shortfall_delta: u64,
}

fn interactive_field_mask() -> FieldMask {
    FieldMask::new(false, true, true, true)
}

fn bench_client_options() -> SearchClientOptions {
    SearchClientOptions {
        enable_reload: false,
        enable_warm: false,
    }
}

fn default_filters() -> SearchFilters {
    SearchFilters::default()
}

fn filtered_search_filters(fixture: &SearchFixture) -> SearchFilters {
    let mut filters = SearchFilters::default();
    filters.agents.insert(fixture.filtered_agent.clone());
    filters
        .workspaces
        .insert(fixture.filtered_workspace.clone());
    filters.created_from = Some(fixture.filtered_from);
    filters.created_to = Some(fixture.filtered_to);
    filters
}

fn build_fixture_conversation(
    conv_idx: usize,
    messages_per_conversation: usize,
) -> FixtureConversation {
    let agent_slug = format!("bench-agent-{}", conv_idx % 10);
    let workspace_path = PathBuf::from(format!("/workspace/project-{}", conv_idx % 20));
    let source_path = PathBuf::from(format!(
        "/tmp/cass-bench/{agent_slug}/conversation-{conv_idx:04}.jsonl"
    ));
    let started_at = 1_700_000_000_000i64 + conv_idx as i64 * 3_600_000;

    let highlight_filtered_lane = conv_idx % 20 == 13;

    let mut normalized_messages = Vec::with_capacity(messages_per_conversation);

    for msg_idx in 0..messages_per_conversation {
        let created_at = started_at + msg_idx as i64 * 45_000;
        let component = COMPONENTS[(conv_idx + msg_idx) % COMPONENTS.len()];
        let operation = OPERATIONS[(conv_idx * 3 + msg_idx) % OPERATIONS.len()];

        let mut content = format!(
            "Conversation {conv_idx} message {msg_idx} investigates {component} during {operation}. \
             The team reviewed authentication middleware, authenticator fallbacks, authorization rules, \
             cache bloom gates, edge ngram prefix indexing, and cass result formatting."
        );

        if highlight_filtered_lane || (conv_idx + msg_idx).is_multiple_of(7) {
            content.push_str(
                " The incident reproduced a frankensqlite write conflict during concurrent indexing.",
            );
        }
        if highlight_filtered_lane || (conv_idx + msg_idx).is_multiple_of(11) {
            content.push_str(
                " Engineers documented the distributed tracing handshake for cross-machine sync.",
            );
        }
        if highlight_filtered_lane || (conv_idx + msg_idx).is_multiple_of(5) {
            content.push_str(
                " We validated token-refresh, token_cache, auth_token, and session_token propagation.",
            );
        }
        if (conv_idx + msg_idx).is_multiple_of(2) {
            content.push_str(" Authentication remained the hot path for auth workflows.");
        } else {
            content.push_str(" The authenticator preserved authz state for the auth worker.");
        }

        let author = (msg_idx % 2 == 1).then(|| format!("model-{}", conv_idx % 4));

        normalized_messages.push(NormalizedMessage {
            idx: msg_idx as i64,
            role: if msg_idx % 2 == 0 {
                "user".to_string()
            } else {
                "agent".to_string()
            },
            author: author.clone(),
            created_at: Some(created_at),
            content: content.clone(),
            extra: serde_json::json!({
                "bench": true,
                "conversation": conv_idx,
                "message": msg_idx,
            }),
            snippets: Vec::new(),
            invocations: Vec::new(),
        });
    }

    let title = format!(
        "Benchmark session {conv_idx}: {}",
        COMPONENTS[conv_idx % COMPONENTS.len()]
    );

    FixtureConversation {
        normalized: NormalizedConversation {
            agent_slug: agent_slug.clone(),
            external_id: Some(format!("bench-conv-{conv_idx:04}")),
            title: Some(title.clone()),
            workspace: Some(workspace_path.clone()),
            source_path: source_path.clone(),
            started_at: Some(started_at),
            ended_at: Some(started_at + messages_per_conversation as i64 * 45_000),
            metadata: serde_json::json!({
                "bench": true,
                "scale_messages": messages_per_conversation,
            }),
            messages: normalized_messages,
        },
    }
}

fn build_fixture() -> Result<SearchFixture> {
    let temp = TempDir::new().context("create tempdir")?;
    let data_dir = temp.path().join("24k_msgs");
    std::fs::create_dir_all(&data_dir).context("create data dir")?;

    let index_path = index_dir(&data_dir).context("resolve index path")?;
    let mut t_index = TantivyIndex::open_or_create(&index_path).context("open tantivy index")?;

    let corpus: Vec<FixtureConversation> = (0..CONVERSATION_COUNT)
        .map(|idx| build_fixture_conversation(idx, MESSAGES_PER_CONVERSATION))
        .collect();

    for conv in &corpus {
        t_index
            .add_conversation(&conv.normalized)
            .context("index benchmark conversation")?;
    }
    t_index.commit().context("commit tantivy index")?;

    let client = SearchClient::open_with_options(&index_path, None, bench_client_options())
        .context("open search client")?
        .context("search client unavailable")?;

    let filtered_from = 1_700_000_000_000i64 + 100 * 3_600_000;
    let filtered_to = 1_700_000_000_000i64 + 900 * 3_600_000;

    Ok(SearchFixture {
        _temp: temp,
        _index_path: index_path,
        client,
        label: "24k_msgs",
        total_messages: CONVERSATION_COUNT * MESSAGES_PER_CONVERSATION,
        filtered_agent: "bench-agent-3".to_string(),
        filtered_workspace: "/workspace/project-13".to_string(),
        filtered_from,
        filtered_to,
    })
}

fn run_search(
    client: &SearchClient,
    query: &str,
    filters: SearchFilters,
) -> Result<BackendSearchResult> {
    client
        .search_with_fallback(
            query,
            filters,
            SEARCH_LIMIT,
            0,
            SPARSE_THRESHOLD,
            interactive_field_mask(),
        )
        .context("run search")
}

fn run_ranked_query(
    client: &SearchClient,
    query: &str,
    filters: SearchFilters,
    ranking: RankingMode,
) -> Result<Vec<SearchHit>> {
    if query.trim().is_empty() {
        let newest_first = !matches!(ranking, RankingMode::DateOldest);
        return client
            .browse_by_date(
                filters,
                SEARCH_LIMIT,
                0,
                newest_first,
                interactive_field_mask(),
            )
            .context("browse by date");
    }

    let result = run_search(client, query, filters)?;
    Ok(apply_ranking_mode(result.hits, ranking))
}

fn apply_ranking_mode(mut hits: Vec<SearchHit>, ranking: RankingMode) -> Vec<SearchHit> {
    if hits.is_empty() {
        return hits;
    }

    if matches!(ranking, RankingMode::DateNewest | RankingMode::DateOldest) {
        let newest_first = matches!(ranking, RankingMode::DateNewest);
        hits.sort_by(|left, right| compare_by_date(left, right, newest_first));
        return hits;
    }

    let max_score = hits
        .iter()
        .map(|hit| hit.score)
        .fold(0.0f32, f32::max)
        .max(1.0);
    let newest_ts = hits
        .iter()
        .filter_map(|hit| hit.created_at)
        .max()
        .unwrap_or(0);
    let oldest_ts = hits
        .iter()
        .filter_map(|hit| hit.created_at)
        .min()
        .unwrap_or(newest_ts);
    let ts_span = (newest_ts - oldest_ts).max(1) as f32;

    hits.sort_by(|left, right| {
        let left_score = ranking_score(left, ranking, max_score, newest_ts, ts_span);
        let right_score = ranking_score(right, ranking, max_score, newest_ts, ts_span);
        right_score
            .partial_cmp(&left_score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| compare_stable(left, right))
    });
    hits
}

fn ranking_score(
    hit: &SearchHit,
    ranking: RankingMode,
    max_score: f32,
    newest_ts: i64,
    ts_span: f32,
) -> f32 {
    let lexical = (hit.score / max_score).clamp(0.0, 1.0);
    let recency = hit
        .created_at
        .map(|ts| ((ts - (newest_ts - ts_span as i64)) as f32 / ts_span).clamp(0.0, 1.0))
        .unwrap_or(0.0);
    let quality = hit.match_type.quality_factor();

    match ranking {
        RankingMode::RecentHeavy => lexical * 0.3 + recency * 0.7,
        RankingMode::Balanced => lexical * 0.5 + recency * 0.5,
        RankingMode::RelevanceHeavy => lexical * 0.8 + recency * 0.2,
        RankingMode::MatchQualityHeavy => lexical * 0.7 + recency * 0.2 + quality * 0.1,
        RankingMode::DateNewest | RankingMode::DateOldest => recency,
    }
}

fn compare_by_date(left: &SearchHit, right: &SearchHit, newest_first: bool) -> Ordering {
    let primary = match (left.created_at, right.created_at) {
        (Some(left_ts), Some(right_ts)) if newest_first => right_ts.cmp(&left_ts),
        (Some(left_ts), Some(right_ts)) => left_ts.cmp(&right_ts),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    };
    primary.then_with(|| compare_stable(left, right))
}

fn compare_stable(left: &SearchHit, right: &SearchHit) -> Ordering {
    left.source_path
        .cmp(&right.source_path)
        .then_with(|| left.line_number.cmp(&right.line_number))
        .then_with(|| left.title.cmp(&right.title))
}

fn summarize_latencies(
    elapsed: &[Duration],
    hit_count: usize,
    cache_before: Option<CacheStats>,
    cache_after: Option<CacheStats>,
) -> LatencySummary {
    assert!(
        !elapsed.is_empty(),
        "latency summary requires at least one sample"
    );

    let mut millis: Vec<f64> = elapsed
        .iter()
        .map(Duration::as_secs_f64)
        .map(|s| s * 1_000.0)
        .collect();
    millis.sort_by(|left, right| left.partial_cmp(right).unwrap_or(Ordering::Equal));

    let pick = |pct: f64| -> f64 {
        let rank = ((millis.len() as f64 * pct).ceil() as usize).saturating_sub(1);
        millis[rank.min(millis.len() - 1)]
    };

    let (cache_hits_delta, cache_miss_delta, cache_shortfall_delta) =
        if let (Some(before), Some(after)) = (cache_before, cache_after) {
            (
                after.cache_hits.saturating_sub(before.cache_hits),
                after.cache_miss.saturating_sub(before.cache_miss),
                after.cache_shortfall.saturating_sub(before.cache_shortfall),
            )
        } else {
            (0, 0, 0)
        };

    LatencySummary {
        p50_ms: pick(0.50),
        p95_ms: pick(0.95),
        p99_ms: pick(0.99),
        max_ms: *millis.last().unwrap_or(&0.0),
        hit_count,
        cache_hits_delta,
        cache_miss_delta,
        cache_shortfall_delta,
    }
}

fn measure_warm_query(
    client: &SearchClient,
    query: &str,
    filters: SearchFilters,
    samples: usize,
) -> Result<LatencySummary> {
    let _ = run_search(client, query, filters.clone())?;
    let cache_before = client.cache_stats();
    let mut elapsed = Vec::with_capacity(samples);
    let mut hit_count = 0usize;

    for _ in 0..samples {
        let started = Instant::now();
        let result = run_search(client, query, filters.clone())?;
        elapsed.push(started.elapsed());
        hit_count = result.hits.len();
    }

    let cache_after = client.cache_stats();
    Ok(summarize_latencies(
        &elapsed,
        hit_count,
        Some(cache_before),
        Some(cache_after),
    ))
}

fn measure_prefix_typing(
    client: &SearchClient,
    filters: SearchFilters,
    sequences: usize,
) -> Result<LatencySummary> {
    for prefix in PREFIX_SEQUENCE {
        let _ = run_search(client, prefix, filters.clone())?;
    }

    let cache_before = client.cache_stats();
    let mut elapsed = Vec::with_capacity(sequences * PREFIX_SEQUENCE.len());
    let mut hit_count = 0usize;

    for _ in 0..sequences {
        for prefix in PREFIX_SEQUENCE {
            let started = Instant::now();
            let result = run_search(client, prefix, filters.clone())?;
            elapsed.push(started.elapsed());
            hit_count = result.hits.len();
        }
    }

    let cache_after = client.cache_stats();
    Ok(summarize_latencies(
        &elapsed,
        hit_count,
        Some(cache_before),
        Some(cache_after),
    ))
}

fn measure_ranked_query(
    client: &SearchClient,
    query: &str,
    filters: SearchFilters,
    ranking: RankingMode,
    samples: usize,
) -> Result<LatencySummary> {
    let _ = run_ranked_query(client, query, filters.clone(), ranking)?;
    let cache_before = client.cache_stats();
    let mut elapsed = Vec::with_capacity(samples);
    let mut hit_count = 0usize;

    for _ in 0..samples {
        let started = Instant::now();
        let hits = run_ranked_query(client, query, filters.clone(), ranking)?;
        elapsed.push(started.elapsed());
        hit_count = hits.len();
    }

    let cache_after = client.cache_stats();
    Ok(summarize_latencies(
        &elapsed,
        hit_count,
        Some(cache_before),
        Some(cache_after),
    ))
}

fn log_summary(scenario: &str, fixture: &SearchFixture, summary: &LatencySummary) {
    eprintln!(
        "[search_latency_e2e] scenario={scenario} corpus={} conversations={} messages={} hits={} p50={:.2}ms p95={:.2}ms p99={:.2}ms max={:.2}ms cache_hits={} cache_miss={} cache_shortfall={}",
        fixture.label,
        CONVERSATION_COUNT,
        fixture.total_messages,
        summary.hit_count,
        summary.p50_ms,
        summary.p95_ms,
        summary.p99_ms,
        summary.max_ms,
        summary.cache_hits_delta,
        summary.cache_miss_delta,
        summary.cache_shortfall_delta,
    );
}

fn assert_latency_budget(label: &str, summary: &LatencySummary, budget_ms: f64) {
    assert!(
        summary.hit_count > 0,
        "{label} returned zero hits; benchmark fixture/query is broken"
    );
    assert!(
        summary.p95_ms <= budget_ms,
        "{label} p95 {:.2}ms exceeded budget {:.2}ms",
        summary.p95_ms,
        budget_ms,
    );
}

fn assert_ratio_budget(label: &str, lhs_ms: f64, rhs_ms: f64, max_ratio: f64) {
    let baseline = rhs_ms.max(0.001);
    let ratio = lhs_ms / baseline;
    assert!(
        ratio <= max_ratio,
        "{label} ratio {:.2} exceeded max {:.2} (lhs {:.2}ms vs rhs {:.2}ms)",
        ratio,
        max_ratio,
        lhs_ms,
        rhs_ms,
    );
}

fn run_preflight_assertions(fixture: &SearchFixture) -> Result<()> {
    let warm_exact = measure_warm_query(
        &fixture.client,
        EXACT_QUERY,
        default_filters(),
        WARM_SAMPLES,
    )?;
    let warm_phrase = measure_warm_query(
        &fixture.client,
        PHRASE_QUERY,
        default_filters(),
        WARM_SAMPLES,
    )?;
    let warm_wildcard = measure_warm_query(
        &fixture.client,
        WILDCARD_QUERY,
        default_filters(),
        WARM_SAMPLES,
    )?;
    let prefix_typing =
        measure_prefix_typing(&fixture.client, default_filters(), PREFIX_SEQUENCES)?;
    let filtered = measure_warm_query(
        &fixture.client,
        EXACT_QUERY,
        filtered_search_filters(fixture),
        WARM_SAMPLES,
    )?;
    let balanced = measure_ranked_query(
        &fixture.client,
        EXACT_QUERY,
        default_filters(),
        RankingMode::Balanced,
        WARM_SAMPLES,
    )?;
    let relevance = measure_ranked_query(
        &fixture.client,
        EXACT_QUERY,
        default_filters(),
        RankingMode::RelevanceHeavy,
        WARM_SAMPLES,
    )?;
    let quality = measure_ranked_query(
        &fixture.client,
        EXACT_QUERY,
        default_filters(),
        RankingMode::MatchQualityHeavy,
        WARM_SAMPLES,
    )?;
    let newest = measure_ranked_query(
        &fixture.client,
        EXACT_QUERY,
        default_filters(),
        RankingMode::DateNewest,
        WARM_SAMPLES,
    )?;
    let oldest = measure_ranked_query(
        &fixture.client,
        EXACT_QUERY,
        default_filters(),
        RankingMode::DateOldest,
        WARM_SAMPLES,
    )?;

    for (scenario, summary) in [
        ("warm_exact", &warm_exact),
        ("warm_phrase", &warm_phrase),
        ("warm_wildcard", &warm_wildcard),
        ("prefix_typing", &prefix_typing),
        ("filtered_exact", &filtered),
        ("ranking_balanced", &balanced),
        ("ranking_relevance", &relevance),
        ("ranking_quality", &quality),
        ("ranking_newest", &newest),
        ("ranking_oldest", &oldest),
    ] {
        log_summary(scenario, fixture, summary);
    }

    assert_latency_budget("warm exact (24k)", &warm_exact, TYPICAL_P95_BUDGET_MS);
    assert_latency_budget("warm phrase (24k)", &warm_phrase, TYPICAL_P95_BUDGET_MS);
    assert_latency_budget("warm wildcard (24k)", &warm_wildcard, TYPICAL_P95_BUDGET_MS);
    assert_latency_budget("prefix typing (24k)", &prefix_typing, TYPICAL_P95_BUDGET_MS);
    assert_latency_budget("balanced ranking (24k)", &balanced, TYPICAL_P95_BUDGET_MS);
    assert_latency_budget("relevance ranking (24k)", &relevance, TYPICAL_P95_BUDGET_MS);
    assert_latency_budget("quality ranking (24k)", &quality, TYPICAL_P95_BUDGET_MS);
    assert_latency_budget("date newest ranking (24k)", &newest, TYPICAL_P95_BUDGET_MS);
    assert_latency_budget("date oldest ranking (24k)", &oldest, TYPICAL_P95_BUDGET_MS);
    assert_ratio_budget(
        "filtered search overhead (24k)",
        filtered.p95_ms,
        warm_exact.p95_ms,
        FILTER_OVERHEAD_MAX_RATIO,
    );

    Ok(())
}

fn bench_search_latency_e2e(c: &mut Criterion) {
    let fixture = build_fixture().expect("build search latency fixture");
    run_preflight_assertions(&fixture).expect("preflight latency assertions");

    let mut warm_group = c.benchmark_group("search_latency_e2e/warm_search");
    warm_group.sample_size(10);
    warm_group.warm_up_time(Duration::from_millis(250));
    warm_group.measurement_time(Duration::from_secs(1));
    warm_group.throughput(Throughput::Elements(fixture.total_messages as u64));
    for (name, query) in [
        ("exact", EXACT_QUERY),
        ("phrase", PHRASE_QUERY),
        ("wildcard", WILDCARD_QUERY),
    ] {
        let _ =
            run_search(&fixture.client, query, default_filters()).expect("warm fixture priming");
        warm_group.bench_with_input(
            BenchmarkId::new(name, fixture.label),
            &fixture,
            |b, fixture| {
                b.iter(|| {
                    let result =
                        run_search(&fixture.client, query, default_filters()).expect("warm search");
                    black_box(result.hits.len())
                })
            },
        );
    }
    warm_group.finish();

    let mut prefix_group = c.benchmark_group("search_latency_e2e/prefix_typing");
    prefix_group.sample_size(10);
    prefix_group.warm_up_time(Duration::from_millis(250));
    prefix_group.measurement_time(Duration::from_secs(1));
    prefix_group.throughput(Throughput::Elements(fixture.total_messages as u64));
    prefix_group.bench_with_input(
        BenchmarkId::new("typing_sequence", fixture.label),
        &fixture,
        |b, fixture| {
            b.iter(|| {
                let mut last_count = 0usize;
                for prefix in PREFIX_SEQUENCE {
                    let result = run_search(&fixture.client, prefix, default_filters())
                        .expect("prefix search");
                    last_count = result.hits.len();
                }
                black_box(last_count)
            })
        },
    );
    prefix_group.finish();

    let mut filtered_group = c.benchmark_group("search_latency_e2e/filtered_search");
    filtered_group.sample_size(10);
    filtered_group.warm_up_time(Duration::from_millis(250));
    filtered_group.measurement_time(Duration::from_secs(1));
    filtered_group.throughput(Throughput::Elements(fixture.total_messages as u64));
    let filters = filtered_search_filters(&fixture);
    let _ = run_search(&fixture.client, EXACT_QUERY, filters.clone())
        .expect("filtered fixture priming");
    filtered_group.bench_with_input(
        BenchmarkId::new("exact_filtered", fixture.label),
        &fixture,
        |b, fixture| {
            b.iter(|| {
                let result = run_search(&fixture.client, EXACT_QUERY, filters.clone())
                    .expect("filtered search");
                black_box(result.hits.len())
            })
        },
    );
    filtered_group.finish();

    let mut ranking_group = c.benchmark_group("search_latency_e2e/ranking_modes");
    ranking_group.sample_size(10);
    ranking_group.warm_up_time(Duration::from_millis(250));
    ranking_group.measurement_time(Duration::from_secs(1));
    for (name, query, ranking) in [
        ("balanced", EXACT_QUERY, RankingMode::Balanced),
        ("relevance", EXACT_QUERY, RankingMode::RelevanceHeavy),
        ("quality", EXACT_QUERY, RankingMode::MatchQualityHeavy),
        ("date_newest", EXACT_QUERY, RankingMode::DateNewest),
        ("date_oldest", EXACT_QUERY, RankingMode::DateOldest),
    ] {
        let _ = run_ranked_query(&fixture.client, query, default_filters(), ranking)
            .expect("ranking fixture priming");
        ranking_group.bench_with_input(
            BenchmarkId::new(name, fixture.label),
            &fixture,
            |b, fixture| {
                b.iter(|| {
                    let hits = run_ranked_query(&fixture.client, query, default_filters(), ranking)
                        .expect("ranked query");
                    black_box(hits.len())
                })
            },
        );
    }
    ranking_group.finish();
}

criterion_group! {
    name = search_latency_e2e;
    config = configure_criterion();
    targets = bench_search_latency_e2e
}
criterion_main!(search_latency_e2e);
