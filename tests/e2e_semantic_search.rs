//! E2E tests for semantic and hybrid search modes.
//!
//! Tests cover:
//! - Vector index build with hash embedder (always available)
//! - Semantic search mode (vector-only results)
//! - Hybrid search mode (combined lexical + semantic)
//! - HNSW approximate nearest neighbor search
//! - Fallback behavior when semantic unavailable
//!
//! Part of bead: coding_agent_session_search-2vvg

use assert_cmd::cargo::cargo_bin_cmd;
use chrono::{SecondsFormat, Utc};
use coding_agent_search::search::ann_index::hnsw_index_path;
use frankensearch::index::VectorIndex;
use std::fs;
use std::path::{Path, PathBuf};

mod util;
use util::EnvGuard;
use util::e2e_log::PhaseTracker;

// =============================================================================
// E2E Logger Support
// =============================================================================

fn tracker_for(test_name: &str) -> PhaseTracker {
    PhaseTracker::new("e2e_semantic_search", test_name)
}

fn codex_iso_timestamp(ts_millis: u64) -> String {
    chrono::DateTime::<Utc>::from_timestamp_millis(ts_millis as i64)
        .expect("valid millis timestamp for codex fixture")
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Helper to create Claude Code session fixture.
#[allow(dead_code)]
fn make_claude_session(root: &Path, project: &str, filename: &str, content: &str, ts: &str) {
    let project_dir = root.join(format!("projects/{project}"));
    fs::create_dir_all(&project_dir).unwrap();
    let file = project_dir.join(filename);
    let sample = format!(
        r#"{{"type": "user", "timestamp": "{ts}", "message": {{"role": "user", "content": "{content}"}}}}
{{"type": "assistant", "timestamp": "{ts}", "message": {{"role": "assistant", "content": "{content}_response"}}}}"#
    );
    fs::write(file, sample).unwrap();
}

/// Helper to create Codex session fixture.
fn make_codex_session(root: &Path, date_path: &str, filename: &str, content: &str, ts: u64) {
    let sessions = root.join(format!("sessions/{date_path}"));
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join(filename);
    let workspace = root.to_string_lossy();
    let lines = [
        serde_json::json!({
            "timestamp": codex_iso_timestamp(ts),
            "type": "session_meta",
            "payload": {
                "id": filename,
                "cwd": workspace,
                "cli_version": "0.42.0"
            }
        }),
        serde_json::json!({
            "timestamp": codex_iso_timestamp(ts + 1_000),
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{ "type": "input_text", "text": content }]
            }
        }),
        serde_json::json!({
            "timestamp": codex_iso_timestamp(ts + 2_000),
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "text", "text": format!("{content}_response") }]
            }
        }),
    ];
    let mut sample = String::new();
    for line in lines {
        sample.push_str(&serde_json::to_string(&line).unwrap());
        sample.push('\n');
    }
    fs::write(file, sample).unwrap();
}

/// Check if any vector index (.fsvi) file exists in data_dir/vector_index/
fn has_vector_index(data_dir: &Path) -> bool {
    let vector_dir = data_dir.join("vector_index");
    if !vector_dir.exists() {
        return false;
    }
    fs::read_dir(&vector_dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .any(|e| e.path().extension().is_some_and(|ext| ext == "fsvi"))
        })
        .unwrap_or(false)
}

/// Check if any HNSW index (.chsw) file exists in data_dir/vector_index/
fn has_hnsw_index(data_dir: &Path) -> bool {
    let vector_dir = data_dir.join("vector_index");
    if !vector_dir.exists() {
        return false;
    }
    fs::read_dir(&vector_dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .any(|e| e.path().extension().is_some_and(|ext| ext == "chsw"))
        })
        .unwrap_or(false)
}

fn first_vector_index_path(data_dir: &Path) -> PathBuf {
    let vector_dir = data_dir.join("vector_index");
    let mut paths = fs::read_dir(&vector_dir)
        .expect("read vector index directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "fsvi"))
        .collect::<Vec<_>>();
    paths.sort();
    paths
        .into_iter()
        .next()
        .expect("semantic indexing must publish an FSVI")
}

fn selected_hnsw_index_path(data_dir: &Path) -> PathBuf {
    let exact_path = first_vector_index_path(data_dir);
    let exact = VectorIndex::open(&exact_path).expect("open selected exact FSVI");
    hnsw_index_path(data_dir, exact.embedder_id())
}

fn write_same_doc_ids_different_vectors(source_path: &Path, replacement_path: &Path) {
    let source = VectorIndex::open(source_path).expect("open selected exact FSVI");
    let mut writer = VectorIndex::create_with_revision(
        replacement_path,
        source.embedder_id(),
        source.embedder_revision(),
        source.dimension(),
        source.quantization(),
    )
    .expect("create replacement exact FSVI");

    for index in 0..source.record_count() {
        let doc_id = source.doc_id_at(index).expect("selected semantic doc ID");
        let mut vector = source
            .vector_at_f32(index)
            .expect("selected semantic vector");
        assert!(
            vector.iter().any(|value| *value != 0.0),
            "same-ID stale fixture requires a non-zero source vector"
        );
        for value in &mut vector {
            *value = -*value;
        }
        writer
            .write_record(doc_id, &vector)
            .expect("write same-ID different-vector record");
    }
    writer.finish().expect("finish replacement exact FSVI");

    let replacement = VectorIndex::open(replacement_path).expect("open replacement exact FSVI");
    assert_eq!(replacement.record_count(), source.record_count());
    for index in 0..source.record_count() {
        assert_eq!(
            replacement
                .doc_id_at(index)
                .expect("replacement semantic doc ID"),
            source.doc_id_at(index).expect("source semantic doc ID"),
            "replacement must preserve the exact ordered document identity"
        );
        assert_ne!(
            replacement
                .vector_at_f32(index)
                .expect("replacement semantic vector"),
            source.vector_at_f32(index).expect("source semantic vector"),
            "replacement must change vector content behind the same document ID"
        );
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ArtifactTreeEntrySnapshot {
    relative_path: PathBuf,
    file_kind: &'static str,
    contents: Option<Vec<u8>>,
    modified: std::time::SystemTime,
    readonly: bool,
    #[cfg(unix)]
    mode: u32,
}

fn artifact_tree_snapshot(root: &Path) -> Vec<ArtifactTreeEntrySnapshot> {
    let mut snapshot = Vec::new();
    for entry in walkdir::WalkDir::new(root).min_depth(1).follow_links(false) {
        let entry = entry.expect("walk semantic artifact tree");
        let path = entry.path();
        let metadata = fs::symlink_metadata(path).expect("stat semantic artifact");
        let file_type = metadata.file_type();
        let (file_kind, contents) = if file_type.is_file() {
            (
                "file",
                Some(fs::read(path).expect("read semantic artifact")),
            )
        } else if file_type.is_dir() {
            ("directory", None)
        } else if file_type.is_symlink() {
            (
                "symlink",
                Some(
                    fs::read_link(path)
                        .expect("read semantic artifact symlink")
                        .to_string_lossy()
                        .as_bytes()
                        .to_vec(),
                ),
            )
        } else {
            ("other", None)
        };
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            metadata.permissions().mode()
        };
        snapshot.push(ArtifactTreeEntrySnapshot {
            relative_path: path
                .strip_prefix(root)
                .expect("artifact path under root")
                .to_path_buf(),
            file_kind,
            contents,
            modified: metadata.modified().expect("artifact mtime"),
            readonly: metadata.permissions().readonly(),
            #[cfg(unix)]
            mode,
        });
    }
    snapshot.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    snapshot
}

// =============================================================================
// Semantic Index Build Tests
// =============================================================================

/// Test: Index with --semantic builds vector index alongside text index.
#[test]
fn semantic_index_builds_vector_file() {
    let tracker = tracker_for("semantic_index_builds_vector_file");
    let _trace_guard = tracker.trace_env_guard();

    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    let ps = tracker.start(
        "create_fixtures",
        Some("Create Codex sessions for semantic indexing"),
    );
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-1.jsonl",
        "machine learning neural networks deep learning",
        1732118400000,
    );
    make_codex_session(
        &codex_home,
        "2024/11/21",
        "rollout-2.jsonl",
        "database optimization query performance tuning",
        1732204800000,
    );
    tracker.end(
        "create_fixtures",
        Some("Create Codex sessions for semantic indexing"),
        ps,
    );

    let ps = tracker.start("run_semantic_index", Some("Run index --full --semantic"));
    let output = cargo_bin_cmd!("cass")
        .args([
            "index",
            "--full",
            "--semantic",
            "--embedder",
            "hash",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .output()
        .expect("index command");
    tracker.end(
        "run_semantic_index",
        Some("Run index --full --semantic"),
        ps,
    );

    assert!(
        output.status.success(),
        "index --semantic failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify vector index file was created
    let ps = tracker.start(
        "verify_vector_index",
        Some("Check vector index file exists"),
    );
    // Hash embedder creates index at vector_index/index-fnv1a-384.fsvi
    let vector_dir = data_dir.join("vector_index");
    assert!(
        vector_dir.exists(),
        "Vector index directory should exist at {:?}",
        vector_dir
    );
    // Find the actual index file (contains embedder ID in name)
    let vector_files: Vec<_> = fs::read_dir(&vector_dir)
        .expect("read vector_index dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "fsvi"))
        .collect();
    assert!(
        !vector_files.is_empty(),
        "Vector index directory should contain .fsvi files"
    );
    let metadata = fs::metadata(vector_files[0].path()).unwrap();
    // A real .fsvi vector index file for a hash-embedder corpus will be
    // at least a few KiB (vector headers + padding + embedding rows for
    // the seeded messages). A 1-byte file would pass `> 0` while
    // clearly indicating the file was created but never populated —
    // exactly the regression a presence-only check fails to catch.
    let file_bytes = metadata.len();
    assert!(
        file_bytes >= 1024,
        "vector index file at {} must be at least 1 KiB to carry a \
         meaningful set of embeddings; got {} bytes (presence-only \
         `> 0` check would have missed this)",
        vector_files[0].path().display(),
        file_bytes
    );
    tracker.end(
        "verify_vector_index",
        Some("Check vector index file exists"),
        ps,
    );

    tracker.complete();
}

/// Test: Index with --semantic --build-hnsw builds HNSW index.
///
/// Hash embeddings deliberately exercise the architecture-sensitive `DistDot`
/// normalization path that previously panicked while building the graph.
#[test]
fn semantic_index_builds_hnsw() {
    let tracker = tracker_for("semantic_index_builds_hnsw");
    let _trace_guard = tracker.trace_env_guard();

    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    let ps = tracker.start("create_fixtures", Some("Create sessions for HNSW index"));
    // Create enough sessions for meaningful HNSW testing
    for i in 0..5 {
        make_codex_session(
            &codex_home,
            &format!("2024/11/{:02}", 20 + i),
            &format!("rollout-{i}.jsonl"),
            &format!("session {i} with unique content for hnsw test topic{i}"),
            1732118400000 + (i as u64 * 86400000),
        );
    }
    tracker.end(
        "create_fixtures",
        Some("Create sessions for HNSW index"),
        ps,
    );

    let ps = tracker.start("run_hnsw_index", Some("Run index --semantic --build-hnsw"));
    let output = cargo_bin_cmd!("cass")
        .args([
            "index",
            "--full",
            "--semantic",
            "--build-hnsw",
            "--embedder",
            "hash",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .output()
        .expect("index command");
    tracker.end(
        "run_hnsw_index",
        Some("Run index --semantic --build-hnsw"),
        ps,
    );

    assert!(
        output.status.success(),
        "index --semantic --build-hnsw failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify HNSW index file was created
    let ps = tracker.start("verify_hnsw_index", Some("Check HNSW index file"));
    assert!(
        has_hnsw_index(&data_dir),
        "HNSW index file (.chsw) should exist in {:?}",
        data_dir.join("vector_index")
    );
    tracker.end("verify_hnsw_index", Some("Check HNSW index file"), ps);

    tracker.complete();
}

// =============================================================================
// Semantic Search Mode Tests
// =============================================================================

/// Test: Search with --mode semantic returns results.
#[test]
fn search_semantic_mode_returns_results() {
    let tracker = tracker_for("search_semantic_mode_returns_results");
    let _trace_guard = tracker.trace_env_guard();

    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    // Setup: Create and index content
    let ps = tracker.start("setup", Some("Create and index semantic content"));
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-1.jsonl",
        "python machine learning tensorflow neural network training",
        1732118400000,
    );
    make_codex_session(
        &codex_home,
        "2024/11/21",
        "rollout-2.jsonl",
        "rust programming systems performance optimization",
        1732204800000,
    );

    cargo_bin_cmd!("cass")
        .args([
            "index",
            "--full",
            "--semantic",
            "--embedder",
            "hash",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();
    tracker.end("setup", Some("Create and index semantic content"), ps);

    // Test semantic search
    let ps = tracker.start("search_semantic", Some("Search with --mode semantic"));
    let output = cargo_bin_cmd!("cass")
        .args([
            "search",
            "--robot",
            "--mode",
            "semantic",
            "--model",
            "hash",
            "--data-dir",
        ])
        .arg(&data_dir)
        .arg("deep learning AI")
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .output()
        .expect("search command");
    tracker.end("search_semantic", Some("Search with --mode semantic"), ps);

    assert!(
        output.status.success(),
        "semantic search failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value =
        serde_json::from_str(&stdout).expect("search output should be valid JSON");

    // Semantic search should return results (hash embedder provides basic similarity)
    let ps = tracker.start(
        "verify_results",
        Some("Verify semantic search returns hits"),
    );
    // Bead 7k7pl: pin TYPE on hits (must be a JSON array), not just
    // "field present". A regression that emitted `null` or a scalar
    // would slip past `.is_some()` while breaking every downstream
    // consumer that calls `.as_array()`.
    assert!(
        json.get("hits").and_then(|v| v.as_array()).is_some(),
        "hits must be an array. Got: {}",
        json
    );
    tracker.end(
        "verify_results",
        Some("Verify semantic search returns hits"),
        ps,
    );

    tracker.complete();
}

/// Test: a fresh search process opens the exact CASS writer artifact and never
/// falls back to FrankenSearch's historical fixed basenames.
///
/// This is the restart boundary that the old contract missed: indexing writes
/// `vector_index/index-<embedder>.fsvi`, while an independently started search
/// process used to probe `vector.fast.idx`, `vector.idx`, and
/// `vector.quality.idx`. Corrupt decoys at all three fixed names make the
/// selected path observable without mocks.
#[test]
fn exact_artifact_contract_restart_uses_writer_path_and_ignores_decoys() {
    let tracker =
        tracker_for("semantic_restart_uses_exact_writer_artifact_and_ignores_fixed_name_decoys");
    let _trace_guard = tracker.trace_env_guard();

    let tmp = tempfile::TempDir::new().expect("semantic restart fixture");
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).expect("create CASS data directory");

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    let target_phrase = "cobalt narwhal exact artifact rendezvous";
    let ps = tracker.start(
        "create_restart_fixture",
        Some("Create one exact semantic target and disjoint distractors"),
    );
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-exact-artifact-target.jsonl",
        target_phrase,
        1732118400000,
    );
    make_codex_session(
        &codex_home,
        "2024/11/21",
        "rollout-exact-artifact-distractor-a.jsonl",
        "database transaction isolation and durable write scheduling",
        1732204800000,
    );
    make_codex_session(
        &codex_home,
        "2024/11/22",
        "rollout-exact-artifact-distractor-b.jsonl",
        "terminal rendering keyboard navigation and color palettes",
        1732291200000,
    );
    tracker.end(
        "create_restart_fixture",
        Some("Create one exact semantic target and disjoint distractors"),
        ps,
    );

    let ps = tracker.start(
        "write_exact_semantic_artifact",
        Some("Run the real CASS semantic writer with the hash embedder"),
    );
    let index_output = cargo_bin_cmd!("cass")
        .args([
            "index",
            "--full",
            "--semantic",
            "--embedder",
            "hash",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .output()
        .expect("semantic index process");
    tracker.end(
        "write_exact_semantic_artifact",
        Some("Run the real CASS semantic writer with the hash embedder"),
        ps,
    );
    assert!(
        index_output.status.success(),
        "semantic indexing failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&index_output.stdout),
        String::from_utf8_lossy(&index_output.stderr)
    );

    let vector_dir = data_dir.join("vector_index");
    let exact_path = vector_dir.join("index-fnv1a-384.fsvi");
    assert!(
        exact_path.is_file(),
        "the CASS writer must publish its selected artifact at {}",
        exact_path.display()
    );

    let ps = tracker.start(
        "plant_corrupt_fixed_name_decoys",
        Some("Plant corrupt historical FrankenSearch basenames beside the exact artifact"),
    );
    let decoys = [
        (
            vector_dir.join("vector.fast.idx"),
            b"corrupt-fast-fixed-name-decoy".as_slice(),
        ),
        (
            vector_dir.join("vector.idx"),
            b"corrupt-fallback-fixed-name-decoy".as_slice(),
        ),
        (
            vector_dir.join("vector.quality.idx"),
            b"corrupt-quality-fixed-name-decoy".as_slice(),
        ),
    ];
    for (path, bytes) in &decoys {
        fs::write(path, bytes).expect("write corrupt fixed-name decoy");
    }
    tracker.end(
        "plant_corrupt_fixed_name_decoys",
        Some("Plant corrupt historical FrankenSearch basenames beside the exact artifact"),
        ps,
    );

    let exact_bytes_before = fs::read(&exact_path).expect("read exact artifact before search");
    let exact_modified_before = fs::metadata(&exact_path)
        .and_then(|metadata| metadata.modified())
        .expect("read exact artifact mtime before search");
    let decoy_snapshots = decoys
        .iter()
        .map(|(path, _)| {
            let bytes = fs::read(path).expect("read decoy before search");
            let modified = fs::metadata(path)
                .and_then(|metadata| metadata.modified())
                .expect("read decoy mtime before search");
            (path.clone(), bytes, modified)
        })
        .collect::<Vec<_>>();
    let mut entries_before = fs::read_dir(&vector_dir)
        .expect("read vector directory before search")
        .map(|entry| entry.expect("vector directory entry").file_name())
        .collect::<Vec<_>>();
    entries_before.sort();

    let ps = tracker.start(
        "restart_and_search_exact_artifact",
        Some("Start a fresh CASS process and run fast-only semantic search"),
    );
    let search_output = cargo_bin_cmd!("cass")
        .args([
            "search",
            "--robot",
            "--mode",
            "semantic",
            "--model",
            "hash",
            "--fast-only",
            "--data-dir",
        ])
        .arg(&data_dir)
        .arg(target_phrase)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .output()
        .expect("fresh semantic search process");
    tracker.end(
        "restart_and_search_exact_artifact",
        Some("Start a fresh CASS process and run fast-only semantic search"),
        ps,
    );
    assert!(
        search_output.status.success(),
        "fresh-process semantic search failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&search_output.stdout),
        String::from_utf8_lossy(&search_output.stderr)
    );

    let response: serde_json::Value =
        serde_json::from_slice(&search_output.stdout).expect("semantic search robot JSON");
    let hits = response
        .get("hits")
        .and_then(serde_json::Value::as_array)
        .expect("semantic search hits array");
    let first_content = hits
        .first()
        .and_then(|hit| hit.get("content"))
        .and_then(serde_json::Value::as_str)
        .expect("semantic search must return a first hit with content");
    assert!(
        first_content.contains(target_phrase),
        "the exact writer artifact must rank the unique target first; got {first_content:?}"
    );

    let ps = tracker.start(
        "verify_read_only_artifact_contract",
        Some("Verify exact artifact, decoys, and directory inventory are unchanged"),
    );
    assert_eq!(
        fs::read(&exact_path).expect("read exact artifact after search"),
        exact_bytes_before,
        "serving must not rewrite the selected FSVI"
    );
    assert_eq!(
        fs::metadata(&exact_path)
            .and_then(|metadata| metadata.modified())
            .expect("read exact artifact mtime after search"),
        exact_modified_before,
        "serving must preserve the selected FSVI mtime"
    );
    for (path, bytes_before, modified_before) in decoy_snapshots {
        assert_eq!(
            fs::read(&path).expect("read decoy after search"),
            bytes_before,
            "search must not read-repair or replace fixed-name decoy {}",
            path.display()
        );
        assert_eq!(
            fs::metadata(&path)
                .and_then(|metadata| metadata.modified())
                .expect("read decoy mtime after search"),
            modified_before,
            "search must not touch fixed-name decoy {}",
            path.display()
        );
    }
    let mut entries_after = fs::read_dir(&vector_dir)
        .expect("read vector directory after search")
        .map(|entry| entry.expect("vector directory entry").file_name())
        .collect::<Vec<_>>();
    entries_after.sort();
    assert_eq!(
        entries_after, entries_before,
        "search must not synthesize, repair, or rename vector artifacts"
    );
    tracker.end(
        "verify_read_only_artifact_contract",
        Some("Verify exact artifact, decoys, and directory inventory are unchanged"),
        ps,
    );

    tracker.complete();
}

/// Readiness and serving must reject the same final-component FSVI symlink.
///
/// A readiness probe that follows the alias while serving rejects it would
/// advertise a semantic capability that a fresh query process cannot use.
#[cfg(unix)]
#[test]
fn fsvi_symlink_is_rejected_by_readiness_and_serving_without_mutation() {
    use std::os::unix::fs::symlink;

    let tracker = tracker_for("fsvi_symlink_is_rejected_by_readiness_and_serving_without_mutation");
    let _trace_guard = tracker.trace_env_guard();

    let tmp = tempfile::TempDir::new().expect("FSVI readiness fixture");
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).expect("create CASS data directory");

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    let ps = tracker.start(
        "build_exact_source",
        Some("Build the authoritative hash FSVI with the real CASS writer"),
    );
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-fsvi-symlink.jsonl",
        "final component aliases must not become semantic capabilities",
        1732118400000,
    );
    cargo_bin_cmd!("cass")
        .args([
            "index",
            "--full",
            "--semantic",
            "--embedder",
            "hash",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();
    tracker.end(
        "build_exact_source",
        Some("Build the authoritative hash FSVI with the real CASS writer"),
        ps,
    );

    let ps = tracker.start(
        "install_fsvi_symlink",
        Some("Move the exact bytes to an unselected target and alias the selected name"),
    );
    let selected_path = first_vector_index_path(&data_dir);
    let target_path = selected_path.with_file_name("unselected-exact-target.fsvi");
    fs::rename(&selected_path, &target_path).expect("move exact FSVI to alias target");
    symlink(&target_path, &selected_path).expect("create selected final-component FSVI symlink");
    let vector_dir = data_dir.join("vector_index");
    let artifacts_before = artifact_tree_snapshot(&vector_dir);
    tracker.end(
        "install_fsvi_symlink",
        Some("Move the exact bytes to an unselected target and alias the selected name"),
        ps,
    );

    let ps = tracker.start(
        "probe_readiness",
        Some("Run status in an isolated fresh process with explicit hash policy"),
    );
    let status_output = cargo_bin_cmd!("cass")
        .args(["status", "--json", "--data-dir"])
        .arg(&data_dir)
        .env("CASS_SEMANTIC_EMBEDDER", "hash")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .output()
        .expect("fresh-process semantic readiness probe");
    tracker.end(
        "probe_readiness",
        Some("Run status in an isolated fresh process with explicit hash policy"),
        ps,
    );
    assert!(
        status_output.status.success(),
        "status must report a rejected semantic artifact without crashing\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&status_output.stdout),
        String::from_utf8_lossy(&status_output.stderr)
    );
    let status_json: serde_json::Value =
        serde_json::from_slice(&status_output.stdout).expect("status readiness JSON");
    assert_eq!(
        status_json
            .pointer("/semantic/availability")
            .and_then(serde_json::Value::as_str),
        Some("load_failed"),
        "readiness must not follow the selected FSVI symlink: {status_json}"
    );
    assert_eq!(
        status_json
            .pointer("/semantic/can_search")
            .and_then(serde_json::Value::as_bool),
        Some(false)
    );
    assert!(
        status_json
            .pointer("/semantic/summary")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|summary| summary.contains("symlink")),
        "readiness must expose the exact-artifact rejection: {status_json}"
    );

    let ps = tracker.start(
        "attempt_serving",
        Some("Run explicit hash semantic search in a second fresh process"),
    );
    let search_output = cargo_bin_cmd!("cass")
        .args([
            "search",
            "--robot",
            "--mode",
            "semantic",
            "--model",
            "hash",
            "--data-dir",
        ])
        .arg(&data_dir)
        .arg("final component aliases")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .output()
        .expect("fresh-process semantic serving attempt");
    tracker.end(
        "attempt_serving",
        Some("Run explicit hash semantic search in a second fresh process"),
        ps,
    );
    assert!(
        !search_output.status.success(),
        "serving must reject the same FSVI symlink\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&search_output.stdout),
        String::from_utf8_lossy(&search_output.stderr)
    );
    let serving_diagnostic = format!(
        "{}\n{}",
        String::from_utf8_lossy(&search_output.stdout),
        String::from_utf8_lossy(&search_output.stderr)
    );
    assert!(
        serving_diagnostic.contains("symlink"),
        "serving and readiness must expose the same rejection class: {serving_diagnostic}"
    );

    let ps = tracker.start(
        "verify_immutable_rejection",
        Some("Verify readiness and serving did not rewrite either alias role"),
    );
    assert_eq!(
        artifact_tree_snapshot(&vector_dir),
        artifacts_before,
        "readiness and serving rejection must preserve bytes, mtimes, permissions, links, and inventory"
    );
    tracker.end(
        "verify_immutable_rejection",
        Some("Verify readiness and serving did not rewrite either alias role"),
        ps,
    );
    tracker.complete();
}

// =============================================================================
// Hybrid Search Mode Tests
// =============================================================================

/// Test: Search with --mode hybrid combines lexical and semantic.
#[test]
fn search_hybrid_mode_combines_results() {
    let tracker = tracker_for("search_hybrid_mode_combines_results");
    let _trace_guard = tracker.trace_env_guard();

    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    // Setup
    let ps = tracker.start("setup", Some("Create and index hybrid content"));
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-1.jsonl",
        "hybrid search combines lexical and semantic signals",
        1732118400000,
    );
    make_codex_session(
        &codex_home,
        "2024/11/21",
        "rollout-2.jsonl",
        "another session about unrelated database queries",
        1732204800000,
    );

    cargo_bin_cmd!("cass")
        .args([
            "index",
            "--full",
            "--semantic",
            "--embedder",
            "hash",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();
    tracker.end("setup", Some("Create and index hybrid content"), ps);

    // Test hybrid search
    let ps = tracker.start("search_hybrid", Some("Search with --mode hybrid"));
    let output = cargo_bin_cmd!("cass")
        .args([
            "search",
            "--robot",
            "--mode",
            "hybrid",
            "--model",
            "hash",
            "--data-dir",
        ])
        .arg(&data_dir)
        .arg("hybrid search lexical semantic")
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .output()
        .expect("search command");
    tracker.end("search_hybrid", Some("Search with --mode hybrid"), ps);

    assert!(
        output.status.success(),
        "hybrid search failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value =
        serde_json::from_str(&stdout).expect("search output should be valid JSON");

    // Hybrid search should return results with exact term matches ranked highly
    let ps = tracker.start("verify_results", Some("Verify hybrid returns hits"));
    let hits = json.get("hits").and_then(|h| h.as_array());
    assert!(hits.is_some(), "Response should have hits array");
    let hits = hits.unwrap();
    assert!(
        !hits.is_empty(),
        "Hybrid search should find matching content"
    );
    tracker.end("verify_results", Some("Verify hybrid returns hits"), ps);

    tracker.complete();
}

// =============================================================================
// HNSW Approximate Search Tests
// =============================================================================

/// Test: Search with --approximate uses HNSW index.
///
/// Hash embeddings deliberately exercise HNSW query normalization without
/// requiring an external model fixture.
#[test]
fn search_approximate_uses_hnsw() {
    let tracker = tracker_for("search_approximate_uses_hnsw");
    let _trace_guard = tracker.trace_env_guard();

    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    // Setup: Create sessions and build HNSW index
    let ps = tracker.start("setup", Some("Create sessions and build HNSW"));
    for i in 0..10 {
        make_codex_session(
            &codex_home,
            &format!("2024/11/{:02}", 15 + i),
            &format!("rollout-{i}.jsonl"),
            &format!("approximate nearest neighbor search test session {i}"),
            1731628800000 + (i as u64 * 86400000),
        );
    }

    cargo_bin_cmd!("cass")
        .args([
            "index",
            "--full",
            "--semantic",
            "--build-hnsw",
            "--embedder",
            "hash",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();
    tracker.end("setup", Some("Create sessions and build HNSW"), ps);

    // Verify HNSW file exists
    assert!(
        has_hnsw_index(&data_dir),
        "HNSW index should exist before approximate search"
    );
    let vector_dir = data_dir.join("vector_index");
    let artifacts_before = artifact_tree_snapshot(&vector_dir);

    // Test approximate search
    let ps = tracker.start("search_approximate", Some("Search with --approximate"));
    let output = cargo_bin_cmd!("cass")
        .args([
            "search",
            "--robot",
            "--robot-meta",
            "--mode",
            "semantic",
            "--model",
            "hash",
            "--approximate",
            "--data-dir",
        ])
        .arg(&data_dir)
        .arg("nearest neighbor")
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .output()
        .expect("search command");
    tracker.end("search_approximate", Some("Search with --approximate"), ps);

    assert!(
        output.status.success(),
        "approximate search failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value =
        serde_json::from_str(&stdout).expect("search output should be valid JSON");

    let ps = tracker.start("verify_results", Some("Verify approximate search results"));
    // Bead 7k7pl: pin TYPE on hits (must be a JSON array), not just
    // "field present". A regression that emitted `null` or a scalar
    // would slip past `.is_some()` while breaking downstream
    // `.as_array()` consumers.
    assert!(
        json.get("hits").and_then(|v| v.as_array()).is_some(),
        "hits must be an array for approximate search. Got: {}",
        json
    );
    assert!(
        json.pointer("/_meta/ann_stats").is_some(),
        "native ANN execution must expose ANN stats: {json}"
    );
    assert!(
        json.pointer("/_meta/ann_unavailable_reason").is_none(),
        "native ANN execution must not report an exact-fallback reason: {json}"
    );
    assert_eq!(
        artifact_tree_snapshot(&vector_dir),
        artifacts_before,
        "fresh-process native ANN search must preserve bytes, mtimes, permissions, and inventory"
    );
    tracker.end(
        "verify_results",
        Some("Verify approximate search results"),
        ps,
    );

    tracker.complete();
}

// =============================================================================
// Fallback Behavior Tests
// =============================================================================

/// Test: Semantic search without vector index reports informative error.
#[test]
fn semantic_without_index_reports_error() {
    let tracker = tracker_for("semantic_without_index_reports_error");
    let _trace_guard = tracker.trace_env_guard();

    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    // Setup: Index WITHOUT semantic
    let ps = tracker.start("setup", Some("Index without --semantic"));
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-1.jsonl",
        "content for lexical only index",
        1732118400000,
    );

    cargo_bin_cmd!("cass")
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();
    tracker.end("setup", Some("Index without --semantic"), ps);

    // Verify no vector index
    assert!(
        !has_vector_index(&data_dir),
        "Vector index should not exist"
    );

    // Test semantic search fails gracefully
    let ps = tracker.start("search_semantic", Some("Try semantic search without index"));
    let output = cargo_bin_cmd!("cass")
        .args([
            "search",
            "--robot",
            "--mode",
            "semantic",
            "--model",
            "hash",
            "--data-dir",
        ])
        .arg(&data_dir)
        .arg("query")
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .output()
        .expect("search command");
    tracker.end(
        "search_semantic",
        Some("Try semantic search without index"),
        ps,
    );

    // Should report error (non-zero exit or error in JSON)
    let ps = tracker.start("verify_error", Some("Verify informative error"));
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Either exits with error or returns JSON with error info
    if output.status.success() {
        // If it succeeded, the JSON should indicate the issue
        let json: serde_json::Value = serde_json::from_str(&stdout).expect("output should be JSON");
        // Check for error field or empty results with explanation
        let has_error = json.get("error").is_some();
        let has_hint = stdout.contains("index --semantic") || stdout.contains("lexical");
        assert!(
            has_error
                || has_hint
                || json
                    .get("hits")
                    .is_none_or(|h| { h.as_array().is_none_or(|a| a.is_empty()) }),
            "Should indicate semantic index missing or return empty results"
        );
    }
    // Non-zero exit is acceptable - it means the error was reported
    tracker.end("verify_error", Some("Verify informative error"), ps);

    tracker.complete();
}

/// Test: Approximate search without HNSW stays useful via exact fallback.
#[test]
fn approximate_without_hnsw_falls_back_exact_with_typed_reason() {
    let tracker = tracker_for("approximate_without_hnsw_falls_back_exact_with_typed_reason");
    let _trace_guard = tracker.trace_env_guard();

    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    // Setup: Index with semantic but WITHOUT HNSW
    let ps = tracker.start("setup", Some("Index with semantic but no HNSW"));
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-1.jsonl",
        "content for vector index without hnsw",
        1732118400000,
    );

    cargo_bin_cmd!("cass")
        .args([
            "index",
            "--full",
            "--semantic",
            "--embedder",
            "hash",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();
    tracker.end("setup", Some("Index with semantic but no HNSW"), ps);

    // Verify vector exists but HNSW does not
    assert!(has_vector_index(&data_dir), "Vector index should exist");
    assert!(!has_hnsw_index(&data_dir), "HNSW index should not exist");
    let vector_dir = data_dir.join("vector_index");
    let artifacts_before = artifact_tree_snapshot(&vector_dir);

    // Test approximate request in a fresh process.
    let ps = tracker.start(
        "search_approximate",
        Some("Request approximate search without HNSW and require exact fallback"),
    );
    let output = cargo_bin_cmd!("cass")
        .args([
            "search",
            "--robot",
            "--robot-meta",
            "--mode",
            "semantic",
            "--model",
            "hash",
            "--approximate",
            "--data-dir",
        ])
        .arg(&data_dir)
        .arg("vector index without hnsw")
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .output()
        .expect("search command");
    tracker.end(
        "search_approximate",
        Some("Request approximate search without HNSW and require exact fallback"),
        ps,
    );

    let verify_ps = tracker.start(
        "verify_exact_fallback",
        Some("Verify hits, bounded reason, and immutable artifacts"),
    );
    assert!(
        output.status.success(),
        "an absent ANN must not make semantic search fail\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value =
        serde_json::from_str(&stdout).expect("fallback output should be valid JSON");
    assert!(
        json.get("hits")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|hits| !hits.is_empty()),
        "exact fallback must return the indexed semantic hit: {json}"
    );
    assert_eq!(
        json.pointer("/_meta/ann_unavailable_reason")
            .and_then(serde_json::Value::as_str),
        Some("ann_sidecar_missing"),
        "robot metadata must explain why exact fallback was selected"
    );
    assert!(
        json.pointer("/_meta/ann_stats").is_none(),
        "exact fallback must not claim ANN execution: {json}"
    );

    let human_search_ps = tracker.start(
        "search_approximate_human",
        Some("Verify the human fallback notice uses the same bounded reason"),
    );
    let human_output = cargo_bin_cmd!("cass")
        .args([
            "search",
            "--mode",
            "semantic",
            "--model",
            "hash",
            "--approximate",
            "--data-dir",
        ])
        .arg(&data_dir)
        .arg("vector index without hnsw")
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .output()
        .expect("human fallback search command");
    tracker.end(
        "search_approximate_human",
        Some("Verify the human fallback notice uses the same bounded reason"),
        human_search_ps,
    );
    assert!(
        human_output.status.success(),
        "human exact fallback must succeed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&human_output.stdout),
        String::from_utf8_lossy(&human_output.stderr)
    );
    let human_stderr = String::from_utf8_lossy(&human_output.stderr);
    assert!(
        human_stderr.contains("ann_sidecar_missing")
            && human_stderr.contains("exact semantic search"),
        "human and robot fallback diagnostics must agree: {human_stderr}"
    );
    assert_eq!(
        artifact_tree_snapshot(&vector_dir),
        artifacts_before,
        "robot and human missing-ANN fallbacks must preserve bytes, mtimes, permissions, and inventory"
    );
    tracker.end(
        "verify_exact_fallback",
        Some("Verify hits, bounded reason, and immutable artifacts"),
        verify_ps,
    );

    tracker.complete();
}

/// A fresh process must reject an ANN built from the same ordered document IDs
/// but different vector contents, then answer from the selected exact FSVI.
#[test]
fn approximate_stale_same_doc_ids_falls_back_exact_without_mutation() {
    let tracker = tracker_for("approximate_stale_same_doc_ids_falls_back_exact_without_mutation");
    let _trace_guard = tracker.trace_env_guard();

    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    let ps = tracker.start(
        "build_selected_generation",
        Some("Build the selected exact FSVI and native ANN from corpus A"),
    );
    for i in 0..4 {
        make_codex_session(
            &codex_home,
            &format!("2024/11/{:02}", 10 + i),
            &format!("rollout-{i}.jsonl"),
            &format!("authoritative corpus alpha document {i}"),
            1731196800000 + (i as u64 * 86400000),
        );
    }
    cargo_bin_cmd!("cass")
        .args([
            "index",
            "--full",
            "--semantic",
            "--build-hnsw",
            "--embedder",
            "hash",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();
    tracker.end(
        "build_selected_generation",
        Some("Build the selected exact FSVI and native ANN from corpus A"),
        ps,
    );

    let ps = tracker.start(
        "install_stale_exact_source",
        Some("Replace only vector payloads behind the exact same ordered document IDs"),
    );
    let selected_exact = first_vector_index_path(&data_dir);
    let replacement_exact = home.join("same-doc-ids-different-vectors.fsvi");
    write_same_doc_ids_different_vectors(&selected_exact, &replacement_exact);
    fs::copy(&replacement_exact, &selected_exact).expect("install different-vector exact FSVI");
    let vector_dir = data_dir.join("vector_index");
    let artifacts_before = artifact_tree_snapshot(&vector_dir);
    tracker.end(
        "install_stale_exact_source",
        Some("Replace only vector payloads behind the exact same ordered document IDs"),
        ps,
    );

    let ps = tracker.start(
        "fresh_process_stale_ann_search",
        Some("Request approximate search and require exact fallback"),
    );
    let output = cargo_bin_cmd!("cass")
        .args([
            "search",
            "--robot",
            "--robot-meta",
            "--mode",
            "semantic",
            "--model",
            "hash",
            "--approximate",
            "--data-dir",
        ])
        .arg(&data_dir)
        .arg("authoritative semantic query")
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .output()
        .expect("fresh-process stale ANN search");
    tracker.end(
        "fresh_process_stale_ann_search",
        Some("Request approximate search and require exact fallback"),
        ps,
    );

    let ps = tracker.start(
        "verify_stale_fallback",
        Some("Verify exact hits, bounded reason, and immutable artifacts"),
    );
    assert!(
        output.status.success(),
        "a stale ANN must fall back instead of failing\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stale fallback robot JSON");
    assert!(
        json.get("hits")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|hits| !hits.is_empty()),
        "exact fallback must return semantic hits: {json}"
    );
    assert_eq!(
        json.pointer("/_meta/ann_unavailable_reason")
            .and_then(serde_json::Value::as_str),
        Some("ann_sidecar_not_native"),
        "same-doc/different-vector ANN must be rejected as non-native"
    );
    assert!(
        json.pointer("/_meta/ann_stats").is_none(),
        "stale ANN fallback must not claim ANN execution: {json}"
    );
    assert_eq!(
        artifact_tree_snapshot(&vector_dir),
        artifacts_before,
        "stale ANN fallback must preserve bytes, mtimes, permissions, and inventory"
    );
    tracker.end(
        "verify_stale_fallback",
        Some("Verify exact hits, bounded reason, and immutable artifacts"),
        ps,
    );

    tracker.complete();
}

#[derive(Debug, Clone, Copy)]
enum InvalidAnnInstallation {
    CorruptMetadata,
    Directory,
    CrossRoleAlias,
    #[cfg(unix)]
    FinalComponentSymlink,
    #[cfg(unix)]
    BrokenFinalComponentSymlink,
}

fn exercise_invalid_ann_fresh_process_fallback(case: InvalidAnnInstallation) {
    let tracker = tracker_for(&format!(
        "approximate_invalid_ann_falls_back_exact_without_mutation::{case:?}"
    ));
    let _trace_guard = tracker.trace_env_guard();

    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    let ps = tracker.start(
        "build_exact_source",
        Some("Build the authoritative exact FSVI without an ANN"),
    );
    make_codex_session(
        &codex_home,
        "2024/11/10",
        "rollout-invalid-ann.jsonl",
        "authoritative exact fallback document",
        1731196800000,
    );
    cargo_bin_cmd!("cass")
        .args([
            "index",
            "--full",
            "--semantic",
            "--embedder",
            "hash",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();
    tracker.end(
        "build_exact_source",
        Some("Build the authoritative exact FSVI without an ANN"),
        ps,
    );

    let exact_path = first_vector_index_path(&data_dir);
    let ann_path = selected_hnsw_index_path(&data_dir);
    assert!(!ann_path.exists(), "fixture must start without an ANN");
    let ps = tracker.start(
        "install_invalid_ann",
        Some("Install a selected but inadmissible optional ANN artifact"),
    );
    match case {
        InvalidAnnInstallation::CorruptMetadata => {
            fs::write(&ann_path, b"not HNSW metadata").expect("write corrupt ANN metadata");
        }
        InvalidAnnInstallation::Directory => {
            fs::create_dir(&ann_path).expect("create non-regular ANN directory");
        }
        InvalidAnnInstallation::CrossRoleAlias => {
            fs::hard_link(&exact_path, &ann_path).expect("create cross-role ANN hard link");
        }
        #[cfg(unix)]
        InvalidAnnInstallation::FinalComponentSymlink => {
            use std::os::unix::fs::symlink;

            let target = ann_path.with_extension("target");
            fs::write(&target, b"unselected ANN target").expect("write ANN symlink target");
            symlink(&target, &ann_path).expect("create final-component ANN symlink");
        }
        #[cfg(unix)]
        InvalidAnnInstallation::BrokenFinalComponentSymlink => {
            use std::os::unix::fs::symlink;

            let absent_target = ann_path.with_extension("absent");
            assert!(
                !absent_target.exists(),
                "broken ANN fixture target must remain absent"
            );
            symlink(&absent_target, &ann_path).expect("create broken final-component ANN symlink");
        }
    }
    tracker.end(
        "install_invalid_ann",
        Some("Install a selected but inadmissible optional ANN artifact"),
        ps,
    );

    let vector_dir = data_dir.join("vector_index");
    let artifacts_before = artifact_tree_snapshot(&vector_dir);
    let ps = tracker.start(
        "fresh_process_invalid_ann_search",
        Some("Request approximate search and require exact fallback"),
    );
    let output = cargo_bin_cmd!("cass")
        .args([
            "search",
            "--robot",
            "--robot-meta",
            "--mode",
            "semantic",
            "--model",
            "hash",
            "--approximate",
            "--data-dir",
        ])
        .arg(&data_dir)
        .arg("authoritative exact fallback")
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .output()
        .expect("fresh-process invalid ANN search");
    tracker.end(
        "fresh_process_invalid_ann_search",
        Some("Request approximate search and require exact fallback"),
        ps,
    );

    let ps = tracker.start(
        "verify_invalid_ann_fallback",
        Some("Verify exact hits, bounded reason, and immutable artifacts"),
    );
    assert!(
        output.status.success(),
        "an invalid optional ANN must not make semantic search fail for {case:?}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("invalid ANN fallback robot JSON");
    assert!(
        json.get("hits")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|hits| !hits.is_empty()),
        "exact fallback must return semantic hits for {case:?}: {json}"
    );
    assert_eq!(
        json.pointer("/_meta/ann_unavailable_reason")
            .and_then(serde_json::Value::as_str),
        Some("ann_sidecar_open_failed"),
        "invalid optional ANN reason disagreed for {case:?}"
    );
    assert!(
        json.pointer("/_meta/ann_stats").is_none(),
        "invalid ANN fallback must not claim ANN execution for {case:?}: {json}"
    );
    assert_eq!(
        artifact_tree_snapshot(&vector_dir),
        artifacts_before,
        "invalid ANN fallback must preserve bytes, mtimes, permissions, and inventory for {case:?}"
    );
    tracker.end(
        "verify_invalid_ann_fallback",
        Some("Verify exact hits, bounded reason, and immutable artifacts"),
        ps,
    );
    tracker.complete();
}

/// Invalid optional ANN artifacts must never disable exact semantic search.
#[test]
fn approximate_invalid_ann_falls_back_exact_without_mutation() {
    let mut cases = vec![
        InvalidAnnInstallation::CorruptMetadata,
        InvalidAnnInstallation::Directory,
        InvalidAnnInstallation::CrossRoleAlias,
    ];
    #[cfg(unix)]
    cases.extend([
        InvalidAnnInstallation::FinalComponentSymlink,
        InvalidAnnInstallation::BrokenFinalComponentSymlink,
    ]);

    for case in cases {
        exercise_invalid_ann_fresh_process_fallback(case);
    }
}

// =============================================================================
// Performance Metrics Tests
// =============================================================================

/// Test: Semantic search captures timing metrics.
#[test]
fn semantic_search_emits_timing() {
    let tracker = tracker_for("semantic_search_emits_timing");
    let _trace_guard = tracker.trace_env_guard();

    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    // Setup
    let ps = tracker.start("setup", Some("Create and index content"));
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-1.jsonl",
        "performance timing test content",
        1732118400000,
    );

    cargo_bin_cmd!("cass")
        .args([
            "index",
            "--full",
            "--semantic",
            "--embedder",
            "hash",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();
    tracker.end("setup", Some("Create and index content"), ps);

    // Run search with timing
    let ps = tracker.start("search_timed", Some("Search and capture timing"));
    let start = std::time::Instant::now();
    let output = cargo_bin_cmd!("cass")
        .args([
            "search",
            "--robot",
            "--mode",
            "semantic",
            "--model",
            "hash",
            "--data-dir",
        ])
        .arg(&data_dir)
        .arg("timing test")
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .output()
        .expect("search command");
    let duration_ms = start.elapsed().as_millis() as u64;
    tracker.end("search_timed", Some("Search and capture timing"), ps);

    assert!(output.status.success());

    // Emit performance metric
    tracker.metrics(
        "semantic_search_latency",
        &util::e2e_log::E2ePerformanceMetrics::new().with_duration(duration_ms),
    );

    tracker.complete();
}
