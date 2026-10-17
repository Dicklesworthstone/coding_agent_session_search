//! Codex history from active and archived rollout collections.

mod archives;
#[path = "codex/implementation.rs"]
mod implementation;
#[doc(hidden)]
pub mod layout_identity;
// Also consumed by raw-mirror admission, including the legacy fallback route.
pub(crate) mod path_policy;

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::{
    Connector, DetectionResult, DiscoveredSourceFile, NormalizedConversation, NormalizedMessage,
    ScanContext, ScanRoot, parse_timestamp, reindex_messages,
};

// Keep the existing crate-internal message projection entry point available.
// These re-exports serve consumers outside this module (ingest diagnostics),
// which the standalone Codex/raw-mirror contract crates do not stage.
#[allow(unused_imports)]
pub(crate) use implementation::modern_codex_message;
#[cfg(test)]
#[allow(unused_imports)]
pub(crate) use implementation::over_budget_scan_error;
#[allow(unused_imports)]
pub(crate) use implementation::over_budget_sources;

/// Archive-aware public surface; parsing and enrichment live in implementation.
#[derive(Default)]
pub struct CodexConnector {
    inner: implementation::CodexConnector,
    archive_data_dir: Option<PathBuf>,
}

impl CodexConnector {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind receipt lookup to the receiving archive without changing the
    /// source-root semantics of ScanContext.
    #[must_use]
    pub fn with_archive_data_dir(mut self, data_dir: &Path) -> Self {
        self.archive_data_dir = Some(data_dir.to_path_buf());
        self
    }
}

impl Connector for CodexConnector {
    fn detect(&self) -> DetectionResult {
        archives::detect(&self.inner)
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        let mut conversations = Vec::new();
        self.scan_with_callback(ctx, &mut |conversation| {
            conversations.push(conversation);
            Ok(())
        })?;
        Ok(conversations)
    }

    fn supports_streaming_scan(&self) -> bool {
        self.inner.supports_streaming_scan()
    }

    fn discover_source_files(&self, ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        path_policy::ScanExclusions::from_env().validate()?;
        archives::discover(&self.inner, ctx)
    }

    fn scan_with_callback(
        &self,
        ctx: &ScanContext,
        on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    ) -> Result<()> {
        self.scan_with_source_boundaries(
            ctx,
            &mut franken_agent_detection::SourceScanHooks::default(),
            on_conversation,
        )
    }

    fn supports_source_boundaries(&self) -> bool {
        self.inner.supports_source_boundaries()
    }

    fn scan_with_source_boundaries(
        &self,
        ctx: &ScanContext,
        hooks: &mut franken_agent_detection::SourceScanHooks<'_>,
        on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    ) -> Result<()> {
        path_policy::ScanExclusions::from_env().validate()?;
        // FAD's data_dir can name the source root or even a single rollout.
        // Runtime indexing binds the receiving archive separately; standalone
        // scans and migration proof parsing retain their supplied context.
        let archive_data_dir = self.archive_data_dir.as_deref().unwrap_or(&ctx.data_dir);
        let current_source = RefCell::new(None);
        let franken_agent_detection::SourceScanHooks {
            should_scan_source,
            on_source_complete,
        } = hooks;
        let mut before = |source: &DiscoveredSourceFile| {
            let admitted = should_scan_source
                .as_mut()
                .is_none_or(|predicate| predicate(source));
            *current_source.borrow_mut() = admitted.then(|| source.clone());
            admitted
        };
        let mut deliver = |conversation: NormalizedConversation| {
            let source = current_source.borrow();
            let source = source
                .as_ref()
                .context("Codex delivery has no admitted source boundary")?;
            layout_identity::deliver(archive_data_dir, ctx, source, conversation, on_conversation)
        };
        // Recovery normalizes the physical source's directory identity before
        // delivery. Receipt proof and canonicalization must finish before the
        // original source-completion callback can advance its durable ledger.
        let mut complete = |completion: &franken_agent_detection::SourceCompletion| {
            on_source_complete
                .as_mut()
                .map_or(Ok(()), |sink| sink(completion))
        };
        self.inner.scan_with_source_boundaries(
            ctx,
            &mut franken_agent_detection::SourceScanHooks {
                should_scan_source: Some(&mut before),
                on_source_complete: Some(&mut complete),
            },
            &mut deliver,
        )
    }
}
