//! Failure-safe publication for both quarantine recovery ledgers.
//!
//! Keep writer coordination at the caller: an atomic snapshot is not a merge
//! of independently loaded states. This layer prevents shared scratch names,
//! partial serialization and failed publication from damaging the old file.

use std::io::{self, BufWriter, Write};
use std::path::Path;

use serde::Serialize;
use tempfile::NamedTempFile;

const BUFFER_BYTES: usize = 8 * 1024;

/// The parent directory must already exist. Encode directly into a bounded
/// writer instead of allocating a second, ledger-sized JSON String/Vec while
/// the caller may already be recovering from an allocation failure.
///
/// File data is synced before atomic replacement. On Unix, also sync the
/// containing directory after publication. A directory-sync error is reported
/// as a published-but-not-confirmed update; it cannot roll back the rename.
/// Other platforms retain atomic replacement and file sync, without claiming
/// directory durability. Neither this nor fsync guarantees faulty hardware,
/// network filesystems or newly created ancestor directories survive power loss.
pub(super) fn save(path: &Path, state: &impl Serialize) -> io::Result<()> {
    Pending::prepare(path, |writer| {
        serde_json::to_writer_pretty(writer, state).map_err(io::Error::other)
    })?
    .publish(path)
}

struct Pending {
    file: NamedTempFile,
    #[cfg(unix)]
    directory: std::fs::File,
}

impl Pending {
    fn prepare(
        path: &Path,
        encode: impl FnOnce(&mut dyn Write) -> io::Result<()>,
    ) -> io::Result<Self> {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        // Open the sync handle before publication, not after the old name has
        // been replaced. The temp file stays in this same filesystem.
        #[cfg(unix)]
        let directory = std::fs::File::open(parent)?;
        // create_new on an unpredictable name: never truncate a leftover
        // shared .tmp file or follow a pre-existing .tmp symlink/hard link.
        // On Unix tempfile also restricts the checkpoint to the current user.
        let mut file = tempfile::Builder::new()
            .prefix(".cass-quarantine-")
            .tempfile_in(parent)?;
        {
            let mut writer = BufWriter::with_capacity(BUFFER_BYTES, file.as_file_mut());
            encode(&mut writer)?;
            writer.flush()?;
        }
        file.as_file().sync_all()?;
        Ok(Self {
            file,
            #[cfg(unix)]
            directory,
        })
    }

    fn publish(self, path: &Path) -> io::Result<()> {
        // No remove-then-rename gap. Persist errors retain the old destination;
        // dropping their owned temporary file cleans up only this attempt.
        self.file.persist(path).map_err(|error| error.error)?;
        #[cfg(unix)]
        self.directory.sync_all().map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("quarantine checkpoint published but directory sync failed: {error}"),
            )
        })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::{QuarantineKey, QuarantineState, record_connector_line};
    use super::*;
    use serde::ser::{SerializeSeq, Serializer};
    use serde_json::{Value, json};
    use std::fs;

    fn state(name: &str) -> QuarantineState {
        let mut state = QuarantineState::default();
        state.record_attempt(
            &QuarantineKey::new(name, 1),
            "ingest_oom",
            chrono::DateTime::from_timestamp(1_774_113_351, 0).unwrap(),
        );
        state
    }

    #[test]
    fn independent_preparations_cannot_publish_each_others_scratch() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("checkpoint.json");
        save(&path, &json!({"generation": "old"}))?;
        let prepare = |generation: &str| {
            Pending::prepare(&path, |writer| {
                serde_json::to_writer_pretty(writer, &json!({"generation": generation}))
                    .map_err(io::Error::other)
            })
        };
        // Both are prepared before either rename: with a shared .tmp, the
        // first publication would install the second writer's bytes and the
        // second would find no file. No timing-dependent threads are needed.
        let first = prepare("first")?;
        let second = prepare("second")?;
        assert_ne!(first.file.path(), second.file.path());
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(&path)?)?["generation"],
            "old"
        );
        first.publish(&path)?;
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(&path)?)?["generation"],
            "first"
        );
        second.publish(&path)?;
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(&path)?)?["generation"],
            "second"
        );
        assert_eq!(fs::read_dir(dir.path())?.count(), 1);
        Ok(())
    }

    #[test]
    fn partial_write_failure_keeps_old_checkpoint_and_cleans_only_own_temp() -> io::Result<()> {
        for bytes in [0, 1, BUFFER_BYTES - 1, BUFFER_BYTES, BUFFER_BYTES * 4] {
            let dir = tempfile::tempdir()?;
            let path = dir.path().join("checkpoint.json");
            let old = br#"{"generation":"recoverable"}"#;
            fs::write(&path, old)?;
            let retained = dir.path().join("checkpoint.json.tmp");
            fs::write(&retained, b"unrelated retained evidence")?;
            let result = Pending::prepare(&path, |writer| {
                writer.write_all(&vec![b'x'; bytes])?;
                assert_eq!(fs::read(&path)?, old);
                Err(io::Error::other(
                    "injected write failure before publication",
                ))
            });
            assert!(result.is_err());
            assert_eq!(fs::read(&path)?, old);
            assert_eq!(fs::read(&retained)?, b"unrelated retained evidence");
            assert_eq!(fs::read_dir(dir.path())?.count(), 2);
        }
        Ok(())
    }

    struct BrokenSerialization;

    impl Serialize for BrokenSerialization {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            let mut sequence = serializer.serialize_seq(Some(2))?;
            // Force bytes through the buffer before the serializer fails.
            sequence.serialize_element(&"x".repeat(BUFFER_BYTES * 2))?;
            Err(serde::ser::Error::custom("injected serialization failure"))
        }
    }

    #[test]
    fn failed_json_encoding_never_replaces_a_recovery_ledger() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let original = state("previous");
        original.save(dir.path())?;
        let path = QuarantineState::path(dir.path());
        let before = fs::read(&path)?;
        let error = save(&path, &BrokenSerialization).unwrap_err();
        assert!(error.to_string().contains("injected serialization failure"));
        assert_eq!(fs::read(&path)?, before);
        assert_eq!(
            QuarantineState::load_for_operator(dir.path())?.entries,
            original.entries
        );
        assert_eq!(fs::read_dir(dir.path())?.count(), 1);
        Ok(())
    }

    #[test]
    fn failed_rename_preserves_destination_and_retained_evidence() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("checkpoint.json");
        fs::create_dir(&path)?;
        let evidence = path.join("keep");
        fs::write(&evidence, b"existing evidence")?;
        let pending = Pending::prepare(&path, |writer| writer.write_all(b"{}"))?;
        assert!(pending.publish(&path).is_err());
        assert!(path.is_dir());
        assert_eq!(fs::read(&evidence)?, b"existing evidence");
        assert_eq!(fs::read_dir(dir.path())?.count(), 1);
        Ok(())
    }

    #[test]
    fn both_real_ledgers_replace_stale_temp_files_without_touching_them() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let stale = dir
            .path()
            .join(format!("{}.tmp", QuarantineState::FILENAME));
        fs::write(&stale, b"retained session-checkpoint evidence")?;
        let first = state("first");
        first.save(dir.path())?;
        let second = state("second");
        second.save(dir.path())?;
        assert_eq!(
            QuarantineState::load_for_operator(dir.path())?.entries,
            second.entries
        );
        assert_eq!(fs::read(&stale)?, b"retained session-checkpoint evidence");
        assert_eq!(
            fs::read(QuarantineState::path(dir.path()))?,
            serde_json::to_vec_pretty(&second)?
        );

        let quarantine = dir.path().join("quarantine");
        fs::create_dir(&quarantine)?;
        let stale = quarantine.join("connector_ingest_lines.json.tmp");
        fs::write(&stale, b"retained line-checkpoint evidence")?;
        let source = dir.path().join("chat-messages.json");
        let payload = b"private malformed transcript sentinel";
        fs::write(&source, payload)?;
        record_connector_line(dir.path(), "codebuff", &source, 4, payload, "parse")?;
        record_connector_line(dir.path(), "codebuff", &source, 4, payload, "parse")?;
        record_connector_line(dir.path(), "gemini", &source, 5, payload, "parse")?;
        let bytes = fs::read(quarantine.join("connector_ingest_lines.json"))?;
        let value: Value = serde_json::from_slice(&bytes)?;
        let entries = value["entries"].as_object().unwrap();
        assert_eq!(entries.len(), 2);
        let record = entries
            .values()
            .find(|record| record["provider"] == "codebuff")
            .unwrap();
        assert_eq!(record["attempt_count"], 2);
        assert_eq!(
            record["payload_blake3"],
            blake3::hash(payload).to_hex().to_string()
        );
        assert!(!bytes.windows(payload.len()).any(|window| window == payload));
        assert_eq!(fs::read(&source)?, payload);
        assert_eq!(fs::read(&stale)?, b"retained line-checkpoint evidence");
        assert_eq!(fs::read_dir(&quarantine)?.count(), 2);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn old_temp_aliases_never_truncate_session_sources_in_either_ledger() -> io::Result<()> {
        use std::os::unix::fs::{PermissionsExt, symlink};

        for hard_link in [false, true] {
            let dir = tempfile::tempdir()?;
            let source = dir.path().join("chat-messages.json");
            let payload = b"original private source bytes";
            fs::write(&source, payload)?;
            let before = fs::metadata(&source)?.modified()?;
            let alias = |link: &Path| {
                if hard_link {
                    fs::hard_link(&source, link)
                } else {
                    symlink(&source, link)
                }
            };
            alias(
                &dir.path()
                    .join(format!("{}.tmp", QuarantineState::FILENAME)),
            )?;
            state("healthy").save(dir.path())?;
            let quarantine = dir.path().join("quarantine");
            fs::create_dir(&quarantine)?;
            alias(&quarantine.join("connector_ingest_lines.json.tmp"))?;
            record_connector_line(dir.path(), "codebuff", &source, 1, payload, "parse")?;
            assert_eq!(fs::read(&source)?, payload);
            assert_eq!(fs::metadata(&source)?.modified()?, before);
            assert_eq!(QuarantineState::load_for_operator(dir.path())?.len(), 1);
            for path in [
                QuarantineState::path(dir.path()),
                quarantine.join("connector_ingest_lines.json"),
            ] {
                assert_eq!(fs::metadata(path)?.permissions().mode() & 0o077, 0);
            }
        }
        Ok(())
    }
}
