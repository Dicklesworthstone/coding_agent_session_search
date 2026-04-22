//! Golden-file regression tests for full `cass export-html` output.
//!
//! Bead `z2hck` freezes scrubbed, complete HTML documents instead of checking
//! small fragments. Regenerate with:
//!
//! ```bash
//! UPDATE_GOLDENS=1 cargo test --test pages_export_golden
//! git diff tests/golden/html_export/        # review every exported HTML diff
//! git add tests/golden/html_export/
//! ```
//!
//! Scrubbing normalizes line endings/trailing whitespace and replaces
//! environment-specific paths, UUIDs, version stamps, ISO timestamps, and
//! encrypted payload bytes. The remaining document still includes the doctype,
//! metadata, styles, scripts, and rendered message HTML.

use assert_cmd::Command;
use regex::Regex;
use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

const GOLDEN_BYTES_LABEL: &str = "z2hck-html-export-golden-v1";

fn fixture_phrase() -> String {
    ["golden", "html", "fixture"].join("-")
}

fn cass_cmd(test_home: &Path) -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("HOME", test_home)
        .env("XDG_CONFIG_HOME", test_home.join(".config"))
        .env("XDG_DATA_HOME", test_home.join(".local/share"))
        .env("NO_COLOR", "1");
    cmd
}

fn write_fixture_session(test_home: &Path) -> PathBuf {
    let session_dir = test_home
        .join(".claude")
        .join("projects")
        .join("cass-html-export-fixture");
    fs::create_dir_all(&session_dir).expect("create fixture session dir");
    let session_path = session_dir.join("session-z2hck.jsonl");

    let messages = [
        json!({
            "type": "user",
            "timestamp": 1_705_334_400_000i64,
            "message": {
                "role": "user",
                "content": "Please review src/auth/login.rs and fix the token refresh bug.\n\nExpected: retry once, then show a clear error."
            }
        }),
        json!({
            "type": "assistant",
            "timestamp": 1_705_334_460_000i64,
            "message": {
                "role": "assistant",
                "content": "I found the refresh loop and changed the guard. The important branch now returns after the retry:\n\n```rust\nif retry_count > 0 {\n    return Err(AuthError::ExpiredToken);\n}\n```"
            }
        }),
        json!({
            "type": "user",
            "timestamp": 1_705_334_520_000i64,
            "message": {
                "role": "user",
                "content": "Add a regression test named token_refresh_stops_after_one_retry."
            }
        }),
    ];

    let mut file = fs::File::create(&session_path).expect("create fixture session");
    for message in messages {
        writeln!(
            file,
            "{}",
            serde_json::to_string(&message).expect("serialize message")
        )
        .expect("write fixture JSONL line");
    }

    session_path
}

fn export_html(
    test_home: &Path,
    session_path: &Path,
    output_dir: &Path,
    filename: &str,
    encrypted: bool,
) -> String {
    let mut cmd = cass_cmd(test_home);
    cmd.arg("export-html")
        .arg(session_path)
        .arg("--output-dir")
        .arg(output_dir)
        .arg("--filename")
        .arg(filename)
        .arg("--json")
        .arg("--no-cdns");

    if encrypted {
        let phrase = fixture_phrase();
        cmd.env("CASS_HTML_EXPORT_GOLDEN_BYTES_LABEL", GOLDEN_BYTES_LABEL)
            .arg("--encrypt")
            .arg("--password")
            .arg(phrase);
    }

    let output = cmd.output().expect("run cass export-html");
    assert!(
        output.status.success(),
        "cass export-html exited non-zero: status={:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    let payload: Value = serde_json::from_str(&stdout).expect("export-html emits JSON");
    assert_eq!(payload["success"], true);
    assert_eq!(payload["exported"]["encrypted"], encrypted);
    assert_eq!(payload["exported"]["messages_count"], 3);

    let output_path = payload["exported"]["output_path"]
        .as_str()
        .expect("output_path string");
    fs::read_to_string(output_path).expect("read exported HTML")
}

fn canonicalize_html(input: &str) -> String {
    let normalized = input.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = String::with_capacity(normalized.len() + 1);
    for line in normalized.lines() {
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

fn scrub_html(input: &str, test_home: &Path) -> String {
    let mut out = canonicalize_html(input);

    let home = test_home.display().to_string();
    if !home.is_empty() {
        out = out.replace(&home, "[TEST_HOME]");
    }

    let version_re = Regex::new(r#"\bcass v?\d+\.\d+\.\d+([-.+][A-Za-z0-9.]+)?\b"#).unwrap();
    out = version_re.replace_all(&out, "cass [VERSION]").to_string();

    let iso_ts_re =
        Regex::new(r#"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:\d{2})?"#).unwrap();
    out = iso_ts_re.replace_all(&out, "[TIMESTAMP]").to_string();

    let uuid_re =
        Regex::new(r#"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}"#).unwrap();
    out = uuid_re.replace_all(&out, "[UUID]").to_string();

    let encrypted_escaped_re =
        Regex::new(r#"(&quot;)(salt|iv|ciphertext)(&quot;\s*:\s*&quot;)([^&]*)(&quot;)"#).unwrap();
    out = encrypted_escaped_re
        .replace_all(&out, |caps: &regex::Captures<'_>| {
            format!(
                "{}{}{}[SCRUBBED-{}]{}",
                &caps[1],
                &caps[2],
                &caps[3],
                caps[2].to_ascii_uppercase(),
                &caps[5]
            )
        })
        .to_string();

    let encrypted_raw_re = Regex::new(r#"(")(salt|iv|ciphertext)("\s*:\s*")([^"]*)(")"#).unwrap();
    encrypted_raw_re
        .replace_all(&out, |caps: &regex::Captures<'_>| {
            format!(
                "{}{}{}[SCRUBBED-{}]{}",
                &caps[1],
                &caps[2],
                &caps[3],
                caps[2].to_ascii_uppercase(),
                &caps[5]
            )
        })
        .to_string()
}

fn assert_golden(name: &str, actual: &str) {
    let golden_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join("html_export")
        .join(name);

    if dotenvy::var("UPDATE_GOLDENS").is_ok() {
        fs::create_dir_all(golden_path.parent().expect("golden parent"))
            .expect("create golden parent dir");
        fs::write(&golden_path, actual).expect("write golden file");
        eprintln!("[GOLDEN] Updated: {}", golden_path.display());
        return;
    }

    let expected = match fs::read_to_string(&golden_path) {
        Ok(expected) => expected,
        Err(err) => {
            let missing_golden = true;
            assert!(
                !missing_golden,
                "Golden file missing or unreadable: {}\n{err}\n\n\
             Run with UPDATE_GOLDENS=1 to create it, then review and commit:\n\
             \tUPDATE_GOLDENS=1 cargo test --test pages_export_golden\n\
             \tgit diff tests/golden/html_export/",
                golden_path.display(),
            );
            String::new()
        }
    };

    if actual != expected {
        let actual_path = golden_path.with_extension("actual");
        fs::write(&actual_path, actual).expect("write .actual file");
        assert!(
            actual == expected,
            "GOLDEN MISMATCH: {name}\n\n\
             Expected: {}\n\
             Actual:   {}\n\n\
             Review the diff, then fix the regression or regenerate intentionally.",
            golden_path.display(),
            actual_path.display(),
        );
    }
}

#[test]
fn basic_export_html_matches_golden() {
    let test_home = TempDir::new().expect("create temp home");
    let output_dir = TempDir::new().expect("create output dir");
    let session_path = write_fixture_session(test_home.path());

    let html = export_html(
        test_home.path(),
        &session_path,
        output_dir.path(),
        "basic_export.html",
        false,
    );

    assert!(html.starts_with("<!DOCTYPE html>"));
    assert!(html.contains("<meta charset=\"UTF-8\">"));
    assert!(html.contains("<style>"));
    assert!(html.contains("<script>"));
    assert!(html.contains("token refresh bug"));
    assert!(html.contains("message-user"));
    assert!(html.contains("message-assistant"));

    let scrubbed = scrub_html(&html, test_home.path());
    assert_golden("basic_export.html.golden", &scrubbed);
}

#[test]
fn encrypted_export_html_matches_golden() {
    let test_home = TempDir::new().expect("create temp home");
    let output_dir_a = TempDir::new().expect("create first output dir");
    let output_dir_b = TempDir::new().expect("create second output dir");
    let session_path = write_fixture_session(test_home.path());

    let first = export_html(
        test_home.path(),
        &session_path,
        output_dir_a.path(),
        "encrypted_export.html",
        true,
    );
    let second = export_html(
        test_home.path(),
        &session_path,
        output_dir_b.path(),
        "encrypted_export.html",
        true,
    );

    assert_eq!(
        first, second,
        "deterministic golden byte label must produce reproducible encrypted HTML"
    );
    assert!(first.contains("id=\"encrypted-content\""));
    assert!(first.contains("crypto.subtle"));
    assert!(!first.contains("return Err(AuthError::ExpiredToken);"));

    let scrubbed = scrub_html(&first, test_home.path());
    assert_golden("encrypted_export.html.golden", &scrubbed);
}
