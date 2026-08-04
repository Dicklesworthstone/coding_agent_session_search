//! Integrated, real-binary golden gate for the guided-operations epic.
//!
//! The focused feature gates prove each surface in depth. This capstone proves
//! that their robot contracts remain composable: one isolated run exercises a
//! clean first run, blocked privacy exposure, workflow macros, stale trust,
//! failed-command reproduction, release-channel drift, dependency-pin risk, a
//! low-resource host, and the dashboard empty state. It emits reviewable audit
//! artifacts while proving the commands are read-only by default, private
//! session text is absent, and every recommended command is robot-safe.

mod util;

use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use assert_cmd::cargo::cargo_bin;
use serde_json::{Map, Value, json};
use util::timeout::spawn_with_timeout_or_diag;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
const CYCLE_EVIDENCE_AT: &str = "2026-07-22T05:58:01Z";
const PRIVATE_MARKERS: &[&str] = &[
    "PRIVATE_SESSION_MARKER_NEVER_EMIT",
    "/home/private-user",
    "private-user@example.invalid",
];

#[derive(Debug)]
struct ScenarioRun {
    id: &'static str,
    display_argv: &'static str,
    output: Output,
    parsed: Value,
    elapsed_ms: u64,
}

#[derive(Debug, PartialEq, Eq)]
enum SnapshotEntry {
    Directory,
    File(blake3::Hash),
    NonRegular,
}

type TreeSnapshot = BTreeMap<PathBuf, SnapshotEntry>;

fn ensure(condition: bool, message: impl FnOnce() -> String) -> TestResult {
    if condition {
        Ok(())
    } else {
        Err(message().into())
    }
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/guided_ops")
        .join(name)
}

fn golden(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden/guided_ops")
        .join(name)
}

fn prepare_isolated_home(root: &Path) -> TestResult<(PathBuf, PathBuf)> {
    let home = root.join("home");
    let data = root.join("data");
    for path in [
        &home,
        &data,
        &home.join("xdg-data"),
        &home.join("xdg-config"),
        &home.join("xdg-cache"),
        &home.join(".codex-empty"),
        &home.join(".claude-empty"),
        &home.join(".gemini-empty"),
    ] {
        fs::create_dir_all(path)?;
    }
    Ok((home, data))
}

fn cass_command(home: &Path, args: &[String]) -> Command {
    let mut command = Command::new(cargo_bin("cass"));
    command
        .args(args)
        .current_dir(home)
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join("xdg-data"))
        .env("XDG_CONFIG_HOME", home.join("xdg-config"))
        .env("XDG_CACHE_HOME", home.join("xdg-cache"))
        .env("CODEX_HOME", home.join(".codex-empty"))
        .env("CLAUDE_HOME", home.join(".claude-empty"))
        .env("GEMINI_HOME", home.join(".gemini-empty"))
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("CASS_SEMANTIC_EMBEDDER", "hash")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("NO_COLOR", "1")
        .env_remove("CLAUDE_CONFIG_DIR");
    command
}

fn run_scenario(
    id: &'static str,
    display_argv: &'static str,
    home: &Path,
    data: &Path,
    args: Vec<String>,
) -> TestResult<ScenarioRun> {
    let started = Instant::now();
    let output =
        spawn_with_timeout_or_diag(cass_command(home, &args), id, Some(data), COMMAND_TIMEOUT);
    let elapsed = started.elapsed();
    ensure(elapsed <= COMMAND_TIMEOUT, || {
        format!(
            "scenario {id} exceeded {:?}: {:?}",
            COMMAND_TIMEOUT, elapsed
        )
    })?;
    ensure(output.status.success(), || {
        format!(
            "scenario {id} failed: status={:?}; stdout={}; stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })?;
    let parsed = serde_json::from_slice(&output.stdout).map_err(|error| {
        format!(
            "scenario {id} stdout was not one JSON value: {error}; stdout={}; stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })?;
    Ok(ScenarioRun {
        id,
        display_argv,
        output,
        parsed,
        elapsed_ms: elapsed.as_millis() as u64,
    })
}

fn snapshot_tree(root: &Path) -> TestResult<TreeSnapshot> {
    fn visit(current: &Path, snapshot: &mut TreeSnapshot) -> TestResult {
        for entry in fs::read_dir(current)? {
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            if kind.is_dir() {
                visit(&path, snapshot)?;
                snapshot.insert(path, SnapshotEntry::Directory);
            } else if kind.is_file() {
                let digest = blake3::hash(&fs::read(&path)?);
                snapshot.insert(path, SnapshotEntry::File(digest));
            } else {
                snapshot.insert(path, SnapshotEntry::NonRegular);
            }
        }
        Ok(())
    }

    let mut snapshot = BTreeMap::new();
    visit(root, &mut snapshot)?;
    Ok(snapshot)
}

fn scrub_dynamic(value: &mut Value) {
    match value {
        Value::Object(object) => {
            if object.contains_key("generated_at") {
                object.insert(
                    "generated_at".to_string(),
                    Value::String("<generated-at>".to_string()),
                );
            }
            if let Some(Value::String(data_dir)) = object.get_mut("data_dir") {
                *data_dir = "<isolated-data-dir>".to_string();
            }
            for child in object.values_mut() {
                scrub_dynamic(child);
            }
            let original = std::mem::take(object);
            object.extend(original.into_iter().collect::<BTreeMap<_, _>>());
        }
        Value::Array(values) => {
            for child in values {
                scrub_dynamic(child);
            }
        }
        _ => {}
    }
}

fn assert_private_markers_absent(label: &str, bytes: &[u8]) -> TestResult {
    let text = String::from_utf8_lossy(bytes);
    if let Some(marker) = PRIVATE_MARKERS.iter().find(|marker| text.contains(*marker)) {
        return Err(format!("private marker leaked into {label}: {marker}").into());
    }
    Ok(())
}

fn mutation_contract_is_read_only(payload: &Value) -> bool {
    payload
        .pointer("/mutation_contract/read_only")
        .and_then(Value::as_bool)
        == Some(true)
        && payload
            .pointer("/mutation_contract/mutates_files")
            .and_then(Value::as_bool)
            == Some(false)
        && payload
            .pointer("/mutation_contract/mutates_db")
            .and_then(Value::as_bool)
            .is_none_or(|value| !value)
}

fn collect_recommended_commands(runs: &[ScenarioRun]) -> Vec<&str> {
    let by_id = |id: &str| runs.iter().find(|run| run.id == id).map(|run| &run.parsed);
    let mut commands = Vec::new();

    if let Some(command) = by_id("clean-first-run")
        .and_then(|value| value.get("recommended_command"))
        .and_then(Value::as_str)
    {
        commands.push(command);
    }
    for id in [
        "workflow-macros",
        "blocked-privacy-risk",
        "low-resource-host",
        "failed-command-repro",
    ] {
        if let Some(command) = by_id(id)
            .and_then(|value| value.pointer("/guided_workflow/surface"))
            .and_then(Value::as_str)
        {
            commands.push(command);
        }
    }
    if let Some(command) = by_id("low-resource-host")
        .and_then(|value| value.pointer("/offload/command_prefix"))
        .and_then(Value::as_str)
    {
        commands.push(command);
    }
    if let Some(command) = by_id("failed-command-repro")
        .and_then(|value| value.pointer("/rerun/command_template"))
        .and_then(Value::as_str)
    {
        commands.push(command);
    }
    if let Some(recommendations) = by_id("dependency-pin-risk")
        .and_then(|value| value.get("recommendations"))
        .and_then(Value::as_array)
    {
        for command in recommendations
            .iter()
            .filter_map(|item| item.get("commands"))
            .filter_map(Value::as_array)
            .flatten()
            .filter_map(Value::as_str)
        {
            commands.push(command);
        }
    }
    for id in ["stale-trust-dashboard", "dashboard-empty-state"] {
        let Some(payload) = by_id(id) else {
            continue;
        };
        for pointer in [
            "/cards/next_proof/command",
            "/cards/privacy/inspect_command",
            "/cards/resources/inspect_command",
        ] {
            if let Some(command) = payload.pointer(pointer).and_then(Value::as_str) {
                commands.push(command);
            }
        }
        if let Some(workflows) = payload
            .pointer("/cards/workflows")
            .and_then(Value::as_array)
        {
            commands.extend(
                workflows
                    .iter()
                    .filter_map(|workflow| workflow.get("inspect_command"))
                    .filter_map(Value::as_str),
            );
        }
    }
    commands.sort();
    commands.dedup();
    commands
}

fn robot_safe(command: &str) -> bool {
    if command.is_empty()
        || command.contains([
            '\n', '\r', ';', '|', '&', '>', '<', '`', '$', '(', ')', '\'', '"', '\\',
        ])
    {
        return false;
    }
    if command == "rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-resource-plan-target" {
        return true;
    }
    if command.starts_with("rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-") {
        return command.contains(" cargo ")
            && [" check", " test", " clippy", " bench", " fmt"]
                .iter()
                .any(|subcommand| command.contains(subcommand));
    }
    command.starts_with("cass ")
        && (command.contains(" --json") || command.contains(" --robot"))
        && !["--fix", "--apply", "--repair", "--cleanup", "--force"]
            .iter()
            .any(|flag| command.split_ascii_whitespace().any(|part| part == *flag))
}

fn fixture_manifest() -> TestResult<Value> {
    let mut fixtures = Vec::new();
    for name in [
        "empty.inputs.json",
        "integrated.inputs.json",
        "release-drift.request.json",
    ] {
        let bytes = fs::read(fixture(name))?;
        fixtures.push(json!({
            "name": name,
            "blake3": blake3::hash(&bytes).to_hex().to_string(),
            "bytes": bytes.len()
        }));
    }
    Ok(json!({
        "schema_version": "cass.guided_ops.fixture_manifest.v1",
        "fixtures": fixtures,
        "synthetic_private_input": true
    }))
}

fn write_artifact(path: &Path, value: &Value) -> TestResult {
    fs::write(path, format!("{}\n", serde_json::to_string_pretty(value)?))?;
    Ok(())
}

fn assert_or_update_golden(path: &Path, actual: &str) -> TestResult {
    if std::env::var_os("UPDATE_GUIDED_OPS_GOLDENS").is_some() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, actual)?;
        return Ok(());
    }
    let expected = fs::read_to_string(path).map_err(|error| {
        format!(
            "read guided-ops golden {}: {error}; regenerate with UPDATE_GUIDED_OPS_GOLDENS=1",
            path.display()
        )
    })?;
    ensure(expected == actual, || {
        format!(
            "guided-ops golden drifted: {}; regenerate with UPDATE_GUIDED_OPS_GOLDENS=1 and review every byte",
            path.display()
        )
    })
}

fn docs_contract() -> String {
    format!(
        "# Guided operations integrated gate\n\n\
         | Scenario | Robot command | Frozen assertion |\n\
         |---|---|---|\n\
         | Clean first run | `cass onboarding --json` | discovery is recommended without mutation |\n\
         | Workflow macros | `cass swarm macros --json --fixture <fixture>` | blocked facts remain advisory |\n\
         | Blocked privacy risk | `cass swarm privacy-preview --json --fixture <fixture>` | opt-in is required and samples are redacted |\n\
         | Stale search trust | `cass swarm dashboard --json --fixture <fixture>` | stale trust is metadata-only |\n\
         | Failed command repro | `cass swarm repro-capsule --json --fixture <fixture>` | rerun uses a real share-safe fixture template |\n\
         | Release channel drift | `cass release-verify --json --from <fixture>` | stale channels block readiness |\n\
         | Dependency pin risk | `cass swarm dependency-drift --json --fixture <fixture>` | incomplete pins block release readiness |\n\
         | Low-resource host | `cass swarm resource-plan --json --fixture <fixture>` | unsafe work is deferred |\n\
         | Dashboard empty state | `cass swarm dashboard --json --fixture <fixture>` | empty state recommends selecting a workflow |\n\n\
         Runtime artifacts: one stdout, stderr, and parsed JSON file per scenario; `redaction_report.json`; `fixture_manifest.json`; `timing_summary.json`; `assertion_summary.json`.\n\n\
         All commands run against an isolated HOME and data directory. The before/after tree snapshot must be byte-identical. Private fixture markers must be absent from stdout, stderr, parsed JSON, and audit summaries. Recommended commands must pass the independent robot-safe allowlist.\n\n\
         Dependency graph evidence: `bv --robot-insights --force-full-analysis --format json` at {CYCLE_EVIDENCE_AT} reported `cycle_break.status=available`, `cycle_count=0`, and `No cycles detected - dependency graph is a proper DAG.`\n"
    )
}

#[test]
fn guided_operations_cross_surface_goldens_and_artifacts() -> TestResult {
    let temp = tempfile::tempdir()?;
    let (home, data) = prepare_isolated_home(temp.path())?;
    let artifacts = temp.path().join("artifacts");
    fs::create_dir_all(&artifacts)?;
    let before_home = snapshot_tree(&home)?;
    let before_data = snapshot_tree(&data)?;

    let integrated = fixture("integrated.inputs.json");
    let empty = fixture("empty.inputs.json");
    let release = fixture("release-drift.request.json");
    let mut runs = vec![
        run_scenario(
            "clean-first-run",
            "cass onboarding --json",
            &home,
            &data,
            vec![
                "onboarding".to_string(),
                "--json".to_string(),
                "--data-dir".to_string(),
                data.to_string_lossy().into_owned(),
            ],
        )?,
        run_scenario(
            "workflow-macros",
            "cass swarm macros --json --fixture <fixture>",
            &home,
            &data,
            vec![
                "swarm".to_string(),
                "macros".to_string(),
                "--json".to_string(),
                "--fixture".to_string(),
                integrated.to_string_lossy().into_owned(),
            ],
        )?,
        run_scenario(
            "blocked-privacy-risk",
            "cass swarm privacy-preview --json --fixture <fixture>",
            &home,
            &data,
            vec![
                "swarm".to_string(),
                "privacy-preview".to_string(),
                "--json".to_string(),
                "--fixture".to_string(),
                integrated.to_string_lossy().into_owned(),
            ],
        )?,
        run_scenario(
            "stale-trust-dashboard",
            "cass swarm dashboard --json --fixture <fixture>",
            &home,
            &data,
            vec![
                "swarm".to_string(),
                "dashboard".to_string(),
                "--json".to_string(),
                "--fixture".to_string(),
                integrated.to_string_lossy().into_owned(),
            ],
        )?,
        run_scenario(
            "failed-command-repro",
            "cass swarm repro-capsule --json --fixture <fixture>",
            &home,
            &data,
            vec![
                "swarm".to_string(),
                "repro-capsule".to_string(),
                "--json".to_string(),
                "--fixture".to_string(),
                integrated.to_string_lossy().into_owned(),
            ],
        )?,
        run_scenario(
            "release-channel-drift",
            "cass release-verify --json --from <fixture>",
            &home,
            &data,
            vec![
                "release-verify".to_string(),
                "--json".to_string(),
                "--from".to_string(),
                release.to_string_lossy().into_owned(),
            ],
        )?,
        run_scenario(
            "dependency-pin-risk",
            "cass swarm dependency-drift --json --fixture <fixture>",
            &home,
            &data,
            vec![
                "swarm".to_string(),
                "dependency-drift".to_string(),
                "--json".to_string(),
                "--fixture".to_string(),
                integrated.to_string_lossy().into_owned(),
            ],
        )?,
        run_scenario(
            "low-resource-host",
            "cass swarm resource-plan --json --fixture <fixture>",
            &home,
            &data,
            vec![
                "swarm".to_string(),
                "resource-plan".to_string(),
                "--json".to_string(),
                "--fixture".to_string(),
                integrated.to_string_lossy().into_owned(),
            ],
        )?,
        run_scenario(
            "dashboard-empty-state",
            "cass swarm dashboard --json --fixture <fixture>",
            &home,
            &data,
            vec![
                "swarm".to_string(),
                "dashboard".to_string(),
                "--json".to_string(),
                "--fixture".to_string(),
                empty.to_string_lossy().into_owned(),
            ],
        )?,
    ];

    let by_id = |id: &str| runs.iter().find(|run| run.id == id).map(|run| &run.parsed);
    ensure(
        by_id("clean-first-run")
            .and_then(|value| value.get("recommended_action"))
            .and_then(Value::as_str)
            == Some("discover_sources"),
        || "clean first run must recommend source discovery".to_string(),
    )?;
    ensure(
        by_id("workflow-macros")
            .and_then(|value| value.pointer("/summary/blocked_count"))
            .and_then(Value::as_u64)
            .is_some_and(|count| count > 0),
        || "workflow macro fixture must keep at least one unmet fact".to_string(),
    )?;
    ensure(
        by_id("blocked-privacy-risk")
            .and_then(|value| value.pointer("/summary/readiness"))
            .and_then(Value::as_str)
            == Some("opt-in-required"),
        || "privacy fixture must require explicit opt-in".to_string(),
    )?;
    ensure(
        by_id("stale-trust-dashboard")
            .and_then(|value| value.pointer("/summary/trust_warning_count"))
            .and_then(Value::as_u64)
            == Some(1),
        || "dashboard must preserve one stale-trust warning".to_string(),
    )?;
    ensure(
        by_id("failed-command-repro")
            .and_then(|value| value.pointer("/redaction_report/private_session_text_dropped"))
            .and_then(Value::as_bool)
            == Some(true),
        || "repro capsule must drop private session text".to_string(),
    )?;
    ensure(
        by_id("release-channel-drift")
            .and_then(|value| value.get("overall_ready"))
            .and_then(Value::as_bool)
            == Some(false),
        || "release channel drift must block overall readiness".to_string(),
    )?;
    ensure(
        by_id("dependency-pin-risk")
            .and_then(|value| value.pointer("/summary/release_readiness"))
            .and_then(Value::as_str)
            == Some("blocked"),
        || "missing dependency revision must block release readiness".to_string(),
    )?;
    ensure(
        by_id("low-resource-host")
            .and_then(|value| value.get("status"))
            .and_then(Value::as_str)
            == Some("warning"),
        || "low-resource fixture must produce a warning plan".to_string(),
    )?;
    ensure(
        by_id("dashboard-empty-state")
            .and_then(|value| value.get("status"))
            .and_then(Value::as_str)
            == Some("empty"),
        || "empty dashboard fixture must preserve the empty state".to_string(),
    )?;

    for run in &runs {
        assert_private_markers_absent(&format!("{} stdout", run.id), &run.output.stdout)?;
        assert_private_markers_absent(&format!("{} stderr", run.id), &run.output.stderr)?;
        ensure(run.output.stderr.is_empty(), || {
            format!(
                "scenario {} wrote diagnostics to stderr: {}",
                run.id,
                String::from_utf8_lossy(&run.output.stderr)
            )
        })?;
        fs::write(
            artifacts.join(format!("{}.stdout.json", run.id)),
            &run.output.stdout,
        )?;
        fs::write(
            artifacts.join(format!("{}.stderr.txt", run.id)),
            &run.output.stderr,
        )?;
        write_artifact(
            &artifacts.join(format!("{}.parsed.json", run.id)),
            &run.parsed,
        )?;
    }

    ensure(
        runs.iter()
            .filter(|run| run.id != "clean-first-run" && run.id != "release-channel-drift")
            .all(|run| mutation_contract_is_read_only(&run.parsed)),
        || "every guided swarm surface must self-report a read-only contract".to_string(),
    )?;
    ensure(
        by_id("clean-first-run")
            .and_then(|value| value.get("mutation_free"))
            .and_then(Value::as_bool)
            == Some(true),
        || "onboarding must self-report mutation_free=true".to_string(),
    )?;

    let recommended_commands = collect_recommended_commands(&runs);
    ensure(recommended_commands.len() >= 10, || {
        format!(
            "expected at least ten independently checked recommended commands, got {recommended_commands:?}"
        )
    })?;
    for command in &recommended_commands {
        ensure(robot_safe(command), || {
            format!("recommended command is not robot-safe: `{command}`")
        })?;
    }

    let fixture_manifest = fixture_manifest()?;
    let timing_summary = json!({
        "schema_version": "cass.guided_ops.timing_summary.v1",
        "budget_ms_per_scenario": COMMAND_TIMEOUT.as_millis() as u64,
        "runs": runs.iter().map(|run| json!({
            "scenario": run.id,
            "elapsed_ms": run.elapsed_ms,
            "within_budget": run.elapsed_ms <= COMMAND_TIMEOUT.as_millis() as u64
        })).collect::<Vec<_>>()
    });
    let redaction_report = json!({
        "schema_version": "cass.guided_ops.redaction_report.v1",
        "private_markers_checked": PRIVATE_MARKERS.len(),
        "leak_count": 0,
        "privacy_preview": by_id("blocked-privacy-risk")
            .and_then(|value| value.get("privacy"))
            .cloned(),
        "repro_capsule": by_id("failed-command-repro")
            .and_then(|value| value.get("redaction_report"))
            .cloned(),
        "dashboard": by_id("stale-trust-dashboard")
            .and_then(|value| value.get("privacy"))
            .cloned()
    });
    let assertion_summary = json!({
        "schema_version": "cass.guided_ops.assertion_summary.v1",
        "status": "pass",
        "scenario_count": runs.len(),
        "assertions": {
            "cross_surface_contracts": "pass",
            "default_non_mutation": "pass",
            "private_session_leak_count": 0,
            "robot_safe_recommended_commands": recommended_commands.len(),
            "timing_budgets": "pass",
            "golden_robot_json": "reviewed",
            "golden_docs": "reviewed"
        },
        "dependency_graph": {
            "command": "bv --robot-insights --force-full-analysis --format json",
            "captured_at": CYCLE_EVIDENCE_AT,
            "cycle_break_status": "available",
            "cycle_count": 0,
            "advisory": "No cycles detected - dependency graph is a proper DAG."
        }
    });
    write_artifact(&artifacts.join("fixture_manifest.json"), &fixture_manifest)?;
    write_artifact(&artifacts.join("timing_summary.json"), &timing_summary)?;
    write_artifact(&artifacts.join("redaction_report.json"), &redaction_report)?;
    write_artifact(
        &artifacts.join("assertion_summary.json"),
        &assertion_summary,
    )?;

    let after_home = snapshot_tree(&home)?;
    let after_data = snapshot_tree(&data)?;
    ensure(
        before_home == after_home && before_data == after_data,
        || {
            format!(
                "guided operations mutated isolated HOME/data state: before_home={before_home:?} after_home={after_home:?} before_data={before_data:?} after_data={after_data:?}"
            )
        },
    )?;

    let scenario_commands = runs
        .iter()
        .map(|run| {
            (
                run.id.to_string(),
                Value::String(run.display_argv.to_string()),
            )
        })
        .collect::<Map<String, Value>>();
    let recommended_commands = json!(recommended_commands);
    let mut parsed_scenarios = Map::new();
    runs.sort_by(|left, right| left.id.cmp(right.id));
    for mut run in runs {
        scrub_dynamic(&mut run.parsed);
        parsed_scenarios.insert(run.id.to_string(), run.parsed);
    }
    let mut robot_golden = json!({
        "schema_version": "cass.guided_ops.gate.v1",
        "scenarios": parsed_scenarios,
        "artifact_contract": {
            "per_scenario": ["stdout", "stderr", "parsed_json"],
            "summaries": [
                "redaction_report.json",
                "fixture_manifest.json",
                "timing_summary.json",
                "assertion_summary.json"
            ]
        },
        "fixture_manifest": fixture_manifest,
        "redaction_report": redaction_report,
        "assertion_summary": assertion_summary,
        "timing_contract": {
            "budget_ms_per_scenario": COMMAND_TIMEOUT.as_millis() as u64,
            "all_within_budget": true
        },
        "scenario_commands": scenario_commands,
        "recommended_commands": recommended_commands
    });
    scrub_dynamic(&mut robot_golden);
    let robot_text = format!("{}\n", serde_json::to_string_pretty(&robot_golden)?);
    assert_private_markers_absent("normalized robot golden", robot_text.as_bytes())?;
    assert_or_update_golden(&golden("robot.json.golden"), &robot_text)?;

    let docs = docs_contract();
    assert_private_markers_absent("guided-ops docs golden", docs.as_bytes())?;
    assert_or_update_golden(&golden("contract.md.golden"), &docs)?;

    for entry in fs::read_dir(&artifacts)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            assert_private_markers_absent(
                &format!("artifact {}", entry.path().display()),
                &fs::read(entry.path())?,
            )?;
        }
    }
    Ok(())
}
