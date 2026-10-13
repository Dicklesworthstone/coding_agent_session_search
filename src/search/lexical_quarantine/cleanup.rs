//! Explicit preview/approval for the same retention policy used by index startup.
//! A plan is not a capability to arbitrary paths: apply rebuilds it under the
//! permanent index-run lock and compares the complete policy/metadata commitment.
//! No database, model, live index or payload content is opened by this module.

use super::*;

mod lease;

const APPROVAL_PREFIX: &str = "lq-cleanup-v1-";

#[derive(Debug, Serialize)]
pub struct CleanupPlan {
    pub schema_version: u32,
    pub data_dir: PathBuf,
    /// Absent for incomplete inventories or disabled retention.
    /// This commits to observed metadata and policy, not file contents.
    pub approval_fingerprint: Option<String>,
    pub inventory: Inventory,
}

#[derive(Debug, thiserror::Error)]
pub enum CleanupError {
    #[error("cleanup requires a complete lq-cleanup-v1 approval fingerprint from --plan")]
    InvalidApproval,
    #[error("lexical quarantine retention is disabled; no quarantine mutation attempted")]
    Disabled,
    #[error("quarantine inventory is incomplete; no quarantine mutation attempted")]
    Incomplete,
    #[error("quarantine inventory or retention decisions changed; preview again before applying")]
    ApprovalChanged,
    #[error("an index run holds index-run.lock; stop that run before applying cleanup")]
    Busy,
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Reports fully removed bundles separately from attempted mutations. A failed
/// attempt may already have renamed a bundle or removed some of its children.
/// No rollback or immediate filesystem free-space gain is implied.
#[derive(Debug, Serialize)]
pub struct CleanupReport {
    pub schema_version: u32,
    pub data_dir: PathBuf,
    pub approval_fingerprint: String,
    pub complete: bool,
    pub selected_bundles: usize,
    pub retained_bundles: usize,
    pub attempted_bundles: usize,
    pub removed_bundles: usize,
    pub removed_logical_bytes: u64,
    pub errors: Vec<String>,
}

struct SealedPlan {
    view: CleanupPlan,
    roots: Vec<(PathBuf, same_file::Handle)>,
}

fn absolute_directory(path: &Path) -> io::Result<PathBuf> {
    // Validate the supplied spelling before canonicalization can erase a final
    // symlink or parent traversal. Ancestor directories remain operator-trusted.
    roots(path)?;
    fs::canonicalize(path)
}

fn field(hash: &mut blake3::Hasher, bytes: &[u8]) {
    hash.update(&(bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

fn check_roots(owners: &[(PathBuf, same_file::Handle)]) -> io::Result<()> {
    for (path, owner) in owners {
        real_directory(path)?;
        if *owner != same_file::Handle::from_path(path)? {
            return Err(io::Error::other("quarantine root changed during cleanup planning"));
        }
    }
    Ok(())
}

fn seal(data_dir: &Path, now: SystemTime, enabled: bool, limits: Limits) -> io::Result<SealedPlan> {
    let data_dir = absolute_directory(data_dir)?;
    let paths = roots(&data_dir)?;
    let owners = paths.iter().map(|path|
        Ok((path.clone(), same_file::Handle::from_path(path)?)))
        .collect::<io::Result<Vec<_>>>()?;
    let mut hash = blake3::Hasher::new();
    field(&mut hash, APPROVAL_PREFIX.as_bytes());
    for path in &paths {
        let metadata = real_directory(path)?;
        field(&mut hash, path.as_os_str().as_encoded_bytes());
        // Root mtime is intentionally excluded: acquiring the permanent lock
        // can create index-run.lock, and unrelated files are not cleanup targets.
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            hash.update(&metadata.dev().to_le_bytes());
            hash.update(&metadata.ino().to_le_bytes());
        }
        #[cfg(not(unix))]
        hash_time(&mut hash, metadata.created()?);
    }
    let inventory = inspect_at(&data_dir, now, enabled, limits)?;
    if paths != roots(&data_dir)? {
        return Err(io::Error::other("quarantine root set changed during cleanup planning"));
    }
    check_roots(&owners)?;
    let approval_fingerprint = if inventory.complete && enabled {
        hash.update(&inventory.min_age_seconds.to_le_bytes());
        hash.update(&(inventory.keep_newest as u64).to_le_bytes());
        hash.update(&[u8::from(inventory.automatic_retention_enabled)]);
        hash.update(&(inventory.artifacts.len() as u64).to_le_bytes());
        for artifact in &inventory.artifacts {
            field(&mut hash, artifact.path.as_os_str().as_encoded_bytes());
            field(&mut hash, artifact.metadata_fingerprint.as_deref()
                .ok_or_else(|| io::Error::other("complete inventory lacks a metadata fingerprint"))?.as_bytes());
            hash.update(&artifact.size_bytes.to_le_bytes());
            hash.update(&[u8::from(artifact.interrupted_reclaim), u8::from(artifact.has_evidence)]);
            // Commit the decision, not elapsed age. Waiting a second preserves
            // approval; crossing the retention threshold changes the decision.
            field(&mut hash, &serde_json::to_vec(&artifact.disposition).map_err(io::Error::other)?);
        }
        Some(format!("{APPROVAL_PREFIX}{}", hash.finalize().to_hex()))
    } else {
        None
    };
    Ok(SealedPlan {
        view: CleanupPlan { schema_version: 1, data_dir, approval_fingerprint, inventory },
        roots: owners,
    })
}

/// Read-only preview, including on an unreadable archive. Does not create or
/// acquire a lock. Incomplete/disabled previews carry no approval fingerprint.
pub fn plan(data_dir: &Path) -> io::Result<CleanupPlan> {
    Ok(seal(data_dir, SystemTime::now(), retention_enabled()?, Limits::default())?.view)
}

/// Reject malformed approval before even opening a lock file.
pub fn valid_approval(value: &str) -> bool {
    value.strip_prefix(APPROVAL_PREFIX).is_some_and(|digest|
        digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
}

fn execute(sealed: SealedPlan, lease: &lease::Lease, now: SystemTime, limits: Limits) -> CleanupReport {
    let plan = sealed.view;
    let selected_bundles = plan.inventory.artifacts.iter()
        .filter(|artifact| artifact.disposition == Disposition::Eligible).count();
    let mut report = CleanupReport {
        schema_version: 1,
        data_dir: plan.data_dir,
        approval_fingerprint: plan.approval_fingerprint.unwrap_or_default(),
        complete: true,
        selected_bundles,
        retained_bundles: plan.inventory.artifacts.len() - selected_bundles,
        attempted_bundles: 0,
        removed_bundles: 0,
        removed_logical_bytes: 0,
        errors: Vec::new(),
    };
    let mut budget = Budget::new(limits);
    for artifact in plan.inventory.artifacts.iter().filter(|a| a.disposition == Disposition::Eligible) {
        if let Err(error) = lease.check().and_then(|()| check_roots(&sealed.roots)) {
            report.complete = false;
            report.errors.push(error.to_string());
            break;
        }
        // Conservative: reclaim_one can fail before mutation, after its rename,
        // or during recursive deletion. Never call a failed attempt read-only.
        report.attempted_bundles += 1;
        if let Err(error) = retention::reclaim_one(artifact, now, &mut budget) {
            report.complete = false;
            report.errors.push(format!("{}: {error}", artifact.path.display()));
            break;
        }
        report.removed_bundles += 1;
        report.removed_logical_bytes = report.removed_logical_bytes.saturating_add(artifact.size_bytes);
    }
    report
}

fn apply_at(data_dir: &Path, approval: &str, now: SystemTime, enabled: bool, limits: Limits)
    -> Result<CleanupReport, CleanupError>
{
    if !valid_approval(approval) { return Err(CleanupError::InvalidApproval); }
    if !enabled { return Err(CleanupError::Disabled); }
    let data_dir = absolute_directory(data_dir)?;
    let lease = lease::Lease::acquire(&data_dir)?;
    let sealed = seal(&data_dir, now, enabled, limits)?;
    if !sealed.view.inventory.complete { return Err(CleanupError::Incomplete); }
    if sealed.view.approval_fingerprint.as_deref() != Some(approval) {
        return Err(CleanupError::ApprovalChanged);
    }
    lease.check()?;
    Ok(execute(sealed, &lease, now, limits))
}

/// Explicit mutation. Re-inventory and compare approval under the SAME permanent
/// OS lock used by normal indexing; never trust caller-provided target paths.
/// Pre-admission errors perform no quarantine mutation. Once execution starts,
/// inspect `complete`, attempted/removed counts and errors even when this is Ok.
/// The lock file may be created but is never truncated, removed or renamed.
pub fn apply(data_dir: &Path, approval: &str) -> Result<CleanupReport, CleanupError> {
    if !valid_approval(approval) { return Err(CleanupError::InvalidApproval); }
    apply_at(data_dir, approval, SystemTime::now(), retention_enabled()?, Limits::default())
}

#[cfg(test)]
mod tests;
