// Dead-code tolerated module-wide: this proof-logging schema lands ahead of
// the bounded E2E runner (.12.2), the report-derived scenario scripts
// (.12.5), and the CI/local proof recipe (.12.6) that emit and consume it.
#![allow(dead_code)]

//! Proof logging schema, artifact manifest, and retention policy (bead
//! cass-fleet-resilience-20260608-uojcg.12.3).
//!
//! The E2E runner, golden gates, and closeout reports all need a structured
//! record that makes it **impossible to confuse** five outcomes that look
//! alike in prose: a command that passed, one that timed out after partial
//! output, a stale artifact that was reused, invalid JSON, and a test that
//! never ran. This module defines that record ([`ProofLogRecord`]), the
//! [`ProofOutcome`] taxonomy and its derivation from observed signals, an
//! artifact manifest, and a [`RetentionPolicy`] that keeps enough for
//! debugging without leaking private session content.
//!
//! Final proof reports cite these structured artifacts, never prose-only
//! claims. All enums serialize as snake_case; `sanitized_env` is expected to
//! already exclude secrets and the retention layer enforces a redaction
//! guard before any artifact is kept.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The mutually-exclusive outcome of a proof run. The whole point of the
/// schema: these five (+ explicit failure) can never be confused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProofOutcome {
    /// Command ran, exited 0, and met the robot/ansi contract.
    Passed,
    /// Command ran but exited non-zero with a valid (non-timeout) result.
    Failed,
    /// Command was killed by its timeout; any output is partial.
    TimedOutPartial,
    /// A pre-existing artifact was reused instead of a fresh run.
    StaleArtifactReused,
    /// Command ran but its expected JSON could not be parsed.
    InvalidJson,
    /// The command never executed (skip, missing binary, harness error).
    DidNotRun,
}

impl ProofOutcome {
    /// Whether this outcome counts as a green proof.
    pub(crate) fn is_pass(self) -> bool {
        matches!(self, Self::Passed)
    }
}

/// Observed signals from a run, from which [`ProofOutcome`] is derived. Kept
/// explicit so the disambiguation is unit-testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OutcomeSignals {
    /// The command actually executed (vs skipped / missing binary).
    pub executed: bool,
    /// The command was killed by its timeout.
    pub timed_out: bool,
    /// A pre-existing artifact was reused rather than freshly produced.
    pub stale_artifact_reused: bool,
    /// Process exit code, when it ran to completion.
    pub exit_code: Option<i32>,
    /// The contract expects parseable JSON on stdout.
    pub expects_json: bool,
    /// The expected JSON parsed successfully.
    pub parsed_json_ok: bool,
    /// The robot contract (required fields/shape) was satisfied.
    pub robot_contract_ok: bool,
    /// stdout was free of ANSI escapes (robot-mode hygiene).
    pub ansi_free_stdout_ok: bool,
}

impl OutcomeSignals {
    /// Derive the single proof outcome. Order matters: not-run, timeout, and
    /// stale-reuse are distinguished before pass/fail so they can never be
    /// mistaken for one another.
    pub(crate) fn outcome(&self) -> ProofOutcome {
        if !self.executed {
            return ProofOutcome::DidNotRun;
        }
        if self.timed_out {
            return ProofOutcome::TimedOutPartial;
        }
        if self.stale_artifact_reused {
            return ProofOutcome::StaleArtifactReused;
        }
        if self.expects_json && !self.parsed_json_ok {
            return ProofOutcome::InvalidJson;
        }
        if self.exit_code == Some(0) && self.robot_contract_ok && self.ansi_free_stdout_ok {
            return ProofOutcome::Passed;
        }
        ProofOutcome::Failed
    }
}

/// Where and how the proof ran — captured so a result is reproducible and
/// attributable to a specific binary/revision/config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProofRunMeta {
    pub cass_binary_path: String,
    pub cass_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_revision: Option<String>,
    pub cargo_profile: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub feature_flags: Vec<String>,
    pub target_dir: String,
    pub data_dir: String,
    pub config_dir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_roots: Vec<String>,
}

/// Exactly what was executed and how it terminated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProofExecution {
    pub argv: Vec<String>,
    /// Environment as executed, already sanitized of secrets.
    #[serde(default)]
    pub sanitized_env: BTreeMap<String, String>,
    pub timeout_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
    pub timed_out: bool,
    pub retry_count: u32,
}

/// The artifact manifest produced by a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProofArtifacts {
    pub stdout_path: String,
    pub stderr_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parsed_stdout_json: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parsed_stderr_json: Option<serde_json::Value>,
    pub robot_contract_ok: bool,
    pub ansi_free_stdout_ok: bool,
}

/// One proof-log record: the canonical, citeable unit of evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProofLogRecord {
    pub run_id: String,
    pub scenario_id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub issue_ids_covered: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixture_id: Option<String>,
    pub command_id: String,
    pub phase: String,
    pub started_at_ms: i64,
    pub finished_at_ms: i64,
    pub elapsed_ms: i64,
    pub meta: ProofRunMeta,
    pub execution: ProofExecution,
    pub artifacts: ProofArtifacts,
    pub outcome: ProofOutcome,
}

impl ProofLogRecord {
    /// Whether this record proves a green run.
    pub(crate) fn is_pass(&self) -> bool {
        self.outcome.is_pass()
    }
}

/// Environment variable name fragments that must never appear in a retained
/// `sanitized_env` (the redaction guard against leaking secrets/PII).
const SECRET_ENV_MARKERS: &[&str] = &[
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "API_KEY",
    "APIKEY",
    "CREDENTIAL",
    "PRIVATE_KEY",
    "SESSION",
];

/// Retention policy for proof artifacts: keep enough to debug, drop the rest,
/// and never retain records whose env still carries secrets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RetentionPolicy {
    /// Always keep the most recent N records per scenario.
    pub keep_last_n: usize,
    /// Always keep non-pass records (failures/timeouts) for debugging.
    pub keep_all_non_pass: bool,
    /// Drop records older than this (ms), unless kept by another rule.
    pub max_age_ms: i64,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            keep_last_n: 20,
            keep_all_non_pass: true,
            max_age_ms: 14 * 24 * 60 * 60 * 1000, // 14 days
        }
    }
}

impl RetentionPolicy {
    /// Whether a record is retained, given its recency rank (0 = newest) and
    /// `now_ms`. Non-pass records and the most-recent N are always kept;
    /// otherwise age decides.
    pub(crate) fn retains(
        &self,
        record: &ProofLogRecord,
        rank_from_newest: usize,
        now_ms: i64,
    ) -> bool {
        if self.keep_all_non_pass && !record.is_pass() {
            return true;
        }
        if rank_from_newest < self.keep_last_n {
            return true;
        }
        let age = (now_ms - record.finished_at_ms).max(0);
        age <= self.max_age_ms
    }

    /// Redaction guard: a record may only be retained if its `sanitized_env`
    /// carries no secret-bearing keys. Returns the offending keys (empty when
    /// safe).
    pub(crate) fn secret_leak_keys(record: &ProofLogRecord) -> Vec<String> {
        record
            .execution
            .sanitized_env
            .keys()
            .filter(|k| {
                let up = k.to_ascii_uppercase();
                SECRET_ENV_MARKERS.iter().any(|m| up.contains(m))
            })
            .cloned()
            .collect()
    }

    /// Whether the record is safe to retain (no secret env leak).
    pub(crate) fn is_redaction_safe(record: &ProofLogRecord) -> bool {
        Self::secret_leak_keys(record).is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn green_signals() -> OutcomeSignals {
        OutcomeSignals {
            executed: true,
            timed_out: false,
            stale_artifact_reused: false,
            exit_code: Some(0),
            expects_json: true,
            parsed_json_ok: true,
            robot_contract_ok: true,
            ansi_free_stdout_ok: true,
        }
    }

    fn record_with(
        outcome: ProofOutcome,
        env: &[(&str, &str)],
        finished_at_ms: i64,
    ) -> ProofLogRecord {
        ProofLogRecord {
            run_id: "run-1".to_string(),
            scenario_id: "scn-archive-risk".to_string(),
            issue_ids_covered: vec!["#248".to_string()],
            fixture_id: Some("ts1_high_archive_risk".to_string()),
            command_id: "status_json".to_string(),
            phase: "verify".to_string(),
            started_at_ms: finished_at_ms - 100,
            finished_at_ms,
            elapsed_ms: 100,
            meta: ProofRunMeta {
                cass_binary_path: "/tmp/cass-tgt/debug/cass".to_string(),
                cass_version: "0.6.13".to_string(),
                git_revision: Some("abc1234".to_string()),
                cargo_profile: "dev".to_string(),
                feature_flags: vec![],
                target_dir: "/tmp/cass-tgt".to_string(),
                data_dir: "/tmp/data".to_string(),
                config_dir: "/tmp/config".to_string(),
                model_dir: None,
                source_roots: vec!["/dp/proj".to_string()],
            },
            execution: ProofExecution {
                argv: vec![
                    "cass".to_string(),
                    "status".to_string(),
                    "--json".to_string(),
                ],
                sanitized_env: env
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                timeout_ms: 5000,
                exit_code: Some(0),
                signal: None,
                timed_out: false,
                retry_count: 0,
            },
            artifacts: ProofArtifacts {
                stdout_path: "/tmp/run-1.stdout".to_string(),
                stderr_path: "/tmp/run-1.stderr".to_string(),
                parsed_stdout_json: Some(serde_json::json!({"healthy": true})),
                parsed_stderr_json: None,
                robot_contract_ok: true,
                ansi_free_stdout_ok: true,
            },
            outcome,
        }
    }

    #[test]
    fn outcomes_serialize_snake_case() {
        assert_eq!(
            serde_json::to_string(&ProofOutcome::TimedOutPartial).unwrap(),
            "\"timed_out_partial\""
        );
        assert_eq!(
            serde_json::to_string(&ProofOutcome::StaleArtifactReused).unwrap(),
            "\"stale_artifact_reused\""
        );
        assert_eq!(
            serde_json::to_string(&ProofOutcome::DidNotRun).unwrap(),
            "\"did_not_run\""
        );
    }

    #[test]
    fn the_five_confusable_outcomes_are_each_distinguished() {
        // passed
        assert_eq!(green_signals().outcome(), ProofOutcome::Passed);
        // did not run
        let mut s = green_signals();
        s.executed = false;
        assert_eq!(s.outcome(), ProofOutcome::DidNotRun);
        // timed out (partial) — dominates even a zero-ish exit
        let mut s = green_signals();
        s.timed_out = true;
        assert_eq!(s.outcome(), ProofOutcome::TimedOutPartial);
        // stale artifact reused
        let mut s = green_signals();
        s.stale_artifact_reused = true;
        assert_eq!(s.outcome(), ProofOutcome::StaleArtifactReused);
        // invalid json
        let mut s = green_signals();
        s.parsed_json_ok = false;
        assert_eq!(s.outcome(), ProofOutcome::InvalidJson);
        // failed (ran, exit non-zero)
        let mut s = green_signals();
        s.exit_code = Some(1);
        assert_eq!(s.outcome(), ProofOutcome::Failed);
    }

    #[test]
    fn timeout_is_never_confused_with_a_clean_pass() {
        // A run that timed out but happened to have a stale exit code must
        // read as timed_out_partial, not passed.
        let mut s = green_signals();
        s.timed_out = true;
        s.exit_code = Some(0);
        assert_ne!(s.outcome(), ProofOutcome::Passed);
        assert_eq!(s.outcome(), ProofOutcome::TimedOutPartial);
    }

    #[test]
    fn record_round_trips_through_json_with_required_fields() {
        let r = record_with(
            ProofOutcome::Passed,
            &[("CASS_DATA_DIR", "/tmp/data")],
            1000,
        );
        let json = serde_json::to_string(&r).unwrap();
        for key in [
            "run_id",
            "scenario_id",
            "command_id",
            "phase",
            "started_at_ms",
            "finished_at_ms",
            "elapsed_ms",
            "cass_binary_path",
            "cass_version",
            "argv",
            "timeout_ms",
            "timed_out",
            "retry_count",
            "stdout_path",
            "stderr_path",
            "robot_contract_ok",
            "ansi_free_stdout_ok",
            "outcome",
        ] {
            assert!(json.contains(key), "record JSON missing {key}");
        }
        let parsed: ProofLogRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, r);
    }

    #[test]
    fn retention_keeps_failures_and_recent_drops_old_passes() {
        let policy = RetentionPolicy {
            keep_last_n: 2,
            keep_all_non_pass: true,
            max_age_ms: 1000,
        };
        let now = 1_000_000;

        // A failure is always kept, even old and beyond keep_last_n.
        let old_fail = record_with(ProofOutcome::Failed, &[], now - 10_000);
        assert!(policy.retains(&old_fail, 50, now));

        // A recent pass (within keep_last_n) is kept.
        let recent_pass = record_with(ProofOutcome::Passed, &[], now - 10_000);
        assert!(policy.retains(&recent_pass, 0, now));

        // An old pass beyond keep_last_n and older than max_age is dropped.
        let old_pass = record_with(ProofOutcome::Passed, &[], now - 10_000);
        assert!(!policy.retains(&old_pass, 50, now));

        // A pass beyond keep_last_n but within max_age is kept.
        let fresh_pass = record_with(ProofOutcome::Passed, &[], now - 500);
        assert!(policy.retains(&fresh_pass, 50, now));
    }

    #[test]
    fn redaction_guard_flags_secret_bearing_env_keys() {
        let safe = record_with(
            ProofOutcome::Passed,
            &[("CASS_DATA_DIR", "/tmp/data")],
            1000,
        );
        assert!(RetentionPolicy::is_redaction_safe(&safe));
        assert!(RetentionPolicy::secret_leak_keys(&safe).is_empty());

        for leak in [
            "ANTHROPIC_API_KEY",
            "GH_TOKEN",
            "DB_PASSWORD",
            "AWS_SECRET_ACCESS_KEY",
        ] {
            let bad = record_with(ProofOutcome::Passed, &[(leak, "x")], 1000);
            assert!(
                !RetentionPolicy::is_redaction_safe(&bad),
                "{leak} should be flagged"
            );
            assert_eq!(
                RetentionPolicy::secret_leak_keys(&bad),
                vec![leak.to_string()]
            );
        }
    }

    #[test]
    fn outcome_in_record_matches_serialized_form() {
        let r = record_with(ProofOutcome::InvalidJson, &[], 1000);
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"outcome\":\"invalid_json\""));
        assert!(!r.is_pass());
    }
}
