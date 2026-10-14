//! GH #524: exercise migration through the shipped cass command, not just the
//! optional companion. All source/config/archive mutations use fresh tempdirs.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use coding_agent_search::connectors::codex::layout_identity::REGISTRY_DIRECTORY;
use coding_agent_search::franken_sync::compat::{
    ConnectionExt, OpenFlags, RowExt, open_with_flags,
};
use coding_agent_search::franken_sync::params;
use serde_json::{Value, json};

const SOURCE: &str = "native-layout-migration";
const SESSION: &str = "00000000-0000-4000-8000-000000000001";
const BEFORE: &str = "nativecodexbefore524";
const AFTER: &str = "nativecodexafter524";

fn message(text: &str) -> String {
    format!(
        "{}\n",
        json!({
            "timestamp": "2026-10-13T16:20:01.000Z",
            "type": "response_item",
            "payload": {"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": text}
            ]}
        })
    )
}

fn rollout() -> String {
    format!(
        "{}\n{}",
        json!({
            "timestamp": "2026-10-13T16:20:00.000Z", "type": "session_meta",
            "payload": {"id": SESSION, "timestamp": "2026-10-13T16:20:00.000Z",
                "cwd": "/tmp/cass-native-layout-test", "originator": "codex_cli_rs",
                "cli_version": "0.1.0"}
        }),
        message(BEFORE)
    )
}

struct Fixture {
    _root: tempfile::TempDir,
    home: PathBuf,
    data: PathBuf,
    flat: PathBuf,
    dated: PathBuf,
    mode: &'static str,
}

#[derive(Debug, PartialEq, Eq)]
struct Archive {
    conversations: Vec<(i64, String)>,
    messages: Vec<(i64, i64, i64, String)>,
}

impl Fixture {
    fn new(mode: &'static str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let data = root.path().join("data");
        let flat_root = root.path().join("snapshot/sessions");
        let dated_root = root.path().join("live/sessions");
        for directory in [
            home.join(".config/cass"),
            home.join("empty-codex"),
            data.clone(),
            flat_root.clone(),
            dated_root.join("2026/10/13"),
        ] {
            fs::create_dir_all(directory).unwrap();
        }
        fs::write(home.join(".config/cass/sources.toml"), format!(
            "disabled_agents = [\"claude\", \"cursor\", \"aider\", \"gemini\", \"openclaw\", \"codebuff\"]\n\n[[sources]]\nname = \"{SOURCE}\"\ntype = \"local\"\npaths = [{}, {}]\n",
            serde_json::to_string(&flat_root).unwrap(),
            serde_json::to_string(&dated_root).unwrap(),
        )).unwrap();
        let name = format!("rollout-2026-10-13T16-20-00-{SESSION}.jsonl");
        Self {
            flat: flat_root.join(&name),
            dated: dated_root.join("2026/10/13").join(name),
            _root: root,
            home,
            data,
            mode,
        }
    }

    fn command(&self, companion: bool) -> assert_cmd::Command {
        let mut command = assert_cmd::Command::new(if companion {
            assert_cmd::cargo::cargo_bin!("cass-codex-layout-migrate")
        } else {
            assert_cmd::cargo::cargo_bin!("cass")
        });
        command
            .env_clear()
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

    fn migration(&self, apply: bool, companion: bool) -> assert_cmd::Command {
        let mut command = self.command(companion);
        if !companion {
            command.arg("codex-layout-migrate");
        }
        command
            .arg("--data-dir")
            .arg(&self.data)
            .args(["--source", SOURCE])
            .arg("--from")
            .arg(&self.flat)
            .arg("--to")
            .arg(&self.dated)
            .arg("--json");
        if apply {
            command.arg("--apply");
        }
        command
    }

    fn migrate(&self, apply: bool, companion: bool) -> Value {
        let output = self
            .migration(apply, companion)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        serde_json::from_slice(&output).unwrap()
    }

    fn index(&self, path: &Path) {
        self.command(false)
            .args(["index", "--watch-once"])
            .arg(path)
            .arg("--data-dir")
            .arg(&self.data)
            .args(["--json", "--no-progress-events"])
            .assert()
            .success();
    }

    fn archive(&self) -> Archive {
        let database = self.data.join("agent_search.db");
        let connection =
            open_with_flags(database.to_str().unwrap(), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let conversations = connection
            .query_map_collect(
                "SELECT c.id, c.external_id FROM conversations c JOIN agents a ON a.id=c.agent_id
             WHERE a.slug='codex' AND c.source_id=?1 ORDER BY c.id",
                params![SOURCE],
                |row| Ok((row.get_typed::<i64>(0)?, row.get_typed::<String>(1)?)),
            )
            .unwrap();
        let messages = connection
            .query_map_collect(
                "SELECT c.id, m.id, m.idx, m.content FROM conversations c
             JOIN agents a ON a.id=c.agent_id JOIN messages m ON m.conversation_id=c.id
             WHERE a.slug='codex' AND c.source_id=?1 ORDER BY c.id, m.idx",
                params![SOURCE],
                |row| {
                    Ok((
                        row.get_typed::<i64>(0)?,
                        row.get_typed::<i64>(1)?,
                        row.get_typed::<i64>(2)?,
                        row.get_typed::<String>(3)?,
                    ))
                },
            )
            .unwrap();
        connection.close_without_checkpoint().unwrap();
        Archive {
            conversations,
            messages,
        }
    }

    fn search_once(&self, needle: &str) {
        let output = self
            .command(false)
            .args([
                "search",
                needle,
                "--agent",
                "codex",
                "--mode",
                "lexical",
                "--json",
                "--no-maintenance",
            ])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let report: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(report["hits"].as_array().unwrap().len(), 1, "{report}");
    }

    fn seed(&self) {
        fs::write(&self.flat, rollout()).unwrap();
        self.index(&self.flat);
        fs::write(&self.dated, rollout()).unwrap();
    }

    fn refused(&self) -> Value {
        let output = self
            .migration(true, false)
            .assert()
            .code(5)
            .get_output()
            .clone();
        assert!(
            output.stdout.is_empty(),
            "failed migration wrote success data"
        );
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(error["error"]["kind"], "codex-layout-migration");
        assert_eq!(error["error"]["retryable"], false);
        error
    }
}

#[test]
fn native_migration_preserves_identity_in_both_orders_and_shares_companion_receipts() {
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
            fs::write(second, rollout()).unwrap();
            let sources = (fs::read(first).unwrap(), fs::read(second).unwrap());
            let native_preview = fixture.migrate(false, false);
            assert_eq!(native_preview, fixture.migrate(false, true));
            assert_eq!(native_preview["applied"], false);
            assert_eq!(
                native_preview["canonical_conversation_id"],
                original.conversations[0].0
            );
            assert_eq!(fixture.archive(), original);
            assert!(!fixture.data.join(REGISTRY_DIRECTORY).exists());
            // Alternate the first publisher. Both entry points must accept the
            // same receipt without rewriting it or allocating a second identity.
            let applied = fixture.migrate(true, dated_first);
            assert_eq!(applied["applied"], true);
            assert_eq!(
                applied["canonical_external_id"],
                original.conversations[0].1
            );
            let receipt = PathBuf::from(applied["receipt_path"].as_str().unwrap());
            let receipt_bytes = fs::read(&receipt).unwrap();
            assert_eq!(
                fixture.migrate(true, !dated_first)["already_registered"],
                true
            );
            assert_eq!(fs::read(&receipt).unwrap(), receipt_bytes);
            assert_eq!(
                (fs::read(first).unwrap(), fs::read(second).unwrap()),
                sources
            );
            assert_eq!(fixture.archive(), original);
            fixture.index(second);
            assert_eq!(fixture.archive(), original);
            OpenOptions::new()
                .append(true)
                .open(second)
                .unwrap()
                .write_all(message(AFTER).as_bytes())
                .unwrap();
            fixture.index(second);
            let appended = fixture.archive();
            assert_eq!(appended.conversations, original.conversations);
            assert_eq!(appended.messages.len(), 2);
            assert!(appended.messages.contains(&original.messages[0]));
            fixture.index(first);
            fixture.index(second);
            assert_eq!(
                fixture.archive(),
                appended,
                "replay must not duplicate or delete history"
            );
            fixture.search_once(BEFORE);
            fixture.search_once(AFTER);
        }
    }
}

#[test]
fn native_conflict_errors_are_structured_and_preserve_archive_and_sources() {
    for conflict in ["content", "metadata"] {
        let fixture = Fixture::new("1");
        fixture.seed();
        let original = fixture.archive();
        let replacement = if conflict == "content" {
            rollout().replace(BEFORE, "conflictingbody524")
        } else {
            rollout().replace(SESSION, "00000000-0000-4000-8000-000000000002")
        };
        fs::write(&fixture.dated, replacement).unwrap();
        let before = (
            fs::read(&fixture.flat).unwrap(),
            fs::read(&fixture.dated).unwrap(),
        );
        fixture.refused();
        assert_eq!(fixture.archive(), original);
        assert_eq!(
            (
                fs::read(&fixture.flat).unwrap(),
                fs::read(&fixture.dated).unwrap()
            ),
            before
        );
        assert!(!fixture.data.join(REGISTRY_DIRECTORY).exists());
    }
}

#[test]
fn native_migration_never_selects_or_deletes_a_preexisting_duplicate() {
    let fixture = Fixture::new("1");
    fixture.seed();
    fixture.index(&fixture.dated);
    let original = fixture.archive();
    assert_eq!(original.conversations.len(), 2);
    fixture.refused();
    assert_eq!(fixture.archive(), original);
    assert!(!fixture.data.join(REGISTRY_DIRECTORY).exists());
}

#[test]
fn native_migration_respects_the_indexers_real_archive_lease() {
    let fixture = Fixture::new("1");
    fixture.seed();
    let original = fixture.archive();
    let database = fixture.data.join("agent_search.db").canonicalize().unwrap();
    let mut lock_name = database.as_os_str().to_os_string();
    lock_name.push(".cass-index-run.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(PathBuf::from(lock_name))
        .unwrap();
    lock.try_lock().unwrap();
    let error = fixture.refused();
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("archive is being indexed")
    );
    assert!(!fixture.data.join(REGISTRY_DIRECTORY).exists());
    drop(lock);
    assert_eq!(fixture.archive(), original);
    assert_eq!(fixture.migrate(true, false)["applied"], true);
}

#[test]
fn native_help_and_argument_errors_do_not_open_or_initialize_an_archive() {
    let fixture = Fixture::new("1");
    let top = fixture
        .command(false)
        .arg("--help")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert!(
        String::from_utf8(top)
            .unwrap()
            .contains("codex-layout-migrate")
    );
    let help = fixture
        .command(false)
        .args(["codex-layout-migrate", "--help"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let help = String::from_utf8(help).unwrap();
    for flag in [
        "--data-dir",
        "--source",
        "--from",
        "--to",
        "--apply",
        "--json",
    ] {
        assert!(help.contains(flag), "missing {flag}");
    }
    fixture
        .command(false)
        .arg("codex-layout-migrate")
        .assert()
        .code(2);
    fixture
        .migration(false, false)
        .arg("--force")
        .assert()
        .code(2);
    // A search term is not a command request, even when its text is the command name.
    let search = fixture
        .command(false)
        .args(["search", "codex-layout-migrate", "--help"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert!(!String::from_utf8(search).unwrap().contains("--from"));
    assert_eq!(fs::read_dir(&fixture.data).unwrap().count(), 0);
}
