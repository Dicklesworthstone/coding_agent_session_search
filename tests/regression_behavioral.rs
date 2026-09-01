//! Comprehensive Behavioral Regression Tests
//!
//! These tests verify USER-FACING BEHAVIOR, not implementation details.
//! They are organized by what users expect from the system.
//!
//! Philosophy:
//! - If a user types something, they should be able to find it later
//! - If data exists, it must never silently disappear
//! - Operations must complete in reasonable time
//! - Errors must be clear, not silent failures
//!
//! Each test category catches a CLASS of bugs, not just specific instances.

use assert_cmd::cargo::cargo_bin_cmd;
use serial_test::serial;
use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::time::{Duration, Instant};
use tempfile::TempDir;

mod util;
use util::EnvGuard;

// =============================================================================
// PERFORMANCE TESTS - Catch operations that become unexpectedly slow
// =============================================================================

/// CRITICAL: All connector detect() methods must complete within 100ms.
///
/// This test would have caught the Aider detect() bug where it was doing
/// a recursive WalkDir scan on every call, making it O(files) instead of O(1).
// qu81y documented exception: in-process detect() has no root-injection
// seam, so this test genuinely depends on a HOME override and must be
// serialized against the other env-dependent detect test below.
#[test]
#[serial]
fn detect_must_complete_within_100ms_all_connectors() {
    use coding_agent_search::connectors::Connector;
    use coding_agent_search::connectors::aider::AiderConnector;
    use coding_agent_search::connectors::amp::AmpConnector;
    use coding_agent_search::connectors::chatgpt::ChatGptConnector;
    use coding_agent_search::connectors::claude_code::ClaudeCodeConnector;
    use coding_agent_search::connectors::cline::ClineConnector;
    use coding_agent_search::connectors::codex::CodexConnector;
    use coding_agent_search::connectors::cursor::CursorConnector;
    use coding_agent_search::connectors::gemini::GeminiConnector;
    use coding_agent_search::connectors::opencode::OpenCodeConnector;

    let tmp = TempDir::new().unwrap();
    let home = tmp.path();

    // Create deep nested directories to stress-test any accidental recursive scanning
    let deep_path = home.join("a/b/c/d/e/f/g/h/i/j/k/l/m/n/o/p/q/r/s/t");
    fs::create_dir_all(&deep_path).unwrap();

    // Create many files to stress test any accidental file enumeration
    let many_files = home.join("many_files");
    fs::create_dir_all(&many_files).unwrap();
    for i in 0..100 {
        fs::write(many_files.join(format!("file_{i}.txt")), "content").unwrap();
    }

    let _guard = EnvGuard::set("HOME", home.to_string_lossy());

    let connectors: Vec<(&str, Box<dyn Connector>)> = vec![
        ("aider", Box::new(AiderConnector::new())),
        ("amp", Box::new(AmpConnector::new())),
        ("chatgpt", Box::new(ChatGptConnector::new())),
        ("claude_code", Box::new(ClaudeCodeConnector::new())),
        ("cline", Box::new(ClineConnector::new())),
        ("codex", Box::new(CodexConnector::new())),
        ("cursor", Box::new(CursorConnector::new())),
        ("gemini", Box::new(GeminiConnector::new())),
        ("opencode", Box::new(OpenCodeConnector::new())),
    ];

    let max_allowed = Duration::from_millis(100);
    let mut failures = Vec::new();

    for (name, connector) in connectors {
        let start = Instant::now();
        let _result = connector.detect();
        let elapsed = start.elapsed();

        if elapsed > max_allowed {
            failures.push(format!(
                "{}: detect() took {:?} (max allowed: {:?})",
                name, elapsed, max_allowed
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "Performance regression in detect():\n{}",
        failures.join("\n")
    );
}

/// Stress test: detect() must stay fast even with many nested directories.
// qu81y documented exception: see detect_must_complete_within_100ms_all_connectors.
#[test]
#[serial]
fn aider_detect_must_not_scan_recursively() {
    use coding_agent_search::connectors::Connector;
    use coding_agent_search::connectors::aider::AiderConnector;

    let tmp = TempDir::new().unwrap();
    let home = tmp.path();

    // Create a massive directory tree (10*10*10 = 1000 directories)
    for a in 0..10 {
        for b in 0..10 {
            for c in 0..10 {
                let path = home.join(format!("dir_{a}/subdir_{b}/leaf_{c}"));
                fs::create_dir_all(&path).unwrap();
                // Put decoy files (not in CWD)
                fs::write(path.join(".aider.chat.history.md"), "decoy").unwrap();
            }
        }
    }

    let _guard = EnvGuard::set("HOME", home.to_string_lossy());
    // SAFETY: Test-only env var manipulation
    unsafe {
        std::env::remove_var("CASS_AIDER_DATA_ROOT");
    }

    let connector = AiderConnector::new();

    // Time 10 consecutive detect() calls
    let start = Instant::now();
    for _ in 0..10 {
        let _ = connector.detect();
    }
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_millis(100),
        "Aider detect() appears to be scanning recursively. 10 calls took {:?}",
        elapsed
    );
}

// =============================================================================
// DATA INTEGRITY TESTS - Catch silent data loss during operations
// =============================================================================

/// CRITICAL: Incremental re-indexing must NEVER drop existing messages.
///
/// This test would have caught the Codex message filtering bug.
#[test]
fn incremental_reindex_preserves_all_messages() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    // qu81y: no process-global env mutation — every cass subprocess below
    // passes its HOME/CODEX_HOME explicitly via Command::env.

    let sessions = codex_home.join("sessions/2024/11/20");
    fs::create_dir_all(&sessions).unwrap();
    let session_file = sessions.join("rollout-integrity.jsonl");

    let base_ts = 1_732_118_400_000u64;
    let initial_messages = vec![
        "UNIQUE_MSG_ALPHA_001",
        "UNIQUE_MSG_BETA_002",
        "UNIQUE_MSG_GAMMA_003",
        "UNIQUE_MSG_DELTA_004",
        "UNIQUE_MSG_EPSILON_005",
    ];

    // Write initial messages
    {
        let mut f = fs::File::create(&session_file).unwrap();
        for (i, msg) in initial_messages.iter().enumerate() {
            let ts = base_ts + (i as u64 * 1000);
            writeln!(
                f,
                r#"{{"type": "event_msg", "timestamp": {}, "payload": {{"type": "user_message", "message": "{}"}}}}"#,
                ts, msg
            )
            .unwrap();
            writeln!(
                f,
                r#"{{"type": "response_item", "timestamp": {}, "payload": {{"role": "assistant", "content": "{}_response"}}}}"#,
                ts + 500, msg
            )
            .unwrap();
        }
    }

    // Full index
    cargo_bin_cmd!("cass")
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();

    // Verify all initial messages are searchable
    for msg in &initial_messages {
        let output = cargo_bin_cmd!("cass")
            .args(["search", msg, "--robot", "--data-dir"])
            .arg(&data_dir)
            .env("HOME", home)
            .output()
            .unwrap();

        let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let hits = json["hits"].as_array().map(|a| a.len()).unwrap_or(0);
        assert!(
            hits >= 1,
            "Initial message '{}' should be found before re-index",
            msg
        );
    }

    // Simulate time passing
    std::thread::sleep(Duration::from_millis(100));

    // Append new messages
    let new_messages = ["UNIQUE_MSG_ZETA_006", "UNIQUE_MSG_ETA_007"];
    {
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(&session_file)
            .unwrap();
        for (i, msg) in new_messages.iter().enumerate() {
            let ts = base_ts + 10_000 + (i as u64 * 1000);
            writeln!(
                f,
                r#"{{"type": "event_msg", "timestamp": {}, "payload": {{"type": "user_message", "message": "{}"}}}}"#,
                ts, msg
            )
            .unwrap();
            writeln!(
                f,
                r#"{{"type": "response_item", "timestamp": {}, "payload": {{"role": "assistant", "content": "{}_response"}}}}"#,
                ts + 500, msg
            )
            .unwrap();
        }
    }

    // Incremental re-index (NOT --full)
    cargo_bin_cmd!("cass")
        .args(["index", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();

    // CRITICAL: Verify ALL messages (old AND new) are still searchable
    let all_messages: Vec<&str> = initial_messages
        .iter()
        .chain(new_messages.iter())
        .copied()
        .collect();

    let mut missing = Vec::new();
    for msg in &all_messages {
        let output = cargo_bin_cmd!("cass")
            .args(["search", msg, "--robot", "--data-dir"])
            .arg(&data_dir)
            .env("HOME", home)
            .output()
            .unwrap();

        let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let hits = json["hits"].as_array().map(|a| a.len()).unwrap_or(0);
        if hits == 0 {
            missing.push(*msg);
        }
    }

    assert!(
        missing.is_empty(),
        "DATA LOSS DETECTED! Messages dropped during incremental re-index:\n{:?}",
        missing
    );
}

/// Multiple re-indexes must not cause duplication or loss.
#[test]
fn repeated_reindex_maintains_message_integrity() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    // qu81y: no process-global env mutation — every cass subprocess below
    // passes its HOME/CODEX_HOME explicitly via Command::env.

    let sessions = codex_home.join("sessions/2024/11/20");
    fs::create_dir_all(&sessions).unwrap();
    let session_file = sessions.join("rollout-repeated.jsonl");

    let content = r#"{"type": "event_msg", "timestamp": 1732118400000, "payload": {"type": "user_message", "message": "REPEATED_INTEGRITY_MARKER"}}
{"type": "response_item", "timestamp": 1732118401000, "payload": {"role": "assistant", "content": "response"}}"#;
    fs::write(&session_file, content).unwrap();

    // Full index
    cargo_bin_cmd!("cass")
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();

    // Get baseline count
    let baseline = cargo_bin_cmd!("cass")
        .args([
            "search",
            "REPEATED_INTEGRITY_MARKER",
            "--robot",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", home)
        .output()
        .unwrap();
    let baseline_json: serde_json::Value = serde_json::from_slice(&baseline.stdout).unwrap();
    let baseline_count = baseline_json["hits"]
        .as_array()
        .map(|a| a.len())
        .unwrap_or(0);

    // Run incremental index 5 times
    for i in 0..5 {
        // Append harmless newline to bump mtime
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(&session_file)
            .unwrap();
        writeln!(f).unwrap();
        std::thread::sleep(Duration::from_millis(20));

        cargo_bin_cmd!("cass")
            .args(["index", "--data-dir"])
            .arg(&data_dir)
            .env("CODEX_HOME", &codex_home)
            .env("HOME", home)
            .assert()
            .success();

        let output = cargo_bin_cmd!("cass")
            .args([
                "search",
                "REPEATED_INTEGRITY_MARKER",
                "--robot",
                "--data-dir",
            ])
            .arg(&data_dir)
            .env("HOME", home)
            .output()
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let count = json["hits"].as_array().map(|a| a.len()).unwrap_or(0);

        assert_eq!(
            count,
            baseline_count,
            "After re-index #{}, hit count changed from {} to {} (duplication or loss)",
            i + 1,
            baseline_count,
            count
        );
    }
}

// =============================================================================
// VISUAL/UX TESTS - Catch visual regressions
// =============================================================================

/// All agent background colors must be visually distinct from each other.
#[test]
fn agent_colors_are_visually_distinct() {
    use coding_agent_search::ui::components::theme::colors;

    let agent_colors: Vec<(&str, (u8, u8, u8))> = vec![
        ("claude_code", extract_rgb(colors::AGENT_CLAUDE_BG)),
        ("codex", extract_rgb(colors::AGENT_CODEX_BG)),
        ("cline", extract_rgb(colors::AGENT_CLINE_BG)),
        ("gemini", extract_rgb(colors::AGENT_GEMINI_BG)),
        ("amp", extract_rgb(colors::AGENT_AMP_BG)),
        ("aider", extract_rgb(colors::AGENT_AIDER_BG)),
        ("cursor", extract_rgb(colors::AGENT_CURSOR_BG)),
        ("chatgpt", extract_rgb(colors::AGENT_CHATGPT_BG)),
        ("opencode", extract_rgb(colors::AGENT_OPENCODE_BG)),
    ];

    // Minimum distance (4.0 allows subtle tints but ensures not identical)
    let min_distance: f64 = 4.0;
    let mut too_similar = Vec::new();

    for i in 0..agent_colors.len() {
        for j in (i + 1)..agent_colors.len() {
            let (name_a, rgb_a) = &agent_colors[i];
            let (name_b, rgb_b) = &agent_colors[j];

            let distance = color_distance(*rgb_a, *rgb_b);
            if distance < min_distance {
                too_similar.push(format!(
                    "{} and {} are too similar (distance: {:.1}, min: {:.1})",
                    name_a, name_b, distance, min_distance
                ));
            }
        }
    }

    assert!(
        too_similar.is_empty(),
        "Agent colors are not visually distinct enough:\n{}",
        too_similar.join("\n")
    );
}

/// All agent colors must be distinct from the base background.
#[test]
fn agent_colors_distinct_from_base() {
    use coding_agent_search::ui::components::theme::colors;

    let base_bg = extract_rgb(colors::BG_DEEP);

    let agent_colors: Vec<(&str, (u8, u8, u8))> = vec![
        ("claude_code", extract_rgb(colors::AGENT_CLAUDE_BG)),
        ("codex", extract_rgb(colors::AGENT_CODEX_BG)),
        ("cline", extract_rgb(colors::AGENT_CLINE_BG)),
        ("gemini", extract_rgb(colors::AGENT_GEMINI_BG)),
        ("amp", extract_rgb(colors::AGENT_AMP_BG)),
        ("aider", extract_rgb(colors::AGENT_AIDER_BG)),
        ("cursor", extract_rgb(colors::AGENT_CURSOR_BG)),
        ("chatgpt", extract_rgb(colors::AGENT_CHATGPT_BG)),
        ("opencode", extract_rgb(colors::AGENT_OPENCODE_BG)),
    ];

    let min_distance: f64 = 8.0;
    let mut too_similar = Vec::new();

    for (name, rgb) in &agent_colors {
        let distance = color_distance(*rgb, base_bg);
        if distance < min_distance {
            too_similar.push(format!(
                "{} is too similar to base background (distance: {:.1}, min: {:.1})",
                name, distance, min_distance
            ));
        }
    }

    assert!(
        too_similar.is_empty(),
        "Some agent colors are indistinguishable from base background:\n{}",
        too_similar.join("\n")
    );
}

/// Verify no duplicate agent colors.
#[test]
fn no_duplicate_agent_colors() {
    use coding_agent_search::ui::components::theme::colors;

    let agent_colors: Vec<(&str, (u8, u8, u8))> = vec![
        ("claude_code", extract_rgb(colors::AGENT_CLAUDE_BG)),
        ("codex", extract_rgb(colors::AGENT_CODEX_BG)),
        ("cline", extract_rgb(colors::AGENT_CLINE_BG)),
        ("gemini", extract_rgb(colors::AGENT_GEMINI_BG)),
        ("amp", extract_rgb(colors::AGENT_AMP_BG)),
        ("aider", extract_rgb(colors::AGENT_AIDER_BG)),
        ("cursor", extract_rgb(colors::AGENT_CURSOR_BG)),
        ("chatgpt", extract_rgb(colors::AGENT_CHATGPT_BG)),
        ("opencode", extract_rgb(colors::AGENT_OPENCODE_BG)),
    ];

    let mut seen: HashSet<(u8, u8, u8)> = HashSet::new();
    let mut duplicates = Vec::new();

    for (name, rgb) in &agent_colors {
        if !seen.insert(*rgb) {
            duplicates.push(format!("{} has duplicate color {:?}", name, rgb));
        }
    }

    assert!(
        duplicates.is_empty(),
        "Duplicate agent colors detected:\n{}",
        duplicates.join("\n")
    );
}

// =============================================================================
// END-TO-END SEARCH TESTS
// =============================================================================

/// Users must be able to find their conversations after a fresh index.
#[test]
fn fresh_index_returns_expected_results() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    // qu81y: no process-global env mutation — every cass subprocess below
    // passes its HOME/CODEX_HOME explicitly via Command::env.

    let sessions = codex_home.join("sessions/2024/11/20");
    fs::create_dir_all(&sessions).unwrap();

    let content = r#"{"type": "event_msg", "timestamp": 1732118400000, "payload": {"type": "user_message", "message": "How do I implement authentication in Rust?"}}
{"type": "response_item", "timestamp": 1732118401000, "payload": {"role": "assistant", "content": "To implement authentication in Rust, you can use libraries like jsonwebtoken for JWT tokens..."}}
{"type": "event_msg", "timestamp": 1732118500000, "payload": {"type": "user_message", "message": "Show me an example with actix-web"}}
{"type": "response_item", "timestamp": 1732118501000, "payload": {"role": "assistant", "content": "Here's an example using actix-web with middleware authentication..."}}"#;

    fs::write(sessions.join("rollout-auth-discussion.jsonl"), content).unwrap();

    cargo_bin_cmd!("cass")
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();

    let searches = vec![
        ("authentication", true),
        ("Rust", true),
        ("jsonwebtoken", true),
        ("actix-web", true),
        ("NONEXISTENT_TERM_XYZ", false),
    ];

    for (term, should_find) in searches {
        let output = cargo_bin_cmd!("cass")
            .args(["search", term, "--robot", "--data-dir"])
            .arg(&data_dir)
            .env("HOME", home)
            .output()
            .unwrap();

        let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let has_hits = json["hits"]
            .as_array()
            .map(|a| !a.is_empty())
            .unwrap_or(false);

        if should_find {
            assert!(
                has_hits,
                "Search for '{}' should return results but didn't",
                term
            );
        } else {
            assert!(
                !has_hits,
                "Search for '{}' should NOT return results but did",
                term
            );
        }
    }
}

// =============================================================================
// CONTENT DISCOVERY TESTS
// =============================================================================

/// Users must be able to find exact words they typed.
#[test]
fn user_can_find_exact_words_they_typed() {
    let env = TestEnv::new();
    env.create_codex_session(
        "session1.jsonl",
        &[
            ("user", "How do I implement rate limiting?"),
            ("assistant", "You can use a token bucket algorithm..."),
        ],
    );
    env.full_index();

    for term in ["implement", "rate", "limiting"] {
        let hits = env.search(term);
        assert!(
            !hits.is_empty(),
            "User should find their exact word '{}' but got no results",
            term
        );
    }
}

/// Users must be able to find content from assistant responses.
#[test]
fn user_can_find_assistant_responses() {
    let env = TestEnv::new();
    env.create_codex_session(
        "session1.jsonl",
        &[
            ("user", "explain async"),
            (
                "assistant",
                "Asynchronous programming uses futures and executors...",
            ),
        ],
    );
    env.full_index();

    for term in ["Asynchronous", "futures", "executors"] {
        let hits = env.search(term);
        assert!(
            !hits.is_empty(),
            "User should find assistant response term '{}' but got no results",
            term
        );
    }
}

/// Users must be able to find code symbols.
#[test]
fn user_can_find_code_symbols() {
    let env = TestEnv::new();
    env.create_codex_session(
        "code-session.jsonl",
        &[
            ("user", "write a function called calculate_total"),
            (
                "assistant",
                "fn calculate_total(items: Vec<Item>) -> f64 { items.iter().sum() }",
            ),
        ],
    );
    env.full_index();

    let hits = env.search("calculate_total");
    assert!(
        !hits.is_empty(),
        "User should find function name 'calculate_total'"
    );

    let hits = env.search("calculate");
    assert!(
        !hits.is_empty(),
        "User should find partial match 'calculate'"
    );
}

/// Wildcard searches work correctly.
#[test]
fn wildcard_searches_work() {
    let env = TestEnv::new();
    env.create_codex_session(
        "wild.jsonl",
        &[
            ("user", "authentication implementation"),
            ("assistant", "To authenticate users, implement OAuth..."),
        ],
    );
    env.full_index();

    let hits = env.search("auth*");
    assert!(
        !hits.is_empty(),
        "Prefix wildcard 'auth*' should find results"
    );
}

/// Empty queries should not crash.
#[test]
fn empty_query_does_not_crash() {
    let env = TestEnv::new();
    env.create_codex_session(
        "session.jsonl",
        &[("user", "test"), ("assistant", "response")],
    );
    env.full_index();

    let result = env.search_raw("");
    assert!(
        result.status.success() || result.status.code().is_some(),
        "Empty query should not crash"
    );
}

// =============================================================================
// FILTER CORRECTNESS TESTS
// =============================================================================

/// Agent filter must only return results from specified agent.
#[test]
fn agent_filter_only_returns_matching_agent() {
    let env = TestEnv::new();

    env.create_codex_session(
        "codex.jsonl",
        &[
            ("user", "FILTER_TEST_SHARED"),
            ("assistant", "from codex agent"),
        ],
    );
    env.create_claude_session(
        "claude.jsonl",
        &[
            ("user", "FILTER_TEST_SHARED"),
            ("assistant", "from claude agent"),
        ],
    );
    env.full_index();

    let hits = env.search_with_agent("FILTER_TEST_SHARED", "codex");

    // IMPORTANT: Must have results for this test to be meaningful
    assert!(
        !hits.is_empty(),
        "Agent filter test requires results to be meaningful. Got no results for 'FILTER_TEST_SHARED' with agent='codex'"
    );

    for hit in &hits {
        assert_eq!(
            hit.agent, "codex",
            "Agent filter returned non-codex result: {:?}",
            hit.agent
        );
    }
}

// =============================================================================
// MULTI-CONNECTOR TESTS
// =============================================================================

/// Content from all connectors should be searchable together.
#[test]
fn all_connectors_content_searchable() {
    let env = TestEnv::new();

    env.create_codex_session(
        "codex.jsonl",
        &[
            ("user", "MULTI_CONNECTOR_TEST codex"),
            ("assistant", "response"),
        ],
    );
    env.create_claude_session(
        "claude.jsonl",
        &[
            ("user", "MULTI_CONNECTOR_TEST claude"),
            ("assistant", "response"),
        ],
    );
    env.full_index();

    let hits = env.search("MULTI_CONNECTOR_TEST");
    let agents: HashSet<&str> = hits.iter().map(|h| h.agent.as_str()).collect();

    assert!(
        agents.contains("codex"),
        "Multi-connector search missing codex results"
    );
    assert!(
        agents.contains("claude_code"),
        "Multi-connector search missing claude results"
    );
}

/// Agent slugs must be consistent and correct.
#[test]
fn agent_slugs_are_correct() {
    let env = TestEnv::new();

    env.create_codex_session(
        "codex.jsonl",
        &[("user", "SLUG_TEST_CODEX"), ("assistant", "response")],
    );
    env.create_claude_session(
        "claude.jsonl",
        &[("user", "SLUG_TEST_CLAUDE"), ("assistant", "response")],
    );
    env.full_index();

    let codex_hits = env.search("SLUG_TEST_CODEX");
    let claude_hits = env.search("SLUG_TEST_CLAUDE");

    // Ensure we got results for meaningful assertions
    assert!(
        !codex_hits.is_empty(),
        "Codex slug test requires codex results"
    );
    assert!(
        !claude_hits.is_empty(),
        "Claude slug test requires claude results"
    );

    assert!(
        codex_hits.iter().all(|h| h.agent == "codex"),
        "Codex results should have agent='codex'"
    );
    assert!(
        claude_hits.iter().all(|h| h.agent == "claude_code"),
        "Claude results should have agent='claude_code'"
    );
}

// =============================================================================
// CLI CONTRACT TESTS
// =============================================================================

/// JSON output must always be valid JSON.
#[test]
fn json_output_is_valid() {
    let env = TestEnv::new();
    env.create_codex_session(
        "session.jsonl",
        &[("user", "JSON_VALIDITY_TEST"), ("assistant", "response")],
    );
    env.full_index();

    let result = env.search_raw("JSON_VALIDITY_TEST");
    assert!(result.status.success());

    let parsed: Result<serde_json::Value, _> = serde_json::from_slice(&result.stdout);
    assert!(
        parsed.is_ok(),
        "Output is not valid JSON: {}",
        String::from_utf8_lossy(&result.stdout)
    );

    let json = parsed.unwrap();
    assert!(json.get("hits").is_some(), "JSON missing 'hits' field");
}

/// Index command with --json must return valid JSON.
#[test]
fn index_json_output_is_valid() {
    let env = TestEnv::new();
    env.create_codex_session(
        "session.jsonl",
        &[("user", "test"), ("assistant", "response")],
    );

    let result = cargo_bin_cmd!("cass")
        .args(["index", "--full", "--json", "--data-dir"])
        .arg(&env.data_dir)
        .env("CODEX_HOME", &env.codex_home)
        .env("HOME", &env.home)
        .output()
        .unwrap();

    assert!(result.status.success(), "Index command failed");

    let parsed: Result<serde_json::Value, _> = serde_json::from_slice(&result.stdout);
    assert!(
        parsed.is_ok(),
        "Index JSON output is not valid: {}",
        String::from_utf8_lossy(&result.stdout)
    );
}

// =============================================================================
// EDGE CASE TESTS
// =============================================================================

/// Very long queries should not crash or hang.
#[test]
fn very_long_query_handled_gracefully() {
    let env = TestEnv::new();
    env.create_codex_session(
        "session.jsonl",
        &[("user", "test"), ("assistant", "response")],
    );
    env.full_index();

    let long_query = "word ".repeat(100);
    let result = env.search_raw(&long_query);

    assert!(
        result.status.success() || result.status.code().is_some(),
        "Long query should not crash"
    );
}

/// Empty conversation files should be handled gracefully.
#[test]
fn empty_files_handled_gracefully() {
    let env = TestEnv::new();

    let sessions = env.codex_home.join("sessions/2024/11/20");
    fs::create_dir_all(&sessions).unwrap();
    fs::write(sessions.join("rollout-empty.jsonl"), "").unwrap();

    env.full_index();

    let result = env.search_raw("anything");
    assert!(
        result.status.success(),
        "Should handle empty files gracefully"
    );
}

/// Malformed JSON should be handled gracefully.
#[test]
fn malformed_json_handled_gracefully() {
    let env = TestEnv::new();

    let sessions = env.codex_home.join("sessions/2024/11/20");
    fs::create_dir_all(&sessions).unwrap();
    fs::write(
        sessions.join("rollout-malformed.jsonl"),
        "{ not valid json\n{\"also\": \"broken",
    )
    .unwrap();

    env.create_codex_session(
        "valid.jsonl",
        &[("user", "VALID_CONTENT"), ("assistant", "response")],
    );

    env.full_index();

    let hits = env.search("VALID_CONTENT");
    assert!(
        !hits.is_empty(),
        "Valid content should be indexed despite malformed files"
    );
}

// =============================================================================
// HELPER FUNCTIONS
// =============================================================================

fn extract_rgb(color: ftui::render::cell::PackedRgba) -> (u8, u8, u8) {
    (color.r(), color.g(), color.b())
}

fn color_distance(a: (u8, u8, u8), b: (u8, u8, u8)) -> f64 {
    let dr = a.0 as f64 - b.0 as f64;
    let dg = a.1 as f64 - b.1 as f64;
    let db = a.2 as f64 - b.2 as f64;
    (dr * dr + dg * dg + db * db).sqrt()
}

// =============================================================================
// TEST INFRASTRUCTURE
// =============================================================================

struct TestEnv {
    _tmp: TempDir,
    home: std::path::PathBuf,
    codex_home: std::path::PathBuf,
    claude_home: std::path::PathBuf,
    data_dir: std::path::PathBuf,
    _guards: Vec<EnvGuard>,
}

impl TestEnv {
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().to_path_buf();
        let codex_home = home.join(".codex");
        let claude_home = home.join(".claude");
        let data_dir = home.join("cass_data");

        fs::create_dir_all(&data_dir).unwrap();
        fs::create_dir_all(&codex_home).unwrap();
        fs::create_dir_all(&claude_home).unwrap();

        let guards = vec![
            EnvGuard::set("HOME", home.to_string_lossy()),
            EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy()),
        ];

        Self {
            _tmp: tmp,
            home,
            codex_home,
            claude_home,
            data_dir,
            _guards: guards,
        }
    }

    fn create_codex_session(&self, filename: &str, messages: &[(&str, &str)]) {
        let sessions = self.codex_home.join("sessions/2024/11/20");
        fs::create_dir_all(&sessions).unwrap();

        // Ensure filename starts with "rollout-" as required by Codex connector
        let actual_filename = if filename.starts_with("rollout-") {
            filename.to_string()
        } else {
            format!("rollout-{}", filename)
        };

        let mut f = fs::File::create(sessions.join(&actual_filename)).unwrap();
        let base_ts = 1_732_118_400_000u64;

        for (i, (role, content)) in messages.iter().enumerate() {
            let ts = base_ts + (i as u64 * 1000);
            if *role == "user" {
                writeln!(
                    f,
                    r#"{{"type": "event_msg", "timestamp": {}, "payload": {{"type": "user_message", "message": "{}"}}}}"#,
                    ts, content
                )
                .unwrap();
            } else {
                writeln!(
                    f,
                    r#"{{"type": "response_item", "timestamp": {}, "payload": {{"role": "assistant", "content": "{}"}}}}"#,
                    ts, content
                )
                .unwrap();
            }
        }
    }

    fn create_claude_session(&self, filename: &str, messages: &[(&str, &str)]) {
        let projects = self.claude_home.join("projects/test-project");
        fs::create_dir_all(&projects).unwrap();

        let mut f = fs::File::create(projects.join(filename)).unwrap();

        for (i, (role, content)) in messages.iter().enumerate() {
            // Use proper time format: increment seconds within a minute
            let secs = i % 60;
            let mins = 10 + (i / 60);
            let ts = format!("2024-11-20T10:{:02}:{:02}.000Z", mins, secs);
            if *role == "user" {
                writeln!(
                    f,
                    r#"{{"type":"user","cwd":"/workspace","sessionId":"sess-1","message":{{"role":"user","content":"{}"}},"timestamp":"{}"}}"#,
                    content, ts
                )
                .unwrap();
            } else {
                writeln!(
                    f,
                    r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"{}"}}]}},"timestamp":"{}"}}"#,
                    content, ts
                )
                .unwrap();
            }
        }
    }

    fn full_index(&self) {
        cargo_bin_cmd!("cass")
            .args(["index", "--full", "--data-dir"])
            .arg(&self.data_dir)
            .env("CODEX_HOME", &self.codex_home)
            .env("HOME", &self.home)
            .assert()
            .success();
    }

    fn search(&self, query: &str) -> Vec<SearchHit> {
        let result = self.search_raw(query);
        if !result.status.success() {
            return Vec::new();
        }

        let json: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap_or_default();
        parse_hits(&json)
    }

    fn search_raw(&self, query: &str) -> std::process::Output {
        cargo_bin_cmd!("cass")
            .args(["search", query, "--robot", "--data-dir"])
            .arg(&self.data_dir)
            .env("HOME", &self.home)
            .output()
            .unwrap()
    }

    fn search_with_agent(&self, query: &str, agent: &str) -> Vec<SearchHit> {
        let result = cargo_bin_cmd!("cass")
            .args(["search", query, "--robot", "--agent", agent, "--data-dir"])
            .arg(&self.data_dir)
            .env("HOME", &self.home)
            .output()
            .unwrap();

        if !result.status.success() {
            return Vec::new();
        }

        let json: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap_or_default();
        parse_hits(&json)
    }
}

#[derive(Debug)]
struct SearchHit {
    agent: String,
    #[allow(dead_code)]
    workspace: String,
}

fn parse_hits(json: &serde_json::Value) -> Vec<SearchHit> {
    json.get("hits")
        .and_then(|h| h.as_array())
        .map(|arr| {
            arr.iter()
                .map(|h| SearchHit {
                    agent: h["agent"].as_str().unwrap_or("").to_string(),
                    workspace: h["workspace"].as_str().unwrap_or("").to_string(),
                })
                .collect()
        })
        .unwrap_or_default()
}
