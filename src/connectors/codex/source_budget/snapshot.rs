//! Keep both Codex parsing passes tied to one observed source generation.
//!
//! This is an observable-change guard, not an atomic filesystem snapshot or
//! protection against in-place edits that preserve all checked metadata.

use std::fs::{self, File, Metadata};
use std::io;
use std::path::Path;

use anyhow::{Context, Result};

pub(super) struct SourceSnapshot {
    file: File,
    before: Metadata,
}

impl SourceSnapshot {
    pub(super) fn capture(path: &Path) -> Result<Self> {
        let file =
            File::open(path).with_context(|| format!("open Codex source snapshot {path:?}"))?;
        let before = file.metadata().context("inspect Codex source snapshot")?;
        if !before.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Codex source snapshot is not a regular file",
            )
            .into());
        }
        Ok(Self { file, before })
    }

    pub(super) fn validate(&self, path: &Path) -> Result<()> {
        let opened = self.file.metadata().context("recheck opened Codex source")?;
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
        CodexConnector, DiscoveredSourceFile, ScanContext, ScanRoot, SourceCompletion,
        SourceScanHooks,
    };
    use std::io::Write;
    use std::path::PathBuf;

    use super::super::super::augment_modern_codex_messages;
    use super::super::scan;

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
        assert_eq!(delivered, 1, "delivery cannot be rolled back by the connector");
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
        assert_eq!(fs::metadata(&path)?.modified()?, snapshot.before.modified()?);
        assert_kind(
            &snapshot.validate(&path).unwrap_err(),
            io::ErrorKind::Interrupted,
        );
        Ok(())
    }
}
