//! Failure-safe reading and publication for both quarantine recovery ledgers.
//!
//! Keep writer coordination at the caller: an atomic snapshot is not a merge
//! of independently loaded states. This layer prevents shared scratch names,
//! partial serialization and failed publication from damaging the old file.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;

use serde::{Serialize, de::DeserializeOwned};
use tempfile::NamedTempFile;

const BUFFER_BYTES: usize = 8 * 1024;

/// Read one regular checkpoint through a pinned descriptor. A FIFO or device
/// must not stall indexing before JSON validation, especially while the caller
/// holds the shared connector-quarantine mutex. No-follow and nonblocking open
/// flags also cover a path replaced after the initial metadata check.
///
/// Stream JSON instead of retaining a second ledger-sized String. The opened
/// file's length bounds the read even if another process keeps appending; one
/// sentinel byte detects growth rather than accepting a truncated JSON prefix.
/// This is a file-read bound, not a limit on the decoded state's heap usage.
pub(super) fn read<T: DeserializeOwned>(path: &Path) -> io::Result<T> {
    read_inner(path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("cannot read quarantine state {}: {error}", path.display()),
        )
    })
}

fn read_inner<T: DeserializeOwned>(path: &Path) -> io::Result<T> {
    let observed = fs::symlink_metadata(path)?;
    if !observed.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "checkpoint must be a regular, non-symlink file",
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "opened checkpoint is not a regular file",
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "opened checkpoint is a reparse point",
            ));
        }
    }
    read_opened(file, metadata.len())
}

fn read_opened<T: DeserializeOwned>(file: File, admitted_bytes: u64) -> io::Result<T> {
    let limit = admitted_bytes.checked_add(1).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "checkpoint length exceeds read limit",
        )
    })?;
    let mut reader = BufReader::with_capacity(BUFFER_BYTES, file.take(limit));
    let state = serde_json::from_reader(&mut reader).map_err(|error| {
        let kind = error.io_error_kind().unwrap_or(io::ErrorKind::InvalidData);
        io::Error::new(kind, error)
    })?;
    if limit - reader.get_ref().limit() != admitted_bytes
        || reader.get_ref().get_ref().metadata()?.len() != admitted_bytes
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "checkpoint length changed during reading; retry the operation",
        ));
    }
    Ok(state)
}

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
    fn checkpoint_read_streams_complete_json_and_keeps_its_opened_file() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("checkpoint.json");
        let expected = json!({
            "generation": "old",
            "value": "x".repeat(BUFFER_BYTES * 3),
        });
        fs::write(&path, serde_json::to_vec(&expected)?)?;
        assert_eq!(read::<Value>(&path)?, expected);

        // Once admitted, replacing the pathname must not switch the reader
        // to another generation halfway through recovery.
        let file = File::open(&path)?;
        let admitted_bytes = file.metadata()?.len();
        fs::rename(&path, dir.path().join("retained-checkpoint.json"))?;
        fs::write(&path, br#"{"generation":"new"}"#)?;
        assert_eq!(read_opened::<Value>(file, admitted_bytes)?, expected);
        assert_eq!(read::<Value>(&path)?, json!({"generation": "new"}));
        Ok(())
    }

    #[test]
    fn checkpoint_read_refuses_growth_and_truncation_after_admission() -> io::Result<()> {
        for grow in [false, true] {
            let dir = tempfile::tempdir()?;
            let path = dir.path().join("checkpoint.json");
            fs::write(&path, br#"{"generation":"original complete checkpoint"}"#)?;
            let file = File::open(&path)?;
            let admitted_bytes = file.metadata()?.len();
            if grow {
                // Trailing whitespace remains valid JSON. Only checking the
                // parser result would incorrectly accept the changed file.
                OpenOptions::new()
                    .append(true)
                    .open(&path)?
                    .write_all(&vec![b' '; BUFFER_BYTES * 4])?;
            } else {
                // A shorter, complete document is also a valid JSON value.
                fs::write(&path, br#"{"generation":"short"}"#)?;
            }
            let before = fs::read(&path)?;
            let error = read_opened::<Value>(file, admitted_bytes).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(error.to_string().contains("length changed"));
            assert_eq!(fs::read(&path)?, before);
        }
        Ok(())
    }

    #[test]
    fn malformed_checkpoints_are_preserved_by_both_operator_paths() -> io::Result<()> {
        for malformed in [
            b"".as_slice(),
            b"not JSON",
            br#"{"entries":[]}"#,
            br#"{"entries":{}} trailing"#,
            br#"{"entries":{}} {}"#,
            &[0xff],
        ] {
            let dir = tempfile::tempdir()?;
            let session_path = QuarantineState::path(dir.path());
            let quarantine_dir = dir.path().join("quarantine");
            fs::create_dir(&quarantine_dir)?;
            let line_path = quarantine_dir.join("connector_ingest_lines.json");
            fs::write(&session_path, malformed)?;
            fs::write(&line_path, malformed)?;

            // The indexer keeps its documented best-effort fallback, but
            // inspection/mutation must preserve the existing recovery ledger.
            assert!(QuarantineState::load(dir.path()).is_empty());
            let error = QuarantineState::load_for_operator(dir.path()).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            let error = record_connector_line(
                dir.path(),
                "codebuff",
                &dir.path().join("source.jsonl"),
                1,
                b"private malformed line",
                "parse",
            )
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert_eq!(fs::read(&session_path)?, malformed);
            assert_eq!(fs::read(&line_path)?, malformed);
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn nonregular_checkpoint_reads_refuse_without_blocking() -> io::Result<()> {
        use std::os::unix::fs::symlink;
        use std::os::unix::net::UnixListener;
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};

        const CHILD_DIRECTORY: &str = "CASS_QUARANTINE_READ_TEST_DIRECTORY";
        if let Some(directory) = std::env::var_os(CHILD_DIRECTORY) {
            let directory = std::path::PathBuf::from(directory);
            let session_path = QuarantineState::path(&directory);
            let line_path = directory.join("quarantine/connector_ingest_lines.json");
            let session_type = fs::symlink_metadata(&session_path)?.file_type();
            let line_type = fs::symlink_metadata(&line_path)?.file_type();

            assert!(QuarantineState::load(&directory).is_empty());
            let error = QuarantineState::load_for_operator(&directory).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            let error = record_connector_line(
                &directory,
                "codebuff",
                &directory.join("source.jsonl"),
                1,
                b"private malformed line",
                "parse",
            )
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert_eq!(fs::symlink_metadata(&session_path)?.file_type(), session_type);
            assert_eq!(fs::symlink_metadata(&line_path)?.file_type(), line_type);
            assert_eq!(
                read::<Value>(Path::new("/dev/zero")).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );

            // The failed connector read must release the shared mutex so a
            // different provider can still record and checkpoint its input.
            record_connector_line(
                &directory.join("healthy"),
                "gemini",
                &directory.join("other-source.jsonl"),
                2,
                b"another malformed line",
                "parse",
            )?;
            fs::write(directory.join("probe-finished"), b"checked")?;
            return Ok(());
        }

        let module = module_path!().split_once("::").unwrap().1;
        let test_name = format!("{module}::nonregular_checkpoint_reads_refuse_without_blocking");
        for kind in ["fifo", "socket", "directory", "symlink", "dangling_symlink"] {
            let dir = tempfile::tempdir()?;
            fs::create_dir(dir.path().join("quarantine"))?;
            let target = dir.path().join("retained-target.json");
            let target_bytes = serde_json::to_vec(&state("retained"))?;
            fs::write(&target, &target_bytes)?;
            let mut sockets = Vec::new();
            for path in [
                QuarantineState::path(dir.path()),
                dir.path().join("quarantine/connector_ingest_lines.json"),
            ] {
                match kind {
                    "fifo" => assert!(Command::new("mkfifo").arg(&path).status()?.success()),
                    "socket" => sockets.push(UnixListener::bind(&path)?),
                    "directory" => fs::create_dir(&path)?,
                    "symlink" => symlink(&target, &path)?,
                    "dangling_symlink" => symlink(dir.path().join("absent"), &path)?,
                    _ => unreachable!(),
                }
            }

            // Run the real public callers in a child: a FIFO regression must
            // fail within a deadline instead of wedging the whole test suite.
            let mut child = Command::new(std::env::current_exe()?)
                .arg(&test_name)
                .args(["--exact", "--nocapture", "--test-threads=1"])
                .env(CHILD_DIRECTORY, dir.path())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()?;
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if let Some(status) = child.try_wait()? {
                    assert!(status.success(), "{kind} checkpoint probe failed: {status}");
                    break;
                }
                if Instant::now() >= deadline {
                    child.kill()?;
                    child.wait()?;
                    panic!("{kind} checkpoint read blocked past the deadline");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            // Also prove the exact child test ran; zero matched tests is not
            // evidence that a nonregular checkpoint was rejected.
            assert_eq!(fs::read(dir.path().join("probe-finished"))?, b"checked");
            assert_eq!(fs::read(&target)?, target_bytes);
        }
        Ok(())
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
