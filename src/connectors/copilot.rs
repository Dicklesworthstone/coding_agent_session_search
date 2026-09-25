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

    /// Native VS Code chat stores the upstream connector would scan in `ctx`:
    /// `workspaceStorage/*/chatSessions`, the empty-window and transferred
    /// session stores, and legacy `state.vscdb` sessions. Not detected when
    /// discovery finds no session source.
    fn native_store_detection(&self, ctx: &ScanContext) -> DetectionResult {
        let Ok(files) = self.inner.discover_source_files(ctx) else {
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
    /// that history was never indexed. Detect those stores too; when neither
    /// is present the upstream (not-found) result and its evidence stand.
    fn detect(&self) -> DetectionResult {
        let upstream = self.inner.detect();
        if upstream.detected {
            return upstream;
        }
        // An empty data dir never exists, so the upstream connector resolves
        // its default per-user VS Code roots, exactly as a default scan does.
        let native = self.native_store_detection(&ScanContext::local_default(PathBuf::new(), None));
        if native.detected { native } else { upstream }
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        self.inner.scan(ctx)
    }

    fn discover_source_files(&self, ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        self.inner.discover_source_files(ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::ScanRoot;

    fn scan_home(home: &std::path::Path) -> ScanContext {
        ScanContext::with_roots(
            home.join("cass-data"),
            vec![ScanRoot::local(home.to_path_buf())],
            None,
        )
    }

    /// A home whose only Copilot history is VS Code's native chat store is
    /// detected, naming the session file and the root it was found under.
    #[test]
    fn native_chat_store_alone_is_detected() {
        let home = tempfile::tempdir().unwrap();
        let sessions = home
            .path()
            .join(".config/Code/User/workspaceStorage/ws-1/chatSessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let session = sessions.join("session-1.json");
        std::fs::write(&session, br#"{"version":3,"requests":[]}"#).unwrap();

        let detection = CopilotConnector::new().native_store_detection(&scan_home(home.path()));
        assert!(detection.detected, "{detection:?}");
        assert_eq!(detection.root_paths, vec![home.path().to_path_buf()]);
        assert!(
            detection
                .evidence
                .iter()
                .any(|line| line.contains("session-1.json")),
            "{detection:?}"
        );
    }

    /// Negative controls: no store, or only a workspace.json sidecar, is not
    /// Copilot history.
    #[test]
    fn home_without_a_native_session_is_not_detected() {
        let home = tempfile::tempdir().unwrap();
        let connector = CopilotConnector::new();
        assert!(
            !connector
                .native_store_detection(&scan_home(home.path()))
                .detected
        );

        let workspace = home.path().join(".config/Code/User/workspaceStorage/ws-1");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(
            workspace.join("workspace.json"),
            br#"{"folder":"file:///w"}"#,
        )
        .unwrap();
        assert!(
            !connector
                .native_store_detection(&scan_home(home.path()))
                .detected
        );
    }
}
