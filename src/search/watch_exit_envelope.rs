// Dead-code tolerated module-wide: the watch-mode exit envelope lands here
// ahead of the `cass index --watch` exit path that will emit it on stderr /
// in the robot JSON. Downstream bead .11.5 (integrated golden + E2E gate)
// asserts these envelopes end-to-end.
#![allow(dead_code)]

//! Parseable watch-mode exit error envelopes (bead
//! cass-fleet-resilience-20260608-uojcg.4.4).
//!
//! Issue #250 found `cass index --watch` exiting code 9 roughly every 2.5
//! minutes with no useful stdout/stderr — only a `drop_close` warning leaked
//! from FrankenStorage. An agent watching that process had nothing parseable
//! to act on: no subsystem, no cause, no retry guidance.
//!
//! This module defines the single [`WatchExitEnvelope`] every watch-mode
//! exit emits: a stable `kind` (`err.kind`), the `subsystem` that failed,
//! the `likely_cause`, a `retryability` verdict, and a copy-pasteable
//! `next_command`. Destructor/drop warnings remain supplementary — the
//! envelope is the primary diagnostic.
//!
//! Constructors map the failure scenarios the report calls out (connector
//! parse failure, storage close/drop failure, lock loss, source-path
//! permission error, timeout) to stable envelopes, so the classification is
//! unit-testable without spawning a watch loop. All enums serialize as
//! snake_case; `next_command` never uses a bare `cass`/`bv` and never
//! suggests destructive cleanup.

use serde::{Deserialize, Serialize};

/// Stable machine-matchable cause of a watch-mode exit (`err.kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WatchExitKind {
    /// A connector failed to parse a session file.
    ConnectorParseFailure,
    /// The storage layer failed to close/flush cleanly (the #250 drop_close
    /// class), risking an unflushed tail.
    StorageCloseFailure,
    /// The exclusive index/watch lock was lost to another process.
    LockLost,
    /// A configured source path is unreadable (permissions / unmounted).
    SourcePathPermission,
    /// The watch cycle exceeded its time budget without completing.
    Timeout,
    /// A clean, intentional shutdown (not an error).
    CleanShutdown,
    /// An exit whose cause could not be classified.
    Unknown,
}

/// The subsystem a watch-mode exit originated in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Subsystem {
    Connector,
    Storage,
    Lock,
    Source,
    Watch,
    Unknown,
}

/// Whether and how the watch can be retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Retryability {
    /// Safe to re-run immediately (transient).
    Retryable,
    /// Retry only after the operator fixes the underlying cause.
    RetryAfterFix,
    /// Not retryable without intervention; do not loop on it.
    Fatal,
}

/// The parseable envelope a watch-mode exit emits. Carries everything an
/// agent needs to decide whether to retry, escalate, or fix-then-retry,
/// without scraping prose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WatchExitEnvelope {
    pub kind: WatchExitKind,
    pub subsystem: Subsystem,
    pub retryability: Retryability,
    /// Process exit code that accompanied this envelope.
    pub exit_code: i32,
    /// One-line human cause.
    pub likely_cause: String,
    /// Copy-pasteable next command, or `None` when waiting/none applies.
    /// Never a bare `cass`/`bv`, never destructive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_command: Option<String>,
    /// Optional supplementary detail (e.g. the offending path or a drop
    /// warning), kept distinct from the structured fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl WatchExitEnvelope {
    fn new(
        kind: WatchExitKind,
        subsystem: Subsystem,
        retryability: Retryability,
        exit_code: i32,
        likely_cause: &str,
        next_command: Option<&str>,
        detail: Option<String>,
    ) -> Self {
        Self {
            kind,
            subsystem,
            retryability,
            exit_code,
            likely_cause: likely_cause.to_string(),
            next_command: next_command.map(str::to_string),
            detail,
        }
    }

    /// Whether following the envelope's guidance is safe to do automatically
    /// (retry now) vs requires an operator fix or a stop.
    pub(crate) fn is_auto_retryable(&self) -> bool {
        matches!(self.retryability, Retryability::Retryable)
    }

    /// A connector failed to parse a session file. Retryable after the
    /// offending input is skipped/quarantined; the watch itself is healthy.
    pub(crate) fn connector_parse_failure(exit_code: i32, detail: impl Into<String>) -> Self {
        Self::new(
            WatchExitKind::ConnectorParseFailure,
            Subsystem::Connector,
            Retryability::RetryAfterFix,
            exit_code,
            "a connector failed to parse a session file; the poison input must be quarantined or skipped",
            Some("cass diag --json --quarantine"),
            Some(detail.into()),
        )
    }

    /// Storage failed to close/flush cleanly (#250 drop_close). Retryable,
    /// but inspect for an unflushed tail first.
    pub(crate) fn storage_close_failure(exit_code: i32, detail: impl Into<String>) -> Self {
        Self::new(
            WatchExitKind::StorageCloseFailure,
            Subsystem::Storage,
            Retryability::Retryable,
            exit_code,
            "storage did not close cleanly on exit; the most recent tail may be unflushed",
            Some("cass health --json"),
            Some(detail.into()),
        )
    }

    /// The exclusive lock was lost to another process. Do not loop; another
    /// indexer likely owns it.
    pub(crate) fn lock_lost(exit_code: i32, detail: impl Into<String>) -> Self {
        Self::new(
            WatchExitKind::LockLost,
            Subsystem::Lock,
            Retryability::RetryAfterFix,
            exit_code,
            "lost the exclusive index/watch lock; another process may be indexing",
            Some("cass status --json"),
            Some(detail.into()),
        )
    }

    /// A configured source path is unreadable. Operator must fix permissions
    /// or remount before retrying.
    pub(crate) fn source_path_permission(exit_code: i32, path: impl Into<String>) -> Self {
        Self::new(
            WatchExitKind::SourcePathPermission,
            Subsystem::Source,
            Retryability::RetryAfterFix,
            exit_code,
            "a configured source path is unreadable (permissions or unmounted); fix access before retrying",
            Some("cass sources list --json"),
            Some(path.into()),
        )
    }

    /// The watch cycle exceeded its time budget. Transient; safe to re-run.
    pub(crate) fn timeout(exit_code: i32, detail: impl Into<String>) -> Self {
        Self::new(
            WatchExitKind::Timeout,
            Subsystem::Watch,
            Retryability::Retryable,
            exit_code,
            "the watch cycle exceeded its time budget before completing",
            Some("cass index --watch"),
            Some(detail.into()),
        )
    }

    /// A clean, intentional shutdown.
    pub(crate) fn clean_shutdown() -> Self {
        Self::new(
            WatchExitKind::CleanShutdown,
            Subsystem::Watch,
            Retryability::Retryable,
            0,
            "watch exited cleanly",
            None,
            None,
        )
    }

    /// An exit that could not be classified — still parseable, never silent.
    pub(crate) fn unknown(exit_code: i32, detail: impl Into<String>) -> Self {
        Self::new(
            WatchExitKind::Unknown,
            Subsystem::Unknown,
            Retryability::RetryAfterFix,
            exit_code,
            "watch exited for an unclassified reason; inspect health before retrying",
            Some("cass health --json"),
            Some(detail.into()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enums_serialize_as_snake_case() {
        assert_eq!(
            serde_json::to_string(&WatchExitKind::ConnectorParseFailure).unwrap(),
            "\"connector_parse_failure\""
        );
        assert_eq!(
            serde_json::to_string(&Subsystem::Storage).unwrap(),
            "\"storage\""
        );
        assert_eq!(
            serde_json::to_string(&Retryability::RetryAfterFix).unwrap(),
            "\"retry_after_fix\""
        );
    }

    #[test]
    fn each_scenario_has_stable_kind_subsystem_and_next_action() {
        let cases = [
            (
                WatchExitEnvelope::connector_parse_failure(9, "chatgpt: bad json at line 4"),
                WatchExitKind::ConnectorParseFailure,
                Subsystem::Connector,
                Retryability::RetryAfterFix,
            ),
            (
                WatchExitEnvelope::storage_close_failure(9, "drop_close: flush returned EIO"),
                WatchExitKind::StorageCloseFailure,
                Subsystem::Storage,
                Retryability::Retryable,
            ),
            (
                WatchExitEnvelope::lock_lost(9, "lock held by pid 1234"),
                WatchExitKind::LockLost,
                Subsystem::Lock,
                Retryability::RetryAfterFix,
            ),
            (
                WatchExitEnvelope::source_path_permission(9, "/mnt/archive"),
                WatchExitKind::SourcePathPermission,
                Subsystem::Source,
                Retryability::RetryAfterFix,
            ),
            (
                WatchExitEnvelope::timeout(9, "cycle exceeded 600s"),
                WatchExitKind::Timeout,
                Subsystem::Watch,
                Retryability::Retryable,
            ),
        ];
        for (env, kind, subsystem, retry) in cases {
            assert_eq!(env.kind, kind);
            assert_eq!(env.subsystem, subsystem);
            assert_eq!(env.retryability, retry);
            // Every error envelope names a non-empty cause and a next command.
            assert!(!env.likely_cause.is_empty(), "{kind:?} needs a cause");
            assert!(env.next_command.is_some(), "{kind:?} needs a next command");
        }
    }

    #[test]
    fn next_commands_are_never_bare_and_never_destructive() {
        let envs = [
            WatchExitEnvelope::connector_parse_failure(9, "x"),
            WatchExitEnvelope::storage_close_failure(9, "x"),
            WatchExitEnvelope::lock_lost(9, "x"),
            WatchExitEnvelope::source_path_permission(9, "/p"),
            WatchExitEnvelope::timeout(9, "x"),
            WatchExitEnvelope::unknown(9, "x"),
        ];
        for env in envs {
            let cmd = env.next_command.unwrap();
            assert_ne!(cmd.trim(), "cass", "must not be a bare cass");
            assert_ne!(cmd.trim(), "bv", "must not be a bare bv");
            for bad in ["rm ", "rm -", "--force-clean", "DROP ", "delete"] {
                assert!(
                    !cmd.contains(bad),
                    "next command must not suggest destructive cleanup: {cmd}"
                );
            }
        }
    }

    #[test]
    fn transient_failures_are_auto_retryable_and_operator_faults_are_not() {
        assert!(WatchExitEnvelope::timeout(9, "x").is_auto_retryable());
        assert!(WatchExitEnvelope::storage_close_failure(9, "x").is_auto_retryable());
        assert!(!WatchExitEnvelope::source_path_permission(9, "/p").is_auto_retryable());
        assert!(!WatchExitEnvelope::lock_lost(9, "x").is_auto_retryable());
        assert!(!WatchExitEnvelope::connector_parse_failure(9, "x").is_auto_retryable());
    }

    #[test]
    fn clean_shutdown_is_zero_exit_with_no_command() {
        let env = WatchExitEnvelope::clean_shutdown();
        assert_eq!(env.kind, WatchExitKind::CleanShutdown);
        assert_eq!(env.exit_code, 0);
        assert!(env.next_command.is_none());
    }

    #[test]
    fn source_path_permission_carries_the_offending_path_as_detail() {
        let env = WatchExitEnvelope::source_path_permission(9, "/mnt/archive");
        assert_eq!(env.detail.as_deref(), Some("/mnt/archive"));
    }

    #[test]
    fn envelope_round_trips_through_json() {
        let env = WatchExitEnvelope::storage_close_failure(9, "drop_close: EIO");
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains("\"kind\":\"storage_close_failure\""));
        assert!(json.contains("\"subsystem\":\"storage\""));
        assert!(json.contains("\"retryability\":\"retryable\""));
        assert!(json.contains("\"exit_code\":9"));
        let parsed: WatchExitEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, env);
    }
}
