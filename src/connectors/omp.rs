//! CASS adapter for `franken_agent_detection::connectors::omp`.
//!
//! FAD owns pi-family parsing. This adapter supplies CASS's provider-qualified
//! Pi/OMP ownership boundary, including native overrides and conventional XDG
//! discovery that must agree across detection, indexing, and watch scans. It
//! also preserves named-profile provenance for direct `sessions` roots.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

use super::{
    Connector, DetectionResult, DiscoveredSourceFile, NormalizedConversation, ScanContext, ScanRoot,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PiFamilyOwner {
    Omp,
    PiAgent,
    Unknown,
}

/// Snapshot of the process-level Pi-family ownership inputs.
///
/// Keeping the snapshot separate from path classification makes precedence
/// explicit and ensures a scan does not observe a mixture of environment
/// values if another test or embedding process changes its environment.
#[derive(Debug, Clone)]
pub(crate) struct PiFamilyOwnership {
    pi_sessions_dir: Option<PathBuf>,
    omp_session_dir: Option<PathBuf>,
    shared_agent_dir: Option<PathBuf>,
    omp_config_root: Option<PathBuf>,
    omp_store_roots: Vec<(PathBuf, Option<String>)>,
}

impl PiFamilyOwnership {
    #[must_use]
    pub(crate) fn live() -> Self {
        let home = dirs::home_dir();
        let config_name = dotenvy::var("PI_CONFIG_DIR")
            .ok()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| ".omp".to_string());
        let omp_config_root = home
            .as_ref()
            .map(|home| home.join(config_name.trim_start_matches(['/', '\\'])));
        let tagged_roots = local_omp_store_roots_from(
            home.as_deref(),
            nonempty_env_path("XDG_DATA_HOME").as_deref(),
            nonempty_env_path("PI_CODING_AGENT_SESSION_DIR").as_deref(),
            nonempty_env_path("CASS_OMP_DATA_ROOT").as_deref(),
            &config_name,
            active_profile_from_env(),
        );

        Self {
            pi_sessions_dir: nonempty_env_path("PI_SESSIONS_DIR"),
            omp_session_dir: nonempty_env_path("PI_CODING_AGENT_SESSION_DIR"),
            shared_agent_dir: nonempty_env_path("PI_CODING_AGENT_DIR"),
            omp_config_root,
            omp_store_roots: tagged_roots,
        }
    }

    /// Resolve one session path to a single provider identity.
    ///
    /// Provider-specific overrides are checked before any layout heuristic.
    /// `PI_CODING_AGENT_DIR` is intentionally lower priority because both
    /// programs honor it; absent an OMP-specific signal it retains the legacy
    /// Pi Agent identity instead of being indexed once by each connector.
    #[must_use]
    pub(crate) fn owner(&self, path: &Path) -> PiFamilyOwner {
        if self
            .pi_sessions_dir
            .as_deref()
            .is_some_and(|root| path_is_within(path, root))
        {
            return PiFamilyOwner::PiAgent;
        }
        if self
            .omp_session_dir
            .as_deref()
            .is_some_and(|root| path_is_within(path, root))
        {
            return PiFamilyOwner::Omp;
        }
        if self
            .omp_store_roots
            .iter()
            .any(|(root, _)| path_is_within(path, root))
        {
            return PiFamilyOwner::Omp;
        }
        if self
            .omp_config_root
            .as_deref()
            .is_some_and(|root| path_is_within(path, root))
            || has_dot_omp_layout_marker(path)
            || has_sanitized_omp_mirror_marker(path)
        {
            return PiFamilyOwner::Omp;
        }
        if has_pi_agent_layout_marker(path) {
            return PiFamilyOwner::PiAgent;
        }
        if self.shared_agent_dir.as_deref().is_some_and(|root| {
            path_is_within(path, root) || path_is_within(path, &root.join("sessions"))
        }) {
            return PiFamilyOwner::PiAgent;
        }
        if has_xdg_omp_layout_marker(path) {
            return PiFamilyOwner::Omp;
        }
        PiFamilyOwner::Unknown
    }

    #[must_use]
    fn omp_scan_roots(&self) -> &[(PathBuf, Option<String>)] {
        &self.omp_store_roots
    }

    #[must_use]
    pub(crate) fn pi_detection_roots(&self) -> Vec<PathBuf> {
        let mut roots = Vec::new();
        if let Some(root) = &self.pi_sessions_dir
            && root.exists()
        {
            roots.push(root.clone());
        }
        if let Some(root) = &self.shared_agent_dir {
            let sessions = root.join("sessions");
            if sessions.exists() {
                roots.push(sessions);
            }
        }
        dedupe_paths(&mut roots);
        roots
    }
}

fn nonempty_env_path(key: &str) -> Option<PathBuf> {
    dotenvy::var(key)
        .ok()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn path_is_within(path: &Path, root: &Path) -> bool {
    if let (Ok(canonical_path), Ok(canonical_root)) =
        (fs::canonicalize(path), fs::canonicalize(root))
    {
        return canonical_path.starts_with(canonical_root);
    }
    path.starts_with(root)
}

fn path_parts(path: &Path) -> Vec<String> {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect()
}

fn has_dot_omp_layout_marker(path: &Path) -> bool {
    path_parts(path).iter().any(|part| part == ".omp")
}

fn has_pi_agent_layout_marker(path: &Path) -> bool {
    path_parts(path)
        .windows(2)
        .any(|parts| parts[0] == ".pi" && parts[1] == "agent")
}

fn has_sanitized_omp_mirror_marker(path: &Path) -> bool {
    let parts = path_parts(path);
    if parts
        .iter()
        .any(|part| part.starts_with(".omp_") || part.starts_with(".local_share_omp_"))
    {
        return true;
    }

    // `sources sync` preserves a configured remote path in the mirror
    // container name. A tilde path starts with the provider marker, while an
    // absolute path retains its leading components, for example:
    //
    //   ~/.omp/agent/sessions       -> .omp_agent_sessions_<hash>
    //   /home/u/.omp/agent/sessions -> home_u_.omp_agent_sessions_<hash>
    //
    // Only trust the embedded absolute-path marker in the actual
    // `remotes/<source>/mirror/<safe-name>` slot. Treating the same substring
    // as an OMP signal in an arbitrary local directory would steal Pi-family
    // logs merely because an unrelated ancestor happened to contain `.omp`.
    parts.windows(4).any(|window| {
        window[0] == "remotes"
            && window[2] == "mirror"
            && (window[3].contains("_.omp_")
                || window[3].contains("_.local_share_omp_"))
    })
}

fn has_xdg_omp_layout_marker(path: &Path) -> bool {
    path_parts(path)
        .windows(2)
        .any(|parts| parts[0] == "omp" && (parts[1] == "sessions" || parts[1] == "profiles"))
}

fn push_tagged_root(
    roots: &mut Vec<(PathBuf, Option<String>)>,
    path: PathBuf,
    profile: Option<String>,
) {
    if path.exists() {
        roots.push((path, profile));
    }
}

fn profile_directories(base: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = fs::read_dir(base) else {
        return Vec::new();
    };
    let mut profiles = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let file_type = entry.file_type().ok()?;
            if !file_type.is_dir() {
                return None;
            }
            let name = entry.file_name();
            let profile = normalize_profile_name(name.to_string_lossy().as_ref())?;
            Some((profile, entry.path()))
        })
        .collect::<Vec<_>>();
    profiles.sort_by(|left, right| left.0.cmp(&right.0));
    profiles
}

fn append_config_layout_roots(roots: &mut Vec<(PathBuf, Option<String>)>, config_root: &Path) {
    for (profile, profile_root) in profile_directories(&config_root.join("profiles")) {
        let agent_root = profile_root.join("agent");
        if agent_root.exists() {
            push_tagged_root(roots, agent_root, Some(profile));
        } else if profile_root.join("sessions").exists() {
            push_tagged_root(roots, profile_root, Some(profile));
        }
    }
    push_tagged_root(roots, config_root.join("agent"), None);
}

fn append_xdg_layout_roots(roots: &mut Vec<(PathBuf, Option<String>)>, app_root: &Path) {
    for (profile, profile_root) in profile_directories(&app_root.join("profiles")) {
        push_tagged_root(roots, profile_root, Some(profile));
    }
    push_tagged_root(roots, app_root.to_path_buf(), None);
}

/// Expand an explicitly OMP-qualified root without sweeping sibling Pi data.
fn declared_omp_store_roots(root: &Path) -> Vec<(PathBuf, Option<String>)> {
    let mut roots = Vec::new();

    append_config_layout_roots(&mut roots, &root.join(".omp"));
    append_xdg_layout_roots(&mut roots, &root.join(".local/share/omp"));
    append_xdg_layout_roots(&mut roots, &root.join("omp"));
    append_config_layout_roots(&mut roots, root);

    if root.join("sessions").exists()
        || root.file_name().is_some_and(|name| name == "sessions")
        || root.file_name().is_some_and(|name| name == "omp")
    {
        push_tagged_root(
            &mut roots,
            root.to_path_buf(),
            profile_from_session_path(root),
        );
    }

    // A provider-specific override may directly name a flat or not-yet-filled
    // store. Only fall back to the declared root when no narrower OMP layout
    // was found, so a copied home containing both `.pi` and `.omp` is not
    // recursively swept as OMP.
    if roots.is_empty() {
        push_tagged_root(
            &mut roots,
            root.to_path_buf(),
            profile_from_session_path(root),
        );
    }

    dedupe_tagged_roots(&mut roots);
    roots
}

fn local_omp_store_roots_from(
    home: Option<&Path>,
    xdg_data_home: Option<&Path>,
    omp_session_dir: Option<&Path>,
    cass_omp_data_root: Option<&Path>,
    config_name: &str,
    active_profile: Option<String>,
) -> Vec<(PathBuf, Option<String>)> {
    let mut roots = Vec::new();

    if let Some(session_dir) = omp_session_dir {
        push_tagged_root(&mut roots, session_dir.to_path_buf(), active_profile);
    }
    if let Some(root) = cass_omp_data_root {
        roots.extend(declared_omp_store_roots(root));
    }
    if let Some(home) = home {
        let config_root = home.join(config_name.trim_start_matches(['/', '\\']));
        append_config_layout_roots(&mut roots, &config_root);

        let xdg_root = xdg_data_home.map_or_else(
            || home.join(".local/share/omp"),
            |data_home| data_home.join("omp"),
        );
        append_xdg_layout_roots(&mut roots, &xdg_root);
    } else if let Some(data_home) = xdg_data_home {
        append_xdg_layout_roots(&mut roots, &data_home.join("omp"));
    }

    dedupe_tagged_roots(&mut roots);
    roots
}

fn cass_omp_store_roots() -> Vec<(PathBuf, Option<String>)> {
    nonempty_env_path("CASS_OMP_DATA_ROOT")
        .as_deref()
        .map(declared_omp_store_roots)
        .unwrap_or_default()
}

fn dedupe_paths(paths: &mut Vec<PathBuf>) {
    let mut seen = HashSet::new();
    paths.retain(|path| {
        let key = fs::canonicalize(path).unwrap_or_else(|_| path.clone());
        seen.insert(key)
    });
}

fn dedupe_tagged_roots(roots: &mut Vec<(PathBuf, Option<String>)>) {
    let mut seen = HashSet::new();
    roots.retain(|(path, _)| {
        let key = fs::canonicalize(path).unwrap_or_else(|_| path.clone());
        seen.insert(key)
    });
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
    if let Some(root) = nonempty_env_path("PI_SESSIONS_DIR")
        && path_is_within(path, &root)
    {
        return None;
    }

    if let Some(root) = nonempty_env_path("PI_CODING_AGENT_SESSION_DIR")
        && path_is_within(path, &root)
    {
        return Some(root);
    }

    for (root, _) in cass_omp_store_roots() {
        if path_is_within(path, &root) {
            return Some(if root.file_name().is_some_and(|name| name == "sessions") {
                root
            } else {
                let sessions = root.join("sessions");
                if sessions.exists() { sessions } else { root }
            });
        }
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
    PiFamilyOwnership::live().owner(path) == PiFamilyOwner::Omp
}

#[cfg(test)]
fn has_omp_layout_marker(path: &Path) -> bool {
    has_dot_omp_layout_marker(path) || has_xdg_omp_layout_marker(path)
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

fn unrecognized_direct_session_roots(
    ctx: &ScanContext,
    ownership: &PiFamilyOwnership,
) -> Vec<ScanRoot> {
    ctx.scan_roots
        .iter()
        .filter(|root| {
            !fad_recognizes_explicit_root(&root.path)
                && ownership.owner(&root.path) == PiFamilyOwner::Omp
        })
        .cloned()
        .collect()
}

pub(crate) fn append_missing_conversations(
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

pub(crate) fn append_missing_sources(
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
        let ownership = PiFamilyOwnership::live();
        let mut detection = self.inner.detect();
        detection
            .root_paths
            .retain(|root| ownership.owner(root) == PiFamilyOwner::Omp);
        for (root, _) in ownership.omp_scan_roots() {
            if ownership.owner(root) != PiFamilyOwner::Omp {
                continue;
            }
            let canonical = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
            let already_reported = detection.root_paths.iter().any(|existing| {
                fs::canonicalize(existing).unwrap_or_else(|_| existing.clone()) == canonical
            });
            if !already_reported {
                detection.evidence.push(format!(
                    "CASS Pi-family ownership policy found OMP root: {}",
                    root.display()
                ));
                detection.root_paths.push(root.to_path_buf());
            }
        }
        detection.detected = !detection.root_paths.is_empty();
        detection
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        let ownership = PiFamilyOwnership::live();
        let mut conversations = self.inner.scan(ctx)?;

        if ctx.use_default_detection() {
            let fallback = franken_agent_detection::connectors::pi_wire::scan_homes_tagged(
                ownership.omp_scan_roots(),
                ctx,
                "omp",
            )?;
            append_missing_conversations(&mut conversations, fallback);
        }

        let direct_roots = unrecognized_direct_session_roots(ctx, &ownership);
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
            append_missing_conversations(&mut conversations, fallback);
        }
        conversations.retain(|conversation| {
            ownership.owner(&conversation.source_path) == PiFamilyOwner::Omp
        });
        fill_missing_profiles(&mut conversations);
        Ok(conversations)
    }

    fn discover_source_files(&self, ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        let ownership = PiFamilyOwnership::live();
        let mut sources = self.inner.discover_source_files(ctx)?;

        if ctx.use_default_detection() {
            let roots = ownership
                .omp_scan_roots()
                .iter()
                .map(|(path, _)| ScanRoot::local(path.clone()))
                .collect::<Vec<_>>();
            let fallback =
                franken_agent_detection::connectors::pi_wire::discover_sources(&roots, ctx, "omp");
            append_missing_sources(&mut sources, fallback);
        }

        let direct_roots = unrecognized_direct_session_roots(ctx, &ownership);
        if !direct_roots.is_empty() {
            let fallback = franken_agent_detection::connectors::pi_wire::discover_sources(
                &direct_roots,
                ctx,
                "omp",
            );
            append_missing_sources(&mut sources, fallback);
        }
        sources.retain(|source| ownership.owner(&source.source_path) == PiFamilyOwner::Omp);
        Ok(sources)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write_session(store_root: &Path, id: &str) -> PathBuf {
        let session_dir = store_root.join("sessions/project");
        fs::create_dir_all(&session_dir).expect("create test session directory");
        let path = session_dir.join(format!("2026-08-24T12-00-00_{id}.jsonl"));
        let transcript = [
            json!({"type":"session","id":id,"timestamp":"2026-08-24T12:00:00Z","cwd":"/project"}),
            json!({"type":"message","timestamp":"2026-08-24T12:00:01Z","message":{"role":"user","content":id}}),
        ]
        .into_iter()
        .map(|entry| entry.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        fs::write(&path, format!("{transcript}\n")).expect("write test session");
        path
    }

    fn ownership(
        pi_sessions_dir: Option<PathBuf>,
        omp_session_dir: Option<PathBuf>,
        shared_agent_dir: Option<PathBuf>,
        omp_store_roots: Vec<PathBuf>,
    ) -> PiFamilyOwnership {
        PiFamilyOwnership {
            pi_sessions_dir,
            omp_session_dir,
            shared_agent_dir,
            omp_config_root: None,
            omp_store_roots: omp_store_roots
                .into_iter()
                .map(|path| (path, None))
                .collect(),
        }
    }

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

    #[test]
    fn provider_specific_overrides_beat_conflicting_layout_markers() {
        let pi_root = PathBuf::from("/srv/omp/sessions");
        let pi_policy = ownership(Some(pi_root.clone()), None, None, Vec::new());
        assert_eq!(
            pi_policy.owner(&pi_root.join("project/session.jsonl")),
            PiFamilyOwner::PiAgent,
            "PI_SESSIONS_DIR must outrank the broad /omp/sessions heuristic"
        );

        let omp_root = PathBuf::from("/srv/.pi/agent/sessions");
        let omp_policy = ownership(None, Some(omp_root.clone()), None, Vec::new());
        assert_eq!(
            omp_policy.owner(&omp_root.join("project/session.jsonl")),
            PiFamilyOwner::Omp,
            "PI_CODING_AGENT_SESSION_DIR must outrank the .pi layout heuristic"
        );
    }

    #[test]
    fn shared_agent_override_has_one_legacy_owner() {
        let shared = PathBuf::from("/srv/shared-agent");
        let policy = ownership(None, None, Some(shared.clone()), Vec::new());
        assert_eq!(
            policy.owner(&shared.join("sessions/project/session.jsonl")),
            PiFamilyOwner::PiAgent
        );
    }

    #[test]
    fn conventional_xdg_and_cass_override_roots_are_scannable() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = temp.path().join("home");
        let xdg_app = home.join(".local/share/omp");
        let cass_root = temp.path().join("custom-omp-store");
        write_session(&xdg_app, "xdg-default");
        write_session(&cass_root, "cass-override");

        let roots =
            local_omp_store_roots_from(Some(&home), None, None, Some(&cass_root), ".omp", None);
        assert!(roots.iter().any(|(root, _)| root == &xdg_app));
        assert!(roots.iter().any(|(root, _)| root == &cass_root));

        let ctx = ScanContext::local_default(temp.path().join("cass-state"), None);
        let mut conversations =
            franken_agent_detection::connectors::pi_wire::scan_homes_tagged(&roots, &ctx, "omp")
                .expect("scan provider-qualified OMP roots");
        conversations.sort_by(|left, right| left.external_id.cmp(&right.external_id));
        assert_eq!(conversations.len(), 2);
        assert!(
            conversations
                .iter()
                .all(|conversation| conversation.agent_slug == "omp")
        );
    }

    #[test]
    fn declared_broad_root_does_not_sweep_sibling_pi_store() {
        let temp = tempfile::tempdir().expect("tempdir");
        let copied_home = temp.path().join("copied-home");
        write_session(&copied_home.join(".omp/agent"), "omp-only");
        write_session(&copied_home.join(".pi/agent"), "pi-only");

        let roots = declared_omp_store_roots(&copied_home);
        assert!(
            roots
                .iter()
                .any(|(root, _)| root == &copied_home.join(".omp/agent"))
        );
        assert!(!roots.iter().any(|(root, _)| root == &copied_home));
        assert!(
            roots
                .iter()
                .all(|(root, _)| !root.starts_with(copied_home.join(".pi")))
        );
    }
}
