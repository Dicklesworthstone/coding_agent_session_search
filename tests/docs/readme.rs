//! README documentation accuracy tests.
//!
//! These tests verify that generated README.md content accurately reflects
//! the actual archive data and system configuration.
//!
//! Run with:
//!   cargo test --test docs

use chrono::{TimeZone, Utc};
use coding_agent_search::pages::docs::{DocConfig, DocLocation, DocumentationGenerator};
use coding_agent_search::pages::summary::{
    AgentSummaryItem, KeySlotSummary, KeySlotType, PrePublishSummary, ScanReportSummary,
};
use regex::Regex;

// =============================================================================
// Test Helpers
// =============================================================================

/// Create a test summary with specified parameters.
fn create_test_summary(conversation_count: usize, agents: Vec<(&str, usize)>) -> PrePublishSummary {
    let earliest = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let latest = Utc.with_ymd_and_hms(2024, 12, 31, 23, 59, 59).unwrap();

    let total_agent_convs: usize = agents.iter().map(|(_, c)| c).sum();

    PrePublishSummary {
        total_conversations: conversation_count,
        total_messages: conversation_count * 20,
        total_characters: conversation_count * 5000,
        estimated_size_bytes: conversation_count * 1000,
        earliest_timestamp: Some(earliest),
        latest_timestamp: Some(latest),
        date_histogram: Vec::new(),
        workspaces: Vec::new(),
        agents: agents
            .into_iter()
            .map(|(name, count)| AgentSummaryItem {
                name: name.to_string(),
                conversation_count: count,
                message_count: count * 20,
                percentage: if total_agent_convs > 0 {
                    (count as f64 / total_agent_convs as f64) * 100.0
                } else {
                    0.0
                },
                included: true,
            })
            .collect(),
        key_slots: vec![KeySlotSummary {
            slot_index: 0,
            slot_type: KeySlotType::Password,
            hint: None,
            created_at: Some(Utc::now()),
        }],
        secret_scan: ScanReportSummary {
            total_findings: 0,
            by_severity: Default::default(),
            has_critical: false,
            truncated: false,
            status_message: "No secrets found".to_string(),
        },
        encryption_config: None,
        generated_at: Utc::now(),
    }
}

/// Parse conversation count from README content.
#[allow(dead_code)]
fn parse_conversation_count(readme: &str) -> Option<usize> {
    // Look for pattern like "123 conversations" or "Total conversations: 123"
    let re = Regex::new(r"(\d+)\s+conversations?|conversations?[:\s]+(\d+)").ok()?;
    if let Some(caps) = re.captures(readme) {
        caps.get(1)
            .or_else(|| caps.get(2))
            .and_then(|m| m.as_str().parse().ok())
    } else {
        None
    }
}

/// Parse date range from README content.
#[allow(dead_code)]
fn parse_date_range(readme: &str) -> Option<(String, String)> {
    // Look for date patterns like "2024-01-01 to 2024-12-31"
    let re = Regex::new(r"(\d{4}-\d{2}-\d{2})[^0-9]+(\d{4}-\d{2}-\d{2})").ok()?;
    re.captures(readme)
        .map(|caps| (caps[1].to_string(), caps[2].to_string()))
}

/// Check if an agent is listed in the README.
fn agent_listed(readme: &str, agent_name: &str) -> bool {
    readme.contains(agent_name)
}

// =============================================================================
// README Accuracy Tests
// =============================================================================

/// Verify conversation count in README matches summary.
#[test]
fn test_readme_conversation_count_accurate() {
    let summary = create_test_summary(1234, vec![("Claude", 500), ("GPT-4", 734)]);
    let config = DocConfig::new().with_url("https://example.com/archive");

    let generator = DocumentationGenerator::new(config, summary);
    let readme_doc = generator.generate_readme();

    assert_eq!(readme_doc.filename, "README.md");
    assert_eq!(readme_doc.location, DocLocation::RepoRoot);

    // Verify the conversation count appears in the README
    assert!(
        readme_doc.content.contains("1234") || readme_doc.content.contains("1,234"),
        "README should contain the conversation count (1234)"
    );
}

/// Verify all agents are listed in README.
#[test]
fn test_readme_agents_listed() {
    let summary = create_test_summary(
        100,
        vec![
            ("Claude Code", 30),
            ("GitHub Copilot", 25),
            ("Cursor", 20),
            ("Gemini", 15),
            ("ChatGPT", 10),
        ],
    );
    let config = DocConfig::new();

    let generator = DocumentationGenerator::new(config, summary);
    let readme_doc = generator.generate_readme();

    assert!(
        agent_listed(&readme_doc.content, "Claude Code"),
        "Claude Code should be listed"
    );
    assert!(
        agent_listed(&readme_doc.content, "GitHub Copilot"),
        "GitHub Copilot should be listed"
    );
    assert!(
        agent_listed(&readme_doc.content, "Cursor"),
        "Cursor should be listed"
    );
    assert!(
        agent_listed(&readme_doc.content, "Gemini"),
        "Gemini should be listed"
    );
    assert!(
        agent_listed(&readme_doc.content, "ChatGPT"),
        "ChatGPT should be listed"
    );
}

/// Verify date range in README matches summary timestamps.
#[test]
fn test_readme_date_range_accurate() {
    let summary = create_test_summary(50, vec![("Test Agent", 50)]);
    let config = DocConfig::new();

    let generator = DocumentationGenerator::new(config, summary);
    let readme_doc = generator.generate_readme();

    // Should contain the start and end dates
    assert!(
        readme_doc.content.contains("2024-01-01"),
        "README should contain start date"
    );
    assert!(
        readme_doc.content.contains("2024-12-31"),
        "README should contain end date"
    );
}

/// Verify deployment URL appears in README when configured.
#[test]
fn test_readme_url_included() {
    let summary = create_test_summary(10, vec![("Agent", 10)]);
    let config = DocConfig::new().with_url("https://my-archive.example.com");

    let generator = DocumentationGenerator::new(config, summary);
    let readme_doc = generator.generate_readme();

    assert!(
        readme_doc
            .content
            .contains("https://my-archive.example.com"),
        "README should include configured URL"
    );
}

/// Verify Argon2 parameters are documented.
#[test]
fn test_readme_argon_params_included() {
    let summary = create_test_summary(10, vec![("Agent", 10)]);
    let config = DocConfig::new().with_argon_params(65536, 3, 4);

    let generator = DocumentationGenerator::new(config, summary);
    let readme_doc = generator.generate_readme();

    // Should mention Argon2 parameters
    assert!(
        readme_doc.content.contains("65536") || readme_doc.content.contains("64MB"),
        "README should include memory parameter"
    );
}

// =============================================================================
// README Completeness Tests
// =============================================================================

/// Verify README has all required sections.
#[test]
fn test_readme_has_required_sections() {
    let summary = create_test_summary(100, vec![("Claude", 100)]);
    let config = DocConfig::new().with_url("https://example.com");

    let generator = DocumentationGenerator::new(config, summary);
    let readme_doc = generator.generate_readme();
    let content = &readme_doc.content;

    // Check for standard README sections (case-insensitive)
    let has_archive_section = content.to_lowercase().contains("archive");
    let has_security_section =
        content.to_lowercase().contains("security") || content.to_lowercase().contains("encrypt");
    let has_usage_section = content.to_lowercase().contains("usage")
        || content.to_lowercase().contains("access")
        || content.to_lowercase().contains("how to");

    assert!(
        has_archive_section,
        "README should have archive information"
    );
    assert!(
        has_security_section,
        "README should mention security/encryption"
    );
    assert!(has_usage_section, "README should have usage instructions");
}

/// Verify version information is included.
#[test]
fn test_readme_includes_version() {
    let summary = create_test_summary(10, vec![("Agent", 10)]);
    let config = DocConfig::new();

    let generator = DocumentationGenerator::new(config, summary);
    let readme_doc = generator.generate_readme();

    // Should contain version number (semantic versioning pattern)
    let version_re = Regex::new(r"\d+\.\d+\.\d+").unwrap();
    assert!(
        version_re.is_match(&readme_doc.content),
        "README should include version number"
    );
}

// =============================================================================
// Edge Cases
// =============================================================================

/// Test README generation with no agents.
#[test]
fn test_readme_empty_agents() {
    let summary = create_test_summary(0, vec![]);
    let config = DocConfig::new();

    let generator = DocumentationGenerator::new(config, summary);
    let readme_doc = generator.generate_readme();

    // Should still generate valid README
    assert!(!readme_doc.content.is_empty(), "README should not be empty");
    assert!(
        readme_doc.content.contains("0") || readme_doc.content.contains("No"),
        "README should indicate no conversations"
    );
}

/// Test README generation with single agent.
#[test]
fn test_readme_single_agent() {
    let summary = create_test_summary(42, vec![("Claude Code", 42)]);
    let config = DocConfig::new();

    let generator = DocumentationGenerator::new(config, summary);
    let readme_doc = generator.generate_readme();

    assert!(
        readme_doc.content.contains("Claude Code"),
        "Single agent should be listed"
    );
    assert!(
        readme_doc.content.contains("42"),
        "Conversation count should appear"
    );
}

/// Test README generation with very large numbers.
#[test]
fn test_readme_large_counts() {
    let summary = create_test_summary(999999, vec![("Agent A", 500000), ("Agent B", 499999)]);
    let config = DocConfig::new();

    let generator = DocumentationGenerator::new(config, summary);
    let readme_doc = generator.generate_readme();

    // Should handle large numbers without panic
    assert!(!readme_doc.content.is_empty());
    // The count might be formatted with commas or plain
    assert!(
        readme_doc.content.contains("999999") || readme_doc.content.contains("999,999"),
        "README should contain the large count"
    );
}
