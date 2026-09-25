//! Connector for GitHub Copilot Chat session logs.
//!
//! Parsing lives in `franken_agent_detection::connectors::copilot`. CASS wraps
//! it only to widen detection (see [`CopilotConnector::detect`]).

use super::{
    Connector, DetectionResult, DiscoveredSourceFile, DiscoveredSourceRole, NormalizedConversation,
    ScanContext,
};
use anyhow::Result;
use std::path::PathBuf;

/// GitHub Copilot Chat (VS Code, VS Code Insiders, VSCodium).
#[derive(Default)]
pub struct CopilotConnector {
    inner: franken_agent_detection::CopilotConnector,
}

impl CopilotConnector {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Native VS Code chat stores the upstream connector would scan under its
    /// default roots: `workspaceStorage/*/chatSessions`, the empty-window and
    /// transferred session stores, and legacy `state.vscdb` sessions.
    fn native_store_detection(&self) -> DetectionResult {
        // An empty data dir never exists, so the upstream connector resolves
        // its default per-user VS Code roots, exactly as a default scan does.
        let ctx = ScanContext::local_default(PathBuf::new(), None);
        let Ok(files) = self.inner.discover_source_files(&ctx) else {
            return DetectionResult::not_found();
        };
        let mut evidence = Vec::new();
        let mut root_paths: Vec<PathBuf> = Vec::new();
        for file in files.iter().filter(|file| {
            file.required_for_reconstruction && file.role != DiscoveredSourceRole::MetadataSidecar
        }) {
            if !root_paths.contains(&file.scan_root) {
                root_paths.push(file.scan_root.clone());
            }
            if evidence.len() < 8 {
                evidence.push(format!(
                    "VS Code native chat store: {}",
                    file.source_path.display()
                ));
            }
        }
        if root_paths.is_empty() {
            return DetectionResult::not_found();
        }
        DetectionResult {
            detected: true,
            evidence,
            root_paths,
        }
    }
}

impl Connector for CopilotConnector {
    /// Upstream detection (franken-agent-detection 0.3.0) probes only the
    /// Copilot Chat *extension* store (`globalStorage/github.copilot-chat`),
    /// while its scanner also reads VS Code's native chat stores, which are
    /// where current VS Code keeps chat history. A user with only native
    /// history was never detected, so the indexer skipped the connector and
    /// that history was never indexed. Detect those stores too.
    fn detect(&self) -> DetectionResult {
        let upstream = self.inner.detect();
        if upstream.detected {
            return upstream;
        }
        self.native_store_detection()
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        self.inner.scan(ctx)
    }

    fn discover_source_files(&self, ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        self.inner.discover_source_files(ctx)
    }
}
