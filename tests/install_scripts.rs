use serial_test::serial;
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

fn fixture(path: &str) -> PathBuf {
    fs::canonicalize(PathBuf::from(path)).expect("fixture path")
}

fn isolated_home() -> tempfile::TempDir {
    let home = tempfile::TempDir::new().unwrap();
    fs::write(home.path().join(".bashrc"), "").unwrap();
    fs::write(home.path().join(".zshrc"), "").unwrap();
    home
}

#[cfg(unix)]
fn make_executable_script(path: &std::path::Path, body: &str) {
    fs::write(path, body).unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).unwrap();
}

struct HttpFixtureServer {
    base_url: String,
    stop: Arc<AtomicBool>,
    wake_addr: String,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Drop for HttpFixtureServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(&self.wake_addr);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn start_http_fixture_server(routes: Vec<(&str, Vec<u8>, &str)>) -> HttpFixtureServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test http server");
    listener
        .set_nonblocking(true)
        .expect("set test http server nonblocking");
    let addr = listener.local_addr().expect("read server address");
    let wake_addr = addr.to_string();
    let base_url = format!("http://{wake_addr}");
    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::clone(&stop);
    let route_map: BTreeMap<String, (Vec<u8>, String)> = routes
        .into_iter()
        .map(|(path, body, content_type)| (path.to_string(), (body, content_type.to_string())))
        .collect();
    let handle = thread::spawn(move || {
        while !stop_flag.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => handle_http_request(stream, &route_map),
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    });
    HttpFixtureServer {
        base_url,
        stop,
        wake_addr,
        handle: Some(handle),
    }
}

fn handle_http_request(mut stream: TcpStream, routes: &BTreeMap<String, (Vec<u8>, String)>) {
    let mut buffer = [0_u8; 8192];
    let read = match stream.read(&mut buffer) {
        Ok(read) => read,
        Err(_) => return,
    };
    let request = String::from_utf8_lossy(&buffer[..read]);
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");
    let path = target
        .split_once('?')
        .map(|(path, _)| path)
        .unwrap_or(target);
    let path = path.split_once('#').map(|(path, _)| path).unwrap_or(path);

    let (status, body, content_type) = match routes.get(path) {
        Some((body, content_type)) => ("200 OK", body.as_slice(), content_type.as_str()),
        None => ("404 Not Found", b"not found".as_slice(), "text/plain"),
    };

    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: {content_type}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

#[test]
#[serial]
#[cfg_attr(not(target_os = "linux"), ignore)]
fn install_sh_succeeds_with_valid_checksum() {
    // Clean up any stale lock from previous runs (CI race condition mitigation)
    let _ = std::fs::remove_dir_all("/tmp/coding-agent-search-install.lock.d");
    let tar = fixture("tests/fixtures/install/coding-agent-search-vtest-linux-x86_64.tar.gz");
    let checksum = fs::read_to_string(
        "tests/fixtures/install/coding-agent-search-vtest-linux-x86_64.tar.gz.sha256",
    )
    .unwrap()
    .trim()
    .to_string();
    let dest = tempfile::TempDir::new().unwrap();
    let home = isolated_home();

    let status = Command::new("bash")
        .arg("install.sh")
        .arg("--version")
        .arg("vtest")
        .arg("--dest")
        .arg(dest.path())
        .arg("--easy-mode")
        .env("HOME", home.path())
        .env("ARTIFACT_URL", format!("file://{}", tar.display()))
        .env("CHECKSUM", checksum)
        .status()
        .expect("run install.sh");

    assert!(status.success());
    let bin = dest.path().join("cass");
    assert!(bin.exists());
    let output = Command::new(&bin).output().expect("run installed bin");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("fixture-linux"));
}

#[test]
#[serial]
#[cfg_attr(not(target_os = "linux"), ignore)]
fn install_sh_fails_with_bad_checksum() {
    // Clean up any stale lock from previous runs (CI race condition mitigation)
    let _ = std::fs::remove_dir_all("/tmp/coding-agent-search-install.lock.d");
    let tar = fixture("tests/fixtures/install/coding-agent-search-vtest-linux-x86_64.tar.gz");
    let dest = tempfile::TempDir::new().unwrap();
    let home = isolated_home();

    let status = Command::new("bash")
        .arg("install.sh")
        .arg("--version")
        .arg("vtest")
        .arg("--dest")
        .arg(dest.path())
        .arg("--easy-mode")
        .env("HOME", home.path())
        .env("ARTIFACT_URL", format!("file://{}", tar.display()))
        .env("CHECKSUM", "deadbeef")
        .status()
        .expect("run install.sh");

    assert!(
        !status.success(),
        "install.sh should fail when checksum does not match"
    );
    assert!(
        !dest.path().join("cass").exists(),
        "cass binary should not be installed on checksum failure"
    );
}

#[test]
#[serial]
#[cfg_attr(not(target_os = "linux"), ignore)]
fn install_sh_falls_back_to_sha256sums_when_per_file_checksum_is_missing() {
    let _ = std::fs::remove_dir_all("/tmp/coding-agent-search-install.lock.d");
    let fixture_tar =
        fixture("tests/fixtures/install/coding-agent-search-vtest-linux-x86_64.tar.gz");
    let checksum = fs::read_to_string(
        "tests/fixtures/install/coding-agent-search-vtest-linux-x86_64.tar.gz.sha256",
    )
    .unwrap()
    .split_whitespace()
    .next()
    .unwrap()
    .to_string();
    let artifact_dir = tempfile::TempDir::new().unwrap();
    let tar_name = "cass-linux-amd64.tar.gz";
    let tar_path = artifact_dir.path().join(tar_name);
    fs::copy(&fixture_tar, &tar_path).unwrap();
    fs::write(
        artifact_dir.path().join("SHA256SUMS.txt"),
        format!("{checksum}  {tar_name}\n"),
    )
    .unwrap();
    let dest = tempfile::TempDir::new().unwrap();
    let home = isolated_home();

    let output = Command::new("bash")
        .arg("install.sh")
        .arg("--version")
        .arg("vtest")
        .arg("--dest")
        .arg(dest.path())
        .arg("--easy-mode")
        .env("HOME", home.path())
        .env("ARTIFACT_URL", format!("file://{}", tar_path.display()))
        .output()
        .expect("run install.sh with SHA256SUMS fallback");

    assert!(
        output.status.success(),
        "install.sh should fall back to SHA256SUMS.txt when the per-file checksum is missing: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(dest.path().join("cass").exists());
}

#[test]
#[serial]
#[cfg_attr(not(target_os = "linux"), ignore)]
fn install_sh_falls_back_to_sha256sums_when_per_file_checksum_is_invalid() {
    let _ = std::fs::remove_dir_all("/tmp/coding-agent-search-install.lock.d");
    let fixture_tar =
        fixture("tests/fixtures/install/coding-agent-search-vtest-linux-x86_64.tar.gz");
    let checksum = fs::read_to_string(
        "tests/fixtures/install/coding-agent-search-vtest-linux-x86_64.tar.gz.sha256",
    )
    .unwrap()
    .split_whitespace()
    .next()
    .unwrap()
    .to_string();
    let artifact_dir = tempfile::TempDir::new().unwrap();
    let tar_name = "cass-linux-amd64.tar.gz";
    let tar_path = artifact_dir.path().join(tar_name);
    fs::copy(&fixture_tar, &tar_path).unwrap();
    fs::write(
        artifact_dir.path().join(format!("{tar_name}.sha256")),
        "not-a-real-checksum\n",
    )
    .unwrap();
    fs::write(
        artifact_dir.path().join("SHA256SUMS.txt"),
        format!("{checksum}  {tar_name}\n"),
    )
    .unwrap();
    let dest = tempfile::TempDir::new().unwrap();
    let home = isolated_home();

    let output = Command::new("bash")
        .arg("install.sh")
        .arg("--version")
        .arg("vtest")
        .arg("--dest")
        .arg(dest.path())
        .arg("--easy-mode")
        .env("HOME", home.path())
        .env("ARTIFACT_URL", format!("file://{}", tar_path.display()))
        .output()
        .expect("run install.sh with invalid per-file checksum");

    assert!(
        output.status.success(),
        "install.sh should ignore malformed per-file checksum data when SHA256SUMS.txt is valid: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(dest.path().join("cass").exists());
}

#[test]
#[serial]
#[cfg_attr(not(target_os = "linux"), ignore)]
fn install_sh_strips_query_suffixes_when_deriving_default_checksum_url() {
    let _ = std::fs::remove_dir_all("/tmp/coding-agent-search-install.lock.d");
    let tar = fixture("tests/fixtures/install/coding-agent-search-vtest-linux-x86_64.tar.gz");
    let checksum = fs::read_to_string(
        "tests/fixtures/install/coding-agent-search-vtest-linux-x86_64.tar.gz.sha256",
    )
    .unwrap()
    .split_whitespace()
    .next()
    .unwrap()
    .to_string();
    let server = start_http_fixture_server(vec![
        (
            "/downloads/cass-linux-amd64.tar.gz",
            fs::read(&tar).unwrap(),
            "application/gzip",
        ),
        (
            "/downloads/cass-linux-amd64.tar.gz.sha256",
            format!("{checksum}  cass-linux-amd64.tar.gz\n").into_bytes(),
            "text/plain",
        ),
    ]);
    let dest = tempfile::TempDir::new().unwrap();
    let home = isolated_home();

    let output = Command::new("bash")
        .arg("install.sh")
        .arg("--version")
        .arg("vtest")
        .arg("--dest")
        .arg(dest.path())
        .arg("--easy-mode")
        .env("HOME", home.path())
        .env(
            "ARTIFACT_URL",
            format!(
                "{}/downloads/cass-linux-amd64.tar.gz?download=1#ignored",
                server.base_url
            ),
        )
        .output()
        .expect("run install.sh with custom artifact url suffixes");

    assert!(
        output.status.success(),
        "install.sh should derive the default checksum URL from the stripped artifact path: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(dest.path().join("cass").exists());
}

#[test]
#[serial]
#[cfg_attr(not(target_os = "linux"), ignore)]
fn install_sh_falls_back_to_shasum_when_sha256sum_fails() {
    let _ = std::fs::remove_dir_all("/tmp/coding-agent-search-install.lock.d");
    let tar = fixture("tests/fixtures/install/coding-agent-search-vtest-linux-x86_64.tar.gz");
    let checksum = fs::read_to_string(
        "tests/fixtures/install/coding-agent-search-vtest-linux-x86_64.tar.gz.sha256",
    )
    .unwrap()
    .trim()
    .to_string();
    let dest = tempfile::TempDir::new().unwrap();
    let home = isolated_home();
    let tool_dir = tempfile::TempDir::new().unwrap();
    let sha256sum_fixture_path = tool_dir.path().join("sha256sum");
    make_executable_script(
        &sha256sum_fixture_path,
        "#!/bin/sh\n# simulate an unavailable sha256sum implementation\nexit 127\n",
    );

    let path = format!(
        "{}:{}",
        tool_dir.path().display(),
        std::env::var("PATH").expect("PATH should be set")
    );

    let status = Command::new("bash")
        .arg("install.sh")
        .arg("--version")
        .arg("vtest")
        .arg("--dest")
        .arg(dest.path())
        .arg("--easy-mode")
        .env("HOME", home.path())
        .env("PATH", path)
        .env("ARTIFACT_URL", format!("file://{}", tar.display()))
        .env("CHECKSUM", checksum)
        .status()
        .expect("run install.sh with shasum fallback");

    assert!(status.success(), "install.sh should fall back to shasum");
    assert!(dest.path().join("cass").exists());
}

fn find_powershell() -> Option<String> {
    for candidate in [&"pwsh", &"powershell"] {
        if let Ok(path) = which::which(candidate) {
            return Some(path.to_string_lossy().into_owned());
        }
    }
    None
}

#[test]
fn install_ps1_succeeds_with_valid_checksum() {
    let Some(ps) = find_powershell() else {
        eprintln!("skipping powershell test: pwsh not found");
        return;
    };

    let zip = fixture("tests/fixtures/install/coding-agent-search-vtest-windows-x86_64.zip");
    let checksum = fs::read_to_string(
        "tests/fixtures/install/coding-agent-search-vtest-windows-x86_64.zip.sha256",
    )
    .unwrap()
    .trim()
    .to_string();
    let dest = tempfile::TempDir::new().unwrap();

    let status = Command::new(ps)
        .arg("-NoProfile")
        .arg("-ExecutionPolicy")
        .arg("Bypass")
        .arg("-File")
        .arg("install.ps1")
        .arg("-Version")
        .arg("vtest")
        .arg("-Dest")
        .arg(dest.path())
        .arg("-Checksum")
        .arg(&checksum)
        .arg("-ArtifactUrl")
        .arg(format!("file://{}", zip.display()))
        .status()
        .expect("run install.ps1");

    assert!(status.success());
    let bin = dest.path().join("cass.exe");
    assert!(bin.exists());
    let content = fs::read_to_string(&bin).unwrap();
    assert!(content.contains("fixture-windows"));
}

#[test]
fn install_ps1_fails_with_bad_checksum() {
    let Some(ps) = find_powershell() else {
        eprintln!("skipping powershell test: pwsh not found");
        return;
    };

    let zip = fixture("tests/fixtures/install/coding-agent-search-vtest-windows-x86_64.zip");
    let dest = tempfile::TempDir::new().unwrap();

    let status = Command::new(ps)
        .arg("-NoProfile")
        .arg("-ExecutionPolicy")
        .arg("Bypass")
        .arg("-File")
        .arg("install.ps1")
        .arg("-Version")
        .arg("vtest")
        .arg("-Dest")
        .arg(dest.path())
        .arg("-Checksum")
        .arg("deadbeef")
        .arg("-ArtifactUrl")
        .arg(format!("file://{}", zip.display()))
        .status()
        .expect("run install.ps1");

    assert!(
        !status.success(),
        "install.ps1 should fail when checksum does not match"
    );
    assert!(
        !dest.path().join("cass.exe").exists(),
        "cass.exe should not be installed on checksum failure"
    );
}

#[test]
#[serial]
fn install_ps1_falls_back_to_sibling_sha256sums_for_custom_artifact_url() {
    let Some(ps) = find_powershell() else {
        eprintln!("skipping powershell test: pwsh not found");
        return;
    };

    let zip = fixture("tests/fixtures/install/coding-agent-search-vtest-windows-x86_64.zip");
    let checksum = fs::read_to_string(
        "tests/fixtures/install/coding-agent-search-vtest-windows-x86_64.zip.sha256",
    )
    .unwrap()
    .split_whitespace()
    .next()
    .unwrap()
    .to_string();
    let server = start_http_fixture_server(vec![
        (
            "/downloads/cass-windows-amd64.zip",
            fs::read(&zip).unwrap(),
            "application/zip",
        ),
        (
            "/downloads/SHA256SUMS.txt",
            format!("{checksum}  cass-windows-amd64.zip\n").into_bytes(),
            "text/plain",
        ),
    ]);
    let dest = tempfile::TempDir::new().unwrap();

    let output = Command::new(ps)
        .arg("-NoProfile")
        .arg("-ExecutionPolicy")
        .arg("Bypass")
        .arg("-File")
        .arg("install.ps1")
        .arg("-Version")
        .arg("vtest")
        .arg("-Dest")
        .arg(dest.path())
        .arg("-ArtifactUrl")
        .arg(format!(
            "{}/downloads/cass-windows-amd64.zip?download=1#ignored",
            server.base_url
        ))
        .output()
        .expect("run install.ps1 with sibling SHA256SUMS fallback");

    assert!(
        output.status.success(),
        "install.ps1 should fall back to sibling SHA256SUMS.txt for custom artifact URLs: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let bin = dest.path().join("cass.exe");
    assert!(bin.exists());
    let content = fs::read_to_string(&bin).unwrap();
    assert!(content.contains("fixture-windows"));
}

// =============================================================================
// Upgrade Process E2E Tests
// =============================================================================

/// Test that upgrading from an older version to a newer version works correctly.
/// This simulates the full upgrade flow:
/// 1. Install an "old" version
/// 2. Upgrade to a "new" version
/// 3. Verify the new version is correctly installed
#[test]
#[serial]
#[cfg_attr(not(target_os = "linux"), ignore)]
fn upgrade_replaces_existing_binary() {
    // Clean up any stale lock from previous runs
    let _ = std::fs::remove_dir_all("/tmp/coding-agent-search-install.lock.d");

    let tar = fixture("tests/fixtures/install/coding-agent-search-vtest-linux-x86_64.tar.gz");
    let checksum = fs::read_to_string(
        "tests/fixtures/install/coding-agent-search-vtest-linux-x86_64.tar.gz.sha256",
    )
    .unwrap()
    .trim()
    .to_string();
    let dest = tempfile::TempDir::new().unwrap();
    let home = isolated_home();

    // Step 1: Create a test "old" binary to simulate an existing installation
    let bin_path = dest.path().join("cass");
    fs::write(&bin_path, "#!/bin/sh\necho 'old-version-0.0.1'\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&bin_path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&bin_path, perms).unwrap();
    }

    // Verify "old" version exists
    let old_output = Command::new(&bin_path).output().expect("run old binary");
    let old_stdout = String::from_utf8_lossy(&old_output.stdout);
    assert!(
        old_stdout.contains("old-version"),
        "old binary should report old version"
    );

    // Step 2: Run the installer to "upgrade"
    let status = Command::new("bash")
        .arg("install.sh")
        .arg("--version")
        .arg("vtest")
        .arg("--dest")
        .arg(dest.path())
        .arg("--easy-mode")
        .env("HOME", home.path())
        .env("ARTIFACT_URL", format!("file://{}", tar.display()))
        .env("CHECKSUM", checksum)
        .status()
        .expect("run install.sh for upgrade");

    assert!(status.success(), "upgrade should succeed");

    // Step 3: Verify the new version replaced the old one
    assert!(bin_path.exists(), "binary should still exist after upgrade");

    let new_output = Command::new(&bin_path)
        .output()
        .expect("run upgraded binary");
    let new_stdout = String::from_utf8_lossy(&new_output.stdout);
    assert!(
        new_stdout.contains("fixture-linux"),
        "upgraded binary should report new version, got: {}",
        new_stdout
    );
    assert!(
        !new_stdout.contains("old-version"),
        "upgraded binary should not report old version"
    );
}

/// Test that the installer correctly handles concurrent upgrade attempts.
/// The lock mechanism should prevent race conditions.
#[test]
#[serial]
#[cfg_attr(not(target_os = "linux"), ignore)]
fn concurrent_installs_are_serialized() {
    // Clean up any stale lock
    let _ = std::fs::remove_dir_all("/tmp/coding-agent-search-install.lock.d");

    let tar = fixture("tests/fixtures/install/coding-agent-search-vtest-linux-x86_64.tar.gz");
    let checksum = fs::read_to_string(
        "tests/fixtures/install/coding-agent-search-vtest-linux-x86_64.tar.gz.sha256",
    )
    .unwrap()
    .trim()
    .to_string();
    let dest1 = tempfile::TempDir::new().unwrap();
    let dest2 = tempfile::TempDir::new().unwrap();
    let home1 = isolated_home();
    let home2 = isolated_home();

    // Spawn two concurrent installs
    let tar1 = tar.clone();
    let checksum1 = checksum.clone();
    let dest1_path = dest1.path().to_path_buf();
    let home1_path = home1.path().to_path_buf();

    let handle1 = std::thread::spawn(move || {
        Command::new("bash")
            .arg("install.sh")
            .arg("--version")
            .arg("vtest")
            .arg("--dest")
            .arg(&dest1_path)
            .arg("--easy-mode")
            .env("HOME", home1_path)
            .env("ARTIFACT_URL", format!("file://{}", tar1.display()))
            .env("CHECKSUM", checksum1)
            .status()
    });

    // Small delay to increase chance of overlap
    std::thread::sleep(std::time::Duration::from_millis(50));

    let tar2 = tar;
    let checksum2 = checksum;
    let dest2_path = dest2.path().to_path_buf();
    let home2_path = home2.path().to_path_buf();

    let handle2 = std::thread::spawn(move || {
        Command::new("bash")
            .arg("install.sh")
            .arg("--version")
            .arg("vtest")
            .arg("--dest")
            .arg(&dest2_path)
            .arg("--easy-mode")
            .env("HOME", home2_path)
            .env("ARTIFACT_URL", format!("file://{}", tar2.display()))
            .env("CHECKSUM", checksum2)
            .status()
    });

    let result1 = handle1.join().expect("thread 1 should complete");
    let result2 = handle2.join().expect("thread 2 should complete");

    let success1 = result1.as_ref().map(|s| s.success()).unwrap_or(false);
    let success2 = result2.as_ref().map(|s| s.success()).unwrap_or(false);

    // One should succeed, one might fail due to lock (or both succeed if serialized)
    // The key is no crashes or corrupted installs
    let success_count = if success1 { 1 } else { 0 } + if success2 { 1 } else { 0 };

    assert!(
        success_count >= 1,
        "at least one concurrent install should succeed"
    );

    // If first succeeded, verify the binary works
    if success1 {
        let bin = dest1.path().join("cass");
        assert!(bin.exists(), "binary should exist after successful install");
    }
}

/// Test that the verify flag actually runs the installed binary.
#[test]
#[serial]
#[cfg_attr(not(target_os = "linux"), ignore)]
fn verify_flag_runs_self_test() {
    // Clean up any stale lock
    let _ = std::fs::remove_dir_all("/tmp/coding-agent-search-install.lock.d");

    let tar = fixture("tests/fixtures/install/coding-agent-search-vtest-linux-x86_64.tar.gz");
    let checksum = fs::read_to_string(
        "tests/fixtures/install/coding-agent-search-vtest-linux-x86_64.tar.gz.sha256",
    )
    .unwrap()
    .trim()
    .to_string();
    let dest = tempfile::TempDir::new().unwrap();
    let home = isolated_home();

    let output = Command::new("bash")
        .arg("install.sh")
        .arg("--version")
        .arg("vtest")
        .arg("--dest")
        .arg(dest.path())
        .arg("--easy-mode")
        .arg("--verify") // This should run the binary after install
        .env("HOME", home.path())
        .env("ARTIFACT_URL", format!("file://{}", tar.display()))
        .env("CHECKSUM", checksum)
        .output()
        .expect("run install.sh with verify");

    assert!(
        output.status.success(),
        "install with verify should succeed"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    // The fixture binary outputs "fixture-linux" which should appear in verify output
    assert!(
        stdout.contains("fixture-linux") || stdout.contains("Self-test complete"),
        "verify should run the binary and show output, got: {}",
        stdout
    );
}
