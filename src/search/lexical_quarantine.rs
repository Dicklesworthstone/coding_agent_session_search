//! Inventory and retention for failed off-live lexical rebuilds (GH #523).
//!
//! These are evidence, not orphan staging. Keep the newest complete bundle and
//! every bundle touched within six hours. Only the indexer's exclusive lock can
//! authorize reclamation. Inspection never reads payloads or opens the archive.
//! Directory parents must be operator-controlled; metadata checks do not defend
//! against a hostile process replacing ancestors during filesystem operations.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

pub mod cleanup;
mod evidence;
pub use evidence::{EvidenceKind, FailureEvidence};
mod retention;
pub(crate) use retention::reclaim_under_index_run_lock;

const QUARANTINE_PREFIX: &str = ".lexical-rebuild-quarantine-";
const RECLAIM_PREFIX: &str = ".lexical-rebuild-reclaim-";
const CHECKPOINT: &str = ".lexical-rebuild-state.json";
const MIN_AGE: Duration = Duration::from_secs(6 * 60 * 60);
const MAX_ROOT_ENTRIES: usize = 16_384;
const MAX_BUNDLES: usize = 1_024;
const MAX_TREE_ENTRIES: usize = 65_536;
const MAX_PATH_BYTES: usize = 4 * 1024 * 1024;
const MAX_DEPTH: usize = 32;
const SCAN_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    RetainNewest,
    RetainRecent,
    RetainDisabled,
    Inspect,
    Eligible,
}

#[derive(Debug, Clone, Serialize)]
pub struct Artifact {
    pub path: PathBuf,
    pub interrupted_reclaim: bool,
    pub has_evidence: bool,
    /// Known diagnostic files, observed without reading their contents.
    /// An empty list is not proof of absence unless size_complete is true.
    pub failure_evidence: Vec<FailureEvidence>,
    /// Logical regular-file lengths, not allocated blocks or guaranteed savings.
    pub size_bytes: u64,
    pub size_complete: bool,
    /// Age of the most recently modified entry anywhere in this bundle.
    pub age_seconds: Option<u64>,
    pub disposition: Disposition,
    pub reason: String,
    #[serde(skip)]
    modified: Option<SystemTime>,
    /// Metadata-only change detector, NOT content authentication or GC approval.
    pub metadata_fingerprint: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Inventory {
    pub schema_version: u32,
    pub complete: bool,
    pub automatic_retention_enabled: bool,
    pub min_age_seconds: u64,
    pub keep_newest: usize,
    pub total_bytes: u64,
    pub eligible_bytes: u64,
    pub artifacts: Vec<Artifact>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Copy)]
struct Limits {
    root_entries: usize,
    bundles: usize,
    tree_entries: usize,
    path_bytes: usize,
    depth: usize,
    timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            root_entries: MAX_ROOT_ENTRIES,
            bundles: MAX_BUNDLES,
            tree_entries: MAX_TREE_ENTRIES,
            path_bytes: MAX_PATH_BYTES,
            depth: MAX_DEPTH,
            timeout: SCAN_TIMEOUT,
        }
    }
}

struct Budget {
    limits: Limits,
    started: Instant,
    root_entries: usize,
    tree_entries: usize,
    path_bytes: usize,
}

impl Budget {
    fn new(limits: Limits) -> Self {
        Self { limits, started: Instant::now(), root_entries: 0, tree_entries: 0, path_bytes: 0 }
    }

    fn check(&self) -> io::Result<()> {
        if self.started.elapsed() >= self.limits.timeout {
            return Err(io::Error::other("quarantine inventory deadline exhausted"));
        }
        Ok(())
    }

    fn root_entry(&mut self) -> io::Result<()> {
        self.check()?;
        if self.root_entries >= self.limits.root_entries {
            return Err(io::Error::other("quarantine root-entry budget exhausted"));
        }
        self.root_entries += 1;
        Ok(())
    }

    fn tree_entry(&mut self, path: &Path, depth: usize) -> io::Result<()> {
        self.check()?;
        if self.tree_entries >= self.limits.tree_entries || depth > self.limits.depth {
            return Err(io::Error::other("quarantine tree-entry/depth budget exhausted"));
        }
        self.path_bytes = self.path_bytes.checked_add(path.as_os_str().as_encoded_bytes().len())
            .filter(|&bytes| bytes <= self.limits.path_bytes)
            .ok_or_else(|| io::Error::other("quarantine path-byte budget exhausted"))?;
        self.tree_entries += 1;
        Ok(())
    }
}

/// The same opt-out is respected by inspection and normal index startup.
/// Invalid values stop reclamation rather than silently selecting a policy.
fn retention_enabled() -> io::Result<bool> {
    match dotenvy::var("CASS_LEXICAL_QUARANTINE_RETENTION") {
        Err(std::env::VarError::NotPresent) => Ok(true),
        Ok(value) if value == "1" => Ok(true),
        Ok(value) if value == "0" => Ok(false),
        _ => Err(io::Error::new(io::ErrorKind::InvalidInput,
            "CASS_LEXICAL_QUARANTINE_RETENTION must be 0 or 1")),
    }
}

fn valid_name(name: &str, prefix: &str) -> bool {
    name.strip_prefix(prefix).is_some_and(|suffix|
        suffix.len() == 6 && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric()))
}

fn real_directory(path: &Path) -> io::Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput,
            "quarantine root must be a real directory, not a symlink"));
    }
    Ok(metadata)
}

fn roots(data_dir: &Path) -> io::Result<Vec<PathBuf>> {
    if data_dir.components().any(|part| matches!(part, std::path::Component::ParentDir)) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput,
            "quarantine data directory must not contain parent traversal"));
    }
    real_directory(data_dir)?;
    let root = crate::search::tantivy::expected_index_dir(data_dir)
        .parent().ok_or_else(|| io::Error::other("lexical index has no parent"))?.to_owned();
    let mut roots = vec![data_dir.to_owned()];
    if root.as_path() != data_dir {
        match real_directory(&root) {
            Ok(_) => roots.push(root),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {},
            Err(error) => return Err(error),
        }
    }
    Ok(roots)
}

fn hash_time(hash: &mut blake3::Hasher, time: SystemTime) {
    let (sign, duration) = match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => (0, duration),
        Err(error) => (1, error.duration()),
    };
    hash.update(&[sign]);
    hash.update(&duration.as_nanos().to_le_bytes());
}

fn stamp(relative: &Path, metadata: &fs::Metadata) -> io::Result<blake3::Hash> {
    let mut hash = blake3::Hasher::new();
    let path = relative.as_os_str().as_encoded_bytes();
    hash.update(&(path.len() as u64).to_le_bytes());
    hash.update(path);
    hash.update(&[u8::from(metadata.is_dir())]);
    hash.update(&metadata.len().to_le_bytes());
    hash_time(&mut hash, metadata.modified()?);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        hash.update(&metadata.dev().to_le_bytes());
        hash.update(&metadata.ino().to_le_bytes());
        hash.update(&metadata.ctime().to_le_bytes());
        hash.update(&metadata.ctime_nsec().to_le_bytes());
        hash.update(&metadata.nlink().to_le_bytes());
    }
    Ok(hash.finalize())
}

fn observe(path: &Path, pending: bool, now: SystemTime, budget: &mut Budget) -> io::Result<Artifact> {
    let root = real_directory(path)?;
    let mut latest = root.modified()?;
    let mut bytes = 0_u64;
    let mut nodes = Vec::new();
    let mut failure_evidence = Vec::new();
    let mut has_index = false;
    let mut has_checkpoint = false;
    let mut has_pending_child = false;
    for entry in walkdir::WalkDir::new(path)
        .follow_links(false)
        .follow_root_links(false)
        .max_open(8)
        .max_depth(budget.limits.depth.saturating_add(1))
    {
        budget.check()?;
        let entry = entry.map_err(io::Error::other)?;
        let relative = entry.path().strip_prefix(path).map_err(io::Error::other)?;
        budget.tree_entry(relative, entry.depth())?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if !metadata.is_dir() && !metadata.is_file() {
            return Err(io::Error::other("quarantine contains a symlink or special file"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.dev() != root.dev() || (metadata.is_file() && metadata.nlink() != 1) {
                return Err(io::Error::other("quarantine crosses a filesystem or contains hard-linked files"));
            }
        }
        // Only the two layouts actually produced by rebuild recovery are owned:
        // index[/...] with optional checkpoint, or a retained checkpoint alone.
        // Interrupted retention wraps precisely one such bundle in quarantine/.
        let parts: Vec<_> = relative.components().collect();
        let parts = if pending && !parts.is_empty() {
            if parts[0].as_os_str() != "quarantine" || (parts.len() == 1 && !metadata.is_dir()) {
                return Err(io::Error::other("unexpected interrupted-reclaim layout"));
            }
            has_pending_child = true;
            &parts[1..]
        } else { parts.as_slice() };
        if let Some(first) = parts.first() {
            if first.as_os_str() == "index" {
                if parts.len() == 1 {
                    if !metadata.is_dir() { return Err(io::Error::other("quarantine index is not a directory")); }
                    has_index = true;
                }
            } else if first.as_os_str() == CHECKPOINT && parts.len() == 1 && metadata.is_file() {
                has_checkpoint = true;
            } else {
                return Err(io::Error::other("quarantine contains unrecognized top-level evidence; retain for inspection"));
            }
        }
        latest = latest.max(metadata.modified()?);
        if metadata.is_file() {
            bytes = bytes.checked_add(metadata.len()).ok_or_else(|| io::Error::other("quarantine byte count overflow"))?;
        }
        if let Some(evidence) = evidence::observe_file(relative, pending, &metadata) {
            failure_evidence.push(evidence);
        }
        nodes.push((relative.to_owned(), stamp(relative, &metadata)?));
    }
    // Empty reservations can remain after a failed rename, or after interrupted
    // deletion removed the last child. They never displace actual evidence from
    // the newest-retained slot. Unknown children were refused above.
    let has_evidence = has_index || has_checkpoint;
    if pending && has_evidence && !has_pending_child {
        return Err(io::Error::other("invalid interrupted quarantine layout"));
    }
    failure_evidence.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    nodes.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let mut hash = blake3::Hasher::new();
    for (_, node) in nodes { hash.update(node.as_bytes()); }
    budget.check()?;
    Ok(Artifact {
        path: path.to_owned(), interrupted_reclaim: pending, has_evidence, failure_evidence, size_bytes: bytes,
        size_complete: true, age_seconds: now.duration_since(latest).ok().map(|age| age.as_secs()),
        disposition: Disposition::Inspect, reason: String::new(), modified: Some(latest),
        metadata_fingerprint: Some(hash.finalize().to_hex().to_string()),
    })
}

fn select(inventory: &mut Inventory) {
    inventory.total_bytes = match inventory.artifacts.iter().try_fold(0_u64,
        |sum, artifact| sum.checked_add(artifact.size_bytes)) {
        Some(total) => total,
        None => {
            inventory.complete = false;
            inventory.warnings.push("quarantine aggregate byte count overflow".into());
            u64::MAX
        }
    };
    // An incomplete listing cannot prove which bundle is newest. Never let a
    // budget, unreadable path or invalid layout turn a partial inventory into GC.
    // Prefer an ordinary bundle: a retention rename makes a pending wrapper
    // look newer without creating newer failure evidence. If ordinary evidence
    // has disappeared, however, keep the best surviving interrupted bundle.
    // A prior cleanup decision is not permission to erase the last evidence on
    // a later run. Empty reservations must never occupy this survivor slot.
    let newest = inventory
        .artifacts
        .iter()
        .enumerate()
        .filter(|(_, artifact)| artifact.has_evidence && artifact.size_complete)
        .max_by(|(_, a), (_, b)| {
            (!a.interrupted_reclaim)
                .cmp(&(!b.interrupted_reclaim))
                .then_with(|| a.modified.cmp(&b.modified))
                .then_with(|| a.path.cmp(&b.path))
        })
        .map(|(index, _)| index);
    for (index, artifact) in inventory.artifacts.iter_mut().enumerate() {
        let (disposition, reason) = if !inventory.complete || !artifact.size_complete {
            (Disposition::Inspect, "inventory incomplete; automatic retention refused")
        } else if !inventory.automatic_retention_enabled {
            (Disposition::RetainDisabled, "automatic retention disabled by operator")
        } else if newest == Some(index) {
            let reason = if artifact.interrupted_reclaim {
                "last available rebuild evidence retained after interrupted cleanup"
            } else {
                "newest complete rebuild evidence retained"
            };
            (Disposition::RetainNewest, reason)
        } else if artifact.age_seconds.is_none_or(|age| age < MIN_AGE.as_secs()) {
            (Disposition::RetainRecent, "recent, future-dated or unknown-age evidence retained")
        } else {
            (Disposition::Eligible, "older derived rebuild evidence; reclaim only under index-run lock")
        };
        artifact.disposition = disposition;
        if artifact.reason.is_empty() { artifact.reason = reason.to_owned(); }
    }
    inventory.eligible_bytes = inventory.artifacts.iter().filter(|artifact| artifact.disposition == Disposition::Eligible)
        .fold(0_u64, |sum, artifact| sum.saturating_add(artifact.size_bytes));
}

fn inspect_at(data_dir: &Path, now: SystemTime, enabled: bool, limits: Limits) -> io::Result<Inventory> {
    let mut budget = Budget::new(limits);
    let mut inventory = Inventory {
        schema_version: 1, complete: true, automatic_retention_enabled: enabled,
        min_age_seconds: MIN_AGE.as_secs(), keep_newest: 1, total_bytes: 0, eligible_bytes: 0,
        artifacts: Vec::new(), warnings: Vec::new(),
    };
    for root in roots(data_dir)? {
        for entry in fs::read_dir(&root)? {
            if let Err(error) = budget.root_entry() {
                inventory.complete = false;
                inventory.warnings.push(error.to_string());
                break;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => { inventory.complete = false; inventory.warnings.push(error.to_string()); break; }
            };
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue; };
            let pending = valid_name(name, RECLAIM_PREFIX);
            if !pending && !valid_name(name, QUARANTINE_PREFIX) { continue; }
            if inventory.artifacts.len() >= limits.bundles {
                inventory.complete = false;
                inventory.warnings.push("quarantine bundle budget exhausted".into());
                break;
            }
            let path = entry.path();
            match observe(&path, pending, now, &mut budget) {
                Ok(artifact) => inventory.artifacts.push(artifact),
                Err(error) => {
                    inventory.complete = false;
                    inventory.artifacts.push(Artifact {
                        path, interrupted_reclaim: pending, has_evidence: false, failure_evidence: Vec::new(), size_bytes: 0, size_complete: false,
                        age_seconds: None, disposition: Disposition::Inspect, reason: error.to_string(),
                        modified: None, metadata_fingerprint: None,
                    });
                }
            }
        }
    }
    if let Err(error) = budget.check() {
        inventory.complete = false;
        inventory.warnings.push(error.to_string());
    }
    inventory.artifacts.sort_by(|a, b| a.path.cmp(&b.path));
    select(&mut inventory);
    Ok(inventory)
}

/// Read-only, bounded inventory for the operator-selected data directory.
/// Does not acquire mutation locks, create directories, read payloads, open a
/// database/model or claim that logical file bytes are immediately reclaimable.
pub fn inspect(data_dir: &Path) -> io::Result<Inventory> {
    inspect_at(data_dir, SystemTime::now(), retention_enabled()?, Limits::default())
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod recovery_tests;
