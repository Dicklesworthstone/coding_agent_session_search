//! Phase-specific diagnostics for rebuild finalization (GH #494).
//!
//! Consuming the canonical cursor and publishing a generation are different
//! milestones. Never turn a failed finalization step into a successful outcome.

use anyhow::Result;
use std::path::Path;
use std::time::Instant;

pub(super) struct Finalization<'a> {
    live: &'a Path,
    candidate: &'a Path,
    indexed_docs: usize,
    processed_conversations: usize,
}

impl<'a> Finalization<'a> {
    pub(super) fn new(
        live: &'a Path,
        candidate: &'a Path,
        indexed_docs: usize,
        processed_conversations: usize,
    ) -> Self {
        Self {
            live,
            candidate,
            indexed_docs,
            processed_conversations,
        }
    }

    pub(super) fn run<T>(
        &self,
        phase: &'static str,
        operation: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let started = Instant::now();
        tracing::info!(
            phase,
            live_index_path = %self.live.display(),
            candidate_index_path = %self.candidate.display(),
            indexed_docs = self.indexed_docs,
            processed_conversations = self.processed_conversations,
            "lexical rebuild finalization step started"
        );
        match operation() {
            Ok(value) => {
                tracing::info!(
                    phase,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "lexical rebuild finalization step completed"
                );
                Ok(value)
            }
            Err(error) => {
                let cause = format!("{error:#}");
                tracing::error!(
                    phase,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    live_index_path = %self.live.display(),
                    candidate_index_path = %self.candidate.display(),
                    indexed_docs = self.indexed_docs,
                    processed_conversations = self.processed_conversations,
                    error = %cause,
                    "lexical rebuild finalization failed"
                );
                // Some CLI consumers display only the outer error. Keep
                // the full cause there AND retain its typed source chain.
                // Do not say "not published": a late failure can occur
                // after the directory swap changed the live generation.
                let diagnostic = format!(
                    "lexical rebuild finalization failed during {phase} \
                     (live={}, candidate={}, indexed_docs={}, processed_conversations={}): {cause}",
                    self.live.display(),
                    self.candidate.display(),
                    self.indexed_docs,
                    self.processed_conversations,
                );
                Err(error.context(diagnostic))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn gh494_finalization_preserves_the_success_value_and_runs_once() {
        let calls = Cell::new(0);
        let context = Finalization::new(Path::new("live"), Path::new("candidate"), 4, 2);
        let value = context
            .run("commit_candidate", || {
                calls.set(calls.get() + 1);
                Ok(42)
            })
            .unwrap();
        assert_eq!(value, 42);
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn gh494_finalization_display_includes_phase_paths_counts_and_original_cause() {
        let context = Finalization::new(Path::new("live"), Path::new("candidate"), 4, 2);
        let error = context
            .run::<()>("publish_staged_generation", || {
                Err(anyhow::Error::new(std::io::Error::from_raw_os_error(13))
                    .context("rename of validated candidate refused"))
            })
            .unwrap_err();
        let message = error.to_string();
        for expected in [
            "publish_staged_generation",
            "live=live",
            "candidate=candidate",
            "indexed_docs=4",
            "processed_conversations=2",
            "rename of validated candidate refused",
        ] {
            assert!(message.contains(expected), "missing {expected}: {message}");
        }
        assert_eq!(
            error
                .downcast_ref::<std::io::Error>()
                .unwrap()
                .raw_os_error(),
            Some(13)
        );
    }

    #[test]
    fn gh494_late_failure_does_not_claim_the_live_generation_was_unchanged() {
        let context = Finalization::new(Path::new("live"), Path::new("candidate"), 4, 2);
        let error = context
            .run::<()>("persist_completed_checkpoint", || {
                anyhow::bail!("checkpoint directory is not writable")
            })
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("checkpoint directory is not writable")
        );
        assert!(!error.to_string().contains("not published"));
        assert!(!error.to_string().contains("unchanged"));
    }
}
