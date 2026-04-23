//! Golden regression tests for seeded health/status semantic-readiness JSON.
//!
//! Bead `[ibuuh.9-golden]`: freeze the robot-mode shape for the new
//! semantic readiness fields landed in ibuuh.9 so future drift on
//! `fast_tier`, `quality_tier`, `backlog`, `checkpoint`, or
//! `recommended_action` fails loudly at commit time.
//!
//! Regenerate with:
//! `UPDATE_GOLDENS=1 cargo test --test golden_readiness`

use assert_cmd::Command;
use coding_agent_search::search::policy::{CHUNKING_STRATEGY_VERSION, SEMANTIC_SCHEMA_VERSION};
use coding_agent_search::search::semantic_manifest::{
    ArtifactRecord, BacklogLedger, BuildCheckpoint, SemanticManifest, TierKind,
};
use coding_agent_search::search::tantivy::index_dir;
use coding_agent_search::storage::sqlite::FrankenStorage;
use std::fs;
use std::path::{Path, PathBuf};

fn cass_cmd(test_home: &Path) -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("XDG_DATA_HOME", test_home)
        .env("HOME", test_home)
        .env("CASS_IGNORE_SOURCES_CONFIG", "1");
    cmd
}

fn scrub_robot_json(input: &str, test_home: &Path) -> String {
    let mut out = input.to_string();

    let crate_version_re = regex::Regex::new(r#""crate_version"\s*:\s*"[^"]*""#).unwrap();
    out = crate_version_re
        .replace_all(&out, r#""crate_version": "[VERSION]""#)
        .to_string();

    let ts_re =
        regex::Regex::new(r#"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:\d{2})?"#)
            .unwrap();
    out = ts_re.replace_all(&out, "[TIMESTAMP]").to_string();

    let home_str = test_home.display().to_string();
    if !home_str.is_empty() {
        out = out.replace(&home_str, "[TEST_HOME]");
    }

    let uuid_re =
        regex::Regex::new(r#"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}"#)
            .unwrap();
    out = uuid_re.replace_all(&out, "[UUID]").to_string();

    let latency_re = regex::Regex::new(r#""latency_ms"\s*:\s*\d+"#).unwrap();
    out = latency_re
        .replace_all(&out, r#""latency_ms": "[LATENCY_MS]""#)
        .to_string();

    for key in ["load_per_core", "psi_cpu_some_avg10"] {
        let re = regex::Regex::new(&format!(
            r#""{key}"\s*:\s*(-?\d+(\.\d+)?([eE][+-]?\d+)?|null)"#
        ))
        .unwrap();
        out = re
            .replace_all(&out, format!(r#""{key}": "[LIVE_METRIC]""#).as_str())
            .to_string();
    }

    for key in [
        "healthy_streak",
        "ticks_total",
        "load_window_len",
        "psi_window_len",
        "observations_total",
    ] {
        let re = regex::Regex::new(&format!(r#""{key}"\s*:\s*\d+"#)).unwrap();
        out = re
            .replace_all(&out, format!(r#""{key}": "[LIVE_COUNTER]""#).as_str())
            .to_string();
    }

    let last_snapshot_obj_re = regex::Regex::new(r#"(?s)"last_snapshot"\s*:\s*\{[^}]*\}"#).unwrap();
    out = last_snapshot_obj_re
        .replace_all(&out, r#""last_snapshot": "[LIVE_SAMPLE]""#)
        .to_string();
    let last_snapshot_null_re = regex::Regex::new(r#""last_snapshot"\s*:\s*null"#).unwrap();
    out = last_snapshot_null_re
        .replace_all(&out, r#""last_snapshot": "[LIVE_SAMPLE]""#)
        .to_string();
    let last_reason_re = regex::Regex::new(r#""last_reason"\s*:\s*(null|"[^"]*")"#).unwrap();
    out = last_reason_re
        .replace_all(&out, r#""last_reason": "[LIVE_SAMPLE]""#)
        .to_string();

    out
}

fn assert_golden(name: &str, actual: &str) {
    let golden_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join(name);

    if std::env::var("UPDATE_GOLDENS").is_ok() {
        std::fs::create_dir_all(golden_path.parent().expect("golden parent"))
            .expect("create golden parent");
        std::fs::write(&golden_path, actual).expect("write golden");
        eprintln!("[GOLDEN] Updated: {}", golden_path.display());
        return;
    }

    let expected = std::fs::read_to_string(&golden_path).unwrap_or_else(|err| {
        panic!(
            "Golden file missing or unreadable: {}\n{err}\n\n\
             Run with UPDATE_GOLDENS=1 to create it, then review and commit:\n\
             \tUPDATE_GOLDENS=1 cargo test --test golden_readiness\n\
             \tgit diff tests/golden/\n\
             \tgit add tests/golden/",
            golden_path.display(),
        )
    });

    if actual != expected {
        let actual_path = golden_path.with_extension("actual");
        std::fs::write(&actual_path, actual).expect("write .actual file");
        panic!(
            "GOLDEN MISMATCH: {name}\n\n\
             Expected: {}\n\
             Actual:   {}\n\n\
             diff the two files, then either fix the code or regenerate with:\n\
             \tUPDATE_GOLDENS=1 cargo test --test golden_readiness",
            golden_path.display(),
            actual_path.display(),
        );
    }
}

fn capture_data_dir_robot_json(data_dir: &Path, subcommand: &str, allow_nonzero: bool) -> String {
    let output = cass_cmd(data_dir)
        .arg(subcommand)
        .arg("--json")
        .arg("--data-dir")
        .arg(data_dir)
        .output()
        .unwrap_or_else(|err| {
            panic!(
                "run cass {subcommand} --json --data-dir {:?}: {err}",
                data_dir
            )
        });
    if !allow_nonzero {
        assert!(
            output.status.success(),
            "cass {subcommand} exited non-zero: status={:?}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap_or_else(|err| {
        panic!("cass {subcommand} stdout is not JSON: {err}\nstdout:\n{stdout}")
    });
    let semantic = match subcommand {
        "health" => parsed["state"]["semantic"].clone(),
        "status" => parsed["semantic"].clone(),
        other => panic!("unsupported readiness golden subcommand: {other}"),
    };
    let projected = serde_json::json!({
        "surface": subcommand,
        "recommended_action": parsed.get("recommended_action").cloned().unwrap_or(serde_json::Value::Null),
        "semantic": {
            "fast_tier": semantic["fast_tier"].clone(),
            "quality_tier": semantic["quality_tier"].clone(),
            "backlog": semantic["backlog"].clone(),
            "checkpoint": semantic["checkpoint"].clone(),
        }
    });
    let canonical = serde_json::to_string_pretty(&projected).expect("pretty-print JSON");
    scrub_robot_json(&canonical, data_dir)
}

fn seed_semantic_progress_fixture(
    data_dir: &Path,
    fast_tier_ready: bool,
    checkpoint_tier: TierKind,
) {
    let db_path = data_dir.join("agent_search.db");
    FrankenStorage::open(&db_path)
        .expect("create canonical DB")
        .close()
        .expect("close canonical DB");

    let index_path = index_dir(data_dir).expect("index dir");
    fs::create_dir_all(&index_path).expect("create index dir");
    fs::write(index_path.join("meta.json"), b"{}").expect("write index meta");

    let mut manifest = SemanticManifest::default();
    if fast_tier_ready {
        manifest.fast_tier = Some(ArtifactRecord {
            tier: TierKind::Fast,
            embedder_id: "hash".to_string(),
            model_revision: "hash".to_string(),
            schema_version: SEMANTIC_SCHEMA_VERSION,
            chunking_version: CHUNKING_STRATEGY_VERSION,
            dimension: 256,
            doc_count: 120,
            conversation_count: 12,
            db_fingerprint: "fixture-db-fingerprint".to_string(),
            index_path: "vector_index/vector.fast.idx".to_string(),
            size_bytes: 4_096,
            started_at_ms: 1_733_100_000_000,
            completed_at_ms: 1_733_100_100_000,
            ready: true,
        });
    }
    manifest.backlog = BacklogLedger {
        total_conversations: 20,
        fast_tier_processed: if fast_tier_ready { 12 } else { 0 },
        quality_tier_processed: 3,
        db_fingerprint: "fixture-db-fingerprint".to_string(),
        computed_at_ms: 1_733_100_200_000,
    };
    manifest.checkpoint = Some(BuildCheckpoint {
        tier: checkpoint_tier,
        embedder_id: "all-minilm-l6-v2".to_string(),
        last_offset: 77,
        docs_embedded: 66,
        conversations_processed: 3,
        total_conversations: 20,
        db_fingerprint: "fixture-db-fingerprint".to_string(),
        schema_version: SEMANTIC_SCHEMA_VERSION,
        chunking_version: CHUNKING_STRATEGY_VERSION,
        saved_at_ms: 1_733_100_300_000,
    });
    manifest.save(data_dir).expect("save semantic manifest");
}

#[test]
fn health_semantic_progress_json_matches_golden() {
    let test_home = tempfile::tempdir().expect("create temp home");
    seed_semantic_progress_fixture(test_home.path(), true, TierKind::Quality);
    let scrubbed = capture_data_dir_robot_json(test_home.path(), "health", true);
    assert_golden("robot/health_semantic_progress.json.golden", &scrubbed);
}

#[test]
fn status_semantic_progress_json_matches_golden() {
    let test_home = tempfile::tempdir().expect("create temp home");
    seed_semantic_progress_fixture(test_home.path(), true, TierKind::Quality);
    let scrubbed = capture_data_dir_robot_json(test_home.path(), "status", false);
    assert_golden("robot/status_semantic_progress.json.golden", &scrubbed);
}

#[test]
fn health_semantic_backfill_wait_json_matches_golden() {
    let test_home = tempfile::tempdir().expect("create temp home");
    seed_semantic_progress_fixture(test_home.path(), false, TierKind::Fast);
    let scrubbed = capture_data_dir_robot_json(test_home.path(), "health", true);
    assert_golden("robot/health_semantic_backfill_wait.json.golden", &scrubbed);
}

#[test]
fn status_semantic_backfill_wait_json_matches_golden() {
    let test_home = tempfile::tempdir().expect("create temp home");
    seed_semantic_progress_fixture(test_home.path(), false, TierKind::Fast);
    let scrubbed = capture_data_dir_robot_json(test_home.path(), "status", false);
    assert_golden("robot/status_semantic_backfill_wait.json.golden", &scrubbed);
}
