//! Connector for Antigravity (agy) session logs.
//!
//! Parsing lives in `franken_agent_detection::connectors::antigravity`.
//! The pinned reader discovers conversation directories, so an explicitly
//! selected primary transcript needs the same single-conversation scope.

use std::borrow::Cow;
use std::path::Path;

use anyhow::Result;
use franken_agent_detection::connectors::SourceScanHooks;

use super::{
    Connector, DetectionResult, DiscoveredSourceFile, NormalizedConversation, ScanContext,
};

#[derive(Default)]
pub struct AntigravityConnector {
    inner: franken_agent_detection::AntigravityConnector,
}

impl AntigravityConnector {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            inner: franken_agent_detection::AntigravityConnector::new(),
        }
    }
}

/// Select the directory owning this exact primary transcript. Never widen a
/// database, fuller transcript, arbitrary JSONL file, or missing path to its
/// parent: doing so could import a different source than the caller selected.
fn transcript_conversation_dir(path: &Path) -> Option<&Path> {
    if path.file_name()? != "transcript.jsonl" || !path.is_file() {
        return None;
    }
    let logs = path
        .parent()
        .filter(|dir| dir.file_name().is_some_and(|name| name == "logs"))?;
    let generated = logs.parent().filter(|dir| {
        dir.file_name()
            .is_some_and(|name| name == ".system_generated")
    })?;
    generated.parent()
}

fn selected_context(ctx: &ScanContext) -> Cow<'_, ScanContext> {
    if !ctx
        .scan_roots
        .iter()
        .any(|root| transcript_conversation_dir(&root.path).is_some())
    {
        return Cow::Borrowed(ctx);
    }
    let mut selected = ctx.clone();
    for root in &mut selected.scan_roots {
        if let Some(conversation_dir) = transcript_conversation_dir(&root.path) {
            root.path = conversation_dir.to_path_buf();
        }
    }
    Cow::Owned(selected)
}

impl Connector for AntigravityConnector {
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
        self.inner.discover_source_files(&selected_context(ctx))
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        self.inner.scan(&selected_context(ctx))
    }

    fn scan_with_callback(
        &self,
        ctx: &ScanContext,
        on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    ) -> Result<()> {
        self.inner
            .scan_with_callback(&selected_context(ctx), on_conversation)
    }

    fn scan_with_source_boundaries(
        &self,
        ctx: &ScanContext,
        hooks: &mut SourceScanHooks<'_>,
        on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    ) -> Result<()> {
        self.inner
            .scan_with_source_boundaries(&selected_context(ctx), hooks, on_conversation)
    }
}
