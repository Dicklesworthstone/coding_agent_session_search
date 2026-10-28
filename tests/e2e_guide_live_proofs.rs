//! Live guide regression gates: invoke the actual cass binary, never a fixture
//! proof override. Every source, archive, and output artifact is temporary.
//! Run alongside e2e_guide_apply_gate, which owns fixture/confirmation coverage.

mod util;

use assert_cmd::Command;
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

const PRIVATE_BODY: &str = "guide-live-private-session-body-sentinel";

fn cass(home: &Path) -> Command {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    // Do not inherit connector roots, remote sources, or agent-home overrides
    // from the developer or RCH worker running this test. Keep only OS/loader
    // infrastructure needed to launch the already-built executable.
    command.env_clear();
    for key in [
        "PATH",
        "SystemRoot",
        "WINDIR",
        "TMP",
        "TEMP",
        "TMPDIR",
        "LD_LIBRARY_PATH",
        "DYLD_LIBRARY_PATH",
    ] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command
        .current_dir(home)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("APPDATA", home.join("appdata"))
        .env("LOCALAPPDATA", home.join("localappdata"))
        .env("XDG_DATA_HOME", home.join("xdg-data"))
        .env("XDG_CONFIG_HOME", home.join("xdg-config"))
        .env("XDG_CACHE_HOME", home.join("xdg-cache"))
        .env("CODEX_HOME", home.join(".codex"))
        .env("CLAUDE_HOME", home.join(".claude"))
        .env("GEMINI_HOME", home.join(".gemini"))
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("CASS_AUTO_REFRESH", "0")
        .env("CASS_SEMANTIC_EMBEDDER", "hash")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("NO_COLOR", "1")
        .env("RUST_MIN_STACK", "134217728")
        .timeout(Duration::from_secs(180));
    command
}

fn run(command: &mut Command, data_dir: &Path) -> (bool, Value) {
    let output = command
        .arg("--data-dir")
        .arg(data_dir)
        .output()
        .expect("run isolated cass");
    let payload = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "cass did not emit JSON: {error}; exit={:?}; stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
                .chars()
                .take(1000)
                .collect::<String>()
        )
    });
    (output.status.success(), payload)
}

fn seed_archive(home: &Path, data_dir: &Path) {
    std::fs::create_dir_all(data_dir).expect("data directory");
    util::seed_codex_session(
        &home.join(".codex"),
        "rollout-guide-live.jsonl",
        PRIVATE_BODY,
        true,
    );
    let (success, output) = run(cass(home).args(["index", "--full", "--json"]), data_dir);
    assert!(success, "seed index failed: {output}");
    assert_eq!(output["success"].as_bool(), Some(true));
}

fn step<'a>(payload: &'a Value, id: &str) -> &'a Value {
    payload
        .pointer("/execution/transcript")
        .and_then(Value::as_array)
        .and_then(|steps| {
            steps
                .iter()
                .find(|step| step["structured_command"].as_str() == Some(id))
        })
        .unwrap_or_else(|| panic!("missing {id} transcript: {payload}"))
}

fn db_digest(data_dir: &Path) -> blake3::Hash {
    blake3::hash(&std::fs::read(data_dir.join("agent_search.db")).expect("archive bytes"))
}

fn configure_remote(home: &Path, remote_path: &str) -> std::path::PathBuf {
    use coding_agent_search::sources::config::{SourceDefinition, SourcesConfig};
    let config_dir = home.join("xdg-config/cass");
    std::fs::create_dir_all(&config_dir).expect("config directory");
    let mut source = SourceDefinition::ssh("guide-remote", "fixture.invalid");
    source.paths = vec![remote_path.into()];
    let config = SourcesConfig {
        sources: vec![source],
        ..Default::default()
    };
    let path = config_dir.join("sources.toml");
    std::fs::write(&path, toml::to_string(&config).expect("source TOML"))
        .expect("source configuration");
    path
}

fn onboard(home: &Path, data_dir: &Path, db: &Path, confirm: bool) -> Value {
    let (_, plan) = run(
        cass(home)
            .env_remove("CASS_IGNORE_SOURCES_CONFIG")
            .args(["guide", "onboard-source", "--json"])
            .arg("--db")
            .arg(db),
        data_dir,
    );
    let privacy = plan["plan"]["privacy_tier"].as_str().expect("privacy");
    let cost = plan["plan"]["cost_risk"]["risk_level"]
        .as_str()
        .expect("cost");
    let mut command = cass(home);
    command
        .env_remove("CASS_IGNORE_SOURCES_CONFIG")
        // Any SSH/rsync invocation would fail: onboarding consumes staged files.
        .env("PATH", home.join("empty-path"))
        .args([
            "guide",
            "onboard-source",
            "--apply",
            "--confirm-fact",
            "sources_config_writable",
            "--accept-privacy-tier",
            privacy,
            "--accept-cost-risk",
            cost,
            "--confirm-stop-conditions-clear",
            "--json",
        ])
        .arg("--db")
        .arg(db);
    if confirm {
        command.args(["--confirm-step", "3"]);
    }
    run(&mut command, data_dir).1
}

#[test]
fn live_onboarding_indexes_a_staged_remote_only_after_explicit_confirmation() {
    let home = tempfile::tempdir().expect("home");
    let data_dir = home.path().join("data with spaces");
    let db = home.path().join("selected archive.db");
    let config = configure_remote(home.path(), "~/.codex");
    let mirror = data_dir.join("remotes/guide-remote/mirror").join(
        coding_agent_search::sources::sync::path_to_safe_dirname("~/.codex"),
    );
    util::seed_codex_session(&mirror, "rollout-guide-remote.jsonl", PRIVATE_BODY, true);
    let config_before = std::fs::read(&config).expect("config bytes");
    let unconfirmed = onboard(home.path(), &data_dir, &db, false);
    let preview = step(&unconfirmed, "sources.dry-sync");
    assert_eq!(preview["proof_gate"]["result"], "passed", "{unconfirmed}");
    assert_eq!(preview["observation"]["transport_started"], false);
    assert_eq!(preview["observation"]["sources_registered"], false);
    assert_eq!(
        preview["environment"]["CASS_DATA_DIR"],
        data_dir.to_string_lossy().as_ref()
    );
    assert_eq!(
        step(&unconfirmed, "index.incremental")["result"],
        "awaiting-confirmation"
    );
    assert_eq!(unconfirmed["execution"]["attempted_mutation_count"], 0);
    assert!(!db.exists());

    let output = onboard(home.path(), &data_dir, &db, true);
    let indexed = step(&output, "index.incremental");
    assert_eq!(indexed["result"], "executed", "{output}");
    assert_eq!(indexed["proof_gate"]["result"], "passed");
    assert_eq!(indexed["observation"]["conversations"], 1);
    assert_eq!(indexed["observation"]["lexical_ready"], true);
    assert_eq!(indexed["observation"]["source_scan_fresh"], true);
    assert_eq!(indexed["execution_scope"]["remote_sync"], false);
    assert_eq!(output["execution"]["applied_mutation_count"], 1);
    assert_eq!(output["mutation_contract"]["mutates_db"], true);
    assert_eq!(output["mutation_contract"]["touches_network"], false);
    assert_eq!(std::fs::read(&config).expect("config after"), config_before);
    assert!(!data_dir.join("agent_search.db").exists());
    assert!(!output.to_string().contains(PRIVATE_BODY));
    let (success, hits) = run_on_archive(
        home.path(),
        &data_dir,
        &db,
        &[
            "search",
            PRIVATE_BODY,
            "--mode",
            "lexical",
            "--source",
            "guide-remote",
            "--json",
        ],
    );
    assert!(success, "{hits}");
    assert!(
        hits["hits"].as_array().is_some_and(|hits| !hits.is_empty()),
        "{hits}"
    );
}

#[test]
fn live_onboarding_refuses_missing_or_invalid_config_and_unstaged_mirrors() {
    for scenario in ["missing-config", "invalid-path", "missing-mirror"] {
        let home = tempfile::tempdir().expect("home");
        let data_dir = home.path().join("data");
        std::fs::create_dir_all(&data_dir).expect("data directory");
        if scenario != "missing-config" {
            configure_remote(
                home.path(),
                if scenario == "invalid-path" {
                    ""
                } else {
                    "~/.codex"
                },
            );
        }
        let db = data_dir.join("agent_search.db");
        let output = onboard(home.path(), &data_dir, &db, true);
        let preview = step(&output, "sources.dry-sync");
        assert_eq!(
            preview["proof_gate"]["result"],
            if scenario == "missing-mirror" {
                "passed"
            } else {
                "failed"
            },
            "{scenario}: {output}"
        );
        assert_ne!(
            step(&output, "index.incremental")["result"],
            "executed",
            "{output}"
        );
        assert_eq!(
            output["execution"]["attempted_mutation_count"], 0,
            "{output}"
        );
        assert!(!db.exists());
    }
}

#[test]
fn live_onboarding_preserves_the_archive_when_another_indexer_holds_the_lock() {
    let home = tempfile::tempdir().expect("home");
    let data_dir = home.path().join("data");
    seed_archive(home.path(), &data_dir);
    configure_remote(home.path(), "~/.codex");
    let mirror = data_dir.join("remotes/guide-remote/mirror").join(
        coding_agent_search::sources::sync::path_to_safe_dirname("~/.codex"),
    );
    util::seed_codex_session(
        &mirror,
        "rollout-guide-locked.jsonl",
        "guidependingremote",
        true,
    );
    let before = db_digest(&data_dir);
    let holder = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(data_dir.join("index-run.lock"))
        .expect("existing index lock");
    fs2::FileExt::try_lock_exclusive(&holder).expect("hold actual index lock");
    let output = onboard(
        home.path(),
        &data_dir,
        &data_dir.join("agent_search.db"),
        true,
    );
    let indexed = step(&output, "index.incremental");
    assert_eq!(indexed["result"], "failed", "{output}");
    assert_eq!(indexed["observation"]["index_busy"], true);
    assert_eq!(indexed["observation"]["exit_code"], 7);
    assert_eq!(output["execution"]["applied_mutation_count"], 0);
    assert_eq!(db_digest(&data_dir), before);
    fs2::FileExt::unlock(&holder).expect("release actual index lock");
}

#[test]
fn live_onboarding_reports_partial_progress_when_a_session_has_an_active_writer() {
    let home = tempfile::tempdir().expect("home");
    let data_dir = home.path().join("data");
    let db = data_dir.join("agent_search.db");
    configure_remote(home.path(), "~/.codex");
    let mirror = data_dir.join("remotes/guide-remote/mirror").join(
        coding_agent_search::sources::sync::path_to_safe_dirname("~/.codex"),
    );
    util::seed_codex_session(
        &mirror,
        "rollout-available.jsonl",
        "guideavailablemarker",
        true,
    );
    // Mirrors are intentionally immutable snapshots for the active-source
    // policy. Hold a real local writer while the staged remote stays readable.
    let local = home.path().join(".codex");
    util::seed_codex_session(&local, "rollout-active.jsonl", "guideactivemarker", true);
    let active = local.join("sessions/2026/04/23/rollout-active.jsonl");
    let holder = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&active)
        .expect("active session");
    fs2::FileExt::try_lock_exclusive(&holder).expect("active writer lock");
    let output = onboard(home.path(), &data_dir, &db, true);
    let indexed = step(&output, "index.incremental");
    assert_eq!(indexed["result"], "failed", "{output}");
    assert_eq!(indexed["mutation_started"], true);
    assert_eq!(indexed["observation"]["deferred_active_sources"], 1);
    assert_eq!(indexed["observation"]["state_may_have_changed"], true);
    assert_eq!(output["execution"]["applied_mutation_count"], 0);
    assert_eq!(output["mutation_contract"]["read_only"], false);
    let (_, status) = run_on_archive(home.path(), &data_dir, &db, &["status", "--json"]);
    assert_eq!(status["database"]["conversations"], 1, "{status}");
    let (success, hits) = run_on_archive(
        home.path(),
        &data_dir,
        &db,
        &[
            "search",
            "guideavailablemarker",
            "--mode",
            "lexical",
            "--json",
        ],
    );
    assert!(success, "{hits}");
    assert!(hits["hits"].as_array().is_some_and(|hits| !hits.is_empty()));
    fs2::FileExt::unlock(&holder).expect("release active writer lock");
    drop(holder);
    let retried = onboard(home.path(), &data_dir, &db, true);
    assert_eq!(
        step(&retried, "index.incremental")["result"],
        "executed",
        "{retried}"
    );
    assert_eq!(
        step(&retried, "index.incremental")["observation"]["conversations"],
        2
    );
    let retry_counts = &step(&retried, "index.incremental")["observation"]["index_observations"];
    assert_eq!(retry_counts["conversations"], 1);
    let (success, active_hits) = run_on_archive(
        home.path(),
        &data_dir,
        &db,
        &["search", "guideactivemarker", "--mode", "lexical", "--json"],
    );
    assert!(success, "{active_hits}");
    assert!(
        active_hits["hits"]
            .as_array()
            .is_some_and(|hits| !hits.is_empty())
    );
    let no_op = onboard(home.path(), &data_dir, &db, true);
    let indexed = step(&no_op, "index.incremental");
    assert_eq!(indexed["result"], "executed", "{no_op}");
    assert_eq!(indexed["observation"]["conversations"], 2);
    assert_eq!(
        indexed["observation"]["index_observations"]["conversations"],
        0
    );
}

#[test]
fn live_missing_archive_fails_readiness_without_creating_an_archive() {
    let home = tempfile::tempdir().expect("home");
    let data_dir = home.path().join("data");
    std::fs::create_dir_all(&data_dir).expect("data directory");
    let (_, output) = run(
        cass(home.path()).args(["guide", "investigate-search-miss", "--apply", "--json"]),
        &data_dir,
    );
    let readiness = step(&output, "search.readiness");
    assert_eq!(readiness["proof_gate"]["source"], "live-command");
    assert_eq!(readiness["proof_gate"]["result"], "failed");
    assert_eq!(readiness["observation"]["healthy"], false);
    assert_eq!(output["execution"]["attempted_mutation_count"], 0);
    assert_eq!(output["mutation_contract"]["read_only"], true);
    assert!(!data_dir.join("agent_search.db").exists());
}

#[test]
fn live_readiness_and_coverage_use_the_running_binary_and_leave_archive_bytes_unchanged() {
    let home = tempfile::tempdir().expect("home");
    let data_dir = home.path().join("data with spaces");
    seed_archive(home.path(), &data_dir);
    let before = db_digest(&data_dir);
    let empty_path = home.path().join("empty-path");
    std::fs::create_dir_all(&empty_path).expect("empty PATH");
    let (_, output) = run(
        cass(home.path()).env("PATH", &empty_path).args([
            "guide",
            "investigate-search-miss",
            "--apply",
            "--json",
        ]),
        &data_dir,
    );
    let readiness = step(&output, "search.readiness");
    assert_eq!(readiness["proof_gate"]["source"], "live-command");
    assert_eq!(readiness["proof_gate"]["result"], "passed", "{output}");
    let coverage = step(&output, "diag.search-coverage");
    assert_eq!(coverage["proof_gate"]["source"], "live-command");
    assert_eq!(coverage["proof_gate"]["result"], "passed", "{output}");
    assert!(
        coverage["observation"]["lexical"]["live_documents"]
            .as_u64()
            .is_some_and(|count| count > 0),
        "coverage must report the real indexed archive: {coverage}"
    );
    assert_eq!(output["execution"]["attempted_mutation_count"], 0);
    assert_eq!(output["mutation_contract"]["read_only"], true);
    assert_eq!(db_digest(&data_dir), before);
    assert!(!output.to_string().contains(PRIVATE_BODY));
}

#[test]
fn live_confirmed_capsule_returns_a_manifest_receipt_without_modifying_the_archive() {
    let home = tempfile::tempdir().expect("home");
    let data_dir = home.path().join("data");
    seed_archive(home.path(), &data_dir);
    let before = db_digest(&data_dir);
    let (_, plan) = run(
        cass(home.path()).args(["guide", "support-capsule", "--json"]),
        &data_dir,
    );
    let privacy = plan["plan"]["privacy_tier"].as_str().expect("privacy tier");
    let cost = plan["plan"]["cost_risk"]["risk_level"]
        .as_str()
        .expect("cost risk");
    let (_, output) = run(
        cass(home.path()).args([
            "guide",
            "support-capsule",
            "--apply",
            "--confirm-step",
            "3",
            "--accept-privacy-tier",
            privacy,
            "--accept-cost-risk",
            cost,
            "--confirm-stop-conditions-clear",
            "--json",
        ]),
        &data_dir,
    );
    let capsule = step(&output, "support.produce-capsule");
    assert_eq!(capsule["result"], "executed", "{output}");
    assert_eq!(capsule["mutation_started"], true);
    assert_eq!(output["execution"]["attempted_mutation_count"], 1);
    assert_eq!(output["execution"]["applied_mutation_count"], 1);
    assert_eq!(output["mutation_contract"]["read_only"], false);
    assert_eq!(output["mutation_contract"]["mutates_db"], false);
    let receipt = &capsule["observation"];
    assert_eq!(receipt["manifest_readable"], true);
    assert_eq!(receipt["contents_independently_verified"], false);
    let relative = receipt["manifest_path"]
        .as_str()
        .and_then(|path| path.strip_prefix("<data-dir>/"))
        .expect("redacted relative manifest path");
    let manifest = std::fs::read(data_dir.join(relative)).expect("produced manifest");
    assert_eq!(
        receipt["manifest_bytes"].as_u64(),
        Some(manifest.len() as u64)
    );
    let expected_hash = blake3::hash(&manifest).to_hex();
    assert_eq!(
        receipt["manifest_blake3"].as_str(),
        Some(expected_hash.as_str())
    );
    assert!(!output.to_string().contains(PRIVATE_BODY));
    assert_eq!(db_digest(&data_dir), before);
}

fn run_on_archive(home: &Path, data_dir: &Path, db: &Path, args: &[&str]) -> (bool, Value) {
    run(cass(home).args(args).arg("--db").arg(db), data_dir)
}

#[test]
fn live_confirmed_lexical_repair_verifies_the_selected_archive_without_ingesting_sources() {
    let home = tempfile::tempdir().expect("home");
    let data_dir = home.path().join("data with spaces");
    let db_path = home.path().join("selected canonical.db");
    std::fs::create_dir_all(&data_dir).expect("data directory");
    util::seed_codex_session(
        &home.path().join(".codex"),
        "rollout-guide-repair.jsonl",
        PRIVATE_BODY,
        true,
    );
    let archive = |args: &[&str]| run_on_archive(home.path(), &data_dir, &db_path, args);
    let (success, seeded) = archive(&["index", "--full", "--json"]);
    assert!(success, "seed index failed: {seeded}");
    let (_, before) = archive(&["status", "--json"]);
    assert_eq!(before["database"]["conversations"], 1, "{before}");
    let index_dir = coding_agent_search::search::tantivy::expected_index_dir(&data_dir);
    std::fs::rename(&index_dir, home.path().join("preserved-lexical-generation"))
        .expect("preserve lexical assets outside the active index directory");
    util::seed_codex_session(
        &home.path().join(".codex"),
        "rollout-guide-pending.jsonl",
        "guideunindexedpendingmarker",
        true,
    );

    let (_, plan) = archive(&["guide", "repair-assets", "--json"]);
    let planned = step(&plan, "doctor.rebuild-stale-assets");
    assert_eq!(planned["result"], "planned");
    assert_eq!(planned["execution_scope"]["source_scan"], false);
    assert_eq!(planned["execution_scope"]["semantic_rebuild"], false);
    assert!(!index_dir.join("MANIFEST").exists());
    let cost = plan["plan"]["cost_risk"]["risk_level"]
        .as_str()
        .expect("cost risk");
    let mut apply_args = vec![
        "guide",
        "repair-assets",
        "--apply",
        "--confirm-fact",
        "disk_headroom_ok",
        "--accept-cost-risk",
        cost,
        "--confirm-stop-conditions-clear",
        "--json",
    ];
    let (_, unconfirmed) = archive(&apply_args);
    assert_eq!(
        step(&unconfirmed, "doctor.rebuild-stale-assets")["result"],
        "awaiting-confirmation",
        "{unconfirmed}"
    );
    assert_eq!(unconfirmed["execution"]["attempted_mutation_count"], 0);
    assert!(!index_dir.join("MANIFEST").exists());

    apply_args.extend(["--confirm-step", "3"]);
    let (_, repaired) = archive(&apply_args);
    let repair = step(&repaired, "doctor.rebuild-stale-assets");
    assert_eq!(repair["result"], "executed", "{repaired}");
    assert_eq!(repair["proof_gate"]["name"], "lexical-ready");
    assert_eq!(repair["proof_gate"]["result"], "passed");
    assert_eq!(repair["mutation_started"], true);
    assert_eq!(repair["observation"]["lexical_ready"], true);
    assert_eq!(repair["observation"]["sources_scanned"], false);
    assert_eq!(repair["observation"]["semantic_rebuilt"], false);
    assert_eq!(repaired["execution"]["applied_mutation_count"], 1);
    assert_eq!(repaired["mutation_contract"]["mutates_db"], true);
    assert_eq!(repaired["mutation_contract"]["mutates_files"], true);
    assert_eq!(repaired["mutation_contract"]["touches_network"], false);
    assert!(!repaired.to_string().contains(PRIVATE_BODY));
    assert!(
        !data_dir.join("agent_search.db").exists(),
        "repair must honor the explicit archive override"
    );

    let (_, after) = archive(&["status", "--json"]);
    for pointer in [
        "/database/conversations",
        "/database/messages",
        "/index/fingerprint/current_db_fingerprint",
    ] {
        assert_eq!(
            after.pointer(pointer),
            before.pointer(pointer),
            "canonical corpus changed at {pointer}: {after}"
        );
    }
    assert_eq!(after["index"]["manifest_source"], "current");
    assert_eq!(
        after["index"]["fingerprint"]["matches_current_db_fingerprint"],
        true
    );
    let (search_ok, searchable) = archive(&["search", PRIVATE_BODY, "--json"]);
    assert!(
        search_ok,
        "repaired archive must be searchable: {searchable}"
    );
    assert!(
        searchable["hits"]
            .as_array()
            .is_some_and(|hits| !hits.is_empty()),
        "{searchable}"
    );
    let (_, pending) = archive(&["search", "guideunindexedpendingmarker", "--json"]);
    assert_eq!(
        pending["hits"].as_array().map(Vec::len),
        Some(0),
        "repair must not ingest pending sources: {pending}"
    );

    let (_, already_ready) = archive(&apply_args);
    assert_eq!(
        step(&already_ready, "doctor.rebuild-stale-assets")["result"],
        "already-ready",
        "{already_ready}"
    );
    assert_eq!(already_ready["execution"]["attempted_mutation_count"], 0);
    assert_eq!(already_ready["mutation_contract"]["read_only"], true);
}

#[test]
fn canonical_repair_guard_refuses_missing_and_empty_archives_without_source_discovery() {
    for existing_empty in [false, true] {
        let home = tempfile::tempdir().expect("home");
        let data_dir = home.path().join("data");
        std::fs::create_dir_all(&data_dir).expect("data directory");
        if existing_empty {
            let (success, empty) = run(
                cass(home.path()).args(["index", "--full", "--json"]),
                &data_dir,
            );
            assert!(success, "initialize empty archive: {empty}");
            assert_eq!(empty["conversations"], 0);
        }
        util::seed_codex_session(
            &home.path().join(".codex"),
            "rollout-guide-guard.jsonl",
            "guidecanonicalguardmarker",
            true,
        );
        let refused = cass(home.path())
            .env("CASS_INDEX_REQUIRE_CANONICAL_REBUILD", "1")
            .args(["index", "--force-rebuild", "--no-progress-events", "--json"])
            .arg("--data-dir")
            .arg(&data_dir)
            .output()
            .expect("run guarded canonical repair");
        assert!(
            !refused.status.success(),
            "scoped repair must refuse missing/empty archive: {}",
            String::from_utf8_lossy(&refused.stdout)
        );
        if existing_empty {
            let (_, after) = run(cass(home.path()).args(["status", "--json"]), &data_dir);
            assert_eq!(
                after["database"]["conversations"], 0,
                "guard must refuse before source discovery: {after}"
            );
        } else {
            assert!(!data_dir.join("agent_search.db").exists());
        }
        // Ordinary first-run --force-rebuild keeps source discovery when the
        // internal canonical-only scope requirement is absent.
        let (success, ordinary) = run(
            cass(home.path()).args(["index", "--force-rebuild", "--no-progress-events", "--json"]),
            &data_dir,
        );
        assert!(
            success,
            "ordinary first-run force rebuild must still work: {ordinary}"
        );
        assert_eq!(ordinary["conversations"], 1, "{ordinary}");
    }
}
