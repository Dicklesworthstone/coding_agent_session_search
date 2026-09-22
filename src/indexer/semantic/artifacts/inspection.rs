//! Operator approval is a snapshot of ownership and file identities, not an
//! integrity attestation for vectors. Never hash multi-GB vector payloads just
//! to report reclaimable scratch. Apply always rediscovers under both locks.

use super::*;
use std::time::SystemTime;

const PLAN_VERSION: u32 = 1;
const MAX_INSPECTION_ENTRIES: usize = 100_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BackfillArtifactCandidate {
    /// Relative to data_dir; always a direct child of vector_index.
    pub path: PathBuf,
    pub directory: bool,
    /// Logical regular-file bytes; symlink targets are never counted.
    pub size_bytes: u64,
    /// Includes this entry and all children of a scratch directory.
    pub entry_count: u64,
    pub identity_fingerprint: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct BackfillArtifactReclaimPlan {
    pub schema_version: u32,
    pub data_dir: PathBuf,
    pub candidates: Vec<BackfillArtifactCandidate>,
    pub reclaimable_files: u64,
    pub reclaimable_directories: u64,
    pub reclaimable_bytes: u64,
    pub checkpoint_missing: bool,
    /// Pass this exact value to apply; stale approvals delete nothing.
    pub plan_fingerprint: String,
}

/// Preview the same candidates as automatic recovery without changing any
/// manifest, checkpoint, vector or scratch artifact. This acquires both leases
/// and may create their lock files, but never creates an archive/vector root.
/// No canonical DB or model is opened, so low-disk recovery stays lightweight.
pub fn plan_backfill_artifacts(data_dir: &Path) -> Result<BackfillArtifactReclaimPlan> {
    let (_index_lock, artifacts) = maintenance_locks(data_dir)?;
    let (plan, _) = capture_plan(&artifacts, false)?;
    Ok(plan)
}

/// Apply a preview only if ownership, archive root and candidate identities
/// still match. No caller-supplied paths are used for deletion. Make current
/// metadata/checkpoint durability explicit before retiring older artifacts.
pub fn apply_backfill_artifact_plan(
    data_dir: &Path,
    expected_fingerprint: &str,
) -> Result<BackfillArtifactReclaimReport> {
    ensure!(
        expected_fingerprint.len() == 64
            && expected_fingerprint
                .bytes()
                .all(|byte| { byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte) }),
        "invalid semantic recovery plan fingerprint; no artifacts removed"
    );
    let (_index_lock, artifacts) = maintenance_locks(data_dir)?;
    let (plan, candidates) = capture_plan(&artifacts, true)?;
    ensure!(
        !plan.checkpoint_missing,
        "resumable checkpoint is missing; no artifacts removed"
    );
    ensure!(
        plan.plan_fingerprint == expected_fingerprint,
        "semantic recovery plan changed; preview again; no artifacts removed"
    );
    artifacts.remove_candidates(candidates)
}

fn maintenance_locks(data_dir: &Path) -> Result<(File, BackfillArtifacts)> {
    let data_dir = data_dir
        .canonicalize()
        .context("recovery requires an existing data directory")?;
    ensure!(data_dir.is_dir(), "recovery data path is not a directory");
    let root = data_dir.join(VECTOR_INDEX_DIR);
    let metadata =
        fs::symlink_metadata(&root).context("recovery requires an existing vector_index")?;
    ensure!(
        metadata.is_dir() && !is_link_or_reparse(&metadata),
        "recovery requires a real vector_index directory"
    );
    let index_lock = lock_file(&data_dir.join("index-run.lock"))
        .context("cannot inspect/reclaim semantic artifacts while an index writer is active")?;
    let artifacts = BackfillArtifacts::lock(&data_dir)?;
    Ok((index_lock, artifacts))
}

fn capture_plan(
    artifacts: &BackfillArtifacts,
    durable: bool,
) -> Result<(BackfillArtifactReclaimPlan, BTreeSet<PathBuf>)> {
    let discovery = artifacts.discover(None, None, durable)?;
    let mut witness = blake3::Hasher::new();
    witness.update(b"cass-backfill-reclaim-plan-v1\0");
    bind(
        &mut witness,
        &(
            PLAN_VERSION,
            &artifacts.data_dir,
            &discovery.metadata_fingerprint,
            discovery.checkpoint_missing,
        ),
    )?;
    let root = artifacts.data_dir.join(VECTOR_INDEX_DIR);
    bind(&mut witness, &stamp(&root)?)?;
    let mut budget = MAX_INSPECTION_ENTRIES;
    // Bind resolved aliases and their main/WAL identities too. A changed
    // checkpoint at the same pathname invalidates the old approval.
    for path in &discovery.protected.paths {
        consume(&mut budget)?;
        bind(&mut witness, &(path, stamp(path)?))?;
    }
    let mut plan = BackfillArtifactReclaimPlan {
        schema_version: PLAN_VERSION,
        data_dir: artifacts.data_dir.clone(),
        candidates: Vec::new(),
        reclaimable_files: 0,
        reclaimable_directories: 0,
        reclaimable_bytes: 0,
        checkpoint_missing: discovery.checkpoint_missing,
        plan_fingerprint: String::new(),
    };
    for path in &discovery.candidates {
        let candidate = snapshot(&artifacts.data_dir, path, &mut budget)?;
        plan.reclaimable_files += u64::from(!candidate.directory);
        plan.reclaimable_directories += u64::from(candidate.directory);
        plan.reclaimable_bytes = plan
            .reclaimable_bytes
            .checked_add(candidate.size_bytes)
            .context("semantic recovery byte count overflow")?;
        plan.candidates.push(candidate);
    }
    bind(&mut witness, &plan.candidates)?;
    plan.plan_fingerprint = witness.finalize().to_hex().to_string();
    Ok((plan, discovery.candidates))
}

#[derive(Debug, Serialize)]
struct FileStamp {
    kind: &'static str,
    len: u64,
    modified: SystemTime,
    created: Option<SystemTime>,
    // Device/inode and ctime prevent same-length rewrites or replacements
    // from reusing a preview even when a tool restores the original mtime.
    #[cfg(unix)]
    unix_identity: (u64, u64, i64, i64, u32, u64),
    link_target: Option<PathBuf>,
}

fn stamp(path: &Path) -> Result<Option<FileStamp>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    };
    let kind = if is_link_or_reparse(&metadata) {
        "symlink"
    } else if metadata.is_file() {
        "file"
    } else if metadata.is_dir() {
        "directory"
    } else {
        bail!(
            "unsupported filesystem entry {}; refusing recovery",
            path.display()
        );
    };
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    Ok(Some(FileStamp {
        kind,
        len: metadata.len(),
        modified: metadata.modified()?,
        created: metadata.created().ok(),
        #[cfg(unix)]
        unix_identity: (
            metadata.dev(),
            metadata.ino(),
            metadata.ctime(),
            metadata.ctime_nsec(),
            metadata.mode(),
            metadata.nlink(),
        ),
        link_target: if kind == "symlink" {
            Some(fs::read_link(path)?)
        } else {
            None
        },
    }))
}

fn snapshot(data_dir: &Path, path: &Path, budget: &mut usize) -> Result<BackfillArtifactCandidate> {
    let first = stamp(path)?.context("recovery candidate disappeared")?;
    ensure!(
        matches!(first.kind, "file" | "directory"),
        "candidate became a link"
    );
    let mut candidate = BackfillArtifactCandidate {
        path: path.strip_prefix(data_dir)?.to_path_buf(),
        directory: first.kind == "directory",
        size_bytes: 0,
        entry_count: 0,
        identity_fingerprint: String::new(),
    };
    let mut witness = blake3::Hasher::new();
    witness.update(b"cass-backfill-scratch-identity-v1\0");
    let mut pending = vec![path.to_path_buf()];
    while let Some(next) = pending.pop() {
        consume(budget)?;
        let observed = stamp(&next)?.context("recovery candidate changed during inspection")?;
        bind(&mut witness, &(next.strip_prefix(data_dir)?, &observed))?;
        candidate.entry_count += 1;
        if observed.kind == "file" {
            candidate.size_bytes = candidate
                .size_bytes
                .checked_add(observed.len)
                .context("semantic scratch byte count overflow")?;
        } else if observed.kind == "directory" {
            let mut children = Vec::new();
            for entry in fs::read_dir(&next)? {
                ensure!(
                    children.len() + pending.len() < *budget,
                    "semantic recovery inventory exceeds its entry budget"
                );
                children.push(entry?.path());
            }
            children.sort();
            pending.extend(children.into_iter().rev());
        }
        // Links are bound as leaf entries; targets are neither read nor visited.
    }
    candidate.identity_fingerprint = witness.finalize().to_hex().to_string();
    Ok(candidate)
}

fn consume(budget: &mut usize) -> Result<()> {
    *budget = budget
        .checked_sub(1)
        .context("semantic recovery inventory exceeds its entry budget")?;
    Ok(())
}

fn bind<T: Serialize + ?Sized>(hasher: &mut blake3::Hasher, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(&bytes);
    Ok(())
}

#[cfg(test)]
mod tests;
