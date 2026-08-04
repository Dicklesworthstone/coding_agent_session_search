//! Real-binary search-quality evaluation gate (bead
//! `coding_agent_session_search-guided-ops-repro-trust-5u82n.7`,
//! "Create search quality evaluation harness with qrels and drift reports").
//!
//! Why this gate exists
//! --------------------
//! `src/search_quality_eval.rs` is the pure, unit-tested scoring core (qrels +
//! observed hits → recall@k / precision@k / MRR / trust-tier distribution /
//! regression diff). This gate proves the *live* half: a checked-in corpus
//! (`tests/fixtures/search_quality/corpus.json`) is seeded as real Codex
//! sessions and indexed once by the real `cass` binary; checked-in relevance
//! judgments (`qrels.json`) are then issued against that index via
//! `cass search --json --robot-meta`, the returned hits are reduced to
//! metadata-only `ObservedHit`s, and the core assembles a `QualityReport`. The
//! gate asserts:
//!   * every judgment is satisfied on a real index (observed == expected),
//!   * the live trust verdict feeds the trust-tier distribution (the aged
//!     `staleadvice` session scores `stale`; fresh sessions score `unverified`),
//!   * JSON + markdown artifacts are emitted with query id / expected refs /
//!     observed refs / mode / latency / per-query diff, and
//!   * **no raw private text leaks**: the privacy fixture's body email never
//!     appears in either artifact (the report holds refs + metrics only).
//!
//! This is the small, deterministic CI suite the bead calls for; larger
//! corpus benchmarking stays separate/manual.
//!
//! Isolation mirrors the readiness/trust gates: a fresh `tempdir` with
//! `HOME`/`XDG_*`/cwd redirected into it, `CASS_SEMANTIC_EMBEDDER=hash` to keep
//! semantic acquisition offline, `NO_COLOR=1`, and the `.12.2` bounded runner
//! (`spawn_with_timeout_or_diag`) so a hang is a loud diagnostic, not a silent
//! pass. The test is panic-free (Result-returning + an `ensure` helper, no
//! `unwrap`/`expect`/`panic!`/`assert!`) so the new file stays UBS 0-critical.

mod util;

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use assert_cmd::cargo::cargo_bin;
use serde::Deserialize;
use serde_json::{Value, json};

use coding_agent_search::search_quality_eval::{
    ObservedHit, Qrel, QualityReport, QueryRun, build_report_labeled, render_markdown,
    sanitize_doc_ref,
};
use util::timeout::spawn_with_timeout_or_diag;

type TestResult = Result<(), Box<dyn Error>>;

/// Bound for the one-time index that builds the fixture corpus (a handful of
/// tiny seeded sessions index well under a second; the bound is slack for CI).
const INDEX_TIMEOUT: Duration = Duration::from_secs(120);
/// Bound for a single bounded `search` invocation.
const SEARCH_TIMEOUT: Duration = Duration::from_secs(60);

const DAY_MS: i64 = 86_400_000;

/// The fake email planted in the privacy fixture's body. It must never appear in
/// an emitted artifact — the report carries refs + metrics, never body text.
const PLANTED_BODY_TEXT: &str = "private.user@example.invalid";

/// Return `Err(msg)` when `cond` is false; the message closure is only paid for
/// on failure. Keeps the gate panic-free (no `assert!` panic surface).
fn ensure(cond: bool, msg: impl FnOnce() -> String) -> TestResult {
    if cond { Ok(()) } else { Err(msg().into()) }
}

fn head(s: &str) -> String {
    s.chars().take(400).collect()
}

/// Current wall-clock epoch milliseconds.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// A fresh isolated `(tempdir guard, home, data_dir)`.
fn isolated_home() -> Result<(tempfile::TempDir, PathBuf, PathBuf), Box<dyn Error>> {
    let tmp = tempfile::TempDir::new()?;
    let home = tmp.path().join("home");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(&data_dir)?;
    Ok((tmp, home, data_dir))
}

// ---------------------------------------------------------------------------
// Checked-in fixtures (corpus + qrels).
// ---------------------------------------------------------------------------

/// One curated corpus document → one seeded Codex rollout session.
#[derive(Debug, Clone, Deserialize)]
struct CorpusDoc {
    stem: String,
    keywords: Vec<String>,
    age_days: i64,
    extra_text: String,
}

#[derive(Debug, Deserialize)]
struct CorpusFile {
    docs: Vec<CorpusDoc>,
}

#[derive(Debug, Deserialize)]
struct QrelsFile {
    qrels: Vec<Qrel>,
}

/// Path to a checked-in fixture under `tests/fixtures/search_quality/`.
fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/search_quality")
        .join(name)
}

fn load_corpus() -> Result<Vec<CorpusDoc>, Box<dyn Error>> {
    let raw = std::fs::read_to_string(fixture_path("corpus.json"))?;
    let parsed: CorpusFile = serde_json::from_str(&raw)?;
    Ok(parsed.docs)
}

fn load_qrels() -> Result<Vec<Qrel>, Box<dyn Error>> {
    let raw = std::fs::read_to_string(fixture_path("qrels.json"))?;
    let parsed: QrelsFile = serde_json::from_str(&raw)?;
    Ok(parsed.qrels)
}

// ---------------------------------------------------------------------------
// Seeding + invocation helpers.
// ---------------------------------------------------------------------------

/// Serialize JSON values to newline-delimited text (flat helper so the per-line
/// `to_string` allocation is not inside a loop in the caller).
fn serialize_jsonl(lines: &[Value]) -> Result<String, Box<dyn Error>> {
    let mut body = String::new();
    for line in lines {
        body.push_str(&serde_json::to_string(line)?);
        body.push('\n');
    }
    Ok(body)
}

/// Seed one corpus doc as `rollout-<stem>.jsonl` under `CODEX_HOME`, dated
/// `age_days` in the past, with a user message carrying its keywords + extra
/// text. The `rollout-` prefix is required for the Codex connector to ingest it.
fn seed_corpus_doc(codex_home: &Path, doc: &CorpusDoc) -> TestResult {
    let sessions = codex_home.join("sessions/2026/04/23");
    std::fs::create_dir_all(&sessions)?;
    let created_ms = now_ms() - doc.age_days * DAY_MS;
    let iso = |ms: i64| -> String {
        chrono::DateTime::from_timestamp_millis(ms)
            .map(|d| d.to_rfc3339())
            .unwrap_or_default()
    };
    let filename = format!("rollout-{}.jsonl", doc.stem);
    let body_text = format!("{} {}", doc.keywords.join(" "), doc.extra_text);
    let workspace = codex_home.to_string_lossy().into_owned();
    let lines = [
        json!({
            "timestamp": iso(created_ms),
            "type": "session_meta",
            "payload": { "id": filename, "cwd": workspace, "cli_version": "0.42.0" },
        }),
        json!({
            "timestamp": iso(created_ms + 1_000),
            "type": "response_item",
            "payload": {
                "type": "message", "role": "user",
                "content": [{ "type": "input_text", "text": body_text }],
            },
        }),
    ];
    let body = serialize_jsonl(&lines)?;
    std::fs::write(sessions.join(filename), body)?;
    Ok(())
}

/// Build a `cass` command with the fixture's isolation env.
fn cass_cmd(home: &Path, codex_home: &Path, args: &[String]) -> Command {
    let mut cmd = Command::new(cargo_bin("cass"));
    cmd.args(args)
        .current_dir(home)
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join("xdg-data"))
        .env("XDG_CONFIG_HOME", home.join("xdg-config"))
        .env("XDG_CACHE_HOME", home.join("xdg-cache"))
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_SEMANTIC_EMBEDDER", "hash")
        .env("NO_COLOR", "1")
        .env("CODEX_HOME", codex_home)
        .env_remove("CLAUDE_CONFIG_DIR");
    cmd
}

/// Append the shared `--data-dir <dir>` tail to a base argv.
fn argv(base: &[&str], data_dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = base.iter().map(|s| (*s).to_string()).collect();
    v.push("--data-dir".to_string());
    v.push(data_dir.to_string_lossy().into_owned());
    v
}

/// The hits array of a search payload.
fn hits(payload: &Value) -> Vec<Value> {
    payload
        .get("hits")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// Reduce a hit's `source_path` to the seeded stable doc ref (the session stem
/// minus the `rollout-` prefix and `.jsonl` suffix), sanitized.
fn doc_ref_from_path(source_path: &str) -> Option<String> {
    let name = Path::new(source_path).file_name()?.to_str()?;
    let stem = name.strip_suffix(".jsonl").unwrap_or(name);
    let stem = stem.strip_prefix("rollout-").unwrap_or(stem);
    let cleaned = sanitize_doc_ref(stem);
    (!cleaned.is_empty()).then_some(cleaned)
}

/// Read an optional string field from a `_meta` block.
fn meta_str(payload: &Value, key: &str) -> Option<String> {
    payload
        .get("_meta")
        .and_then(|m| m.get(key))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Run one qrel against the live index and reduce the result to a `QueryRun`.
fn run_qrel(
    home: &Path,
    codex_home: &Path,
    data_dir: &Path,
    qrel: &Qrel,
) -> Result<QueryRun, Box<dyn Error>> {
    let limit = qrel.k.to_string();
    let args = argv(
        &[
            "search",
            qrel.query.as_str(),
            "--json",
            "--robot-meta",
            "--limit",
            limit.as_str(),
        ],
        data_dir,
    );
    let cmd = cass_cmd(home, codex_home, &args);
    let label = format!("search_quality_{}", qrel.id);
    let started = Instant::now();
    let out = spawn_with_timeout_or_diag(cmd, &label, Some(data_dir), SEARCH_TIMEOUT);
    let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

    let stdout = String::from_utf8_lossy(&out.stdout);
    let payload: Value = serde_json::from_str(stdout.trim())
        .map_err(|e| format!("{label}: stdout not JSON: {e}; head: {}", head(&stdout)))?;

    let mut observed: Vec<ObservedHit> = Vec::new();
    for (idx, hit) in hits(&payload).iter().enumerate() {
        let source_path = hit
            .get("source_path")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let Some(doc_ref) = doc_ref_from_path(source_path) else {
            continue;
        };
        let trust_tier = hit
            .get("trust")
            .and_then(|t| t.get("trust_tier"))
            .and_then(Value::as_str)
            .map(str::to_string);
        observed.push(ObservedHit {
            rank: idx + 1,
            doc_ref,
            trust_tier,
        });
    }

    Ok(QueryRun {
        qrel: qrel.clone(),
        observed,
        realized_mode: meta_str(&payload, "search_mode"),
        fallback_tier: meta_str(&payload, "fallback_tier"),
        latency_ms,
    })
}

/// The trust tier observed for `doc_ref` within a run (empty when absent).
fn tier_for(run: &QueryRun, doc_ref: &str) -> String {
    run.observed
        .iter()
        .find(|h| h.doc_ref == doc_ref)
        .and_then(|h| h.trust_tier.clone())
        .unwrap_or_default()
}

#[test]
fn search_quality_suite_meets_qrels_and_emits_clean_artifacts() -> TestResult {
    let (tmp, home, data_dir) = isolated_home()?;
    let codex_home = home.join(".codex");

    // 1) Seed the curated corpus from the checked-in manifest.
    let corpus = load_corpus()?;
    ensure(corpus.len() >= 4, || {
        format!("expected >=4 corpus docs, got {}", corpus.len())
    })?;
    for doc in &corpus {
        seed_corpus_doc(&codex_home, doc)?;
    }

    // 2) Index once.
    let index_args = argv(
        &["index", "--full", "--json", "--no-progress-events"],
        &data_dir,
    );
    let index_cmd = cass_cmd(&home, &codex_home, &index_args);
    let index_out = spawn_with_timeout_or_diag(
        index_cmd,
        "search_quality_index",
        Some(&data_dir),
        INDEX_TIMEOUT,
    );
    let index_stdout = String::from_utf8_lossy(&index_out.stdout);
    let index_json: Value = serde_json::from_str(index_stdout.trim())
        .map_err(|e| format!("index stdout not JSON: {e}; head: {}", head(&index_stdout)))?;
    ensure(
        index_json.get("success").and_then(Value::as_bool) == Some(true),
        || {
            format!(
                "index did not report success=true: {}",
                head(&index_json.to_string())
            )
        },
    )?;

    // 3) Run every checked-in judgment against the live index.
    let qrels = load_qrels()?;
    ensure(!qrels.is_empty(), || "no qrels loaded".to_string())?;
    for qrel in &qrels {
        ensure(!qrel.expected_refs.is_empty(), || {
            format!("qrel `{}` has no expected_refs", qrel.id)
        })?;
        ensure(qrel.k > 0, || format!("qrel `{}` has k=0", qrel.id))?;
        let sanitized: Vec<String> = qrel
            .expected_refs
            .iter()
            .map(|doc_ref| sanitize_doc_ref(doc_ref))
            .collect();
        ensure(sanitized == qrel.expected_refs, || {
            format!(
                "qrel `{}` contains a non-canonical document ref: {:?}",
                qrel.id, qrel.expected_refs
            )
        })?;
    }
    let mut runs: Vec<QueryRun> = Vec::new();
    for qrel in &qrels {
        runs.push(run_qrel(&home, &codex_home, &data_dir, qrel)?);
    }

    // 4) Assemble the report via the pure core.
    let report = build_report_labeled(&runs, Some("search-quality-e2e".to_string()));

    // 5) Every judgment must be satisfied on a real index. Both false negatives
    //    and false positives fail the gate and surface an actionable diff.
    ensure(report.aggregate.query_count == qrels.len(), || {
        format!(
            "evaluated {} queries, expected {}",
            report.aggregate.query_count,
            qrels.len()
        )
    })?;
    for q in &report.queries {
        ensure(q.passed, || {
            format!(
                "qrel `{}` (query `{}`) failed quality: recall={:.3}, precision={:.3}, missing={:?}, unexpected={:?}, observed={:?}",
                q.id,
                q.query,
                q.recall_at_k,
                q.precision_at_k,
                q.missing_refs,
                q.unexpected_refs,
                q.observed_refs
            )
        })?;
    }
    ensure(report.aggregate.failed_count == 0, || {
        format!(
            "{} queries failed their judgment",
            report.aggregate.failed_count
        )
    })?;
    ensure(report.aggregate.mean_recall_at_k >= 1.0 - 1e-9, || {
        format!(
            "mean recall@k below 1.0: {}",
            report.aggregate.mean_recall_at_k
        )
    })?;
    ensure(report.aggregate.mean_precision_at_k >= 1.0 - 1e-9, || {
        format!(
            "mean precision@k below 1.0: {}",
            report.aggregate.mean_precision_at_k
        )
    })?;
    ensure(
        report
            .queries
            .iter()
            .all(|query| query.unexpected_refs.is_empty()),
        || "one or more qrels returned unexpected refs".to_string(),
    )?;

    // 6) The live trust verdict feeds the distribution: the aged advice session
    //    is `stale`; fresh sessions are `unverified`. This proves the trust-tier
    //    distribution column is wired end-to-end, not just in the unit core.
    let dist = &report.aggregate.trust_tier_distribution;
    ensure(dist.get("stale").copied().unwrap_or(0) >= 1, || {
        format!("expected >=1 `stale` hit in trust distribution, got {dist:?}")
    })?;
    ensure(dist.get("unverified").copied().unwrap_or(0) >= 1, || {
        format!("expected >=1 `unverified` hit in trust distribution, got {dist:?}")
    })?;
    let stale_run = runs
        .iter()
        .find(|r| r.qrel.id == "q-stale-advice")
        .ok_or("q-stale-advice run missing")?;
    ensure(tier_for(stale_run, "staleadvice") == "stale", || {
        format!(
            "aged advice hit should score `stale`, got `{}`",
            tier_for(stale_run, "staleadvice")
        )
    })?;

    // 7) Emit JSON + markdown artifacts (query id / expected / observed / mode /
    //    latency / diff). Honor an override dir for CI capture; default to the
    //    tempdir so a routine run leaves nothing behind.
    let artifact_dir = match std::env::var("CASS_SEARCH_QUALITY_ARTIFACT_DIR") {
        Ok(d) if !d.is_empty() => PathBuf::from(d),
        _ => tmp.path().join("artifacts"),
    };
    std::fs::create_dir_all(&artifact_dir)?;
    let json_path = artifact_dir.join("search-quality-report.json");
    let md_path = artifact_dir.join("search-quality-report.md");
    let json_str = serde_json::to_string_pretty(&report)?;
    let md_str = render_markdown(&report);
    std::fs::write(&json_path, &json_str)?;
    std::fs::write(&md_path, &md_str)?;

    // The JSON artifact round-trips back into the typed report.
    let reparsed: QualityReport = serde_json::from_str(&json_str)?;
    ensure(reparsed == report, || {
        "emitted JSON did not round-trip back into QualityReport".to_string()
    })?;
    ensure(md_str.contains("# Search Quality Report"), || {
        "markdown artifact missing its header".to_string()
    })?;
    ensure(
        md_str.contains("## Per-query") && md_str.contains("## Trust-tier distribution"),
        || "markdown artifact missing required sections".to_string(),
    )?;

    // 8) No raw private text: the privacy fixture's body email is in the seeded
    //    corpus but must never reach an emitted artifact.
    ensure(!json_str.contains(PLANTED_BODY_TEXT), || {
        "JSON artifact leaked the privacy fixture's body email".to_string()
    })?;
    ensure(!md_str.contains(PLANTED_BODY_TEXT), || {
        "markdown artifact leaked the privacy fixture's body email".to_string()
    })?;

    // A one-line summary so a remote (rch) run surfaces the outcome on stderr.
    eprintln!(
        "[search-quality] queries={} passed={} mean_recall={:.3} mean_precision={:.3} mean_mrr={:.3} trust_dist={:?}",
        report.aggregate.query_count,
        report.aggregate.passed_count,
        report.aggregate.mean_recall_at_k,
        report.aggregate.mean_precision_at_k,
        report.aggregate.mean_mrr,
        report.aggregate.trust_tier_distribution,
    );

    Ok(())
}
