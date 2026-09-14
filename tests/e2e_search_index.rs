//! E2E integration tests for search/index pipeline.
//!
//! Tests cover:
//! - Full index flow with temp data-dir
//! - Search with JSON output (hits, match_type, aggregations)
//! - Watch-once environment path functionality
//! - Trace/log file capture (no mocks)
//!
//! Part of bead: coding_agent_session_search-0jt (TST.11)

use chrono::{SecondsFormat, Utc};
use coding_agent_search::franken_sync::compat::{ConnectionExt, RowExt};
use coding_agent_search::search::tantivy::{
    Fields, SearchableIndexSummary, expected_index_dir, index_dir, open_federated_search_readers,
    searchable_index_summary,
};
use coding_agent_search::storage::sqlite::SqliteStorage;
use rusqlite::Connection as RusqliteConnection;
use serial_test::serial;
use std::fs;
use std::path::Path;
#[cfg(target_os = "linux")]
use std::process::Command as StdCommand;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[macro_use]
mod util;
// `cass_std_cmd` (the only consumer) is compiled on Linux only.
#[cfg(target_os = "linux")]
use util::e2e_log::E2eCommandEnvironment;
use util::e2e_log::{E2ePerformanceMetrics, PhaseTracker};

// =============================================================================
// E2E Logger Support
// =============================================================================

// GH472: full source coverage with conditional lexical rebuilding.
mod gh472 {
    use super::*;
    use serde_json::{Value, json};
    use std::cell::Cell;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    type Archive = BTreeMap<PathBuf, Value>;

    struct Fixture {
        _temp: tempfile::TempDir,
        home: PathBuf,
        tracker: PhaseTracker,
        streaming: &'static str,
        invocation: Cell<usize>,
    }

    struct Run {
        report: Value,
        events: Vec<Value>,
    }

    impl Run {
        fn rebuild_entries(&self) -> usize {
            self.events
                .iter()
                .filter(|event| {
                    event["fields"]["message"] == "lexical rebuild prep profile"
                        && event["fields"]["component"] == "main"
                        && event["fields"]["step"] == "open_readonly"
                })
                .count()
        }

        fn assert_no_rebuild(&self) {
            assert_eq!(self.rebuild_entries(), 0, "{:?}", self.events);
            assert!(
                self.events.iter().all(|event| {
                    event["fields"]["message"]
                        != "lexical rebuild authoritative equivalence evidence"
                }),
                "{:?}",
                self.events
            );
        }

        fn assert_inline(&self) {
            assert_eq!(
                self.report["indexing_stats"]["lexical_strategy"], "incremental_inline",
                "{}",
                self.report
            );
            assert_eq!(
                self.report["indexing_stats"]["lexical_strategy_reason"],
                "incremental_scan_applies_inline_lexical_updates_only_for_new_messages",
                "{}",
                self.report
            );
            assert!(
                self.report["indexing_stats"]
                    .get("lexical_repair")
                    .is_none()
            );
            assert_eq!(self.report["lexical_update_deferred"], false);
            self.assert_no_rebuild();
        }

        fn assert_codex_observations(&self, skipped: usize, parsed: usize) {
            // Mutable path dependencies deliberately cannot certify parser
            // identity. Those builds must parse every source; pinned builds
            // must demonstrate the exact durable-observation reuse below.
            let (skipped, parsed) = if env!("CASS_SOURCE_INGEST_REUSE") == "true" {
                (skipped, parsed)
            } else {
                (0, skipped + parsed)
            };
            let observations = self
                .events
                .iter()
                .filter(|event| {
                    event["fields"]["message"] == "source_ingest_observation"
                        && event["fields"]["connector"] == "codex"
                })
                .map(|event| {
                    event["fields"]["skipped"]
                        .as_bool()
                        .expect("source observation includes an explicit skip decision")
                })
                .collect::<Vec<_>>();
            assert_eq!(
                observations.iter().filter(|&&value| value).count(),
                skipped,
                "{:?}",
                self.events
            );
            assert_eq!(
                observations.iter().filter(|&&value| !value).count(),
                parsed,
                "{:?}",
                self.events
            );
        }

        fn assert_totals(&self, conversations: usize, messages: usize) {
            assert_eq!(
                self.report["conversations"], conversations,
                "{}",
                self.report
            );
            assert_eq!(self.report["messages"], messages, "{}", self.report);
            assert_eq!(
                self.report["indexing_stats"]["total_conversations"], conversations,
                "{}",
                self.report
            );
            assert_eq!(
                self.report["indexing_stats"]["total_messages"], messages,
                "{}",
                self.report
            );
        }
    }

    impl Fixture {
        fn new(name: &str, streaming: &'static str) -> Self {
            let temp = tempfile::TempDir::new().expect("create isolated GH472 home");
            let home = fs::canonicalize(temp.path()).expect("canonical fixture home");
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(home.join(".env"))
                .expect("stop dotenv discovery before reaching an ancestor checkout");
            for directory in [
                ".codex",
                ".claude",
                ".config",
                ".local/share",
                "data",
                "aider",
                "traces",
            ] {
                fs::create_dir_all(home.join(directory)).expect("create fixture directory");
            }
            let fixture_name = home.file_name().unwrap().to_str().unwrap();
            let tracker = tracker_for(&format!(
                "gh472_{name}_streaming_{streaming}_{fixture_name}"
            ));
            Self {
                _temp: temp,
                home,
                tracker,
                streaming,
                invocation: Cell::new(0),
            }
        }

        fn data(&self) -> PathBuf {
            self.home.join("data")
        }

        fn index(&self) -> PathBuf {
            expected_index_dir(&self.data())
        }

        fn checkpoint_path(&self) -> PathBuf {
            self.index().join(".lexical-rebuild-state.json")
        }

        fn checkpoint(&self) -> Value {
            let path = self.checkpoint_path();
            let bytes = fs::read(&path).expect("read real lexical checkpoint");
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|error| panic!("invalid checkpoint {}: {error}", path.display()))
        }

        fn run(&self, args: &[&str], overrides: &[(&str, &str)]) -> Run {
            let number = self.invocation.get();
            self.invocation.set(number + 1);
            let trace = self.home.join("traces").join(format!("run-{number}.jsonl"));
            assert!(!trace.exists(), "each child must use a fresh trace");
            let environment = self
                .tracker
                .command_environment()
                .with_home(&self.home)
                .with_codex_home(self.home.join(".codex"));
            let mut command = environment.cass_assert_command();
            command.env_clear();
            for key in ["PATH", "SystemRoot", "WINDIR"] {
                if let Some(value) = std::env::var_os(key) {
                    command.env(key, value);
                }
            }
            environment.apply_to_assert(&mut command);
            command
                .current_dir(&self.home)
                .env("USERPROFILE", &self.home)
                .env("XDG_CONFIG_HOME", self.home.join(".config"))
                .env("XDG_DATA_HOME", self.home.join(".local/share"))
                .env("CLAUDE_CONFIG_DIR", self.home.join(".claude"))
                .env("CASS_AIDER_DATA_ROOT", self.home.join("aider"))
                .env("CASS_DATA_DIR", self.data())
                .env("CASS_IGNORE_SOURCES_CONFIG", "1")
                .env("CASS_STREAMING_INDEX", self.streaming)
                .env("CASS_AUTO_REFRESH", "0")
                .env("CASS_RESPONSIVENESS_DISABLE", "1")
                .env("CASS_PREP_PROFILE", "1")
                .env("CASS_INDEX_NO_PROGRESS_EVENTS", "1")
                .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
                .env("RUST_MIN_STACK", "134217728")
                .env("TUI_HEADLESS", "1")
                .env("NO_COLOR", "1")
                .env("CASS_TRACE_FILE", &trace)
                .env(
                    "CASS_TRACE_FILTER",
                    "warn,coding_agent_search::indexer=debug",
                )
                .env("CASS_TRACE_MAX_BYTES", "8388608")
                .env("CASS_TRACE_MAX_EVENTS", "65536")
                .args(["--color", "never"]);
            if args.first() == Some(&"schedule") {
                // The trace sink holds an exclusive file lock. Override only
                // the scheduler parent's CLI path; its one index child must
                // inherit CASS_TRACE_FILE and produce the asserted trace.
                let parent_trace = self
                    .home
                    .join("traces")
                    .join(format!("run-{number}-parent.jsonl"));
                assert!(!parent_trace.exists(), "fresh scheduler parent trace");
                command.arg("--trace-file").arg(parent_trace);
            }
            command.args(args).timeout(Duration::from_secs(180));
            for (key, value) in overrides {
                command.env(key, value);
            }
            let output = command.output().expect("run actual GH472 cass child");
            assert!(
                output.status.success(),
                "args={args:?} overrides={overrides:?}\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let report = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
                panic!(
                    "invalid JSON for {args:?}: {error}\nstdout:\n{}\nstderr:\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                )
            });
            let log = fs::read_to_string(&trace).unwrap_or_else(|error| {
                panic!(
                    "missing trace {}: {error}\nstderr:\n{}",
                    trace.display(),
                    String::from_utf8_lossy(&output.stderr)
                )
            });
            let events = log
                .lines()
                .map(|line| {
                    let event: Value = serde_json::from_str(line)
                        .unwrap_or_else(|error| panic!("invalid trace record {line:?}: {error}"));
                    assert_ne!(event["event"], "trace_truncated", "{log}");
                    assert_ne!(event["fields"]["event"], "trace_truncated", "{log}");
                    event
                })
                .collect::<Vec<_>>();
            assert!(!events.is_empty(), "trace must contain real child events");
            Run { report, events }
        }

        fn nightly(&self, overrides: &[(&str, &str)]) -> Run {
            let mut run = self.run(
                &[
                    "schedule",
                    "run",
                    "--job",
                    "nightly",
                    "--force",
                    "--no-semantic",
                    "--json",
                ],
                overrides,
            );
            assert_eq!(run.report["ok"], true, "{}", run.report);
            let index_steps = run.report["steps"]
                .as_array()
                .expect("schedule steps")
                .iter()
                .filter(|step| {
                    step["argv"]
                        .as_array()
                        .is_some_and(|argv| argv.iter().any(|arg| arg == "index"))
                })
                .collect::<Vec<_>>();
            assert_eq!(index_steps.len(), 1, "{}", run.report);
            let step = index_steps[0];
            assert_eq!(step["ok"], true, "{step}");
            assert_eq!(step["exit_code"], 0, "{step}");
            assert!(
                step["argv"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|arg| arg == "--full"),
                "nightly must retain full source census: {step}"
            );
            let result = step["result"].clone();
            assert_eq!(result["success"], true, "{result}");
            assert_eq!(result["full"], true, "{result}");
            run.report = result;
            run
        }

        fn seed(&self, day: &str, name: &str, needle: &str) -> PathBuf {
            let filename = format!("rollout-{name}.jsonl");
            make_codex_session(
                &self.home.join(".codex"),
                day,
                &filename,
                needle,
                1_732_118_400_000,
            );
            self.home.join(".codex/sessions").join(day).join(filename)
        }

        fn archive(&self) -> Archive {
            let storage = SqliteStorage::open_readonly(&self.data().join("agent_search.db"))
                .expect("read canonical archive without changing its checkpoint fingerprint");
            let mut archive = Archive::new();
            for conversation in storage.list_conversations(100, 0).unwrap() {
                let id = conversation.id.expect("canonical conversation ID");
                let messages = storage
                    .fetch_messages(id)
                    .unwrap()
                    .into_iter()
                    .map(|message| {
                        json!({
                            "id": message.id.expect("canonical message ID"),
                            "idx": message.idx,
                            "role": message.role,
                            "content": message.content
                        })
                    })
                    .collect::<Vec<_>>();
                assert!(
                    archive
                        .insert(
                            conversation.source_path,
                            json!({
                                "id": id,
                                "agent": conversation.agent_slug,
                                "external_id": conversation.external_id,
                                "title": conversation.title,
                                "workspace": conversation.workspace,
                                "source_id": conversation.source_id,
                                "origin_host": conversation.origin_host,
                                "messages": messages
                            })
                        )
                        .is_none(),
                    "fixture must not collapse distinct canonical source identities"
                );
            }
            archive
        }

        fn baseline(&self) -> Archive {
            let first = self.seed("2024/11/20", "first", "gh472firstanchorz");
            let second = self.seed("2024/11/21", "second", "gh472secondanchorz");
            let run = self.run(&["index", "--full", "--json"], &[]);
            assert!(
                run.rebuild_entries() > 0,
                "positive trace control: {:?}",
                run.events
            );
            run.assert_totals(2, 4);
            let archive = self.archive();
            assert_eq!(
                archive.keys().cloned().collect::<Vec<_>>(),
                vec![first.clone(), second.clone()],
                "only the two generated sources may be admitted"
            );
            assert_codex_pair(&archive, &first, "gh472firstanchorz");
            assert_codex_pair(&archive, &second, "gh472secondanchorz");
            let storage =
                SqliteStorage::open_readonly(&self.data().join("agent_search.db")).unwrap();
            assert_eq!(storage.source_ingest_ledger_entries().unwrap().len(), 2);
            drop(storage);
            assert_eq!(self.checkpoint()["completed"], true);
            assert_eq!(self.checkpoint()["indexed_docs"], 4);
            archive
        }

        fn receipts(&self) -> Vec<Vec<u8>> {
            [
                ".lexical-rebuild-equivalence.json",
                ".lexical-refresh-ledger.json",
            ]
            .map(|name| fs::read(self.index().join(name)).expect("authoritative publish receipt"))
            .into_iter()
            .collect()
        }

        fn assert_search(&self, needle: &str, archive: &Archive) {
            // These unique literal needles query both indexed title and content.
            // Match canonical rows independently of search order, scores, or previews.
            let mut expected = Vec::new();
            for (source, conversation) in archive {
                let title_matches = conversation["title"]
                    .as_str()
                    .is_some_and(|title| title.contains(needle));
                for message in conversation["messages"].as_array().unwrap() {
                    if title_matches || message["content"].as_str().unwrap().contains(needle) {
                        expected.push(
                            json!({
                                "source_path": source,
                                "line_number": message["idx"].as_i64().unwrap() + 1,
                                "title": conversation["title"].as_str().unwrap_or(""),
                                "content": message["content"],
                                "agent": conversation["agent"],
                                "workspace": conversation["workspace"].as_str().unwrap_or(""),
                                "source_id": conversation["source_id"],
                                "origin_kind": "local",
                                "origin_host": conversation["origin_host"]
                            })
                            .to_string(),
                        );
                    }
                }
            }
            let run = self.run(
                &[
                    "search",
                    needle,
                    "--json",
                    "--mode",
                    "lexical",
                    "--no-maintenance",
                    "--limit",
                    "100",
                ],
                &[],
            );
            assert_ne!(run.report.pointer("/budget/timed_out"), Some(&json!(true)));
            let mut actual = run.report["hits"]
                .as_array()
                .expect("actual lexical hits")
                .iter()
                .map(|hit| {
                    json!({
                        "source_path": hit["source_path"],
                        "line_number": hit["line_number"],
                        "title": hit["title"],
                        "content": hit["content"],
                        "agent": hit["agent"],
                        "workspace": hit["workspace"],
                        "source_id": hit["source_id"],
                        "origin_kind": hit["origin_kind"],
                        "origin_host": hit["origin_host"]
                    })
                    .to_string()
                })
                .collect::<Vec<_>>();
            expected.sort();
            actual.sort();
            assert_eq!(
                actual, expected,
                "missing/duplicate/misattributed hits: {}",
                run.report
            );
        }

        fn replace_with_sparse_donor(&self) {
            let donor = Self::new("sparse_donor", self.streaming);
            let source = donor.seed("2024/11/22", "donor", "gh472donoranchorz");
            append_jsonl_lines(
                &source,
                &[json!({
                    "timestamp": "2024-11-20T12:00:00Z",
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_text", "text": "gh472donoranchorz third message"}]
                    }
                })],
            );
            let run = donor.run(&["index", "--full", "--json"], &[]);
            run.assert_totals(1, 3);
            assert!(run.rebuild_entries() > 0);
            assert_eq!(
                raw_lexical_total_matches(&donor.index(), "gh472donoranchorz"),
                3
            );
            let backup = self.data().join("before-sparse-donor");
            fs::rename(self.index(), &backup).expect("retain original real generation");
            fs::rename(donor.index(), self.index()).expect("install real three-document donor");
            // This is the original archive's completed state, not the donor's
            // unrelated checkpoint. Thus the sparse planner is admitted with
            // expected=4/observed=3, above the separate 50% hollow threshold.
            fs::copy(
                backup.join(".lexical-rebuild-state.json"),
                self.checkpoint_path(),
            )
            .expect("restore matching original archive checkpoint");
            assert_eq!(self.checkpoint()["completed"], true);
            assert_eq!(self.checkpoint()["indexed_docs"], 4);
            assert_eq!(
                raw_lexical_total_matches(&self.index(), "gh472firstanchorz"),
                0
            );
            assert_eq!(
                raw_lexical_total_matches(&self.index(), "gh472donoranchorz"),
                3
            );
        }
    }

    fn assert_retained(before: &Archive, after: &Archive) {
        for (source, original) in before {
            assert_eq!(
                after.get(source),
                Some(original),
                "canonical identity/content changed: {source:?}"
            );
        }
    }

    fn assert_codex_pair(archive: &Archive, source: &Path, needle: &str) {
        let conversation = &archive[source];
        assert_eq!(conversation["agent"], "codex");
        assert_eq!(conversation["source_id"], "local");
        assert!(conversation["id"].as_i64().unwrap() > 0);
        let messages = conversation["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["idx"], 0);
        assert_eq!(messages[1]["idx"], 1);
        assert_eq!(messages[0]["content"], needle);
        assert_eq!(messages[1]["content"], format!("{needle}_response"));
        assert!(
            messages
                .iter()
                .all(|message| message["id"].as_i64().unwrap() > 0)
        );
        assert_ne!(messages[0]["id"], messages[1]["id"]);
    }

    #[test]
    fn nightly_healthy_reconciliation_preserves_ids_and_appends_inline() {
        for streaming in ["0", "1"] {
            let fixture = Fixture::new("healthy", streaming);
            let original = fixture.baseline();
            let receipts = fixture.receipts();
            let checkpoint = fs::read(fixture.checkpoint_path()).unwrap();
            let noop = fixture.nightly(&[]);
            noop.assert_inline();
            noop.assert_totals(2, 4);
            noop.assert_codex_observations(2, 0);
            assert_eq!(fixture.archive(), original);
            assert_eq!(fs::read(fixture.checkpoint_path()).unwrap(), checkpoint);

            let new_source = fixture.seed("2024/11/23", "third", "gh472thirdanchorz");
            let appended = fixture.nightly(&[]);
            appended.assert_inline();
            appended.assert_codex_observations(2, 1);
            let archive = fixture.archive();
            assert_retained(&original, &archive);
            assert_eq!(archive.len(), 3);
            assert_codex_pair(&archive, &new_source, "gh472thirdanchorz");
            assert_eq!(fixture.receipts(), receipts);
            for needle in [
                "gh472firstanchorz",
                "gh472secondanchorz",
                "gh472thirdanchorz",
            ] {
                fixture.assert_search(needle, &archive);
            }

            // Growing a known source must preserve its old row IDs as well.
            let first = original.keys().next().unwrap();
            append_codex_session(first, "gh472tailanchorz", 1_732_118_500_000);
            let grown = fixture.nightly(&[]);
            grown.assert_inline();
            grown.assert_codex_observations(2, 1);
            let after_growth = fixture.archive();
            let old = &archive[first];
            let new = &after_growth[first];
            assert_eq!(new["id"], old["id"]);
            assert_eq!(new["messages"].as_array().unwrap().len(), 4);
            assert_eq!(
                &new["messages"].as_array().unwrap()[..2],
                old["messages"].as_array().unwrap().as_slice()
            );
            assert_eq!(new["messages"][2]["content"], "gh472tailanchorz");
            assert_eq!(new["messages"][3]["content"], "gh472tailanchorz_response");
            for (source, row) in &archive {
                if source != first {
                    assert_eq!(after_growth.get(source), Some(row));
                }
            }
            fixture.assert_search("gh472tailanchorz", &after_growth);
            fixture.assert_search("gh472firstanchorz", &after_growth);
            let final_noop = fixture.nightly(&[]);
            final_noop.assert_inline();
            final_noop.assert_totals(3, 8);
            final_noop.assert_codex_observations(3, 0);
            assert_eq!(fixture.archive(), after_growth);
            assert_eq!(fixture.receipts(), receipts);
        }
    }

    #[test]
    fn nightly_sparse_reconciliation_recounts_after_skipped_scan() {
        for streaming in ["0", "1"] {
            for add_source in [false, true] {
                let fixture = Fixture::new(
                    if add_source {
                        "sparse_plus_new"
                    } else {
                        "sparse_no_new"
                    },
                    streaming,
                );
                let original = fixture.baseline();
                fixture.replace_with_sparse_donor();
                let new_source =
                    add_source.then(|| fixture.seed("2024/11/23", "third", "gh472thirdanchorz"));
                let repaired = fixture.nightly(&[]);
                assert_eq!(repaired.rebuild_entries(), 1, "{:?}", repaired.events);
                let repair = &repaired.report["indexing_stats"]["lexical_repair"];
                assert_eq!(
                    repair["kind"], "authoritative_canonical_db_rebuild",
                    "{repair}"
                );
                assert_eq!(
                    repair["reason"],
                    "incremental_index_repairs_sparse_tantivy_from_authoritative_canonical_db_before_scan",
                    "{repair}"
                );
                assert_eq!(repair["canonical_conversations"], 2);
                assert_eq!(repair["canonical_messages"], 4);
                assert_eq!(repair["observed_tantivy_docs"], 3);
                repaired.assert_codex_observations(2, usize::from(add_source));
                let conversations = 2 + usize::from(add_source);
                let messages = 4 + 2 * usize::from(add_source);
                repaired.assert_totals(conversations, messages);
                let archive = fixture.archive();
                assert_eq!(archive.len(), conversations);
                assert_retained(&original, &archive);
                if let Some(source) = &new_source {
                    assert_codex_pair(&archive, source, "gh472thirdanchorz");
                }
                let checkpoint = fixture.checkpoint();
                assert_eq!(checkpoint["completed"], true);
                assert_eq!(checkpoint["db"]["total_conversations"], conversations);
                assert_eq!(checkpoint["db"]["total_messages"], messages);
                assert_eq!(checkpoint["indexed_docs"], messages);
                assert_eq!(
                    raw_lexical_total_matches(&fixture.index(), "gh472donoranchorz"),
                    0
                );
                for needle in [
                    "gh472firstanchorz",
                    "gh472secondanchorz",
                    "gh472thirdanchorz",
                ] {
                    fixture.assert_search(needle, &archive);
                }
                let unchanged = fixture.nightly(&[]);
                unchanged.assert_inline();
                unchanged.assert_totals(conversations, messages);
                unchanged.assert_codex_observations(conversations, 0);
                assert_eq!(fixture.archive(), archive);
            }
        }
    }

    #[test]
    fn nightly_reconciliation_censuses_old_mtime_fallback_sources() {
        let fixture = Fixture::new("fallback_census", "1");
        let original = fixture.baseline();
        let initial = fixture.home.join("aider/initial/.aider.chat.history.md");
        fs::create_dir_all(initial.parent().unwrap()).unwrap();
        let history = include_str!("fixtures/aider/.aider.chat.history.md");
        fs::write(
            &initial,
            format!("{history}\n> gh472aiderinitialz\n\nRetain gh472aiderinitialz discussion.\n"),
        )
        .unwrap();
        fixture.run(&["index", "--json"], &[]);
        let with_aider = fixture.archive();
        assert_retained(&original, &with_aider);
        assert_eq!(with_aider.len(), 3);
        assert_eq!(with_aider[&initial]["agent"], "aider");
        fixture.assert_search("gh472aiderinitialz", &with_aider);
        let storage =
            SqliteStorage::open_readonly(&fixture.data().join("agent_search.db")).unwrap();
        assert!(
            storage
                .get_connector_last_scan_ts("aider")
                .unwrap()
                .is_some_and(|watermark| watermark > 101_000),
            "establish this fallback provider's own watermark before adding an old file"
        );
        drop(storage);

        let old_source = fixture.home.join("aider/late/.aider.chat.history.md");
        fs::create_dir_all(old_source.parent().unwrap()).unwrap();
        fs::write(
            &old_source,
            format!("{history}\n> gh472aideroldz\n\nRecover gh472aideroldz discussion.\n"),
        )
        .unwrap();
        let old_time = std::time::UNIX_EPOCH + Duration::from_secs(100);
        fs::File::open(&old_source)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(old_time))
            .unwrap();
        assert_eq!(
            fs::metadata(&old_source).unwrap().modified().unwrap(),
            old_time
        );

        let incremental = fixture.run(&["index", "--json"], &[]);
        incremental.assert_codex_observations(2, 0);
        assert_eq!(fixture.archive(), with_aider);
        assert_eq!(
            raw_lexical_total_matches(&fixture.index(), "gh472aideroldz"),
            0,
            "ordinary timestamp-filtered discovery must demonstrate the negative branch"
        );
        let nightly = fixture.nightly(&[]);
        nightly.assert_inline();
        nightly.assert_codex_observations(2, 0);
        let archive = fixture.archive();
        assert_retained(&with_aider, &archive);
        assert_eq!(archive.len(), 4);
        assert_eq!(archive[&old_source]["agent"], "aider");
        assert!(
            archive[&old_source]["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| message["content"]
                    .as_str()
                    .unwrap()
                    .contains("gh472aideroldz"))
        );
        fixture.assert_search("gh472aideroldz", &archive);
        fixture.assert_search("gh472firstanchorz", &archive);
        assert_eq!(
            fs::metadata(&old_source).unwrap().modified().unwrap(),
            old_time
        );
        let noop = fixture.nightly(&[]);
        noop.assert_inline();
        assert_eq!(fixture.archive(), archive);
        fixture.assert_search("gh472aideroldz", &archive);
    }

    #[test]
    fn nightly_reconciliation_uncertain_admission_keeps_full_repair() {
        for case in [
            "deferred",
            "count_skipped",
            "checkpoint_missing",
            "checkpoint_malformed",
            "checkpoint_foreign",
        ] {
            let fixture = Fixture::new(case, "1");
            let original = fixture.baseline();
            let checkpoint = fixture.checkpoint_path();
            let mut overrides = Vec::new();
            match case {
                "deferred" => overrides.push(("CASS_DEFER_LEXICAL_UPDATES", "1")),
                "count_skipped" => {
                    fixture.replace_with_sparse_donor();
                    overrides.push(("CASS_SKIP_PREFLIGHT_COUNT_TOTAL_MESSAGES", "1"));
                }
                "checkpoint_missing" => {
                    fs::rename(&checkpoint, fixture.data().join("retained-checkpoint.json"))
                        .unwrap();
                }
                "checkpoint_malformed" => {
                    fs::rename(&checkpoint, fixture.data().join("retained-checkpoint.json"))
                        .unwrap();
                    fs::write(&checkpoint, b"{incomplete checkpoint").unwrap();
                }
                "checkpoint_foreign" => {
                    let mut state = fixture.checkpoint();
                    state["db"]["db_path"] = json!(fixture.home.join("foreign/agent_search.db"));
                    fs::rename(&checkpoint, fixture.data().join("retained-checkpoint.json"))
                        .unwrap();
                    fs::write(&checkpoint, serde_json::to_vec(&state).unwrap()).unwrap();
                }
                _ => unreachable!("closed table of admission controls"),
            }
            // Keep the count-skip case at 3/4 with no new rows, so the
            // test cannot pass merely because an unrelated append repaired it.
            let new_source = (case != "count_skipped")
                .then(|| fixture.seed("2024/11/23", "third", "gh472thirdanchorz"));
            let repaired = fixture.nightly(&overrides);
            assert!(
                repaired.rebuild_entries() > 0,
                "uncertain admission must retain authoritative repair: case={case}, {:?}",
                repaired.events
            );
            assert_eq!(
                repaired.report["indexing_stats"]["lexical_strategy"],
                "deferred_authoritative_db_rebuild",
                "case={case}, {}",
                repaired.report
            );
            assert_eq!(
                repaired.report["indexing_stats"]["lexical_strategy_reason"],
                "full_refresh_defers_inline_lexical_writes_to_authoritative_db_rebuild",
                "case={case}, {}",
                repaired.report
            );
            let conversations = if case == "count_skipped" { 2 } else { 3 };
            repaired.assert_totals(conversations, conversations * 2);
            let archive = fixture.archive();
            assert_eq!(archive.len(), conversations, "case={case}");
            assert_retained(&original, &archive);
            if let Some(source) = &new_source {
                assert_codex_pair(&archive, source, "gh472thirdanchorz");
            }
            assert_eq!(fixture.checkpoint()["completed"], true);
            assert_eq!(fixture.checkpoint()["indexed_docs"], conversations * 2);
            assert_eq!(
                raw_lexical_total_matches(&fixture.index(), "gh472donoranchorz"),
                0
            );
            for needle in [
                "gh472firstanchorz",
                "gh472secondanchorz",
                "gh472thirdanchorz",
            ] {
                fixture.assert_search(needle, &archive);
            }
            // Removing the child-only override permits healthy reconciliation
            // after repair; no inherited env or persistent config is changed.
            let healthy = fixture.nightly(&[]);
            healthy.assert_inline();
            assert_eq!(fixture.archive(), archive);
        }
    }

    #[test]
    fn operator_full_and_force_ignore_nightly_optimization() {
        let fixture = Fixture::new("operator_controls", "1");
        let original = fixture.baseline();
        let staged = fixture.seed("2024/11/23", "third", "gh472thirdanchorz");
        let force = fixture.run(
            &["index", "--full", "--force-rebuild", "--json"],
            &[("CASS_NIGHTLY_RECONCILIATION", "1")],
        );
        assert_eq!(force.report["force_rebuild"], true);
        assert_eq!(force.report["full"], true);
        assert!(force.rebuild_entries() > 0, "{:?}", force.events);
        force.assert_totals(2, 4);
        force.assert_codex_observations(0, 0);
        assert_eq!(fixture.archive(), original);
        assert!(!fixture.archive().contains_key(&staged));
        assert_eq!(
            raw_lexical_total_matches(&fixture.index(), "gh472thirdanchorz"),
            0,
            "force-rebuild intentionally rebuilds the canonical archive without rescanning raw logs"
        );
        fixture.assert_search("gh472firstanchorz", &original);

        // Fixture::run clears the environment, so this operator invocation
        // has no nightly marker. A real append defeats the legitimate noop
        // optimization already present in ordinary unchanged --full runs.
        let full = fixture.run(&["index", "--full", "--json"], &[]);
        assert_eq!(full.report["force_rebuild"], false);
        assert!(full.rebuild_entries() > 0, "{:?}", full.events);
        full.assert_totals(3, 6);
        let archive = fixture.archive();
        assert_retained(&original, &archive);
        assert_eq!(archive.len(), 3);
        assert_codex_pair(&archive, &staged, "gh472thirdanchorz");
        fixture.assert_search("gh472thirdanchorz", &archive);
        fixture.assert_search("gh472firstanchorz", &archive);
    }

    #[test]
    fn nightly_truncated_and_disappeared_sources_preserve_canonical_history() {
        for streaming in ["0", "1"] {
            let fixture = Fixture::new("retained_history", streaming);
            let original = fixture.baseline();
            let source = original.keys().next().unwrap();
            append_codex_session(source, "gh472retainedtailz", 1_732_118_500_000);
            fixture.nightly(&[]).assert_inline();
            let grown = fixture.archive();
            assert_eq!(grown[source]["id"], original[source]["id"]);
            assert_eq!(grown[source]["messages"].as_array().unwrap().len(), 4);
            assert_eq!(
                grown[source]["messages"][2]["content"],
                "gh472retainedtailz"
            );
            assert_eq!(
                grown[source]["messages"][3]["content"],
                "gh472retainedtailz_response"
            );
            let receipts = fixture.receipts();
            // Retain the original raw bytes outside discovery; truncating an
            // append-oriented log must not delete already-canonical history.
            let raw = fs::read_to_string(source).unwrap();
            fs::write(fixture.home.join("retained-original-source.txt"), &raw).unwrap();
            let short = raw.lines().take(2).collect::<Vec<_>>().join("\n") + "\n";
            fs::write(source, short).unwrap();
            fs::File::open(source)
                .unwrap()
                .set_times(
                    fs::FileTimes::new()
                        .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(100)),
                )
                .unwrap();
            let truncated = fixture.nightly(&[]);
            truncated.assert_inline();
            truncated.assert_codex_observations(1, 1);
            truncated.assert_totals(2, 6);
            assert_eq!(fixture.archive(), grown);
            fixture.assert_search("gh472retainedtailz", &grown);

            // Disappearance is archival retention, not a request to erase
            // canonical messages. Rename rather than delete the fixture.
            fs::rename(source, fixture.home.join("disappeared-source.jsonl")).unwrap();
            let absent = fixture.nightly(&[]);
            absent.assert_inline();
            absent.assert_codex_observations(1, 0);
            absent.assert_totals(2, 6);
            assert_eq!(fixture.archive(), grown);
            assert_eq!(fixture.receipts(), receipts);
            fixture.assert_search("gh472retainedtailz", &grown);
            fixture.assert_search("gh472firstanchorz", &grown);
        }
    }
}

fn tracker_for(test_name: &str) -> PhaseTracker {
    PhaseTracker::new("e2e_search_index", test_name)
}

fn codex_iso_timestamp(ts_millis: u64) -> String {
    chrono::DateTime::<Utc>::from_timestamp_millis(ts_millis as i64)
        .expect("valid millis timestamp for codex fixture")
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn write_jsonl_lines(file: &Path, lines: &[serde_json::Value]) {
    let mut sample = String::new();
    for line in lines {
        sample.push_str(&serde_json::to_string(line).unwrap());
        sample.push('\n');
    }
    fs::write(file, sample).unwrap();
}

fn append_jsonl_lines(file: &Path, lines: &[serde_json::Value]) {
    use std::io::Write;

    let mut handle = std::fs::OpenOptions::new()
        .append(true)
        .open(file)
        .expect("open rollout for append");
    for line in lines {
        handle
            .write_all(serde_json::to_string(line).unwrap().as_bytes())
            .unwrap();
        handle.write_all(b"\n").unwrap();
    }
}

/// Helper to create Codex session with modern envelope format.
fn make_codex_session(root: &Path, date_path: &str, filename: &str, content: &str, ts: u64) {
    let sessions = root.join(format!("sessions/{date_path}"));
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join(filename);
    let workspace = root.to_string_lossy();
    write_jsonl_lines(
        &file,
        &[
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
        ],
    );
}

/// Helper to create Claude Code session.
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

/// Append an additional Codex message pair (user + assistant) to an existing rollout file.
fn append_codex_session(file: &Path, content: &str, ts: u64) {
    append_jsonl_lines(
        file,
        &[
            serde_json::json!({
                "timestamp": codex_iso_timestamp(ts),
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": content }]
                }
            }),
            serde_json::json!({
                "timestamp": codex_iso_timestamp(ts + 1_000),
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "text", "text": format!("{content}_response") }]
                }
            }),
        ],
    );
}

fn make_codex_session_with_turns(
    root: &Path,
    date_path: &str,
    filename: &str,
    common_token: &str,
    unique_suffix: &str,
    ts: u64,
    turns: usize,
) {
    let sessions = root.join(format!("sessions/{date_path}"));
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join(filename);
    let workspace = root.to_string_lossy();
    let mut lines = vec![serde_json::json!({
        "timestamp": codex_iso_timestamp(ts),
        "type": "session_meta",
        "payload": {
            "id": filename,
            "cwd": workspace,
            "cli_version": "0.42.0"
        }
    })];

    for turn in 0..turns {
        let turn_ts = ts + ((turn as u64) + 1) * 3_000;
        lines.push(serde_json::json!({
            "timestamp": codex_iso_timestamp(turn_ts),
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{
                    "type": "input_text",
                    "text": format!("{common_token} {unique_suffix} user_turn_{turn}")
                }]
            }
        }));
        lines.push(serde_json::json!({
            "timestamp": codex_iso_timestamp(turn_ts + 1_000),
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "content": [{
                    "type": "text",
                    "text": format!("{common_token} {unique_suffix} assistant_turn_{turn}")
                }]
            }
        }));
    }

    write_jsonl_lines(&file, &lines);
}

fn make_bulk_codex_sessions(
    root: &Path,
    date_path: &str,
    batch_prefix: &str,
    common_token: &str,
    start_ts: u64,
    session_count: usize,
    turns_per_session: usize,
) {
    for idx in 0..session_count {
        make_codex_session_with_turns(
            root,
            date_path,
            &format!("{batch_prefix}-{idx:03}.jsonl"),
            common_token,
            &format!("session_{idx:03}"),
            start_ts + (idx as u64) * 50_000,
            turns_per_session,
        );
    }
}

fn count_messages(db_path: &Path) -> i64 {
    let storage = SqliteStorage::open(db_path).expect("open sqlite");
    storage
        .raw()
        .query_row_map("SELECT COUNT(*) FROM messages", &[], |r| r.get_typed(0))
        .expect("count messages")
}

fn total_matches_from_search_output(output: &[u8]) -> u64 {
    let json: serde_json::Value = serde_json::from_slice(output).expect("parse search json");
    json.get("total_matches")
        .and_then(|matches| matches.as_u64())
        .unwrap_or_else(|| {
            json.get("hits")
                .and_then(|hits| hits.as_array())
                .map(|hits| hits.len() as u64)
                .unwrap_or(0)
        })
}

fn command_output_kind_is(output: &[u8], expected_kind: &str) -> bool {
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(output) else {
        return false;
    };
    json.get("kind")
        .and_then(|kind| kind.as_str())
        .is_some_and(|actual_kind| actual_kind.eq(expected_kind))
}

fn raw_lexical_total_matches(index_path: &Path, query: &str) -> u64 {
    let mut total = 0usize;
    if let Some(readers) = open_federated_search_readers(index_path)
        .expect("open federated lexical readers for raw count")
    {
        for (reader, fields) in readers {
            total = total.saturating_add(raw_lexical_reader_matches(&reader, &fields, query));
        }
    } else {
        let reader = coding_agent_search::search::quill_bridge::open_cass_reader(index_path)
            .expect("open lexical reader");
        let fields = coding_agent_search::search::quill_bridge::QuillCassFields::compiled();
        total = total.saturating_add(raw_lexical_reader_matches(&reader, &fields, query));
    }
    total as u64
}

/// Independent match count, built from the engine's own parser rather than
/// from the code under test.
fn raw_lexical_reader_matches(
    reader: &frankensearch::quill::QuillSearchIndex,
    _fields: &Fields,
    query: &str,
) -> usize {
    let parser = frankensearch::quill::query::CassQueryParser::new(
        frankensearch::quill::schema::CASS_SEMANTIC_SCHEMA,
    )
    .expect("build the CASS query parser");
    let parsed = parser.parse(
        query,
        &frankensearch::quill::query::CassQueryFilters::default(),
    );
    // The exact total is computed independently of the requested page, so ask
    // for the smallest page: a huge limit only sizes the top-k heap.
    coding_agent_search::search::quill_bridge::search_paginated(reader, &parsed.query, 1, 0, true)
        .expect("count raw lexical matches")
        .total_count
        .unwrap_or(0)
}

fn force_federated_publish_env(cmd: &mut assert_cmd::Command) {
    cmd.env("CASS_TANTIVY_REBUILD_WORKERS", "6");
    cmd.env("CASS_TANTIVY_MAX_WRITER_THREADS", "2");
    cmd.env("CASS_TANTIVY_REBUILD_BATCH_FETCH_CONVERSATIONS", "1");
    cmd.env(
        "CASS_TANTIVY_REBUILD_INITIAL_BATCH_FETCH_CONVERSATIONS",
        "1",
    );
    cmd.env("CASS_TANTIVY_REBUILD_COMMIT_EVERY_CONVERSATIONS", "1");
    cmd.env(
        "CASS_TANTIVY_REBUILD_INITIAL_COMMIT_EVERY_CONVERSATIONS",
        "1",
    );
    cmd.env("CASS_TANTIVY_REBUILD_COMMIT_EVERY_MESSAGES", "2");
    cmd.env("CASS_TANTIVY_REBUILD_INITIAL_COMMIT_EVERY_MESSAGES", "2");
    cmd.env("CASS_TANTIVY_REBUILD_COMMIT_EVERY_MESSAGE_BYTES", "4096");
    cmd.env(
        "CASS_TANTIVY_REBUILD_INITIAL_COMMIT_EVERY_MESSAGE_BYTES",
        "4096",
    );
}

#[cfg(target_os = "linux")]
fn cass_std_cmd(command_env: &E2eCommandEnvironment, home: &Path) -> StdCommand {
    let mut cmd = command_env.cass_std_command();
    cmd.current_dir(home);
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");
    cmd.env("CASS_IGNORE_SOURCES_CONFIG", "1");
    cmd
}

#[cfg(target_os = "linux")]
fn lexical_publish_in_progress_backup_path(index_path: &Path) -> std::path::PathBuf {
    let file_name = index_path
        .file_name()
        .expect("live index path should have a file name")
        .to_string_lossy();
    index_path.with_file_name(format!(".{file_name}.publish-in-progress.bak"))
}

#[cfg(target_os = "linux")]
fn lexical_publish_backups_dir(index_path: &Path) -> std::path::PathBuf {
    index_path
        .parent()
        .expect("live index path should have a parent directory")
        .join(".lexical-publish-backups")
}

#[cfg(target_os = "linux")]
fn wait_for_publish_kill_relaunch_sentinel(path: &Path, timeout: Duration) -> serde_json::Value {
    let deadline = Instant::now() + timeout;
    loop {
        match fs::read(path) {
            Ok(bytes) => {
                return serde_json::from_slice(&bytes)
                    .expect("parse lexical publish kill-relaunch sentinel json");
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(err) => {
                panic!(
                    "expected lexical publish kill-relaunch sentinel at {}: {err}",
                    path.display()
                );
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for lexical publish kill-relaunch sentinel at {}",
            path.display()
        );
    }
}

#[derive(Debug, Default)]
struct SearchLoopStats {
    attempts: usize,
    successes: usize,
    max_duration_ms: u64,
    failures: Vec<String>,
}

#[test]
#[serial]
fn duplicate_fts_schema_rows_do_not_block_cli_reads_and_writes() {
    let sqlite3_available = std::process::Command::new("sqlite3")
        .arg("-version")
        .output()
        .is_ok_and(|output| output.status.success());
    if !sqlite3_available {
        eprintln!(
            "skipping duplicate_fts_schema_rows_do_not_block_cli_reads_and_writes: \
             duplicate sqlite_master repair requires the sqlite3 CLI"
        );
        return;
    }
    let tracker = tracker_for("duplicate_fts_schema_rows_do_not_block_cli_reads_and_writes");
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.to_path_buf();
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    let command_env = tracker
        .command_environment()
        .with_home(home)
        .with_codex_home(&codex_home);

    let ts = 1_732_118_400_000u64;
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-fts-repair.jsonl",
        "fts_repair_initial_token",
        ts,
    );
    let session_file = codex_home.join("sessions/2024/11/20/rollout-fts-repair.jsonl");

    command_env
        .cass_assert_command()
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .current_dir(home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();

    let db_path = data_dir.join("agent_search.db");
    let baseline_messages = count_messages(&db_path);
    assert_eq!(
        baseline_messages, 2,
        "initial full index should ingest both messages"
    );

    let duplicate_legacy_fts_sql = "CREATE VIRTUAL TABLE fts_messages USING fts5(content, title, agent, workspace, source_path, created_at UNINDEXED, message_id UNINDEXED, tokenize='porter')";
    let injection =
        RusqliteConnection::open(&db_path).expect("open db for writable_schema fixture");
    injection
        .execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS fts_messages USING fts5(
                content,
                title,
                agent,
                workspace,
                source_path,
                created_at UNINDEXED,
                message_id UNINDEXED,
                tokenize='porter'
             );",
        )
        .expect("materialize canonical fts schema before duplicate injection");
    injection
        .execute_batch("PRAGMA writable_schema = ON;")
        .expect("enable writable_schema");
    injection
        .execute(
            "INSERT INTO sqlite_master(type, name, tbl_name, rootpage, sql)
             VALUES('table', 'fts_messages', 'fts_messages', 0, ?1)",
            [duplicate_legacy_fts_sql],
        )
        .expect("inject duplicate fts schema row");
    injection
        .execute(
            "DELETE FROM meta WHERE key = ?1",
            ["fts_frankensqlite_rebuild_generation"],
        )
        .expect("delete stale fts generation marker");
    injection
        .execute_batch("PRAGMA writable_schema = OFF;")
        .expect("disable writable_schema");
    drop(injection);

    let broken_read = RusqliteConnection::open(&db_path)
        .expect("reopen db for broken-read assertion")
        .query_row("SELECT COUNT(*) FROM fts_messages", [], |row| {
            row.get::<_, i64>(0)
        });
    assert!(
        broken_read.is_err(),
        "the injected duplicate schema row should reproduce the unreadable pre-fix SQLite state"
    );

    let existing_search = command_env
        .cass_assert_command()
        .args([
            "search",
            "fts_repair_initial_token",
            "--robot",
            "--data-dir",
        ])
        .arg(&data_dir)
        .current_dir(home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .output()
        .expect("search for existing content after duplicate schema injection");
    assert!(
        existing_search.status.success(),
        "search should continue to succeed even when the fallback SQLite FTS table is malformed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&existing_search.stdout),
        String::from_utf8_lossy(&existing_search.stderr)
    );
    let existing_hits = serde_json::from_slice::<serde_json::Value>(&existing_search.stdout)
        .expect("parse existing search json")
        .get("hits")
        .and_then(|hits| hits.as_array())
        .map(|hits| hits.len())
        .unwrap_or(0);
    assert!(
        existing_hits >= 1,
        "the Tantivy index should remain authoritative for search results"
    );

    let incremental_index = command_env
        .cass_assert_command()
        .args(["index", "--data-dir"])
        .arg(&data_dir)
        .current_dir(home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .output()
        .expect("run index after duplicate schema injection");
    assert!(
        incremental_index.status.success(),
        "incremental index should succeed even when the fallback SQLite FTS table is malformed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&incremental_index.stdout),
        String::from_utf8_lossy(&incremental_index.stderr)
    );

    let health = command_env
        .cass_assert_command()
        .args(["health", "--json", "--data-dir"])
        .arg(&data_dir)
        .current_dir(home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .output()
        .expect("run health after duplicate schema repair");
    assert!(
        health.status.success(),
        "health should report the repaired database as healthy\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&health.stdout),
        String::from_utf8_lossy(&health.stderr)
    );
    let health_json: serde_json::Value =
        serde_json::from_slice(&health.stdout).expect("parse health json");
    assert_eq!(
        health_json["healthy"],
        serde_json::Value::Bool(true),
        "health should treat the canonical archive plus Tantivy index as healthy"
    );

    std::thread::sleep(std::time::Duration::from_millis(1200));
    append_codex_session(&session_file, "fts_repair_appended_token", ts + 10_000);
    std::thread::sleep(std::time::Duration::from_millis(50));

    command_env
        .cass_assert_command()
        .args(["index", "--data-dir"])
        .arg(&data_dir)
        .current_dir(home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();

    let after_messages = count_messages(&db_path);
    assert_eq!(
        after_messages,
        baseline_messages + 2,
        "incremental writes should resume after repair and append the new turn"
    );

    let appended = command_env
        .cass_assert_command()
        .args([
            "search",
            "fts_repair_appended_token",
            "--robot",
            "--data-dir",
        ])
        .arg(&data_dir)
        .current_dir(home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .output()
        .expect("search for appended content after repair");
    assert!(
        appended.status.success(),
        "search should succeed after incremental write even with malformed fallback FTS\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&appended.stdout),
        String::from_utf8_lossy(&appended.stderr)
    );
    let appended_hits = serde_json::from_slice::<serde_json::Value>(&appended.stdout)
        .expect("parse appended search json")
        .get("hits")
        .and_then(|hits| hits.as_array())
        .map(|hits| hits.len())
        .unwrap_or(0);
    assert!(
        appended_hits >= 1,
        "the post-index incremental content should be searchable"
    );

    tracker.flush();
}

#[test]
#[serial]
fn concurrent_search_processes_do_not_block_incremental_index_json() {
    let tracker = tracker_for("concurrent_search_processes_do_not_block_incremental_index_json");
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path().to_path_buf();
    let codex_home = home.to_path_buf();
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    let command_env = tracker
        .command_environment()
        .with_home(&home)
        .with_codex_home(&codex_home);

    tracker.phase(
        "seed_initial_fixture",
        "Create baseline session and full index",
        || {
            make_codex_session(
                &codex_home,
                "2024/11/20",
                "rollout-baseline-lock-search.jsonl",
                "baselinelockanchor",
                1_732_118_400_000,
            );

            command_env
                .cass_assert_command()
                .args(["index", "--full", "--json", "--data-dir"])
                .arg(&data_dir)
                .current_dir(&home)
                .env("CODEX_HOME", &codex_home)
                .env("HOME", &home)
                .timeout(Duration::from_secs(30))
                .assert()
                .success();
        },
    );

    tracker.phase(
        "verify_baseline_search_fixture",
        "Confirm the baseline lexical query is searchable before starting concurrent readers",
        || {
            let mut baseline_command = command_env.cass_assert_command();
            baseline_command
                .args([
                    "search",
                    "baselinelockanchor",
                    "--json",
                    "--mode",
                    "lexical",
                    "--fields",
                    "minimal",
                    "--limit",
                    "5",
                    "--data-dir",
                ])
                .arg(&data_dir)
                .current_dir(&home)
                .env("CODEX_HOME", &codex_home)
                .env("HOME", &home)
                .timeout(Duration::from_secs(20));
            let baseline_output = baseline_command
                .output()
                .expect("baseline lexical search should run");
            assert!(
                baseline_output.status.success(),
                "baseline lexical search should succeed before concurrency begins\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&baseline_output.stdout),
                String::from_utf8_lossy(&baseline_output.stderr)
            );
            let baseline_json: serde_json::Value =
                serde_json::from_slice(&baseline_output.stdout).expect("parse baseline search JSON");
            let baseline_hits = baseline_json
                .get("total_matches")
                .and_then(|matches| matches.as_u64())
                .unwrap_or_else(|| {
                    baseline_json
                        .get("hits")
                        .and_then(|hits| hits.as_array())
                        .map(|hits| hits.len() as u64)
                        .unwrap_or(0)
                });
            assert!(
                baseline_hits > 0,
                "baseline lexical search fixture must be searchable before starting concurrent readers"
            );
        },
    );

    let stop_search = Arc::new(AtomicBool::new(false));
    let index_running = Arc::new(AtomicBool::new(false));
    let search_attempts_during_index = Arc::new(AtomicUsize::new(0));
    let (ready_tx, ready_rx) = mpsc::channel();

    let search_home = home.clone();
    let search_codex_home = codex_home.clone();
    let search_data_dir = data_dir.clone();
    let stop_search_worker = Arc::clone(&stop_search);
    let index_running_worker = Arc::clone(&index_running);
    let search_attempts_during_index_worker = Arc::clone(&search_attempts_during_index);
    let search_command_env = command_env.clone();

    let search_handle = std::thread::spawn(move || {
        let mut stats = SearchLoopStats::default();
        let mut ready_sent = false;

        loop {
            if stop_search_worker.load(Ordering::Relaxed) {
                break;
            }

            if index_running_worker.load(Ordering::Relaxed) {
                search_attempts_during_index_worker.fetch_add(1, Ordering::Relaxed);
            }

            let search_start = Instant::now();
            let mut search_command = search_command_env.cass_assert_command();
            search_command
                .args([
                    "search",
                    "baselinelockanchor",
                    "--json",
                    "--mode",
                    "lexical",
                    "--fields",
                    "minimal",
                    "--limit",
                    "5",
                    "--data-dir",
                ])
                .arg(&search_data_dir)
                .current_dir(&search_home)
                .env("CODEX_HOME", &search_codex_home)
                .env("HOME", &search_home)
                .timeout(Duration::from_secs(20));
            let output = search_command
                .output()
                .expect("spawn concurrent cass search");
            let elapsed_ms = search_start.elapsed().as_millis() as u64;
            stats.attempts += 1;
            stats.max_duration_ms = stats.max_duration_ms.max(elapsed_ms);

            if output.status.success() {
                let parsed: serde_json::Value =
                    serde_json::from_slice(&output.stdout).expect("parse concurrent search JSON");
                let hit_count = parsed
                    .get("total_matches")
                    .and_then(|matches| matches.as_u64())
                    .unwrap_or_else(|| {
                        parsed
                            .get("hits")
                            .and_then(|hits| hits.as_array())
                            .map(|hits| hits.len() as u64)
                            .unwrap_or(0)
                    });
                if hit_count == 0 {
                    stats.failures.push(format!(
                        "concurrent search returned zero hits; stdout={}",
                        String::from_utf8_lossy(&output.stdout)
                    ));
                } else {
                    stats.successes += 1;
                }
            } else {
                stats.failures.push(format!(
                    "concurrent search failed: status={:?}\nstdout:\n{}\nstderr:\n{}",
                    output.status.code(),
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                ));
            }

            if !ready_sent {
                ready_tx.send(()).ok();
                ready_sent = true;
            }

            std::thread::sleep(Duration::from_millis(40));
        }

        stats
    });

    ready_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("concurrent search should start promptly");

    tracker.phase(
        "stage_incremental_fixture",
        "Create a substantial incremental batch while searches continue",
        || {
            make_bulk_codex_sessions(
                &codex_home,
                "2024/11/21",
                "rollout-incremental-lock-batch",
                "incrementalloadanchor",
                1_732_200_000_000,
                40,
                6,
            );
        },
    );

    let index_start = tracker.start(
        "incremental_index_under_read_load",
        Some("Run cass index --json while concurrent cass search processes read the same DB"),
    );
    index_running.store(true, Ordering::Relaxed);
    let index_output = command_env
        .cass_assert_command()
        .args(["index", "--json", "--data-dir"])
        .arg(&data_dir)
        .current_dir(&home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", &home)
        .timeout(Duration::from_secs(60))
        .output()
        .expect("run incremental index under concurrent search load");
    index_running.store(false, Ordering::Relaxed);
    let index_duration_ms = index_start.elapsed().as_millis() as u64;
    tracker.end(
        "incremental_index_under_read_load",
        Some("Run cass index --json while concurrent cass search processes read the same DB"),
        index_start,
    );

    stop_search.store(true, Ordering::Relaxed);
    let search_stats = search_handle.join().expect("join concurrent search thread");

    assert!(
        index_output.status.success(),
        "incremental index should succeed under concurrent search load\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&index_output.stdout),
        String::from_utf8_lossy(&index_output.stderr)
    );
    let index_json: serde_json::Value =
        serde_json::from_slice(&index_output.stdout).expect("parse index json");
    assert_eq!(
        index_json.get("success").and_then(|v| v.as_bool()),
        Some(true),
        "index --json should report success"
    );

    assert!(
        search_stats.failures.is_empty(),
        "concurrent searches should not fail while index runs:\n{}",
        search_stats.failures.join("\n---\n")
    );
    assert!(
        search_stats.successes > 0,
        "expected at least one successful concurrent search attempt"
    );
    assert!(
        search_attempts_during_index.load(Ordering::Relaxed) > 0,
        "expected real search overlap while index was running"
    );

    let after_messages = count_messages(&data_dir.join("agent_search.db")) as u64;
    let expected_min_messages = 2 + (40_u64 * 6 * 2);
    assert!(
        after_messages >= expected_min_messages,
        "incremental index should ingest the staged batch: expected at least {expected_min_messages} messages, got {after_messages}"
    );

    let mut verify_command = command_env.cass_assert_command();
    verify_command
        .args([
            "search",
            "incrementalloadanchor",
            "--json",
            "--mode",
            "lexical",
            "--fields",
            "minimal",
            "--limit",
            "10",
            "--data-dir",
        ])
        .arg(&data_dir)
        .current_dir(&home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", &home)
        .timeout(Duration::from_secs(20));
    let verify_search = verify_command
        .output()
        .expect("search for newly indexed batch");
    assert!(
        verify_search.status.success(),
        "search for newly indexed batch should succeed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&verify_search.stdout),
        String::from_utf8_lossy(&verify_search.stderr)
    );
    let verify_json: serde_json::Value =
        serde_json::from_slice(&verify_search.stdout).expect("parse verification search JSON");
    let verification_hits = verify_json
        .get("total_matches")
        .and_then(|matches| matches.as_u64())
        .unwrap_or_else(|| {
            verify_json
                .get("hits")
                .and_then(|hits| hits.as_array())
                .map(|hits| hits.len() as u64)
                .unwrap_or(0)
        });
    assert!(
        verification_hits > 0,
        "newly indexed batch should be searchable after concurrent index run"
    );

    tracker.metrics(
        "concurrent_search_vs_index",
        &E2ePerformanceMetrics::new()
            .with_duration(index_duration_ms)
            .with_custom("search_attempts", search_stats.attempts as u64)
            .with_custom("search_successes", search_stats.successes as u64)
            .with_custom(
                "search_attempts_during_index",
                search_attempts_during_index.load(Ordering::Relaxed) as u64,
            )
            .with_custom("max_search_duration_ms", search_stats.max_duration_ms)
            .with_custom("messages_after_index", after_messages),
    );
    tracker.complete();
}

#[test]
#[serial]
fn force_rebuild_preserves_search_results_and_reader_surface_during_atomic_publish() {
    let tracker = tracker_for(
        "force_rebuild_preserves_search_results_and_reader_surface_during_atomic_publish",
    );
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path().to_path_buf();
    let codex_home = home.to_path_buf();
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    let command_env = tracker
        .command_environment()
        .with_home(&home)
        .with_codex_home(&codex_home);

    const QUERY: &str = "atomicswapsearchanchor";

    tracker.phase(
        "seed_and_index_single_shard_fixture",
        "Create a minimal fixture and build the baseline lexical index",
        || {
            make_codex_session(
                &codex_home,
                "2024/11/22",
                "rollout-atomic-search-consistency.jsonl",
                QUERY,
                1_732_300_000_000,
            );

            command_env
                .cass_assert_command()
                .args(["index", "--full", "--json", "--data-dir"])
                .arg(&data_dir)
                .current_dir(&home)
                .env("CODEX_HOME", &codex_home)
                .env("HOME", &home)
                .timeout(Duration::from_secs(90))
                .assert()
                .success();
        },
    );

    let live_index_path = index_dir(&data_dir).expect("resolve live Tantivy index path");
    let before_summary = searchable_index_summary(&live_index_path)
        .expect("read baseline searchable index summary")
        .expect("baseline index should exist");
    let before_docs = before_summary.docs;
    assert!(
        before_docs > 0,
        "baseline index should contain at least one doc"
    );

    let mut baseline_command = command_env.cass_assert_command();
    baseline_command
        .args([
            "search",
            QUERY,
            "--json",
            "--mode",
            "lexical",
            "--fields",
            "minimal",
            "--limit",
            "5",
            "--data-dir",
        ])
        .arg(&data_dir)
        .current_dir(&home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", &home)
        .timeout(Duration::from_secs(20));
    let baseline_search = baseline_command
        .output()
        .expect("run baseline lexical search");
    assert!(
        baseline_search.status.success(),
        "baseline lexical search should succeed before force rebuild\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&baseline_search.stdout),
        String::from_utf8_lossy(&baseline_search.stderr)
    );
    let baseline_total_matches = total_matches_from_search_output(&baseline_search.stdout);
    assert!(
        baseline_total_matches > 0,
        "baseline lexical search should return at least one hit before rebuild"
    );

    let stop = Arc::new(AtomicBool::new(false));
    let rebuild_running = Arc::new(AtomicBool::new(false));
    let search_in_flight = Arc::new(AtomicBool::new(false));
    let reader_attempts_during_rebuild = Arc::new(AtomicUsize::new(0));
    let search_attempts_during_rebuild = Arc::new(AtomicUsize::new(0));
    let (ready_tx, ready_rx) = mpsc::channel();

    let reader_ready_tx = ready_tx.clone();
    let reader_stop = Arc::clone(&stop);
    let reader_rebuild_running = Arc::clone(&rebuild_running);
    let reader_overlap = Arc::clone(&reader_attempts_during_rebuild);
    let reader_index_path = live_index_path.clone();
    let reader_deadline = Instant::now() + Duration::from_secs(20);
    let reader_handle = std::thread::spawn(move || {
        let _ = reader_ready_tx.send("reader");
        let mut observations: Vec<Result<Option<SearchableIndexSummary>, String>> = Vec::new();
        while !reader_stop.load(Ordering::Relaxed) && Instant::now() < reader_deadline {
            if reader_rebuild_running.load(Ordering::Relaxed) {
                reader_overlap.fetch_add(1, Ordering::Relaxed);
            }
            let obs = searchable_index_summary(&reader_index_path).map_err(|e| format!("{e:#}"));
            observations.push(obs);
            std::thread::sleep(Duration::from_millis(1));
        }
        observations
    });

    let search_ready_tx = ready_tx.clone();
    let search_stop = Arc::clone(&stop);
    let search_rebuild_running = Arc::clone(&rebuild_running);
    let search_in_flight_thread = Arc::clone(&search_in_flight);
    let search_overlap = Arc::clone(&search_attempts_during_rebuild);
    let search_home = home.clone();
    let search_codex_home = codex_home.clone();
    let search_data_dir = data_dir.clone();
    let search_command_env = command_env.clone();
    let search_handle = std::thread::spawn(move || {
        let _ = search_ready_tx.send("search");
        let mut stats = SearchLoopStats::default();
        while !search_stop.load(Ordering::Relaxed) {
            let search_started = Instant::now();
            search_in_flight_thread.store(true, Ordering::Relaxed);
            let started_during_rebuild = search_rebuild_running.load(Ordering::Relaxed);
            let mut search_command = search_command_env.cass_assert_command();
            search_command
                .args([
                    "search",
                    QUERY,
                    "--json",
                    "--mode",
                    "lexical",
                    "--fields",
                    "minimal",
                    "--limit",
                    "5",
                    "--data-dir",
                ])
                .arg(&search_data_dir)
                .current_dir(&search_home)
                .env("CODEX_HOME", &search_codex_home)
                .env("HOME", &search_home)
                .timeout(Duration::from_secs(20));
            let output = search_command.output().expect("run concurrent cass search");
            search_in_flight_thread.store(false, Ordering::Relaxed);
            let search_finished = Instant::now();
            if started_during_rebuild || search_rebuild_running.load(Ordering::Relaxed) {
                search_overlap.fetch_add(1, Ordering::Relaxed);
            }
            let elapsed_ms = search_finished.duration_since(search_started).as_millis() as u64;
            stats.attempts += 1;
            stats.max_duration_ms = stats.max_duration_ms.max(elapsed_ms);

            if output.status.success() {
                let hit_count = total_matches_from_search_output(&output.stdout);
                if hit_count != baseline_total_matches {
                    stats.failures.push(format!(
                        "concurrent lexical search returned {hit_count} hits; expected stable total_matches={baseline_total_matches}\nstdout:\n{}",
                        String::from_utf8_lossy(&output.stdout)
                    ));
                } else {
                    stats.successes += 1;
                }
            } else {
                stats.failures.push(format!(
                    "concurrent lexical search failed: status={:?}\nstdout:\n{}\nstderr:\n{}",
                    output.status.code(),
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                ));
            }

            std::thread::sleep(Duration::from_millis(40));
        }

        stats
    });
    drop(ready_tx);

    for _ in 0..2 {
        ready_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("reader and search concurrency helpers should start promptly");
    }

    let rebuild_start = tracker.start(
        "force_rebuild_under_concurrent_reader_and_search",
        Some("Run cass index --full --force-rebuild while a direct reader and cass search poll the same live index"),
    );
    rebuild_running.store(true, Ordering::Relaxed);
    if search_in_flight.load(Ordering::Relaxed) {
        search_attempts_during_rebuild.fetch_add(1, Ordering::Relaxed);
    }
    let publish_pause_sentinel = home.join("atomic-publish-overlap-sentinel.json");
    let mut attempt = 0usize;
    let rebuild_output = loop {
        let output = command_env
            .cass_assert_command()
            .args(["index", "--full", "--force-rebuild", "--json", "--data-dir"])
            .arg(&data_dir)
            .current_dir(&home)
            .env("CODEX_HOME", &codex_home)
            .env("HOME", &home)
            .env(
                "CASS_TEST_LEXICAL_PUBLISH_KILL_RELAUNCH_SENTINEL",
                &publish_pause_sentinel,
            )
            .env("CASS_TEST_LEXICAL_PUBLISH_KILL_RELAUNCH_SLEEP_MS", "2000")
            .timeout(Duration::from_secs(60))
            .output()
            .expect("run force rebuild under concurrent read/search load");
        let retry_busy = !output.status.success()
            && command_output_kind_is(&output.stdout, "index-busy")
            && attempt < 4;
        if !retry_busy {
            break output;
        }
        attempt += 1;
        std::thread::sleep(Duration::from_millis(200));
    };
    rebuild_running.store(false, Ordering::Relaxed);
    let rebuild_duration_ms = rebuild_start.elapsed().as_millis() as u64;
    tracker.end(
        "force_rebuild_under_concurrent_reader_and_search",
        Some("Run cass index --full --force-rebuild while a direct reader and cass search poll the same live index"),
        rebuild_start,
    );

    stop.store(true, Ordering::Relaxed);
    let reader_observations = reader_handle.join().expect("join reader thread");
    let search_stats = search_handle.join().expect("join search thread");

    assert!(
        rebuild_output.status.success(),
        "force rebuild should succeed under concurrent reader/search load\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&rebuild_output.stdout),
        String::from_utf8_lossy(&rebuild_output.stderr)
    );
    let rebuild_json: serde_json::Value =
        serde_json::from_slice(&rebuild_output.stdout).expect("parse force rebuild json");
    assert_eq!(
        rebuild_json
            .get("success")
            .and_then(|value| value.as_bool()),
        Some(true),
        "force rebuild should report success in --json output"
    );

    assert!(
        !reader_observations.is_empty(),
        "reader should collect at least one observation during the force rebuild window"
    );
    assert!(
        reader_attempts_during_rebuild.load(Ordering::Relaxed) > 0,
        "expected direct reader overlap while force rebuild was running"
    );
    for (idx, observation) in reader_observations.iter().enumerate() {
        if let Ok(Some(summary)) = observation {
            assert_eq!(
                summary.docs, before_docs,
                "reader observation #{idx} returned docs={} instead of the stable count {before_docs}; \
                 this indicates a half-torn lexical index surface during atomic publish",
                summary.docs
            );
        }
    }

    assert!(
        search_attempts_during_rebuild.load(Ordering::Relaxed) > 0,
        "expected cass search overlap while force rebuild was running"
    );
    assert!(
        search_stats.failures.is_empty(),
        "concurrent cass search should stay logically stable during force rebuild:\n{}",
        search_stats.failures.join("\n---\n")
    );
    assert!(
        search_stats.successes > 0,
        "expected at least one successful concurrent cass search attempt"
    );

    let after_summary = searchable_index_summary(&live_index_path)
        .expect("read searchable summary after rebuild")
        .expect("live index should still exist after rebuild");
    assert_eq!(
        after_summary.docs, before_docs,
        "force rebuild on unchanged content should preserve the live doc count"
    );

    let mut after_command = command_env.cass_assert_command();
    after_command
        .args([
            "search",
            QUERY,
            "--json",
            "--mode",
            "lexical",
            "--fields",
            "minimal",
            "--limit",
            "5",
            "--data-dir",
        ])
        .arg(&data_dir)
        .current_dir(&home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", &home)
        .timeout(Duration::from_secs(20));
    let after_search = after_command
        .output()
        .expect("run post-rebuild lexical search");
    assert!(
        after_search.status.success(),
        "post-rebuild lexical search should succeed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&after_search.stdout),
        String::from_utf8_lossy(&after_search.stderr)
    );
    let after_total_matches = total_matches_from_search_output(&after_search.stdout);
    assert_eq!(
        after_total_matches, baseline_total_matches,
        "force rebuild on unchanged content should preserve the logical search result count"
    );

    tracker.metrics(
        "force_rebuild_concurrency_surface",
        &E2ePerformanceMetrics::new()
            .with_duration(rebuild_duration_ms)
            .with_custom(
                "reader_attempts_during_rebuild",
                reader_attempts_during_rebuild.load(Ordering::Relaxed) as u64,
            )
            .with_custom(
                "search_attempts_during_rebuild",
                search_attempts_during_rebuild.load(Ordering::Relaxed) as u64,
            )
            .with_custom("search_attempts_total", search_stats.attempts as u64)
            .with_custom("search_successes_total", search_stats.successes as u64)
            .with_custom("max_search_duration_ms", search_stats.max_duration_ms)
            .with_custom("stable_doc_count", before_docs as u64)
            .with_custom("stable_total_matches", baseline_total_matches),
    );
    tracker.complete();
}

#[test]
#[serial]
// Exercises the FEDERATED atomic-publish path, which is the Tantivy-only
// staged-shard machinery disabled for the Quill backend. The single-index
// atomic swap that actually runs is covered by
// tests/atomic_swap_publish_crash_window.rs. Ignored, not weakened.
#[ignore = "federated staged-shard publish is Tantivy-only; disabled on the Quill backend"]
fn force_rebuild_preserves_search_results_and_reader_surface_during_federated_atomic_publish() {
    let tracker = tracker_for(
        "force_rebuild_preserves_search_results_and_reader_surface_during_federated_atomic_publish",
    );
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path().to_path_buf();
    let codex_home = home.to_path_buf();
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    let command_env = tracker
        .command_environment()
        .with_home(&home)
        .with_codex_home(&codex_home);

    const QUERY: &str = "federatedatomicswapsearchanchor";
    for (filename, content, ts) in [
        (
            "rollout-fed-atomic-1.jsonl",
            format!("{QUERY} session_alpha"),
            1_732_310_000_000_u64,
        ),
        (
            "rollout-fed-atomic-2.jsonl",
            format!("{QUERY} session_beta"),
            1_732_310_100_000_u64,
        ),
        (
            "rollout-fed-atomic-3.jsonl",
            format!("{QUERY} session_gamma"),
            1_732_310_200_000_u64,
        ),
    ] {
        make_codex_session(&codex_home, "2024/11/23", filename, &content, ts);
    }

    tracker.phase(
        "seed_and_index_federated_fixture",
        "Create three sessions and force a federated lexical publish bundle",
        || {
            let mut initial_index = command_env.cass_assert_command();
            force_federated_publish_env(&mut initial_index);
            initial_index
                .args(["index", "--full", "--json", "--data-dir"])
                .arg(&data_dir)
                .current_dir(&home)
                .env("CODEX_HOME", &codex_home)
                .env("HOME", &home)
                .timeout(Duration::from_secs(30))
                .assert()
                .success();
        },
    );

    let live_index_path = index_dir(&data_dir).expect("resolve live Tantivy index path");
    let before_summary = searchable_index_summary(&live_index_path)
        .expect("read baseline federated searchable index summary")
        .expect("baseline federated index should exist");
    let before_docs = before_summary.docs;
    assert!(
        before_docs >= 3,
        "baseline federated index should contain multiple docs"
    );
    let before_federated_readers = open_federated_search_readers(&live_index_path)
        .expect("load federated readers before rebuild")
        .expect("baseline federated manifest should exist");
    assert!(
        before_federated_readers.len() > 1,
        "forced shard planner settings should produce a federated live index"
    );

    let mut baseline_command = command_env.cass_assert_command();
    baseline_command
        .args([
            "search",
            QUERY,
            "--json",
            "--mode",
            "lexical",
            "--fields",
            "minimal",
            "--limit",
            "10",
            "--data-dir",
        ])
        .arg(&data_dir)
        .current_dir(&home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", &home)
        .timeout(Duration::from_secs(20));
    let baseline_search = baseline_command
        .output()
        .expect("run baseline federated lexical search");
    assert!(
        baseline_search.status.success(),
        "baseline federated lexical search should succeed before force rebuild\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&baseline_search.stdout),
        String::from_utf8_lossy(&baseline_search.stderr)
    );
    let baseline_total_matches = total_matches_from_search_output(&baseline_search.stdout);
    assert!(
        baseline_total_matches > 0,
        "baseline federated lexical search should return at least one hit before rebuild"
    );

    let stop = Arc::new(AtomicBool::new(false));
    let rebuild_running = Arc::new(AtomicBool::new(false));
    let search_in_flight = Arc::new(AtomicBool::new(false));
    let reader_attempts_during_rebuild = Arc::new(AtomicUsize::new(0));
    let search_attempts_during_rebuild = Arc::new(AtomicUsize::new(0));
    let (ready_tx, ready_rx) = mpsc::channel();

    let reader_ready_tx = ready_tx.clone();
    let reader_stop = Arc::clone(&stop);
    let reader_rebuild_running = Arc::clone(&rebuild_running);
    let reader_overlap = Arc::clone(&reader_attempts_during_rebuild);
    let reader_index_path = live_index_path.clone();
    let reader_deadline = Instant::now() + Duration::from_secs(20);
    let reader_handle = std::thread::spawn(move || {
        let _ = reader_ready_tx.send("reader");
        let mut observations: Vec<Result<Option<SearchableIndexSummary>, String>> = Vec::new();
        while !reader_stop.load(Ordering::Relaxed) && Instant::now() < reader_deadline {
            if reader_rebuild_running.load(Ordering::Relaxed) {
                reader_overlap.fetch_add(1, Ordering::Relaxed);
            }
            let obs = searchable_index_summary(&reader_index_path).map_err(|e| format!("{e:#}"));
            observations.push(obs);
            std::thread::sleep(Duration::from_millis(1));
        }
        observations
    });

    let search_ready_tx = ready_tx.clone();
    let search_stop = Arc::clone(&stop);
    let search_rebuild_running = Arc::clone(&rebuild_running);
    let search_in_flight_thread = Arc::clone(&search_in_flight);
    let search_overlap = Arc::clone(&search_attempts_during_rebuild);
    let search_home = home.clone();
    let search_codex_home = codex_home.clone();
    let search_data_dir = data_dir.clone();
    let search_command_env = command_env.clone();
    let search_handle = std::thread::spawn(move || {
        let _ = search_ready_tx.send("search");
        let mut stats = SearchLoopStats::default();
        while !search_stop.load(Ordering::Relaxed) {
            let search_started = Instant::now();
            search_in_flight_thread.store(true, Ordering::Relaxed);
            let started_during_rebuild = search_rebuild_running.load(Ordering::Relaxed);
            let output = search_command_env
                .cass_assert_command()
                .args([
                    "search",
                    QUERY,
                    "--json",
                    "--mode",
                    "lexical",
                    "--fields",
                    "minimal",
                    "--limit",
                    "10",
                    "--data-dir",
                ])
                .arg(&search_data_dir)
                .current_dir(&search_home)
                .env("CODEX_HOME", &search_codex_home)
                .env("HOME", &search_home)
                .timeout(Duration::from_secs(20))
                .output()
                .expect("run concurrent federated cass search");
            search_in_flight_thread.store(false, Ordering::Relaxed);
            let search_finished = Instant::now();
            if started_during_rebuild || search_rebuild_running.load(Ordering::Relaxed) {
                search_overlap.fetch_add(1, Ordering::Relaxed);
            }
            let elapsed_ms = search_finished.duration_since(search_started).as_millis() as u64;
            stats.attempts += 1;
            stats.max_duration_ms = stats.max_duration_ms.max(elapsed_ms);

            if output.status.success() {
                let hit_count = total_matches_from_search_output(&output.stdout);
                if hit_count != baseline_total_matches {
                    stats.failures.push(format!(
                        "concurrent federated lexical search returned {hit_count} hits; expected stable total_matches={baseline_total_matches}\nstdout:\n{}",
                        String::from_utf8_lossy(&output.stdout)
                    ));
                } else {
                    stats.successes += 1;
                }
            } else {
                stats.failures.push(format!(
                    "concurrent federated lexical search failed: status={:?}\nstdout:\n{}\nstderr:\n{}",
                    output.status.code(),
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                ));
            }

            std::thread::sleep(Duration::from_millis(40));
        }

        stats
    });
    drop(ready_tx);

    for _ in 0..2 {
        ready_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("reader and search concurrency helpers should start promptly");
    }

    let rebuild_start = tracker.start(
        "force_federated_rebuild_under_concurrent_reader_and_search",
        Some("Run cass index --full --force-rebuild with forced multi-shard planning while a direct reader and cass search poll the same live index"),
    );
    rebuild_running.store(true, Ordering::Relaxed);
    if search_in_flight.load(Ordering::Relaxed) {
        search_attempts_during_rebuild.fetch_add(1, Ordering::Relaxed);
    }
    let mut rebuild = command_env.cass_assert_command();
    force_federated_publish_env(&mut rebuild);
    let publish_pause_sentinel = home.join("federated-atomic-publish-overlap-sentinel.json");
    let rebuild_output = rebuild
        .args(["index", "--full", "--force-rebuild", "--json", "--data-dir"])
        .arg(&data_dir)
        .current_dir(&home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", &home)
        .env(
            "CASS_TEST_LEXICAL_PUBLISH_KILL_RELAUNCH_SENTINEL",
            &publish_pause_sentinel,
        )
        .env("CASS_TEST_LEXICAL_PUBLISH_KILL_RELAUNCH_SLEEP_MS", "2000")
        .timeout(Duration::from_secs(60))
        .output()
        .expect("run federated force rebuild under concurrent read/search load");
    rebuild_running.store(false, Ordering::Relaxed);
    let rebuild_duration_ms = rebuild_start.elapsed().as_millis() as u64;
    tracker.end(
        "force_federated_rebuild_under_concurrent_reader_and_search",
        Some("Run cass index --full --force-rebuild with forced multi-shard planning while a direct reader and cass search poll the same live index"),
        rebuild_start,
    );

    stop.store(true, Ordering::Relaxed);
    let reader_observations = reader_handle.join().expect("join federated reader thread");
    let search_stats = search_handle.join().expect("join federated search thread");

    assert!(
        rebuild_output.status.success(),
        "federated force rebuild should succeed under concurrent reader/search load\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&rebuild_output.stdout),
        String::from_utf8_lossy(&rebuild_output.stderr)
    );
    let rebuild_json: serde_json::Value =
        serde_json::from_slice(&rebuild_output.stdout).expect("parse federated force rebuild json");
    assert_eq!(
        rebuild_json
            .get("success")
            .and_then(|value| value.as_bool()),
        Some(true),
        "federated force rebuild should report success in --json output"
    );

    assert!(
        !reader_observations.is_empty(),
        "reader should collect at least one observation during the federated force rebuild window"
    );
    assert!(
        reader_attempts_during_rebuild.load(Ordering::Relaxed) > 0,
        "expected direct reader overlap while federated force rebuild was running"
    );
    for (idx, observation) in reader_observations.iter().enumerate() {
        if let Ok(Some(summary)) = observation {
            assert_eq!(
                summary.docs, before_docs,
                "federated reader observation #{idx} returned docs={} instead of the stable count {before_docs}; \
                 this indicates a half-torn federated lexical index surface during atomic publish",
                summary.docs
            );
        }
    }

    assert!(
        search_attempts_during_rebuild.load(Ordering::Relaxed) > 0,
        "expected cass search overlap while federated force rebuild was running"
    );
    assert!(
        search_stats.failures.is_empty(),
        "concurrent cass search should stay logically stable during federated force rebuild:\n{}",
        search_stats.failures.join("\n---\n")
    );
    assert!(
        search_stats.successes > 0,
        "expected at least one successful concurrent federated cass search attempt"
    );

    let after_summary = searchable_index_summary(&live_index_path)
        .expect("read searchable summary after federated rebuild")
        .expect("live index should still exist after federated rebuild");
    assert_eq!(
        after_summary.docs, before_docs,
        "federated force rebuild on unchanged content should preserve the live doc count"
    );
    let after_federated_readers = open_federated_search_readers(&live_index_path)
        .expect("load federated readers after rebuild")
        .expect("federated manifest should still exist after rebuild");
    assert!(
        after_federated_readers.len() > 1,
        "post-rebuild live surface should remain a federated lexical bundle"
    );

    let after_search = command_env
        .cass_assert_command()
        .args([
            "search",
            QUERY,
            "--json",
            "--mode",
            "lexical",
            "--fields",
            "minimal",
            "--limit",
            "10",
            "--data-dir",
        ])
        .arg(&data_dir)
        .current_dir(&home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", &home)
        .timeout(Duration::from_secs(20))
        .output()
        .expect("run post-rebuild federated lexical search");
    assert!(
        after_search.status.success(),
        "post-rebuild federated lexical search should succeed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&after_search.stdout),
        String::from_utf8_lossy(&after_search.stderr)
    );
    let after_total_matches = total_matches_from_search_output(&after_search.stdout);
    assert_eq!(
        after_total_matches, baseline_total_matches,
        "federated force rebuild on unchanged content should preserve the logical search result count"
    );

    tracker.metrics(
        "force_federated_rebuild_concurrency_surface",
        &E2ePerformanceMetrics::new()
            .with_duration(rebuild_duration_ms)
            .with_custom(
                "reader_attempts_during_rebuild",
                reader_attempts_during_rebuild.load(Ordering::Relaxed) as u64,
            )
            .with_custom(
                "search_attempts_during_rebuild",
                search_attempts_during_rebuild.load(Ordering::Relaxed) as u64,
            )
            .with_custom("search_attempts_total", search_stats.attempts as u64)
            .with_custom("search_successes_total", search_stats.successes as u64)
            .with_custom("max_search_duration_ms", search_stats.max_duration_ms)
            .with_custom("stable_doc_count", before_docs as u64)
            .with_custom("stable_total_matches", baseline_total_matches)
            .with_custom(
                "federated_shard_count",
                after_federated_readers.len() as u64,
            ),
    );
    tracker.complete();
}

#[test]
#[serial]
// Exercises the FEDERATED atomic-publish path, which is the Tantivy-only
// staged-shard machinery disabled for the Quill backend. The single-index
// atomic swap that actually runs is covered by
// tests/atomic_swap_publish_crash_window.rs. Ignored, not weakened.
#[ignore = "federated staged-shard publish is Tantivy-only; disabled on the Quill backend"]
fn repeated_force_rebuild_preserves_federated_reader_and_search_stability() {
    let tracker =
        tracker_for("repeated_force_rebuild_preserves_federated_reader_and_search_stability");
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path().to_path_buf();
    let codex_home = home.to_path_buf();
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    let command_env = tracker
        .command_environment()
        .with_home(&home)
        .with_codex_home(&codex_home);

    const QUERY: &str = "federatedrebuildstabilityanchor";
    const REBUILD_CYCLES: usize = 20;
    for (filename, content, ts) in [
        (
            "rollout-fed-stability-1.jsonl",
            format!("{QUERY} stability_alpha"),
            1_732_311_000_000_u64,
        ),
        (
            "rollout-fed-stability-2.jsonl",
            format!("{QUERY} stability_beta"),
            1_732_311_100_000_u64,
        ),
        (
            "rollout-fed-stability-3.jsonl",
            format!("{QUERY} stability_gamma"),
            1_732_311_200_000_u64,
        ),
    ] {
        make_codex_session(&codex_home, "2024/11/24", filename, &content, ts);
    }

    tracker.phase(
        "seed_and_index_repeated_federated_fixture",
        "Create three sessions and force an initial federated lexical publish bundle",
        || {
            let mut initial_index = command_env.cass_assert_command();
            force_federated_publish_env(&mut initial_index);
            initial_index
                .args(["index", "--full", "--json", "--data-dir"])
                .arg(&data_dir)
                .current_dir(&home)
                .env("CODEX_HOME", &codex_home)
                .env("HOME", &home)
                .timeout(Duration::from_secs(30))
                .assert()
                .success();
        },
    );

    let live_index_path = index_dir(&data_dir).expect("resolve live Tantivy index path");
    let before_summary = searchable_index_summary(&live_index_path)
        .expect("read baseline federated searchable index summary")
        .expect("baseline federated index should exist");
    let before_docs = before_summary.docs;
    assert!(
        before_docs >= 3,
        "baseline federated index should contain multiple docs"
    );
    let before_federated_readers = open_federated_search_readers(&live_index_path)
        .expect("load federated readers before repeated rebuilds")
        .expect("baseline federated manifest should exist");
    let baseline_federated_reader_count = before_federated_readers.len();
    assert!(
        baseline_federated_reader_count > 1,
        "forced shard planner settings should produce a federated live index"
    );

    let baseline_search = command_env
        .cass_assert_command()
        .args([
            "search",
            QUERY,
            "--json",
            "--mode",
            "lexical",
            "--fields",
            "minimal",
            "--limit",
            "10",
            "--data-dir",
        ])
        .arg(&data_dir)
        .current_dir(&home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", &home)
        .timeout(Duration::from_secs(20))
        .output()
        .expect("run baseline repeated federated lexical search");
    assert!(
        baseline_search.status.success(),
        "baseline repeated federated lexical search should succeed before force rebuild loop\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&baseline_search.stdout),
        String::from_utf8_lossy(&baseline_search.stderr)
    );
    let baseline_total_matches = total_matches_from_search_output(&baseline_search.stdout);
    assert!(
        baseline_total_matches > 0,
        "baseline repeated federated lexical search should return at least one hit before rebuild loop"
    );

    let repeated_rebuild_started = tracker.start(
        "repeat_federated_force_rebuilds_and_validate_stability",
        Some("Run repeated cass index --full --force-rebuild cycles with forced multi-shard planning and verify reader/search stability after every publish"),
    );
    let mut max_rebuild_duration_ms = 0_u64;
    for cycle in 0..REBUILD_CYCLES {
        let rebuild_started = Instant::now();
        let mut rebuild = command_env.cass_assert_command();
        force_federated_publish_env(&mut rebuild);
        let rebuild_output = rebuild
            .args(["index", "--full", "--force-rebuild", "--json", "--data-dir"])
            .arg(&data_dir)
            .current_dir(&home)
            .env("CODEX_HOME", &codex_home)
            .env("HOME", &home)
            .timeout(Duration::from_secs(60))
            .output()
            .expect("run repeated federated force rebuild");
        let rebuild_duration_ms = rebuild_started.elapsed().as_millis() as u64;
        max_rebuild_duration_ms = max_rebuild_duration_ms.max(rebuild_duration_ms);

        assert!(
            rebuild_output.status.success(),
            "repeated federated force rebuild cycle {cycle} should succeed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&rebuild_output.stdout),
            String::from_utf8_lossy(&rebuild_output.stderr)
        );
        let rebuild_json: serde_json::Value = serde_json::from_slice(&rebuild_output.stdout)
            .expect("parse repeated federated force rebuild json");
        assert_eq!(
            rebuild_json
                .get("success")
                .and_then(|value| value.as_bool()),
            Some(true),
            "repeated federated force rebuild cycle {cycle} should report success in --json output"
        );

        let cycle_summary = searchable_index_summary(&live_index_path)
            .expect("read searchable summary after repeated federated rebuild cycle")
            .expect("live index should exist after repeated federated rebuild cycle");
        assert_eq!(
            cycle_summary.docs, before_docs,
            "repeated federated force rebuild cycle {cycle} changed the live doc count from {before_docs} to {}; \
             the publish path should remain stable for unchanged content",
            cycle_summary.docs
        );

        let cycle_federated_readers = open_federated_search_readers(&live_index_path)
            .expect("load federated readers after repeated rebuild cycle")
            .expect("federated manifest should exist after repeated rebuild cycle");
        assert_eq!(
            cycle_federated_readers.len(),
            baseline_federated_reader_count,
            "repeated federated force rebuild cycle {cycle} changed the shard bundle width from {baseline_federated_reader_count} to {}; \
             forced federated publish should remain structurally stable",
            cycle_federated_readers.len()
        );

        let cycle_search = command_env
            .cass_assert_command()
            .args([
                "search",
                QUERY,
                "--json",
                "--mode",
                "lexical",
                "--fields",
                "minimal",
                "--limit",
                "10",
                "--data-dir",
            ])
            .arg(&data_dir)
            .current_dir(&home)
            .env("CODEX_HOME", &codex_home)
            .env("HOME", &home)
            .timeout(Duration::from_secs(20))
            .output()
            .expect("run repeated post-rebuild federated lexical search");
        assert!(
            cycle_search.status.success(),
            "repeated federated lexical search after cycle {cycle} should succeed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&cycle_search.stdout),
            String::from_utf8_lossy(&cycle_search.stderr)
        );
        let cycle_total_matches = total_matches_from_search_output(&cycle_search.stdout);
        assert_eq!(
            cycle_total_matches, baseline_total_matches,
            "repeated federated force rebuild cycle {cycle} changed the logical lexical hit count from {baseline_total_matches} to {cycle_total_matches}"
        );
    }
    tracker.end(
        "repeat_federated_force_rebuilds_and_validate_stability",
        Some("Run repeated cass index --full --force-rebuild cycles with forced multi-shard planning and verify reader/search stability after every publish"),
        repeated_rebuild_started,
    );

    tracker.metrics(
        "repeated_federated_rebuild_stability_surface",
        &E2ePerformanceMetrics::new()
            .with_duration(repeated_rebuild_started.elapsed().as_millis() as u64)
            .with_custom("rebuild_cycles", REBUILD_CYCLES as u64)
            .with_custom("max_rebuild_duration_ms", max_rebuild_duration_ms)
            .with_custom("stable_doc_count", before_docs as u64)
            .with_custom("stable_total_matches", baseline_total_matches)
            .with_custom(
                "federated_shard_count",
                baseline_federated_reader_count as u64,
            ),
    );
    tracker.complete();
}

#[cfg(target_os = "linux")]
#[test]
#[serial]
// Exercises the FEDERATED atomic-publish path, which is the Tantivy-only
// staged-shard machinery disabled for the Quill backend. The single-index
// atomic swap that actually runs is covered by
// tests/atomic_swap_publish_crash_window.rs. Ignored, not weakened.
#[ignore = "federated staged-shard publish is Tantivy-only; disabled on the Quill backend"]
fn force_rebuild_recovers_cleanly_after_sigkill_between_linux_swap_and_retain() {
    let tracker =
        tracker_for("force_rebuild_recovers_cleanly_after_sigkill_between_linux_swap_and_retain");
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path().to_path_buf();
    let codex_home = home.to_path_buf();
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    let command_env = tracker
        .command_environment()
        .with_home(&home)
        .with_codex_home(&codex_home);

    const QUERY: &str = "killrelaunchpublishanchor";

    tracker.phase(
        "seed_and_index_single_shard_fixture",
        "Create a minimal fixture and build the baseline lexical index",
        || {
            make_codex_session(
                &codex_home,
                "2024/11/24",
                "rollout-kill-relaunch.jsonl",
                QUERY,
                1_732_320_000_000,
            );

            command_env
                .cass_assert_command()
                .args(["index", "--full", "--json", "--data-dir"])
                .arg(&data_dir)
                .current_dir(&home)
                .env("CODEX_HOME", &codex_home)
                .env("HOME", &home)
                .timeout(Duration::from_secs(30))
                .assert()
                .success();
        },
    );

    let live_index_path = index_dir(&data_dir).expect("resolve live Tantivy index path");
    let canonical_sidecar = lexical_publish_in_progress_backup_path(&live_index_path);
    let backups_dir = lexical_publish_backups_dir(&live_index_path);
    let before_summary = searchable_index_summary(&live_index_path)
        .expect("read baseline searchable index summary")
        .expect("baseline index should exist");
    let before_docs = before_summary.docs;
    assert!(
        before_docs > 0,
        "baseline index should contain at least one doc"
    );

    let baseline_search = command_env
        .cass_assert_command()
        .args([
            "search",
            QUERY,
            "--json",
            "--mode",
            "lexical",
            "--fields",
            "minimal",
            "--limit",
            "5",
            "--data-dir",
        ])
        .arg(&data_dir)
        .current_dir(&home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", &home)
        .timeout(Duration::from_secs(20))
        .output()
        .expect("run baseline lexical search");
    assert!(
        baseline_search.status.success(),
        "baseline lexical search should succeed before kill/relaunch test\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&baseline_search.stdout),
        String::from_utf8_lossy(&baseline_search.stderr)
    );
    let baseline_total_matches = total_matches_from_search_output(&baseline_search.stdout);
    assert!(
        baseline_total_matches > 0,
        "baseline lexical search should return at least one hit before kill/relaunch"
    );

    let sentinel_path = home.join("publish-kill-relaunch-sentinel.json");
    let rebuild_start = tracker.start(
        "sigkill_force_rebuild_in_linux_publish_window",
        Some(
            "Spawn cass index --full --force-rebuild, pause after NEW is live and OLD is parked, then SIGKILL the process",
        ),
    );
    let mut child = cass_std_cmd(&command_env, &home);
    child.env(
        "CASS_TEST_LEXICAL_PUBLISH_KILL_RELAUNCH_SENTINEL",
        &sentinel_path,
    );
    child.env("CASS_TEST_LEXICAL_PUBLISH_KILL_RELAUNCH_SLEEP_MS", "30000");
    child.args(["index", "--full", "--force-rebuild", "--json", "--data-dir"]);
    child.arg(&data_dir);
    let mut child = child.spawn().expect("spawn force rebuild child process");

    let sentinel = wait_for_publish_kill_relaunch_sentinel(&sentinel_path, Duration::from_secs(20));
    assert_eq!(
        sentinel.get("stage").and_then(|value| value.as_str()),
        Some("linux_swap_committed_prior_live_parked"),
        "sentinel must prove the child paused after NEW went live and OLD was parked"
    );
    assert_eq!(
        sentinel
            .get("live_index_path")
            .and_then(|value| value.as_str()),
        Some(live_index_path.to_string_lossy().as_ref()),
        "sentinel should describe the live index path under test"
    );
    assert_eq!(
        sentinel
            .get("canonical_sidecar_path")
            .and_then(|value| value.as_str()),
        Some(canonical_sidecar.to_string_lossy().as_ref()),
        "sentinel should describe the canonical sidecar path under test"
    );

    assert!(
        live_index_path.exists(),
        "live lexical index must still exist while the child is paused in the publish window"
    );
    assert!(
        canonical_sidecar.exists(),
        "prior live generation must be parked at the canonical sidecar before SIGKILL"
    );
    let paused_summary = searchable_index_summary(&live_index_path)
        .expect("read live summary while child is paused")
        .expect("live index should remain readable while paused");
    assert_eq!(
        paused_summary.docs, before_docs,
        "paused publish window must still expose the stable live doc count"
    );

    let paused_search = command_env
        .cass_assert_command()
        .args([
            "search",
            QUERY,
            "--json",
            "--mode",
            "lexical",
            "--fields",
            "minimal",
            "--limit",
            "5",
            "--data-dir",
        ])
        .arg(&data_dir)
        .current_dir(&home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", &home)
        .timeout(Duration::from_secs(20))
        .output()
        .expect("run lexical search while child is paused");
    assert!(
        paused_search.status.success(),
        "lexical search should still succeed while the child is paused in the publish window\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&paused_search.stdout),
        String::from_utf8_lossy(&paused_search.stderr)
    );
    assert_eq!(
        total_matches_from_search_output(&paused_search.stdout),
        baseline_total_matches,
        "paused publish window must preserve stable search results"
    );

    child.kill().expect("send SIGKILL to paused rebuild child");
    let child_status = child.wait().expect("wait for killed rebuild child");
    tracker.end(
        "sigkill_force_rebuild_in_linux_publish_window",
        Some(
            "Spawn cass index --full --force-rebuild, pause after NEW is live and OLD is parked, then SIGKILL the process",
        ),
        rebuild_start,
    );
    assert!(
        !child_status.success(),
        "SIGKILLed rebuild child must not report success"
    );
    assert!(
        live_index_path.exists(),
        "live lexical index must still exist immediately after SIGKILL"
    );
    assert!(
        canonical_sidecar.exists(),
        "SIGKILL should strand the canonical sidecar for restart recovery"
    );

    let relaunch_start = tracker.start(
        "relaunch_force_rebuild_and_recover_sidecar",
        Some(
            "Relaunch cass index --full --force-rebuild and prove recovery finalizes the stranded sidecar cleanly",
        ),
    );
    let relaunch_output = command_env
        .cass_assert_command()
        .args(["index", "--full", "--force-rebuild", "--json", "--data-dir"])
        .arg(&data_dir)
        .current_dir(&home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", &home)
        .timeout(Duration::from_secs(60))
        .output()
        .expect("relaunch force rebuild after SIGKILL");
    tracker.end(
        "relaunch_force_rebuild_and_recover_sidecar",
        Some(
            "Relaunch cass index --full --force-rebuild and prove recovery finalizes the stranded sidecar cleanly",
        ),
        relaunch_start,
    );
    assert!(
        relaunch_output.status.success(),
        "relaunch after SIGKILL should succeed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&relaunch_output.stdout),
        String::from_utf8_lossy(&relaunch_output.stderr)
    );
    let relaunch_json: serde_json::Value =
        serde_json::from_slice(&relaunch_output.stdout).expect("parse relaunch index json");
    assert_eq!(
        relaunch_json
            .get("success")
            .and_then(|value| value.as_bool()),
        Some(true),
        "relaunch force rebuild should report success in --json output"
    );

    assert!(
        !canonical_sidecar.exists(),
        "relaunch recovery must consume the stranded canonical sidecar"
    );
    let retained_backup_count = fs::read_dir(&backups_dir)
        .map(|entries| entries.filter_map(Result::ok).count())
        .unwrap_or(0);
    assert!(
        retained_backup_count >= 1,
        "relaunch recovery should retain at least one prior-live artifact after cleaning the stranded sidecar"
    );

    let after_summary = searchable_index_summary(&live_index_path)
        .expect("read live summary after relaunch recovery")
        .expect("live index should remain readable after relaunch recovery");
    assert_eq!(
        after_summary.docs, before_docs,
        "relaunch recovery must preserve the stable live doc count"
    );

    let after_search = command_env
        .cass_assert_command()
        .args([
            "search",
            QUERY,
            "--json",
            "--mode",
            "lexical",
            "--fields",
            "minimal",
            "--limit",
            "5",
            "--data-dir",
        ])
        .arg(&data_dir)
        .current_dir(&home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", &home)
        .timeout(Duration::from_secs(20))
        .output()
        .expect("run lexical search after relaunch recovery");
    assert!(
        after_search.status.success(),
        "lexical search should succeed after relaunch recovery\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&after_search.stdout),
        String::from_utf8_lossy(&after_search.stderr)
    );
    assert_eq!(
        total_matches_from_search_output(&after_search.stdout),
        baseline_total_matches,
        "relaunch recovery must preserve stable lexical search results"
    );

    tracker.metrics(
        "kill_relaunch_publish_recovery",
        &E2ePerformanceMetrics::new()
            .with_custom("stable_doc_count", before_docs as u64)
            .with_custom("stable_total_matches", baseline_total_matches)
            .with_custom(
                "retained_backup_count_after_relaunch",
                retained_backup_count as u64,
            ),
    );
    tracker.complete();
}

/// Test: Full index pipeline - index --full creates DB and index
#[test]
#[serial]
fn index_full_creates_artifacts() {
    verbose!("Starting index_full_creates_artifacts test");
    let tracker = tracker_for("index_full_creates_artifacts");
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    verbose!("Created temp directory at {:?}", home);
    let codex_home = home.to_path_buf();
    let command_env = tracker
        .command_environment()
        .with_home(home)
        .with_codex_home(&codex_home);
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    verbose!("Data directory: {:?}", data_dir);

    // Create fixture data
    let phase_start = tracker.start("create_fixtures", Some("Create Codex session fixture"));
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-1.jsonl",
        "hello world",
        1732118400000,
    );
    tracker.end(
        "create_fixtures",
        Some("Create Codex session fixture"),
        phase_start,
    );

    // Capture memory/IO before indexing (for delta calculation)
    let mem_before = E2ePerformanceMetrics::capture_memory();
    let io_before = E2ePerformanceMetrics::capture_io();

    // Run index --full
    let phase_start = tracker.start("index_full", Some("Execute full index command"));
    command_env
        .cass_assert_command()
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        // Avoid connector detection from the repository CWD (e.g. `.aider.chat.history.md`).
        .current_dir(home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();
    let index_duration_ms = phase_start.elapsed().as_millis() as u64;
    tracker.end(
        "index_full",
        Some("Execute full index command"),
        phase_start,
    );

    // Capture memory/IO after indexing
    let mem_after = E2ePerformanceMetrics::capture_memory();
    let io_after = E2ePerformanceMetrics::capture_io();
    verbose!("Index completed in {}ms", index_duration_ms);

    // Verify artifacts created
    let phase_start = tracker.start("verify_artifacts", Some("Verify database and index exist"));
    verbose!("Verifying artifacts at {:?}", data_dir);
    assert!(
        data_dir.join("agent_search.db").exists(),
        "SQLite DB should be created"
    );
    assert!(
        data_dir.join("index").exists(),
        "Tantivy index directory should exist"
    );
    tracker.end(
        "verify_artifacts",
        Some("Verify database and index exist"),
        phase_start,
    );

    // Count messages and emit performance metrics
    let msg_count = count_messages(&data_dir.join("agent_search.db")) as u64;
    verbose!("Indexed {} messages", msg_count);
    let mut metrics = E2ePerformanceMetrics::new()
        .with_duration(index_duration_ms)
        .with_throughput(msg_count, index_duration_ms);

    // Add memory delta if available
    if let (Some(before), Some(after)) = (mem_before, mem_after) {
        metrics = metrics.with_memory(after.saturating_sub(before));
    }

    // Add I/O delta if available
    if let (Some((rb, wb)), Some((ra, wa))) = (io_before, io_after) {
        metrics = metrics.with_io(0, 0, ra.saturating_sub(rb), wa.saturating_sub(wb));
    }

    tracker.metrics("index_full", &metrics);
    tracker.flush();
    verbose!("Test index_full_creates_artifacts completed successfully");
}

/// Incremental re-index must preserve existing messages and ingest new ones from the same file.
#[test]
#[serial]
fn incremental_reindex_preserves_and_appends_messages() {
    let tracker = tracker_for("incremental_reindex_preserves_and_appends_messages");
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.to_path_buf();
    let command_env = tracker
        .command_environment()
        .with_home(home)
        .with_codex_home(&codex_home);
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    // Initial session
    let phase_start = tracker.start(
        "create_initial_fixture",
        Some("Create initial session with test content"),
    );
    let ts = 1_732_118_400_000u64; // stable timestamp
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-incremental.jsonl",
        "initial_keep_token",
        ts,
    );
    let session_file = codex_home.join("sessions/2024/11/20/rollout-incremental.jsonl");
    tracker.end(
        "create_initial_fixture",
        Some("Create initial session with test content"),
        phase_start,
    );

    // Full index
    let phase_start = tracker.start("index_full", Some("Run initial full index"));
    command_env
        .cass_assert_command()
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        // Avoid connector detection from the repository CWD (e.g. `.aider.chat.history.md`).
        .current_dir(home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();
    tracker.end("index_full", Some("Run initial full index"), phase_start);

    // Ensure subsequent writes get a later mtime than the recorded scan start
    std::thread::sleep(std::time::Duration::from_millis(1200));

    // Baseline search should find the initial content
    let phase_start = tracker.start(
        "search_baseline",
        Some("Verify initial content is searchable"),
    );
    let baseline = command_env
        .cass_assert_command()
        .args(["search", "initial_keep_token", "--robot", "--data-dir"])
        .arg(&data_dir)
        // Avoid connector detection from the repository CWD (e.g. `.aider.chat.history.md`).
        .current_dir(home)
        .env("HOME", home)
        .output()
        .expect("baseline search");
    assert!(baseline.status.success());
    let baseline_json: serde_json::Value =
        serde_json::from_slice(&baseline.stdout).expect("baseline json");
    let baseline_hits = baseline_json
        .get("hits")
        .and_then(|h| h.as_array())
        .map(|v| v.len())
        .unwrap_or(0);
    assert!(baseline_hits >= 1, "initial content should be indexed");
    tracker.end(
        "search_baseline",
        Some("Verify initial content is searchable"),
        phase_start,
    );

    // Append new content to the same file (simulates conversation growth)
    let phase_start = tracker.start(
        "append_content",
        Some("Append new messages to session file"),
    );
    append_codex_session(&session_file, "appended_token_beta", ts + 10_000);
    tracker.end(
        "append_content",
        Some("Append new messages to session file"),
        phase_start,
    );

    // On some filesystems, mtime resolution is 1s; give a small buffer before reindex
    std::thread::sleep(std::time::Duration::from_millis(50));

    // Incremental re-index (no --full)
    let phase_start = tracker.start("index_incremental", Some("Run incremental reindex"));
    command_env
        .cass_assert_command()
        .args(["index", "--data-dir"])
        .arg(&data_dir)
        // Avoid connector detection from the repository CWD (e.g. `.aider.chat.history.md`).
        .current_dir(home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();
    tracker.end(
        "index_incremental",
        Some("Run incremental reindex"),
        phase_start,
    );

    // Original content must still be present
    let phase_start = tracker.start(
        "search_preserved",
        Some("Verify original content preserved"),
    );
    let preserved = command_env
        .cass_assert_command()
        .args(["search", "initial_keep_token", "--robot", "--data-dir"])
        .arg(&data_dir)
        // Avoid connector detection from the repository CWD (e.g. `.aider.chat.history.md`).
        .current_dir(home)
        .env("HOME", home)
        .output()
        .expect("preserved search");
    assert!(preserved.status.success());
    let preserved_hits = serde_json::from_slice::<serde_json::Value>(&preserved.stdout)
        .unwrap()
        .get("hits")
        .and_then(|h| h.as_array())
        .map(|v| v.len())
        .unwrap_or(0);
    assert!(
        preserved_hits >= baseline_hits,
        "existing messages should not be dropped on reindex"
    );
    tracker.end(
        "search_preserved",
        Some("Verify original content preserved"),
        phase_start,
    );

    // New content must be discoverable
    let phase_start = tracker.start("search_appended", Some("Verify appended content indexed"));
    let appended = command_env
        .cass_assert_command()
        .args(["search", "appended_token_beta", "--robot", "--data-dir"])
        .arg(&data_dir)
        // Avoid connector detection from the repository CWD (e.g. `.aider.chat.history.md`).
        .current_dir(home)
        .env("HOME", home)
        .output()
        .expect("appended search");
    assert!(appended.status.success());
    let appended_hits = serde_json::from_slice::<serde_json::Value>(&appended.stdout)
        .unwrap()
        .get("hits")
        .and_then(|h| h.as_array())
        .map(|v| v.len())
        .unwrap_or(0);
    assert!(
        appended_hits >= 1,
        "appended content should be indexed during incremental run"
    );
    tracker.end(
        "search_appended",
        Some("Verify appended content indexed"),
        phase_start,
    );

    tracker.flush();
}

/// Reindexing must never drop previously ingested messages in SQLite or Tantivy.
#[test]
#[serial]
fn reindex_does_not_drop_messages_in_db_or_search() {
    let tracker = tracker_for("reindex_does_not_drop_messages_in_db_or_search");
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.to_path_buf();
    let command_env = tracker
        .command_environment()
        .with_home(home)
        .with_codex_home(&codex_home);
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    let xdg_data = home.join(".local/share");
    let xdg_config = home.join(".config");
    fs::create_dir_all(&xdg_data).unwrap();
    fs::create_dir_all(&xdg_config).unwrap();

    // Seed a rollout with two messages
    let ts = 1_732_118_400_000u64;
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-drop-guard.jsonl",
        "persist_me",
        ts,
    );
    let session_file = codex_home.join("sessions/2024/11/20/rollout-drop-guard.jsonl");

    command_env
        .cass_assert_command()
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        // Avoid connector detection from the repository CWD (e.g. `.aider.chat.history.md`).
        .current_dir(home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .env("XDG_DATA_HOME", &xdg_data)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .assert()
        .success();

    // Ensure next write has strictly newer mtime than initial scan start
    std::thread::sleep(std::time::Duration::from_millis(1200));

    let db_path = data_dir.join("agent_search.db");
    let baseline_count = count_messages(&db_path);
    assert_eq!(baseline_count, 2, "initial two messages recorded");

    // Append another turn and reindex incrementally
    append_codex_session(&session_file, "persist_me_again", ts + 5_000);
    std::thread::sleep(std::time::Duration::from_millis(50));
    command_env
        .cass_assert_command()
        .args(["index", "--data-dir"])
        .arg(&data_dir)
        // Avoid connector detection from the repository CWD (e.g. `.aider.chat.history.md`).
        .current_dir(home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .env("XDG_DATA_HOME", &xdg_data)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .assert()
        .success();

    let after_count = count_messages(&db_path);
    assert_eq!(
        after_count,
        baseline_count + 2,
        "messages should only grow after reindex"
    );

    // Verify both old and new content are searchable (Tantivy layer)
    for term in ["persist_me", "persist_me_again"] {
        let out = command_env
            .cass_assert_command()
            .args(["search", term, "--robot", "--data-dir"])
            .arg(&data_dir)
            // Avoid connector detection from the repository CWD (e.g. `.aider.chat.history.md`).
            .current_dir(home)
            .env("HOME", home)
            .env("XDG_DATA_HOME", &xdg_data)
            .env("XDG_CONFIG_HOME", &xdg_config)
            .output()
            .expect("search");
        assert!(out.status.success(), "search should succeed for {term}");
        let hits = serde_json::from_slice::<serde_json::Value>(&out.stdout)
            .unwrap()
            .get("hits")
            .and_then(|h| h.as_array())
            .map(|v| v.len())
            .unwrap_or(0);
        assert!(hits >= 1, "{term} should remain indexed");
    }
}

/// Test: Search returns hits with correct match_type
#[test]
#[serial]
fn search_returns_hits_with_match_type() {
    let tracker = tracker_for("search_returns_hits_with_match_type");
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.to_path_buf();
    let command_env = tracker
        .command_environment()
        .with_home(home)
        .with_codex_home(&codex_home);
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    // Create fixture with unique content
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-1.jsonl",
        "unique_search_term_alpha",
        1732118400000,
    );

    // Index first
    command_env
        .cass_assert_command()
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();

    // Search and verify JSON output
    let output = command_env
        .cass_assert_command()
        .args([
            "search",
            "unique_search_term_alpha",
            "--robot",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", home)
        .output()
        .expect("search command");

    assert!(output.status.success(), "Search should succeed");

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid JSON output");

    // Verify hits array exists
    let hits = json
        .get("hits")
        .and_then(|h| h.as_array())
        .expect("hits array should exist");
    assert!(!hits.is_empty(), "Should find at least one hit");

    // Verify match_type field
    let first_hit = &hits[0];
    assert!(
        first_hit.get("match_type").is_some(),
        "Hit should have match_type field"
    );
    let match_type = first_hit["match_type"].as_str().unwrap();
    assert!(
        ["exact", "prefix", "wildcard", "fuzzy", "wildcard_fallback"].contains(&match_type),
        "match_type should be a known type, got: {}",
        match_type
    );

    // Verify content contains search term
    let content = first_hit["content"].as_str().unwrap_or("");
    assert!(
        content.contains("unique_search_term_alpha"),
        "Content should contain search term"
    );
}

/// Test: Search aggregations include agent buckets
#[test]
#[serial]
fn search_aggregations_include_agents() {
    let tracker = tracker_for("search_aggregations_include_agents");
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.to_path_buf();
    let command_env = tracker
        .command_environment()
        .with_home(home)
        .with_codex_home(&codex_home);
    let claude_home = home.join(".claude");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    // Create fixtures from multiple connectors
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-agg.jsonl",
        "aggregation_test_content",
        1732118400000,
    );
    make_claude_session(
        &claude_home,
        "agg-project",
        "session-agg.jsonl",
        "aggregation_test_content",
        "2024-11-20T10:00:00Z",
    );

    // Index
    command_env
        .cass_assert_command()
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();

    // Search with aggregations
    let output = command_env
        .cass_assert_command()
        .args([
            "search",
            "aggregation_test_content",
            "--aggregate",
            "agent",
            "--robot",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", home)
        .output()
        .expect("search command");

    assert!(output.status.success(), "Search should succeed");

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");

    // Verify aggregations
    let aggregations = json
        .get("aggregations")
        .expect("aggregations field should exist");
    let agent_agg = aggregations.get("agent").expect("agent aggregation");
    let buckets = agent_agg
        .get("buckets")
        .and_then(|b| b.as_array())
        .expect("buckets array");

    let agent_keys: std::collections::HashSet<_> = buckets
        .iter()
        .filter_map(|b| b.get("key").and_then(|k| k.as_str()))
        .collect();

    // At least one of our fixtures should be found in aggregations
    // (Claude works reliably via HOME; Codex via CODEX_HOME may vary by platform)
    assert!(
        agent_keys.contains("codex") || agent_keys.contains("claude_code"),
        "Should include at least one expected agent. Found: {:?}",
        agent_keys
    );
}

/// Test: Watch-once mode indexes specific paths
#[test]
#[serial]
fn watch_once_indexes_specified_path() {
    let tracker = tracker_for("watch_once_indexes_specified_path");
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.to_path_buf();
    let command_env = tracker
        .command_environment()
        .with_home(home)
        .with_codex_home(&codex_home);
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    // Create initial data
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-watch.jsonl",
        "watch_once_initial",
        1732118400000,
    );

    // Initial index
    command_env
        .cass_assert_command()
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();

    // Create new file to watch
    let watch_file = codex_home.join("sessions/2024/11/21/rollout-new.jsonl");
    fs::create_dir_all(watch_file.parent().unwrap()).unwrap();

    // Use current timestamp so message is indexed
    let now_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    let sample = format!(
        r#"{{"type": "event_msg", "timestamp": {now_ts}, "payload": {{"type": "user_message", "message": "watch_once_new_content"}}}}
{{"type": "response_item", "timestamp": {}, "payload": {{"role": "assistant", "content": "watch_once_response"}}}}"#,
        now_ts + 1000
    );
    fs::write(&watch_file, sample).unwrap();

    // Run watch-once with specific path
    command_env
        .cass_assert_command()
        .args(["index", "--watch-once"])
        .arg(&watch_file)
        .arg("--data-dir")
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();

    // Verify new content is searchable
    let output = command_env
        .cass_assert_command()
        .args(["search", "watch_once_new_content", "--robot", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", home)
        .output()
        .expect("search command");

    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    let hits = json.get("hits").and_then(|h| h.as_array()).expect("hits");
    assert!(
        !hits.is_empty(),
        "Should find the newly indexed watch-once content"
    );
}

/// Test: Search with filters (agent, time range)
#[test]
#[serial]
fn search_with_filters() {
    let tracker = tracker_for("search_with_filters");
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.to_path_buf();
    let command_env = tracker
        .command_environment()
        .with_home(home)
        .with_codex_home(&codex_home);
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    // Create multiple sessions with distinct content
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-filter1.jsonl",
        "filter_test_content",
        1732118400000, // Nov 20, 2024
    );
    make_codex_session(
        &codex_home,
        "2024/11/21",
        "rollout-filter2.jsonl",
        "filter_test_content",
        1732204800000, // Nov 21, 2024
    );

    // Index
    command_env
        .cass_assert_command()
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();

    // Search with agent filter
    let output = command_env
        .cass_assert_command()
        .args([
            "search",
            "filter_test_content",
            "--agent",
            "codex",
            "--robot",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", home)
        .output()
        .expect("search command");

    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    let hits = json.get("hits").and_then(|h| h.as_array()).expect("hits");

    // All hits should be from codex agent
    for hit in hits {
        assert_eq!(
            hit["agent"].as_str().unwrap(),
            "codex",
            "All hits should be from codex agent"
        );
    }
}

/// Test: Search returns total_matches and pagination info
#[test]
#[serial]
fn search_returns_pagination_info() {
    let tracker = tracker_for("search_returns_pagination_info");
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.to_path_buf();
    let command_env = tracker
        .command_environment()
        .with_home(home)
        .with_codex_home(&codex_home);
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    // Create multiple sessions
    for i in 1..=5 {
        make_codex_session(
            &codex_home,
            "2024/11/20",
            &format!("rollout-page{i}.jsonl"),
            "pagination_test_term",
            1732118400000 + (i as u64 * 1000),
        );
    }

    // Index
    command_env
        .cass_assert_command()
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();

    // Search with limit
    let output = command_env
        .cass_assert_command()
        .args([
            "search",
            "pagination_test_term",
            "--limit",
            "3",
            "--robot",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", home)
        .output()
        .expect("search command");

    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");

    // Verify pagination fields
    let total = json
        .get("total_matches")
        .and_then(|t| t.as_u64())
        .expect("total_matches");
    let limit = json.get("limit").and_then(|l| l.as_u64()).expect("limit");
    let hits = json
        .get("hits")
        .and_then(|h| h.as_array())
        .expect("hits")
        .len();

    // We created 5 sessions, each with 2 messages (user + response), so we expect >= 5 hits
    // But some may not match the search term exactly
    assert!(
        total >= 1,
        "Should have at least 1 total match. Got: {}",
        total
    );
    assert_eq!(limit, 3, "Limit should be 3");
    assert!(hits <= 3, "Returned hits should be <= limit");
}

/// Test: Force rebuild recreates index
#[test]
#[serial]
fn force_rebuild_recreates_index() {
    let tracker = tracker_for("force_rebuild_recreates_index");
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.to_path_buf();
    let command_env = tracker
        .command_environment()
        .with_home(home)
        .with_codex_home(&codex_home);
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    // Create initial data
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-rebuild.jsonl",
        "rebuild_test_initial",
        1732118400000,
    );

    // Initial index
    command_env
        .cass_assert_command()
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();

    // Bead cxhqb: capture a content-based fingerprint of the index
    // tree before the force-rebuild. Previously this compared a single
    // directory-mtime before/after with a 1-second sleep in between —
    // fragile on filesystems with ≥2s mtime granularity (FAT32, some
    // NFS setups) and wasteful wall-clock time on every test run.
    // Listing every file under the index tree with its size is
    // independent of filesystem mtime precision: a real rebuild writes
    // new Tantivy segments (new UUIDs, different sizes), so the set is
    // guaranteed to change even when mtime cannot.
    fn index_fingerprint(root: &Path) -> Vec<(String, u64)> {
        let mut entries: Vec<(String, u64)> = walkdir::WalkDir::new(root)
            .sort_by_file_name()
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
            .map(|e| {
                let rel = e
                    .path()
                    .strip_prefix(root)
                    .unwrap_or(e.path())
                    .to_string_lossy()
                    .into_owned();
                let len = e.metadata().map(|m| m.len()).unwrap_or(0);
                (rel, len)
            })
            .collect();
        entries.sort();
        entries
    }
    let index_dir = data_dir.join("index");
    let initial_fingerprint = index_fingerprint(&index_dir);
    assert!(
        !initial_fingerprint.is_empty(),
        "precondition: initial index tree at {} must contain files",
        index_dir.display()
    );

    // Force rebuild
    command_env
        .cass_assert_command()
        .args(["index", "--force-rebuild", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();

    // Verify index was rebuilt: the set of files (names + sizes) under
    // the index tree must differ from the pre-rebuild snapshot. A
    // regression where --force-rebuild silently no-ops would leave the
    // same Tantivy segments in place and this assertion would fire.
    let new_fingerprint = index_fingerprint(&index_dir);
    assert_ne!(
        initial_fingerprint,
        new_fingerprint,
        "index tree content must change after --force-rebuild; \
         before ({} entries) == after ({} entries)",
        initial_fingerprint.len(),
        new_fingerprint.len()
    );

    // Verify content is still searchable
    let output = command_env
        .cass_assert_command()
        .args(["search", "rebuild_test_initial", "--robot", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", home)
        .output()
        .expect("search command");

    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    let hits = json.get("hits").and_then(|h| h.as_array()).expect("hits");
    // Bead 7k7pl: pin EXACT content — at least one hit must carry
    // the seeded token `rebuild_test_initial` in its content field.
    // A regression that returned unrelated hits after force-rebuild
    // would slip past `!is_empty()` while breaking searchability.
    assert!(
        !hits.is_empty(),
        "Content should still be searchable after force-rebuild"
    );
    let seeded_hit = hits.iter().find(|hit| {
        hit.get("content")
            .and_then(|c| c.as_str())
            .is_some_and(|c| c.contains("rebuild_test_initial"))
    });
    assert!(
        seeded_hit.is_some(),
        "at least one hit must contain the seeded token `rebuild_test_initial`; \
         got hits={hits:?}"
    );
}

/// Test: JSON output mode (--json) for index command
#[test]
#[serial]
fn index_json_output_mode() {
    let tracker = tracker_for("index_json_output_mode");
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.to_path_buf();
    let command_env = tracker
        .command_environment()
        .with_home(home)
        .with_codex_home(&codex_home);
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    // Create fixture
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-json.jsonl",
        "json_output_test",
        1732118400000,
    );

    // Index with --json
    let output = command_env
        .cass_assert_command()
        .args(["index", "--full", "--json", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .output()
        .expect("index command");

    assert!(output.status.success());

    // Debug: print actual output
    eprintln!(
        "Index JSON output: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    // Verify JSON output structure - index --json outputs various fields
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid JSON output");

    // Index JSON output should be a valid JSON object
    assert!(
        json.is_object(),
        "JSON output should be an object. Got: {}",
        json
    );
}

/// Test: Help text includes expected options
#[test]
#[serial]
fn index_help_includes_options() {
    let tracker = tracker_for("index_help_includes_options");
    let command_env = tracker.command_environment();
    let output = command_env
        .cass_assert_command()
        .args(["index", "--help"])
        .output()
        .expect("help command");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(stdout.contains("--full"), "Help should mention --full");
    assert!(stdout.contains("--watch"), "Help should mention --watch");
    assert!(
        stdout.contains("--force-rebuild"),
        "Help should mention --force-rebuild"
    );
    assert!(
        stdout.contains("--semantic"),
        "Help should mention --semantic"
    );
    assert!(
        stdout.contains("--embedder"),
        "Help should mention --embedder"
    );
    assert!(
        stdout.contains("--data-dir"),
        "Help should mention --data-dir"
    );
}

/// Test: Search help includes expected options
#[test]
#[serial]
fn search_help_includes_options() {
    let tracker = tracker_for("search_help_includes_options");
    let command_env = tracker.command_environment();
    let output = command_env
        .cass_assert_command()
        .args(["search", "--help"])
        .output()
        .expect("help command");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(stdout.contains("--robot"), "Help should mention --robot");
    assert!(stdout.contains("--limit"), "Help should mention --limit");
    assert!(stdout.contains("--agent"), "Help should mention --agent");
    assert!(
        stdout.contains("--aggregate"),
        "Help should mention --aggregate"
    );
}

/// Test: Search with wildcard query
#[test]
#[serial]
fn search_wildcard_query() {
    let tracker = tracker_for("search_wildcard_query");
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.to_path_buf();
    let command_env = tracker
        .command_environment()
        .with_home(home)
        .with_codex_home(&codex_home);
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    // Create fixture with unique prefix
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-wild.jsonl",
        "wildcardtest_unique_suffix",
        1732118400000,
    );

    // Index
    command_env
        .cass_assert_command()
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();

    // Search with wildcard prefix
    let output = command_env
        .cass_assert_command()
        .args(["search", "wildcardtest*", "--robot", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", home)
        .output()
        .expect("search command");

    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    let hits = json.get("hits").and_then(|h| h.as_array()).expect("hits");

    // Bead 7k7pl: pin EXACT content — the wildcard `wildcardtest*`
    // must match the seeded token `wildcardtest_unique_suffix` in at
    // least one hit's content. A regression that returned unrelated
    // hits (wildcard falling back to match-all) would slip past
    // `!is_empty()`.
    assert!(
        !hits.is_empty(),
        "Wildcard prefix search should find results"
    );
    let matched_seed = hits.iter().any(|hit| {
        hit.get("content")
            .and_then(|c| c.as_str())
            .is_some_and(|c| c.contains("wildcardtest_unique_suffix"))
    });
    assert!(
        matched_seed,
        "wildcard search must return the seeded `wildcardtest_unique_suffix` token; \
         got hits={hits:?}"
    );
}

/// Test: Trace logging works when enabled
#[test]
#[serial]
fn trace_logging_to_file() {
    let tracker = tracker_for("trace_logging_to_file");
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.to_path_buf();
    let command_env = tracker
        .command_environment()
        .with_home(home)
        .with_codex_home(&codex_home);
    let data_dir = home.join("cass_data");
    let trace_dir = home.join("traces");
    fs::create_dir_all(&data_dir).unwrap();
    fs::create_dir_all(&trace_dir).unwrap();

    // Create fixture
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-trace.jsonl",
        "trace_test_content",
        1732118400000,
    );

    // Index with tracing enabled
    command_env
        .cass_assert_command()
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .env("CASS_TRACE_DIR", &trace_dir)
        .assert()
        .success();

    // Note: Trace file creation depends on tracing-appender setup in the binary
    // This test verifies the env var is recognized without crashing
}

/// Test: Empty query returns recent results
#[test]
#[serial]
fn empty_query_returns_recent() {
    let tracker = tracker_for("empty_query_returns_recent");
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.to_path_buf();
    let command_env = tracker
        .command_environment()
        .with_home(home)
        .with_codex_home(&codex_home);
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    // Create fixture
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-recent.jsonl",
        "recent_results_test",
        1732118400000,
    );

    // Index
    command_env
        .cass_assert_command()
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();

    // Search with empty query (should show recent)
    let output = command_env
        .cass_assert_command()
        .args(["search", "", "--robot", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", home)
        .output()
        .expect("search command");

    assert!(
        output.status.success(),
        "Empty query should succeed after a successful index: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("empty-query search JSON");
    let hits = json["hits"].as_array().expect("hits array");
    assert!(
        !hits.is_empty(),
        "Empty query should return recent indexed conversations"
    );
    // Bead 7k7pl: pin the SHAPE of every returned hit. Empty-query
    // returns recency-sorted conversations, so every hit must be a
    // proper hit object with string `content` + `source_path` fields.
    // A regression that emitted malformed hits (null content, missing
    // source) would slip past `!is_empty()` while breaking consumers.
    for hit in hits {
        assert!(
            hit.get("content").and_then(|c| c.as_str()).is_some(),
            "every empty-query hit must have a string `content` field; got {hit}"
        );
        assert!(
            hit.get("source_path").and_then(|s| s.as_str()).is_some(),
            "every empty-query hit must have a string `source_path` field; got {hit}"
        );
    }
}

#[test]
#[serial]
fn large_message_minimal_search_stays_on_the_tantivy_fast_path() {
    let tracker = tracker_for("large_message_minimal_search_stays_on_the_tantivy_fast_path");
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.to_path_buf();
    let command_env = tracker
        .command_environment()
        .with_home(home)
        .with_codex_home(&codex_home);
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let large_content = format!(
        "tantivy_large_anchor {}",
        "overflowpayload ".repeat(180_000)
    );

    tracker.phase(
        "seed_large_message_fixture",
        "Create a real Codex rollout with a multi-megabyte message body",
        || {
            make_codex_session(
                &codex_home,
                "2024/11/22",
                "rollout-large-tantivy-fast-path.jsonl",
                &large_content,
                1_732_300_000_000,
            );
        },
    );

    tracker.phase(
        "index_large_message_fixture",
        "Build the real index before searching the large message",
        || {
            command_env
                .cass_assert_command()
                .args(["index", "--full", "--json", "--data-dir"])
                .arg(&data_dir)
                .current_dir(home)
                .env("CODEX_HOME", &codex_home)
                .env("HOME", home)
                .timeout(Duration::from_secs(90))
                .assert()
                .success();
        },
    );

    let search_started = tracker.start(
        "search_large_message_minimal",
        Some("Run a real lexical cass search against the multi-megabyte session"),
    );
    let output = command_env
        .cass_assert_command()
        .args([
            "search",
            "tantivy_large_anchor",
            "--json",
            "--mode",
            "lexical",
            "--fields",
            "minimal",
            "--limit",
            "5",
            "--data-dir",
        ])
        .arg(&data_dir)
        .current_dir(home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .timeout(Duration::from_secs(20))
        .output()
        .expect("search large indexed message");
    tracker.end(
        "search_large_message_minimal",
        Some("Run a real lexical cass search against the multi-megabyte session"),
        search_started,
    );

    assert!(
        output.status.success(),
        "large-message lexical search should stay healthy after indexing\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("large-message search JSON");
    let hits = json
        .get("total_matches")
        .and_then(|matches| matches.as_u64())
        .unwrap_or_else(|| {
            json.get("hits")
                .and_then(|hits| hits.as_array())
                .map(|hits| hits.len() as u64)
                .unwrap_or(0)
        });
    assert!(
        hits > 0,
        "large indexed message should remain searchable with minimal lexical fields"
    );

    tracker.flush();
}

#[test]
#[serial]
fn incremental_index_repairs_sparse_tantivy_from_canonical_db_before_scanning_new_files() {
    let tracker = tracker_for(
        "incremental_index_repairs_sparse_tantivy_from_canonical_db_before_scanning_new_files",
    );
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path().to_path_buf();
    let codex_home = home.to_path_buf();
    let command_env = tracker
        .command_environment()
        .with_home(&home)
        .with_codex_home(&codex_home);
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    tracker.phase(
        "seed_baseline_archive",
        "Create a real multi-session Codex archive and build the canonical DB plus lexical index",
        || {
            make_bulk_codex_sessions(
                &codex_home,
                "2024/11/23",
                "rollout-repair-baseline",
                "repairoldanchor",
                1_732_400_000_000,
                5,
                4,
            );

            command_env
                .cass_assert_command()
                .args(["index", "--full", "--json", "--data-dir"])
                .arg(&data_dir)
                .current_dir(&home)
                .env("CODEX_HOME", &codex_home)
                .env("HOME", &home)
                .timeout(Duration::from_secs(60))
                .assert()
                .success();
        },
    );

    let db_path = data_dir.join("agent_search.db");
    let baseline_messages = count_messages(&db_path) as u64;
    assert!(
        baseline_messages >= 40,
        "baseline archive should populate the canonical DB with many messages"
    );

    tracker.phase(
        "swap_in_sparse_real_tantivy_index",
        "Replace the healthy lexical index with a real but sparse one built from a different archive",
        || {
            let sparse_home = home.join("sparse_home");
            let sparse_codex_home = sparse_home.clone();
            let sparse_data_dir = sparse_home.join("cass_data");
            fs::create_dir_all(&sparse_data_dir).unwrap();

            make_codex_session(
                &sparse_codex_home,
                "2024/11/23",
                "rollout-sparse-replacement.jsonl",
                "sparseanchoronly",
                1_732_450_000_000,
            );

            command_env
                .cass_assert_command()
                .args(["index", "--full", "--json", "--data-dir"])
                .arg(&sparse_data_dir)
                .current_dir(&sparse_home)
                .env("CODEX_HOME", &sparse_codex_home)
                .env("HOME", &sparse_home)
                .timeout(Duration::from_secs(60))
                .assert()
                .success();

            let live_index = expected_index_dir(&data_dir);
            let backup_name = live_index
                .file_name()
                .map(|name| format!("{}.baseline-backup", name.to_string_lossy()))
                .unwrap_or_else(|| "lexical-index.baseline-backup".to_string());
            let backup_index = live_index.with_file_name(backup_name);
            let sparse_index = expected_index_dir(&sparse_data_dir);
            fs::rename(&live_index, &backup_index).expect("move healthy index aside");
            fs::rename(&sparse_index, &live_index)
                .expect("replace healthy index with sparse real tantivy index");
        },
    );

    assert_eq!(
        raw_lexical_total_matches(&expected_index_dir(&data_dir), "repairoldanchor"),
        0,
        "the swapped-in sparse index should not contain the baseline token before repair; \
         use a raw lexical reader here so cass search cannot self-heal the fixture early"
    );

    tracker.phase(
        "stage_new_incremental_session",
        "Add a brand-new session after the sparse index swap so plain cass index must both repair and ingest",
        || {
            make_codex_session(
                &codex_home,
                "2024/11/24",
                "rollout-repair-new-session.jsonl",
                "repairnewanchor",
                1_732_500_000_000,
            );
        },
    );

    let repair_started = tracker.start(
        "repair_sparse_tantivy_then_incremental_scan",
        Some("Run plain cass index --json and require canonical repair plus new-session ingestion"),
    );
    let repair_output = command_env
        .cass_assert_command()
        .args(["index", "--json", "--data-dir"])
        .arg(&data_dir)
        .current_dir(&home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", &home)
        .timeout(Duration::from_secs(60))
        .output()
        .expect("run repairing incremental index");
    let repair_duration_ms = repair_started.elapsed().as_millis() as u64;
    tracker.end(
        "repair_sparse_tantivy_then_incremental_scan",
        Some("Run plain cass index --json and require canonical repair plus new-session ingestion"),
        repair_started,
    );

    assert!(
        repair_output.status.success(),
        "plain index should repair the sparse Tantivy index and ingest new sessions\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&repair_output.stdout),
        String::from_utf8_lossy(&repair_output.stderr)
    );

    let repair_json: serde_json::Value =
        serde_json::from_slice(&repair_output.stdout).expect("parse repair index json");
    let repair_stats = repair_json
        .get("indexing_stats")
        .and_then(|value| value.as_object())
        .expect("indexing_stats object");
    assert_eq!(
        repair_stats
            .get("lexical_strategy")
            .and_then(|value| value.as_str()),
        Some("deferred_authoritative_db_rebuild")
    );
    assert_eq!(
        repair_stats
            .get("lexical_strategy_reason")
            .and_then(|value| value.as_str()),
        Some(
            "incremental_index_repairs_sparse_tantivy_from_authoritative_canonical_db_before_scan"
        )
    );

    let after_messages = count_messages(&db_path) as u64;
    assert_eq!(
        after_messages,
        baseline_messages + 2,
        "plain incremental index should still ingest the newly added session after repairing Tantivy"
    );

    let repaired_old_search = command_env
        .cass_assert_command()
        .args([
            "search",
            "repairoldanchor",
            "--json",
            "--mode",
            "lexical",
            "--fields",
            "minimal",
            "--limit",
            "5",
            "--data-dir",
        ])
        .arg(&data_dir)
        .current_dir(&home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", &home)
        .timeout(Duration::from_secs(20))
        .output()
        .expect("search repaired baseline token");
    assert!(
        repaired_old_search.status.success(),
        "search should succeed after canonical lexical repair\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&repaired_old_search.stdout),
        String::from_utf8_lossy(&repaired_old_search.stderr)
    );
    assert!(
        total_matches_from_search_output(&repaired_old_search.stdout) > 0,
        "repair should restore baseline archive hits from the canonical DB"
    );

    let repaired_new_search = command_env
        .cass_assert_command()
        .args([
            "search",
            "repairnewanchor",
            "--json",
            "--mode",
            "lexical",
            "--fields",
            "minimal",
            "--limit",
            "5",
            "--data-dir",
        ])
        .arg(&data_dir)
        .current_dir(&home)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", &home)
        .timeout(Duration::from_secs(20))
        .output()
        .expect("search new incremental token after repair");
    assert!(
        repaired_new_search.status.success(),
        "new incremental content should be searchable after the repair-first index run\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&repaired_new_search.stdout),
        String::from_utf8_lossy(&repaired_new_search.stderr)
    );
    assert!(
        total_matches_from_search_output(&repaired_new_search.stdout) > 0,
        "repair-first incremental index should still ingest the newly added session"
    );

    tracker.metrics(
        "repair_sparse_tantivy_then_incremental_scan",
        &E2ePerformanceMetrics::new()
            .with_duration(repair_duration_ms)
            .with_custom("baseline_messages", baseline_messages)
            .with_custom("messages_after_repair", after_messages),
    );
    tracker.flush();
}
