//! Bead coding_agent_session_search-0a8y3 (child of ibuuh.10):
//! E2E regression that the "explicit `--mode hybrid` fails open to
//! lexical when semantic assets are absent" contract from commit
//! 86c88d0b holds on a freshly-built corpus.
//!
//! The sibling test
//! `tests/cli_robot.rs::search_robot_meta_reports_explicit_hybrid_fail_open`
//! exercises the same contract against the committed
//! `tests/fixtures/search_demo_data` snapshot. This test complements
//! that coverage by:
//!   - Building the canonical DB AND the lexical index fresh from
//!     seeded Codex sessions (so a schema or pipeline regression
//!     that only affects fresh-build corpora is caught here).
//!   - Isolating HOME / XDG_DATA_HOME / XDG_CONFIG_HOME / CODEX_HOME
//!     to a tempdir so the test doesn't pollute or read the user's
//!     real session corpus.
//!   - Setting CASS_IGNORE_SOURCES_CONFIG=1 so the indexer doesn't
//!     pick up the operator's real `~/.config/cass/sources.toml`.

use assert_cmd::Command;
use coding_agent_search::franken_sync::compat::{ConnectionExt, ParamValue, RowExt};
use coding_agent_search::indexer::semantic::{
    EmbeddingInput, SemanticIndexer, SemanticShardBuildPlan,
};
use coding_agent_search::search::semantic_manifest::{SemanticShardManifest, TierKind};
use coding_agent_search::storage::sqlite::FrankenStorage;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::TempDir;
use walkdir::WalkDir;

mod util;

fn cass_cmd(temp_home: &std::path::Path) -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");
    cmd.env("CASS_IGNORE_SOURCES_CONFIG", "1");
    cmd.env("HOME", temp_home);
    cmd.env("XDG_DATA_HOME", temp_home.join(".local/share"));
    cmd.env("XDG_CONFIG_HOME", temp_home.join(".config"));
    cmd.env("CODEX_HOME", temp_home.join(".codex"));
    // Provider overrides and CWD discovery precede HOME for some connectors.
    // Keep concurrent fleet tests and the worker's own sessions out of fixtures.
    cmd.current_dir(temp_home);
    cmd.env("CLAUDE_HOME", temp_home.join(".claude"));
    cmd.env("GEMINI_HOME", temp_home.join(".gemini"));
    cmd.env("OPENCODE_STORAGE_ROOT", temp_home.join(".opencode"));
    cmd.env("CASS_AIDER_DATA_ROOT", temp_home.join(".aider-missing"));
    cmd.env("PI_SESSIONS_DIR", temp_home.join(".pi-sessions-missing"));
    cmd.env("PI_CODING_AGENT_DIR", temp_home.join(".pi-agent-missing"));
    cmd.env(
        "PI_CODING_AGENT_SESSION_DIR",
        temp_home.join(".pi-coding-agent-sessions-missing"),
    );
    cmd.env_remove("PI_CONFIG_DIR");
    cmd.env_remove("PI_PROFILE");
    cmd.env("CASS_AUTO_REFRESH", "0");
    cmd.env("CASS_DAEMON_SOCKET", temp_home.join("cass-daemon.sock"));
    cmd
}

fn seed_codex_session(codex_home: &std::path::Path, filename: &str, keyword: &str) {
    // Full user + assistant corpus so the post-index search has
    // content to match on either turn.
    util::seed_codex_session(codex_home, filename, keyword, true);
}

/// GH422's original route is a worker inside ordinary search, not the detached
/// background indexer. Reuse the existing post-commit pause to freeze that
/// worker while its real heartbeat and watchdog continue. This tiny corpus
/// proves containment and cold recovery, not reporter-scale rebuild latency.
#[cfg(unix)]
#[test]
fn gh422_search_refresh_stall_releases_lock_and_recovers_cold_query() {
    use coding_agent_search::model::types::{Agent, AgentKind};
    use fs2::FileExt;
    use std::process::{Child, Stdio};
    use std::time::Instant;

    // A harness deadline is a failure. Reap the child on every unwind and put
    // both captured streams in the test output before the fixture is dropped.
    struct SearchChild {
        child: Child,
        stdout: PathBuf,
        stderr: PathBuf,
    }
    impl Drop for SearchChild {
        fn drop(&mut self) {
            if !matches!(self.child.try_wait(), Ok(Some(_))) {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
            if std::thread::panicking() {
                eprintln!(
                    "search stdout: {}",
                    fs::read_to_string(&self.stdout).unwrap_or_default()
                );
                eprintln!(
                    "search stderr: {}",
                    fs::read_to_string(&self.stderr).unwrap_or_default()
                );
            }
        }
    }

    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    // dotenvy must not discover an ancestor's real configuration.
    fs::File::options()
        .write(true)
        .create_new(true)
        .open(home.join(".env"))
        .unwrap();
    let data_dir = home.join("cass_data");
    fs::create_dir(&data_dir).unwrap();
    let db_path = data_dir.join("agent_search.db");
    let storage = FrankenStorage::open(&db_path).unwrap();
    let agent = storage
        .ensure_agent(&Agent {
            id: None,
            slug: "codex".into(),
            name: "Codex".into(),
            version: None,
            kind: AgentKind::Cli,
        })
        .unwrap();
    let mut canonical = Vec::new();
    let mut expected_hits = std::collections::BTreeSet::new();
    for ordinal in 0..2 {
        let path = home.join(format!("retained-{ordinal}.jsonl"));
        let conversation = util::ConversationFixtureBuilder::new("codex")
            .external_id(format!("gh422-{ordinal}"))
            .source_path(&path)
            .messages(1)
            .with_content(
                0,
                format!("gh422stalledneedle retained conversation {ordinal}"),
            )
            .build_conversation();
        let inserted = storage
            .insert_conversation_tree(agent, None, &conversation)
            .unwrap();
        assert_eq!(inserted.inserted_indices, vec![0]);
        canonical.push((
            inserted.conversation_id,
            serde_json::to_value(storage.fetch_messages(inserted.conversation_id).unwrap())
                .unwrap(),
        ));
        expected_hits.insert((path.to_string_lossy().into_owned(), 1_u64));
    }
    storage.close().unwrap();
    let index_path = coding_agent_search::search::tantivy::expected_index_dir(&data_dir);
    assert!(!coding_agent_search::search::tantivy::searchable_index_exists(&index_path));

    let sentinel = home.join("committed-before-checkpoint.json");
    let stdout_path = home.join("stalled-search.stdout");
    let stderr_path = home.join("stalled-search.stderr");
    let mut command = std::process::Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    command
        .current_dir(home)
        .env_clear()
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("RUST_MIN_STACK", "134217728")
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("CASS_DATA_DIR", &data_dir)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("CASS_AUTO_REFRESH", "0")
        .env("CASS_INDEX_STALL_DETECT_SECS", "5")
        .env("CASS_INDEX_STALL_ABORT_SECS", "20")
        .env("CASS_INDEX_FINALIZE_ABORT_SECS", "20")
        .env("CASS_INDEX_STALL_ABORT_ALL_PHASES", "0")
        .env(
            "CASS_TEST_LEXICAL_REBUILD_KILL_AFTER_COMMIT_SENTINEL",
            &sentinel,
        )
        .env("CASS_TEST_LEXICAL_REBUILD_KILL_AFTER_COMMITS", "1")
        .env(
            "CASS_TEST_LEXICAL_REBUILD_KILL_AFTER_COMMIT_SLEEP_MS",
            "60000",
        )
        .env("CASS_TANTIVY_REBUILD_BATCH_FETCH_CONVERSATIONS", "1")
        .env(
            "CASS_TANTIVY_REBUILD_INITIAL_BATCH_FETCH_CONVERSATIONS",
            "1",
        )
        .env("CASS_TANTIVY_REBUILD_COMMIT_EVERY_CONVERSATIONS", "1")
        .env(
            "CASS_TANTIVY_REBUILD_INITIAL_COMMIT_EVERY_CONVERSATIONS",
            "1",
        )
        .args([
            "search",
            "gh422stalledneedle",
            "--json",
            "--no-daemon",
            "--mode",
            "lexical",
            "--timeout",
            "120000",
            "--limit",
            "10",
            "--data-dir",
        ])
        .arg(&data_dir)
        .stdout(Stdio::from(fs::File::create(&stdout_path).unwrap()))
        .stderr(Stdio::from(fs::File::create(&stderr_path).unwrap()));
    let mut search = SearchChild {
        child: command.spawn().expect("spawn ordinary search"),
        stdout: stdout_path,
        stderr: stderr_path,
    };
    let startup_deadline = Instant::now() + Duration::from_secs(60);
    let committed: Value = loop {
        if let Ok(bytes) = fs::read(&sentinel)
            && let Ok(value) = serde_json::from_slice(&bytes)
        {
            break value;
        }
        assert!(
            search.child.try_wait().unwrap().is_none(),
            "search exited before a real commit"
        );
        assert!(
            Instant::now() < startup_deadline,
            "search never reached the commit pause"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    let paused_at = Instant::now();
    let paused_wall_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    assert_eq!(committed["pid"], search.child.id());
    assert_eq!(
        committed["stage"],
        "staged_commit_published_checkpoint_not_yet_written"
    );
    assert!(committed["committed_indexed_docs"].as_u64().unwrap() > 0);
    assert!(
        committed["committed_indexed_docs"].as_u64().unwrap()
            > committed["checkpoint_indexed_docs"].as_u64().unwrap()
    );
    assert_eq!(
        Path::new(committed["state_path"].as_str().unwrap()),
        index_path
    );
    let paused_checkpoint = lexical_checkpoint(&data_dir);
    assert_eq!(paused_checkpoint["completed"], false);
    assert_eq!(
        paused_checkpoint["indexed_docs"],
        committed["checkpoint_indexed_docs"]
    );
    assert_eq!(
        paused_checkpoint["pending"]["indexed_docs"],
        committed["committed_indexed_docs"]
    );

    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(data_dir.join("index-run.lock"))
        .unwrap();
    assert_eq!(
        FileExt::try_lock_exclusive(&lock).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    let mut first_heartbeat = None;
    let mut heartbeat_advanced_without_progress = false;
    let exit = loop {
        if let Some(status) = search.child.try_wait().unwrap() {
            break status;
        }
        assert!(
            paused_at.elapsed() < Duration::from_secs(35),
            "watchdog did not exit before the 60-second pause; harness cleanup is not a pass"
        );
        if let Ok(metadata) = fs::read_to_string(data_dir.join("index-run.lock.meta")) {
            let fields: BTreeMap<_, _> = metadata
                .lines()
                .filter_map(|line| line.split_once('='))
                .collect();
            if let (Some(pid), Some(kind), Some(updated), Some(progress)) = (
                fields.get("pid"),
                fields.get("job_kind"),
                fields.get("updated_at_ms"),
                fields.get("last_progress_at_ms"),
            ) {
                assert_eq!(pid.parse::<u32>().unwrap(), search.child.id());
                assert_eq!(*kind, "lexical_refresh");
                let sample = (
                    updated.parse::<u64>().unwrap(),
                    progress.parse::<u64>().unwrap(),
                );
                // The first post-pause heartbeat incorporates the worker's
                // last pre-pause atomic bump; older metadata can still lag it.
                if sample.0 < paused_wall_ms {
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
                if let Some((old_updated, old_progress)) = first_heartbeat {
                    assert_eq!(
                        sample.1, old_progress,
                        "real progress advanced during the commit pause"
                    );
                    heartbeat_advanced_without_progress |= sample.0 >= old_updated + 2_000;
                } else {
                    first_heartbeat = Some(sample);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(
        heartbeat_advanced_without_progress,
        "must observe a live heartbeat with frozen real progress"
    );
    assert_eq!(
        exit.code(),
        Some(70),
        "only the watchdog's actual process exit satisfies this test"
    );
    assert!(paused_at.elapsed() < Duration::from_secs(35));
    let stderr = fs::read_to_string(&search.stderr).unwrap();
    let envelope = stderr
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|value| value["kind"] == "index-stalled")
        .expect("watchdog error envelope");
    assert_eq!(envelope["code"], 70);
    assert_eq!(envelope["success"], false);
    assert_eq!(envelope["retryable"], true);
    assert_eq!(envelope["abort_threshold_secs"], 20);
    assert!(envelope["stall_elapsed_ms"].as_u64().unwrap() >= 20_000);
    FileExt::try_lock_exclusive(&lock).expect("watchdog exit must release the real flock");
    FileExt::unlock(&lock).unwrap();
    let interrupted_checkpoint = lexical_checkpoint(&data_dir);
    assert_eq!(interrupted_checkpoint["completed"], false);
    assert_eq!(
        interrupted_checkpoint["indexed_docs"],
        paused_checkpoint["indexed_docs"]
    );
    assert_eq!(
        interrupted_checkpoint["pending"], paused_checkpoint["pending"],
        "the watchdog must leave the interrupted durable commit recoverable"
    );

    // A fresh process gets no pause settings and must repair/search the same
    // canonical rows. Its normal partial-timeout contract remains unchanged:
    // a timed-out empty response cannot satisfy this positive recovery check.
    command
        .env_remove("CASS_TEST_LEXICAL_REBUILD_KILL_AFTER_COMMIT_SENTINEL")
        .env_remove("CASS_TEST_LEXICAL_REBUILD_KILL_AFTER_COMMITS")
        .env_remove("CASS_TEST_LEXICAL_REBUILD_KILL_AFTER_COMMIT_SLEEP_MS")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = Command::from_std(command)
        .timeout(Duration::from_secs(135))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "cold search failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let payload: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_ne!(payload["budget"]["timed_out"], true, "{payload}");
    let hits = payload["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 2, "{payload}");
    let actual_hits = hits
        .iter()
        .map(|hit| {
            (
                hit["source_path"].as_str().unwrap().to_string(),
                hit["line_number"].as_u64().unwrap(),
            )
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        actual_hits, expected_hits,
        "cold repair must retain both identities exactly once"
    );
    let repaired_checkpoint = lexical_checkpoint(&data_dir);
    assert_eq!(repaired_checkpoint["completed"], true);
    assert_eq!(repaired_checkpoint["indexed_docs"], 2);
    assert!(repaired_checkpoint["pending"].is_null());
    assert!(
        coding_agent_search::search::tantivy::searchable_index_exists(&index_path),
        "cold recovery must publish readable lexical assets, not only serve a SQLite fallback"
    );
    let storage = FrankenStorage::open_readonly(&db_path).unwrap();
    assert_eq!(storage.list_conversations(10, 0).unwrap().len(), 2);
    for (id, messages) in canonical {
        assert_eq!(
            serde_json::to_value(storage.fetch_messages(id).unwrap()).unwrap(),
            messages
        );
    }
    storage.close().unwrap();
}

#[derive(Debug, PartialEq, Eq)]
enum DataTreeEntry {
    Directory,
    File {
        size_bytes: usize,
        digest: blake3::Hash,
    },
    Symlink(PathBuf),
}

fn data_tree_snapshot(root: &Path) -> BTreeMap<PathBuf, DataTreeEntry> {
    WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .map(|entry| entry.unwrap_or_else(|err| panic!("walk {}: {err}", root.display())))
        .map(|entry| {
            let relative = entry
                .path()
                .strip_prefix(root)
                .unwrap_or_else(|err| {
                    panic!(
                        "strip data-dir prefix {} from {}: {err}",
                        root.display(),
                        entry.path().display()
                    )
                })
                .to_path_buf();
            let value =
                if entry.file_type().is_dir() {
                    DataTreeEntry::Directory
                } else if entry.file_type().is_symlink() {
                    DataTreeEntry::Symlink(fs::read_link(entry.path()).unwrap_or_else(|err| {
                        panic!("read symlink {}: {err}", entry.path().display())
                    }))
                } else {
                    let bytes = fs::read(entry.path()).unwrap_or_else(|err| {
                        panic!("read data-tree file {}: {err}", entry.path().display())
                    });
                    DataTreeEntry::File {
                        size_bytes: bytes.len(),
                        digest: blake3::hash(&bytes),
                    }
                };
            (relative, value)
        })
        .collect()
}

fn run_fresh_index(home: &Path, data_dir: &Path) {
    let mut index = cass_cmd(home);
    index
        .args(["index", "--full", "--json", "--data-dir"])
        .arg(data_dir)
        .timeout(Duration::from_secs(120));
    let index_output = index.output().expect("run cass index --full");
    assert!(
        index_output.status.success(),
        "cass index --full must succeed on the seeded corpus. stdout: {} stderr: {}",
        String::from_utf8_lossy(&index_output.stdout),
        String::from_utf8_lossy(&index_output.stderr)
    );
}

fn run_forced_full_index(home: &Path, data_dir: &Path) {
    let mut index = cass_cmd(home);
    index
        .args(["index", "--full", "--force-rebuild", "--json", "--data-dir"])
        .arg(data_dir)
        .timeout(Duration::from_secs(120));
    let output = index
        .output()
        .expect("run cass index --full --force-rebuild");
    assert!(
        output.status.success(),
        "forced full rebuild must succeed on the seeded corpus. stdout: {} stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let checkpoint = lexical_checkpoint(data_dir);
    assert_eq!(
        checkpoint.get("completed").and_then(Value::as_bool),
        Some(true),
        "a successful forced full rebuild must leave a completed lexical checkpoint"
    );
    let index_path = coding_agent_search::search::tantivy::expected_index_dir(data_dir);
    assert!(
        coding_agent_search::search::tantivy::searchable_index_exists(&index_path),
        "a successful forced full rebuild must leave a readable lexical generation"
    );
}

fn lexical_checkpoint(data_dir: &Path) -> Value {
    let checkpoint_path = coding_agent_search::search::tantivy::expected_index_dir(data_dir)
        .join(".lexical-rebuild-state.json");
    let body = fs::read(&checkpoint_path).unwrap_or_else(|err| {
        panic!(
            "read completed lexical checkpoint {}: {err}",
            checkpoint_path.display()
        )
    });
    serde_json::from_slice(&body).unwrap_or_else(|err| {
        panic!(
            "parse completed lexical checkpoint {}: {err}",
            checkpoint_path.display()
        )
    })
}

fn semantic_inputs_from_db(db_path: &Path) -> Vec<EmbeddingInput> {
    let storage = FrankenStorage::open_readonly(db_path).unwrap_or_else(|err| {
        panic!("open seeded cass DB {}: {err}", db_path.display());
    });
    let empty: &[ParamValue] = &[];
    let rows: Vec<(i64, i64, String)> = storage
        .raw()
        .query_map_collect(
            "SELECT id, COALESCE(created_at, 0), content
             FROM messages
             ORDER BY id ASC",
            empty,
            |row| Ok((row.get_typed(0)?, row.get_typed(1)?, row.get_typed(2)?)),
        )
        .unwrap_or_else(|err| {
            panic!(
                "load semantic message inputs from {}: {err}",
                db_path.display()
            )
        });

    rows.into_iter()
        .map(|(message_id, created_at_ms, content)| {
            let mut input = EmbeddingInput::new(
                u64::try_from(message_id).expect("cass message ids must be positive"),
                content,
            );
            input.created_at_ms = created_at_ms;
            input
        })
        .collect()
}

fn build_hash_semantic_assets(data_dir: &Path, sharded: bool) {
    let checkpoint = lexical_checkpoint(data_dir);
    assert_eq!(
        checkpoint.get("completed").and_then(Value::as_bool),
        Some(true),
        "semantic assets must be built against a completed lexical generation"
    );
    let db_fingerprint = checkpoint
        .get("db")
        .and_then(|db| db.get("storage_fingerprint"))
        .and_then(Value::as_str)
        .unwrap_or_else(|| {
            panic!("lexical checkpoint must carry db.storage_fingerprint: {checkpoint}")
        })
        .to_string();
    let total_conversations = checkpoint
        .get("db")
        .and_then(|db| db.get("total_conversations"))
        .and_then(Value::as_u64)
        .unwrap_or_else(|| {
            panic!("lexical checkpoint must carry db.total_conversations: {checkpoint}")
        });

    let db_path = data_dir.join("agent_search.db");
    let inputs = semantic_inputs_from_db(&db_path);
    assert!(
        inputs.len() >= 4,
        "shard proof needs several semantic docs; inputs: {}",
        inputs.len()
    );

    let indexer = SemanticIndexer::new("hash", Some(data_dir))
        .unwrap_or_else(|err| panic!("construct hash semantic indexer: {err}"));
    let embedded = indexer
        .embed_messages(&inputs)
        .unwrap_or_else(|err| panic!("embed seeded messages: {err}"));

    if sharded {
        let outcome = indexer
            .build_and_save_index_shards(
                embedded,
                data_dir,
                SemanticShardBuildPlan {
                    tier: TierKind::Fast,
                    db_fingerprint,
                    model_revision: "hash".to_string(),
                    total_conversations,
                    max_records_per_shard: 2,
                    build_ann: false,
                },
            )
            .unwrap_or_else(|err| panic!("build semantic shard generation: {err}"));
        assert!(
            outcome.complete,
            "published shard generation must be complete: {outcome:?}"
        );
        assert!(
            outcome.shard_count > 1,
            "test must exercise multi-shard loading, got {outcome:?}"
        );
    } else {
        let index = indexer
            .build_and_save_index(embedded, data_dir)
            .unwrap_or_else(|err| panic!("build monolithic semantic index: {err}"));
        assert_eq!(
            index.record_count(),
            inputs.len(),
            "monolithic semantic index should contain every embedded message"
        );
    }
}

fn mark_first_semantic_shard_not_ready(data_dir: &Path) {
    let mut manifest = SemanticShardManifest::load(data_dir)
        .unwrap_or_else(|err| panic!("load semantic shard manifest: {err}"))
        .unwrap_or_else(|| {
            panic!(
                "semantic shard manifest should exist under {}",
                data_dir.display()
            )
        });
    let shard = manifest
        .shards
        .iter_mut()
        .find(|shard| shard.ready)
        .unwrap_or_else(|| panic!("semantic shard manifest should contain a ready shard"));
    shard.ready = false;
    manifest
        .save(data_dir)
        .unwrap_or_else(|err| panic!("save incomplete semantic shard manifest: {err}"));
}

fn run_hybrid_hash_search(home: &Path, data_dir: &Path, query: &str) -> Value {
    let mut search = cass_cmd(home);
    search
        .args([
            "search",
            query,
            "--json",
            "--robot-meta",
            "--mode",
            "hybrid",
            "--model",
            "hash",
            "--limit",
            "10",
            "--data-dir",
        ])
        .arg(data_dir);
    let output = search.output().expect("run cass hybrid hash search");
    assert!(
        output.status.success(),
        "hybrid hash search must succeed for {}. stdout: {}\nstderr: {}",
        data_dir.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice::<Value>(&output.stdout).unwrap_or_else(|err| {
        panic!(
            "hybrid hash search output must be valid JSON for {}: {err}\nstdout: {}",
            data_dir.display(),
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

#[test]
fn explicit_semantic_budget_pressure_never_realizes_lexical() {
    let tmp = TempDir::new().expect("create isolated semantic-budget home");
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).expect("create isolated semantic-budget data dir");

    for idx in 1..=3 {
        let name = format!("rollout-explicit-semantic-budget-{idx:02}.jsonl");
        seed_codex_session(
            &codex_home,
            &name,
            &format!("semanticbudgetprobe concept {idx}"),
        );
    }
    run_fresh_index(home, &data_dir);
    build_hash_semantic_assets(&data_dir, false);

    // A one-millisecond budget is deterministically at least NearLimit even
    // at elapsed_ms=0 because the integer 80% threshold rounds down to zero.
    // Hash is an explicit production control vector space here: it makes the
    // requested semantic path fully usable without a model download, so a
    // missing-model error cannot accidentally make this regression pass.
    let mut search = cass_cmd(home);
    search
        .args([
            "search",
            "semanticbudgetprobe",
            "--json",
            "--robot-meta",
            "--mode",
            "semantic",
            "--model",
            "hash",
            "--timeout",
            "1",
            "--limit",
            "5",
            "--data-dir",
        ])
        .arg(&data_dir);
    let output = search
        .output()
        .expect("run explicit semantic search under robot budget pressure");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let emitted_bytes = output.stdout.len().saturating_add(output.stderr.len());
    let emitted_lines = stdout
        .lines()
        .count()
        .saturating_add(stderr.lines().count());
    assert!(
        emitted_bytes < 8 * 1024 * 1024,
        "bounded semantic-budget probe emitted {emitted_bytes} bytes"
    );
    assert!(
        emitted_lines < 10_000,
        "bounded semantic-budget probe emitted {emitted_lines} lines"
    );

    if output.status.success() {
        let payload: Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|err| {
            panic!(
                "successful semantic-budget output must be valid JSON: {err}\n\
                 stdout: {stdout}\nstderr: {stderr}"
            )
        });
        let meta = payload
            .get("_meta")
            .and_then(Value::as_object)
            .unwrap_or_else(|| panic!("--robot-meta must emit _meta: {payload}"));
        assert_eq!(
            meta.get("requested_search_mode").and_then(Value::as_str),
            Some("semantic"),
            "explicit semantic intent must survive budget shedding: {payload}"
        );
        assert_eq!(
            meta.get("search_mode").and_then(Value::as_str),
            Some("semantic"),
            "budget pressure must not substitute lexical execution for explicit semantic: {payload}"
        );
        assert_ne!(
            meta.get("fallback_tier").and_then(Value::as_str),
            Some("lexical"),
            "explicit semantic must never report lexical fallback: {payload}"
        );
    } else {
        let last_line = stderr
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or_else(|| {
                panic!(
                    "failed semantic-budget search must emit a structured error; \
                     stdout: {stdout}\nstderr: {stderr}"
                )
            });
        let payload: Value = serde_json::from_str(last_line.trim()).unwrap_or_else(|err| {
            panic!(
                "failed semantic-budget output must end in a JSON error: {err}\n\
                 stdout: {stdout}\nstderr: {stderr}"
            )
        });
        let error = payload
            .get("error")
            .and_then(Value::as_object)
            .unwrap_or_else(|| {
                panic!("failed semantic-budget output must carry an error object: {payload}")
            });
        assert_eq!(
            error.get("kind").and_then(Value::as_str),
            Some("timeout"),
            "budget-shed explicit semantic search must fail with the typed timeout kind: {payload}"
        );
        assert_eq!(
            error.get("code").and_then(Value::as_i64),
            Some(10),
            "budget-shed explicit semantic search must preserve timeout exit code 10: {payload}"
        );
        assert_eq!(
            error.get("retryable").and_then(Value::as_bool),
            Some(true),
            "the same semantic request should be retryable with a larger budget: {payload}"
        );
        let hint = error
            .get("hint")
            .and_then(Value::as_str)
            .unwrap_or_else(|| {
                panic!("semantic timeout must include an actionable hint: {payload}")
            });
        assert!(
            hint.contains("Increase --timeout") && hint.contains("hybrid"),
            "semantic timeout hint must preserve semantic intent and name opt-in hybrid fail-open: {hint}"
        );
        assert!(
            !stdout.contains("\"search_mode\":\"lexical\"")
                && !stdout.contains("\"search_mode\": \"lexical\""),
            "strict semantic failure must not emit a lexical realization: {stdout}"
        );
    }
}

fn run_lexical_search(home: &Path, data_dir: &Path, query: &str) -> Value {
    let mut search = cass_cmd(home);
    search
        .args([
            "search",
            query,
            "--json",
            "--robot-meta",
            "--mode",
            "lexical",
            "--limit",
            "10",
            "--data-dir",
        ])
        .arg(data_dir);
    let output = search.output().expect("run cass lexical search");
    assert!(
        output.status.success(),
        "lexical search must succeed for {}. stdout: {}\nstderr: {}",
        data_dir.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice::<Value>(&output.stdout).unwrap_or_else(|err| {
        panic!(
            "lexical search output must be valid JSON for {}: {err}\nstdout: {}",
            data_dir.display(),
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

#[test]
fn no_maintenance_lexical_search_is_byte_stable_across_the_real_cli_path() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_no_maintenance_data");
    fs::create_dir_all(&data_dir).unwrap();

    seed_codex_session(
        &codex_home,
        "rollout-no-maintenance-byte-stability.jsonl",
        "nomaintenancebyteprobe immutable archive proof",
    );
    run_fresh_index(home, &data_dir);
    run_forced_full_index(home, &data_dir);

    let before = data_tree_snapshot(&data_dir);
    let mut search = cass_cmd(home);
    search
        .args([
            "search",
            "nomaintenancebyteprobe",
            "--json",
            "--robot-meta",
            "--mode",
            "lexical",
            "--no-maintenance",
            "--limit",
            "10",
            "--data-dir",
        ])
        .arg(&data_dir)
        .timeout(Duration::from_secs(20));
    let output = search
        .output()
        .expect("run real cass search --no-maintenance subprocess");
    assert!(
        output.status.success(),
        "strict read-only lexical search must succeed. stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let payload: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
        panic!(
            "strict read-only search output must be JSON: {err}\nstdout: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    let hits = payload
        .get("hits")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("strict read-only search must return a hits array: {payload}"));
    assert!(
        !hits.is_empty(),
        "strict read-only search needs a positive matching hit; payload: {payload}"
    );
    assert_eq!(
        payload
            .get("_meta")
            .and_then(|meta| meta.get("search_mode"))
            .and_then(Value::as_str),
        Some("lexical"),
        "robot metadata must truthfully report the realized lexical tier"
    );

    assert_eq!(
        before,
        data_tree_snapshot(&data_dir),
        "search --no-maintenance must not add, remove, or change any data-dir directory, file, symlink, SQLite sidecar, checkpoint, lock, or search artifact"
    );
}

#[test]
fn no_maintenance_hybrid_search_with_semantic_assets_is_byte_stable() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_no_maintenance_hybrid_data");
    fs::create_dir_all(&data_dir).unwrap();

    for idx in 1..=3 {
        seed_codex_session(
            &codex_home,
            &format!("rollout-no-maintenance-hybrid-{idx:02}.jsonl"),
            &format!("nomaintenancehybridprobe semantic archive proof {idx}"),
        );
    }
    run_fresh_index(home, &data_dir);
    build_hash_semantic_assets(&data_dir, true);

    let before = data_tree_snapshot(&data_dir);
    let mut search = cass_cmd(home);
    search
        .args([
            "search",
            "nomaintenancehybridprobe",
            "--json",
            "--robot-meta",
            "--mode",
            "hybrid",
            "--model",
            "hash",
            "--no-maintenance",
            "--limit",
            "10",
            "--data-dir",
        ])
        .arg(&data_dir)
        .timeout(Duration::from_secs(20));
    let output = search
        .output()
        .expect("run real strict hash-hybrid search subprocess");
    assert!(
        output.status.success(),
        "strict hash-hybrid search must succeed. stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let payload: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
        panic!(
            "strict hash-hybrid search output must be JSON: {err}\nstdout: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    assert!(
        payload
            .get("hits")
            .and_then(Value::as_array)
            .is_some_and(|hits| !hits.is_empty()),
        "strict hash-hybrid search needs a positive matching hit: {payload}"
    );
    let meta = payload
        .get("_meta")
        .unwrap_or_else(|| panic!("strict hash-hybrid search needs robot metadata: {payload}"));
    assert_eq!(
        meta.get("search_mode").and_then(Value::as_str),
        Some("hybrid")
    );
    assert_eq!(
        meta.get("semantic_refinement").and_then(Value::as_bool),
        Some(true),
        "the test must exercise live semantic DB hydration, not lexical fallback"
    );
    assert_eq!(
        before,
        data_tree_snapshot(&data_dir),
        "semantic search --no-maintenance must leave every data-dir byte and path unchanged"
    );
}

#[test]
fn structured_pack_preserves_stale_checkpoint_and_returns_real_citations() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("pack_read_only_data");
    fs::create_dir_all(&data_dir).unwrap();
    let filename = "rollout-pack-read-only.jsonl";
    seed_codex_session(&codex_home, filename, "packreadonlyneedle");
    run_fresh_index(home, &data_dir);
    let index_path = coding_agent_search::search::tantivy::expected_index_dir(&data_dir);
    let mut checkpoint = lexical_checkpoint(&data_dir);
    *checkpoint
        .pointer_mut("/db/storage_fingerprint")
        .expect("existing storage fingerprint") =
        Value::String("missing-passive-fingerprint".into());
    fs::write(
        index_path.join(".lexical-rebuild-state.json"),
        serde_json::to_vec(&checkpoint).unwrap(),
    )
    .unwrap();
    let before = data_tree_snapshot(&data_dir);
    let source_path = codex_home.join("sessions/2026/04/23").join(filename);
    let source = fs::read_to_string(&source_path).unwrap();

    let output = cass_cmd(home)
        .args([
            "pack",
            "packreadonlyneedle",
            "--json",
            "--mode",
            "lexical",
            "--require-evidence",
            "--freshness-policy",
            "allow-stale",
            "--data-dir",
        ])
        .arg(&data_dir)
        .timeout(Duration::from_secs(20))
        .output()
        .expect("run structured pack on readable stale generation");
    assert!(
        output.status.success(),
        "pack must return existing evidence: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let payload: Value = serde_json::from_slice(&output.stdout).expect("pack JSON");
    let evidence = payload["evidence"].as_array().expect("evidence array");
    assert!(!evidence.is_empty(), "pack must return useful evidence");
    for item in evidence {
        let citation = &item["citation"];
        assert_eq!(
            Path::new(citation["source_path"].as_str().expect("source path")),
            source_path
        );
        let line = citation["line_start"].as_u64().expect("citation line") as usize;
        assert!(line > 0);
        let cited = source.lines().nth(line - 1).expect("line exists in source");
        assert!(cited.contains("packreadonlyneedle"));
        assert_eq!(citation["verified"], true);
        assert_eq!(citation["line_end"], citation["line_start"]);
        assert_eq!(
            citation["span_hash"],
            blake3::hash(cited.as_bytes()).to_hex().to_string()
        );
        let citation_core = format!(
            "local\n{}\n{line}\n{line}\n{}",
            source_path.display(),
            blake3::hash(cited.as_bytes()).to_hex()
        );
        let encoded = item["id"]
            .as_str()
            .expect("evidence ID")
            .strip_prefix("ev_")
            .expect("evidence ID prefix");
        assert_eq!(encoded.len(), 52, "ID must carry the full 256-bit digest");
        // Decode the emitted base32 one bit at a time, independently of the
        // production encoder's 40-bit blocks, then compare every digest byte.
        let mut decoded = [0u8; 32];
        for (index, character) in encoded.bytes().enumerate() {
            let digit = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567"
                .iter()
                .position(|allowed| *allowed == character)
                .expect("RFC 4648 base32 character") as u8;
            for offset in 0..5 {
                let bit_index = index * 5 + offset;
                let bit = (digit >> (4 - offset)) & 1;
                if bit_index < 256 {
                    decoded[bit_index / 8] |= bit << (7 - bit_index % 8);
                } else {
                    assert_eq!(bit, 0, "base32 trailing pad bits must be zero");
                }
            }
        }
        assert_eq!(
            &decoded,
            blake3::hash(citation_core.as_bytes()).as_bytes(),
            "evidence identity must bind the actual verified source span"
        );
        assert!(citation["message_index"].is_u64());
        assert!(
            item["excerpt"]
                .as_str()
                .unwrap()
                .contains("packreadonlyneedle")
        );
    }
    assert_eq!(
        before,
        data_tree_snapshot(&data_dir),
        "structured pack must not repair checkpoints or mutate archive assets"
    );
    assert_eq!(source, fs::read_to_string(&source_path).unwrap());

    // Each command reads the real archive. Compare stable citation/evidence
    // fields; freshness age legitimately advances between invocations. TOON's
    // value model compares equivalent integer/float JSON numbers numerically.
    let stable_evidence = |value: &Value| {
        value["evidence"]
            .as_array()
            .expect("evidence array")
            .iter()
            .map(|item| {
                let mut citation = item["citation"].clone();
                citation
                    .as_object_mut()
                    .expect("citation object")
                    .remove("freshness_age_seconds");
                toon::JsonValue::from(serde_json::json!({
                    "id": item["id"],
                    "excerpt": item["excerpt"],
                    "excerpt_truncated": item["excerpt_truncated"],
                    "citation": citation,
                }))
            })
            .collect::<Vec<_>>()
    };
    let expected_evidence = stable_evidence(&payload);
    for format in ["compact", "jsonl", "toon"] {
        let output = cass_cmd(home)
            .args([
                "pack",
                "packreadonlyneedle",
                "--robot-format",
                format,
                "--mode",
                "lexical",
                "--require-evidence",
                "--freshness-policy",
                "allow-stale",
                "--data-dir",
            ])
            .arg(&data_dir)
            .timeout(Duration::from_secs(20))
            .output()
            .expect("run structured pack format against real archive");
        assert!(
            output.status.success(),
            "pack {format} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = std::str::from_utf8(&output.stdout).expect("UTF-8 pack output");
        let projected: Value = match format {
            "jsonl" => {
                let lines = text
                    .lines()
                    .map(|line| serde_json::from_str::<Value>(line).expect("JSONL object"))
                    .collect::<Vec<_>>();
                assert_eq!(lines.len(), evidence.len() + 4);
                assert_eq!(lines[0]["_meta"]["format"], "jsonl");
                assert!(lines[1]["pack"].is_object());
                let items = lines[2..2 + evidence.len()]
                    .iter()
                    .map(|line| line["evidence"].clone())
                    .collect::<Vec<_>>();
                serde_json::json!({
                    "pack": lines[1]["pack"],
                    "evidence": items,
                    "omitted": lines[lines.len() - 2]["omitted"],
                    "privacy": lines[lines.len() - 1]["privacy"],
                })
            }
            "toon" => Value::from(toon::try_decode(text, None).expect("valid TOON pack")),
            _ => {
                assert_eq!(text.lines().count(), 1, "compact JSON is one line");
                serde_json::from_str(text).expect("compact JSON pack")
            }
        };
        assert_eq!(stable_evidence(&projected), expected_evidence, "{format}");
        for section in ["pack", "omitted", "privacy"] {
            assert_eq!(
                toon::JsonValue::from(projected[section].clone()),
                toon::JsonValue::from(payload[section].clone()),
                "{format} {section} projection"
            );
        }
        assert_eq!(before, data_tree_snapshot(&data_dir), "{format}");
        assert_eq!(source, fs::read_to_string(&source_path).unwrap());
    }
    for section in ["answer_outline", "handoff"] {
        let entries = payload["pack"][section].as_array().expect("pack section");
        assert_eq!(entries.len(), evidence.len());
        for entry in entries {
            let ids = entry["evidence_ids"]
                .as_array()
                .expect("evidence references");
            assert!(!ids.is_empty());
            for id in ids {
                assert!(evidence.iter().any(|item| item["id"] == *id));
            }
        }
    }
    let verified_ids = evidence
        .iter()
        .map(|item| item["id"].as_str().expect("evidence ID").to_string())
        .collect::<Vec<_>>();

    let retained_source = source_path.with_extension("retained-jsonl");
    fs::rename(&source_path, &retained_source).unwrap();
    let output = cass_cmd(home)
        .args([
            "pack",
            "packreadonlyneedle",
            "--json",
            "--mode",
            "lexical",
            "--require-evidence",
            "--freshness-policy",
            "allow-stale",
            "--data-dir",
        ])
        .arg(&data_dir)
        .timeout(Duration::from_secs(20))
        .output()
        .expect("pack must preserve archived evidence after the source moves");
    assert!(
        output.status.success(),
        "source loss must not lose archived evidence: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let payload: Value = serde_json::from_slice(&output.stdout).expect("archived pack JSON");
    let evidence = payload["evidence"].as_array().expect("archived evidence");
    assert!(!evidence.is_empty());
    for item in evidence {
        assert_eq!(item["citation"]["verified"], false);
        assert!(item["citation"]["line_start"].is_null());
        assert!(item["citation"]["line_end"].is_null());
        assert!(
            !verified_ids.iter().any(|id| item["id"] == *id),
            "unverified archive evidence must not reuse a verified physical-span ID"
        );
        assert!(item["citation"]["message_index"].is_u64());
        assert!(
            item["excerpt"]
                .as_str()
                .unwrap()
                .contains("packreadonlyneedle")
        );
    }
    assert_eq!(before, data_tree_snapshot(&data_dir));
    assert_eq!(source, fs::read_to_string(retained_source).unwrap());
}

#[test]
fn structured_pack_shortens_large_evidence_without_losing_verified_citations() {
    use sha2::Digest as _;

    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("pack_budget_data");
    fs::create_dir_all(&data_dir).unwrap();
    let filename = "rollout-pack-budget.jsonl";
    let secret = format!("sk-{}", "A".repeat(40));
    let body = format!(
        "budgetexcerptneedle {} api_key={secret} {}",
        "界".repeat(2_400),
        "終".repeat(1_000)
    );
    seed_codex_session(&codex_home, filename, &body);
    run_fresh_index(home, &data_dir);
    let source_path = codex_home.join("sessions/2026/04/23").join(filename);
    let source = fs::read_to_string(&source_path).unwrap();
    let before = data_tree_snapshot(&data_dir);
    let mut packs = Vec::new();
    for max_tokens in ["12000", "1024"] {
        let output = cass_cmd(home)
            .args([
                "pack",
                "budgetexcerptneedle",
                "--json",
                "--mode",
                "lexical",
                "--require-evidence",
                "--max-tokens",
                max_tokens,
                "--max-excerpt-chars",
                "8000",
                "--fields",
                "evidence,limits,omitted",
                "--freshness-policy",
                "allow-stale",
                "--data-dir",
            ])
            .arg(&data_dir)
            .timeout(Duration::from_secs(20))
            .output()
            .expect("pack long evidence at the requested budget");
        assert!(
            output.status.success(),
            "pack must shorten useful evidence to fit: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let rendered = String::from_utf8(output.stdout).unwrap();
        assert!(!rendered.contains(&secret));
        assert!(!rendered.contains("sk-"));
        let budget: usize = max_tokens.parse().unwrap();
        assert!(rendered.chars().count().div_ceil(4) <= budget + budget / 20);
        packs.push(serde_json::from_str::<Value>(&rendered).unwrap());
        assert_eq!(source, fs::read_to_string(&source_path).unwrap());
        assert_eq!(before, data_tree_snapshot(&data_dir));
    }
    let full_evidence = packs[0]["evidence"].as_array().unwrap();
    let short_evidence = packs[1]["evidence"].as_array().unwrap();
    assert_eq!(full_evidence.len(), 2);
    assert_eq!(short_evidence.len(), 1);
    let shortened = &short_evidence[0];
    let full = full_evidence
        .iter()
        .find(|item| item["id"] == shortened["id"])
        .expect("shortening must preserve the selected citation identity");
    let full_excerpt = full["excerpt"].as_str().unwrap();
    let short_excerpt = shortened["excerpt"].as_str().unwrap();
    let evidence_tokens = short_excerpt.chars().count().div_ceil(4);
    // Final output also pays for citations, omission records and JSON syntax.
    // The planner's 60% excerpt allowance is an upper bound, not the emitted cost.
    assert!(evidence_tokens > 1 && evidence_tokens <= 1_024 * 60 / 100);
    let expected: String = full_excerpt
        .chars()
        .take(short_excerpt.chars().count() - 3)
        .collect();
    assert!(full_excerpt.chars().count() > short_excerpt.chars().count());
    assert_eq!(short_excerpt, format!("{expected}..."));
    assert_eq!(shortened["excerpt_truncated"], true);
    assert_eq!(full["excerpt_truncated"], false);
    assert_eq!(shortened["estimated_tokens"], evidence_tokens);
    assert_eq!(shortened["selection"]["token_cost"], evidence_tokens);
    assert_eq!(packs[1]["limits"]["estimated_tokens"], evidence_tokens);
    assert_eq!(shortened["citation"]["verified"], true);
    assert_eq!(
        shortened["citation"]["excerpt_sha256"],
        hex::encode(sha2::Sha256::digest(short_excerpt.as_bytes()))
    );
    assert_ne!(
        shortened["citation"]["excerpt_sha256"],
        full["citation"]["excerpt_sha256"]
    );
    for (field, value) in shortened["citation"].as_object().unwrap() {
        if field != "excerpt_sha256" && field != "freshness_age_seconds" {
            assert_eq!(value, &full["citation"][field], "citation field {field}");
        }
    }
    let line = shortened["citation"]["line_start"].as_u64().unwrap() as usize;
    let cited = source.lines().nth(line - 1).unwrap();
    assert!(cited.contains("budgetexcerptneedle"));
    assert_eq!(
        shortened["citation"]["span_hash"],
        blake3::hash(cited.as_bytes()).to_hex().to_string()
    );
    let omitted = packs[1]["omitted"]["items"].as_array().unwrap();
    assert_eq!(omitted.len(), 1);
    assert_eq!(omitted[0]["reason"], "token_budget_exhausted");
}

#[test]
fn pack_final_output_budget_bounds_all_formats_and_refuses_oversized_metadata() {
    let temp = TempDir::new().unwrap();
    let home = temp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("final_pack_budget");
    fs::create_dir_all(&data_dir).unwrap();
    for index in 0..6 {
        seed_codex_session(
            &codex_home,
            &format!("rollout-output-budget-{index}.jsonl"),
            &format!(
                "finalbudgetneedle session{index} {}",
                "界😀\"\\\u{1}`".repeat(450)
            ),
        );
    }
    run_fresh_index(home, &data_dir);
    let archive_before = data_tree_snapshot(&data_dir);
    let sources_before = data_tree_snapshot(&codex_home);
    let budget: usize = 3_600;
    let mut reduced_formats = 0;
    for format in ["json", "compact", "jsonl", "toon", "markdown"] {
        let mut command = cass_cmd(home);
        command
            .args([
                "pack",
                "finalbudgetneedle",
                "--mode",
                "lexical",
                "--require-evidence",
                "--freshness-policy",
                "allow-stale",
                "--max-excerpt-chars",
                "8000",
                "--max-tokens",
                &budget.to_string(),
                "--data-dir",
            ])
            .arg(&data_dir);
        if format == "markdown" {
            command.args(["--display", "markdown"]);
        } else {
            command.args(["--robot-format", format]);
        }
        let output = command.timeout(Duration::from_secs(20)).output().unwrap();
        assert!(
            output.status.success(),
            "{format}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(
            text.chars().count().div_ceil(4) <= budget + budget / 20,
            "{format}"
        );
        if format == "markdown" {
            let mut excerpts = 0;
            for event in pulldown_cmark::Parser::new(&text) {
                if matches!(
                    event,
                    pulldown_cmark::Event::Start(pulldown_cmark::Tag::CodeBlock(_))
                ) {
                    excerpts += 1;
                }
            }
            assert!(excerpts > 0, "bounded Markdown must retain real evidence");
        } else {
            let evidence = match format {
                "jsonl" => text
                    .lines()
                    .filter_map(|line| {
                        serde_json::from_str::<Value>(line)
                            .unwrap()
                            .get("evidence")
                            .cloned()
                    })
                    .collect::<Vec<_>>(),
                "toon" => Value::from(toon::try_decode(&text, None).unwrap())["evidence"]
                    .as_array()
                    .unwrap()
                    .clone(),
                _ => serde_json::from_str::<Value>(&text).unwrap()["evidence"]
                    .as_array()
                    .unwrap()
                    .clone(),
            };
            assert!(
                !evidence.is_empty(),
                "{format} must preserve required evidence"
            );
            let excerpt_tokens: usize = evidence
                .iter()
                .map(|item| {
                    let tokens = item["excerpt"]
                        .as_str()
                        .unwrap()
                        .chars()
                        .count()
                        .div_ceil(4);
                    // TOON represents numbers as f64. Compare exact numeric
                    // values to a cost derived independently from emitted text.
                    assert_eq!(
                        toon::JsonValue::from(item["estimated_tokens"].clone()),
                        toon::JsonValue::from(serde_json::json!(tokens))
                    );
                    tokens
                })
                .sum();
            if excerpt_tokens < budget * 60 / 100 {
                reduced_formats += 1;
            }
            for item in evidence {
                assert_eq!(item["citation"]["verified"], true);
                let source =
                    fs::read_to_string(item["citation"]["source_path"].as_str().unwrap()).unwrap();
                let line_index = source
                    .lines()
                    .position(|line| {
                        blake3::hash(line.as_bytes()).to_hex().as_str()
                            == item["citation"]["span_hash"].as_str().unwrap()
                    })
                    .expect("citation hash must identify an original source line");
                let line = line_index + 1;
                assert_eq!(
                    toon::JsonValue::from(item["citation"]["line_start"].clone()),
                    toon::JsonValue::from(serde_json::json!(line))
                );
            }
        }
        assert_eq!(archive_before, data_tree_snapshot(&data_dir));
        assert_eq!(sources_before, data_tree_snapshot(&codex_home));
    }
    assert!(
        reduced_formats > 0,
        "fixture must exercise final-output reduction"
    );

    let request_id = "metadata-overflow-".repeat(1_000);
    for format in ["json", "compact", "jsonl", "toon"] {
        let output = cass_cmd(home)
            .args([
                "pack",
                "finalbudgetneedle",
                "--mode",
                "lexical",
                "--robot-format",
                format,
                "--max-tokens",
                "1024",
                "--request-id",
                &request_id,
                "--data-dir",
            ])
            .arg(&data_dir)
            .timeout(Duration::from_secs(20))
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(2),
            "{format}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stdout.is_empty(),
            "no partial output on budget error: {format}"
        );
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(error["error"]["kind"], "pack-budget-too-small");
        assert_eq!(error["error"]["retryable"], false);
    }
    let masked = cass_cmd(home)
        .args([
            "pack",
            "finalbudgetneedle",
            "--json",
            "--mode",
            "lexical",
            "--max-tokens",
            "1024",
            "--request-id",
            &request_id,
            "--fields",
            "limits",
            "--data-dir",
        ])
        .arg(&data_dir)
        .timeout(Duration::from_secs(20))
        .output()
        .unwrap();
    assert!(
        masked.status.success(),
        "{}",
        String::from_utf8_lossy(&masked.stderr)
    );
    let projected: Value = serde_json::from_slice(&masked.stdout).unwrap();
    assert_eq!(projected.as_object().unwrap().len(), 1);
    assert!(projected["limits"].is_object());
    assert!(
        String::from_utf8(masked.stdout)
            .unwrap()
            .chars()
            .count()
            .div_ceil(4)
            <= 1_075
    );
    assert_eq!(archive_before, data_tree_snapshot(&data_dir));
    assert_eq!(sources_before, data_tree_snapshot(&codex_home));
}

#[test]
fn structured_pack_retains_source_verification_after_slow_planning() {
    let temp = TempDir::new().unwrap();
    let home = temp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("planning_source_budget");
    let content = "planningdelayneedle verified evidence after a bounded planning delay";
    util::seed_codex_session(&codex_home, "rollout-planning.jsonl", content, false);
    run_fresh_index(home, &data_dir);
    let archive_before = data_tree_snapshot(&data_dir);
    let sources_before = data_tree_snapshot(&codex_home);

    let output = cass_cmd(home)
        .env("CASS_TEST_PACK_PLAN_SLOW_MS", "1200")
        .args([
            "pack",
            "planningdelayneedle",
            "--json",
            "--mode",
            "lexical",
            "--require-evidence",
            "--freshness-policy",
            "allow-stale",
            "--timeout",
            "10000",
            "--data-dir",
        ])
        .arg(&data_dir)
        .timeout(Duration::from_secs(20))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(archive_before, data_tree_snapshot(&data_dir));
    assert_eq!(sources_before, data_tree_snapshot(&codex_home));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["budget"]["budget_ms"], 10000);
    assert_eq!(value["budget"]["timed_out"], false);
    let elapsed_ms = value["budget"]["elapsed_ms"].as_u64().unwrap();
    assert!(elapsed_ms >= 1200, "the planning delay must actually run");
    assert!(elapsed_ms < 10000, "the request still has time to verify");
    let evidence = value["evidence"].as_array().unwrap();
    assert_eq!(evidence.len(), 1);
    assert_eq!(evidence[0]["excerpt"], content);
    let citation = &evidence[0]["citation"];
    assert_eq!(
        citation["verified"], true,
        "planning must not consume the source phase allowance: budget={} citation={citation}",
        value["budget"]
    );
    let source_path = codex_home.join("sessions/2026/04/23/rollout-planning.jsonl");
    assert_eq!(
        citation["source_path"],
        source_path.to_string_lossy().as_ref()
    );
    assert_eq!(citation["line_start"], 2);
    let source = fs::read_to_string(source_path).unwrap();
    assert_eq!(
        citation["span_hash"],
        blake3::hash(source.lines().nth(1).unwrap().as_bytes())
            .to_hex()
            .as_str()
    );
}

#[test]
fn pack_excludes_skill_payloads_without_losing_ordinary_evidence() {
    let temp = TempDir::new().unwrap();
    let home = temp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("pack_skill_privacy");
    let ordinary = "skillprivacyneedle ordinary skill design remains useful";
    let credential = "abcdefghijklmnopqrst";
    util::seed_codex_session(&codex_home, "rollout-ordinary.jsonl", ordinary, false);
    for (index, marker) in [
        "Base directory for this skill:",
        "<system-reminder>",
        "The following skills are available for use with the Skill tool:",
        "skillInjection: matchedSkills",
        "<!-- skillInjection:",
    ]
    .into_iter()
    .enumerate()
    {
        util::seed_codex_session(
            &codex_home,
            &format!("rollout-skill-{index}.jsonl"),
            &format!(
                "skillprivacyneedle onlyinjectedneedle PROPRIETARY PLAYBOOK Authorization: Bearer {credential}\n{} {marker}",
                "界".repeat(120)
            ),
            false,
        );
    }
    run_fresh_index(home, &data_dir);
    let archive_before = data_tree_snapshot(&data_dir);
    let sources_before = data_tree_snapshot(&codex_home);

    for format in ["json", "compact", "jsonl", "toon", "markdown"] {
        let mut command = cass_cmd(home);
        command
            .args([
                "pack",
                "skillprivacyneedle",
                "--mode",
                "lexical",
                "--require-evidence",
                "--freshness-policy",
                "allow-stale",
                "--max-excerpt-chars",
                "80",
                "--data-dir",
            ])
            .arg(&data_dir);
        if format == "markdown" {
            command.args(["--display", "markdown"]);
        } else {
            command.args(["--robot-format", format]);
        }
        let output = command.timeout(Duration::from_secs(20)).output().unwrap();
        assert!(
            output.status.success(),
            "{format}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(
            text.contains(ordinary),
            "{format} must retain ordinary evidence"
        );
        assert!(!text.contains("PROPRIETARY PLAYBOOK"), "{format}");
        assert!(!text.contains("onlyinjectedneedle"), "{format}");
        if format != "markdown" {
            let (evidence, omitted, privacy) = if format == "jsonl" {
                let records = text
                    .lines()
                    .map(|line| serde_json::from_str::<Value>(line).unwrap())
                    .collect::<Vec<_>>();
                let evidence = records
                    .iter()
                    .filter_map(|record| record.get("evidence").cloned())
                    .collect::<Vec<_>>();
                let omitted = records
                    .iter()
                    .find_map(|record| record.get("omitted"))
                    .unwrap()
                    .clone();
                let privacy = records
                    .iter()
                    .find_map(|record| record.get("privacy"))
                    .unwrap()
                    .clone();
                (evidence, omitted, privacy)
            } else {
                let value = if format == "toon" {
                    Value::from(toon::try_decode(&text, None).unwrap())
                } else {
                    serde_json::from_str::<Value>(&text).unwrap()
                };
                (
                    value["evidence"].as_array().unwrap().clone(),
                    value["omitted"].clone(),
                    value["privacy"].clone(),
                )
            };
            assert_eq!(evidence.len(), 1, "{format}");
            assert_eq!(evidence[0]["excerpt"], ordinary);
            let citation = &evidence[0]["citation"];
            assert_eq!(citation["verified"], true);
            let source = fs::read_to_string(citation["source_path"].as_str().unwrap()).unwrap();
            let line = source.lines().nth(1).unwrap();
            assert_eq!(
                citation["span_hash"],
                blake3::hash(line.as_bytes()).to_hex().as_str()
            );
            assert_eq!(
                toon::JsonValue::from(citation["line_start"].clone()),
                toon::JsonValue::from(serde_json::json!(2))
            );
            let omitted = omitted["items"].as_array().unwrap();
            assert_eq!(omitted.len(), 5, "{format}");
            for item in omitted {
                assert_eq!(item["reason"], "redacted_to_empty");
            }
            assert_eq!(privacy["skill_content_included"], false);
            assert_eq!(privacy["redaction_applied"], true);
        }
        assert_eq!(archive_before, data_tree_snapshot(&data_dir));
        assert_eq!(sources_before, data_tree_snapshot(&codex_home));
    }

    for format in ["json", "compact", "jsonl", "toon", "markdown"] {
        let mut command = cass_cmd(home);
        command
            .args([
                "pack",
                "skillprivacyneedle",
                "--include-skill-content",
                "--mode",
                "lexical",
                "--require-evidence",
                "--freshness-policy",
                "allow-stale",
                "--max-excerpt-chars",
                "800",
                "--data-dir",
            ])
            .arg(&data_dir);
        if format == "markdown" {
            command.args(["--display", "markdown"]);
        } else {
            command.args(["--robot-format", format]);
        }
        let output = command.timeout(Duration::from_secs(20)).output().unwrap();
        assert!(
            output.status.success(),
            "opt-in {format}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.contains(ordinary), "opt-in {format}");
        assert!(text.contains("PROPRIETARY PLAYBOOK"), "opt-in {format}");
        assert!(text.contains("onlyinjectedneedle"), "opt-in {format}");
        assert!(!text.contains(credential), "opt-in {format}");
        if format != "markdown" {
            let value = match format {
                "jsonl" => {
                    let records = text
                        .lines()
                        .map(|line| serde_json::from_str::<Value>(line).unwrap())
                        .collect::<Vec<_>>();
                    serde_json::json!({
                        "evidence": records.iter().filter_map(|record| record.get("evidence")).collect::<Vec<_>>(),
                        "privacy": records.iter().find_map(|record| record.get("privacy")).unwrap(),
                        "omitted": records.iter().find_map(|record| record.get("omitted")).unwrap(),
                    })
                }
                "toon" => Value::from(toon::try_decode(&text, None).unwrap()),
                _ => serde_json::from_str(&text).unwrap(),
            };
            let evidence = value["evidence"].as_array().unwrap();
            assert_eq!(evidence.len(), 6, "opt-in {format}");
            assert_eq!(value["privacy"]["skill_content_included"], true);
            assert_eq!(value["privacy"]["redaction_applied"], true);
            assert_eq!(value["privacy"]["redaction_policy"], "strict");
            assert!(value["omitted"]["items"].as_array().unwrap().is_empty());
            for item in evidence {
                let citation = &item["citation"];
                assert_eq!(citation["verified"], true, "opt-in {format}");
                let source = fs::read_to_string(citation["source_path"].as_str().unwrap()).unwrap();
                assert_eq!(
                    citation["span_hash"],
                    blake3::hash(source.lines().nth(1).unwrap().as_bytes())
                        .to_hex()
                        .as_str()
                );
            }
        }
        assert_eq!(archive_before, data_tree_snapshot(&data_dir));
        assert_eq!(sources_before, data_tree_snapshot(&codex_home));
    }

    for require_evidence in [false, true] {
        let mut command = cass_cmd(home);
        command
            .args([
                "pack",
                "onlyinjectedneedle",
                "--json",
                "--mode",
                "lexical",
                "--freshness-policy",
                "allow-stale",
                "--data-dir",
            ])
            .arg(&data_dir);
        if require_evidence {
            command.arg("--require-evidence");
        }
        let output = command.timeout(Duration::from_secs(20)).output().unwrap();
        if require_evidence {
            assert_eq!(output.status.code(), Some(13));
            assert!(output.stdout.is_empty());
            let error: Value = serde_json::from_slice(&output.stderr).unwrap();
            assert_eq!(error["error"]["kind"], "not-found");
            assert_eq!(error["error"]["retryable"], false);
        } else {
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let value: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert!(value["evidence"].as_array().unwrap().is_empty());
            assert_eq!(value["omitted"]["items"].as_array().unwrap().len(), 5);
            assert!(
                value["warnings"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|warning| warning == "no_evidence_found")
            );
            assert!(!String::from_utf8_lossy(&output.stdout).contains("PROPRIETARY PLAYBOOK"));
        }
        assert_eq!(archive_before, data_tree_snapshot(&data_dir));
        assert_eq!(sources_before, data_tree_snapshot(&codex_home));
    }

    // The shared policy must retain export's explicit operator opt-in.
    let source_path = codex_home.join("sessions/2026/04/23/rollout-skill-0.jsonl");
    let output = cass_cmd(home)
        .args(["export", "--format", "json", "--include-skills"])
        .arg(&source_path)
        .timeout(Duration::from_secs(20))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("PROPRIETARY PLAYBOOK"));
    assert_eq!(sources_before, data_tree_snapshot(&codex_home));
}

#[test]
fn markdown_pack_preserves_json_excerpts_without_interpreting_source_markup() {
    use pulldown_cmark::{Event, Parser, Tag, TagEnd};

    let fixture = TempDir::new().expect("isolated Markdown pack fixture");
    let home = fixture.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("markdown_pack_data");
    // ubs:ignore — Synthetic credential verifies ingestion/output redaction; no live secret.
    let secret = "sk-12345678901234567890";
    let content = format!(
        "\n\nmarkdownpackneedle <script>alert(1)</script> [link](https://example.invalid)\n{}\n\n\
         ```rust\nfn main() {{}}\n```\n~~~\n# source heading\nfinal preserved context\n\n",
        "Unicode αβ 🚀 context. ".repeat(25)
    );
    util::seed_codex_session(&codex_home, "rollout-markdown-safe.jsonl", &content, false);
    util::seed_codex_session(
        &codex_home,
        "rollout-markdown-secret.jsonl",
        &format!("{content}\nAPI_KEY={secret}"),
        false,
    );
    run_fresh_index(home, &data_dir);
    let archive_before = data_tree_snapshot(&data_dir);
    let sources_before = data_tree_snapshot(&codex_home);

    for max_excerpt_chars in ["80", "1600"] {
        let mut outputs = Vec::new();
        for format in [&["--json"][..], &["--display", "markdown"][..]] {
            let output = cass_cmd(home)
                .args([
                    "pack",
                    "markdownpackneedle",
                    "--mode",
                    "lexical",
                    "--require-evidence",
                    "--freshness-policy",
                    "allow-stale",
                    "--max-evidence",
                    "2",
                    "--max-excerpt-chars",
                    max_excerpt_chars,
                    "--data-dir",
                ])
                .arg(&data_dir)
                .args(format)
                .timeout(Duration::from_secs(20))
                .output()
                .expect("run pack format comparison");
            assert!(
                output.status.success(),
                "pack {format:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let text = String::from_utf8(output.stdout).expect("UTF-8 pack output");
            assert!(
                !text.contains(secret),
                "source credential must not reach output"
            );
            outputs.push(text);
        }
        let json: Value = serde_json::from_str(&outputs[0]).expect("pack JSON");
        let evidence = json["evidence"].as_array().expect("selected evidence");
        assert_eq!(
            evidence.len(),
            2,
            "both real source records must be selected"
        );
        let mut expected = Vec::new();
        let mut safe_record_verified = false;
        let mut secret_record_selected = false;
        for item in evidence {
            let excerpt = item["excerpt"].as_str().expect("JSON excerpt");
            expected.push(if excerpt.ends_with('\n') {
                excerpt.to_string()
            } else {
                format!("{excerpt}\n")
            });
            let source = Path::new(
                item["citation"]["source_path"]
                    .as_str()
                    .expect("source path"),
            );
            assert!(source.is_file());
            if source.file_name().and_then(|name| name.to_str())
                == Some("rollout-markdown-safe.jsonl")
            {
                assert_eq!(item["citation"]["verified"], true);
                assert_eq!(item["citation"]["line_start"], 2);
                safe_record_verified = true;
            } else if source.file_name().and_then(|name| name.to_str())
                == Some("rollout-markdown-secret.jsonl")
            {
                secret_record_selected = true;
            }
            if max_excerpt_chars == "80" {
                assert_eq!(item["excerpt_truncated"], true);
                assert!(excerpt.chars().count() <= 80);
            } else {
                assert!(excerpt.contains("final preserved context"));
                assert!(excerpt.chars().count() > 220);
            }
        }
        assert!(
            safe_record_verified,
            "the verifiable source must be selected"
        );
        assert!(
            secret_record_selected,
            "the credential-bearing source must be selected to prove redaction"
        );
        let mut excerpts = Vec::new();
        let mut current_excerpt = None::<String>;
        for event in Parser::new(&outputs[1]) {
            match event {
                Event::Start(Tag::CodeBlock(_)) => {
                    current_excerpt = Some(String::new());
                }
                Event::Text(text) => {
                    if let Some(excerpt) = current_excerpt.as_mut() {
                        excerpt.push_str(&text);
                    }
                }
                Event::End(TagEnd::CodeBlock) => {
                    excerpts.push(current_excerpt.take().expect("open evidence block"));
                }
                Event::Html(_)
                | Event::InlineHtml(_)
                | Event::Start(Tag::Link { .. } | Tag::Image { .. }) => {
                    panic!("session markup must not become active Markdown content");
                }
                _ => {}
            }
        }
        assert_eq!(
            excerpts, expected,
            "Markdown must carry the same selected excerpts as JSON"
        );
        assert_eq!(data_tree_snapshot(&data_dir), archive_before);
        assert_eq!(data_tree_snapshot(&codex_home), sources_before);
    }
}

#[test]
fn structured_pack_refuses_unreadable_lexical_assets_without_repair() {
    for missing in [false, true] {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let data_dir = home.join("pack_unreadable_data");
        fs::create_dir_all(&data_dir).unwrap();
        seed_codex_session(
            &home.join(".codex"),
            "rollout-pack-unreadable.jsonl",
            "packunreadableneedle",
        );
        run_fresh_index(home, &data_dir);
        let index_path = coding_agent_search::search::tantivy::expected_index_dir(&data_dir);
        if missing {
            fs::rename(&index_path, data_dir.join("retained-lexical-before-pack")).unwrap();
        } else {
            for manifest in ["MANIFEST", "MANIFEST.prev"] {
                fs::write(index_path.join(manifest), b"unreadable lexical generation").unwrap();
            }
        }
        let before = data_tree_snapshot(&data_dir);
        let output = cass_cmd(home)
            .args([
                "pack",
                "packunreadableneedle",
                "--json",
                "--mode",
                "lexical",
                "--data-dir",
            ])
            .arg(&data_dir)
            .timeout(Duration::from_secs(20))
            .output()
            .expect("run structured pack with unreadable lexical assets");
        assert_eq!(output.status.code(), Some(5), "missing={missing}");
        assert!(
            output.stdout.is_empty(),
            "a refused pack has no data output"
        );
        let stderr = String::from_utf8(output.stderr).expect("UTF-8 diagnostics");
        let payload: Value = serde_json::from_str(stderr.lines().last().expect("error line"))
            .expect("pack error JSON");
        assert_eq!(payload["error"]["kind"], "maintenance-required");
        let hint = payload["error"]["hint"].as_str().expect("pack repair hint");
        assert!(hint.contains("index --full --json"));
        assert!(hint.contains(data_dir.to_str().unwrap()));
        assert!(hint.contains(data_dir.join("agent_search.db").to_str().unwrap()));
        assert!(!hint.contains("--no-maintenance"));
        assert_eq!(
            before,
            data_tree_snapshot(&data_dir),
            "structured pack must not rebuild an unreadable generation; missing={missing}"
        );
    }
}

#[test]
fn explicit_empty_session_scope_never_expands_to_the_archive() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("empty_scope_data");
    fs::create_dir_all(&data_dir).unwrap();
    for filename in [
        "rollout-scope-selected.jsonl",
        "rollout-scope-excluded.jsonl",
    ] {
        let content = format!("sessionboundaryneedle {filename}");
        seed_codex_session(&codex_home, filename, &content);
    }
    run_fresh_index(home, &data_dir);
    build_hash_semantic_assets(&data_dir, true);
    let selected = codex_home
        .join("sessions/2026/04/23")
        .join("rollout-scope-selected.jsonl");
    let scope_path = home.join("session-scope.txt");
    let before = data_tree_snapshot(&data_dir);

    for mode in ["lexical", "semantic", "hybrid"] {
        for (contents, offset, empty) in [
            (Some(String::new()), "0", true),
            (Some("  \n# no candidates\n\t\n".to_string()), "0", true),
            (Some(String::new()), "50", true),
            (Some(format!("{}\n", selected.display())), "0", false),
            (None, "0", false),
        ] {
            let mut cmd = cass_cmd(home);
            cmd.arg("search");
            if let Some(contents) = &contents {
                fs::write(&scope_path, contents).unwrap();
                cmd.arg("--sessions-from").arg(&scope_path);
            }
            let output = cmd
                .args([
                    "sessionboundaryneedle",
                    "--json",
                    "--robot-meta",
                    "--no-maintenance",
                    "--mode",
                    mode,
                    "--model",
                    "hash",
                    "--limit",
                    "10",
                    "--offset",
                    offset,
                ])
                .arg("--data-dir")
                .arg(&data_dir)
                .timeout(Duration::from_secs(20))
                .output()
                .expect("search within explicit session scope");
            assert!(
                output.status.success(),
                "mode={mode}, empty={empty}, stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let payload: Value = serde_json::from_slice(&output.stdout).expect("search JSON");
            let hits = payload["hits"].as_array().expect("hits array");
            if empty {
                assert!(
                    hits.is_empty(),
                    "an empty scope must not become unrestricted"
                );
                assert_eq!(payload["total_matches"], 0, "offset cannot invent matches");
                assert_eq!(payload["sessions_filter"]["requested"], 0);
                assert_eq!(payload["sessions_filter"]["matched"], 0);
                assert_eq!(payload["_meta"]["semantic_refinement"], false);
            } else if contents.is_some() {
                assert!(
                    !hits.is_empty(),
                    "a populated scope must return matching evidence"
                );
                for hit in hits {
                    assert_eq!(
                        Path::new(hit["source_path"].as_str().expect("hit source")),
                        selected
                    );
                }
            } else {
                let paths = hits
                    .iter()
                    .map(|hit| hit["source_path"].as_str().expect("hit source"))
                    .collect::<std::collections::HashSet<_>>();
                assert_eq!(
                    paths.len(),
                    2,
                    "omitting the scope searches both sessions: mode={mode}, paths={paths:?}"
                );
            }
            assert_eq!(before, data_tree_snapshot(&data_dir));
        }
    }
}

#[test]
fn no_maintenance_search_and_structured_pack_preserve_abandoned_lock() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let data_dir = home.join("abandoned_lock_data");
    fs::create_dir_all(&data_dir).unwrap();
    seed_codex_session(
        &home.join(".codex"),
        "rollout-abandoned-lock.jsonl",
        "abandonedlockneedle",
    );
    seed_codex_session(
        &home.join(".codex"),
        "rollout-abandoned-lock-second.jsonl",
        "abandonedlockneedle independent session",
    );
    run_fresh_index(home, &data_dir);
    build_hash_semantic_assets(&data_dir, true);
    let metadata = format!(
        "pid=4242\nstarted_at_ms=1733000111000\nupdated_at_ms=1733000112000\ndb_path={}\nmode=index\njob_id=abandoned-4242\njob_kind=lexical_refresh\nphase=rebuilding\n",
        data_dir.join("agent_search.db").display()
    );
    fs::write(data_dir.join("index-run.lock"), &metadata).unwrap();
    let before = data_tree_snapshot(&data_dir);

    for (command, mode) in [
        ("search", "lexical"),
        ("search", "semantic"),
        ("search", "hybrid"),
        ("pack", "lexical"),
    ] {
        let mut cmd = cass_cmd(home);
        cmd.args([command, "abandonedlockneedle", "--json", "--mode", mode]);
        if command == "search" {
            cmd.args(["--no-maintenance", "--robot-meta", "--model", "hash"]);
        }
        let output = cmd
            .arg("--data-dir")
            .arg(&data_dir)
            .timeout(Duration::from_secs(20))
            .output()
            .expect("read evidence with abandoned lock metadata");
        assert!(
            output.status.success(),
            "command={command}, mode={mode}, stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        let payload: Value = serde_json::from_slice(&output.stdout).expect("evidence JSON");
        let evidence_field = if command == "search" {
            "hits"
        } else {
            "evidence"
        };
        assert!(
            !payload[evidence_field].as_array().unwrap().is_empty(),
            "abandoned metadata must not prevent useful evidence"
        );
        assert_eq!(
            before,
            data_tree_snapshot(&data_dir),
            "{command} {mode} must not reap abandoned lock metadata"
        );
    }
}

#[test]
fn gh452_semantic_only_search_ignores_corrupt_lexical_assets() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_semantic_isolation_data");
    fs::create_dir_all(&data_dir).unwrap();
    for idx in 1..=3 {
        seed_codex_session(
            &codex_home,
            &format!("rollout-semantic-isolation-{idx:02}.jsonl"),
            &format!("semanticisolationneedle independent vector retrieval {idx}"),
        );
    }
    run_fresh_index(home, &data_dir);
    // Hash embeddings isolate dispatch and hydration; this is not a MiniLM
    // relevance or performance claim.
    build_hash_semantic_assets(&data_dir, true);
    let index_path = coding_agent_search::search::tantivy::expected_index_dir(&data_dir);
    fs::write(index_path.join("MANIFEST"), b"invalid lexical contract").unwrap();
    fs::write(
        index_path.join("MANIFEST.prev"),
        b"invalid prior lexical contract",
    )
    .unwrap();
    let before = data_tree_snapshot(&data_dir);
    let output = cass_cmd(home)
        .args([
            "search",
            "semanticisolationneedle",
            "--json",
            "--robot-meta",
            "--mode",
            "semantic",
            "--model",
            "hash",
            "--no-maintenance",
            "--limit",
            "0",
            "--data-dir",
        ])
        .arg(&data_dir)
        .timeout(Duration::from_secs(20))
        .output()
        .expect("run semantic-only search subprocess");
    assert!(
        output.status.success(),
        "semantic-only search must ignore corrupt lexical assets. stdout: {} stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let payload: Value = serde_json::from_slice(&output.stdout).expect("search JSON");
    assert!(
        payload["hits"]
            .as_array()
            .is_some_and(|hits| hits.len() > 1)
    );
    assert_eq!(payload["_meta"]["search_mode"], "semantic");
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("Tantivy search index"),
        "intentional semantic-only admission must not emit a missing lexical warning"
    );
    assert_eq!(before, data_tree_snapshot(&data_dir));
}

#[test]
fn explicit_hybrid_mode_fails_open_to_lexical_when_semantic_assets_missing() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    // Seed one Codex session with a single-word keyword (no underscores
    // to stay clear of tokenizer split behavior downstream).
    seed_codex_session(
        &codex_home,
        "rollout-failopen-fixture-01.jsonl",
        "failopenprobe",
    );

    // Build canonical DB + lexical index from the freshly seeded
    // session. No `--semantic` flag: the semantic tier is deliberately
    // absent so the fail-open path activates below.
    let mut index = cass_cmd(home);
    index
        .args(["index", "--full", "--json", "--data-dir"])
        .arg(&data_dir);
    let index_output = index.output().expect("run cass index --full");
    assert!(
        index_output.status.success(),
        "cass index --full must succeed on a fresh seeded corpus. stdout: {} stderr: {}",
        String::from_utf8_lossy(&index_output.stdout),
        String::from_utf8_lossy(&index_output.stderr)
    );

    // Request hybrid search explicitly. With no semantic assets, the
    // 86c88d0b contract says cass fails open to lexical rather than
    // erroring out, and the robot meta reports every realized-tier
    // field so observability stays truthful.
    let mut search = cass_cmd(home);
    search
        .args([
            "search",
            "failopenprobe",
            "--json",
            "--robot-meta",
            "--mode",
            "hybrid",
            "--limit",
            "5",
            "--data-dir",
        ])
        .arg(&data_dir);
    let search_output = search.output().expect("run cass search --mode hybrid");
    let search_stdout = String::from_utf8_lossy(&search_output.stdout);
    let search_stderr = String::from_utf8_lossy(&search_output.stderr);
    assert!(
        search_output.status.success(),
        "cass search --mode hybrid must fail open, not error, when semantic \
         assets are absent.\nstdout: {search_stdout}\nstderr: {search_stderr}"
    );

    let payload: Value = serde_json::from_str(search_stdout.trim()).unwrap_or_else(|err| {
        panic!("cass search --json output is not valid JSON: {err}\nstdout: {search_stdout}")
    });
    let meta = payload
        .get("_meta")
        .and_then(Value::as_object)
        .unwrap_or_else(|| panic!("--robot-meta must populate `_meta`; payload: {payload}"));

    assert_eq!(
        meta.get("requested_search_mode").and_then(Value::as_str),
        Some("hybrid"),
        "explicit --mode hybrid must be preserved as the requested intent"
    );
    assert_eq!(
        meta.get("search_mode").and_then(Value::as_str),
        Some("lexical"),
        "realized tier must be lexical when semantic assets are missing"
    );
    assert_eq!(
        meta.get("mode_defaulted").and_then(Value::as_bool),
        Some(false),
        "the user explicitly passed --mode hybrid; mode_defaulted must be false"
    );
    assert_eq!(
        meta.get("fallback_tier").and_then(Value::as_str),
        Some("lexical"),
        "robot meta must name the fail-open tier so agents can diagnose degraded results"
    );
    assert_eq!(
        meta.get("semantic_refinement").and_then(Value::as_bool),
        Some(false),
        "no semantic pass happened, so semantic_refinement must be false"
    );

    // Bead 2hh1s: the `fallback_reason` field is the agent-diagnostic
    // string populated by `SearchModeMeta::fall_back_to_lexical` in
    // src/lib.rs. It must be present (not null) and non-empty on every
    // fail-open path, otherwise agents consuming --robot-meta cannot tell
    // WHY the planner demoted. The exact prefix depends on which branch
    // fired (rejected, unavailable, hybrid execution unavailable, or
    // semantic assets unavailable) — all of those are acceptable.
    let fallback_reason = meta
        .get("fallback_reason")
        .and_then(Value::as_str)
        .unwrap_or_else(|| {
            panic!(
                "--robot-meta must populate `_meta.fallback_reason` on fail-open; meta: {meta:?}"
            )
        });
    assert!(
        !fallback_reason.is_empty(),
        "fallback_reason must be a non-empty diagnostic string; got: {fallback_reason:?}"
    );
    assert!(
        fallback_reason.contains("semantic") || fallback_reason.contains("hybrid"),
        "fallback_reason should describe why the planner demoted (expected 'semantic'/'hybrid' \
         in the reason string); got: {fallback_reason:?}"
    );
}

// Bead coding_agent_session_search-jogco (child of ibuuh.10, scenario C:
// default-hybrid result quality in lexical-only state).
//
// The sibling test above pins the `_meta` truthfulness on the fail-open
// path but never looks at the actual result set. ibuuh.10's AC calls
// for "default-hybrid result quality across lexical-only, fast-tier,
// and full-hybrid states" — this test covers the LEXICAL-ONLY slice
// (no semantic model installed, which is the default cass install).
//
// Claim pinned: when semantic assets are absent, the default-hybrid
// planner is expected to fail open to lexical AND produce exactly the
// same hit list — same source_path+line_number keys in the same order
// — as an explicit `--mode lexical` search. If a future refactor made
// the default path silently rank differently, drop hits, or run a
// reranker that lexical-mode doesn't, users see a quality regression
// that pure _meta tests don't catch.
fn hit_keys(hits: &[Value]) -> Vec<(String, i64)> {
    // Fail loud on null/missing source_path or line_number instead of
    // defaulting to "" / -1. A silently-defaulted hit would make two
    // modes look equivalent even when both are emitting malformed
    // hits — hollowing out the equivalence guarantee this helper
    // exists to enforce.
    hits.iter()
        .map(|h| {
            let path = h
                .get("source_path")
                .and_then(Value::as_str)
                .unwrap_or_else(|| {
                    panic!(
                        "hit must have a non-null source_path string; \
                         got hit: {h}"
                    )
                })
                .to_string();
            let line = h
                .get("line_number")
                .and_then(Value::as_i64)
                .unwrap_or_else(|| {
                    panic!(
                        "hit must have a non-null integer line_number; \
                         got hit: {h}"
                    )
                });
            (path, line)
        })
        .collect()
}

#[test]
fn default_hybrid_hit_list_equals_explicit_lexical_when_semantic_absent() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    // Seed three rollouts so the corpus is large enough to give the
    // planner real ranking work. Filenames start with `rollout-` per
    // franken_agent_detection::CodexConnector::is_rollout_file (line
    // ~77). Multiple conversations also sidesteps the single-conv
    // shard-plan bug tracked in bead rx1ex.
    for idx in 1..=3 {
        let name = format!("rollout-equiv-{idx:02}.jsonl");
        seed_codex_session(&codex_home, &name, "equivprobe");
    }

    let mut index = cass_cmd(home);
    index
        .args(["index", "--full", "--json", "--data-dir"])
        .arg(&data_dir);
    let index_output = index.output().expect("run cass index --full");
    assert!(
        index_output.status.success(),
        "cass index --full must succeed on the seeded corpus. stdout: {} stderr: {}",
        String::from_utf8_lossy(&index_output.stdout),
        String::from_utf8_lossy(&index_output.stderr)
    );

    // Search in DEFAULT mode (hybrid-preferred per AGENTS.md but
    // failing open to lexical since no semantic model is installed).
    let mut default_search = cass_cmd(home);
    default_search
        .args([
            "search",
            "equivprobe",
            "--json",
            "--robot-meta",
            "--limit",
            "10",
            "--data-dir",
        ])
        .arg(&data_dir);
    let default_out = default_search.output().expect("run default search");
    assert!(
        default_out.status.success(),
        "default-mode search must succeed. stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&default_out.stdout),
        String::from_utf8_lossy(&default_out.stderr)
    );
    let default_json: Value = serde_json::from_slice(&default_out.stdout)
        .unwrap_or_else(|err| panic!("default search JSON parse failed: {err}"));
    let default_meta = default_json
        .get("_meta")
        .and_then(Value::as_object)
        .unwrap_or_else(|| panic!("default search must include robot _meta: {default_json}"));
    assert_eq!(
        default_meta
            .get("requested_search_mode")
            .and_then(Value::as_str),
        Some("hybrid"),
        "default search intent must remain hybrid-preferred"
    );
    assert_eq!(
        default_meta.get("mode_defaulted").and_then(Value::as_bool),
        Some(true),
        "default search must report that the search mode was not user-specified"
    );
    assert_eq!(
        default_meta.get("search_mode").and_then(Value::as_str),
        Some("lexical"),
        "default hybrid search must realize lexical mode when semantic assets are absent"
    );
    assert_eq!(
        default_meta.get("fallback_tier").and_then(Value::as_str),
        Some("lexical"),
        "default hybrid fail-open must identify the realized fallback tier"
    );
    assert_eq!(
        default_meta
            .get("semantic_refinement")
            .and_then(Value::as_bool),
        Some(false),
        "lexical-only fallback must not claim semantic refinement"
    );
    let default_fallback_reason = default_meta
        .get("fallback_reason")
        .and_then(Value::as_str)
        .unwrap_or_else(|| {
            panic!("default hybrid fail-open must explain why it demoted: {default_meta:?}")
        });
    assert!(
        default_fallback_reason.contains("semantic") || default_fallback_reason.contains("hybrid"),
        "fallback_reason should describe the semantic/hybrid demotion; got: {default_fallback_reason:?}"
    );
    let default_hits = default_json
        .get("hits")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    // Search with EXPLICIT --mode lexical on the same corpus.
    let mut lexical_search = cass_cmd(home);
    lexical_search
        .args([
            "search",
            "equivprobe",
            "--json",
            "--robot-meta",
            "--mode",
            "lexical",
            "--limit",
            "10",
            "--data-dir",
        ])
        .arg(&data_dir);
    let lexical_out = lexical_search.output().expect("run lexical search");
    assert!(
        lexical_out.status.success(),
        "--mode lexical search must succeed. stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&lexical_out.stdout),
        String::from_utf8_lossy(&lexical_out.stderr)
    );
    let lexical_json: Value = serde_json::from_slice(&lexical_out.stdout)
        .unwrap_or_else(|err| panic!("lexical search JSON parse failed: {err}"));
    let lexical_meta = lexical_json
        .get("_meta")
        .and_then(Value::as_object)
        .unwrap_or_else(|| {
            panic!("explicit lexical search must include robot _meta: {lexical_json}")
        });
    assert_eq!(
        lexical_meta
            .get("requested_search_mode")
            .and_then(Value::as_str),
        Some("lexical"),
        "explicit lexical search must preserve the requested intent"
    );
    assert_eq!(
        lexical_meta.get("mode_defaulted").and_then(Value::as_bool),
        Some(false),
        "explicit --mode lexical must not be reported as defaulted"
    );
    assert_eq!(
        lexical_meta.get("search_mode").and_then(Value::as_str),
        Some("lexical"),
        "explicit lexical search must realize lexical mode"
    );
    assert_eq!(
        lexical_meta.get("fallback_tier"),
        Some(&Value::Null),
        "explicit lexical mode is not a fail-open path"
    );
    assert_eq!(
        lexical_meta.get("fallback_reason"),
        Some(&Value::Null),
        "explicit lexical mode should not emit a fallback reason"
    );
    assert_eq!(
        lexical_meta
            .get("semantic_refinement")
            .and_then(Value::as_bool),
        Some(false),
        "explicit lexical search must not claim semantic refinement"
    );
    let lexical_hits = lexical_json
        .get("hits")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    // Guard: there really should be hits for the seeded keyword. A
    // zero-hit corpus would make the equivalence trivially true and
    // hide real regressions.
    assert!(
        !default_hits.is_empty(),
        "default search must return >=1 hit for the seeded keyword; \
         payload: {default_json}"
    );

    // The actual contract pin: same hits in the same order.
    let default_keys = hit_keys(&default_hits);
    let lexical_keys = hit_keys(&lexical_hits);
    assert_eq!(
        default_keys, lexical_keys,
        "default-mode hit list must equal --mode lexical hit list when \
         semantic assets are absent.\ndefault: {default_keys:?}\nlexical: {lexical_keys:?}"
    );

    // Hit counts must also match — guards against a regression where
    // the planner silently truncates or expands one of the paths.
    assert_eq!(
        default_json.get("count").and_then(Value::as_u64),
        lexical_json.get("count").and_then(Value::as_u64),
        "default and lexical `count` must match in lexical-only state. \
         default: {default_json}\nlexical: {lexical_json}"
    );
    assert_eq!(
        default_json.get("total_matches").and_then(Value::as_u64),
        lexical_json.get("total_matches").and_then(Value::as_u64),
        "default and lexical `total_matches` must match in lexical-only state. \
         default: {default_json}\nlexical: {lexical_json}"
    );
}

#[test]
fn explicit_hybrid_hit_list_matches_monolithic_when_semantic_shards_are_promoted() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let monolithic_data_dir = home.join("cass_monolithic_data");
    let sharded_data_dir = home.join("cass_sharded_data");
    fs::create_dir_all(&monolithic_data_dir).unwrap();
    fs::create_dir_all(&sharded_data_dir).unwrap();

    for idx in 1..=4 {
        let name = format!("rollout-shardproof-{idx:02}.jsonl");
        seed_codex_session(
            &codex_home,
            &name,
            &format!("shardprobe topic {idx} shared semantic proof"),
        );
    }

    run_fresh_index(home, &monolithic_data_dir);
    run_fresh_index(home, &sharded_data_dir);
    build_hash_semantic_assets(&monolithic_data_dir, false);
    build_hash_semantic_assets(&sharded_data_dir, true);

    let monolithic_json = run_hybrid_hash_search(home, &monolithic_data_dir, "shardprobe shared");
    let sharded_json = run_hybrid_hash_search(home, &sharded_data_dir, "shardprobe shared");

    for (label, payload) in [("monolithic", &monolithic_json), ("sharded", &sharded_json)] {
        let meta = payload
            .get("_meta")
            .and_then(Value::as_object)
            .unwrap_or_else(|| panic!("{label} hybrid search must include robot _meta: {payload}"));
        assert_eq!(
            meta.get("requested_search_mode").and_then(Value::as_str),
            Some("hybrid"),
            "{label} search must preserve explicit hybrid intent"
        );
        assert_eq!(
            meta.get("search_mode").and_then(Value::as_str),
            Some("hybrid"),
            "{label} search must realize hybrid mode when hash semantic assets are ready"
        );
        assert_eq!(
            meta.get("fallback_tier"),
            Some(&Value::Null),
            "{label} search must not fail open when semantic assets are ready"
        );
        assert_eq!(
            meta.get("fallback_reason"),
            Some(&Value::Null),
            "{label} search must not report a fallback reason"
        );
        assert_eq!(
            meta.get("semantic_refinement").and_then(Value::as_bool),
            Some(true),
            "{label} search must report semantic refinement"
        );
    }

    let monolithic_hits = monolithic_json
        .get("hits")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let sharded_hits = sharded_json
        .get("hits")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    assert!(
        !monolithic_hits.is_empty(),
        "monolithic hybrid search must return hits for the seeded shardprobe corpus: {monolithic_json}"
    );
    assert_eq!(
        hit_keys(&sharded_hits),
        hit_keys(&monolithic_hits),
        "complete semantic shard generations must preserve the robot-visible hit identity of the \
         equivalent monolithic semantic index.\nmonolithic: {monolithic_json}\nsharded: {sharded_json}"
    );
    assert_eq!(
        sharded_json.get("count").and_then(Value::as_u64),
        monolithic_json.get("count").and_then(Value::as_u64),
        "sharded and monolithic hybrid count must match"
    );
    assert_eq!(
        sharded_json.get("total_matches").and_then(Value::as_u64),
        monolithic_json.get("total_matches").and_then(Value::as_u64),
        "sharded and monolithic hybrid total_matches must match"
    );
}

#[test]
fn explicit_hybrid_fails_open_when_semantic_shard_generation_is_incomplete() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_incomplete_shards_data");
    fs::create_dir_all(&data_dir).unwrap();

    for idx in 1..=3 {
        let name = format!("rollout-incomplete-shardproof-{idx:02}.jsonl");
        seed_codex_session(
            &codex_home,
            &name,
            &format!("incompleteshardprobe topic {idx} lexical fallback proof"),
        );
    }

    run_fresh_index(home, &data_dir);
    build_hash_semantic_assets(&data_dir, true);
    mark_first_semantic_shard_not_ready(&data_dir);

    let hybrid_json = run_hybrid_hash_search(home, &data_dir, "incompleteshardprobe fallback");
    let lexical_json = run_lexical_search(home, &data_dir, "incompleteshardprobe fallback");

    let hybrid_meta = hybrid_json
        .get("_meta")
        .and_then(Value::as_object)
        .unwrap_or_else(|| {
            panic!("hybrid fail-open search must include robot _meta: {hybrid_json}")
        });
    assert_eq!(
        hybrid_meta
            .get("requested_search_mode")
            .and_then(Value::as_str),
        Some("hybrid"),
        "explicit hybrid intent must be preserved"
    );
    assert_eq!(
        hybrid_meta.get("search_mode").and_then(Value::as_str),
        Some("lexical"),
        "incomplete shard generations must not realize hybrid mode"
    );
    assert_eq!(
        hybrid_meta.get("fallback_tier").and_then(Value::as_str),
        Some("lexical"),
        "incomplete shard generations must fail open to lexical"
    );
    assert_eq!(
        hybrid_meta
            .get("semantic_refinement")
            .and_then(Value::as_bool),
        Some(false),
        "incomplete shard generations must not claim semantic refinement"
    );
    let fallback_reason = hybrid_meta
        .get("fallback_reason")
        .and_then(Value::as_str)
        .unwrap_or_else(|| {
            panic!("incomplete shard fail-open must explain the semantic demotion: {hybrid_meta:?}")
        });
    assert!(
        fallback_reason.contains("semantic"),
        "fallback_reason should name semantic unavailability; got {fallback_reason:?}"
    );

    let hybrid_hits = hybrid_json
        .get("hits")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let lexical_hits = lexical_json
        .get("hits")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    assert!(
        !hybrid_hits.is_empty(),
        "hybrid fail-open search must still return lexical hits: {hybrid_json}"
    );
    assert_eq!(
        hit_keys(&hybrid_hits),
        hit_keys(&lexical_hits),
        "incomplete semantic shards must preserve explicit lexical hit identity while failing open"
    );
    assert_eq!(
        hybrid_json.get("count").and_then(Value::as_u64),
        lexical_json.get("count").and_then(Value::as_u64),
        "hybrid fail-open count must match explicit lexical count"
    );
    assert_eq!(
        hybrid_json.get("total_matches").and_then(Value::as_u64),
        lexical_json.get("total_matches").and_then(Value::as_u64),
        "hybrid fail-open total_matches must match explicit lexical total_matches"
    );
}
