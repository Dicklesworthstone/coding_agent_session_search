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
    let (success, output) = run(
        cass(home).args(["index", "--full", "--json"]),
        data_dir,
    );
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
        cass(home.path())
            .env("PATH", &empty_path)
            .args(["guide", "investigate-search-miss", "--apply", "--json"]),
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
