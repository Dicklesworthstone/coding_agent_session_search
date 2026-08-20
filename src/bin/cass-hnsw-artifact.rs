//! Build or verify the HNSW accelerator for an already-published quality FSVI.
//!
//! This deliberately does not open CASS storage, enumerate session sources, or
//! run an embedding backfill. It accepts only the durable quality artifact
//! selected by `semantic_manifest.json`, validates the newly saved native graph
//! against that exact FSVI, then atomically updates the manifest record.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use frankensearch::index::{
    HNSW_DEFAULT_EF_SEARCH, HnswConfig, HnswIndex, VectorIndex, wal_path_for,
};
use serde_json::{Value, json};

const MANIFEST_RELATIVE_PATH: &str = "vector_index/semantic_manifest.json";
const QUALITY_EMBEDDER: &str = "minilm-384";
const HNSW_RELATIVE_PATH: &str = "vector_index/hnsw-minilm-384.chsw";

fn usage() -> ! {
    eprintln!("usage: cass-hnsw-artifact <cass-data-dir> [--check]");
    std::process::exit(64);
}

fn main() -> Result<()> {
    let mut args = std::env::args_os();
    let _program = args.next();
    let Some(data_dir) = args.next() else {
        usage();
    };
    let check_only = match args.next() {
        None => false,
        Some(flag) if flag == "--check" => true,
        Some(_) => usage(),
    };
    if args.next().is_some() {
        usage();
    }

    let data_dir = std::fs::canonicalize(&data_dir).context("canonicalize CASS data dir")?;
    ensure!(data_dir.is_dir(), "CASS data dir is not a directory");
    let manifest_path = data_dir.join(MANIFEST_RELATIVE_PATH);
    let manifest = read_manifest(&manifest_path)?;
    let quality = quality_artifact(&manifest, &data_dir)?;
    let quality_index = open_quality_index(&quality.index_path)?;
    let hnsw_path = data_dir.join(HNSW_RELATIVE_PATH);

    if check_only {
        verify_published_hnsw(&manifest, &quality, &hnsw_path, &quality_index)?;
        print_result("checked", &quality, &hnsw_path)?;
        return Ok(());
    }

    let hnsw = HnswIndex::build_from_vector_index(&quality_index, HnswConfig::default())
        .map_err(|error| anyhow::anyhow!("build HNSW from published quality FSVI: {error}"))?;
    hnsw.save(&hnsw_path)
        .map_err(|error| anyhow::anyhow!("save HNSW generation: {error}"))?;

    // A completed save is not enough: prove that the strict native reader can
    // reopen the exact generation before letting the manifest advertise it.
    HnswIndex::try_load_native(&hnsw_path, &quality_index)
        .map_err(|error| anyhow::anyhow!("reopen the just-published HNSW natively: {error}"))?
        .context("native HNSW validation rejected the just-published graph")?;

    let current_manifest = read_manifest(&manifest_path)?;
    let current_quality = quality_artifact(&current_manifest, &data_dir)?;
    ensure!(
        current_quality.manifest_value == quality.manifest_value,
        "quality manifest changed while HNSW was building; graph retained but not advertised"
    );
    ensure!(
        current_manifest.get("hnsw") == manifest.get("hnsw"),
        "semantic manifest HNSW record changed while building; graph retained but not advertised"
    );

    let mut updated_manifest = current_manifest;
    let hnsw_size = std::fs::metadata(&hnsw_path)
        .with_context(|| format!("stat native HNSW metadata {}", hnsw_path.display()))?
        .len();
    updated_manifest
        .as_object_mut()
        .context("semantic manifest must be a JSON object")?
        .insert(
            "hnsw".to_owned(),
            json!({
                "base_tier": "quality",
                "embedder_id": QUALITY_EMBEDDER,
                "ef_search": HNSW_DEFAULT_EF_SEARCH,
                "index_path": HNSW_RELATIVE_PATH,
                "size_bytes": hnsw_size,
                "built_at_ms": unix_time_ms()?,
                "ready": true,
            }),
        );
    write_json_atomically(&manifest_path, &updated_manifest)?;
    print_result("built", &quality, &hnsw_path)?;
    Ok(())
}

struct QualityArtifact {
    index_path: PathBuf,
    manifest_value: Value,
}

fn read_manifest(path: &Path) -> Result<Value> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("read semantic manifest {}", path.display()))?;
    let manifest: Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse semantic manifest JSON {}", path.display()))?;
    ensure!(
        manifest.is_object(),
        "semantic manifest must be a JSON object"
    );
    Ok(manifest)
}

fn quality_artifact(manifest: &Value, data_dir: &Path) -> Result<QualityArtifact> {
    let quality = manifest
        .get("quality_tier")
        .context("semantic manifest has no quality_tier")?;
    ensure!(
        quality.get("ready").and_then(Value::as_bool) == Some(true),
        "quality tier is not ready"
    );
    ensure!(
        quality.get("tier").and_then(Value::as_str) == Some("quality"),
        "quality tier has unexpected tier marker"
    );
    ensure!(
        quality.get("embedder_id").and_then(Value::as_str) == Some(QUALITY_EMBEDDER),
        "quality tier embedder is not {QUALITY_EMBEDDER}"
    );
    ensure!(
        quality
            .get("doc_count")
            .and_then(Value::as_u64)
            .unwrap_or_default()
            > 0,
        "quality tier has zero documents"
    );
    let relative = quality
        .get("index_path")
        .and_then(Value::as_str)
        .context("quality_tier.index_path is missing or not a string")?;
    ensure!(
        Path::new(relative)
            .components()
            .all(|component| matches!(component, Component::Normal(_))),
        "quality_tier.index_path must be a relative path without traversal"
    );
    let index_path = std::fs::canonicalize(data_dir.join(relative))
        .context("canonicalize required CASS quality artifact")?;
    ensure!(
        index_path.starts_with(data_dir),
        "quality tier index path escapes the CASS data dir"
    );
    let metadata = std::fs::symlink_metadata(&index_path)
        .with_context(|| format!("stat quality vector index {}", index_path.display()))?;
    ensure!(
        metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
        "quality vector index is not a local regular file"
    );
    Ok(QualityArtifact {
        index_path,
        manifest_value: quality.clone(),
    })
}

fn open_quality_index(path: &Path) -> Result<VectorIndex> {
    let wal_path = wal_path_for(path);
    ensure!(
        !wal_path.exists(),
        "quality FSVI has a live WAL; publish an immutable quality generation before HNSW"
    );
    VectorIndex::open(path)
        .map_err(|error| anyhow::anyhow!("open quality vector index {}: {error}", path.display()))
}

fn verify_published_hnsw(
    manifest: &Value,
    quality: &QualityArtifact,
    hnsw_path: &Path,
    quality_index: &VectorIndex,
) -> Result<()> {
    let hnsw = manifest
        .get("hnsw")
        .context("semantic manifest has no HNSW record")?;
    ensure!(
        hnsw.get("ready").and_then(Value::as_bool) == Some(true),
        "published HNSW is not ready"
    );
    ensure!(
        hnsw.get("base_tier").and_then(Value::as_str) == Some("quality")
            && hnsw.get("embedder_id").and_then(Value::as_str) == Some(QUALITY_EMBEDDER)
            && hnsw.get("index_path").and_then(Value::as_str) == Some(HNSW_RELATIVE_PATH),
        "published HNSW record does not bind the selected quality artifact"
    );
    ensure!(hnsw_path.is_file(), "published HNSW metadata is missing");
    HnswIndex::try_load_native(hnsw_path, quality_index)
        .map_err(|error| anyhow::anyhow!("validate existing native HNSW: {error}"))?
        .context("native HNSW validation rejected the published graph")?;
    ensure!(
        quality
            .manifest_value
            .get("index_path")
            .and_then(Value::as_str)
            .is_some(),
        "quality artifact became invalid while checking HNSW"
    );
    Ok(())
}

fn write_json_atomically(path: &Path, value: &Value) -> Result<()> {
    let parent = path
        .parent()
        .context("semantic manifest has no parent directory")?;
    let bytes = serde_json::to_vec(value).context("serialize semantic manifest")?;
    let pid = std::process::id();
    for sequence in 0_u32..1024 {
        let candidate = parent.join(format!(
            ".semantic_manifest.hnsw-publish.{pid}.{sequence}.tmp"
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(mut file) => {
                file.write_all(&bytes).with_context(|| {
                    format!(
                        "write semantic manifest staging file {}",
                        candidate.display()
                    )
                })?;
                file.sync_all().with_context(|| {
                    format!(
                        "sync semantic manifest staging file {}",
                        candidate.display()
                    )
                })?;
                drop(file);
                std::fs::rename(&candidate, path).with_context(|| {
                    format!(
                        "atomically publish semantic manifest {} from {}",
                        path.display(),
                        candidate.display()
                    )
                })?;
                File::open(parent)
                    .with_context(|| format!("open semantic manifest parent {}", parent.display()))?
                    .sync_all()
                    .with_context(|| {
                        format!("sync semantic manifest parent {}", parent.display())
                    })?;
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "unable to allocate a unique semantic manifest staging file in {}",
                        parent.display()
                    )
                });
            }
        }
    }
    bail!("too many temporary semantic manifest staging files")
}

fn unix_time_ms() -> Result<i64> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("current time precedes the Unix epoch")?
        .as_millis();
    i64::try_from(millis).context("current time does not fit i64 milliseconds")
}

fn print_result(action: &str, quality: &QualityArtifact, hnsw_path: &Path) -> Result<()> {
    let hnsw_size = std::fs::metadata(hnsw_path)
        .with_context(|| format!("stat HNSW metadata {}", hnsw_path.display()))?
        .len();
    println!(
        "{}",
        serde_json::to_string(&json!({
            "success": true,
            "action": action,
            "quality_vector_path": quality.index_path,
            "hnsw_metadata_path": hnsw_path,
            "hnsw_metadata_bytes": hnsw_size,
        }))?
    );
    Ok(())
}
