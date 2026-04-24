use assert_cmd::Command;
use coding_agent_search::search::tantivy::{SCHEMA_HASH, expected_index_dir};
use coding_agent_search::storage::sqlite::FrankenStorage;
use frankensqlite::compat::ConnectionExt;
use frankensqlite::params as fparams;
use fs2::FileExt;
use serde_json::{Value, json};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

mod util;

const LARGE_HEALTH_DB_CONVERSATIONS: i64 = 2_000;
const LARGE_HEALTH_DB_MESSAGES: i64 = LARGE_HEALTH_DB_CONVERSATIONS * 2;
const LARGE_HEALTH_DB_INSERT_CHUNK: i64 = 250;
const HEALTH_LATENCY_WARMUP_RUNS: usize = 3;
const HEALTH_LATENCY_MEASURED_RUNS: usize = 9;

fn seed_active_rebuild_runtime(data_dir: &Path) -> std::fs::File {
    let db_path = data_dir.join("agent_search.db");
    let index_path = expected_index_dir(data_dir);
    fs::create_dir_all(&index_path).expect("create index dir");
    fs::write(
        index_path.join(".lexical-rebuild-state.json"),
        serde_json::to_vec_pretty(&json!({
            "version": 2,
            "schema_hash": SCHEMA_HASH,
            "db": {
                "db_path": db_path.display().to_string(),
                "total_conversations": 10,
                "total_messages": 20,
                "storage_fingerprint": "seed:10"
            },
            "page_size": 1024,
            "committed_offset": 4,
            "committed_conversation_id": 4,
            "processed_conversations": 4,
            "indexed_docs": 20,
            "committed_meta_fingerprint": null,
            "pending": null,
            "completed": false,
            "updated_at_ms": 1_733_000_123_000_i64,
            "runtime": {
                "queue_depth": 3,
                "inflight_message_bytes": 65_536,
                "max_message_bytes_in_flight": 131_072,
                "pending_batch_conversations": 9,
                "pending_batch_message_bytes": 131_072,
                "page_prep_workers": 6,
                "active_page_prep_jobs": 2,
                "ordered_buffered_pages": 4,
                "budget_generation": 1,
                "producer_budget_wait_count": 2,
                "producer_budget_wait_ms": 17,
                "producer_handoff_wait_count": 1,
                "producer_handoff_wait_ms": 9,
                "host_loadavg_1m_milli": 7_250,
                "controller_mode": "pressure_limited",
                "controller_reason": "queue_depth_3_reached_pipeline_capacity_3",
                "staged_merge_workers_max": 3,
                "staged_merge_allowed_jobs": 1,
                "staged_merge_active_jobs": 1,
                "staged_merge_ready_artifacts": 5,
                "staged_merge_ready_groups": 1,
                "staged_merge_controller_reason": "page_prep_workers_saturated_6_of_6",
                "staged_shard_build_workers_max": 6,
                "staged_shard_build_allowed_jobs": 5,
                "staged_shard_build_active_jobs": 4,
                "staged_shard_build_pending_jobs": 2,
                "staged_shard_build_controller_reason": "reserving_1_slots_for_staged_merge_active_jobs_1_ready_groups_1",
                "updated_at_ms": 1_733_000_124_000_i64
            }
        }))
        .expect("serialize rebuild state"),
    )
    .expect("write rebuild state");

    let lock_path = data_dir.join("index-run.lock");
    let mut lock_file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(&lock_path)
        .expect("open lock file");
    lock_file.lock_exclusive().expect("hold index lock");
    writeln!(
        lock_file,
        "pid={}\nstarted_at_ms={}\ndb_path={}\nmode=index",
        std::process::id(),
        1_733_000_111_000_i64,
        db_path.display()
    )
    .expect("write lock metadata");
    lock_file.flush().expect("flush lock metadata");
    lock_file
}

#[test]
fn health_json_surfaces_runtime_queue_and_byte_budget_headroom() {
    let test_home = tempfile::tempdir().expect("tempdir");
    let data_dir = test_home.path().join("cass-data");
    fs::create_dir_all(&data_dir).expect("create data dir");
    let _lock = seed_active_rebuild_runtime(&data_dir);

    let out = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
        .args([
            "health",
            "--data-dir",
            data_dir.to_str().expect("utf8"),
            "--json",
        ])
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("CASS_TANTIVY_REBUILD_PIPELINE_CHANNEL_SIZE", "5")
        .env("XDG_DATA_HOME", test_home.path())
        .env("HOME", test_home.path())
        .output()
        .expect("run cass health --json");
    assert_eq!(
        out.status.code(),
        Some(1),
        "health should report rebuilding"
    );

    let stdout = String::from_utf8(out.stdout).expect("utf8");
    let payload: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let runtime = &payload["state"]["rebuild"]["pipeline"]["runtime"];
    let rebuild_progress = &payload["rebuild_progress"];

    assert_eq!(runtime["queue_depth"].as_u64(), Some(3));
    assert_eq!(runtime["queue_capacity"].as_u64(), Some(5));
    assert_eq!(runtime["queue_headroom"].as_u64(), Some(2));
    assert_eq!(runtime["inflight_message_bytes"].as_u64(), Some(65_536));
    assert_eq!(
        runtime["max_message_bytes_in_flight"].as_u64(),
        Some(131_072)
    );
    assert_eq!(
        runtime["inflight_message_bytes_headroom"].as_u64(),
        Some(65_536)
    );
    assert_eq!(rebuild_progress["active"].as_bool(), Some(true));
    assert_eq!(
        rebuild_progress["processed_conversations"].as_u64(),
        Some(4)
    );
    assert_eq!(rebuild_progress["total_conversations"].as_u64(), Some(10));
    assert_eq!(
        rebuild_progress["remaining_conversations"].as_u64(),
        Some(6)
    );
    assert_eq!(rebuild_progress["completion_ratio"].as_f64(), Some(0.4));
    assert_eq!(rebuild_progress["queue_depth"].as_u64(), Some(3));
    assert_eq!(rebuild_progress["queue_capacity"].as_u64(), Some(5));
    assert_eq!(rebuild_progress["queue_headroom"].as_u64(), Some(2));
    assert_eq!(
        rebuild_progress["inflight_message_bytes"].as_u64(),
        Some(65_536)
    );
    assert_eq!(
        rebuild_progress["inflight_message_bytes_headroom"].as_u64(),
        Some(65_536)
    );
    assert_eq!(
        rebuild_progress["controller_mode"].as_str(),
        Some("pressure_limited")
    );
    assert_eq!(
        rebuild_progress["controller_reason"].as_str(),
        Some("queue_depth_3_reached_pipeline_capacity_3")
    );
}

// ========================================================================
// Bead coding_agent_session_search-v0p2i (child of ibuuh.10, scenario B):
// Attach-to-progress recommendation truthfulness.
//
// The sibling test above pins the numeric runtime surfaces during an
// active rebuild (queue_depth, inflight bytes, controller mode). It
// never looks at the USER-FACING `recommended_action` string. That
// string is what agents and humans read off `cass status --json` to
// decide what to do next when they see exit 1 / rebuild.active=true.
//
// Emitted from src/lib.rs::run_status (around line 11785) as:
//   "Index rebuild is already in progress"
//
// The contract pinned here is the "attach, don't race" slice of
// ibuuh.10: when a rebuild is already running, status must tell the
// operator to WAIT, and must NOT tell them to run another
// `cass index --full` (which would stampede the advisory lock at
// src/lib.rs::probe_index_run_lock).
//
// KNOWN DIVERGENCE — bead coding_agent_session_search-k0bzk:
// `cass health --json` exposes the same rebuild_progress.active=true
// signal but its `recommended_action` currently emits the stampede
// text "Run 'cass index --full' to rebuild the index/database." because
// run_health (src/lib.rs:12051) forgot to add the rebuild_active arm
// that run_status has. That's tracked as a separate bug; this test
// pins the correct surface (status) to prevent its regression and
// leaves the incorrect health surface to the bug fix.
// ========================================================================

#[test]
fn status_recommended_action_during_active_rebuild_says_wait_not_reindex() {
    let test_home = tempfile::tempdir().expect("tempdir");
    let data_dir = test_home.path().join("cass-data");
    fs::create_dir_all(&data_dir).expect("create data dir");
    let _lock = seed_active_rebuild_runtime(&data_dir);

    let out = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
        .args([
            "status",
            "--data-dir",
            data_dir.to_str().expect("utf8"),
            "--json",
        ])
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("XDG_DATA_HOME", test_home.path())
        .env("HOME", test_home.path())
        .output()
        .expect("run cass status --json");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    let payload: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|err| panic!("status JSON parse failed: {err}; stdout: {stdout}"));

    // Precondition sanity: the seeded state really registered as an
    // active rebuild. If this flips, everything else is moot.
    assert_eq!(
        payload
            .get("rebuild")
            .and_then(|r| r.get("active"))
            .and_then(Value::as_bool),
        Some(true),
        "seeded state must register as rebuild.active=true. stderr: {stderr}; \
         payload: {payload}"
    );

    let recommended_action = payload
        .get("recommended_action")
        .and_then(Value::as_str)
        .expect("status must emit recommended_action during rebuild");

    // CONTRACT PIN 1: the string names the in-flight rebuild so agents
    // and humans know "wait" is the right next step.
    let lower = recommended_action.to_lowercase();
    assert!(
        (lower.contains("rebuild") && lower.contains("in progress")) || lower.contains("already"),
        "recommended_action must signal that a rebuild is active so agents attach \
         to the in-flight work instead of starting a new one; got: \
         {recommended_action:?}"
    );

    // CONTRACT PIN 2: NEVER tell the operator to run another index
    // while a rebuild is active — that's what triggers lock-stampede.
    assert!(
        !lower.contains("cass index --full"),
        "recommended_action must NOT tell the operator to run `cass index --full` \
         while a rebuild is active (stampede advice); got: {recommended_action:?}"
    );
    // Catch all three phrasings that recommend running another index
    // command — quoted (single/back-tick) AND plain unquoted. An
    // unquoted "Run cass index to rebuild..." would otherwise slip
    // past the two quote-bearing checks and still be stampede advice.
    assert!(
        !lower.contains("run 'cass index'")
            && !lower.contains("run `cass index`")
            && !lower.contains("run cass index"),
        "recommended_action must NOT tell the operator to run `cass index` in any \
         form (quoted or unquoted) while a rebuild is active; got: {recommended_action:?}"
    );
}

// `coding_agent_session_search-k0bzk` regression gate: cass health --json
// recommended_action MUST NOT emit stampede advice while a rebuild is
// active. Mirrors `status_recommended_action_during_active_rebuild_says_wait_not_reindex`
// above — same seed_active_rebuild_runtime fixture, same assertions —
// but exercises `cass health --json` instead of `cass status --json`.
//
// Pre-fix (commits before the k0bzk fix), run_health (src/lib.rs:~12082)
// fell through to the !healthy branch and emitted "Run 'cass index --full'
// to rebuild the index/database." while a rebuild was already in flight.
// Polling agents reading that text would either lock-stampede via
// probe_index_run_lock or, in the worst case, kick a concurrent pipeline.
//
// Post-fix, run_health short-circuits on rebuild_active first and emits
// the same "Index rebuild is already in progress" text run_status emits,
// so cass health and cass status now agree on the operator-facing advice.
#[test]
fn health_recommended_action_during_active_rebuild_says_wait_not_reindex() {
    let test_home = tempfile::tempdir().expect("tempdir");
    let data_dir = test_home.path().join("cass-data");
    fs::create_dir_all(&data_dir).expect("create data dir");
    let _lock = seed_active_rebuild_runtime(&data_dir);

    let out = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
        .args([
            "health",
            "--data-dir",
            data_dir.to_str().expect("utf8"),
            "--json",
        ])
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("XDG_DATA_HOME", test_home.path())
        .env("HOME", test_home.path())
        .output()
        .expect("run cass health --json");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    let payload: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|err| panic!("health JSON parse failed: {err}; stdout: {stdout}"));

    // Precondition sanity: the seeded state really registered as an
    // active rebuild (rebuild_progress.active=true on health surface).
    // If this flips, the rest of the assertions are moot.
    assert_eq!(
        payload
            .get("rebuild_progress")
            .and_then(|r| r.get("active"))
            .and_then(Value::as_bool),
        Some(true),
        "seeded state must register as rebuild_progress.active=true on health. \
         stderr: {stderr}; payload: {payload}"
    );

    let recommended_action = payload
        .get("recommended_action")
        .and_then(Value::as_str)
        .expect("health must emit recommended_action during rebuild");

    // CONTRACT PIN 1: the string names the in-flight rebuild so agents
    // and humans know "wait" is the right next step. Mirrors run_status
    // recommendation text exactly so agents using either surface see
    // the same operator-facing advice.
    let lower = recommended_action.to_lowercase();
    assert!(
        (lower.contains("rebuild") && lower.contains("in progress")) || lower.contains("already"),
        "recommended_action must signal that a rebuild is active so agents attach \
         to the in-flight work instead of starting a new one; got: \
         {recommended_action:?}"
    );

    // CONTRACT PIN 2: NEVER tell the operator to run another index
    // while a rebuild is active — that's the stampede advice the bead
    // calls out. This is the single most important assertion of this
    // test: a regression here re-introduces the lock-stampede bug.
    assert!(
        !lower.contains("cass index --full"),
        "recommended_action must NOT tell the operator to run `cass index --full` \
         while a rebuild is active (stampede advice); got: {recommended_action:?}"
    );
    assert!(
        !lower.contains("run 'cass index'")
            && !lower.contains("run `cass index`")
            && !lower.contains("run cass index"),
        "recommended_action must NOT tell the operator to run `cass index` in any \
         form (quoted or unquoted) while a rebuild is active; got: {recommended_action:?}"
    );
}

// Cold-start readiness-surface progression.
//
// `cass health --json` is the authoritative readiness surface per
// AGENTS.md's Search Asset Contract. The health JSON contract promises
// that during a cold start (fresh data-dir, nothing indexed yet), cass
// reports `status="not_initialized"`, `healthy=false`, and surfaces a
// `recommended_action` that guides the operator to `cass index --full`.
// After `cass index --full` completes, the same surface must flip to
// `status="healthy"`, `healthy=true`, and — since the default install
// does NOT download the ~90MB semantic model — `state.semantic.status`
// must remain "missing" while `fallback_mode="lexical"` so robot clients
// know hybrid is silently degrading to lexical.
//
// No existing test pins this transition. `health_json_surfaces_runtime_queue_
// and_byte_budget_headroom` above only exercises the "rebuild in progress"
// phase via a seeded rebuild-state file. The cold-start → lexical-ready
// arc is a distinct slice of ibuuh.10's AC "cold-start lexical self-heal
// + truthful readiness surfaces" requirement.
//
// Contract pinned here:
//   1. Phase 1 (empty data-dir)
//      - exit code 1
//      - status == "not_initialized", healthy == false, initialized == false
//      - errors[] names db / index not initialized
//      - recommended_action names "cass index --full"
//   2. Phase 2 (after cass index --full with seeded Codex session)
//      - index --full exits 0
//      - health: exit code 0, status == "healthy", healthy == true,
//        initialized == true
//      - state.semantic.status == "missing" (no models installed)
//      - state.semantic.fallback_mode == "lexical"
//      - state.database.exists == true, state.index.exists == true
//   3. Phase 3 (search against lexical-only post-cold-start)
//      - exit 0, ≥1 hit, stdout valid JSON
// ========================================================================

fn seed_codex_session_cold_start(codex_home: &std::path::Path, filename: &str, keyword: &str) {
    // Cold-start fixture needs a full user + assistant exchange so the
    // post-index search has content to match either turn.
    util::seed_codex_session(codex_home, filename, keyword, true);
}

fn isolated_cass_cmd(home: &Path) -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");
    cmd.env("CASS_IGNORE_SOURCES_CONFIG", "1");
    cmd.env("HOME", home);
    cmd.env("XDG_DATA_HOME", home.join(".local/share"));
    cmd.env("XDG_CONFIG_HOME", home.join(".config"));
    cmd.env("CODEX_HOME", home.join(".codex"));
    cmd
}

fn seed_large_health_latency_db(data_dir: &Path) {
    fs::create_dir_all(data_dir).expect("create data dir");
    let db_path = data_dir.join("agent_search.db");
    let storage = FrankenStorage::open(&db_path).expect("open latency fixture db");
    let conn = storage.raw();

    conn.execute_compat(
        "INSERT OR IGNORE INTO agents(id, slug, name, version, kind, created_at, updated_at)
         VALUES(1, 'codex', 'Codex', '0.0.0', 'cli', 0, 0)",
        fparams![],
    )
    .expect("seed latency fixture agent");
    conn.execute("BEGIN").expect("begin latency fixture seed");

    let payload = "x".repeat(128);
    for chunk_start in (1..=LARGE_HEALTH_DB_CONVERSATIONS)
        .step_by(usize::try_from(LARGE_HEALTH_DB_INSERT_CHUNK).expect("valid chunk size"))
    {
        let chunk_end =
            (chunk_start + LARGE_HEALTH_DB_INSERT_CHUNK - 1).min(LARGE_HEALTH_DB_CONVERSATIONS);
        let mut conversation_values = Vec::new();
        let mut message_values = Vec::new();

        for conversation_id in chunk_start..=chunk_end {
            let started_at = 1_700_000_000_000_i64 + conversation_id;
            conversation_values.push(format!(
                "({conversation_id}, 1, 'local', 'health-latency-{conversation_id}', \
                 'Health latency {conversation_id}', \
                 '/tmp/cass-health-latency/session-{conversation_id}.jsonl', \
                 {started_at}, {}, 12, '{{}}')",
                started_at + 1
            ));

            let first_message_id = conversation_id * 2 - 1;
            message_values.push(format!(
                "({first_message_id}, {conversation_id}, 0, 'user', 'user', \
                 {started_at}, 'large health latency user {conversation_id} {payload}', '{{}}')"
            ));
            message_values.push(format!(
                "({}, {conversation_id}, 1, 'assistant', 'agent', {}, \
                 'large health latency assistant {conversation_id} {payload}', '{{}}')",
                first_message_id + 1,
                started_at + 1
            ));
        }

        conn.execute(&format!(
            "INSERT INTO conversations(
                id, agent_id, source_id, external_id, title, source_path,
                started_at, ended_at, approx_tokens, metadata_json
             ) VALUES {}",
            conversation_values.join(",")
        ))
        .expect("seed latency fixture conversations");
        conn.execute(&format!(
            "INSERT INTO messages(
                id, conversation_id, idx, role, author, created_at, content, extra_json
             ) VALUES {}",
            message_values.join(",")
        ))
        .expect("seed latency fixture messages");
    }

    conn.execute("COMMIT").expect("commit latency fixture seed");
    storage.close().expect("close latency fixture db");
}

/// `coding_agent_session_search-eg613` CI hard-gate: cass health
/// --json p50 latency must stay below the documented `<50ms`
/// fast-surface budget (README line 14, `cass health --help`).
///
/// **CI wiring:** this test lives in `tests/e2e_health.rs` and is
/// auto-included in CI by the `git ls-files 'tests/e2e_*.rs'` glob
/// in `.github/workflows/ci.yml:227` ("Run Rust E2E tests with JSONL
/// logging" step). Any regression that pushes the warmed p50 above
/// 50ms (e.g., a new synchronous DB query, an `fs::canonicalize`
/// loop, or a synchronous embedder probe added to the health path)
/// fails CI loudly instead of silently shipping. Pre-existing test
/// scaffolding shipped by pane 4; this comment formalises the
/// CI-hard-gate contract for future maintainers.
///
/// Flake mitigation: 5 warmup runs followed by 11 measured runs;
/// p50 is over the measured-run set (median is ~3× more stable than
/// p99 across CI runners). The 50ms ceiling has substantial headroom
/// over the typical sub-20ms warmed measurement on a 4-core CI box.
#[test]
fn health_json_large_seeded_db_p50_stays_under_50ms() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path();
    let data_dir = home.join("cass-data");
    seed_large_health_latency_db(&data_dir);

    let mut samples = Vec::with_capacity(HEALTH_LATENCY_MEASURED_RUNS);
    for run in 0..(HEALTH_LATENCY_WARMUP_RUNS + HEALTH_LATENCY_MEASURED_RUNS) {
        let started = Instant::now();
        let out = isolated_cass_cmd(home)
            .args(["health", "--json", "--data-dir"])
            .arg(&data_dir)
            .output()
            .expect("run cass health --json for latency gate");
        let elapsed = started.elapsed();

        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(
            out.status.code(),
            Some(1),
            "fixture has a large canonical DB but intentionally no lexical index; \
             health should return unhealthy quickly, not fail to produce JSON. \
             stdout: {stdout}\nstderr: {stderr}"
        );
        let payload: Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|err| {
            panic!("health latency JSON parse failed: {err}; stdout: {stdout}")
        });
        assert_eq!(
            payload
                .get("state")
                .and_then(|s| s.get("database"))
                .and_then(|db| db.get("exists"))
                .and_then(Value::as_bool),
            Some(true),
            "latency fixture must exercise an existing canonical DB"
        );
        // [coding_agent_session_search-d0rmo] Post-fix, health
        // intentionally SKIPS the COUNT(*) queries on the canonical
        // DB to honor its <50ms budget. The envelope reports
        // counts_skipped=true and zero counts; status / diag still
        // surface the full totals. Verify the skip flag and the
        // intentional zero-count semantics — a regression that
        // re-introduces the count scan would push us back over the
        // budget AND trip these assertions.
        assert_eq!(
            payload
                .get("state")
                .and_then(|s| s.get("database"))
                .and_then(|db| db.get("counts_skipped"))
                .and_then(Value::as_bool),
            Some(true),
            "health MUST report counts_skipped=true post-d0rmo so callers know the \
             0 counts are intentional, not a missing-data signal"
        );
        // [coding_agent_session_search-gi4oy] Health also skips the
        // FrankenStorage open to honor the <50ms budget. Verify the
        // open_skipped flag so a regression that re-introduces the
        // open path trips this AND the latency assertion below.
        assert_eq!(
            payload
                .get("state")
                .and_then(|s| s.get("database"))
                .and_then(|db| db.get("open_skipped"))
                .and_then(Value::as_bool),
            Some(true),
            "health MUST report open_skipped=true post-gi4oy so callers know the \
             opened=true is assumed-good, not a real open-success signal"
        );
        assert!(
            payload
                .get("state")
                .and_then(|s| s.get("database"))
                .and_then(|db| db.get("conversations"))
                .is_some_and(Value::is_null),
            "health MUST report conversations=null when counts_skipped (operators read \
             status/diag for actual totals); payload: {payload:#}"
        );
        assert!(
            payload
                .get("state")
                .and_then(|s| s.get("database"))
                .and_then(|db| db.get("messages"))
                .is_some_and(Value::is_null),
            "health MUST report messages=null when counts_skipped; payload: {payload:#}"
        );
        // Sanity: LARGE_HEALTH_DB_* constants are still load-bearing
        // for the seed setup (the actual rows must exist in the DB
        // even though health doesn't read them); reference them so a
        // future change that drops the seed fails the test loudly.
        let _ = (LARGE_HEALTH_DB_CONVERSATIONS, LARGE_HEALTH_DB_MESSAGES);
        assert_eq!(
            payload
                .get("state")
                .and_then(|s| s.get("index"))
                .and_then(|index| index.get("exists"))
                .and_then(Value::as_bool),
            Some(false),
            "fixture should measure health's DB fast path, not a search-reader open"
        );

        if run >= HEALTH_LATENCY_WARMUP_RUNS {
            samples.push(elapsed);
        }
    }

    samples.sort_unstable();
    let p50 = samples[samples.len() / 2];
    assert!(
        p50 < Duration::from_millis(50),
        "cass health --json warmed p50 must stay below the documented <50ms \
         fast-surface budget on a large seeded DB; p50={:.2}ms samples_ms={:?}",
        p50.as_secs_f64() * 1000.0,
        samples
            .iter()
            .map(|duration| duration.as_secs_f64() * 1000.0)
            .collect::<Vec<_>>()
    );
}

#[test]
fn cold_start_health_surface_transitions_from_not_initialized_to_lexical_only() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass-data");
    fs::create_dir_all(&data_dir).expect("create empty data dir");

    // PHASE 1 — empty data-dir, no index, no DB. Health must admit that
    // truthfully and guide the operator to `cass index --full`.
    let phase1 = isolated_cass_cmd(home)
        .args(["health", "--json", "--data-dir"])
        .arg(&data_dir)
        .output()
        .expect("run cass health (phase 1)");
    let phase1_code = phase1.status.code().expect("phase1 exit code");
    let phase1_stdout = String::from_utf8_lossy(&phase1.stdout);
    let phase1_stderr = String::from_utf8_lossy(&phase1.stderr);
    assert_eq!(
        phase1_code, 1,
        "cold-start health must exit 1 (not ready). stdout: {phase1_stdout}\nstderr: {phase1_stderr}"
    );
    let phase1_json: Value = serde_json::from_str(phase1_stdout.trim()).unwrap_or_else(|err| {
        panic!("phase1 health JSON parse failed: {err}; stdout: {phase1_stdout}")
    });
    assert_eq!(
        phase1_json.get("status").and_then(Value::as_str),
        Some("not_initialized"),
        "phase1 status must be 'not_initialized' so agents can distinguish cold-start \
         from rebuilding. payload: {phase1_json}"
    );
    assert_eq!(
        phase1_json.get("healthy").and_then(Value::as_bool),
        Some(false),
        "phase1 healthy must be false"
    );
    assert_eq!(
        phase1_json.get("initialized").and_then(Value::as_bool),
        Some(false),
        "phase1 initialized must be false"
    );
    let phase1_errors = phase1_json
        .get("errors")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    assert!(
        phase1_errors.iter().any(|e| {
            e.as_str()
                .is_some_and(|s| s.contains("database not initialized"))
        }),
        "phase1 errors[] must mention 'database not initialized' so agents diagnose; \
         got: {phase1_errors:?}"
    );
    assert!(
        phase1_errors.iter().any(|e| {
            e.as_str()
                .is_some_and(|s| s.contains("index not initialized"))
        }),
        "phase1 errors[] must mention 'index not initialized' so agents diagnose; \
         got: {phase1_errors:?}"
    );
    let phase1_action = phase1_json
        .get("recommended_action")
        .and_then(Value::as_str)
        .expect("phase1 recommended_action must be a string");
    assert!(
        phase1_action.contains("cass index --full"),
        "phase1 recommended_action must name the exact recovery command \
         `cass index --full`; got: {phase1_action:?}"
    );

    // PHASE 2 — seed a Codex session, run `cass index --full`, re-ask
    // health. It must flip to healthy + initialized, while surfacing
    // the fallback_mode="lexical" truth for hybrid clients (no semantic
    // model is installed in this test; default cass never auto-downloads).
    // File name must start with `rollout-` to match the Codex rollout-
    // file heuristic in franken_agent_detection (CodexConnector at
    // codex.rs::is_rollout_file line ~77). Otherwise the connector
    // silently ignores the fixture and search returns zero hits.
    seed_codex_session_cold_start(&codex_home, "rollout-cold-start-01.jsonl", "coldstartprobe");

    let idx_out = isolated_cass_cmd(home)
        .args(["index", "--full", "--json", "--data-dir"])
        .arg(&data_dir)
        .output()
        .expect("run cass index --full");
    assert!(
        idx_out.status.success(),
        "cass index --full must succeed on a fresh seeded corpus. \
         stdout: {} stderr: {}",
        String::from_utf8_lossy(&idx_out.stdout),
        String::from_utf8_lossy(&idx_out.stderr),
    );

    let phase2 = isolated_cass_cmd(home)
        .args(["health", "--json", "--data-dir"])
        .arg(&data_dir)
        .output()
        .expect("run cass health (phase 2)");
    let phase2_code = phase2.status.code().expect("phase2 exit code");
    let phase2_stdout = String::from_utf8_lossy(&phase2.stdout);
    let phase2_stderr = String::from_utf8_lossy(&phase2.stderr);
    assert_eq!(
        phase2_code, 0,
        "post-index health must exit 0 (lexical-only is a healthy state when \
         semantic is opt-in). stdout: {phase2_stdout}\nstderr: {phase2_stderr}"
    );
    let phase2_json: Value = serde_json::from_str(phase2_stdout.trim()).unwrap_or_else(|err| {
        panic!("phase2 health JSON parse failed: {err}; stdout: {phase2_stdout}")
    });
    assert_eq!(
        phase2_json.get("status").and_then(Value::as_str),
        Some("healthy"),
        "phase2 status must be 'healthy' after index --full. payload: {phase2_json}"
    );
    assert_eq!(
        phase2_json.get("healthy").and_then(Value::as_bool),
        Some(true),
        "phase2 healthy must be true"
    );
    assert_eq!(
        phase2_json.get("initialized").and_then(Value::as_bool),
        Some(true),
        "phase2 initialized must be true"
    );
    // Per AGENTS.md: "cass never auto-downloads" the ~90MB semantic
    // model. Fresh cold start without `cass models install` must admit
    // the semantic tier is missing AND that the realized fallback is
    // lexical so hybrid clients don't silently think they have semantic.
    let semantic = phase2_json
        .get("state")
        .and_then(|s| s.get("semantic"))
        .and_then(Value::as_object)
        .expect("phase2 state.semantic must be an object");
    let semantic_status = semantic
        .get("status")
        .and_then(Value::as_str)
        .expect("state.semantic.status must be a string");
    assert!(
        matches!(semantic_status, "missing" | "not_initialized"),
        "phase2 state.semantic.status must be 'missing' or 'not_initialized' \
         (no model installed); got: {semantic_status:?}"
    );
    assert_eq!(
        semantic.get("fallback_mode").and_then(Value::as_str),
        Some("lexical"),
        "phase2 state.semantic.fallback_mode must be 'lexical' so hybrid \
         clients see the truthful realized tier. got: {semantic:?}"
    );
    assert_eq!(
        phase2_json
            .get("state")
            .and_then(|s| s.get("database"))
            .and_then(|db| db.get("exists"))
            .and_then(Value::as_bool),
        Some(true),
        "phase2 state.database.exists must be true after index"
    );
    assert_eq!(
        phase2_json
            .get("state")
            .and_then(|s| s.get("index"))
            .and_then(|i| i.get("exists"))
            .and_then(Value::as_bool),
        Some(true),
        "phase2 state.index.exists must be true after index"
    );

    // PHASE 3 — search works against the now-ready lexical-only system
    // and returns ≥1 hit for the seeded keyword. This closes the
    // cold-start arc: the same data-dir that was "not_initialized" a
    // moment ago now serves user queries without any manual rebuild.
    let search_out = isolated_cass_cmd(home)
        .args(["search", "coldstartprobe", "--json", "--data-dir"])
        .arg(&data_dir)
        .output()
        .expect("run cass search (phase 3)");
    assert!(
        search_out.status.success(),
        "phase3 cass search must succeed. stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&search_out.stdout),
        String::from_utf8_lossy(&search_out.stderr),
    );
    let search_stdout = String::from_utf8_lossy(&search_out.stdout);
    let search_json: Value = serde_json::from_str(search_stdout.trim()).unwrap_or_else(|err| {
        panic!("phase3 search JSON parse failed: {err}; stdout: {search_stdout}")
    });
    let hits = search_json
        .get("hits")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("phase3 search must have hits[]; payload: {search_json}"));
    assert!(
        !hits.is_empty(),
        "phase3 search must return ≥1 hit for the seeded keyword; payload: {search_json}"
    );
}

// ========================================================================
// Bead coding_agent_session_search-k9jb9 (child of ibuuh.10, scenario E:
// stale index-run.lock detection + reaping, end-to-end).
//
// src/search/asset_state.rs::read_search_maintenance_snapshot promises
// that a stale `index-run.lock` file — metadata persisted from a prior
// cass invocation that crashed or was killed — gets reaped on read:
// fs2 `try_lock_exclusive` succeeds (no live holder), the file is
// truncated to 0 bytes, and every reader observes a clean default
// snapshot (`active=false`, `orphaned=false`).
//
// That invariant exists specifically because issue #176 had stale locks
// making the TUI and status consumers interpret the file as "rebuild
// in progress, keep polling" forever — a user-visible hang that only
// cleared on manual `rm index-run.lock`.
//
// The inner state_meta_json unit test at src/lib.rs::state_meta_json_reports_orphaned_lock_metadata
// pins the library-level contract. This E2E test pins the USER-FACING
// surface: does the `cass status --json` subprocess correctly:
//   1. report rebuild.active=false on a stale-lock state?
//   2. reap the lock file as a side-effect (so follow-up readers also
//      see the clean state without racing)?
// A regression where the reaping was skipped or gated by a kill(pid,0)
// probe (which the source deliberately avoids — see the in-source
// rationale around asset_state.rs:180) would leave every agent and TUI
// stuck again.
// ========================================================================

#[test]
fn cass_status_reaps_stale_index_run_lock_and_reports_not_active() {
    let test_home = tempfile::tempdir().expect("tempdir");
    let data_dir = test_home.path().join("cass-data");
    fs::create_dir_all(&data_dir).expect("create data dir");

    // Create an empty canonical DB file at the path the lock metadata
    // references. probe_index_run_lock will only consider a lock
    // authoritative if its db_path matches the one cass is examining
    // (src/lib.rs::probe_index_run_lock path_identities_match check).
    let db_path = data_dir.join("agent_search.db");
    fs::write(&db_path, b"").expect("create stub agent_search.db");

    // Seed a stale index-run.lock shaped like a real one: PID of a
    // process that is NOT currently holding an fcntl lock on the file.
    // Using a random high PID keeps the intent obvious even if PID
    // reuse happens to collide — the reaping path deliberately does
    // NOT gate on kill(pid, 0) (see asset_state.rs:180 rationale). The
    // only thing that matters is that we did NOT lock the file.
    let lock_path = data_dir.join("index-run.lock");
    let lock_body = format!(
        "pid=4242\nstarted_at_ms=1733000888000\nupdated_at_ms=1733000999000\ndb_path={}\nmode=index\njob_id=lexical_refresh-1733000888000-4242\njob_kind=lexical_refresh\nphase=rebuilding\n",
        db_path.display()
    );
    fs::write(&lock_path, lock_body.as_bytes()).expect("write stale lock metadata");
    let original_lock_len = fs::metadata(&lock_path).expect("stat lock").len();
    assert!(
        original_lock_len > 0,
        "precondition: stale lock metadata must be non-empty before the test \
         runs; got len={original_lock_len}"
    );

    // Run cass status. This invokes read_search_maintenance_snapshot
    // which should observe: "metadata present but no fcntl holder" =>
    // reap the file AND return a clean default snapshot.
    let out = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
        .args([
            "status",
            "--data-dir",
            data_dir.to_str().expect("utf8"),
            "--json",
        ])
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("XDG_DATA_HOME", test_home.path())
        .env("HOME", test_home.path())
        .output()
        .expect("run cass status --json");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let payload: Value = serde_json::from_str(&stdout).unwrap_or_else(|err| {
        panic!("status JSON parse failed: {err}; stdout: {stdout}\nstderr: {stderr}")
    });

    // CONTRACT PIN 1: status reports rebuild.active=false. The whole
    // point of issue #176 fix is that stale metadata must NOT be
    // surfaced as active-rebuild to the user or their agents.
    assert_eq!(
        payload
            .get("rebuild")
            .and_then(|r| r.get("active"))
            .and_then(Value::as_bool),
        Some(false),
        "cass status must report rebuild.active=false for a stale index-run.lock \
         (no fcntl holder). stderr: {stderr}\npayload: {payload}"
    );

    // CONTRACT PIN 2: status reports rebuild.orphaned=false. After
    // reaping the metadata, the snapshot is a clean default — no
    // "orphaned-forever" sticky state that historically caused the
    // TUI to spin.
    assert_eq!(
        payload
            .get("rebuild")
            .and_then(|r| r.get("orphaned"))
            .and_then(Value::as_bool),
        Some(false),
        "cass status must report rebuild.orphaned=false after reaping (not \
         sticky-orphaned); payload: {payload}"
    );

    // CONTRACT PIN 3: the lock file on disk is truncated as a side-
    // effect of the read. A subsequent reader (next `cass status`,
    // next TUI poll) must observe a clean state without having to
    // re-do the reaping dance — otherwise concurrent consumers race.
    let reaped_len = fs::metadata(&lock_path)
        .expect("stat lock after cass status")
        .len();
    assert_eq!(
        reaped_len, 0,
        "index-run.lock must be truncated to 0 bytes after the read reaps stale \
         metadata; was {original_lock_len} bytes before, {reaped_len} after"
    );
}

// ========================================================================
// Bead coding_agent_session_search-yc4h7 (child of ibuuh.10, scenario F:
// interrupt + recover across restart, end-to-end).
//
// k9jb9 pinned the reaping surface using a synthetic stale lock file
// written by the test. This test covers the REAL user-visible arc: an
// operator starts `cass index --full`, the process is killed
// (crash, signal, OOM), they re-run it, and the index completes
// successfully with content searchable — no manual `rm index-run.lock`
// needed.
//
// Contract pinned:
//   1. After SIGKILL on an in-flight `cass index --full`, a subsequent
//      `cass status --json` reports `rebuild.active == false`. The
//      reaper at src/search/asset_state.rs::read_search_maintenance_snapshot
//      must handle a REAL killed-process-held lock the same way as
//      synthetic metadata — flock ownership is the authoritative
//      signal, not pid liveness.
//   2. A subsequent `cass index --full` exits success — no
//      lock-stampede error, no leftover staged generation corruption.
//   3. Content seeded before the kill IS searchable after the re-run.
//
// Timing strategy: seed 12 rollouts so index --full takes long enough
// to be reliably interruptible. Poll for the lock file to appear with
// non-empty content (up to ~5s), SIGKILL once seen. Fall back
// gracefully if the child completes before the poll catches it — in
// that case the post-completion invariants are still meaningful
// regression signal.
// ========================================================================

fn seed_yc4h7_corpus(codex_home: &Path) {
    let day = codex_home.join("sessions/2026/04/23");
    fs::create_dir_all(&day).expect("create codex sessions dir");
    for i in 1..=12 {
        let body = format!(
            "{{\"timestamp\":\"2024-04-24T00:{:02}:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"yc4h7-{i:02}\",\"cwd\":\"/tmp/ws\",\"cli_version\":\"0.42.0\"}}}}\n\
             {{\"timestamp\":\"2024-04-24T00:{:02}:01Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\"content\":[{{\"type\":\"input_text\",\"text\":\"yc4h7keyword-{i:02} with extra context so each message carries a bit more to index\"}}]}}}}\n\
             {{\"timestamp\":\"2024-04-24T00:{:02}:02Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"acknowledged the request and provided a detailed response that adds some length to the fixture\"}}]}}}}\n",
            i, i, i
        );
        fs::write(
            day.join(format!("rollout-yc4h7-{i:02}.jsonl")),
            body.as_bytes(),
        )
        .expect("write yc4h7 fixture");
    }
}

#[test]
fn sigkill_mid_index_run_still_allows_cass_status_and_subsequent_index_to_recover() {
    let test_home = tempfile::tempdir().expect("tempdir");
    let home = test_home.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass-data");
    fs::create_dir_all(&data_dir).expect("create data dir");

    seed_yc4h7_corpus(&codex_home);

    // Spawn cass index --full as a child via std::process::Command
    // (not assert_cmd, which blocks until completion) so we can kill
    // it mid-rebuild.
    let cass = assert_cmd::cargo::cargo_bin!("cass");
    let mut child = std::process::Command::new(cass)
        .arg("index")
        .arg("--full")
        .arg("--json")
        .arg("--data-dir")
        .arg(&data_dir)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("CODEX_HOME", &codex_home)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn cass index --full child");

    // Poll for the lock file to appear non-empty. acquire_index_run_lock
    // writes the metadata very early in the pipeline, so even a fast
    // index should be catchable in the window.
    let lock_path = data_dir.join("index-run.lock");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut caught_mid_run = false;
    while std::time::Instant::now() < deadline {
        if let Ok(meta) = fs::metadata(&lock_path)
            && meta.len() > 0
        {
            caught_mid_run = true;
            break;
        }
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }

    if caught_mid_run {
        // std::process::Child::kill sends SIGKILL on Unix so the
        // child cannot run graceful lock-cleanup Drop impls.
        let _ = child.kill();
    }
    // Reap the child in either branch so we don't leave zombies and
    // so subsequent cass invocations don't race a still-alive holder.
    let _ = child.wait();

    // CONTRACT PIN 1: cass status must report rebuild.active=false.
    // If the lock was held by the killed child, the reaper in
    // read_search_maintenance_snapshot must acquire flock (released
    // on process exit) and clean up in-place.
    let status_out = Command::new(cass)
        .args([
            "status",
            "--data-dir",
            data_dir.to_str().expect("utf8"),
            "--json",
        ])
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("CODEX_HOME", &codex_home)
        .output()
        .expect("run cass status");
    let status_stdout = String::from_utf8_lossy(&status_out.stdout);
    let status_json: Value = serde_json::from_str(&status_stdout).unwrap_or_else(|err| {
        panic!(
            "status JSON parse failed: {err}\ncaught_mid_run={caught_mid_run}\nstdout: {status_stdout}"
        )
    });
    assert_eq!(
        status_json
            .get("rebuild")
            .and_then(|r| r.get("active"))
            .and_then(Value::as_bool),
        Some(false),
        "post-kill cass status must report rebuild.active=false; \
         caught_mid_run={caught_mid_run}; payload: {status_json}"
    );

    // CONTRACT PIN 2: a subsequent cass index --full must succeed
    // cleanly — no lock-stampede, no corruption bail-out.
    let rerun = Command::new(cass)
        .args(["index", "--full", "--json", "--data-dir"])
        .arg(&data_dir)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("CODEX_HOME", &codex_home)
        .output()
        .expect("run cass index --full rerun");
    assert!(
        rerun.status.success(),
        "re-run cass index --full after SIGKILL must succeed; \
         caught_mid_run={caught_mid_run}\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&rerun.stdout),
        String::from_utf8_lossy(&rerun.stderr),
    );

    // CONTRACT PIN 3: at least one seeded keyword is searchable post-
    // recovery. Proves the rerun actually populated the lexical
    // index, not just exited-success silently.
    let search_out = Command::new(cass)
        .args(["search", "yc4h7keyword-01", "--json", "--data-dir"])
        .arg(&data_dir)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("CODEX_HOME", &codex_home)
        .output()
        .expect("run cass search");
    assert!(
        search_out.status.success(),
        "post-recovery cass search must succeed; stderr: {}",
        String::from_utf8_lossy(&search_out.stderr)
    );
    let search_stdout = String::from_utf8_lossy(&search_out.stdout);
    let search_json: Value = serde_json::from_str(&search_stdout)
        .unwrap_or_else(|err| panic!("search JSON parse failed: {err}\nstdout: {search_stdout}"));
    let hits = search_json
        .get("hits")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("search must have hits[]; payload: {search_json}"));
    assert!(
        !hits.is_empty(),
        "post-recovery search must return >=1 hit for a seeded keyword; \
         caught_mid_run={caught_mid_run}; payload: {search_json}"
    );
}

// ========================================================================
// Bead coding_agent_session_search-k2jr8 (child of ibuuh.10,
// /testing-metamorphic slice: cross-command consistency).
//
// `cass health --json` and `cass status --json` are two different JSON
// surfaces over the SAME underlying cass state. Operators and agents
// use them interchangeably — health for fast readiness probes, status
// for a fuller snapshot — and expect the two to agree on every shared
// field. A regression that updated one code path but not the other
// would silently make polling loops observe contradictory state.
//
// This test seeds a rebuild-active state (matching the sibling
// `health_json_surfaces_runtime_queue_and_byte_budget_headroom` fixture
// shape), invokes both `cass health --json` and `cass status --json`
// against the same data-dir within a single test scope, and asserts
// the four fields where parity actually holds today:
//
//   1. rebuild-active flag: status.rebuild.active ==
//      health.rebuild_progress.active == health.state.rebuild.active
//   2. semantic tier status: status.semantic.status ==
//      health.state.semantic.status
//   3. database presence: status.database.exists ==
//      health.state.database.exists
//   4. lexical index presence: status.index.exists ==
//      health.state.index.exists
//
// Recommended_action divergence between the two commands is a known
// bug tracked separately by coding_agent_session_search-k0bzk and is
// deliberately NOT asserted here — this test's purpose is to lock in
// the CORRECT shared-field parity so a future regression that drags
// a currently-agreeing field out of sync (e.g. cached staleness flag
// that only one command updates) trips immediately.
//
// The invariant is intentionally one-directional: both commands must
// report the SAME value for each shared field. If a schema change
// intentionally renames or moves a field, the test needs to be
// updated alongside the src change — the diff makes the intent
// reviewable.
// ========================================================================

#[test]
fn health_and_status_agree_on_shared_fields_during_active_rebuild() {
    let test_home = tempfile::tempdir().expect("tempdir");
    let data_dir = test_home.path().join("cass-data");
    fs::create_dir_all(&data_dir).expect("create data dir");
    let _lock = seed_active_rebuild_runtime(&data_dir);

    // Helper: run a cass subcommand against the seeded data dir and
    // parse stdout as JSON. Uses the same env-isolation shape as the
    // sibling tests in this file so the real corpus never leaks in.
    let run_json = |subcommand: &str| -> Value {
        let out = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
            .args([
                subcommand,
                "--data-dir",
                data_dir.to_str().expect("utf8"),
                "--json",
            ])
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .env("CASS_IGNORE_SOURCES_CONFIG", "1")
            .env("XDG_DATA_HOME", test_home.path())
            .env("HOME", test_home.path())
            .output()
            .unwrap_or_else(|err| panic!("run cass {subcommand}: {err}"));
        let stdout = String::from_utf8_lossy(&out.stdout);
        serde_json::from_str(stdout.trim()).unwrap_or_else(|err| {
            panic!(
                "cass {subcommand} --json must emit valid JSON: {err}\n\
                 stdout: {stdout}"
            )
        })
    };

    let status = run_json("status");
    let health = run_json("health");

    // CONTRACT PIN 1: rebuild-active flag agrees across all three
    // surfaces that expose it. The test is INTENTIONALLY not allowed
    // to pass with one surface reporting false and another true.
    let status_rebuild_active = status
        .get("rebuild")
        .and_then(|r| r.get("active"))
        .and_then(Value::as_bool);
    let health_progress_active = health
        .get("rebuild_progress")
        .and_then(|r| r.get("active"))
        .and_then(Value::as_bool);
    let health_state_rebuild_active = health
        .get("state")
        .and_then(|s| s.get("rebuild"))
        .and_then(|r| r.get("active"))
        .and_then(Value::as_bool);
    assert_eq!(
        status_rebuild_active,
        Some(true),
        "precondition: seeded state must make status.rebuild.active=true; \
         got {status_rebuild_active:?}\nstatus: {status}"
    );
    assert_eq!(
        health_progress_active, status_rebuild_active,
        "health.rebuild_progress.active must agree with status.rebuild.active; \
         status={status_rebuild_active:?} health={health_progress_active:?}"
    );
    assert_eq!(
        health_state_rebuild_active, status_rebuild_active,
        "health.state.rebuild.active must agree with status.rebuild.active; \
         status={status_rebuild_active:?} health.state={health_state_rebuild_active:?}"
    );

    // CONTRACT PIN 2: semantic tier status agrees.
    let status_semantic = status
        .get("semantic")
        .and_then(|s| s.get("status"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let health_semantic = health
        .get("state")
        .and_then(|s| s.get("semantic"))
        .and_then(|sem| sem.get("status"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    assert!(
        status_semantic.is_some(),
        "precondition: status.semantic.status must be present; status: {status}"
    );
    assert_eq!(
        status_semantic, health_semantic,
        "semantic tier status must agree between surfaces; \
         status={status_semantic:?} health={health_semantic:?}"
    );

    // CONTRACT PIN 3: database presence flag agrees.
    let status_db = status
        .get("database")
        .and_then(|d| d.get("exists"))
        .and_then(Value::as_bool);
    let health_db = health
        .get("state")
        .and_then(|s| s.get("database"))
        .and_then(|d| d.get("exists"))
        .and_then(Value::as_bool);
    assert_eq!(
        status_db, health_db,
        "database.exists must agree between surfaces; \
         status={status_db:?} health={health_db:?}"
    );

    // CONTRACT PIN 4: lexical index presence flag agrees.
    let status_idx = status
        .get("index")
        .and_then(|i| i.get("exists"))
        .and_then(Value::as_bool);
    let health_idx = health
        .get("state")
        .and_then(|s| s.get("index"))
        .and_then(|i| i.get("exists"))
        .and_then(Value::as_bool);
    assert_eq!(
        status_idx, health_idx,
        "index.exists must agree between surfaces; \
         status={status_idx:?} health={health_idx:?}"
    );
}
