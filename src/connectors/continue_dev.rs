//! CASS admission and retry semantics for Continue's single-file snapshots.
//!
//! The pinned FAD parser owns discovery, normalization and source observations.
//! This adapter applies operator exclusions before content reads and lets the
//! durable source ledger, when supplied, override a global timestamp cutoff.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use anyhow::{Result, ensure};
use franken_agent_detection::connectors::{SourceCompletion, SourceScanHooks};

use super::codex::path_policy::ScanExclusions;
use super::{Connector, DetectionResult, DiscoveredSourceFile, NormalizedConversation, ScanContext};

#[derive(Default)]
pub struct ContinueConnector {
    inner: franken_agent_detection::connectors::continue_dev::ContinueConnector,
}

// FAD can skip an unreadable/invalid snapshot without returning an error. Track
// its sequential lifecycle rather than certifying an aggregate successful scan
// after an admitted source failed to complete. Retain at most one observation
// and one diagnostic path, not a transcript-sized list of failed sources.
#[derive(Default)]
struct SourceProgress {
    pending: Option<DiscoveredSourceFile>,
    incomplete: usize,
    first_incomplete: Option<PathBuf>,
}

impl SourceProgress {
    fn finish_pending(&mut self) {
        if let Some(source) = self.pending.take() {
            self.incomplete = self.incomplete.saturating_add(1);
            self.first_incomplete.get_or_insert(source.source_path);
        }
    }

    fn finish(mut self) -> Result<()> {
        self.finish_pending();
        if let Some(path) = self.first_incomplete {
            anyhow::bail!(
                "Continue source scan incomplete: {} admitted snapshot(s) did not complete; \
                 first source: {}. Retry after resolving unreadable, invalid or changing snapshots",
                self.incomplete,
                path.display()
            );
        }
        Ok(())
    }
}

fn selected_scope(ctx: &ScanContext, excludes: &dyn Fn(&Path) -> bool) -> Option<ScanContext> {
    let mut scope = ctx.clone();
    if !ctx.use_default_detection() {
        scope.scan_roots.retain(|root| !excludes(&root.path));
        // All explicitly selected roots excluded is an empty selection, not
        // permission to inspect the implicit Continue home instead.
        if scope.scan_roots.is_empty() {
            return None;
        }
    }
    Some(scope)
}

fn source_excluded(source: &DiscoveredSourceFile, excludes: &dyn Fn(&Path) -> bool) -> bool {
    excludes(&source.source_path) || excludes(&source.scan_root)
}

impl ContinueConnector {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn discover_allowed(
        &self,
        ctx: &ScanContext,
        excludes: &dyn Fn(&Path) -> bool,
    ) -> Result<Vec<DiscoveredSourceFile>> {
        let Some(scope) = selected_scope(ctx, excludes) else {
            return Ok(Vec::new());
        };
        let mut sources = self.inner.discover_source_files(&scope)?;
        sources.retain(|source| !source_excluded(source, excludes));
        Ok(sources)
    }

    fn scan_allowed(
        &self,
        ctx: &ScanContext,
        hooks: &mut SourceScanHooks<'_>,
        on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
        excludes: &dyn Fn(&Path) -> bool,
    ) -> Result<()> {
        let Some(mut scope) = selected_scope(ctx, excludes) else {
            return Ok(());
        };
        if hooks.should_scan_source.is_some() {
            // An old failed snapshot can become readable without an mtime
            // change. Discover it before asking the authoritative ledger.
            scope.since_ts = None;
        }
        let progress = RefCell::new(SourceProgress::default());
        {
            let SourceScanHooks {
                should_scan_source,
                on_source_complete,
            } = hooks;
            let mut admit = |source: &DiscoveredSourceFile| {
                progress.borrow_mut().finish_pending();
                if source_excluded(source, excludes)
                    || !should_scan_source
                        .as_mut()
                        .is_none_or(|predicate| predicate(source))
                {
                    return false;
                }
                // A host hook can change filesystem aliases. Recheck policy
                // before FAD opens the admitted source; no decision is cached
                // across hooks. This is not a handle-bound filesystem sandbox.
                if source_excluded(source, excludes) {
                    return false;
                }
                progress.borrow_mut().pending = Some(source.clone());
                true
            };
            let mut complete = |completion: &SourceCompletion| {
                ensure!(
                    progress.borrow().pending.as_ref() == Some(&completion.source),
                    "Continue source completion does not match the admitted observation"
                );
                if source_excluded(&completion.source, excludes) {
                    // Leave it pending: a policy/namespace change is not a
                    // successful checkpoint, even if content was delivered.
                    return Ok(());
                }
                if let Some(sink) = on_source_complete.as_mut() {
                    sink(completion)?;
                }
                progress.borrow_mut().pending = None;
                Ok(())
            };
            self.inner.scan_with_source_boundaries(
                &scope,
                &mut SourceScanHooks {
                    should_scan_source: Some(&mut admit),
                    on_source_complete: Some(&mut complete),
                },
                on_conversation,
            )?;
        }
        progress.into_inner().finish()
    }
}

impl Connector for ContinueConnector {
    fn detect(&self) -> DetectionResult {
        self.inner.detect()
    }

    fn supports_streaming_scan(&self) -> bool {
        self.inner.supports_streaming_scan()
    }

    fn supports_source_boundaries(&self) -> bool {
        self.inner.supports_source_boundaries()
    }

    fn discover_source_files(&self, ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        let exclusions = ScanExclusions::from_env();
        exclusions.validate()?;
        self.discover_allowed(ctx, &|path| exclusions.excludes(path))
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        let mut conversations = Vec::new();
        self.scan_with_callback(ctx, &mut |conversation| {
            conversations.push(conversation);
            Ok(())
        })?;
        Ok(conversations)
    }

    fn scan_with_callback(
        &self,
        ctx: &ScanContext,
        on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    ) -> Result<()> {
        self.scan_with_source_boundaries(ctx, &mut SourceScanHooks::default(), on_conversation)
    }

    fn scan_with_source_boundaries(
        &self,
        ctx: &ScanContext,
        hooks: &mut SourceScanHooks<'_>,
        on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    ) -> Result<()> {
        let exclusions = ScanExclusions::from_env();
        exclusions.validate()?;
        self.scan_allowed(ctx, hooks, on_conversation, &|path| exclusions.excludes(path))
    }
}

#[cfg(test)]
mod tests;
