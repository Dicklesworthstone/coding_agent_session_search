//! Isolate failed Codex files without hiding incomplete scan coverage.
//!
//! Discovery selects the inventory once. Each file is then parsed in its own
//! explicit scope using the real connector and the same integrity guard. This
//! is not a retry loop: changing/unfinished sources are retried on a later scan.

use std::cell::Cell;
use std::fmt;
use std::io;
use std::path::Path;

use franken_agent_detection::ScanRoot;
use franken_agent_detection::connectors::codex::rollout_session_path;
use serde::Serialize;

use super::{
    Connector, DiscoveredSourceFile, DiscoveredSourceRole, IncompleteScan, MAX_REJECTION_SAMPLES,
    NormalizedConversation, RejectedSource, Result, ScanAdmission, ScanContext, ScanExclusions,
    ScanLimits, SourceCompletion, SourceScanHooks, scan_with_admission,
};
use crate::connectors::codex::archives;
use crate::connectors::codex::stable_prefix::{
    self, PrefixDeliveryGuard, StablePrefixReceipt, StablePrefixSnapshot,
};

/// Source-local causes, captured before aggregating a partial scan. Keep this
/// typed so diagnostics never infer a path or cause from formatted error text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SourceFailureReason {
    ReadBudgetExceeded,
    UnfinishedSource,
    SourceChanged,
    SourceMissing,
    SourceUnreadable,
    InvalidSourceData,
    SourceReadFailed,
}

#[derive(Debug, Serialize)]
struct FailedSource {
    source_path: String,
    reason: SourceFailureReason,
    // Preserve the native path for host accounting, including non-UTF-8 names.
    // Only the escaped display path belongs in the diagnostic wire summary.
    #[serde(skip)]
    path: std::path::PathBuf,
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

/// Borrow the bounded per-file failure evidence without losing mixed causes
/// behind the first error's chain. The aggregate count/omissions remain in the
/// scan error even when not every failed source fits the diagnostic sample.
pub(crate) fn for_each_source_failure(
    error: &anyhow::Error,
    mut observe: impl FnMut(&Path, SourceFailureReason),
) -> bool {
    let report = error.downcast_ref::<FailureReport>();
    let admissions = error.downcast_ref::<AdmissionFailureReport>();
    if report.is_none() && admissions.is_none() {
        return false;
    }
    for report in report.into_iter().chain(admissions.map(|report| &report.0)) {
        for source in &report.failed_sources {
            observe(&source.path, source.reason);
        }
    }
    // A budget-only parser error has no FailureReport. When host admission
    // also refused a different source, retain both causes in the typed path.
    // A direct admission-budget error carries both report types, so avoid
    // repeating samples already represented by its native-path evidence.
    if report.is_none()
        && let Some(admission_report) = admissions
        && let Some(budget) = error.downcast_ref::<IncompleteScan>()
    {
        for source in &budget.rejected_sources {
            let path = Path::new(&source.source_path);
            let already_reported = admission_report.0.failed_sources.iter().any(|failed| {
                failed.path.as_path() == path
                    && failed.reason == SourceFailureReason::ReadBudgetExceeded
            });
            if !already_reported {
                observe(path, SourceFailureReason::ReadBudgetExceeded);
            }
        }
    }
    true
}

/// Host predicates cannot return errors through FAD's boolean admission hook.
/// Retain bounded per-source evidence separately from terminal delivery/SQL
/// errors so one rejected prior proof never vetoes unrelated later sources.
#[derive(Default)]
pub(crate) struct SourceAdmissionFailures(Failures);

#[derive(Debug)]
struct AdmissionFailureReport(FailureReport);

impl fmt::Display for AdmissionFailureReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl SourceAdmissionFailures {
    pub(crate) fn record(&mut self, source: &DiscoveredSourceFile, error: anyhow::Error) {
        // Identity/content changes and genuine I/O failures already carry an
        // I/O kind. A malformed receipt is invalid evidence, never an inferred
        // permissions failure or a reason to forget previously occupied rows.
        let error = if error.downcast_ref::<io::Error>().is_none() {
            error.context(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid previous Codex source evidence",
            ))
        } else {
            error
        };
        self.0.report.reason = "source_admission_failures";
        self.0.record(source, error);
    }

    pub(crate) fn finish(self, scan_result: Result<()>) -> Result<()> {
        let Failures {
            report,
            budgets,
            first_error,
        } = self.0;
        if report.failed_source_count == 0 {
            return scan_result;
        }
        // Preserve the existing error's typed causes, including terminal
        // consumer failures, while adding a distinct report type so the
        // visitor can still see the parser's own source-failure report.
        // Budget-only refusals intentionally have no first_error; they still
        // make this admission incomplete and must retain their typed evidence.
        let error = scan_result
            .err()
            .or(first_error)
            .unwrap_or_else(|| budgets.into());
        Err(error.context(AdmissionFailureReport(report)))
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
            let configured_limit = self.budgets.limit_bytes;
            self.budgets.rejected_sources.extend(
                budget
                    .rejected_sources
                    .iter()
                    .take(available)
                    .map(|sample| RejectedSource {
                        source_path: sample.source_path.clone(),
                        observed_bytes: sample.observed_bytes,
                        limit_bytes: {
                            let actual = sample.limit_bytes.unwrap_or(budget.limit_bytes);
                            (actual != configured_limit).then_some(actual)
                        },
                    }),
            );
            self.budgets.omitted_source_count = self
                .budgets
                .rejected_source_count
                .saturating_sub(self.budgets.rejected_sources.len());
            SourceFailureReason::ReadBudgetExceeded
        } else {
            let reason = match error.downcast_ref::<io::Error>().map(io::Error::kind) {
                Some(io::ErrorKind::UnexpectedEof) => SourceFailureReason::UnfinishedSource,
                Some(io::ErrorKind::Interrupted) => SourceFailureReason::SourceChanged,
                Some(io::ErrorKind::NotFound) => SourceFailureReason::SourceMissing,
                Some(io::ErrorKind::PermissionDenied) => SourceFailureReason::SourceUnreadable,
                Some(io::ErrorKind::InvalidData) => SourceFailureReason::InvalidSourceData,
                // A compressed rollout whose decoded text passed the budget
                // without declaring its length up front (GH #513).
                Some(io::ErrorKind::FileTooLarge) => SourceFailureReason::ReadBudgetExceeded,
                _ => SourceFailureReason::SourceReadFailed,
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
                path: source.source_path.clone(),
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
        .filter(|relative| !relative.as_os_str().is_empty())
        .and_then(|relative| rollout_session_path(relative).to_str().map(str::to_owned))
        .or_else(|| {
            rollout_session_path(Path::new(source.source_path.file_name()?))
                .to_str()
                .map(str::to_owned)
        })
}

fn scan_source_strict(
    inner: &dyn Connector,
    ctx: &ScanContext,
    source: &DiscoveredSourceFile,
    hooks: &mut SourceScanHooks<'_>,
    on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    admission: &ScanAdmission,
    enrich: &mut dyn FnMut(&mut NormalizedConversation) -> Result<()>,
) -> (Result<()>, bool) {
    let consumer_failed = Cell::new(false);
    let visited = Cell::new(false);
    let scope_changed = Cell::new(false);
    let directory_id = archives::session_id(ctx, source)
        .map(Some)
        .or_else(|| (source.scan_root != source.source_path).then(|| directory_session_id(source)));
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
    let result = scan_with_admission(
        inner,
        &scoped_context(ctx, source),
        &mut guarded,
        &mut deliver,
        admission,
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

fn prefix_session_id(ctx: &ScanContext, source: &DiscoveredSourceFile) -> Option<String> {
    archives::session_id(ctx, source).or_else(|| {
        if source.scan_root != source.source_path {
            return directory_session_id(source);
        }
        // Match FAD's explicit-file contract before substituting a private
        // parser path. A filename-only ID would duplicate dated sessions.
        source
            .source_path
            .ancestors()
            .find(|ancestor| ancestor.file_name().is_some_and(|name| name == "sessions"))
            .and_then(|sessions| source.source_path.strip_prefix(sessions).ok())
            .and_then(|relative| rollout_session_path(relative).to_str().map(str::to_owned))
            .or_else(|| directory_session_id(source))
    })
}

fn attach_prefix_receipt(
    conversation: &mut NormalizedConversation,
    receipt: &StablePrefixReceipt,
) -> Result<()> {
    if !conversation.metadata.is_object() {
        conversation.metadata = serde_json::json!({});
    }
    let cass = conversation
        .metadata
        .as_object_mut()
        .expect("metadata is an object")
        .entry("cass")
        .or_insert_with(|| serde_json::json!({}));
    if !cass.is_object() {
        *cass = serde_json::json!({});
    }
    cass["codex_stable_prefix"] = serde_json::to_value(receipt)?;
    Ok(())
}

/// Parse one immutable bounded view through the pinned FAD parser and the
/// existing enrichment path. Host hooks only observe the original identity.
fn scan_source(
    inner: &dyn Connector,
    ctx: &ScanContext,
    source: &DiscoveredSourceFile,
    hooks: &mut SourceScanHooks<'_>,
    on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    admission: &ScanAdmission,
    enrich: &mut dyn FnMut(&mut NormalizedConversation) -> Result<()>,
) -> (Result<()>, bool) {
    if !stable_prefix::eligible(&source.source_path) {
        return scan_source_strict(
            inner,
            ctx,
            source,
            hooks,
            on_conversation,
            admission,
            enrich,
        );
    }
    let consumer_failed = Cell::new(false);
    let result = (|| -> Result<()> {
        // Exclusions ran on the real source before reaching this function.
        // In particular, the host may veto copying any bytes at all.
        stable_prefix::clear_previous_prefix_receipt();
        let source = source.clone().with_fs_metadata();
        if !hooks
            .should_scan_source
            .as_mut()
            .is_none_or(|predicate| predicate(&source))
        {
            stable_prefix::clear_previous_prefix_receipt();
            return Ok(());
        }
        let previous = stable_prefix::take_previous_prefix_receipt(&source.source_path)?;
        if admission.exclusions.excludes(&source.source_path) {
            return Ok(());
        }
        let limit = admission.limits.for_path(&source.source_path);
        if let Some(size) = super::observed_over_limit_with_limit(&source, limit) {
            let mut state = super::ScanState {
                limits: admission.limits,
                incomplete: admission.limits.incomplete(),
                ..super::ScanState::default()
            };
            state.reject(&source.source_path, size);
            return Err(state.incomplete.into());
        }
        // Preserve the existing source-missing diagnostic on a deletion race.
        std::fs::symlink_metadata(&source.source_path)?;
        if !stable_prefix::eligible(&source.source_path) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Codex source identity changed during prefix admission",
            )
            .into());
        }
        let Some(snapshot) = StablePrefixSnapshot::capture(
            &source.source_path,
            limit,
            previous.as_ref(),
            ctx.progress_tick.as_deref(),
        )?
        else {
            // Inputs with no admitted record boundary retain their existing
            // strict semantics. The original host predicate already ran.
            let mut admitted = |_: &DiscoveredSourceFile| true;
            let mut complete = |done: &SourceCompletion| {
                hooks
                    .on_source_complete
                    .as_mut()
                    .map_or(Ok(()), |sink| sink(done))
            };
            let mut strict_hooks = SourceScanHooks {
                should_scan_source: Some(&mut admitted),
                on_source_complete: Some(&mut complete),
            };
            let (result, failed) = scan_source_strict(
                inner,
                ctx,
                &source,
                &mut strict_hooks,
                on_conversation,
                admission,
                enrich,
            );
            consumer_failed.set(failed);
            return result;
        };
        let mut parser_ctx = scoped_context(ctx, &source);
        parser_ctx.scan_roots = parser_ctx
            .scan_roots
            .iter()
            .map(|root| root.with_path(snapshot.path().to_path_buf()))
            .collect();
        let seen = Cell::new(false);
        let completed = Cell::new(false);
        let emitted = Cell::new(0_usize);
        let receipt = std::cell::RefCell::new(None::<StablePrefixReceipt>);
        let mut before = |observed: &DiscoveredSourceFile| {
            let matches = observed.source_path == snapshot.path();
            seen.set(seen.get() || matches);
            matches
        };
        let mut complete_view = |done: &SourceCompletion| {
            anyhow::ensure!(
                done.source.source_path == snapshot.path()
                    && done.conversations_emitted == emitted.get(),
                "Codex prefix parser completed an unexpected source"
            );
            completed.set(true);
            Ok(())
        };
        let mut deliver = |mut conversation: NormalizedConversation| {
            anyhow::ensure!(
                conversation.source_path == snapshot.path() && emitted.get() == 0,
                "Codex prefix parser delivered an unexpected source"
            );
            snapshot.validate(ctx.progress_tick.as_deref())?;
            conversation.source_path.clone_from(&source.source_path);
            conversation.external_id = prefix_session_id(ctx, &source);
            let evidence = snapshot.receipt_for(Some(&conversation), previous.as_ref())?;
            attach_prefix_receipt(&mut conversation, &evidence)?;
            {
                let _delivery = PrefixDeliveryGuard::enter(evidence.clone());
                on_conversation(conversation).inspect_err(|_| consumer_failed.set(true))?;
            }
            // Delivery may have side effects. Never certify a changed source,
            // even if an earlier bounded canonical batch already committed.
            snapshot.validate(ctx.progress_tick.as_deref())?;
            *receipt.borrow_mut() = Some(evidence);
            emitted.set(1);
            Ok(())
        };
        let _parser_view = stable_prefix::PrefixParserViewGuard::enter(&snapshot);
        scan_with_admission(
            inner,
            &parser_ctx,
            &mut SourceScanHooks {
                should_scan_source: Some(&mut before),
                on_source_complete: Some(&mut complete_view),
            },
            &mut deliver,
            admission,
            enrich,
        )?;
        anyhow::ensure!(
            seen.get() && (completed.get() || emitted.get() == 0),
            "Codex immutable parser view was not completely consumed"
        );
        if emitted.get() == 0 {
            // scan_with_admission already ran the real zero-output validator.
            // A valid header-only/blank prefix is not a fabricated completion.
            // With an unfinished suffix it made no ingest progress at all:
            // retain the existing zero-message failure/retry contract.
            snapshot.validate(ctx.progress_tick.as_deref())?;
            let evidence = snapshot.receipt_for(None, None)?;
            if evidence.committed_bytes < evidence.admitted_bytes {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "Codex source has an unfinished tail and no complete messages",
                )
                .into());
            }
            anyhow::ensure!(
                previous
                    .as_ref()
                    .is_none_or(|receipt| receipt.message_count == 0),
                "Codex source no longer projects its previously published messages"
            );
            return Ok(());
        }
        let evidence = match receipt.into_inner() {
            Some(receipt) => snapshot.finish_receipt(receipt)?,
            None => snapshot.receipt_for(None, previous.as_ref())?,
        };
        let mut completed_source = source;
        if evidence.partial {
            // Existing generic consumers must not mistake this callback for
            // whole-file stability. Only the scoped typed evidence authorizes
            // CASS's distinct prefix-publication receipt.
            completed_source.size_bytes = None;
            completed_source.modified_at_ms = None;
        }
        let _completion = PrefixDeliveryGuard::enter_completion(evidence);
        hooks.on_source_complete.as_mut().map_or(Ok(()), |sink| {
            sink(&SourceCompletion {
                source: completed_source,
                required_sidecars: Vec::new(),
                conversations_emitted: emitted.get(),
            })
            .inspect_err(|_| consumer_failed.set(true))
        })
    })();
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
    // Resolve once, before discovery or callbacks can perform any work.
    let limits = ScanLimits::from_env()?;
    // Since franken-agent-detection 0.3.1 the primary rollout reader applies
    // its own admission budget (100 MiB by default) and refused every larger
    // rollout as invalid data after this policy had admitted it. Hand it the
    // same policy.
    franken_agent_detection::connectors::codex::set_codex_rollout_byte_budget(limits.jsonl_bytes);
    let admission = ScanAdmission {
        exclusions: ScanExclusions::from_env(),
        limits,
    };
    let (sources, discovery_result) = archives::discover_for_scan(ctx)?.into_parts();
    let mut failures = Failures {
        budgets: limits.incomplete(),
        ..Failures::default()
    };
    for source in sources {
        if admission.exclusions.excludes(&source.source_path) {
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
            &admission,
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
    match (discovery_result, failures.finish()) {
        (Err(discovery_error), Err(source_error)) => {
            // Retain discovery as the typed cause; this mixed failure must
            // never be mistaken for a budget-only refusal by its caller.
            Err(discovery_error.context(source_error))
        }
        (Err(error), Ok(())) => Err(error),
        (Ok(()), result) => result,
    }
}

#[cfg(test)]
mod contract_tests;
#[cfg(test)]
mod resume_tests;
#[cfg(test)]
mod tests;
