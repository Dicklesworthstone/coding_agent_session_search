//! Read-only source-schedule admission. Unknown configuration or sync history is
//! not an empty plan: never launch sync/index/backfill from a fabricated default.

use std::path::Path;

use crate::sources::config::{SourcesConfig, SyncSchedule};
use crate::sources::sync::{SourceSyncAction, SyncStatus};

/// Deliberately omits underlying parser text, paths and remote credentials.
/// The configuration and sync ledger remain untouched for operator inspection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SourceScheduleError {
    #[error("cannot read or validate source configuration; no scheduled work was started")]
    Configuration,
    #[error("cannot read or validate sync history; no scheduled work was started")]
    SyncHistory,
}

impl SourceScheduleError {
    pub fn reason_code(self) -> &'static str {
        match self {
            Self::Configuration => "source_configuration_unavailable",
            Self::SyncHistory => "source_sync_history_unavailable",
        }
    }
}

/// Remote sources whose automatic schedule is due now. A genuinely absent
/// configuration or first-run sync ledger is supported; unreadable or malformed
/// state is a failure, not permission to sync everything or claim nothing is due.
pub fn due_remote_sources(
    data_dir: &Path,
    now_ms: i64,
) -> Result<Vec<(String, String)>, SourceScheduleError> {
    let path = SourcesConfig::config_path().map_err(|_| SourceScheduleError::Configuration)?;
    due_sources_at(&path, data_dir, now_ms)
}

fn due_sources_at(
    config_path: &Path,
    data_dir: &Path,
    now_ms: i64,
) -> Result<Vec<(String, String)>, SourceScheduleError> {
    // SourcesConfig's convenience loader treats `exists() == false` as absent.
    // Preserve that intentional first-run case, but do not mask metadata errors
    // or a dangling configured symlink as "no remote sources".
    match std::fs::symlink_metadata(config_path) {
        Ok(_) => {
            std::fs::metadata(config_path).map_err(|_| SourceScheduleError::Configuration)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => return Err(SourceScheduleError::Configuration),
    }
    let config = SourcesConfig::load_from(&config_path.to_path_buf())
        .map_err(|_| SourceScheduleError::Configuration)?;
    // The sync ledger is irrelevant to a local-only/manual-only configuration.
    // Do not make an unrelated corrupt ledger prevent ordinary local indexing.
    if !config
        .remote_sources()
        .any(|source| !matches!(source.sync_schedule, SyncSchedule::Manual))
    {
        return Ok(Vec::new());
    }
    let status = SyncStatus::load(data_dir).map_err(|_| SourceScheduleError::SyncHistory)?;
    Ok(config
        .remote_sources()
        .filter_map(|source| {
            let decision = status.decision_for_source_at(source, now_ms, false);
            match decision.action {
                SourceSyncAction::Sync => Some((source.name.clone(), decision.reasons.join("; "))),
                SourceSyncAction::Skip | SourceSyncAction::Defer => None,
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const AUTOMATIC: &str = "[[sources]]\nname = 'fixture'\ntype = 'ssh'\nhost = 'fixture.invalid'\npaths = ['~/.claude/projects']\nsync_schedule = 'hourly'\n";

    #[test]
    fn missing_configuration_and_first_run_history_are_distinct_valid_cases() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("sources.toml");
        assert!(due_sources_at(&config, directory.path(), 100).unwrap().is_empty());
        assert!(!config.exists());
        std::fs::write(&config, AUTOMATIC).unwrap();
        let due = due_sources_at(&config, directory.path(), 100).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].0, "fixture");
        assert!(!directory.path().join("sync_status.json").exists());
        assert_eq!(std::fs::read_to_string(config).unwrap(), AUTOMATIC);
    }

    #[test]
    fn malformed_configuration_is_not_an_empty_plan_and_diagnostics_are_redacted() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("sources.toml");
        let bytes = "private-configuration-sentinel = [unterminated";
        std::fs::write(&config, bytes).unwrap();
        let error = due_sources_at(&config, directory.path(), 100).unwrap_err();
        assert_eq!(error, SourceScheduleError::Configuration);
        assert_eq!(error.reason_code(), "source_configuration_unavailable");
        assert!(!format!("{error:?} {error}").contains("private-configuration-sentinel"));
        assert_eq!(std::fs::read_to_string(config).unwrap(), bytes);
    }

    #[test]
    fn invalid_sync_ledger_cannot_reset_failure_backoff() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("sources.toml");
        let ledger = directory.path().join("sync_status.json");
        std::fs::write(&config, AUTOMATIC).unwrap();
        let bytes = "{\"private-ledger-sentinel\":";
        std::fs::write(&ledger, bytes).unwrap();
        let error = due_sources_at(&config, directory.path(), 100).unwrap_err();
        assert_eq!(error, SourceScheduleError::SyncHistory);
        assert_eq!(error.reason_code(), "source_sync_history_unavailable");
        assert!(!format!("{error:?} {error}").contains("private-ledger-sentinel"));
        assert_eq!(std::fs::read_to_string(ledger).unwrap(), bytes);
    }

    #[test]
    fn filesystem_errors_are_not_first_run_defaults() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("sources.toml");
        std::fs::create_dir(&config).unwrap();
        assert_eq!(
            due_sources_at(&config, directory.path(), 100).unwrap_err(),
            SourceScheduleError::Configuration
        );
        let other_config = directory.path().join("valid.toml");
        std::fs::write(&other_config, AUTOMATIC).unwrap();
        std::fs::create_dir(directory.path().join("sync_status.json")).unwrap();
        assert_eq!(
            due_sources_at(&other_config, directory.path(), 100).unwrap_err(),
            SourceScheduleError::SyncHistory
        );
    }

    #[test]
    fn manual_only_configuration_does_not_consult_an_unused_ledger() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("sources.toml");
        std::fs::write(&config, AUTOMATIC.replace("hourly", "manual")).unwrap();
        std::fs::write(directory.path().join("sync_status.json"), "not json").unwrap();
        assert!(due_sources_at(&config, directory.path(), 100).unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn dangling_configuration_symlink_is_an_error_not_a_missing_configuration() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("sources.toml");
        std::os::unix::fs::symlink(directory.path().join("missing.toml"), &config).unwrap();
        assert_eq!(
            due_sources_at(&config, directory.path(), 100).unwrap_err(),
            SourceScheduleError::Configuration
        );
        assert!(std::fs::symlink_metadata(config).unwrap().file_type().is_symlink());
    }
}
