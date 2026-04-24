use assert_cmd::Command;
use coding_agent_search::search::tantivy::{SCHEMA_HASH, expected_index_dir};
use fs2::FileExt;
use serde_json::{Value, json};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

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
        (lower.contains("rebuild") && lower.contains("in progress"))
            || lower.contains("already"),
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

fn seed_codex_session_cold_start(
    codex_home: &std::path::Path,
    filename: &str,
    keyword: &str,
) {
    let sessions = codex_home.join("sessions/2026/04/23");
    fs::create_dir_all(&sessions).expect("create codex sessions dir");
    let ts_ms = 1_714_000_000_000_u64;
    let iso = |offset_ms: u64| -> String {
        chrono::DateTime::from_timestamp_millis(
            i64::try_from(ts_ms + offset_ms).unwrap_or(i64::MAX),
        )
        .unwrap()
        .to_rfc3339()
    };
    let workspace = codex_home.to_string_lossy().into_owned();
    let lines = [
        json!({
            "timestamp": iso(0),
            "type": "session_meta",
            "payload": { "id": filename, "cwd": workspace, "cli_version": "0.42.0" },
        }),
        json!({
            "timestamp": iso(1_000),
            "type": "response_item",
            "payload": {
                "type": "message", "role": "user",
                "content": [{ "type": "input_text", "text": keyword }],
            },
        }),
        json!({
            "timestamp": iso(2_000),
            "type": "response_item",
            "payload": {
                "type": "message", "role": "assistant",
                "content": [{ "type": "text", "text": format!("{keyword} response") }],
            },
        }),
    ];
    let mut body = String::new();
    for line in lines {
        body.push_str(&serde_json::to_string(&line).unwrap());
        body.push('\n');
    }
    fs::write(sessions.join(filename), body).expect("write codex session fixture");
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
