//! End-to-end integration tests for deployment workflows.
//!
//! This module tests deploy_github and deploy_cloudflare using local
//! infrastructure (bare git repos) with detailed logging artifacts.
//!
//! # Running
//!
//! ```bash
//! # Run all deploy E2E tests
//! cargo test --test e2e_deploy
//!
//! # Run with detailed logging
//! RUST_LOG=debug cargo test --test e2e_deploy -- --nocapture
//! ```

use anyhow::Result;
use coding_agent_search::pages::deploy_cloudflare::{
    CloudflareDeployer, Prerequisites as CfPrereqs,
};
use coding_agent_search::pages::deploy_github::{GitHubDeployer, Prerequisites as GhPrereqs};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

#[path = "util/mod.rs"]
mod util;

use util::e2e_log::PhaseTracker;

// =============================================================================
// Test Constants
// =============================================================================

const TEST_REPO_NAME: &str = "cass-archive-test";

// =============================================================================
// E2E Logger Support
// =============================================================================

fn tracker_for(test_name: &str) -> PhaseTracker {
    PhaseTracker::new("e2e_deploy", test_name)
}

// =============================================================================
// Local Git Infrastructure Helpers
// =============================================================================

/// Create a local bare git repository for testing
fn create_local_bare_repo(temp_dir: &Path, name: &str) -> Result<PathBuf> {
    let repo_path = temp_dir.join(format!("{}.git", name));
    let output = Command::new("git")
        .args(["init", "--bare", repo_path.to_str().unwrap()])
        .output()?;

    if !output.status.success() {
        anyhow::bail!(
            "Failed to create bare repo: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(repo_path)
}

/// Create a minimal test bundle for deployment
fn create_test_bundle(temp_dir: &Path) -> Result<PathBuf> {
    let bundle_dir = temp_dir.join("bundle");
    let site_dir = bundle_dir.join("site");
    fs::create_dir_all(&site_dir)?;

    // Create minimal HTML
    fs::write(
        site_dir.join("index.html"),
        r#"<!DOCTYPE html>
<html>
<head><title>CASS Archive</title></head>
<body>
<h1>CASS Archive Test</h1>
<p>This is a test deployment.</p>
</body>
</html>
"#,
    )?;

    // Create config
    fs::write(
        site_dir.join("config.json"),
        r#"{"version":"1.0","encrypted":false}"#,
    )?;

    // Create assets directory
    let assets_dir = site_dir.join("assets");
    fs::create_dir_all(&assets_dir)?;
    fs::write(
        assets_dir.join("style.css"),
        "body { font-family: sans-serif; }",
    )?;

    Ok(site_dir)
}

/// Clone a local bare repo to a working directory
fn clone_local_repo(bare_repo: &Path, work_dir: &Path) -> Result<()> {
    let output = Command::new("git")
        .args([
            "clone",
            bare_repo.to_str().unwrap(),
            work_dir.to_str().unwrap(),
        ])
        .output()?;

    if !output.status.success() {
        anyhow::bail!(
            "Failed to clone: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

fn checkout_remote_branch(work_dir: &Path, branch_name: &str) -> Result<()> {
    let remote_ref = format!("origin/{branch_name}");
    let output = Command::new("git")
        .current_dir(work_dir)
        .args(["checkout", "--detach", remote_ref.as_str()])
        .output()?;

    if !output.status.success() {
        anyhow::bail!(
            "Failed to checkout {remote_ref}: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

fn read_git_blob(repo: &Path, branch_name: &str, blob_path: &str) -> Result<String> {
    let spec = format!("{branch_name}:{blob_path}");
    let output = Command::new("git")
        .args([
            "--git-dir",
            repo.to_str().expect("utf8 repo path"),
            "show",
            &spec,
        ])
        .output()?;

    if !output.status.success() {
        anyhow::bail!(
            "Failed to read {spec}: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(String::from_utf8(output.stdout)?)
}

/// Create an orphan branch and push to local bare repo
fn create_and_push_orphan(work_dir: &Path, branch_name: &str, bundle_dir: &Path) -> Result<String> {
    // Configure git
    let _ = Command::new("git")
        .current_dir(work_dir)
        .args(["config", "user.email", "test@example.com"])
        .output()?;
    let _ = Command::new("git")
        .current_dir(work_dir)
        .args(["config", "user.name", "Test User"])
        .output()?;

    // Create orphan branch
    let output = Command::new("git")
        .current_dir(work_dir)
        .args(["checkout", "--orphan", branch_name])
        .output()?;

    if !output.status.success() {
        anyhow::bail!(
            "Failed to create orphan branch: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Copy bundle contents
    for entry in fs::read_dir(bundle_dir)? {
        let entry = entry?;
        let dest = work_dir.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir_recursive(&entry.path(), &dest)?;
        } else {
            fs::copy(entry.path(), dest)?;
        }
    }

    // Add all files
    let output = Command::new("git")
        .current_dir(work_dir)
        .args(["add", "--force", "--all", "--", "."])
        .output()?;

    if !output.status.success() {
        anyhow::bail!(
            "Failed to stage files: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Commit
    let output = Command::new("git")
        .current_dir(work_dir)
        .args(["commit", "-m", "Deploy CASS archive"])
        .output()?;

    if !output.status.success() {
        anyhow::bail!(
            "Failed to commit: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Push
    let output = Command::new("git")
        .current_dir(work_dir)
        .args(["push", "-u", "origin", branch_name, "--force"])
        .output()?;

    if !output.status.success() {
        anyhow::bail!(
            "Failed to push: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Get commit SHA
    let output = Command::new("git")
        .current_dir(work_dir)
        .args(["rev-parse", "HEAD"])
        .output()?;

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

// =============================================================================
// GitHub Deployer Tests - Prerequisites and Size Checks
// =============================================================================

#[test]
fn e2e_github_prerequisites_validation() {
    let tracker = tracker_for("e2e_github_prerequisites_validation");
    let _guard = tracker.trace_env_guard();

    // Test 1: All prerequisites met
    let start = tracker.start("all_ready", Some("Test prerequisites all ready"));
    let prereqs = GhPrereqs {
        gh_version: Some("gh version 2.40.0".to_string()),
        gh_authenticated: true,
        gh_username: Some("testuser".to_string()),
        git_version: Some("git version 2.43.0".to_string()),
        disk_space_mb: 10000,
        estimated_size_mb: 100,
    };
    assert!(prereqs.is_ready(), "all prerequisites should be met");
    assert!(prereqs.missing().is_empty(), "no missing prerequisites");
    tracker.end("all_ready", Some("Test prerequisites all ready"), start);

    // Test 2: gh CLI missing
    let start = tracker.start("gh_missing", Some("Test gh CLI not installed"));
    let prereqs = GhPrereqs {
        gh_version: None,
        gh_authenticated: false,
        gh_username: None,
        git_version: Some("git version 2.43.0".to_string()),
        disk_space_mb: 10000,
        estimated_size_mb: 100,
    };
    assert!(!prereqs.is_ready(), "should not be ready without gh CLI");
    let missing = prereqs.missing();
    assert!(
        missing.iter().any(|m| m.contains("gh CLI")),
        "should mention gh CLI: {:?}",
        missing
    );
    tracker.end("gh_missing", Some("Test gh CLI not installed"), start);

    // Test 3: git missing
    let start = tracker.start("git_missing", Some("Test git not installed"));
    let prereqs = GhPrereqs {
        gh_version: Some("gh version 2.40.0".to_string()),
        gh_authenticated: true,
        gh_username: Some("testuser".to_string()),
        git_version: None,
        disk_space_mb: 10000,
        estimated_size_mb: 100,
    };
    assert!(!prereqs.is_ready(), "should not be ready without git");
    let missing = prereqs.missing();
    assert!(
        missing.iter().any(|m| m.contains("git")),
        "should mention git: {:?}",
        missing
    );
    tracker.end("git_missing", Some("Test git not installed"), start);

    // Test 4: Not authenticated
    let start = tracker.start("not_auth", Some("Test not authenticated"));
    let prereqs = GhPrereqs {
        gh_version: Some("gh version 2.40.0".to_string()),
        gh_authenticated: false,
        gh_username: None,
        git_version: Some("git version 2.43.0".to_string()),
        disk_space_mb: 10000,
        estimated_size_mb: 100,
    };
    assert!(!prereqs.is_ready(), "should not be ready without auth");
    let missing = prereqs.missing();
    assert!(
        missing.iter().any(|m| m.contains("authenticated")),
        "should mention authentication: {:?}",
        missing
    );
    tracker.end("not_auth", Some("Test not authenticated"), start);
}

#[test]
fn e2e_github_size_check() -> Result<()> {
    let tracker = tracker_for("e2e_github_size_check");
    let _guard = tracker.trace_env_guard();

    let temp_dir = TempDir::new()?;

    // Create a test bundle
    let start = tracker.start("create_bundle", Some("Create test bundle for size check"));
    let bundle_dir = create_test_bundle(temp_dir.path())?;
    tracker.end(
        "create_bundle",
        Some("Create test bundle for size check"),
        start,
    );

    // Check size
    let start = tracker.start("check_size", Some("Run size check on bundle"));
    let deployer = GitHubDeployer::new(TEST_REPO_NAME);
    let size_result = deployer.check_size(&bundle_dir)?;

    assert!(size_result.total_bytes > 0, "total bytes should be > 0");
    assert!(
        !size_result.exceeds_limit,
        "small test bundle should not exceed limit"
    );
    assert!(
        !size_result.has_oversized_files,
        "test bundle should not have oversized files"
    );
    tracker.end("check_size", Some("Run size check on bundle"), start);

    Ok(())
}

// =============================================================================
// Local Git Deployment Workflow Tests
// =============================================================================

#[test]
fn e2e_local_git_orphan_branch_workflow() -> Result<()> {
    let tracker = tracker_for("e2e_local_git_orphan_branch_workflow");
    let _guard = tracker.trace_env_guard();

    let temp_dir = TempDir::new()?;

    // Step 1: Create local bare repo
    let start = tracker.start("create_bare_repo", Some("Create local bare git repo"));
    let bare_repo = create_local_bare_repo(temp_dir.path(), TEST_REPO_NAME)?;
    assert!(bare_repo.exists(), "bare repo should exist");
    tracker.end(
        "create_bare_repo",
        Some("Create local bare git repo"),
        start,
    );

    // Step 2: Clone to work directory
    let start = tracker.start("clone_repo", Some("Clone to working directory"));
    let work_dir = temp_dir.path().join("work");
    clone_local_repo(&bare_repo, &work_dir)?;
    assert!(work_dir.join(".git").exists(), "work dir should have .git");
    tracker.end("clone_repo", Some("Clone to working directory"), start);

    // Step 3: Create test bundle
    let start = tracker.start("create_bundle", Some("Create test bundle"));
    let bundle_dir = create_test_bundle(temp_dir.path())?;
    assert!(
        bundle_dir.join("index.html").exists(),
        "bundle should have index.html"
    );
    tracker.end("create_bundle", Some("Create test bundle"), start);

    // Step 4: Create orphan branch and push
    let start = tracker.start("create_orphan_push", Some("Create orphan branch and push"));
    let commit_sha = create_and_push_orphan(&work_dir, "gh-pages", &bundle_dir)?;
    assert!(!commit_sha.is_empty(), "commit SHA should not be empty");
    tracker.end(
        "create_orphan_push",
        Some("Create orphan branch and push"),
        start,
    );

    // Step 5: Verify push to bare repo
    let start = tracker.start("verify_push", Some("Verify content in bare repo"));
    let verify_dir = temp_dir.path().join("verify");
    clone_local_repo(&bare_repo, &verify_dir)?;

    checkout_remote_branch(&verify_dir, "gh-pages")?;

    assert!(
        verify_dir.join("index.html").exists(),
        "pushed content should include index.html"
    );
    assert!(
        verify_dir.join("config.json").exists(),
        "pushed content should include config.json"
    );
    assert!(
        verify_dir.join("assets").is_dir(),
        "pushed content should include assets/"
    );
    tracker.end("verify_push", Some("Verify content in bare repo"), start);

    Ok(())
}

#[test]
fn e2e_local_git_bundle_copy_integrity() -> Result<()> {
    let tracker = tracker_for("e2e_local_git_bundle_copy_integrity");
    let _guard = tracker.trace_env_guard();

    let temp_dir = TempDir::new()?;

    // Create bundle with specific content
    let start = tracker.start("create_bundle", Some("Create bundle with known content"));
    let bundle_dir = temp_dir.path().join("bundle");
    fs::create_dir_all(&bundle_dir)?;

    let test_content = "Test content with special chars: <>&\"'\nLine 2\nLine 3";
    fs::write(bundle_dir.join("test.txt"), test_content)?;
    fs::write(
        bundle_dir.join("index.html"),
        "<html><body>Test</body></html>",
    )?;

    let nested = bundle_dir.join("nested/deep");
    fs::create_dir_all(&nested)?;
    fs::write(nested.join("file.json"), r#"{"key": "value"}"#)?;
    tracker.end(
        "create_bundle",
        Some("Create bundle with known content"),
        start,
    );

    // Create local git infrastructure
    let start = tracker.start("setup_git", Some("Setup local git repos"));
    let bare_repo = create_local_bare_repo(temp_dir.path(), "integrity-test")?;
    let work_dir = temp_dir.path().join("work");
    clone_local_repo(&bare_repo, &work_dir)?;
    tracker.end("setup_git", Some("Setup local git repos"), start);

    // Push bundle
    let start = tracker.start("push_bundle", Some("Push bundle to local repo"));
    let _commit_sha = create_and_push_orphan(&work_dir, "gh-pages", &bundle_dir)?;
    tracker.end("push_bundle", Some("Push bundle to local repo"), start);

    // Verify content integrity
    let start = tracker.start("verify_integrity", Some("Verify bundle content integrity"));
    // Check content matches
    let pushed_content = read_git_blob(&bare_repo, "gh-pages", "test.txt")?;
    assert_eq!(
        pushed_content, test_content,
        "pushed content should match original"
    );

    let pushed_nested = read_git_blob(&bare_repo, "gh-pages", "nested/deep/file.json")?;
    assert_eq!(
        pushed_nested, r#"{"key": "value"}"#,
        "nested content should match"
    );
    tracker.end(
        "verify_integrity",
        Some("Verify bundle content integrity"),
        start,
    );

    Ok(())
}

// =============================================================================
// Cloudflare Deployer Tests - Prerequisites and Headers
// =============================================================================

#[test]
fn e2e_cloudflare_prerequisites_validation() {
    let tracker = tracker_for("e2e_cloudflare_prerequisites_validation");
    let _guard = tracker.trace_env_guard();

    // Test 1: All prerequisites met with interactive auth
    let start = tracker.start("interactive_auth", Some("Test with interactive auth"));
    let prereqs = CfPrereqs {
        wrangler_version: Some("wrangler 3.0.0".to_string()),
        wrangler_authenticated: true,
        account_email: Some("test@example.com".to_string()),
        api_credentials_present: false,
        account_id: None,
        disk_space_mb: 10000,
    };
    assert!(prereqs.is_ready(), "should be ready with interactive auth");
    assert!(prereqs.missing().is_empty(), "no missing prerequisites");
    tracker.end(
        "interactive_auth",
        Some("Test with interactive auth"),
        start,
    );

    // Test 2: All prerequisites met with API credentials
    let start = tracker.start("api_auth", Some("Test with API credentials"));
    let prereqs = CfPrereqs {
        wrangler_version: Some("wrangler 3.0.0".to_string()),
        wrangler_authenticated: false,
        account_email: None,
        api_credentials_present: true,
        account_id: Some("abc123".to_string()),
        disk_space_mb: 10000,
    };
    assert!(prereqs.is_ready(), "should be ready with API credentials");
    tracker.end("api_auth", Some("Test with API credentials"), start);

    // Test 3: Wrangler not installed
    let start = tracker.start("wrangler_missing", Some("Test wrangler not installed"));
    let prereqs = CfPrereqs {
        wrangler_version: None,
        wrangler_authenticated: false,
        account_email: None,
        api_credentials_present: false,
        account_id: None,
        disk_space_mb: 10000,
    };
    assert!(!prereqs.is_ready(), "should not be ready without wrangler");
    let missing = prereqs.missing();
    assert!(
        missing
            .iter()
            .any(|m| m.contains("wrangler") || m.contains("install")),
        "should mention wrangler: {:?}",
        missing
    );
    tracker.end(
        "wrangler_missing",
        Some("Test wrangler not installed"),
        start,
    );

    // Test 4: Not authenticated (neither method)
    let start = tracker.start("not_auth", Some("Test not authenticated"));
    let prereqs = CfPrereqs {
        wrangler_version: Some("wrangler 3.0.0".to_string()),
        wrangler_authenticated: false,
        account_email: None,
        api_credentials_present: false,
        account_id: None,
        disk_space_mb: 10000,
    };
    assert!(!prereqs.is_ready(), "should not be ready without any auth");
    tracker.end("not_auth", Some("Test not authenticated"), start);
}

#[test]
fn e2e_cloudflare_headers_generation() -> Result<()> {
    let tracker = tracker_for("e2e_cloudflare_headers_generation");
    let _guard = tracker.trace_env_guard();

    let temp_dir = TempDir::new()?;
    let bundle_dir = temp_dir.path().join("bundle");
    fs::create_dir_all(&bundle_dir)?;

    // Generate headers
    let start = tracker.start(
        "generate_headers",
        Some("Generate Cloudflare _headers file"),
    );
    let deployer = CloudflareDeployer::default();
    deployer.generate_headers_file(&bundle_dir)?;

    let headers_path = bundle_dir.join("_headers");
    assert!(headers_path.exists(), "_headers file should be created");
    tracker.end(
        "generate_headers",
        Some("Generate Cloudflare _headers file"),
        start,
    );

    // Verify headers content
    let start = tracker.start("verify_headers", Some("Verify headers content"));
    let content = fs::read_to_string(&headers_path)?;

    // Check COOP/COEP headers (critical for SharedArrayBuffer)
    assert!(
        content.contains("Cross-Origin-Opener-Policy"),
        "should include COOP header"
    );
    assert!(
        content.contains("Cross-Origin-Embedder-Policy"),
        "should include COEP header"
    );
    assert!(
        content.contains("same-origin"),
        "COOP should be same-origin"
    );
    assert!(
        content.contains("require-corp"),
        "COEP should be require-corp"
    );

    // Check security headers
    assert!(
        content.contains("X-Content-Type-Options"),
        "should include content type options"
    );
    assert!(content.contains("nosniff"), "should have nosniff");
    assert!(
        content.contains("X-Frame-Options"),
        "should include frame options"
    );

    // Check caching headers
    assert!(
        content.contains("Cache-Control"),
        "should include cache control"
    );
    tracker.end("verify_headers", Some("Verify headers content"), start);

    Ok(())
}

#[test]
fn e2e_cloudflare_redirects_generation() -> Result<()> {
    let tracker = tracker_for("e2e_cloudflare_redirects_generation");
    let _guard = tracker.trace_env_guard();

    let temp_dir = TempDir::new()?;
    let bundle_dir = temp_dir.path().join("bundle");
    fs::create_dir_all(&bundle_dir)?;

    // Generate redirects
    let start = tracker.start(
        "generate_redirects",
        Some("Generate Cloudflare _redirects file"),
    );
    let deployer = CloudflareDeployer::default();
    deployer.generate_redirects_file(&bundle_dir)?;

    let redirects_path = bundle_dir.join("_redirects");
    assert!(redirects_path.exists(), "_redirects file should be created");
    tracker.end(
        "generate_redirects",
        Some("Generate Cloudflare _redirects file"),
        start,
    );

    // Verify redirects content
    let start = tracker.start("verify_redirects", Some("Verify redirects content"));
    let content = fs::read_to_string(&redirects_path)?;

    // Check SPA fallback
    assert!(
        content.contains("/* /index.html 200"),
        "should include SPA fallback rule"
    );
    tracker.end("verify_redirects", Some("Verify redirects content"), start);

    Ok(())
}

// =============================================================================
// Combined Deploy Pipeline Tests
// =============================================================================

#[test]
fn e2e_full_deploy_bundle_preparation() -> Result<()> {
    let tracker = tracker_for("e2e_full_deploy_bundle_preparation");
    let _guard = tracker.trace_env_guard();

    let temp_dir = TempDir::new()?;

    // Create test bundle
    let start = tracker.start("create_bundle", Some("Create test bundle"));
    let bundle_dir = create_test_bundle(temp_dir.path())?;
    tracker.end("create_bundle", Some("Create test bundle"), start);

    // Add Cloudflare files
    let start = tracker.start("add_cf_files", Some("Add Cloudflare-specific files"));
    let deployer = CloudflareDeployer::default();
    deployer.generate_headers_file(&bundle_dir)?;
    deployer.generate_redirects_file(&bundle_dir)?;
    tracker.end("add_cf_files", Some("Add Cloudflare-specific files"), start);

    // Verify complete bundle
    let start = tracker.start("verify_bundle", Some("Verify complete bundle"));
    assert!(
        bundle_dir.join("index.html").exists(),
        "bundle should have index.html"
    );
    assert!(
        bundle_dir.join("config.json").exists(),
        "bundle should have config.json"
    );
    assert!(
        bundle_dir.join("_headers").exists(),
        "bundle should have _headers"
    );
    assert!(
        bundle_dir.join("_redirects").exists(),
        "bundle should have _redirects"
    );
    assert!(
        bundle_dir.join("assets").is_dir(),
        "bundle should have assets/"
    );
    tracker.end("verify_bundle", Some("Verify complete bundle"), start);

    // Size check
    let start = tracker.start("size_check", Some("Run size check on complete bundle"));
    let gh_deployer = GitHubDeployer::new(TEST_REPO_NAME);
    let size_result = gh_deployer.check_size(&bundle_dir)?;
    assert!(
        !size_result.exceeds_limit,
        "bundle should not exceed size limit"
    );
    tracker.end(
        "size_check",
        Some("Run size check on complete bundle"),
        start,
    );

    // Deploy to local git
    let start = tracker.start("local_deploy", Some("Deploy to local git"));
    let bare_repo = create_local_bare_repo(temp_dir.path(), "full-deploy-test")?;
    let work_dir = temp_dir.path().join("work");
    clone_local_repo(&bare_repo, &work_dir)?;
    let commit_sha = create_and_push_orphan(&work_dir, "gh-pages", &bundle_dir)?;
    assert!(!commit_sha.is_empty(), "should have commit SHA");
    tracker.end("local_deploy", Some("Deploy to local git"), start);

    Ok(())
}

#[test]
fn e2e_deploy_error_paths() -> Result<()> {
    let tracker = tracker_for("e2e_deploy_error_paths");
    let _guard = tracker.trace_env_guard();

    let _temp_dir = TempDir::new()?;

    // Test 1: Size check on non-existent directory
    let start = tracker.start(
        "nonexistent_dir",
        Some("Size check on non-existent directory"),
    );
    let deployer = GitHubDeployer::new(TEST_REPO_NAME);
    let result = deployer.check_size(Path::new("/nonexistent/path/12345"));
    assert!(result.is_err(), "should error on non-existent path");
    tracker.end(
        "nonexistent_dir",
        Some("Size check on non-existent directory"),
        start,
    );

    // Test 2: Headers generation on non-existent directory
    let start = tracker.start("headers_error", Some("Headers generation error path"));
    let cf_deployer = CloudflareDeployer::default();
    let result = cf_deployer.generate_headers_file(Path::new("/nonexistent/path/12345"));
    assert!(result.is_err(), "should error on non-existent path");
    tracker.end(
        "headers_error",
        Some("Headers generation error path"),
        start,
    );

    // Test 3: Redirects generation on non-existent directory
    let start = tracker.start("redirects_error", Some("Redirects generation error path"));
    let result = cf_deployer.generate_redirects_file(Path::new("/nonexistent/path/12345"));
    assert!(result.is_err(), "should error on non-existent path");
    tracker.end(
        "redirects_error",
        Some("Redirects generation error path"),
        start,
    );

    Ok(())
}
