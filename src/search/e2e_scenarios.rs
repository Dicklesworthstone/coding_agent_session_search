//! Report-derived E2E scenario scripts for fleet and archive states (bead
//! cass-fleet-resilience-20260608-uojcg.12.5).
//!
//! Deterministic, executable scenario definitions that simulate the 2026-06-08
//! report's named fleet states. Each scenario states its fixture setup, the
//! `cass` command sequence, the expected JSON assertions, the expected
//! structured-log artifacts, a privacy note, and the owning implementation
//! bead a failure points to. Live-host execution is opt-in
//! (`requires_live_host`) and never required for default CI — the default is
//! to replay against the deterministic fixtures.
//!
//! The integration gate consumes this machine-readable contract directly;
//! this is not a serialize-only catalog. It composes the landed contracts:
//! the readiness fixtures (`.1.5`), liveness fixtures (`.4.5`),
//! workspace/source fixtures (`.7.4`), quarantine compat fixtures (`.3.4`),
//! the recovery journeys (`.13.1`), and the proof contracts (`.11.4`/`.12.3`).
//! The `.12.2` runner executes them; this module is the deterministic spec.
//! Commands are tokenized `cass` arguments (never shell text or destructive).

use serde::Serialize;

/// One deterministic environment override. Values may contain fixture path
/// placeholders such as `{data_dir}` or `{trace_path}`; the integration runner
/// resolves them without invoking a shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ScenarioEnv {
    pub key: &'static str,
    pub value: &'static str,
}

/// Machine-evaluable JSON assertion for a robot command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "predicate", rename_all = "snake_case")]
pub enum JsonExpectation {
    Exists {
        pointer: &'static str,
    },
    EqualsString {
        pointer: &'static str,
        expected: &'static str,
    },
    EqualsBool {
        pointer: &'static str,
        expected: bool,
    },
    EqualsU64 {
        pointer: &'static str,
        expected: u64,
    },
    ContainsString {
        pointer: &'static str,
        expected_fragment: &'static str,
    },
    NonEmptyArray {
        pointer: &'static str,
    },
    OneOfStrings {
        pointer: &'static str,
        expected: &'static [&'static str],
    },
}

impl JsonExpectation {
    /// Stable assertion label used in proof logs.
    pub fn label(self) -> String {
        match self {
            Self::Exists { pointer } => format!("{pointer}:exists"),
            Self::EqualsString { pointer, expected } => {
                format!("{pointer}:equals:{expected}")
            }
            Self::EqualsBool { pointer, expected } => {
                format!("{pointer}:equals:{expected}")
            }
            Self::EqualsU64 { pointer, expected } => {
                format!("{pointer}:equals:{expected}")
            }
            Self::ContainsString {
                pointer,
                expected_fragment,
            } => format!("{pointer}:contains:{expected_fragment}"),
            Self::NonEmptyArray { pointer } => format!("{pointer}:non_empty_array"),
            Self::OneOfStrings { pointer, expected } => {
                format!("{pointer}:one_of:{}", expected.join("|"))
            }
        }
    }

    /// Evaluate against one parsed robot JSON document.
    #[must_use]
    pub fn evaluate(self, value: &serde_json::Value) -> bool {
        match self {
            Self::Exists { pointer } => value.pointer(pointer).is_some(),
            Self::EqualsString { pointer, expected } => {
                value.pointer(pointer).and_then(serde_json::Value::as_str) == Some(expected)
            }
            Self::EqualsBool { pointer, expected } => {
                value.pointer(pointer).and_then(serde_json::Value::as_bool) == Some(expected)
            }
            Self::EqualsU64 { pointer, expected } => {
                value.pointer(pointer).and_then(serde_json::Value::as_u64) == Some(expected)
            }
            Self::ContainsString {
                pointer,
                expected_fragment,
            } => value
                .pointer(pointer)
                .and_then(serde_json::Value::as_str)
                .is_some_and(|actual| actual.contains(expected_fragment)),
            Self::NonEmptyArray { pointer } => value
                .pointer(pointer)
                .and_then(serde_json::Value::as_array)
                .is_some_and(|items| !items.is_empty()),
            Self::OneOfStrings { pointer, expected } => value
                .pointer(pointer)
                .and_then(serde_json::Value::as_str)
                .is_some_and(|actual| expected.contains(&actual)),
        }
    }
}

/// One real-binary command in a scenario.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScenarioCommand {
    pub id: &'static str,
    /// Tokenized argv after the resolved cass binary.
    pub args: &'static [&'static str],
    pub env: &'static [ScenarioEnv],
    pub accepted_exit_codes: &'static [i32],
    pub json_assertions: &'static [JsonExpectation],
    /// Markers forbidden from captured stdout/stderr.
    pub forbidden_stream_markers: &'static [&'static str],
    /// Extra scenario-owned artifacts beyond the runner's standard four.
    pub expected_extra_artifacts: &'static [&'static str],
    pub timeout_ms: u64,
}

/// One report-derived E2E scenario definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct E2eScenario {
    pub id: &'static str,
    pub description: &'static str,
    /// The implementation bead a failure of this scenario points to.
    pub owning_bead: &'static str,
    /// How the deterministic fixture state is established.
    pub fixture_setup: &'static str,
    /// The tokenized real-binary command sequence to run, in order.
    pub commands: &'static [ScenarioCommand],
    /// Expected retained stream, event, proof, and scenario artifacts.
    pub expected_log_artifacts: &'static [&'static str],
    /// Privacy note: what is/ isn't surfaced and how it is redacted.
    pub privacy_note: &'static str,
    /// Opt-in live-host replay. False = deterministic fixture mode (CI).
    pub requires_live_host: bool,
}

static SCENARIOS: &[E2eScenario] = &[
    E2eScenario {
        id: "local_healthy_lexical_semantic_unavailable",
        description: "healthy lexical index with semantic unavailable; search succeeds with truthful lexical fallback metadata",
        owning_bead: "coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.5.1",
        fixture_setup: "synthetic Codex JSONL -> real cass index --full; no semantic model/vector artifacts",
        commands: &[ScenarioCommand {
            id: "hybrid-search-lexical-fallback",
            args: &[
                "search",
                "resilienceprobe",
                "--json",
                "--robot-meta",
                "--mode",
                "hybrid",
                "--limit",
                "5",
                "--data-dir",
                "{data_dir}",
            ],
            env: &[],
            accepted_exit_codes: &[0],
            json_assertions: &[
                JsonExpectation::NonEmptyArray { pointer: "/hits" },
                JsonExpectation::EqualsString {
                    pointer: "/_meta/requested_search_mode",
                    expected: "hybrid",
                },
                JsonExpectation::EqualsString {
                    pointer: "/_meta/search_mode",
                    expected: "lexical",
                },
                JsonExpectation::EqualsString {
                    pointer: "/_meta/fallback_tier",
                    expected: "lexical",
                },
                JsonExpectation::EqualsBool {
                    pointer: "/_meta/semantic_refinement",
                    expected: false,
                },
                JsonExpectation::Exists {
                    pointer: "/_meta/fallback_reason",
                },
            ],
            forbidden_stream_markers: &[],
            expected_extra_artifacts: &[],
            timeout_ms: 30_000,
        }],
        expected_log_artifacts: &[
            "stdout.json",
            "stderr.log",
            "event.json",
            "hybrid-search-lexical-fallback.proof.json",
        ],
        privacy_note: "result snippets follow existing redaction; no raw model/vector paths surfaced",
        requires_live_host: false,
    },
    E2eScenario {
        id: "local_stale_quarantine",
        description: "stale derived assets plus ingest-OOM quarantine; health/status/doctor explain rebuild+quarantine without data-loss advice",
        owning_bead: "coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.3.4",
        fixture_setup: "fresh synthetic archive/index plus a schema-valid ingest quarantine JSONL; --stale-threshold 0 forces stale classification",
        commands: &[
            ScenarioCommand {
                id: "stale-quarantine-status",
                args: &[
                    "status",
                    "--json",
                    "--stale-threshold",
                    "0",
                    "--data-dir",
                    "{data_dir}",
                ],
                env: &[],
                accepted_exit_codes: &[0, 1],
                json_assertions: &[
                    JsonExpectation::EqualsU64 {
                        pointer: "/ingest_quarantine/quarantined_conversations",
                        expected: 1,
                    },
                    JsonExpectation::EqualsBool {
                        pointer: "/search_completeness/complete",
                        expected: false,
                    },
                    JsonExpectation::EqualsBool {
                        pointer: "/search_completeness/can_search",
                        expected: true,
                    },
                    JsonExpectation::ContainsString {
                        pointer: "/search_completeness/next_command",
                        expected_fragment: "quarantine",
                    },
                ],
                forbidden_stream_markers: &["rm -rf", "DROP TABLE"],
                expected_extra_artifacts: &[],
                timeout_ms: 30_000,
            },
            ScenarioCommand {
                id: "stale-quarantine-doctor",
                args: &["doctor", "--json", "--data-dir", "{data_dir}"],
                env: &[],
                accepted_exit_codes: &[0, 1],
                json_assertions: &[
                    JsonExpectation::NonEmptyArray { pointer: "/checks" },
                    JsonExpectation::EqualsBool {
                        pointer: "/auto_fix_applied",
                        expected: false,
                    },
                    JsonExpectation::Exists {
                        pointer: "/doctor_command",
                    },
                ],
                forbidden_stream_markers: &["rm -rf", "DROP TABLE"],
                expected_extra_artifacts: &[],
                timeout_ms: 30_000,
            },
        ],
        expected_log_artifacts: &["stdout.json", "stderr.log", "event.json", "*.proof.json"],
        privacy_note: "quarantine entries reported by count/cause; no raw conversation text",
        requires_live_host: false,
    },
    E2eScenario {
        id: "archive_risk_backup_first",
        description: "high archive-risk host; status/doctor require backup/inspection before any repair",
        owning_bead: "coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.1.3",
        fixture_setup: "synthetic unreadable agent_search.db with no candidate or source authority; fixture bytes fingerprinted before and after",
        commands: &[ScenarioCommand {
            id: "archive-risk-doctor",
            args: &["doctor", "--json", "--data-dir", "{data_dir}"],
            env: &[],
            accepted_exit_codes: &[5],
            json_assertions: &[
                JsonExpectation::EqualsBool {
                    pointer: "/healthy",
                    expected: false,
                },
                JsonExpectation::EqualsString {
                    pointer: "/health_class",
                    expected: "degraded-archive-risk",
                },
                JsonExpectation::EqualsBool {
                    pointer: "/auto_fix_applied",
                    expected: false,
                },
                JsonExpectation::NonEmptyArray { pointer: "/checks" },
                JsonExpectation::Exists {
                    pointer: "/doctor_command",
                },
            ],
            forbidden_stream_markers: &["rm -rf", "DROP TABLE", "delete the database"],
            expected_extra_artifacts: &["{fixture_manifest_path}"],
            timeout_ms: 30_000,
        }],
        expected_log_artifacts: &[
            "fixture-manifest.json",
            "stdout.json",
            "stderr.log",
            "event.json",
            "archive-risk-doctor.proof.json",
        ],
        privacy_note: "reports risk level + data_dir only; no archive contents",
        requires_live_host: false,
    },
    E2eScenario {
        id: "long_rebuild_watch_stall",
        description: "active rebuild exposes forward progress while an injected slow probe returns a bounded partial envelope",
        owning_bead: "coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.4.1",
        fixture_setup: "schema-valid lexical rebuild checkpoint plus held advisory lock and deterministic status-delay failpoint",
        commands: &[
            ScenarioCommand {
                id: "active-rebuild-progress",
                args: &["status", "--json", "--data-dir", "{data_dir}"],
                env: &[ScenarioEnv {
                    key: "CASS_TANTIVY_REBUILD_PIPELINE_CHANNEL_SIZE",
                    value: "5",
                }],
                accepted_exit_codes: &[0, 1],
                json_assertions: &[
                    JsonExpectation::EqualsBool {
                        pointer: "/rebuild_progress/active",
                        expected: true,
                    },
                    JsonExpectation::EqualsU64 {
                        pointer: "/rebuild_progress/processed_conversations",
                        expected: 4,
                    },
                    JsonExpectation::EqualsU64 {
                        pointer: "/rebuild_progress/remaining_conversations",
                        expected: 6,
                    },
                    JsonExpectation::EqualsString {
                        pointer: "/rebuild_progress/controller_mode",
                        expected: "pressure_limited",
                    },
                ],
                forbidden_stream_markers: &[],
                expected_extra_artifacts: &[],
                timeout_ms: 30_000,
            },
            ScenarioCommand {
                id: "bounded-stall-envelope",
                args: &["status", "--json", "--data-dir", "{data_dir}"],
                env: &[
                    ScenarioEnv {
                        key: "CASS_STATUS_BUDGET_MS",
                        value: "150",
                    },
                    ScenarioEnv {
                        key: "CASS_TEST_STATUS_SLOW_MS",
                        value: "2500",
                    },
                ],
                accepted_exit_codes: &[0, 1],
                json_assertions: &[
                    JsonExpectation::EqualsBool {
                        pointer: "/budget/timed_out",
                        expected: true,
                    },
                    JsonExpectation::NonEmptyArray {
                        pointer: "/budget/skipped_sections",
                    },
                    JsonExpectation::Exists {
                        pointer: "/budget/recommended_next_probe",
                    },
                ],
                forbidden_stream_markers: &[],
                expected_extra_artifacts: &[],
                timeout_ms: 3_000,
            },
        ],
        expected_log_artifacts: &["stdout.json", "stderr.log", "event.json", "*.proof.json"],
        privacy_note: "synthetic counts and queue pressure only; no session text",
        requires_live_host: false,
    },
    E2eScenario {
        id: "dependency_noise",
        description: "deliberately noisy dependency tracing cannot corrupt robot stdout/stderr and is routed to an explicit trace artifact",
        owning_bead: "coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.2.5",
        fixture_setup: "isolated empty data dir with noisy RUST_LOG and an explicit trace-file target",
        commands: &[ScenarioCommand {
            id: "status-under-dependency-noise",
            args: &[
                "status",
                "--json",
                "--data-dir",
                "{data_dir}",
                "--trace-file",
                "{trace_path}",
            ],
            env: &[ScenarioEnv {
                key: "RUST_LOG",
                value: "trace,fsqlite=trace,fsqlite_core=trace",
            }],
            accepted_exit_codes: &[0, 1],
            json_assertions: &[
                JsonExpectation::Exists { pointer: "/status" },
                JsonExpectation::Exists {
                    pointer: "/recommended_action",
                },
            ],
            forbidden_stream_markers: &[" INFO ", " DEBUG ", " TRACE ", "fsqlite"],
            expected_extra_artifacts: &["{trace_path}"],
            timeout_ms: 30_000,
        }],
        expected_log_artifacts: &[
            "stdout.json",
            "stderr.log",
            "event.json",
            "status-under-dependency-noise.proof.json",
            "trace.jsonl",
        ],
        privacy_note: "trace contains only synthetic empty-dir diagnostics",
        requires_live_host: false,
    },
    E2eScenario {
        id: "moved_workspace_archive_drilldown",
        description: "a vanished/moved source path remains viewable from the canonical archive with explicit provenance",
        owning_bead: "coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.7.3",
        fixture_setup: "FrankenStorage archive row whose synthetic macOS source path does not exist on the test host",
        commands: &[ScenarioCommand {
            id: "archive-only-view",
            args: &[
                "view",
                "{source_path}",
                "--source",
                "remote-proof",
                "--json",
                "--db",
                "{db_path}",
            ],
            env: &[],
            accepted_exit_codes: &[0],
            json_assertions: &[
                JsonExpectation::EqualsBool {
                    pointer: "/source_exists",
                    expected: false,
                },
                JsonExpectation::EqualsBool {
                    pointer: "/archive_only",
                    expected: true,
                },
                JsonExpectation::NonEmptyArray { pointer: "/lines" },
            ],
            forbidden_stream_markers: &[],
            expected_extra_artifacts: &[],
            timeout_ms: 30_000,
        }],
        expected_log_artifacts: &[
            "stdout.json",
            "stderr.log",
            "event.json",
            "archive-only-view.proof.json",
        ],
        privacy_note: "only synthetic archive content and a synthetic moved path",
        requires_live_host: false,
    },
    E2eScenario {
        id: "remote_unreachable_old_host",
        description: "source doctor preserves host identity across old-binary and bounded-unreachable/timeout states",
        owning_bead: "coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.8.6",
        fixture_setup: "synthetic sources.toml plus a size-bounded external-probe fixture; no live SSH",
        commands: &[
            ScenarioCommand {
                id: "old-host-source-doctor",
                args: &["sources", "doctor", "--json"],
                env: &[ScenarioEnv {
                    key: "CASS_TEST_SOURCES_DOCTOR_PROBE",
                    value: "{probe_path}",
                }],
                accepted_exit_codes: &[1],
                json_assertions: &[
                    JsonExpectation::EqualsString {
                        pointer: "/sources/0/state",
                        expected: "old_cass",
                    },
                    JsonExpectation::EqualsBool {
                        pointer: "/sources/0/host_reached",
                        expected: true,
                    },
                    JsonExpectation::EqualsBool {
                        pointer: "/mutation_free",
                        expected: true,
                    },
                    JsonExpectation::EqualsString {
                        pointer: "/diagnostics/0/host_report/cass_version",
                        expected: "0.0.1",
                    },
                    JsonExpectation::Exists {
                        pointer: "/sources/0/safe_next_command",
                    },
                ],
                forbidden_stream_markers: &["password", "private key"],
                expected_extra_artifacts: &["{probe_path}"],
                timeout_ms: 30_000,
            },
            ScenarioCommand {
                id: "bounded-unreachable-source-doctor",
                args: &["sources", "doctor", "--json"],
                env: &[
                    ScenarioEnv {
                        key: "CASS_TEST_SOURCES_DOCTOR_PROBE",
                        value: "{probe_path}",
                    },
                    ScenarioEnv {
                        key: "CASS_FLEET_BUDGET_MS",
                        value: "1",
                    },
                    ScenarioEnv {
                        key: "CASS_FLEET_PER_HOST_BUDGET_MS",
                        value: "20",
                    },
                ],
                accepted_exit_codes: &[1],
                json_assertions: &[
                    JsonExpectation::EqualsString {
                        pointer: "/sources/0/source_id",
                        expected: "fixture-source",
                    },
                    JsonExpectation::OneOfStrings {
                        pointer: "/sources/0/state",
                        expected: &["timeout", "unreachable"],
                    },
                    JsonExpectation::EqualsBool {
                        pointer: "/budget/timed_out",
                        expected: true,
                    },
                    JsonExpectation::EqualsBool {
                        pointer: "/mutation_free",
                        expected: true,
                    },
                ],
                forbidden_stream_markers: &["password", "private key"],
                expected_extra_artifacts: &["{probe_path}"],
                timeout_ms: 3_000,
            },
        ],
        expected_log_artifacts: &[
            "stdout.json",
            "stderr.log",
            "event.json",
            "*.proof.json",
            "source-doctor-probe.json",
        ],
        privacy_note: "synthetic host alias and version only; no credentials or live paths",
        requires_live_host: false,
    },
    E2eScenario {
        id: "incident_mining_large_corpus",
        description: "bounded incident mining emits truthful partial metadata, ranked categories, redaction summary, and citable artifacts",
        owning_bead: "coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.10.3",
        fixture_setup: "two synthetic FrankenStorage conversations ordered so --max-messages 1 deterministically caps the scan",
        commands: &[ScenarioCommand {
            id: "bounded-incident-scan",
            args: &[
                "analytics",
                "incidents",
                "--json",
                "--limit",
                "10",
                "--max-sessions",
                "10",
                "--max-messages",
                "1",
                "--max-bytes",
                "1048576",
                "--budget-ms",
                "10000",
                "--data-dir",
                "{data_dir}",
            ],
            env: &[],
            accepted_exit_codes: &[0],
            json_assertions: &[
                JsonExpectation::EqualsString {
                    pointer: "/command",
                    expected: "analytics/incidents",
                },
                JsonExpectation::EqualsBool {
                    pointer: "/data/discovery/partial",
                    expected: true,
                },
                JsonExpectation::EqualsString {
                    pointer: "/data/discovery/stop_reason",
                    expected: "lines-capped",
                },
                JsonExpectation::EqualsString {
                    pointer: "/data/redaction/private_text_policy",
                    expected: "suppress_all",
                },
                JsonExpectation::NonEmptyArray {
                    pointer: "/data/top_sessions",
                },
            ],
            forbidden_stream_markers: &["CASS_PRIVATE_MARKER_DO_NOT_EMIT"],
            expected_extra_artifacts: &[],
            timeout_ms: 30_000,
        }],
        expected_log_artifacts: &[
            "stdout.json",
            "stderr.log",
            "event.json",
            "bounded-incident-scan.proof.json",
        ],
        privacy_note: "raw synthetic message text is suppressed; basenames/fingerprints only",
        requires_live_host: false,
    },
    E2eScenario {
        id: "live_remote_replay",
        description: "operator-only replay against explicitly supplied live source configuration",
        owning_bead: "coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.8.6",
        fixture_setup: "operator supplies an isolated XDG_CONFIG_HOME and explicitly opts in; never part of deterministic CI",
        commands: &[ScenarioCommand {
            id: "live-source-doctor",
            args: &["sources", "doctor", "--json"],
            env: &[],
            accepted_exit_codes: &[0, 1],
            json_assertions: &[
                JsonExpectation::EqualsBool {
                    pointer: "/mutation_free",
                    expected: true,
                },
                JsonExpectation::Exists {
                    pointer: "/sources",
                },
            ],
            forbidden_stream_markers: &["password", "private key"],
            expected_extra_artifacts: &[],
            timeout_ms: 120_000,
        }],
        expected_log_artifacts: &["stdout.json", "stderr.log", "event.json", "*.proof.json"],
        privacy_note: "operator must review host/path metadata before retaining live artifacts",
        requires_live_host: true,
    },
];

/// All report-derived E2E scenarios, in a stable order.
pub fn e2e_scenarios() -> &'static [E2eScenario] {
    SCENARIOS
}

/// The scenarios that run in default CI (deterministic; no live host).
pub fn ci_scenarios() -> Vec<&'static E2eScenario> {
    SCENARIOS.iter().filter(|s| !s.requires_live_host).collect()
}

/// Explicitly opt-in live-host scenarios.
pub fn live_scenarios() -> Vec<&'static E2eScenario> {
    SCENARIOS.iter().filter(|s| s.requires_live_host).collect()
}

/// Look up a scenario by id.
pub fn scenario(id: &str) -> Option<&'static E2eScenario> {
    SCENARIOS.iter().find(|s| s.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scenario_test_error(message: std::fmt::Arguments<'_>) -> String {
        message.to_string()
    }

    #[test]
    fn required_report_scenarios_are_present() -> Result<(), String> {
        for id in [
            "local_healthy_lexical_semantic_unavailable",
            "local_stale_quarantine",
            "archive_risk_backup_first",
            "long_rebuild_watch_stall",
            "dependency_noise",
            "moved_workspace_archive_drilldown",
            "remote_unreachable_old_host",
            "incident_mining_large_corpus",
        ] {
            if scenario(id).is_none() {
                return Err(scenario_test_error(format_args!(
                    "missing required scenario {id}"
                )));
            }
        }
        if ci_scenarios().len() != 8 {
            return Err(
                "default deterministic matrix must contain exactly the eight required scenarios"
                    .to_string(),
            );
        }
        let command_count = ci_scenarios()
            .iter()
            .map(|scenario| scenario.commands.len())
            .sum::<usize>();
        if command_count != 11 {
            return Err(format!(
                "the deterministic matrix must execute all eleven report-derived commands; found {command_count}"
            ));
        }
        Ok(())
    }

    #[test]
    fn deterministic_scenario_command_keys_are_unique() -> Result<(), String> {
        let keys = ci_scenarios()
            .iter()
            .flat_map(|scenario| {
                scenario
                    .commands
                    .iter()
                    .map(|command| format!("{}/{}", scenario.id, command.id))
            })
            .collect::<Vec<_>>();
        let unique = keys.iter().collect::<std::collections::BTreeSet<_>>();
        if unique.len() != keys.len() {
            return Err(format!(
                "ambiguous duplicate scenario/command key in {keys:?}"
            ));
        }
        Ok(())
    }

    #[test]
    fn every_scenario_is_fully_specified() -> Result<(), String> {
        for s in e2e_scenarios() {
            if s.description.is_empty() {
                return Err(scenario_test_error(format_args!("{} description", s.id)));
            }
            if s.fixture_setup.is_empty() {
                return Err(scenario_test_error(format_args!("{} fixture_setup", s.id)));
            }
            if s.commands.is_empty() {
                return Err(scenario_test_error(format_args!("{} commands", s.id)));
            }
            for command in s.commands {
                if command.id.is_empty() {
                    return Err(scenario_test_error(format_args!("{} command id", s.id)));
                }
                if command.args.is_empty() {
                    return Err(scenario_test_error(format_args!("{} command args", s.id)));
                }
                if command.json_assertions.is_empty() {
                    return Err(scenario_test_error(format_args!(
                        "{} command {} assertions",
                        s.id, command.id
                    )));
                }
                if command.timeout_ms == 0 {
                    return Err(scenario_test_error(format_args!(
                        "{} command {} timeout",
                        s.id, command.id
                    )));
                }
            }
            if s.expected_log_artifacts.is_empty() {
                return Err(scenario_test_error(format_args!("{} log artifacts", s.id)));
            }
            if s.privacy_note.is_empty() {
                return Err(scenario_test_error(format_args!("{} privacy note", s.id)));
            }
            // Failures must point at an owning implementation bead.
            if !s.owning_bead.contains("uojcg.") {
                return Err(scenario_test_error(format_args!(
                    "{} owning_bead must reference a bead: {}",
                    s.id, s.owning_bead
                )));
            }
        }
        Ok(())
    }

    #[test]
    fn commands_are_concrete_cass_and_never_destructive() {
        for s in e2e_scenarios() {
            for command in s.commands {
                assert_ne!(
                    command.args.first().copied(),
                    Some("cass"),
                    "{}: argv is after the resolved binary, so it must not launch bare cass",
                    s.id
                );
                for bad in ["rm ", "rm -", "--force-clean", "DROP ", "delete "] {
                    assert!(
                        command.args.iter().all(|arg| !arg.contains(bad)),
                        "{} destructive command token in {:?}",
                        s.id,
                        command.args
                    );
                }
            }
        }
    }

    #[test]
    fn default_ci_requires_no_live_host() {
        // The .12.5 acceptance: live-host execution is opt-in, never required
        // for default CI.
        let ci = ci_scenarios();
        assert!(!ci.is_empty(), "there must be deterministic CI scenarios");
        assert!(ci.iter().all(|s| !s.requires_live_host));
        // The named fleet/archive states are all CI-runnable without a host.
        for id in [
            "local_healthy_lexical_semantic_unavailable",
            "local_stale_quarantine",
            "archive_risk_backup_first",
            "long_rebuild_watch_stall",
            "dependency_noise",
            "moved_workspace_archive_drilldown",
            "remote_unreachable_old_host",
            "incident_mining_large_corpus",
        ] {
            assert!(
                !scenario(id).unwrap().requires_live_host,
                "{id} must be CI-runnable"
            );
        }
    }

    #[test]
    fn live_replay_is_explicitly_separate_from_ci() {
        let live: Vec<&str> = live_scenarios().iter().map(|s| s.id).collect();
        assert_eq!(live, vec!["live_remote_replay"]);
        assert!(
            ci_scenarios()
                .iter()
                .all(|candidate| candidate.id != "live_remote_replay")
        );
    }

    #[test]
    fn scenario_serializes_with_expected_fields() {
        let s = scenario("archive_risk_backup_first").unwrap();
        let json = serde_json::to_string(s).unwrap();
        assert!(json.contains("\"id\":\"archive_risk_backup_first\""));
        assert!(json.contains("\"requires_live_host\":false"));
        assert!(json.contains("\"owning_bead\""));
        assert!(json.contains("\"expected_log_artifacts\""));
        assert!(json.contains("\"json_assertions\""));
    }

    #[test]
    fn scenarios_are_deterministic_in_order() {
        let a: Vec<&str> = e2e_scenarios().iter().map(|s| s.id).collect();
        assert_eq!(
            a.first(),
            Some(&"local_healthy_lexical_semantic_unavailable")
        );
        assert_eq!(a, e2e_scenarios().iter().map(|s| s.id).collect::<Vec<_>>());
    }

    #[test]
    fn every_assertion_is_machine_evaluable() -> Result<(), String> {
        let fixture = serde_json::json!({
            "ok": true,
            "state": "timeout",
            "count": 1,
            "message": "inspect quarantine",
            "items": [1]
        });
        for assertion in [
            JsonExpectation::Exists { pointer: "/ok" },
            JsonExpectation::EqualsBool {
                pointer: "/ok",
                expected: true,
            },
            JsonExpectation::EqualsString {
                pointer: "/state",
                expected: "timeout",
            },
            JsonExpectation::EqualsU64 {
                pointer: "/count",
                expected: 1,
            },
            JsonExpectation::ContainsString {
                pointer: "/message",
                expected_fragment: "quarantine",
            },
            JsonExpectation::NonEmptyArray { pointer: "/items" },
            JsonExpectation::OneOfStrings {
                pointer: "/state",
                expected: &["timeout", "unreachable"],
            },
        ] {
            if !assertion.evaluate(&fixture) {
                return Err(assertion.label());
            }
        }
        Ok(())
    }
}
