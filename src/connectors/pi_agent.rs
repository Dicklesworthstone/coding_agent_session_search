//! CASS adapter for `franken_agent_detection::connectors::pi_agent`.
//!
//! FAD owns parsing and discovery. This adapter enforces CASS's first-class
//! OMP identity boundary when a broad copied-home or remote-mirror root makes
//! Pi's permissive explicit-root detection walk into an OMP store.

use anyhow::Result;

use super::{
    Connector, DetectionResult, DiscoveredSourceFile, NormalizedConversation, ScanContext,
};

pub struct PiAgentConnector {
    inner: franken_agent_detection::PiAgentConnector,
}

impl Default for PiAgentConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl PiAgentConnector {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            inner: franken_agent_detection::PiAgentConnector::new(),
        }
    }
}

impl Connector for PiAgentConnector {
    fn detect(&self) -> DetectionResult {
        self.inner.detect()
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        let mut conversations = self.inner.scan(ctx)?;
        conversations
            .retain(|conversation| !super::omp::owns_session_path(&conversation.source_path));
        Ok(conversations)
    }

    fn discover_source_files(&self, ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        let mut sources = self.inner.discover_source_files(ctx)?;
        sources.retain(|source| !super::omp::owns_session_path(&source.source_path));
        Ok(sources)
    }
}
