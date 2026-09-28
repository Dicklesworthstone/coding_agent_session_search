//! Codex history from active and archived rollout collections.

mod archives;
#[path = "codex/implementation.rs"]
mod implementation;
// Also consumed by raw-mirror admission, including the legacy fallback route.
pub(crate) mod path_policy;

use anyhow::Result;

use super::{
    Connector, DetectionResult, DiscoveredSourceFile, NormalizedConversation, NormalizedMessage,
    ScanContext, ScanRoot, parse_timestamp, reindex_messages,
};

// Keep the existing crate-internal message projection entry point available.
#[allow(unused_imports)]
pub(crate) use implementation::modern_codex_message;
pub(crate) use implementation::over_budget_sources;

/// Archive-aware public surface; parsing and enrichment live in implementation.
#[derive(Default)]
pub struct CodexConnector {
    inner: implementation::CodexConnector,
}

impl CodexConnector {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl Connector for CodexConnector {
    fn detect(&self) -> DetectionResult {
        archives::detect(&self.inner)
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        path_policy::ScanExclusions::from_env().validate()?;
        self.inner.scan(ctx)
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
        path_policy::ScanExclusions::from_env().validate()?;
        self.inner.scan_with_callback(ctx, on_conversation)
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
        self.inner
            .scan_with_source_boundaries(ctx, hooks, on_conversation)
    }
}
