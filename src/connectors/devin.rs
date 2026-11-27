//! Devin CLI discovery with the native Windows roaming-data location.
//!
//! Parsing remains in FAD. The pinned connector only defaults to the Unix
//! location; this adapter supplies the Windows store to detection, discovery,
//! batch scans, and watch/retry factories without widening explicit selections.

use std::borrow::Cow;
use std::path::PathBuf;

use anyhow::Result;

use super::{
    Connector, DetectionResult, DiscoveredSourceFile, NormalizedConversation, ScanContext, ScanRoot,
};

pub struct DevinConnector {
    inner: franken_agent_detection::DevinConnector,
    windows_default: Option<PathBuf>,
}

impl Default for DevinConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl DevinConnector {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: franken_agent_detection::DevinConnector::new(),
            windows_default: windows_default_database(),
        }
    }

    fn detected_windows_default(&self, override_active: bool) -> Option<DetectionResult> {
        let database = self.windows_default.as_ref()?;
        if override_active || !database.is_file() {
            return None;
        }
        Some(DetectionResult {
            detected: true,
            evidence: vec![format!(
                "Devin Windows roaming database exists: {}",
                database.display()
            )],
            root_paths: vec![database.clone()],
        })
    }

    fn scan_context<'a>(
        &self,
        ctx: &'a ScanContext,
        override_active: bool,
    ) -> Cow<'a, ScanContext> {
        // Keep FAD's precedence for explicit roots, a selected database, a
        // selected store directory, and CASS_DEVIN_DATA_ROOT. In particular,
        // a missing explicit selection must never fall back to another store.
        if !ctx.use_default_detection()
            || ctx
                .data_dir
                .extension()
                .is_some_and(|extension| extension == "db")
            || override_active
            || (!ctx.data_dir.as_os_str().is_empty()
                && franken_agent_detection::DevinConnector::database_path_from_root(&ctx.data_dir)
                    .is_file())
        {
            return Cow::Borrowed(ctx);
        }
        let Some(database) = self.windows_default.as_ref().filter(|path| path.is_file()) else {
            // Retain upstream's legacy Unix-style fallback when the native
            // Windows store is absent, including on non-Windows hosts.
            return Cow::Borrowed(ctx);
        };
        let mut selected = ctx.clone();
        selected.scan_roots = vec![ScanRoot::local(database.clone())];
        Cow::Owned(selected)
    }
}

fn override_active() -> bool {
    dotenvy::var("CASS_DEVIN_DATA_ROOT")
        .ok()
        .is_some_and(|value| !value.trim().is_empty())
}

#[cfg(windows)]
fn windows_default_database() -> Option<PathBuf> {
    // Honor the native APPDATA location, including redirected profiles.
    // Fall back to the OS known-folder lookup if the variable is absent.
    dotenvy::var("APPDATA")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .or_else(dirs::data_dir)
        .map(|root| root.join("devin").join("cli").join("sessions.db"))
}

#[cfg(not(windows))]
fn windows_default_database() -> Option<PathBuf> {
    None
}

impl Connector for DevinConnector {
    fn detect(&self) -> DetectionResult {
        self.detected_windows_default(override_active())
            .unwrap_or_else(|| self.inner.detect())
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        self.inner.scan(&self.scan_context(ctx, override_active()))
    }

    fn discover_source_files(&self, ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        self.inner
            .discover_source_files(&self.scan_context(ctx, override_active()))
    }

    fn supports_streaming_scan(&self) -> bool {
        self.inner.supports_streaming_scan()
    }

    fn scan_with_callback(
        &self,
        ctx: &ScanContext,
        on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    ) -> Result<()> {
        self.inner
            .scan_with_callback(&self.scan_context(ctx, override_active()), on_conversation)
    }

    fn supports_source_boundaries(&self) -> bool {
        self.inner.supports_source_boundaries()
    }

    fn scan_with_source_boundaries(
        &self,
        ctx: &ScanContext,
        hooks: &mut franken_agent_detection::connectors::SourceScanHooks<'_>,
        on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    ) -> Result<()> {
        self.inner.scan_with_source_boundaries(
            &self.scan_context(ctx, override_active()),
            hooks,
            on_conversation,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::Origin;

    fn fixture() -> (tempfile::TempDir, DevinConnector, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let database = temp
            .path()
            .join("AppData")
            .join("Roaming")
            .join("devin")
            .join("cli")
            .join("sessions.db");
        std::fs::create_dir_all(database.parent().unwrap()).unwrap();
        std::fs::write(&database, b"discovery fixture").unwrap();
        let connector = DevinConnector {
            inner: franken_agent_detection::DevinConnector::new(),
            windows_default: Some(database.clone()),
        };
        (temp, connector, database)
    }

    #[test]
    fn windows_default_is_shared_by_detection_and_source_discovery() {
        let (temp, connector, database) = fixture();
        let detection = connector.detected_windows_default(false).unwrap();
        assert!(detection.detected);
        assert_eq!(detection.root_paths, vec![database.clone()]);

        let ctx = ScanContext::local_default(temp.path().join("cass-data"), Some(1234));
        let selected = connector.scan_context(&ctx, false);
        assert!(matches!(&selected, Cow::Owned(_)));
        assert_eq!(selected.since_ts, ctx.since_ts);
        assert_eq!(selected.data_dir, ctx.data_dir);
        assert_eq!(selected.scan_roots.len(), 1);
        assert_eq!(selected.scan_roots[0].path, database);
        let sources = connector.inner.discover_source_files(&selected).unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].source_path, database);
    }

    #[test]
    fn explicit_remote_or_missing_roots_never_include_the_windows_default() {
        let (temp, connector, database) = fixture();
        for path in [temp.path().join("remote/sessions.db"), database] {
            let root = ScanRoot::remote(path, Origin::remote("work-laptop"), None);
            let ctx =
                ScanContext::with_roots(temp.path().join("cass-data"), vec![root], Some(9876));
            let selected = connector.scan_context(&ctx, false);
            assert!(matches!(selected, Cow::Borrowed(_)));
            assert_eq!(selected.scan_roots[0].origin.source_id, "work-laptop");
        }
    }

    #[test]
    fn explicit_override_and_scoped_database_keep_their_precedence() {
        let (temp, connector, _) = fixture();
        let ctx = ScanContext::local_default(temp.path().join("cass-data"), None);
        assert!(connector.detected_windows_default(true).is_none());
        assert!(matches!(
            connector.scan_context(&ctx, true),
            Cow::Borrowed(_)
        ));

        let missing = ScanContext::local_default(temp.path().join("missing.db"), None);
        assert!(matches!(
            connector.scan_context(&missing, false),
            Cow::Borrowed(_)
        ));

        let directory = temp.path().join("selected-store");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("sessions.db"), b"selected source").unwrap();
        let scoped = ScanContext::local_default(directory, None);
        assert!(matches!(
            connector.scan_context(&scoped, false),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn absent_windows_store_preserves_upstream_default_discovery() {
        let (temp, mut connector, _) = fixture();
        connector.windows_default = Some(temp.path().join("not-installed/sessions.db"));
        let ctx = ScanContext::local_default(temp.path().join("cass-data"), None);
        assert!(connector.detected_windows_default(false).is_none());
        assert!(matches!(
            connector.scan_context(&ctx, false),
            Cow::Borrowed(_)
        ));
        connector.windows_default = None;
        assert!(matches!(
            connector.scan_context(&ctx, false),
            Cow::Borrowed(_)
        ));
    }
}
