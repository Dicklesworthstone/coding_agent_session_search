//! Run optional raw-mirror maintenance before normal indexing's disk preflight.
use coding_agent_search::{Cli, Commands};
use std::path::PathBuf;

fn index_data_dir(cli: &Cli) -> anyhow::Result<Option<PathBuf>> {
    if cli.robot_help { return Ok(None); }
    let Some(Commands::Index { data_dir, .. }) = cli.command.as_ref() else { return Ok(None); };
    let directory = coding_agent_search::resolve_data_dir(data_dir, cli.db.as_ref());
    // The existing keep-tag query intentionally addresses agent_search.db.
    // Do not consult a different archive's pins for a custom database basename.
    if let Some(db) = cli.db.as_ref() {
        let absolute = |path: &std::path::Path| -> std::io::Result<PathBuf> {
            if path.is_absolute() { Ok(path.to_path_buf()) }
            else { Ok(std::env::current_dir()?.join(path)) }
        };
        let actual = absolute(db)?;
        let expected = absolute(&directory.join("agent_search.db"))?;
        let same = actual == expected || (actual.exists() && expected.exists()
            && std::fs::canonicalize(&actual)? == std::fs::canonicalize(&expected)?);
        if !same {
            anyhow::bail!("automatic raw-mirror retention skipped: a custom --db requires explicit mirror pruning so keep-tag pins are not read from the wrong archive");
        }
    }
    Ok(Some(directory))
}

pub fn maintain(cli: &Cli) {
    let result = index_data_dir(cli).and_then(|directory| directory
        .map(|directory| coding_agent_search::raw_mirror::maintain_before_index(&directory))
        .transpose());
    // JSON on stderr keeps robot stdout data-only even before tracing starts.
    match result {
        Ok(Some(report)) if report.retired_manifests > 0 || report.capture_paused => {
            eprintln!("{}", serde_json::json!({
                "raw_mirror_maintenance": report,
                "hint": "Indexing retains its normal disk-safety checks. If capture remains paused, free space or review cass mirror prune (dry-run first); protected evidence and hold-downs are never overridden."
            }));
        }
        Ok(_) => {}
        Err(error) => {
            eprintln!("{}", serde_json::json!({
                "raw_mirror_maintenance": {"status": "skipped", "reason": error.to_string()},
                "hint": "Indexing will still attempt its normal disk preflight. Review cass mirror prune and CASS_RAW_MIRROR_MAX_BYTES / CASS_RAW_MIRROR_MIN_FREE_BYTES; no protected evidence was intentionally discarded."
            }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn only_explicit_index_commands_run_automatic_retention() {
        for args in [&["cass", "health"][..], &["cass", "search", "probe"][..], &["cass"][..]] {
            assert!(index_data_dir(&Cli::try_parse_from(args).unwrap()).unwrap().is_none());
        }
        for args in [&["cass", "index"][..], &["cass", "index", "--watch"][..]] {
            assert!(index_data_dir(&Cli::try_parse_from(args).unwrap()).unwrap().is_some());
        }
    }

    #[test]
    fn explicit_data_directory_is_used_without_touching_the_default_archive() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("isolated");
        let cli = Cli::try_parse_from(["cass", "index", "--data-dir", directory.to_str().unwrap()]).unwrap();
        assert_eq!(index_data_dir(&cli).unwrap(), Some(directory.clone()));
        maintain(&cli);
        assert!(!directory.exists(), "an absent mirror needs no maintenance writes");
    }

    #[test]
    fn custom_database_name_cannot_read_pins_from_a_neighboring_archive() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("scratch.db");
        let cli = Cli::try_parse_from(["cass", "--db", path.to_str().unwrap(), "index"]).unwrap();
        assert!(index_data_dir(&cli).is_err());
        let path = temp.path().join("agent_search.db");
        let cli = Cli::try_parse_from(["cass", "--db", path.to_str().unwrap(), "index"]).unwrap();
        assert_eq!(index_data_dir(&cli).unwrap(), Some(temp.path().to_path_buf()));
    }
}
