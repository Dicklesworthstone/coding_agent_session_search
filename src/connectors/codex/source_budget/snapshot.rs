//! Keep both Codex parsing passes tied to one observed source generation.
//!
//! This is an observable-change guard, not an atomic filesystem snapshot or
//! protection against in-place edits that preserve all checked metadata.

use std::cell::RefCell;
use std::fs::{self, File, Metadata};
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub(super) struct SourceSnapshot {
    file: File,
    before: Metadata,
}

struct CompleteSourceObservation {
    path: PathBuf,
    metadata: Metadata,
}

thread_local! {
    /// This evidence exists only while the guarded Codex connector delivers
    /// one conversation. A file name or caller-supplied provider label alone
    /// cannot authorize the indexer's post-parse active-source exemption.
    static COMPLETE_CODEX_DELIVERY: RefCell<Option<CompleteSourceObservation>> = const {
        RefCell::new(None)
    };
}

pub(super) struct CompleteSourceDeliveryGuard {
    previous: Option<CompleteSourceObservation>,
    _thread_bound: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl Drop for CompleteSourceDeliveryGuard {
    fn drop(&mut self) {
        COMPLETE_CODEX_DELIVERY.with(|current| {
            *current.borrow_mut() = self.previous.take();
        });
    }
}

fn plain_rollout(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension == "jsonl")
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("rollout-"))
}

/// Pre-parse eligibility, not evidence that parsing completed. The actual
/// connector must still validate its bounded whole-file read before delivery.
pub(crate) fn can_attempt_active_jsonl_source(path: &Path) -> bool {
    if !plain_rollout(path) {
        return false;
    }
    let Ok(limit) = super::source_read_budget(path) else {
        return false;
    };
    can_attempt_active_jsonl_source_with_limit(path, limit)
}

fn can_attempt_active_jsonl_source_with_limit(path: &Path, limit: u64) -> bool {
    let Ok(file) = open_active_source(path) else {
        return false;
    };
    let Ok(before) = file.metadata() else {
        return false;
    };
    if !before.is_file() || before.len() > limit {
        return false;
    }
    SourceSnapshot { file, before }
        .is_complete_unlocked_jsonl(path)
        .unwrap_or(false)
}

fn open_active_source(path: &Path) -> io::Result<File> {
    if !plain_rollout(path) || !fs::symlink_metadata(path)?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "active Codex admission requires a regular plain rollout",
        ));
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Replacing a discovered file with a FIFO must not block the watcher;
        // replacing it with a symlink must not widen this narrow admission.
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    options.open(path)
}

/// Match only the source generation the real Codex parser just delivered.
/// Reopening here also preserves advisory-lock deferral if an idle writable
/// descriptor caused the writer census to return before checking locks.
pub(crate) fn guarded_active_jsonl_source(path: &Path) -> bool {
    COMPLETE_CODEX_DELIVERY.with(|current| {
        let current = current.borrow();
        let Some(observation) = current.as_ref().filter(|value| value.path == path) else {
            return false;
        };
        let Ok(file) = open_active_source(path) else {
            return false;
        };
        let Ok(metadata) = file.metadata() else {
            return false;
        };
        super::super::same_rollout_snapshot(&observation.metadata, &metadata).unwrap_or(false)
            && source_is_unlocked(&file)
    })
}

fn source_is_unlocked(file: &File) -> bool {
    // A failed lock attempt cannot establish safe admission, even when the
    // platform reports a different error from WouldBlock.
    if file.try_lock().is_err() {
        return false;
    }
    file.unlock().is_ok()
}

/// Open a regular source without waiting for a peer on a substituted FIFO.
///
/// Path metadata cannot protect a later open from replacement. On Unix, apply
/// nonblocking mode to the actual open, then validate the opened object before
/// any reads. Ordinary snapshot reads deliberately keep following symlinks to
/// regular files; the stricter active-source exemption has its own opener.
pub(super) fn open_regular_source(path: &Path) -> io::Result<(File, Metadata)> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Codex source snapshot is not a regular file",
        ));
    }
    Ok((file, metadata))
}

impl SourceSnapshot {
    #[cfg(test)]
    pub(super) fn capture(path: &Path) -> Result<Self> {
        Self::capture_with_limit(path, super::MAX_AUGMENT_ROLLOUT_BYTES)
    }

    pub(super) fn capture_with_limit(path: &Path, limit: u64) -> Result<Self> {
        let (file, before) = open_regular_source(path)
            .with_context(|| format!("open Codex source snapshot {path:?}"))?;
        let observed = super::text_len(path, &file, before.len());
        if observed > limit {
            return Err(super::EnrichmentBudgetExceeded {
                observed_bytes: observed,
            }
            .into());
        }
        Ok(Self { file, before })
    }

    pub(super) fn len(&self) -> u64 {
        self.before.len()
    }

    fn is_complete_unlocked_jsonl(&self, path: &Path) -> Result<bool> {
        if !plain_rollout(path) || self.before.len() == 0 || !fs::symlink_metadata(path)?.is_file()
        {
            return Ok(false);
        }
        self.validate(path)?;
        if !source_is_unlocked(&self.file) {
            return Ok(false);
        }
        let mut file = &self.file;
        file.seek(SeekFrom::Start(self.before.len() - 1))
            .context("seek to admitted Codex record boundary")?;
        let mut last = [0_u8; 1];
        file.read_exact(&mut last)
            .context("read admitted Codex record boundary")?;
        // A newline appended after the parser's snapshot, or a replacement at
        // the same path, cannot turn an incomplete read into a complete one.
        self.validate(path)?;
        Ok(last == *b"\n")
    }

    pub(super) fn guard_complete_delivery(
        &self,
        path: &Path,
    ) -> Result<CompleteSourceDeliveryGuard> {
        let observation =
            self.is_complete_unlocked_jsonl(path)?
                .then(|| CompleteSourceObservation {
                    path: path.to_path_buf(),
                    metadata: self.before.clone(),
                });
        let previous = COMPLETE_CODEX_DELIVERY.with(|current| current.replace(observation));
        Ok(CompleteSourceDeliveryGuard {
            previous,
            _thread_bound: std::marker::PhantomData,
        })
    }

    /// Enrich the complete admitted prefix using the existing CASS parser.
    /// No pathname reopen, temporary source copy, truncation or second model of
    /// Codex messages. The spanning snapshot guard still surrounds both passes.
    pub(super) fn enrich(
        &self,
        conversation: &mut super::NormalizedConversation,
        progress_tick: Option<&(dyn Fn() + Send + Sync)>,
    ) -> Result<()> {
        self.validate(&conversation.source_path)?;
        let mut file = &self.file;
        file.rewind().context("rewind admitted Codex source")?;
        let mut reader = BufReader::new(file.take(self.before.len()));
        let consumed =
            super::super::augment_modern_codex_reader(conversation, progress_tick, &mut reader)?;
        if consumed != self.before.len() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "admitted Codex source was truncated while enriching",
            )
            .into());
        }
        self.validate(&conversation.source_path)
    }

    pub(super) fn validate(&self, path: &Path) -> Result<()> {
        let opened = self
            .file
            .metadata()
            .context("recheck opened Codex source")?;
        let named = fs::metadata(path).with_context(|| format!("recheck Codex source {path:?}"))?;
        // Reuse the same full-resolution size/mtime and Unix object-identity
        // checks as enrichment. Keep the original handle alive until the next
        // source, so path replacement cannot discard our identity observation.
        if !super::super::same_rollout_snapshot(&self.before, &opened)?
            || !super::super::same_rollout_snapshot(&self.before, &named)?
        {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Codex source changed across parsing or enrichment; retry this source",
            )
            .into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use franken_agent_detection::{
        CodexConnector, Connector, DiscoveredSourceFile, ScanContext, ScanRoot, SourceCompletion,
        SourceScanHooks,
    };
    use std::io::Write;
    use std::path::PathBuf;

    use super::super::super::augment_modern_codex_messages;
    use super::super::{EnrichmentBudgetExceeded, MAX_AUGMENT_ROLLOUT_BYTES, scan};

    fn content(extension: &str, text: &str) -> String {
        let message = serde_json::json!({"role": "user", "content": text});
        let value = if extension == "json" {
            serde_json::json!({"items": [message]})
        } else {
            serde_json::json!({"type": "response_item", "payload": message})
        };
        format!("{value}\n")
    }

    fn fixture(extension: &str) -> Result<(tempfile::TempDir, PathBuf, ScanContext)> {
        let root = tempfile::tempdir()?;
        let path = root.path().join(format!("rollout-snapshot.{extension}"));
        fs::write(&path, content(extension, "original"))?;
        let ctx = ScanContext::with_roots(
            root.path().join("cass"),
            vec![ScanRoot::local(path.clone())],
            None,
        );
        Ok((root, path, ctx))
    }

    fn assert_kind(error: &anyhow::Error, expected: io::ErrorKind) {
        assert_eq!(
            error.downcast_ref::<io::Error>().map(io::Error::kind),
            Some(expected),
            "{error:#}"
        );
    }

    #[test]
    fn active_candidate_requires_a_complete_bounded_regular_plain_rollout() -> Result<()> {
        let (root, path, _) = fixture("jsonl")?;
        let bytes = fs::read(&path)?;
        let length = u64::try_from(bytes.len())?;
        assert!(can_attempt_active_jsonl_source_with_limit(&path, length));
        assert!(!can_attempt_active_jsonl_source_with_limit(
            &path,
            length - 1
        ));
        assert!(!guarded_active_jsonl_source(&path));

        // A syntactically valid final record without its newline is still an
        // unfinished append for this narrow active-source admission policy.
        fs::write(&path, &bytes[..bytes.len() - 1])?;
        assert!(!can_attempt_active_jsonl_source(&path));
        fs::write(&path, b"")?;
        assert!(!can_attempt_active_jsonl_source(&path));
        fs::write(&path, &bytes)?;

        for name in [
            "session.jsonl",
            "rollout-snapshot.json",
            "rollout-snapshot.jsonl.zst",
        ] {
            let other = root.path().join(name);
            fs::write(&other, &bytes)?;
            assert!(!can_attempt_active_jsonl_source(&other), "{name}");
        }
        let directory = root.path().join("rollout-directory.jsonl");
        fs::create_dir(&directory)?;
        assert!(!can_attempt_active_jsonl_source(&directory));
        assert_eq!(fs::read(&path)?, bytes);
        Ok(())
    }

    #[test]
    fn held_writer_complete_delivery_is_bound_to_real_parser_source_and_thread() -> Result<()> {
        let (root, path, ctx) = fixture("jsonl")?;
        let foreign = root.path().join("rollout-foreign.jsonl");
        fs::copy(&path, &foreign)?;
        let writer = fs::OpenOptions::new().append(true).open(&path)?;
        let bytes = fs::read(&path)?;
        let modified = fs::metadata(&path)?.modified()?;
        let connector = crate::connectors::codex::CodexConnector::new();
        let mut delivered = 0;
        let mut completed = 0;
        let mut complete = |done: &SourceCompletion| {
            assert_eq!(done.source.source_path, path);
            assert_eq!(done.conversations_emitted, 1);
            // Delivery evidence never leaks into a later source boundary.
            assert!(!guarded_active_jsonl_source(&path));
            completed += 1;
            Ok(())
        };
        let mut hooks = SourceScanHooks {
            should_scan_source: None,
            on_source_complete: Some(&mut complete),
        };
        connector.scan_with_source_boundaries(&ctx, &mut hooks, &mut |conversation| {
            assert_eq!(conversation.source_path, path);
            assert_eq!(conversation.messages.len(), 1);
            assert_eq!(conversation.messages[0].content, "original");
            assert!(guarded_active_jsonl_source(&path));
            assert!(!guarded_active_jsonl_source(&foreign));
            let on_other_thread = path.clone();
            assert!(
                !std::thread::spawn(move || guarded_active_jsonl_source(&on_other_thread))
                    .join()
                    .expect("inspect delivery scope from another thread")
            );
            delivered += 1;
            Ok(())
        })?;
        assert_eq!((delivered, completed), (1, 1));
        assert!(!guarded_active_jsonl_source(&path));
        assert_eq!(fs::read(&path)?, bytes);
        assert_eq!(fs::metadata(&path)?.modified()?, modified);
        drop(writer);
        Ok(())
    }

    #[test]
    fn complete_eof_without_newline_has_no_active_delivery_permission() -> Result<()> {
        let (_root, path, ctx) = fixture("jsonl")?;
        let body = content("jsonl", "complete but not committed by newline");
        fs::write(&path, body.trim_end_matches('\n'))?;
        let writer = fs::OpenOptions::new().append(true).open(&path)?;
        let mut delivered = 0;
        crate::connectors::codex::CodexConnector::new().scan_with_callback(&ctx, &mut |_| {
            assert!(!guarded_active_jsonl_source(&path));
            delivered += 1;
            Ok(())
        })?;
        assert_eq!(
            delivered, 1,
            "the historical parser contract still permits complete EOF"
        );
        drop(writer);
        Ok(())
    }

    #[test]
    fn advisory_lock_blocks_candidate_and_revokes_guarded_delivery() -> Result<()> {
        let (_root, path, _) = fixture("jsonl")?;
        let snapshot = SourceSnapshot::capture(&path)?;
        let _guard = snapshot.guard_complete_delivery(&path)?;
        assert!(guarded_active_jsonl_source(&path));
        let lock = File::open(&path)?;
        lock.try_lock()?;
        assert!(!can_attempt_active_jsonl_source(&path));
        assert!(!guarded_active_jsonl_source(&path));
        lock.unlock()?;
        assert!(can_attempt_active_jsonl_source(&path));
        assert!(guarded_active_jsonl_source(&path));
        Ok(())
    }

    #[test]
    fn changing_a_guarded_source_revokes_active_delivery_permission() -> Result<()> {
        for mutation in ["append", "truncate", "rewrite", "replace"] {
            let (_root, path, _) = fixture("jsonl")?;
            let snapshot = SourceSnapshot::capture(&path)?;
            let _guard = snapshot.guard_complete_delivery(&path)?;
            assert!(guarded_active_jsonl_source(&path));
            match mutation {
                "append" => fs::OpenOptions::new()
                    .append(true)
                    .open(&path)?
                    .write_all(b"\n")?,
                "truncate" => File::create(&path)?.set_len(0)?,
                "rewrite" => {
                    fs::write(&path, content("jsonl", "replaced"))?;
                    File::open(&path)?.set_modified(
                        snapshot.before.modified()? + std::time::Duration::from_secs(2),
                    )?;
                }
                "replace" => {
                    fs::rename(&path, path.with_extension("retained"))?;
                    fs::write(&path, content("jsonl", "replacement generation"))?;
                }
                _ => unreachable!(),
            }
            assert!(!guarded_active_jsonl_source(&path), "{mutation}");
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn active_candidate_does_not_follow_a_symlink() -> Result<()> {
        let (root, path, _) = fixture("jsonl")?;
        let alias = root.path().join("rollout-alias.jsonl");
        std::os::unix::fs::symlink(&path, &alias)?;
        assert!(!can_attempt_active_jsonl_source(&alias));
        Ok(())
    }

    #[test]
    fn failed_delivery_drops_its_active_source_permission() -> Result<()> {
        let (_root, path, ctx) = fixture("jsonl")?;
        let error = crate::connectors::codex::CodexConnector::new()
            .scan_with_callback(&ctx, &mut |_| {
                assert!(guarded_active_jsonl_source(&path));
                anyhow::bail!("consumer stopped")
            })
            .unwrap_err();
        assert!(error.to_string().contains("consumer stopped"));
        assert!(!guarded_active_jsonl_source(&path));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn source_snapshot_refuses_fifo_replacements_with_a_bounded_child() -> Result<()> {
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};

        let root = tempfile::tempdir()?;
        let (_, module) = module_path!()
            .split_once("::")
            .context("snapshot test module has a crate prefix")?;
        let child_test = format!("{module}::source_snapshot_fifo_child");
        let mut child = Command::new(std::env::current_exe()?)
            .args(["--exact", &child_test, "--nocapture", "--test-threads=1"])
            .env("CASS_TEST_CODEX_SNAPSHOT_FIFO_ROOT", root.path())
            .stdin(Stdio::null())
            .spawn()?;
        let started = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if started.elapsed() < Duration::from_secs(10) => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                outcome => {
                    let _ = child.kill();
                    let _ = child.wait();
                    match outcome {
                        Err(error) => return Err(error.into()),
                        _ => anyhow::bail!("Codex source admission waited for a FIFO writer"),
                    }
                }
            }
        };
        assert!(status.success(), "snapshot FIFO child failed");
        // A receipt proves that the exact child filter executed every case;
        // a successful empty test selection cannot satisfy this regression.
        assert_eq!(
            fs::read(root.path().join("snapshot-fifo-receipt"))?,
            b"plain,compressed,symlink,replace-after-discovery,retry"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn source_snapshot_fifo_child() -> Result<()> {
        let Ok(root) = dotenvy::var("CASS_TEST_CODEX_SNAPSHOT_FIFO_ROOT") else {
            return Ok(());
        };
        let root = PathBuf::from(root);
        let make_fifo = |path: &Path| -> Result<()> {
            anyhow::ensure!(
                std::process::Command::new("mkfifo")
                    .arg(path)
                    .status()?
                    .success(),
                "create source FIFO"
            );
            Ok(())
        };

        // No writer is ever opened. A blocking read-open would therefore wait
        // forever; the parent kills this test process if that regresses.
        for extension in ["jsonl", "jsonl.zst"] {
            let fifo = root.join(format!("rollout-no-writer.{extension}"));
            make_fifo(&fifo)?;
            let error = SourceSnapshot::capture(&fifo)
                .err()
                .context("a FIFO cannot be admitted as a source snapshot")?;
            assert_kind(&error, io::ErrorKind::InvalidInput);
        }
        let alias = root.join("rollout-fifo-alias.jsonl");
        std::os::unix::fs::symlink(root.join("rollout-no-writer.jsonl"), &alias)?;
        let error = SourceSnapshot::capture(&alias)
            .err()
            .context("a symlink to a FIFO cannot be admitted")?;
        assert_kind(&error, io::ErrorKind::InvalidInput);

        // Exercise the real connector boundary, where an earlier discovery
        // already saw a regular file. Keep the original bytes for the retry.
        let (_fixture, path, ctx) = fixture("jsonl")?;
        let retained = path.with_extension("retained");
        let original = fs::read(&path)?;
        let mut swapped = false;
        let mut predicate = |source: &DiscoveredSourceFile| {
            assert_eq!(source.source_path, path);
            if !swapped {
                fs::rename(&path, &retained).expect("retain discovered source");
                make_fifo(&path).expect("substitute source after discovery");
                swapped = true;
            }
            true
        };
        let mut complete = |_: &SourceCompletion| panic!("FIFO source cannot complete");
        let mut hooks = SourceScanHooks {
            should_scan_source: Some(&mut predicate),
            on_source_complete: Some(&mut complete),
        };
        let error = scan(
            &CodexConnector::new(),
            &ctx,
            &mut hooks,
            &mut |_| panic!("FIFO source cannot emit a conversation"),
            |_| panic!("FIFO source cannot be enriched"),
        )
        .unwrap_err();
        assert_kind(&error, io::ErrorKind::InvalidInput);
        assert!(swapped);
        assert_eq!(fs::read(&retained)?, original);

        fs::rename(&path, path.with_extension("retained-fifo"))?;
        fs::rename(&retained, &path)?;
        let mut delivered = 0;
        let mut completed = 0;
        let mut complete = |_: &SourceCompletion| {
            completed += 1;
            Ok(())
        };
        let mut hooks = SourceScanHooks {
            should_scan_source: None,
            on_source_complete: Some(&mut complete),
        };
        scan(
            &CodexConnector::new(),
            &ctx,
            &mut hooks,
            &mut |conversation| {
                assert_eq!(conversation.messages.len(), 1);
                assert_eq!(conversation.messages[0].content, "original");
                delivered += 1;
                Ok(())
            },
            |conversation| augment_modern_codex_messages(conversation, None),
        )?;
        assert_eq!((delivered, completed), (1, 1));
        assert_eq!(fs::read(&path)?, original);
        fs::write(
            root.join("snapshot-fifo-receipt"),
            b"plain,compressed,symlink,replace-after-discovery,retry",
        )?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn ordinary_snapshot_preserves_regular_symlink_reads_and_identity_checks() -> Result<()> {
        let (root, path, _) = fixture("jsonl")?;
        let alias = root.path().join("rollout-regular-alias.jsonl");
        std::os::unix::fs::symlink(&path, &alias)?;
        let snapshot = SourceSnapshot::capture(&alias)?;
        snapshot.validate(&alias)?;
        assert_eq!(snapshot.len(), fs::metadata(&path)?.len());
        // Normal historical reads permit the alias; active-source admission
        // remains deliberately stricter.
        assert!(!can_attempt_active_jsonl_source(&alias));
        let _guard = snapshot.guard_complete_delivery(&alias)?;
        assert!(!guarded_active_jsonl_source(&alias));

        let error = SourceSnapshot::capture_with_limit(&alias, snapshot.len() - 1)
            .err()
            .context("symlink reads must still obey the source budget")?;
        assert_eq!(
            error
                .downcast_ref::<EnrichmentBudgetExceeded>()
                .context("typed source budget refusal")?
                .observed_bytes,
            snapshot.len()
        );

        let replacement = root.path().join("rollout-replacement.jsonl");
        fs::write(&replacement, content("jsonl", "replaced"))?;
        File::open(&replacement)?.set_modified(snapshot.before.modified()?)?;
        assert_eq!(fs::metadata(&replacement)?.len(), snapshot.len());
        fs::rename(&alias, alias.with_extension("retained-link"))?;
        std::os::unix::fs::symlink(&replacement, &alias)?;
        assert_kind(
            &snapshot.validate(&alias).unwrap_err(),
            io::ErrorKind::Interrupted,
        );
        Ok(())
    }

    #[test]
    fn rewrites_between_passes_never_publish_mixed_history_and_retry_succeeds() -> Result<()> {
        for extension in ["jsonl", "json"] {
            for mutation in ["rewrite", "append", "truncate", "replace"] {
                let (_root, path, ctx) = fixture(extension)?;
                let mut delivered = Vec::new();
                let mut completed = 0;
                let mut complete = |_: &SourceCompletion| {
                    completed += 1;
                    Ok(())
                };
                let mut hooks = SourceScanHooks {
                    should_scan_source: None,
                    on_source_complete: Some(&mut complete),
                };
                let error = scan(
                    &CodexConnector::new(),
                    &ctx,
                    &mut hooks,
                    &mut |conversation| {
                        delivered.push(conversation);
                        Ok(())
                    },
                    |conversation| {
                        assert_eq!(conversation.messages[0].content, "original");
                        match mutation {
                            "rewrite" => fs::write(&path, content(extension, "replacement body"))?,
                            "append" => {
                                let mut file = fs::OpenOptions::new().append(true).open(&path)?;
                                file.write_all(b"\n\n")?;
                            }
                            "truncate" => File::create(&path)?.set_len(0)?,
                            "replace" => {
                                fs::rename(&path, path.with_extension("retained"))?;
                                fs::write(&path, content(extension, "replacement body"))?;
                            }
                            _ => unreachable!(),
                        }
                        // Each pass on its own sees a stable file. Without the
                        // spanning guard this can publish old AND new messages.
                        augment_modern_codex_messages(conversation, None)
                    },
                )
                .unwrap_err();
                assert_kind(&error, io::ErrorKind::Interrupted);
                assert!(delivered.is_empty(), "{extension}/{mutation}");
                assert_eq!(completed, 0, "{extension}/{mutation}");

                fs::write(&path, content(extension, "stable retry"))?;
                let mut complete = |done: &SourceCompletion| {
                    assert_eq!(done.conversations_emitted, 1);
                    completed += 1;
                    Ok(())
                };
                let mut hooks = SourceScanHooks {
                    should_scan_source: None,
                    on_source_complete: Some(&mut complete),
                };
                scan(
                    &CodexConnector::new(),
                    &ctx,
                    &mut hooks,
                    &mut |conversation| {
                        delivered.push(conversation);
                        Ok(())
                    },
                    |conversation| augment_modern_codex_messages(conversation, None),
                )?;
                assert_eq!(completed, 1);
                assert_eq!(delivered.len(), 1);
                assert_eq!(delivered[0].messages.len(), 1);
                assert_eq!(delivered[0].messages[0].content, "stable retry");
            }
        }
        Ok(())
    }

    #[test]
    fn sink_mutation_returns_failure_without_certifying_completion() -> Result<()> {
        let (_root, path, ctx) = fixture("jsonl")?;
        let mut completed = 0;
        let mut complete = |_: &SourceCompletion| {
            completed += 1;
            Ok(())
        };
        let mut hooks = SourceScanHooks {
            should_scan_source: None,
            on_source_complete: Some(&mut complete),
        };
        let mut delivered = 0;
        let error = scan(
            &CodexConnector::new(),
            &ctx,
            &mut hooks,
            &mut |_| {
                delivered += 1;
                fs::write(&path, content("jsonl", "new source after delivery"))?;
                Ok(())
            },
            |_| Ok(()),
        )
        .unwrap_err();
        assert_kind(&error, io::ErrorKind::Interrupted);
        assert_eq!(
            delivered, 1,
            "delivery cannot be rolled back by the connector"
        );
        assert_eq!(completed, 0);
        Ok(())
    }

    #[test]
    fn enrichment_cannot_reassign_the_source_path() -> Result<()> {
        let (root, path, ctx) = fixture("jsonl")?;
        let foreign = root.path().join("rollout-foreign.jsonl");
        fs::copy(&path, &foreign)?;
        let error = scan(
            &CodexConnector::new(),
            &ctx,
            &mut SourceScanHooks::default(),
            &mut |_| panic!("foreign source must not reach the sink"),
            |conversation| {
                conversation.source_path.clone_from(&foreign);
                Ok(())
            },
        )
        .unwrap_err();
        assert_kind(&error, io::ErrorKind::InvalidData);
        Ok(())
    }

    #[test]
    fn capture_failure_cannot_be_swallowed_by_legacy_parser() -> Result<()> {
        let (_root, path, ctx) = fixture("json")?;
        let mut predicate = |_: &DiscoveredSourceFile| {
            fs::rename(&path, path.with_extension("retained")).unwrap();
            true
        };
        let mut complete = |_: &SourceCompletion| panic!("missing source cannot complete");
        let mut hooks = SourceScanHooks {
            should_scan_source: Some(&mut predicate),
            on_source_complete: Some(&mut complete),
        };
        let error = scan(
            &CodexConnector::new(),
            &ctx,
            &mut hooks,
            &mut |_| panic!("missing source cannot emit"),
            |_| panic!("missing source cannot be enriched"),
        )
        .unwrap_err();
        assert_kind(&error, io::ErrorKind::NotFound);
        Ok(())
    }

    #[test]
    fn capture_rechecks_opened_size_after_an_earlier_small_stat() -> Result<()> {
        for extension in ["jsonl", "json"] {
            let (_root, path, _ctx) = fixture(extension)?;
            assert!(fs::metadata(&path)?.len() < MAX_AUGMENT_ROLLOUT_BYTES);
            let size = MAX_AUGMENT_ROLLOUT_BYTES + 1;
            fs::OpenOptions::new()
                .write(true)
                .open(&path)?
                .set_len(size)?;
            let error = SourceSnapshot::capture(&path)
                .err()
                .expect("opened source exceeds the budget");
            assert_eq!(
                error
                    .downcast_ref::<EnrichmentBudgetExceeded>()
                    .unwrap()
                    .observed_bytes,
                size
            );
            assert_eq!(fs::metadata(&path)?.len(), size);
        }
        Ok(())
    }

    #[test]
    fn same_length_rewrite_is_detected() -> Result<()> {
        let (_root, path, _ctx) = fixture("jsonl")?;
        let snapshot = SourceSnapshot::capture(&path)?;
        let modified = snapshot.before.modified()? + std::time::Duration::from_secs(2);
        fs::write(&path, content("jsonl", "replaced"))?;
        fs::OpenOptions::new()
            .write(true)
            .open(&path)?
            .set_modified(modified)?;
        assert_eq!(fs::metadata(&path)?.len(), snapshot.before.len());
        assert_kind(
            &snapshot.validate(&path).unwrap_err(),
            io::ErrorKind::Interrupted,
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn replacement_with_restored_size_and_mtime_is_detected_by_opened_identity() -> Result<()> {
        let (_root, path, _ctx) = fixture("jsonl")?;
        let snapshot = SourceSnapshot::capture(&path)?;
        fs::rename(&path, path.with_extension("retained"))?;
        fs::write(&path, content("jsonl", "replaced"))?;
        File::open(&path)?.set_modified(snapshot.before.modified()?)?;
        assert_eq!(fs::metadata(&path)?.len(), snapshot.before.len());
        assert_eq!(
            fs::metadata(&path)?.modified()?,
            snapshot.before.modified()?
        );
        assert_kind(
            &snapshot.validate(&path).unwrap_err(),
            io::ErrorKind::Interrupted,
        );
        Ok(())
    }
}
