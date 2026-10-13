//! GH521: timeline must not materialize unrelated message payloads.
//!
//! Small fixtures exercise the real CLI, including legacy nullable metadata.
//! The opt-in Linux RSS test adds 256 MiB of unrelated message bodies with
//! bounded fixture construction, then supervises fresh CLI processes. Run with:
//! cargo test --test timeline_memory_regression -- --include-ignored --test-threads=1

use coding_agent_search::franken_sync::compat::ConnectionExt;
use coding_agent_search::franken_sync::{Connection, params};
use serde_json::{Value, json};
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::time::Duration;
use tempfile::TempDir;

const START: i64 = 1_735_819_200_000; // 2025-01-02T12:00:00Z
const END: i64 = START + 3_600_000;
const WINDOW: [&str; 4] = [
    "--since",
    "2025-01-02T12:00:00Z",
    "--until",
    "2025-01-02T13:00:00Z",
];

fn fixture(root: &Path) {
    let conn = Connection::open(root.join("agent_search.db").to_string_lossy().as_ref()).unwrap();
    // A legacy archive deliberately has no analytics rollups or count cache.
    // Timeline must remain a read-only metadata query against canonical rows.
    conn.execute_batch(
        "CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT);
         INSERT INTO meta VALUES('schema_version', '20');
         CREATE TABLE agents(id INTEGER PRIMARY KEY, slug TEXT);
         INSERT INTO agents VALUES(1, 'claude_code'), (2, 'codex');
         CREATE TABLE sources(id TEXT PRIMARY KEY, kind TEXT);
         INSERT INTO sources VALUES
             ('local', 'local'), ('work', 'ssh'), ('server', 'ssh'),
             ('   ', 'ssh'), ('  local  ', 'local');
         CREATE TABLE conversations(
             id INTEGER PRIMARY KEY, agent_id INTEGER, title TEXT,
             started_at INTEGER, ended_at INTEGER, source_path TEXT,
             source_id TEXT, origin_host TEXT
         );
         CREATE INDEX idx_conversations_started_at ON conversations(started_at);
         CREATE TABLE messages(
             id INTEGER PRIMARY KEY, conversation_id INTEGER NOT NULL,
             idx INTEGER NOT NULL, role TEXT, content TEXT, created_at INTEGER,
             UNIQUE(conversation_id, idx)
         );
         CREATE INDEX idx_messages_conversation ON messages(conversation_id);",
    )
    .unwrap();
    // id, agent, start, source, host, message count.
    let sessions = [
        (1, Some(1), Some(START - 1), Some("local"), None, 5),
        (2, Some(1), Some(START), Some("local"), None, 3),
        (3, Some(2), Some(START + 1000), Some("local"), None, 2),
        (
            4,
            Some(1),
            Some(START + 2000),
            Some("work"),
            Some("work-host"),
            4,
        ),
        (
            5,
            Some(2),
            Some(END),
            Some("server"),
            Some("server-host"),
            1,
        ),
        (6, Some(2), Some(END + 1), Some("local"), None, 3),
        (7, None, Some(START + 3000), None, None, 0),
        (
            8,
            Some(1),
            Some(START + 4000),
            Some("   "),
            Some("remote-host"),
            2,
        ),
        (9, Some(1), None, Some("local"), None, 2),
        (10, Some(2), Some(START + 5000), Some("  local  "), None, 0),
        (11, None, Some(START + 6000), Some("orphan"), None, 1),
    ];
    conn.execute("BEGIN IMMEDIATE").unwrap();
    for (id, agent, started, source, host, count) in sessions {
        let title = (id != 7).then(|| format!("session-{id} café"));
        conn.execute_compat(
            "INSERT INTO conversations VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                id,
                agent,
                title,
                started,
                END + 60_000,
                format!("/timeline/{id}.jsonl"),
                source,
                host
            ],
        )
        .unwrap();
        for idx in 0..count {
            // Counts include messages outside the requested window too.
            let at = if idx % 2 == 0 {
                START - 86_400_000
            } else {
                END + 86_400_000
            };
            conn.execute_compat(
                "INSERT INTO messages(conversation_id, idx, role, content, created_at)
                 VALUES(?1, ?2, 'user', 'message outside window', ?3)",
                params![id, idx, at],
            )
            .unwrap();
        }
    }
    conn.execute("COMMIT").unwrap();
    conn.close().unwrap();
}

fn command(root: &Path, window: &[&str], extra: &[&str], json_output: bool) -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    cmd.env("HOME", root)
        .env("XDG_DATA_HOME", root)
        .env("XDG_CONFIG_HOME", root)
        .env("CASS_DATA_DIR", root)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("NO_COLOR", "1")
        .env_remove("CASS_DB_PATH")
        .env_remove("CASS_CONFIG_PATH")
        .env_remove("CASS_ROBOT_FORMAT")
        .arg("timeline")
        .arg("--data-dir")
        .arg(root)
        .args(window);
    if json_output {
        cmd.arg("--json");
    }
    cmd.args(extra);
    cmd
}

fn run(root: &Path, extra: &[&str]) -> Value {
    run_window(root, &WINDOW, extra)
}

fn run_window(root: &Path, window: &[&str], extra: &[&str]) -> Value {
    let mut cmd = assert_cmd::Command::from_std(command(root, window, extra, true));
    let out = cmd
        .timeout(Duration::from_secs(30))
        .assert()
        .success()
        .get_output()
        .clone();
    serde_json::from_slice(&out.stdout).expect("timeline JSON")
}

fn ids(value: &Value) -> Vec<i64> {
    value["sessions"]
        .as_array()
        .expect("ungrouped sessions")
        .iter()
        .map(|row| row["id"].as_i64().expect("session id"))
        .collect()
}

#[test]
fn timeline_boundaries_counts_nulls_and_readonly_contract() {
    let root = TempDir::new().unwrap();
    fixture(root.path());
    let before = fs::read(root.path().join("agent_search.db")).unwrap();
    let value = run(root.path(), &["--group-by", "none"]);
    assert_eq!(value["range"], json!({"start": START, "end": END}));
    assert_eq!(value["total_sessions"], 8);
    assert_eq!(ids(&value), vec![5, 11, 10, 8, 7, 4, 3, 2]);
    let counts: Vec<i64> = value["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["message_count"].as_i64().unwrap())
        .collect();
    assert_eq!(counts, vec![1, 1, 0, 2, 0, 4, 2, 3]);
    let legacy = &value["sessions"][4];
    assert_eq!(legacy["agent"], "unknown");
    assert!(legacy["title"].is_null());
    assert_eq!(legacy["source_id"], "local");
    assert_eq!(legacy["origin_kind"], "local");
    assert!(legacy["origin_host"].is_null());
    assert_eq!(value["sessions"][3]["source_id"], "remote-host");
    assert_eq!(value["sessions"][3]["origin_kind"], "remote");
    assert_eq!(value["sessions"][7]["duration_seconds"], 3660);
    assert_eq!(
        fs::read(root.path().join("agent_search.db")).unwrap(),
        before
    );
}

#[test]
fn timeline_agent_and_source_filters_compose_without_dropping_zero_counts() {
    let root = TempDir::new().unwrap();
    fixture(root.path());
    let cases: Vec<(Vec<&str>, Vec<i64>)> = vec![
        (vec!["--agent", "claude_code"], vec![8, 4, 2]),
        (vec!["--agent", "codex"], vec![5, 10, 3]),
        (vec!["--agent", "missing"], vec![]),
        (vec!["--source", "local"], vec![10, 7, 3, 2]),
        (vec!["--source", "remote"], vec![5, 11, 8, 4]),
        (vec!["--source", "work"], vec![4]),
        (vec!["--source", "remote-host"], vec![8]),
        (vec!["--source", "missing"], vec![]),
        (
            vec!["--agent", "claude_code", "--source", "remote"],
            vec![8, 4],
        ),
        (vec!["--agent", "codex", "--source", "local"], vec![10, 3]),
        (
            vec!["--agent", "claude_code", "--agent", "codex"],
            vec![5, 10, 8, 4, 3, 2],
        ),
    ];
    for (mut args, expected) in cases {
        args.extend(["--group-by", "none"]);
        let value = run(root.path(), &args);
        assert_eq!(ids(&value), expected, "filters: {args:?}");
        assert_eq!(
            value["total_sessions"].as_u64().unwrap(),
            expected.len() as u64
        );
    }
}

#[test]
fn timeline_relative_and_today_windows_apply_agent_and_source_filters() {
    use chrono::{Local, TimeZone, Utc};

    let root = TempDir::new().unwrap();
    fixture(root.path());
    let now = Utc::now().timestamp_millis();
    let conn = Connection::open(
        root.path()
            .join("agent_search.db")
            .to_string_lossy()
            .as_ref(),
    )
    .unwrap();
    conn.execute("BEGIN IMMEDIATE").unwrap();
    conn.execute("UPDATE conversations SET started_at = NULL")
        .unwrap();
    let recent = [(2_i64, now - 60_000), (3, now - 90_000), (4, now - 120_000)];
    for (id, started) in recent {
        conn.execute_compat(
            "UPDATE conversations SET started_at = ?1 WHERE id = ?2",
            params![started, id],
        )
        .unwrap();
    }
    // Neither old nor future sessions belong in these relative windows.
    for (id, started) in [(5_i64, now - 7 * 86_400_000), (6, now + 3_600_000)] {
        conn.execute_compat(
            "UPDATE conversations SET started_at = ?1 WHERE id = ?2",
            params![started, id],
        )
        .unwrap();
    }
    conn.execute("COMMIT").unwrap();
    conn.close().unwrap();

    let cases: Vec<(Vec<&str>, Vec<i64>)> = vec![
        (vec![], vec![2, 3, 4]),
        (vec!["--agent", "claude_code"], vec![2, 4]),
        (vec!["--source", "local"], vec![2, 3]),
        (vec!["--source", "remote"], vec![4]),
        (vec!["--agent", "claude_code", "--source", "work"], vec![4]),
    ];
    for window in [vec!["--since", "5m"], vec!["--today"]] {
        for (filter, eligible) in &cases {
            let mut args = filter.clone();
            args.extend(["--group-by", "none"]);
            let before = Local::now();
            let value = run_window(root.path(), &window, &args);
            let after = Local::now();
            let start = value["range"]["start"].as_i64().unwrap();
            let end = value["range"]["end"].as_i64().unwrap();
            assert!((before.timestamp_millis()..=after.timestamp_millis()).contains(&end));
            if window == ["--since", "5m"] {
                // Relative parsing samples its own clock, after the default end.
                assert!(start >= before.timestamp_millis() - 300_000);
                assert!(start <= after.timestamp_millis() - 300_000);
            } else {
                let midnight = |at: chrono::DateTime<Local>| {
                    Local
                        .from_local_datetime(&at.date_naive().and_hms_opt(0, 0, 0).unwrap())
                        .earliest()
                        .map(|at| at.timestamp_millis())
                        .unwrap_or_else(|| {
                            at.date_naive()
                                .and_hms_opt(0, 0, 0)
                                .unwrap()
                                .and_utc()
                                .timestamp_millis()
                        })
                };
                // Accept either day if the subprocess runs across midnight.
                assert!(start == midnight(before) || start == midnight(after));
            }
            // Near midnight, today's range may exclude one of the recent rows.
            // Expected membership comes from known fixture timestamps, not output rows.
            let expected: Vec<i64> = recent
                .iter()
                .filter(|(id, at)| eligible.contains(id) && *at >= start && *at <= end)
                .map(|(id, _)| *id)
                .collect();
            assert_eq!(
                ids(&value),
                expected,
                "window={window:?}, filters={filter:?}"
            );
            assert_eq!(
                value["total_sessions"].as_u64().unwrap(),
                expected.len() as u64
            );
            for row in value["sessions"].as_array().unwrap() {
                let count = match row["id"].as_i64().unwrap() {
                    2 => 3,
                    3 => 2,
                    4 => 4,
                    other => panic!("unexpected session {other}"),
                };
                assert_eq!(row["message_count"], count);
            }
        }
    }
}

#[test]
fn timeline_equal_start_times_do_not_drop_or_duplicate_sessions() {
    let root = TempDir::new().unwrap();
    fixture(root.path());
    let conn = Connection::open(
        root.path()
            .join("agent_search.db")
            .to_string_lossy()
            .as_ref(),
    )
    .unwrap();
    conn.execute_compat(
        "UPDATE conversations SET started_at = ?1 WHERE id = 3",
        params![START],
    )
    .unwrap();
    conn.close().unwrap();
    let value = run(root.path(), &["--group-by", "none"]);
    let rows = value["sessions"].as_array().unwrap();
    assert_eq!(rows.len(), 8);
    assert!(rows.windows(2).all(|pair| {
        pair[0]["started_at"].as_i64().unwrap() >= pair[1]["started_at"].as_i64().unwrap()
    }));
    let mut tied: Vec<i64> = rows[6..]
        .iter()
        .map(|row| row["id"].as_i64().unwrap())
        .collect();
    tied.sort_unstable();
    assert_eq!(tied, vec![2, 3]);
}

#[test]
fn timeline_grouping_and_human_output_preserve_session_order_and_totals() {
    let root = TempDir::new().unwrap();
    fixture(root.path());
    for group in ["day", "hour"] {
        let value = run(root.path(), &["--group-by", group]);
        assert_eq!(value["total_sessions"], 8);
        let groups = value["groups"].as_object().unwrap();
        let mut all_ids = Vec::new();
        for entries in groups.values() {
            let entries = entries.as_array().unwrap();
            assert!(entries.windows(2).all(|rows| {
                rows[0]["started_at"].as_i64().unwrap() >= rows[1]["started_at"].as_i64().unwrap()
            }));
            all_ids.extend(entries.iter().map(|row| row["id"].as_i64().unwrap()));
        }
        all_ids.sort_unstable();
        assert_eq!(all_ids, vec![2, 3, 4, 5, 7, 8, 10, 11]);
        if group == "hour" {
            assert_eq!(groups["2025-01-02 13:00"][0]["id"], 5);
        }
    }
    let mut cmd = assert_cmd::Command::from_std(command(
        root.path(),
        &WINDOW,
        &["--group-by", "none"],
        false,
    ));
    let out: Output = cmd
        .timeout(Duration::from_secs(30))
        .assert()
        .success()
        .get_output()
        .clone();
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("Total: 8 sessions"));
    assert!(text.contains("(untitled)"));
    assert!(text.contains("[work-host]"));
    assert!(!text.contains("[local]"));
}

#[cfg(target_os = "linux")]
mod memory {
    use super::*;
    use std::process::{Child, Stdio};
    use std::time::Instant;

    const RSS_LIMIT_KIB: u64 = 512 * 1024;
    const RSS_GROWTH_LIMIT_KIB: u64 = 128 * 1024;

    struct ChildGuard(Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            if self.0.try_wait().ok().flatten().is_none() {
                let _ = self.0.kill();
            }
            let _ = self.0.wait();
        }
    }

    fn measured(root: &Path, window: &[&str], extra: &[&str]) -> (Value, u64) {
        let output_path = root.join("timeline-memory-output.json");
        let error_path = root.join("timeline-memory-stderr.txt");
        let mut cmd = command(root, window, extra, true);
        cmd.stdout(Stdio::from(fs::File::create(&output_path).unwrap()))
            .stderr(Stdio::from(fs::File::create(&error_path).unwrap()));
        let mut child = ChildGuard(cmd.spawn().unwrap());
        let started = Instant::now();
        let mut peak_kib = 0;
        let status = loop {
            if let Ok(status) = fs::read_to_string(format!("/proc/{}/status", child.0.id())) {
                for line in status.lines().filter(|line| line.starts_with("VmHWM:")) {
                    let kib = line
                        .split_whitespace()
                        .nth(1)
                        .unwrap()
                        .parse::<u64>()
                        .unwrap();
                    peak_kib = peak_kib.max(kib);
                }
            }
            assert!(
                peak_kib <= RSS_LIMIT_KIB,
                "timeline exceeded RSS budget: {peak_kib} KiB, args={extra:?}"
            );
            if let Some(status) = child.0.try_wait().unwrap() {
                break status;
            }
            assert!(
                started.elapsed() < Duration::from_secs(90),
                "timeline timed out: {extra:?}"
            );
            std::thread::sleep(Duration::from_millis(2));
        };
        assert!(
            status.success(),
            "timeline failed: {}",
            fs::read_to_string(error_path).unwrap()
        );
        assert!(
            peak_kib > 0,
            "RSS sampling must observe the actual child, not report zero"
        );
        let result = serde_json::from_slice(&fs::read(output_path).unwrap()).unwrap();
        eprintln!(
            "GH521: window={window:?} args={extra:?} peak_rss_kib={peak_kib} elapsed_ms={}",
            started.elapsed().as_millis()
        );
        (result, peak_kib)
    }

    fn add_unrelated_archive(root: &Path) {
        let conn =
            Connection::open(root.join("agent_search.db").to_string_lossy().as_ref()).unwrap();
        // Exactly 12,667 total conversations. The first 4,096 old sessions
        // each have 32 messages, for 131,072 unrelated 2-KiB message bodies.
        // No Vec of messages, payloads, or conversation objects is built.
        let payload = "x".repeat(2048);
        // Each statement remains bounded to one session's 32 messages. Reuse
        // the payload parameter rather than parsing 131,072 separate INSERTs.
        let message_values = (0..32)
            .map(|idx| format!("(?1, {idx}, 'assistant', ?2, ?3)"))
            .collect::<Vec<_>>()
            .join(", ");
        let insert_messages = format!(
            "INSERT INTO messages(conversation_id, idx, role, content, created_at) VALUES {message_values}"
        );
        for batch in 0..127_i64 {
            conn.execute("BEGIN IMMEDIATE").unwrap();
            for offset in 0..100_i64 {
                let id = 12 + batch * 100 + offset;
                if id > 12_667 {
                    break;
                }
                conn.execute_compat(
                    "INSERT INTO conversations(id, agent_id, title, started_at, source_path, source_id)
                     VALUES(?1, 2, 'unrelated', ?2, ?3, 'local')",
                    params![id, START - 86_400_000, format!("/old/{id}.jsonl")],
                )
                .unwrap();
                if id < 12 + 4096 {
                    conn.execute_compat(
                        &insert_messages,
                        params![id, payload.as_str(), START - 86_400_000],
                    )
                    .unwrap();
                }
            }
            conn.execute("COMMIT").unwrap();
        }
        conn.close().unwrap();
    }

    #[test]
    #[ignore = "Linux RSS integration gate: builds an archive with >256 MiB of message payloads"]
    fn timeline_rss_is_bounded_when_unrelated_archive_grows() {
        let root = TempDir::new().unwrap();
        fixture(root.path());
        let (before, baseline_kib) = measured(root.path(), &WINDOW, &["--group-by", "none"]);
        add_unrelated_archive(root.path());
        for filter in [
            vec![],
            vec!["--agent", "claude_code"],
            vec!["--source", "local"],
            vec!["--source", "remote"],
            vec!["--source", "work", "--agent", "claude_code"],
            vec!["--source", "missing"],
        ] {
            let mut args = filter.clone();
            args.extend(["--group-by", "none"]);
            let (after, peak_kib) = measured(root.path(), &WINDOW, &args);
            assert!(
                peak_kib <= baseline_kib + RSS_GROWTH_LIMIT_KIB,
                "unrelated payloads increased RSS by {} KiB (baseline={baseline_kib}, peak={peak_kib})",
                peak_kib.saturating_sub(baseline_kib)
            );
            if filter.is_empty() {
                assert_eq!(
                    after, before,
                    "unrelated archive growth must not change narrow results"
                );
            } else {
                let expected = match filter.as_slice() {
                    ["--agent", "claude_code"] => vec![8, 4, 2],
                    ["--source", "local"] => vec![10, 7, 3, 2],
                    ["--source", "remote"] => vec![5, 11, 8, 4],
                    ["--source", "work", "--agent", "claude_code"] => vec![4],
                    ["--source", "missing"] => vec![],
                    _ => unreachable!(),
                };
                assert_eq!(ids(&after), expected);
            }
        }
        // The same plan must also handle an intentionally broad result set;
        // no session pagination or truncation may be used to satisfy the RSS gate.
        let (wide, peak_kib) = measured(
            root.path(),
            &[
                "--since",
                "2025-01-01T00:00:00Z",
                "--until",
                "2025-01-03T00:00:00Z",
            ],
            &["--group-by", "none"],
        );
        assert!(peak_kib <= baseline_kib + RSS_GROWTH_LIMIT_KIB);
        assert_eq!(wide["total_sessions"], 12_666);
        let rows = wide["sessions"].as_array().unwrap();
        assert_eq!(rows.len(), 12_666); // Only the NULL started_at session is excluded.
        assert!(rows.windows(2).all(|pair| {
            pair[0]["started_at"].as_i64().unwrap() >= pair[1]["started_at"].as_i64().unwrap()
        }));
        let message_count: i64 = rows
            .iter()
            .map(|row| row["message_count"].as_i64().unwrap())
            .sum();
        assert_eq!(message_count, 131_072 + 21);
    }
}
