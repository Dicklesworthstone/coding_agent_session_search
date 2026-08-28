use serial_test::serial;
use sha2::{Digest, Sha256};
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

fn must<T, E: std::fmt::Display>(result: Result<T, E>, context: &str) -> T {
    match result {
        Ok(value) => value,
        Err(err) => std::panic::panic_any(format!("{context}: {err}")),
    }
}

fn fixture(path: &str) -> PathBuf {
    must(fs::canonicalize(PathBuf::from(path)), "fixture path")
}

fn isolated_home() -> tempfile::TempDir {
    let home = must(tempfile::TempDir::new(), "create isolated home");
    must(fs::write(home.path().join(".bashrc"), ""), "seed bashrc");
    must(fs::write(home.path().join(".zshrc"), ""), "seed zshrc");
    home
}

fn isolated_install_tmp_root() -> tempfile::TempDir {
    tempfile::TempDir::new().expect("installer temp root")
}

fn install_sh_command(tmp_root: &tempfile::TempDir) -> Command {
    let mut command = Command::new("bash");
    command.arg("install.sh").env("TMPDIR", tmp_root.path());
    command
}

#[test]
fn install_sh_rejects_unknown_options() {
    let output = Command::new("bash")
        .arg("install.sh")
        .arg("--quiet")
        .arg("--verison")
        .arg("vtest")
        .output()
        .expect("run install.sh with a misspelled option");

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Unknown option: --verison"),
        "installer should identify the invalid option, got: {stderr}"
    );
}

#[test]
fn install_sh_rejects_options_with_missing_values() {
    let output = Command::new("bash")
        .arg("install.sh")
        .arg("--version")
        .arg("--quiet")
        .output()
        .expect("run install.sh with a missing option value");

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--version requires a value"),
        "installer should explain the missing value, got: {stderr}"
    );
}

#[test]
#[serial]
#[cfg_attr(not(target_os = "linux"), ignore)]
fn install_sh_does_not_fallback_from_an_explicit_artifact_url() {
    let dest = tempfile::TempDir::new().expect("install destination");
    let home = isolated_home();
    let tmp_root = isolated_install_tmp_root();
    let output = install_sh_command(&tmp_root)
        .arg("--version")
        .arg("vtest")
        .arg("--dest")
        .arg(dest.path())
        .arg("--easy-mode")
        .env("HOME", home.path())
        .env(
            "ARTIFACT_URL",
            format!("file://{}/missing.tar.gz", tmp_root.path().display()),
        )
        .output()
        .expect("run install.sh with a missing explicit artifact");

    assert!(
        !output.status.success(),
        "an unavailable explicit artifact must fail the install"
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("Could not download explicitly requested artifact"),
        "installer should identify the explicit artifact failure, got: {combined}"
    );
    assert!(
        !combined.contains("Building from source"),
        "installer must not substitute a source build for an explicit artifact"
    );
    assert!(!dest.path().join("cass").exists());
}

#[test]
fn install_sh_has_no_baseline_artifact_selection() -> Result<(), String> {
    // cass#308 / bead tg5o9: the ONNX runtime is gone, so the installer must
    // never select a `-baseline` asset (they are not published anymore) and
    // the source fallback builds default features on every CPU.
    let script = fs::read_to_string("install.sh").map_err(|err| err.to_string())?;
    let powershell = fs::read_to_string("install.ps1").map_err(|err| err.to_string())?;

    if script.contains("TARGET=\"linux-amd64-baseline\"")
        || script.contains("TARGET=\"windows-amd64-baseline\"")
    {
        return Err("installer must not select retired -baseline assets".to_string());
    }
    if script.contains("host_has_avx2()") || powershell.contains("Test-HostHasAvx2") {
        return Err(
            "the Unix and PowerShell AVX2 probes were retired with ONNX (cass#308)".to_string(),
        );
    }
    if script.contains("SOURCE_CARGO_ARGS")
        || script.contains("CASS_FORCE_BASELINE")
        || powershell.contains("CASS_FORCE_BASELINE")
    {
        return Err("installers must not retain dead baseline feature-selection controls".into());
    }
    if !script.contains("cargo build --locked --release)") {
        return Err(
            "source fallback must use the ordinary release feature set on every CPU".to_string(),
        );
    }
    Ok(())
}

#[test]
fn source_installer_uses_the_checkout_pinned_toolchain() {
    let script = fs::read_to_string("install.sh").expect("read install.sh");

    for required in [
        "ensure_rust \"$TMP/src\"",
        "rustup show active-toolchain",
        "rustup toolchain install",
        "--default-toolchain none",
    ] {
        assert!(
            script.contains(required),
            "source installer is missing pinned-toolchain behavior: {required}"
        );
    }

    let clone_offset = script
        .find("git clone --depth 1 --branch")
        .expect("source installer must clone the requested release");
    let bootstrap_offset = script
        .find("ensure_rust \"$TMP/src\"")
        .expect("source installer must bootstrap the checkout toolchain");
    assert!(
        clone_offset < bootstrap_offset,
        "the checkout must exist before rustup reads rust-toolchain.toml"
    );
    assert!(
        !script.contains("--default-toolchain stable"),
        "source bootstrap must not download an unrelated stable toolchain"
    );
}

#[test]
fn install_sh_keeps_tmp_root_warnings_out_of_command_substitution() {
    let script = fs::read_to_string("install.sh").expect("read install.sh");
    assert!(
        script.contains(
            "warn() { [ \"$QUIET\" -eq 1 ] && return 0; echo -e \"\\033[1;33m⚠\\033[0m $*\" >&2; }"
        ),
        "installer warnings must go to stderr"
    );
    assert!(
        script.contains("TMP_ROOT=\"$(resolve_tmp_root)\""),
        "test must remain coupled to the command-substitution risk"
    );
    assert!(
        script.contains(
            "warn \"Ignoring TMPDIR=${TMPDIR} because it is not an accessible directory\""
        ),
        "test must remain coupled to the invalid-TMPDIR warning path"
    );
}

#[test]
fn install_ps1_derives_sibling_urls_without_host_path_semantics() {
    let script = fs::read_to_string("install.ps1").expect("read install.ps1");
    assert!(
        !script.contains("[System.IO.Path]::GetDirectoryName($path.TrimEnd('/'))"),
        "URI directory derivation must not depend on Windows filesystem separators"
    );
    for required in [
        "$trimmedPath = $path.TrimEnd('/')",
        "$lastSlash = $trimmedPath.LastIndexOf('/')",
        "$trimmedPath.Substring(0, $lastSlash) + \"/$SiblingName\"",
    ] {
        assert!(
            script.contains(required),
            "PowerShell sibling URL derivation is missing: {required}"
        );
    }
}

#[test]
fn release_workflow_builds_and_publishes_the_exact_requested_tag() -> Result<(), String> {
    let workflow =
        fs::read_to_string(".github/workflows/release.yml").map_err(|err| err.to_string())?;

    let exact_ref_checkouts = workflow.matches("ref: ${{ env.RELEASE_REF }}").count();
    if exact_ref_checkouts != 3 {
        return Err(format!(
            "build, release, and crates publish must all checkout RELEASE_REF; found {exact_ref_checkouts} exact-ref checkouts"
        ));
    }
    for required in [
        "RELEASE_REF: ${{ github.event_name == 'workflow_dispatch' && inputs.tag || github.ref }}",
        "DISPATCH_TAG: ${{ inputs.tag }}",
        "git rev-parse --verify \"${RAW_TAG}^{commit}\"",
        "Checked-out commit (${CHECKED_OUT_COMMIT}) does not match ${RAW_TAG} (${TAG_COMMIT}).",
    ] {
        if !workflow.contains(required) {
            return Err(format!(
                "release workflow is missing exact-tag integrity guard: {required}"
            ));
        }
    }
    if workflow.contains("Clone sibling dependencies") {
        return Err(
            "release workflow must not clone unused sibling repositories before Cargo builds"
                .to_string(),
        );
    }
    if !workflow.contains("cargo build --locked --release --target ${{ matrix.target }}") {
        return Err("release binaries must be built from the tagged Cargo.lock".to_string());
    }
    for required in [
        "if [[ \"${API_VERSION}\" != \"1\" ]]",
        "$apiVersionJson = & $binary api-version --json",
        "if ($apiVersion.api_version -ne 1)",
        "id: registry_version",
        "already_published=true",
        "steps.registry_version.outputs.already_published != 'true'",
    ] {
        if !workflow.contains(required) {
            return Err(format!(
                "release binaries must prove the pinned robot API contract: {required}"
            ));
        }
    }
    if workflow.contains("dtolnay/rust-toolchain@stable") {
        return Err("release workflow actions must be immutable-SHA pinned".to_string());
    }

    Ok(())
}

fn file_sha256_hex(path: &std::path::Path) -> String {
    let mut file = fs::File::open(path).expect("open file for sha256");
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 8192];

    loop {
        let read = file.read(&mut buffer).expect("read file for sha256");
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    hex::encode(hasher.finalize())
}

#[cfg(unix)]
fn make_executable_script(path: &std::path::Path, body: &str) {
    fs::write(path, body).unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).unwrap();
}

#[cfg(not(unix))]
fn make_executable_script(path: &std::path::Path, body: &str) {
    drop(fs::write(path, body));
}

#[test]
#[serial]
#[cfg(unix)]
fn source_install_bootstraps_the_toolchain_after_clone() {
    let harness = tempfile::TempDir::new().expect("source-install harness");
    let fake_bin = harness.path().join("bin");
    fs::create_dir(&fake_bin).expect("create fake bin");
    let event_log = harness.path().join("events.log");
    let home = isolated_home();
    let dest = tempfile::TempDir::new().expect("install destination");
    let tmp_root = isolated_install_tmp_root();

    make_executable_script(
        &fake_bin.join("git"),
        r#"#!/bin/sh
set -eu
checkout=""
for argument in "$@"; do checkout="$argument"; done
printf 'git|%s|%s\n' "$PWD" "$*" >> "$FAKE_INSTALL_EVENT_LOG"
mkdir -p "$checkout/target/release"
printf '%s\n' '[toolchain]' 'channel = "nightly-test-date"' > "$checkout/rust-toolchain.toml"
printf '%s\n' '#!/bin/sh' 'echo source-fixture' > "$checkout/target/release/cass"
chmod 755 "$checkout/target/release/cass"
"#,
    );
    make_executable_script(
        &fake_bin.join("rustup"),
        r#"#!/bin/sh
set -eu
printf 'rustup|%s|%s\n' "$PWD" "$*" >> "$FAKE_INSTALL_EVENT_LOG"
case "${1:-}:${2:-}" in
  show:active-toolchain) exit 1 ;;
  toolchain:install)
    test -f rust-toolchain.toml
    grep -q 'nightly-test-date' rust-toolchain.toml
    : > .pinned-toolchain-installed
    ;;
  *) exit 2 ;;
esac
"#,
    );
    make_executable_script(
        &fake_bin.join("cargo"),
        r#"#!/bin/sh
set -eu
printf 'cargo|%s|%s\n' "$PWD" "$*" >> "$FAKE_INSTALL_EVENT_LOG"
test -f rust-toolchain.toml
test -f .pinned-toolchain-installed
test "${1:-}" = build
"#,
    );

    let inherited_path = std::env::var("PATH").expect("PATH should be set");
    let fake_path = format!("{}:{inherited_path}", fake_bin.display());
    let output = install_sh_command(&tmp_root)
        .arg("--version")
        .arg("vtest")
        .arg("--dest")
        .arg(dest.path())
        .arg("--easy-mode")
        .arg("--from-source")
        .env("HOME", home.path())
        .env("PATH", fake_path)
        .env("FAKE_INSTALL_EVENT_LOG", &event_log)
        .env_remove("RUSTUP_INIT_SKIP")
        .output()
        .expect("run source installer with fake toolchain commands");

    assert!(
        output.status.success(),
        "source install failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(dest.path().join("cass").is_file());

    let events = fs::read_to_string(&event_log).expect("read source-install events");
    let git_offset = events.find("git|").expect("git clone event");
    let show_offset = events
        .find("|show active-toolchain")
        .expect("active toolchain probe event");
    let install_offset = events
        .find("|toolchain install")
        .expect("pinned toolchain install event");
    let cargo_offset = events.find("cargo|").expect("cargo build event");
    assert!(
        git_offset < show_offset && show_offset < install_offset && install_offset < cargo_offset,
        "expected clone -> probe -> toolchain install -> build, got:\n{events}"
    );
    for event in events.lines().filter(|line| line.starts_with("rustup|") || line.starts_with("cargo|")) {
        assert!(
            event.contains("/src|"),
            "toolchain commands must run from the cloned checkout: {event}"
        );
    }
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
    let tar = fixture("tests/fixtures/install/coding-agent-search-vtest-linux-x86_64.tar.gz");
    let checksum = fs::read_to_string(
        "tests/fixtures/install/coding-agent-search-vtest-linux-x86_64.tar.gz.sha256",
    )
    .unwrap()
    .trim()
    .to_string();
    let dest = tempfile::TempDir::new().unwrap();
    let home = isolated_home();
    let tmp_root = isolated_install_tmp_root();

    let status = install_sh_command(&tmp_root)
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
fn install_sh_rejects_archive_path_traversal_before_extracting() {
    let artifact_dir = tempfile::TempDir::new().unwrap();
    let payload_dir = tempfile::TempDir::new().unwrap();
    let payload_cass = payload_dir.path().join("cass");
    make_executable_script(&payload_cass, "#!/bin/sh\necho fixture-linux\n");

    let tar_path = artifact_dir.path().join("cass-linux-amd64.tar.gz");
    let tar_status = Command::new("tar")
        .arg("-czf")
        .arg(&tar_path)
        .arg("-C")
        .arg(payload_dir.path())
        .arg("--transform")
        .arg("s#^cass$#../pwned#")
        .arg("cass")
        .status()
        .expect("create traversal tarball");
    assert!(tar_status.success(), "test tarball should be created");

    let checksum = file_sha256_hex(&tar_path);
    let dest = tempfile::TempDir::new().unwrap();
    let home = isolated_home();
    let tmp_root = isolated_install_tmp_root();

    let output = install_sh_command(&tmp_root)
        .arg("--version")
        .arg("vtest")
        .arg("--dest")
        .arg(dest.path())
        .arg("--easy-mode")
        .env("HOME", home.path())
        .env("ARTIFACT_URL", format!("file://{}", tar_path.display()))
        .env("CHECKSUM", checksum)
        .output()
        .expect("run install.sh with traversal archive");

    assert!(
        !output.status.success(),
        "install.sh should reject path traversal archive members"
    );
    let combined_output = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined_output.contains("Unsafe archive member: ../pwned"),
        "installer should explain the rejected member, got: {combined_output}"
    );
    assert!(
        !dest.path().join("cass").exists(),
        "cass binary should not be installed from a rejected archive"
    );
    assert!(
        !tmp_root.path().join("pwned").exists(),
        "path traversal member should not be extracted into the temp root"
    );
}

#[test]
#[serial]
#[cfg(target_os = "linux")]
fn install_sh_rejects_symlink_archive_members_before_extracting() {
    let artifact_dir = tempfile::TempDir::new().expect("artifact directory");
    let payload_dir = tempfile::TempDir::new().expect("payload directory");
    let payload_cass = payload_dir.path().join("cass");
    let link_status = Command::new("ln")
        .arg("-s")
        .arg("../outside-installer-tree")
        .arg(&payload_cass)
        .status()
        .expect("create malicious symlink payload");
    assert!(link_status.success(), "test symlink should be created");

    let tar_path = artifact_dir.path().join("cass-linux-amd64.tar.gz");
    let tar_status = Command::new("tar")
        .arg("-czf")
        .arg(&tar_path)
        .arg("-C")
        .arg(payload_dir.path())
        .arg("cass")
        .status()
        .expect("create symlink tarball");
    assert!(tar_status.success(), "test tarball should be created");

    let checksum = file_sha256_hex(&tar_path);
    let dest = tempfile::TempDir::new().expect("install destination");
    let home = isolated_home();
    let tmp_root = isolated_install_tmp_root();
    let output = install_sh_command(&tmp_root)
        .arg("--version")
        .arg("vtest")
        .arg("--dest")
        .arg(dest.path())
        .arg("--easy-mode")
        .env("HOME", home.path())
        .env("ARTIFACT_URL", format!("file://{}", tar_path.display()))
        .env("CHECKSUM", checksum)
        .output()
        .expect("run install.sh with symlink archive");

    assert!(
        !output.status.success(),
        "install.sh should reject symlink archive members"
    );
    let combined_output = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined_output.contains("Archive contains unsupported entry type: l"),
        "installer should explain the rejected entry type, got: {combined_output}"
    );
    assert!(
        !dest.path().join("cass").exists(),
        "cass binary should not be installed from a symlink archive"
    );
}

#[test]
#[serial]
#[cfg_attr(not(target_os = "linux"), ignore)]
fn install_sh_fails_with_bad_checksum() {
    let tar = fixture("tests/fixtures/install/coding-agent-search-vtest-linux-x86_64.tar.gz");
    let dest = tempfile::TempDir::new().unwrap();
    let home = isolated_home();
    let tmp_root = isolated_install_tmp_root();

    let status = install_sh_command(&tmp_root)
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
    let tmp_root = isolated_install_tmp_root();

    let output = install_sh_command(&tmp_root)
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
    let tmp_root = isolated_install_tmp_root();

    let output = install_sh_command(&tmp_root)
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
    let tmp_root = isolated_install_tmp_root();

    let output = install_sh_command(&tmp_root)
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
    let tar = fixture("tests/fixtures/install/coding-agent-search-vtest-linux-x86_64.tar.gz");
    let checksum = fs::read_to_string(
        "tests/fixtures/install/coding-agent-search-vtest-linux-x86_64.tar.gz.sha256",
    )
    .unwrap()
    .trim()
    .to_string();
    let dest = tempfile::TempDir::new().unwrap();
    let home = isolated_home();
    let tmp_root = isolated_install_tmp_root();
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

    let status = install_sh_command(&tmp_root)
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

#[test]
#[serial]
fn install_ps1_falls_back_to_unsuffixed_sha256sums_for_custom_artifact_url() {
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
            "/downloads/SHA256SUMS",
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
        .expect("run install.ps1 with unsuffixed SHA256SUMS fallback");

    assert!(
        output.status.success(),
        "install.ps1 should fall back to sibling SHA256SUMS when SHA256SUMS.txt is missing: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let bin = dest.path().join("cass.exe");
    assert!(bin.exists());
    let content = fs::read_to_string(&bin).unwrap();
    assert!(content.contains("fixture-windows"));
}

#[test]
#[serial]
fn install_ps1_parses_local_aggregate_checksum_by_artifact_name() {
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
    let dest = tempfile::TempDir::new().unwrap();
    let manifest_dir = tempfile::TempDir::new().unwrap();
    let manifest = manifest_dir.path().join("SHA256SUMS");
    let zip_name = zip.file_name().unwrap().to_string_lossy();
    fs::write(
        &manifest,
        format!(
            "0000000000000000000000000000000000000000000000000000000000000000  other.zip\n{checksum}  {zip_name}\n"
        ),
    )
    .unwrap();

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
        .arg("-ChecksumUrl")
        .arg(&manifest)
        .arg("-ArtifactUrl")
        .arg(format!("file://{}", zip.display()))
        .output()
        .expect("run install.ps1 with local aggregate checksum manifest");

    assert!(
        output.status.success(),
        "install.ps1 should parse local SHA256SUMS by artifact name instead of using the first manifest hash: {}",
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
    let tar = fixture("tests/fixtures/install/coding-agent-search-vtest-linux-x86_64.tar.gz");
    let checksum = fs::read_to_string(
        "tests/fixtures/install/coding-agent-search-vtest-linux-x86_64.tar.gz.sha256",
    )
    .unwrap()
    .trim()
    .to_string();
    let dest = tempfile::TempDir::new().unwrap();
    let home = isolated_home();
    let tmp_root = isolated_install_tmp_root();

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
    let status = install_sh_command(&tmp_root)
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
    let tmp_root = isolated_install_tmp_root();
    let tmp_root_path = tmp_root.path().to_path_buf();

    // Spawn two concurrent installs
    let tar1 = tar.clone();
    let checksum1 = checksum.clone();
    let dest1_path = dest1.path().to_path_buf();
    let home1_path = home1.path().to_path_buf();
    let tmp_root_path1 = tmp_root_path.clone();

    let handle1 = std::thread::spawn(move || {
        Command::new("bash")
            .arg("install.sh")
            .arg("--version")
            .arg("vtest")
            .arg("--dest")
            .arg(&dest1_path)
            .arg("--easy-mode")
            .env("HOME", home1_path)
            .env("TMPDIR", tmp_root_path1)
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
    let tmp_root_path2 = tmp_root_path;

    let handle2 = std::thread::spawn(move || {
        Command::new("bash")
            .arg("install.sh")
            .arg("--version")
            .arg("vtest")
            .arg("--dest")
            .arg(&dest2_path)
            .arg("--easy-mode")
            .env("HOME", home2_path)
            .env("TMPDIR", tmp_root_path2)
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
    let tar = fixture("tests/fixtures/install/coding-agent-search-vtest-linux-x86_64.tar.gz");
    let checksum = fs::read_to_string(
        "tests/fixtures/install/coding-agent-search-vtest-linux-x86_64.tar.gz.sha256",
    )
    .unwrap()
    .trim()
    .to_string();
    let dest = tempfile::TempDir::new().unwrap();
    let home = isolated_home();
    let tmp_root = isolated_install_tmp_root();

    let output = install_sh_command(&tmp_root)
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

#[test]
#[serial]
#[cfg_attr(not(target_os = "linux"), ignore)]
fn verify_flag_rejects_a_binary_whose_version_probe_fails() {
    let artifact_dir = tempfile::TempDir::new().unwrap();
    let payload_dir = tempfile::TempDir::new().unwrap();
    let payload_cass = payload_dir.path().join("cass");
    make_executable_script(&payload_cass, "#!/bin/sh\nexit 42\n");

    let tar_path = artifact_dir.path().join("cass-linux-amd64.tar.gz");
    let tar_status = Command::new("tar")
        .arg("-czf")
        .arg(&tar_path)
        .arg("-C")
        .arg(payload_dir.path())
        .arg("cass")
        .status()
        .expect("create failing-binary tarball");
    assert!(tar_status.success(), "test tarball should be created");

    let checksum = file_sha256_hex(&tar_path);
    let dest = tempfile::TempDir::new().unwrap();
    let home = isolated_home();
    let tmp_root = isolated_install_tmp_root();
    let output = install_sh_command(&tmp_root)
        .arg("--version")
        .arg("vtest")
        .arg("--dest")
        .arg(dest.path())
        .arg("--easy-mode")
        .arg("--verify")
        .env("HOME", home.path())
        .env("ARTIFACT_URL", format!("file://{}", tar_path.display()))
        .env("CHECKSUM", checksum)
        .output()
        .expect("run install.sh with a failing version probe");

    assert!(
        !output.status.success(),
        "--verify must fail when the installed binary exits non-zero"
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("Self-test failed"),
        "verification failure should be explicit, got: {combined}"
    );
    assert!(
        !combined.contains("Self-test complete"),
        "installer must not claim a failed self-test completed"
    );
}

#[test]
fn powershell_verify_contract_fails_closed_on_native_command_errors() {
    let script = fs::read_to_string("install.ps1").expect("read install.ps1");
    for required in [
        "$LASTEXITCODE = $null",
        "$verifyExitCode = $LASTEXITCODE",
        "if ($null -eq $verifyExitCode)",
        "did not report an exit code",
        "if ($verifyExitCode -ne 0)",
        "exit $verifyExitCode",
        "Self-test complete",
    ] {
        assert!(
            script.contains(required),
            "PowerShell verification is missing: {required}"
        );
    }
}

#[test]
fn installers_reject_link_and_special_archive_entries() {
    let shell = fs::read_to_string("install.sh").expect("read install.sh");
    for required in [
        "tar -tvzf \"$archive\" > \"$metadata_list\"",
        "Archive contains unsupported entry type",
        "[ -f \"$BIN\" ] && [ -x \"$BIN\" ]",
    ] {
        assert!(
            shell.contains(required),
            "POSIX archive type validation is missing: {required}"
        );
    }

    let powershell = fs::read_to_string("install.ps1").expect("read install.ps1");
    for required in [
        "function Test-ZipEntryHasSafeType",
        "($Entry.ExternalAttributes -shr 16) -band 0xF000",
        "$unixType -eq 0x8000",
        "$unixType -eq 0x4000",
        "Test-ZipEntryHasSafeType $Entry",
    ] {
        assert!(
            powershell.contains(required),
            "PowerShell archive type validation is missing: {required}"
        );
    }
}
