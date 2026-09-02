//! Tests for the setup workflow module.
//!
//! These tests cover:
//! - SetupOptions default values and construction
//! - SetupState serialization/deserialization
//! - SetupState::has_progress() logic
//! - SetupResult structure
//! - SetupError display implementations
//!
//! Note: Tests requiring actual SSH connectivity are not included here.
//! Instead, we use fixture-based probe results from tests/fixtures/sources/probe/.

// Allow field reassignment in tests - we deliberately test has_progress() by setting individual fields
#![allow(clippy::field_reassign_with_default)]

mod util;

use coding_agent_search::sources::probe::CassStatus;
use coding_agent_search::sources::setup::{SetupError, SetupOptions, SetupResult, SetupState};
use util::probe_fixtures;

// =============================================================================
// SetupOptions Tests
// =============================================================================

/// Test that SetupOptions::default() produces expected values.
#[test]
fn setup_options_default_values() {
    let opts = SetupOptions::default();

    assert!(!opts.dry_run, "dry_run should default to false");
    assert!(
        !opts.non_interactive,
        "non_interactive should default to false"
    );
    assert!(opts.hosts.is_none(), "hosts should default to None");
    assert!(!opts.skip_install, "skip_install should default to false");
    assert!(!opts.skip_index, "skip_index should default to false");
    assert!(!opts.skip_sync, "skip_sync should default to false");
    assert_eq!(opts.timeout, 10, "timeout should default to 10 seconds");
    assert!(!opts.resume, "resume should default to false");
    assert!(!opts.verbose, "verbose should default to false");
    assert!(!opts.json, "json should default to false");
}

/// Test SetupOptions with various configurations.
#[test]
fn setup_options_custom_values() {
    let opts = SetupOptions {
        dry_run: true,
        non_interactive: true,
        hosts: Some(vec!["host1".to_string(), "host2".to_string()]),
        skip_install: true,
        skip_index: true,
        skip_sync: true,
        timeout: 30,
        resume: true,
        verbose: true,
        json: true,
    };

    assert!(opts.dry_run);
    assert!(opts.non_interactive);
    assert_eq!(
        opts.hosts,
        Some(vec!["host1".to_string(), "host2".to_string()])
    );
    assert!(opts.skip_install);
    assert!(opts.skip_index);
    assert!(opts.skip_sync);
    assert_eq!(opts.timeout, 30);
    assert!(opts.resume);
    assert!(opts.verbose);
    assert!(opts.json);
}

// =============================================================================
// SetupState Tests
// =============================================================================

/// Test that SetupState::default() produces empty state.
#[test]
fn setup_state_default_is_empty() {
    let state = SetupState::default();

    assert!(!state.discovery_complete);
    assert_eq!(state.discovered_hosts, 0);
    assert!(state.discovered_host_names.is_empty());
    assert!(!state.probing_complete);
    assert!(state.probed_hosts.is_empty());
    assert!(!state.selection_complete);
    assert!(state.selected_host_names.is_empty());
    assert!(!state.installation_complete);
    assert!(state.completed_installs.is_empty());
    assert!(!state.indexing_complete);
    assert!(state.completed_indexes.is_empty());
    assert!(!state.configuration_complete);
    assert!(!state.sync_complete);
    assert!(state.current_operation.is_none());
    assert!(state.started_at.is_none());
}

/// Test SetupState::has_progress() returns false for empty state.
#[test]
fn setup_state_has_progress_empty() {
    let state = SetupState::default();
    assert!(!state.has_progress(), "Empty state should have no progress");
}

/// Test SetupState::has_progress() returns true when discovery is complete.
#[test]
fn setup_state_has_progress_discovery() {
    let state = SetupState {
        discovery_complete: true,
        ..Default::default()
    };
    assert!(
        state.has_progress(),
        "State with discovery_complete should have progress"
    );
}

/// Test SetupState::has_progress() returns true when probing is complete.
#[test]
fn setup_state_has_progress_probing() {
    let state = SetupState {
        probing_complete: true,
        ..Default::default()
    };
    assert!(
        state.has_progress(),
        "State with probing_complete should have progress"
    );
}

/// Test SetupState::has_progress() returns true when selection is complete.
#[test]
fn setup_state_has_progress_selection() {
    let state = SetupState {
        selection_complete: true,
        ..Default::default()
    };
    assert!(
        state.has_progress(),
        "State with selection_complete should have progress"
    );
}

/// Test SetupState::has_progress() returns true when installation is complete.
#[test]
fn setup_state_has_progress_installation() {
    let state = SetupState {
        installation_complete: true,
        ..Default::default()
    };
    assert!(
        state.has_progress(),
        "State with installation_complete should have progress"
    );
}

/// Test SetupState::has_progress() returns true when indexing is complete.
#[test]
fn setup_state_has_progress_indexing() {
    let state = SetupState {
        indexing_complete: true,
        ..Default::default()
    };
    assert!(
        state.has_progress(),
        "State with indexing_complete should have progress"
    );
}

/// Test SetupState::has_progress() returns true when configuration is complete.
#[test]
fn setup_state_has_progress_configuration() {
    let state = SetupState {
        configuration_complete: true,
        ..Default::default()
    };
    assert!(
        state.has_progress(),
        "State with configuration_complete should have progress"
    );
}

/// Test SetupState serialization and deserialization roundtrip.
#[test]
fn setup_state_serde_roundtrip() {
    let state = SetupState {
        discovery_complete: true,
        discovered_hosts: 5,
        discovered_host_names: vec!["host1".to_string(), "host2".to_string()],
        probing_complete: true,
        selection_complete: true,
        selected_host_names: vec!["host1".to_string()],
        installation_complete: true,
        completed_installs: vec!["host1".to_string()],
        started_at: Some("2025-01-01T00:00:00Z".to_string()),
        current_operation: Some("Testing".to_string()),
        ..Default::default()
    };

    // Serialize to JSON
    let json = serde_json::to_string(&state).expect("Failed to serialize SetupState");

    // Deserialize back
    let deserialized: SetupState =
        serde_json::from_str(&json).expect("Failed to deserialize SetupState");

    assert_eq!(deserialized.discovery_complete, state.discovery_complete);
    assert_eq!(deserialized.discovered_hosts, state.discovered_hosts);
    assert_eq!(
        deserialized.discovered_host_names,
        state.discovered_host_names
    );
    assert_eq!(deserialized.probing_complete, state.probing_complete);
    assert_eq!(deserialized.selection_complete, state.selection_complete);
    assert_eq!(deserialized.selected_host_names, state.selected_host_names);
    assert_eq!(
        deserialized.installation_complete,
        state.installation_complete
    );
    assert_eq!(deserialized.completed_installs, state.completed_installs);
    assert_eq!(deserialized.started_at, state.started_at);
    assert_eq!(deserialized.current_operation, state.current_operation);
}

/// Test SetupState serialization produces valid JSON.
#[test]
fn setup_state_json_format() {
    let state = SetupState {
        discovery_complete: true,
        discovered_hosts: 3,
        ..Default::default()
    };

    let json = serde_json::to_string_pretty(&state).expect("Failed to serialize SetupState");

    // Verify it's valid JSON by parsing it back
    let value: serde_json::Value = serde_json::from_str(&json).expect("Invalid JSON output");

    assert_eq!(value["discovery_complete"], true);
    assert_eq!(value["discovered_hosts"], 3);
}

/// Test SetupState with HostProbeResult serialization using fixtures.
#[test]
fn setup_state_with_probe_results() {
    // Load a probe result from fixture instead of manual construction
    let probe = probe_fixtures::no_cass_host();

    let state = SetupState {
        probed_hosts: vec![probe],
        probing_complete: true,
        ..Default::default()
    };

    // Serialize and deserialize
    let json = serde_json::to_string(&state).expect("Failed to serialize");
    let deserialized: SetupState = serde_json::from_str(&json).expect("Failed to deserialize");

    assert_eq!(deserialized.probed_hosts.len(), 1);
    // Verify fixture data (no_cass_host.json has host_name: "macbook-air")
    assert!(!deserialized.probed_hosts[0].host_name.is_empty());
    assert!(deserialized.probed_hosts[0].reachable);
}

/// Test SetupState with multiple fixture-based probe results.
#[test]
fn setup_state_with_multiple_probe_results() {
    // Load different host scenarios from fixtures
    let indexed = probe_fixtures::indexed_host();
    let no_cass = probe_fixtures::no_cass_host();
    let unreachable = probe_fixtures::unreachable_host();

    let state = SetupState {
        probed_hosts: vec![indexed.clone(), no_cass.clone(), unreachable.clone()],
        probing_complete: true,
        ..Default::default()
    };

    // Serialize and deserialize roundtrip
    let json = serde_json::to_string(&state).expect("Failed to serialize");
    let deserialized: SetupState = serde_json::from_str(&json).expect("Failed to deserialize");

    assert_eq!(deserialized.probed_hosts.len(), 3);

    // Verify indexed host has cass installed
    assert!(deserialized.probed_hosts[0].cass_status.is_installed());
    // Verify no_cass host doesn't have cass
    assert!(!deserialized.probed_hosts[1].cass_status.is_installed());
    // Verify unreachable host is not reachable
    assert!(!deserialized.probed_hosts[2].reachable);
}

// =============================================================================
// SetupError Tests
// =============================================================================

/// Test SetupError::Cancelled display.
#[test]
fn setup_error_cancelled_display() {
    let err = SetupError::Cancelled;
    assert_eq!(format!("{err}"), "Setup cancelled by user");
}

/// Test SetupError::NoHosts display.
#[test]
fn setup_error_no_hosts_display() {
    let err = SetupError::NoHosts;
    assert_eq!(format!("{err}"), "No SSH hosts found or selected");
}

/// Test SetupError::Interrupted display.
#[test]
fn setup_error_interrupted_display() {
    let err = SetupError::Interrupted;
    assert_eq!(format!("{err}"), "Setup interrupted");
}

/// Test SetupError::Io display.
#[test]
fn setup_error_io_display() {
    let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
    let err = SetupError::Io(io_err);
    assert!(format!("{err}").contains("IO error"));
}

/// Test SetupError::Json display.
#[test]
fn setup_error_json_display() {
    // Create a JSON error by parsing invalid JSON
    let json_err = serde_json::from_str::<SetupState>("invalid json").unwrap_err();
    let err = SetupError::Json(json_err);
    assert!(format!("{err}").contains("JSON error"));
}

// =============================================================================
// SetupResult Tests
// =============================================================================

/// Test SetupResult structure.
#[test]
fn setup_result_structure() {
    let result = SetupResult {
        sources_added: 3,
        hosts_installed: 1,
        hosts_indexed: 2,
        total_sessions: 150,
        dry_run: false,
        sync_pending: false,
    };

    assert_eq!(result.sources_added, 3);
    assert_eq!(result.hosts_installed, 1);
    assert_eq!(result.hosts_indexed, 2);
    assert_eq!(result.total_sessions, 150);
    assert!(!result.dry_run);
}

/// Test SetupResult for dry run.
#[test]
fn setup_result_dry_run() {
    let result = SetupResult {
        sources_added: 5,
        hosts_installed: 2,
        hosts_indexed: 3,
        total_sessions: 0,
        dry_run: true,
        sync_pending: false,
    };

    assert!(result.dry_run);
    assert_eq!(result.sources_added, 5);
}

// =============================================================================
// CassStatus Helper Tests (used in setup workflow)
// =============================================================================

/// Test CassStatus::is_installed() for NotFound.
#[test]
fn cass_status_not_found_not_installed() {
    let status = CassStatus::NotFound;
    assert!(!status.is_installed());
}

/// Test CassStatus::is_installed() for Unknown.
#[test]
fn cass_status_unknown_not_installed() {
    let status = CassStatus::Unknown;
    assert!(!status.is_installed());
}

/// Test CassStatus::is_installed() for InstalledNotIndexed.
#[test]
fn cass_status_installed_not_indexed_is_installed() {
    let status = CassStatus::InstalledNotIndexed {
        version: "0.1.50".to_string(),
    };
    assert!(status.is_installed());
}

/// Test CassStatus::is_installed() for Indexed.
#[test]
fn cass_status_indexed_is_installed() {
    let status = CassStatus::Indexed {
        version: "0.1.50".to_string(),
        session_count: 100,
        last_indexed: Some("2025-01-01T00:00:00Z".to_string()),
    };
    assert!(status.is_installed());
}

// =============================================================================
// State Workflow Tests
// =============================================================================

/// Test state progression through phases.
#[test]
fn setup_state_phase_progression() {
    let mut state = SetupState::default();

    // Phase 1: Discovery
    assert!(!state.has_progress());
    state.discovery_complete = true;
    state.discovered_hosts = 5;
    state.discovered_host_names = vec![
        "host1".to_string(),
        "host2".to_string(),
        "host3".to_string(),
        "host4".to_string(),
        "host5".to_string(),
    ];
    assert!(state.has_progress());

    // Phase 2: Probing
    state.probing_complete = true;

    // Phase 3: Selection
    state.selection_complete = true;
    state.selected_host_names = vec!["host1".to_string(), "host2".to_string()];

    // Phase 4: Installation
    state.installation_complete = true;
    state.completed_installs = vec!["host2".to_string()];

    // Phase 5: Indexing
    state.indexing_complete = true;
    state.completed_indexes = vec!["host1".to_string(), "host2".to_string()];

    // Phase 6: Configuration
    state.configuration_complete = true;

    // Phase 7: Sync
    state.sync_complete = true;

    // Verify all phases recorded
    assert!(state.discovery_complete);
    assert!(state.probing_complete);
    assert!(state.selection_complete);
    assert!(state.installation_complete);
    assert!(state.indexing_complete);
    assert!(state.configuration_complete);
    assert!(state.sync_complete);

    // Verify state can be serialized
    let json = serde_json::to_string(&state).unwrap();
    let restored: SetupState = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.discovered_hosts, 5);
    assert_eq!(restored.selected_host_names.len(), 2);
    assert_eq!(restored.completed_installs.len(), 1);
    assert_eq!(restored.completed_indexes.len(), 2);
}

/// Test that sync_complete doesn't affect has_progress().
/// has_progress() is used to determine if there's a resumable session,
/// and sync_complete being true means the setup is done, not resumable.
#[test]
fn setup_state_sync_complete_not_in_has_progress() {
    let mut state = SetupState::default();
    state.sync_complete = true;

    // sync_complete alone doesn't trigger has_progress (correct behavior)
    // because has_progress checks only the phases that represent actual work
    assert!(
        !state.has_progress(),
        "sync_complete alone should not indicate resumable progress"
    );
}
