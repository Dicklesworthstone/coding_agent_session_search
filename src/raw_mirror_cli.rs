//! Optional raw-mirror maintenance inside the observed indexing preflight.
use std::path::{Path, PathBuf};

fn validate_maintenance_archive(data_dir: &Path, db_path: &Path) -> anyhow::Result<()> {
    // The existing keep-tag query intentionally addresses agent_search.db.
    // Do not consult a different archive's pins for a custom database basename.
    let absolute = |path: &Path| -> std::io::Result<PathBuf> {
        if path.is_absolute() {
            Ok(path.to_path_buf())
        } else {
            Ok(std::env::current_dir()?.join(path))
        }
    };
    let actual = absolute(db_path)?;
    let expected = absolute(&data_dir.join("agent_search.db"))?;
    let same = actual == expected
        || (super::store::raw_mirror_io("archive_exists", &actual, || actual.try_exists())?
            && super::store::raw_mirror_io("archive_exists", &expected, || expected.try_exists())?
            && super::store::raw_mirror_io("canonicalize_archive", &actual, || {
                std::fs::canonicalize(&actual)
            })? == super::store::raw_mirror_io("canonicalize_archive", &expected, || {
                std::fs::canonicalize(&expected)
            })?);
    if !same {
        anyhow::bail!(
            "automatic raw-mirror retention skipped: a custom --db requires explicit mirror pruning so keep-tag pins are not read from the wrong archive"
        );
    }
    Ok(())
}

/// Called by the index owner after tracing, progress observation and its
/// watchdog have started. The concrete index-run lease prevents a second
/// acquisition of the same lock while preserving index-before-mirror ordering.
/// Optional failure never blocks indexing; the caller still checks cancellation
/// after return.
pub(crate) fn maintain_for_index(
    data_dir: &Path,
    db_path: &Path,
    index_lease: &crate::indexer::IndexRunLockGuard,
) {
    let result = validate_maintenance_archive(data_dir, db_path)
        .and_then(|()| super::store::policy::maintain_while_index_owned(data_dir, index_lease));
    // JSON on stderr keeps robot stdout data-only during preflight.
    match result {
        Ok(report) if report.retired_manifests > 0 || report.capture_paused => {
            eprintln!(
                "{}",
                serde_json::json!({
                    "raw_mirror_maintenance": report,
                    "hint": "Indexing retains its normal disk-safety checks. If capture remains paused, free space or review cass mirror prune (dry-run first); protected evidence and hold-downs are never overridden."
                })
            );
        }
        Ok(_) => {}
        Err(error) => {
            eprintln!(
                "{}",
                serde_json::json!({
                    "raw_mirror_maintenance": {"status": "skipped", "reason": error.to_string()},
                    "hint": "Indexing will still attempt its normal disk preflight. Review cass mirror prune and CASS_RAW_MIRROR_MAX_BYTES / CASS_RAW_MIRROR_MIN_FREE_BYTES; no protected evidence was intentionally discarded."
                })
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_data_directory_is_used_without_touching_the_default_archive() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("isolated");
        let db_path = directory.join("agent_search.db");
        validate_maintenance_archive(&directory, &db_path).unwrap();
        super::super::maintain_before_index(&directory).unwrap();
        assert!(
            !directory.exists(),
            "an absent mirror needs no maintenance writes"
        );
    }

    #[test]
    fn custom_database_name_cannot_read_pins_from_a_neighboring_archive() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("scratch.db");
        assert!(validate_maintenance_archive(temp.path(), &path).is_err());
        assert!(std::fs::read_dir(temp.path()).unwrap().next().is_none());
        let path = temp.path().join("agent_search.db");
        validate_maintenance_archive(temp.path(), &path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn archive_alias_is_allowed_only_when_it_resolves_to_the_same_pin_database() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("agent_search.db");
        let alias = temp.path().join("archive-alias.db");
        std::fs::write(&archive, []).unwrap();
        std::os::unix::fs::symlink(&archive, &alias).unwrap();
        validate_maintenance_archive(temp.path(), &alias).unwrap();
        let other = temp.path().join("another.db");
        std::fs::write(&other, []).unwrap();
        assert!(validate_maintenance_archive(temp.path(), &other).is_err());
    }
}
