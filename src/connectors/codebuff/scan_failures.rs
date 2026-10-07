//! Bounded failure evidence for a partial Codebuff scan.
//!
//! A source failure must not hide healthy callbacks or the failures that follow
//! it. Keep the original first error chain for existing typed classification;
//! retain a bounded set of additional path/cause pairs for the operator.

use std::error::Error;
use std::fmt;
use std::path::PathBuf;

const MAX_FAILURE_SAMPLES: usize = 32;

#[derive(Debug)]
struct SourceFailure {
    path: PathBuf,
    error: anyhow::Error,
}

#[derive(Debug, Default)]
pub(super) struct ScanFailures {
    total: usize,
    samples: Vec<SourceFailure>,
}

impl ScanFailures {
    pub(super) fn record(&mut self, path: PathBuf, error: anyhow::Error) {
        self.total = self.total.saturating_add(1);
        if self.samples.len() < MAX_FAILURE_SAMPLES {
            self.samples.push(SourceFailure { path, error });
        }
    }

    pub(super) fn finish(self) -> anyhow::Result<()> {
        if self.total == 0 {
            Ok(())
        } else {
            Err(anyhow::Error::new(self))
        }
    }
}

impl fmt::Display for ScanFailures {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Some(first) = self.samples.first() else {
            return f.write_str("Codebuff / Freebuff scan has no source failures");
        };
        // FAD's top message already names its failing transcript. Include our
        // selected source too: post-parse observation errors may name only a
        // directory. Never infer paths by splitting a formatted error string.
        write!(f, "{}: {:#}", first.path.display(), first.error)?;
        if self.total > 1 {
            write!(f, " (and {} more failed sources)", self.total - 1)?;
            for failure in self.samples.iter().skip(1) {
                write!(f, "\n{}: {:#}", failure.path.display(), failure.error)?;
            }
        }
        let omitted = self.total.saturating_sub(self.samples.len());
        if omitted != 0 {
            write!(
                f,
                "\n{omitted} additional source failures omitted (first {MAX_FAILURE_SAMPLES} shown; {} total); the scan remains incomplete",
                self.total
            )?;
        }
        Ok(())
    }
}

impl Error for ScanFailures {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.samples.first()?.error.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::codebuff::CodebuffConnector;
    use crate::connectors::{Connector, ScanContext, ScanRoot};
    use franken_agent_detection::connectors::SourceScanHooks;
    use serde_json::json;
    use std::fs;
    use std::path::Path;

    fn transcript(root: &Path, project: &str, bytes: &[u8]) -> PathBuf {
        let chat = root.join(project).join("chats/2026-03-21T17-14-03.768Z");
        fs::create_dir_all(&chat).unwrap();
        let path = chat.join("chat-messages.json");
        fs::write(&path, bytes).unwrap();
        path
    }

    fn native_record(variant: &str, timestamp: &str) -> Vec<u8> {
        serde_json::to_vec(&json!([{
            "id": "user-1774113351457", "variant": variant,
            "content": "multierrorhealthyproof", "timestamp": timestamp
        }]))
        .unwrap()
    }

    #[test]
    fn gh511_each_sample_retains_its_source_and_cause_without_hiding_good_chats() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("projects");
        let json_bad = transcript(&root, "a-bad-json", b"[");
        let good = transcript(&root, "middle-good", &native_record("user", "01:15 PM"));
        let iso_bad = transcript(
            &root,
            "y-bad-time",
            &native_record("user", "2026-13-45T99:00:00Z"),
        );
        let variant_bad = transcript(
            &root,
            "z-bad-variant",
            &native_record("assistant", "01:15 PM"),
        );
        let ctx = ScanContext::with_roots(
            temp.path().join("data"),
            vec![ScanRoot::local(root)],
            None,
        );
        let mut delivered = Vec::new();
        let mut completed = Vec::new();
        let error = CodebuffConnector::new()
            .scan_with_source_boundaries(
                &ctx,
                &mut SourceScanHooks {
                    should_scan_source: None,
                    on_source_complete: Some(&mut |completion| {
                        completed.push(completion.source.source_path.clone());
                        Ok(())
                    }),
                },
                &mut |conversation| {
                    assert_eq!(conversation.messages[0].created_at, Some(1_774_113_351_457));
                    delivered.push(conversation.source_path);
                    Ok(())
                },
            )
            .unwrap_err();
        assert_eq!(delivered, vec![good.clone()]);
        assert_eq!(completed, vec![good]);
        let failures = error.downcast_ref::<ScanFailures>().unwrap();
        assert_eq!(failures.total, 3);
        assert_eq!(failures.samples.len(), 3);
        let message = error.to_string();
        for (path, cause) in [
            (&json_bad, "invalid Codebuff / Freebuff transcript JSON"),
            (&iso_bad, "unparseable ISO-8601 timestamp"),
            (&variant_bad, "unsupported shared CLI message variant"),
        ] {
            let failure = failures
                .samples
                .iter()
                .find(|sample| &sample.path == path)
                .unwrap();
            assert!(format!("{:#}", failure.error).contains(cause), "{message}");
            assert!(message.contains(path.to_string_lossy().as_ref()), "{message}");
            assert!(message.contains(cause), "{message}");
        }
        assert!(error.chain().any(|cause| cause.is::<serde_json::Error>()));
    }

    #[test]
    fn gh511_failure_evidence_is_bounded_but_the_total_and_first_typed_cause_survive() {
        ScanFailures::default().finish().unwrap();
        let mut failures = ScanFailures::default();
        let total = MAX_FAILURE_SAMPLES + 7;
        for index in 0..total {
            failures.record(
                PathBuf::from(format!("/projects/source-{index}/chat-messages.json")),
                std::io::Error::new(std::io::ErrorKind::PermissionDenied, "fixture read denied")
                    .into(),
            );
        }
        let error = failures.finish().unwrap_err().context("outer scan context");
        let failures = error.downcast_ref::<ScanFailures>().unwrap();
        assert_eq!(failures.samples.len(), MAX_FAILURE_SAMPLES);
        assert_eq!(failures.total, total);
        let message = format!("{error:#}");
        assert!(message.contains("source-0/chat-messages.json"));
        assert!(message.contains("source-31/chat-messages.json"));
        assert!(!message.contains("source-32/chat-messages.json"));
        assert!(message.contains("7 additional source failures omitted"));
        assert!(message.contains("39 total"));
        assert!(error.chain().any(|cause| {
            cause.downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::PermissionDenied)
        }));
    }

    #[test]
    fn gh511_sink_failure_after_source_failure_still_stops_the_scan_immediately() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("projects");
        transcript(&root, "a-bad", b"[");
        transcript(&root, "middle-good", &native_record("user", "01:15 PM"));
        transcript(&root, "z-later", &native_record("user", "01:15 PM"));
        let ctx = ScanContext::with_roots(
            temp.path().join("data"),
            vec![ScanRoot::local(root)],
            None,
        );
        for fail_completion in [false, true] {
            let mut delivered = 0;
            let mut completed = 0;
            let error = CodebuffConnector::new()
                .scan_with_source_boundaries(
                    &ctx,
                    &mut SourceScanHooks {
                        should_scan_source: None,
                        on_source_complete: Some(&mut |_| {
                            completed += 1;
                            Err(std::io::Error::other("completion sink sentinel").into())
                        }),
                    },
                    &mut |_| {
                        delivered += 1;
                        if fail_completion {
                            Ok(())
                        } else {
                            Err(std::io::Error::other("conversation sink sentinel").into())
                        }
                    },
                )
                .unwrap_err();
            assert_eq!(delivered, 1);
            assert_eq!(completed, usize::from(fail_completion));
            assert!(error.downcast_ref::<ScanFailures>().is_none());
            assert!(error.chain().any(|cause| cause.is::<std::io::Error>()));
            assert!(error.to_string().contains("sink sentinel"));
        }
    }
}
