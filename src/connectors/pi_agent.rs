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
        let ownership = super::omp::PiFamilyOwnership::live();
        let mut detection = self.inner.detect();
        detection
            .root_paths
            .retain(|root| ownership.owner(root) == super::omp::PiFamilyOwner::PiAgent);
        for root in ownership.pi_detection_roots() {
            let canonical = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
            let already_reported = detection.root_paths.iter().any(|existing| {
                std::fs::canonicalize(existing).unwrap_or_else(|_| existing.clone()) == canonical
            });
            if !already_reported {
                detection.evidence.push(format!(
                    "CASS Pi-family ownership policy found Pi Agent root: {}",
                    root.display()
                ));
                detection.root_paths.push(root);
            }
        }
        detection.detected = !detection.root_paths.is_empty();
        detection
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        let ownership = super::omp::PiFamilyOwnership::live();
        let mut conversations = self.inner.scan(ctx)?;

        if !ctx.use_default_detection() {
            let direct_roots = ctx
                .scan_roots
                .iter()
                .filter(|root| ownership.owner(&root.path) == super::omp::PiFamilyOwner::PiAgent)
                .map(|root| root.path.clone())
                .collect::<Vec<_>>();
            if !direct_roots.is_empty() {
                let fallback = franken_agent_detection::connectors::pi_wire::scan_homes(
                    &direct_roots,
                    ctx,
                    "pi_agent",
                )?;
                super::omp::append_missing_conversations(&mut conversations, fallback);
            }
        }

        conversations.retain(|conversation| {
            ownership.owner(&conversation.source_path) != super::omp::PiFamilyOwner::Omp
        });
        super::omp::promote_transcript_session_ids(&mut conversations);
        Ok(conversations)
    }

    fn discover_source_files(&self, ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        let ownership = super::omp::PiFamilyOwnership::live();
        let mut sources = self.inner.discover_source_files(ctx)?;

        if !ctx.use_default_detection() {
            let direct_roots = ctx
                .scan_roots
                .iter()
                .filter(|root| ownership.owner(&root.path) == super::omp::PiFamilyOwner::PiAgent)
                .cloned()
                .collect::<Vec<_>>();
            if !direct_roots.is_empty() {
                let fallback = franken_agent_detection::connectors::pi_wire::discover_sources(
                    &direct_roots,
                    ctx,
                    "pi_agent",
                );
                super::omp::append_missing_sources(&mut sources, fallback);
            }
        }

        sources.retain(|source| {
            ownership.owner(&source.source_path) != super::omp::PiFamilyOwner::Omp
        });
        Ok(sources)
    }
}
