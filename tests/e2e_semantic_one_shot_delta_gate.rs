//! Real-binary gate for bead `coding_agent_session_search-tpndx` (GH #394
//! remainder): a one-shot `cass index --semantic` whose embedding watermark
//! merely TRAILS the corpus must embed only the delta (WAL-append onto the
//! existing `.fsvi`) instead of re-embedding the whole corpus.
//!
//! 7f657026 already short-circuits the fully-covered case; this gate pins the
//! trailing case: seed one session, build the semantic artifact, add a second
//! session, run a plain one-shot `index --semantic`, and prove (a) the delta
//! path was taken (its tracing marker, human mode honours `RUST_LOG`), (b) the
//! new session is semantically searchable afterwards, and (c) the artifact was
//! appended to, not replaced (same file, larger).

mod util;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use assert_cmd::cargo::cargo_bin;
use serde_json::Value;

use util::timeout::spawn_with_timeout_or_diag;

const INDEX_TIMEOUT: Duration = Duration::from_secs(180);
const SEARCH_TIMEOUT: Duration = Duration::from_secs(60);
const FIRST_KEYWORD: &str = "delta-gate-first-session-zxqv";
const SECOND_KEYWORD: &str = "delta-gate-second-session-plmk";
const DELTA_MARKER: &str = "one-shot semantic delta embed";

struct Fixture {
    _home: tempfile::TempDir,
    home: PathBuf,
    data_dir: PathBuf,
    codex_home: PathBuf,
}

fn cass(fixture: &Fixture, args: &[&str], env: &[(&str, &str)]) -> Command {
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
    for (key, value) in env {
        cmd.env(key, value);
    }
    cmd
}

fn run(fixture: &Fixture, label: &str, args: &[&str], env: &[(&str, &str)], t: Duration) -> Output {
    spawn_with_timeout_or_diag(cass(fixture, args, env), label, Some(&fixture.data_dir), t)
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn vector_index_file(data_dir: &Path) -> Result<PathBuf, String> {
    let dir = data_dir.join("vector_index");
    let entries = std::fs::read_dir(&dir).map_err(|e| format!("read {}: {e}", dir.display()))?;
    entries
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "fsvi"))
        .ok_or_else(|| format!("no .fsvi under {}", dir.display()))
}

fn semantic_hits(fixture: &Fixture, label: &str, keyword: &str) -> Result<u64, String> {
    let out = run(
        fixture,
        label,
        &["search", keyword, "--json", "--mode", "semantic"],
        &[],
        SEARCH_TIMEOUT,
    );
    let value: Value = serde_json::from_str(text(&out.stdout).trim()).map_err(|e| {
        format!(
            "{label}: search stdout not JSON: {e}; stderr: {}",
            text(&out.stderr)
        )
    })?;
    value
        .get("total_matches")
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("{label}: total_matches missing: {value}"))
}

fn check() -> Result<(), String> {
    let home = tempfile::tempdir().map_err(|e| format!("tempdir: {e}"))?;
    let home_path = home.path().to_path_buf();
    let data_dir = home_path.join("cass-data");
    std::fs::create_dir_all(&data_dir).map_err(|e| format!("data dir: {e}"))?;
    let codex_home = home_path.join(".codex");
    util::seed_codex_session(
        &codex_home,
        "rollout-2026-04-23T10-00-00-delta-one.jsonl",
        FIRST_KEYWORD,
        true,
    );
    let fixture = Fixture {
        _home: home,
        home: home_path,
        data_dir,
        codex_home,
    };

    // 1. Build lexical + semantic artifacts for the first session.
    let full = run(
        &fixture,
        "delta_gate_full_semantic",
        &[
            "index",
            "--full",
            "--semantic",
            "--embedder",
            "hash",
            "--json",
            "--no-progress-events",
        ],
        &[],
        INDEX_TIMEOUT,
    );
    let full_json: Value = serde_json::from_str(text(&full.stdout).trim()).map_err(|e| {
        format!(
            "full index stdout not JSON: {e}; stderr: {}",
            text(&full.stderr)
        )
    })?;
    if full_json.get("success").and_then(Value::as_bool) != Some(true) {
        return Err(format!("full semantic index did not succeed: {full_json}"));
    }
    let fsvi = vector_index_file(&fixture.data_dir)?;
    let before_len = std::fs::metadata(&fsvi)
        .map_err(|e| format!("stat fsvi: {e}"))?
        .len();
    if semantic_hits(&fixture, "semantic_first_before", FIRST_KEYWORD)? < 1 {
        return Err("first session not semantically searchable after full build".to_string());
    }

    // 2. A second session arrives: the watermark now trails the corpus.
    util::seed_codex_session(
        &fixture.codex_home,
        "rollout-2026-04-24T10-00-00-delta-two.jsonl",
        SECOND_KEYWORD,
        true,
    );

    // 3. Plain one-shot `index --semantic` (human mode so RUST_LOG=info reaches
    //    stderr) must take the delta path.
    let delta = run(
        &fixture,
        "delta_gate_one_shot",
        &[
            "index",
            "--semantic",
            "--embedder",
            "hash",
            "--no-progress-events",
        ],
        &[("RUST_LOG", "info")],
        INDEX_TIMEOUT,
    );
    let delta_err = text(&delta.stderr);
    if !delta.status.success() {
        return Err(format!(
            "one-shot semantic index failed; stderr: {delta_err}"
        ));
    }
    if !delta_err.contains(DELTA_MARKER) {
        return Err(format!(
            "one-shot index --semantic did not take the delta path (marker '{DELTA_MARKER}' absent); stderr: {delta_err}"
        ));
    }
    if delta_err.contains("falling back to the bulk semantic pass") {
        return Err(format!(
            "delta embed fell back to bulk; stderr: {delta_err}"
        ));
    }

    // 4. Artifact appended (same file, not smaller) and both sessions searchable.
    let fsvi_after = vector_index_file(&fixture.data_dir)?;
    if fsvi_after.cmp(&fsvi).is_ne() {
        return Err(format!(
            "vector index was replaced ({} -> {}), expected an in-place append",
            fsvi.display(),
            fsvi_after.display()
        ));
    }
    let after_len = std::fs::metadata(&fsvi_after)
        .map_err(|e| format!("stat fsvi: {e}"))?
        .len();
    if after_len < before_len {
        return Err(format!(
            "vector index shrank after delta embed: {before_len} -> {after_len}"
        ));
    }
    if semantic_hits(&fixture, "semantic_second_after", SECOND_KEYWORD)? < 1 {
        return Err("second session not semantically searchable after delta embed".to_string());
    }
    if semantic_hits(&fixture, "semantic_first_after", FIRST_KEYWORD)? < 1 {
        return Err("first session lost from semantic search after delta embed".to_string());
    }
    Ok(())
}

#[test]
fn one_shot_semantic_index_embeds_only_the_delta_when_watermark_trails() -> Result<(), String> {
    check()
}

#[test]
fn gh470_covered_watermark_rebuilds_old_prefix_and_reuses_current_passages()
-> Result<(), Box<dyn std::error::Error>> {
    use coding_agent_search::indexer::semantic::HASH_VECTOR_SPACE_REVISION;
    use coding_agent_search::search::canonicalize::{canonicalize_for_embedding, content_hash};
    use coding_agent_search::search::embedder::Embedder;
    use coding_agent_search::search::hash_embedder::HashEmbedder;
    use coding_agent_search::search::policy::CHUNKING_STRATEGY_VERSION;
    use coding_agent_search::search::semantic_manifest::SemanticManifest;
    use coding_agent_search::search::vector_index::{
        Quantization, SemanticDocId, VectorIndex, parse_semantic_doc_id, vector_index_path,
    };
    use coding_agent_search::storage::sqlite::FrankenStorage;

    type TestResult<T> = Result<T, Box<dyn std::error::Error>>;
    const SKIP_MARKER: &str = "skipping bulk semantic re-embed: watermark already covers";
    const SOURCE_NAME: &str = "rollout-2026-04-23T10-00-00-passage-migration.jsonl";

    let home = tempfile::tempdir()?;
    let home_path = home.path().to_path_buf();
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(home_path.join(".env"))?;
    let fixture = Fixture {
        _home: home,
        data_dir: home_path.join("cass-data"),
        codex_home: home_path.join(".codex"),
        home: home_path,
    };
    std::fs::create_dir_all(&fixture.data_dir)?;
    let source = format!(
        "{} The antimeridian longitude correction preserves crossing route geometry.",
        "Review Unicode café diagnostics and retain every original source location. ".repeat(100)
    );
    util::seed_codex_session(&fixture.codex_home, SOURCE_NAME, &source, false);
    let source_path = fixture
        .codex_home
        .join("sessions/2026/04/23")
        .join(SOURCE_NAME);
    let source_bytes = std::fs::read(&source_path)?;
    let run_index = |label: &str, full: bool| -> TestResult<String> {
        let mut args = vec![
            "index",
            "--semantic",
            "--embedder",
            "hash",
            "--no-progress-events",
        ];
        if full {
            args.push("--full");
        }
        let mut command = cass(&fixture, &args, &[]);
        command.env_clear();
        for key in ["PATH", "SystemRoot", "WINDIR"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        command
            .env("HOME", &fixture.home)
            .env("USERPROFILE", &fixture.home)
            .env("XDG_CONFIG_HOME", fixture.home.join("xdg-config"))
            .env("XDG_DATA_HOME", fixture.home.join("xdg-data"))
            .env("XDG_CACHE_HOME", fixture.home.join("xdg-cache"))
            .env("CLAUDE_CONFIG_DIR", fixture.home.join(".claude"))
            .env("CODEX_HOME", &fixture.codex_home)
            .env("TUI_HEADLESS", "1")
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .env("CASS_RESPONSIVENESS_DISABLE", "1")
            .env("RUST_MIN_STACK", "134217728")
            .env("NO_COLOR", "1")
            .env("RUST_LOG", "info");
        let output =
            spawn_with_timeout_or_diag(command, label, Some(&fixture.data_dir), INDEX_TIMEOUT);
        assert!(
            output.status.success(),
            "{label}: stdout: {}\nstderr: {}",
            text(&output.stdout),
            text(&output.stderr)
        );
        Ok(text(&output.stderr))
    };
    run_index("gh470_migration_initial_build", true)?;

    let db_path = fixture.data_dir.join("agent_search.db");
    let canonical_snapshot = || -> TestResult<(Value, Option<i64>)> {
        let storage = FrankenStorage::open_readonly(&db_path)?;
        let mut conversations = storage.list_conversations(i64::MAX, 0)?;
        for conversation in &mut conversations {
            conversation.messages = storage
                .fetch_messages(conversation.id.ok_or("missing canonical conversation ID")?)?;
        }
        Ok((
            serde_json::to_value(conversations)?,
            storage.get_last_embedded_message_id()?,
        ))
    };
    let before = canonical_snapshot()?;
    assert_eq!(before.0.as_array().ok_or("conversation array")?.len(), 1);
    let messages = before.0[0]["messages"].as_array().ok_or("message array")?;
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["content"], source);
    let message_id = messages[0]["id"].as_u64().ok_or("canonical message ID")?;
    assert_eq!(before.1, Some(i64::try_from(message_id)?));

    let fsvi_path = vector_index_path(&fixture.data_dir, "fnv1a-384");
    let vector_snapshot = || -> TestResult<Vec<(SemanticDocId, Vec<u32>)>> {
        let index = VectorIndex::open(&fsvi_path)?;
        assert_eq!(index.embedder_id(), "fnv1a-384");
        assert_eq!(index.embedder_revision(), HASH_VECTOR_SPACE_REVISION);
        assert_eq!(index.dimension(), 384);
        assert_eq!(index.wal_record_count(), 0);
        assert_eq!(index.tombstone_count(), 0);
        let mut records = Vec::new();
        for ordinal in 0..index.record_count() {
            let id = parse_semantic_doc_id(index.doc_id_at(ordinal)?)
                .ok_or("invalid semantic document ID")?;
            assert_eq!(id.message_id, message_id);
            let vector = index.vector_at_f32(ordinal)?;
            assert!(vector.iter().all(|value| value.is_finite()));
            records.push((id, vector.into_iter().map(f32::to_bits).collect()));
        }
        records.sort_by_key(|(id, _)| id.chunk_idx);
        Ok(records)
    };
    let expected = vector_snapshot()?;
    assert_eq!(expected.len(), 8);
    assert_eq!(
        expected
            .iter()
            .map(|(id, _)| id.chunk_idx)
            .collect::<Vec<_>>(),
        (0..8).collect::<Vec<u8>>()
    );

    // Replace only the derived artifact with an actual historical prefix
    // embedding. The canonical source and covered watermark remain intact.
    let prefix = canonicalize_for_embedding(&source);
    assert!(!prefix.contains("antimeridian"));
    let legacy_id = SemanticDocId {
        content_hash: Some(content_hash(&prefix)),
        ..expected[0].0
    };
    let legacy_vector = HashEmbedder::default().embed_sync(&prefix)?;
    let mut legacy = VectorIndex::create_with_revision(
        &fsvi_path,
        "fnv1a-384",
        "hash-fnv1a-modular-v1",
        384,
        Quantization::F16,
    )?;
    legacy.write_record(&legacy_id.to_doc_id_string(), &legacy_vector)?;
    legacy.finish()?;
    let mut manifest = SemanticManifest::load(&fixture.data_dir)?.ok_or("missing manifest")?;
    let artifact = manifest.fast_tier.as_mut().ok_or("missing fast artifact")?;
    artifact.chunking_version = 1;
    artifact.doc_count = 1;
    artifact.size_bytes = std::fs::metadata(&fsvi_path)?.len();
    manifest.save(&fixture.data_dir)?;
    let old = VectorIndex::open(&fsvi_path)?;
    assert_eq!(old.embedder_revision(), "hash-fnv1a-modular-v1");
    assert_eq!(old.record_count(), 1);
    assert_eq!(old.doc_id_at(0)?, legacy_id.to_doc_id_string());
    drop(old);
    assert_eq!(canonical_snapshot()?, before);
    assert_eq!(std::fs::read(&source_path)?, source_bytes);

    let migrated = run_index("gh470_migration_covered_watermark", false)?;
    assert!(
        migrated.contains("starting semantic indexing"),
        "{migrated}"
    );
    assert!(!migrated.contains(SKIP_MARKER), "{migrated}");
    assert_eq!(vector_snapshot()?, expected);
    let manifest = SemanticManifest::load(&fixture.data_dir)?.ok_or("missing new manifest")?;
    let artifact = manifest.fast_tier.ok_or("missing new fast artifact")?;
    assert_eq!(artifact.chunking_version, CHUNKING_STRATEGY_VERSION);
    assert_eq!(artifact.doc_count, 8);
    assert!(artifact.ready);
    assert_eq!(canonical_snapshot()?, before);
    assert_eq!(std::fs::read(&source_path)?, source_bytes);

    let current_bytes = std::fs::read(&fsvi_path)?;
    let unchanged = run_index("gh470_migration_current_unchanged", false)?;
    assert!(unchanged.contains(SKIP_MARKER), "{unchanged}");
    assert!(
        !unchanged.contains("starting semantic indexing"),
        "{unchanged}"
    );
    assert_eq!(std::fs::read(&fsvi_path)?, current_bytes);
    assert_eq!(canonical_snapshot()?, before);
    assert_eq!(std::fs::read(&source_path)?, source_bytes);
    Ok(())
}
