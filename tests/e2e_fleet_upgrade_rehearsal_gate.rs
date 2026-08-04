//! Real-binary gate for the `cass fleet upgrade-rehearsal` surface.
//!
//! Bead `coding_agent_session_search-sc8sp` ("Wire fleet upgrade rehearsal
//! (6.6) into a cass CLI/robot surface").
//!
//! Why this exists
//! ---------------
//! `src/fleet_upgrade_rehearsal.rs` (bead 6.6) is the pure composition core
//! (`rehearse_host`/`rehearse_fleet`/`verify_post_upgrade`) over the version-skew
//! (6.3) and archive-coverage (6.4) assessments, with no live caller. This gate
//! drives the **real `cass` binary** for the new `cass fleet upgrade-rehearsal`
//! command and proves the surface-level contract the unit tests cannot:
//!
//!   1. **Pure JSON on stdout** — `serde_json::from_str` consumes the whole
//!      trimmed stdout (stdout=data / stderr=diagnostics hygiene). A robot
//!      surface that leaks a diagnostic onto stdout fails this.
//!   2. **Local-by-default, mutation-free** — without `--live` the rehearsal
//!      covers only the local host from cass-owned local evidence, contacts no
//!      remote machine (`probed_remote_hosts=false`, `mode="fixture"`), and
//!      leaves a configured `sources.toml` byte-identical.
//!   3. **Honest disposition** — with the default target (this binary's version)
//!      the local host is `up-to-date`; with a far-ahead `--target-version` it
//!      needs an upgrade and carries a bounded, archive-safe plan.
//!   4. **Safe next commands** — every emitted safe/recommended command is a
//!      concrete `cass …` invocation and never a destructive one; the blocked
//!      commands are kept strictly separate.
//!   5. **Bounded local verification** — `--verify` drives the post-upgrade
//!      check battery against the local binary through the shared E2E runner and
//!      classifies each into a proof artifact, surfaced under `local_verification`.
//!
//! All commands run against an isolated empty data dir + config dir, so the gate
//! never touches the operator's real corpus, and every invocation is bounded so a
//! hang surfaces as a loud `TIMEOUT DIAGNOSTIC` rather than a silent stall.

mod util;

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use serde_json::Value;

use util::timeout::spawn_with_timeout_or_diag;

/// Bound for a single rehearsal invocation. The default/target paths are
/// sub-second; `--verify` spawns a handful of bounded `--json` post-checks
/// against an empty data dir, comfortably under this ceiling.
const REHEARSAL_TIMEOUT: Duration = Duration::from_secs(90);

/// Build a `cass` command with the standard test isolation so the rehearsal
/// never reaches the operator's real corpus, config, or update prompt.
/// `ignore_sources` controls whether the configured `sources.toml` is honored —
/// the deferral test needs it honored so it can assert a remote source is named
/// but not contacted.
fn fleet_command(home: &Path, args: &[&str], ignore_sources: bool) -> Command {
    let mut cmd = Command::new(util::cass_bin());
    cmd.args(args)
        .current_dir(home)
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join("xdg-data"))
        .env("XDG_CONFIG_HOME", home.join("xdg-config"))
        .env("XDG_CACHE_HOME", home.join("xdg-cache"))
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_SEMANTIC_EMBEDDER", "hash")
        .env("NO_COLOR", "1")
        .env_remove("CODEX_HOME")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CASS_FLEET_BUDGET_MS")
        .env_remove("CASS_FLEET_PER_HOST_BUDGET_MS")
        .env_remove("CASS_TEST_FLEET_PROBE_SLOW_MS")
        .env_remove("CASS_TEST_FLEET_VERIFY_SLOW_MS");
    if ignore_sources {
        cmd.env("CASS_IGNORE_SOURCES_CONFIG", "1");
    }
    cmd
}

/// Run the rehearsal and parse stdout as one pure JSON object. The parse failing
/// is itself the stdout-hygiene assertion.
fn run_rehearsal_json(home: &Path, args: &[&str], ignore_sources: bool) -> Value {
    run_rehearsal_json_with_expected_status(home, args, ignore_sources, true)
}

fn run_rehearsal_json_with_expected_status(
    home: &Path,
    args: &[&str],
    ignore_sources: bool,
    expect_success: bool,
) -> Value {
    let data_dir = home.join("xdg-data").join("coding-agent-search");
    let cmd = fleet_command(home, args, ignore_sources);
    let out = spawn_with_timeout_or_diag(
        cmd,
        "fleet-upgrade-rehearsal",
        Some(&data_dir),
        REHEARSAL_TIMEOUT,
    );
    assert_eq!(
        out.status.success(),
        expect_success,
        "cass {args:?} had unexpected status {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    // No alt-screen / ANSI escapes on stdout (would indicate a bare TUI launch).
    assert!(
        !out.stdout.contains(&0x1b),
        "cass {args:?} leaked ANSI escapes onto stdout"
    );
    let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
    // Surface full context on stderr before failing (debuggable without a
    // rerun); fail via `expect` rather than the `panic!` macro — the bug
    // scanner classes a bare `panic!` as critical, while `expect` is the
    // accepted test idiom.
    let parsed = serde_json::from_str::<Value>(stdout.trim());
    if parsed.is_err() {
        eprintln!("cass {args:?} stdout is not a single pure JSON object:\n{stdout}");
    }
    parsed.expect("rehearsal stdout must be a single pure JSON object")
}

/// The envelope's invariant fields, asserted everywhere.
fn assert_envelope_invariants(v: &Value, expected_mode: &str) {
    assert_eq!(
        v["mutation_free"].as_bool(),
        Some(true),
        "rehearsal must always be mutation-free"
    );
    assert_eq!(
        v["mode"].as_str(),
        Some(expected_mode),
        "unexpected rehearsal mode"
    );
    assert_eq!(
        v["generated_by"].as_str(),
        Some("cass fleet upgrade-rehearsal")
    );
    assert!(
        v["schema_version"].as_u64().is_some(),
        "schema_version must be an integer"
    );
    assert!(
        v["rehearsal"].is_object(),
        "rehearsal block must be present"
    );
    assert!(
        v["target_version"].as_str().is_some_and(|s| !s.is_empty()),
        "target_version must be a non-empty string"
    );
    assert!(
        v["budget"].is_object(),
        "rehearsal must expose its bounded execution contract"
    );
    assert!(v["budget"]["elapsed_ms"].as_u64().is_some());
    assert!(v["budget"]["budget_ms"].as_u64().is_some());
}

/// A safe next command carries no recommendation-shaped destructive token. It is
/// either a concrete `cass …` invocation or the blessed binary installer
/// (curl|bash / brew / scoop / `cass self-update`) — never a data-loss command.
fn is_safe_command(cmd: &str) -> bool {
    let trimmed = cmd.trim();
    let head = trimmed.split("   #").next().unwrap_or(trimmed).trim();
    let nondestructive = !head.contains("rm ")
        && !head.contains("--purge")
        && !head.contains("--delete")
        && !head.to_ascii_lowercase().contains("drop ")
        && !head.contains("> /")
        && !head.contains("dd ");
    let recognized = head.starts_with("cass ")
        || head.contains("install.sh")
        || head.contains("brew ")
        || head.contains("scoop ")
        || head.contains("self-update");
    nondestructive && recognized
}

fn write_unreachable_remote_source(home: &Path) {
    let config_dir = home.join("xdg-config").join("cass");
    std::fs::create_dir_all(&config_dir).expect("create config dir");
    let sources_toml = config_dir.join("sources.toml");
    let toml = "[[sources]]\nname = \"unreachable-host\"\ntype = \"ssh\"\nhost = \"nobody@unreachable.invalid\"\npaths = [\"~/.claude/projects\"]\n";
    std::fs::write(sources_toml, toml).expect("write sources.toml");
}

#[test]
fn default_rehearsal_is_pure_local_json_and_up_to_date() {
    let home = tempfile::tempdir().expect("temp home");
    // Default target is this binary's version, so the only host (the local one)
    // is already current.
    let v = run_rehearsal_json(home.path(), &["fleet", "upgrade-rehearsal", "--json"], true);

    assert_envelope_invariants(&v, "fixture");
    assert_eq!(
        v["probed_remote_hosts"].as_bool(),
        Some(false),
        "default mode must contact no remote host"
    );

    let rehearsal = &v["rehearsal"];
    let hosts = rehearsal["hosts"].as_array().expect("hosts array");
    assert_eq!(hosts.len(), 1, "default mode covers only the local host");
    let local = &hosts[0];
    assert_eq!(local["host_alias"].as_str(), Some("local"));
    assert_eq!(
        local["disposition"].as_str(),
        Some("up-to-date"),
        "local host at the running version is up-to-date"
    );
    assert_eq!(rehearsal["hosts_up_to_date"].as_u64(), Some(1));
    assert_eq!(rehearsal["hosts_needing_upgrade"].as_u64(), Some(0));
    assert_eq!(rehearsal["hosts_unreachable"].as_u64(), Some(0));
    // No verification was requested.
    assert!(
        v.get("local_verification").is_none(),
        "verification only appears with --verify"
    );
}

#[test]
fn far_ahead_target_makes_local_host_need_an_archive_safe_upgrade() {
    let home = tempfile::tempdir().expect("temp home");
    let v = run_rehearsal_json(
        home.path(),
        &[
            "fleet",
            "upgrade-rehearsal",
            "--target-version",
            "99.0.0",
            "--json",
        ],
        true,
    );

    assert_envelope_invariants(&v, "fixture");
    assert_eq!(v["target_version"].as_str(), Some("99.0.0"));

    let rehearsal = &v["rehearsal"];
    assert_eq!(rehearsal["hosts_needing_upgrade"].as_u64(), Some(1));
    let local = &rehearsal["hosts"][0];
    // A major jump behind the target is not a blind self-update.
    assert_eq!(
        local["disposition"].as_str(),
        Some("needs-manual-upgrade"),
        "a major version gap forces a manual installer path"
    );
    // The bounded post-upgrade checks the plan must clear are present.
    let post_checks = local["post_checks"].as_array().expect("post_checks array");
    assert!(
        !post_checks.is_empty(),
        "an upgrade plan must carry its bounded post-checks"
    );
    // The per-action plan distinguishes the five distinct upgrade actions.
    let actions = local["actions"].as_array().expect("actions array");
    assert_eq!(actions.len(), 5, "five distinct upgrade actions");
}

#[test]
fn every_emitted_next_command_is_a_safe_cass_invocation() {
    let home = tempfile::tempdir().expect("temp home");
    let v = run_rehearsal_json(
        home.path(),
        &[
            "fleet",
            "upgrade-rehearsal",
            "--target-version",
            "99.0.0",
            "--json",
        ],
        true,
    );

    let hosts = v["rehearsal"]["hosts"].as_array().expect("hosts array");
    let mut checked = 0usize;
    for host in hosts {
        if let Some(safe) = host["safe_next_commands"].as_array() {
            for cmd in safe {
                let cmd = cmd.as_str().expect("command string");
                assert!(
                    is_safe_command(cmd),
                    "unsafe or unrecognized safe command surfaced: {cmd:?}"
                );
                checked += 1;
            }
        }
        // Blocked commands are kept separate and must never appear in the safe set;
        // each carries its unblock precondition.
        if let Some(blocked) = host["blocked_next_commands"].as_array() {
            for b in blocked {
                assert!(
                    b["command"]
                        .as_str()
                        .is_some_and(|c| c.starts_with("cass ")),
                    "blocked command must still be a cass invocation: {b}"
                );
                assert!(
                    b["unblock_precondition"]
                        .as_str()
                        .is_some_and(|s| !s.is_empty()),
                    "a blocked command must name its unblock precondition: {b}"
                );
            }
        }
    }
    assert!(
        checked > 0,
        "an upgrade-needed host must surface at least one safe next command"
    );
}

#[test]
fn verify_drives_the_bounded_local_post_upgrade_battery() {
    let home = tempfile::tempdir().expect("temp home");
    let v = run_rehearsal_json_with_expected_status(
        home.path(),
        &["fleet", "upgrade-rehearsal", "--verify", "--json"],
        true,
        false,
    );

    assert_envelope_invariants(&v, "fixture");
    let verification = v
        .get("local_verification")
        .filter(|x| x.is_object())
        .expect("local_verification present with --verify");

    assert_eq!(verification["host_alias"].as_str(), Some("local"));
    let proofs = verification["check_proofs"]
        .as_array()
        .expect("check_proofs array");
    assert!(
        !proofs.is_empty(),
        "the post-upgrade battery must classify at least one check"
    );

    let known_checks = [
        "api-version",
        "health-status-readiness",
        "source-coverage",
        "quarantine-status",
        "lexical-semantic-fallback",
        "human-robot-parity",
    ];
    let known_statuses = [
        "pass",
        "fail",
        "partial-proof",
        "generated-only",
        "skipped",
        "stale-artifact",
        "timeout",
    ];
    for proof in proofs {
        let check = proof["check"].as_str().expect("check label");
        assert!(
            known_checks.contains(&check),
            "unknown check facet: {check}"
        );
        let status = proof["proof"]["status"].as_str().expect("proof status");
        assert!(
            known_statuses.contains(&status),
            "unknown proof status: {status}"
        );
    }
    assert_ne!(
        verification["overall_status"].as_str(),
        Some("pass"),
        "an isolated empty archive must not claim a trustworthy verification pass"
    );
    assert!(
        verification["summary"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "verification must carry a one-line summary"
    );
}

#[test]
fn requested_verification_skipped_by_budget_emits_json_and_exits_one() {
    let home = tempfile::tempdir().expect("temp home");
    let data_dir = home.path().join("xdg-data").join("coding-agent-search");
    let mut cmd = fleet_command(
        home.path(),
        &["fleet", "upgrade-rehearsal", "--verify", "--json"],
        true,
    );
    cmd.env("CASS_FLEET_BUDGET_MS", "50")
        .env("CASS_TEST_FLEET_VERIFY_SLOW_MS", "2000");
    let started = std::time::Instant::now();
    let out = spawn_with_timeout_or_diag(
        cmd,
        "fleet-upgrade-rehearsal-verify-budget",
        Some(&data_dir),
        REHEARSAL_TIMEOUT,
    );

    assert!(
        started.elapsed() < Duration::from_millis(1200),
        "verification fixture escaped the configured fleet budget"
    );
    assert!(
        !out.status.success(),
        "requested verification without proof must exit non-zero"
    );
    let payload: Value =
        serde_json::from_slice(&out.stdout).expect("skipped verification stdout must be JSON");
    assert_eq!(payload["local_verification"], Value::Null);
    assert_eq!(payload["budget"]["timed_out"], true);
    assert!(
        payload["budget"]["skipped_sections"]
            .as_array()
            .is_some_and(|sections| sections
                .iter()
                .any(|section| section.as_str() == Some("local_verification"))),
        "budget must name the unproved verification battery: {payload}"
    );
}

#[test]
fn remote_source_is_named_but_not_contacted_without_live() {
    let home = tempfile::tempdir().expect("temp home");
    // Configure a remote source whose host is in the RFC 6761 `.invalid` TLD, so
    // even an accidental probe would deterministically fail to resolve. Without
    // `--live` the rehearsal must not contact it at all.
    let config_dir = home.path().join("xdg-config").join("cass");
    std::fs::create_dir_all(&config_dir).expect("create config dir");
    let sources_toml = config_dir.join("sources.toml");
    let toml = "[[sources]]\nname = \"deferred-host\"\ntype = \"ssh\"\nhost = \"nobody@deferred.invalid\"\npaths = [\"~/.claude/projects\"]\n";
    std::fs::write(&sources_toml, toml).expect("write sources.toml");
    let before = std::fs::read(&sources_toml).expect("read sources.toml before");

    // Honor the configured sources (ignore_sources=false) so the remote source is
    // discovered, then assert it is deferred rather than probed.
    let v = run_rehearsal_json(
        home.path(),
        &["fleet", "upgrade-rehearsal", "--json"],
        false,
    );

    assert_envelope_invariants(&v, "fixture");
    assert_eq!(
        v["probed_remote_hosts"].as_bool(),
        Some(false),
        "without --live no remote host is contacted"
    );
    let deferred = v["deferred_remote_sources"]
        .as_array()
        .expect("deferred_remote_sources array");
    assert!(
        deferred.iter().any(|s| s.as_str() == Some("deferred-host")),
        "the configured remote source must be named as deferred, not silently dropped: {deferred:?}"
    );
    // The local host is still the only one in the rehearsal (remote not probed).
    let hosts = v["rehearsal"]["hosts"].as_array().expect("hosts array");
    assert!(
        hosts
            .iter()
            .all(|h| h["host_alias"].as_str() == Some("local")),
        "no remote host plan without --live"
    );

    // Mutation-free: classifying did not rewrite the source config.
    let after = std::fs::read(&sources_toml).expect("read sources.toml after");
    assert_eq!(
        before, after,
        "sources.toml must be byte-identical after a rehearsal"
    );
}

#[test]
fn unknown_source_filter_fails_closed_instead_of_rehearsing_only_local() {
    let home = tempfile::tempdir().expect("temp home");
    write_unreachable_remote_source(home.path());
    let data_dir = home.path().join("xdg-data").join("coding-agent-search");
    let cmd = fleet_command(
        home.path(),
        &[
            "fleet",
            "upgrade-rehearsal",
            "--source",
            "typo-host",
            "--json",
        ],
        false,
    );
    let out = spawn_with_timeout_or_diag(
        cmd,
        "fleet-upgrade-rehearsal-unknown-source",
        Some(&data_dir),
        REHEARSAL_TIMEOUT,
    );

    assert_eq!(
        out.status.code(),
        Some(13),
        "an unknown source filter must use the not-found exit code"
    );
    assert!(
        out.stdout.is_empty(),
        "an unknown source must not emit a misleading local-only rehearsal"
    );
    let error: Value =
        serde_json::from_slice(&out.stderr).expect("unknown-source stderr must be pure JSON");
    assert_eq!(error["error"]["kind"], "not-found");
    assert_eq!(error["error"]["retryable"], false);
    assert!(
        error["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("typo-host"))
            && error["error"]["hint"]
                .as_str()
                .is_some_and(|hint| hint.contains("unreachable-host")),
        "unknown-source error must identify the typo and list the configured source: {error}"
    );
}

#[test]
fn live_all_unreachable_emits_complete_json_then_exits_one() {
    let home = tempfile::tempdir().expect("temp home");
    write_unreachable_remote_source(home.path());

    let v = run_rehearsal_json_with_expected_status(
        home.path(),
        &["fleet", "upgrade-rehearsal", "--live", "--json"],
        false,
        false,
    );

    assert_envelope_invariants(&v, "live");
    assert_eq!(
        v["probed_remote_hosts"].as_bool(),
        Some(false),
        "an SSH exit-255 transport failure is not confirmed remote contact"
    );
    assert_eq!(v["rehearsal"]["hosts_unreachable"].as_u64(), Some(1));
    assert!(
        v["rehearsal"]["hosts"]
            .as_array()
            .is_some_and(|hosts| hosts.iter().any(|host| {
                host["host_alias"].as_str() == Some("unreachable-host")
                    && host["disposition"].as_str() == Some("unreachable")
            })),
        "the non-zero payload must preserve the unreachable host evidence"
    );
}

#[test]
fn live_probe_timeout_preserves_host_identity_and_partial_budget() {
    let home = tempfile::tempdir().expect("temp home");
    write_unreachable_remote_source(home.path());
    let data_dir = home.path().join("xdg-data").join("coding-agent-search");
    let mut cmd = fleet_command(
        home.path(),
        &[
            "fleet",
            "upgrade-rehearsal",
            "--live",
            "--source",
            "unreachable-host",
            "--target-version",
            "9.9.9",
            "--json",
            "--budget-ms",
            "1000",
            "--per-host-budget-ms",
            "50",
        ],
        false,
    );
    cmd.env("CASS_TEST_FLEET_PROBE_SLOW_MS", "2000");
    let started = std::time::Instant::now();
    let out = spawn_with_timeout_or_diag(
        cmd,
        "fleet-upgrade-rehearsal-budget-timeout",
        Some(&data_dir),
        REHEARSAL_TIMEOUT,
    );
    assert!(
        started.elapsed() < Duration::from_millis(1200),
        "pre-SSH fixture escaped the configured per-host budget"
    );
    assert!(
        out.status.success(),
        "a named partial timeout is a valid rehearsal report: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let payload: Value =
        serde_json::from_slice(&out.stdout).expect("fleet timeout stdout must be JSON");
    let budget = &payload["budget"];
    assert_eq!(budget["timed_out"], false);
    assert!(
        budget["skipped_sections"]
            .as_array()
            .is_some_and(|sections| sections.iter().any(|section| {
                section.as_str() == Some("source:unreachable-host:remote_probe")
            })),
        "fleet budget must name the skipped host probe: {budget}"
    );
    let retry = budget["recommended_next_probe"]
        .as_str()
        .expect("fleet partial must carry a retry probe");
    assert!(
        retry.contains(
            "fleet upgrade-rehearsal --target-version 9.9.9 --live --source unreachable-host --json"
        ) && retry.contains("--budget-ms 2000")
            && retry.contains("--per-host-budget-ms 1000")
            && retry.contains("--data-dir")
            && !retry.contains("CASS_FLEET_"),
        "fleet retry must preserve scope and raise both budgets with cross-platform CLI flags: {retry}"
    );
    assert_eq!(
        payload["probed_remote_hosts"], false,
        "a host shed before SSH starts must not be reported as contacted"
    );
    assert!(
        payload["rehearsal"]["hosts"]
            .as_array()
            .is_some_and(|hosts| hosts.iter().any(|host| {
                host["host_alias"].as_str() == Some("unreachable-host")
                    && host["disposition"].as_str() == Some("probe-timed-out")
                    && host["channel"].as_str() == Some("unsupported")
            })),
        "a timed-out host with no platform hint must survive without a fabricated Linux channel: {payload}"
    );
    assert_eq!(
        payload["rehearsal"]["hosts_unreachable"], 0,
        "a timeout is unknown and must not count as confirmed unreachable"
    );
    assert_eq!(
        payload["rehearsal"]["hosts_timed_out"], 1,
        "the timed-out host must have its own fleet rollup"
    );
}

#[cfg(unix)]
#[test]
fn live_hung_ssh_child_is_killed_within_the_host_budget() {
    use std::os::unix::fs::PermissionsExt;

    let home = tempfile::tempdir().expect("temp home");
    write_unreachable_remote_source(home.path());
    let fixture_bin = home.path().join("fixture-bin");
    std::fs::create_dir_all(&fixture_bin).expect("create fixture bin");
    let fake_ssh = fixture_bin.join("ssh");
    std::fs::write(&fake_ssh, "#!/bin/sh\nsleep 30\n").expect("write fake ssh");
    std::fs::set_permissions(&fake_ssh, std::fs::Permissions::from_mode(0o755))
        .expect("make fake ssh executable");
    let original_path = dotenvy::var("PATH").unwrap_or_default();
    let data_dir = home.path().join("xdg-data").join("coding-agent-search");
    let mut cmd = fleet_command(
        home.path(),
        &["fleet", "upgrade-rehearsal", "--live", "--json"],
        false,
    );
    cmd.env("PATH", format!("{}:{original_path}", fixture_bin.display()))
        .env("CASS_FLEET_PER_HOST_BUDGET_MS", "150")
        .env("CASS_FLEET_BUDGET_MS", "1000");
    let started = std::time::Instant::now();
    let out = spawn_with_timeout_or_diag(
        cmd,
        "fleet-upgrade-rehearsal-hung-ssh",
        Some(&data_dir),
        REHEARSAL_TIMEOUT,
    );

    assert!(
        started.elapsed() < Duration::from_secs(2),
        "hung SSH child or grandchild escaped the fleet deadline"
    );
    assert!(
        out.status.success(),
        "a named partial timeout is a valid rehearsal report: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let payload: Value = serde_json::from_slice(&out.stdout).expect("hung-ssh stdout must be JSON");
    assert_eq!(
        payload["probed_remote_hosts"], false,
        "starting a local ssh child is not proof that the remote command layer was contacted"
    );
    assert_eq!(payload["budget"]["timed_out"], false);
    assert!(
        payload["budget"]["elapsed_ms"]
            .as_u64()
            .is_some_and(|elapsed| elapsed < 1000),
        "fleet report exceeded its configured total deadline: {payload}"
    );
    assert!(
        payload["budget"]["skipped_sections"]
            .as_array()
            .is_some_and(|sections| sections
                .iter()
                .any(|section| section.as_str() == Some("source:unreachable-host:remote_probe"))),
        "the timed-out host probe must be named: {payload}"
    );
}

#[test]
fn live_all_unreachable_emits_human_report_before_exit_one() {
    let home = tempfile::tempdir().expect("temp home");
    write_unreachable_remote_source(home.path());
    let data_dir = home.path().join("xdg-data").join("coding-agent-search");
    let cmd = fleet_command(
        home.path(),
        &["fleet", "upgrade-rehearsal", "--live"],
        false,
    );
    let out = spawn_with_timeout_or_diag(
        cmd,
        "fleet-upgrade-rehearsal-human-unreachable",
        Some(&data_dir),
        REHEARSAL_TIMEOUT,
    );

    assert_eq!(out.status.code(), Some(1));
    let stdout = String::from_utf8(out.stdout).expect("utf8 human stdout");
    assert!(stdout.contains("Fleet upgrade rehearsal"));
    assert!(stdout.contains("unreachable-host [unreachable]"));
    assert!(stdout.contains("mutation-free"));
}
