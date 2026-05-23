//! Performance sanity tests for robot mode CLI flows.
//!
//! These tests verify that robot-help, robot-docs, and trace mode
//! execute within acceptable latency bounds for AI agent usage.
//! Targets: <200ms for --robot-help, <300ms for robot-docs topics.

use assert_cmd::Command;
use coding_agent_search::search::pack_planner::{
    PackCandidate, PackFreshnessPolicy, PackPlanRequest, PackPlannerLimits, PackRenderFormat,
    PackRenderRequest, PlannedAnswerPack, plan_answer_pack, render_answer_pack,
};
use coding_agent_search::search::query::{MatchType, SearchHit};
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

const ANSWER_PACK_PERF_NOW_MS: i64 = 1_764_000_000_000;
const ANSWER_PACK_FRESHNESS_WINDOW_SECONDS: i64 = 30 * 24 * 60 * 60;
static CLI_PERF_LOCK: Mutex<()> = Mutex::new(());

fn cli_perf_lock() -> MutexGuard<'static, ()> {
    match CLI_PERF_LOCK.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn base_cmd() -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");
    cmd
}

fn health_fixture_data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("search_demo_data")
}

/// Measure execution time of a command.
fn measure_cmd(cmd: &mut Command) -> (Duration, bool) {
    let start = Instant::now();
    let result = cmd.output();
    let elapsed = start.elapsed();
    let success = result.map(|o| o.status.success()).unwrap_or(false);
    (elapsed, success)
}

/// Run a command multiple times and return the median duration.
fn measure_median(args: &[&str], runs: usize) -> Duration {
    let mut durations: Vec<Duration> = Vec::with_capacity(runs);

    for _ in 0..runs {
        let mut cmd = base_cmd();
        cmd.args(args);
        let (elapsed, _) = measure_cmd(&mut cmd);
        durations.push(elapsed);
    }

    durations.sort();
    durations[runs / 2]
}

fn percentile_duration(
    mut durations: Vec<Duration>,
    numerator: usize,
    denominator: usize,
) -> Duration {
    durations.sort();
    let index = durations.len().saturating_sub(1) * numerator / denominator;
    durations[index]
}

fn answer_pack_perf_hit(idx: usize) -> SearchHit {
    SearchHit {
        title: format!("Answer pack SLO fixture {idx}"),
        snippet: format!("checkout failure answer pack evidence snippet {idx}"),
        content: format!(
            "Fixture candidate {idx} covers checkout failure, answer pack evidence, freshness \
             health, token budget pressure, omitted citations, and JSON Markdown rendering. \
             It includes enough text to exercise excerpt truncation and redaction paths without \
             loading a real user archive."
        ),
        content_hash: 0x5151_0000_u64 + idx as u64,
        score: 1_000.0 - idx as f32,
        source_path: format!("/tmp/cass-answer-pack-slo/session-{idx:04}.jsonl"),
        agent: format!("bench-agent-{}", idx % 8),
        workspace: format!("/workspace/pack-slo-{}", idx % 12),
        workspace_original: idx
            .is_multiple_of(5)
            .then(|| format!("~/src/pack-slo-{idx}")),
        created_at: Some(ANSWER_PACK_PERF_NOW_MS - (idx as i64 % 90) * 3_600_000),
        line_number: Some(idx % 240 + 1),
        match_type: if idx.is_multiple_of(3) {
            MatchType::Exact
        } else {
            MatchType::Substring
        },
        source_id: if idx.is_multiple_of(7) {
            "remote-source".to_string()
        } else {
            "local".to_string()
        },
        origin_kind: if idx.is_multiple_of(7) {
            "remote".to_string()
        } else {
            "local".to_string()
        },
        origin_host: idx
            .is_multiple_of(7)
            .then(|| format!("worker-{}.local", idx % 3)),
        conversation_id: Some(idx as i64),
    }
}

fn answer_pack_perf_candidates(count: usize) -> Vec<PackCandidate> {
    (0..count)
        .map(|idx| {
            let hit = answer_pack_perf_hit(idx);
            let mut candidate = PackCandidate::from_search_hit(&hit, 4, 1);
            candidate.hybrid_rank = Some(idx + 1);
            candidate.source_explicitly_requested = idx.is_multiple_of(11);
            candidate
        })
        .collect()
}

fn answer_pack_perf_limits() -> PackPlannerLimits {
    PackPlannerLimits {
        max_tokens: 12_000,
        max_sessions: 8,
        max_evidence: 24,
        context_lines: 3,
        max_excerpt_chars: 1_200,
    }
}

fn answer_pack_perf_plan(
    candidates: Vec<PackCandidate>,
    limits: PackPlannerLimits,
) -> PlannedAnswerPack {
    plan_answer_pack(PackPlanRequest {
        now_ms: ANSWER_PACK_PERF_NOW_MS,
        limits,
        freshness_policy: PackFreshnessPolicy::PreferRecent,
        freshness_window_seconds: ANSWER_PACK_FRESHNESS_WINDOW_SECONDS,
        candidates,
        explain_selection: true,
    })
    .expect("answer pack SLO plan")
}

fn answer_pack_perf_render_request(
    format: PackRenderFormat,
    limits: PackPlannerLimits,
) -> PackRenderRequest {
    PackRenderRequest {
        query_text: "checkout failure answer pack freshness".to_string(),
        normalized_query: "checkout failure answer pack freshness".to_string(),
        generated_at_ms: ANSWER_PACK_PERF_NOW_MS,
        elapsed_ms: 0,
        request_id: Some("answer-pack-slo".to_string()),
        format,
        limits,
        search_mode: "lexical".to_string(),
        fallback_mode: Some("lexical".to_string()),
        semantic_joined: false,
        freshness_policy: PackFreshnessPolicy::PreferRecent,
        freshness_window_seconds: ANSWER_PACK_FRESHNESS_WINDOW_SECONDS,
        redaction_policy: "strict".to_string(),
        sensitive_output: false,
        skill_content_included: false,
        explain_selection: true,
        readiness: Default::default(),
    }
}

fn answer_pack_candidate_memory_proxy_bytes(candidates: &[PackCandidate]) -> usize {
    candidates
        .iter()
        .map(|candidate| {
            std::mem::size_of::<PackCandidate>()
                + candidate.candidate_id.len()
                + candidate.source_path.len()
                + candidate.workspace.len()
                + candidate.agent.len()
                + candidate.excerpt.len()
        })
        .sum()
}

// =============================================================================
// Robot-help latency tests
// =============================================================================

#[test]
fn robot_help_latency_under_200ms() {
    let _guard = cli_perf_lock();

    // Warm-up run (cold start may be slower)
    let _ = base_cmd().args(["--robot-help"]).output();

    let median = measure_median(&["--robot-help"], 5);

    assert!(
        median < Duration::from_millis(200),
        "robot-help median latency {}ms exceeds 200ms threshold",
        median.as_millis()
    );
}

#[test]
fn robot_help_with_color_never_latency() {
    let _guard = cli_perf_lock();

    let _ = base_cmd().args(["--color=never", "--robot-help"]).output();

    let median = measure_median(&["--color=never", "--robot-help"], 5);

    assert!(
        median < Duration::from_millis(200),
        "robot-help (--color=never) median latency {}ms exceeds 200ms threshold",
        median.as_millis()
    );
}

// =============================================================================
// Robot-docs latency tests
// =============================================================================

#[test]
fn robot_docs_guide_latency_under_300ms() {
    let _guard = cli_perf_lock();

    let _ = base_cmd().args(["robot-docs", "guide"]).output();

    let median = measure_median(&["robot-docs", "guide"], 5);

    assert!(
        median < Duration::from_millis(300),
        "robot-docs guide median latency {}ms exceeds 300ms threshold",
        median.as_millis()
    );
}

#[test]
fn robot_docs_commands_latency_under_300ms() {
    let _guard = cli_perf_lock();

    let _ = base_cmd().args(["robot-docs", "commands"]).output();

    let median = measure_median(&["robot-docs", "commands"], 5);

    assert!(
        median < Duration::from_millis(300),
        "robot-docs commands median latency {}ms exceeds 300ms threshold",
        median.as_millis()
    );
}

#[test]
fn robot_docs_topics_latency_under_200ms() {
    let _guard = cli_perf_lock();

    let _ = base_cmd().args(["robot-docs", "topics"]).output();

    let median = measure_median(&["robot-docs", "topics"], 5);

    assert!(
        median < Duration::from_millis(200),
        "robot-docs topics median latency {}ms exceeds 200ms threshold",
        median.as_millis()
    );
}

#[test]
fn robot_docs_exit_codes_latency_under_200ms() {
    let _guard = cli_perf_lock();

    let _ = base_cmd().args(["robot-docs", "exit-codes"]).output();

    let median = measure_median(&["robot-docs", "exit-codes"], 5);

    assert!(
        median < Duration::from_millis(200),
        "robot-docs exit-codes median latency {}ms exceeds 200ms threshold",
        median.as_millis()
    );
}

#[test]
fn robot_docs_wrap_latency_under_200ms() {
    let _guard = cli_perf_lock();

    let _ = base_cmd().args(["robot-docs", "wrap"]).output();

    let median = measure_median(&["robot-docs", "wrap"], 5);

    assert!(
        median < Duration::from_millis(200),
        "robot-docs wrap median latency {}ms exceeds 200ms threshold",
        median.as_millis()
    );
}

// =============================================================================
// Introspection latency tests
// =============================================================================

#[test]
fn introspect_latency_under_300ms() {
    let _guard = cli_perf_lock();

    let _ = base_cmd().args(["introspect", "--json"]).output();

    let median = measure_median(&["introspect", "--json"], 5);

    assert!(
        median < Duration::from_millis(300),
        "introspect median latency {}ms exceeds 300ms threshold",
        median.as_millis()
    );
}

#[test]
fn api_version_latency_under_150ms() {
    let _guard = cli_perf_lock();

    let _ = base_cmd().args(["api-version", "--json"]).output();

    let median = measure_median(&["api-version", "--json"], 5);

    assert!(
        median < Duration::from_millis(150),
        "api-version median latency {}ms exceeds 150ms threshold",
        median.as_millis()
    );
}

#[test]
fn capabilities_latency_under_300ms() {
    let _guard = cli_perf_lock();

    let _ = base_cmd().args(["capabilities", "--json"]).output();

    let median = measure_median(&["capabilities", "--json"], 5);

    assert!(
        median < Duration::from_millis(300),
        "capabilities median latency {}ms exceeds 300ms threshold",
        median.as_millis()
    );
}

// =============================================================================
// Trace mode overhead tests
// =============================================================================

#[test]
fn trace_mode_adds_minimal_overhead() {
    let _guard = cli_perf_lock();

    // Warm-up runs
    let _ = base_cmd().args(["--robot-help"]).output();
    let _ = base_cmd().args(["--trace", "--robot-help"]).output();

    // Measure without trace
    let baseline = measure_median(&["--robot-help"], 5);

    // Measure with trace
    let with_trace = measure_median(&["--trace", "--robot-help"], 5);

    // Trace should add at most 50ms overhead
    let overhead = with_trace.saturating_sub(baseline);
    assert!(
        overhead < Duration::from_millis(50),
        "trace mode adds {}ms overhead (threshold: 50ms), baseline: {}ms, with_trace: {}ms",
        overhead.as_millis(),
        baseline.as_millis(),
        with_trace.as_millis()
    );
}

#[test]
fn trace_mode_on_robot_docs_adds_minimal_overhead() {
    let _guard = cli_perf_lock();

    // Warm-up runs
    let _ = base_cmd().args(["robot-docs", "guide"]).output();
    let _ = base_cmd().args(["--trace", "robot-docs", "guide"]).output();

    // Measure without trace
    let baseline = measure_median(&["robot-docs", "guide"], 5);

    // Measure with trace
    let with_trace = measure_median(&["--trace", "robot-docs", "guide"], 5);

    // Trace should add at most 50ms overhead
    let overhead = with_trace.saturating_sub(baseline);
    assert!(
        overhead < Duration::from_millis(50),
        "trace mode on robot-docs adds {}ms overhead (threshold: 50ms), baseline: {}ms, with_trace: {}ms",
        overhead.as_millis(),
        baseline.as_millis(),
        with_trace.as_millis()
    );
}

// =============================================================================
// Answer-pack planner/render SLO tests
// =============================================================================

#[test]
fn answer_pack_planner_and_render_p95_under_slo() {
    const CANDIDATE_COUNT: usize = 192;
    const RUNS: usize = 30;
    const STAGE_P95_BUDGET: Duration = Duration::from_millis(250);

    let candidates = answer_pack_perf_candidates(CANDIDATE_COUNT);
    let limits = answer_pack_perf_limits();
    let memory_proxy_bytes = answer_pack_candidate_memory_proxy_bytes(&candidates);

    let mut plan_durations = Vec::with_capacity(RUNS);
    let mut json_durations = Vec::with_capacity(RUNS);
    let mut markdown_durations = Vec::with_capacity(RUNS);
    let mut last_plan = answer_pack_perf_plan(candidates.clone(), limits.clone());

    for _ in 0..RUNS {
        let start = Instant::now();
        let plan = answer_pack_perf_plan(candidates.clone(), limits.clone());
        plan_durations.push(start.elapsed());

        let json_request = answer_pack_perf_render_request(PackRenderFormat::Json, limits.clone());
        let start = Instant::now();
        let json = render_answer_pack(&plan, &json_request).expect("render answer pack JSON");
        json_durations.push(start.elapsed());
        assert!(
            json.contains("\"evidence\""),
            "JSON render should preserve evidence field"
        );

        let markdown_request =
            answer_pack_perf_render_request(PackRenderFormat::Markdown, limits.clone());
        let start = Instant::now();
        let markdown =
            render_answer_pack(&plan, &markdown_request).expect("render answer pack Markdown");
        markdown_durations.push(start.elapsed());
        assert!(
            markdown.contains("Evidence"),
            "Markdown render should preserve evidence section"
        );

        last_plan = plan;
    }

    let plan_p50 = percentile_duration(plan_durations.clone(), 50, 100);
    let plan_p95 = percentile_duration(plan_durations, 95, 100);
    let json_p50 = percentile_duration(json_durations.clone(), 50, 100);
    let json_p95 = percentile_duration(json_durations, 95, 100);
    let markdown_p50 = percentile_duration(markdown_durations.clone(), 50, 100);
    let markdown_p95 = percentile_duration(markdown_durations, 95, 100);
    let budget = last_plan.diagnostics.budget;
    let utilization_pct = last_plan.estimated_tokens.saturating_mul(100) / budget.max_tokens;

    println!(
        "answer-pack SLO: candidates={CANDIDATE_COUNT}, selected={}, omitted={}, utilization={}%, memory_proxy={}KiB, plan_p50={}ms, plan_p95={}ms, json_p50={}ms, json_p95={}ms, markdown_p50={}ms, markdown_p95={}ms",
        last_plan.selected_evidence_count,
        last_plan.omitted.len(),
        utilization_pct,
        memory_proxy_bytes / 1024,
        plan_p50.as_millis(),
        plan_p95.as_millis(),
        json_p50.as_millis(),
        json_p95.as_millis(),
        markdown_p50.as_millis(),
        markdown_p95.as_millis()
    );

    assert!(
        last_plan.estimated_tokens <= budget.max_output_tokens_with_overflow,
        "answer-pack token budget overflow: estimated={} max_with_overflow={} candidates={} selected={} omitted={} utilization={}%",
        last_plan.estimated_tokens,
        budget.max_output_tokens_with_overflow,
        CANDIDATE_COUNT,
        last_plan.selected_evidence_count,
        last_plan.omitted.len(),
        utilization_pct
    );

    assert!(
        plan_p95 < STAGE_P95_BUDGET
            && json_p95 < STAGE_P95_BUDGET
            && markdown_p95 < STAGE_P95_BUDGET,
        "answer-pack SLO exceeded: candidates={CANDIDATE_COUNT}, selected={}, omitted={}, utilization={}%, memory_proxy={}KiB, plan_p50={}ms, plan_p95={}ms, json_p50={}ms, json_p95={}ms, markdown_p50={}ms, markdown_p95={}ms, budget={}ms",
        last_plan.selected_evidence_count,
        last_plan.omitted.len(),
        utilization_pct,
        memory_proxy_bytes / 1024,
        plan_p50.as_millis(),
        plan_p95.as_millis(),
        json_p50.as_millis(),
        json_p95.as_millis(),
        markdown_p50.as_millis(),
        markdown_p95.as_millis(),
        STAGE_P95_BUDGET.as_millis()
    );
}

// =============================================================================
// Startup latency tests
// =============================================================================

#[test]
fn help_flag_latency_under_200ms() {
    let _guard = cli_perf_lock();

    let _ = base_cmd().args(["--help"]).output();

    let median = measure_median(&["--help"], 5);

    assert!(
        median < Duration::from_millis(200),
        "--help median latency {}ms exceeds 200ms threshold",
        median.as_millis()
    );
}

#[test]
fn version_flag_latency_under_150ms() {
    let _guard = cli_perf_lock();

    let _ = base_cmd().args(["--version"]).output();

    let median = measure_median(&["--version"], 5);

    assert!(
        median < Duration::from_millis(150),
        "--version median latency {}ms exceeds 150ms threshold",
        median.as_millis()
    );
}

// =============================================================================
// Cold start tests (first invocation)
// =============================================================================

#[test]
fn robot_help_cold_start_under_500ms() {
    let _guard = cli_perf_lock();

    // Single invocation (no warm-up) - cold start scenario
    let mut cmd = base_cmd();
    cmd.args(["--robot-help"]);
    let (elapsed, success) = measure_cmd(&mut cmd);

    assert!(success, "robot-help command should succeed");
    assert!(
        elapsed < Duration::from_millis(500),
        "robot-help cold start latency {}ms exceeds 500ms threshold",
        elapsed.as_millis()
    );
}

// =============================================================================
// Combined workflow latency tests
// =============================================================================

#[test]
fn typical_agent_discovery_workflow_under_1sec() {
    let _guard = cli_perf_lock();

    // Simulate typical agent discovery workflow:
    // 1. api-version
    // 2. capabilities
    // 3. robot-docs guide

    let start = Instant::now();

    let _ = base_cmd().args(["api-version", "--json"]).output();
    let _ = base_cmd().args(["capabilities", "--json"]).output();
    let _ = base_cmd().args(["robot-docs", "guide"]).output();

    let total = start.elapsed();

    assert!(
        total < Duration::from_secs(1),
        "typical agent discovery workflow took {}ms (threshold: 1000ms)",
        total.as_millis()
    );
}

#[test]
fn health_check_latency_under_100ms() {
    let _guard = cli_perf_lock();

    let data_dir = health_fixture_data_dir();
    let data_dir = data_dir
        .to_str()
        .expect("fixture path should be valid UTF-8");

    let _ = base_cmd()
        .args(["health", "--json", "--data-dir", data_dir])
        .output();

    let median = measure_median(&["health", "--json", "--data-dir", data_dir], 5);

    assert!(
        median < Duration::from_millis(100),
        "health check median latency {}ms exceeds 100ms threshold",
        median.as_millis()
    );
}
