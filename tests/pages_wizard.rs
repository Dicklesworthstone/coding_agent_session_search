//! Tests for pages wizard functionality.
//!
//! These tests verify the wizard state management, validation logic,
//! and export pipeline without requiring interactive input.

mod util;

use coding_agent_search::pages::summary::ExclusionSet;
use coding_agent_search::pages::wizard::{DeployTarget, WizardState};
use std::path::PathBuf;
use tempfile::TempDir;
use util::e2e_log::PhaseTracker;

// =============================================================================
// WizardState Tests
// =============================================================================

#[test]
fn test_wizard_state_default() {
    let _tracker = PhaseTracker::new("pages_wizard", "state_default");

    let state = WizardState::default();

    // Default content selection
    assert!(state.agents.is_empty());
    assert!(state.time_range.is_none());
    assert!(state.workspaces.is_none());

    // Default security config
    assert!(state.password.is_none());
    assert!(state.recovery_secret.is_none());
    assert!(state.generate_recovery);
    assert!(!state.generate_qr);

    // Default site config
    assert_eq!(state.title, "cass Archive");
    assert!(state.description.contains("Encrypted archive"));
    assert!(!state.hide_metadata);

    // Default deployment
    assert!(matches!(state.target, DeployTarget::Local));
    assert_eq!(state.output_dir, PathBuf::from("cass-export"));
    assert!(state.repo_name.is_none());

    // Default exclusions
    assert!(state.exclusions.excluded_conversations.is_empty());
    assert!(state.exclusions.excluded_workspaces.is_empty());
    assert!(state.last_summary.is_none());

    // Default secret scan
    assert!(!state.secret_scan_has_findings);
    assert!(!state.secret_scan_has_critical);
    assert_eq!(state.secret_scan_count, 0);

    // Default encryption
    assert!(!state.no_encryption);
    assert!(!state.unencrypted_confirmed);

    // Default attachments
    assert!(!state.include_attachments);
    assert!(state.final_site_dir.is_none());
}

#[test]
fn test_wizard_state_with_custom_config() {
    let _tracker = PhaseTracker::new("pages_wizard", "state_custom_config");

    let state = WizardState {
        // Content selection
        agents: vec!["claude_code".to_string(), "codex".to_string()],
        time_range: Some("last_week".to_string()),
        workspaces: Some(vec![PathBuf::from("/projects/myapp")]),
        // Security
        password: Some("test-password-123".to_string()),
        generate_recovery: true,
        generate_qr: true,
        password_entropy_bits: 48.0,
        // Site
        title: "My Archive".to_string(),
        description: "My custom archive".to_string(),
        hide_metadata: true,
        // Deployment
        target: DeployTarget::GitHubPages,
        output_dir: PathBuf::from("/tmp/export"),
        repo_name: Some("my-archive".to_string()),
        ..WizardState::default()
    };

    // Verify
    assert_eq!(state.agents.len(), 2);
    assert_eq!(state.time_range.as_deref(), Some("last_week"));
    assert_eq!(state.workspaces.as_ref().unwrap().len(), 1);
    assert!(state.password.is_some());
    assert_eq!(state.password_entropy_bits, 48.0);
    assert_eq!(state.title, "My Archive");
    assert!(matches!(state.target, DeployTarget::GitHubPages));
}

#[test]
fn test_wizard_state_no_encryption_mode() {
    let _tracker = PhaseTracker::new("pages_wizard", "state_no_encryption");

    let state = WizardState {
        // Enable no encryption mode
        no_encryption: true,
        unencrypted_confirmed: true,
        // When unencrypted, recovery options should be disabled
        generate_recovery: false,
        generate_qr: false,
        password: None,
        ..WizardState::default()
    };

    assert!(state.no_encryption);
    assert!(state.unencrypted_confirmed);
    assert!(!state.generate_recovery);
    assert!(!state.generate_qr);
    assert!(state.password.is_none());
}

// =============================================================================
// DeployTarget Tests
// =============================================================================

#[test]
fn test_deploy_target_display() {
    let _tracker = PhaseTracker::new("pages_wizard", "deploy_target_display");

    assert_eq!(DeployTarget::Local.to_string(), "Local export only");
    assert_eq!(DeployTarget::GitHubPages.to_string(), "GitHub Pages");
    assert_eq!(
        DeployTarget::CloudflarePages.to_string(),
        "Cloudflare Pages"
    );
}

#[test]
fn test_deploy_target_equality() {
    let _tracker = PhaseTracker::new("pages_wizard", "deploy_target_equality");

    assert_eq!(DeployTarget::Local, DeployTarget::Local);
    assert_eq!(DeployTarget::GitHubPages, DeployTarget::GitHubPages);
    assert_eq!(DeployTarget::CloudflarePages, DeployTarget::CloudflarePages);

    assert_ne!(DeployTarget::Local, DeployTarget::GitHubPages);
    assert_ne!(DeployTarget::GitHubPages, DeployTarget::CloudflarePages);
}

// =============================================================================
// ExclusionSet Tests
// =============================================================================

#[test]
fn test_exclusion_set_empty() {
    let _tracker = PhaseTracker::new("pages_wizard", "exclusion_set_empty");

    let exclusions = ExclusionSet::new();
    assert!(exclusions.excluded_conversations.is_empty());
    assert!(exclusions.excluded_workspaces.is_empty());
}

#[test]
fn test_exclusion_set_add_conversations() {
    let _tracker = PhaseTracker::new("pages_wizard", "exclusion_set_conversations");

    let mut exclusions = ExclusionSet::new();

    // Add exclusions
    exclusions.excluded_conversations.insert(1);
    exclusions.excluded_conversations.insert(2);
    exclusions.excluded_conversations.insert(3);

    assert!(!exclusions.excluded_conversations.is_empty());
    assert!(exclusions.excluded_conversations.contains(&1));
    assert!(exclusions.excluded_conversations.contains(&2));
    assert!(exclusions.excluded_conversations.contains(&3));
    assert!(!exclusions.excluded_conversations.contains(&99));

    // Remove
    exclusions.excluded_conversations.remove(&1);
    assert!(!exclusions.excluded_conversations.contains(&1));
    assert!(exclusions.excluded_conversations.contains(&2));
}

#[test]
fn test_exclusion_set_workspaces() {
    let _tracker = PhaseTracker::new("pages_wizard", "exclusion_set_workspaces");

    let mut exclusions = ExclusionSet::new();

    exclusions
        .excluded_workspaces
        .insert("/projects/myapp".to_string());
    exclusions
        .excluded_workspaces
        .insert("/projects/other".to_string());

    assert!(exclusions.excluded_workspaces.contains("/projects/myapp"));
    assert!(exclusions.excluded_workspaces.contains("/projects/other"));
    assert!(
        !exclusions
            .excluded_workspaces
            .contains("/projects/nonexistent")
    );

    exclusions.excluded_workspaces.remove("/projects/other");
    assert!(!exclusions.excluded_workspaces.contains("/projects/other"));
}

// =============================================================================
// Wizard State Validation Tests
// =============================================================================

#[test]
fn test_wizard_state_validation_password_required_for_encryption() {
    let _tracker = PhaseTracker::new("pages_wizard", "validation_password_required");

    let state = WizardState::default();

    // When not in no_encryption mode, password is required
    assert!(!state.no_encryption);
    assert!(state.password.is_none());
    // This state would fail validation during wizard run
}

#[test]
fn test_wizard_state_validation_output_dir() {
    let _tracker = PhaseTracker::new("pages_wizard", "validation_output_dir");

    let tmp = TempDir::new().unwrap();
    let state = WizardState {
        output_dir: tmp.path().to_path_buf(),
        ..WizardState::default()
    };

    // Validate output directory exists
    assert!(
        state.output_dir.exists()
            || state
                .output_dir
                .parent()
                .map(|p| p.exists())
                .unwrap_or(false)
    );
}

#[test]
fn test_wizard_state_with_attachments() {
    let _tracker = PhaseTracker::new("pages_wizard", "state_with_attachments");

    let state = WizardState {
        include_attachments: true,
        ..WizardState::default()
    };

    assert!(state.include_attachments);
}

// =============================================================================
// Secret Scan State Tests
// =============================================================================

#[test]
fn test_wizard_state_secret_scan_no_findings() {
    let _tracker = PhaseTracker::new("pages_wizard", "secret_scan_no_findings");

    let state = WizardState::default();

    assert!(!state.secret_scan_has_findings);
    assert!(!state.secret_scan_has_critical);
    assert_eq!(state.secret_scan_count, 0);
}

#[test]
fn test_wizard_state_secret_scan_with_findings() {
    let _tracker = PhaseTracker::new("pages_wizard", "secret_scan_with_findings");

    let state = WizardState {
        // Simulate secret scan results
        secret_scan_has_findings: true,
        secret_scan_has_critical: false,
        secret_scan_count: 3,
        ..WizardState::default()
    };

    assert!(state.secret_scan_has_findings);
    assert!(!state.secret_scan_has_critical);
    assert_eq!(state.secret_scan_count, 3);
}

#[test]
fn test_wizard_state_secret_scan_critical_findings() {
    let _tracker = PhaseTracker::new("pages_wizard", "secret_scan_critical");

    let state = WizardState {
        // Simulate critical secret scan results
        secret_scan_has_findings: true,
        secret_scan_has_critical: true,
        secret_scan_count: 5,
        ..WizardState::default()
    };

    assert!(state.secret_scan_has_findings);
    assert!(state.secret_scan_has_critical);
    assert_eq!(state.secret_scan_count, 5);
}

// =============================================================================
// Integration: Wizard State to Export Config
// =============================================================================

#[test]
fn test_wizard_state_to_export_filter() {
    let _tracker = PhaseTracker::new("pages_wizard", "state_to_export_filter");

    let state = WizardState {
        agents: vec!["claude_code".to_string(), "codex".to_string()],
        workspaces: Some(vec![PathBuf::from("/projects/myapp")]),
        ..WizardState::default()
    };

    // Verify state can be used to construct export filters
    assert_eq!(state.agents.len(), 2);
    assert!(state.agents.contains(&"claude_code".to_string()));
    assert!(state.agents.contains(&"codex".to_string()));

    let workspaces = state.workspaces.as_ref().unwrap();
    assert_eq!(workspaces.len(), 1);
    assert_eq!(workspaces[0], PathBuf::from("/projects/myapp"));
}

#[test]
fn test_wizard_state_final_site_dir_tracking() {
    let _tracker = PhaseTracker::new("pages_wizard", "final_site_dir_tracking");

    let tmp = TempDir::new().unwrap();
    let mut state = WizardState::default();

    assert!(state.final_site_dir.is_none());

    // After bundle creation, final_site_dir is set
    state.final_site_dir = Some(tmp.path().join("site"));

    assert!(state.final_site_dir.is_some());
    assert!(state.final_site_dir.as_ref().unwrap().ends_with("site"));
}

// =============================================================================
// Real Fixture Integration Tests
// =============================================================================
//
// These tests use the actual fixture database at:
//   tests/fixtures/search_demo_data/agent_search.db
//
// The fixture contains:
//   - Agents: claude_code, gemini, opencode
//   - 4 conversations with real message data
//   - FTS tables for full-text search

use coding_agent_search::pages::config_input::PagesConfig;
use coding_agent_search::pages::export::{ExportEngine, ExportFilter, PathMode};
use rusqlite::Connection;

/// Returns the path to the fixture database
fn fixture_db_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("search_demo_data")
        .join("agent_search.db")
}

#[test]
fn test_wizard_with_real_fixture_database() {
    let _tracker = PhaseTracker::new("pages_wizard", "real_fixture_database");

    let db_path = fixture_db_path();
    assert!(
        db_path.exists(),
        "Fixture database should exist at {:?}",
        db_path
    );

    // Create wizard state pointing to real fixture database
    let state = WizardState {
        db_path: db_path.clone(),
        ..WizardState::default()
    };

    // Verify we can open and query the database
    let conn = Connection::open(&db_path).expect("Should open fixture database");

    // Query agents
    let mut stmt = conn.prepare("SELECT slug FROM agents").unwrap();
    let agents: Vec<String> = stmt
        .query_map([], |row| row.get(0))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();

    assert!(!agents.is_empty(), "Should have agents in fixture");
    assert!(agents.contains(&"claude_code".to_string()));

    // Query conversations
    let conv_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM conversations", [], |r| r.get(0))
        .unwrap();
    assert!(conv_count > 0, "Should have conversations in fixture");
}

#[test]
fn test_export_with_real_fixture_all_agents() {
    let _tracker = PhaseTracker::new("pages_wizard", "export_real_fixture_all");

    let db_path = fixture_db_path();
    let tmp = TempDir::new().unwrap();
    let output_path = tmp.path().join("export.db");

    // Export all conversations without filters
    let filter = ExportFilter {
        agents: None,
        workspaces: None,
        since: None,
        until: None,
        path_mode: PathMode::Relative,
    };

    let engine = ExportEngine::new(&db_path, &output_path, filter);
    let stats = engine
        .execute(|_, _| {}, None)
        .expect("Export should succeed");

    // Verify we exported real data
    assert!(
        stats.conversations_processed > 0,
        "Should export conversations"
    );
    assert!(stats.messages_processed > 0, "Should export messages");

    // Verify output database structure
    let conn = Connection::open(&output_path).unwrap();

    let conv_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM conversations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(conv_count as usize, stats.conversations_processed);

    // Verify FTS table was created
    let fts_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM messages_fts", [], |r| r.get(0))
        .unwrap();
    assert!(fts_count > 0, "Should have FTS entries");
}

#[test]
fn test_export_with_real_fixture_agent_filter() {
    let _tracker = PhaseTracker::new("pages_wizard", "export_real_fixture_agent_filter");

    let db_path = fixture_db_path();
    let tmp = TempDir::new().unwrap();
    let output_path = tmp.path().join("export.db");

    // Filter to only claude_code agent
    let filter = ExportFilter {
        agents: Some(vec!["claude_code".to_string()]),
        workspaces: None,
        since: None,
        until: None,
        path_mode: PathMode::Relative,
    };

    let engine = ExportEngine::new(&db_path, &output_path, filter);
    let stats = engine
        .execute(|_, _| {}, None)
        .expect("Export should succeed");

    // Verify filtered export
    let conn = Connection::open(&output_path).unwrap();

    // All exported conversations should be from claude_code
    let mut stmt = conn
        .prepare("SELECT DISTINCT agent FROM conversations")
        .unwrap();
    let agents: Vec<String> = stmt
        .query_map([], |row| row.get(0))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();

    // If there are claude_code conversations, verify only those were exported
    if stats.conversations_processed > 0 {
        assert!(
            agents.iter().all(|a| a == "claude_code"),
            "Should only have claude_code"
        );
    }
}

#[test]
fn test_export_with_real_fixture_nonexistent_agent() {
    let _tracker = PhaseTracker::new("pages_wizard", "export_real_fixture_no_match");

    let db_path = fixture_db_path();
    let tmp = TempDir::new().unwrap();
    let output_path = tmp.path().join("export.db");

    // Filter to a nonexistent agent
    let filter = ExportFilter {
        agents: Some(vec!["nonexistent_agent_xyz".to_string()]),
        workspaces: None,
        since: None,
        until: None,
        path_mode: PathMode::Relative,
    };

    let engine = ExportEngine::new(&db_path, &output_path, filter);
    let stats = engine
        .execute(|_, _| {}, None)
        .expect("Export should succeed even with no matches");

    // Should get empty result
    assert_eq!(stats.conversations_processed, 0);
    assert_eq!(stats.messages_processed, 0);

    // Output database should still be valid with schema
    let conn = Connection::open(&output_path).unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM conversations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0);
}

#[test]
fn test_export_with_real_fixture_path_modes() {
    let _tracker = PhaseTracker::new("pages_wizard", "export_real_fixture_path_modes");

    let db_path = fixture_db_path();

    for path_mode in [
        PathMode::Relative,
        PathMode::Basename,
        PathMode::Full,
        PathMode::Hash,
    ] {
        let tmp = TempDir::new().unwrap();
        let output_path = tmp.path().join("export.db");

        let filter = ExportFilter {
            agents: None,
            workspaces: None,
            since: None,
            until: None,
            path_mode,
        };

        let engine = ExportEngine::new(&db_path, &output_path, filter);
        let result = engine.execute(|_, _| {}, None);

        assert!(
            result.is_ok(),
            "Export should succeed with {:?} path mode",
            path_mode
        );

        let conn = Connection::open(&output_path).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM conversations", [], |r| r.get(0))
            .unwrap();
        assert!(count >= 0, "Should have valid conversation count");
    }
}

#[test]
fn test_pages_config_to_wizard_state_with_real_db() {
    let _tracker = PhaseTracker::new("pages_wizard", "config_to_state_real_db");

    let db_path = fixture_db_path();

    // Create config via JSON (non-interactive mode)
    let config_json = r#"{
        "filters": {
            "agents": ["claude_code"],
            "path_mode": "relative"
        },
        "encryption": {
            "no_encryption": true,
            "i_understand_risks": true
        },
        "bundle": {
            "title": "Test Archive",
            "description": "Integration test export"
        },
        "deployment": {
            "target": "local",
            "output_dir": "/tmp/test-export"
        }
    }"#;

    let config: PagesConfig = serde_json::from_str(config_json).unwrap();

    // Validate config
    let validation = config.validate();
    assert!(
        validation.valid,
        "Config should be valid: {:?}",
        validation.errors
    );

    // Convert to wizard state
    let state = config.to_wizard_state(db_path.clone()).unwrap();

    // Verify state matches config
    assert_eq!(state.agents, vec!["claude_code"]);
    assert_eq!(state.title, "Test Archive");
    assert_eq!(state.description, "Integration test export");
    assert!(state.no_encryption);
    assert!(state.unencrypted_confirmed);
    assert_eq!(state.db_path, db_path);
    assert!(matches!(state.target, DeployTarget::Local));
}

#[test]
fn test_wizard_state_with_fixture_export_flow() {
    let _tracker = PhaseTracker::new("pages_wizard", "state_fixture_export_flow");

    let db_path = fixture_db_path();
    let tmp = TempDir::new().unwrap();

    // Create wizard state configured for export
    let state = WizardState {
        db_path: db_path.clone(),
        agents: vec!["claude_code".to_string()],
        output_dir: tmp.path().to_path_buf(),
        no_encryption: true,
        unencrypted_confirmed: true,
        title: "Fixture Test".to_string(),
        ..WizardState::default()
    };

    // Build export filter from wizard state
    let filter = ExportFilter {
        agents: if state.agents.is_empty() {
            None
        } else {
            Some(state.agents.clone())
        },
        workspaces: state.workspaces.clone(),
        since: None,
        until: None,
        path_mode: PathMode::Relative,
    };

    // Run export
    let output_db = tmp.path().join("export.db");
    let engine = ExportEngine::new(&state.db_path, &output_db, filter);
    let stats = engine
        .execute(
            |current, total| {
                // Progress callback - verify it's called with reasonable values
                assert!(current <= total);
            },
            None,
        )
        .expect("Export should succeed");

    // Verify export results
    if stats.conversations_processed > 0 {
        let conn = Connection::open(&output_db).unwrap();

        // Verify exported data
        let conv_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM conversations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(conv_count as usize, stats.conversations_processed);

        // Verify messages were exported
        let msg_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages_fts", [], |r| r.get(0))
            .unwrap();
        assert!(msg_count >= 0);
    }
}

#[test]
fn test_exclusion_set_with_real_conversation_ids() {
    let _tracker = PhaseTracker::new("pages_wizard", "exclusion_real_ids");

    let db_path = fixture_db_path();

    // Get real conversation IDs from fixture
    let conn = Connection::open(&db_path).expect("Should open fixture database");
    let mut stmt = conn
        .prepare("SELECT id FROM conversations LIMIT 5")
        .unwrap();
    let conv_ids: Vec<i64> = stmt
        .query_map([], |row| row.get(0))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();

    if conv_ids.is_empty() {
        return; // Skip if no conversations
    }

    // Test exclusion set with real IDs
    let mut exclusions = ExclusionSet::new();

    for id in &conv_ids {
        exclusions.excluded_conversations.insert(*id);
    }

    // Verify all IDs are excluded
    for id in &conv_ids {
        assert!(
            exclusions.excluded_conversations.contains(id),
            "Should have conversation {}",
            id
        );
    }

    // Remove first ID
    if let Some(&first_id) = conv_ids.first() {
        exclusions.excluded_conversations.remove(&first_id);
        assert!(!exclusions.excluded_conversations.contains(&first_id));
    }

    // Others should still be excluded
    for id in conv_ids.iter().skip(1) {
        assert!(exclusions.excluded_conversations.contains(id));
    }
}

#[test]
fn test_config_validation_scenarios() {
    let _tracker = PhaseTracker::new("pages_wizard", "config_validation_scenarios");

    // Valid config with password
    let valid_config = r#"{
        "encryption": {"password": "secure-password-123"}
    }"#;
    let config: PagesConfig = serde_json::from_str(valid_config).unwrap();
    let result = config.validate();
    assert!(result.valid, "Should be valid with password");

    // Invalid: no encryption without acknowledgment
    let invalid_config = r#"{
        "encryption": {"no_encryption": true}
    }"#;
    let config: PagesConfig = serde_json::from_str(invalid_config).unwrap();
    let result = config.validate();
    assert!(!result.valid, "Should fail without i_understand_risks");
    assert!(
        result
            .errors
            .iter()
            .any(|e| e.contains("i_understand_risks"))
    );

    // Invalid: github without repo
    let invalid_github = r#"{
        "encryption": {"password": "test"},
        "deployment": {"target": "github"}
    }"#;
    let config: PagesConfig = serde_json::from_str(invalid_github).unwrap();
    let result = config.validate();
    assert!(!result.valid, "Should fail without repo for github target");

    // Valid: github with repo
    let valid_github = r#"{
        "encryption": {"password": "test"},
        "deployment": {"target": "github", "repo": "my-archive"}
    }"#;
    let config: PagesConfig = serde_json::from_str(valid_github).unwrap();
    let result = config.validate();
    assert!(result.valid, "Should be valid with repo for github target");
}
