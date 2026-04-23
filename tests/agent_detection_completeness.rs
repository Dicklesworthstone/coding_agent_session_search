//! Integration tests verifying franken_agent_detection (FAD) completeness in cass.
//!
//! Bead: coding_agent_session_search-3arih
//!
//! These tests ensure:
//! 1. Zero hardcoded agent paths remain in production code (all come from FAD)
//! 2. Connector detection round-trip works correctly
//! 3. Probe script generation uses FAD paths dynamically
//! 4. Agent counts are consistent across all APIs
//! 5. Detection-only connectors (goose, continue, windsurf) are properly handled

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
const DETECTION_ONLY: &[&str] = &["goose", "continue", "windsurf"];

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
        "codex", "cline", "gemini", "claude", "clawdbot", "vibe", "amp", "aider", "pi_agent",
        "factory", "openclaw", "copilot",
    ] {
        assert!(
            unique.contains(required),
            "Required base connector '{required}' missing"
        );
    }
}

/// Feature-gated connectors (chatgpt, cursor, opencode, crush) are available
/// because cass enables all four features in Cargo.toml.
#[test]
fn feature_gated_connectors_available() {
    let slugs = factory_fad_slugs();
    for gated in ["chatgpt", "cursor", "opencode", "crush"] {
        assert!(
            slugs.contains(gated),
            "Feature-gated connector '{gated}' not found. \
             Check Cargo.toml enables the feature for franken-agent-detection"
        );
    }
    assert_eq!(slugs.len(), 19, "Expected 19 connector factories");
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
