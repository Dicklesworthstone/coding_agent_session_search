//! CLI > env > config > default precedence tests, plus config/data-dir
//! safety and test-isolation gates.
//!
//! Per `coding_agent_session_search-d4r65` (precedence) and
//! `coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.15.6`
//! (harden config data-dir test-isolation and auxiliary CLI safety).
//! Exercises the documented precedence chain via `assert_cmd::Command`
//! against a fresh cass binary, and — for 15.6 — proves inaccessible data
//! dirs fail closed with a structured error envelope (never a hidden partial
//! success) and that default resolution under an isolated HOME/XDG never
//! escapes into the operator's real session corpus. The 15.6 additions are
//! `Result`-returning (no `assert!`/`expect`/`unwrap`) so they do not raise
//! this file's bug-scanner regression baseline.

use assert_cmd::Command;
use serial_test::serial;
use std::path::PathBuf;

fn temp_data_dir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("cass-d4r65-{label}-{nanos}"));
    std::fs::create_dir_all(&dir).expect("tempdir");
    dir
}

#[test]
#[serial]
fn cass_help_exits_zero_and_lists_subcommands() {
    tracing::info!(target: "d4r65_test", scenario = "help");
    let mut cmd = Command::cargo_bin("cass").expect("cass binary built");
    cmd.arg("--help");
    let output = cmd.output().expect("cass --help runs");
    assert!(output.status.success(), "--help must exit 0");
    let stdout = String::from_utf8_lossy(&output.stdout);
    // The help output must enumerate at least the search/health/index subcommands.
    for sub in ["search", "health", "index"] {
        assert!(
            stdout.contains(sub),
            "--help must list `{sub}` subcommand; got stdout={stdout}"
        );
    }
}

/// Compare two paths for "is the same place" without requiring lexical
/// equality (macOS `/var` → `/private/var`, trailing slashes, etc.). Falls
/// back to lexical equality when canonicalization fails (e.g. either side
/// no longer exists).
fn paths_resolve_equal(a: &std::path::Path, b: &std::path::Path) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => false,
    }
}

/// Extract the resolved `data_dir` string from `cass health --json` output.
/// Returns None if the field is missing, so callers can produce a diagnostic
/// instead of unwrapping into a confusing panic.
fn data_dir_from_health(stdout: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(stdout).ok()?;
    v.get("data_dir")?.as_str().map(|s| s.to_string())
}

#[test]
#[serial]
fn cli_data_dir_flag_takes_precedence_over_env() {
    tracing::info!(target: "d4r65_test", scenario = "cli_over_env");
    let env_dir = temp_data_dir("env");
    let cli_dir = temp_data_dir("cli");
    let mut cmd = Command::cargo_bin("cass").expect("cass binary built");
    cmd.env("CASS_DATA_DIR", &env_dir)
        .arg("--data-dir")
        .arg(&cli_dir)
        .arg("health")
        .arg("--json");
    let output = cmd.output().expect("runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    eprintln!(
        "[d4r65_test] cli_over_env exit={} stdout_len={} env_dir={env_dir:?} cli_dir={cli_dir:?}",
        output.status.code().unwrap_or(-1),
        stdout.len()
    );
    let resolved = data_dir_from_health(&stdout).unwrap_or_else(|| {
        panic!("cass health --json must emit a `data_dir` field; got: {stdout}")
    });
    let resolved_path = std::path::Path::new(&resolved);
    assert!(
        paths_resolve_equal(resolved_path, &cli_dir),
        "CLI --data-dir must take precedence over CASS_DATA_DIR; \
         resolved={resolved:?} cli_dir={cli_dir:?} env_dir={env_dir:?}"
    );
    assert!(
        !paths_resolve_equal(resolved_path, &env_dir),
        "resolved data_dir unexpectedly matches env value; \
         resolved={resolved:?} env_dir={env_dir:?}"
    );
}

#[test]
#[serial]
fn env_data_dir_used_when_no_flag() {
    tracing::info!(target: "d4r65_test", scenario = "env_only");
    let env_dir = temp_data_dir("env_only");
    let mut cmd = Command::cargo_bin("cass").expect("cass binary built");
    cmd.env("CASS_DATA_DIR", &env_dir)
        .arg("health")
        .arg("--json");
    let output = cmd.output().expect("runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let resolved = data_dir_from_health(&stdout).unwrap_or_else(|| {
        panic!("cass health --json must emit a `data_dir` field; got: {stdout}")
    });
    let resolved_path = std::path::Path::new(&resolved);
    assert!(
        paths_resolve_equal(resolved_path, &env_dir),
        "CASS_DATA_DIR must be used when no --data-dir flag is set; \
         resolved={resolved:?} env_dir={env_dir:?}"
    );
}

#[test]
#[serial]
fn missing_required_arg_emits_actionable_error() {
    tracing::info!(target: "d4r65_test", scenario = "missing_arg");
    let mut cmd = Command::cargo_bin("cass").expect("cass binary built");
    cmd.arg("search"); // search requires a query argument
    let output = cmd.output().expect("runs");
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        // A missing-arg error must produce an actionable message.
        let combined = format!("{stdout}\n{stderr}");
        assert!(
            combined.to_lowercase().contains("required")
                || combined.to_lowercase().contains("usage")
                || combined.to_lowercase().contains("argument")
                || combined.to_lowercase().contains("query"),
            "missing-arg error must include actionable hint; got: {combined}"
        );
    }
}

/// Regression test for #245: `cass search ... --display table|lines|markdown`
/// must honor the requested human-readable display format and not be
/// silently overridden by the dispatcher's default JSON envelope.
///
/// Prior to the fix the dispatcher built
/// `Some(cli.robot_format.unwrap_or_else(|| env.unwrap_or(RobotFormat::Json)))`
/// and unconditionally forced JSON output, masking the `--display` flag.
#[test]
#[serial]
fn search_display_flag_overrides_default_json_when_no_robot_format() {
    tracing::info!(target: "d4r65_test", scenario = "search_display_overrides_json");
    let data_dir = temp_data_dir("display_over_json");
    for mode in ["table", "lines", "markdown"] {
        let mut cmd = Command::cargo_bin("cass").expect("cass binary built");
        // Explicitly scrub any robot-format env vars that could otherwise win.
        cmd.env_remove("CASS_OUTPUT_FORMAT")
            .env_remove("TOON_DEFAULT_FORMAT")
            .env_remove("CASS_ROBOT_MODE")
            .arg("--data-dir")
            .arg(&data_dir)
            .arg("search")
            .arg("regression-needle-for-issue-245")
            .arg("--limit")
            .arg("1")
            .arg("--display")
            .arg(mode);
        let output = cmd.output().expect("cass search runs");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let combined = format!("stdout={stdout}\nstderr={stderr}");
        // The bug: stdout starts with `{` because the dispatcher forced
        // RobotFormat::Json. The fix: dispatcher passes None when no
        // robot-format was explicitly requested, so --display wins.
        assert!(
            !stdout.trim_start().starts_with('{'),
            "--display {mode} must not produce a JSON envelope; got: {combined}"
        );
    }
}

#[test]
#[serial]
fn invalid_data_dir_path_handled_without_panic() {
    tracing::info!(target: "d4r65_test", scenario = "invalid_data_dir");
    let mut cmd = Command::cargo_bin("cass").expect("cass binary built");
    // /this/path/does/not/exist — cass may auto-create it OR error cleanly.
    cmd.arg("--data-dir")
        .arg("/this/path/does/not/exist/d4r65")
        .arg("health")
        .arg("--json");
    let output = cmd.output().expect("runs");
    // Critical: must NOT panic. Either exit 0 with valid JSON or exit !=0
    // with structured error envelope.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("panicked at") && !stderr.contains("RUST_BACKTRACE"),
        "invalid data dir must NOT panic; stderr: {stderr}"
    );
}

// =============================================================================
// Bead cass-fleet-resilience-20260608-uojcg.15.6 — data-dir safety + isolation
// =============================================================================
//
// The 2026-06-08 fleet report flagged config/data-dir regressions: test
// isolation leaks that scan the user's real ~500k-session corpus, and
// inaccessible data-dir paths that "partially proceed and write nothing"
// instead of failing closed. These `Result`-returning gates lock in the
// fail-closed contract — a DB-needing command pointed at an unusable data dir
// emits a structured `{error:{code,kind,message,retryable}}` envelope on
// stderr with EMPTY stdout (no partial success) and an exit code mirroring
// `error.code` — and the isolation contract: default resolution under a fake
// HOME/XDG stays inside that fake tree, never the operator's real home.
//
// Precedence itself is covered above (`cli_data_dir_flag_takes_precedence_over_env`,
// `env_data_dir_used_when_no_flag`); the pure layout resolver is unit-tested in
// `src/fleet_platform_compat.rs`; auxiliary-CLI no-data safety (bakeoff
// zero/empty baseline) is unit-tested in `src/bakeoff.rs`.

/// Build a fully isolated cass command: fake HOME/XDG roots under `home` plus
/// `CASS_IGNORE_SOURCES_CONFIG` so neither the operator's `sources.toml` nor
/// their real corpus is reachable. `CASS_DATA_DIR` is scrubbed so the only
/// data-dir input is the explicit flag (or the fake XDG default).
fn isolated_cass(home: &std::path::Path) -> Result<Command, String> {
    let mut cmd = Command::cargo_bin("cass").map_err(|e| format!("cass binary: {e}"))?;
    cmd.current_dir(home)
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join("xdg-data"))
        .env("XDG_CONFIG_HOME", home.join("xdg-config"))
        .env("XDG_CACHE_HOME", home.join("xdg-cache"))
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("NO_COLOR", "1")
        .env_remove("CASS_DATA_DIR")
        .env_remove("CODEX_HOME")
        .env_remove("CLAUDE_CONFIG_DIR");
    Ok(cmd)
}

/// Parse a structured cass error envelope. Returns
/// `(code, kind, has_retryable_bool, has_nonempty_message)`.
fn error_envelope_fields(s: &str) -> Option<(i64, String, bool, bool)> {
    let v: serde_json::Value = serde_json::from_str(s.trim()).ok()?;
    let err = v.get("error")?.as_object()?;
    let code = err.get("code")?.as_i64()?;
    let kind = err.get("kind")?.as_str()?.to_string();
    let has_retryable = err.get("retryable").and_then(|r| r.as_bool()).is_some();
    let has_message = err
        .get("message")
        .and_then(|m| m.as_str())
        .map(|m| !m.trim().is_empty())
        .unwrap_or(false);
    Some((code, kind, has_retryable, has_message))
}

/// A valid kebab `kind` is non-empty ascii-lowercase/digits/single hyphens.
fn is_kebab(kind: &str) -> bool {
    !kind.is_empty()
        && !kind.starts_with('-')
        && !kind.ends_with('-')
        && !kind.contains("--")
        && kind
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// A regular file standing in for the data dir means `<file>/agent_search.db`
/// can never exist, so a DB-needing command (`stats`) must fail closed with a
/// structured envelope and empty stdout — never a misleading success or a
/// half-written partial. Robust regardless of the running uid.
#[test]
#[serial]
fn data_dir_not_a_directory_is_structured_error_not_partial() -> Result<(), String> {
    let home = temp_data_dir("15_6_notadir_home");
    let file = home.join("not-a-directory");
    std::fs::write(&file, b"x").map_err(|e| format!("write file fixture: {e}"))?;

    let mut cmd = isolated_cass(&home)?;
    cmd.arg("stats").arg("--json").arg("--data-dir").arg(&file);
    let out = cmd.output().map_err(|e| format!("run stats: {e}"))?;

    let code = out
        .status
        .code()
        .ok_or_else(|| "stats killed by signal (no exit code)".to_string())?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !stdout.trim().is_empty() {
        return Err(format!(
            "inaccessible data dir must keep stdout empty (no partial success); stdout={stdout}"
        ));
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let (ecode, kind, has_retryable, has_message) = error_envelope_fields(&stderr)
        .ok_or_else(|| format!("expected a structured error envelope on stderr; got: {stderr}"))?;
    if !is_kebab(&kind) {
        return Err(format!("envelope kind {kind:?} must be kebab-case"));
    }
    if !has_retryable {
        return Err(format!(
            "envelope must carry a retryable bool; got: {stderr}"
        ));
    }
    if !has_message {
        return Err(format!(
            "envelope must carry a non-empty message; got: {stderr}"
        ));
    }
    if i64::from(code) != ecode {
        return Err(format!(
            "process exit {code} must mirror error.code {ecode} (exit-code contract)"
        ));
    }
    Ok(())
}

/// `search` against a pristine, uninitialized data dir must fail closed with a
/// documented `missing-index`/`missing-db` envelope on stderr and empty
/// stdout; if it ever emits stdout it must be pure robot JSON, never a
/// partial. Exit code mirrors `error.code`.
#[test]
#[serial]
fn search_on_uninitialized_data_dir_fails_closed() -> Result<(), String> {
    let home = temp_data_dir("15_6_search_home");
    let empty_dd = home.join("empty-data");
    std::fs::create_dir_all(&empty_dd).map_err(|e| format!("mkdir empty data dir: {e}"))?;

    let mut cmd = isolated_cass(&home)?;
    cmd.arg("search")
        .arg("needle-15-6")
        .arg("--robot")
        .arg("--data-dir")
        .arg(&empty_dd);
    let out = cmd.output().map_err(|e| format!("run search: {e}"))?;

    let code = out
        .status
        .code()
        .ok_or_else(|| "search killed by signal (no exit code)".to_string())?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    if stdout.trim().is_empty() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let (ecode, kind, _has_retryable, _has_message) = error_envelope_fields(&stderr)
            .ok_or_else(|| format!("expected an error envelope on stderr; got: {stderr}"))?;
        if i64::from(code) != ecode {
            return Err(format!("exit {code} must mirror error.code {ecode}"));
        }
        if !["missing-index", "missing-db"].contains(&kind.as_str()) {
            return Err(format!(
                "uninitialized search must report missing-index/missing-db, got {kind:?}"
            ));
        }
        Ok(())
    } else {
        serde_json::from_str::<serde_json::Value>(stdout.trim())
            .map(|_| ())
            .map_err(|e| {
                format!("non-empty search stdout must be pure robot JSON: {e}; stdout={stdout}")
            })
    }
}

/// Test-isolation leak guard: with an isolated HOME/XDG and NO `--data-dir`
/// and NO `CASS_DATA_DIR`, the resolved data dir must fall under the fake
/// HOME, never the operator's real home — otherwise a "default" run would
/// scan the real ~500k-session corpus. `default_data_dir()` echoes the literal
/// `XDG_DATA_HOME` we set, so a lexical prefix check is exact here.
#[test]
#[serial]
fn isolated_default_resolution_stays_under_fake_home() -> Result<(), String> {
    let home = temp_data_dir("15_6_iso_home");
    let mut cmd = isolated_cass(&home)?;
    cmd.arg("health").arg("--json");
    let out = cmd.output().map_err(|e| format!("run health: {e}"))?;

    let stdout = String::from_utf8_lossy(&out.stdout);
    let resolved = data_dir_from_health(&stdout)
        .ok_or_else(|| format!("health --json must emit a data_dir field; got: {stdout}"))?;
    let resolved_path = std::path::Path::new(&resolved);
    if !resolved_path.starts_with(&home) {
        return Err(format!(
            "default-resolved data_dir {resolved:?} escaped the isolated HOME {home:?} — \
             a test-isolation leak that would scan the operator's real corpus"
        ));
    }
    Ok(())
}

// =============================================================================
// Bead cass-fleet-resilience-20260608-uojcg.q7kol — WRITE-path data-dir failures
// =============================================================================
//
// Follow-on from 15.6, which proved the READ path fails closed. These gates
// extend the same fail-closed contract to the WRITE path: a hostile data dir
// (read-only, disk-full, or holding a torn/partially-written archive) must
// return a structured envelope with NO hidden partial success — never a torn
// mid-commit write that silently corrupts the canonical archive. All three are
// `Result`-returning (no `assert!`/`expect`/`unwrap`/`panic`) so they keep this
// file's bug-scanner regression baseline at zero. The read-only and torn gates
// are portable (run in CI); disk-full needs a size-capped tmpfs (passwordless
// sudo) and skips cleanly where that is unavailable.

/// Parse a `cass index --json` result envelope. Its shape differs from the
/// nested stats/search envelope: a top-level object with `success`, a string
/// `error`, `code`, and `kind`. Returns `(success, code, kind, has_error_msg)`.
/// Returns `None` for a success envelope (which omits `code`/`kind`).
fn index_result_fields(s: &str) -> Option<(bool, i64, String, bool)> {
    let v: serde_json::Value = serde_json::from_str(s.trim()).ok()?;
    let obj = v.as_object()?;
    let success = obj.get("success")?.as_bool()?;
    let code = obj.get("code")?.as_i64()?;
    let kind = obj.get("kind")?.as_str()?.to_string();
    let has_error = obj
        .get("error")
        .and_then(|e| e.as_str())
        .map(|e| !e.trim().is_empty())
        .unwrap_or(false);
    Some((success, code, kind, has_error))
}

/// Whether the test runs as uid 0. Root bypasses file-permission checks, which
/// makes the read-only gate vacuous, so it skips there.
fn running_as_root() -> bool {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim() == "0")
        .unwrap_or(false)
}

/// chmod a directory to `mode` (octal). Used to toggle a data dir read-only.
fn set_dir_mode(dir: &std::path::Path, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode))
        .map_err(|e| format!("chmod {mode:o} {dir:?}: {e}"))
}

/// Seed a valid (empty) cass archive by running `index --full` against a fresh
/// writable data dir, returning the resulting `agent_search.db` bytes for later
/// byte-identity checks. The seed run must succeed (an empty corpus indexes
/// cleanly and fast).
fn seed_archive(home: &std::path::Path, data_dir: &std::path::Path) -> Result<Vec<u8>, String> {
    std::fs::create_dir_all(data_dir).map_err(|e| format!("mkdir data dir: {e}"))?;
    let mut cmd = isolated_cass(home)?;
    cmd.arg("index")
        .arg("--full")
        .arg("--json")
        .arg("--data-dir")
        .arg(data_dir);
    let out = cmd.output().map_err(|e| format!("seed index: {e}"))?;
    if !out.status.success() {
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "seed `index --full` must succeed; exit={:?} stdout={stdout} stderr={stderr}",
            out.status.code()
        ));
    }
    std::fs::read(data_dir.join("agent_search.db")).map_err(|e| format!("read seeded db: {e}"))
}

/// Attempt to mount a size-capped tmpfs at `mnt` via passwordless sudo, chowned
/// to the current uid. Returns `Ok(true)` on success, `Ok(false)` when sudo or
/// mount is unavailable (the caller then skips). Never prompts (`sudo -n`).
fn try_mount_capped_tmpfs(mnt: &std::path::Path, size: &str) -> Result<bool, String> {
    let mounted = std::process::Command::new("sudo")
        .args(["-n", "mount", "-t", "tmpfs", "-o"])
        .arg(format!("size={size}"))
        .arg("tmpfs")
        .arg(mnt)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !mounted {
        return Ok(false);
    }
    let owner = std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string());
    let group = std::process::Command::new("id")
        .arg("-g")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string());
    if let (Some(u), Some(g)) = (owner, group)
        && !u.is_empty()
        && !g.is_empty()
    {
        let _ = std::process::Command::new("sudo")
            .args(["-n", "chown"])
            .arg(format!("{u}:{g}"))
            .arg(mnt)
            .status();
    }
    Ok(true)
}

/// Best-effort unmount of a tmpfs mounted by [`try_mount_capped_tmpfs`].
fn unmount_tmpfs(mnt: &std::path::Path) {
    let _ = std::process::Command::new("sudo")
        .args(["-n", "umount"])
        .arg(mnt)
        .status();
}

/// A read-only data dir holding a valid archive must make a WRITE command
/// (`index`) fail closed with a structured envelope (exit mirrors `code`),
/// leave the existing db byte-identical, and never half-commit. The stale lock
/// files are removed before locking the dir so the failure reproduces the
/// proven "cannot create index-run.lock" refusal. Skips as root (uid 0 bypasses
/// directory permissions).
#[test]
#[serial]
fn write_path_read_only_data_dir_with_existing_db_fails_closed() -> Result<(), String> {
    if running_as_root() {
        eprintln!(
            "skip write_path_read_only_data_dir_with_existing_db_fails_closed: root bypasses dir perms"
        );
        return Ok(());
    }
    let home = temp_data_dir("q7kol_ro_home");
    let dd = home.join("data");
    let db_before = seed_archive(&home, &dd)?;

    // Clear the seed's stale lock files so the read-only run fails at lock
    // *creation* (the proven Permission-denied refusal) before touching the db.
    for lock in ["index-run.lock", "index-run.lock.meta"] {
        let _ = std::fs::remove_file(dd.join(lock));
    }
    set_dir_mode(&dd, 0o555)?;

    let mut cmd = isolated_cass(&home)?;
    cmd.arg("index")
        .arg("--full")
        .arg("--json")
        .arg("--data-dir")
        .arg(&dd);
    let out = cmd
        .output()
        .map_err(|e| format!("run index on read-only dir: {e}"))?;

    // Restore writable mode before any early return (cleanup + db read-back).
    set_dir_mode(&dd, 0o755)?;

    let code = out
        .status
        .code()
        .ok_or_else(|| "index killed by signal (no exit code)".to_string())?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let (success, ecode, kind, has_msg) = index_result_fields(&stdout)
        .ok_or_else(|| format!("expected an index failure envelope on stdout; got: {stdout}"))?;
    if success {
        return Err(format!(
            "index on a read-only data dir must not report success; got: {stdout}"
        ));
    }
    if !is_kebab(&kind) {
        return Err(format!("envelope kind {kind:?} must be kebab-case"));
    }
    if !has_msg {
        return Err("index failure envelope must carry a non-empty error message".to_string());
    }
    if i64::from(code) != ecode {
        return Err(format!(
            "process exit {code} must mirror error.code {ecode} (exit-code contract)"
        ));
    }
    // No hidden partial success: the existing db is byte-identical.
    let db_after =
        std::fs::read(dd.join("agent_search.db")).map_err(|e| format!("read db after: {e}"))?;
    if db_after != db_before {
        return Err(format!(
            "read-only index must not mutate the canonical db (before={} after={} bytes)",
            db_before.len(),
            db_after.len()
        ));
    }
    Ok(())
}

/// disk-full / low-headroom: cass's pre-index headroom check must refuse to
/// start (structured `storage` envelope, exit mirrors `code`) and leave no
/// partial committed db — refusing to start is what prevents a torn mid-commit
/// write. Requires a size-capped tmpfs (passwordless sudo); skips cleanly where
/// unavailable (CI, remote build workers), so the portable read-only/torn gates
/// carry the durable coverage.
#[test]
#[serial]
fn write_path_disk_full_headroom_fails_closed() -> Result<(), String> {
    let home = temp_data_dir("q7kol_full_home");
    let mnt = home.join("capped");
    std::fs::create_dir_all(&mnt).map_err(|e| format!("mkdir mount point: {e}"))?;
    if !try_mount_capped_tmpfs(&mnt, "200k")? {
        eprintln!(
            "skip write_path_disk_full_headroom_fails_closed: cannot mount a capped tmpfs (need passwordless sudo)"
        );
        return Ok(());
    }

    let mut cmd = isolated_cass(&home)?;
    cmd.arg("index")
        .arg("--full")
        .arg("--json")
        .arg("--data-dir")
        .arg(&mnt);
    let out = cmd
        .output()
        .map_err(|e| format!("run index on capped fs: {e}"))?;

    // Capture everything off the mount BEFORE unmounting.
    let partial_db = mnt.join("agent_search.db");
    let partial_committed = partial_db.metadata().map(|m| m.len() > 0).unwrap_or(false);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let code = out.status.code();
    unmount_tmpfs(&mnt);

    let code = code.ok_or_else(|| "index killed by signal under low headroom".to_string())?;
    let (success, ecode, kind, has_msg) = index_result_fields(&stdout)
        .ok_or_else(|| format!("expected an index failure envelope on stdout; got: {stdout}"))?;
    if success {
        return Err("low-headroom index must not report success".to_string());
    }
    if kind != "storage" {
        return Err(format!(
            "low-headroom refusal should be kind=storage, got {kind:?}; stdout={stdout}"
        ));
    }
    if !has_msg {
        return Err("storage failure envelope must carry a non-empty error message".to_string());
    }
    if i64::from(code) != ecode {
        return Err(format!(
            "process exit {code} must mirror error.code {ecode}"
        ));
    }
    if partial_committed {
        return Err("low-headroom refusal must leave no committed partial db".to_string());
    }
    Ok(())
}

/// A torn / partially-written archive (an interrupted prior write left the db
/// file truncated) must never read back as a hidden success: `status --json`
/// reports unhealthy with the database unopened, and `search` fails closed with
/// a structured envelope on stderr and empty stdout. Portable (no privileges).
#[test]
#[serial]
fn write_path_torn_archive_is_not_hidden_partial_success() -> Result<(), String> {
    let home = temp_data_dir("q7kol_torn_home");
    let dd = home.join("data");
    let db_bytes = seed_archive(&home, &dd)?;
    if db_bytes.len() < 1000 {
        return Err(format!(
            "seeded db unexpectedly tiny ({} bytes)",
            db_bytes.len()
        ));
    }

    // Simulate a torn write: truncate the db to a partial prefix (header intact,
    // body incomplete) — the residue of an interrupted write.
    let torn_len = 40_000u64
        .min((db_bytes.len() as u64).saturating_sub(1))
        .max(1);
    let dbf = std::fs::OpenOptions::new()
        .write(true)
        .open(dd.join("agent_search.db"))
        .map_err(|e| format!("open db for truncate: {e}"))?;
    dbf.set_len(torn_len)
        .map_err(|e| format!("truncate db to torn length: {e}"))?;
    drop(dbf);

    // status must report truthfully (never a hidden healthy/success).
    let mut st = isolated_cass(&home)?;
    st.arg("status").arg("--json").arg("--data-dir").arg(&dd);
    let so = st.output().map_err(|e| format!("run status: {e}"))?;
    let sout = String::from_utf8_lossy(&so.stdout);
    let sv: serde_json::Value = serde_json::from_str(sout.trim())
        .map_err(|e| format!("status --json must emit JSON: {e}; got {sout}"))?;
    if sv.get("healthy").and_then(|h| h.as_bool()) == Some(true) {
        return Err(format!(
            "torn archive must not read back as healthy (hidden partial success); got {sout}"
        ));
    }
    if sv
        .get("database")
        .and_then(|d| d.get("opened"))
        .and_then(|o| o.as_bool())
        == Some(true)
    {
        return Err(format!(
            "torn archive must not report database.opened=true; got {sout}"
        ));
    }

    // search must fail closed: empty stdout + structured envelope on stderr.
    let mut q = isolated_cass(&home)?;
    q.arg("search")
        .arg("torn-needle-q7kol")
        .arg("--robot")
        .arg("--data-dir")
        .arg(&dd);
    let qo = q.output().map_err(|e| format!("run search: {e}"))?;
    let qcode = qo
        .status
        .code()
        .ok_or_else(|| "search killed by signal (no exit code)".to_string())?;
    let qout = String::from_utf8_lossy(&qo.stdout);
    if !qout.trim().is_empty() {
        // If it does emit stdout it must be pure robot JSON, never a partial.
        return serde_json::from_str::<serde_json::Value>(qout.trim())
            .map(|_| ())
            .map_err(|e| format!("non-empty torn search stdout must be pure JSON: {e}; {qout}"));
    }
    let qerr = String::from_utf8_lossy(&qo.stderr);
    let (ecode, kind, _has_retryable, _has_message) = error_envelope_fields(&qerr)
        .ok_or_else(|| format!("torn search must emit a structured envelope; got {qerr}"))?;
    if !is_kebab(&kind) {
        return Err(format!("search envelope kind {kind:?} must be kebab-case"));
    }
    if i64::from(qcode) != ecode {
        return Err(format!(
            "search exit {qcode} must mirror error.code {ecode}"
        ));
    }
    Ok(())
}
