//! E2E Install Easy Mode Tests
//!
//! Validates install.sh logic using the real system toolchain (rustc, cargo,
//! sha256sum). Runs install.sh against a fixture tarball in an isolated temp
//! HOME, verifying checksum verification, unpacking, and path setup.
//!
//! ## Running Tests
//!
//! These tests are skipped by default locally. To run them:
//! ```bash
//! E2E_INSTALL_TESTS=1 cargo test --test e2e_install_easy
//! ```
//!
//! On CI, these tests run automatically via `.github/workflows/install-test.yml`
//! which builds a real release binary end-to-end.
//!
//! ## Artifact Storage
//!
//! All test artifacts are stored in `test-results/e2e/e2e_install_easy/<test>/`:
//! - `stdout` - Installer stdout capture
//! - `stderr` - Installer stderr capture
//! - `cass.log` - Structured JSONL events
//! - `trace.jsonl` - CLI trace spans
//! - `checksum.txt` - Binary checksum (if applicable)

use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

mod util;
use util::e2e_log::{E2eError, E2eErrorContext, E2ePerformanceMetrics, PhaseTracker};

// ============================================
// Test Helpers
// ============================================

/// Check if install tests should run (skip locally unless E2E_INSTALL_TESTS is set)
fn should_run_install_tests() -> bool {
    std::env::var("E2E_INSTALL_TESTS").is_ok()
        || std::env::var("CI").is_ok()
        || std::env::var("GITHUB_ACTIONS").is_ok()
}

/// Skip test if not in CI or E2E_INSTALL_TESTS not set
macro_rules! skip_unless_install_tests {
    () => {
        if !should_run_install_tests() {
            eprintln!("Skipping: set E2E_INSTALL_TESTS=1 to run locally");
            return;
        }
    };
}

fn truncate_output(bytes: &[u8], max_len: usize) -> String {
    let s = String::from_utf8_lossy(bytes);
    if s.len() > max_len {
        format!(
            "{}... [truncated {} bytes]",
            &s[..max_len],
            s.len() - max_len
        )
    } else {
        s.to_string()
    }
}

fn tracker_for(test_name: &str) -> PhaseTracker {
    PhaseTracker::new("e2e_install_easy", test_name)
}

fn fixture(name: &str) -> PathBuf {
    fs::canonicalize(PathBuf::from(name)).expect("fixture path")
}

/// Save stdout/stderr artifacts to the test artifact directory
fn save_command_artifacts(tracker: &PhaseTracker, stdout: &[u8], stderr: &[u8]) {
    let artifacts = tracker.artifacts();

    // Write stdout
    if let Ok(mut f) = File::create(&artifacts.stdout_path) {
        let _ = f.write_all(stdout);
    }

    // Write stderr
    if let Ok(mut f) = File::create(&artifacts.stderr_path) {
        let _ = f.write_all(stderr);
    }
}

/// Compute SHA256 checksum of a file and save to artifact
fn save_binary_checksum(tracker: &PhaseTracker, binary_path: &std::path::Path) -> Option<String> {
    let binary_content = fs::read(binary_path).ok()?;
    let checksum = sha256_hex(&binary_content);

    let checksum_path = tracker.artifacts().dir.join("checksum.txt");
    if let Ok(mut f) = File::create(&checksum_path) {
        let _ = writeln!(f, "{}  {}", checksum, binary_path.display());
    }

    Some(checksum)
}

/// Compute SHA256 hex string for bytes
fn sha256_hex(data: &[u8]) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    // Use a simple hash for test purposes (not cryptographic)
    // Real checksum verification is done by sha256sum in install.sh
    let mut hasher = DefaultHasher::new();
    data.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Clean up stale install lock (tests may leave locks behind if they crash)
fn cleanup_stale_install_lock() {
    let lock_dir = std::path::Path::new("/tmp/coding-agent-search-install.lock.d");
    if lock_dir.exists() {
        // Check if the lock is stale (PID not running)
        let pid_file = lock_dir.join("pid");
        if let Ok(pid_str) = fs::read_to_string(&pid_file) {
            let pid = pid_str.trim();
            if !pid.is_empty() {
                // Use kill -0 to check if process is still running
                let is_running = Command::new("kill")
                    .arg("-0")
                    .arg(pid)
                    .output()
                    .is_ok_and(|o| o.status.success());
                if !is_running {
                    // Stale lock, remove it
                    let _ = fs::remove_dir_all(lock_dir);
                }
            }
        }
    }
}

// ============================================
// Fixture Helpers
// ============================================

struct InstallFixture {
    tar_path: PathBuf,
    checksum: String,
    temp_home: tempfile::TempDir,
    dest: tempfile::TempDir,
}

impl InstallFixture {
    fn new() -> Self {
        // Clean up any stale locks from previous test runs
        cleanup_stale_install_lock();

        let tar_path =
            fixture("tests/fixtures/install/coding-agent-search-vtest-linux-x86_64.tar.gz");
        let checksum = fs::read_to_string(
            "tests/fixtures/install/coding-agent-search-vtest-linux-x86_64.tar.gz.sha256",
        )
        .expect("read checksum file")
        .trim()
        .to_string();

        let temp_home = tempfile::TempDir::new().expect("create temp HOME");
        let dest = tempfile::TempDir::new().expect("create dest dir");

        Self {
            tar_path,
            checksum,
            temp_home,
            dest,
        }
    }

    fn artifact_url(&self) -> String {
        format!("file://{}", self.tar_path.display())
    }

    fn binary_path(&self) -> PathBuf {
        self.dest.path().join("cass")
    }
}

// ============================================
// Happy Path Tests
// ============================================

/// Full install flow with --easy-mode and --verify flags
#[test]
#[cfg_attr(not(target_os = "linux"), ignore)]
fn install_easy_mode_end_to_end() {
    skip_unless_install_tests!();

    let tracker = tracker_for("install_easy_mode_end_to_end");
    let _trace_guard = tracker.trace_env_guard();

    // Phase: Setup isolated test environment
    let phase_start = tracker.start("setup", Some("Create isolated test environment"));
    let fix = InstallFixture::new();
    tracker.end(
        "setup",
        Some("Create isolated test environment"),
        phase_start,
    );

    // Phase: Run install.sh with real system toolchain
    let phase_start = tracker.start("run_install", Some("Run install.sh with real toolchain"));
    let install_start = std::time::Instant::now();

    let output = Command::new("timeout")
        .arg("30s")
        .arg("bash")
        .arg("install.sh")
        .arg("--version")
        .arg("vtest")
        .arg("--easy-mode")
        .arg("--verify")
        .arg("--dest")
        .arg(fix.dest.path())
        .env("HOME", fix.temp_home.path())
        .env("ARTIFACT_URL", fix.artifact_url())
        .env("CHECKSUM", &fix.checksum)
        .env("RUSTUP_INIT_SKIP", "1")
        .output()
        .expect("run installer");

    let install_duration = install_start.elapsed().as_millis() as u64;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Save artifacts
    save_command_artifacts(&tracker, &output.stdout, &output.stderr);

    if !output.status.success() {
        let ctx = E2eErrorContext::new()
            .with_command("bash install.sh --version vtest --easy-mode --verify")
            .capture_cwd()
            .add_state("exit_code", serde_json::json!(output.status.code()))
            .add_state(
                "stdout_tail",
                serde_json::json!(truncate_output(&output.stdout, 1000)),
            )
            .add_state(
                "stderr_tail",
                serde_json::json!(truncate_output(&output.stderr, 1000)),
            );
        tracker.fail(E2eError::with_type("install.sh failed", "COMMAND_FAILED").with_context(ctx));
        assert!(
            output.status.success(),
            "install.sh failed (exit {:?})\nstdout: {stdout}\nstderr: {stderr}",
            output.status.code()
        );
        return;
    }
    tracker.end(
        "run_install",
        Some("Run install.sh with real toolchain"),
        phase_start,
    );

    // Phase: Verify installation artifacts and checksums
    let phase_start = tracker.start("verify_install", Some("Verify binary and checksums"));

    let bin = fix.binary_path();
    assert!(
        bin.exists(),
        "Binary not found at expected path: {}",
        bin.display()
    );

    // Save binary checksum
    let binary_checksum = save_binary_checksum(&tracker, &bin);
    assert!(
        binary_checksum.is_some(),
        "Failed to compute binary checksum"
    );

    // Verify self-test output from install --verify
    assert!(
        stdout.contains("fixture-linux"),
        "Expected fixture-linux in stdout: {stdout}"
    );
    assert!(
        stdout.contains("Done. Run: cass"),
        "Expected completion message in stdout: {stdout}"
    );

    // Verify installed binary runs
    let help_output = Command::new(&bin)
        .arg("--help")
        .output()
        .expect("run binary --help");
    assert!(
        help_output.status.success(),
        "Binary --help should succeed (exit {:?})",
        help_output.status.code()
    );

    // Verify installed binary content matches fixture source
    let installed_binary = fs::read(&bin).expect("read installed binary");
    let fixture_binary =
        fs::read("tests/fixtures/install/coding-agent-search").expect("read fixture binary");
    assert_eq!(
        installed_binary, fixture_binary,
        "Installed binary should match fixture binary (checksum mismatch)"
    );

    tracker.end(
        "verify_install",
        Some("Verify binary and checksums"),
        phase_start,
    );

    tracker.metrics(
        "install_easy_mode",
        &E2ePerformanceMetrics::new()
            .with_duration(install_duration)
            .with_custom("stdout_lines", serde_json::json!(stdout.lines().count()))
            .with_custom("stderr_lines", serde_json::json!(stderr.lines().count()))
            .with_custom("binary_size", serde_json::json!(installed_binary.len())),
    );

    tracker.complete();
}

/// Install without --easy-mode (basic install)
#[test]
#[cfg_attr(not(target_os = "linux"), ignore)]
fn install_basic_mode() {
    skip_unless_install_tests!();

    let tracker = tracker_for("install_basic_mode");
    let _trace_guard = tracker.trace_env_guard();

    let phase_start = tracker.start("setup", Some("Create isolated test environment"));
    let fix = InstallFixture::new();
    tracker.end(
        "setup",
        Some("Create isolated test environment"),
        phase_start,
    );

    let phase_start = tracker.start("run_install", Some("Run install.sh in basic mode"));
    let install_start = std::time::Instant::now();

    let output = Command::new("timeout")
        .arg("30s")
        .arg("bash")
        .arg("install.sh")
        .arg("--version")
        .arg("vtest")
        .arg("--dest")
        .arg(fix.dest.path())
        .env("HOME", fix.temp_home.path())
        .env("ARTIFACT_URL", fix.artifact_url())
        .env("CHECKSUM", &fix.checksum)
        .env("RUSTUP_INIT_SKIP", "1")
        .output()
        .expect("run installer");

    let install_duration = install_start.elapsed().as_millis() as u64;
    save_command_artifacts(&tracker, &output.stdout, &output.stderr);

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "install.sh basic mode failed (exit {:?})\nstdout: {stdout}\nstderr: {stderr}",
        output.status.code()
    );
    tracker.end(
        "run_install",
        Some("Run install.sh in basic mode"),
        phase_start,
    );

    // Verify binary exists
    let phase_start = tracker.start("verify", Some("Verify installation"));
    let bin = fix.binary_path();
    assert!(bin.exists(), "Binary not found at {}", bin.display());

    // Verify binary runs
    let help_output = Command::new(&bin)
        .arg("--help")
        .output()
        .expect("run --help");
    assert!(
        help_output.status.success(),
        "Installed binary --help should succeed"
    );

    save_binary_checksum(&tracker, &bin);
    tracker.end("verify", Some("Verify installation"), phase_start);

    tracker.metrics(
        "install_basic_mode",
        &E2ePerformanceMetrics::new().with_duration(install_duration),
    );

    tracker.complete();
}

/// Install with --quiet flag (minimal output)
#[test]
#[cfg_attr(not(target_os = "linux"), ignore)]
fn install_quiet_mode() {
    skip_unless_install_tests!();

    let tracker = tracker_for("install_quiet_mode");
    let _trace_guard = tracker.trace_env_guard();

    let phase_start = tracker.start("setup", Some("Create isolated test environment"));
    let fix = InstallFixture::new();
    tracker.end(
        "setup",
        Some("Create isolated test environment"),
        phase_start,
    );

    let phase_start = tracker.start("run_install", Some("Run install.sh with --quiet"));

    let output = Command::new("timeout")
        .arg("30s")
        .arg("bash")
        .arg("install.sh")
        .arg("--version")
        .arg("vtest")
        .arg("--quiet")
        .arg("--dest")
        .arg(fix.dest.path())
        .env("HOME", fix.temp_home.path())
        .env("ARTIFACT_URL", fix.artifact_url())
        .env("CHECKSUM", &fix.checksum)
        .env("RUSTUP_INIT_SKIP", "1")
        .output()
        .expect("run installer");

    save_command_artifacts(&tracker, &output.stdout, &output.stderr);

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "install.sh --quiet failed (exit {:?})\nstdout: {stdout}\nstderr: {stderr}",
        output.status.code()
    );
    tracker.end(
        "run_install",
        Some("Run install.sh with --quiet"),
        phase_start,
    );

    // Verify binary exists
    let bin = fix.binary_path();
    assert!(bin.exists(), "Binary not found at {}", bin.display());

    // Quiet mode should produce minimal output
    assert!(
        stdout.is_empty() || stdout.lines().count() < 3,
        "Quiet mode should produce minimal output, got {} lines: {stdout}",
        stdout.lines().count()
    );

    tracker.complete();
}

// ============================================
// Error Path Tests
// ============================================

/// Install with checksum mismatch should fail
#[test]
#[cfg_attr(not(target_os = "linux"), ignore)]
fn install_checksum_mismatch_fails() {
    skip_unless_install_tests!();

    let tracker = tracker_for("install_checksum_mismatch_fails");
    let _trace_guard = tracker.trace_env_guard();

    let phase_start = tracker.start("setup", Some("Create isolated test environment"));
    let fix = InstallFixture::new();
    tracker.end(
        "setup",
        Some("Create isolated test environment"),
        phase_start,
    );

    let phase_start = tracker.start("run_install", Some("Run install.sh with bad checksum"));

    // Use an invalid checksum
    let bad_checksum = "0000000000000000000000000000000000000000000000000000000000000000";

    let output = Command::new("timeout")
        .arg("30s")
        .arg("bash")
        .arg("install.sh")
        .arg("--version")
        .arg("vtest")
        .arg("--dest")
        .arg(fix.dest.path())
        .env("HOME", fix.temp_home.path())
        .env("ARTIFACT_URL", fix.artifact_url())
        .env("CHECKSUM", bad_checksum)
        .env("RUSTUP_INIT_SKIP", "1")
        .output()
        .expect("run installer");

    save_command_artifacts(&tracker, &output.stdout, &output.stderr);

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    tracker.end(
        "run_install",
        Some("Run install.sh with bad checksum"),
        phase_start,
    );

    // Should fail due to checksum mismatch
    assert!(
        !output.status.success(),
        "install.sh should fail with bad checksum, but succeeded"
    );

    // Should mention checksum in error
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.to_lowercase().contains("checksum")
            || combined.contains("sha256sum")
            || combined.contains("mismatch"),
        "Error output should mention checksum failure: {combined}"
    );

    // Binary should not be installed
    let bin = fix.binary_path();
    assert!(
        !bin.exists(),
        "Binary should not exist after checksum failure"
    );

    tracker.complete();
}

/// Install with missing artifact URL should fail gracefully
#[test]
#[cfg_attr(not(target_os = "linux"), ignore)]
fn install_missing_artifact_fails() {
    skip_unless_install_tests!();

    let tracker = tracker_for("install_missing_artifact_fails");
    let _trace_guard = tracker.trace_env_guard();

    let phase_start = tracker.start("setup", Some("Create isolated test environment"));
    let fix = InstallFixture::new();
    tracker.end(
        "setup",
        Some("Create isolated test environment"),
        phase_start,
    );

    let phase_start = tracker.start("run_install", Some("Run install.sh with missing artifact"));

    // Use a non-existent file URL
    let bad_url = "file:///nonexistent/path/to/artifact.tar.gz";

    let output = Command::new("timeout")
        .arg("30s")
        .arg("bash")
        .arg("install.sh")
        .arg("--version")
        .arg("vtest")
        .arg("--dest")
        .arg(fix.dest.path())
        .env("HOME", fix.temp_home.path())
        .env("ARTIFACT_URL", bad_url)
        .env("CHECKSUM", &fix.checksum)
        .env("RUSTUP_INIT_SKIP", "1")
        .output()
        .expect("run installer");

    save_command_artifacts(&tracker, &output.stdout, &output.stderr);
    tracker.end(
        "run_install",
        Some("Run install.sh with missing artifact"),
        phase_start,
    );

    // Should fail (either download fails or checksum required)
    assert!(
        !output.status.success(),
        "install.sh should fail with missing artifact"
    );

    // Binary should not be installed
    let bin = fix.binary_path();
    assert!(
        !bin.exists(),
        "Binary should not exist after download failure"
    );

    tracker.complete();
}

/// Install with --help should show usage and exit cleanly
#[test]
#[cfg_attr(not(target_os = "linux"), ignore)]
fn install_help_flag() {
    skip_unless_install_tests!();

    let tracker = tracker_for("install_help_flag");
    let _trace_guard = tracker.trace_env_guard();

    let phase_start = tracker.start("run_help", Some("Run install.sh --help"));

    let output = Command::new("bash")
        .arg("install.sh")
        .arg("--help")
        .output()
        .expect("run installer --help");

    save_command_artifacts(&tracker, &output.stdout, &output.stderr);
    tracker.end("run_help", Some("Run install.sh --help"), phase_start);

    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        output.status.success(),
        "install.sh --help should succeed (exit {:?})",
        output.status.code()
    );

    // Should show usage information
    assert!(
        stdout.contains("Usage") || stdout.contains("--version") || stdout.contains("--dest"),
        "Help output should contain usage info: {stdout}"
    );

    tracker.complete();
}

// ============================================
// Concurrent Install Protection Tests
// ============================================

/// Verify lock file prevents concurrent installs.
///
/// NOTE: This test is ignored by default because it creates a global lock
/// at `/tmp/coding-agent-search-install.lock.d` which can interfere with
/// other install tests running in parallel.
///
/// Run with: `cargo test --test e2e_install_easy install_lock_prevents_concurrent -- --ignored`
#[test]
#[ignore] // Uses global lock that interferes with parallel tests
fn install_lock_prevents_concurrent() {
    skip_unless_install_tests!();

    let tracker = tracker_for("install_lock_prevents_concurrent");
    let _trace_guard = tracker.trace_env_guard();

    let phase_start = tracker.start("setup", Some("Create isolated test environment"));
    let fix = InstallFixture::new();

    // Create a fake lock directory to simulate another installer running
    let lock_dir = std::path::Path::new("/tmp/coding-agent-search-install.lock.d");
    if lock_dir.exists() {
        let _ = fs::remove_dir_all(lock_dir);
    }
    fs::create_dir_all(lock_dir).expect("create lock dir");
    // Write a fake PID that's still "running" (use current PID)
    fs::write(lock_dir.join("pid"), format!("{}", std::process::id())).expect("write fake pid");

    tracker.end(
        "setup",
        Some("Create isolated test environment"),
        phase_start,
    );

    let phase_start = tracker.start("run_install", Some("Run install.sh with lock held"));

    let output = Command::new("timeout")
        .arg("5s")
        .arg("bash")
        .arg("install.sh")
        .arg("--version")
        .arg("vtest")
        .arg("--dest")
        .arg(fix.dest.path())
        .env("HOME", fix.temp_home.path())
        .env("ARTIFACT_URL", fix.artifact_url())
        .env("CHECKSUM", &fix.checksum)
        .env("RUSTUP_INIT_SKIP", "1")
        .output()
        .expect("run installer");

    save_command_artifacts(&tracker, &output.stdout, &output.stderr);

    // Clean up lock
    let _ = fs::remove_dir_all(lock_dir);

    tracker.end(
        "run_install",
        Some("Run install.sh with lock held"),
        phase_start,
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let combined = format!("{stdout}{stderr}");

    // Should fail due to lock
    assert!(
        !output.status.success(),
        "install.sh should fail when lock is held, but got success"
    );

    // Should mention lock in error
    assert!(
        combined.contains("lock") || combined.contains("Another installer"),
        "Error should mention lock: {combined}"
    );

    tracker.complete();
}

// ============================================
// Artifact Verification Tests
// ============================================

/// Verify that all expected artifacts are created
#[test]
#[cfg_attr(not(target_os = "linux"), ignore)]
fn install_creates_expected_artifacts() {
    skip_unless_install_tests!();

    let tracker = tracker_for("install_creates_expected_artifacts");
    let _trace_guard = tracker.trace_env_guard();

    let phase_start = tracker.start("setup", Some("Create isolated test environment"));
    let fix = InstallFixture::new();
    tracker.end(
        "setup",
        Some("Create isolated test environment"),
        phase_start,
    );

    let phase_start = tracker.start("run_install", Some("Run install.sh"));

    let output = Command::new("timeout")
        .arg("30s")
        .arg("bash")
        .arg("install.sh")
        .arg("--version")
        .arg("vtest")
        .arg("--dest")
        .arg(fix.dest.path())
        .env("HOME", fix.temp_home.path())
        .env("ARTIFACT_URL", fix.artifact_url())
        .env("CHECKSUM", &fix.checksum)
        .env("RUSTUP_INIT_SKIP", "1")
        .output()
        .expect("run installer");

    save_command_artifacts(&tracker, &output.stdout, &output.stderr);
    tracker.end("run_install", Some("Run install.sh"), phase_start);

    assert!(
        output.status.success(),
        "install.sh should succeed for artifact verification test"
    );

    // Phase: Verify artifacts
    let phase_start = tracker.start("verify_artifacts", Some("Verify test artifacts exist"));
    let artifacts = tracker.artifacts();

    // Check that stdout was captured
    assert!(
        artifacts.stdout_path.exists(),
        "stdout artifact should exist at {}",
        artifacts.stdout_path.display()
    );

    // Check that stderr was captured
    assert!(
        artifacts.stderr_path.exists(),
        "stderr artifact should exist at {}",
        artifacts.stderr_path.display()
    );

    // Check that trace.jsonl was created
    assert!(
        artifacts.trace_path.exists(),
        "trace.jsonl should exist at {}",
        artifacts.trace_path.display()
    );

    // Save and verify binary checksum
    let bin = fix.binary_path();
    let checksum = save_binary_checksum(&tracker, &bin);
    assert!(
        checksum.is_some(),
        "Binary checksum should be computed and saved"
    );

    let checksum_path = artifacts.dir.join("checksum.txt");
    assert!(
        checksum_path.exists(),
        "checksum.txt should exist at {}",
        checksum_path.display()
    );

    tracker.end(
        "verify_artifacts",
        Some("Verify test artifacts exist"),
        phase_start,
    );

    tracker.complete();
}
