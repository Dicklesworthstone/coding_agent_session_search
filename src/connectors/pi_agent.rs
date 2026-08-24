//! CASS adapter for `franken_agent_detection::connectors::pi_agent`.
//!
//! FAD owns parsing and discovery. This adapter enforces CASS's first-class
//! OMP identity boundary when a broad copied-home or remote-mirror root makes
//! Pi's permissive explicit-root detection walk into an OMP store.

use std::path::Path;

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

fn is_omp_store_path(path: &Path) -> bool {
    let normalized = path.to_string_lossy().replace('\\', "/");
    if normalized.contains("/.omp/")
        || normalized.ends_with("/.omp")
        || normalized.starts_with(".omp/")
        || normalized == ".omp"
        || normalized.contains("/omp/sessions/")
        || normalized.contains("/omp/profiles/")
        || normalized.starts_with("omp/sessions/")
        || normalized.starts_with("omp/profiles/")
    {
        return true;
    }

    if let Some(root) = dotenvy::var("PI_CODING_AGENT_SESSION_DIR")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(std::path::PathBuf::from)
        && path.starts_with(root)
    {
        return true;
    }

    if let Some(config_name) = dotenvy::var("PI_CONFIG_DIR")
        .ok()
        .filter(|value| !value.trim().is_empty())
        && let Some(home) = dirs::home_dir()
        && path.starts_with(home.join(config_name.trim_start_matches(['/', '\\'])))
    {
        return true;
    }

    false
}

impl Connector for PiAgentConnector {
    fn detect(&self) -> DetectionResult {
        self.inner.detect()
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        let mut conversations = self.inner.scan(ctx)?;
        conversations.retain(|conversation| !is_omp_store_path(&conversation.source_path));
        Ok(conversations)
    }

    fn discover_source_files(&self, ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        let mut sources = self.inner.discover_source_files(ctx)?;
        sources.retain(|source| !is_omp_store_path(&source.source_path));
        Ok(sources)
    }
}
