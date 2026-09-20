//! GH #486: the CLI must exclude raw copies as well as canonical/search rows.
//! Run with `cargo test --test e2e_codex_exclusion_capture` in a full CASS build.

use coding_agent_search::raw_mirror::storage_summary;
use coding_agent_search::storage::sqlite::SqliteStorage;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, UNIX_EPOCH};

struct Fixture {
    home: tempfile::TempDir,
    hidden: PathBuf,
    archived: PathBuf,
    public: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let hidden = home.path().join(".codex/sessions/private/rollout-hidden.jsonl");
        let archived = home.path().join(".codex/archived_sessions/rollout-archived.json");
        let public = home.path().join(".codex/sessions/public/rollout-public.jsonl");
        for (path, text) in [
            (&hidden, "cassprivateproof9z"),
            (&archived, "cassarchiveproof8z"),
            (&public, "casspublicproof7z"),
        ] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let message = json!({"role":"user","content":text});
            let record = if path.extension().unwrap() == "json" {
                json!({"items":[message]})
            } else {
                json!({"type":"response_item","payload":message})
            };
            fs::write(path, format!("{record}\n")).unwrap();
            // Removing an exclusion must not require touching the source or
            // inventing a newer mtime to make missed history eligible again.
            fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(1000)))
                .unwrap();
        }
        Self { home, hidden, archived, public }
    }

    fn command(&self, streaming: &str, exclusions: &str) -> Command {
        let home = self.home.path();
        let mut command = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
        command.env_clear();
        // Retain only OS runtime essentials, not another agent's configured
        // history roots, exclusions, database or source configuration.
        for key in ["PATH", "SystemRoot", "WINDIR"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        command
            .env("HOME", home)
            .env("USERPROFILE", home)
            .env("CODEX_HOME", home.join(".codex"))
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("APPDATA", home.join("AppData/Roaming"))
            .env("LOCALAPPDATA", home.join("AppData/Local"))
            .env("CASS_DATA_DIR", home.join("data"))
            .env("CASS_IGNORE_SOURCES_CONFIG", "1")
            .env("CASS_STREAMING_INDEX", streaming)
            .env("CASS_EXCLUDE_PATHS", exclusions)
            .env("CASS_AUTO_REFRESH", "0")
            .env("RUST_MIN_STACK", "134217728")
            .env("NO_COLOR", "1")
            .current_dir(home)
            .args(["--color", "never"]);
        command
    }

    fn index(&self, streaming: &str, exclusions: &str, full: bool) {
        let mut command = assert_cmd::Command::from_std(self.command(streaming, exclusions));
        command.args(["index", "--json"]);
        if full {
            command.arg("--full");
        }
        let output = command
            .timeout(Duration::from_secs(180))
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let summary: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(summary["success"], true, "{summary}");
    }

    fn assert_sources(&self, expected: &[&Path]) {
        let storage = SqliteStorage::open_readonly(&self.home.path().join("data/agent_search.db"))
            .unwrap();
        let rows = storage.list_conversations(100, 0).unwrap();
        let mut actual: Vec<_> = rows.iter().map(|row| row.source_path.as_path()).collect();
        actual.sort();
        let mut expected = expected.to_vec();
        expected.sort();
        assert_eq!(actual, expected);
        for row in rows {
            assert_eq!(storage.fetch_messages(row.id.unwrap()).unwrap().len(), 1);
        }
    }

    fn assert_hits(&self, token: &str, expected: usize) {
        let output = assert_cmd::Command::from_std(self.command("1", ""))
            .args(["search", token, "--mode", "lexical", "--json", "--no-maintenance"])
            .timeout(Duration::from_secs(30))
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let result: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(result["hits"].as_array().unwrap().len(), expected, "{result}");
    }
}

#[test]
fn all_excluded_inventory_never_falls_back_to_raw_copy_in_either_ingest_mode() {
    for streaming in ["0", "1"] {
        let fixture = Fixture::new();
        let home = fixture.home.path();
        let before = [&fixture.hidden, &fixture.archived, &fixture.public]
            .map(|path| fs::read(path).unwrap());
        fixture.index(streaming, home.join(".codex").to_str().unwrap(), true);
        fixture.assert_sources(&[]);
        assert!(!home.join("data/raw-mirror").exists());
        for (path, original) in [&fixture.hidden, &fixture.archived, &fixture.public].into_iter().zip(before) {
            assert_eq!(fs::read(path).unwrap(), original);
        }
    }
}

#[test]
fn removing_exclusions_ingests_old_sources_without_hidden_raw_copies_or_duplicate_rows() {
    for streaming in ["0", "1"] {
        let fixture = Fixture::new();
        let exclusions = format!(
            "{},{}",
            fixture.hidden.parent().unwrap().display(),
            fixture.archived.parent().unwrap().display(),
        );
        fixture.index(streaming, &exclusions, true);
        fixture.assert_sources(&[&fixture.public]);
        let summary = storage_summary(&fixture.home.path().join("data"));
        assert_eq!(summary.manifest_count, 1);
        assert_eq!(summary.invalid_manifest_count, 0);
        fixture.assert_hits("cassprivateproof9z", 0);
        fixture.assert_hits("cassarchiveproof8z", 0);
        fixture.assert_hits("casspublicproof7z", 1);

        // No full rebuild and no source mutation: exclusion-aware watermark
        // preservation must make both previously omitted files reachable.
        fixture.index(streaming, "", false);
        fixture.assert_sources(&[&fixture.hidden, &fixture.archived, &fixture.public]);
        fixture.assert_hits("cassprivateproof9z", 1);
        fixture.assert_hits("cassarchiveproof8z", 1);
        let summary = storage_summary(&fixture.home.path().join("data"));
        assert_eq!(summary.manifest_count, 3);
        assert_eq!(summary.invalid_manifest_count, 0);

        fixture.index(streaming, "", false);
        fixture.assert_sources(&[&fixture.hidden, &fixture.archived, &fixture.public]);
        fixture.assert_hits("cassprivateproof9z", 1);
        fixture.assert_hits("cassarchiveproof8z", 1);
    }
}
