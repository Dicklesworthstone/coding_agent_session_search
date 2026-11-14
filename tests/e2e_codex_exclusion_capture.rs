//! GH #486: the CLI must exclude raw copies as well as canonical/search rows.
//! GH #506: `CASS_RAW_MIRROR=0` stops raw copies without excluding any rows.
//! Bead 0f1k0: an exclusion covers every connector (Gemini included) and every
//! spelling of the directory (`..`, relative, symlink alias).
//! GH #554: unrelated exclusions preserve virtual sessions in a shared database.
//! GH #542: raw-capture budget pauses are summarized and visible in status/health.
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
        let hidden = home
            .path()
            .join(".codex/sessions/private/rollout-hidden.jsonl");
        let archived = home
            .path()
            .join(".codex/archived_sessions/rollout-archived.json");
        let public = home
            .path()
            .join(".codex/sessions/public/rollout-public.jsonl");
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
                .set_times(
                    fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(1000)),
                )
                .unwrap();
        }
        Self {
            home,
            hidden,
            archived,
            public,
        }
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
            // HOME does not isolate Claude Code or Gemini on Windows (their
            // home comes from the profile known folder), so the host's real
            // sessions would be indexed into the test archive.
            .env("CLAUDE_CONFIG_DIR", home.join(".claude"))
            .env("GEMINI_HOME", home.join(".gemini/tmp"))
            .env(
                "OPENCODE_SQLITE_DB",
                home.join(".local/share/opencode/opencode.db"),
            )
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

    fn index(&self, streaming: &str, exclusions: &str, full: bool) -> Value {
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
        let summary: Value = serde_json::from_slice(&output).unwrap_or_else(|err| {
            panic!(
                "index --json must print one JSON document ({err}); stdout:\n{}",
                String::from_utf8_lossy(&output)
            )
        });
        assert_eq!(summary["success"], true, "{summary}");
        summary
    }

    fn assert_sources(&self, expected: &[&Path]) {
        let storage =
            SqliteStorage::open_readonly(&self.home.path().join("data/agent_search.db")).unwrap();
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
            .args([
                "search",
                token,
                "--mode",
                "lexical",
                "--json",
                "--no-maintenance",
            ])
            .timeout(Duration::from_secs(30))
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let result: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(
            result["hits"].as_array().unwrap().len(),
            expected,
            "{result}"
        );
    }
}

/// Two logical conversations whose source handles descend from one real file.
fn write_opencode_database(fixture: &Fixture) -> PathBuf {
    use coding_agent_search::franken_sync::{Connection, compat::ConnectionExt, params};

    let source = fixture
        .home
        .path()
        .join(".local/share/opencode/opencode.db");
    fs::create_dir_all(source.parent().unwrap()).unwrap();
    let conn = Connection::open(source.to_string_lossy().as_ref()).unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         CREATE TABLE session (
            id TEXT PRIMARY KEY, project_id TEXT, title TEXT, directory TEXT,
            time_created INTEGER, time_updated INTEGER
         );
         CREATE TABLE message (
            id TEXT PRIMARY KEY, session_id TEXT NOT NULL, data TEXT NOT NULL,
            time_created INTEGER, time_updated INTEGER
         );
         CREATE TABLE part (
            id TEXT PRIMARY KEY, message_id TEXT NOT NULL, session_id TEXT NOT NULL,
            data TEXT NOT NULL, time_created INTEGER, time_updated INTEGER
         );",
    )
    .unwrap();
    let timestamp = 1_767_225_600_000_i64;
    for (session, marker) in [
        ("ses-private", "cassopencodeprivate6z"),
        ("ses-public", "cassopencodepublic5z"),
    ] {
        conn.execute_compat(
            "INSERT INTO session VALUES (?1, 'project', ?1, '/work', ?2, ?2)",
            params![session, timestamp],
        )
        .unwrap();
        conn.execute_compat(
            "INSERT INTO message VALUES (?1, ?1, ?2, ?3, ?3)",
            params![
                session,
                json!({"role": "user", "time": {"created": timestamp}}).to_string(),
                timestamp
            ],
        )
        .unwrap();
        conn.execute_compat(
            "INSERT INTO part VALUES (?1, ?1, ?1, ?2, ?3, ?3)",
            params![
                session,
                json!({"type": "text", "text": marker}).to_string(),
                timestamp
            ],
        )
        .unwrap();
    }
    conn.execute("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
    drop(conn);
    source
}

#[test]
fn gh554_unrelated_exclusions_preserve_opencode_database_sessions() {
    for streaming in ["0", "1"] {
        let fixture = Fixture::new();
        let source = write_opencode_database(&fixture);
        let original = fs::read(&source).unwrap();
        let private = fixture.home.path().join("unrelated-private");
        fs::create_dir(&private).unwrap();
        let exclusions = format!("{},missing/private", private.display());
        let summary = fixture.index(streaming, &exclusions, true);
        assert_eq!(
            summary["connector_summary"]["opencode"]["indexed"], 2,
            "{summary}"
        );
        assert_eq!(
            summary["connector_summary"]["opencode"]["skipped"], 0,
            "{summary}"
        );
        fixture.assert_sources(&[
            &fixture.hidden,
            &fixture.archived,
            &fixture.public,
            &source.join("ses-private"),
            &source.join("ses-public"),
        ]);
        fixture.assert_hits("cassopencodeprivate6z", 1);
        fixture.assert_hits("cassopencodepublic5z", 1);
        assert_eq!(fs::read(&source).unwrap(), original);
    }
}

#[test]
fn gh554_database_exclusion_remains_private_and_removal_recovers_old_sessions() {
    for streaming in ["0", "1"] {
        let fixture = Fixture::new();
        let source = write_opencode_database(&fixture);
        let original = fs::read(&source).unwrap();
        let summary = fixture.index(streaming, &source.display().to_string(), true);
        assert_eq!(
            summary["connector_summary"]["opencode"]["indexed"], 0,
            "{summary}"
        );
        fixture.assert_sources(&[&fixture.hidden, &fixture.archived, &fixture.public]);
        for marker in ["cassopencodeprivate6z", "cassopencodepublic5z"] {
            assert!(files_containing(&fixture.home.path().join("data"), marker).is_empty());
            fixture.assert_hits(marker, 0);
        }
        assert!(!fixture.captured_sources().contains(&source));

        fixture.index(streaming, "", false);
        fixture.assert_sources(&[
            &fixture.hidden,
            &fixture.archived,
            &fixture.public,
            &source.join("ses-private"),
            &source.join("ses-public"),
        ]);
        fixture.assert_hits("cassopencodeprivate6z", 1);
        fixture.assert_hits("cassopencodepublic5z", 1);
        assert_eq!(fs::read(&source).unwrap(), original);
    }
}

/// Explicit OpenCode watch selects one provider. FAD's separate policy parser
/// still refuses virtual policy entries in other providers; this regression
/// exercises CASS's post-parser admission and skipped-source accounting. A raw
/// copy cannot separate the sessions, so the entire shared DB is withheld.
#[test]
fn gh554_watch_once_virtual_session_exclusion_keeps_its_sibling_and_reports_skip() {
    let fixture = Fixture::new();
    fixture.index("1", "", true);
    let source = write_opencode_database(&fixture);
    let excluded = source.join("ses-private");
    let output =
        assert_cmd::Command::from_std(fixture.command("1", &excluded.display().to_string()))
            .args(["index", "--watch-once"])
            .arg(&source)
            .args(["--json", "--no-progress-events"])
            .timeout(Duration::from_secs(180))
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
    let report: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(report["success"], true, "{report}");
    assert_eq!(
        report["connector_summary"]["opencode"]["indexed"], 1,
        "{report}"
    );
    assert_eq!(
        report["connector_summary"]["opencode"]["skipped"], 1,
        "{report}"
    );
    fixture.assert_sources(&[
        &fixture.hidden,
        &fixture.archived,
        &fixture.public,
        &source.join("ses-public"),
    ]);
    fixture.assert_hits("cassopencodeprivate6z", 0);
    fixture.assert_hits("cassopencodepublic5z", 1);
    assert!(
        !fixture.captured_sources().contains(&source),
        "raw capture must withhold a shared DB containing an excluded session"
    );
    let leaked = files_containing(&fixture.home.path().join("data"), "cassopencodeprivate6z");
    assert!(
        leaked.is_empty(),
        "excluded session bytes leaked into {leaked:?}"
    );
}

impl Fixture {
    /// Source paths of the raw-mirror captures taken from this fixture's home,
    /// sorted. VS Code-based connectors (Copilot) resolve `%APPDATA%` through
    /// the Windows profile known folder, which no environment variable
    /// redirects, so a Windows run also captures the host's own Copilot
    /// stores; those are not this fixture's sources.
    fn captured_sources(&self) -> Vec<PathBuf> {
        let home = self.home.path();
        let Ok(entries) = fs::read_dir(home.join("data/raw-mirror/v1/manifests")) else {
            return Vec::new();
        };
        let mut captured: Vec<PathBuf> = entries
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .filter_map(|path| {
                let manifest: Value = serde_json::from_slice(&fs::read(&path).unwrap())
                    .unwrap_or_else(|err| panic!("{}: {err}", path.display()));
                let original = PathBuf::from(manifest["original_path"].as_str().unwrap());
                original.starts_with(home).then_some(original)
            })
            .collect();
        captured.sort();
        captured
    }

    fn all_sources(&self) -> Vec<PathBuf> {
        let mut all = vec![
            self.hidden.clone(),
            self.archived.clone(),
            self.public.clone(),
        ];
        all.sort();
        all
    }
}

/// One single-message Codex rollout under a fresh directory.
fn write_rollout(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let record = json!({"type":"response_item","payload":{"role":"user","content":text}});
    fs::write(path, format!("{record}\n")).unwrap();
}

/// Files under `root` whose bytes contain `needle`: the canonical DB and its
/// sidecars, raw mirror, lexical and vector assets, checkpoints and logs.
fn files_containing(root: &Path, needle: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => pending.push(path),
                Ok(kind)
                    if kind.is_file()
                        && fs::read(&path).is_ok_and(|bytes| {
                            bytes
                                .windows(needle.len())
                                .any(|window| window == needle.as_bytes())
                        }) =>
                {
                    found.push(path);
                }
                _ => {}
            }
        }
    }
    found
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
        assert_eq!(fixture.captured_sources(), Vec::<PathBuf>::new());
        for (path, original) in [&fixture.hidden, &fixture.archived, &fixture.public]
            .into_iter()
            .zip(before)
        {
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
        assert_eq!(fixture.captured_sources(), vec![fixture.public.clone()]);
        assert_eq!(summary.invalid_manifest_count, 0);
        fixture.assert_hits("cassprivateproof9z", 0);
        fixture.assert_hits("cassarchiveproof8z", 0);
        fixture.assert_hits("casspublicproof7z", 1);
        // Bead 2l1b0.37: an excluded sentinel reaches no byte of the data
        // directory. Positive control: the included one is found, so the sweep
        // can see stored text.
        let data = fixture.home.path().join("data");
        for excluded in ["cassprivateproof9z", "cassarchiveproof8z"] {
            assert_eq!(
                files_containing(&data, excluded),
                Vec::<PathBuf>::new(),
                "{excluded} leaked into the data dir (streaming={streaming})"
            );
        }
        assert!(
            !files_containing(&data, "casspublicproof7z").is_empty(),
            "the sweep must find the included sentinel (streaming={streaming})"
        );

        // No full rebuild and no source mutation: exclusion-aware watermark
        // preservation must make both previously omitted files reachable.
        fixture.index(streaming, "", false);
        fixture.assert_sources(&[&fixture.hidden, &fixture.archived, &fixture.public]);
        fixture.assert_hits("cassprivateproof9z", 1);
        fixture.assert_hits("cassarchiveproof8z", 1);
        let summary = storage_summary(&fixture.home.path().join("data"));
        assert_eq!(fixture.captured_sources(), fixture.all_sources());
        assert_eq!(summary.invalid_manifest_count, 0);

        fixture.index(streaming, "", false);
        fixture.assert_sources(&[&fixture.hidden, &fixture.archived, &fixture.public]);
        fixture.assert_hits("cassprivateproof9z", 1);
        fixture.assert_hits("cassarchiveproof8z", 1);
    }
}

/// GH #506: with `CASS_RAW_MIRROR=0` an index run writes and serves every
/// conversation but copies no source into the raw mirror, and doctor reports
/// the switch rather than a backfill gap. Control: the same fixture indexed
/// with the switch unset captures all three sources.
#[test]
fn raw_mirror_switch_indexes_without_raw_copies_and_doctor_discloses_it() {
    for streaming in ["0", "1"] {
        let fixture = Fixture::new();
        let home = fixture.home.path();
        let mut index = fixture.command(streaming, "");
        index
            .env("CASS_RAW_MIRROR", "0")
            .args(["index", "--json", "--full"]);
        let output = assert_cmd::Command::from_std(index)
            .timeout(Duration::from_secs(180))
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let summary: Value = serde_json::from_slice(&output).unwrap_or(Value::Null);
        assert_eq!(summary["success"], true, "{summary}");
        fixture.assert_sources(&[&fixture.hidden, &fixture.archived, &fixture.public]);
        fixture.assert_hits("casspublicproof7z", 1);
        assert!(
            !home.join("data/raw-mirror").exists(),
            "no raw copy may be written (streaming={streaming})"
        );

        let mut doctor = fixture.command(streaming, "");
        doctor
            .env("CASS_RAW_MIRROR", "0")
            .args(["doctor", "--json"]);
        let output = assert_cmd::Command::from_std(doctor)
            .timeout(Duration::from_secs(120))
            .output()
            .unwrap();
        let report: Value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
        let risk = &report["coverage_risk"];
        assert_eq!(risk["status"], "raw_mirror_capture_disabled", "{report}");
        assert_eq!(risk["raw_mirror_capture_disabled"], true, "{risk}");
        assert_eq!(risk["db_without_raw_mirror_count"], 3, "{risk}");
        let coverage_check = report["checks"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|check| check["name"] == "source_coverage")
            .cloned()
            .unwrap_or(Value::Null);
        assert_eq!(coverage_check["status"], "warn", "{coverage_check}");
        assert_eq!(coverage_check["fix_available"], false, "{coverage_check}");
        assert!(
            coverage_check["message"]
                .as_str()
                .is_some_and(|message| message.contains("CASS_RAW_MIRROR")),
            "{coverage_check}"
        );

        let control = Fixture::new();
        control.index(streaming, "", true);
        assert_eq!(
            control.captured_sources(),
            control.all_sources(),
            "streaming={streaming}"
        );
    }
}

#[test]
fn gh542_capture_budget_pause_is_bounded_visible_and_recovers_without_losing_search() {
    for streaming in ["0", "1"] {
        let fixture = Fixture::new();
        let mut index = fixture.command(streaming, "");
        index
            .env("CASS_RAW_MIRROR_MAX_BYTES", "0")
            .env("CASS_RAW_MIRROR_MIN_FREE_BYTES", "0")
            .args(["index", "--json", "--full"]);
        let output = assert_cmd::Command::from_std(index)
            .timeout(Duration::from_secs(180))
            .assert()
            .success()
            .get_output()
            .clone();
        let summary: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(summary["success"], true, "{summary}");
        fixture.assert_sources(&[&fixture.hidden, &fixture.archived, &fixture.public]);
        fixture.assert_hits("casspublicproof7z", 1);
        assert!(fixture.captured_sources().is_empty());

        let stderr = String::from_utf8_lossy(&output.stderr);
        let events: Vec<Value> = stderr
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        for event in ["raw_mirror_capture_paused", "raw_mirror_capture_summary"] {
            assert_eq!(
                events
                    .iter()
                    .filter(|value| value["event"] == event)
                    .count(),
                1,
                "one bounded {event} event expected: {stderr}"
            );
        }
        assert!(
            !stderr.contains("failed to capture discovered provider source")
                && !stderr.contains("failed to capture explicit scan-root source")
                && !stderr.contains("failed to capture parsed conversation source"),
            "budget refusals must not produce a warning per source: {stderr}"
        );

        let receipt_path = fixture.home.path().join("data/raw-mirror-capture.json");
        let receipt_before = fs::read(&receipt_path).unwrap();
        let mut observed_run_id = None;
        for surface in ["status", "health"] {
            let mut probe = fixture.command(streaming, "");
            probe
                // Current configuration must not relabel the historical pause
                // as a live probe or a successful resume.
                .env("CASS_RAW_MIRROR_MAX_BYTES", "1048576")
                .env("CASS_RAW_MIRROR_MIN_FREE_BYTES", "0")
                .args([surface, "--json"]);
            let output = assert_cmd::Command::from_std(probe)
                .timeout(Duration::from_secs(30))
                .assert()
                .success()
                .get_output()
                .stdout
                .clone();
            let report: Value = serde_json::from_slice(&output).unwrap();
            assert_eq!(report["healthy"], true, "{surface}: {report}");
            let capture = &report["raw_mirror_capture"];
            assert_eq!(capture["status"], "last_run_paused", "{capture}");
            assert_eq!(capture["receipt_status"], "recorded", "{capture}");
            assert_eq!(capture["configuration"]["max_bytes"], 1_048_576);
            let run = &capture["last_run"];
            assert_eq!(run["max_bytes"], 0);
            assert_eq!(run["completed"], true);
            assert_eq!(run["capture_paused"], true);
            assert_eq!(run["skipped_sources"], 3, "{run}");
            assert_eq!(run["skipped_sources_exact"], true);
            assert_eq!(run["counters_exact"], true);
            assert!(run["pause_attempts"].as_u64().unwrap() >= 3);
            assert!(run["requested_reservation_bytes"].as_u64().unwrap() > 0);
            assert_eq!(run["last_pause"]["max_bytes"], 0);
            assert!(run["finished_at_ms"].is_i64());
            if let Some(run_id) = &observed_run_id {
                assert_eq!(&run["run_id"], run_id);
            } else {
                observed_run_id = Some(run["run_id"].clone());
            }
            assert!(
                report["warnings"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|warning| warning.as_str().is_some_and(|text| {
                        text.to_ascii_lowercase().contains("raw") && text.contains("3")
                    })),
                "{surface} must explain the missing raw protection: {report}"
            );
        }
        assert_eq!(
            fs::read(&receipt_path).unwrap(),
            receipt_before,
            "status and health must not rewrite the receipt"
        );

        // The original sources remain unchanged. A full scan with the normal
        // budget can preserve them and replace the last-run observation.
        fixture.index(streaming, "", true);
        assert_eq!(fixture.captured_sources(), fixture.all_sources());
        let receipt: Value = serde_json::from_slice(&fs::read(receipt_path).unwrap()).unwrap();
        assert_eq!(receipt["completed"], true);
        assert_eq!(receipt["capture_paused"], false);
        assert_eq!(receipt["skipped_sources"], 0);
        assert_eq!(receipt["pause_attempts"], 0);
        assert!(receipt["last_pause"].is_null());
        assert_ne!(Some(receipt["run_id"].clone()), observed_run_id);
        fixture.assert_hits("cassprivateproof9z", 1);
    }
}

/// Bead 0f1k0: an excluded directory stays out of the data dir for every
/// connector and every spelling of the directory: as written, through a `..`
/// path, as a relative path and through a symlink alias. Before the fix the
/// Gemini reader, which never consults `CASS_EXCLUDE_PATHS`, indexed an
/// exactly named excluded project, and FAD 0.3.7's Claude reader indexed the
/// `..`, relative and alias spellings; the raw mirror withheld both, so the
/// sessions were searchable while their raw copies were refused.
#[test]
fn every_spelling_of_an_exclusion_covers_claude_codex_and_gemini_sessions() {
    fn gemini_session(path: &Path, session: &str, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let project = path
            .parent()
            .and_then(Path::parent)
            .and_then(Path::file_name)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let record = json!({
            "sessionId": session,
            "projectHash": project,
            "startTime": "2026-08-01T10:00:00Z",
            "lastUpdated": "2026-08-01T10:01:00Z",
            "messages": [
                {"type": "user", "content": text, "timestamp": "2026-08-01T10:00:00Z"},
            ],
        });
        fs::write(path, record.to_string()).unwrap();
    }

    fn claude_session(path: &Path, session: &str, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let record = json!({
            "parentUuid": null,
            "cwd": "/work",
            "sessionId": session,
            "version": "2.0.37",
            "type": "user",
            "message": {"role": "user", "content": text},
            "uuid": format!("{session}-1"),
            "timestamp": "2026-08-01T10:00:00.000Z",
        });
        fs::write(path, format!("{record}\n")).unwrap();
    }

    #[cfg(unix)]
    fn alias_dir(target: &Path, link: &Path) -> Option<()> {
        std::os::unix::fs::symlink(target, link).unwrap();
        Some(())
    }
    // Creating a Windows symlink needs a privilege CI runners may lack.
    #[cfg(not(unix))]
    fn alias_dir(_: &Path, _: &Path) -> Option<()> {
        None
    }

    // The excluded directories, relative to HOME, which is also the working
    // directory the harness runs cass in.
    const EXCLUDED: [&str; 3] = [
        ".claude/projects/-secret",
        ".codex/sessions/private",
        ".gemini/tmp/secrethash",
    ];
    // Spells one excluded directory (relative to HOME) as an exclusion entry;
    // `None` when the platform cannot express that spelling.
    type Spelling = fn(&Path, &str) -> Option<String>;
    let spellings: [(&str, Spelling); 6] = [
        ("exact", |home, dir| {
            Some(home.join(dir).display().to_string())
        }),
        ("dotdot", |home, dir| {
            let dir = Path::new(dir);
            let parent = dir.parent()?;
            let spelled = home
                .join(parent)
                .join("..")
                .join(parent.file_name()?)
                .join(dir.file_name()?);
            Some(spelled.display().to_string())
        }),
        ("relative", |_, dir| Some(dir.to_string())),
        ("alias", |home, dir| {
            let link = home.join("aliases").join(dir.replace('/', "-"));
            fs::create_dir_all(link.parent()?).unwrap();
            alias_dir(&home.join(dir), &link)?;
            Some(link.display().to_string())
        }),
        // Windows paths are case-insensitive and accept `/` as a separator
        // (bead 2l1b0.37); elsewhere both spellings name a different path.
        ("case", |home, dir| {
            cfg!(windows).then(|| home.join(dir).display().to_string().to_uppercase())
        }),
        ("slash", |home, dir| {
            cfg!(windows).then(|| home.join(dir).display().to_string().replace('\\', "/"))
        }),
    ];

    for (name, spelling) in spellings {
        for streaming in ["0", "1"] {
            let fixture = Fixture::new();
            let home = fixture.home.path();
            claude_session(
                &home.join(".claude/projects/-secret/secret.jsonl"),
                "secret",
                "cassclaudesecret6z",
            );
            claude_session(
                &home.join(".claude/projects/-open/open.jsonl"),
                "open",
                "cassclaudeopen5z",
            );
            gemini_session(
                &home.join(".gemini/tmp/secrethash/chats/session-secret.json"),
                "secret",
                "cassgeminisecret4z",
            );
            gemini_session(
                &home.join(".gemini/tmp/openhash/chats/session-open.json"),
                "open",
                "cassgeminiopen3z",
            );
            let Some(entries) = EXCLUDED
                .iter()
                .map(|dir| spelling(home, dir))
                .collect::<Option<Vec<_>>>()
            else {
                continue;
            };
            let exclusions = entries.join(",");
            fixture.index(streaming, &exclusions, true);
            let data = home.join("data");
            for excluded in [
                "cassclaudesecret6z",
                "cassprivateproof9z",
                "cassgeminisecret4z",
            ] {
                assert_eq!(
                    files_containing(&data, excluded),
                    Vec::<PathBuf>::new(),
                    "{excluded} leaked into the data dir ({name}: {exclusions}, streaming={streaming})"
                );
            }
            // Positive controls: the sibling sessions are indexed and found.
            for included in ["cassclaudeopen5z", "casspublicproof7z", "cassgeminiopen3z"] {
                fixture.assert_hits(included, 1);
            }
        }
    }
}

/// Bead 2l1b0.37: an exclusion is prospective. Conversations archived before
/// it was set stay stored and searchable (cass never deletes archived history
/// because of an exclusion), while a session written later into the excluded
/// directory is kept out.
#[test]
fn adding_an_exclusion_keeps_previously_archived_conversations() {
    for streaming in ["0", "1"] {
        let fixture = Fixture::new();
        fixture.index(streaming, "", true);
        fixture.assert_sources(&[&fixture.hidden, &fixture.archived, &fixture.public]);

        let private = fixture.hidden.parent().unwrap();
        write_rollout(&private.join("rollout-later.jsonl"), "casslaterproof5z");
        fixture.index(streaming, &private.display().to_string(), false);
        fixture.assert_sources(&[&fixture.hidden, &fixture.archived, &fixture.public]);
        fixture.assert_hits("cassprivateproof9z", 1);
        fixture.assert_hits("casslaterproof5z", 0);
        assert_eq!(
            files_containing(&fixture.home.path().join("data"), "casslaterproof5z"),
            Vec::<PathBuf>::new(),
            "a session written after the exclusion leaked (streaming={streaming})"
        );
    }
}

/// Bead 2l1b0.37: an excluded directory with a non-ASCII name, holding a
/// session and a metadata-only compressed rollout, leaves no byte in the data
/// dir and no trace in the command output, and neither file counts as a
/// failed source. The included control is indexed.
#[test]
fn unicode_and_zero_output_excluded_sources_leave_no_trace_in_data_or_output() {
    for streaming in ["0", "1"] {
        let fixture = Fixture::new();
        let excluded = fixture.home.path().join(".codex/sessions/私密-Ünïcode");
        write_rollout(
            &excluded.join("rollout-unicode.jsonl"),
            "cassunicodeproof4z",
        );
        let metadata = br#"{"type":"session_meta","payload":{"id":"cassmetaproof3z"}}
"#;
        fs::write(
            excluded.join("rollout-meta.jsonl.zst"),
            zstd::stream::encode_all(&metadata[..], 0).unwrap(),
        )
        .unwrap();
        let exclusions = format!(
            "{},{}",
            excluded.display(),
            fixture.hidden.parent().unwrap().display()
        );
        let output = assert_cmd::Command::from_std(fixture.command(streaming, &exclusions))
            .args(["index", "--json", "--full"])
            .timeout(Duration::from_secs(180))
            .assert()
            .success()
            .get_output()
            .clone();
        let summary: Value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
        assert_eq!(summary["success"], true, "{summary}");
        fixture.assert_sources(&[&fixture.archived, &fixture.public]);
        fixture.assert_hits("casspublicproof7z", 1);
        let data = fixture.home.path().join("data");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        for needle in [
            "cassunicodeproof4z",
            "cassmetaproof3z",
            "cassprivateproof9z",
        ] {
            assert_eq!(
                files_containing(&data, needle),
                Vec::<PathBuf>::new(),
                "{needle} leaked into the data dir (streaming={streaming})"
            );
            assert!(
                !stdout.contains(needle) && !stderr.contains(needle),
                "{needle} appeared in the index output (streaming={streaming})"
            );
        }
    }
}

/// Bead 2l1b0.37: an explicit `--watch-once` path inside an excluded
/// directory is excluded too, and the same run without the exclusion ingests
/// it, so the refusal is the exclusion's.
#[test]
fn watch_once_of_an_excluded_codex_rollout_ingests_nothing() {
    for streaming in ["0", "1"] {
        let fixture = Fixture::new();
        let private = fixture.hidden.parent().unwrap().display().to_string();
        fixture.index(streaming, &private, true);
        fixture.assert_sources(&[&fixture.archived, &fixture.public]);
        for (exclusions, expected) in [
            (private.as_str(), vec![&fixture.archived, &fixture.public]),
            (
                "",
                vec![&fixture.hidden, &fixture.archived, &fixture.public],
            ),
        ] {
            let output = assert_cmd::Command::from_std(fixture.command(streaming, exclusions))
                .args(["index", "--watch-once"])
                .arg(&fixture.hidden)
                .args(["--json", "--no-progress-events"])
                .timeout(Duration::from_secs(180))
                .assert()
                .success()
                .get_output()
                .stdout
                .clone();
            let report: Value = serde_json::from_slice(&output).unwrap_or(Value::Null);
            assert_eq!(report["success"], true, "{report}");
            let expected: Vec<&Path> = expected.into_iter().map(PathBuf::as_path).collect();
            fixture.assert_sources(&expected);
        }
        fixture.assert_hits("cassprivateproof9z", 1);
    }
}
