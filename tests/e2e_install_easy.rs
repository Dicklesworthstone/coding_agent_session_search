//! E2E Install Easy Mode Test
//!
//! Validates install.sh logic using the real system toolchain (rustc, cargo,
//! sha256sum). Runs install.sh against a fixture tarball in an isolated temp
//! HOME, verifying checksum verification, unpacking, and path setup.
//!
//! Additional CI coverage exists via `.github/workflows/install-test.yml`
//! which builds a real release binary end-to-end.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

mod util;
use util::e2e_log::{E2eError, E2eErrorContext, E2ePerformanceMetrics, PhaseTracker};

fn truncate_output(bytes: &[u8], max_len: usize) -> String {
    let s = String::from_utf8_lossy(bytes);
    if s.len() > max_len {
        format!("{}... [truncated {} bytes]", &s[..max_len], s.len() - max_len)
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

#[test]
#[cfg_attr(not(target_os = "linux"), ignore)]
fn install_easy_mode_end_to_end() {
    let tracker = tracker_for("install_easy_mode_end_to_end");

    // Phase: Setup isolated test environment
    let phase_start = tracker.start("setup", Some("Create isolated test environment"));
    let tar = fixture("tests/fixtures/install/coding-agent-search-vtest-linux-x86_64.tar.gz");
    let checksum = fs::read_to_string(
        "tests/fixtures/install/coding-agent-search-vtest-linux-x86_64.tar.gz.sha256",
    )
    .unwrap()
    .trim()
    .to_string();

    let temp_home = tempfile::TempDir::new().unwrap();
    let dest = tempfile::TempDir::new().unwrap();
    tracker.end("setup", Some("Create isolated test environment"), phase_start);

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
        .arg(dest.path())
        .env("HOME", temp_home.path())
        .env("ARTIFACT_URL", format!("file://{}", tar.display()))
        .env("CHECKSUM", &checksum)
        .env("RUSTUP_INIT_SKIP", "1")
        .output()
        .expect("run installer");

    let install_duration = install_start.elapsed().as_millis() as u64;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        let ctx = E2eErrorContext::new()
            .with_command("bash install.sh --version vtest --easy-mode --verify")
            .capture_cwd()
            .add_state("exit_code", serde_json::json!(output.status.code()))
            .add_state("stdout_tail", serde_json::json!(truncate_output(&output.stdout, 1000)))
            .add_state("stderr_tail", serde_json::json!(truncate_output(&output.stderr, 1000)));
        tracker.fail(E2eError::with_type("install.sh failed", "COMMAND_FAILED").with_context(ctx));
        panic!("install.sh failed (exit {:?})\nstdout: {stdout}\nstderr: {stderr}", output.status.code());
    }
    tracker.end("run_install", Some("Run install.sh with real toolchain"), phase_start);

    // Phase: Verify installation artifacts and checksums
    let phase_start = tracker.start("verify_install", Some("Verify binary and checksums"));

    let bin = dest.path().join("cass");
    assert!(bin.exists(), "Binary not found at expected path");

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
        "Binary --help should succeed"
    );

    // Verify installed binary content matches fixture source
    let installed_binary = fs::read(&bin).expect("read installed binary");
    let fixture_binary =
        fs::read("tests/fixtures/install/coding-agent-search").expect("read fixture binary");
    assert_eq!(
        installed_binary, fixture_binary,
        "Installed binary should match fixture binary"
    );

    tracker.end("verify_install", Some("Verify binary and checksums"), phase_start);

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
