//! GH #457: a hollow published Quill generation — one that serves a handful
//! of documents while its completed rebuild checkpoint, content fingerprint
//! and generation manifest all still look right — must fail readiness on
//! every surface, and the next `cass index` must rebuild it from the
//! canonical database instead of certifying it.
//!
//! The hollow state is produced the way the engine produces it: a later
//! generation that drops the live documents (`delete_all` publishes the empty
//! successor MANIFEST; one stray document is then published on top so the
//! served count is small but nonzero, as in the report) while the checkpoint
//! and generation manifest written by the last rebuild stay untouched — the
//! exact shape the reporter saw (`checkpoint.completed: true`, fingerprint
//! matching, 3 hits where the archive holds 37,513).

use assert_cmd::Command;
use coding_agent_search::search::quill_bridge::QuillCassIndex;
use coding_agent_search::search::tantivy::{TantivyIndex, expected_index_dir};
use frankensearch::quill::cass::CassDocument;
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

mod util;

const PROBE: &str = "hollowprobe";
const SESSIONS: usize = 6;

fn cass(home: &Path) -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");
    cmd.env("CASS_IGNORE_SOURCES_CONFIG", "1");
    cmd.env("CASS_AUTO_REFRESH", "0");
    cmd.env("HOME", home);
    cmd.env("XDG_DATA_HOME", home.join(".local/share"));
    cmd.env("XDG_CONFIG_HOME", home.join(".config"));
    cmd.env("CODEX_HOME", home.join(".codex"));
    cmd.timeout(Duration::from_secs(240));
    cmd
}

fn run(cmd: &mut Command, what: &str) -> (bool, String, String) {
    let output = cmd
        .output()
        .unwrap_or_else(|err| panic!("run {what}: {err}"));
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn run_json(cmd: &mut Command, what: &str) -> (bool, Value) {
    let (success, stdout, stderr) = run(cmd, what);
    let json: Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|err| {
        panic!("{what}: stdout is not JSON ({err})\nstdout: {stdout}\nstderr: {stderr}")
    });
    (success, json)
}

fn index_full(home: &Path, data_dir: &Path) {
    let (success, json) = run_json(
        cass(home)
            .args(["index", "--full", "--json", "--data-dir"])
            .arg(data_dir),
        "cass index --full",
    );
    assert!(success, "cass index --full must succeed: {json}");
    assert_eq!(json["success"].as_bool(), Some(true), "{json}");
}

fn lexical_matches(home: &Path, data_dir: &Path) -> u64 {
    let (success, json) = run_json(
        cass(home)
            .args([
                "search",
                PROBE,
                "--json",
                "--mode",
                "lexical",
                "--limit",
                "100",
                "--data-dir",
            ])
            .arg(data_dir),
        "cass search",
    );
    assert!(success, "cass search must succeed: {json}");
    json["total_matches"].as_u64().unwrap_or_else(|| {
        json["hits"]
            .as_array()
            .map(|hits| hits.len() as u64)
            .unwrap_or(0)
    })
}

fn status_json(home: &Path, data_dir: &Path) -> Value {
    run_json(
        cass(home)
            .args(["status", "--json", "--data-dir"])
            .arg(data_dir),
        "cass status --json",
    )
    .1
}

fn health_json(home: &Path, data_dir: &Path) -> Value {
    run_json(
        cass(home)
            .args(["health", "--json", "--data-dir"])
            .arg(data_dir),
        "cass health --json",
    )
    .1
}

fn doctor_checks(home: &Path, data_dir: &Path) -> Vec<Value> {
    let (_, json) = run_json(
        cass(home)
            .args(["doctor", "check", "--json", "--data-dir"])
            .arg(data_dir),
        "cass doctor check --json",
    );
    json["checks"]
        .as_array()
        .cloned()
        .unwrap_or_else(|| panic!("doctor payload has no checks array: {json}"))
}

fn checkpoint_completed(index_path: &Path) -> bool {
    let bytes = std::fs::read(index_path.join(".lexical-rebuild-state.json"))
        .expect("read lexical rebuild checkpoint");
    let json: Value = serde_json::from_slice(&bytes).expect("parse lexical rebuild checkpoint");
    json["completed"].as_bool().expect("checkpoint.completed")
}

/// Publish a hollow successor generation: every live document dropped, then
/// one stray document that does not match the probe, so the generation
/// serves 1 where the checkpoint certified `expected_docs`.
fn hollow_the_generation(index_path: &Path) {
    {
        let mut live = TantivyIndex::open_or_create(index_path).expect("open live generation");
        live.delete_all()
            .expect("publish the empty successor generation");
    }
    let mut index = QuillCassIndex::open_or_create(index_path).expect("reopen hollow generation");
    index
        .add_cass_documents(&[CassDocument {
            agent: "codex".to_owned(),
            workspace: Some("/stray".to_owned()),
            workspace_original: Some("/stray".to_owned()),
            source_path: "/stray/rollout-stray.jsonl".to_owned(),
            msg_idx: 0,
            created_at: Some(1_714_000_000),
            title: Some("stray".to_owned()),
            content: "a stray survivor that never matches the probe".to_owned(),
            source_id: "stray".to_owned(),
            origin_kind: "local".to_owned(),
            origin_host: None,
            conversation_id: Some(1),
        }])
        .expect("publish one stray document");
    index.commit().expect("commit the stray document");
}

#[test]
fn gh457_hollow_generation_fails_readiness_and_the_next_index_run_rebuilds_it() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path();
    let data_dir = home.join("cass_data");
    std::fs::create_dir_all(&data_dir).expect("create data dir");
    let codex_home = home.join(".codex");
    for n in 0..SESSIONS {
        util::seed_codex_session(
            &codex_home,
            &format!("rollout-hollow-{n:02}.jsonl"),
            PROBE,
            true,
        );
    }
    let expected_docs = (SESSIONS * 2) as u64;

    // Baseline: a healthy archive whose generation serves every document.
    index_full(home, &data_dir);
    let index_path = expected_index_dir(&data_dir);
    assert!(checkpoint_completed(&index_path));
    assert_eq!(lexical_matches(home, &data_dir), expected_docs);
    let healthy = health_json(home, &data_dir);
    assert_eq!(healthy["healthy"].as_bool(), Some(true), "{healthy}");
    let ready = status_json(home, &data_dir);
    assert_eq!(ready["index"]["status"].as_str(), Some("ready"), "{ready}");
    assert_eq!(ready["index"]["hollow"].as_bool(), Some(false), "{ready}");
    assert_eq!(
        ready["index"]["live_documents"].as_u64(),
        Some(expected_docs),
        "{ready}"
    );
    assert_eq!(
        ready["index"]["documents"].as_u64(),
        Some(expected_docs),
        "{ready}"
    );

    // Hollow it: the engine publishes successor generations that serve one
    // stray document, while every cass-side certificate stays exactly as the
    // rebuild left it.
    hollow_the_generation(&index_path);
    assert!(
        checkpoint_completed(&index_path),
        "the rebuild checkpoint must still certify the (now hollow) generation"
    );
    assert_eq!(
        lexical_matches(home, &data_dir),
        0,
        "the hollow generation answers the probe with nothing"
    );

    // status: its own status, the served count, the checkpoint still
    // matching — the silent-healthy shape is now named.
    let hollow = status_json(home, &data_dir);
    assert_eq!(
        hollow["index"]["status"].as_str(),
        Some("hollow"),
        "{hollow}"
    );
    assert_eq!(hollow["index"]["hollow"].as_bool(), Some(true), "{hollow}");
    assert_eq!(
        hollow["index"]["live_documents"].as_u64(),
        Some(1),
        "{hollow}"
    );
    assert_eq!(hollow["index"]["documents"].as_u64(), Some(1), "{hollow}");
    assert_eq!(
        hollow["index"]["empty_with_messages"].as_bool(),
        Some(false),
        "one served document is hollow, not empty: {hollow}"
    );
    assert_eq!(hollow["index"]["fresh"].as_bool(), Some(false), "{hollow}");
    assert_eq!(hollow["index"]["stale"].as_bool(), Some(true), "{hollow}");
    assert_eq!(
        hollow["index"]["checkpoint"]["completed"].as_bool(),
        Some(true),
        "{hollow}"
    );
    assert_eq!(
        hollow["index"]["fingerprint"]["matches_current_db_fingerprint"].as_bool(),
        Some(true),
        "{hollow}"
    );
    let reason = hollow["index"]["reason"].as_str().unwrap_or_default();
    assert!(reason.contains("HOLLOW"), "{hollow}");
    assert!(reason.contains("1 live document"), "{hollow}");
    assert!(
        reason.contains(&format!("{expected_docs} indexed document")),
        "{hollow}"
    );
    assert!(reason.contains("Run `cass index`"), "{hollow}");
    assert_eq!(hollow["healthy"].as_bool(), Some(false), "{hollow}");
    let action = hollow["recommended_action"].as_str().unwrap_or_default();
    assert!(action.contains("Run 'cass index'"), "{hollow}");
    assert!(action.contains("hollow"), "{hollow}");

    // health: unhealthy, with the gap named in errors[] and the remedy in
    // recommended_action — without opening the archive or the engine.
    let unhealthy = health_json(home, &data_dir);
    assert_eq!(unhealthy["healthy"].as_bool(), Some(false), "{unhealthy}");
    let errors = unhealthy["errors"].as_array().cloned().unwrap_or_default();
    assert!(
        errors.iter().any(|err| err
            .as_str()
            .is_some_and(|text| text.contains("index hollow"))),
        "{unhealthy}"
    );
    assert!(
        !errors.iter().any(|err| err.as_str() == Some("index stale")),
        "hollow replaces the generic stale error: {unhealthy}"
    );
    assert!(
        unhealthy["recommended_action"]
            .as_str()
            .is_some_and(|text| text.contains("Run 'cass index'") && text.contains("hollow")),
        "{unhealthy}"
    );
    assert_eq!(
        unhealthy["state"]["index"]["status"].as_str(),
        Some("hollow"),
        "{unhealthy}"
    );

    // doctor: index_sync names the shortfall as a warning with a fix.
    let checks = doctor_checks(home, &data_dir);
    let index_sync = checks
        .iter()
        .find(|check| check["name"].as_str() == Some("index_sync"))
        .unwrap_or_else(|| panic!("doctor must emit an index_sync check: {checks:?}"));
    assert_eq!(index_sync["status"].as_str(), Some("warn"), "{index_sync}");
    assert_eq!(
        index_sync["fix_available"].as_bool(),
        Some(true),
        "{index_sync}"
    );
    assert!(
        index_sync["message"]
            .as_str()
            .is_some_and(|text| text.contains("HOLLOW") && text.contains("Run `cass index`")),
        "{index_sync}"
    );

    // The named remedy works: a plain `cass index` sees the sparse live index
    // before scanning, rebuilds the generation from the canonical database,
    // ingests the new session, and certifies the result.
    util::seed_codex_session(
        &codex_home,
        &format!("rollout-hollow-{SESSIONS:02}.jsonl"),
        PROBE,
        true,
    );
    let (success, stdout, stderr) = run(
        cass(home)
            .args(["index", "--json", "--data-dir"])
            .arg(&data_dir),
        "cass index (incremental on a hollow generation)",
    );
    assert!(
        success,
        "the incremental run must repair the hollow generation\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("authoritative_canonical_db_rebuild")
            && stdout.contains("repairs_sparse_tantivy"),
        "the run must report the pre-scan canonical rebuild\nstdout: {stdout}"
    );
    let repaired_docs = ((SESSIONS + 1) * 2) as u64;
    assert_eq!(lexical_matches(home, &data_dir), repaired_docs);
    let repaired = status_json(home, &data_dir);
    // The generation is whole again: the hollow verdict is gone and the
    // served count covers every session (the pre-scan repair certified the
    // six it rebuilt from; the scan's own ingest then added the seventh, so
    // the checkpoint's content fingerprint may lag the archive by that one
    // session until the next run — ordinary staleness, not hollowness).
    assert_ne!(
        repaired["index"]["status"].as_str(),
        Some("hollow"),
        "{repaired}"
    );
    assert_eq!(
        repaired["index"]["hollow"].as_bool(),
        Some(false),
        "{repaired}"
    );
    assert!(
        !repaired["index"]["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("HOLLOW"),
        "{repaired}"
    );
    assert_eq!(
        repaired["index"]["live_documents"].as_u64(),
        Some(repaired_docs),
        "{repaired}"
    );
    assert!(checkpoint_completed(&index_path));
    let repaired_health = health_json(home, &data_dir);
    assert!(
        !repaired_health["errors"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .any(|err| err.as_str().is_some_and(|text| text.contains("hollow"))),
        "{repaired_health}"
    );
    let checks = doctor_checks(home, &data_dir);
    assert!(
        !checks
            .iter()
            .any(|check| check["name"].as_str() == Some("index_sync")),
        "no coverage warning after the repair: {checks:?}"
    );
}
