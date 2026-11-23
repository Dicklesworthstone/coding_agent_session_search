//! GH520: exercise configured subtree mirrors through the real CASS binary.
//!
//! Connector-only tests cannot catch CASS routing, persistence, or search
//! regressions. These fixtures use actual safe-name containers, without SSH,
//! installed agents, HOME mutation in this process, or workaround symlinks.
#![cfg(target_os = "linux")]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use coding_agent_search::franken_sync::compat::{ConnectionExt, RowExt};
use coding_agent_search::sources::sync::path_to_safe_dirname;
use coding_agent_search::storage::sqlite::FrankenStorage;
use serde_json::{Value, json};

mod util;
use util::e2e_log::PhaseTracker;

const SOURCE_ID: &str = "mirror-subtrees";
const HOST: &str = "operator@mirror-subtrees";

#[derive(Debug, PartialEq, Eq)]
struct Archived {
    id: i64,
    agent: String,
    external_id: String,
    path: PathBuf,
    source: String,
    host: Option<String>,
}

type Messages = Vec<(i64, i64, String, String)>;

fn snapshot(data: &Path) -> (Vec<Archived>, Messages) {
    let storage = FrankenStorage::open_readonly(&data.join("agent_search.db")).unwrap();
    let conversations = storage
        .raw()
        .query(
            "SELECT c.id, a.slug, c.external_id, c.source_path, c.source_id, c.origin_host \
             FROM conversations c JOIN agents a ON a.id = c.agent_id ORDER BY c.id",
        )
        .unwrap()
        .into_iter()
        .map(|row| Archived {
            id: row.get_typed(0).unwrap(),
            agent: row.get_typed(1).unwrap(),
            external_id: row.get_typed(2).unwrap(),
            path: PathBuf::from(row.get_typed::<String>(3).unwrap()),
            source: row.get_typed(4).unwrap(),
            host: row.get_typed(5).unwrap(),
        })
        .collect();
    let messages = storage
        .raw()
        .query(
            "SELECT conversation_id, idx, role, content FROM messages ORDER BY conversation_id, idx",
        )
        .unwrap()
        .into_iter()
        .map(|row| {
            (
                row.get_typed(0).unwrap(),
                row.get_typed(1).unwrap(),
                row.get_typed(2).unwrap(),
                row.get_typed(3).unwrap(),
            )
        })
        .collect();
    (conversations, messages)
}

fn transcript(agent: &str, id: &str) -> Vec<u8> {
    let messages = [
        ("user", format!("{agent}subtreeprobe {id} question")),
        ("assistant", format!("{agent} {id} answer")),
    ];
    if agent == "gemini" {
        return serde_json::to_vec(&json!({
            "sessionId": id,
            "projectHash": "project",
            "messages": messages.iter().enumerate().map(|(index, (role, content))| {
                let time = 1_700_000_000_000_i64 + i64::try_from(index).unwrap() * 1000;
                json!({"type": if *role == "user" { "user" } else { "gemini" },
                       "content": content, "timestamp": time})
            }).collect::<Vec<_>>()
        }))
        .unwrap();
    }
    let mut rows = Vec::new();
    if agent == "factory" {
        rows.push(json!({"type": "session_start", "id": id, "cwd": "/remote/project"}));
    } else if agent == "prime_agent" {
        rows.push(json!({"type": "session", "version": 3, "id": id,
                         "timestamp": "2023-11-14T22:13:20Z", "cwd": "/remote/project"}));
    }
    for (index, (role, content)) in messages.into_iter().enumerate() {
        let time = 1_700_000_000_000_i64 + i64::try_from(index).unwrap() * 1000;
        rows.push(if agent == "vibe" {
            json!({"role": role, "content": content, "timestamp": time})
        } else {
            json!({"type": "message", "timestamp": time,
                   "message": {"role": role, "content": content}})
        });
    }
    (rows
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n")
        .into_bytes()
}

fn cursor_database(path: &Path, id: &str) {
    let conn = coding_agent_search::franken_sync::Connection::open(path.to_string_lossy().as_ref())
        .unwrap();
    conn.execute("CREATE TABLE cursorDiskKV (key TEXT PRIMARY KEY, value TEXT)")
        .unwrap();
    conn.execute("CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value TEXT)")
        .unwrap();
    let key = format!("composerData:{id}");
    let value = json!({
        "createdAt": 1_700_000_000_000_i64,
        "lastUpdatedAt": 1_700_000_001_000_i64,
        "tabs": [{"bubbles": [
            {"type": 1, "text": format!("cursorsubtreeprobe {id} question")},
            {"type": 2, "text": format!("cursor {id} answer")}
        ]}]
    })
    .to_string();
    conn.execute_compat(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?1, ?2)",
        coding_agent_search::franken_sync::params![key.as_str(), value.as_str()],
    )
    .unwrap();
}

fn exercise(test_name: &str, trailing_slash: bool, retain_basename: bool) {
    let tracker = PhaseTracker::new("e2e_remote_mirror_subtrees", test_name);
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let config = tmp.path().join("config");
    let data = tmp.path().join("data");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(config.join("cass")).unwrap();
    fs::write(home.join(".env"), "").unwrap();
    let mirror = data.join("remotes").join(SOURCE_ID).join("mirror");
    let cases = [
        (
            "factory",
            "~/.factory/sessions/project-a",
            "nested/session.jsonl",
            "factory-subtree",
        ),
        (
            "prime_agent",
            "~/.prime/agent/sessions/project-a",
            "nested/session.jsonl",
            "prime-subtree",
        ),
        (
            "vibe",
            "~/.vibe/logs/session/team-a/run",
            "messages.jsonl",
            "team-a/run",
        ),
        (
            "vibe",
            "~/.vibe/logs/session/team-b/run",
            "messages.jsonl",
            "team-b/run",
        ),
        (
            "gemini",
            "~/.gemini/tmp",
            "project/chats/session-test.json",
            "gemini-subtree",
        ),
        (
            "cursor",
            "~/.config/Cursor/User/workspaceStorage",
            "a/state.vscdb",
            "cursor-a",
        ),
        (
            "cursor",
            "~/.config/Cursor/User/workspaceStorage",
            "b/state.vscdb",
            "cursor-b",
        ),
        (
            "cursor",
            "~/.config/Cursor/User/workspaceStorage/selected",
            "state.vscdb",
            "cursor-selected",
        ),
    ];
    let mut paths = BTreeSet::new();
    let mut expected_paths = BTreeMap::new();
    let mut before = BTreeMap::new();
    let mut appended_file = None;
    for (agent, source, relative, id) in cases {
        let configured = format!("{source}{}", if trailing_slash { "/" } else { "" });
        let container = mirror.join(path_to_safe_dirname(&configured));
        let payload = if retain_basename {
            container.join(Path::new(source).file_name().unwrap())
        } else {
            container
        };
        let file = payload.join(relative);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        if agent == "cursor" {
            cursor_database(&file, id);
        } else {
            fs::write(&file, transcript(agent, id)).unwrap();
        }
        let conversation_path = if agent == "cursor" {
            file.join(id)
        } else {
            file.clone()
        };
        assert!(
            expected_paths
                .insert(conversation_path, (agent, id))
                .is_none()
        );
        before.insert(file.clone(), fs::read(&file).unwrap());
        if id == "team-a/run" {
            appended_file = Some(file);
        }
        paths.insert(configured);
    }
    fs::write(
        config.join("cass/sources.toml"),
        format!(
            "[[sources]]\nname = {SOURCE_ID:?}\ntype = \"ssh\"\nhost = {HOST:?}\npaths = {}\n",
            serde_json::to_string(&paths).unwrap()
        ),
    )
    .unwrap();

    let run = |args: &[&str]| {
        let mut command = tracker.cass_std_command();
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", &config)
            .env("XDG_DATA_HOME", tmp.path().join("xdg"))
            .env("CASS_DATA_DIR", &data)
            .env("CASS_AUTO_REFRESH", "0")
            .env("RUST_MIN_STACK", "134217728")
            .current_dir(&home);
        tracker.command_environment().apply_to_std(&mut command);
        let output = assert_cmd::Command::from_std(command)
            .args(args)
            .timeout(Duration::from_secs(120))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };

    run(&["index", "--full", "--json"]);
    let initial = snapshot(&data);
    assert_eq!(initial.0.len(), cases.len(), "{:?}", initial.0);
    assert_eq!(initial.1.len(), cases.len() * 2);
    let mut identities = BTreeSet::new();
    for row in &initial.0 {
        let (agent, fixture_id) = expected_paths
            .get(&row.path)
            .copied()
            .expect("conversation escaped its configured payload");
        assert_eq!(row.agent, agent);
        assert_eq!(row.source, SOURCE_ID);
        assert_eq!(row.host.as_deref(), Some(HOST));
        assert!(!row.external_id.is_empty());
        assert!(identities.insert((&row.agent, &row.external_id)));
        let actual_messages: Vec<_> = initial
            .1
            .iter()
            .filter(|message| message.0 == row.id)
            .collect();
        assert_eq!(actual_messages.len(), 2);
        assert_eq!(
            (actual_messages[0].1, actual_messages[0].2.as_str()),
            (0, "user")
        );
        assert_eq!(
            actual_messages[0].3,
            format!("{agent}subtreeprobe {fixture_id} question")
        );
        // The archive stores MessageRole::Agent as "agent".
        assert_eq!(
            (actual_messages[1].1, actual_messages[1].2.as_str()),
            (1, "agent")
        );
        assert_eq!(actual_messages[1].3, format!("{agent} {fixture_id} answer"));
    }
    for (agent, count) in [
        ("factory", 1),
        ("prime_agent", 1),
        ("vibe", 2),
        ("gemini", 1),
        ("cursor", 3),
    ] {
        let marker = format!("{agent}subtreeprobe");
        let output = run(&[
            "search",
            &marker,
            "--robot",
            "--mode",
            "lexical",
            "--no-daemon",
        ]);
        let result: Value = serde_json::from_slice(&output).unwrap();
        // Count sessions, not hits: a conversation whose title is its first
        // question (factory) also matches through that title on its answer.
        let conversations = result["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|hit| hit["conversation_id"].as_i64().unwrap())
            .collect::<BTreeSet<_>>();
        assert_eq!(conversations.len(), count, "{marker}: {result}");
    }
    run(&["index", "--json"]);
    assert_eq!(
        snapshot(&data),
        initial,
        "incremental indexing rekeyed or duplicated rows"
    );
    for (file, bytes) in &before {
        assert_eq!(
            fs::read(file).unwrap(),
            *bytes,
            "indexing modified {}",
            file.display()
        );
    }

    // An appended selected session must update its existing conversation, not
    // create a new empty/basename identity or be skipped as an already-seen root.
    let file = appended_file.unwrap();
    let mut bytes = before[&file].clone();
    bytes.extend_from_slice(
        b"{\"role\":\"user\",\"content\":\"vibeappendprobe\",\"timestamp\":1700000002000}\n",
    );
    fs::write(&file, &bytes).unwrap();
    run(&["index", "--json"]);
    let updated = snapshot(&data);
    assert_eq!(updated.0, initial.0);
    assert_eq!(updated.1.len(), initial.1.len() + 1);
    let selected_id = initial.0.iter().find(|row| row.path == file).unwrap().id;
    let added = (
        selected_id,
        2,
        "user".to_owned(),
        "vibeappendprobe".to_owned(),
    );
    let mut expected_messages = initial.1;
    expected_messages.push(added);
    expected_messages.sort();
    assert_eq!(updated.1, expected_messages);
    let output = run(&[
        "search",
        "vibeappendprobe",
        "--robot",
        "--mode",
        "lexical",
        "--no-daemon",
    ]);
    let hits: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(hits["hits"].as_array().unwrap().len(), 1);
    assert_eq!(fs::read(&file).unwrap(), bytes);
    tracker.complete();
}

#[test]
fn basename_preserving_subtrees_index_search_and_update() {
    exercise(
        "basename_preserving_subtrees_index_search_and_update",
        false,
        true,
    );
}

#[test]
fn trailing_slash_with_retained_basename_indexes_subtrees() {
    exercise(
        "trailing_slash_with_retained_basename_indexes_subtrees",
        true,
        true,
    );
}

#[test]
fn rsync_contents_only_subtrees_index_search_and_update() {
    exercise(
        "rsync_contents_only_subtrees_index_search_and_update",
        true,
        false,
    );
}

fn gh574_session(id: &str) -> Vec<u8> {
    [
        json!({"type": "session", "version": 3, "id": id,
               "timestamp": "2023-11-14T22:13:20Z", "cwd": "/remote/project"}),
        json!({"type": "message", "timestamp": "2023-11-14T22:13:21Z",
               "message": {"role": "user", "content": format!("{id} question")}}),
        json!({"type": "message", "timestamp": "2023-11-14T22:13:22Z",
               "message": {"role": "assistant", "content": format!("{id} answer")}}),
    ]
    .into_iter()
    .map(|entry| entry.to_string() + "\n")
    .collect::<String>()
    .into_bytes()
}

fn gh574_preserve_old_mtime(path: &Path) {
    let old_time = std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    fs::File::open(path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(old_time))
        .unwrap();
    assert_eq!(fs::metadata(path).unwrap().modified().unwrap(), old_time);
}

fn gh574_assert_scanned(report: &Value, expected: &[(&str, u64, u64)]) {
    assert_eq!(
        report["indexing_stats"]["scan_had_errors"], false,
        "{report}"
    );
    let mut actual = BTreeMap::new();
    for connector in report["indexing_stats"]["connectors"]
        .as_array()
        .expect("index report must contain per-connector scan counts")
    {
        assert!(connector["error"].is_null(), "{connector}");
        let conversations = connector["conversations"].as_u64().unwrap();
        let messages = connector["messages"].as_u64().unwrap();
        if conversations != 0 || messages != 0 {
            let counts = actual
                .entry(connector["name"].as_str().unwrap())
                .or_insert((0_u64, 0_u64));
            counts.0 += conversations;
            counts.1 += messages;
        }
    }
    let expected: BTreeMap<_, _> = expected
        .iter()
        .map(|(name, conversations, messages)| (*name, (*conversations, *messages)))
        .collect();
    assert_eq!(
        actual, expected,
        "scanned conversations must reflect parsing work, not just deduplicated archive rows: {report}"
    );
}

/// GH574: one SSH source can have several mirror roots. Their successful-scan
/// receipts must coexist, and changing one transcript must not reparse its
/// unchanged siblings. rsync preserves remote mtimes, so both newly delivered
/// files and appends deliberately retain timestamps older than every scan.
#[test]
fn gh574_ssh_mirrors_index_only_changed_sessions_in_both_modes() {
    for streaming in ["0", "1"] {
        let tracker = PhaseTracker::new(
            "e2e_remote_mirror_subtrees",
            &format!("gh574_incremental_streaming_{streaming}"),
        );
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let config = tmp.path().join("config");
        let data = tmp.path().join("data");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(config.join("cass")).unwrap();
        fs::write(home.join(".env"), "").unwrap();
        let mirror = data.join("remotes").join(SOURCE_ID).join("mirror");
        let paths = ["~/.pi/agent/sessions", "~/.omp/agent/sessions"];
        let mut originals = BTreeMap::new();
        for (provider, source) in ["pi", "omp"].into_iter().zip(paths) {
            let directory = mirror
                .join(path_to_safe_dirname(source))
                .join("sessions/--remote-project--");
            fs::create_dir_all(&directory).unwrap();
            for index in 0..2 {
                let id = format!("gh574{provider}session{index}");
                let file = directory.join(format!("2023-11-14T22-13-20_{id}.jsonl"));
                fs::write(&file, gh574_session(&id)).unwrap();
                gh574_preserve_old_mtime(&file);
                originals.insert((provider, index), file);
            }
        }
        fs::write(
            config.join("cass/sources.toml"),
            format!(
                "[[sources]]\nname = {SOURCE_ID:?}\ntype = \"ssh\"\nhost = {HOST:?}\npaths = {}\n",
                serde_json::to_string(&paths).unwrap()
            ),
        )
        .unwrap();

        let run = |args: &[&str]| -> Value {
            let mut command = tracker.cass_std_command();
            command
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", &home)
                .env("XDG_CONFIG_HOME", &config)
                .env("XDG_DATA_HOME", tmp.path().join("xdg"))
                .env("CASS_DATA_DIR", &data)
                .env("CASS_STREAMING_INDEX", streaming)
                .env("CASS_AUTO_REFRESH", "0")
                .env("RUST_MIN_STACK", "134217728")
                .current_dir(&home);
            tracker.command_environment().apply_to_std(&mut command);
            let output = assert_cmd::Command::from_std(command)
                .args(args)
                .timeout(Duration::from_secs(120))
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "streaming={streaming}, args={args:?}\nstdout={}\nstderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            serde_json::from_slice(&output.stdout).unwrap()
        };
        let incremental = ["index", "--json", "--no-progress-events"];
        let full = ["index", "--full", "--json", "--no-progress-events"];

        gh574_assert_scanned(&run(&full), &[("pi_agent", 2, 4), ("omp", 2, 4)]);
        let initial = snapshot(&data);
        assert_eq!((initial.0.len(), initial.1.len()), (4, 8));
        assert!(
            initial
                .0
                .iter()
                .all(|row| { row.source == SOURCE_ID && row.host.as_deref() == Some(HOST) })
        );
        for _ in 0..2 {
            gh574_assert_scanned(&run(&incremental), &[]);
            assert_eq!(snapshot(&data), initial);
        }

        for (provider, slug, expected_messages) in [("pi", "pi_agent", 9), ("omp", "omp", 10)] {
            let file = &originals[&(provider, 0)];
            let marker = format!("gh574{provider}appendprobe");
            let mut bytes = fs::read(file).unwrap();
            bytes.extend_from_slice(
                format!(
                    "{}\n",
                    json!({"type": "message", "timestamp": "2023-11-14T22:13:23Z",
                           "message": {"role": "user", "content": marker}})
                )
                .as_bytes(),
            );
            fs::write(file, &bytes).unwrap();
            gh574_preserve_old_mtime(file);
            gh574_assert_scanned(&run(&incremental), &[(slug, 1, 3)]);
            let updated = snapshot(&data);
            assert_eq!(updated.0, initial.0, "an append must retain canonical IDs");
            assert_eq!(updated.1.len(), expected_messages);
            let selected_id = initial.0.iter().find(|row| row.path == *file).unwrap().id;
            assert!(
                updated
                    .1
                    .contains(&(selected_id, 2, "user".to_owned(), marker.clone()))
            );
            let search = run(&[
                "search",
                "--robot",
                "--mode",
                "lexical",
                "--no-daemon",
                &marker,
            ]);
            assert_eq!(search["hits"].as_array().unwrap().len(), 1, "{search}");
            gh574_assert_scanned(&run(&incremental), &[]);
            assert_eq!(fs::read(file).unwrap(), bytes);
        }

        let new_file = originals[&("pi", 0)]
            .parent()
            .unwrap()
            .join("2023-11-14T22-13-20_gh574newarrival.jsonl");
        fs::write(&new_file, gh574_session("gh574newarrival")).unwrap();
        gh574_preserve_old_mtime(&new_file);
        gh574_assert_scanned(&run(&incremental), &[("pi_agent", 1, 2)]);
        let with_new = snapshot(&data);
        assert_eq!((with_new.0.len(), with_new.1.len()), (5, 12));
        assert!(with_new.0.iter().any(|row| row.path == new_file));
        for original in &initial.0 {
            assert!(
                with_new.0.contains(original),
                "new arrivals must not rekey old sessions"
            );
        }
        let search = run(&[
            "search",
            "gh574newarrival",
            "--robot",
            "--mode",
            "lexical",
            "--no-daemon",
        ]);
        let found: BTreeSet<_> = search["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|hit| hit["conversation_id"].as_i64().unwrap())
            .collect();
        assert_eq!(found.len(), 1, "{search}");
        gh574_assert_scanned(&run(&incremental), &[]);
        assert_eq!(snapshot(&data), with_new);

        gh574_assert_scanned(&run(&full), &[("pi_agent", 3, 7), ("omp", 2, 5)]);
        assert_eq!(
            snapshot(&data),
            with_new,
            "--full must retain archive identity"
        );
        gh574_assert_scanned(&run(&incremental), &[]);
        tracker.complete();
    }
}
