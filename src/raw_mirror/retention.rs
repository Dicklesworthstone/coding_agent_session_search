// Automatic retention is deliberately narrower than explicit `mirror prune`:
// only a checksum-verified prefix of a retained same-source snapshot can retire.
use super::*;
use std::io::{Seek, SeekFrom};

#[derive(Debug, Clone)]
pub struct RawMirrorAutomaticRetention {
    pub capture: RawMirrorRetentionPolicy,
    pub keep_latest: usize,
    pub safety_hold_down_ms: i64,
    pub keep_tags: Vec<String>,
}

impl RawMirrorAutomaticRetention {
    pub fn from_env() -> Result<Self> {
        let keep_latest = usize::try_from(byte_setting("CASS_RAW_MIRROR_KEEP_LATEST", 2)?)?;
        if keep_latest == 0 {
            anyhow::bail!("CASS_RAW_MIRROR_KEEP_LATEST must be at least 1");
        }
        let hold = byte_setting("CASS_RAW_MIRROR_SAFETY_HOLD_DOWN_SECS", 6 * 60 * 60)?;
        let safety_hold_down_ms = hold.checked_mul(1000)
            .and_then(|ms| i64::try_from(ms).ok())
            .ok_or_else(|| anyhow!("CASS_RAW_MIRROR_SAFETY_HOLD_DOWN_SECS is too large"))?;
        let tags = match dotenvy::var("CASS_RAW_MIRROR_KEEP_TAGS") {
            Ok(tags) => tags,
            Err(dotenvy::Error::EnvVar(std::env::VarError::NotPresent)) => "keep,pinned,recovery".to_string(),
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            capture: RawMirrorRetentionPolicy::from_env()?,
            keep_latest,
            safety_hold_down_ms,
            keep_tags: tags.split(',').map(str::trim).filter(|tag| !tag.is_empty())
                .map(ToOwned::to_owned).collect(),
        })
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RawMirrorMaintenanceReport {
    pub used_bytes_before: u64,
    pub used_bytes_after: u64,
    pub available_bytes_after: u64,
    pub retired_manifests: u64,
    pub reclaimed_bytes: u64,
    pub protected_manifests: u64,
    pub capture_paused: bool,
}

/// Run before an indexer acquires its index-run lease and before disk preflight.
/// Errors must be diagnostic-only at the CLI boundary: optional preservation
/// must not disable otherwise viable indexing. No guard or safety check is
/// disabled here, and both locks are released before the caller starts indexing.
pub fn maintain_before_index(data_dir: &Path) -> Result<RawMirrorMaintenanceReport> {
    let Some(policy) = automatic_retention_policy_from_env()? else {
        return Ok(RawMirrorMaintenanceReport::default());
    };
    maintain(data_dir, &policy, now_ms())
}

/// The index owner already holds the data-dir lease before its observed
/// preflight starts. Require that concrete lease, bound to this directory,
/// instead of attempting to lock the same file through a second handle.
pub(crate) fn maintain_while_index_owned(
    data_dir: &Path,
    index_lease: &crate::indexer::IndexRunLockGuard,
) -> Result<RawMirrorMaintenanceReport> {
    let Some(policy) = automatic_retention_policy_from_env()? else {
        return Ok(RawMirrorMaintenanceReport::default());
    };
    maintain_with_index_lease(data_dir, &policy, now_ms(), Some(index_lease))
}

fn automatic_retention_policy_from_env() -> Result<Option<RawMirrorAutomaticRetention>> {
    if !super::super::super::capture_enabled() {
        return Ok(None);
    }
    if let Ok(value) = dotenvy::var("CASS_RAW_MIRROR_AUTO_PRUNE")
        && matches!(value.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no" | "off")
    {
        return Ok(None);
    }
    RawMirrorAutomaticRetention::from_env().map(Some)
}

fn content_chunks(manifest: &RawMirrorManifestFile) -> Vec<RawMirrorChunkRef> {
    match &manifest.content_storage {
        Some(storage) => storage.chunks.clone(),
        None => vec![RawMirrorChunkRef {
            blob_relative_path: manifest.blob_relative_path.clone(),
            blob_blake3: manifest.blob_blake3.clone(),
            blob_size_bytes: manifest.blob_size_bytes,
        }],
    }
}

// Skip equal full chunks by their already-verified content address. Only a
// changing tail (or a legacy whole-blob/chunk boundary) needs byte comparison.
fn preserved_prefix(root: &Path, old: &RawMirrorManifestFile, new: &RawMirrorManifestFile) -> Result<bool> {
    if old.source_size_bytes > new.source_size_bytes { return Ok(false); }
    let left = content_chunks(old);
    let right = content_chunks(new);
    let (mut i, mut j, mut a, mut b) = (0_usize, 0_usize, 0_u64, 0_u64);
    let mut compared = 0_u64;
    let mut left_bytes = [0_u8; 64 * 1024];
    let mut right_bytes = [0_u8; 64 * 1024];
    while compared < old.source_size_bytes {
        raw_mirror_checkpoint()?;
        let l = left.get(i).ok_or_else(|| anyhow!("short old raw-mirror content"))?;
        let r = right.get(j).ok_or_else(|| anyhow!("short successor raw-mirror content"))?;
        let remaining_left = l.blob_size_bytes.checked_sub(a).ok_or_else(|| anyhow!("invalid chunk offset"))?;
        let remaining_right = r.blob_size_bytes.checked_sub(b).ok_or_else(|| anyhow!("invalid chunk offset"))?;
        if remaining_left == 0 { i += 1; a = 0; continue; }
        if remaining_right == 0 { j += 1; b = 0; continue; }
        let length = remaining_left.min(remaining_right).min(old.source_size_bytes - compared);
        if a == 0 && b == 0 && l.blob_size_bytes == r.blob_size_bytes {
            if l.blob_blake3 != r.blob_blake3 { return Ok(false); }
            compared += length;
            a += length;
            b += length;
            continue;
        }
        let old_path = root.join(&l.blob_relative_path);
        let new_path = root.join(&r.blob_relative_path);
        let mut old_file =
            raw_mirror_io("open_retention_prefix", &old_path, || File::open(&old_path))?;
        let mut new_file =
            raw_mirror_io("open_retention_prefix", &new_path, || File::open(&new_path))?;
        raw_mirror_io("seek_retention_prefix", &old_path, || {
            old_file.seek(SeekFrom::Start(a))
        })?;
        raw_mirror_io("seek_retention_prefix", &new_path, || {
            new_file.seek(SeekFrom::Start(b))
        })?;
        let mut remaining = length;
        while remaining > 0 {
            let count = remaining.min(left_bytes.len() as u64) as usize;
            raw_mirror_io("read_retention_prefix", &old_path, || {
                old_file.read_exact(&mut left_bytes[..count])
            })?;
            raw_mirror_io("read_retention_prefix", &new_path, || {
                new_file.read_exact(&mut right_bytes[..count])
            })?;
            if left_bytes[..count] != right_bytes[..count] { return Ok(false); }
            remaining -= count as u64;
        }
        compared += length;
        a += length;
        b += length;
    }
    Ok(true)
}

fn ordinary_capture(manifest: &RawMirrorManifestFile) -> bool {
    manifest.verification.status == "captured"
        && manifest.verification.verifier == "cass_indexer"
        && manifest.encryption.state == "none"
        && manifest.compression.state == "none"
}

fn verify_capture(root: &Path, manifest: &RawMirrorManifestFile, verified: &mut HashSet<String>) -> Result<()> {
    if verified.insert(manifest.manifest_id.clone()) {
        // Blob hashes alone are insufficient: the saved descriptor must also
        // agree with the manifest and the concatenated content hash. Reuse the
        // streaming recovery verifier, including its 64 KiB buffers.
        let data_dir = root.parent().and_then(Path::parent)
            .ok_or_else(|| anyhow!("raw-mirror root has no data directory"))?;
        verify_source_capture(data_dir, &manifest.manifest_id)?;
    }
    Ok(())
}

fn source_key(manifest: &RawMirrorManifestFile) -> Result<String> {
    Ok(serde_json::to_string(&(
        &manifest.provider, &manifest.source_id, &manifest.origin_kind,
        &manifest.origin_host, &manifest.original_path_blake3,
    ))?)
}

fn maintain(data_dir: &Path, policy: &RawMirrorAutomaticRetention, now: i64) -> Result<RawMirrorMaintenanceReport> {
    maintain_with_index_lease(data_dir, policy, now, None)
}

fn maintain_with_index_lease(
    data_dir: &Path,
    policy: &RawMirrorAutomaticRetention,
    now: i64,
    index_lease: Option<&crate::indexer::IndexRunLockGuard>,
) -> Result<RawMirrorMaintenanceReport> {
    if policy.keep_latest == 0 || policy.safety_hold_down_ms < 0 {
        anyhow::bail!("invalid automatic raw-mirror retention policy");
    }
    if let Some(lease) = index_lease {
        anyhow::ensure!(
            lease.protects_data_dir(data_dir),
            "automatic retention requires the index lease for this data directory"
        );
    }
    let root = raw_mirror_root(data_dir);
    match raw_mirror_symlink_metadata(&root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(RawMirrorMaintenanceReport::default()),
        Err(error) => return Err(error.into()),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            anyhow::bail!("refusing automatic retention on an invalid mirror root");
        }
        Ok(_) => {}
    }
    // Acquire the nonblocking index lease FIRST; never wait for an indexer
    // while holding the mirror lock it needs for capture.
    let _index_run = if index_lease.is_some() {
        None
    } else {
        Some(try_acquire_index_run_lock_for_prune(data_dir)?)
    };
    let _mutation = acquire_raw_mirror_mutation_lock(&root)?;
    let used = measured_usage(&root)?;
    let available = raw_mirror_io("available_space", &root, || fs2::available_space(&root))?;
    let mut result = RawMirrorMaintenanceReport {
        used_bytes_before: used, used_bytes_after: used, available_bytes_after: available,
        ..RawMirrorMaintenanceReport::default()
    };
    let inventory = collect_prune_manifests(&root)?;
    let physical: HashMap<_, _> = collect_raw_mirror_physical_blobs(&root)?.into_iter()
        .map(|blob| (blob.relative_path.clone(), blob)).collect();
    for manifest in &inventory {
        raw_mirror_checkpoint()?;
        for reference in &manifest.blob_references {
            let blob = physical.get(&reference.blob_relative_path)
                .ok_or_else(|| anyhow!("automatic retention refused: a referenced blob is missing"))?;
            if blob.blob_blake3 != reference.blob_blake3 || blob.size_bytes != reference.blob_size_bytes {
                anyhow::bail!("automatic retention refused: a referenced blob has drifted");
            }
        }
    }
    let mut protected = pinned_prune_manifest_ids(data_dir, &inventory, &policy.keep_tags,
        policy.safety_hold_down_ms, now)?;
    let mut manifests = HashMap::new();
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for item in &inventory {
        raw_mirror_checkpoint()?;
        let manifest = read_validated_raw_mirror_manifest(&root, &item.manifest_id)?;
        groups.entry(source_key(&manifest)?).or_default().push(item.manifest_id.clone());
        if !ordinary_capture(&manifest) { protected.insert(item.manifest_id.clone()); }
        manifests.insert(item.manifest_id.clone(), manifest);
    }
    // Inventory is oldest first; reverse only within the exact origin identity.
    for ids in groups.values_mut() {
        raw_mirror_checkpoint()?;
        ids.sort_by(|left, right| {
            let a = &manifests[left];
            let b = &manifests[right];
            b.captured_at_ms.cmp(&a.captured_at_ms)
                .then_with(|| b.source_size_bytes.cmp(&a.source_size_bytes))
                .then_with(|| right.cmp(left))
        });
        protected.extend(ids.iter().take(policy.keep_latest).cloned());
    }
    let mut selected: BTreeMap<String, String> = BTreeMap::new();
    let mut verified = HashSet::new();
    for ids in groups.values() {
        raw_mirror_checkpoint()?;
        let anchors: Vec<_> = ids.iter().filter(|id| protected.contains(*id)).collect();
        for id in ids {
            raw_mirror_checkpoint()?;
            if protected.contains(id) { continue; }
            let old = &manifests[id];
            for successor in &anchors {
                let new = &manifests[*successor];
                if !ordinary_capture(new) || new.captured_at_ms < old.captured_at_ms
                    || new.source_size_bytes < old.source_size_bytes { continue; }
                verify_capture(&root, old, &mut verified)?;
                verify_capture(&root, new, &mut verified)?;
                if preserved_prefix(&root, old, new)? {
                    selected.insert(id.clone(), (*successor).clone());
                    break;
                }
            }
        }
    }
    result.protected_manifests = inventory.len() as u64 - selected.len() as u64;
    // Orphans, abandoned tmp files, nonstandard evidence and divergent versions
    // are NEVER automatically selected. Explicit prune remains operator policy.
    let mut references: BTreeMap<String, (RawMirrorChunkRef, bool)> = BTreeMap::new();
    for item in &inventory {
        raw_mirror_checkpoint()?;
        for reference in &item.blob_references {
            let entry = references.entry(reference.blob_relative_path.clone())
                .or_insert_with(|| (reference.clone(), false));
            if entry.0.blob_blake3 != reference.blob_blake3
                || entry.0.blob_size_bytes != reference.blob_size_bytes {
                anyhow::bail!("raw-mirror manifests disagree about a shared blob");
            }
            entry.1 |= !selected.contains_key(&item.manifest_id);
        }
    }
    let mut report = RawMirrorPruneReport {
        initialized: true, root_path: root.display().to_string(), mode: "apply".to_string(),
        manifest_count: inventory.len() as u64, unique_blob_count: references.len() as u64,
        current_blob_bytes: references.values().map(|(reference, _)| reference.blob_size_bytes)
            .fold(0, u64::saturating_add),
        providers: Vec::new(), source_path: None, scope_blob_bytes: None,
        orphan_blob_count: 0, orphan_blob_bytes: 0,
        safety_hold_down_ms: policy.safety_hold_down_ms, keep_tags: policy.keep_tags.clone(),
        keep_latest: Some(policy.keep_latest),
        pinned_manifest_count: result.protected_manifests,
        pinned_blob_count: references.values().filter(|(_, retained)| *retained).count() as u64,
        planned_manifest_count: selected.len() as u64, planned_blob_count: 0, planned_reclaim_bytes: 0,
        planned_staging_count: 0,
        applied_manifest_count: 0, applied_blob_count: 0, applied_staging_count: 0, applied_reclaim_bytes: 0,
        audit_log_path: None, omitted_entry_count: 0, entries: Vec::new(),
    };
    for item in &inventory {
        raw_mirror_checkpoint()?;
        if let Some(successor) = selected.get(&item.manifest_id) {
            let old = &manifests[&item.manifest_id];
            report.entries.push(RawMirrorPruneEntry {
                kind: "manifest".to_string(), path: item.relative_path.clone(),
                blob_blake3: Some(item.blob_blake3.clone()), manifest_blake3: Some(item.manifest_blake3.clone()),
                size_bytes: item.size_bytes, applied: false,
                reason: format!("automatic: verified {}-byte prefix of retained manifest {}; content_blake3={}",
                    old.source_size_bytes, successor, old.verification.content_blake3.as_deref().unwrap_or("unknown")),
            });
        }
    }
    for (path, (reference, retained)) in &references {
        raw_mirror_checkpoint()?;
        if !retained {
            report.planned_blob_count += 1;
            report.entries.push(RawMirrorPruneEntry {
                kind: "blob".to_string(), path: path.clone(),
                blob_blake3: Some(reference.blob_blake3.clone()), manifest_blake3: None,
                size_bytes: reference.blob_size_bytes, applied: false,
                reason: "automatic: all referencing snapshots have verified retained successors".to_string(),
            });
        }
    }
    report.planned_reclaim_bytes = report.entries.iter().map(|entry| entry.size_bytes)
        .fold(0, u64::saturating_add);
    for entry in &report.entries { validate_prune_target_file(&root, &root.join(&entry.path))?; }
    if !report.entries.is_empty() {
        note_mutation(&root)?;
        // Preserve DB links before retiring descriptors. A failed merge leaves
        // every old descriptor intact; a crash can only add redundant links.
        for (id, successor) in &selected {
            let new = &manifests[successor];
            merge_raw_mirror_manifest_db_links(&root,
                &root.join(raw_mirror_manifest_relative_path(successor)),
                &manifests[id].db_links, Some(&new.blob_blake3))?;
        }
        // Reuse the explicit pruner's durable intent-before-delete protocol,
        // per-target validation, manifest-before-blob ordering and fsyncs.
        let (audit_path, mut audit_file) = open_prune_audit_log(&root)?;
        report.audit_log_path = Some(audit_path.display().to_string());
        apply_prune_entries(&root, &mut report, &audit_path, &mut audit_file)?;
    }
    result.retired_manifests = report.applied_manifest_count;
    result.reclaimed_bytes = report.applied_reclaim_bytes;
    result.used_bytes_after = measured_usage(&root)?;
    result.available_bytes_after =
        raw_mirror_io("available_space", &root, || fs2::available_space(&root))?;
    result.capture_paused = result.used_bytes_after >= policy.capture.max_bytes
        || result.available_bytes_after <= policy.capture.min_free_bytes;
    Ok(result)
}

#[cfg(test)]
mod tests {
    include!("retention_tests.rs");
}
