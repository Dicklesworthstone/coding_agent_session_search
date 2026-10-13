//! GH #524: reporter-style separate snapshot/live roots, real CLI, canonical
//! archive and lexical search. No private history or downloaded models needed.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use coding_agent_search::connectors::codex::layout_identity::REGISTRY_DIRECTORY;
use coding_agent_search::franken_sync::compat::{
    ConnectionExt, OpenFlags, RowExt, open_with_flags,
};
use coding_agent_search::franken_sync::params;
use coding_agent_search::search::canonicalize::{canonicalize_for_embedding, content_hash};
use coding_agent_search::search::vector_index::{SemanticDocId, role_code_from_str};
use serde_json::{Value, json};

const SOURCE: &str = "full-history-plan";
const SESSION: &str = "00000000-0000-4000-8000-000000000001";
const BEFORE: &str = "codexlayoutbeforeprobe524";
const AFTER: &str = "codexlayoutappendprobe524";
const FINAL: &str = "codexlayoutfinalprobe524";

fn message(text: &str) -> String {
    format!("{}\n", json!({
        "timestamp":"2026-10-13T16:20:01.000Z", "type":"response_item",
        "payload":{"type":"message", "role":"user", "content":[{
            "type":"input_text", "text":text
        }]}
    }))
}

fn rollout() -> String {
    format!("{}\n{}", json!({
        "timestamp":"2026-10-13T16:20:00.000Z", "type":"session_meta",
        "payload":{"id":SESSION, "timestamp":"2026-10-13T16:20:00.000Z",
            "cwd":"/tmp/cass-native-watch-probe", "originator":"codex_cli_rs",
            "cli_version":"0.1.0"}
    }), message(BEFORE))
}

struct Fixture {
    temp: tempfile::TempDir,
    home: PathBuf,
    data: PathBuf,
    flat: PathBuf,
    dated: PathBuf,
    mode: &'static str,
}

impl Fixture {
    fn new(mode: &'static str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let data = temp.path().join("data");
        let flat_root = temp.path().join("flat/sessions");
        let dated_root = temp.path().join("dated/sessions");
        for directory in [&home.join(".config/cass"), &home.join("empty-codex"),
            &data, &flat_root, &dated_root.join("2026/10/13")] {
            fs::create_dir_all(directory).unwrap();
        }
        let config = format!(
            "disabled_agents = [\"claude\", \"cursor\", \"aider\", \"gemini\", \"openclaw\", \"codebuff\"]\n\n[[sources]]\nname = \"{SOURCE}\"\ntype = \"local\"\npaths = [{}, {}]\n",
            serde_json::to_string(&flat_root).unwrap(),
            serde_json::to_string(&dated_root).unwrap(),
        );
        fs::write(home.join(".config/cass/sources.toml"), config).unwrap();
        let name = format!("rollout-2026-10-13T16-20-00-{SESSION}.jsonl");
        let flat = flat_root.join(&name);
        let dated = dated_root.join("2026/10/13").join(&name);
        Self { temp, home, data, flat, dated, mode }
    }

    fn command(&self, migration: bool) -> assert_cmd::Command {
        let binary = if migration {
            assert_cmd::cargo::cargo_bin!("cass-codex-layout-migrate")
        } else {
            assert_cmd::cargo::cargo_bin!("cass")
        };
        let mut command = assert_cmd::Command::new(binary);
        command.env_clear()
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("APPDATA", self.home.join("AppData/Roaming"))
            .env("LOCALAPPDATA", self.home.join("AppData/Local"))
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("CODEX_HOME", self.home.join("empty-codex"))
            .env("CASS_DATA_DIR", &self.data)
            .env("CASS_STREAMING_INDEX", self.mode)
            .env("CASS_AUTO_REFRESH", "0")
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .env("RUST_MIN_STACK", "134217728")
            .current_dir(&self.home)
            .timeout(Duration::from_secs(180));
        for name in ["SystemRoot", "WINDIR"] {
            if let Ok(value) = dotenvy::var(name) {
                command.env(name, value);
            }
        }
        command
    }

    fn index_command(&self, path: &Path) -> assert_cmd::Command {
        let mut command = self.command(false);
        command.args(["index", "--watch-once"]).arg(path)
            .arg("--data-dir").arg(&self.data)
            .args(["--json", "--no-progress-events"]);
        command
    }

    fn index(&self, path: &Path) {
        self.index_command(path).assert().success();
    }

    fn migrate_command(&self, apply: bool) -> assert_cmd::Command {
        let mut command = self.command(true);
        command.arg("--data-dir").arg(&self.data)
            .args(["--source", SOURCE])
            .arg("--from").arg(&self.flat)
            .arg("--to").arg(&self.dated)
            .arg("--json");
        if apply { command.arg("--apply"); }
        command
    }

    fn migrate(&self, apply: bool) -> Value {
        let output = self.migrate_command(apply).assert().success()
            .get_output().stdout.clone();
        serde_json::from_slice(&output).unwrap()
    }

    fn search(&self, needle: &str, count: usize) -> Value {
        let output = self.command(false)
            .args(["search", needle, "--agent", "codex", "--mode", "lexical",
                "--json", "--no-maintenance"])
            .assert().success().get_output().stdout.clone();
        let report: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(report["hits"].as_array().unwrap().len(), count, "{report}");
        report
    }

    fn archive(&self) -> Archive {
        let database = self.data.join("agent_search.db");
        let connection = open_with_flags(
            database.to_str().unwrap(), OpenFlags::SQLITE_OPEN_READ_ONLY,
        ).unwrap();
        connection.execute("PRAGMA query_only = ON").unwrap();
        let conversations = connection.query_map_collect(
            "SELECT c.id, c.external_id FROM conversations c
             JOIN agents a ON a.id=c.agent_id
             WHERE a.slug='codex' AND c.source_id=?1 ORDER BY c.id",
            params![SOURCE],
            |row| Ok((row.get_typed::<i64>(0)?, row.get_typed::<String>(1)?)),
        ).unwrap();
        let messages = connection.query_map_collect(
            "SELECT c.id, m.id, m.idx, m.content FROM conversations c
             JOIN agents a ON a.id=c.agent_id
             JOIN messages m ON m.conversation_id=c.id
             WHERE a.slug='codex' AND c.source_id=?1 ORDER BY c.id, m.id, m.idx",
            params![SOURCE],
            |row| Ok((row.get_typed::<i64>(0)?, row.get_typed::<i64>(1)?,
                row.get_typed::<i64>(2)?, row.get_typed::<String>(3)?)),
        ).unwrap();
        // Use the production encoder and content canonicalizer, not a guessed
        // path-based ID. These short fixtures have exactly one semantic chunk.
        let semantic_documents = connection.query_map_collect(
            "SELECT m.id, c.agent_id, COALESCE(c.workspace_id,0), c.source_id,
                    m.created_at, m.content, m.role
             FROM messages m JOIN conversations c ON c.id=m.conversation_id
             JOIN agents a ON a.id=c.agent_id
             WHERE a.slug='codex' AND c.source_id=?1 ORDER BY m.id",
            params![SOURCE],
            |row| Ok((row.get_typed::<i64>(0)?, row.get_typed::<i64>(1)?,
                row.get_typed::<i64>(2)?, row.get_typed::<String>(3)?,
                row.get_typed::<Option<i64>>(4)?, row.get_typed::<String>(5)?,
                row.get_typed::<String>(6)?)),
        ).unwrap().into_iter().map(|(message, agent, workspace, source, created, content, role)| {
            SemanticDocId {
                message_id: u64::try_from(message).unwrap(),
                chunk_idx: 0,
                agent_id: u32::try_from(agent).unwrap(),
                workspace_id: u32::try_from(workspace).unwrap(),
                source_id: crc32fast::hash(source.as_bytes()),
                role: role_code_from_str(&role).unwrap(),
                created_at_ms: created.unwrap(),
                content_hash: Some(content_hash(&canonicalize_for_embedding(&content))),
            }.to_doc_id_string()
        }).collect();
        Archive { conversations, messages, semantic_documents }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Archive {
    conversations: Vec<(i64, String)>,
    messages: Vec<(i64, i64, i64, String)>,
    semantic_documents: Vec<String>,
}

fn append(path: &Path, text: &str) {
    OpenOptions::new().append(true).open(path).unwrap()
        .write_all(message(text).as_bytes()).unwrap();
}

#[test]
fn both_indexing_orders_keep_canonical_ids_messages_search_and_idempotence() {
    for mode in ["0", "1"] {
        for dated_first in [false, true] {
            let fixture = Fixture::new(mode);
            let (first, second) = if dated_first {
                (&fixture.dated, &fixture.flat)
            } else {
                (&fixture.flat, &fixture.dated)
            };
            fs::write(first, rollout()).unwrap();
            fixture.index(first);
            let original = fixture.archive();
            assert_eq!(original.conversations.len(), 1);
            assert_eq!(original.messages.len(), 1);
            fixture.search(BEFORE, 1);
            fs::write(second, rollout()).unwrap();
            let dry = fixture.migrate(false);
            assert_eq!(dry["applied"], false);
            assert_eq!(dry["canonical_conversation_id"], original.conversations[0].0);
            assert_eq!(fixture.archive(), original);
            assert!(!fixture.data.join(REGISTRY_DIRECTORY).exists());
            let applied = fixture.migrate(true);
            assert_eq!(applied["canonical_external_id"], original.conversations[0].1);
            let receipt = PathBuf::from(applied["receipt_path"].as_str().unwrap());
            let receipt_before = fs::read(&receipt).unwrap();
            assert_eq!(fixture.migrate(true)["already_registered"], true);
            assert_eq!(fs::read(&receipt).unwrap(), receipt_before);
            for _ in 0..2 {
                fixture.index(second);
                fixture.index(first);
                assert_eq!(fixture.archive(), original);
                fixture.search(BEFORE, 1);
            }
            append(second, AFTER);
            fixture.index(second);
            let appended = fixture.archive();
            assert_eq!(appended.conversations, original.conversations);
            assert_eq!(appended.messages.len(), 2);
            assert!(appended.messages.contains(&original.messages[0]));
            assert!(appended.semantic_documents.contains(&original.semantic_documents[0]));
            fixture.search(BEFORE, 1);
            fixture.search(AFTER, 1);
            fixture.index(first);
            fixture.index(second);
            assert_eq!(fixture.archive(), appended, "a shorter old copy must not delete newer history");
            // The operator may retire the snapshot after registration; this
            // test retains it outside the configured roots rather than deletes.
            fs::rename(first, fixture.temp.path().join("retained-original.jsonl")).unwrap();
            append(second, FINAL);
            fixture.index(second);
            let final_archive = fixture.archive();
            assert_eq!(final_archive.conversations, original.conversations);
            assert_eq!(final_archive.messages.len(), 3);
            for old in appended.messages { assert!(final_archive.messages.contains(&old)); }
            for old in appended.semantic_documents {
                assert!(final_archive.semantic_documents.contains(&old));
            }
            fixture.search(FINAL, 1);
            fixture.index(second);
            assert_eq!(fixture.archive(), final_archive);
        }
    }
}

#[test]
fn registration_refuses_conflicting_content_or_native_session_metadata_without_mutation() {
    for conflict in ["content", "metadata", "concatenated"] {
        let fixture = Fixture::new("1");
        fs::write(&fixture.flat, rollout()).unwrap();
        fixture.index(&fixture.flat);
        let original = fixture.archive();
        let bad = match conflict {
            "content" => rollout().replace(BEFORE, "differentcontentprobe524"),
            "metadata" => rollout().replace(SESSION, "00000000-0000-4000-8000-000000000002"),
            _ => format!("{}{}", rollout(), rollout().replace(SESSION, "00000000-0000-4000-8000-000000000002")),
        };
        fs::write(&fixture.dated, bad).unwrap();
        fixture.migrate_command(true).assert().failure();
        assert_eq!(fixture.archive(), original);
        assert!(!fixture.data.join(REGISTRY_DIRECTORY).exists());
        fixture.search(BEFORE, 1);
    }
}

#[test]
fn preexisting_duplicate_conversations_are_never_deleted_or_arbitrarily_selected() {
    let fixture = Fixture::new("1");
    fs::write(&fixture.flat, rollout()).unwrap();
    fixture.index(&fixture.flat);
    fs::write(&fixture.dated, rollout()).unwrap();
    fixture.index(&fixture.dated);
    let before = fixture.archive();
    assert_eq!(before.conversations.len(), 2);
    assert_eq!(before.messages.len(), 2);
    fixture.migrate_command(true).assert().failure();
    assert_eq!(fixture.archive(), before);
    assert!(!fixture.data.join(REGISTRY_DIRECTORY).exists());
}

#[test]
fn divergent_post_migration_append_cannot_overwrite_the_canonical_history() {
    let fixture = Fixture::new("1");
    fs::write(&fixture.flat, rollout()).unwrap();
    fixture.index(&fixture.flat);
    fs::write(&fixture.dated, rollout()).unwrap();
    fixture.migrate(true);
    append(&fixture.dated, AFTER);
    fixture.index(&fixture.dated);
    let before = fixture.archive();
    append(&fixture.flat, "divergentbranchprobe524");
    // Some index modes report partial-scan failure in JSON rather than exit
    // status. In either case the rejected source must not mutate the archive.
    let output = fixture.index_command(&fixture.flat).output().unwrap();
    let report: Value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
    assert!(!output.status.success()
        || report["success"] == false
        || report["indexing_stats"]["scan_had_errors"] == true,
        "conflict must be visible: {report}");
    assert_eq!(fixture.archive(), before);
    fixture.search(AFTER, 1);
    fixture.search("divergentbranchprobe524", 0);
}

#[test]
fn source_configuration_ambiguity_and_cross_source_moves_are_refused() {
    for same_owner in [false, true] {
        let fixture = Fixture::new("1");
        fs::write(&fixture.flat, rollout()).unwrap();
        fixture.index(&fixture.flat);
        fs::write(&fixture.dated, rollout()).unwrap();
        let path = fixture.home.join(".config/cass/sources.toml");
        let mut config: coding_agent_search::sources::config::SourcesConfig =
            toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let dated_root = fixture.dated.parent().unwrap().ancestors().nth(3).unwrap();
        if !same_owner {
            config.sources[0].paths.retain(|root| Path::new(root) != dated_root);
        }
        let mut other = coding_agent_search::sources::config::SourceDefinition::local("other-owner");
        other.paths = vec![dated_root.to_string_lossy().into_owned()];
        config.sources.push(other);
        fs::write(path, toml::to_string(&config).unwrap()).unwrap();
        let original = fixture.archive();
        fixture.migrate_command(true).assert().failure();
        assert_eq!(fixture.archive(), original);
        assert!(!fixture.data.join(REGISTRY_DIRECTORY).exists());
    }
}

#[test]
fn registration_respects_the_existing_archive_writer_lease() {
    let fixture = Fixture::new("1");
    fs::write(&fixture.flat, rollout()).unwrap();
    fixture.index(&fixture.flat);
    fs::write(&fixture.dated, rollout()).unwrap();
    let database = fs::canonicalize(fixture.data.join("agent_search.db")).unwrap();
    let mut lock_path = database.as_os_str().to_os_string();
    lock_path.push(".cass-index-run.lock");
    let lock = OpenOptions::new().create(true).truncate(false).read(true).write(true)
        .open(PathBuf::from(lock_path)).unwrap();
    lock.try_lock().unwrap();
    let original = fixture.archive();
    fixture.migrate_command(true).assert().failure();
    assert_eq!(fixture.archive(), original);
    drop(lock);
    fixture.migrate(true);
}

#[test]
fn migration_of_an_existing_archive_accepts_an_already_extended_destination() {
    for dated_first in [false, true] {
        let fixture = Fixture::new("1");
        let (first, second) = if dated_first {
            (&fixture.dated, &fixture.flat)
        } else {
            (&fixture.flat, &fixture.dated)
        };
        fs::write(first, rollout()).unwrap();
        fixture.index(first);
        let original = fixture.archive();
        fs::write(second, format!("{}{}", rollout(), message(AFTER))).unwrap();
        fixture.migrate(true);
        fixture.index(second);
        let migrated = fixture.archive();
        assert_eq!(migrated.conversations, original.conversations);
        assert_eq!(migrated.messages.len(), 2);
        assert!(migrated.messages.contains(&original.messages[0]));
        assert!(migrated.semantic_documents.contains(&original.semantic_documents[0]));
        fixture.search(BEFORE, 1);
        fixture.search(AFTER, 1);
        fixture.index(first);
        assert_eq!(fixture.archive(), migrated);
    }
}
