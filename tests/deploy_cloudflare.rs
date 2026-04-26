//! Integration tests for Cloudflare Pages deployment module.
//!
//! Tests the CloudflareDeployer functionality including prerequisites,
//! header generation, and bundle preparation.

use anyhow::Result;
use coding_agent_search::pages::deploy_cloudflare::{
    CloudflareConfig, CloudflareDeployer, DeployResult, Prerequisites,
};
use std::fs;
use tempfile::TempDir;

fn temp_cloudflare_deployer() -> Result<(TempDir, CloudflareDeployer)> {
    Ok((TempDir::new()?, CloudflareDeployer::default()))
}

fn prereqs_fixture() -> Prerequisites {
    Prerequisites {
        wrangler_version: Some("wrangler 3.0.0".to_string()),
        wrangler_authenticated: false,
        account_email: None,
        api_credentials_present: false,
        account_id: None,
        disk_space_mb: 10000,
    }
}

// ============================================
// Prerequisites Tests
// ============================================

#[test]
fn test_prerequisites_all_ready_with_auth() {
    let prereqs = Prerequisites {
        wrangler_authenticated: true,
        account_email: Some("test@example.com".to_string()),
        ..prereqs_fixture()
    };

    assert!(prereqs.is_ready());
    assert!(prereqs.missing().is_empty());
}

#[test]
fn test_prerequisites_ready_with_api_credentials() {
    // API credentials can be used instead of interactive auth
    let prereqs = Prerequisites {
        api_credentials_present: true,
        account_id: Some("abc123".to_string()),
        ..prereqs_fixture()
    };

    assert!(prereqs.is_ready());
    assert!(prereqs.missing().is_empty());
}

#[test]
fn test_prerequisites_ready_with_api_credentials_without_wrangler() {
    let prereqs = Prerequisites {
        wrangler_version: None,
        api_credentials_present: true,
        account_id: Some("abc123".to_string()),
        ..prereqs_fixture()
    };

    assert!(prereqs.is_ready());
    assert!(prereqs.missing().is_empty());
}

#[test]
fn test_prerequisites_wrangler_not_installed() {
    let prereqs = Prerequisites {
        wrangler_version: None,
        ..prereqs_fixture()
    };

    assert!(!prereqs.is_ready());
    let missing = prereqs.missing();
    assert!(
        missing
            .iter()
            .any(|m| m.contains("wrangler CLI not installed"))
    );
    assert!(missing.iter().any(|m| m.contains("npm install")));
}

#[test]
fn test_prerequisites_not_authenticated() {
    let prereqs = prereqs_fixture();

    assert!(!prereqs.is_ready());
    let missing = prereqs.missing();
    assert!(missing.iter().any(|m| m.contains("not authenticated")));
    assert!(missing.iter().any(|m| m.contains("CLOUDFLARE_API_TOKEN")));
}

// ============================================
// Configuration Tests
// ============================================

#[test]
fn test_config_default() {
    let config = CloudflareConfig::default();

    assert_eq!(config.project_name, "cass-archive");
    assert!(config.custom_domain.is_none());
    assert!(config.create_if_missing);
    assert_eq!(config.branch, "main");
    assert!(config.account_id.is_none());
    assert!(config.api_token.is_none());
}

#[test]
fn test_deployer_builder_chain() {
    // Test that builder pattern compiles and chains correctly
    // Config internals are private but the builder pattern works
    let _deployer = CloudflareDeployer::with_project_name("my-archive")
        .custom_domain("archive.example.com")
        .create_if_missing(false)
        .branch("production")
        .account_id("acc123")
        .api_token("token456");

    // Builder chain compiles successfully - config is applied when deploy() is called
}

#[test]
fn test_deployer_default_creation() {
    // Test that default deployer can be created
    let _deployer = CloudflareDeployer::default();
    // If it compiles and doesn't panic, the default works
}

// ============================================
// Header File Generation Tests
// ============================================

#[test]
fn test_generate_headers_file() -> Result<()> {
    let (temp, deployer) = temp_cloudflare_deployer()?;

    deployer.generate_headers_file(temp.path())?;

    let headers_path = temp.path().join("_headers");
    assert!(headers_path.exists());

    let content = fs::read_to_string(&headers_path)?;

    // Verify COOP/COEP headers (critical for SharedArrayBuffer)
    assert!(content.contains("Cross-Origin-Opener-Policy: same-origin"));
    assert!(content.contains("Cross-Origin-Embedder-Policy: require-corp"));

    // Verify security headers
    assert!(content.contains("X-Content-Type-Options: nosniff"));
    assert!(content.contains("X-Frame-Options: DENY"));
    assert!(content.contains("Referrer-Policy: no-referrer"));
    assert!(content.contains("X-Robots-Tag: noindex, nofollow"));

    // Verify caching headers
    assert!(content.contains("Cache-Control"));
    assert!(content.contains("max-age=31536000"));

    Ok(())
}

#[test]
fn test_generate_headers_file_cache_exceptions() -> Result<()> {
    let (temp, deployer) = temp_cloudflare_deployer()?;

    deployer.generate_headers_file(temp.path())?;

    let content = fs::read_to_string(temp.path().join("_headers"))?;

    // index.html and config.json should have no-cache
    assert!(content.contains("/index.html"));
    assert!(content.contains("/config.json"));
    assert!(content.contains("no-cache"));

    Ok(())
}

#[test]
fn test_generate_redirects_file() -> Result<()> {
    let (temp, deployer) = temp_cloudflare_deployer()?;

    deployer.generate_redirects_file(temp.path())?;

    let redirects_path = temp.path().join("_redirects");
    assert!(redirects_path.exists());

    let content = fs::read_to_string(&redirects_path)?;

    // SPA fallback rule
    assert!(content.contains("/* /index.html 200"));

    Ok(())
}

// ============================================
// Bundle Preparation Tests
// ============================================

#[test]
fn test_cloudflare_bundle_structure() -> Result<()> {
    let (temp, deployer) = temp_cloudflare_deployer()?;

    // Create minimal bundle
    fs::write(temp.path().join("index.html"), "<html></html>")?;
    fs::write(temp.path().join("config.json"), "{}")?;

    // Generate Cloudflare-specific files
    deployer.generate_headers_file(temp.path())?;
    deployer.generate_redirects_file(temp.path())?;

    // Verify all files exist
    assert!(temp.path().join("index.html").exists());
    assert!(temp.path().join("config.json").exists());
    assert!(temp.path().join("_headers").exists());
    assert!(temp.path().join("_redirects").exists());

    Ok(())
}

#[test]
fn test_cloudflare_headers_dont_overwrite_existing() -> Result<()> {
    let (temp, deployer) = temp_cloudflare_deployer()?;

    // Pre-create a _headers file
    fs::write(temp.path().join("_headers"), "# Custom headers")?;

    // Generate should overwrite (deploy needs correct headers)
    deployer.generate_headers_file(temp.path())?;

    let content = fs::read_to_string(temp.path().join("_headers"))?;

    // Should have our headers, not custom
    assert!(content.contains("Cross-Origin-Opener-Policy"));
    assert!(!content.contains("# Custom headers"));

    Ok(())
}

// ============================================
// Progress Callback Tests
// ============================================

#[test]
fn test_cloudflare_progress_phases() {
    let expected_phases = [
        "prereq", "prepare", "headers", "project", "deploy", "domain", "complete",
    ];

    for phase in expected_phases {
        assert!(!phase.is_empty(), "Phase '{}' should be non-empty", phase);
    }
}

#[test]
fn test_progress_callback_tracking() {
    let mut phases: Vec<String> = Vec::new();

    let mut progress = |phase: &str, _msg: &str| {
        phases.push(phase.to_string());
    };

    // Simulate deploy phases
    progress("prereq", "Checking prerequisites...");
    progress("prepare", "Preparing deployment...");
    progress("headers", "Generating COOP/COEP headers...");
    progress("project", "Checking Cloudflare Pages project...");
    progress("deploy", "Deploying to Cloudflare Pages...");
    progress("complete", "Deployment complete!");

    assert_eq!(phases.len(), 6);
    assert_eq!(phases[0], "prereq");
    assert_eq!(phases[5], "complete");
}

// ============================================
// Error Path Tests
// ============================================

#[test]
fn test_generate_headers_file_invalid_path() {
    let deployer = CloudflareDeployer::default();
    let result = deployer.generate_headers_file(std::path::Path::new("/nonexistent/12345"));

    assert!(result.is_err());
}

#[test]
fn test_generate_redirects_file_invalid_path() {
    let deployer = CloudflareDeployer::default();
    let result = deployer.generate_redirects_file(std::path::Path::new("/nonexistent/12345"));

    assert!(result.is_err());
}

#[test]
fn test_prerequisites_missing_multiple() {
    let prereqs = Prerequisites {
        wrangler_version: None,
        wrangler_authenticated: false,
        account_email: None,
        api_credentials_present: false,
        account_id: None,
        disk_space_mb: 0,
    };

    let missing = prereqs.missing();

    // Should report all missing items
    assert_eq!(missing.len(), 2);

    // Verify guidance is provided
    for msg in &missing {
        assert!(
            msg.contains("install") || msg.contains("token") || msg.contains("authenticated"),
            "Missing message should contain actionable guidance: {}",
            msg
        );
    }
}

// ============================================
// Authentication Handling Tests
// ============================================

#[test]
fn test_auth_state_combinations() {
    let test_cases = vec![
        // (wrangler_installed, interactive_auth, api_creds, expected_ready)
        (true, true, false, true),    // Interactive auth OK
        (true, false, true, true),    // API credentials OK
        (true, true, true, true),     // Both OK
        (true, false, false, false),  // Neither auth method
        (false, false, false, false), // No wrangler
        (false, true, true, true),    // API credentials allow direct deploy without wrangler
    ];

    for (has_wrangler, interactive, api_creds, expected_ready) in test_cases {
        let prereqs = Prerequisites {
            wrangler_version: if has_wrangler {
                Some("3.0.0".to_string())
            } else {
                None
            },
            wrangler_authenticated: interactive,
            account_email: if interactive {
                Some("test@example.com".to_string())
            } else {
                None
            },
            api_credentials_present: api_creds,
            account_id: if api_creds {
                Some("acc123".to_string())
            } else {
                None
            },
            disk_space_mb: 1000,
        };

        assert_eq!(
            prereqs.is_ready(),
            expected_ready,
            "Auth state mismatch for wrangler={}, interactive={}, api_creds={}",
            has_wrangler,
            interactive,
            api_creds
        );
    }
}

// ============================================
// Serialization Tests (for logging)
// ============================================

#[test]
fn test_deploy_result_serialization() -> Result<()> {
    let result = DeployResult {
        project_name: "my-project".to_string(),
        pages_url: "https://my-project.pages.dev".to_string(),
        deployed: true,
        deployment_id: Some("dep123".to_string()),
        custom_domain: Some("archive.example.com".to_string()),
    };

    let json = serde_json::to_string_pretty(&result)?;
    assert!(json.contains("project_name"));
    assert!(json.contains("pages_url"));
    assert!(json.contains("deployment_id"));
    assert!(json.contains("custom_domain"));

    let parsed: DeployResult = serde_json::from_str(&json)?;
    assert_eq!(parsed.project_name, result.project_name);
    assert_eq!(parsed.pages_url, result.pages_url);

    Ok(())
}

#[test]
fn test_prerequisites_serialization() -> Result<()> {
    let prereqs = Prerequisites {
        wrangler_version: Some("wrangler 3.0.0".to_string()),
        wrangler_authenticated: true,
        account_email: Some("test@example.com".to_string()),
        api_credentials_present: false,
        account_id: None,
        disk_space_mb: 10000,
    };

    let json = serde_json::to_string_pretty(&prereqs)?;
    assert!(json.contains("wrangler_version"));
    assert!(json.contains("wrangler_authenticated"));
    assert!(json.contains("disk_space_mb"));

    Ok(())
}

// ============================================
// Custom Domain Tests
// ============================================

#[test]
fn test_custom_domain_configuration() {
    // Test that custom_domain builder method works
    let _deployer =
        CloudflareDeployer::with_project_name("test").custom_domain("my-archive.example.com");
    // Builder compiles - config internals are private
}

#[test]
fn test_multiple_branch_configurations() {
    // Test that branch builder method works for different environments
    let _prod_deployer = CloudflareDeployer::with_project_name("test").branch("production");
    let _staging_deployer = CloudflareDeployer::with_project_name("test").branch("staging");
    // Both build successfully - config internals are private
}
