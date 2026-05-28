use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
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

static TEMP_NONCE: AtomicU64 = AtomicU64::new(0);
static BLOB_CAPTURE_CACHE: OnceLock<Mutex<HashMap<RawMirrorBlobCacheKey, RawMirrorBlobRecord>>> =
    OnceLock::new();
static MANIFEST_UPDATE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

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
    pub captured_at_ms: i64,
    pub source_mtime_ms: Option<i64>,
    pub already_present: bool,
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

        let Some(blob_relative_path) = raw_mirror_blob_relative_path(&manifest.blob_blake3) else {
            summary.invalid_manifest_count = summary.invalid_manifest_count.saturating_add(1);
            continue;
        };
        if manifest.blob_relative_path != blob_relative_path {
            summary.invalid_manifest_count = summary.invalid_manifest_count.saturating_add(1);
            continue;
        }

        if !seen_blobs.insert(blob_relative_path.clone()) {
            continue;
        }
        let blob_path = root.join(blob_relative_path);
        match fs::symlink_metadata(&blob_path) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
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

    summary
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
    pub size_bytes: u64,
    pub reason: String,
    pub applied: bool,
}

#[derive(Debug, Clone)]
struct RawMirrorPruneManifest {
    manifest_id: String,
    relative_path: String,
    size_bytes: u64,
    blob_blake3: String,
    blob_relative_path: String,
    blob_size_bytes: u64,
    captured_at_ms: i64,
    provider: String,
    original_path: String,
    db_links: Vec<RawMirrorDbLink>,
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

    let manifests = collect_prune_manifests(&root)?;
    report.manifest_count = manifests.len() as u64;

    let mut blob_to_manifests: HashMap<String, Vec<String>> = HashMap::new();
    let mut manifest_by_id: HashMap<String, &RawMirrorPruneManifest> = HashMap::new();
    let mut blob_size_by_relative: HashMap<String, u64> = HashMap::new();
    for manifest in &manifests {
        manifest_by_id.insert(manifest.manifest_id.clone(), manifest);
        blob_to_manifests
            .entry(manifest.blob_relative_path.clone())
            .or_default()
            .push(manifest.manifest_id.clone());
        blob_size_by_relative
            .entry(manifest.blob_relative_path.clone())
            .or_insert_with(|| {
                blob_file_size(&root.join(&manifest.blob_relative_path))
                    .unwrap_or(manifest.blob_size_bytes)
            });
    }
    report.unique_blob_count = blob_size_by_relative.len() as u64;
    report.current_blob_bytes = blob_size_by_relative
        .values()
        .copied()
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
    let pinned_blobs: HashSet<String> = blob_to_manifests
        .iter()
        .filter(|(_, manifest_ids)| manifest_ids.iter().any(|id| pinned_manifests.contains(id)))
        .map(|(blob_relative_path, _)| blob_relative_path.clone())
        .collect();
    report.pinned_blob_count = pinned_blobs.len() as u64;

    let mut selected_manifests: HashSet<String> = HashSet::new();
    let mut manifest_reasons: HashMap<String, String> = HashMap::new();

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
    }

    if let Some(max_size_bytes) = options.max_size_bytes
        && report.current_blob_bytes > max_size_bytes
    {
        let mut blob_groups: Vec<_> = blob_to_manifests
            .iter()
            .map(|(blob_relative_path, manifest_ids)| {
                let oldest_capture = manifest_ids
                    .iter()
                    .filter_map(|id| manifest_by_id.get(id).map(|m| m.captured_at_ms))
                    .min()
                    .unwrap_or(i64::MAX);
                let size = blob_size_by_relative
                    .get(blob_relative_path)
                    .copied()
                    .unwrap_or(0);
                (
                    blob_relative_path.clone(),
                    manifest_ids.clone(),
                    oldest_capture,
                    size,
                )
            })
            .collect::<Vec<_>>();
        blob_groups.sort_by(|left, right| left.2.cmp(&right.2).then_with(|| left.0.cmp(&right.0)));

        let mut projected_bytes = report.current_blob_bytes;
        for (blob_relative_path, manifest_ids, _, size) in blob_groups {
            if projected_bytes <= max_size_bytes {
                break;
            }
            if pinned_blobs.contains(&blob_relative_path) {
                continue;
            }
            for manifest_id in manifest_ids {
                if !pinned_manifests.contains(&manifest_id) {
                    selected_manifests.insert(manifest_id.clone());
                    manifest_reasons.entry(manifest_id).or_insert_with(|| {
                        format!("max-size over budget; retiring blob {blob_relative_path}")
                    });
                }
            }
            projected_bytes = projected_bytes.saturating_sub(size);
        }
    }

    let selected_blobs: HashSet<String> = blob_to_manifests
        .iter()
        .filter(|(_, manifest_ids)| {
            manifest_ids
                .iter()
                .all(|id| selected_manifests.contains(id))
        })
        .map(|(blob_relative_path, _)| blob_relative_path.clone())
        .collect();

    let mut entries = Vec::new();
    let mut selected_manifest_ids = selected_manifests.into_iter().collect::<Vec<_>>();
    selected_manifest_ids.sort();
    for manifest_id in selected_manifest_ids {
        let Some(manifest) = manifest_by_id.get(&manifest_id) else {
            continue;
        };
        let reason = manifest_reasons
            .remove(&manifest_id)
            .unwrap_or_else(|| "selected by retention policy".to_string());
        entries.push(RawMirrorPruneEntry {
            kind: "manifest".to_string(),
            path: manifest.relative_path.clone(),
            blob_blake3: Some(manifest.blob_blake3.clone()),
            size_bytes: manifest.size_bytes,
            reason,
            applied: false,
        });
    }

    let mut selected_blob_paths = selected_blobs.into_iter().collect::<Vec<_>>();
    selected_blob_paths.sort();
    for blob_relative_path in selected_blob_paths {
        let size = blob_size_by_relative
            .get(&blob_relative_path)
            .copied()
            .unwrap_or(0);
        let blob_blake3 = blob_relative_path
            .rsplit('/')
            .next()
            .and_then(|name| name.strip_suffix(".raw"))
            .map(ToOwned::to_owned);
        entries.push(RawMirrorPruneEntry {
            kind: "blob".to_string(),
            path: blob_relative_path,
            blob_blake3,
            size_bytes: size,
            reason: "no retained manifest references this blob after prune plan".to_string(),
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

    if options.apply {
        for entry in &mut entries {
            let path = root.join(&entry.path);
            let removed = remove_prune_target_file(&path)
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
    }

    report.entries = entries;
    if !report.entries.is_empty() {
        let audit_path = append_prune_audit_log(&root, &report)?;
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
        let manifest = read_raw_mirror_manifest(&path)?;
        if manifest.manifest_kind != RAW_MIRROR_MANIFEST_KIND {
            anyhow::bail!(
                "refusing to prune with unexpected raw mirror manifest kind `{}` in {}",
                manifest.manifest_kind,
                path.display()
            );
        }
        let Some(expected_blob_relative_path) =
            raw_mirror_blob_relative_path(&manifest.blob_blake3)
        else {
            anyhow::bail!(
                "refusing to prune raw mirror manifest {} with invalid blob hash",
                path.display()
            );
        };
        if manifest.blob_relative_path != expected_blob_relative_path {
            anyhow::bail!(
                "refusing to prune raw mirror manifest {} with unexpected blob path `{}`",
                path.display(),
                manifest.blob_relative_path
            );
        }
        let relative_path = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .display()
            .to_string();
        manifests.push(RawMirrorPruneManifest {
            manifest_id: manifest.manifest_id,
            relative_path,
            size_bytes: manifest_metadata.len(),
            blob_blake3: manifest.blob_blake3,
            blob_relative_path: manifest.blob_relative_path,
            blob_size_bytes: manifest.blob_size_bytes,
            captured_at_ms: manifest.captured_at_ms,
            provider: manifest.provider,
            original_path: manifest.original_path,
            db_links: manifest.db_links,
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
    use frankensqlite::compat::{ConnectionExt as _, ParamValue, RowExt as _};

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
            .query_map_collect(&sql, &params, |row: &frankensqlite::Row| row.get_typed(0))
            .with_context(|| "query raw-mirror prune keep-tag conversation pins")?;
        pinned.extend(rows);
    }

    Ok(pinned)
}

fn blob_file_size(path: &Path) -> Option<u64> {
    fs::symlink_metadata(path)
        .ok()
        .filter(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
        .map(|metadata| metadata.len())
}

fn remove_prune_target_file(path: &Path) -> Result<bool> {
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
    fs::remove_file(path).with_context(|| format!("remove raw mirror file {}", path.display()))?;
    sync_parent(path)?;
    Ok(true)
}

fn append_prune_audit_log(root: &Path, report: &RawMirrorPruneReport) -> Result<PathBuf> {
    ensure_private_dir(root)?;
    let audit_path = root.join("pruned.jsonl");
    ensure_prune_audit_log_appendable(&audit_path)?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&audit_path)
        .with_context(|| format!("open raw mirror prune audit {}", audit_path.display()))?;
    set_private_file_permissions(&audit_path)?;
    let now = now_ms();
    for entry in &report.entries {
        let record = json!({
            "schema_version": 1,
            "recorded_at_ms": now,
            "mode": report.mode,
            "kind": entry.kind,
            "path": entry.path,
            "blob_blake3": entry.blob_blake3,
            "size_bytes": entry.size_bytes,
            "reason": entry.reason,
            "applied": entry.applied,
        });
        writeln!(file, "{record}")
            .with_context(|| format!("write raw mirror prune audit {}", audit_path.display()))?;
    }
    file.sync_all()
        .with_context(|| format!("sync raw mirror prune audit {}", audit_path.display()))?;
    sync_parent(&audit_path)?;
    Ok(audit_path)
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RawMirrorBlobRecord {
    blob_blake3: String,
    bytes_copied: u64,
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
    compression: RawMirrorCompressionEnvelope,
    encryption: RawMirrorEncryptionEnvelope,
    db_links: Vec<RawMirrorDbLink>,
    verification: RawMirrorVerificationRecord,
    manifest_blake3: Option<String>,
}

pub fn capture_source_file(input: RawMirrorCaptureInput<'_>) -> Result<RawMirrorCaptureRecord> {
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

    let cache_key = raw_mirror_blob_cache_key(&input, &source_metadata);
    let (blob_blake3, bytes_copied, blob_already_present) =
        match cached_raw_mirror_blob_record(&cache_key, &root) {
            Some(record) => (record.blob_blake3, record.bytes_copied, true),
            None => {
                let temp_dir = unique_capture_temp_dir(&root);
                ensure_private_dir_descendant(&root, &temp_dir)?;
                let CopyToTempResult {
                    temp_path,
                    blob_blake3,
                    bytes_copied,
                } = copy_source_to_private_temp(input.source_path, &temp_dir, &source_metadata)?;
                let blob_relative_path = raw_mirror_blob_relative_path(&blob_blake3)
                    .ok_or_else(|| anyhow!("computed invalid raw mirror blake3 digest"))?;
                let blob_path = root.join(&blob_relative_path);
                let already_present =
                    publish_content_addressed_temp(&root, &temp_path, &blob_path, &blob_blake3)?;
                remove_empty_temp_dir_best_effort(&temp_dir);
                cache_raw_mirror_blob_record(
                    cache_key.clone(),
                    RawMirrorBlobRecord {
                        blob_blake3: blob_blake3.clone(),
                        bytes_copied,
                    },
                );
                (blob_blake3, bytes_copied, already_present)
            }
        };
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
        blob_size_bytes: bytes_copied,
        provider: input.provider.to_string(),
        source_id: input.source_id.to_string(),
        origin_kind: input.origin_kind.to_string(),
        origin_host: input.origin_host.map(ToOwned::to_owned),
        original_path,
        redacted_original_path: redacted_original_path(input.provider, input.source_path),
        original_path_blake3,
        captured_at_ms,
        source_mtime_ms,
        source_size_bytes: source_metadata.len(),
        compression: RawMirrorCompressionEnvelope {
            state: "none".to_string(),
            algorithm: None,
            uncompressed_size_bytes: Some(bytes_copied),
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
            content_blake3: Some(blob_blake3.clone()),
            verified_at_ms: Some(captured_at_ms),
        },
        manifest_blake3: None,
    };
    manifest.manifest_blake3 = Some(raw_mirror_manifest_blake3(&manifest));
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
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
            (bytes_copied, captured_at_ms, source_mtime_ms)
        };

    Ok(RawMirrorCaptureRecord {
        manifest_id,
        manifest_relative_path,
        blob_relative_path,
        blob_blake3,
        blob_size_bytes: record_blob_size_bytes,
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
    let manifest_path = raw_mirror_manifest_path_from_relative(&root, manifest_relative_path)?;
    merge_raw_mirror_manifest_db_links(&root, &manifest_path, links, None)
}

struct CopyToTempResult {
    temp_path: PathBuf,
    blob_blake3: String,
    bytes_copied: u64,
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
    temp.sync_all()
        .with_context(|| format!("sync raw mirror temp {}", temp_path.display()))?;

    let final_source_metadata = source
        .metadata()
        .with_context(|| format!("stat opened raw mirror source {}", source_path.display()))?;
    if source_file_changed_during_capture(source_metadata, &final_source_metadata) {
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
    if initial.len() != final_metadata.len() {
        return true;
    }
    match (initial.modified().ok(), final_metadata.modified().ok()) {
        (Some(initial_mtime), Some(final_mtime)) => initial_mtime != final_mtime,
        _ => false,
    }
}

fn publish_content_addressed_temp(
    root: &Path,
    temp_path: &Path,
    final_path: &Path,
    expected_blake3: &str,
) -> Result<bool> {
    ensure_private_dir_descendant(
        root,
        final_path
            .parent()
            .ok_or_else(|| anyhow!("raw mirror blob path has no parent"))?,
    )?;
    if final_path.exists() {
        verify_existing_file(final_path, expected_blake3)?;
        remove_temp_best_effort(temp_path);
        return Ok(true);
    }

    match fs::hard_link(temp_path, final_path) {
        Ok(()) => {
            sync_file(final_path)?;
            sync_parent(final_path)?;
            remove_temp_best_effort(temp_path);
            Ok(false)
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            verify_existing_file(final_path, expected_blake3)?;
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
        verify_existing_manifest(manifest_path, blob_blake3)?;
        return Ok(true);
    }

    let temp_dir = unique_capture_temp_dir(root);
    ensure_private_dir_descendant(root, &temp_dir)?;
    let temp_path = unique_temp_path(&temp_dir, "manifest");
    let mut temp = private_create_new_file(&temp_path)?;
    temp.write_all(manifest_bytes)
        .with_context(|| format!("write raw mirror manifest temp {}", temp_path.display()))?;
    temp.sync_all()
        .with_context(|| format!("sync raw mirror manifest temp {}", temp_path.display()))?;

    match fs::hard_link(&temp_path, manifest_path) {
        Ok(()) => {
            sync_file(manifest_path)?;
            sync_parent(manifest_path)?;
            remove_temp_best_effort(&temp_path);
            remove_empty_temp_dir_best_effort(&temp_dir);
            Ok(false)
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            verify_existing_manifest(manifest_path, blob_blake3)?;
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

    let mut manifest = read_raw_mirror_manifest(manifest_path)?;
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
    temp.sync_all().with_context(|| {
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
    set_private_file_permissions(manifest_path)?;
    sync_file(manifest_path)?;
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
            "existing raw mirror blob {} has blake3 {}, expected {}",
            path.display(),
            actual,
            expected_blake3
        ))
    }
}

fn verify_existing_manifest(path: &Path, expected_blob_blake3: &str) -> Result<()> {
    let manifest = read_raw_mirror_manifest(path)?;
    if manifest.blob_blake3 == expected_blob_blake3 {
        Ok(())
    } else {
        Err(anyhow!(
            "existing raw mirror manifest {} points at blob {}, expected {}",
            path.display(),
            manifest.blob_blake3,
            expected_blob_blake3
        ))
    }
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
) -> RawMirrorBlobCacheKey {
    RawMirrorBlobCacheKey {
        data_dir: input.data_dir.to_path_buf(),
        source_path: input.source_path.to_path_buf(),
        source_identity: source_identity_token(source_metadata),
        source_size_bytes: source_metadata.len(),
        source_mtime_ns: source_metadata.modified().ok().and_then(system_time_to_ns),
        source_change_time_ns: source_change_time_ns(source_metadata),
    }
}

fn cached_raw_mirror_blob_record(
    key: &RawMirrorBlobCacheKey,
    root: &Path,
) -> Option<RawMirrorBlobRecord> {
    let cache = BLOB_CAPTURE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let record = {
        let mut guard = cache.lock().ok()?;
        let record = guard.get(key).cloned()?;
        if raw_mirror_blob_relative_path(&record.blob_blake3).is_none() {
            guard.remove(key);
            return None;
        }
        record
    };

    let blob_relative_path = raw_mirror_blob_relative_path(&record.blob_blake3)?;
    let blob_path = root.join(blob_relative_path);
    let metadata_valid = fs::symlink_metadata(&blob_path)
        .map(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
        .unwrap_or(false);
    if !metadata_valid {
        remove_cached_raw_mirror_blob_record_if_unchanged(cache, key, &record);
        return None;
    }

    match file_blake3(&blob_path) {
        Ok(actual) if actual == record.blob_blake3 => Some(record),
        Ok(actual) => {
            tracing::warn!(
                path = %blob_path.display(),
                expected_blake3 = %record.blob_blake3,
                actual_blake3 = %actual,
                "discarding raw mirror blob cache entry with mismatched content"
            );
            remove_cached_raw_mirror_blob_record_if_unchanged(cache, key, &record);
            None
        }
        Err(err) => {
            tracing::debug!(
                path = %blob_path.display(),
                error = %err,
                "discarding unreadable raw mirror blob cache entry"
            );
            remove_cached_raw_mirror_blob_record_if_unchanged(cache, key, &record);
            None
        }
    }
}

fn remove_cached_raw_mirror_blob_record_if_unchanged(
    cache: &Mutex<HashMap<RawMirrorBlobCacheKey, RawMirrorBlobRecord>>,
    key: &RawMirrorBlobCacheKey,
    stale_record: &RawMirrorBlobRecord,
) {
    if let Ok(mut guard) = cache.lock()
        && guard
            .get(key)
            .is_some_and(|current| current == stale_record)
    {
        guard.remove(key);
    }
}

fn cache_raw_mirror_blob_record(key: RawMirrorBlobCacheKey, record: RawMirrorBlobRecord) {
    let cache = BLOB_CAPTURE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(mut guard) = cache.lock() {
        guard.insert(key, record);
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
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
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

fn sync_file(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|file| file.sync_all())
        .with_context(|| format!("sync raw mirror file {}", path.display()))
}

fn sync_parent(path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    File::open(parent)
        .and_then(|file| file.sync_all())
        .with_context(|| format!("sync raw mirror parent {}", parent.display()))
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

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("set raw mirror file permissions {}", path.display()))
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &Path) -> Result<()> {
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
        }
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

        assert!(
            err.to_string().contains("unexpected blob path"),
            "error should explain the unsafe manifest inventory: {err}"
        );
        assert!(manifest_path.exists());
        assert!(blob_path.exists());
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
                apply: false,
            },
        ) {
            Ok(_) => anyhow::bail!("symlinked prune audit log was accepted"),
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
        use frankensqlite::compat::ConnectionExt as _;

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
        let conn = frankensqlite::Connection::open(db_path.to_string_lossy().into_owned())
            .expect("open keep-tag db");
        conn.execute_compat(
            "CREATE TABLE tags (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE)",
            frankensqlite::params![],
        )
        .expect("create tags");
        conn.execute_compat(
            "CREATE TABLE conversation_tags (conversation_id INTEGER NOT NULL, tag_id INTEGER NOT NULL, PRIMARY KEY (conversation_id, tag_id))",
            frankensqlite::params![],
        )
        .expect("create conversation_tags");
        conn.execute_compat(
            "INSERT INTO tags (id, name) VALUES (1, 'keep')",
            frankensqlite::params![],
        )
        .expect("insert tag");
        conn.execute_compat(
            "INSERT INTO conversation_tags (conversation_id, tag_id) VALUES (7, 1)",
            frankensqlite::params![],
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
        assert!(
            err.to_string().contains("existing raw mirror blob"),
            "unexpected cached-blob error: {err:#}"
        );
        assert_eq!(fs::read(&source_path).expect("source bytes"), source_bytes);
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
            err.to_string().contains("symlink raw mirror blob"),
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
