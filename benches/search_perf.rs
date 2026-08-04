use coding_agent_search::default_data_dir;
use coding_agent_search::robot_budget_envelope::BudgetBlock;
use coding_agent_search::search::canonicalize::{MAX_EMBED_CHARS, canonicalize_for_embedding};
use coding_agent_search::search::embedder::Embedder;
use coding_agent_search::search::hash_embedder::HashEmbedder;
use coding_agent_search::search::pack_planner::{
    PackCandidate, PackFreshnessPolicy, PackLexicalReadiness, PackPlanRequest, PackPlannerLimits,
    PackReadinessSnapshot, PackRenderFormat, PackRenderRequest, PackSemanticReadiness,
    PackSourceReadiness, PlannedAnswerPack, plan_answer_pack, render_answer_pack,
};
use coding_agent_search::search::query::{
    FieldMask, MatchType, SearchClient, SearchFilters, SearchHit, rrf_fuse_hits,
};
use coding_agent_search::search::tantivy::index_dir;
use coding_agent_search::search::vector_index::{
    Quantization, SemanticDocId, SemanticFilter, VectorIndex, dot_product_f16_scalar_bench,
    dot_product_f16_simd_bench, dot_product_scalar_bench, dot_product_simd_bench,
};
use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use half::f16;
use std::collections::HashSet;
use std::hint::black_box;
use std::mem::size_of;
use tempfile::TempDir;

const PACK_BENCH_NOW_MS: i64 = 1_764_000_000_000;
const PACK_BENCH_FRESHNESS_WINDOW_SECONDS: i64 = 30 * 24 * 60 * 60;
const PACK_BENCH_QUERY_TERMS: usize = 4;
const PACK_BENCH_QUERY_PHRASES: usize = 1;

// =============================================================================
// Hash Embedder Benchmarks
// =============================================================================

/// Benchmark hash embedder on 1000 documents.
/// Target: <1ms per doc (so <1s total for 1000 docs)
fn bench_hash_embed_1000_docs(c: &mut Criterion) {
    let embedder = HashEmbedder::default_dimension();
    let docs: Vec<String> = (0..1000)
        .map(|i| format!("This is document number {} with some sample content for embedding benchmarks. It contains various words like rust programming language testing performance.", i))
        .collect();

    c.bench_function("hash_embed_1000_docs", |b| {
        b.iter(|| {
            for doc in &docs {
                let _ = black_box(embedder.embed_sync(doc));
            }
        })
    });
}

/// Benchmark hash embedder batch embedding.
fn bench_hash_embed_batch(c: &mut Criterion) {
    let embedder = HashEmbedder::default_dimension();
    let docs: Vec<&str> = (0..100)
        .map(|_| "Sample document for batch embedding benchmark with multiple words")
        .collect();

    c.bench_function("hash_embed_batch_100", |b| {
        b.iter(|| {
            let _ = black_box(embedder.embed_batch_sync(&docs));
        })
    });
}

// =============================================================================
// Canonicalization Benchmarks
// =============================================================================

/// Benchmark canonicalization of a long message.
fn make_long_message() -> String {
    // Create a realistic long message (~10KB)
    (0..100)
        .map(|i| {
            format!(
                "Paragraph {}: Lorem ipsum dolor sit amet, consectetur adipiscing elit. \
                 Sed do eiusmod tempor incididunt ut labore et dolore magna aliqua. \
                 Ut enim ad minim veniam, quis nostrud exercitation ullamco laboris. ",
                i
            )
        })
        .collect()
}

fn make_sized_message(target_len: usize) -> String {
    let chunk = "This is a sample sentence for canonicalization benchmarks. ";
    let mut msg = String::with_capacity(target_len + chunk.len());
    while msg.len() < target_len {
        msg.push_str(chunk);
    }
    msg.truncate(target_len);
    msg
}

fn bench_canonicalize_long_message(c: &mut Criterion) {
    let long_message = make_long_message();
    c.bench_function("canonicalize_long_message", |b| {
        b.iter(|| black_box(canonicalize_for_embedding(&long_message)))
    });
}

/// Benchmark canonicalization with code blocks.
fn bench_canonicalize_with_code(c: &mut Criterion) {
    let message_with_code = r#"
Here's the Rust code to implement a binary search:

```rust
fn binary_search<T: Ord>(arr: &[T], target: &T) -> Option<usize> {
    let mut left = 0;
    let mut right = arr.len();

    while left < right {
        let mid = left + (right - left) / 2;
        match arr[mid].cmp(target) {
            std::cmp::Ordering::Equal => return Some(mid),
            std::cmp::Ordering::Less => left = mid + 1,
            std::cmp::Ordering::Greater => right = mid,
        }
    }
    None
}
```

This has O(log n) time complexity and O(1) space complexity.
"#;

    c.bench_function("canonicalize_with_code", |b| {
        b.iter(|| black_box(canonicalize_for_embedding(message_with_code)))
    });
}

/// Benchmark canonicalization across input sizes.
fn bench_canonicalize_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("canonicalize_scaling");
    let sizes = [100usize, 1_000, 10_000, MAX_EMBED_CHARS + 500];

    for size in sizes {
        let text = make_sized_message(size);
        group.bench_with_input(BenchmarkId::new("canonicalize", size), &text, |b, input| {
            b.iter(|| black_box(canonicalize_for_embedding(input)))
        });
    }
    group.finish();
}

// =============================================================================
// RRF Fusion Benchmarks
// =============================================================================

/// Create a test search hit for benchmarking.
fn make_bench_hit(id: &str, score: f32) -> SearchHit {
    SearchHit {
        title: id.to_string(),
        snippet: format!("Snippet for {id}"),
        content: format!("Content for {id}"),
        content_hash: 0,
        score,
        source_path: format!("/path/to/{id}.jsonl"),
        agent: "test".to_string(),
        workspace: "/workspace".to_string(),
        workspace_original: None,
        created_at: Some(1704067200000), // 2024-01-01
        line_number: Some(1),
        match_type: MatchType::Exact,
        source_id: "local".to_string(),
        origin_kind: "local".to_string(),
        origin_host: None,
        conversation_id: None,
    }
}

/// Benchmark RRF fusion with 100 results from each source.
/// Target: <5ms
fn bench_rrf_fusion_100_results(c: &mut Criterion) {
    let lexical: Vec<SearchHit> = (0..100)
        .map(|i| make_bench_hit(&format!("L{i}"), 100.0 - i as f32))
        .collect();

    let semantic: Vec<SearchHit> = (0..100)
        .map(|i| make_bench_hit(&format!("S{i}"), 1.0 - 0.01 * i as f32))
        .collect();

    c.bench_function("rrf_fusion_100_results", |b| {
        b.iter(|| {
            let fused = rrf_fuse_hits(black_box(&lexical), black_box(&semantic), "", 25, 0);
            black_box(fused)
        })
    });
}

/// Benchmark RRF fusion with overlapping results.
fn bench_rrf_fusion_overlapping(c: &mut Criterion) {
    // 50% overlap between lexical and semantic
    let lexical: Vec<SearchHit> = (0..100)
        .map(|i| make_bench_hit(&format!("doc{i}"), 100.0 - i as f32))
        .collect();

    let semantic: Vec<SearchHit> = (50..150)
        .map(|i| make_bench_hit(&format!("doc{i}"), 1.0 - 0.01 * (i - 50) as f32))
        .collect();

    c.bench_function("rrf_fusion_50pct_overlap", |b| {
        b.iter(|| {
            let fused = rrf_fuse_hits(black_box(&lexical), black_box(&semantic), "", 25, 0);
            black_box(fused)
        })
    });
}

// =============================================================================
// Answer Pack Benchmarks
// =============================================================================

fn make_pack_bench_hit(idx: usize) -> SearchHit {
    let mut hit = make_bench_hit(&format!("pack-{idx:05}"), 1000.0 - idx as f32 * 0.1);
    let agent = format!("bench-agent-{}", idx % 12);
    let workspace = format!("/workspace/answer-pack-project-{}", idx % 16);
    let source_id = if idx.is_multiple_of(7) {
        "remote-build"
    } else {
        "local"
    };
    let origin_kind = if idx.is_multiple_of(7) {
        "remote"
    } else {
        "local"
    };
    let created_at = PACK_BENCH_NOW_MS - (idx as i64 % 180) * 3_600_000;
    let query_lane = match idx % 4 {
        0 => "checkout failure root cause",
        1 => "answer pack cited evidence",
        2 => "freshness health fallback",
        _ => "token budget omitted evidence",
    };
    let duplicate_note = if idx.is_multiple_of(13) {
        " Duplicate snippet for suppression coverage."
    } else {
        ""
    };
    let privacy_note = if idx.is_multiple_of(17) {
        " The raw note mentioned API_KEY=sk-test-redacted and /home/alice/private/session.jsonl."
    } else {
        ""
    };

    hit.title = format!("Answer pack benchmark session {idx}");
    hit.snippet = format!("{query_lane} summary for candidate {idx}.{duplicate_note}");
    hit.content = format!(
        "Candidate {idx} covers {query_lane}. The team selected cited evidence, \
         checked source readiness, reviewed semantic fallback, and measured JSON plus Markdown \
         rendering cost for agent handoff workflows.{duplicate_note}{privacy_note}"
    );
    hit.content_hash = 0xfeed_0000_u64 + idx as u64;
    hit.source_path = format!("/tmp/cass-pack-bench/{agent}/session-{idx:05}.jsonl");
    hit.agent = agent;
    hit.workspace = workspace;
    hit.workspace_original = (idx.is_multiple_of(5)).then(|| format!("~/src/pack-{idx}"));
    hit.created_at = Some(created_at);
    hit.line_number = Some(idx % 300 + 1);
    hit.match_type = match idx % 5 {
        0 => MatchType::Exact,
        1 => MatchType::Prefix,
        2 => MatchType::Substring,
        3 => MatchType::Wildcard,
        _ => MatchType::ImplicitWildcard,
    };
    hit.source_id = source_id.to_string();
    hit.origin_kind = origin_kind.to_string();
    hit.origin_host = idx
        .is_multiple_of(7)
        .then(|| format!("worker-{}.local", idx % 3));
    hit.conversation_id = Some(idx as i64);
    hit
}

fn build_pack_bench_hits(count: usize) -> Vec<SearchHit> {
    (0..count).map(make_pack_bench_hit).collect()
}

fn build_pack_bench_candidates(count: usize) -> Vec<PackCandidate> {
    build_pack_bench_hits(count)
        .iter()
        .enumerate()
        .map(|(rank, hit)| {
            let mut candidate = PackCandidate::from_search_hit(
                hit,
                PACK_BENCH_QUERY_TERMS,
                PACK_BENCH_QUERY_PHRASES,
            );
            candidate.hybrid_rank = Some(rank + 1);
            candidate.source_readiness = if rank.is_multiple_of(29) {
                PackSourceReadiness::StaleReadable
            } else {
                PackSourceReadiness::Healthy
            };
            candidate.source_explicitly_requested = rank.is_multiple_of(11);
            candidate
        })
        .collect()
}

fn pack_bench_limits(max_tokens: usize) -> PackPlannerLimits {
    PackPlannerLimits {
        max_tokens,
        max_sessions: 12,
        max_evidence: 32,
        context_lines: 3,
        max_excerpt_chars: 1_200,
    }
}

fn pack_bench_plan(candidates: Vec<PackCandidate>, limits: PackPlannerLimits) -> PlannedAnswerPack {
    plan_answer_pack(PackPlanRequest {
        now_ms: PACK_BENCH_NOW_MS,
        limits,
        freshness_policy: PackFreshnessPolicy::PreferRecent,
        freshness_window_seconds: PACK_BENCH_FRESHNESS_WINDOW_SECONDS,
        candidates,
        explain_selection: true,
    })
    .expect("answer pack benchmark plan")
}

fn pack_bench_render_request(
    format: PackRenderFormat,
    limits: PackPlannerLimits,
) -> PackRenderRequest {
    PackRenderRequest {
        query_text: "checkout failure answer pack freshness".to_string(),
        normalized_query: "checkout failure answer pack freshness".to_string(),
        generated_at_ms: PACK_BENCH_NOW_MS,
        elapsed_ms: 0,
        budget: BudgetBlock {
            elapsed_ms: 0,
            budget_ms: 8_000,
            timed_out: false,
            skipped_sections: Vec::new(),
            recommended_next_probe: None,
        },
        request_id: Some("bench-answer-pack".to_string()),
        format,
        limits,
        search_mode: "lexical".to_string(),
        fallback_mode: Some("lexical".to_string()),
        semantic_joined: false,
        freshness_policy: PackFreshnessPolicy::PreferRecent,
        freshness_window_seconds: PACK_BENCH_FRESHNESS_WINDOW_SECONDS,
        redaction_policy: "strict".to_string(),
        sensitive_output: false,
        skill_content_included: false,
        explain_selection: true,
        readiness: PackReadinessSnapshot {
            index_generation: Some("bench-generation".to_string()),
            lexical_readiness: PackLexicalReadiness::Ready,
            semantic_readiness: PackSemanticReadiness::FallbackLexical,
            active_rebuild: false,
            lock_state: None,
            missing_database: false,
            source_sync_gaps: Vec::new(),
            recommended_action: None,
        },
    }
}

fn approx_pack_candidate_bytes(candidates: &[PackCandidate]) -> usize {
    candidates
        .iter()
        .map(|candidate| {
            size_of::<PackCandidate>()
                + candidate.candidate_id.len()
                + candidate.source_path.len()
                + candidate.source_id.len()
                + candidate.origin_kind.len()
                + candidate.workspace.len()
                + candidate.agent.len()
                + candidate.content_hash.len()
                + candidate.span_hash.len()
                + candidate.excerpt.len()
                + candidate
                    .origin_host
                    .as_ref()
                    .map_or(0, std::string::String::len)
                + candidate
                    .workspace_original
                    .as_ref()
                    .map_or(0, std::string::String::len)
        })
        .sum()
}

fn pack_bench_id(stage: &str, candidate_count: usize, plan: &PlannedAnswerPack) -> BenchmarkId {
    let utilization_pct =
        plan.estimated_tokens.saturating_mul(100) / plan.diagnostics.budget.max_tokens;
    BenchmarkId::new(
        stage,
        format!(
            "{candidate_count}_candidates_{}_selected_{}_omitted_{utilization_pct}pct_budget",
            plan.selected_evidence_count,
            plan.omitted.len()
        ),
    )
}

fn bench_answer_pack_candidate_hydration(c: &mut Criterion) {
    let mut group = c.benchmark_group("answer_pack_candidate_hydration");
    for count in [64usize, 512, 2_048] {
        let hits = build_pack_bench_hits(count);
        let candidates = build_pack_bench_candidates(count);
        let memory_proxy_kib = approx_pack_candidate_bytes(&candidates) / 1024;
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(
            BenchmarkId::new(
                "from_search_hit",
                format!("{count}_hits_{memory_proxy_kib}kib_proxy"),
            ),
            &hits,
            |bench, input| {
                bench.iter(|| {
                    let candidates = input
                        .iter()
                        .enumerate()
                        .map(|(rank, hit)| {
                            let mut candidate = PackCandidate::from_search_hit(
                                black_box(hit),
                                PACK_BENCH_QUERY_TERMS,
                                PACK_BENCH_QUERY_PHRASES,
                            );
                            candidate.hybrid_rank = Some(rank + 1);
                            candidate
                        })
                        .collect::<Vec<_>>();
                    black_box(candidates);
                });
            },
        );
    }
    group.finish();
}

fn bench_answer_pack_planner_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("answer_pack_planner_scaling");
    for count in [64usize, 512, 2_048] {
        let candidates = build_pack_bench_candidates(count);
        let limits = pack_bench_limits(12_000);
        let plan = pack_bench_plan(candidates.clone(), limits.clone());
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(
            pack_bench_id("plan", count, &plan),
            &candidates,
            |bench, input| {
                bench.iter_batched(
                    || input.clone(),
                    |candidates| black_box(pack_bench_plan(candidates, limits.clone())),
                    BatchSize::LargeInput,
                );
            },
        );
    }
    group.finish();
}

fn bench_answer_pack_renderers(c: &mut Criterion) {
    let candidates = build_pack_bench_candidates(512);
    let limits = pack_bench_limits(12_000);
    let plan = pack_bench_plan(candidates, limits.clone());
    let mut group = c.benchmark_group("answer_pack_renderers");
    group.throughput(Throughput::Elements(plan.selected_evidence_count as u64));

    for (label, format) in [
        ("json_pretty", PackRenderFormat::Json),
        ("json_compact", PackRenderFormat::CompactJson),
        ("markdown", PackRenderFormat::Markdown),
    ] {
        let request = pack_bench_render_request(format, limits.clone());
        group.bench_function(BenchmarkId::new(label, "512_candidates"), |bench| {
            bench.iter(|| {
                let rendered =
                    render_answer_pack(black_box(&plan), black_box(&request)).expect(label);
                black_box(rendered);
            });
        });
    }
    group.finish();
}

fn bench_answer_pack_token_budget_scaling(c: &mut Criterion) {
    let candidates = build_pack_bench_candidates(768);
    let mut group = c.benchmark_group("answer_pack_token_budget_scaling");
    for max_tokens in [4_000usize, 12_000, 48_000] {
        let limits = pack_bench_limits(max_tokens);
        let plan = pack_bench_plan(candidates.clone(), limits.clone());
        group.throughput(Throughput::Elements(plan.selected_evidence_count as u64));
        group.bench_function(pack_bench_id("plan_and_render_json", 768, &plan), |bench| {
            bench.iter_batched(
                || candidates.clone(),
                |candidates| {
                    let plan = pack_bench_plan(candidates, limits.clone());
                    let request = pack_bench_render_request(PackRenderFormat::Json, limits.clone());
                    let rendered = render_answer_pack(&plan, &request).expect("render json");
                    black_box((plan, rendered));
                },
                BatchSize::LargeInput,
            );
        });
    }
    group.finish();
}

// =============================================================================
// Vector Index Benchmarks
// =============================================================================

fn bench_empty_search(c: &mut Criterion) {
    let data_dir = default_data_dir();
    let index_path = index_dir(&data_dir).unwrap();
    let client = SearchClient::open(&index_path, None).unwrap();
    // Note: This benchmark requires a real index to exist; skipped if not present
    if let Some(client) = client {
        c.bench_function("search_empty_query", |b| {
            b.iter(|| {
                let result = client
                    .search("", SearchFilters::default(), 10, 0, FieldMask::FULL)
                    .unwrap_or_default();
                black_box(result)
            })
        });
    }
}

/// Benchmark vector search with 10k entries.
/// Target: <5ms
fn bench_vector_index_search_10k(c: &mut Criterion) {
    let dimension = 384;
    let count = 10_000;
    let (_tmp, index) =
        build_temp_fsvi_index("bench-embedder", dimension, Quantization::F16, count);
    let query = build_query(dimension);

    c.bench_function("vector_index_search_10k", |b| {
        b.iter(|| {
            let results = index
                .search_top_k(black_box(&query), 25, None)
                .unwrap_or_default();
            black_box(results);
        });
    });
}

/// Benchmark vector search with 50k entries (no filter).
/// Target: <20ms
fn bench_vector_index_search_50k(c: &mut Criterion) {
    let dimension = 384;
    let count = 50_000;
    let (_tmp, index) =
        build_temp_fsvi_index("bench-embedder", dimension, Quantization::F16, count);
    let query = build_query(dimension);

    c.bench_function("vector_index_search_50k", |b| {
        b.iter(|| {
            let results = index
                .search_top_k(black_box(&query), 25, None)
                .unwrap_or_default();
            black_box(results);
        });
    });
}

/// Benchmark vector search with 50k entries and filtering.
/// Target: <20ms
fn bench_vector_index_search_50k_filtered(c: &mut Criterion) {
    let dimension = 384;
    let count = 50_000;
    let (_tmp, index) =
        build_temp_fsvi_index("bench-embedder", dimension, Quantization::F16, count);
    let query = build_query(dimension);

    // Filter to agents 0, 1, 2 (out of 8 possible)
    let mut agent_filter = HashSet::new();
    agent_filter.insert(0u32);
    agent_filter.insert(1u32);
    agent_filter.insert(2u32);

    let filter = SemanticFilter {
        agents: Some(agent_filter),
        workspaces: None,
        sources: None,
        roles: None,
        created_from: None,
        created_to: None,
    };

    c.bench_function("vector_index_search_50k_filtered", |b| {
        b.iter(|| {
            let results = index
                .search_top_k(black_box(&query), 25, Some(&filter))
                .unwrap_or_default();
            black_box(results);
        });
    });
}

/// Parameterized benchmark for different index sizes.
fn bench_vector_search_scaling(c: &mut Criterion) {
    let dimension = 384;
    let mut group = c.benchmark_group("vector_search_scaling");

    for size in [1_000, 5_000, 10_000, 25_000, 50_000] {
        let (_tmp, index) =
            build_temp_fsvi_index("bench-embedder", dimension, Quantization::F16, size);
        let query = build_query(dimension);

        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, _| {
            b.iter(|| {
                let results = index
                    .search_top_k(black_box(&query), 25, None)
                    .unwrap_or_default();
                black_box(results);
            });
        });
    }
    group.finish();
}

fn build_temp_fsvi_index(
    embedder_id: &str,
    dimension: usize,
    quantization: Quantization,
    count: usize,
) -> (TempDir, VectorIndex) {
    let temp = TempDir::new().expect("tempdir");
    let path = temp.path().join("bench.fsvi");
    let mut writer =
        VectorIndex::create_with_revision(&path, embedder_id, "bench", dimension, quantization)
            .expect("create fsvi writer");

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
    writer.finish().expect("finish fsvi");

    let index = VectorIndex::open(&path).expect("open fsvi");
    (temp, index)
}

fn normalize_in_place(vec: &mut [f32]) {
    let norm_sq: f32 = vec.iter().map(|v| v * v).sum();
    let norm = norm_sq.sqrt();
    if norm > 0.0 {
        for v in vec {
            *v /= norm;
        }
    }
}

fn build_query(dimension: usize) -> Vec<f32> {
    let mut query = Vec::with_capacity(dimension);
    for d in 0..dimension {
        query.push((d % 17) as f32 / 17.0);
    }
    normalize_in_place(&mut query);
    query
}

/// Benchmark vector search with 50k entries loaded from disk (F16 pre-conversion).
/// This tests P0 Opt 1: Pre-Convert F16→F32 Slab at Load Time.
/// Target (local, 2026-01-11): ~1.8ms with pre-conversion, ~4.6ms without.
fn bench_vector_index_search_50k_loaded(c: &mut Criterion) {
    let dimension = 384;
    let count = 50_000;
    let (temp, loaded) =
        build_temp_fsvi_index("bench-embedder", dimension, Quantization::F16, count);
    let query = build_query(dimension);

    c.bench_function("vector_index_search_50k_loaded", |b| {
        b.iter(|| {
            let results = loaded
                .search_top_k(black_box(&query), 25, None)
                .unwrap_or_default();
            black_box(results);
        });
    });
    drop(temp);
}

// =============================================================================
// Opt 1.1: F16 SIMD Dot Product Benchmarks
// =============================================================================

/// Benchmark f32 dot product (scalar vs SIMD) at typical embedding dimensions.
fn bench_dot_product_f32(c: &mut Criterion) {
    let mut group = c.benchmark_group("dot_product_f32");

    for dim in [128, 256, 384, 512, 768, 1024] {
        let a: Vec<f32> = (0..dim).map(|i| (i as f32 * 0.001).sin()).collect();
        let b: Vec<f32> = (0..dim).map(|i| (i as f32 * 0.001).cos()).collect();

        group.bench_with_input(BenchmarkId::new("scalar", dim), &dim, |bench, _| {
            bench.iter(|| black_box(dot_product_scalar_bench(&a, &b)))
        });

        group.bench_with_input(BenchmarkId::new("simd", dim), &dim, |bench, _| {
            bench.iter(|| black_box(dot_product_simd_bench(&a, &b)))
        });
    }
    group.finish();
}

/// Benchmark f16 dot product (scalar vs SIMD) at typical embedding dimensions.
/// Opt 1.1: This measures the impact of the SIMD optimization for f16→f32 dot product.
fn bench_dot_product_f16(c: &mut Criterion) {
    let mut group = c.benchmark_group("dot_product_f16");

    for dim in [128, 256, 384, 512, 768, 1024] {
        let a: Vec<f16> = (0..dim)
            .map(|i| f16::from_f32((i as f32 * 0.001).sin()))
            .collect();
        let b: Vec<f32> = (0..dim).map(|i| (i as f32 * 0.001).cos()).collect();

        group.bench_with_input(BenchmarkId::new("scalar", dim), &dim, |bench, _| {
            bench.iter(|| black_box(dot_product_f16_scalar_bench(&a, &b)))
        });

        group.bench_with_input(BenchmarkId::new("simd", dim), &dim, |bench, _| {
            bench.iter(|| black_box(dot_product_f16_simd_bench(&a, &b)))
        });
    }
    group.finish();
}

/// Benchmark f16 dot product throughput for vector search simulation.
/// Simulates searching through 10k, 25k, 50k vectors at dimension 384.
fn bench_dot_product_f16_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("dot_product_f16_throughput");
    let dim = 384;

    for count in [10_000, 25_000, 50_000] {
        let vectors: Vec<Vec<f16>> = (0..count)
            .map(|i| {
                (0..dim)
                    .map(|d| f16::from_f32(((i + d * 31) % 997) as f32 / 997.0))
                    .collect()
            })
            .collect();
        let query: Vec<f32> = (0..dim).map(|d| (d % 17) as f32 / 17.0).collect();

        group.bench_with_input(BenchmarkId::new("scalar", count), &count, |bench, _| {
            bench.iter(|| {
                let mut sum = 0.0f32;
                for v in &vectors {
                    sum += dot_product_f16_scalar_bench(v, &query);
                }
                black_box(sum)
            })
        });

        group.bench_with_input(BenchmarkId::new("simd", count), &count, |bench, _| {
            bench.iter(|| {
                let mut sum = 0.0f32;
                for v in &vectors {
                    sum += dot_product_f16_simd_bench(v, &query);
                }
                black_box(sum)
            })
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    // Hash embedder benchmarks
    bench_hash_embed_1000_docs,
    bench_hash_embed_batch,
    // Canonicalization benchmarks
    bench_canonicalize_long_message,
    bench_canonicalize_with_code,
    bench_canonicalize_scaling,
    // RRF fusion benchmarks
    bench_rrf_fusion_100_results,
    bench_rrf_fusion_overlapping,
    // Answer-pack planner/render benchmarks
    bench_answer_pack_candidate_hydration,
    bench_answer_pack_planner_scaling,
    bench_answer_pack_renderers,
    bench_answer_pack_token_budget_scaling,
    // Vector index benchmarks
    bench_empty_search,
    bench_vector_index_search_10k,
    bench_vector_index_search_50k,
    bench_vector_index_search_50k_filtered,
    bench_vector_index_search_50k_loaded,
    bench_vector_search_scaling,
    // Opt 1.1: Dot product benchmarks (scalar vs SIMD)
    bench_dot_product_f32,
    bench_dot_product_f16,
    bench_dot_product_f16_throughput,
);
criterion_main!(benches);
