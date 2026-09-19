//! Isolate failed Codex files without hiding incomplete scan coverage.
//!
//! Discovery selects the inventory once. Each file is then parsed in its own
//! explicit scope using the real connector and the same integrity guard. This
//! is not a retry loop: changing/unfinished sources are retried on a later scan.

use std::cell::Cell;
use std::fmt;
use std::io;

use franken_agent_detection::ScanRoot;
use serde::Serialize;

use crate::connectors::codex::archives;
use super::{
    Connector, DiscoveredSourceFile, DiscoveredSourceRole, IncompleteScan, MAX_REJECTION_SAMPLES,
    NormalizedConversation, RejectedSource, Result, ScanContext, ScanExclusions, SourceCompletion,
    SourceScanHooks, scan_with_exclusions,
};

#[derive(Debug, Serialize)]
struct FailedSource {
    source_path: String,
    reason: &'static str,
}

#[derive(Debug, Serialize)]
struct FailureReport {
    reason: &'static str,
    failed_source_count: usize,
    omitted_source_count: usize,
    failed_sources: Vec<FailedSource>,
}

impl Default for FailureReport {
    fn default() -> Self {
        Self {
            reason: "source_read_failures",
            failed_source_count: 0,
            omitted_source_count: 0,
            failed_sources: Vec::new(),
        }
    }
}

impl fmt::Display for FailureReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Codex scan incomplete: {}",
            serde_json::to_string(self).map_err(|_| fmt::Error)?
        )
    }
}

#[derive(Default)]
struct Failures {
    report: FailureReport,
    budgets: IncompleteScan,
    first_error: Option<anyhow::Error>,
}

impl Failures {
    fn record(&mut self, source: &DiscoveredSourceFile, error: anyhow::Error) {
        let reason = if let Some(budget) = error.downcast_ref::<IncompleteScan>() {
            self.budgets.rejected_source_count = self
                .budgets
                .rejected_source_count
                .saturating_add(budget.rejected_source_count);
            let available = MAX_REJECTION_SAMPLES - self.budgets.rejected_sources.len();
            self.budgets.rejected_sources.extend(
                budget
                    .rejected_sources
                    .iter()
                    .take(available)
                    .map(|sample| RejectedSource {
                        source_path: sample.source_path.clone(),
                        observed_bytes: sample.observed_bytes,
                    }),
            );
            self.budgets.omitted_source_count = self
                .budgets
                .rejected_source_count
                .saturating_sub(self.budgets.rejected_sources.len());
            "read_budget_exceeded"
        } else {
            let reason = match error.downcast_ref::<io::Error>().map(io::Error::kind) {
                Some(io::ErrorKind::UnexpectedEof) => "unfinished_source",
                Some(io::ErrorKind::Interrupted) => "source_changed",
                Some(io::ErrorKind::NotFound) => "source_missing",
                Some(io::ErrorKind::PermissionDenied) => "source_unreadable",
                Some(io::ErrorKind::InvalidData) => "invalid_source_data",
                _ => "source_read_failed",
            };
            // Retain one original error for typed downcasts and its cause chain.
            // The bounded summary never contains session bodies or parser text.
            if self.first_error.is_none() {
                self.first_error = Some(error);
            }
            reason
        };
        self.report.failed_source_count = self.report.failed_source_count.saturating_add(1);
        if self.report.failed_sources.len() < MAX_REJECTION_SAMPLES {
            self.report.failed_sources.push(FailedSource {
                source_path: source.source_path.to_string_lossy().into_owned(),
                reason,
            });
        } else {
            self.report.omitted_source_count = self.report.omitted_source_count.saturating_add(1);
        }
    }

    fn finish(self) -> Result<()> {
        if let Some(error) = self.first_error {
            return Err(error.context(self.report));
        }
        // Preserve the existing budget-only diagnostic schema and error type.
        if self.budgets.rejected_source_count > 0 {
            return Err(self.budgets.into());
        }
        Ok(())
    }
}

fn scoped_context(ctx: &ScanContext, source: &DiscoveredSourceFile) -> ScanContext {
    let root = ctx
        .scan_roots
        .iter()
        .find(|root| {
            root.origin == source.origin
                && (source.scan_root.starts_with(&root.path)
                    || root.path.starts_with(&source.scan_root))
        })
        .map_or_else(
            || {
                ScanRoot::remote(
                    source.source_path.clone(),
                    source.origin.clone(),
                    source.platform,
                )
            },
            |root| root.with_path(source.source_path.clone()),
        );
    // Discovery already applied the incremental cutoff. Applying it twice
    // could silently skip a selected file after its metadata changes.
    let mut scoped = ScanContext::with_roots(ctx.data_dir.clone(), vec![root], None);
    scoped.progress_tick.clone_from(&ctx.progress_tick);
    scoped
}

fn original_identity(
    mut observed: DiscoveredSourceFile,
    inventory: &DiscoveredSourceFile,
) -> DiscoveredSourceFile {
    // Explicit-file scanning is an implementation detail. Durable ledgers must
    // see the original discovery identity, with fresh pre-parse size/mtime.
    observed.scan_root.clone_from(&inventory.scan_root);
    observed.origin.clone_from(&inventory.origin);
    observed.platform = inventory.platform;
    observed
}

fn directory_session_id(source: &DiscoveredSourceFile) -> Option<String> {
    // FAD's directory scan derives the ID relative to root/sessions when that
    // is the selected subtree, otherwise relative to the root itself. Its
    // explicit-file API instead finds the nearest sessions ancestor. Preserve
    // the original directory contract for dated subroots and custom mirrors;
    // changing the scope for recovery must not create duplicate stored sessions.
    source
        .source_path
        .strip_prefix(source.scan_root.join("sessions"))
        .or_else(|_| source.source_path.strip_prefix(&source.scan_root))
        .ok()
        .and_then(|relative| relative.with_extension("").to_str().map(str::to_owned))
        .or_else(|| source.source_path.file_stem()?.to_str().map(str::to_owned))
}

fn scan_source(
    inner: &dyn Connector,
    ctx: &ScanContext,
    source: &DiscoveredSourceFile,
    hooks: &mut SourceScanHooks<'_>,
    on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    exclusions: &ScanExclusions,
    enrich: &mut dyn FnMut(&mut NormalizedConversation) -> Result<()>,
) -> (Result<()>, bool) {
    let consumer_failed = Cell::new(false);
    let visited = Cell::new(false);
    let scope_changed = Cell::new(false);
    let directory_id = archives::session_id(ctx, source).map(Some).or_else(|| {
        (source.scan_root != source.source_path).then(|| directory_session_id(source))
    });
    let SourceScanHooks {
        should_scan_source,
        on_source_complete,
    } = hooks;
    let mut before = |observed: &DiscoveredSourceFile| {
        // A discovered file can become a directory before the explicit scan.
        // Never let that race broaden the selection to undiscovered children.
        if observed.source_path != source.source_path {
            scope_changed.set(true);
            return false;
        }
        visited.set(true);
        let observed = original_identity(observed.clone(), source);
        should_scan_source
            .as_mut()
            .is_none_or(|predicate| predicate(&observed))
    };
    let mut complete = |done: &SourceCompletion| {
        let done = SourceCompletion {
            source: original_identity(done.source.clone(), source),
            required_sidecars: done.required_sidecars.clone(),
            conversations_emitted: done.conversations_emitted,
        };
        on_source_complete
            .as_mut()
            .map_or(Ok(()), |sink| sink(&done))
            .inspect_err(|_| consumer_failed.set(true))
    };
    let mut deliver = |mut conversation: NormalizedConversation| {
        if let Some(id) = &directory_id {
            conversation.external_id.clone_from(id);
        }
        on_conversation(conversation).inspect_err(|_| consumer_failed.set(true))
    };
    let mut guarded = SourceScanHooks {
        should_scan_source: Some(&mut before),
        on_source_complete: Some(&mut complete),
    };
    let result = scan_with_exclusions(
        inner,
        &scoped_context(ctx, source),
        &mut guarded,
        &mut deliver,
        exclusions,
        enrich,
    )
    .and_then(|()| {
        if scope_changed.get() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Codex source scope changed after discovery; retry this source",
            )
            .into());
        }
        if !visited.get() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "Codex source disappeared after discovery; retry this source",
            )
            .into());
        }
        Ok(())
    });
    (result, consumer_failed.get())
}

pub(super) fn scan(
    inner: &dyn Connector,
    ctx: &ScanContext,
    hooks: &mut SourceScanHooks<'_>,
    on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    mut enrich: impl FnMut(&mut NormalizedConversation) -> Result<()>,
) -> Result<()> {
    anyhow::ensure!(
        inner.supports_source_boundaries(),
        "Codex recovery requires source boundaries"
    );
    let exclusions = ScanExclusions::from_env();
    let sources = archives::discover(inner, ctx)?;
    let mut failures = Failures::default();
    for source in sources {
        if exclusions.excludes(&source.source_path) {
            continue;
        }
        anyhow::ensure!(
            source.provider_slug == "codex"
                && source.role == DiscoveredSourceRole::PrimarySessionLog,
            "Codex recovery requires independent rollout sources"
        );
        let (result, consumer_failed) = scan_source(
            inner,
            ctx,
            &source,
            hooks,
            on_conversation,
            &exclusions,
            &mut enrich,
        );
        if let Err(error) = result {
            // Classify by where the error originated, never its type or text:
            // a storage/cancellation callback can return the same I/O error as
            // an unfinished file and must still abort immediately.
            if consumer_failed {
                return Err(error);
            }
            failures.record(&source, error);
        }
    }
    failures.finish()
}

#[cfg(test)]
mod contract_tests;
#[cfg(test)]
mod resume_tests;
#[cfg(test)]
mod tests;
