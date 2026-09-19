//! Contain only the explicitly typed enrichment-size rejection (GH #484).
//!
//! A rejected rollout must neither stop later sources nor receive a successful
//! completion. Other parse, I/O, cancellation and sink errors still abort. The
//! final aggregate error keeps connector/global watermarks behind and makes a
//! partial scan distinguishable from success. Rejection samples are bounded;
//! this is not a quarantine and never changes the source files.

use std::cell::RefCell;
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::Result;
use franken_agent_detection::DiscoveredSourceRole;
use franken_agent_detection::connectors::{SourceCompletion, SourceScanHooks};
use serde::Serialize;

use super::exclusions::ScanExclusions;
use super::{
    Connector, DiscoveredSourceFile, MAX_AUGMENT_ROLLOUT_BYTES, NormalizedConversation, ScanContext,
};

const MAX_REJECTION_SAMPLES: usize = 32;

/// A precise rejection type; never identify a recoverable error by its text or
/// by the broad `InvalidData` kind (which also covers corruption/invalid UTF-8).
#[derive(Debug, thiserror::Error)]
#[error("Codex enrichment source exceeds the 100 MiB read budget ({observed_bytes} bytes)")]
pub(super) struct EnrichmentBudgetExceeded {
    pub(super) observed_bytes: u64,
}

#[derive(Debug, Serialize)]
struct RejectedSource {
    source_path: String,
    observed_bytes: u64,
}

/// Bounded, source-specific diagnostics carried by the final scan error. Only
/// provenance and byte counts are recorded, never rollout text or JSON lines.
#[derive(Debug, Serialize)]
pub(super) struct IncompleteScan {
    reason: &'static str,
    limit_bytes: u64,
    rejected_source_count: usize,
    omitted_source_count: usize,
    rejected_sources: Vec<RejectedSource>,
}

impl Default for IncompleteScan {
    fn default() -> Self {
        Self {
            reason: "enrichment_read_budget_exceeded",
            limit_bytes: MAX_AUGMENT_ROLLOUT_BYTES,
            rejected_source_count: 0,
            omitted_source_count: 0,
            rejected_sources: Vec::new(),
        }
    }
}

impl fmt::Display for IncompleteScan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // JSON escaping keeps unusual source names from injecting log lines.
        write!(
            f,
            "Codex scan incomplete: {}",
            serde_json::to_string(self).map_err(|_| fmt::Error)?
        )
    }
}

impl std::error::Error for IncompleteScan {}

#[derive(Default)]
struct ScanState {
    current_source: Option<PathBuf>,
    withheld_source: Option<PathBuf>,
    incomplete: IncompleteScan,
}

impl ScanState {
    fn reject(&mut self, source: &Path, observed_bytes: u64) {
        self.withheld_source = Some(source.to_path_buf());
        self.incomplete.rejected_source_count =
            self.incomplete.rejected_source_count.saturating_add(1);
        if self.incomplete.rejected_sources.len() < MAX_REJECTION_SAMPLES {
            self.incomplete.rejected_sources.push(RejectedSource {
                source_path: source.to_string_lossy().into_owned(),
                observed_bytes,
            });
        } else {
            self.incomplete.omitted_source_count =
                self.incomplete.omitted_source_count.saturating_add(1);
        }
    }
}

fn observed_over_limit(source: &DiscoveredSourceFile) -> Option<u64> {
    if source.provider_slug != "codex"
        || source.role != DiscoveredSourceRole::PrimarySessionLog
        || !source
            .source_path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
    {
        return None;
    }
    // Discovery metadata is only a hint: recheck after the host's pre-parse
    // predicate. Unreadable/nonregular sources retain existing error handling.
    let metadata = std::fs::metadata(&source.source_path).ok()?;
    (metadata.is_file() && metadata.len() > MAX_AUGMENT_ROLLOUT_BYTES).then_some(metadata.len())
}

pub(super) fn scan(
    inner: &dyn Connector,
    ctx: &ScanContext,
    hooks: &mut SourceScanHooks<'_>,
    on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    mut enrich: impl FnMut(&mut NormalizedConversation) -> Result<()>,
) -> Result<()> {
    let exclusions = ScanExclusions::from_env();
    let state = RefCell::new(ScanState::default());
    let SourceScanHooks {
        should_scan_source,
        on_source_complete,
    } = hooks;
    let mut should_scan = |source: &DiscoveredSourceFile| {
        // GH #486: filter before host hooks, budget rejection, or either parse.
        // Explicit-file roots pass through this hook too. An excluded oversized
        // file must not turn an otherwise successful scan into IncompleteScan.
        if exclusions.excludes(&source.source_path) {
            return false;
        }
        {
            let mut state = state.borrow_mut();
            state.current_source = Some(source.source_path.clone());
            state.withheld_source = None;
        }
        // Preserve durable reuse, operator filters, pending batch flushes and
        // cancellation decisions. An intentionally excluded source isn't a
        // failed attempted read.
        if !should_scan_source
            .as_mut()
            .is_none_or(|predicate| predicate(source))
        {
            return false;
        }
        if let Some(size) = observed_over_limit(source) {
            state.borrow_mut().reject(&source.source_path, size);
            return false;
        }
        true
    };
    let mut complete = |completion: &SourceCompletion| {
        if state.borrow().withheld_source.as_deref()
            == Some(completion.source.source_path.as_path())
        {
            return Ok(());
        }
        on_source_complete
            .as_mut()
            .map_or(Ok(()), |sink| sink(completion))
    };
    let mut forward = |mut conversation: NormalizedConversation| {
        if let Err(error) = enrich(&mut conversation) {
            // The file may cross the cap after preflight. Contain that specific
            // owned enrichment error only when the enclosing source is known;
            // interpose on completion so FAD cannot certify the discarded data.
            if let Some(rejection) = error.downcast_ref::<EnrichmentBudgetExceeded>() {
                let mut state = state.borrow_mut();
                if state.current_source.as_deref() == Some(conversation.source_path.as_path()) {
                    state.reject(&conversation.source_path, rejection.observed_bytes);
                    return Ok(());
                }
            }
            return Err(error);
        }
        // Deliberately outside the enrichment error match: sink/storage errors
        // must abort even when a caller returns the same concrete error type.
        on_conversation(conversation)
    };
    let mut guarded = SourceScanHooks {
        should_scan_source: Some(&mut should_scan),
        on_source_complete: Some(&mut complete),
    };
    inner.scan_with_source_boundaries(ctx, &mut guarded, &mut forward)?;
    let incomplete = state.into_inner().incomplete;
    if incomplete.rejected_source_count > 0 {
        return Err(incomplete.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests;
