//! Exact-schema delegation must retain both the admitted file and its receipt.

use super::*;

const ARCHIVE_ID: &str = "pinned-restore-fixture";

fn backup(root: &Path, name: &str, note: &str) -> Result<(PathBuf, (Header, Completion))> {
    let database = root.join(format!("{name}.db"));
    drop(SqliteStorage::open(&database)?);
    let writer = Connection::open(export::path_text(&database)?)?;
    writer.execute_with_params(
        "INSERT INTO meta (key, value) VALUES ('pinned_note', ?)",
        &[SqliteValue::Text(note.into())],
    )?;
    writer.close()?;
    let path = root.join(format!("{name}.jsonl"));
    let receipt = export::export_file(&database, &path, ARCHIVE_ID.into())?;
    Ok((path, receipt))
}

fn note(path: &Path) -> Result<String> {
    let reader = export::open_source(path)?;
    let value = reader
        .query_row("SELECT value FROM meta WHERE key = 'pinned_note'")?
        .get_typed::<String>(0)?;
    reader.execute("ROLLBACK")?;
    reader.close_without_checkpoint()?;
    Ok(value)
}

fn assert_integrity(error: anyhow::Error) {
    assert_eq!(
        super::super::classify_failure(&error),
        (5, "logical-archive-integrity", false)
    );
    assert!(!error.to_string().contains("PRIVATE-"));
}

/// Give a second valid archive exactly the first archive's header. Export time
/// is not in the canonical digest, so this still has a valid, different footer.
fn same_header_bytes(path: &Path, header: &Header) -> Result<Vec<u8>> {
    let bytes = fs::read(path)?;
    let end = bytes.iter().position(|byte| *byte == b'\n').unwrap() + 1;
    let mut changed = codec::encode(&Record::Header {
        header: header.clone(),
    })?;
    changed.extend_from_slice(&bytes[end..]);
    assert_eq!(
        codec::verify(&mut std::io::Cursor::new(&changed))?.0,
        *header
    );
    Ok(changed)
}

#[cfg(unix)]
#[test]
fn pathname_replacement_does_not_substitute_the_inspected_restore() -> Result<()> {
    let root = tempfile::tempdir()?;
    let (input, expected) = backup(root.path(), "original", "PRIVATE-ORIGINAL")?;
    let (replacement, _) = backup(root.path(), "replacement", "PRIVATE-REPLACEMENT")?;
    let original_bytes = fs::read(&input)?;
    let mut pinned = open_input(&input)?;
    // Leave the same descriptor at EOF, just as migration inspection does.
    assert_eq!(codec::verify(&mut BufReader::new(&mut pinned))?, expected);
    let retired = root.path().join("retired.jsonl");
    fs::rename(&input, &retired)?;
    fs::copy(&replacement, &input)?;
    let destination = root.path().join("restored.db");
    let result = import_inspected_file(pinned, &destination, &expected.0, &expected.1, false)?;
    assert!(result.2);
    assert_eq!((result.0, result.1), expected);
    assert_eq!(note(&destination)?, "PRIVATE-ORIGINAL");
    assert_eq!(fs::read(&retired)?, original_bytes);
    assert_eq!(fs::read(&input)?, fs::read(&replacement)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn pinned_reimport_compares_the_original_file_without_reopening_its_name() -> Result<()> {
    let root = tempfile::tempdir()?;
    let (input, expected) = backup(root.path(), "original", "PRIVATE-ORIGINAL")?;
    let (replacement, _) = backup(root.path(), "replacement", "PRIVATE-REPLACEMENT")?;
    let destination = root.path().join("restored.db");
    import_file(&input, &destination, ARCHIVE_ID)?;
    let before = fs::read(&destination)?;
    let mut pinned = open_input(&input)?;
    assert_eq!(codec::verify(&mut BufReader::new(&mut pinned))?, expected);
    fs::rename(&input, root.path().join("retired.jsonl"))?;
    fs::copy(&replacement, &input)?;
    let result = import_inspected_file(pinned, &destination, &expected.0, &expected.1, true)?;
    assert!(!result.2);
    assert_eq!((result.0, result.1), expected);
    assert_eq!(fs::read(&destination)?, before);
    Ok(())
}

#[test]
fn a_valid_in_place_rewrite_cannot_publish_under_the_previous_inspection() -> Result<()> {
    let root = tempfile::tempdir()?;
    let (input, expected) = backup(root.path(), "original", "PRIVATE-ORIGINAL")?;
    let (replacement, _) = backup(root.path(), "replacement", "PRIVATE-REPLACEMENT")?;
    let mut pinned = open_input(&input)?;
    assert_eq!(codec::verify(&mut BufReader::new(&mut pinned))?, expected);
    let changed = same_header_bytes(&replacement, &expected.0)?;
    let actual = codec::verify(&mut std::io::Cursor::new(&changed))?;
    assert_eq!(actual.0, expected.0);
    assert_ne!(actual.1, expected.1);
    // This updates the admitted inode, not just the name. Pinning alone is
    // insufficient: the receipt check must happen BEFORE publication.
    fs::write(&input, changed)?;
    let destination = root.path().join("must-not-publish.db");
    assert_integrity(
        import_inspected_file(pinned, &destination, &expected.0, &expected.1, false)
            .unwrap_err(),
    );
    assert!(!destination.exists());
    for sidecar in sidecars(&destination) {
        assert!(!sidecar.exists());
    }
    Ok(())
}

#[test]
fn a_matching_replacement_destination_cannot_be_reported_as_the_inspected_archive() -> Result<()> {
    let root = tempfile::tempdir()?;
    let (input, expected) = backup(root.path(), "original", "PRIVATE-ORIGINAL")?;
    let (replacement, _) = backup(root.path(), "replacement", "PRIVATE-REPLACEMENT")?;
    let destination = root.path().join("replacement.db.copy");
    import_file(&replacement, &destination, ARCHIVE_ID)?;
    let before = fs::read(&destination)?;
    let mut pinned = open_input(&input)?;
    assert_eq!(codec::verify(&mut BufReader::new(&mut pinned))?, expected);
    fs::write(&input, same_header_bytes(&replacement, &expected.0)?)?;
    // Input B is valid and matches destination B. It still is not inspected A.
    assert_integrity(
        import_inspected_file(pinned, &destination, &expected.0, &expected.1, true)
            .unwrap_err(),
    );
    assert_eq!(fs::read(&destination)?, before);
    assert_eq!(note(&destination)?, "PRIVATE-REPLACEMENT");
    Ok(())
}

#[test]
fn changed_header_fails_before_any_restore_destination_state_is_created() -> Result<()> {
    let root = tempfile::tempdir()?;
    let (input, expected) = backup(root.path(), "original", "PRIVATE-ORIGINAL")?;
    let mut altered = expected.0.clone();
    altered.exported_at_ms += 1;
    fs::write(&input, same_header_bytes(&input, &altered)?)?;
    let destination = root.path().join("must-not-create.db");
    let entries = || -> Result<Vec<PathBuf>> {
        let mut paths = fs::read_dir(root.path())?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<io::Result<Vec<_>>>()?;
        paths.sort();
        Ok(paths)
    };
    let before = entries()?;
    assert_integrity(
        import_inspected_file(open_input(&input)?, &destination, &expected.0, &expected.1, false)
            .unwrap_err(),
    );
    assert_eq!(entries()?, before, "even destination lock/staging creation must be deferred");
    assert!(!destination.exists());
    Ok(())
}

#[test]
fn unchanged_current_schema_still_delegates_to_exact_restore_and_reimport() -> Result<()> {
    let root = tempfile::tempdir()?;
    let (input, expected) = backup(root.path(), "original", "PRIVATE-ORIGINAL")?;
    let destination = root.path().join("restored.db");
    let created = super::super::migrate::import_compatible(
        &input, &destination, ARCHIVE_ID, false,
    )?;
    assert!(created.created);
    assert!(created.migration.is_none());
    assert_eq!((created.header, created.completion), expected);
    let before = fs::read(&destination)?;
    let repeated = super::super::migrate::import_compatible(
        &input, &destination, ARCHIVE_ID, true,
    )?;
    assert!(!repeated.created);
    assert!(repeated.migration.is_none());
    assert_eq!((repeated.header, repeated.completion), expected);
    assert_eq!(fs::read(&destination)?, before);
    Ok(())
}
