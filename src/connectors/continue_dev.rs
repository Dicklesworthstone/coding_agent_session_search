//! CASS admission and retry semantics for Continue's single-file snapshots.
//!
//! CASS owns fallible metadata discovery; the pinned FAD parser still owns
//! normalization and source observations for each admitted snapshot.
//! This adapter applies operator exclusions before content reads and lets the
//! durable source ledger, when supplied, override a global timestamp cutoff.

mod discovery;

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use anyhow::{Result, ensure};
use franken_agent_detection::connectors::{SourceCompletion, SourceScanHooks};

use super::codex::path_policy::ScanExclusions;
use super::{
    Connector, DetectionResult, DiscoveredSourceFile, NormalizedConversation, ScanContext,
};

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

// Only the parser selector changes when narrowing to a discovered file.
fn same_observation(left: &DiscoveredSourceFile, right: &DiscoveredSourceFile) -> bool {
    left.provider_slug == right.provider_slug
        && left.source_path == right.source_path
        && left.role == right.role
        && left.origin == right.origin
        && left.platform == right.platform
        && left.required_for_reconstruction == right.required_for_reconstruction
        && left.size_bytes == right.size_bytes
        && left.modified_at_ms == right.modified_at_ms
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
        Ok(discovery::discover(&scope, excludes)?
            .into_iter()
            .map(|source| source.file)
            .filter(|source| !source_excluded(source, excludes))
            .collect())
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
        // Discover once. FAD's collecting discovery suppresses directory I/O
        // errors and scanning it again could silently lose a removed source.
        // Each inventory member now gets its own observed completion outcome.
        let sources = discovery::discover(&scope, excludes)?;
        let progress = RefCell::new(SourceProgress::default());
        for selected in sources {
            progress.borrow_mut().finish_pending();
            let source = &selected.file;
            if source_excluded(source, excludes) || !hooks.should_scan(source) {
                continue;
            }
            if source_excluded(source, excludes) {
                continue;
            }
            progress.borrow_mut().pending = Some(source.clone());
            // The inventory/ledger already admitted this source. A second
            // global cutoff must not make an admitted file vanish silently.
            // Construct just this root, not a clone of the whole root list.
            let mut single = ScanContext::with_roots(
                scope.data_dir.clone(),
                vec![selected.root.with_path(source.source_path.clone())],
                None,
            );
            single.progress_tick.clone_from(&scope.progress_tick);
            let mut admit = |observed: &DiscoveredSourceFile| {
                same_observation(source, observed) && !source_excluded(source, excludes)
            };
            let mut complete = |completion: &SourceCompletion| {
                ensure!(
                    progress.borrow().pending.as_ref() == Some(source)
                        && same_observation(source, &completion.source),
                    "Continue source completion does not match the admitted observation"
                );
                if source_excluded(source, excludes) || source.fs_metadata_changed() {
                    return Ok(());
                }
                // Narrowing the parser's selector does not change the ledger
                // identity. Discovery, admission and completion retain the
                // original expanded scan root, provenance and observations.
                hooks.complete(&SourceCompletion {
                    source: source.clone(),
                    required_sidecars: completion.required_sidecars.clone(),
                    conversations_emitted: completion.conversations_emitted,
                })?;
                progress.borrow_mut().pending = None;
                Ok(())
            };
            self.inner.scan_with_source_boundaries(
                &single,
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
        self.scan_allowed(ctx, hooks, on_conversation, &|path| {
            exclusions.excludes(path)
        })
    }
}

#[cfg(test)]
mod tests;
