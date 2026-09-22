//! Idempotent reimport is a comparison, not permission to replace or merge.
//! The caller holds the destination lock and has admitted the input header.

use std::io::{self, BufRead};
use std::path::Path;

use anyhow::{Result, anyhow, ensure};
use coding_agent_search::franken_sync::{Connection, FileIdentity};

use super::codec::{self, Completion, Header, Validator};
use super::{export, import};

fn identity(path: &Path) -> Result<FileIdentity> {
    // The same no-follow regular-file admission as the interchange input.
    // Identity comes from the opened descriptor, not path-derived metadata.
    let file = import::open_input(path)?;
    FileIdentity::from_file(&file)?
        .ok_or_else(|| anyhow!("cannot prove the existing destination's file identity"))
}

fn require_same_file(connection: &Connection, path: &Path) -> Result<()> {
    let actual = identity(path)?;
    ensure!(
        connection.file_identity()? == Some(actual),
        "restore destination changed during comparison; retry without replacing it"
    );
    Ok(())
}

pub(super) fn verify_existing(
    input: &mut impl BufRead,
    header: Header,
    destination: &Path,
) -> Result<(Header, Completion)> {
    // Verify all input, including EOF and the completion digest, before even
    // opening the existing database. A matching header or prefix is no proof.
    let mut validator = Validator::new(header)?;
    let mut line = 2u64;
    while let Some(record) = codec::read_record(input, line)? {
        validator
            .push(&record)
            .map_err(|error| anyhow!("record {line}: {error}"))?;
        line = line
            .checked_add(1)
            .ok_or_else(|| anyhow!("logical record position overflow"))?;
    }
    let expected = validator.finish()?;
    let admitted = identity(destination)?;
    let reader = export::open_source(destination)?;
    ensure!(
        reader.file_identity()? == Some(admitted),
        "restore destination changed before comparison; nothing was replaced"
    );
    let actual = export::snapshot(&reader, expected.0.archive_id.clone(), &mut io::sink())?;
    ensure!(
        actual.1 == expected.1,
        "restore conflict: existing canonical data differs from the verified input; nothing was replaced"
    );
    import::verify_database(&reader)?;
    require_same_file(&reader, destination)?;
    reader.execute("ROLLBACK")?;
    reader.close_without_checkpoint()?;
    // Equality describes one pinned read snapshot, not a lease preventing other
    // writers after this check. No import receipt is written into the archive.
    Ok(expected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;

    use coding_agent_search::franken_sync::compat::RowExt;
    use coding_agent_search::storage::sqlite::SqliteStorage;

    fn database_files(path: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        ["", "-wal", "-shm", "-journal"]
            .into_iter()
            .filter_map(|suffix| {
                let mut name = path.as_os_str().to_os_string();
                name.push(suffix);
                let path = PathBuf::from(name);
                match fs::read(&path) {
                    Ok(bytes) => Some((path, bytes)),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                    Err(error) => panic!("cannot inspect test database: {error}"),
                }
            })
            .collect()
    }

    fn fixture(root: &Path) -> (PathBuf, PathBuf) {
        let source = root.join("source.db");
        drop(SqliteStorage::open(&source).unwrap());
        let input = root.join("history.jsonl");
        export::export_file(&source, &input, "reimport-test".to_owned()).unwrap();
        let destination = root.join("restored.db");
        import::import_file(&input, &destination, "reimport-test").unwrap();
        (input, destination)
    }

    #[test]
    fn repeated_identical_import_is_read_only_and_reports_unchanged() {
        let root = tempfile::tempdir().unwrap();
        let (input, destination) = fixture(root.path());
        let before = database_files(&destination);
        let receipt =
            import::import_file_with_policy(&input, &destination, "reimport-test", true).unwrap();
        assert!(!receipt.2);
        assert_eq!(receipt.1, export::verify_file(&input).unwrap().1);
        assert_eq!(before, database_files(&destination));
        // Default behavior stays fail-closed; no opt-in means no existing target.
        assert!(import::import_file(&input, &destination, "reimport-test").is_err());
        assert_eq!(before, database_files(&destination));
    }

    #[test]
    fn modified_canonical_content_is_a_conflict_not_an_upsert() {
        let root = tempfile::tempdir().unwrap();
        let (input, destination) = fixture(root.path());
        let writer = Connection::open(export::path_text(&destination).unwrap()).unwrap();
        writer
            .execute("INSERT INTO meta (key, value) VALUES ('operator_note', 'keep me')")
            .unwrap();
        writer.close().unwrap();
        let before = database_files(&destination);
        let error = import::import_file_with_policy(&input, &destination, "reimport-test", true)
            .unwrap_err();
        assert!(error.to_string().contains("restore conflict"));
        assert_eq!(before, database_files(&destination));
        let reader = export::open_source(&destination).unwrap();
        assert_eq!(
            reader
                .query_row("SELECT value FROM meta WHERE key = 'operator_note'")
                .unwrap()
                .get_typed::<String>(0)
                .unwrap(),
            "keep me"
        );
        reader.execute("ROLLBACK").unwrap();
        reader.close_without_checkpoint().unwrap();
    }

    #[test]
    fn matching_prefix_and_archive_id_do_not_excuse_a_truncated_input() {
        let root = tempfile::tempdir().unwrap();
        let (input, destination) = fixture(root.path());
        let mut bytes = fs::read(&input).unwrap();
        bytes.pop(); // Completion JSON is present but the mandatory newline is not.
        fs::write(&input, bytes).unwrap();
        let before = database_files(&destination);
        assert!(
            import::import_file_with_policy(&input, &destination, "reimport-test", true).is_err()
        );
        assert_eq!(before, database_files(&destination));
    }

    #[test]
    fn opt_in_still_creates_a_new_destination_through_the_verified_restore() {
        let root = tempfile::tempdir().unwrap();
        let (input, _) = fixture(root.path());
        let destination = root.path().join("second.db");
        let receipt =
            import::import_file_with_policy(&input, &destination, "reimport-test", true).unwrap();
        assert!(receipt.2);
        assert!(destination.is_file());
        assert_eq!(receipt.1, export::verify_file(&input).unwrap().1);
    }

    #[test]
    fn caller_identity_is_checked_even_when_destination_content_matches() {
        let root = tempfile::tempdir().unwrap();
        let (input, destination) = fixture(root.path());
        let before = database_files(&destination);
        assert!(
            import::import_file_with_policy(&input, &destination, "other-archive", true).is_err()
        );
        assert_eq!(before, database_files(&destination));
    }

    #[cfg(unix)]
    #[test]
    fn file_identity_is_descriptor_bound_and_links_are_refused() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let (input, destination) = fixture(root.path());
        let twin = root.path().join("twin.db");
        fs::copy(&destination, &twin).unwrap();
        assert_ne!(identity(&destination).unwrap(), identity(&twin).unwrap());
        let reader = export::open_source(&destination).unwrap();
        assert!(require_same_file(&reader, &destination).is_ok());
        assert!(require_same_file(&reader, &twin).is_err());
        reader.execute("ROLLBACK").unwrap();
        reader.close_without_checkpoint().unwrap();
        let link = root.path().join("linked.db");
        symlink(&destination, &link).unwrap();
        assert!(import::import_file_with_policy(&input, &link, "reimport-test", true).is_err());
    }
}
