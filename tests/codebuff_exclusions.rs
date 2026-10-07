//! Codebuff source admission must protect canonical rows, search and raw copies.
//! These tests run the real CASS CLI with the native GH511 time-of-day records.

use coding_agent_search::raw_mirror::storage_summary;
use coding_agent_search::storage::sqlite::SqliteStorage;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

const PRIVATE: &str = "codebuffprivateproof9z";
const PUBLIC: &str = "codebuffpublicproof7z";

struct Fixture {
    home: tempfile::TempDir,
    private: PathBuf,
    public: PathBuf,
}

fn write_old(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).unwrap();
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::from_secs(1_774_113_351))
        .unwrap();
}

impl Fixture {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let projects = home.path().join(".config/manicode/projects");
        let chat = "chats/2026-03-21T17-14-03.768Z/chat-messages.json";
        let private = projects.join("private").join(chat);
        // A component-prefix exclusion must not hide this independent project.
        let public = projects.join("private-copy").join(chat);
        for (path, content) in [(&private, PRIVATE), (&public, PUBLIC)] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            write_old(
                path,
                &serde_json::to_vec(&json!([
                    {"id":"user-1774113351457", "variant":"user", "content":content,
                     "timestamp":"01:15 PM"}
                ]))
                .unwrap(),
            );
            write_old(
                &path.with_file_name("run-state.json"),
                br#"{"sessionState":{"fileContext":{"projectRoot":"/synthetic/probe"}}}"#,
            );
        }
        Self { home, private, public }
    }

    fn data(&self) -> PathBuf {
        self.home.path().join("data")
    }

    fn projects(&self) -> PathBuf {
        self.home.path().join(".config/manicode/projects")
    }

    fn command(&self, mode: &str, exclusions: &str) -> assert_cmd::Command {
        let home = self.home.path();
        let mut command = assert_cmd::Command::new(assert_cmd::cargo::cargo_bin!("cass"));
        command.env_clear();
        for name in ["SystemRoot", "WINDIR"] {
            if let Ok(value) = dotenvy::var(name) {
                command.env(name, value);
            }
        }
        command
            .env("HOME", home)
            .env("USERPROFILE", home)
            .env("APPDATA", home.join("AppData/Roaming"))
            .env("LOCALAPPDATA", home.join("AppData/Local"))
            .env("CASS_DATA_DIR", self.data())
            .env("CASS_STREAMING_INDEX", mode)
            .env("CASS_EXCLUDE_PATHS", exclusions)
            .env("CASS_IGNORE_SOURCES_CONFIG", "1")
            .env("CASS_AUTO_REFRESH", "0")
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .env("RUST_MIN_STACK", "134217728")
            .current_dir(home)
            .timeout(Duration::from_secs(180));
        // No XDG variables or Codebuff root override: also exercise the
        // home-relative default store, including on native macOS.
        command
    }

    fn index(&self, mode: &str, exclusions: &str, full: bool) -> Value {
        let mut command = self.command(mode, exclusions);
        command.args(["index", "--json", "--no-progress-events"]);
        if full {
            command.arg("--full");
        }
        let output = command.assert().success().get_output().stdout.clone();
        let report: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(report["success"], true, "{report}");
        assert_eq!(report["indexing_stats"]["scan_had_errors"], false, "{report}");
        report
    }

    fn assert_sources(&self, expected: &[&Path]) {
        let storage = SqliteStorage::open_readonly(&self.data().join("agent_search.db")).unwrap();
        let rows = storage.list_conversations(100, 0).unwrap();
        let mut actual: Vec<_> = rows
            .iter()
            .map(|row| row.source_path.canonicalize().unwrap())
            .collect();
        actual.sort();
        let mut expected: Vec<_> = expected.iter().map(|path| path.canonicalize().unwrap()).collect();
        expected.sort();
        assert_eq!(actual, expected, "canonical sources must honor exclusions");
        for row in rows {
            let messages = storage.fetch_messages(row.id.unwrap()).unwrap();
            assert_eq!(messages.len(), 1, "retries must not duplicate messages");
            assert_eq!(messages[0].created_at, Some(1_774_113_351_457));
        }
    }

    fn assert_hits(&self, token: &str, expected: usize) {
        let output = self.command("1", "")
            .args(["search", token, "--agent", "codebuff", "--mode", "lexical", "--json", "--no-maintenance"])
            .assert().success().get_output().stdout.clone();
        let result: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(result["hits"].as_array().unwrap().len(), expected, "{result}");
    }

    fn source_snapshot(&self) -> Vec<(PathBuf, Vec<u8>, std::time::SystemTime)> {
        [&self.private, &self.public].into_iter().flat_map(|primary| {
            [primary.clone(), primary.with_file_name("run-state.json")]
        }).map(|path| {
            let bytes = fs::read(&path).unwrap();
            let mtime = fs::metadata(&path).unwrap().modified().unwrap();
            (path, bytes, mtime)
        }).collect()
    }
}

fn assert_unchanged(snapshot: &[(PathBuf, Vec<u8>, std::time::SystemTime)]) {
    for (path, bytes, mtime) in snapshot {
        assert_eq!(&fs::read(path).unwrap(), bytes);
        assert_eq!(&fs::metadata(path).unwrap().modified().unwrap(), mtime);
    }
}

fn files_containing(root: &Path, token: &str) -> Vec<PathBuf> {
    let mut pending = vec![root.to_path_buf()];
    let mut found = Vec::new();
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() && fs::read(entry.path()).unwrap()
                .windows(token.len()).any(|bytes| bytes == token.as_bytes()) {
                found.push(entry.path());
            }
        }
    }
    found
}

#[test]
fn codebuff_exclusions_keep_private_bytes_out_and_resume_without_source_mutation() {
    for mode in ["0", "1"] {
        let fixture = Fixture::new();
        let snapshot = fixture.source_snapshot();
        // Relative scope, mixed delimiters and whitespace share the same
        // admission semantics as the existing Codex/raw-mirror policy.
        let policy = " , .config/manicode/projects/private\n, ";
        fixture.index(mode, policy, true);
        fixture.assert_sources(&[&fixture.public]);
        fixture.assert_hits(PRIVATE, 0);
        fixture.assert_hits(PUBLIC, 1);
        assert!(files_containing(&fixture.data(), PRIVATE).is_empty());
        assert!(!files_containing(&fixture.data(), PUBLIC).is_empty(), "positive byte-sweep control");
        assert_eq!(storage_summary(&fixture.data()).invalid_manifest_count, 0);
        assert_unchanged(&snapshot);

        // The excluded source and its metadata still have March mtimes.
        // Clearing the policy must reach them on an ordinary incremental run.
        fixture.index(mode, "", false);
        fixture.assert_sources(&[&fixture.private, &fixture.public]);
        fixture.assert_hits(PRIVATE, 1);
        fixture.assert_hits(PUBLIC, 1);
        fixture.index(mode, "", false);
        fixture.assert_sources(&[&fixture.private, &fixture.public]);
        assert_unchanged(&snapshot);
    }
}

#[test]
fn codebuff_exclusions_on_primary_or_sidecar_prevent_malformed_chat_parsing() {
    for mode in ["0", "1"] {
        for excluded_name in ["chat-messages.json", "run-state.json"] {
            let fixture = Fixture::new();
            let original = fs::read(&fixture.private).unwrap();
            write_old(&fixture.private, b"[");
            let excluded = fixture.private.with_file_name(excluded_name);
            let snapshot = fixture.source_snapshot();
            fixture.index(mode, excluded.to_str().unwrap(), true);
            fixture.assert_sources(&[&fixture.public]);
            fixture.assert_hits(PUBLIC, 1);
            assert_unchanged(&snapshot);

            // A real negative control: unprotected FAD parsing of precisely
            // this native store fails, rather than silently accepting the bad
            // file. No replacement parser or mock is involved.
            let context = coding_agent_search::connectors::ScanContext::with_roots(
                fixture.data(),
                vec![coding_agent_search::connectors::ScanRoot::local(fixture.projects())],
                None,
            );
            use coding_agent_search::connectors::Connector;
            assert!(franken_agent_detection::CodebuffConnector::new().scan(&context).is_err());

            write_old(&fixture.private, &original);
            fixture.index(mode, "", false);
            fixture.assert_sources(&[&fixture.private, &fixture.public]);
            fixture.assert_hits(PRIVATE, 1);
        }
    }
}

#[test]
fn codebuff_exclusions_all_selected_inputs_never_fall_back_to_default_history() {
    for mode in ["0", "1"] {
        let fixture = Fixture::new();
        let snapshot = fixture.source_snapshot();
        let root = fixture.projects();
        fixture.index(mode, root.to_str().unwrap(), true);
        fixture.assert_sources(&[]);
        assert!(!fixture.data().join("raw-mirror").exists());
        assert_unchanged(&snapshot);
        fixture.index(mode, "", false);
        fixture.assert_sources(&[&fixture.private, &fixture.public]);
        assert_unchanged(&snapshot);
    }
}

#[test]
fn codebuff_exclusions_watch_once_uses_the_same_admission_as_initial_indexing() {
    for mode in ["0", "1"] {
        let fixture = Fixture::new();
        let policy = fixture.projects().join("private");
        fixture.index(mode, policy.to_str().unwrap(), true);
        // This unrequested default-store chat did not exist at initial index.
        // Empty explicit roots must not silently turn into default discovery.
        let unrelated = fixture.projects()
            .join("out-of-scope/chats/2026-03-21T17-14-03.768Z/chat-messages.json");
        fs::create_dir_all(unrelated.parent().unwrap()).unwrap();
        let unrelated_bytes = serde_json::to_vec(&json!([
            {"id":"user-1774113351457", "variant":"user", "content":"unrequestedcodebuffproof",
             "timestamp":"01:15 PM"}
        ])).unwrap();
        write_old(&unrelated, &unrelated_bytes);
        // Ensure this targeted event cannot be dismissed by an unchanged-root
        // fast path. The malformed file must never reach FAD's parser.
        fs::write(&fixture.private, b"[").unwrap();
        fs::File::options().write(true).open(&fixture.private).unwrap()
            .set_modified(std::time::SystemTime::now() + Duration::from_secs(10)).unwrap();
        let snapshot = fixture.source_snapshot();
        let output = fixture.command(mode, policy.to_str().unwrap())
            .args(["index", "--watch-once"])
            .arg(&fixture.private)
            .args(["--json", "--no-progress-events"])
            .assert().success().get_output().stdout.clone();
        let report: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(report["success"], true, "{report}");
        fixture.assert_sources(&[&fixture.public]);
        fixture.assert_hits(PUBLIC, 1);
        assert_eq!(fs::read(&unrelated).unwrap(), unrelated_bytes);
        assert_unchanged(&snapshot);
    }
}
