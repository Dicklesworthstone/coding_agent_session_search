//! Real-binary gate for bead `coding_agent_session_search-uaulb` (gh#408):
//! `cass models build-hnsw` builds / verifies the HNSW accelerator for an
//! already-published semantic artifact without opening the archive or
//! embedding, records it in the semantic manifest, and `--check` never
//! writes.

mod util;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, SystemTime};

use assert_cmd::cargo::cargo_bin;
use coding_agent_search::search::semantic_manifest::{HnswRecord, SemanticManifest, TierKind};
use frankensearch::index::{HnswConfig, HnswIndex, VectorIndex};
use serde_json::Value;

use util::timeout::spawn_with_timeout_or_diag;

const INDEX_TIMEOUT: Duration = Duration::from_secs(180);
const MODELS_TIMEOUT: Duration = Duration::from_secs(120);
const KEYWORD: &str = "build-hnsw-gate-keyword-qwer";

struct Fixture {
    _home: tempfile::TempDir,
    home: PathBuf,
    data_dir: PathBuf,
    codex_home: PathBuf,
}

fn run(fixture: &Fixture, label: &str, args: &[&str], timeout: Duration) -> Output {
    let mut cmd = Command::new(cargo_bin("cass"));
    cmd.args(args)
        .arg("--data-dir")
        .arg(&fixture.data_dir)
        .current_dir(&fixture.home)
        .env("HOME", &fixture.home)
        .env("XDG_DATA_HOME", fixture.home.join("xdg-data"))
        .env("XDG_CONFIG_HOME", fixture.home.join("xdg-config"))
        .env("XDG_CACHE_HOME", fixture.home.join("xdg-cache"))
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_SEMANTIC_EMBEDDER", "hash")
        .env("CODEX_HOME", &fixture.codex_home)
        .env("NO_COLOR", "1")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("RUST_LOG");
    spawn_with_timeout_or_diag(cmd, label, Some(&fixture.data_dir), timeout)
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn json_stdout(out: &Output, label: &str) -> Result<Value, String> {
    serde_json::from_str(text(&out.stdout).trim()).map_err(|e| {
        format!(
            "{label}: stdout not JSON: {e}; stdout: {}; stderr: {}",
            text(&out.stdout),
            text(&out.stderr)
        )
    })
}

fn str_at<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn semantic_fixture() -> Result<Fixture, String> {
    let home = tempfile::tempdir().map_err(|e| format!("tempdir: {e}"))?;
    let home_path = home.path().to_path_buf();
    let data_dir = home_path.join("cass-data");
    std::fs::create_dir_all(&data_dir).map_err(|e| format!("data dir: {e}"))?;
    let codex_home = home_path.join(".codex");
    util::seed_codex_session(
        &codex_home,
        "rollout-2026-04-23T10-00-00-hnsw.jsonl",
        KEYWORD,
        true,
    );
    let fixture = Fixture {
        _home: home,
        home: home_path,
        data_dir,
        codex_home,
    };
    let out = run(
        &fixture,
        "build_hnsw_gate_index",
        &[
            "index",
            "--full",
            "--semantic",
            "--embedder",
            "hash",
            "--json",
            "--no-progress-events",
        ],
        INDEX_TIMEOUT,
    );
    let value = json_stdout(&out, "index")?;
    if value
        .get("success")
        .and_then(Value::as_bool)
        .is_none_or(|ok| !ok)
    {
        return Err(format!("semantic index did not succeed: {value}"));
    }
    Ok(fixture)
}

fn manifest_hnsw(data_dir: &Path) -> Result<Option<Value>, String> {
    let path = data_dir.join("vector_index").join("semantic_manifest.json");
    let bytes = std::fs::read(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let manifest: Value =
        serde_json::from_slice(&bytes).map_err(|e| format!("parse manifest: {e}"))?;
    Ok(manifest.get("hnsw").filter(|v| !v.is_null()).cloned())
}

fn build_hnsw(fixture: &Fixture, label: &str, extra: &[&str]) -> Result<(Output, Value), String> {
    let mut args = vec!["models", "build-hnsw", "--json"];
    args.extend_from_slice(extra);
    let out = run(fixture, label, &args, MODELS_TIMEOUT);
    let value = json_stdout(&out, label)?;
    Ok((out, value))
}

fn load_manifest(data_dir: &Path) -> Result<SemanticManifest, String> {
    SemanticManifest::load(data_dir)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "fixture semantic manifest is missing".to_string())
}

fn file_snapshot(root: &Path) -> Result<BTreeMap<PathBuf, (Vec<u8>, SystemTime)>, String> {
    let mut files = BTreeMap::new();
    for entry in walkdir::WalkDir::new(root) {
        let entry = entry.map_err(|error| error.to_string())?;
        if entry.file_type().is_file() {
            files.insert(
                entry
                    .path()
                    .strip_prefix(root)
                    .map_err(|error| error.to_string())?
                    .to_path_buf(),
                (
                    std::fs::read(entry.path()).map_err(|error| error.to_string())?,
                    entry
                        .metadata()
                        .map_err(|error| error.to_string())?
                        .modified()
                        .map_err(|error| error.to_string())?,
                ),
            );
        }
    }
    Ok(files)
}

/// Publish a real native graph at a nonconventional manifest-selected path.
fn declare_native_graph(fixture: &Fixture, relative: &str) -> Result<(PathBuf, PathBuf), String> {
    let mut manifest = load_manifest(&fixture.data_dir)?;
    let artifact = manifest
        .fast_tier
        .as_ref()
        .filter(|artifact| artifact.ready)
        .ok_or_else(|| "fixture has no published fast artifact".to_string())?
        .clone();
    let vector_path = fixture.data_dir.join(&artifact.index_path);
    let vectors = VectorIndex::open_read_only(&vector_path).map_err(|error| error.to_string())?;
    let graph_path = fixture.data_dir.join(relative);
    let parent = graph_path
        .parent()
        .ok_or_else(|| "fixture graph has no parent".to_string())?;
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    HnswIndex::build_from_vector_index(&vectors, HnswConfig::default())
        .and_then(|graph| graph.save(&graph_path))
        .map_err(|error| error.to_string())?;
    manifest.publish_hnsw(HnswRecord {
        base_tier: TierKind::Fast,
        embedder_id: artifact.embedder_id,
        ef_search: frankensearch::index::HNSW_DEFAULT_EF_SEARCH,
        index_path: relative.to_string(),
        size_bytes: std::fs::metadata(&graph_path)
            .map_err(|error| error.to_string())?
            .len(),
        built_at_ms: chrono::Utc::now().timestamp_millis(),
        ready: true,
    });
    manifest
        .save(&fixture.data_dir)
        .map_err(|error| error.to_string())?;
    Ok((vector_path, graph_path))
}

fn check() -> Result<(), String> {
    let fixture = semantic_fixture()?;
    let mut failures = Vec::new();

    // 1. `--check` on a fresh artifact: no accelerator yet, nothing written.
    let (out, before) = build_hnsw(&fixture, "build_hnsw_check_missing", &["--check"])?;
    if !out.status.success() {
        failures.push(format!("--check exited non-zero: {}", text(&out.stderr)));
    }
    if str_at(&before, "state_before").cmp("missing").is_ne()
        || str_at(&before, "action").cmp("check_only").is_ne()
        || before
            .get("current")
            .and_then(Value::as_bool)
            .is_none_or(|c| c)
    {
        failures.push(format!(
            "check on a fresh artifact should report missing: {before}"
        ));
    }
    if str_at(&before, "next_command")
        .cmp("cass models build-hnsw")
        .is_ne()
    {
        failures.push(format!("check should name the build command: {before}"));
    }
    if manifest_hnsw(&fixture.data_dir)?.is_some()
        || before
            .get("manifest_recorded")
            .and_then(Value::as_bool)
            .is_none_or(|r| r)
    {
        failures.push("--check must not record an accelerator in the manifest".to_string());
    }
    let hnsw_path = fixture.data_dir.join(str_at(&before, "hnsw_path"));
    if hnsw_path.exists() {
        failures.push(format!("--check must not write {}", hnsw_path.display()));
    }
    let archive_before = std::fs::read(fixture.data_dir.join("agent_search.db"))
        .map_err(|e| format!("read archive: {e}"))?;

    // 2. Build: the accelerator appears, loads back as native, is recorded.
    let (out, built) = build_hnsw(&fixture, "build_hnsw_build", &[])?;
    if !out.status.success() {
        failures.push(format!("build exited non-zero: {}", text(&out.stderr)));
    }
    if str_at(&built, "action").cmp("rebuilt").is_ne()
        || str_at(&built, "state_after").cmp("native_valid").is_ne()
        || built
            .get("manifest_published")
            .and_then(Value::as_bool)
            .is_none_or(|p| !p)
        || built
            .get("current")
            .and_then(Value::as_bool)
            .is_none_or(|c| !c)
        || built
            .get("manifest_recorded")
            .and_then(Value::as_bool)
            .is_none_or(|r| !r)
    {
        failures.push(format!(
            "build should rebuild to native_valid and publish: {built}"
        ));
    }
    if built
        .get("vector_count")
        .and_then(Value::as_u64)
        .is_none_or(|n| n < 1)
    {
        failures.push(format!("published artifact should carry vectors: {built}"));
    }
    if !hnsw_path.is_file() {
        failures.push(format!(
            "accelerator metadata missing at {}",
            hnsw_path.display()
        ));
    }
    match manifest_hnsw(&fixture.data_dir)? {
        Some(record) => {
            if record
                .get("ready")
                .and_then(Value::as_bool)
                .is_none_or(|r| !r)
                || str_at(&record, "embedder_id")
                    .cmp(str_at(&built, "embedder_id"))
                    .is_ne()
                || str_at(&record, "base_tier")
                    .cmp(str_at(&built, "tier"))
                    .is_ne()
            {
                failures.push(format!(
                    "manifest hnsw record disagrees with the report: {record}"
                ));
            }
        }
        None => failures.push("build must record the accelerator in the manifest".to_string()),
    }
    let archive_after = std::fs::read(fixture.data_dir.join("agent_search.db"))
        .map_err(|e| format!("read archive: {e}"))?;
    if archive_after.cmp(&archive_before).is_ne() {
        failures.push("build-hnsw must never modify the canonical archive".to_string());
    }

    // 3. Re-run without --force: current graph is kept, nothing rebuilt.
    let (_, again) = build_hnsw(&fixture, "build_hnsw_again", &[])?;
    if str_at(&again, "action").cmp("unchanged").is_ne()
        || str_at(&again, "state_before").cmp("native_valid").is_ne()
    {
        failures.push(format!(
            "second build should be a no-op on a current graph: {again}"
        ));
    }

    // 4. `--check` now proves the native graph; `--force` rebuilds anyway.
    let (_, verified) = build_hnsw(&fixture, "build_hnsw_check_valid", &["--check"])?;
    if str_at(&verified, "state_before")
        .cmp("native_valid")
        .is_ne()
        || verified
            .get("current")
            .and_then(Value::as_bool)
            .is_none_or(|c| !c)
    {
        failures.push(format!(
            "check after build should be native_valid: {verified}"
        ));
    }
    let (_, forced) = build_hnsw(&fixture, "build_hnsw_force", &["--force"])?;
    if str_at(&forced, "action").cmp("rebuilt").is_ne()
        || str_at(&forced, "state_after").cmp("native_valid").is_ne()
    {
        failures.push(format!("--force should rebuild to native_valid: {forced}"));
    }

    // 5. An unpublished tier is refused with the missing-index envelope.
    let out = run(
        &fixture,
        "build_hnsw_quality_missing",
        &["models", "build-hnsw", "--json", "--tier", "quality"],
        MODELS_TIMEOUT,
    );
    if out.status.success() {
        failures.push("build-hnsw on an unpublished tier must fail".to_string());
    }
    let combined = format!("{}{}", text(&out.stdout), text(&out.stderr));
    if !combined.contains("no published quality semantic artifact") {
        failures.push(format!(
            "unpublished tier error should name the tier: {combined}"
        ));
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("\n"))
    }
}

#[test]
fn models_build_hnsw_builds_verifies_and_records_accelerator() -> Result<(), String> {
    check()
}

#[test]
fn models_hnsw_maintenance_honors_declared_graph_path() -> Result<(), String> {
    let fixture = semantic_fixture()?;
    let (out, conventional) = build_hnsw(&fixture, "declared_hnsw_conventional_control", &[])?;
    if !out.status.success() || str_at(&conventional, "state_after") != "native_valid" {
        return Err(format!(
            "conventional native control failed: {conventional}"
        ));
    }
    let conventional_path = fixture.data_dir.join(str_at(&conventional, "hnsw_path"));
    let conventional_bytes =
        std::fs::read(&conventional_path).map_err(|error| error.to_string())?;
    let selected_relative = "vector_index/alternate-accelerators/selected.chsw";
    let (vector_path, selected_path) = declare_native_graph(&fixture, selected_relative)?;
    let vector_bytes = std::fs::read(&vector_path).map_err(|error| error.to_string())?;
    let before = file_snapshot(&fixture.data_dir.join("vector_index"))?;

    let (out, inspected) = build_hnsw(&fixture, "declared_hnsw_check", &["--check"])?;
    if !out.status.success()
        || str_at(&inspected, "hnsw_path") != selected_relative
        || str_at(&inspected, "state_before") != "native_valid"
        || inspected["manifest_recorded"] != true
        || file_snapshot(&fixture.data_dir.join("vector_index"))? != before
    {
        return Err(format!(
            "--check must inspect the selected graph without writes: {inspected}"
        ));
    }
    let (out, unchanged) = build_hnsw(&fixture, "declared_hnsw_unchanged", &[])?;
    if !out.status.success()
        || str_at(&unchanged, "hnsw_path") != selected_relative
        || str_at(&unchanged, "action") != "unchanged"
        || unchanged["manifest_published"] != false
        || file_snapshot(&fixture.data_dir.join("vector_index"))? != before
    {
        return Err(format!(
            "current selected graph must remain untouched: {unchanged}"
        ));
    }

    // The index entry point must use the same path authority, including when
    // no embedding work is needed and its HNSW maintenance runs separately.
    let selected_before = std::fs::read(&selected_path).map_err(|error| error.to_string())?;
    let record_before = manifest_hnsw(&fixture.data_dir)?;
    let out = run(
        &fixture,
        "declared_hnsw_index_unchanged",
        &[
            "index",
            "--semantic",
            "--build-hnsw",
            "--embedder",
            "hash",
            "--json",
            "--no-progress-events",
        ],
        INDEX_TIMEOUT,
    );
    if !out.status.success()
        || manifest_hnsw(&fixture.data_dir)? != record_before
        || std::fs::read(&selected_path).map_err(|error| error.to_string())? != selected_before
    {
        return Err(format!(
            "index maintenance changed the declared graph: {} {}",
            text(&out.stdout),
            text(&out.stderr)
        ));
    }

    let (out, forced) = build_hnsw(&fixture, "declared_hnsw_force", &["--force"])?;
    if !out.status.success()
        || str_at(&forced, "hnsw_path") != selected_relative
        || str_at(&forced, "action") != "rebuilt"
        || str_at(&forced, "state_after") != "native_valid"
    {
        return Err(format!(
            "--force must replace the selected native generation: {forced}"
        ));
    }

    // A valid conventional graph must not hide damage to the selected one.
    std::fs::write(&selected_path, b"truncated selected native metadata")
        .map_err(|error| error.to_string())?;
    let corrupted = file_snapshot(&fixture.data_dir.join("vector_index"))?;
    let (out, inspected) = build_hnsw(&fixture, "declared_hnsw_check_corrupt", &["--check"])?;
    if !out.status.success()
        || str_at(&inspected, "hnsw_path") != selected_relative
        || inspected["current"] != false
        || file_snapshot(&fixture.data_dir.join("vector_index"))? != corrupted
    {
        return Err(format!(
            "--check inspected an undeclared graph or repaired damage: {inspected}"
        ));
    }
    let (out, repaired) = build_hnsw(&fixture, "declared_hnsw_repair", &[])?;
    if !out.status.success()
        || str_at(&repaired, "hnsw_path") != selected_relative
        || str_at(&repaired, "action") != "rebuilt"
        || str_at(&repaired, "state_after") != "native_valid"
    {
        return Err(format!(
            "repair must retain the selected graph path: {repaired}"
        ));
    }

    // A revoked declaration still owns its path. Explicit maintenance may
    // re-record the verified graph, while --check leaves revocation intact.
    let mut manifest = load_manifest(&fixture.data_dir)?;
    manifest
        .hnsw
        .as_mut()
        .ok_or("missing selected graph record")?
        .ready = false;
    manifest
        .save(&fixture.data_dir)
        .map_err(|error| error.to_string())?;
    let revoked = file_snapshot(&fixture.data_dir.join("vector_index"))?;
    let (out, inspected) = build_hnsw(&fixture, "declared_hnsw_revoked_check", &["--check"])?;
    if !out.status.success()
        || str_at(&inspected, "hnsw_path") != selected_relative
        || inspected["current"] != true
        || inspected["manifest_recorded"] != false
        || file_snapshot(&fixture.data_dir.join("vector_index"))? != revoked
    {
        return Err(format!(
            "revocation must not redirect read-only inspection: {inspected}"
        ));
    }
    let graph_before = std::fs::read(&selected_path).map_err(|error| error.to_string())?;
    let (out, restored) = build_hnsw(&fixture, "declared_hnsw_restore", &[])?;
    if !out.status.success()
        || str_at(&restored, "hnsw_path") != selected_relative
        || str_at(&restored, "action") != "unchanged"
        || restored["manifest_published"] != true
        || std::fs::read(&selected_path).map_err(|error| error.to_string())? != graph_before
        || std::fs::read(&conventional_path).map_err(|error| error.to_string())?
            != conventional_bytes
        || std::fs::read(&vector_path).map_err(|error| error.to_string())? != vector_bytes
    {
        return Err(format!(
            "explicit re-recording must preserve native and source bytes: {restored}"
        ));
    }
    let missing_relative = "vector_index/newly-declared/nested/native.chsw";
    let missing_path = fixture.data_dir.join(missing_relative);
    let missing_parent = missing_path.parent().ok_or("missing graph parent")?;
    let mut manifest = load_manifest(&fixture.data_dir)?;
    manifest
        .hnsw
        .as_mut()
        .ok_or("missing selected graph record")?
        .index_path = missing_relative.to_string();
    manifest
        .save(&fixture.data_dir)
        .map_err(|error| error.to_string())?;
    let before_missing_check = file_snapshot(&fixture.data_dir.join("vector_index"))?;
    let (out, missing) = build_hnsw(&fixture, "declared_hnsw_missing_check", &["--check"])?;
    if !out.status.success()
        || str_at(&missing, "hnsw_path") != missing_relative
        || str_at(&missing, "state_before") != "missing"
        || missing_parent.exists()
        || file_snapshot(&fixture.data_dir.join("vector_index"))? != before_missing_check
    {
        return Err(format!(
            "checking a missing declared graph must not create its directory or inspect a decoy: {missing}"
        ));
    }
    let (out, created) = build_hnsw(&fixture, "declared_hnsw_missing_create", &[])?;
    if !out.status.success()
        || str_at(&created, "hnsw_path") != missing_relative
        || str_at(&created, "state_after") != "native_valid"
        || !missing_path.is_file()
        || std::fs::read(&conventional_path).map_err(|error| error.to_string())?
            != conventional_bytes
        || std::fs::read(&vector_path).map_err(|error| error.to_string())? != vector_bytes
    {
        return Err(format!(
            "explicit maintenance must create the missing declared graph: {created}"
        ));
    }
    let out = run(
        &fixture,
        "declared_hnsw_native_search",
        &[
            "search",
            KEYWORD,
            "--mode",
            "semantic",
            "--model",
            "hash",
            "--approximate",
            "--robot",
            "--robot-meta",
            "--limit",
            "5",
        ],
        MODELS_TIMEOUT,
    );
    let results = json_stdout(&out, "declared native search")?;
    if !out.status.success()
        || results
            .pointer("/_meta/ann_stats")
            .is_none_or(Value::is_null)
        || results.pointer("/_meta/ann_unavailable_reason").is_some()
    {
        return Err(format!(
            "maintained selected graph must serve native search: {results}"
        ));
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn models_hnsw_maintenance_refuses_unsafe_or_colliding_declarations() -> Result<(), String> {
    let fixture = semantic_fixture()?;
    let (vector_path, selected_path) =
        declare_native_graph(&fixture, "vector_index/selected.chsw")?;
    let manifest = load_manifest(&fixture.data_dir)?;
    let vector_relative = manifest
        .fast_tier
        .as_ref()
        .ok_or("missing fast artifact")?
        .index_path
        .clone();
    let certificate = manifest
        .coverage
        .values()
        .next()
        .ok_or("missing coverage certificate")?;
    let hardlink_relative = "vector_index/aliased-vector.chsw";
    std::fs::hard_link(&vector_path, fixture.data_dir.join(hardlink_relative))
        .map_err(|error| error.to_string())?;
    let leaf_alias = "vector_index/aliased-graph.chsw";
    std::os::unix::fs::symlink(&selected_path, fixture.data_dir.join(leaf_alias))
        .map_err(|error| error.to_string())?;
    let outside = fixture.home.join("outside-native");
    std::fs::create_dir_all(&outside).map_err(|error| error.to_string())?;
    let vectors = VectorIndex::open_read_only(&vector_path).map_err(|error| error.to_string())?;
    HnswIndex::build_from_vector_index(&vectors, HnswConfig::default())
        .and_then(|graph| graph.save(&outside.join("selected.chsw")))
        .map_err(|error| error.to_string())?;
    std::os::unix::fs::symlink(
        &outside,
        fixture.data_dir.join("vector_index/aliased-parent"),
    )
    .map_err(|error| error.to_string())?;
    let title_root = fixture
        .data_dir
        .join("vector_index/titles/another-producer");
    std::fs::create_dir_all(&title_root).map_err(|error| error.to_string())?;
    // The alias is not among the body's registered identities. Its owner can
    // be an independent title publication, so all hardlinked outputs are refused.
    std::fs::hard_link(&selected_path, title_root.join("current.json"))
        .map_err(|error| error.to_string())?;
    // Neither file is selected by the mutable ledger. Their immutable parent
    // namespaces still own them: an old proof and an actual native ANN graph
    // must not become mutable maintenance outputs through a new declaration.
    let old_proof_relative = "vector_index/served/unselected-proof.parents";
    std::fs::copy(
        fixture.data_dir.join(&certificate.parent_path),
        fixture.data_dir.join(old_proof_relative),
    )
    .map_err(|error| error.to_string())?;
    let old_generation_relative = "vector_index/generations/old-unselected/fast/native.chsw";
    let old_generation_path = fixture.data_dir.join(old_generation_relative);
    std::fs::create_dir_all(
        old_generation_path
            .parent()
            .ok_or("old generation parent")?,
    )
    .map_err(|error| error.to_string())?;
    HnswIndex::build_from_vector_index(&vectors, HnswConfig::default())
        .and_then(|graph| graph.save(&old_generation_path))
        .map_err(|error| error.to_string())?;
    let db_path = fixture.data_dir.join("agent_search.db");
    let archive_before = std::fs::read(&db_path).map_err(|error| error.to_string())?;
    let outside_before = file_snapshot(&outside)?;
    for relative in [
        "../outside-native/selected.chsw".to_string(),
        selected_path.to_string_lossy().into_owned(),
        vector_relative,
        certificate.parent_path.clone(),
        "agent_search.db".to_string(),
        "Agent_Search.DB".to_string(),
        "vector_index/semantic_manifest.json".to_string(),
        hardlink_relative.to_string(),
        "vector_index/selected.chsw".to_string(),
        "vector_index/titles/new-producer/new-graph.chsw".to_string(),
        "Vector_Index/Titles/new-producer/new-graph.chsw".to_string(),
        "vector_index/titles/another-producer/current.json".to_string(),
        old_proof_relative.to_string(),
        old_generation_relative.to_string(),
        "Vector_Index/Served/unselected-proof.parents".to_string(),
        "Vector_Index/Generations/old-unselected/fast/native.chsw".to_string(),
        leaf_alias.to_string(),
        "vector_index/aliased-parent/selected.chsw".to_string(),
    ] {
        let mut refused = manifest.clone();
        refused
            .hnsw
            .as_mut()
            .ok_or("missing graph record")?
            .index_path = relative.clone();
        refused
            .save(&fixture.data_dir)
            .map_err(|error| error.to_string())?;
        let before = file_snapshot(&fixture.data_dir.join("vector_index"))?;
        for args in [
            vec!["models", "build-hnsw", "--json", "--check"],
            vec!["models", "build-hnsw", "--json", "--force"],
        ] {
            let out = run(
                &fixture,
                "declared_hnsw_refused_path",
                &args,
                MODELS_TIMEOUT,
            );
            let rendered = format!("{}{}", text(&out.stdout), text(&out.stderr));
            if out.status.code() != Some(5)
                || !rendered.contains("HNSW accelerator")
                || file_snapshot(&fixture.data_dir.join("vector_index"))? != before
                || std::fs::read(&db_path).map_err(|error| error.to_string())? != archive_before
                || file_snapshot(&outside)? != outside_before
            {
                return Err(format!(
                    "unsafe declaration {relative:?} must refuse without altering archive or artifacts: {rendered}"
                ));
            }
        }
    }
    // Global --db selects the canonical archive even for this database-free
    // command. A manifest must not turn that alternate archive into ANN output.
    let custom_db = fixture.data_dir.join("custom-history.sqlite3");
    std::fs::copy(&db_path, &custom_db).map_err(|error| error.to_string())?;
    let custom_db_arg = custom_db
        .to_str()
        .ok_or("custom database path is not UTF-8")?;
    let mut refused = manifest.clone();
    refused
        .hnsw
        .as_mut()
        .ok_or("missing graph record")?
        .index_path = "custom-history.sqlite3".to_string();
    refused
        .save(&fixture.data_dir)
        .map_err(|error| error.to_string())?;
    let before = file_snapshot(&fixture.data_dir.join("vector_index"))?;
    for mode in ["--check", "--force"] {
        let out = run(
            &fixture,
            "declared_hnsw_refused_custom_archive",
            &[
                "--db",
                custom_db_arg,
                "models",
                "build-hnsw",
                "--json",
                mode,
            ],
            MODELS_TIMEOUT,
        );
        if out.status.code() != Some(5)
            || std::fs::read(&custom_db).map_err(|error| error.to_string())? != archive_before
            || std::fs::read(&db_path).map_err(|error| error.to_string())? != archive_before
            || file_snapshot(&fixture.data_dir.join("vector_index"))? != before
        {
            return Err(format!(
                "custom canonical archive must not become HNSW output: {} {}",
                text(&out.stdout),
                text(&out.stderr)
            ));
        }
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn models_hnsw_maintenance_reserves_missing_archive_aliases() -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;

    let fixture = semantic_fixture()?;
    declare_native_graph(&fixture, "vector_index/existing-selected.chsw")?;
    let writer_lease_path = fixture.data_dir.join("agent_search.db.cass-index-run.lock");
    let writer_lease = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&writer_lease_path)
        .map_err(|error| error.to_string())?;
    writer_lease.lock().map_err(|error| error.to_string())?;
    let lease_metadata = writer_lease.metadata().map_err(|error| error.to_string())?;
    let assert_writer_lease_held = || -> Result<(), String> {
        let contender = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&writer_lease_path)
            .map_err(|error| error.to_string())?;
        let metadata = contender.metadata().map_err(|error| error.to_string())?;
        if (metadata.dev(), metadata.ino()) != (lease_metadata.dev(), lease_metadata.ino()) {
            return Err("HNSW maintenance replaced the held archive writer lease inode".into());
        }
        match contender.try_lock() {
            Err(std::fs::TryLockError::WouldBlock) => Ok(()),
            Err(std::fs::TryLockError::Error(error)) => Err(error.to_string()),
            Ok(()) => Err("a competing handle acquired the still-held archive writer lease".into()),
        }
    };
    assert_writer_lease_held()?;
    let unrelated_db = fixture.home.join("unrelated-uncreated.sqlite3");
    let unrelated_arg = unrelated_db.to_str().ok_or("non-UTF-8 control path")?;
    let control = run(
        &fixture,
        "declared_hnsw_unrelated_missing_archive_control",
        &[
            "--db",
            unrelated_arg,
            "models",
            "build-hnsw",
            "--json",
            "--check",
        ],
        MODELS_TIMEOUT,
    );
    let control_json = json_stdout(&control, "unrelated missing archive control")?;
    if !control.status.success()
        || str_at(&control_json, "state_before") != "native_valid"
        || unrelated_db.exists()
    {
        return Err(format!(
            "unrelated missing archive must permit read-only checking: {control_json}"
        ));
    }

    let parent_alias = fixture.home.join("archive-parent-alias");
    std::os::unix::fs::symlink(&fixture.data_dir, &parent_alias)
        .map_err(|error| error.to_string())?;
    let filename = "uncreated-custom-archive.sqlite3";
    let reserved = fixture.data_dir.join(filename);
    let absolute_alias = fixture.home.join("archive-absolute-link.sqlite3");
    std::os::unix::fs::symlink(&reserved, &absolute_alias).map_err(|error| error.to_string())?;
    let relative_alias = fixture.home.join("archive-relative-link.sqlite3");
    std::os::unix::fs::symlink(Path::new("cass-data").join(filename), &relative_alias)
        .map_err(|error| error.to_string())?;
    let chain_alias = fixture.home.join("archive-link-chain.sqlite3");
    std::os::unix::fs::symlink(&relative_alias, &chain_alias).map_err(|error| error.to_string())?;

    let links_before = [
        &parent_alias,
        &absolute_alias,
        &relative_alias,
        &chain_alias,
    ]
    .into_iter()
    .map(|path| {
        std::fs::read_link(path)
            .map(|target| (path.clone(), target))
            .map_err(|error| error.to_string())
    })
    .collect::<Result<Vec<_>, String>>()?;
    let archive_snapshot = || -> Result<Vec<Option<Vec<u8>>>, String> {
        [
            "agent_search.db",
            "agent_search.db-wal",
            "agent_search.db-shm",
            "agent_search.db-journal",
            "agent_search.db.cass-index-run.lock",
        ]
        .into_iter()
        .map(|name| match std::fs::read(fixture.data_dir.join(name)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.to_string()),
        })
        .collect()
    };
    let archive_before = archive_snapshot()?;
    let custom_archives = [
        parent_alias.join(filename),
        absolute_alias,
        relative_alias,
        chain_alias,
        fixture
            .home
            .join("not-created/../archive-relative-link.sqlite3"),
    ];
    for suffix in ["", "-wal", "-shm", "-journal", ".cass-index-run.lock"] {
        let output_relative = format!("{filename}{suffix}");
        let protected_output = fixture.data_dir.join(&output_relative);
        let mut manifest = load_manifest(&fixture.data_dir)?;
        manifest
            .hnsw
            .as_mut()
            .ok_or("missing graph record")?
            .index_path = output_relative;
        manifest
            .save(&fixture.data_dir)
            .map_err(|error| error.to_string())?;
        let semantic_before = file_snapshot(&fixture.data_dir.join("vector_index"))?;
        for custom_archive in &custom_archives {
            let custom_arg = custom_archive.to_str().ok_or("non-UTF-8 archive alias")?;
            for mode in ["--check", "--force"] {
                let out = run(
                    &fixture,
                    "declared_hnsw_refused_missing_archive_alias",
                    &["--db", custom_arg, "models", "build-hnsw", "--json", mode],
                    MODELS_TIMEOUT,
                );
                let rendered = format!("{}{}", text(&out.stdout), text(&out.stderr));
                if out.status.code() != Some(5)
                    || !rendered.contains("collides with protected archive data")
                    || protected_output
                        .try_exists()
                        .map_err(|error| error.to_string())?
                    || reserved.try_exists().map_err(|error| error.to_string())?
                    || file_snapshot(&fixture.data_dir.join("vector_index"))? != semantic_before
                    || archive_snapshot()? != archive_before
                {
                    return Err(format!(
                        "{mode} for suffix {suffix:?} via {custom_archive:?} must preserve reserved archive and semantic artifacts: {rendered}"
                    ));
                }
                for (path, target) in &links_before {
                    if std::fs::read_link(path).map_err(|error| error.to_string())? != *target {
                        return Err(format!("archive alias changed: {path:?}"));
                    }
                }
            }
        }
    }
    // Existing archive sidecars must also survive an alternate database
    // filename. A lexical suffix on the alias does not name the target WAL.
    let existing_target = fixture.data_dir.join("agent_search.db");
    let existing_alias = fixture.home.join("existing-archive-link.sqlite3");
    std::os::unix::fs::symlink(&existing_target, &existing_alias)
        .map_err(|error| error.to_string())?;
    let existing_arg = existing_alias
        .to_str()
        .ok_or("non-UTF-8 existing archive alias")?;
    for suffix in ["", "-wal", "-shm", "-journal", ".cass-index-run.lock"] {
        let mut manifest = load_manifest(&fixture.data_dir)?;
        manifest
            .hnsw
            .as_mut()
            .ok_or("missing graph record")?
            .index_path = format!("agent_search.db{suffix}");
        manifest
            .save(&fixture.data_dir)
            .map_err(|error| error.to_string())?;
        let semantic_before = file_snapshot(&fixture.data_dir.join("vector_index"))?;
        for mode in ["--check", "--force"] {
            let out = run(
                &fixture,
                "declared_hnsw_refused_existing_archive_alias",
                &["--db", existing_arg, "models", "build-hnsw", "--json", mode],
                MODELS_TIMEOUT,
            );
            let rendered = format!("{}{}", text(&out.stdout), text(&out.stderr));
            if out.status.code() != Some(5)
                || file_snapshot(&fixture.data_dir.join("vector_index"))? != semantic_before
                || archive_snapshot()? != archive_before
                || std::fs::read_link(&existing_alias).map_err(|error| error.to_string())?
                    != existing_target
            {
                return Err(format!(
                    "{mode} must preserve existing archive and sidecar {suffix:?} through its alias: {rendered}"
                ));
            }
        }
    }
    assert_writer_lease_held()?;
    drop(writer_lease);
    let after_release = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&writer_lease_path)
        .map_err(|error| error.to_string())?;
    after_release
        .try_lock()
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[test]
fn index_build_hnsw_maintains_already_published_vectors() -> Result<(), String> {
    let fixture = semantic_fixture()?;
    let (out, missing) = build_hnsw(&fixture, "index_hnsw_before", &["--check"])?;
    if !out.status.success() || str_at(&missing, "state_before") != "missing" {
        return Err(format!(
            "fixture must start without an accelerator: {missing}"
        ));
    }
    let vector_path = fixture.data_dir.join(str_at(&missing, "artifact_path"));
    let vectors_before = std::fs::read(&vector_path).map_err(|e| e.to_string())?;
    let index_args = [
        "index",
        "--semantic",
        "--build-hnsw",
        "--embedder",
        "hash",
        "--json",
        "--no-progress-events",
    ];
    let out = run(
        &fixture,
        "index_hnsw_existing_vectors",
        &index_args,
        INDEX_TIMEOUT,
    );
    if !out.status.success() {
        return Err(format!(
            "index graph maintenance failed: {}",
            text(&out.stderr)
        ));
    }
    if std::fs::read(&vector_path).map_err(|e| e.to_string())? != vectors_before {
        return Err("graph maintenance must not rewrite already-current vectors".into());
    }
    let (out, built) = build_hnsw(&fixture, "index_hnsw_admission", &["--check"])?;
    if !out.status.success()
        || str_at(&built, "state_before") != "native_valid"
        || built.get("manifest_recorded").and_then(Value::as_bool) != Some(true)
    {
        return Err(format!(
            "index must publish a natively admitted graph: {built}"
        ));
    }
    let graph_path = fixture.data_dir.join(str_at(&built, "hnsw_path"));
    let graph_before = std::fs::read(&graph_path).map_err(|e| e.to_string())?;
    let modified_before = std::fs::metadata(&graph_path)
        .and_then(|metadata| metadata.modified())
        .map_err(|e| e.to_string())?;
    let manifest_before = manifest_hnsw(&fixture.data_dir)?;

    let out = run(
        &fixture,
        "index_hnsw_current_noop",
        &index_args,
        INDEX_TIMEOUT,
    );
    if !out.status.success() {
        return Err(format!("repeated index failed: {}", text(&out.stderr)));
    }
    if std::fs::read(&graph_path).map_err(|e| e.to_string())? != graph_before
        || std::fs::metadata(&graph_path)
            .and_then(|metadata| metadata.modified())
            .map_err(|e| e.to_string())?
            != modified_before
        || manifest_hnsw(&fixture.data_dir)? != manifest_before
    {
        return Err("a current native graph must be retained without rewriting its receipt".into());
    }

    // A subsequent small ingest must maintain the new vector generation too;
    // the bulk-embedding branch alone cannot implement the requested flag.
    const DELTA_KEYWORD: &str = "orbitalquartzdelta searchable appended evidence";
    util::seed_codex_session(
        &fixture.codex_home,
        "rollout-2026-04-23T11-00-00-hnsw-delta.jsonl",
        DELTA_KEYWORD,
        true,
    );
    let out = run(&fixture, "index_hnsw_delta", &index_args, INDEX_TIMEOUT);
    if !out.status.success() {
        return Err(format!("delta index failed: {}", text(&out.stderr)));
    }
    let (out, current) = build_hnsw(&fixture, "index_hnsw_delta_admission", &["--check"])?;
    if !out.status.success()
        || str_at(&current, "state_before") != "native_valid"
        || current.get("manifest_recorded").and_then(Value::as_bool) != Some(true)
        || current.get("vector_count").and_then(Value::as_u64)
            <= built.get("vector_count").and_then(Value::as_u64)
    {
        return Err(format!(
            "delta must be covered by the current native graph: {current}"
        ));
    }
    let out = run(
        &fixture,
        "index_hnsw_delta_search",
        &[
            "search",
            DELTA_KEYWORD,
            "--mode",
            "semantic",
            "--model",
            "hash",
            "--approximate",
            "--robot",
            "--robot-meta",
            "--limit",
            "5",
        ],
        MODELS_TIMEOUT,
    );
    let results = json_stdout(&out, "delta semantic search")?;
    if !out.status.success()
        || results.pointer("/_meta/ann_stats").is_none()
        || results.pointer("/_meta/ann_unavailable_reason").is_some()
        || !results
            .get("hits")
            .and_then(Value::as_array)
            .is_some_and(|hits| {
                hits.iter().any(|hit| {
                    str_at(hit, "source_path")
                        .ends_with("rollout-2026-04-23T11-00-00-hnsw-delta.jsonl")
                })
            })
    {
        return Err(format!(
            "new evidence must remain searchable after maintenance: {results}"
        ));
    }
    Ok(())
}

#[test]
fn models_build_hnsw_serializes_publication_with_indexing() -> Result<(), String> {
    use fs2::FileExt;

    let fixture = semantic_fixture()?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(fixture.data_dir.join("index-run.lock"))
        .map_err(|e| e.to_string())?;
    lock.lock_exclusive().map_err(|e| e.to_string())?;
    let out = run(
        &fixture,
        "hnsw_build_busy",
        &["models", "build-hnsw", "--json"],
        MODELS_TIMEOUT,
    );
    if out.status.code() != Some(7) || manifest_hnsw(&fixture.data_dir)?.is_some() {
        return Err(format!(
            "concurrent indexing must refuse graph publication with exit 7: {} {}",
            text(&out.stdout),
            text(&out.stderr)
        ));
    }
    let (out, check) = build_hnsw(&fixture, "hnsw_check_while_busy", &["--check"])?;
    if !out.status.success() || str_at(&check, "state_before") != "missing" {
        return Err(format!(
            "read-only graph checks should remain available: {check}"
        ));
    }
    drop(lock);
    let (out, built) = build_hnsw(&fixture, "hnsw_build_after_busy", &[])?;
    if !out.status.success() || str_at(&built, "state_after") != "native_valid" {
        return Err(format!(
            "graph maintenance must succeed after the writer exits: {built}"
        ));
    }
    Ok(())
}
