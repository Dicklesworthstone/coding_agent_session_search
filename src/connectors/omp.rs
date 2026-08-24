//! CASS adapter for `franken_agent_detection::connectors::omp`.
//!
//! FAD owns OMP discovery and pi-family parsing. This adapter preserves named
//! profile provenance when CASS supplies a profile's `sessions` directory as
//! an explicit scan root: that root shape is intentionally accepted by FAD,
//! but it cannot recover the profile name without the surrounding broad home.

use std::path::{Path, PathBuf};

use anyhow::Result;

use super::{
    Connector, DetectionResult, DiscoveredSourceFile, NormalizedConversation, ScanContext,
    ScanRoot,
};

pub struct OmpConnector {
    inner: franken_agent_detection::OmpConnector,
}

impl Default for OmpConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl OmpConnector {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            inner: franken_agent_detection::OmpConnector::new(),
        }
    }
}

/// Normalize an OMP profile name using the same contract as OMP v18.
#[must_use]
pub(crate) fn normalize_profile_name(profile: &str) -> Option<String> {
    let name = profile.trim();
    if name.is_empty() || name == "default" || name == "." || name == ".." {
        return None;
    }
    if name.ends_with('.') || name.len() > 64 {
        return None;
    }
    let mut chars = name.chars();
    if !chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._-".contains(c))
    {
        return None;
    }
    let base = name.split('.').next().unwrap_or(name);
    let upper = base.to_ascii_uppercase();
    let reserved = matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (upper.len() == 4
            && (upper.starts_with("COM") || upper.starts_with("LPT"))
            && upper.as_bytes()[3].is_ascii_digit());
    (!reserved).then(|| name.to_string())
}

/// Resolve the active profile without allowing an empty `OMP_PROFILE` to fall
/// through to legacy `PI_PROFILE`.
#[must_use]
pub(crate) fn active_profile_from_env() -> Option<String> {
    match dotenvy::var("OMP_PROFILE") {
        Ok(value) => normalize_profile_name(&value),
        Err(_) => dotenvy::var("PI_PROFILE")
            .ok()
            .and_then(|value| normalize_profile_name(&value)),
    }
}

fn nonempty_env_path(key: &str) -> Option<PathBuf> {
    dotenvy::var(key)
        .ok()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn configured_root_from_config_dir(path: &Path) -> Option<PathBuf> {
    let config_name = dotenvy::var("PI_CONFIG_DIR")
        .ok()
        .filter(|value| !value.is_empty())?;
    let home = dirs::home_dir()?;
    let base = home.join(config_name.trim_start_matches(['/', '\\']));
    let sessions = active_profile_from_env().map_or_else(
        || base.join("agent/sessions"),
        |profile| base.join("profiles").join(profile).join("agent/sessions"),
    );
    path.starts_with(&sessions).then_some(sessions)
}

/// Return a configured OMP session root that owns `path`.
///
/// `PI_CODING_AGENT_DIR` is deliberately included only as a resume/scan-root
/// resolver, not as an OMP identity signal: Pi Agent and OMP both honor that
/// variable, so it cannot safely distinguish the two by itself.
#[must_use]
pub(crate) fn configured_session_root(path: &Path) -> Option<PathBuf> {
    if let Some(root) = nonempty_env_path("PI_CODING_AGENT_SESSION_DIR")
        && path.starts_with(&root)
    {
        return Some(root);
    }

    if active_profile_from_env().is_none()
        && let Some(agent_root) = nonempty_env_path("PI_CODING_AGENT_DIR")
    {
        let sessions = agent_root.join("sessions");
        if path.starts_with(&sessions) {
            return Some(sessions);
        }
    }

    configured_root_from_config_dir(path)
}

/// Recover a valid named profile from canonical, custom-config, or XDG OMP
/// session paths. Callers must first establish that the path belongs to OMP.
#[must_use]
pub(crate) fn profile_from_session_path(path: &Path) -> Option<String> {
    let parts = path
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>();

    for window in parts.windows(4) {
        if window[0] == "profiles" && window[2] == "agent" && window[3] == "sessions" {
            return normalize_profile_name(window[1].as_ref());
        }
    }
    for window in parts.windows(3) {
        if window[0] == "profiles" && window[2] == "sessions" {
            return normalize_profile_name(window[1].as_ref());
        }
    }
    None
}

/// True when an unambiguous OMP layout or OMP-only environment override owns
/// `path`.
#[must_use]
pub(crate) fn owns_session_path(path: &Path) -> bool {
    if has_omp_layout_marker(path) {
        return true;
    }

    if let Some(root) = nonempty_env_path("PI_CODING_AGENT_SESSION_DIR")
        && path.starts_with(root)
    {
        return true;
    }

    if let Ok(config_name) = dotenvy::var("PI_CONFIG_DIR")
        && !config_name.is_empty()
        && let Some(home) = dirs::home_dir()
        && path.starts_with(home.join(config_name.trim_start_matches(['/', '\\'])))
    {
        return true;
    }

    false
}

fn has_omp_layout_marker(path: &Path) -> bool {
    let normalized = path.to_string_lossy().replace('\\', "/");
    if normalized.contains("/.omp/")
        || normalized.ends_with("/.omp")
        || normalized.starts_with(".omp/")
        || normalized == ".omp"
        || normalized.contains("/omp/sessions/")
        || normalized.ends_with("/omp/sessions")
        || normalized.contains("/omp/profiles/")
        || normalized.starts_with("omp/sessions/")
        || normalized == "omp/sessions"
        || normalized.starts_with("omp/profiles/")
    {
        return true;
    }
    false
}

/// Preserve profile provenance for explicit roots that FAD cannot tag on its
/// own (for example `~/.omp/profiles/work/agent/sessions`).
fn fill_missing_profiles(conversations: &mut [NormalizedConversation]) {
    for conversation in conversations {
        let profile_missing = conversation
            .metadata
            .get("profile")
            .and_then(serde_json::Value::as_str)
            .is_none();
        if !profile_missing {
            continue;
        }
        let profile = profile_from_session_path(&conversation.source_path).or_else(|| {
            configured_session_root(&conversation.source_path)
                .and_then(|_| active_profile_from_env())
        });
        if let Some(profile) = profile
            && let Some(metadata) = conversation.metadata.as_object_mut()
        {
            metadata.insert("profile".into(), serde_json::Value::String(profile));
        }
    }
}

fn fad_recognizes_explicit_root(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| name == "sessions" || name == "omp")
        || path.to_string_lossy().contains(".omp")
}

fn unrecognized_direct_session_roots(ctx: &ScanContext) -> Vec<ScanRoot> {
    ctx.scan_roots
        .iter()
        .filter(|root| {
            !fad_recognizes_explicit_root(&root.path)
                && configured_session_root(&root.path).as_ref() == Some(&root.path)
        })
        .cloned()
        .collect()
}

fn append_missing_conversations(
    conversations: &mut Vec<NormalizedConversation>,
    additional: impl IntoIterator<Item = NormalizedConversation>,
) {
    let mut seen = conversations
        .iter()
        .map(|conversation| {
            std::fs::canonicalize(&conversation.source_path)
                .unwrap_or_else(|_| conversation.source_path.clone())
        })
        .collect::<std::collections::HashSet<_>>();
    conversations.extend(additional.into_iter().filter(|conversation| {
        let key = std::fs::canonicalize(&conversation.source_path)
            .unwrap_or_else(|_| conversation.source_path.clone());
        seen.insert(key)
    }));
}

fn append_missing_sources(
    sources: &mut Vec<DiscoveredSourceFile>,
    additional: impl IntoIterator<Item = DiscoveredSourceFile>,
) {
    let mut seen = sources
        .iter()
        .map(|source| {
            std::fs::canonicalize(&source.source_path)
                .unwrap_or_else(|_| source.source_path.clone())
        })
        .collect::<std::collections::HashSet<_>>();
    sources.extend(additional.into_iter().filter(|source| {
        let key = std::fs::canonicalize(&source.source_path)
            .unwrap_or_else(|_| source.source_path.clone());
        seen.insert(key)
    }));
}

impl Connector for OmpConnector {
    fn detect(&self) -> DetectionResult {
        self.inner.detect()
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        let mut conversations = self.inner.scan(ctx)?;
        let direct_roots = unrecognized_direct_session_roots(ctx);
        if !direct_roots.is_empty() {
            let profile = active_profile_from_env();
            let tagged_roots = direct_roots
                .iter()
                .map(|root| (root.path.clone(), profile.clone()))
                .collect::<Vec<_>>();
            let fallback = franken_agent_detection::connectors::pi_wire::scan_homes_tagged(
                &tagged_roots,
                ctx,
                "omp",
            )?;
            append_missing_conversations(
                &mut conversations,
                fallback,
            );
        }
        fill_missing_profiles(&mut conversations);
        Ok(conversations)
    }

    fn discover_source_files(&self, ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        let mut sources = self.inner.discover_source_files(ctx)?;
        let direct_roots = unrecognized_direct_session_roots(ctx);
        if !direct_roots.is_empty() {
            let fallback = franken_agent_detection::connectors::pi_wire::discover_sources(
                &direct_roots,
                ctx,
                "omp",
            );
            append_missing_sources(&mut sources, fallback);
        }
        Ok(sources)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_paths_are_validated_across_omp_layouts() {
        assert_eq!(
            profile_from_session_path(Path::new(
                "/home/dev/.omp/profiles/work/agent/sessions/project/session.jsonl"
            )),
            Some("work".to_string())
        );
        assert_eq!(
            profile_from_session_path(Path::new(
                "/home/dev/custom-omp/profiles/review/agent/sessions/project/session.jsonl"
            )),
            Some("review".to_string())
        );
        assert_eq!(
            profile_from_session_path(Path::new(
                "/home/dev/.local/share/omp/profiles/fast/sessions/project/session.jsonl"
            )),
            Some("fast".to_string())
        );
        assert_eq!(
            profile_from_session_path(Path::new(
                "/home/dev/.omp/profiles/con/agent/sessions/project/session.jsonl"
            )),
            None,
            "reserved profile names must never reach `omp --profile`"
        );
    }

    #[test]
    fn canonical_and_xdg_paths_are_unambiguous_omp_owners() {
        assert!(has_omp_layout_marker(Path::new(
            "/home/dev/.omp/agent/sessions/project/session.jsonl"
        )));
        assert!(has_omp_layout_marker(Path::new(
            "/home/dev/.local/share/omp/profiles/work/sessions/project/session.jsonl"
        )));
        assert!(!has_omp_layout_marker(Path::new(
            "/home/dev/.pi/agent/sessions/project/session.jsonl"
        )));
    }
}
