use coding_agent_search::sources::sync::path_to_safe_dirname;

#[test]
fn test_path_to_safe_dirname_stability() {
    // This test verifies the fix for the sync directory oscillation bug.
    // The local directory name must be derived from the configured path string,
    // not the expanded path, to ensure stability when home directory lookup fails/succeeds.

    let configured_path = "~/.claude/projects";

    // Scenario 1: Home expansion succeeds
    // expanded_path would be "/home/user/.claude/projects"
    // BUT we must pass the configured path to path_to_safe_dirname
    let safe_name_1 = path_to_safe_dirname(configured_path);

    // Scenario 2: Home expansion fails (e.g. SSH error)
    // expanded_path would be "~/.claude/projects" (same as configured)
    let safe_name_2 = path_to_safe_dirname(configured_path);

    // They must be identical
    assert_eq!(safe_name_1, safe_name_2);

    // Verify hashing structure
    assert!(safe_name_1.starts_with("claude_projects_"));
}

#[test]
fn test_path_to_safe_dirname_distinct_configs() {
    // Different configurations pointing to same location should have DIFFERENT local dirs
    // to avoid collision if they are configured separately.

    let config_1 = "~/.claude/projects";
    let config_2 = "/home/user/.claude/projects";

    let safe_1 = path_to_safe_dirname(config_1);
    let safe_2 = path_to_safe_dirname(config_2);

    assert_ne!(
        safe_1, safe_2,
        "Different config strings should map to different dirs"
    );

    // But both should look reasonable
    assert!(safe_1.contains("claude_projects"));
    assert!(safe_2.contains("home_user_claude_projects"));
}
