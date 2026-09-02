use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const RAW_MIRROR_SCHEMA_VERSION: u32 = 1;
const RAW_MIRROR_ROOT_DIR: &str = "raw-mirror";
const RAW_MIRROR_VERSION_DIR: &str = "v1";
const RAW_MIRROR_MANIFEST_KIND: &str = "cass_raw_session_mirror_v1";
const RAW_MIRROR_HASH_ALGORITHM: &str = "blake3";
const RAW_MIRROR_BLOB_EXTENSION: &str = "raw";
const RAW_MIRROR_MANIFEST_MAX_BYTES: u64 = 16 * 1024 * 1024;
const RAW_MIRROR_CHUNK_SIZE_BYTES: usize = 4 * 1024 * 1024;
const RAW_MIRROR_CHUNK_THRESHOLD_BYTES: u64 = 8 * 1024 * 1024;
const RAW_MIRROR_FIXED_CHUNKS_KIND: &str = "fixed_chunks_v1";
const RAW_MIRROR_BLOB_CACHE_MAX_ENTRIES: usize = 16_384;
const RAW_MIRROR_MUTATION_LOCK_FILE: &str = ".mutation.lock";

static TEMP_NONCE: AtomicU64 = AtomicU64::new(0);
static BLOB_CAPTURE_CACHE: OnceLock<
    Mutex<HashMap<RawMirrorBlobSourceKey, CachedRawMirrorBlobRecord>>,
> = OnceLock::new();
static MANIFEST_UPDATE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[derive(Debug)]
struct RawMirrorFileLockGuard {
    file: File,
}

impl Drop for RawMirrorFileLockGuard {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

fn raw_mirror_fsync_enabled() -> bool {
    dotenvy::var("CASS_RAW_MIRROR_FSYNC")
        .ok()
        .is_some_and(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
}

#[derive(Debug, Clone)]
pub struct RawMirrorCaptureInput<'a> {
    pub data_dir: &'a Path,
    pub provider: &'a str,
    pub source_id: &'a str,
    pub origin_kind: &'a str,
    pub origin_host: Option<&'a str>,
    pub source_path: &'a Path,
    pub db_links: &'a [RawMirrorDbLink],
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RawMirrorCaptureRecord {
    pub manifest_id: String,
    pub manifest_relative_path: String,
    pub blob_relative_path: String,
    pub blob_blake3: String,
    pub blob_size_bytes: u64,
    pub source_content_blake3: String,
    pub source_size_bytes: u64,
    pub storage_kind: String,
    pub chunk_count: usize,
    pub captured_at_ms: i64,
    pub source_mtime_ms: Option<i64>,
    pub already_present: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawMirrorVerifiedCapture {
    pub storage_kind: String,
    pub source_content_blake3: String,
    pub source_size_bytes: u64,
    pub stored_blob_count: usize,
    pub stored_bytes: u64,
    pub stored_blobs: Vec<(String, u64)>,
    pub chunk_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RawMirrorDbLink {
    pub conversation_id: Option<i64>,
    pub message_count: Option<usize>,
    pub source_path: Option<String>,
    pub started_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RawMirrorStorageSummary {
    pub initialized: bool,
    pub root_path: String,
    pub total_storage_bytes: u64,
    pub manifest_count: u64,
    pub manifest_bytes: u64,
    pub unique_blob_count: u64,
    pub total_blob_bytes: u64,
    pub largest_blob_bytes: u64,
    pub orphan_blob_count: u64,
    pub orphan_blob_bytes: u64,
    pub missing_blob_count: u64,
    pub invalid_manifest_count: u64,
    pub oldest_capture_at_ms: Option<i64>,
    pub newest_capture_at_ms: Option<i64>,
    pub oldest_source_mtime_ms: Option<i64>,
    pub newest_source_mtime_ms: Option<i64>,
}

pub fn storage_summary(data_dir: &Path) -> RawMirrorStorageSummary {
    let root = raw_mirror_root(data_dir);
    let mut summary = RawMirrorStorageSummary {
        root_path: root.display().to_string(),
        ..RawMirrorStorageSummary::default()
    };
    let root_metadata = match fs::symlink_metadata(&root) {
        Ok(metadata) => metadata,
        Err(_) => return summary,
    };
    summary.initialized = true;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        summary.invalid_manifest_count = 1;
        return summary;
    }

    summary.total_storage_bytes = raw_mirror_dir_file_bytes(&root);

    let manifests_dir = root.join("manifests");
    let Ok(manifests_metadata) = fs::symlink_metadata(&manifests_dir) else {
        populate_raw_mirror_orphan_summary(&root, &HashSet::new(), &mut summary);
        return summary;
    };
    if manifests_metadata.file_type().is_symlink() || !manifests_metadata.is_dir() {
        summary.invalid_manifest_count = summary.invalid_manifest_count.saturating_add(1);
        return summary;
    }
    let entries = match fs::read_dir(&manifests_dir) {
        Ok(entries) => entries,
        Err(_) => return summary,
    };
    let mut seen_blobs = HashSet::new();
    for entry in entries {
        let Ok(entry) = entry else {
            summary.invalid_manifest_count = summary.invalid_manifest_count.saturating_add(1);
            continue;
        };
        let path = entry.path();
        let manifest_metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => metadata,
            _ => {
                summary.invalid_manifest_count = summary.invalid_manifest_count.saturating_add(1);
                continue;
            }
        };
        summary.manifest_bytes = summary
            .manifest_bytes
            .saturating_add(manifest_metadata.len());
        let manifest = match read_raw_mirror_manifest(&path) {
            Ok(manifest) if manifest.manifest_kind == RAW_MIRROR_MANIFEST_KIND => manifest,
            _ => {
                summary.invalid_manifest_count = summary.invalid_manifest_count.saturating_add(1);
                continue;
            }
        };
        summary.manifest_count = summary.manifest_count.saturating_add(1);
        let expected_path = root.join(raw_mirror_manifest_relative_path(&manifest.manifest_id));
        let blob_references = if path == expected_path {
            validate_raw_mirror_manifest_contents(&manifest, &manifest.manifest_id).ok()
        } else {
            None
        };
        let Some(blob_references) = blob_references else {
            summary.invalid_manifest_count = summary.invalid_manifest_count.saturating_add(1);
            continue;
        };
        merge_min_max(
            &mut summary.oldest_capture_at_ms,
            &mut summary.newest_capture_at_ms,
            Some(manifest.captured_at_ms),
        );
        merge_min_max(
            &mut summary.oldest_source_mtime_ms,
            &mut summary.newest_source_mtime_ms,
            manifest.source_mtime_ms,
        );

        for blob_reference in blob_references {
            if !seen_blobs.insert(blob_reference.blob_relative_path.clone()) {
                continue;
            }
            let blob_path = root.join(&blob_reference.blob_relative_path);
            if raw_mirror_path_has_symlink_below_root(&root, &blob_path) {
                summary.missing_blob_count = summary.missing_blob_count.saturating_add(1);
                continue;
            }
            match fs::symlink_metadata(&blob_path) {
                Ok(metadata)
                    if metadata.is_file()
                        && !metadata.file_type().is_symlink()
                        && metadata.len() == blob_reference.blob_size_bytes =>
                {
                    let size = metadata.len();
                    summary.unique_blob_count = summary.unique_blob_count.saturating_add(1);
                    summary.total_blob_bytes = summary.total_blob_bytes.saturating_add(size);
                    summary.largest_blob_bytes = summary.largest_blob_bytes.max(size);
                }
                _ => {
                    summary.missing_blob_count = summary.missing_blob_count.saturating_add(1);
                }
            }
        }
    }

    if summary.invalid_manifest_count == 0 {
        populate_raw_mirror_orphan_summary(&root, &seen_blobs, &mut summary);
    }

    summary
}

fn populate_raw_mirror_orphan_summary(
    root: &Path,
    referenced_blob_paths: &HashSet<String>,
    summary: &mut RawMirrorStorageSummary,
) {
    let Ok(physical_blobs) = collect_raw_mirror_physical_blobs(root) else {
        return;
    };
    for blob in physical_blobs {
        if !referenced_blob_paths.contains(&blob.relative_path) {
            summary.orphan_blob_count = summary.orphan_blob_count.saturating_add(1);
            summary.orphan_blob_bytes = summary.orphan_blob_bytes.saturating_add(blob.size_bytes);
        }
    }
}

pub(crate) fn physical_storage_bytes(data_dir: &Path) -> u64 {
    raw_mirror_dir_file_bytes(&raw_mirror_root(data_dir))
}

#[derive(Debug, Clone, Default)]
pub struct RawMirrorPruneOptions {
    pub older_than_ms: Option<i64>,
    pub max_size_bytes: Option<u64>,
    pub keep_tags: Vec<String>,
    pub safety_hold_down_ms: i64,
    pub apply: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RawMirrorPruneReport {
    pub initialized: bool,
    pub root_path: String,
    pub mode: String,
    pub manifest_count: u64,
    pub unique_blob_count: u64,
    pub current_blob_bytes: u64,
    pub orphan_blob_count: u64,
    pub orphan_blob_bytes: u64,
    pub safety_hold_down_ms: i64,
    pub keep_tags: Vec<String>,
    pub pinned_manifest_count: u64,
    pub pinned_blob_count: u64,
    pub planned_manifest_count: u64,
    pub planned_blob_count: u64,
    pub planned_reclaim_bytes: u64,
    pub applied_manifest_count: u64,
    pub applied_blob_count: u64,
    pub applied_reclaim_bytes: u64,
    pub audit_log_path: Option<String>,
    pub entries: Vec<RawMirrorPruneEntry>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RawMirrorPruneEntry {
    pub kind: String,
    pub path: String,
    pub blob_blake3: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest_blake3: Option<String>,
    pub size_bytes: u64,
    pub reason: String,
    pub applied: bool,
}

#[derive(Debug, Clone)]
struct RawMirrorPruneManifest {
    manifest_id: String,
    manifest_blake3: String,
    blob_blake3: String,
    relative_path: String,
    size_bytes: u64,
    blob_references: Vec<RawMirrorChunkRef>,
    captured_at_ms: i64,
    provider: String,
    original_path: String,
    db_links: Vec<RawMirrorDbLink>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RawMirrorPhysicalBlob {
    relative_path: String,
    blob_blake3: String,
    size_bytes: u64,
    modified_at_ms: Option<i64>,
}

pub fn prune(data_dir: &Path, options: RawMirrorPruneOptions) -> Result<RawMirrorPruneReport> {
    let root = raw_mirror_root(data_dir);
    let mut report = RawMirrorPruneReport {
        initialized: false,
        root_path: root.display().to_string(),
        mode: if options.apply {
            "apply".to_string()
        } else {
            "dry-run".to_string()
        },
        manifest_count: 0,
        unique_blob_count: 0,
        current_blob_bytes: 0,
        orphan_blob_count: 0,
        orphan_blob_bytes: 0,
        safety_hold_down_ms: options.safety_hold_down_ms,
        keep_tags: options.keep_tags.clone(),
        pinned_manifest_count: 0,
        pinned_blob_count: 0,
        planned_manifest_count: 0,
        planned_blob_count: 0,
        planned_reclaim_bytes: 0,
        applied_manifest_count: 0,
        applied_blob_count: 0,
        applied_reclaim_bytes: 0,
        audit_log_path: None,
        entries: Vec::new(),
    };

    let metadata = match fs::symlink_metadata(&root) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(report),
        Err(err) => {
            return Err(err).with_context(|| format!("stat raw mirror root {}", root.display()));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        anyhow::bail!(
            "refusing to prune invalid raw mirror root {}",
            root.display()
        );
    }
    report.initialized = true;
    // Dry-runs append the same audit stream as applied prunes, so both modes
    // need one stable manifest/blob view and serialized audit writes.
    let _mutation_lock = acquire_raw_mirror_mutation_lock(&root)?;
    let _index_run_lock = options
        .apply
        .then(|| try_acquire_index_run_lock_for_prune(data_dir))
        .transpose()?;

    let manifests = collect_prune_manifests(&root)?;
    report.manifest_count = manifests.len() as u64;
    let physical_blobs = collect_raw_mirror_physical_blobs(&root)?;
    let physical_blob_by_relative = physical_blobs
        .iter()
        .map(|blob| (blob.relative_path.clone(), blob))
        .collect::<BTreeMap<_, _>>();

    let mut blob_to_manifests: HashMap<String, Vec<String>> = HashMap::new();
    let mut manifest_by_id: HashMap<String, &RawMirrorPruneManifest> = HashMap::new();
    let mut blob_size_by_relative: HashMap<String, u64> = HashMap::new();
    for manifest in &manifests {
        manifest_by_id.insert(manifest.manifest_id.clone(), manifest);
        let mut seen_manifest_blobs = HashSet::new();
        for blob_reference in &manifest.blob_references {
            if !seen_manifest_blobs.insert(blob_reference.blob_relative_path.clone()) {
                continue;
            }
            blob_to_manifests
                .entry(blob_reference.blob_relative_path.clone())
                .or_default()
                .push(manifest.manifest_id.clone());
            let physical = physical_blob_by_relative
                .get(&blob_reference.blob_relative_path)
                .ok_or_else(|| {
                    anyhow!(
                        "refusing to prune because manifest {} references missing blob {}",
                        manifest.manifest_id,
                        blob_reference.blob_relative_path
                    )
                })?;
            if physical.size_bytes != blob_reference.blob_size_bytes
                || physical.blob_blake3 != blob_reference.blob_blake3
            {
                return Err(anyhow!(
                    "refusing to prune because manifest {} disagrees with physical blob {}",
                    manifest.manifest_id,
                    blob_reference.blob_relative_path
                ));
            }
            blob_size_by_relative
                .entry(blob_reference.blob_relative_path.clone())
                .or_insert(physical.size_bytes);
        }
    }
    let orphan_blobs = physical_blobs
        .iter()
        .filter(|blob| !blob_to_manifests.contains_key(&blob.relative_path))
        .collect::<Vec<_>>();
    report.unique_blob_count = physical_blobs.len() as u64;
    report.current_blob_bytes = physical_blobs
        .iter()
        .map(|blob| blob.size_bytes)
        .fold(0u64, u64::saturating_add);
    report.orphan_blob_count = orphan_blobs.len() as u64;
    report.orphan_blob_bytes = orphan_blobs
        .iter()
        .map(|blob| blob.size_bytes)
        .fold(0u64, u64::saturating_add);

    let now = now_ms();
    let pinned_manifests = pinned_prune_manifest_ids(
        data_dir,
        &manifests,
        &options.keep_tags,
        options.safety_hold_down_ms,
        now,
    )?;
    report.pinned_manifest_count = pinned_manifests.len() as u64;
    let mut pinned_blobs: HashSet<String> = blob_to_manifests
        .iter()
        .filter(|(_, manifest_ids)| manifest_ids.iter().any(|id| pinned_manifests.contains(id)))
        .map(|(blob_relative_path, _)| blob_relative_path.clone())
        .collect();
    if options.safety_hold_down_ms > 0 {
        let hold_down_cutoff = now.saturating_sub(options.safety_hold_down_ms);
        pinned_blobs.extend(
            orphan_blobs
                .iter()
                .filter(|blob| {
                    blob.modified_at_ms
                        .is_none_or(|modified_at_ms| modified_at_ms > hold_down_cutoff)
                })
                .map(|blob| blob.relative_path.clone()),
        );
    }
    report.pinned_blob_count = pinned_blobs.len() as u64;

    let mut selected_manifests: HashSet<String> = HashSet::new();
    let mut manifest_reasons: HashMap<String, String> = HashMap::new();
    let mut selected_orphan_blobs: HashSet<String> = HashSet::new();
    let mut orphan_blob_reasons: HashMap<String, String> = HashMap::new();

    if let Some(older_than_ms) = options.older_than_ms {
        let cutoff_ms = now.saturating_sub(older_than_ms.max(0));
        for manifest in &manifests {
            if manifest.captured_at_ms <= cutoff_ms
                && !pinned_manifests.contains(&manifest.manifest_id)
            {
                selected_manifests.insert(manifest.manifest_id.clone());
                manifest_reasons
                    .entry(manifest.manifest_id.clone())
                    .or_insert_with(|| format!("captured_at_ms <= {cutoff_ms}"));
            }
        }
        for blob in &orphan_blobs {
            if !pinned_blobs.contains(&blob.relative_path)
                && blob
                    .modified_at_ms
                    .is_some_and(|modified_at_ms| modified_at_ms <= cutoff_ms)
            {
                selected_orphan_blobs.insert(blob.relative_path.clone());
                orphan_blob_reasons.insert(
                    blob.relative_path.clone(),
                    format!("unreferenced blob modified_at_ms <= {cutoff_ms}"),
                );
            }
        }
    }

    if let Some(max_size_bytes) = options.max_size_bytes
        && report.current_blob_bytes > max_size_bytes
    {
        let mut projected_bytes = report.current_blob_bytes;
        for (blob_relative_path, manifest_ids) in &blob_to_manifests {
            if manifest_ids
                .iter()
                .all(|id| selected_manifests.contains(id))
            {
                projected_bytes = projected_bytes.saturating_sub(
                    blob_size_by_relative
                        .get(blob_relative_path)
                        .copied()
                        .unwrap_or(0),
                );
            }
        }
        for blob in &orphan_blobs {
            if selected_orphan_blobs.contains(&blob.relative_path) {
                projected_bytes = projected_bytes.saturating_sub(blob.size_bytes);
            }
        }

        for blob in &orphan_blobs {
            if projected_bytes <= max_size_bytes {
                break;
            }
            if pinned_blobs.contains(&blob.relative_path)
                || selected_orphan_blobs.contains(&blob.relative_path)
            {
                continue;
            }
            selected_orphan_blobs.insert(blob.relative_path.clone());
            orphan_blob_reasons
                .entry(blob.relative_path.clone())
                .or_insert_with(|| {
                    "max-size over budget; reclaiming oldest unreferenced blob".to_string()
                });
            projected_bytes = projected_bytes.saturating_sub(blob.size_bytes);
        }

        for manifest in &manifests {
            if projected_bytes <= max_size_bytes {
                break;
            }
            if pinned_manifests.contains(&manifest.manifest_id)
                || selected_manifests.contains(&manifest.manifest_id)
            {
                continue;
            }
            selected_manifests.insert(manifest.manifest_id.clone());
            manifest_reasons
                .entry(manifest.manifest_id.clone())
                .or_insert_with(|| {
                    "max-size over budget; retiring oldest unpinned capture".to_string()
                });

            let mut seen_manifest_blobs = HashSet::new();
            for blob_reference in &manifest.blob_references {
                if !seen_manifest_blobs.insert(blob_reference.blob_relative_path.clone()) {
                    continue;
                }
                let becomes_unreferenced = blob_to_manifests
                    .get(&blob_reference.blob_relative_path)
                    .is_some_and(|manifest_ids| {
                        manifest_ids
                            .iter()
                            .all(|id| selected_manifests.contains(id))
                    });
                if becomes_unreferenced {
                    projected_bytes = projected_bytes.saturating_sub(
                        blob_size_by_relative
                            .get(&blob_reference.blob_relative_path)
                            .copied()
                            .unwrap_or(0),
                    );
                }
            }
        }
    }

    let mut selected_blobs: HashSet<String> = blob_to_manifests
        .iter()
        .filter(|(_, manifest_ids)| {
            manifest_ids
                .iter()
                .all(|id| selected_manifests.contains(id))
        })
        .map(|(blob_relative_path, _)| blob_relative_path.clone())
        .collect();
    selected_blobs.extend(selected_orphan_blobs);

    let mut entries = Vec::new();
    let mut selected_manifest_ids = selected_manifests.into_iter().collect::<Vec<_>>();
    selected_manifest_ids.sort();
    for manifest_id in &selected_manifest_ids {
        let Some(manifest) = manifest_by_id.get(manifest_id) else {
            continue;
        };
        let reason = manifest_reasons
            .remove(manifest_id)
            .unwrap_or_else(|| "selected by retention policy".to_string());
        entries.push(RawMirrorPruneEntry {
            kind: "manifest".to_string(),
            path: manifest.relative_path.clone(),
            blob_blake3: Some(manifest.blob_blake3.clone()),
            manifest_blake3: Some(manifest.manifest_blake3.clone()),
            size_bytes: manifest.size_bytes,
            reason,
            applied: false,
        });
    }

    let mut selected_blob_paths = selected_blobs.into_iter().collect::<Vec<_>>();
    selected_blob_paths.sort();
    for blob_relative_path in selected_blob_paths {
        let size = physical_blob_by_relative
            .get(&blob_relative_path)
            .map(|blob| blob.size_bytes)
            .unwrap_or(0);
        let blob_blake3 = blob_relative_path
            .rsplit('/')
            .next()
            .and_then(|name| name.strip_suffix(".raw"))
            .map(ToOwned::to_owned);
        let reason = orphan_blob_reasons
            .remove(&blob_relative_path)
            .unwrap_or_else(|| {
                "no retained manifest references this blob after prune plan".to_string()
            });
        entries.push(RawMirrorPruneEntry {
            kind: "blob".to_string(),
            path: blob_relative_path,
            blob_blake3,
            manifest_blake3: None,
            size_bytes: size,
            reason,
            applied: false,
        });
    }

    report.planned_manifest_count = entries
        .iter()
        .filter(|entry| entry.kind == "manifest")
        .count() as u64;
    report.planned_blob_count = entries.iter().filter(|entry| entry.kind == "blob").count() as u64;
    report.planned_reclaim_bytes = entries
        .iter()
        .map(|entry| entry.size_bytes)
        .fold(0, u64::saturating_add);

    report.entries = entries;
    if options.apply {
        // A destructive apply is authority-bearing. Re-read every selected
        // manifest and hash every blob it depends on before removing even the
        // first file, including shared chunks that the plan retains. This
        // prevents a prune from erasing the last useful pointer after latent
        // content corruption or manifest drift.
        let mut verified_blob_paths = HashSet::new();
        for manifest_id in &selected_manifest_ids {
            let manifest = read_validated_raw_mirror_manifest(&root, manifest_id)
                .with_context(|| format!("preflight selected raw mirror manifest {manifest_id}"))?;
            for reference in raw_mirror_manifest_blob_references(&manifest)? {
                if verified_blob_paths.insert(reference.blob_relative_path.clone()) {
                    verify_existing_blob_reference(&root, &reference).with_context(|| {
                        format!("preflight content checksum for selected manifest {manifest_id}")
                    })?;
                }
            }
        }
        for entry in report.entries.iter().filter(|entry| entry.kind == "blob") {
            if !verified_blob_paths.insert(entry.path.clone()) {
                continue;
            }
            let physical = physical_blob_by_relative.get(&entry.path).ok_or_else(|| {
                anyhow!(
                    "preflight selected raw mirror blob disappeared: {}",
                    entry.path
                )
            })?;
            verify_existing_blob_reference(
                &root,
                &RawMirrorChunkRef {
                    blob_relative_path: physical.relative_path.clone(),
                    blob_blake3: physical.blob_blake3.clone(),
                    blob_size_bytes: physical.size_bytes,
                },
            )
            .with_context(|| format!("preflight selected raw mirror blob {}", entry.path))?;
        }
        for entry in &report.entries {
            let path = root.join(&entry.path);
            validate_prune_target_file(&root, &path)
                .with_context(|| format!("preflight raw mirror prune target {}", path.display()))?;
        }

        // Record and fsync the exact authority-bearing plan before removing
        // the first file. If the process crashes or a later result append
        // fails, the audit still contains a durable intent record for every
        // target that may have been touched.
        let mut audit = if report.entries.is_empty() {
            None
        } else {
            let (audit_path, mut audit_file) = open_prune_audit_log(&root)?;
            append_prune_audit_records(&mut audit_file, &audit_path, &report, "intent")?;
            report.audit_log_path = Some(audit_path.display().to_string());
            Some((audit_path, audit_file))
        };

        for entry in &mut report.entries {
            let path = root.join(&entry.path);
            let removed = remove_prune_entry_file(&root, entry)
                .with_context(|| format!("applying raw mirror prune for {}", path.display()))?;
            entry.applied = removed;
            if removed {
                if entry.kind == "manifest" {
                    report.applied_manifest_count = report.applied_manifest_count.saturating_add(1);
                } else if entry.kind == "blob" {
                    report.applied_blob_count = report.applied_blob_count.saturating_add(1);
                }
                report.applied_reclaim_bytes = report
                    .applied_reclaim_bytes
                    .saturating_add(entry.size_bytes);
            }
        }
        if let Some((audit_path, audit_file)) = audit.as_mut() {
            append_prune_audit_records(audit_file, audit_path, &report, "result")?;
        }
    } else if !report.entries.is_empty() {
        let (audit_path, mut audit_file) = open_prune_audit_log(&root)?;
        append_prune_audit_records(&mut audit_file, &audit_path, &report, "result")?;
        report.audit_log_path = Some(audit_path.display().to_string());
    }
    Ok(report)
}

fn collect_prune_manifests(root: &Path) -> Result<Vec<RawMirrorPruneManifest>> {
    let manifests_dir = root.join("manifests");
    let metadata = match fs::symlink_metadata(&manifests_dir) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("stat {}", manifests_dir.display())),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        anyhow::bail!(
            "refusing to prune invalid raw mirror manifests directory {}",
            manifests_dir.display()
        );
    }

    let mut manifests = Vec::new();
    for entry in
        fs::read_dir(&manifests_dir).with_context(|| format!("read {}", manifests_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let manifest_metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("stat raw mirror manifest {}", path.display()))?;
        if manifest_metadata.file_type().is_symlink() || !manifest_metadata.is_file() {
            anyhow::bail!(
                "refusing to prune with non-regular raw mirror manifest {}",
                path.display()
            );
        }
        let parsed_manifest = read_raw_mirror_manifest(&path)?;
        let expected_path = root.join(raw_mirror_manifest_relative_path(
            &parsed_manifest.manifest_id,
        ));
        if path != expected_path {
            anyhow::bail!(
                "refusing to prune non-canonical raw mirror manifest path {}; expected {}",
                path.display(),
                expected_path.display()
            );
        }
        let blob_references =
            validate_raw_mirror_manifest_contents(&parsed_manifest, &parsed_manifest.manifest_id)
                .with_context(|| {
                format!(
                    "refusing to prune raw mirror manifest without valid identity and checksum {}",
                    path.display()
                )
            })?;
        let manifest_blake3 = parsed_manifest.manifest_blake3.clone().ok_or_else(|| {
            anyhow!(
                "validated raw mirror manifest {} is missing its descriptor checksum",
                path.display()
            )
        })?;
        let relative_path = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .display()
            .to_string();
        manifests.push(RawMirrorPruneManifest {
            manifest_id: parsed_manifest.manifest_id,
            manifest_blake3,
            blob_blake3: parsed_manifest.blob_blake3,
            relative_path,
            size_bytes: manifest_metadata.len(),
            blob_references,
            captured_at_ms: parsed_manifest.captured_at_ms,
            provider: parsed_manifest.provider,
            original_path: parsed_manifest.original_path,
            db_links: parsed_manifest.db_links,
        });
    }
    manifests.sort_by(|left, right| {
        left.captured_at_ms
            .cmp(&right.captured_at_ms)
            .then_with(|| left.provider.cmp(&right.provider))
            .then_with(|| left.original_path.cmp(&right.original_path))
            .then_with(|| left.manifest_id.cmp(&right.manifest_id))
    });
    Ok(manifests)
}

fn pinned_prune_manifest_ids(
    data_dir: &Path,
    manifests: &[RawMirrorPruneManifest],
    keep_tags: &[String],
    safety_hold_down_ms: i64,
    now_ms: i64,
) -> Result<HashSet<String>> {
    let mut pinned = HashSet::new();
    if safety_hold_down_ms > 0 {
        let cutoff_ms = now_ms.saturating_sub(safety_hold_down_ms);
        for manifest in manifests {
            if manifest.captured_at_ms > cutoff_ms {
                pinned.insert(manifest.manifest_id.clone());
            }
        }
    }

    let normalized_keep_tags = keep_tags
        .iter()
        .map(|tag| tag.trim())
        .filter(|tag| !tag.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    if normalized_keep_tags.is_empty() {
        return Ok(pinned);
    }

    let keep_tag_conversation_ids =
        load_keep_tag_conversation_ids(data_dir, manifests, &normalized_keep_tags)?;
    for manifest in manifests {
        if manifest.db_links.iter().any(|link| {
            link.conversation_id
                .is_some_and(|id| keep_tag_conversation_ids.contains(&id))
        }) {
            pinned.insert(manifest.manifest_id.clone());
        }
    }
    Ok(pinned)
}

fn load_keep_tag_conversation_ids(
    data_dir: &Path,
    manifests: &[RawMirrorPruneManifest],
    keep_tags: &[String],
) -> Result<HashSet<i64>> {
    use crate::franken_sync::compat::{ConnectionExt as _, ParamValue, RowExt as _};

    let mut conversation_ids = manifests
        .iter()
        .flat_map(|manifest| manifest.db_links.iter())
        .filter_map(|link| link.conversation_id)
        .collect::<Vec<_>>();
    conversation_ids.sort_unstable();
    conversation_ids.dedup();
    if conversation_ids.is_empty() {
        return Ok(HashSet::new());
    }

    let db_path = data_dir.join("agent_search.db");
    let conn = crate::storage::sqlite::open_franken_raw_readonly_connection_with_timeout(
        &db_path,
        Duration::from_secs(30),
    )
    .with_context(|| {
        format!(
            "open {} to honor raw-mirror prune --keep-tag",
            db_path.display()
        )
    })?;
    let _ = conn.execute("PRAGMA query_only = 1;");

    let mut pinned = HashSet::new();
    for id_chunk in conversation_ids.chunks(400) {
        let tag_placeholders = (0..keep_tags.len())
            .map(|idx| format!("?{}", idx + 1))
            .collect::<Vec<_>>()
            .join(", ");
        let id_offset = keep_tags.len();
        let id_placeholders = (0..id_chunk.len())
            .map(|idx| format!("?{}", id_offset + idx + 1))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT DISTINCT ct.conversation_id \
             FROM conversation_tags ct \
             JOIN tags t ON t.id = ct.tag_id \
             WHERE t.name IN ({tag_placeholders}) \
               AND ct.conversation_id IN ({id_placeholders})"
        );
        let mut params = keep_tags
            .iter()
            .map(|tag| ParamValue::from(tag.as_str()))
            .collect::<Vec<_>>();
        params.extend(id_chunk.iter().copied().map(ParamValue::from));
        let rows: Vec<i64> = conn
            .query_map_collect(&sql, &params, |row: &crate::franken_sync::Row| {
                row.get_typed(0)
            })
            .with_context(|| "query raw-mirror prune keep-tag conversation pins")?;
        pinned.extend(rows);
    }

    Ok(pinned)
}

fn collect_raw_mirror_physical_blobs(root: &Path) -> Result<Vec<RawMirrorPhysicalBlob>> {
    let blobs_root = root.join("blobs").join(RAW_MIRROR_HASH_ALGORITHM);
    let metadata = match fs::symlink_metadata(&blobs_root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("stat raw mirror blob root {}", blobs_root.display()));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(anyhow!(
            "refusing to inventory invalid raw mirror blob root {}",
            blobs_root.display()
        ));
    }

    let mut stack = vec![blobs_root];
    let mut blobs = Vec::new();
    while let Some(directory) = stack.pop() {
        for entry in fs::read_dir(&directory)
            .with_context(|| format!("read raw mirror blob directory {}", directory.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).with_context(|| {
                format!("stat raw mirror blob inventory path {}", path.display())
            })?;
            if metadata.file_type().is_symlink() {
                return Err(anyhow!(
                    "refusing to inventory symlinked raw mirror blob path {}",
                    path.display()
                ));
            }
            if metadata.is_dir() {
                stack.push(path);
                continue;
            }
            if !metadata.is_file() {
                return Err(anyhow!(
                    "refusing to inventory non-file raw mirror blob path {}",
                    path.display()
                ));
            }
            if path.extension().and_then(|extension| extension.to_str())
                != Some(RAW_MIRROR_BLOB_EXTENSION)
            {
                continue;
            }

            let blob_blake3 = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .ok_or_else(|| anyhow!("raw mirror blob filename is not valid UTF-8"))?
                .to_string();
            let expected_relative = raw_mirror_blob_relative_path(&blob_blake3)
                .ok_or_else(|| anyhow!("raw mirror blob filename has an invalid digest"))?;
            let actual_relative = path.strip_prefix(root).with_context(|| {
                format!("raw mirror blob escaped inventory root {}", path.display())
            })?;
            if actual_relative != Path::new(&expected_relative) {
                return Err(anyhow!(
                    "refusing non-canonical raw mirror blob path {}; expected {}",
                    path.display(),
                    root.join(&expected_relative).display()
                ));
            }
            blobs.push(RawMirrorPhysicalBlob {
                relative_path: expected_relative,
                blob_blake3,
                size_bytes: metadata.len(),
                modified_at_ms: metadata.modified().ok().and_then(system_time_to_ms),
            });
        }
    }
    blobs.sort_by(|left, right| {
        left.modified_at_ms
            .unwrap_or(i64::MAX)
            .cmp(&right.modified_at_ms.unwrap_or(i64::MAX))
            .then_with(|| left.relative_path.cmp(&right.relative_path))
    });
    Ok(blobs)
}

fn validate_prune_target_file(root: &Path, path: &Path) -> Result<bool> {
    if raw_mirror_path_has_symlink_below_root(root, path) {
        anyhow::bail!(
            "refusing to prune raw mirror path with a symlinked ancestor {}",
            path.display()
        );
    }
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err).with_context(|| format!("stat {}", path.display())),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        anyhow::bail!(
            "refusing to prune non-regular raw mirror file {}",
            path.display()
        );
    }
    Ok(true)
}

fn remove_prune_entry_file(root: &Path, entry: &RawMirrorPruneEntry) -> Result<bool> {
    let path = root.join(&entry.path);
    if !validate_prune_target_file(root, &path)? {
        return Ok(false);
    }
    let metadata = fs::symlink_metadata(&path)
        .with_context(|| format!("restat raw mirror prune target {}", path.display()))?;
    if metadata.len() != entry.size_bytes {
        return Err(anyhow!(
            "raw mirror prune target {} changed size after preflight: observed {}, expected {}",
            path.display(),
            metadata.len(),
            entry.size_bytes
        ));
    }

    match entry.kind.as_str() {
        "blob" => {
            let expected_blake3 = entry.blob_blake3.as_deref().ok_or_else(|| {
                anyhow!(
                    "raw mirror prune blob target {} is missing its expected checksum",
                    path.display()
                )
            })?;
            verify_existing_blob_reference(
                root,
                &RawMirrorChunkRef {
                    blob_relative_path: entry.path.clone(),
                    blob_blake3: expected_blake3.to_string(),
                    blob_size_bytes: entry.size_bytes,
                },
            )
            .with_context(|| {
                format!(
                    "revalidating raw mirror blob immediately before deletion {}",
                    path.display()
                )
            })?;
        }
        "manifest" => {
            let expected_blob_blake3 = entry.blob_blake3.as_deref().ok_or_else(|| {
                anyhow!(
                    "raw mirror prune manifest target {} is missing its expected content identity",
                    path.display()
                )
            })?;
            let expected_manifest_blake3 = entry.manifest_blake3.as_deref().ok_or_else(|| {
                anyhow!(
                    "raw mirror prune manifest target {} is missing its expected descriptor checksum",
                    path.display()
                )
            })?;
            let observed_manifest = validated_existing_manifest(root, &path, expected_blob_blake3)
                .with_context(|| {
                    format!(
                        "revalidating raw mirror manifest immediately before deletion {}",
                        path.display()
                    )
                })?;
            if observed_manifest.manifest_blake3.as_deref() != Some(expected_manifest_blake3) {
                return Err(anyhow!(
                    "raw mirror prune manifest checksum changed after preflight for {}: observed {:?}, expected {}",
                    path.display(),
                    observed_manifest.manifest_blake3,
                    expected_manifest_blake3
                ));
            }
        }
        kind => {
            return Err(anyhow!(
                "raw mirror prune target {} has unsupported kind {kind}",
                path.display()
            ));
        }
    }

    fs::remove_file(&path).with_context(|| format!("remove raw mirror file {}", path.display()))?;
    sync_parent(&path)?;
    Ok(true)
}

fn open_prune_audit_log(root: &Path) -> Result<(PathBuf, File)> {
    ensure_private_dir(root)?;
    let audit_path = root.join("pruned.jsonl");
    ensure_prune_audit_log_appendable(&audit_path)?;
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    configure_lock_open_options(&mut options);
    let file = options
        .open(&audit_path)
        .with_context(|| format!("open raw mirror prune audit {}", audit_path.display()))?;
    let opened_metadata = file.metadata().with_context(|| {
        format!(
            "stat opened raw mirror prune audit {}",
            audit_path.display()
        )
    })?;
    let path_metadata = fs::symlink_metadata(&audit_path)
        .with_context(|| format!("restat raw mirror prune audit {}", audit_path.display()))?;
    if path_metadata.file_type().is_symlink()
        || !path_metadata.is_file()
        || !same_source_identity(&opened_metadata, &path_metadata)
    {
        return Err(anyhow!(
            "raw mirror prune audit {} changed identity while being opened",
            audit_path.display()
        ));
    }
    set_private_open_file_permissions(&file, &audit_path)?;
    Ok((audit_path, file))
}

fn append_prune_audit_records(
    file: &mut File,
    audit_path: &Path,
    report: &RawMirrorPruneReport,
    phase: &str,
) -> Result<()> {
    let now = now_ms();
    for entry in &report.entries {
        let record = json!({
            "schema_version": 1,
            "recorded_at_ms": now,
            "mode": report.mode,
            "phase": phase,
            "kind": entry.kind,
            "path": entry.path,
            "blob_blake3": entry.blob_blake3,
            "manifest_blake3": entry.manifest_blake3,
            "size_bytes": entry.size_bytes,
            "reason": entry.reason,
            "applied": entry.applied,
        });
        writeln!(file, "{record}")
            .with_context(|| format!("write raw mirror prune audit {}", audit_path.display()))?;
    }
    sync_open_file_if_required(file, || {
        format!("sync raw mirror prune audit {}", audit_path.display())
    })?;
    sync_parent(audit_path)?;
    Ok(())
}

fn ensure_prune_audit_log_appendable(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            anyhow::bail!(
                "refusing to append raw mirror prune audit through symlink {}",
                path.display()
            );
        }
        Ok(metadata) if !metadata.is_file() => {
            anyhow::bail!(
                "refusing to append raw mirror prune audit to non-file {}",
                path.display()
            );
        }
        Ok(_) => Ok(()),
        Err(err) if matches!(err.kind(), std::io::ErrorKind::NotFound) => Ok(()),
        Err(err) => Err(err).with_context(|| {
            format!(
                "inspect raw mirror prune audit before append {}",
                path.display()
            )
        }),
    }
}

fn acquire_raw_mirror_mutation_lock(root: &Path) -> Result<RawMirrorFileLockGuard> {
    let lock_path = root.join(RAW_MIRROR_MUTATION_LOCK_FILE);
    if raw_mirror_path_has_symlink_below_root(root, &lock_path) {
        return Err(anyhow!(
            "raw mirror mutation lock path {} contains a symlink",
            lock_path.display()
        ));
    }
    match fs::symlink_metadata(&lock_path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(anyhow!(
                "raw mirror mutation lock path {} is not a regular file",
                lock_path.display()
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("stat raw mirror mutation lock {}", lock_path.display()));
        }
    }

    // bet45: first use of a mirror may acquire the mutation lock before the
    // root has ever been created; the root was just verified symlink-free
    // (or nonexistent) above, so creating it here is safe.
    fs::create_dir_all(root).with_context(|| {
        format!(
            "create raw mirror root for mutation lock {}",
            root.display()
        )
    })?;

    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    configure_lock_open_options(&mut options);
    let file = options
        .open(&lock_path)
        .with_context(|| format!("open raw mirror mutation lock {}", lock_path.display()))?;
    fs2::FileExt::lock_exclusive(&file)
        .with_context(|| format!("acquire raw mirror mutation lock {}", lock_path.display()))?;

    let opened_metadata = file.metadata().with_context(|| {
        format!(
            "stat opened raw mirror mutation lock {}",
            lock_path.display()
        )
    })?;
    let path_metadata = fs::symlink_metadata(&lock_path)
        .with_context(|| format!("restat raw mirror mutation lock {}", lock_path.display()))?;
    if path_metadata.file_type().is_symlink()
        || !path_metadata.is_file()
        || !same_source_identity(&opened_metadata, &path_metadata)
    {
        let _ = fs2::FileExt::unlock(&file);
        return Err(anyhow!(
            "raw mirror mutation lock {} changed identity while being acquired",
            lock_path.display()
        ));
    }
    set_private_open_file_permissions(&file, &lock_path)?;

    Ok(RawMirrorFileLockGuard { file })
}

fn try_acquire_index_run_lock_for_prune(data_dir: &Path) -> Result<RawMirrorFileLockGuard> {
    let lock_path = data_dir.join("index-run.lock");
    match fs::symlink_metadata(&lock_path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(anyhow!(
                "refusing raw mirror prune because index-run lock {} is not a regular file",
                lock_path.display()
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("stat index-run lock {}", lock_path.display()));
        }
    }
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    configure_lock_open_options(&mut options);
    let file = options
        .open(&lock_path)
        .with_context(|| format!("open index-run lock {}", lock_path.display()))?;
    match fs2::FileExt::try_lock_exclusive(&file) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            return Err(anyhow!(
                "refusing raw mirror prune because an index run acquired {}: {error}",
                lock_path.display()
            ));
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("acquire index-run lock {}", lock_path.display()));
        }
    }
    let opened_metadata = file
        .metadata()
        .with_context(|| format!("stat opened index-run lock {}", lock_path.display()))?;
    let path_metadata = fs::symlink_metadata(&lock_path)
        .with_context(|| format!("restat index-run lock {}", lock_path.display()))?;
    if path_metadata.file_type().is_symlink()
        || !path_metadata.is_file()
        || !same_source_identity(&opened_metadata, &path_metadata)
    {
        let _ = fs2::FileExt::unlock(&file);
        return Err(anyhow!(
            "refusing raw mirror prune because index-run lock {} changed identity while being acquired",
            lock_path.display()
        ));
    }
    Ok(RawMirrorFileLockGuard { file })
}

fn configure_lock_open_options(options: &mut OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(not(unix))]
    let _ = options;
}

#[cfg(unix)]
fn set_private_open_file_permissions(file: &File, path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    file.set_permissions(fs::Permissions::from_mode(0o600))
        .with_context(|| format!("set raw mirror lock permissions {}", path.display()))
}

#[cfg(not(unix))]
fn set_private_open_file_permissions(_file: &File, _path: &Path) -> Result<()> {
    Ok(())
}

fn merge_min_max(min: &mut Option<i64>, max: &mut Option<i64>, value: Option<i64>) {
    let Some(value) = value else {
        return;
    };
    *min = Some(min.map_or(value, |current| current.min(value)));
    *max = Some(max.map_or(value, |current| current.max(value)));
}

fn raw_mirror_dir_file_bytes(root: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_file() {
            total = total.saturating_add(metadata.len());
        } else if metadata.is_dir() {
            let Ok(entries) = fs::read_dir(&path) else {
                continue;
            };
            for entry in entries.flatten() {
                stack.push(entry.path());
            }
        }
    }
    total
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RawMirrorBlobCacheKey {
    data_dir: PathBuf,
    source_path: PathBuf,
    source_identity: Option<String>,
    source_size_bytes: u64,
    source_mtime_ns: Option<u128>,
    source_change_time_ns: Option<u128>,
    chunk_threshold_bytes: u64,
    chunk_size_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RawMirrorBlobSourceKey {
    data_dir: PathBuf,
    source_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CachedRawMirrorBlobRecord {
    cache_key: RawMirrorBlobCacheKey,
    record: RawMirrorBlobRecord,
    stored_blob_fingerprints: Vec<RawMirrorStoredBlobFingerprint>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RawMirrorStoredBlobFingerprint {
    blob_relative_path: String,
    file_identity: String,
    size_bytes: u64,
    modified_ns: u128,
    change_time_ns: u128,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RawMirrorBlobRecord {
    blob_blake3: String,
    blob_size_bytes: u64,
    source_content_blake3: String,
    source_size_bytes: u64,
    content_storage: Option<RawMirrorContentStorage>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct RawMirrorChunkRef {
    blob_relative_path: String,
    blob_blake3: String,
    blob_size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct RawMirrorContentStorage {
    kind: String,
    content_hash_algorithm: String,
    content_blake3: String,
    content_size_bytes: u64,
    chunk_size_bytes: u64,
    chunks: Vec<RawMirrorChunkRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RawMirrorCompressionEnvelope {
    state: String,
    algorithm: Option<String>,
    uncompressed_size_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RawMirrorEncryptionEnvelope {
    state: String,
    algorithm: Option<String>,
    key_id: Option<String>,
    envelope_version: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RawMirrorVerificationRecord {
    status: String,
    verifier: String,
    content_blake3: Option<String>,
    verified_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RawMirrorManifestFile {
    schema_version: u32,
    manifest_kind: String,
    manifest_id: String,
    blob_hash_algorithm: String,
    blob_relative_path: String,
    blob_blake3: String,
    blob_size_bytes: u64,
    provider: String,
    source_id: String,
    origin_kind: String,
    origin_host: Option<String>,
    original_path: String,
    redacted_original_path: String,
    original_path_blake3: String,
    captured_at_ms: i64,
    source_mtime_ms: Option<i64>,
    source_size_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    content_storage: Option<RawMirrorContentStorage>,
    compression: RawMirrorCompressionEnvelope,
    encryption: RawMirrorEncryptionEnvelope,
    db_links: Vec<RawMirrorDbLink>,
    verification: RawMirrorVerificationRecord,
    manifest_blake3: Option<String>,
}

pub fn capture_source_file(input: RawMirrorCaptureInput<'_>) -> Result<RawMirrorCaptureRecord> {
    capture_source_file_with_chunk_policy(
        input,
        RAW_MIRROR_CHUNK_THRESHOLD_BYTES,
        RAW_MIRROR_CHUNK_SIZE_BYTES,
    )
}

pub(crate) fn capture_source_file_with_chunk_policy(
    input: RawMirrorCaptureInput<'_>,
    chunk_threshold_bytes: u64,
    chunk_size_bytes: usize,
) -> Result<RawMirrorCaptureRecord> {
    if chunk_threshold_bytes == 0 {
        return Err(anyhow!(
            "raw mirror chunk threshold must be greater than zero"
        ));
    }
    if chunk_size_bytes == 0 {
        return Err(anyhow!("raw mirror chunk size must be greater than zero"));
    }
    let source_metadata = fs::symlink_metadata(input.source_path)
        .with_context(|| format!("stat raw mirror source {}", input.source_path.display()))?;
    if source_metadata.file_type().is_symlink() {
        return Err(anyhow!(
            "refusing to raw-mirror symlink source {}",
            input.source_path.display()
        ));
    }
    if !source_metadata.is_file() {
        return Err(anyhow!(
            "refusing to raw-mirror non-file source {}",
            input.source_path.display()
        ));
    }

    let root = ensure_raw_mirror_root(input.data_dir)?;
    ensure_private_dir_descendant(&root, &root.join("tmp"))?;
    let _mutation_lock = acquire_raw_mirror_mutation_lock(&root)?;

    let cache_key = raw_mirror_blob_cache_key(
        &input,
        &source_metadata,
        chunk_threshold_bytes,
        chunk_size_bytes,
    );
    let (blob_record, pending_publish) = match cached_raw_mirror_blob_record(&cache_key, &root) {
        Some(record) => (record, None),
        None => {
            let temp_dir = unique_capture_temp_dir(&root);
            ensure_private_dir_descendant(&root, &temp_dir)?;
            let prepared = prepare_source_content(
                &root,
                input.source_path,
                &temp_dir,
                &source_metadata,
                chunk_threshold_bytes,
                chunk_size_bytes,
            )?;
            (prepared.record, Some((temp_dir, prepared.files)))
        }
    };
    let blob_blake3 = blob_record.blob_blake3.clone();
    let blob_relative_path = raw_mirror_blob_relative_path(&blob_blake3)
        .ok_or_else(|| anyhow!("computed invalid raw mirror blake3 digest"))?;

    let original_path = input.source_path.display().to_string();
    let original_path_blake3 = raw_mirror_original_path_blake3(&original_path);
    let manifest_id = raw_mirror_manifest_id(
        input.provider,
        input.source_id,
        input.origin_kind,
        input.origin_host,
        &original_path_blake3,
        &blob_blake3,
    );
    let manifest_relative_path = raw_mirror_manifest_relative_path(&manifest_id);
    let manifest_path = root.join(&manifest_relative_path);
    let captured_at_ms = now_ms();
    let source_mtime_ms = source_metadata.modified().ok().and_then(system_time_to_ms);
    let mut manifest = RawMirrorManifestFile {
        schema_version: RAW_MIRROR_SCHEMA_VERSION,
        manifest_kind: RAW_MIRROR_MANIFEST_KIND.to_string(),
        manifest_id: manifest_id.clone(),
        blob_hash_algorithm: RAW_MIRROR_HASH_ALGORITHM.to_string(),
        blob_relative_path: blob_relative_path.clone(),
        blob_blake3: blob_blake3.clone(),
        blob_size_bytes: blob_record.blob_size_bytes,
        provider: input.provider.to_string(),
        source_id: input.source_id.to_string(),
        origin_kind: input.origin_kind.to_string(),
        origin_host: input.origin_host.map(ToOwned::to_owned),
        original_path,
        redacted_original_path: redacted_original_path(input.provider, input.source_path),
        original_path_blake3,
        captured_at_ms,
        source_mtime_ms,
        source_size_bytes: blob_record.source_size_bytes,
        content_storage: blob_record.content_storage.clone(),
        compression: RawMirrorCompressionEnvelope {
            state: "none".to_string(),
            algorithm: None,
            uncompressed_size_bytes: Some(blob_record.source_size_bytes),
        },
        encryption: RawMirrorEncryptionEnvelope {
            state: "none".to_string(),
            algorithm: None,
            key_id: None,
            envelope_version: None,
        },
        db_links: unique_db_links(input.db_links),
        verification: RawMirrorVerificationRecord {
            status: "captured".to_string(),
            verifier: "cass_indexer".to_string(),
            content_blake3: Some(blob_record.source_content_blake3.clone()),
            verified_at_ms: Some(captured_at_ms),
        },
        manifest_blake3: None,
    };
    manifest.manifest_blake3 = Some(raw_mirror_manifest_blake3(&manifest));
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    if manifest_bytes.len() as u64 > RAW_MIRROR_MANIFEST_MAX_BYTES {
        if let Some((temp_dir, files)) = &pending_publish {
            for file in files {
                remove_temp_best_effort(&file.temp_path);
            }
            remove_empty_temp_dir_best_effort(temp_dir);
        }
        return Err(anyhow!(
            "raw mirror manifest for {} exceeds the {}-byte validation limit",
            input.source_path.display(),
            RAW_MIRROR_MANIFEST_MAX_BYTES
        ));
    }
    let blob_already_present = if let Some((temp_dir, files)) = pending_publish {
        let mut all_already_present = true;
        for file in &files {
            let file_blob_relative_path = raw_mirror_blob_relative_path(&file.blob_blake3)
                .ok_or_else(|| anyhow!("computed invalid raw mirror blake3 digest"))?;
            let blob_path = root.join(&file_blob_relative_path);
            all_already_present &= publish_content_addressed_temp(
                &root,
                &file.temp_path,
                &blob_path,
                &file.blob_blake3,
            )?;
        }
        remove_empty_temp_dir_best_effort(&temp_dir);
        cache_raw_mirror_blob_record(cache_key, blob_record.clone());
        all_already_present
    } else {
        true
    };
    let manifest_already_present =
        publish_manifest_bytes_create_new(&root, &manifest_path, &manifest_bytes, &blob_blake3)?;
    let (record_blob_size_bytes, record_captured_at_ms, record_source_mtime_ms) =
        if manifest_already_present {
            merge_raw_mirror_manifest_db_links(
                &root,
                &manifest_path,
                input.db_links,
                Some(&blob_blake3),
            )?;
            let published = read_raw_mirror_manifest(&manifest_path)?;
            (
                published.blob_size_bytes,
                published.captured_at_ms,
                published.source_mtime_ms,
            )
        } else {
            (blob_record.blob_size_bytes, captured_at_ms, source_mtime_ms)
        };

    let storage_kind = blob_record.content_storage.as_ref().map_or_else(
        || "whole_blob_v1".to_string(),
        |storage| storage.kind.clone(),
    );
    let chunk_count = blob_record
        .content_storage
        .as_ref()
        .map_or(1, |storage| storage.chunks.len());

    Ok(RawMirrorCaptureRecord {
        manifest_id,
        manifest_relative_path,
        blob_relative_path,
        blob_blake3: blob_blake3.clone(),
        blob_size_bytes: record_blob_size_bytes,
        source_content_blake3: blob_record.source_content_blake3,
        source_size_bytes: blob_record.source_size_bytes,
        storage_kind,
        chunk_count,
        captured_at_ms: record_captured_at_ms,
        source_mtime_ms: record_source_mtime_ms,
        already_present: blob_already_present && manifest_already_present,
    })
}

pub fn merge_manifest_db_links(
    data_dir: &Path,
    manifest_relative_path: &str,
    links: &[RawMirrorDbLink],
) -> Result<()> {
    if links.is_empty() {
        return Ok(());
    }
    let root = raw_mirror_root(data_dir);
    let _mutation_lock = acquire_raw_mirror_mutation_lock(&root)?;
    let manifest_path = raw_mirror_manifest_path_from_relative(&root, manifest_relative_path)?;
    merge_raw_mirror_manifest_db_links(&root, &manifest_path, links, None)
}

struct CopyToTempResult {
    temp_path: PathBuf,
    blob_blake3: String,
    bytes_copied: u64,
}

struct PreparedRawMirrorContent {
    record: RawMirrorBlobRecord,
    files: Vec<CopyToTempResult>,
}

fn prepare_source_content(
    root: &Path,
    source_path: &Path,
    temp_dir: &Path,
    source_metadata: &fs::Metadata,
    chunk_threshold_bytes: u64,
    chunk_size_bytes: usize,
) -> Result<PreparedRawMirrorContent> {
    if source_metadata.len() < chunk_threshold_bytes {
        let whole = copy_source_to_private_temp(source_path, temp_dir, source_metadata)?;
        return Ok(PreparedRawMirrorContent {
            record: RawMirrorBlobRecord {
                blob_blake3: whole.blob_blake3.clone(),
                blob_size_bytes: whole.bytes_copied,
                source_content_blake3: whole.blob_blake3.clone(),
                source_size_bytes: whole.bytes_copied,
                content_storage: None,
            },
            files: vec![whole],
        });
    }

    copy_source_to_private_chunks(
        root,
        source_path,
        temp_dir,
        source_metadata,
        chunk_size_bytes,
    )
}

fn copy_source_to_private_temp(
    source_path: &Path,
    temp_dir: &Path,
    source_metadata: &fs::Metadata,
) -> Result<CopyToTempResult> {
    let temp_path = unique_temp_path(temp_dir, "blob");
    let mut source = open_stable_source_file(source_path, source_metadata)?;
    let mut temp = private_create_new_file(&temp_path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut bytes_copied = 0u64;
    loop {
        let read = source
            .read(&mut buffer)
            .with_context(|| format!("read raw mirror source {}", source_path.display()))?;
        if read == 0 {
            break;
        }
        temp.write_all(&buffer[..read])
            .with_context(|| format!("write raw mirror temp {}", temp_path.display()))?;
        hasher.update(&buffer[..read]);
        bytes_copied = bytes_copied.saturating_add(read as u64);
    }
    sync_open_file_if_required(&temp, || {
        format!("sync raw mirror temp {}", temp_path.display())
    })?;

    let final_source_metadata = source
        .metadata()
        .with_context(|| format!("stat opened raw mirror source {}", source_path.display()))?;
    if source_file_changed_during_capture(source_metadata, &final_source_metadata)
        || source_path_changed_identity_during_capture(source_path, source_metadata)
    {
        remove_temp_best_effort(&temp_path);
        return Err(anyhow!(
            "raw mirror source {} changed while it was being captured; retry indexing to capture a stable copy",
            source_path.display()
        ));
    }

    Ok(CopyToTempResult {
        temp_path,
        blob_blake3: hasher.finalize().to_hex().to_string(),
        bytes_copied,
    })
}

fn copy_source_to_private_chunks(
    root: &Path,
    source_path: &Path,
    temp_dir: &Path,
    source_metadata: &fs::Metadata,
    chunk_size_bytes: usize,
) -> Result<PreparedRawMirrorContent> {
    let mut source = open_stable_source_file(source_path, source_metadata)?;
    let mut content_hasher = blake3::Hasher::new();
    let mut chunk_buffer = vec![0_u8; chunk_size_bytes];
    let mut chunk_files = Vec::new();
    let mut prepared_blob_blake3 = HashSet::new();
    let mut chunks = Vec::new();
    let mut content_size_bytes = 0_u64;

    loop {
        let mut filled = 0_usize;
        while filled < chunk_buffer.len() {
            let read = source
                .read(&mut chunk_buffer[filled..])
                .with_context(|| format!("read raw mirror source {}", source_path.display()))?;
            if read == 0 {
                break;
            }
            filled += read;
        }
        if filled == 0 {
            break;
        }

        let chunk_bytes = &chunk_buffer[..filled];
        content_hasher.update(chunk_bytes);
        content_size_bytes = content_size_bytes.saturating_add(filled as u64);
        let chunk_blake3 = blake3::hash(chunk_bytes).to_hex().to_string();
        let chunk_relative_path = raw_mirror_blob_relative_path(&chunk_blake3)
            .ok_or_else(|| anyhow!("computed invalid raw mirror chunk digest"))?;
        let chunk_reference = RawMirrorChunkRef {
            blob_relative_path: chunk_relative_path,
            blob_blake3: chunk_blake3.clone(),
            blob_size_bytes: filled as u64,
        };
        if prepared_blob_blake3.insert(chunk_blake3.clone())
            && !content_addressed_blob_is_verified(root, &chunk_reference)?
        {
            let chunk_temp_path = unique_temp_path(temp_dir, "chunk");
            let mut chunk_temp = private_create_new_file(&chunk_temp_path)?;
            chunk_temp.write_all(chunk_bytes).with_context(|| {
                format!("write raw mirror chunk temp {}", chunk_temp_path.display())
            })?;
            sync_open_file_if_required(&chunk_temp, || {
                format!("sync raw mirror chunk temp {}", chunk_temp_path.display())
            })?;
            drop(chunk_temp);
            chunk_files.push(CopyToTempResult {
                temp_path: chunk_temp_path,
                blob_blake3: chunk_blake3,
                bytes_copied: filled as u64,
            });
        }
        chunks.push(chunk_reference);

        if filled < chunk_buffer.len() {
            break;
        }
    }

    let final_source_metadata = source
        .metadata()
        .with_context(|| format!("stat opened raw mirror source {}", source_path.display()))?;
    if source_file_changed_during_capture(source_metadata, &final_source_metadata)
        || source_path_changed_identity_during_capture(source_path, source_metadata)
    {
        for chunk in &chunk_files {
            remove_temp_best_effort(&chunk.temp_path);
        }
        return Err(anyhow!(
            "raw mirror source {} changed while it was being captured; retry indexing to capture a stable copy",
            source_path.display()
        ));
    }

    let content_blake3 = content_hasher.finalize().to_hex().to_string();
    let content_storage = RawMirrorContentStorage {
        kind: RAW_MIRROR_FIXED_CHUNKS_KIND.to_string(),
        content_hash_algorithm: RAW_MIRROR_HASH_ALGORITHM.to_string(),
        content_blake3: content_blake3.clone(),
        content_size_bytes,
        chunk_size_bytes: chunk_size_bytes as u64,
        chunks,
    };
    let descriptor_bytes = serde_json::to_vec(&content_storage)?;
    let descriptor_blake3 = blake3::hash(&descriptor_bytes).to_hex().to_string();
    let descriptor_reference = RawMirrorChunkRef {
        blob_relative_path: raw_mirror_blob_relative_path(&descriptor_blake3)
            .ok_or_else(|| anyhow!("computed invalid raw mirror descriptor digest"))?,
        blob_blake3: descriptor_blake3.clone(),
        blob_size_bytes: descriptor_bytes.len() as u64,
    };
    if prepared_blob_blake3.insert(descriptor_blake3.clone())
        && !content_addressed_blob_is_verified(root, &descriptor_reference)?
    {
        let descriptor_temp_path = unique_temp_path(temp_dir, "chunk-descriptor");
        let mut descriptor_temp = private_create_new_file(&descriptor_temp_path)?;
        descriptor_temp
            .write_all(&descriptor_bytes)
            .with_context(|| {
                format!(
                    "write raw mirror chunk descriptor {}",
                    descriptor_temp_path.display()
                )
            })?;
        sync_open_file_if_required(&descriptor_temp, || {
            format!(
                "sync raw mirror chunk descriptor {}",
                descriptor_temp_path.display()
            )
        })?;
        drop(descriptor_temp);
        chunk_files.push(CopyToTempResult {
            temp_path: descriptor_temp_path,
            blob_blake3: descriptor_blake3.clone(),
            bytes_copied: descriptor_bytes.len() as u64,
        });
    }

    Ok(PreparedRawMirrorContent {
        record: RawMirrorBlobRecord {
            blob_blake3: descriptor_blake3,
            blob_size_bytes: descriptor_bytes.len() as u64,
            source_content_blake3: content_blake3,
            source_size_bytes: content_size_bytes,
            content_storage: Some(content_storage),
        },
        files: chunk_files,
    })
}

fn content_addressed_blob_is_verified(root: &Path, reference: &RawMirrorChunkRef) -> Result<bool> {
    let path = root.join(&reference.blob_relative_path);
    if raw_mirror_path_has_symlink_below_root(root, &path) {
        return Err(anyhow!(
            "raw mirror blob path {} contains a symlink",
            path.display()
        ));
    }
    match fs::symlink_metadata(&path) {
        Ok(_) => {
            verify_existing_blob_reference(root, reference)?;
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => {
            Err(error).with_context(|| format!("stat raw mirror blob {}", path.display()))
        }
    }
}

fn open_stable_source_file(source_path: &Path, expected_metadata: &fs::Metadata) -> Result<File> {
    let source = File::open(source_path)
        .with_context(|| format!("open raw mirror source {}", source_path.display()))?;
    let opened_metadata = source
        .metadata()
        .with_context(|| format!("stat opened raw mirror source {}", source_path.display()))?;
    if !same_source_identity(expected_metadata, &opened_metadata) {
        return Err(anyhow!(
            "raw mirror source {} changed identity before capture",
            source_path.display()
        ));
    }
    let current_path_metadata = fs::symlink_metadata(source_path)
        .with_context(|| format!("restat raw mirror source {}", source_path.display()))?;
    if current_path_metadata.file_type().is_symlink() {
        return Err(anyhow!(
            "refusing to raw-mirror symlink source {}",
            source_path.display()
        ));
    }
    if !same_source_identity(expected_metadata, &current_path_metadata) {
        return Err(anyhow!(
            "raw mirror source {} changed identity before capture",
            source_path.display()
        ));
    }
    Ok(source)
}

#[cfg(unix)]
fn same_source_identity(expected: &fs::Metadata, actual: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    actual.is_file() && expected.dev() == actual.dev() && expected.ino() == actual.ino()
}

#[cfg(not(unix))]
fn same_source_identity(_expected: &fs::Metadata, actual: &fs::Metadata) -> bool {
    actual.is_file()
}

#[cfg(unix)]
fn source_identity_token(metadata: &fs::Metadata) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    Some(format!("{}:{}", metadata.dev(), metadata.ino()))
}

#[cfg(not(unix))]
fn source_identity_token(_metadata: &fs::Metadata) -> Option<String> {
    None
}

#[cfg(unix)]
fn source_change_time_ns(metadata: &fs::Metadata) -> Option<u128> {
    use std::os::unix::fs::MetadataExt;

    let seconds = u128::try_from(metadata.ctime()).ok()?;
    let nanoseconds = u128::try_from(metadata.ctime_nsec()).ok()?;
    Some(
        seconds
            .saturating_mul(1_000_000_000)
            .saturating_add(nanoseconds),
    )
}

#[cfg(not(unix))]
fn source_change_time_ns(_metadata: &fs::Metadata) -> Option<u128> {
    None
}

fn source_file_changed_during_capture(
    initial: &fs::Metadata,
    final_metadata: &fs::Metadata,
) -> bool {
    if !same_source_identity(initial, final_metadata) {
        return true;
    }
    if initial.len() != final_metadata.len() {
        return true;
    }
    if let (Some(initial_change), Some(final_change)) = (
        source_change_time_ns(initial),
        source_change_time_ns(final_metadata),
    ) && initial_change != final_change
    {
        return true;
    }
    match (initial.modified().ok(), final_metadata.modified().ok()) {
        (Some(initial_mtime), Some(final_mtime)) => initial_mtime != final_mtime,
        _ => false,
    }
}

fn source_path_changed_identity_during_capture(source_path: &Path, initial: &fs::Metadata) -> bool {
    match fs::symlink_metadata(source_path) {
        Ok(current) => current.file_type().is_symlink() || !same_source_identity(initial, &current),
        Err(_) => true,
    }
}

fn publish_content_addressed_temp(
    root: &Path,
    temp_path: &Path,
    final_path: &Path,
    expected_blake3: &str,
) -> Result<bool> {
    let expected_relative_path = raw_mirror_blob_relative_path(expected_blake3)
        .ok_or_else(|| anyhow!("computed invalid raw mirror blake3 digest"))?;
    if final_path != root.join(&expected_relative_path) {
        return Err(anyhow!(
            "raw mirror blob publish target {} is not the canonical path for {}",
            final_path.display(),
            expected_blake3
        ));
    }
    let temp_size_bytes = fs::symlink_metadata(temp_path)
        .with_context(|| format!("stat raw mirror temp {}", temp_path.display()))?
        .len();
    let reference = RawMirrorChunkRef {
        blob_relative_path: expected_relative_path,
        blob_blake3: expected_blake3.to_string(),
        blob_size_bytes: temp_size_bytes,
    };
    ensure_private_dir_descendant(
        root,
        final_path
            .parent()
            .ok_or_else(|| anyhow!("raw mirror blob path has no parent"))?,
    )?;
    match fs::symlink_metadata(final_path) {
        Ok(_) => {
            verify_existing_blob_reference(root, &reference)?;
            remove_temp_best_effort(temp_path);
            return Ok(true);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("stat raw mirror blob {}", final_path.display()));
        }
    }

    match fs::hard_link(temp_path, final_path) {
        Ok(()) => {
            sync_file(final_path)?;
            sync_parent(final_path)?;
            remove_temp_best_effort(temp_path);
            Ok(false)
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            verify_existing_blob_reference(root, &reference)?;
            remove_temp_best_effort(temp_path);
            Ok(true)
        }
        Err(err) => Err(anyhow!(
            "publish raw mirror blob {} from {}: {err}",
            final_path.display(),
            temp_path.display()
        )),
    }
}

fn publish_manifest_bytes_create_new(
    root: &Path,
    manifest_path: &Path,
    manifest_bytes: &[u8],
    blob_blake3: &str,
) -> Result<bool> {
    ensure_private_dir_descendant(
        root,
        manifest_path
            .parent()
            .ok_or_else(|| anyhow!("raw mirror manifest path has no parent"))?,
    )?;
    if manifest_path.exists() {
        verify_existing_manifest(root, manifest_path, blob_blake3)?;
        return Ok(true);
    }

    let temp_dir = unique_capture_temp_dir(root);
    ensure_private_dir_descendant(root, &temp_dir)?;
    let temp_path = unique_temp_path(&temp_dir, "manifest");
    let mut temp = private_create_new_file(&temp_path)?;
    temp.write_all(manifest_bytes)
        .with_context(|| format!("write raw mirror manifest temp {}", temp_path.display()))?;
    sync_open_file_if_required(&temp, || {
        format!("sync raw mirror manifest temp {}", temp_path.display())
    })?;

    match fs::hard_link(&temp_path, manifest_path) {
        Ok(()) => {
            sync_file(manifest_path)?;
            sync_parent(manifest_path)?;
            remove_temp_best_effort(&temp_path);
            remove_empty_temp_dir_best_effort(&temp_dir);
            Ok(false)
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            verify_existing_manifest(root, manifest_path, blob_blake3)?;
            remove_temp_best_effort(&temp_path);
            remove_empty_temp_dir_best_effort(&temp_dir);
            Ok(true)
        }
        Err(err) => Err(anyhow!(
            "publish raw mirror manifest {} from {}: {err}",
            manifest_path.display(),
            temp_path.display()
        )),
    }
}

fn merge_raw_mirror_manifest_db_links(
    root: &Path,
    manifest_path: &Path,
    links: &[RawMirrorDbLink],
    expected_blob_blake3: Option<&str>,
) -> Result<()> {
    if links.is_empty() {
        return Ok(());
    }

    let lock = MANIFEST_UPDATE_LOCK.get_or_init(|| Mutex::new(()));
    let _guard = lock
        .lock()
        .map_err(|_| anyhow!("raw mirror manifest update lock poisoned"))?;

    let parsed_manifest = read_raw_mirror_manifest(manifest_path)?;
    let expected_manifest_path = root.join(raw_mirror_manifest_relative_path(
        &parsed_manifest.manifest_id,
    ));
    if manifest_path != expected_manifest_path {
        return Err(anyhow!(
            "raw mirror manifest {} is not at its canonical path {}",
            manifest_path.display(),
            expected_manifest_path.display()
        ));
    }
    let mut manifest = read_validated_raw_mirror_manifest(root, &parsed_manifest.manifest_id)?;
    if let Some(expected_blob_blake3) = expected_blob_blake3
        && manifest.blob_blake3 != expected_blob_blake3
    {
        return Err(anyhow!(
            "existing raw mirror manifest {} points at blob {}, expected {}",
            manifest_path.display(),
            manifest.blob_blake3,
            expected_blob_blake3
        ));
    }

    let mut merged_links = manifest.db_links.clone();
    merged_links.extend_from_slice(links);
    let merged_links = unique_db_links(&merged_links);
    if merged_links == manifest.db_links {
        return Ok(());
    }

    manifest.db_links = merged_links;
    manifest.manifest_blake3 = Some(raw_mirror_manifest_blake3(&manifest));
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    if manifest_bytes.len() as u64 > RAW_MIRROR_MANIFEST_MAX_BYTES {
        return Err(anyhow!(
            "refusing to replace raw mirror manifest {} with {} bytes because the validation limit is {} bytes",
            manifest_path.display(),
            manifest_bytes.len(),
            RAW_MIRROR_MANIFEST_MAX_BYTES
        ));
    }
    replace_manifest_bytes(root, manifest_path, &manifest_bytes)
}

fn replace_manifest_bytes(root: &Path, manifest_path: &Path, manifest_bytes: &[u8]) -> Result<()> {
    ensure_private_dir_descendant(
        root,
        manifest_path
            .parent()
            .ok_or_else(|| anyhow!("raw mirror manifest path has no parent"))?,
    )?;
    let temp_dir = unique_capture_temp_dir(root);
    ensure_private_dir_descendant(root, &temp_dir)?;
    let temp_path = unique_temp_path(&temp_dir, "manifest-update");
    let mut temp = private_create_new_file(&temp_path)?;
    temp.write_all(manifest_bytes).with_context(|| {
        format!(
            "write raw mirror manifest update temp {}",
            temp_path.display()
        )
    })?;
    sync_open_file_if_required(&temp, || {
        format!(
            "sync raw mirror manifest update temp {}",
            temp_path.display()
        )
    })?;
    drop(temp);

    fs::rename(&temp_path, manifest_path).with_context(|| {
        format!(
            "replace raw mirror manifest {} from {}",
            manifest_path.display(),
            temp_path.display()
        )
    })?;
    sync_parent(manifest_path)?;
    remove_empty_temp_dir_best_effort(&temp_dir);
    Ok(())
}

fn raw_mirror_manifest_path_from_relative(root: &Path, relative_path: &str) -> Result<PathBuf> {
    let relative = Path::new(relative_path);
    if relative.is_absolute() {
        return Err(anyhow!(
            "raw mirror manifest path must be relative: {relative_path}"
        ));
    }

    let mut normal_components = Vec::new();
    for component in relative.components() {
        match component {
            std::path::Component::Normal(part) => normal_components.push(part),
            _ => {
                return Err(anyhow!(
                    "raw mirror manifest path must use only normal relative components: {relative_path}"
                ));
            }
        }
    }

    if normal_components.len() != 2
        || normal_components[0] != std::ffi::OsStr::new("manifests")
        || Path::new(normal_components[1])
            .extension()
            .and_then(|ext| ext.to_str())
            != Some("json")
    {
        return Err(anyhow!(
            "raw mirror manifest path must match manifests/<id>.json: {relative_path}"
        ));
    }

    Ok(root.join(relative))
}

fn verify_existing_file(path: &Path, expected_blake3: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("stat raw mirror blob {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(anyhow!(
            "refusing to read symlink raw mirror blob {}",
            path.display()
        ));
    }
    if !metadata.is_file() {
        return Err(anyhow!(
            "refusing to read non-file raw mirror blob {}",
            path.display()
        ));
    }
    let actual = file_blake3(path)?;
    if actual == expected_blake3 {
        Ok(())
    } else {
        Err(anyhow!(
            "raw mirror blob checksum mismatch for {}: observed blake3 {}, expected {}",
            path.display(),
            actual,
            expected_blake3
        ))
    }
}

fn verify_existing_manifest(root: &Path, path: &Path, expected_blob_blake3: &str) -> Result<()> {
    validated_existing_manifest(root, path, expected_blob_blake3).map(|_| ())
}

/// Read exactly one descriptor snapshot, then bind its canonical path,
/// self-checksum, derived identity, and source-content identity together.
/// Callers authorizing deletion must not validate one read and compare fields
/// from a later read of the same path.
fn validated_existing_manifest(
    root: &Path,
    path: &Path,
    expected_blob_blake3: &str,
) -> Result<RawMirrorManifestFile> {
    let parsed_manifest = read_raw_mirror_manifest(path)?;
    let expected_manifest_path = root.join(raw_mirror_manifest_relative_path(
        &parsed_manifest.manifest_id,
    ));
    if path != expected_manifest_path {
        return Err(anyhow!(
            "existing raw mirror manifest {} is not at its canonical path {}",
            path.display(),
            expected_manifest_path.display()
        ));
    }
    validate_raw_mirror_manifest_contents(&parsed_manifest, &parsed_manifest.manifest_id)?;
    if parsed_manifest.blob_blake3 != expected_blob_blake3 {
        return Err(anyhow!(
            "existing raw mirror manifest {} points at blob {}, expected {}",
            path.display(),
            parsed_manifest.blob_blake3,
            expected_blob_blake3
        ));
    }
    Ok(parsed_manifest)
}

fn read_raw_mirror_manifest(path: &Path) -> Result<RawMirrorManifestFile> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("stat raw mirror manifest {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(anyhow!(
            "refusing to read symlink raw mirror manifest {}",
            path.display()
        ));
    }
    if !metadata.is_file() {
        return Err(anyhow!(
            "refusing to read non-file raw mirror manifest {}",
            path.display()
        ));
    }
    if metadata.len() > RAW_MIRROR_MANIFEST_MAX_BYTES {
        return Err(anyhow!(
            "refusing to read raw mirror manifest {} larger than {} bytes",
            path.display(),
            RAW_MIRROR_MANIFEST_MAX_BYTES
        ));
    }
    serde_json::from_slice(
        &fs::read(path).with_context(|| format!("read raw mirror manifest {}", path.display()))?,
    )
    .with_context(|| format!("parse raw mirror manifest {}", path.display()))
}

fn raw_mirror_root(data_dir: &Path) -> PathBuf {
    data_dir
        .join(RAW_MIRROR_ROOT_DIR)
        .join(RAW_MIRROR_VERSION_DIR)
}

fn ensure_raw_mirror_root(data_dir: &Path) -> Result<PathBuf> {
    let root_parent = data_dir.join(RAW_MIRROR_ROOT_DIR);
    ensure_private_dir(&root_parent)?;
    let root = root_parent.join(RAW_MIRROR_VERSION_DIR);
    ensure_private_dir(&root)?;
    Ok(root)
}

fn raw_mirror_blob_cache_key(
    input: &RawMirrorCaptureInput<'_>,
    source_metadata: &fs::Metadata,
    chunk_threshold_bytes: u64,
    chunk_size_bytes: usize,
) -> RawMirrorBlobCacheKey {
    RawMirrorBlobCacheKey {
        data_dir: input.data_dir.to_path_buf(),
        source_path: input.source_path.to_path_buf(),
        source_identity: source_identity_token(source_metadata),
        source_size_bytes: source_metadata.len(),
        source_mtime_ns: source_metadata.modified().ok().and_then(system_time_to_ns),
        source_change_time_ns: source_change_time_ns(source_metadata),
        chunk_threshold_bytes,
        chunk_size_bytes,
    }
}

fn cached_raw_mirror_blob_record(
    key: &RawMirrorBlobCacheKey,
    root: &Path,
) -> Option<RawMirrorBlobRecord> {
    if !raw_mirror_blob_cache_key_is_strong(key) {
        return None;
    }
    let cache = BLOB_CAPTURE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let source_key = raw_mirror_blob_source_key(key);
    let cached = {
        let mut guard = cache.lock().ok()?;
        let cached = guard.get(&source_key).cloned()?;
        if cached.cache_key != *key {
            guard.remove(&source_key);
            return None;
        }
        if raw_mirror_blob_relative_path(&cached.record.blob_blake3).is_none() {
            guard.remove(&source_key);
            return None;
        }
        cached
    };
    let record = cached.record.clone();

    if raw_mirror_stored_blob_fingerprints(root, &record)
        .is_some_and(|current| current == cached.stored_blob_fingerprints)
    {
        return Some(record);
    }

    match cached_raw_mirror_blob_record_is_verified(root, &record) {
        Ok(()) => {
            refresh_cached_raw_mirror_blob_fingerprints(cache, key, &record, root);
            Some(record)
        }
        Err(err) => {
            let blob_relative_path = raw_mirror_blob_relative_path(&record.blob_blake3)?;
            let blob_path = root.join(blob_relative_path);
            tracing::warn!(
                path = %blob_path.display(),
                expected_blake3 = %record.blob_blake3,
                error = %err,
                "discarding raw mirror blob cache entry with mismatched content"
            );
            remove_cached_raw_mirror_blob_record_if_unchanged(cache, key, &record);
            None
        }
    }
}

fn raw_mirror_stored_blob_fingerprints(
    root: &Path,
    record: &RawMirrorBlobRecord,
) -> Option<Vec<RawMirrorStoredBlobFingerprint>> {
    let mut references = vec![RawMirrorChunkRef {
        blob_relative_path: raw_mirror_blob_relative_path(&record.blob_blake3)?,
        blob_blake3: record.blob_blake3.clone(),
        blob_size_bytes: record.blob_size_bytes,
    }];
    if let Some(storage) = &record.content_storage {
        references.extend(storage.chunks.iter().cloned());
    }

    let mut seen = HashSet::new();
    let mut fingerprints = Vec::with_capacity(references.len());
    for reference in references {
        if !seen.insert(reference.blob_relative_path.clone()) {
            continue;
        }
        let path = root.join(&reference.blob_relative_path);
        if raw_mirror_path_has_symlink_below_root(root, &path) {
            return None;
        }
        let metadata = fs::symlink_metadata(&path).ok()?;
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.len() != reference.blob_size_bytes
        {
            return None;
        }
        fingerprints.push(RawMirrorStoredBlobFingerprint {
            blob_relative_path: reference.blob_relative_path,
            file_identity: source_identity_token(&metadata)?,
            size_bytes: metadata.len(),
            modified_ns: metadata.modified().ok().and_then(system_time_to_ns)?,
            change_time_ns: source_change_time_ns(&metadata)?,
        });
    }
    Some(fingerprints)
}

fn refresh_cached_raw_mirror_blob_fingerprints(
    cache: &Mutex<HashMap<RawMirrorBlobSourceKey, CachedRawMirrorBlobRecord>>,
    key: &RawMirrorBlobCacheKey,
    record: &RawMirrorBlobRecord,
    root: &Path,
) {
    let Some(stored_blob_fingerprints) = raw_mirror_stored_blob_fingerprints(root, record) else {
        return;
    };
    let source_key = raw_mirror_blob_source_key(key);
    if let Ok(mut guard) = cache.lock()
        && let Some(current) = guard.get_mut(&source_key)
        && current.cache_key == *key
        && current.record == *record
    {
        current.stored_blob_fingerprints = stored_blob_fingerprints;
    }
}

fn cached_raw_mirror_blob_record_is_verified(
    root: &Path,
    record: &RawMirrorBlobRecord,
) -> Result<()> {
    let descriptor = RawMirrorChunkRef {
        blob_relative_path: raw_mirror_blob_relative_path(&record.blob_blake3)
            .ok_or_else(|| anyhow!("cached raw mirror blob has an invalid digest"))?,
        blob_blake3: record.blob_blake3.clone(),
        blob_size_bytes: record.blob_size_bytes,
    };
    verify_existing_blob_reference(root, &descriptor)?;
    let mut verified_blobs = HashSet::from([descriptor.blob_blake3]);
    if let Some(storage) = &record.content_storage {
        for chunk in &storage.chunks {
            if verified_blobs.insert(chunk.blob_blake3.clone()) {
                verify_existing_blob_reference(root, chunk)?;
            }
        }
    }
    Ok(())
}

fn remove_cached_raw_mirror_blob_record_if_unchanged(
    cache: &Mutex<HashMap<RawMirrorBlobSourceKey, CachedRawMirrorBlobRecord>>,
    key: &RawMirrorBlobCacheKey,
    stale_record: &RawMirrorBlobRecord,
) {
    let source_key = raw_mirror_blob_source_key(key);
    if let Ok(mut guard) = cache.lock()
        && guard
            .get(&source_key)
            .is_some_and(|current| current.cache_key == *key && current.record == *stale_record)
    {
        guard.remove(&source_key);
    }
}

fn cache_raw_mirror_blob_record(key: RawMirrorBlobCacheKey, record: RawMirrorBlobRecord) {
    if !raw_mirror_blob_cache_key_is_strong(&key) {
        return;
    }
    let root = raw_mirror_root(&key.data_dir);
    let Some(stored_blob_fingerprints) = raw_mirror_stored_blob_fingerprints(&root, &record) else {
        return;
    };
    let cache = BLOB_CAPTURE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(mut guard) = cache.lock() {
        let source_key = raw_mirror_blob_source_key(&key);
        if !guard.contains_key(&source_key)
            && guard.len() >= RAW_MIRROR_BLOB_CACHE_MAX_ENTRIES
            && let Some(evicted) = guard.keys().next().cloned()
        {
            guard.remove(&evicted);
        }
        guard.insert(
            source_key,
            CachedRawMirrorBlobRecord {
                cache_key: key,
                record,
                stored_blob_fingerprints,
            },
        );
    }
}

fn raw_mirror_blob_cache_key_is_strong(key: &RawMirrorBlobCacheKey) -> bool {
    key.source_identity.is_some() && key.source_change_time_ns.is_some()
}

fn raw_mirror_blob_source_key(key: &RawMirrorBlobCacheKey) -> RawMirrorBlobSourceKey {
    RawMirrorBlobSourceKey {
        data_dir: key.data_dir.clone(),
        source_path: key.source_path.clone(),
    }
}

fn raw_mirror_blob_relative_path(blob_blake3: &str) -> Option<String> {
    if blob_blake3.len() != 64 || !blob_blake3.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let lower = blob_blake3.to_ascii_lowercase();
    Some(format!(
        "blobs/{}/{}/{}.{}",
        RAW_MIRROR_HASH_ALGORITHM,
        &lower[..2],
        lower,
        RAW_MIRROR_BLOB_EXTENSION
    ))
}

fn raw_mirror_manifest_blob_references(
    manifest: &RawMirrorManifestFile,
) -> Result<Vec<RawMirrorChunkRef>> {
    let expected_descriptor_path = raw_mirror_blob_relative_path(&manifest.blob_blake3)
        .ok_or_else(|| anyhow!("raw mirror manifest has an invalid blob digest"))?;
    if manifest.blob_relative_path != expected_descriptor_path {
        return Err(anyhow!(
            "raw mirror manifest blob path does not match its digest"
        ));
    }

    let descriptor = RawMirrorChunkRef {
        blob_relative_path: manifest.blob_relative_path.clone(),
        blob_blake3: manifest.blob_blake3.clone(),
        blob_size_bytes: manifest.blob_size_bytes,
    };
    let Some(storage) = manifest.content_storage.as_ref() else {
        if manifest.source_size_bytes != manifest.blob_size_bytes {
            return Err(anyhow!(
                "whole-blob raw mirror source size does not match blob size"
            ));
        }
        if manifest
            .verification
            .content_blake3
            .as_deref()
            .is_some_and(|digest| digest != manifest.blob_blake3)
        {
            return Err(anyhow!(
                "whole-blob raw mirror content digest does not match blob digest"
            ));
        }
        return Ok(vec![descriptor]);
    };

    if storage.kind != RAW_MIRROR_FIXED_CHUNKS_KIND
        || storage.content_hash_algorithm != RAW_MIRROR_HASH_ALGORITHM
        || storage.chunk_size_bytes == 0
        || storage.chunks.is_empty()
        || storage.content_size_bytes != manifest.source_size_bytes
        || manifest.verification.content_blake3.as_deref() != Some(storage.content_blake3.as_str())
        || raw_mirror_blob_relative_path(&storage.content_blake3).is_none()
    {
        return Err(anyhow!(
            "raw mirror fixed-chunk storage metadata is invalid"
        ));
    }
    let descriptor_bytes = serde_json::to_vec(storage)?;
    if descriptor_bytes.len() as u64 != manifest.blob_size_bytes
        || blake3::hash(&descriptor_bytes).to_hex().as_str() != manifest.blob_blake3.as_str()
    {
        return Err(anyhow!(
            "raw mirror fixed-chunk descriptor does not match the manifest blob"
        ));
    }

    let mut content_size_bytes = 0_u64;
    for (index, chunk) in storage.chunks.iter().enumerate() {
        let expected_chunk_path = raw_mirror_blob_relative_path(&chunk.blob_blake3)
            .ok_or_else(|| anyhow!("raw mirror chunk has an invalid digest"))?;
        let is_last = index + 1 == storage.chunks.len();
        if chunk.blob_relative_path != expected_chunk_path
            || chunk.blob_size_bytes == 0
            || chunk.blob_size_bytes > storage.chunk_size_bytes
            || (!is_last && chunk.blob_size_bytes != storage.chunk_size_bytes)
        {
            return Err(anyhow!("raw mirror fixed-chunk entry is invalid"));
        }
        content_size_bytes = content_size_bytes
            .checked_add(chunk.blob_size_bytes)
            .ok_or_else(|| anyhow!("raw mirror fixed-chunk sizes overflow the source size"))?;
    }
    if content_size_bytes != storage.content_size_bytes {
        return Err(anyhow!(
            "raw mirror fixed-chunk sizes do not reconstruct the source size"
        ));
    }

    let mut references = Vec::with_capacity(storage.chunks.len() + 1);
    references.push(descriptor);
    references.extend(storage.chunks.iter().cloned());
    Ok(references)
}

pub(crate) fn read_source_bytes(data_dir: &Path, manifest_id: &str) -> Result<Vec<u8>> {
    let root = raw_mirror_root(data_dir);
    let manifest = read_validated_raw_mirror_manifest(&root, manifest_id)?;
    let references = raw_mirror_manifest_blob_references(&manifest)?;

    let Some(storage) = manifest.content_storage.as_ref() else {
        validate_existing_blob_metadata(&root, &references[0])?;
        let bytes = fs::read(root.join(&manifest.blob_relative_path)).with_context(|| {
            format!(
                "read raw mirror source blob for manifest {}",
                manifest.manifest_id
            )
        })?;
        if bytes.len() as u64 != manifest.source_size_bytes
            || blake3::hash(&bytes).to_hex().as_str() != manifest.blob_blake3.as_str()
        {
            return Err(anyhow!(
                "raw mirror source blob has the wrong size or digest for manifest {}",
                manifest.manifest_id
            ));
        }
        return Ok(bytes);
    };

    validate_existing_blob_metadata(&root, &references[0])?;
    let descriptor_bytes =
        fs::read(root.join(&manifest.blob_relative_path)).with_context(|| {
            format!(
                "read raw mirror chunk descriptor for manifest {}",
                manifest.manifest_id
            )
        })?;
    if blake3::hash(&descriptor_bytes).to_hex().as_str() != manifest.blob_blake3.as_str() {
        return Err(anyhow!(
            "raw mirror chunk descriptor failed content verification for manifest {}",
            manifest.manifest_id
        ));
    }
    let descriptor: RawMirrorContentStorage = serde_json::from_slice(&descriptor_bytes)
        .with_context(|| {
            format!(
                "parse raw mirror chunk descriptor for manifest {}",
                manifest.manifest_id
            )
        })?;
    if &descriptor != storage {
        return Err(anyhow!(
            "raw mirror chunk descriptor disagrees with manifest {}",
            manifest.manifest_id
        ));
    }

    let content_capacity = usize::try_from(storage.content_size_bytes)
        .map_err(|_| anyhow!("raw mirror source is too large to reconstruct in memory"))?;
    let mut content = Vec::with_capacity(content_capacity);
    for chunk in &storage.chunks {
        let chunk_path = root.join(&chunk.blob_relative_path);
        validate_existing_blob_metadata(&root, chunk)?;
        let bytes = fs::read(&chunk_path)
            .with_context(|| format!("read raw mirror chunk {}", chunk_path.display()))?;
        if bytes.len() as u64 != chunk.blob_size_bytes
            || blake3::hash(&bytes).to_hex().as_str() != chunk.blob_blake3.as_str()
        {
            return Err(anyhow!(
                "raw mirror chunk {} has the wrong size or digest",
                chunk_path.display()
            ));
        }
        content.extend_from_slice(&bytes);
    }
    if content.len() as u64 != storage.content_size_bytes
        || blake3::hash(&content).to_hex().as_str() != storage.content_blake3.as_str()
    {
        return Err(anyhow!(
            "raw mirror chunks do not reconstruct the recorded source content"
        ));
    }
    Ok(content)
}

pub(crate) fn verify_source_capture(
    data_dir: &Path,
    manifest_id: &str,
) -> Result<RawMirrorVerifiedCapture> {
    let root = raw_mirror_root(data_dir);
    let manifest = read_validated_raw_mirror_manifest(&root, manifest_id)?;
    let references = raw_mirror_manifest_blob_references(&manifest)?;
    let descriptor_path = root.join(&references[0].blob_relative_path);
    verify_existing_blob_reference(&root, &references[0])?;

    let Some(storage) = manifest.content_storage.as_ref() else {
        return Ok(RawMirrorVerifiedCapture {
            storage_kind: "whole_blob_v1".to_string(),
            source_content_blake3: manifest.blob_blake3.clone(),
            source_size_bytes: manifest.source_size_bytes,
            stored_blob_count: 1,
            stored_bytes: manifest.blob_size_bytes,
            stored_blobs: vec![(manifest.blob_blake3, manifest.blob_size_bytes)],
            chunk_count: 1,
        });
    };

    let descriptor_bytes = fs::read(&descriptor_path).with_context(|| {
        format!(
            "read raw mirror chunk descriptor for manifest {}",
            manifest.manifest_id
        )
    })?;
    let descriptor: RawMirrorContentStorage = serde_json::from_slice(&descriptor_bytes)
        .with_context(|| {
            format!(
                "parse raw mirror chunk descriptor for manifest {}",
                manifest.manifest_id
            )
        })?;
    if &descriptor != storage {
        return Err(anyhow!(
            "raw mirror chunk descriptor disagrees with manifest {}",
            manifest.manifest_id
        ));
    }

    let mut content_hasher = blake3::Hasher::new();
    let mut content_size_bytes = 0_u64;
    let mut stored_paths = HashSet::new();
    stored_paths.insert(manifest.blob_relative_path.clone());
    let mut stored_bytes = manifest.blob_size_bytes;
    let mut stored_blobs = vec![(manifest.blob_blake3.clone(), manifest.blob_size_bytes)];
    let mut buffer = [0_u8; 64 * 1024];
    for chunk in &storage.chunks {
        let chunk_path = root.join(&chunk.blob_relative_path);
        validate_existing_blob_metadata(&root, chunk)?;
        let mut file = File::open(&chunk_path)
            .with_context(|| format!("open raw mirror chunk {}", chunk_path.display()))?;
        let mut chunk_hasher = blake3::Hasher::new();
        let mut observed_chunk_bytes = 0_u64;
        loop {
            let read = file
                .read(&mut buffer)
                .with_context(|| format!("read raw mirror chunk {}", chunk_path.display()))?;
            if read == 0 {
                break;
            }
            chunk_hasher.update(&buffer[..read]);
            content_hasher.update(&buffer[..read]);
            observed_chunk_bytes = observed_chunk_bytes.saturating_add(read as u64);
        }
        let observed_chunk_blake3 = chunk_hasher.finalize().to_hex().to_string();
        if observed_chunk_bytes != chunk.blob_size_bytes
            || observed_chunk_blake3 != chunk.blob_blake3
        {
            return Err(anyhow!(
                "raw mirror chunk {} failed content verification",
                chunk_path.display()
            ));
        }
        content_size_bytes = content_size_bytes.saturating_add(observed_chunk_bytes);
        if stored_paths.insert(chunk.blob_relative_path.clone()) {
            stored_bytes = stored_bytes.saturating_add(observed_chunk_bytes);
            stored_blobs.push((chunk.blob_blake3.clone(), observed_chunk_bytes));
        }
    }
    let observed_content_blake3 = content_hasher.finalize().to_hex().to_string();
    if content_size_bytes != storage.content_size_bytes
        || observed_content_blake3 != storage.content_blake3
    {
        return Err(anyhow!(
            "raw mirror chunks do not reconstruct the recorded source content"
        ));
    }
    Ok(RawMirrorVerifiedCapture {
        storage_kind: storage.kind.clone(),
        source_content_blake3: storage.content_blake3.clone(),
        source_size_bytes: storage.content_size_bytes,
        stored_blob_count: stored_paths.len(),
        stored_bytes,
        stored_blobs,
        chunk_count: storage.chunks.len(),
    })
}

fn read_validated_raw_mirror_manifest(
    root: &Path,
    manifest_id: &str,
) -> Result<RawMirrorManifestFile> {
    let manifest_relative_path = raw_mirror_manifest_relative_path(manifest_id);
    let manifest_path = raw_mirror_manifest_path_from_relative(root, &manifest_relative_path)?;
    if raw_mirror_path_has_symlink_below_root(root, &manifest_path) {
        return Err(anyhow!("raw mirror manifest path contains a symlink"));
    }
    let manifest = read_raw_mirror_manifest(&manifest_path)?;
    validate_raw_mirror_manifest_contents(&manifest, manifest_id)?;
    Ok(manifest)
}

fn validate_raw_mirror_manifest_contents(
    manifest: &RawMirrorManifestFile,
    expected_manifest_id: &str,
) -> Result<Vec<RawMirrorChunkRef>> {
    let expected_manifest_checksum = raw_mirror_manifest_blake3(manifest);
    let expected_original_path_blake3 = raw_mirror_original_path_blake3(&manifest.original_path);
    let derived_manifest_id = raw_mirror_manifest_id(
        &manifest.provider,
        &manifest.source_id,
        &manifest.origin_kind,
        manifest.origin_host.as_deref(),
        &manifest.original_path_blake3,
        &manifest.blob_blake3,
    );
    if manifest.manifest_id != expected_manifest_id
        || manifest.manifest_id != derived_manifest_id
        || manifest.original_path_blake3 != expected_original_path_blake3
        || manifest.manifest_kind != RAW_MIRROR_MANIFEST_KIND
        || manifest.schema_version != RAW_MIRROR_SCHEMA_VERSION
        || manifest.blob_hash_algorithm != RAW_MIRROR_HASH_ALGORITHM
        || manifest.manifest_blake3.as_deref() != Some(expected_manifest_checksum.as_str())
    {
        return Err(anyhow!(
            "raw mirror manifest identity or checksum is invalid"
        ));
    }
    raw_mirror_manifest_blob_references(manifest)
        .context("raw mirror manifest storage metadata is invalid")
}

fn verify_existing_blob_reference(root: &Path, reference: &RawMirrorChunkRef) -> Result<()> {
    validate_existing_blob_metadata(root, reference)?;
    verify_existing_file(
        &root.join(&reference.blob_relative_path),
        &reference.blob_blake3,
    )
}

fn validate_existing_blob_metadata(root: &Path, reference: &RawMirrorChunkRef) -> Result<()> {
    let expected_relative_path = raw_mirror_blob_relative_path(&reference.blob_blake3)
        .ok_or_else(|| anyhow!("raw mirror blob reference has an invalid digest"))?;
    if reference.blob_relative_path != expected_relative_path {
        return Err(anyhow!(
            "raw mirror blob reference path does not match its digest"
        ));
    }
    let path = root.join(&reference.blob_relative_path);
    if raw_mirror_path_has_symlink_below_root(root, &path) {
        return Err(anyhow!(
            "raw mirror blob path {} contains a symlink",
            path.display()
        ));
    }
    let metadata = fs::symlink_metadata(&path)
        .with_context(|| format!("stat raw mirror blob {}", path.display()))?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() != reference.blob_size_bytes
    {
        return Err(anyhow!(
            "raw mirror blob {} is missing, unsafe, or has the wrong size",
            path.display()
        ));
    }
    Ok(())
}

fn raw_mirror_path_has_symlink_below_root(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return true;
    };
    // bet45: a NONEXISTENT root is not a symlink — first-use callers (e.g.
    // acquiring the mutation lock before the mirror root has ever been
    // created) must not be refused with a misleading symlink error. This
    // mirrors the component walk below, where NotFound => false. Any other
    // metadata error stays fail-closed.
    match fs::symlink_metadata(root) {
        Ok(metadata) if metadata.file_type().is_symlink() => return true,
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return false,
        Err(_) => return true,
    }
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return true;
        };
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => return true,
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return false,
            Err(_) => return true,
        }
    }
    false
}

fn raw_mirror_manifest_relative_path(manifest_id: &str) -> String {
    format!("manifests/{manifest_id}.json")
}

fn raw_mirror_original_path_blake3(original_path: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"doctor-raw-mirror-original-path-v1");
    hasher.update(&[0]);
    hasher.update(original_path.as_bytes());
    hasher.finalize().to_hex().to_string()
}

fn raw_mirror_manifest_id(
    provider: &str,
    source_id: &str,
    origin_kind: &str,
    origin_host: Option<&str>,
    original_path_blake3: &str,
    blob_blake3: &str,
) -> String {
    canonical_blake3(
        "doctor-raw-mirror-manifest-id-v1",
        json!({
            "provider": provider,
            "source_id": source_id,
            "origin_kind": origin_kind,
            "origin_host": origin_host,
            "original_path_blake3": original_path_blake3,
            "blob_blake3": blob_blake3,
        }),
    )
}

fn raw_mirror_manifest_blake3(manifest: &RawMirrorManifestFile) -> String {
    let mut value = serde_json::to_value(manifest).unwrap_or_default();
    if let Value::Object(map) = &mut value {
        map.remove("manifest_blake3");
    }
    canonical_blake3("doctor-raw-mirror-manifest-v1", value)
}

fn canonical_blake3(prefix: &str, value: Value) -> String {
    let encoded = serde_json::to_vec(&canonical_json_value(value)).unwrap_or_default();
    let mut hasher = blake3::Hasher::new();
    hasher.update(prefix.as_bytes());
    hasher.update(&[0]);
    hasher.update(&encoded);
    format!("{prefix}-{}", hasher.finalize().to_hex())
}

fn canonical_json_value(value: Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.into_iter().map(canonical_json_value).collect()),
        Value::Object(map) => {
            let mut entries: Vec<_> = map.into_iter().collect();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            let mut canonical = serde_json::Map::new();
            for (key, value) in entries {
                canonical.insert(key, canonical_json_value(value));
            }
            Value::Object(canonical)
        }
        other => other,
    }
}

fn unique_db_links(links: &[RawMirrorDbLink]) -> Vec<RawMirrorDbLink> {
    let mut dedup = links.to_vec();
    dedup.sort_by(|left, right| {
        (
            left.conversation_id,
            left.message_count,
            left.started_at_ms,
            left.source_path.as_deref().unwrap_or(""),
        )
            .cmp(&(
                right.conversation_id,
                right.message_count,
                right.started_at_ms,
                right.source_path.as_deref().unwrap_or(""),
            ))
    });
    dedup.dedup();
    dedup
}

fn file_blake3(path: &Path) -> Result<String> {
    let expected_metadata = fs::symlink_metadata(path)
        .with_context(|| format!("stat {} before hashing", path.display()))?;
    if expected_metadata.file_type().is_symlink() || !expected_metadata.is_file() {
        return Err(anyhow!(
            "refusing to hash non-regular raw mirror file {}",
            path.display()
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    configure_lock_open_options(&mut options);
    let mut file = options
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    let opened_metadata = file
        .metadata()
        .with_context(|| format!("stat opened raw mirror file {}", path.display()))?;
    let current_path_metadata = fs::symlink_metadata(path)
        .with_context(|| format!("restat raw mirror file {}", path.display()))?;
    if current_path_metadata.file_type().is_symlink()
        || !same_source_identity(&expected_metadata, &opened_metadata)
        || !same_source_identity(&opened_metadata, &current_path_metadata)
    {
        return Err(anyhow!(
            "raw mirror file {} changed identity while being opened for hashing",
            path.display()
        ));
    }
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("read {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn ensure_private_dir(path: &Path) -> Result<()> {
    create_private_dir_all(path)
        .with_context(|| format!("create raw mirror dir {}", path.display()))?;
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("stat raw mirror dir {}", path.display()))?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        return Err(anyhow!(
            "refusing to use symlink raw mirror dir {}",
            path.display()
        ));
    }
    if !file_type.is_dir() {
        return Err(anyhow!(
            "refusing to use non-directory raw mirror path {}",
            path.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o777 != 0o700 {
            set_private_dir_permissions(path)?;
        }
    }
    #[cfg(not(unix))]
    {
        set_private_dir_permissions(path)?;
    }
    Ok(())
}

fn ensure_private_dir_descendant(root: &Path, path: &Path) -> Result<()> {
    let relative = path.strip_prefix(root).with_context(|| {
        format!(
            "raw mirror private dir {} is not under root {}",
            path.display(),
            root.display()
        )
    })?;

    if let Some(root_parent) = root.parent() {
        ensure_private_dir(root_parent)?;
    }
    ensure_private_dir(root)?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        match component {
            Component::Normal(part) => {
                current.push(part);
                ensure_private_dir(&current)?;
            }
            Component::CurDir => {}
            _ => {
                return Err(anyhow!(
                    "raw mirror private dir contains non-normal component: {}",
                    path.display()
                ));
            }
        }
    }

    Ok(())
}

fn private_create_new_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    set_private_create_file_mode(&mut options);
    let file = options
        .open(path)
        .with_context(|| format!("create raw mirror file {}", path.display()))?;
    Ok(file)
}

#[cfg(unix)]
fn create_private_dir_all(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o700).create(path)
}

#[cfg(not(unix))]
fn create_private_dir_all(path: &Path) -> std::io::Result<()> {
    fs::create_dir_all(path)
}

#[cfg(unix)]
fn set_private_create_file_mode(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;

    options.mode(0o600);
}

#[cfg(not(unix))]
fn set_private_create_file_mode(_options: &mut OpenOptions) {}

fn sync_open_file_if_required(message_file: &File, context: impl FnOnce() -> String) -> Result<()> {
    if !raw_mirror_fsync_enabled() {
        return Ok(());
    }
    message_file.sync_all().with_context(context)
}

fn sync_file(path: &Path) -> Result<()> {
    if !raw_mirror_fsync_enabled() {
        return Ok(());
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    options.write(true);
    options
        .open(path)
        .and_then(|file| file.sync_all())
        .with_context(|| format!("sync raw mirror file {}", path.display()))
}

#[cfg(not(windows))]
fn sync_parent(path: &Path) -> Result<()> {
    if !raw_mirror_fsync_enabled() {
        return Ok(());
    }
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    File::open(parent)
        .and_then(|file| file.sync_all())
        .with_context(|| format!("sync raw mirror parent {}", parent.display()))
}

#[cfg(windows)]
fn sync_parent(_path: &Path) -> Result<()> {
    Ok(())
}

fn unique_temp_path(dir: &Path, label: &str) -> PathBuf {
    let nonce = TEMP_NONCE.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    dir.join(format!(
        ".{label}.{}.{}.{}.tmp",
        std::process::id(),
        nanos,
        nonce
    ))
}

fn unique_capture_temp_dir(root: &Path) -> PathBuf {
    let nonce = TEMP_NONCE.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    root.join("tmp").join(format!(
        "capture.{}.{}.{}",
        std::process::id(),
        nanos,
        nonce
    ))
}

fn remove_temp_best_effort(path: &Path) {
    if let Err(err) = fs::remove_file(path) {
        tracing::debug!(
            path = %path.display(),
            error = %err,
            "failed to remove raw mirror temp file"
        );
    }
}

fn remove_empty_temp_dir_best_effort(path: &Path) {
    if let Err(err) = fs::remove_dir(path) {
        tracing::debug!(
            path = %path.display(),
            error = %err,
            "failed to remove raw mirror temp directory"
        );
    }
}

fn redacted_original_path(provider: &str, source_path: &Path) -> String {
    let file_name = source_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("session");
    format!("[{provider}]/{file_name}")
}

fn now_ms() -> i64 {
    system_time_to_ms(SystemTime::now()).unwrap_or(0)
}

fn system_time_to_ms(time: SystemTime) -> Option<i64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
}

fn system_time_to_ns(time: SystemTime) -> Option<u128> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_nanos())
}

#[cfg(unix)]
fn set_private_dir_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("set raw mirror dir permissions {}", path.display()))
}

#[cfg(not(unix))]
fn set_private_dir_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_source_file_writes_doctor_compatible_manifest_idempotently() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let data_dir = temp.path().join("cass-data");
        let source_path = temp.path().join("rollout-fixture.jsonl");
        let source_bytes = b"{\"type\":\"message\",\"text\":\"hello\"}\n";
        fs::write(&source_path, source_bytes).expect("write source");
        let db_link = RawMirrorDbLink {
            conversation_id: Some(42),
            message_count: Some(1),
            source_path: Some(source_path.display().to_string()),
            started_at_ms: Some(1_733_000_000_000),
        };

        let first = capture_source_file(RawMirrorCaptureInput {
            data_dir: &data_dir,
            provider: "codex",
            source_id: "local",
            origin_kind: "local",
            origin_host: None,
            source_path: &source_path,
            db_links: std::slice::from_ref(&db_link),
        })
        .expect("first capture");
        let second = capture_source_file(RawMirrorCaptureInput {
            data_dir: &data_dir,
            provider: "codex",
            source_id: "local",
            origin_kind: "local",
            origin_host: None,
            source_path: &source_path,
            db_links: std::slice::from_ref(&db_link),
        })
        .expect("second capture");

        assert_eq!(first.manifest_id, second.manifest_id);
        assert_eq!(first.blob_blake3, second.blob_blake3);
        assert_eq!(first.captured_at_ms, second.captured_at_ms);
        assert_eq!(first.source_mtime_ms, second.source_mtime_ms);
        assert!(!first.already_present);
        assert!(second.already_present);
        assert_eq!(fs::read(&source_path).expect("source bytes"), source_bytes);

        let blob_path = data_dir
            .join(RAW_MIRROR_ROOT_DIR)
            .join(RAW_MIRROR_VERSION_DIR)
            .join(&first.blob_relative_path);
        let manifest_path = data_dir
            .join(RAW_MIRROR_ROOT_DIR)
            .join(RAW_MIRROR_VERSION_DIR)
            .join(&first.manifest_relative_path);
        assert_eq!(fs::read(blob_path).expect("blob bytes"), source_bytes);

        let manifest: Value =
            serde_json::from_slice(&fs::read(&manifest_path).expect("manifest bytes"))
                .expect("manifest json");
        assert_eq!(
            manifest["manifest_kind"].as_str(),
            Some(RAW_MIRROR_MANIFEST_KIND)
        );
        assert_eq!(manifest["provider"].as_str(), Some("codex"));
        assert_eq!(
            manifest["blob_blake3"].as_str(),
            Some(first.blob_blake3.as_str())
        );
        assert_eq!(
            manifest["redacted_original_path"].as_str(),
            Some("[codex]/rollout-fixture.jsonl")
        );
        assert_eq!(
            manifest["db_links"][0]["conversation_id"].as_i64(),
            Some(42)
        );
        assert_eq!(manifest["db_links"][0]["message_count"].as_u64(), Some(1));
        assert!(
            manifest["manifest_blake3"]
                .as_str()
                .is_some_and(|value| value.starts_with("doctor-raw-mirror-manifest-v1-"))
        );
        let tmp_root = data_dir
            .join(RAW_MIRROR_ROOT_DIR)
            .join(RAW_MIRROR_VERSION_DIR)
            .join("tmp");
        assert_eq!(
            fs::read_dir(&tmp_root)
                .expect("raw mirror tmp root")
                .collect::<Vec<_>>()
                .len(),
            0,
            "successful captures must not leave doctor-visible interrupted temp artifacts"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let root = data_dir
                .join(RAW_MIRROR_ROOT_DIR)
                .join(RAW_MIRROR_VERSION_DIR);
            assert_eq!(
                fs::metadata(&root)
                    .expect("raw mirror root metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(&manifest_path)
                    .expect("manifest metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(root.join(RAW_MIRROR_MUTATION_LOCK_FILE))
                    .expect("mutation lock metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn growing_sources_reuse_fixed_chunks_and_reconstruct_every_snapshot() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let data_dir = temp.path().join("cass-data");
        let source_path = temp.path().join("growing-rollout.jsonl");
        let first_bytes = b"0123456789abcdefABCDEFGHIJKLMNOPqrstuvwxyz!@#$%^";
        assert_eq!(first_bytes.len(), 48);
        fs::write(&source_path, first_bytes).expect("write first source version");

        let first = capture_source_file_with_chunk_policy(
            RawMirrorCaptureInput {
                data_dir: &data_dir,
                provider: "codex",
                source_id: "local",
                origin_kind: "local",
                origin_host: None,
                source_path: &source_path,
                db_links: &[],
            },
            1,
            16,
        )
        .expect("capture first chunked version");

        let mut second_bytes = first_bytes.to_vec();
        second_bytes.extend_from_slice(b"tail!");
        fs::write(&source_path, &second_bytes).expect("append next source version");
        let second = capture_source_file_with_chunk_policy(
            RawMirrorCaptureInput {
                data_dir: &data_dir,
                provider: "codex",
                source_id: "local",
                origin_kind: "local",
                origin_host: None,
                source_path: &source_path,
                db_links: &[],
            },
            1,
            16,
        )
        .expect("capture appended chunked version");

        assert_eq!(first.storage_kind, RAW_MIRROR_FIXED_CHUNKS_KIND);
        assert_eq!(first.chunk_count, 3);
        assert_eq!(second.storage_kind, RAW_MIRROR_FIXED_CHUNKS_KIND);
        assert_eq!(second.chunk_count, 4);
        assert_eq!(
            read_source_bytes(&data_dir, &first.manifest_id).expect("reconstruct first version"),
            first_bytes
        );
        assert_eq!(
            read_source_bytes(&data_dir, &second.manifest_id).expect("reconstruct second version"),
            second_bytes
        );

        let root = raw_mirror_root(&data_dir);
        let first_manifest = read_raw_mirror_manifest(&root.join(&first.manifest_relative_path))
            .expect("first manifest");
        let second_manifest = read_raw_mirror_manifest(&root.join(&second.manifest_relative_path))
            .expect("second manifest");
        let first_storage = first_manifest.content_storage.expect("first chunk storage");
        let second_storage = second_manifest
            .content_storage
            .expect("second chunk storage");
        assert_eq!(
            first_storage.chunks,
            second_storage.chunks[..3],
            "an append must reuse every complete prior chunk instead of copying the full file"
        );
        assert_eq!(second_storage.chunks[3].blob_size_bytes, 5);

        let mut third_bytes = second_bytes.clone();
        third_bytes[20] ^= 0x20;
        fs::write(&source_path, &third_bytes).expect("write in-place-mutated source version");
        let third = capture_source_file_with_chunk_policy(
            RawMirrorCaptureInput {
                data_dir: &data_dir,
                provider: "cursor",
                source_id: "local",
                origin_kind: "local",
                origin_host: None,
                source_path: &source_path,
                db_links: &[],
            },
            1,
            16,
        )
        .expect("capture in-place-mutated chunked version");
        assert_eq!(
            read_source_bytes(&data_dir, &third.manifest_id).expect("reconstruct third version"),
            third_bytes
        );
        let third_manifest = read_raw_mirror_manifest(&root.join(&third.manifest_relative_path))
            .expect("third manifest");
        let third_storage = third_manifest.content_storage.expect("third chunk storage");
        assert_eq!(third_storage.chunks.len(), 4);
        assert_eq!(third_storage.chunks[0], second_storage.chunks[0]);
        assert_ne!(third_storage.chunks[1], second_storage.chunks[1]);
        assert_eq!(third_storage.chunks[2], second_storage.chunks[2]);
        assert_eq!(third_storage.chunks[3], second_storage.chunks[3]);

        let summary = storage_summary(&data_dir);
        assert_eq!(summary.manifest_count, 3);
        assert_eq!(summary.unique_blob_count, 8);
        assert_eq!(
            summary.total_blob_bytes,
            69 + first.blob_size_bytes + second.blob_size_bytes + third.blob_size_bytes,
            "physical inventory must count shared content chunks once plus one descriptor per snapshot"
        );

        let first_manifest_path = root.join(&first.manifest_relative_path);
        let mut old_first_manifest =
            read_raw_mirror_manifest(&first_manifest_path).expect("re-read first manifest");
        old_first_manifest.captured_at_ms = 0;
        old_first_manifest.manifest_blake3 = Some(raw_mirror_manifest_blake3(&old_first_manifest));
        fs::write(
            &first_manifest_path,
            serde_json::to_vec_pretty(&old_first_manifest).expect("serialize old manifest"),
        )
        .expect("age first manifest fixture");
        let prune_report = prune(
            &data_dir,
            RawMirrorPruneOptions {
                older_than_ms: Some(now_ms() / 2),
                safety_hold_down_ms: 0,
                apply: false,
                ..RawMirrorPruneOptions::default()
            },
        )
        .expect("plan chunk-aware prune");
        assert_eq!(prune_report.planned_manifest_count, 1);
        assert_eq!(prune_report.planned_blob_count, 1);
        let planned_manifest = prune_report
            .entries
            .iter()
            .find(|entry| entry.kind == "manifest" && entry.path == first.manifest_relative_path)
            .expect("retired chunked manifest prune entry");
        assert_eq!(
            planned_manifest.blob_blake3.as_deref(),
            Some(first.blob_blake3.as_str()),
            "a chunked manifest entry must pin its canonical descriptor identity instead of depending on reference ordering"
        );
        assert_eq!(
            planned_manifest.manifest_blake3.as_deref(),
            old_first_manifest.manifest_blake3.as_deref(),
            "a manifest prune plan must pin the exact descriptor checksum"
        );
        assert!(
            prune_report
                .entries
                .iter()
                .any(|entry| { entry.kind == "blob" && entry.path == first.blob_relative_path }),
            "the retired snapshot descriptor should be reclaimable"
        );
        for shared_chunk in &first_storage.chunks {
            assert!(
                prune_report.entries.iter().all(|entry| {
                    entry.kind != "blob" || entry.path != shared_chunk.blob_relative_path
                }),
                "a chunk retained by a newer snapshot must not be reclaimed: {shared_chunk:?}"
            );
        }

        let size_report = prune(
            &data_dir,
            RawMirrorPruneOptions {
                max_size_bytes: Some(
                    summary
                        .total_blob_bytes
                        .saturating_sub(first.blob_size_bytes),
                ),
                safety_hold_down_ms: 0,
                apply: false,
                ..RawMirrorPruneOptions::default()
            },
        )
        .expect("plan chunk-aware max-size prune");
        assert_eq!(
            size_report.planned_manifest_count, 1,
            "a shared base chunk must not cause max-size pruning to retire every snapshot"
        );
        assert_eq!(size_report.planned_blob_count, 1);
        assert!(size_report.entries.iter().any(|entry| {
            entry.kind == "manifest" && entry.path == first.manifest_relative_path
        }));

        let shared_chunk_path = root.join(&first_storage.chunks[0].blob_relative_path);
        fs::write(&shared_chunk_path, b"tampered-16-byte").expect("plant same-size corruption");
        assert_eq!(
            fs::metadata(&shared_chunk_path)
                .expect("chunk metadata")
                .len(),
            16
        );
        let error = verify_source_capture(&data_dir, &third.manifest_id)
            .expect_err("same-size chunk corruption must invalidate reconstruction authority");
        assert!(
            error.to_string().contains("failed content verification"),
            "unexpected chunk verification error: {error}"
        );
        let recapture_error = capture_source_file_with_chunk_policy(
            RawMirrorCaptureInput {
                data_dir: &data_dir,
                provider: "cursor",
                source_id: "local",
                origin_kind: "local",
                origin_host: None,
                source_path: &source_path,
                db_links: &[],
            },
            1,
            16,
        )
        .expect_err("a cache hit must not conceal same-size chunk corruption");
        // bet45: same-size corruption is now surfaced as an explicit blake3
        // "checksum mismatch" instead of the old "existing raw mirror blob"
        // phrasing — strictly better diagnostics; pin the semantic.
        assert!(
            recapture_error.to_string().contains("checksum mismatch"),
            "unexpected recapture error: {recapture_error}"
        );
        assert_eq!(
            fs::read(&source_path).expect("source remains untouched"),
            third_bytes
        );
    }

    #[test]
    fn growing_source_with_partial_tail_reuses_only_complete_prefix_chunks() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let data_dir = temp.path().join("cass-data");
        let source_path = temp.path().join("partial-tail.jsonl");
        let first_bytes = b"0123456789abcdefABCDEFGHIJKLMNOPqrstuvwxyz!@#$%^xyz";
        assert_eq!(first_bytes.len(), 51);
        fs::write(&source_path, first_bytes).expect("write first partial-tail version");

        let first = capture_source_file_with_chunk_policy(
            RawMirrorCaptureInput {
                data_dir: &data_dir,
                provider: "codex",
                source_id: "local",
                origin_kind: "local",
                origin_host: None,
                source_path: &source_path,
                db_links: &[],
            },
            1,
            16,
        )
        .expect("capture first partial-tail version");

        let mut second_bytes = first_bytes.to_vec();
        second_bytes.extend_from_slice(b"tail!");
        fs::write(&source_path, &second_bytes).expect("append partial tail");
        let second = capture_source_file_with_chunk_policy(
            RawMirrorCaptureInput {
                data_dir: &data_dir,
                provider: "codex",
                source_id: "local",
                origin_kind: "local",
                origin_host: None,
                source_path: &source_path,
                db_links: &[],
            },
            1,
            16,
        )
        .expect("capture extended partial-tail version");

        let root = raw_mirror_root(&data_dir);
        let first_storage = read_raw_mirror_manifest(&root.join(&first.manifest_relative_path))
            .expect("first manifest")
            .content_storage
            .expect("first chunk storage");
        let second_storage = read_raw_mirror_manifest(&root.join(&second.manifest_relative_path))
            .expect("second manifest")
            .content_storage
            .expect("second chunk storage");
        assert_eq!(first_storage.chunks.len(), 4);
        assert_eq!(first_storage.chunks[3].blob_size_bytes, 3);
        assert_eq!(second_storage.chunks.len(), 4);
        assert_eq!(second_storage.chunks[3].blob_size_bytes, 8);
        assert_eq!(first_storage.chunks[..3], second_storage.chunks[..3]);
        assert_ne!(first_storage.chunks[3], second_storage.chunks[3]);
        assert_eq!(
            read_source_bytes(&data_dir, &first.manifest_id).expect("reconstruct first snapshot"),
            first_bytes
        );
        assert_eq!(
            read_source_bytes(&data_dir, &second.manifest_id).expect("reconstruct second snapshot"),
            second_bytes
        );
        assert_eq!(
            storage_summary(&data_dir).unique_blob_count,
            7,
            "an append into a partial tail should add only one replacement tail and one descriptor"
        );
    }

    #[test]
    fn prune_apply_accepts_exact_chunked_manifest_identity() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let data_dir = temp.path().join("cass-data");
        let source_path = temp.path().join("chunked-prune.jsonl");
        fs::write(&source_path, b"0123456789abcdefABCDEFGHIJKLMNOP").expect("write chunked source");
        let captured = capture_source_file_with_chunk_policy(
            RawMirrorCaptureInput {
                data_dir: &data_dir,
                provider: "codex",
                source_id: "local",
                origin_kind: "local",
                origin_host: None,
                source_path: &source_path,
                db_links: &[],
            },
            1,
            16,
        )
        .expect("capture chunked source");
        let root = raw_mirror_root(&data_dir);
        let manifest_path = root.join(&captured.manifest_relative_path);
        let mut manifest = read_raw_mirror_manifest(&manifest_path).expect("read manifest");
        manifest.captured_at_ms = 0;
        manifest.manifest_blake3 = Some(raw_mirror_manifest_blake3(&manifest));
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).expect("serialize aged manifest"),
        )
        .expect("age manifest");

        let report = prune(
            &data_dir,
            RawMirrorPruneOptions {
                older_than_ms: Some(now_ms() / 2),
                safety_hold_down_ms: 0,
                apply: true,
                ..RawMirrorPruneOptions::default()
            },
        )
        .expect("apply chunk-aware prune");

        assert_eq!(report.applied_manifest_count, 1);
        assert!(report.applied_blob_count >= 3);
        assert!(!manifest_path.exists());
    }

    #[test]
    fn sparse_page_store_mutation_reuses_unchanged_middle_chunks() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let data_dir = temp.path().join("cass-data");
        let source_path = temp.path().join("page-store.sqlite");
        let first_bytes = b"0123456789abcdefABCDEFGHIJKLMNOPqrstuvwxyz!@#$%^&*()-_=+[]{}<>?!";
        assert_eq!(first_bytes.len(), 64);
        fs::write(&source_path, first_bytes).expect("write first page-store snapshot");

        let first = capture_source_file_with_chunk_policy(
            RawMirrorCaptureInput {
                data_dir: &data_dir,
                provider: "cursor",
                source_id: "local",
                origin_kind: "local",
                origin_host: None,
                source_path: &source_path,
                db_links: &[],
            },
            1,
            16,
        )
        .expect("capture first page-store snapshot");

        let mut second_bytes = first_bytes.to_vec();
        second_bytes[2] ^= 0x20;
        second_bytes[50] ^= 0x20;
        fs::write(&source_path, &second_bytes).expect("write sparse page-store mutation");
        let second = capture_source_file_with_chunk_policy(
            RawMirrorCaptureInput {
                data_dir: &data_dir,
                provider: "cursor",
                source_id: "local",
                origin_kind: "local",
                origin_host: None,
                source_path: &source_path,
                db_links: &[],
            },
            1,
            16,
        )
        .expect("capture sparse page-store mutation");

        let root = raw_mirror_root(&data_dir);
        let first_storage = read_raw_mirror_manifest(&root.join(&first.manifest_relative_path))
            .expect("first page-store manifest")
            .content_storage
            .expect("first page-store chunks");
        let second_storage = read_raw_mirror_manifest(&root.join(&second.manifest_relative_path))
            .expect("second page-store manifest")
            .content_storage
            .expect("second page-store chunks");
        assert_eq!(first_storage.chunks.len(), 4);
        assert_eq!(second_storage.chunks.len(), 4);
        assert_ne!(first_storage.chunks[0], second_storage.chunks[0]);
        assert_eq!(first_storage.chunks[1], second_storage.chunks[1]);
        assert_eq!(first_storage.chunks[2], second_storage.chunks[2]);
        assert_ne!(first_storage.chunks[3], second_storage.chunks[3]);
        assert_eq!(
            read_source_bytes(&data_dir, &second.manifest_id)
                .expect("reconstruct sparse page-store mutation"),
            second_bytes
        );
    }

    #[test]
    fn blob_cache_does_not_cross_chunk_policy_boundaries() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let data_dir = temp.path().join("cass-data");
        let source_path = temp.path().join("policy-sensitive.jsonl");
        let source_bytes = b"0123456789abcdef0123456789abcdef";
        fs::write(&source_path, source_bytes).expect("write source");

        let chunked = capture_source_file_with_chunk_policy(
            RawMirrorCaptureInput {
                data_dir: &data_dir,
                provider: "codex",
                source_id: "local",
                origin_kind: "local",
                origin_host: None,
                source_path: &source_path,
                db_links: &[],
            },
            1,
            16,
        )
        .expect("capture with chunking");
        let whole = capture_source_file_with_chunk_policy(
            RawMirrorCaptureInput {
                data_dir: &data_dir,
                provider: "codex",
                source_id: "local",
                origin_kind: "local",
                origin_host: None,
                source_path: &source_path,
                db_links: &[],
            },
            u64::MAX,
            16,
        )
        .expect("capture without chunking");

        assert_eq!(chunked.storage_kind, RAW_MIRROR_FIXED_CHUNKS_KIND);
        assert_eq!(whole.storage_kind, "whole_blob_v1");
        assert_ne!(chunked.manifest_id, whole.manifest_id);
        assert_eq!(
            read_source_bytes(&data_dir, &whole.manifest_id).expect("read whole snapshot"),
            source_bytes
        );
    }

    #[test]
    fn fixed_chunk_manifest_rejects_reconstructed_size_overflow() {
        let content_blake3 = "11".repeat(32);
        let first_chunk_blake3 = "22".repeat(32);
        let second_chunk_blake3 = "33".repeat(32);
        let storage = RawMirrorContentStorage {
            kind: RAW_MIRROR_FIXED_CHUNKS_KIND.to_string(),
            content_hash_algorithm: RAW_MIRROR_HASH_ALGORITHM.to_string(),
            content_blake3: content_blake3.clone(),
            content_size_bytes: u64::MAX,
            chunk_size_bytes: u64::MAX,
            chunks: vec![
                RawMirrorChunkRef {
                    blob_relative_path: raw_mirror_blob_relative_path(&first_chunk_blake3)
                        .expect("first chunk path"),
                    blob_blake3: first_chunk_blake3,
                    blob_size_bytes: u64::MAX,
                },
                RawMirrorChunkRef {
                    blob_relative_path: raw_mirror_blob_relative_path(&second_chunk_blake3)
                        .expect("second chunk path"),
                    blob_blake3: second_chunk_blake3,
                    blob_size_bytes: 1,
                },
            ],
        };
        let descriptor_bytes = serde_json::to_vec(&storage).expect("serialize descriptor");
        let descriptor_blake3 = blake3::hash(&descriptor_bytes).to_hex().to_string();
        let manifest = RawMirrorManifestFile {
            schema_version: RAW_MIRROR_SCHEMA_VERSION,
            manifest_kind: RAW_MIRROR_MANIFEST_KIND.to_string(),
            manifest_id: "overflow-fixture".to_string(),
            blob_hash_algorithm: RAW_MIRROR_HASH_ALGORITHM.to_string(),
            blob_relative_path: raw_mirror_blob_relative_path(&descriptor_blake3)
                .expect("descriptor path"),
            blob_blake3: descriptor_blake3,
            blob_size_bytes: descriptor_bytes.len() as u64,
            provider: "codex".to_string(),
            source_id: "local".to_string(),
            origin_kind: "local".to_string(),
            origin_host: None,
            original_path: "/fixture/overflow.jsonl".to_string(),
            redacted_original_path: "[codex]/overflow.jsonl".to_string(),
            original_path_blake3: "unused-by-storage-validator".to_string(),
            captured_at_ms: 0,
            source_mtime_ms: None,
            source_size_bytes: u64::MAX,
            content_storage: Some(storage),
            compression: RawMirrorCompressionEnvelope {
                state: "none".to_string(),
                algorithm: None,
                uncompressed_size_bytes: Some(u64::MAX),
            },
            encryption: RawMirrorEncryptionEnvelope {
                state: "none".to_string(),
                algorithm: None,
                key_id: None,
                envelope_version: None,
            },
            db_links: Vec::new(),
            verification: RawMirrorVerificationRecord {
                status: "captured".to_string(),
                verifier: "cass_indexer".to_string(),
                content_blake3: Some(content_blake3),
                verified_at_ms: Some(0),
            },
            manifest_blake3: None,
        };

        let error = raw_mirror_manifest_blob_references(&manifest)
            .expect_err("overflowing reconstructed size must be rejected");
        assert!(error.to_string().contains("sizes overflow"), "{error:#}");
    }

    #[test]
    fn appended_chunk_preparation_writes_only_the_new_tail_and_descriptor() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let data_dir = temp.path().join("cass-data");
        let source_path = temp.path().join("growing-rollout.jsonl");
        let first_bytes = b"0123456789abcdefABCDEFGHIJKLMNOPqrstuvwxyz!@#$%^";
        fs::write(&source_path, first_bytes).expect("write first source version");
        capture_source_file_with_chunk_policy(
            RawMirrorCaptureInput {
                data_dir: &data_dir,
                provider: "codex",
                source_id: "local",
                origin_kind: "local",
                origin_host: None,
                source_path: &source_path,
                db_links: &[],
            },
            1,
            16,
        )
        .expect("capture first chunked version");

        let mut appended_bytes = first_bytes.to_vec();
        appended_bytes.extend_from_slice(b"tail!");
        fs::write(&source_path, &appended_bytes).expect("append source version");
        let root = raw_mirror_root(&data_dir);
        let temp_dir = root.join("tmp/manual-prepare");
        ensure_private_dir_descendant(&root, &temp_dir).expect("create preparation temp dir");
        let source_metadata = fs::symlink_metadata(&source_path).expect("source metadata");

        let prepared =
            prepare_source_content(&root, &source_path, &temp_dir, &source_metadata, 1, 16)
                .expect("prepare appended source");

        assert_eq!(
            prepared.record.source_size_bytes,
            appended_bytes.len() as u64
        );
        assert_eq!(prepared.files.len(), 2);
        assert!(
            prepared.files.iter().any(|file| file.bytes_copied == 5),
            "the appended tail must be the only new source-content temp"
        );
        assert!(
            prepared.files.iter().all(|file| file.bytes_copied != 16),
            "unchanged complete chunks must be verified and reused without temp rewrites"
        );
    }

    #[test]
    fn chunk_preparation_materializes_repeated_content_once() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let data_dir = temp.path().join("cass-data");
        let source_path = temp.path().join("repeated-chunks.jsonl");
        fs::write(&source_path, b"abcdefghijklmnopabcdefghijklmnop")
            .expect("write repeated chunks");
        let root = ensure_raw_mirror_root(&data_dir).expect("create raw mirror root");
        let temp_dir = root.join("tmp/manual-repeated-prepare");
        ensure_private_dir_descendant(&root, &temp_dir).expect("create preparation temp dir");
        let source_metadata = fs::symlink_metadata(&source_path).expect("source metadata");

        let prepared =
            prepare_source_content(&root, &source_path, &temp_dir, &source_metadata, 1, 16)
                .expect("prepare repeated chunks");

        assert_eq!(prepared.record.source_size_bytes, 32);
        assert_eq!(
            prepared
                .record
                .content_storage
                .as_ref()
                .expect("chunk storage")
                .chunks
                .len(),
            2
        );
        assert_eq!(
            prepared
                .files
                .iter()
                .filter(|file| file.bytes_copied == 16)
                .count(),
            1,
            "one repeated content hash must create one pending chunk temp"
        );
        assert_eq!(prepared.files.len(), 2, "one chunk plus one descriptor");
    }

    #[test]
    fn capture_source_file_merges_db_links_into_existing_manifest() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let data_dir = temp.path().join("cass-data");
        let source_path = temp.path().join("preparse-then-parsed.jsonl");
        let source_bytes = b"{\"type\":\"message\",\"text\":\"hello\"}\n";
        fs::write(&source_path, source_bytes).expect("write source");

        let preparse = capture_source_file(RawMirrorCaptureInput {
            data_dir: &data_dir,
            provider: "codex",
            source_id: "local",
            origin_kind: "local",
            origin_host: None,
            source_path: &source_path,
            db_links: &[],
        })
        .expect("preparse capture");

        let parsed_link = RawMirrorDbLink {
            conversation_id: None,
            message_count: Some(1),
            source_path: Some(source_path.display().to_string()),
            started_at_ms: Some(1_733_000_000_000),
        };
        let parsed = capture_source_file(RawMirrorCaptureInput {
            data_dir: &data_dir,
            provider: "codex",
            source_id: "local",
            origin_kind: "local",
            origin_host: None,
            source_path: &source_path,
            db_links: std::slice::from_ref(&parsed_link),
        })
        .expect("parsed capture");

        assert_eq!(preparse.manifest_id, parsed.manifest_id);
        assert_eq!(preparse.blob_blake3, parsed.blob_blake3);
        assert!(parsed.already_present);

        let manifest_path = data_dir
            .join(RAW_MIRROR_ROOT_DIR)
            .join(RAW_MIRROR_VERSION_DIR)
            .join(&parsed.manifest_relative_path);
        let manifest = read_raw_mirror_manifest(&manifest_path).expect("merged manifest");
        assert_eq!(
            manifest.db_links,
            vec![parsed_link],
            "second capture must enrich the pre-parse manifest with DB-link evidence"
        );
        let expected_manifest_blake3 = raw_mirror_manifest_blake3(&manifest);
        assert_eq!(
            manifest.manifest_blake3.as_deref(),
            Some(expected_manifest_blake3.as_str()),
            "manifest checksum must be recomputed after DB-link merge"
        );
        assert_eq!(fs::read(&source_path).expect("source bytes"), source_bytes);
    }

    #[test]
    fn merge_manifest_db_links_rejects_hostile_relative_paths() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let data_dir = temp.path().join("cass-data");
        let db_link = RawMirrorDbLink {
            conversation_id: Some(42),
            message_count: Some(1),
            source_path: Some("source.jsonl".to_string()),
            started_at_ms: Some(1_733_000_000_000),
        };

        for relative in [
            "../escape.json",
            "/tmp/escape.json",
            "manifests/../escape.json",
            "blobs/blake3/ab/not-a-manifest.raw",
            "manifests/not-json.txt",
        ] {
            let err = merge_manifest_db_links(&data_dir, relative, std::slice::from_ref(&db_link))
                .expect_err("hostile manifest path should be rejected");
            assert!(
                err.to_string().contains("raw mirror manifest path"),
                "unexpected error for {relative}: {err}"
            );
        }
    }

    #[test]
    fn merge_manifest_db_links_refuses_to_publish_an_unreadable_oversized_manifest() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let data_dir = temp.path().join("cass-data");
        let source_path = temp.path().join("source.jsonl");
        fs::write(
            &source_path,
            b"{\"type\":\"message\",\"text\":\"bounded\"}\n",
        )
        .expect("write source");
        let captured = capture_source_file(RawMirrorCaptureInput {
            data_dir: &data_dir,
            provider: "codex",
            source_id: "local",
            origin_kind: "local",
            origin_host: None,
            source_path: &source_path,
            db_links: &[],
        })
        .expect("capture source");
        let manifest_path = raw_mirror_root(&data_dir).join(&captured.manifest_relative_path);
        let original_manifest_bytes = fs::read(&manifest_path).expect("read original manifest");
        let oversized_link = RawMirrorDbLink {
            conversation_id: Some(42),
            message_count: Some(1),
            source_path: Some("x".repeat(RAW_MIRROR_MANIFEST_MAX_BYTES as usize)),
            started_at_ms: Some(1_733_000_000_000),
        };

        let err = merge_manifest_db_links(
            &data_dir,
            &captured.manifest_relative_path,
            std::slice::from_ref(&oversized_link),
        )
        .expect_err("oversized merged manifest must be refused");

        assert!(
            format!("{err:#}").contains("validation limit is 16777216 bytes"),
            "unexpected oversized-merge error: {err:#}"
        );
        assert_eq!(
            fs::read(&manifest_path).expect("re-read manifest"),
            original_manifest_bytes,
            "a refused merge must leave the last readable manifest intact"
        );
        read_raw_mirror_manifest(&manifest_path).expect("original manifest remains readable");
    }

    #[test]
    fn recapture_and_db_link_merge_refuse_to_launder_manifest_checksum_drift() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let data_dir = temp.path().join("cass-data");
        let source_path = temp.path().join("source.jsonl");
        fs::write(
            &source_path,
            b"{\"type\":\"message\",\"text\":\"integrity\"}\n",
        )
        .expect("write source");
        let captured = capture_source_file(RawMirrorCaptureInput {
            data_dir: &data_dir,
            provider: "codex",
            source_id: "local",
            origin_kind: "local",
            origin_host: None,
            source_path: &source_path,
            db_links: &[],
        })
        .expect("capture source");
        let manifest_path = raw_mirror_root(&data_dir).join(&captured.manifest_relative_path);
        let mut manifest = read_raw_mirror_manifest(&manifest_path).expect("read manifest");
        manifest.redacted_original_path = "[tampered]/source.jsonl".to_string();
        let tampered_bytes = serde_json::to_vec_pretty(&manifest).expect("serialize drift");
        fs::write(&manifest_path, &tampered_bytes).expect("plant checksum drift");

        let summary = storage_summary(&data_dir);
        assert_eq!(summary.manifest_count, 1);
        assert_eq!(summary.invalid_manifest_count, 1);
        assert_eq!(summary.unique_blob_count, 0);
        assert_eq!(summary.total_blob_bytes, 0);

        let recapture_error = capture_source_file(RawMirrorCaptureInput {
            data_dir: &data_dir,
            provider: "codex",
            source_id: "local",
            origin_kind: "local",
            origin_host: None,
            source_path: &source_path,
            db_links: &[],
        })
        .expect_err("idempotent recapture must reject manifest checksum drift");
        assert!(
            recapture_error
                .to_string()
                .contains("identity or checksum is invalid"),
            "unexpected recapture error: {recapture_error:#}"
        );

        let merge_error = merge_manifest_db_links(
            &data_dir,
            &captured.manifest_relative_path,
            &[RawMirrorDbLink {
                conversation_id: Some(42),
                message_count: Some(1),
                source_path: Some(source_path.display().to_string()),
                started_at_ms: Some(1_733_000_000_000),
            }],
        )
        .expect_err("DB-link merge must reject manifest checksum drift");
        assert!(
            merge_error
                .to_string()
                .contains("identity or checksum is invalid"),
            "unexpected merge error: {merge_error:#}"
        );
        assert_eq!(
            fs::read(&manifest_path).expect("read refused manifest"),
            tampered_bytes,
            "refusal must not rewrite a drifted manifest with a fresh checksum"
        );
    }

    #[cfg(unix)]
    #[test]
    fn merge_manifest_db_links_rejects_symlink_manifest_path() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let data_dir = temp.path().join("cass-data");
        let manifest_dir = data_dir.join("raw-mirror/v1/manifests");
        fs::create_dir_all(&manifest_dir).expect("manifest dir");
        let outside = temp.path().join("outside.json");
        fs::write(&outside, "{}").expect("outside manifest");
        std::os::unix::fs::symlink(&outside, manifest_dir.join("link.json"))
            .expect("symlink manifest");
        let db_link = RawMirrorDbLink {
            conversation_id: Some(42),
            message_count: Some(1),
            source_path: Some("source.jsonl".to_string()),
            started_at_ms: Some(1_733_000_000_000),
        };

        let err = merge_manifest_db_links(
            &data_dir,
            "manifests/link.json",
            std::slice::from_ref(&db_link),
        )
        .expect_err("symlink manifest should be rejected");
        assert!(
            err.to_string().contains("symlink raw mirror manifest"),
            "unexpected symlink-manifest error: {err}"
        );
    }

    #[test]
    fn capture_source_file_deduplicates_blob_for_distinct_source_paths() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let data_dir = temp.path().join("cass-data");
        let first_source = temp.path().join("first.jsonl");
        let second_source = temp.path().join("second.jsonl");
        let source_bytes = b"{\"type\":\"message\",\"text\":\"shared\"}\n";
        fs::write(&first_source, source_bytes).expect("write first source");
        fs::write(&second_source, source_bytes).expect("write second source");

        let first = capture_source_file(RawMirrorCaptureInput {
            data_dir: &data_dir,
            provider: "codex",
            source_id: "local",
            origin_kind: "local",
            origin_host: None,
            source_path: &first_source,
            db_links: &[],
        })
        .expect("first capture");
        let second = capture_source_file(RawMirrorCaptureInput {
            data_dir: &data_dir,
            provider: "codex",
            source_id: "local",
            origin_kind: "local",
            origin_host: None,
            source_path: &second_source,
            db_links: &[],
        })
        .expect("second capture");

        assert_eq!(first.blob_blake3, second.blob_blake3);
        assert_eq!(first.blob_relative_path, second.blob_relative_path);
        assert_ne!(first.manifest_id, second.manifest_id);
        assert!(
            !second.already_present,
            "a duplicate blob with a new source manifest is not a full capture replay"
        );

        let manifest_root = data_dir
            .join(RAW_MIRROR_ROOT_DIR)
            .join(RAW_MIRROR_VERSION_DIR)
            .join("manifests");
        let manifests = fs::read_dir(manifest_root)
            .expect("manifest dir")
            .collect::<std::io::Result<Vec<_>>>()
            .expect("manifest entries");
        assert_eq!(manifests.len(), 2);

        let summary = storage_summary(&data_dir);
        assert!(summary.initialized);
        assert_eq!(summary.manifest_count, 2);
        assert_eq!(summary.unique_blob_count, 1);
        assert_eq!(summary.total_blob_bytes, source_bytes.len() as u64);
        assert_eq!(summary.largest_blob_bytes, source_bytes.len() as u64);
        assert_eq!(summary.missing_blob_count, 0);
        assert_eq!(summary.invalid_manifest_count, 0);
        assert!(summary.total_storage_bytes >= source_bytes.len() as u64);
    }

    #[cfg(unix)]
    #[test]
    fn storage_summary_rejects_blob_below_symlinked_directory() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let data_dir = temp.path().join("cass-data");
        let source_path = temp.path().join("source.jsonl");
        let source_bytes = b"{\"type\":\"message\",\"text\":\"outside\"}\n";
        fs::write(&source_path, source_bytes).expect("source bytes");
        let captured = capture_source_file(RawMirrorCaptureInput {
            data_dir: &data_dir,
            provider: "codex",
            source_id: "local",
            origin_kind: "local",
            origin_host: None,
            source_path: &source_path,
            db_links: &[],
        })
        .expect("capture source");
        let root = raw_mirror_root(&data_dir);
        let blob_path = root.join(&captured.blob_relative_path);
        let blob_parent = blob_path.parent().expect("blob parent");
        let relocated_parent = temp.path().join("relocated-blob-prefix");
        fs::rename(blob_parent, &relocated_parent).expect("relocate blob prefix");
        std::os::unix::fs::symlink(&relocated_parent, blob_parent).expect("symlink blob prefix");

        let summary = storage_summary(&data_dir);
        assert_eq!(summary.manifest_count, 1);
        assert_eq!(summary.unique_blob_count, 0);
        assert_eq!(summary.total_blob_bytes, 0);
        assert_eq!(summary.missing_blob_count, 1);
        assert_eq!(
            fs::read(&blob_path).expect("outside blob bytes"),
            source_bytes
        );
    }

    #[test]
    fn storage_summary_rejects_hostile_blob_relative_path() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let data_dir = temp.path().join("cass-data");
        let source_path = temp.path().join("source.jsonl");
        fs::write(
            &source_path,
            b"{\"type\":\"message\",\"text\":\"hostile\"}\n",
        )
        .expect("write source");

        let captured = capture_source_file(RawMirrorCaptureInput {
            data_dir: &data_dir,
            provider: "codex",
            source_id: "local",
            origin_kind: "local",
            origin_host: None,
            source_path: &source_path,
            db_links: &[],
        })
        .expect("capture source");
        let manifest_path = data_dir
            .join(RAW_MIRROR_ROOT_DIR)
            .join(RAW_MIRROR_VERSION_DIR)
            .join(&captured.manifest_relative_path);
        let mut manifest = read_raw_mirror_manifest(&manifest_path).expect("read manifest");
        manifest.blob_relative_path = "../outside.raw".to_string();
        manifest.manifest_blake3 = Some(raw_mirror_manifest_blake3(&manifest));
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).expect("serialize manifest"),
        )
        .expect("tamper manifest");

        let summary = storage_summary(&data_dir);
        assert_eq!(summary.manifest_count, 1);
        assert_eq!(summary.invalid_manifest_count, 1);
        assert_eq!(summary.unique_blob_count, 0);
        assert_eq!(summary.total_blob_bytes, 0);

        let recapture_error = capture_source_file(RawMirrorCaptureInput {
            data_dir: &data_dir,
            provider: "codex",
            source_id: "local",
            origin_kind: "local",
            origin_host: None,
            source_path: &source_path,
            db_links: &[],
        })
        .expect_err("recapture must reject checksum-valid but structurally invalid metadata");
        assert!(
            format!("{recapture_error:#}").contains("storage metadata is invalid"),
            "unexpected structurally-invalid recapture error: {recapture_error:#}"
        );
    }

    #[test]
    fn oversized_manifest_is_rejected_before_json_allocation_or_prune_planning() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let data_dir = temp.path().join("cass-data");
        let source_path = temp.path().join("source.jsonl");
        fs::write(
            &source_path,
            b"{\"type\":\"message\",\"text\":\"bounded\"}\n",
        )
        .expect("write source");
        let captured = capture_source_file(RawMirrorCaptureInput {
            data_dir: &data_dir,
            provider: "codex",
            source_id: "local",
            origin_kind: "local",
            origin_host: None,
            source_path: &source_path,
            db_links: &[],
        })
        .expect("capture source");
        let root = raw_mirror_root(&data_dir);
        let manifest_path = root.join(&captured.manifest_relative_path);
        OpenOptions::new()
            .write(true)
            .open(&manifest_path)
            .expect("open manifest fixture")
            .set_len(RAW_MIRROR_MANIFEST_MAX_BYTES + 1)
            .expect("plant oversized sparse manifest");

        let summary = storage_summary(&data_dir);
        assert_eq!(summary.manifest_count, 0);
        assert_eq!(summary.invalid_manifest_count, 1);
        let err = prune(
            &data_dir,
            RawMirrorPruneOptions {
                older_than_ms: Some(0),
                apply: false,
                ..RawMirrorPruneOptions::default()
            },
        )
        .expect_err("oversized manifest must not enter prune planning");
        assert!(
            format!("{err:#}").contains("larger than 16777216 bytes"),
            "unexpected oversized-manifest error: {err:#}"
        );
        assert!(manifest_path.exists());
        assert!(root.join(&captured.blob_relative_path).exists());
    }

    #[test]
    fn prune_fails_closed_on_hostile_manifest_inventory() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let data_dir = temp.path().join("cass-data");
        let source_path = temp.path().join("source.jsonl");
        fs::write(
            &source_path,
            b"{\"type\":\"message\",\"text\":\"hostile\"}\n",
        )
        .expect("write source");

        let captured = capture_source_file(RawMirrorCaptureInput {
            data_dir: &data_dir,
            provider: "codex",
            source_id: "local",
            origin_kind: "local",
            origin_host: None,
            source_path: &source_path,
            db_links: &[],
        })
        .expect("capture source");
        let root = data_dir
            .join(RAW_MIRROR_ROOT_DIR)
            .join(RAW_MIRROR_VERSION_DIR);
        let manifest_path = root.join(&captured.manifest_relative_path);
        let blob_path = root.join(&captured.blob_relative_path);
        let mut manifest = read_raw_mirror_manifest(&manifest_path).expect("read manifest");
        manifest.blob_relative_path = "../outside.raw".to_string();
        manifest.manifest_blake3 = Some(raw_mirror_manifest_blake3(&manifest));
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).expect("serialize manifest"),
        )
        .expect("tamper manifest");

        let err = prune(
            &data_dir,
            RawMirrorPruneOptions {
                older_than_ms: Some(0),
                max_size_bytes: None,
                keep_tags: Vec::new(),
                safety_hold_down_ms: 0,
                apply: true,
            },
        )
        .expect_err("hostile inventory should fail closed");

        let detail = format!("{err:#}");
        assert!(
            detail.contains("blob path does not match its digest"),
            "error should explain the unsafe manifest inventory: {detail}"
        );
        assert!(manifest_path.exists());
        assert!(blob_path.exists());
        assert!(!root.join("pruned.jsonl").exists());
    }

    #[test]
    fn prune_fails_closed_on_manifest_checksum_drift_before_removing_evidence() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let data_dir = temp.path().join("cass-data");
        let source_path = temp.path().join("source.jsonl");
        fs::write(&source_path, b"{\"type\":\"message\",\"text\":\"old\"}\n")
            .expect("write source");
        let captured = capture_source_file(RawMirrorCaptureInput {
            data_dir: &data_dir,
            provider: "codex",
            source_id: "local",
            origin_kind: "local",
            origin_host: None,
            source_path: &source_path,
            db_links: &[],
        })
        .expect("capture source");
        let root = raw_mirror_root(&data_dir);
        let manifest_path = root.join(&captured.manifest_relative_path);
        let blob_path = root.join(&captured.blob_relative_path);
        let mut manifest = read_raw_mirror_manifest(&manifest_path).expect("read manifest");
        manifest.captured_at_ms = 0;
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).expect("serialize drifted manifest"),
        )
        .expect("plant manifest checksum drift");

        let err = prune(
            &data_dir,
            RawMirrorPruneOptions {
                older_than_ms: Some(0),
                safety_hold_down_ms: 0,
                apply: true,
                ..RawMirrorPruneOptions::default()
            },
        )
        .expect_err("manifest checksum drift must not authorize pruning");
        let detail = format!("{err:#}");
        assert!(
            detail.contains("valid identity and checksum"),
            "unexpected checksum-drift error: {detail}"
        );
        assert!(manifest_path.exists());
        assert!(blob_path.exists());
        assert!(!root.join("pruned.jsonl").exists());
    }

    #[test]
    fn prune_inventories_and_reclaims_crash_orphaned_content_blobs() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let data_dir = temp.path().join("cass-data");
        let root = ensure_raw_mirror_root(&data_dir).expect("raw mirror root");
        let orphan_bytes = b"published chunk whose manifest never committed";
        let orphan_blake3 = blake3::hash(orphan_bytes).to_hex().to_string();
        let orphan_relative =
            raw_mirror_blob_relative_path(&orphan_blake3).expect("orphan blob path");
        let orphan_path = root.join(&orphan_relative);
        ensure_private_dir_descendant(&root, orphan_path.parent().expect("orphan parent"))
            .expect("orphan parent directory");
        fs::write(&orphan_path, orphan_bytes).expect("plant crash-orphaned blob");

        let summary = storage_summary(&data_dir);
        assert_eq!(summary.unique_blob_count, 0);
        assert_eq!(summary.total_blob_bytes, 0);
        assert_eq!(summary.orphan_blob_count, 1);
        assert_eq!(summary.orphan_blob_bytes, orphan_bytes.len() as u64);

        let dry_run = prune(
            &data_dir,
            RawMirrorPruneOptions {
                older_than_ms: Some(0),
                safety_hold_down_ms: 0,
                apply: false,
                ..RawMirrorPruneOptions::default()
            },
        )
        .expect("plan orphan reclaim");
        assert_eq!(dry_run.manifest_count, 0);
        assert_eq!(dry_run.unique_blob_count, 1);
        assert_eq!(dry_run.current_blob_bytes, orphan_bytes.len() as u64);
        assert_eq!(dry_run.orphan_blob_count, 1);
        assert_eq!(dry_run.orphan_blob_bytes, orphan_bytes.len() as u64);
        assert_eq!(dry_run.planned_manifest_count, 0);
        assert_eq!(dry_run.planned_blob_count, 1);
        assert!(dry_run.entries.iter().any(|entry| {
            entry.kind == "blob"
                && entry.path == orphan_relative
                && entry.reason.contains("unreferenced blob")
        }));
        assert!(orphan_path.exists(), "dry-run must retain orphan evidence");

        let applied = prune(
            &data_dir,
            RawMirrorPruneOptions {
                older_than_ms: Some(0),
                safety_hold_down_ms: 0,
                apply: true,
                ..RawMirrorPruneOptions::default()
            },
        )
        .expect("apply orphan reclaim");
        assert_eq!(applied.applied_manifest_count, 0);
        assert_eq!(applied.applied_blob_count, 1);
        assert_eq!(applied.applied_reclaim_bytes, orphan_bytes.len() as u64);
        assert!(!orphan_path.exists());
    }

    #[test]
    fn prune_hold_down_keeps_recent_crash_orphaned_blob() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let data_dir = temp.path().join("cass-data");
        let root = ensure_raw_mirror_root(&data_dir).expect("raw mirror root");
        let orphan_bytes = b"recent crash orphan";
        let orphan_blake3 = blake3::hash(orphan_bytes).to_hex().to_string();
        let orphan_relative =
            raw_mirror_blob_relative_path(&orphan_blake3).expect("orphan blob path");
        let orphan_path = root.join(&orphan_relative);
        ensure_private_dir_descendant(&root, orphan_path.parent().expect("orphan parent"))
            .expect("orphan parent directory");
        fs::write(&orphan_path, orphan_bytes).expect("plant recent orphan");

        let report = prune(
            &data_dir,
            RawMirrorPruneOptions {
                max_size_bytes: Some(0),
                safety_hold_down_ms: 7 * 24 * 60 * 60 * 1_000,
                apply: false,
                ..RawMirrorPruneOptions::default()
            },
        )
        .expect("plan held-down orphan");
        assert_eq!(report.orphan_blob_count, 1);
        assert_eq!(report.pinned_blob_count, 1);
        assert_eq!(report.planned_blob_count, 0);
        assert!(orphan_path.exists());
    }

    #[test]
    fn prune_apply_hashes_orphan_before_deleting_any_evidence() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let data_dir = temp.path().join("cass-data");
        let root = ensure_raw_mirror_root(&data_dir).expect("raw mirror root");
        let expected_bytes = b"expected orphan bytes";
        let orphan_blake3 = blake3::hash(expected_bytes).to_hex().to_string();
        let orphan_relative =
            raw_mirror_blob_relative_path(&orphan_blake3).expect("orphan blob path");
        let orphan_path = root.join(&orphan_relative);
        ensure_private_dir_descendant(&root, orphan_path.parent().expect("orphan parent"))
            .expect("orphan parent directory");
        fs::write(&orphan_path, b"corrupted orphan byte").expect("plant corrupt orphan");
        assert_eq!(
            fs::metadata(&orphan_path).expect("orphan metadata").len(),
            expected_bytes.len() as u64,
            "fixture must preserve size so only the checksum catches corruption"
        );

        let error = prune(
            &data_dir,
            RawMirrorPruneOptions {
                older_than_ms: Some(0),
                safety_hold_down_ms: 0,
                apply: true,
                ..RawMirrorPruneOptions::default()
            },
        )
        .expect_err("checksum-drifted orphan must not be deleted");
        assert!(
            format!("{error:#}").contains("checksum mismatch"),
            "unexpected corrupt-orphan error: {error:#}"
        );
        assert!(orphan_path.exists());
        assert!(!root.join("pruned.jsonl").exists());
    }

    #[test]
    fn prune_delete_revalidates_exact_blob_after_plan_preflight() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let data_dir = temp.path().join("cass-data");
        let root = ensure_raw_mirror_root(&data_dir).expect("raw mirror root");
        let expected_bytes = b"preflighted raw mirror evidence";
        let expected_blake3 = blake3::hash(expected_bytes).to_hex().to_string();
        let relative_path = raw_mirror_blob_relative_path(&expected_blake3).expect("blob path");
        let blob_path = root.join(&relative_path);
        ensure_private_dir_descendant(&root, blob_path.parent().expect("blob parent directory"))
            .expect("create blob parent directory");
        fs::write(&blob_path, expected_bytes).expect("plant preflighted blob");
        let entry = RawMirrorPruneEntry {
            kind: "blob".to_string(),
            path: relative_path,
            blob_blake3: Some(expected_blake3),
            manifest_blake3: None,
            size_bytes: expected_bytes.len() as u64,
            reason: "test target".to_string(),
            applied: false,
        };

        let mut replacement = expected_bytes.to_vec();
        replacement[0] ^= 1;
        fs::write(&blob_path, replacement).expect("replace blob after plan preflight");

        let error = remove_prune_entry_file(&root, &entry)
            .expect_err("same-size replacement must not be deleted");
        assert!(
            format!("{error:#}").contains("checksum mismatch"),
            "unexpected changed-target error: {error:#}"
        );
        assert!(
            blob_path.exists(),
            "a target whose content changed after preflight must remain on disk"
        );
    }

    #[test]
    fn prune_delete_revalidates_exact_manifest_after_plan_preflight() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let data_dir = temp.path().join("cass-data");
        let source_path = temp.path().join("session.jsonl");
        fs::write(&source_path, b"{\"type\":\"message\",\"text\":\"safe\"}\n")
            .expect("write source");
        let captured = capture_source_file(RawMirrorCaptureInput {
            data_dir: &data_dir,
            provider: "codex",
            source_id: "local",
            origin_kind: "local",
            origin_host: None,
            source_path: &source_path,
            db_links: &[],
        })
        .expect("capture source");
        let root = raw_mirror_root(&data_dir);
        let manifest_path = root.join(&captured.manifest_relative_path);
        let mut manifest = read_raw_mirror_manifest(&manifest_path).expect("read manifest");
        let expected_manifest_blake3 = manifest
            .manifest_blake3
            .clone()
            .expect("captured manifest checksum");
        let original_size = fs::metadata(&manifest_path)
            .expect("manifest metadata")
            .len();
        let entry = RawMirrorPruneEntry {
            kind: "manifest".to_string(),
            path: captured.manifest_relative_path,
            blob_blake3: Some(captured.blob_blake3),
            manifest_blake3: Some(expected_manifest_blake3),
            size_bytes: original_size,
            reason: "test target".to_string(),
            applied: false,
        };

        manifest.captured_at_ms = manifest.captured_at_ms.saturating_add(1);
        manifest.manifest_blake3 = Some(raw_mirror_manifest_blake3(&manifest));
        let replacement = serde_json::to_vec_pretty(&manifest).expect("serialize replacement");
        assert_eq!(
            replacement.len() as u64,
            original_size,
            "fixture must preserve manifest size so the exact checksum is causal"
        );
        fs::write(&manifest_path, replacement).expect("replace manifest after preflight");

        let error = remove_prune_entry_file(&root, &entry)
            .expect_err("same-size valid replacement manifest must not be deleted");
        assert!(
            format!("{error:#}").contains("manifest checksum changed after preflight"),
            "unexpected changed-manifest error: {error:#}"
        );
        assert!(
            manifest_path.exists(),
            "a manifest whose descriptor changed after preflight must remain on disk"
        );
    }

    #[test]
    fn prune_apply_hashes_selected_manifest_blobs_before_deleting_manifest() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let data_dir = temp.path().join("cass-data");
        let source_path = temp.path().join("selected.jsonl");
        let source_bytes = b"{\"type\":\"message\",\"text\":\"safe\"}\n";
        fs::write(&source_path, source_bytes).expect("write source");
        let captured = capture_source_file(RawMirrorCaptureInput {
            data_dir: &data_dir,
            provider: "codex",
            source_id: "local",
            origin_kind: "local",
            origin_host: None,
            source_path: &source_path,
            db_links: &[],
        })
        .expect("capture source");
        let root = raw_mirror_root(&data_dir);
        let manifest_path = root.join(&captured.manifest_relative_path);
        let blob_path = root.join(&captured.blob_relative_path);
        let mut manifest = read_raw_mirror_manifest(&manifest_path).expect("read manifest");
        manifest.captured_at_ms = 0;
        manifest.manifest_blake3 = Some(raw_mirror_manifest_blake3(&manifest));
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).expect("serialize aged manifest"),
        )
        .expect("age manifest fixture");
        let mut corrupted = source_bytes.to_vec();
        corrupted[0] ^= 1;
        fs::write(&blob_path, &corrupted).expect("plant same-size blob corruption");

        let error = prune(
            &data_dir,
            RawMirrorPruneOptions {
                older_than_ms: Some(0),
                safety_hold_down_ms: 0,
                apply: true,
                ..RawMirrorPruneOptions::default()
            },
        )
        .expect_err("corrupt selected evidence must stop prune before deletion");
        assert!(
            format!("{error:#}").contains("checksum mismatch"),
            "unexpected selected-blob checksum error: {error:#}"
        );
        assert!(manifest_path.exists());
        assert!(blob_path.exists());
        assert!(!root.join("pruned.jsonl").exists());
    }

    #[test]
    fn prune_apply_refuses_a_held_index_run_lock_without_removing_evidence() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let data_dir = temp.path().join("cass-data");
        let source_path = temp.path().join("source.jsonl");
        fs::write(&source_path, b"{\"type\":\"message\",\"text\":\"old\"}\n")
            .expect("write source");
        let captured = capture_source_file(RawMirrorCaptureInput {
            data_dir: &data_dir,
            provider: "codex",
            source_id: "local",
            origin_kind: "local",
            origin_host: None,
            source_path: &source_path,
            db_links: &[],
        })
        .expect("capture source");
        let root = raw_mirror_root(&data_dir);
        let index_lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(data_dir.join("index-run.lock"))
            .expect("open index-run lock");
        fs2::FileExt::try_lock_exclusive(&index_lock).expect("hold index-run lock");

        let error = prune(
            &data_dir,
            RawMirrorPruneOptions {
                older_than_ms: Some(0),
                safety_hold_down_ms: 0,
                apply: true,
                ..RawMirrorPruneOptions::default()
            },
        )
        .expect_err("held index-run lock must block applied prune");

        assert!(
            error.to_string().contains("index run acquired"),
            "{error:#}"
        );
        assert!(root.join(captured.manifest_relative_path).exists());
        assert!(root.join(captured.blob_relative_path).exists());
        assert!(!root.join("pruned.jsonl").exists());
    }

    #[test]
    fn prune_dry_run_audits_without_removing_manifest_or_blob() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let data_dir = temp.path().join("cass-data");
        let source_path = temp.path().join("source.jsonl");
        fs::write(&source_path, b"{\"type\":\"message\",\"text\":\"old\"}\n")
            .expect("write source");
        let captured = capture_source_file(RawMirrorCaptureInput {
            data_dir: &data_dir,
            provider: "codex",
            source_id: "local",
            origin_kind: "local",
            origin_host: None,
            source_path: &source_path,
            db_links: &[],
        })
        .expect("capture source");

        let report = prune(
            &data_dir,
            RawMirrorPruneOptions {
                older_than_ms: Some(0),
                max_size_bytes: None,
                keep_tags: Vec::new(),
                safety_hold_down_ms: 0,
                apply: false,
            },
        )
        .expect("dry-run prune");

        assert!(report.initialized);
        assert_eq!(report.mode, "dry-run");
        assert_eq!(report.planned_manifest_count, 1);
        assert_eq!(report.planned_blob_count, 1);
        assert_eq!(report.applied_reclaim_bytes, 0);
        let root = data_dir
            .join(RAW_MIRROR_ROOT_DIR)
            .join(RAW_MIRROR_VERSION_DIR);
        assert!(root.join(&captured.manifest_relative_path).exists());
        assert!(root.join(&captured.blob_relative_path).exists());
        let audit_path = root.join("pruned.jsonl");
        let audit = fs::read_to_string(audit_path).expect("read audit");
        assert!(audit.contains("\"mode\":\"dry-run\""));
        assert!(audit.contains("\"applied\":false"));
    }

    #[test]
    #[cfg(unix)]
    fn prune_refuses_symlinked_audit_log_without_writing_target() -> Result<()> {
        use std::os::unix::fs::symlink;

        let temp = tempfile::TempDir::new()?;
        let data_dir = temp.path().join("cass-data");
        let source_path = temp.path().join("source.jsonl");
        let protected_audit_target = temp.path().join("protected-prune-audit.jsonl");
        fs::write(&source_path, b"{\"type\":\"message\",\"text\":\"old\"}\n")?;
        fs::write(&protected_audit_target, b"protected\n")?;

        let captured = capture_source_file(RawMirrorCaptureInput {
            data_dir: &data_dir,
            provider: "codex",
            source_id: "local",
            origin_kind: "local",
            origin_host: None,
            source_path: &source_path,
            db_links: &[],
        })?;
        let root = data_dir
            .join(RAW_MIRROR_ROOT_DIR)
            .join(RAW_MIRROR_VERSION_DIR);
        let audit_path = root.join("pruned.jsonl");
        symlink(&protected_audit_target, &audit_path)?;

        let err = match prune(
            &data_dir,
            RawMirrorPruneOptions {
                older_than_ms: Some(0),
                max_size_bytes: None,
                keep_tags: Vec::new(),
                safety_hold_down_ms: 0,
                apply: true,
            },
        ) {
            Ok(_) => anyhow::bail!("symlinked prune audit log was accepted before deletion"),
            Err(err) => err,
        };

        if !err.to_string().contains("prune audit through symlink") {
            anyhow::bail!("unexpected audit symlink error: {err:#}");
        }
        if !fs::read(&protected_audit_target)?
            .as_slice()
            .eq(b"protected\n")
        {
            anyhow::bail!("protected audit target was modified");
        }
        if !fs::read_link(&audit_path)?
            .as_os_str()
            .eq(protected_audit_target.as_os_str())
        {
            anyhow::bail!("audit path did not remain a symlink to the protected target");
        }
        if !root.join(&captured.manifest_relative_path).exists() {
            anyhow::bail!("failed audit append removed the captured manifest");
        }
        if !root.join(&captured.blob_relative_path).exists() {
            anyhow::bail!("failed audit append removed the captured blob");
        }
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn prune_preflight_rejects_symlinked_blob_ancestor_before_removing_manifest() -> Result<()> {
        use std::os::unix::fs::symlink;

        let temp = tempfile::TempDir::new()?;
        let data_dir = temp.path().join("cass-data");
        let source_path = temp.path().join("source.jsonl");
        fs::write(&source_path, b"{\"type\":\"message\",\"text\":\"old\"}\n")?;
        let captured = capture_source_file(RawMirrorCaptureInput {
            data_dir: &data_dir,
            provider: "codex",
            source_id: "local",
            origin_kind: "local",
            origin_host: None,
            source_path: &source_path,
            db_links: &[],
        })?;
        let root = raw_mirror_root(&data_dir);
        let manifest_path = root.join(&captured.manifest_relative_path);
        let blob_path = root.join(&captured.blob_relative_path);
        let blob_bucket = blob_path.parent().context("blob bucket")?;
        let outside_bucket = temp.path().join("outside-blob-bucket");
        fs::rename(blob_bucket, &outside_bucket)?;
        symlink(&outside_bucket, blob_bucket)?;

        let err = prune(
            &data_dir,
            RawMirrorPruneOptions {
                older_than_ms: Some(0),
                safety_hold_down_ms: 0,
                apply: true,
                ..RawMirrorPruneOptions::default()
            },
        )
        .expect_err("a symlinked blob ancestor must abort prune before any removal");
        let detail = format!("{err:#}");
        // bet45: the refusal wording moved from "symlinked ancestor" to the
        // blob-path phrasing; pin the semantic (a symlink refusal) rather
        // than the exact sentence.
        assert!(
            detail.contains("symlink"),
            "unexpected prune preflight error: {detail}"
        );
        assert!(
            manifest_path.exists(),
            "preflight failure removed the manifest"
        );
        assert!(blob_path.exists(), "preflight failure removed the blob");
        assert!(
            outside_bucket
                .join(blob_path.file_name().context("blob file name")?)
                .exists(),
            "preflight failure removed the external blob target"
        );
        assert!(!root.join("pruned.jsonl").exists());
        Ok(())
    }

    #[test]
    fn prune_apply_removes_selected_manifest_and_unreferenced_blob() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let data_dir = temp.path().join("cass-data");
        let source_path = temp.path().join("source.jsonl");
        fs::write(&source_path, b"{\"type\":\"message\",\"text\":\"apply\"}\n")
            .expect("write source");
        let captured = capture_source_file(RawMirrorCaptureInput {
            data_dir: &data_dir,
            provider: "codex",
            source_id: "local",
            origin_kind: "local",
            origin_host: None,
            source_path: &source_path,
            db_links: &[],
        })
        .expect("capture source");
        let root = data_dir
            .join(RAW_MIRROR_ROOT_DIR)
            .join(RAW_MIRROR_VERSION_DIR);
        let manifest_path = root.join(&captured.manifest_relative_path);
        let blob_path = root.join(&captured.blob_relative_path);

        let report = prune(
            &data_dir,
            RawMirrorPruneOptions {
                older_than_ms: Some(0),
                max_size_bytes: None,
                keep_tags: Vec::new(),
                safety_hold_down_ms: 0,
                apply: true,
            },
        )
        .expect("apply prune");

        assert_eq!(report.applied_manifest_count, 1);
        assert_eq!(report.applied_blob_count, 1);
        assert!(!manifest_path.exists());
        assert!(!blob_path.exists());
        let audit = fs::read_to_string(root.join("pruned.jsonl")).expect("read audit");
        assert!(audit.contains("\"mode\":\"apply\""));
        assert!(audit.contains("\"phase\":\"intent\""));
        assert!(audit.contains("\"phase\":\"result\""));
        assert!(audit.contains("\"applied\":true"));
    }

    #[test]
    fn prune_apply_keeps_blob_referenced_by_retained_manifest() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let data_dir = temp.path().join("cass-data");
        let first_source = temp.path().join("first.jsonl");
        let second_source = temp.path().join("second.jsonl");
        let bytes = b"{\"type\":\"message\",\"text\":\"shared-retained\"}\n";
        fs::write(&first_source, bytes).expect("write first");
        fs::write(&second_source, bytes).expect("write second");
        let first = capture_source_file(RawMirrorCaptureInput {
            data_dir: &data_dir,
            provider: "codex",
            source_id: "local",
            origin_kind: "local",
            origin_host: None,
            source_path: &first_source,
            db_links: &[],
        })
        .expect("capture first");
        let second = capture_source_file(RawMirrorCaptureInput {
            data_dir: &data_dir,
            provider: "codex",
            source_id: "local",
            origin_kind: "local",
            origin_host: None,
            source_path: &second_source,
            db_links: &[],
        })
        .expect("capture second");
        let root = data_dir
            .join(RAW_MIRROR_ROOT_DIR)
            .join(RAW_MIRROR_VERSION_DIR);
        let first_manifest_path = root.join(&first.manifest_relative_path);
        let second_manifest_path = root.join(&second.manifest_relative_path);
        let mut first_manifest =
            read_raw_mirror_manifest(&first_manifest_path).expect("first manifest");
        first_manifest.captured_at_ms = now_ms().saturating_sub(2 * 86_400_000);
        first_manifest.manifest_blake3 = Some(raw_mirror_manifest_blake3(&first_manifest));
        fs::write(
            &first_manifest_path,
            serde_json::to_vec_pretty(&first_manifest).expect("serialize first manifest"),
        )
        .expect("rewrite first manifest");

        let report = prune(
            &data_dir,
            RawMirrorPruneOptions {
                older_than_ms: Some(86_400_000),
                max_size_bytes: None,
                keep_tags: Vec::new(),
                safety_hold_down_ms: 0,
                apply: true,
            },
        )
        .expect("apply one-manifest prune");

        assert_eq!(report.applied_manifest_count, 1);
        assert_eq!(report.applied_blob_count, 0);
        assert!(!first_manifest_path.exists());
        assert!(second_manifest_path.exists());
        assert!(
            root.join(&first.blob_relative_path).exists(),
            "shared blob must stay while a retained manifest still references it"
        );
    }

    #[test]
    fn prune_apply_keep_tag_pins_linked_manifest_and_blob() {
        use crate::franken_sync::compat::ConnectionExt as _;

        let temp = tempfile::TempDir::new().expect("tempdir");
        let data_dir = temp.path().join("cass-data");
        std::fs::create_dir_all(&data_dir).expect("create data dir");
        let source_path = temp.path().join("tagged.jsonl");
        fs::write(
            &source_path,
            b"{\"type\":\"message\",\"text\":\"tagged\"}\n",
        )
        .expect("write source");
        let db_link = RawMirrorDbLink {
            conversation_id: Some(7),
            message_count: Some(1),
            source_path: Some(source_path.display().to_string()),
            started_at_ms: Some(1_733_000_000_000),
        };
        let captured = capture_source_file(RawMirrorCaptureInput {
            data_dir: &data_dir,
            provider: "codex",
            source_id: "local",
            origin_kind: "local",
            origin_host: None,
            source_path: &source_path,
            db_links: std::slice::from_ref(&db_link),
        })
        .expect("capture source");
        let db_path = data_dir.join("agent_search.db");
        let conn = crate::franken_sync::Connection::open(db_path.to_string_lossy().into_owned())
            .expect("open keep-tag db");
        conn.execute_compat(
            "CREATE TABLE tags (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE)",
            crate::franken_sync::params![],
        )
        .expect("create tags");
        conn.execute_compat(
            "CREATE TABLE conversation_tags (conversation_id INTEGER NOT NULL, tag_id INTEGER NOT NULL, PRIMARY KEY (conversation_id, tag_id))",
            crate::franken_sync::params![],
        )
        .expect("create conversation_tags");
        conn.execute_compat(
            "INSERT INTO tags (id, name) VALUES (1, 'keep')",
            crate::franken_sync::params![],
        )
        .expect("insert tag");
        conn.execute_compat(
            "INSERT INTO conversation_tags (conversation_id, tag_id) VALUES (7, 1)",
            crate::franken_sync::params![],
        )
        .expect("insert conversation tag");
        drop(conn);

        let report = prune(
            &data_dir,
            RawMirrorPruneOptions {
                older_than_ms: Some(0),
                max_size_bytes: Some(0),
                keep_tags: vec!["keep".to_string()],
                safety_hold_down_ms: 0,
                apply: true,
            },
        )
        .expect("keep-tag prune");

        let root = data_dir
            .join(RAW_MIRROR_ROOT_DIR)
            .join(RAW_MIRROR_VERSION_DIR);
        assert_eq!(report.pinned_manifest_count, 1);
        assert_eq!(report.pinned_blob_count, 1);
        assert_eq!(report.planned_manifest_count, 0);
        assert_eq!(report.planned_blob_count, 0);
        assert!(root.join(&captured.manifest_relative_path).exists());
        assert!(root.join(&captured.blob_relative_path).exists());
    }

    #[test]
    fn prune_apply_safety_hold_down_pins_recent_manifest_during_size_prune() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let data_dir = temp.path().join("cass-data");
        let source_path = temp.path().join("recent.jsonl");
        fs::write(
            &source_path,
            b"{\"type\":\"message\",\"text\":\"recent\"}\n",
        )
        .expect("write source");
        let captured = capture_source_file(RawMirrorCaptureInput {
            data_dir: &data_dir,
            provider: "codex",
            source_id: "local",
            origin_kind: "local",
            origin_host: None,
            source_path: &source_path,
            db_links: &[],
        })
        .expect("capture source");

        let report = prune(
            &data_dir,
            RawMirrorPruneOptions {
                older_than_ms: None,
                max_size_bytes: Some(0),
                keep_tags: Vec::new(),
                safety_hold_down_ms: 7 * 86_400_000,
                apply: true,
            },
        )
        .expect("hold-down prune");

        let root = data_dir
            .join(RAW_MIRROR_ROOT_DIR)
            .join(RAW_MIRROR_VERSION_DIR);
        assert_eq!(report.pinned_manifest_count, 1);
        assert_eq!(report.pinned_blob_count, 1);
        assert_eq!(report.planned_manifest_count, 0);
        assert_eq!(report.planned_blob_count, 0);
        assert!(root.join(&captured.manifest_relative_path).exists());
        assert!(root.join(&captured.blob_relative_path).exists());
    }

    #[test]
    fn capture_source_file_revalidates_cached_blob_contents() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let data_dir = temp.path().join("cass-data");
        let source_path = temp.path().join("cached-source.jsonl");
        let source_bytes = b"{\"type\":\"message\",\"text\":\"cache me\"}\n";
        fs::write(&source_path, source_bytes).expect("write source");

        let first = capture_source_file(RawMirrorCaptureInput {
            data_dir: &data_dir,
            provider: "codex",
            source_id: "local",
            origin_kind: "local",
            origin_host: None,
            source_path: &source_path,
            db_links: &[],
        })
        .expect("first capture");

        let blob_path = data_dir
            .join(RAW_MIRROR_ROOT_DIR)
            .join(RAW_MIRROR_VERSION_DIR)
            .join(&first.blob_relative_path);
        fs::write(&blob_path, b"corrupted cached blob").expect("corrupt cached blob");

        let err = capture_source_file(RawMirrorCaptureInput {
            data_dir: &data_dir,
            provider: "codex",
            source_id: "local",
            origin_kind: "local",
            origin_host: None,
            source_path: &source_path,
            db_links: &[],
        })
        .expect_err("corrupted content-addressed blob must be rejected");
        // bet45: the refusal wording moved (size/safety classification now
        // says "raw mirror blob ... is missing, unsafe, or has the wrong
        // size"); pin the shared "raw mirror blob" refusal identity.
        assert!(
            err.to_string().contains("raw mirror blob"),
            "unexpected cached-blob error: {err:#}"
        );
        assert_eq!(fs::read(&source_path).expect("source bytes"), source_bytes);
    }

    #[test]
    fn blob_cache_keeps_one_bounded_entry_per_source_path() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let source_path = temp.path().join("source.jsonl");
        let first_key = RawMirrorBlobCacheKey {
            data_dir: temp.path().join("cass-data"),
            source_path: source_path.clone(),
            source_identity: Some("first".to_string()),
            source_size_bytes: 1,
            source_mtime_ns: Some(1),
            source_change_time_ns: Some(1),
            chunk_threshold_bytes: 1,
            chunk_size_bytes: 16,
        };
        let second_key = RawMirrorBlobCacheKey {
            source_identity: Some("second".to_string()),
            source_size_bytes: 2,
            source_mtime_ns: Some(2),
            source_change_time_ns: Some(2),
            ..first_key.clone()
        };
        let first_record = RawMirrorBlobRecord {
            blob_blake3: "00".repeat(32),
            blob_size_bytes: 1,
            source_content_blake3: "00".repeat(32),
            source_size_bytes: 1,
            content_storage: None,
        };
        let second_record = RawMirrorBlobRecord {
            blob_blake3: "11".repeat(32),
            blob_size_bytes: 2,
            source_content_blake3: "11".repeat(32),
            source_size_bytes: 2,
            content_storage: None,
        };
        let root = ensure_raw_mirror_root(&first_key.data_dir).expect("create mirror root");
        for (record, bytes) in [(&first_record, &b"a"[..]), (&second_record, &b"bb"[..])] {
            let relative = raw_mirror_blob_relative_path(&record.blob_blake3)
                .expect("fixture blob relative path");
            let path = root.join(relative);
            ensure_private_dir_descendant(&root, path.parent().expect("blob parent"))
                .expect("create fixture blob parent");
            fs::write(path, bytes).expect("write fixture blob");
        }

        cache_raw_mirror_blob_record(first_key, first_record);
        cache_raw_mirror_blob_record(second_key.clone(), second_record.clone());

        let source_key = raw_mirror_blob_source_key(&second_key);
        let cache = BLOB_CAPTURE_CACHE.get().expect("cache initialized");
        let guard = cache.lock().expect("cache lock");
        let cached = guard.get(&source_key).expect("latest source cache entry");
        assert_eq!(cached.cache_key, second_key);
        assert_eq!(cached.record, second_record);
        assert!(guard.len() <= RAW_MIRROR_BLOB_CACHE_MAX_ENTRIES);
    }

    #[test]
    fn blob_cache_refuses_keys_without_file_identity_and_change_time() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let weak_key = RawMirrorBlobCacheKey {
            data_dir: temp.path().join("cass-data"),
            source_path: temp.path().join("weak-key-source.jsonl"),
            source_identity: None,
            source_size_bytes: 1,
            source_mtime_ns: Some(1),
            source_change_time_ns: None,
            chunk_threshold_bytes: 1,
            chunk_size_bytes: 16,
        };
        let record = RawMirrorBlobRecord {
            blob_blake3: "22".repeat(32),
            blob_size_bytes: 1,
            source_content_blake3: "22".repeat(32),
            source_size_bytes: 1,
            content_storage: None,
        };

        assert!(!raw_mirror_blob_cache_key_is_strong(&weak_key));
        cache_raw_mirror_blob_record(weak_key.clone(), record);

        let source_key = raw_mirror_blob_source_key(&weak_key);
        // bet45: the global cache is a OnceLock that a refused weak-key
        // insert never initializes — under filtered test runs no other test
        // may have initialized it either, in which case the property holds
        // vacuously (nothing was cached at all).
        if let Some(cache) = BLOB_CAPTURE_CACHE.get() {
            assert!(
                !cache.lock().expect("cache lock").contains_key(&source_key),
                "a weak non-Unix-style metadata key must never authorize cache reuse"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn final_source_path_check_detects_same_size_atomic_replacement() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let source_path = temp.path().join("rotating-source.jsonl");
        let replacement_path = temp.path().join("replacement.jsonl");
        fs::write(&source_path, b"original\n").expect("write original source");
        fs::write(&replacement_path, b"replaced\n").expect("write same-size replacement");
        let initial = fs::symlink_metadata(&source_path).expect("initial source metadata");

        assert!(!source_path_changed_identity_during_capture(
            &source_path,
            &initial
        ));
        fs::rename(&replacement_path, &source_path).expect("atomically replace source path");
        assert!(
            source_path_changed_identity_during_capture(&source_path, &initial),
            "capture must reject bytes from an inode no longer reachable through the recorded path"
        );
    }

    #[cfg(unix)]
    #[test]
    fn capture_source_file_does_not_reuse_cache_after_same_size_mtime_preserving_rewrite() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let data_dir = temp.path().join("cass-data");
        let source_path = temp.path().join("same-size-rewrite.jsonl");
        let first_bytes = b"same length payload A\n";
        let second_bytes = b"same length payload B\n";
        fs::write(&source_path, first_bytes).expect("write first source");

        let first_modified = fs::metadata(&source_path)
            .expect("first metadata")
            .modified()
            .expect("first modified time");
        let first = capture_source_file(RawMirrorCaptureInput {
            data_dir: &data_dir,
            provider: "codex",
            source_id: "local",
            origin_kind: "local",
            origin_host: None,
            source_path: &source_path,
            db_links: &[],
        })
        .expect("first capture");

        std::thread::sleep(std::time::Duration::from_millis(5));
        fs::write(&source_path, second_bytes).expect("rewrite source");
        let source = OpenOptions::new()
            .write(true)
            .open(&source_path)
            .expect("open rewritten source");
        source
            .set_times(std::fs::FileTimes::new().set_modified(first_modified))
            .expect("restore original mtime");

        let second = capture_source_file(RawMirrorCaptureInput {
            data_dir: &data_dir,
            provider: "codex",
            source_id: "local",
            origin_kind: "local",
            origin_host: None,
            source_path: &source_path,
            db_links: &[],
        })
        .expect("second capture");

        assert_ne!(first.blob_blake3, second.blob_blake3);
        assert_eq!(
            second.blob_blake3,
            blake3::hash(second_bytes).to_hex().to_string()
        );
        assert_eq!(
            fs::read(&source_path).expect("source bytes after rewrite"),
            second_bytes
        );
    }

    #[cfg(unix)]
    #[test]
    fn capture_source_file_rejects_symlinked_existing_blob_path() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let data_dir = temp.path().join("cass-data");
        let source_path = temp.path().join("cached-source.jsonl");
        let source_bytes = b"{\"type\":\"message\",\"text\":\"cache me\"}\n";
        fs::write(&source_path, source_bytes).expect("write source");

        let blob_blake3 = blake3::hash(source_bytes).to_hex().to_string();
        let blob_relative_path =
            raw_mirror_blob_relative_path(&blob_blake3).expect("blob relative path");
        let blob_path = data_dir
            .join(RAW_MIRROR_ROOT_DIR)
            .join(RAW_MIRROR_VERSION_DIR)
            .join(&blob_relative_path);
        fs::create_dir_all(blob_path.parent().expect("blob parent")).expect("blob parent dir");
        let outside = temp.path().join("outside.raw");
        fs::write(&outside, source_bytes).expect("outside blob bytes");
        std::os::unix::fs::symlink(&outside, &blob_path).expect("symlink blob");

        let err = capture_source_file(RawMirrorCaptureInput {
            data_dir: &data_dir,
            provider: "codex",
            source_id: "local",
            origin_kind: "local",
            origin_host: None,
            source_path: &source_path,
            db_links: &[],
        })
        .expect_err("symlinked content-addressed blob path must be rejected");
        assert!(
            err.to_string().contains("contains a symlink"),
            "unexpected symlink-blob error: {err:#}"
        );

        let manifest_root = data_dir
            .join(RAW_MIRROR_ROOT_DIR)
            .join(RAW_MIRROR_VERSION_DIR)
            .join("manifests");
        assert!(
            !manifest_root.exists(),
            "failed blob publish must not write a manifest pointing at a symlinked blob"
        );
        assert_eq!(fs::read(&source_path).expect("source bytes"), source_bytes);
        assert_eq!(fs::read(&outside).expect("outside bytes"), source_bytes);
    }

    #[cfg(unix)]
    #[test]
    fn capture_source_file_rejects_symlinked_mutation_lock() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let data_dir = temp.path().join("cass-data");
        let root = data_dir
            .join(RAW_MIRROR_ROOT_DIR)
            .join(RAW_MIRROR_VERSION_DIR);
        let source_path = temp.path().join("source.jsonl");
        let protected_target = temp.path().join("protected-lock-target");
        let source_bytes = b"{\"type\":\"message\",\"text\":\"hello\"}\n";
        fs::create_dir_all(&root).expect("raw mirror root");
        fs::write(&source_path, source_bytes).expect("source bytes");
        fs::write(&protected_target, b"protected\n").expect("protected target");
        std::os::unix::fs::symlink(&protected_target, root.join(RAW_MIRROR_MUTATION_LOCK_FILE))
            .expect("symlink mutation lock");

        let error = capture_source_file(RawMirrorCaptureInput {
            data_dir: &data_dir,
            provider: "codex",
            source_id: "local",
            origin_kind: "local",
            origin_host: None,
            source_path: &source_path,
            db_links: &[],
        })
        .expect_err("symlinked mutation lock must be rejected");

        assert!(
            error.to_string().contains("mutation lock path"),
            "{error:#}"
        );
        assert_eq!(
            fs::read(&protected_target).expect("protected bytes"),
            b"protected\n"
        );
        assert!(!root.join("manifests").exists());
    }

    #[cfg(unix)]
    #[test]
    fn capture_source_file_rejects_symlinked_raw_mirror_root_dir() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let data_dir = temp.path().join("cass-data");
        let source_path = temp.path().join("source.jsonl");
        let outside_mirror = temp.path().join("outside-mirror");
        let source_bytes = b"{\"type\":\"message\",\"text\":\"do not redirect archive\"}\n";

        fs::create_dir_all(&data_dir).expect("data dir");
        fs::create_dir_all(&outside_mirror).expect("outside mirror dir");
        fs::write(&source_path, source_bytes).expect("write source");
        std::os::unix::fs::symlink(&outside_mirror, data_dir.join(RAW_MIRROR_ROOT_DIR))
            .expect("symlink raw mirror root");

        let err = capture_source_file(RawMirrorCaptureInput {
            data_dir: &data_dir,
            provider: "codex",
            source_id: "local",
            origin_kind: "local",
            origin_host: None,
            source_path: &source_path,
            db_links: &[],
        })
        .expect_err("symlinked raw-mirror root must be rejected");

        assert!(
            err.to_string().contains("symlink raw mirror dir"),
            "unexpected symlink-root error: {err:#}"
        );
        assert!(
            !outside_mirror.join(RAW_MIRROR_VERSION_DIR).exists(),
            "raw mirror capture must not create redirected archive state outside data_dir"
        );
        assert_eq!(fs::read(&source_path).expect("source bytes"), source_bytes);
    }

    #[cfg(unix)]
    #[test]
    fn capture_source_file_rejects_symlinked_blob_directory_component() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let data_dir = temp.path().join("cass-data");
        let root = data_dir
            .join(RAW_MIRROR_ROOT_DIR)
            .join(RAW_MIRROR_VERSION_DIR);
        let source_path = temp.path().join("source.jsonl");
        let outside_blobs = temp.path().join("outside-blobs");
        let source_bytes = b"{\"type\":\"message\",\"text\":\"do not redirect blobs\"}\n";

        fs::create_dir_all(&root).expect("raw mirror root");
        fs::create_dir_all(&outside_blobs).expect("outside blobs dir");
        fs::write(&source_path, source_bytes).expect("write source");
        std::os::unix::fs::symlink(&outside_blobs, root.join("blobs")).expect("symlink blobs dir");

        let err = capture_source_file(RawMirrorCaptureInput {
            data_dir: &data_dir,
            provider: "codex",
            source_id: "local",
            origin_kind: "local",
            origin_host: None,
            source_path: &source_path,
            db_links: &[],
        })
        .expect_err("symlinked blob directory must be rejected");

        assert!(
            err.to_string().contains("symlink raw mirror dir"),
            "unexpected symlink-blob-dir error: {err:#}"
        );
        assert!(
            !outside_blobs.join(RAW_MIRROR_HASH_ALGORITHM).exists(),
            "raw mirror capture must not create redirected blob state outside data_dir"
        );
        assert!(
            !root.join("manifests").exists(),
            "failed blob publish must not write a manifest"
        );
        assert_eq!(fs::read(&source_path).expect("source bytes"), source_bytes);
    }

    #[test]
    fn capture_source_file_rejects_non_file_sources() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let data_dir = temp.path().join("cass-data");
        let source_dir = temp.path().join("source-dir");
        fs::create_dir(&source_dir).expect("source dir");

        let err = capture_source_file(RawMirrorCaptureInput {
            data_dir: &data_dir,
            provider: "codex",
            source_id: "local",
            origin_kind: "local",
            origin_host: None,
            source_path: &source_dir,
            db_links: &[],
        })
        .expect_err("directory source should be rejected");
        assert!(
            err.to_string().contains("non-file source"),
            "unexpected non-file-source error: {err}"
        );
        assert!(
            !data_dir.join(RAW_MIRROR_ROOT_DIR).exists(),
            "rejected non-file sources must not initialize raw mirror storage"
        );
    }

    #[cfg(unix)]
    #[test]
    fn capture_source_file_rejects_unreadable_sources_without_manifest() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::TempDir::new().expect("tempdir");
        let data_dir = temp.path().join("cass-data");
        let source_path = temp.path().join("unreadable.jsonl");
        fs::write(&source_path, b"private session bytes\n").expect("source");
        fs::set_permissions(&source_path, fs::Permissions::from_mode(0o000))
            .expect("make source unreadable");

        // root (or CAP_DAC_OVERRIDE) bypasses POSIX permission checks, so the
        // rejection under test cannot trigger. Probe instead of asserting a
        // uid so the skip covers capability-granted environments too.
        if fs::File::open(&source_path).is_ok() {
            eprintln!(
                "skipping capture_source_file_rejects_unreadable_sources_without_manifest: \
                 process bypasses POSIX permission checks (root/CAP_DAC_OVERRIDE)"
            );
            let _ = fs::set_permissions(&source_path, fs::Permissions::from_mode(0o600));
            return;
        }

        let err = capture_source_file(RawMirrorCaptureInput {
            data_dir: &data_dir,
            provider: "codex",
            source_id: "local",
            origin_kind: "local",
            origin_host: None,
            source_path: &source_path,
            db_links: &[],
        })
        .expect_err("unreadable source should be rejected");
        fs::set_permissions(&source_path, fs::Permissions::from_mode(0o600))
            .expect("restore source perms");
        assert!(
            err.to_string().contains("open raw mirror source"),
            "unexpected unreadable-source error: {err}"
        );
        assert!(
            !data_dir.join("raw-mirror/v1/manifests").exists(),
            "failed unreadable-source captures must not publish manifests"
        );
    }

    #[cfg(unix)]
    #[test]
    fn capture_source_file_rejects_symlink_sources() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::TempDir::new().expect("tempdir");
        let data_dir = temp.path().join("cass-data");
        let real_source = temp.path().join("real.jsonl");
        let symlink_source = temp.path().join("link.jsonl");
        fs::write(&real_source, b"secret session").expect("write source");
        symlink(&real_source, &symlink_source).expect("symlink");

        let err = capture_source_file(RawMirrorCaptureInput {
            data_dir: &data_dir,
            provider: "codex",
            source_id: "local",
            origin_kind: "local",
            origin_host: None,
            source_path: &symlink_source,
            db_links: &[],
        })
        .expect_err("symlink source should be rejected");
        assert!(
            err.to_string().contains("symlink source"),
            "unexpected error: {err:#}"
        );
    }
}
