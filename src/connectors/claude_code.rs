//! Claude Code connector.
//!
//! The parser lives in `franken_agent_detection`; this wrapper only makes
//! *detection* agree with the parser's own root resolver (GH #448).

use std::path::{Path, PathBuf};

use anyhow::Result;

use super::{
    Connector, DetectionResult, DiscoveredSourceFile, NormalizedConversation, ScanContext,
};

/// Claude Code roots implied by the env redirects Claude Code documents, in
/// the resolver's precedence order: an explicit `CLAUDE_CONFIG_DIR` replaces
/// everything else; otherwise `XDG_CONFIG_HOME` contributes
/// `<xdg>/claude-code`. Both the `projects` dir and its parent are listed so
/// a freshly redirected profile without sessions yet still counts as
/// installed (the scan then simply finds nothing). Pure so it can be tested
/// without touching the process environment; blank values are ignored.
pub(crate) fn claude_env_redirect_roots(
    claude_config_dir: Option<&str>,
    xdg_config_home: Option<&str>,
) -> Vec<PathBuf> {
    let nonempty = |value: Option<&str>| {
        value
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    };
    if let Some(config_dir) = nonempty(claude_config_dir) {
        return vec![config_dir.join("projects"), config_dir];
    }
    match nonempty(xdg_config_home) {
        Some(xdg) => {
            let claude_code = xdg.join("claude-code");
            vec![claude_code.join("projects"), claude_code]
        }
        None => Vec::new(),
    }
}

/// GH #448: the pinned `franken_agent_detection` registry probe only checks
/// `~/.claude`, `~/.config/claude`, and the macOS Desktop dirs, while the
/// connector's resolver honors `CLAUDE_CONFIG_DIR` / `XDG_CONFIG_HOME`. When
/// the probe says "not installed" but a redirected root exists, report it as
/// detected so the indexer asks the connector to scan (which is where the
/// env-aware resolver runs). Upstream FAD fixes the probe itself
/// (`env_override_roots("claude")` + XDG default root); this keeps cass
/// correct against the currently pinned crate.
fn detection_with_env_redirects(
    detected: DetectionResult,
    redirect_roots: &[PathBuf],
    exists: impl Fn(&Path) -> bool,
) -> DetectionResult {
    if detected.detected {
        return detected;
    }
    let existing: Vec<PathBuf> = redirect_roots
        .iter()
        .filter(|root| exists(root))
        .cloned()
        .collect();
    if existing.is_empty() {
        return detected;
    }
    let mut evidence = detected.evidence;
    evidence.extend(
        existing
            .iter()
            .map(|root| format!("env redirect root exists: {}", root.display())),
    );
    DetectionResult {
        detected: true,
        evidence,
        root_paths: existing,
    }
}

pub struct ClaudeCodeConnector {
    inner: franken_agent_detection::connectors::claude_code::ClaudeCodeConnector,
}

impl Default for ClaudeCodeConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl ClaudeCodeConnector {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            inner: franken_agent_detection::connectors::claude_code::ClaudeCodeConnector::new(),
        }
    }
}

impl Connector for ClaudeCodeConnector {
    fn detect(&self) -> DetectionResult {
        let redirect_roots = claude_env_redirect_roots(
            dotenvy::var("CLAUDE_CONFIG_DIR").ok().as_deref(),
            dotenvy::var("XDG_CONFIG_HOME").ok().as_deref(),
        );
        detection_with_env_redirects(self.inner.detect(), &redirect_roots, Path::exists)
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        self.inner.scan(ctx)
    }

    fn supports_streaming_scan(&self) -> bool {
        self.inner.supports_streaming_scan()
    }

    fn discover_source_files(&self, ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        self.inner.discover_source_files(ctx)
    }

    fn scan_with_callback(
        &self,
        ctx: &ScanContext,
        on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    ) -> Result<()> {
        self.inner.scan_with_callback(ctx, on_conversation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_roots_prefer_claude_config_dir_over_xdg() {
        assert_eq!(
            claude_env_redirect_roots(Some("/srv/tenant-a"), Some("/home/u/.config")),
            vec![
                PathBuf::from("/srv/tenant-a/projects"),
                PathBuf::from("/srv/tenant-a")
            ]
        );
        assert_eq!(
            claude_env_redirect_roots(None, Some("/home/u/.config")),
            vec![
                PathBuf::from("/home/u/.config/claude-code/projects"),
                PathBuf::from("/home/u/.config/claude-code")
            ]
        );
        assert_eq!(
            claude_env_redirect_roots(Some("  /srv/b "), None),
            vec![PathBuf::from("/srv/b/projects"), PathBuf::from("/srv/b")]
        );
    }

    #[test]
    fn redirect_roots_ignore_unset_or_blank_values() {
        assert!(claude_env_redirect_roots(None, None).is_empty());
        assert!(claude_env_redirect_roots(Some(""), Some("   ")).is_empty());
    }

    #[test]
    fn env_redirect_promotes_not_found_to_detected_when_a_root_exists() {
        let roots = claude_env_redirect_roots(Some("/srv/tenant-a"), None);
        let projects = PathBuf::from("/srv/tenant-a/projects");
        let result = detection_with_env_redirects(DetectionResult::not_found(), &roots, |path| {
            path == projects
        });
        assert!(result.detected);
        assert_eq!(result.root_paths, vec![projects.clone()]);
        assert_eq!(
            result.evidence,
            vec![format!("env redirect root exists: {}", projects.display())]
        );
    }

    #[test]
    fn env_redirect_leaves_not_found_alone_when_no_root_exists() {
        let roots = claude_env_redirect_roots(None, Some("/home/u/.config"));
        let result = detection_with_env_redirects(DetectionResult::not_found(), &roots, |_| false);
        assert!(!result.detected);
        assert!(result.root_paths.is_empty());
        let result = detection_with_env_redirects(DetectionResult::not_found(), &[], |_| true);
        assert!(!result.detected);
    }

    #[test]
    fn env_redirect_never_overrides_an_already_detected_probe() {
        let detected = DetectionResult {
            detected: true,
            evidence: vec!["default root exists: /home/u/.claude".to_string()],
            root_paths: vec![PathBuf::from("/home/u/.claude")],
        };
        let roots = claude_env_redirect_roots(Some("/srv/tenant-a"), None);
        let result = detection_with_env_redirects(detected.clone(), &roots, |_| true);
        assert_eq!(result.root_paths, detected.root_paths);
        assert_eq!(result.evidence, detected.evidence);
    }

    #[test]
    fn wrapper_detects_a_real_redirected_profile_root() {
        // Same shape as the #448 repro: a `projects` dir under a redirected
        // config dir, and no `~/.claude`. Exercises the real filesystem probe
        // without mutating the process environment.
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let config_dir = tmp.path().join("claude-cfg");
        std::fs::create_dir_all(config_dir.join("projects")).expect("projects dir");
        let roots = claude_env_redirect_roots(config_dir.to_str(), None);
        let result =
            detection_with_env_redirects(DetectionResult::not_found(), &roots, Path::exists);
        assert!(result.detected);
        assert_eq!(
            result.root_paths,
            vec![config_dir.join("projects"), config_dir]
        );
    }
}
