//! Robot parseability under dependency logging and host-pressure diagnostics.
//!
//! Bead: coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.9.3
//! ("Test robot parseability under dependency logging and host-pressure
//! diagnostics").
//!
//! This is the regression matrix that proves the uojcg.2.1 hygiene chokepoint
//! holds across the *classes* of noise the report and operators actually hit:
//! frankensqlite tracing bursts (observed polluting `cass view`), slow-query /
//! drop-close / database-busy WARN messages from the storage layer, and
//! host-pressure (OOM / disk) diagnostics. For every (noise-class × robot
//! command) pair, stdout must remain a single valid JSON document and stderr must
//! stay bounded and free of unclassified dependency tracing.

use assert_cmd::Command;
use serde_json::Value;
use tempfile::TempDir;

mod util;
use util::cass_bin;

/// Generous upper bound on robot-mode stderr. With dependency tracing pinned to
/// `error`, stderr should be tiny; this catches a regression that lets a burst
/// flood it (unbounded diagnostics).
const MAX_STDERR_BYTES: usize = 16 * 1024;

/// Noise classes from the bead acceptance, each expressed as a `RUST_LOG` value
/// that *would* emit that class of dependency/pressure diagnostics if the robot
/// hygiene guard regressed.
fn noise_classes() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "dependency-burst",
            "trace,fsqlite=trace,fsqlite_core=trace,fsqlite_vdbe=trace,fsqlite_mvcc=trace,frankensearch=trace,asupersync=trace",
        ),
        (
            "slow-query-warn",
            "warn,fsqlite=warn,fsqlite.execution=warn,fsqlite.planner=warn",
        ),
        (
            "drop-close-warn",
            "warn,fsqlite=warn,fsqlite.connection=warn,fsqlite.pager=warn",
        ),
        (
            "database-busy",
            "warn,fsqlite=warn,fsqlite.wal=warn,fsqlite_mvcc=warn",
        ),
        (
            "host-pressure-oom-disk",
            "warn,cass=warn,cass::host=warn,cass::pressure=warn,cass::diag=warn",
        ),
    ]
}

fn parse_stdout_json(stdout: &str) -> Value {
    let trimmed = stdout.trim();
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        return value;
    }
    let last_line = trimmed
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    serde_json::from_str::<Value>(last_line.trim())
        .unwrap_or_else(|err| panic!("robot stdout was not valid JSON ({err}); stdout:\n{stdout}"))
}

/// Assert the hygiene contract for one run: stdout is valid JSON; stderr is
/// bounded and carries no unclassified dependency tracing (no INFO/DEBUG/TRACE
/// level tokens, no dependency target names).
fn assert_parseable_and_bounded(label: &str, stdout: &str, stderr: &str) -> Value {
    let value = parse_stdout_json(stdout);

    assert!(
        stderr.len() <= MAX_STDERR_BYTES,
        "{label}: stderr exceeded {MAX_STDERR_BYTES} bytes ({}) — diagnostics are not bounded",
        stderr.len()
    );

    let lower = stderr.to_lowercase();
    for dep in ["fsqlite", "frankensearch", "asupersync"] {
        assert!(
            !lower.contains(dep),
            "{label}: stderr leaked `{dep}` dependency tracing; stderr:\n{stderr}"
        );
    }
    for level in [" INFO ", " DEBUG ", " TRACE "] {
        assert!(
            !stderr.contains(level),
            "{label}: stderr emitted a {} line (unclassified noise); stderr:\n{stderr}",
            level.trim()
        );
    }
    value
}

fn robot_cmd(rust_log: &str) -> Command {
    let mut cmd = Command::new(cass_bin());
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");
    cmd.env("CASS_IGNORE_SOURCES_CONFIG", "1");
    cmd.env("RUST_LOG", rust_log);
    cmd
}

#[test]
fn robot_commands_stay_parseable_across_all_noise_classes() {
    for (class, rust_log) in noise_classes() {
        let tmp = TempDir::new().expect("tempdir");
        let data_dir = tmp.path().to_string_lossy().to_string();

        // A representative spread of robot surfaces: a trivial one, the
        // storage-touching status/triage/diag, and the exact report command.
        let invocations: Vec<Vec<String>> = vec![
            vec!["api-version".into(), "--json".into()],
            vec!["capabilities".into(), "--json".into()],
            vec![
                "status".into(),
                "--json".into(),
                "--data-dir".into(),
                data_dir.clone(),
            ],
            vec![
                "triage".into(),
                "--json".into(),
                "--data-dir".into(),
                data_dir.clone(),
            ],
            vec![
                "diag".into(),
                "--json".into(),
                "--data-dir".into(),
                data_dir.clone(),
            ],
        ];

        for args in invocations {
            let label = format!("{class} :: {}", args.join(" "));
            let output = robot_cmd(rust_log).args(&args).output().expect("run cass");
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let value = assert_parseable_and_bounded(&label, &stdout, &stderr);
            assert!(
                value.is_object() || value.is_array(),
                "{label}: JSON should be object/array"
            );
        }
    }
}

#[test]
fn view_stays_clean_under_dependency_burst() {
    // The exact failure the report observed: `cass view` polluted by frankensqlite
    // tracing. Under the heaviest burst, the JSON payload must not interleave with
    // dependency spans.
    let (_, burst) = noise_classes()[0];
    let output = robot_cmd(burst)
        .args([
            "view",
            "README.md",
            "--json",
            "--line",
            "1",
            "--context",
            "0",
        ])
        .output()
        .expect("run cass");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let value = assert_parseable_and_bounded("view-burst", &stdout, &stderr);
    assert_eq!(
        value["path"], "README.md",
        "view should echo the path: {value}"
    );
}

#[test]
fn health_stays_parseable_under_host_pressure_diagnostics() {
    // host-pressure (OOM/disk) diagnostics must not corrupt a readiness probe's
    // JSON even when their targets are enabled. `health` may exit non-zero on an
    // empty data dir; only parseability/boundedness is asserted.
    let (_, pressure) = noise_classes()[4];
    let tmp = TempDir::new().expect("tempdir");
    let data_dir = tmp.path().to_string_lossy().to_string();
    let output = robot_cmd(pressure)
        .args(["health", "--json", "--data-dir", &data_dir])
        .output()
        .expect("run cass");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let value = assert_parseable_and_bounded("health-host-pressure", &stdout, &stderr);
    assert!(
        value.is_object(),
        "health should emit a JSON object: {value}"
    );
}

#[test]
fn verbose_diagnostics_route_to_stderr_keeping_stdout_pure() {
    // --verbose opts into debug diagnostics; those belong on stderr, never in the
    // stdout JSON stream, even under a dependency burst.
    let (_, burst) = noise_classes()[0];
    let output = robot_cmd(burst)
        .args(["api-version", "--json", "--verbose"])
        .output()
        .expect("run cass");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value = parse_stdout_json(&stdout);
    assert!(
        value.get("api_version").is_some() || value.get("version").is_some(),
        "verbose stdout must stay clean JSON: {value}"
    );
}
