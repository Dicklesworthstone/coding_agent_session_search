//! Integration tests verifying franken_agent_detection (FAD) completeness in cass.
//!
//! Bead: coding_agent_session_search-3arih
//!
//! These tests ensure:
//! 1. Zero hardcoded agent paths remain in production code (all come from FAD)
//! 2. Connector detection round-trip works correctly
//! 3. Probe script generation uses FAD paths dynamically
//! 4. Agent counts are consistent across all APIs
//! 5. Detection-only connectors (continue, windsurf) are properly handled

use coding_agent_search::indexer::get_connector_factories;
use std::collections::HashSet;
use std::path::Path;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Map FAD internal slug → cass public slug.
fn public_slug(fad_slug: &str) -> &str {
    match fad_slug {
        "claude" => "claude_code",
        other => other,
    }
}

/// Map cass public slug → FAD internal slug.
fn fad_slug(public: &str) -> &str {
    match public {
        "claude_code" => "claude",
        other => other,
    }
}

/// Collect factory slugs as the FAD internal names (e.g. "claude", "copilot").
fn factory_fad_slugs() -> HashSet<String> {
    get_connector_factories()
        .into_iter()
        .map(|(slug, _)| slug.to_string())
        .collect()
}

/// Collect probe path slugs from FAD.
fn probe_slugs() -> HashSet<String> {
    franken_agent_detection::default_probe_paths_tilde()
        .into_iter()
        .map(|(slug, _)| slug.to_string())
        .collect()
}

/// Detection-only connectors: have probe paths and detection entries but
/// no parser implementation (no entry in `get_connector_factories()`).
const DETECTION_ONLY: &[&str] = &["continue", "windsurf"];

/// Extract a function body from source code, including the braces.
fn extract_function_body(source: &str, fn_prefix: &str) -> String {
    let start = source
        .find(fn_prefix)
        .unwrap_or_else(|| panic!("function not found: {fn_prefix}"));
    let after = &source[start..];
    let open = after
        .find('{')
        .unwrap_or_else(|| panic!("no opening brace for: {fn_prefix}"));
    let mut depth = 0usize;
    let mut end_idx = None;
    for (i, ch) in after[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    end_idx = Some(open + i + 1);
                    break;
                }
            }
            _ => {}
        }
    }
    let end = end_idx.unwrap_or_else(|| panic!("no closing brace for: {fn_prefix}"));
    after[open..end].to_string()
}

// ---------------------------------------------------------------------------
// Test 1: Connector detection round-trip
// ---------------------------------------------------------------------------

/// Every connector factory must produce a valid connector that can run detect().
/// Root paths returned must be absolute or tilde-relative.
#[test]
fn connector_factories_all_instantiate_and_detect() {
    let factories = get_connector_factories();

    // Must have at least 12 base connectors
    assert!(
        factories.len() >= 12,
        "Expected >=12 connector factories, got {}",
        factories.len()
    );

    let mut slugs = Vec::new();
    for (slug, factory_fn) in &factories {
        let connector = factory_fn();
        let result = connector.detect();
        for root in &result.root_paths {
            let s = root.to_string_lossy();
            assert!(
                root.is_absolute() || s.starts_with("~/"),
                "connector {slug} returned non-absolute root path: {}",
                root.display()
            );
        }
        slugs.push(*slug);
        eprintln!(
            "  [OK] {slug}: detected={}, {} root path(s)",
            result.detected,
            result.root_paths.len()
        );
    }

    // No duplicate slugs
    let unique: HashSet<&str> = slugs.iter().copied().collect();
    assert_eq!(unique.len(), slugs.len(), "Duplicate factory slugs");

    // Required base connectors always present
    for required in [
        "codex",
        "cline",
        "gemini",
        "claude",
        "clawdbot",
        "vibe",
        "amp",
        "aider",
        "pi_agent",
        "factory",
        "omp",
        "openclaw",
        "copilot",
        "grok",
        "muse",
        "prime_agent",
        "kiro",
        "devin",
    ] {
        assert!(
            unique.contains(required),
            "Required base connector '{required}' missing"
        );
    }
}

/// Feature-gated connectors (chatgpt, cursor, opencode, crush, goose, hermes, devin)
/// are available because cass enables those features in Cargo.toml.
#[test]
fn feature_gated_connectors_available() {
    let slugs = factory_fad_slugs();
    for gated in [
        "chatgpt", "cursor", "opencode", "crush", "goose", "hermes", "devin",
    ] {
        assert!(
            slugs.contains(gated),
            "Feature-gated connector '{gated}' not found. \
             Check Cargo.toml enables the feature for franken-agent-detection"
        );
    }
    assert_eq!(slugs.len(), 29, "Expected 29 connector factories");
}

// ---------------------------------------------------------------------------
// Test 2: Probe path coverage
// ---------------------------------------------------------------------------

/// Every factory connector must have a corresponding probe path entry.
/// Detection-only connectors have probe paths but no factory.
#[test]
fn probe_paths_cover_all_factory_connectors() {
    let factory = factory_fad_slugs();
    let probes = probe_slugs();

    // Map factory slugs to their FAD probe slug equivalents.
    // Note: "copilot" factory slug maps to "github-copilot" in KNOWN_CONNECTORS.
    let factory_mapped: HashSet<String> = factory
        .iter()
        .map(|s| match s.as_str() {
            "copilot" => "github-copilot".to_string(),
            other => other.to_string(),
        })
        .collect();

    let missing: Vec<_> = factory_mapped.difference(&probes).cloned().collect();
    assert!(
        missing.is_empty(),
        "Factory connectors missing from probe paths: {missing:?}"
    );

    // Detection-only connectors must be in probes but NOT in factory
    for slug in DETECTION_ONLY {
        assert!(
            probes.contains(*slug),
            "Detection-only connector '{slug}' missing from probe paths"
        );
        assert!(
            !factory.contains(*slug),
            "Detection-only connector '{slug}' should NOT have a factory"
        );
    }

    eprintln!(
        "  Factory: {} connectors, Probes: {} entries, Detection-only: {}",
        factory.len(),
        probes.len(),
        DETECTION_ONLY.len()
    );
}

/// All probe paths must use tilde-relative format (suitable for SSH).
#[test]
fn probe_paths_are_tilde_relative() {
    let paths = franken_agent_detection::default_probe_paths_tilde();
    for (slug, paths) in &paths {
        if *slug == "shelley" {
            // A live SQLite database and WAL cannot yet be copied as a
            // consistent remote bundle (GH #415). Local detection remains
            // available, but advertising remote paths would be unsafe.
            assert!(
                paths.is_empty(),
                "Shelley remote probes must remain disabled"
            );
            continue;
        }
        assert!(!paths.is_empty(), "Connector '{slug}' has no probe paths");
        for path in paths {
            assert!(
                path.starts_with("~/"),
                "Probe path for '{slug}' is not tilde-relative: {path}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Test 3: detect_installed_agents API
// ---------------------------------------------------------------------------

/// detect_installed_agents must return a valid report covering all KNOWN_CONNECTORS.
#[test]
fn detect_installed_agents_report_structure() {
    let opts = franken_agent_detection::AgentDetectOptions {
        include_undetected: true,
        ..Default::default()
    };
    let report = franken_agent_detection::detect_installed_agents(&opts)
        .expect("detect_installed_agents should not fail");

    // Must cover the full KNOWN_CONNECTORS set.
    assert!(
        report.installed_agents.len() >= 15,
        "Expected >=15 agents in report, got {}",
        report.installed_agents.len()
    );
    assert_eq!(report.format_version, 1);
    // Bead 7k7pl: generated_at must be an ISO-8601-shaped timestamp,
    // not just any non-empty string. A regression that stored
    // `"unknown"` or a Unix-epoch integer as a string would slip past
    // `!is_empty()` while breaking downstream parsers that expect
    // RFC-3339. Check the canonical prefix shape: "YYYY-MM-DD" (10
    // chars, dashes at positions 4 and 7).
    assert!(
        report.generated_at.len() >= 10,
        "generated_at must be an ISO-8601 timestamp (>= 10 chars); got {:?}",
        report.generated_at
    );
    let bytes = report.generated_at.as_bytes();
    assert!(
        bytes[4] == b'-' && bytes[7] == b'-',
        "generated_at must have dashes at positions 4 and 7 (YYYY-MM-DD prefix); \
         got {:?}",
        report.generated_at
    );
    assert_eq!(report.summary.total_count, report.installed_agents.len());

    let slugs: HashSet<&str> = report
        .installed_agents
        .iter()
        .map(|e| e.slug.as_str())
        .collect();

    // Detection-only connectors must appear
    for slug in DETECTION_ONLY {
        assert!(
            slugs.contains(slug),
            "Detection-only connector '{slug}' missing from detection report"
        );
    }

    for entry in &report.installed_agents {
        assert!(!entry.slug.is_empty());
        eprintln!(
            "  [{}] {}: {} path(s)",
            if entry.detected { "YES" } else { " no" },
            entry.slug,
            entry.root_paths.len()
        );
    }
}

// ---------------------------------------------------------------------------
// Test 4: Agent count consistency
// ---------------------------------------------------------------------------

/// Detection and probe APIs should enumerate the same set of connectors.
/// Factory connectors are a subset (they exclude detection-only connectors).
#[test]
fn agent_counts_consistent_across_apis() {
    let factory = factory_fad_slugs();
    let probes = probe_slugs();

    let opts = franken_agent_detection::AgentDetectOptions {
        include_undetected: true,
        ..Default::default()
    };
    let report = franken_agent_detection::detect_installed_agents(&opts)
        .expect("detect_installed_agents should not fail");
    let detection: HashSet<String> = report
        .installed_agents
        .iter()
        .map(|e| e.slug.clone())
        .collect();

    // Detection and probe should enumerate the same slugs
    assert_eq!(
        detection.len(),
        probes.len(),
        "Detection ({}) and probe ({}) counts differ",
        detection.len(),
        probes.len()
    );

    // Factory must be a strict subset of detection (after slug mapping)
    let factory_mapped: HashSet<String> = factory
        .iter()
        .map(|s| match s.as_str() {
            "copilot" => "github-copilot".to_string(),
            other => other.to_string(),
        })
        .collect();
    for slug in &factory_mapped {
        assert!(
            detection.contains(slug),
            "Factory connector '{slug}' not in detection report"
        );
    }

    eprintln!(
        "  Factories: {}, Detection: {}, Probes: {}, Detection-only: {}",
        factory.len(),
        detection.len(),
        probes.len(),
        DETECTION_ONLY.len()
    );
}

// ---------------------------------------------------------------------------
// Test 5: Source code audit — no hardcoded paths
// ---------------------------------------------------------------------------

/// diagnostics_connector_paths() in lib.rs must use FAD's detect_installed_agents,
/// not hardcoded path lists.
#[test]
fn diagnostics_connector_paths_is_dynamic() {
    let src = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs"))
        .expect("should read src/lib.rs");
    let body = extract_function_body(&src, "fn diagnostics_connector_paths(");

    assert!(
        body.contains("detect_installed_agents"),
        "diagnostics_connector_paths should call detect_installed_agents"
    );
    for banned in [
        ".claude/projects",
        ".codex/sessions",
        ".gemini",
        ".goose/sessions",
        ".continue/sessions",
        "sourcegraph.amp",
        "saoudrizwan.claude-dev",
    ] {
        assert!(
            !body.contains(banned),
            "diagnostics_connector_paths still hardcodes: {banned}"
        );
    }
}

/// probe.rs build_probe_script() must source paths from FAD's
/// default_probe_paths_tilde(), not a hardcoded list.
#[test]
fn probe_script_uses_fad_api() {
    let src =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/sources/probe.rs"))
            .expect("should read src/sources/probe.rs");
    let body = extract_function_body(&src, "fn build_probe_script(");

    assert!(
        body.contains("default_probe_paths_tilde"),
        "build_probe_script should call default_probe_paths_tilde"
    );

    // The function should NOT contain hardcoded agent directory paths
    for banned in [
        "\".codex/sessions\"",
        "\".claude/projects\"",
        "\".gemini/tmp\"",
        "\".goose/sessions\"",
    ] {
        assert!(
            !body.contains(banned),
            "build_probe_script still hardcodes: {banned}"
        );
    }
}

// ---------------------------------------------------------------------------
// Test 6: Slug mapping consistency
// ---------------------------------------------------------------------------

/// FAD uses "claude" internally; cass exposes "claude_code" publicly.
/// FAD uses "copilot" for the factory; KNOWN_CONNECTORS uses "github-copilot".
#[test]
fn slug_mappings_are_correct() {
    let factory = factory_fad_slugs();

    // FAD factory uses "claude", not "claude_code"
    assert!(factory.contains("claude"));
    assert!(!factory.contains("claude_code"));
    assert_eq!(public_slug("claude"), "claude_code");
    assert_eq!(fad_slug("claude_code"), "claude");

    // FAD factory uses "copilot"
    assert!(factory.contains("copilot"));
}

// ---------------------------------------------------------------------------
// Test 7: New agent auto-discovery mechanism
// ---------------------------------------------------------------------------

/// Documents and verifies the auto-discovery integration points.
/// When a new connector is added to FAD, cass picks it up automatically via:
/// - get_connector_factories() (indexing)
/// - detect_installed_agents() (diagnostics)
/// - default_probe_paths_tilde() (SSH probing)
#[test]
fn new_agent_auto_discovery_documented() {
    let factories = get_connector_factories();
    let probes = franken_agent_detection::default_probe_paths_tilde();
    let report = franken_agent_detection::detect_installed_agents(
        &franken_agent_detection::AgentDetectOptions {
            include_undetected: true,
            ..Default::default()
        },
    )
    .expect("detection should work");

    assert!(!factories.is_empty());
    assert!(!probes.is_empty());
    assert!(!report.installed_agents.is_empty());

    eprintln!("\n  Auto-Discovery Verification:");
    eprintln!(
        "  - Factories: {} connectors (with parsers)",
        factories.len()
    );
    eprintln!("  - Probe paths: {} entries (all known)", probes.len());
    eprintln!(
        "  - Detection: {} entries ({} detected on this machine)",
        report.installed_agents.len(),
        report.summary.detected_count
    );
    eprintln!("  - Adding a connector to FAD auto-discovers in cass.");
}

/// GH449: the Devin factory exists even when its SQLite parser is compiled out.
/// Exercise the persisted provider format through the factory and real CLI so
/// slug enumeration alone cannot certify support again.
mod devin_ingestion {
    use super::*;
    use coding_agent_search::connectors::{ScanContext, ScanRoot};
    use coding_agent_search::franken_sync::compat::ConnectionExt;
    use coding_agent_search::franken_sync::{Connection, params};
    use serde_json::{Value, json};
    use std::fs;
    use std::time::Duration;

    fn seed_store(path: &Path) {
        let conn = Connection::open(path.to_string_lossy().as_ref()).expect("create Devin store");
        // Schema and JSON shapes from the published FAD 0.2.3 Devin connector,
        // independently populated here with branch, hidden and empty sessions.
        conn.execute_batch(
            "BEGIN;
             CREATE TABLE sessions (
                 id TEXT PRIMARY KEY, title TEXT, working_directory TEXT,
                 model TEXT, agent_mode TEXT, created_at REAL,
                 last_activity_at REAL, main_chain_id INTEGER, hidden INTEGER
             );
             CREATE TABLE message_nodes (
                 session_id TEXT, node_id INTEGER, parent_node_id INTEGER,
                 chat_message TEXT, created_at REAL,
                 PRIMARY KEY (session_id, node_id)
             );
             INSERT INTO sessions VALUES
                 ('kept', 'Devin branch repair', '/work/devin', 'model', 'agent',
                  1700000000, 1700000060, 5, 0),
                 ('hidden', 'Retired', NULL, NULL, NULL, 1700000000, 1700000060, 1, 1),
                 ('empty', NULL, NULL, NULL, NULL, NULL, NULL, NULL, 0);
             COMMIT;",
        )
        .expect("seed Devin schema");
        for (session, id, parent, message) in [
            (
                "kept",
                1,
                None,
                json!({"role":"system", "content":"excludedpolicy"}),
            ),
            (
                "kept",
                2,
                Some(1),
                json!({"role":"user", "content":"devinneedle fix the branch",
                "images":[{"data":"excludedimagepayload", "mime_type":"image/png"}]}),
            ),
            (
                "kept",
                3,
                Some(2),
                json!({"role":"assistant", "content":"devinneedle inspect",
                "thinking":{"thinking":"follow the parent chain", "signature":"signature"},
                "tool_calls":[{"id":"call-1", "index":0, "kind":"function", "name":"shell",
                    "arguments":{"command":"git status"}}]}),
            ),
            (
                "kept",
                4,
                Some(3),
                json!({"role":"tool", "content":"devinneedle clean tree", "tool_call_id":"call-1"}),
            ),
            (
                "kept",
                5,
                Some(4),
                json!({"role":"assistant", "content":"devinneedle repaired"}),
            ),
            (
                "kept",
                6,
                Some(2),
                json!({"role":"assistant", "content":"excludedabandonedbranch"}),
            ),
            (
                "hidden",
                1,
                None,
                json!({"role":"user", "content":"excludedhiddensession"}),
            ),
        ] {
            conn.execute_compat(
                "INSERT INTO message_nodes VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    session,
                    id,
                    parent,
                    message.to_string(),
                    1_700_000_000.0 + f64::from(id)
                ],
            )
            .expect("insert Devin message node");
        }
    }

    fn cass(home: &Path, data: &Path) -> assert_cmd::Command {
        let mut cmd = assert_cmd::Command::new(assert_cmd::cargo::cargo_bin!("cass"));
        cmd.env_clear()
            .env("HOME", home)
            .env("USERPROFILE", home)
            .env("PATH", "")
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("CASS_DEVIN_DATA_ROOT", home.join("sessions.db"))
            .env("CASS_IGNORE_SOURCES_CONFIG", "1")
            .env("CASS_AUTO_REFRESH", "0")
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .env("RUST_MIN_STACK", "134217728")
            .current_dir(home)
            .arg("--data-dir")
            .arg(data)
            .timeout(Duration::from_secs(120));
        if let Ok(system_root) = dotenvy::var("SystemRoot") {
            cmd.env("SystemRoot", system_root);
        }
        cmd
    }

    #[test]
    fn devin_factory_reads_main_chain_without_mutating_source() {
        let home = tempfile::tempdir().expect("isolated home");
        let db = home.path().join("sessions.db");
        seed_store(&db);
        let before = fs::read(&db).expect("source bytes");
        let (_, factory) = get_connector_factories()
            .into_iter()
            .find(|(slug, _)| *slug == "devin")
            .expect("Devin factory");
        let ctx = ScanContext::with_roots(
            home.path().to_path_buf(),
            vec![ScanRoot::local(db.clone())],
            None,
        );
        let conversations = factory().scan(&ctx).expect("real Devin scan");
        assert_eq!(
            conversations.len(),
            1,
            "disabled parser or wrong branch selection"
        );
        let conversation = &conversations[0];
        assert_eq!(conversation.agent_slug, "devin");
        assert_eq!(conversation.external_id.as_deref(), Some("kept"));
        assert_eq!(
            conversation.workspace.as_deref(),
            Some(Path::new("/work/devin"))
        );
        assert_eq!(conversation.source_path, db.join("kept"));
        assert_eq!(conversation.started_at, Some(1_700_000_000_000));
        assert_eq!(conversation.metadata["off_chain_nodes"], 1);
        assert_eq!(conversation.messages.len(), 4);
        assert_eq!(
            conversation
                .messages
                .iter()
                .map(|m| m.role.as_str())
                .collect::<Vec<_>>(),
            ["user", "assistant", "tool", "assistant"]
        );
        for message in &conversation.messages {
            assert!(!message.content.contains("excluded"));
        }
        assert_eq!(fs::read(&db).expect("source after scan"), before);
    }

    #[test]
    fn devin_cli_indexes_searches_and_reopens_without_duplicates() {
        let home = tempfile::tempdir().expect("isolated home");
        let db = home.path().join("sessions.db");
        let data = home.path().join("cass-data");
        seed_store(&db);
        let before = fs::read(&db).expect("source bytes");
        for _ in 0..2 {
            cass(home.path(), &data)
                .args(["index", "--full", "--json"])
                .assert()
                .success();
            let output = cass(home.path(), &data)
                .args([
                    "search",
                    "devinneedle",
                    "--mode",
                    "lexical",
                    "--agent",
                    "devin",
                    "--json",
                    "--limit",
                    "20",
                ])
                .assert()
                .success()
                .get_output()
                .stdout
                .clone();
            let result: Value = serde_json::from_slice(&output).expect("search JSON");
            let hits = result["hits"].as_array().expect("search hits");
            assert_eq!(hits.len(), 4, "{result}");
            for hit in hits {
                assert_eq!(hit["agent"], "devin");
                assert_eq!(
                    hit["source_path"],
                    db.join("kept").to_string_lossy().as_ref()
                );
            }
            assert_eq!(fs::read(&db).expect("source after indexing"), before);
        }
    }
}
