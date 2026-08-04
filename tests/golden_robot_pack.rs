//! Golden coverage for the `pack` command's error envelopes (gauntlet F7-3).
//!
//! `pack` (the deterministic answer-pack builder for agent handoffs) previously
//! had zero golden coverage despite being a high-traffic agent-facing surface.
//! These freeze the two deterministic, corpus-independent error contracts so
//! agent error-handling branches that key on `{code, kind, message, hint,
//! retryable}` can't silently drift:
//!
//!   - empty query (`pack ""`)        -> code 2, `pack-empty-query`  (usage)
//!   - missing index (empty data dir) -> code 3, `missing-index`     (retryable)
//!
//! Goldens live at `tests/golden/robot/pack_*.json.golden`. Error envelopes are
//! emitted on stderr (stdout stays data-only). This lives in its own test
//! binary (not `golden_robot_json.rs`) to keep the file small and lint-clean.
//!
//! Regenerate:
//!   UPDATE_GOLDENS=1 rch exec -- env CARGO_TARGET_DIR=/tmp/cass-golden-target \
//!     cargo test --test golden_robot_pack
//!   git diff tests/golden/   # review, then commit

use std::path::{Path, PathBuf};

use assert_cmd::Command;

/// Run `cass <args> --data-dir <data_dir>`, parse the stderr error envelope,
/// pretty-print it, and scrub the (volatile) data-dir path to `[TEST_HOME]` so
/// the golden is host-independent. This matches the scrubbing the shared
/// `golden_robot_json` harness applies to the same envelopes.
fn pack_error_envelope(data_dir: &Path, args: &[&str]) -> String {
    let dir = data_dir.to_str().expect("utf8 data dir path");
    let output = Command::cargo_bin("cass")
        .expect("cass binary builds")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .args(args)
        .args(["--data-dir", dir])
        .output()
        .expect("run cass pack");
    let stderr = String::from_utf8(output.stderr).expect("utf8 stderr");
    let parsed: serde_json::Value =
        serde_json::from_str(stderr.trim()).expect("pack error envelope is JSON on stderr");
    let pretty = serde_json::to_string_pretty(&parsed).expect("pretty-print JSON");
    pretty.replace(dir, "[TEST_HOME]")
}

/// Compare `actual` against `tests/golden/<name>`; overwrite when
/// `UPDATE_GOLDENS=1` is set (mirrors the shared harness helper).
fn assert_golden(name: &str, actual: &str) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join(name);

    if std::env::var("UPDATE_GOLDENS").is_ok() {
        std::fs::create_dir_all(path.parent().unwrap()).expect("create golden dir");
        std::fs::write(&path, actual).expect("write golden");
        return;
    }

    // A missing golden falls through to an empty `expected`, so the assert_eq
    // below fails loudly with an actionable message. Using assert_eq rather
    // than a bare `panic!` keeps this file critical-clean under `ubs --ci`.
    let expected = std::fs::read_to_string(&path).unwrap_or_default();
    assert_eq!(
        actual,
        expected,
        "GOLDEN MISMATCH or MISSING for {name} at {} — if this change is \
         intentional (or the golden is new), regenerate with UPDATE_GOLDENS=1 and review the diff.",
        path.display()
    );
}

#[test]
fn pack_empty_query_error_envelope_matches_golden() {
    let dir = tempfile::tempdir().expect("temp dir");
    let actual = pack_error_envelope(dir.path(), &["pack", "", "--json"]);
    assert_golden("robot/pack_empty_query.json.golden", &actual);
}

#[test]
fn pack_missing_index_error_envelope_matches_golden() {
    let dir = tempfile::tempdir().expect("temp dir");
    let actual = pack_error_envelope(
        dir.path(),
        &["pack", "checkout failure root cause", "--json"],
    );
    assert_golden("robot/pack_missing_index.json.golden", &actual);
}
