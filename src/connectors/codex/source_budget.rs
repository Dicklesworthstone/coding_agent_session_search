//! Guard source integrity and contain explicitly typed rollout-size rejection.
//!
//! Each guarded attempt fails immediately on parse, I/O, or callback errors.
//! The recovery driver isolates attempts by discovered file: healthy sources
//! continue after source-local failures, but consumer errors still abort the
//! entire scan. Any incomplete coverage returns an error, keeping watermarks
//! conservative. Diagnostics are bounded and source files are never modified.

mod empty_source;
mod recovery;
mod snapshot;

use std::cell::RefCell;
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use franken_agent_detection::DiscoveredSourceRole;
use franken_agent_detection::connectors::{SourceCompletion, SourceScanHooks};
use serde::Serialize;

use super::exclusions::ScanExclusions;
use super::{
    Connector, DiscoveredSourceFile, MAX_AUGMENT_ROLLOUT_BYTES, NormalizedConversation, ScanContext,
};
use snapshot::SourceSnapshot;

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
    snapshot: Option<SourceSnapshot>,
    preflight_error: Option<anyhow::Error>,
    withheld_source: Option<PathBuf>,
    delivered_conversation: bool,
    incomplete: IncompleteScan,
}

impl ScanState {
    fn validate_current_source(&self, path: &Path) -> Result<()> {
        if self.current_source.as_deref() != Some(path) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Codex conversation or completion does not match its admitted source",
            )
            .into());
        }
        self.snapshot
            .as_ref()
            .context("Codex source was not admitted for parsing")?
            .validate(path)
    }

    fn finish_pending_source(&self, progress_tick: Option<&(dyn Fn() + Send + Sync)>) -> Result<()> {
        // No conversation means enrichment never ran. Metadata stability alone
        // cannot distinguish valid empty history from a swallowed parse/read
        // failure in the published dependency. Validate that input separately;
        // do not invent a conversation or a successful source completion.
        if let Some(path) = self.current_source.as_deref()
            && self.snapshot.is_some()
            && self.withheld_source.as_deref() != Some(path)
        {
            self.validate_current_source(path)?;
            if !self.delivered_conversation {
                empty_source::validate(path, progress_tick)?;
                self.validate_current_source(path)?;
            }
        }
        Ok(())
    }

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
            .is_some_and(|extension| {
                extension.eq_ignore_ascii_case("jsonl") || extension.eq_ignore_ascii_case("json")
            })
    {
        return None;
    }
    // Both formats share the same cap. The published legacy parser silently
    // skips oversized JSON; contain it here so the scan reports incomplete
    // coverage and continues with later healthy sources, just like JSONL.
    let metadata = std::fs::metadata(&source.source_path).ok()?;
    (metadata.is_file() && metadata.len() > MAX_AUGMENT_ROLLOUT_BYTES).then_some(metadata.len())
}

pub(super) fn scan(
    inner: &dyn Connector,
    ctx: &ScanContext,
    hooks: &mut SourceScanHooks<'_>,
    on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    enrich: impl FnMut(&mut NormalizedConversation) -> Result<()>,
) -> Result<()> {
    recovery::scan(inner, ctx, hooks, on_conversation, enrich)
}

#[cfg(test)]
fn scan_guarded(
    inner: &dyn Connector,
    ctx: &ScanContext,
    hooks: &mut SourceScanHooks<'_>,
    on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    enrich: impl FnMut(&mut NormalizedConversation) -> Result<()>,
) -> Result<()> {
    let exclusions = ScanExclusions::from_env();
    scan_with_exclusions(inner, ctx, hooks, on_conversation, &exclusions, enrich)
}

fn scan_with_exclusions(
    inner: &dyn Connector,
    ctx: &ScanContext,
    hooks: &mut SourceScanHooks<'_>,
    on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    exclusions: &ScanExclusions,
    mut enrich: impl FnMut(&mut NormalizedConversation) -> Result<()>,
) -> Result<()> {
    let state = RefCell::new(ScanState::default());
    let SourceScanHooks {
        should_scan_source,
        on_source_complete,
    } = hooks;
    let mut should_scan = |source: &DiscoveredSourceFile| {
        {
            let mut state = state.borrow_mut();
            // The upstream predicate cannot return an error. Stop admitting
            // work after a source fails, then return that error from scan().
            if state.preflight_error.is_some() {
                return false;
            }
            if let Err(error) = state.finish_pending_source(ctx.progress_tick.as_deref()) {
                state.preflight_error = Some(error);
                return false;
            }
            state.current_source = None;
            state.snapshot = None;
            state.withheld_source = None;
            state.delivered_conversation = false;
        }
        // GH #486: filter before host hooks, budget rejection, or either parse.
        // Explicit-file roots pass through this hook too. An excluded oversized
        // file must not turn an otherwise successful scan into IncompleteScan.
        if exclusions.excludes(&source.source_path) {
            return false;
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
        state.borrow_mut().current_source = Some(source.source_path.clone());
        if let Some(size) = observed_over_limit(source) {
            state.borrow_mut().reject(&source.source_path, size);
            return false;
        }
        // One observation spans BOTH primary parsing and CASS enrichment.
        // Per-pass checks alone allow a rewrite between the two reads to mix
        // old primary messages with new enrichment in the same conversation.
        match SourceSnapshot::capture(&source.source_path) {
            Ok(snapshot) => state.borrow_mut().snapshot = Some(snapshot),
            Err(error) => {
                let mut state = state.borrow_mut();
                // Capture rechecks the opened object's size after the cheap
                // path-based preflight, closing the intervening growth race.
                if let Some(rejection) = error.downcast_ref::<EnrichmentBudgetExceeded>() {
                    state.reject(&source.source_path, rejection.observed_bytes);
                } else {
                    state.preflight_error = Some(error);
                }
                return false;
            }
        }
        true
    };
    let mut complete = |completion: &SourceCompletion| {
        {
            let state = state.borrow();
            if state.withheld_source.as_deref() == Some(completion.source.source_path.as_path()) {
                return Ok(());
            }
            state.validate_current_source(&completion.source.source_path)?;
            state.finish_pending_source(ctx.progress_tick.as_deref())?;
        }
        on_source_complete
            .as_mut()
            .map_or(Ok(()), |sink| sink(completion))?;
        // Once completion succeeds, later appends to this already-consumed
        // source are normal live activity, not failure of a subsequent source.
        let mut state = state.borrow_mut();
        state.current_source = None;
        state.snapshot = None;
        Ok(())
    };
    let mut forward = |mut conversation: NormalizedConversation| {
        state
            .borrow()
            .validate_current_source(&conversation.source_path)?;
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
        state
            .borrow()
            .validate_current_source(&conversation.source_path)?;
        // Deliberately outside the enrichment error match: sink/storage errors
        // must abort even when a caller returns the same concrete error type.
        let source_path = conversation.source_path.clone();
        on_conversation(conversation)?;
        let mut state = state.borrow_mut();
        state.delivered_conversation = true;
        // A sink can mutate the source too. Delivery cannot be rolled back
        // here, but the scan must fail and must not certify its fingerprint.
        state.validate_current_source(&source_path)
    };
    let mut guarded = SourceScanHooks {
        should_scan_source: Some(&mut should_scan),
        on_source_complete: Some(&mut complete),
    };
    inner.scan_with_source_boundaries(ctx, &mut guarded, &mut forward)?;
    let mut state = state.into_inner();
    if let Some(error) = state.preflight_error.take() {
        return Err(error);
    }
    state.finish_pending_source(ctx.progress_tick.as_deref())?;
    let incomplete = state.incomplete;
    if incomplete.rejected_source_count > 0 {
        return Err(incomplete.into());
    }
    Ok(())
}

#[cfg(test)]
mod coverage_tests {
    use super::scan_guarded as scan;
    include!("source_budget/coverage_tests.rs");
}
#[cfg(test)]
mod tests {
    use super::scan_guarded as scan;
    include!("source_budget/tests.rs");
}
