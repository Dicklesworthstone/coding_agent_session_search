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
    active_profile: Option<String>,
}

impl PiFamilyOwnership {
    #[must_use]
    pub(crate) fn live() -> Self {
        let home = dirs::home_dir();
        let active_profile = active_profile_from_env();
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
            active_profile.clone(),
        );

        Self {
            pi_sessions_dir: nonempty_env_path("PI_SESSIONS_DIR"),
            omp_session_dir: nonempty_env_path("PI_CODING_AGENT_SESSION_DIR"),
            shared_agent_dir: nonempty_env_path("PI_CODING_AGENT_DIR"),
            omp_config_root,
            omp_store_roots: tagged_roots,
            active_profile,
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
        let archive_layout = classify_omp_archive_path(path);
        if self
            .omp_config_root
            .as_deref()
            .is_some_and(|root| path_is_within(path, root))
            || archive_layout == Some(OmpArchivePathClass::ConfigOrMirror)
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
        if archive_layout == Some(OmpArchivePathClass::Xdg) {
            return PiFamilyOwner::Omp;
        }
        PiFamilyOwner::Unknown
    }

    #[must_use]
    fn omp_scan_roots(&self) -> &[(PathBuf, Option<String>)] {
        &self.omp_store_roots
    }

    /// Return the active process profile only when the transcript is inside
    /// the exact live session-directory override that profile configures.
    ///
    /// OMP ownership alone is not sufficient evidence: CASS archive roots,
    /// copied config homes, XDG stores, and remote mirrors may all belong to a
    /// different invocation. Their profile must come from the path itself.
    #[must_use]
    fn live_session_override_profile(&self, path: &Path) -> Option<&str> {
        if self
            .pi_sessions_dir
            .as_deref()
            .is_some_and(|root| path_is_within(path, root))
        {
            return None;
        }
        self.omp_session_dir
            .as_deref()
            .filter(|root| path_is_within(path, root))?;
        self.active_profile.as_deref()
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

    /// Stable, non-reversible identity of the live inputs that can change
    /// archive ownership. Storage binds its completed legacy-migration marker
    /// to this value so adding a custom XDG/CASS root later cannot strand an
    /// old Pi-labelled OMP row behind a stale fast path.
    #[must_use]
    pub(crate) fn archive_reclassification_context(&self) -> String {
        fn add_path(hasher: &mut blake3::Hasher, label: &str, path: Option<&Path>) {
            hasher.update(label.as_bytes());
            hasher.update(b"\0");
            if let Some(path) = path {
                hasher.update(path.to_string_lossy().as_bytes());
            }
            hasher.update(b"\0");
        }

        let mut hasher = blake3::Hasher::new();
        hasher.update(b"cass-pi-family-ownership-v1\0");
        add_path(&mut hasher, "pi_sessions_dir", self.pi_sessions_dir.as_deref());
        add_path(&mut hasher, "omp_session_dir", self.omp_session_dir.as_deref());
        add_path(&mut hasher, "shared_agent_dir", self.shared_agent_dir.as_deref());
        add_path(&mut hasher, "omp_config_root", self.omp_config_root.as_deref());
        for (root, _) in &self.omp_store_roots {
            add_path(&mut hasher, "omp_store_root", Some(root));
        }
        hasher.finalize().to_hex().to_string()
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

fn path_parts(path: &Path) -> Option<Vec<String>> {
    // Archived paths may have been written on another operating system. Split
    // both separator styles explicitly instead of letting the current host's
    // `Path::components` reinterpret a Windows path as one Unix component.
    // A parent component invalidates structural evidence entirely: archived or
    // missing paths cannot be canonicalized reliably, and accepting a marker
    // before `..` would let the resolved path escape into another provider.
    let mut parts = Vec::new();
    for component in path
        .as_os_str()
        .to_string_lossy()
        .split(['/', '\\'])
        .filter(|component| !component.is_empty() && *component != ".")
    {
        if component == ".." {
            return None;
        }
        parts.push(component.to_owned());
    }
    Some(parts)
}

fn has_config_omp_layout_marker(parts: &[String]) -> bool {
    parts
        .windows(2)
        .any(|window| window[0] == ".omp" && window[1] == "agent")
        || parts.windows(4).any(|window| {
            window[0] == ".omp"
                && window[1] == "profiles"
                && normalize_profile_name(&window[2]).is_some()
                && window[3] == "agent"
        })
}

fn has_pi_agent_layout_marker(path: &Path) -> bool {
    path_parts(path).is_some_and(|parts| {
        parts
            .windows(2)
            .any(|parts| parts[0] == ".pi" && parts[1] == "agent")
    })
}

fn is_safe_mirror_hash(value: &str) -> bool {
    (8..=16).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn encoded_path_has_marker(encoded: &str, marker: &str) -> bool {
    encoded.match_indices(marker).any(|(index, _)| {
        let end = index + marker.len();
        (index == 0 || encoded.as_bytes()[index - 1] == b'_')
            && (end == encoded.len() || encoded.as_bytes()[end] == b'_')
    })
}

fn safe_mirror_name_has_omp_layout(name: &str) -> bool {
    let Some((encoded, hash)) = name.rsplit_once('_') else {
        return false;
    };
    if !is_safe_mirror_hash(hash) {
        return false;
    }

    encoded == ".omp"
        || encoded_path_has_marker(encoded, ".omp_agent")
        || encoded_path_has_marker(encoded, ".omp_profiles")
        || encoded_path_has_marker(encoded, ".local_share_omp")
}

fn has_sanitized_omp_mirror_marker(parts: &[String]) -> bool {
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
            && safe_mirror_name_has_omp_layout(&window[3])
    })
}

fn has_xdg_omp_layout_marker(parts: &[String]) -> bool {
    parts
        .windows(4)
        .any(|window| {
            window[0] == ".local"
                && window[1] == "share"
                && window[2] == "omp"
                && window[3] == "sessions"
        })
        || parts.windows(5).any(|window| {
            window[0] == ".local"
                && window[1] == "share"
                && window[2] == "omp"
                && window[3] == "profiles"
                && normalize_profile_name(&window[4]).is_some()
        })
}

/// Durable OMP evidence encoded in an archived transcript path.
///
/// This classifier is deliberately independent of the live process
/// environment so the connector ownership boundary and legacy SQLite
/// reclassification use exactly the same structural evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OmpArchivePathClass {
    /// A canonical `.omp` config layout or a production remote-mirror slot.
    ConfigOrMirror,
    /// The conventional `~/.local/share/omp` XDG layout.
    Xdg,
}

#[must_use]
pub(crate) fn classify_omp_archive_path(path: &Path) -> Option<OmpArchivePathClass> {
    let parts = path_parts(path)?;
    if has_config_omp_layout_marker(&parts) || has_sanitized_omp_mirror_marker(&parts) {
        return Some(OmpArchivePathClass::ConfigOrMirror);
    }
    has_xdg_omp_layout_marker(&parts).then_some(OmpArchivePathClass::Xdg)
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

/// Return an explicit OMP session/store override that owns `path`.
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

    None
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

/// Resolve the profile needed to reopen one OMP transcript.
///
/// A path-encoded named profile is durable provenance. The process profile is
/// only valid for the exact `PI_CODING_AGENT_SESSION_DIR` live override; an
/// unrelated active profile must never be attached to a CASS archive or copy.
#[must_use]
pub(crate) fn resume_profile_from_path(path: &Path) -> Option<String> {
    profile_from_session_path(path).or_else(|| {
        PiFamilyOwnership::live()
            .live_session_override_profile(path)
            .map(str::to_owned)
    })
}

fn path_is_in_resolved_config_store_from(
    path: &Path,
    home: &Path,
    config_name: &str,
    active_profile: Option<&str>,
) -> bool {
    let path_profile = profile_from_session_path(path);
    if path_profile.is_none() && active_profile.is_some() {
        return false;
    }

    let config_root = home.join(config_name.trim_start_matches(['/', '\\']));
    let sessions = path_profile.as_deref().map_or_else(
        || config_root.join("agent/sessions"),
        |profile| {
            config_root
                .join("profiles")
                .join(profile)
                .join("agent/sessions")
        },
    );
    path_is_within(path, &sessions)
}

/// True only when `path` belongs to the home/config store that the generated
/// OMP command will resolve without an explicit `--session-dir`.
///
/// Archive discovery intentionally indexes copied homes, remote mirrors, and
/// secondary XDG stores as well. Structural `.omp` or profile markers alone
/// therefore cannot prove that the current OMP process will reopen that same
/// store.
#[must_use]
pub(crate) fn is_resolved_live_config_session_path(path: &Path) -> bool {
    let Some(home) = dirs::home_dir() else {
        return false;
    };
    let config_name = dotenvy::var("PI_CONFIG_DIR")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| ".omp".to_string());
    let active_profile = active_profile_from_env();
    path_is_in_resolved_config_store_from(
        path,
        &home,
        &config_name,
        active_profile.as_deref(),
    )
}

/// True when an unambiguous OMP layout or OMP-only environment override owns
/// `path`.
#[must_use]
pub(crate) fn owns_session_path(path: &Path) -> bool {
    PiFamilyOwnership::live().owner(path) == PiFamilyOwner::Omp
}

#[cfg(test)]
fn has_omp_layout_marker(path: &Path) -> bool {
    classify_omp_archive_path(path).is_some()
}

/// Reconcile profile provenance for explicit roots that FAD cannot tag on its
/// own (for example `~/.omp/profiles/work/agent/sessions`). A structural path
/// profile is authoritative over a process-level fallback tag.
fn fill_missing_profiles(
    conversations: &mut [NormalizedConversation],
    ownership: &PiFamilyOwnership,
) {
    for conversation in conversations {
        // The transcript path is the durable provenance source. An explicit
        // fallback scan may have been seeded with the process's active profile,
        // but that local launch setting must not override a named profile that
        // is encoded in an XDG/config-layout path (and may belong to a mirror).
        if let Some(profile) = profile_from_session_path(&conversation.source_path) {
            if let Some(metadata) = conversation.metadata.as_object_mut() {
                metadata.insert("profile".into(), serde_json::Value::String(profile));
            }
            continue;
        }

        let profile_missing = conversation
            .metadata
            .get("profile")
            .and_then(serde_json::Value::as_str)
            .is_none();
        if !profile_missing {
            continue;
        }
        let profile = ownership
            .live_session_override_profile(&conversation.source_path)
            .map(str::to_owned);
        if let Some(profile) = profile
            && let Some(metadata) = conversation.metadata.as_object_mut()
        {
            metadata.insert("profile".into(), serde_json::Value::String(profile));
        }
    }
}

fn direct_root_profile(root: &ScanRoot, ownership: &PiFamilyOwnership) -> Option<String> {
    profile_from_session_path(&root.path).or_else(|| {
        // A process-level profile describes only the live session override. It
        // says nothing about a copied local transcript or a remote transcript.
        if root.origin.is_remote() {
            None
        } else {
            ownership
                .live_session_override_profile(&root.path)
                .map(str::to_owned)
        }
    })
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
            let tagged_roots = direct_roots
                .iter()
                .map(|root| (root.path.clone(), direct_root_profile(root, &ownership)))
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
        fill_missing_profiles(&mut conversations, &ownership);
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
    use crate::connectors::{Origin, Platform};
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
        active_profile: Option<&str>,
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
            active_profile: active_profile.map(str::to_owned),
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
        assert!(!has_omp_layout_marker(Path::new(
            "/home/dev/.omp-cache/agent/sessions/project/session.jsonl"
        )));
        assert!(!has_omp_layout_marker(Path::new(
            "/srv/omp/sessions/project/session.jsonl"
        )));
        assert!(has_omp_layout_marker(Path::new(
            r"C:\Users\dev\.omp\agent\sessions\project\session.jsonl"
        )));
        assert!(!has_omp_layout_marker(Path::new(
            r"C:\Users\dev\.omp-cache\agent\sessions\project\session.jsonl"
        )));
    }

    #[test]
    fn parent_traversal_invalidates_all_pi_family_layout_evidence() {
        let policy = ownership(None, None, None, Vec::new(), None);
        for path in [
            Path::new("/archive/.omp/agent/../../.pi/agent/sessions/x.jsonl"),
            Path::new(r"C:\archive\.omp\agent\..\..\.pi\agent\sessions\x.jsonl"),
            Path::new("/archive/.pi/agent/../../.omp/agent/sessions/x.jsonl"),
            Path::new(r"C:\archive\.pi\agent\..\..\.omp\agent\sessions\x.jsonl"),
        ] {
            assert_eq!(classify_omp_archive_path(path), None);
            assert!(!has_pi_agent_layout_marker(path));
            assert_eq!(
                policy.owner(path),
                PiFamilyOwner::Unknown,
                "a lexical parent traversal must fail closed for both providers: {}",
                path.display()
            );
        }
    }

    #[test]
    fn archive_context_tracks_ownership_roots_not_profile_metadata() {
        let root = PathBuf::from("/srv/omp-sessions");
        let mut work = ownership(
            None,
            Some(root.clone()),
            None,
            vec![root.clone()],
            Some("work"),
        );
        work.omp_store_roots[0].1 = Some("work".to_string());
        let mut review = work.clone();
        review.active_profile = Some("review".to_string());
        review.omp_store_roots[0].1 = Some("review".to_string());
        assert_eq!(
            work.archive_reclassification_context(),
            review.archive_reclassification_context(),
            "profile tags change metadata and resume behavior, not archive ownership"
        );

        let mut moved = review;
        moved.omp_session_dir = Some(PathBuf::from("/srv/other-omp-sessions"));
        assert_ne!(
            work.archive_reclassification_context(),
            moved.archive_reclassification_context(),
            "a provider-qualified ownership root change must invalidate migration completion"
        );
    }

    #[test]
    fn sanitized_omp_markers_require_the_exact_production_mirror_slot() {
        let tilde_name =
            crate::sources::sync::path_to_safe_dirname("~/.omp/agent/sessions");
        let absolute_name = crate::sources::sync::path_to_safe_dirname(
            "/home/dev/.local/share/omp/profiles/work/sessions",
        );
        for safe_name in [&tilde_name, &absolute_name] {
            let production_path = PathBuf::from("/cass/remotes/build-host/mirror")
                .join(safe_name)
                .join("sessions/project/session.jsonl");
            assert_eq!(
                classify_omp_archive_path(&production_path),
                Some(OmpArchivePathClass::ConfigOrMirror)
            );

            let incidental_path = PathBuf::from("/cass/ordinary-cache")
                .join(safe_name)
                .join("sessions/project/session.jsonl");
            assert_eq!(classify_omp_archive_path(&incidental_path), None);
        }

        assert_eq!(
            classify_omp_archive_path(Path::new(
                "/cass/remotes/build-host/mirror/.omp_agent_sessions_not-a-hash/sessions/session.jsonl"
            )),
            None,
            "a marker-shaped mirror name without the sync hash is not trustworthy provenance"
        );
        assert_eq!(
            classify_omp_archive_path(Path::new(
                "/cass/remotes/build-host/mirror/.omp_backup_0123456789abcdef/sessions/session.jsonl"
            )),
            None,
            "an unrelated dot-directory must not become OMP merely because its name starts with .omp"
        );
        for invalid_hash in [
            "0123456",
            "0123456789abcdef0",
            "0123456789abcdeF",
            "0123456789abcdeg",
        ] {
            let path = PathBuf::from("/cass/remotes/build-host/mirror")
                .join(format!(".omp_agent_sessions_{invalid_hash}"))
                .join("sessions/session.jsonl");
            assert_eq!(
                classify_omp_archive_path(&path),
                None,
                "only the lowercase 8-to-16-character hex producer range is valid: {invalid_hash}"
            );
        }
    }

    #[test]
    fn resolved_config_store_rejects_copies_and_inactive_default_store() {
        let home = Path::new("/home/dev");
        assert!(path_is_in_resolved_config_store_from(
            Path::new("/home/dev/.omp/agent/sessions/project/session.jsonl"),
            home,
            ".omp",
            None,
        ));
        assert!(!path_is_in_resolved_config_store_from(
            Path::new("/archive/dev/.omp/agent/sessions/project/session.jsonl"),
            home,
            ".omp",
            None,
        ));
        assert!(!path_is_in_resolved_config_store_from(
            Path::new("/home/dev/.omp/agent/sessions/project/session.jsonl"),
            home,
            ".omp",
            Some("work"),
        ));
        assert!(path_is_in_resolved_config_store_from(
            Path::new(
                "/home/dev/custom/profiles/work/agent/sessions/project/session.jsonl"
            ),
            home,
            "custom",
            Some("other"),
        ));
    }

    #[test]
    fn provider_specific_overrides_beat_conflicting_layout_markers() {
        let pi_root = PathBuf::from("/srv/omp/sessions");
        let pi_policy = ownership(Some(pi_root.clone()), None, None, Vec::new(), None);
        assert_eq!(
            pi_policy.owner(&pi_root.join("project/session.jsonl")),
            PiFamilyOwner::PiAgent,
            "PI_SESSIONS_DIR must outrank the broad /omp/sessions heuristic"
        );

        let omp_root = PathBuf::from("/srv/.pi/agent/sessions");
        let omp_policy = ownership(None, Some(omp_root.clone()), None, Vec::new(), None);
        assert_eq!(
            omp_policy.owner(&omp_root.join("project/session.jsonl")),
            PiFamilyOwner::Omp,
            "PI_CODING_AGENT_SESSION_DIR must outrank the .pi layout heuristic"
        );
    }

    #[test]
    fn shared_agent_override_has_one_legacy_owner() {
        let shared = PathBuf::from("/srv/shared-agent");
        let policy = ownership(None, None, Some(shared.clone()), Vec::new(), None);
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
    fn structural_profile_replaces_a_conflicting_fallback_tag() {
        let temp = tempfile::tempdir().expect("tempdir");
        let profile_root = temp.path().join("share/omp/profiles/work");
        write_session(&profile_root, "profile-authority");
        let ctx = ScanContext::local_default(temp.path().join("cass-state"), None);
        let mut conversations =
            franken_agent_detection::connectors::pi_wire::scan_homes_tagged(
                &[(profile_root, Some("other".to_string()))],
                &ctx,
                "omp",
            )
            .expect("scan deliberately mistagged profile root");
        assert_eq!(conversations[0].metadata["profile"], "other");

        let ownership = ownership(None, None, None, Vec::new(), None);
        fill_missing_profiles(&mut conversations, &ownership);

        assert_eq!(conversations[0].metadata["profile"], "work");
    }

    #[test]
    fn missing_profile_fallback_is_limited_to_the_live_session_override() {
        let temp = tempfile::tempdir().expect("tempdir");
        let archive_root = temp.path().join("cass-archive");
        write_session(&archive_root, "archive-profile");
        let ctx = ScanContext::local_default(temp.path().join("cass-state"), None);
        let mut archived = franken_agent_detection::connectors::pi_wire::scan_homes_tagged(
            &[(archive_root.clone(), None)],
            &ctx,
            "omp",
        )
        .expect("scan untagged CASS archive");
        let archive_policy = ownership(
            None,
            None,
            None,
            vec![archive_root],
            Some("local-profile"),
        );

        fill_missing_profiles(&mut archived, &archive_policy);

        assert!(
            archived[0]
                .metadata
                .get("profile")
                .and_then(serde_json::Value::as_str)
                .is_none(),
            "an unrelated process profile must not relabel an untagged CASS archive"
        );

        let override_root = temp.path().join("live-session-override");
        write_session(&override_root, "live-profile");
        let mut live = franken_agent_detection::connectors::pi_wire::scan_homes_tagged(
            &[(override_root.clone(), None)],
            &ctx,
            "omp",
        )
        .expect("scan untagged live override");
        let live_policy = ownership(
            None,
            Some(override_root),
            None,
            Vec::new(),
            Some("local-profile"),
        );

        fill_missing_profiles(&mut live, &live_policy);

        assert_eq!(live[0].metadata["profile"], "local-profile");
    }

    #[test]
    fn only_the_live_session_override_inherits_the_active_profile() {
        let local_profile = Some("local-profile");
        let archive_root = PathBuf::from("/cass/archives/custom-store");
        let archive_policy = ownership(
            None,
            None,
            None,
            vec![archive_root.clone()],
            local_profile,
        );
        let local_archive = ScanRoot::local(archive_root);
        assert_eq!(direct_root_profile(&local_archive, &archive_policy), None);

        let remote = ScanRoot::remote(
            PathBuf::from("/cass/remotes/build-host/mirror/custom-store"),
            Origin::remote_with_host("build-host", "build-host.example"),
            Some(Platform::Linux),
        );
        assert_eq!(direct_root_profile(&remote, &archive_policy), None);

        let structural = ScanRoot::local(PathBuf::from(
            "/home/dev/.local/share/omp/profiles/work/sessions",
        ));
        assert_eq!(
            direct_root_profile(&structural, &archive_policy).as_deref(),
            Some("work")
        );

        let override_root = PathBuf::from("/srv/omp-live-sessions");
        let override_policy = ownership(
            None,
            Some(override_root.clone()),
            None,
            Vec::new(),
            local_profile,
        );
        let live_override = ScanRoot::local(override_root.join("project"));
        assert_eq!(
            direct_root_profile(&live_override, &override_policy).as_deref(),
            local_profile
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
