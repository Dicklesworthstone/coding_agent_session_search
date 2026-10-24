//! Real-process checks for one-shot canonical transcript paging.
//! Synthetic FrankenSQLite records only: never a user's archive or raw files.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use coding_agent_search::franken_sync::Connection;
use coding_agent_search::franken_sync::compat::ConnectionExt;
use coding_agent_search::franken_sync::params;
use serde_json::{Value, json};
use wait_timeout::ChildExt;

const SOURCE: &str = "/never-opened/δ ' %_ transcript.jsonl";
const PAGE_BYTES: usize = 32 * 1024;

struct Fixture {
    root: tempfile::TempDir,
    db: PathBuf,
}

impl Fixture {
    fn new(rows: &[(i64, i64, &str)]) -> Result<Self> {
        let root = tempfile::tempdir()?;
        let db = root.path().join("archive.db");
        let writer = Connection::open(db.to_str().context("fixture path")?)?;
        writer.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE conversations (id INTEGER PRIMARY KEY, source_path TEXT, source_id TEXT);
             CREATE TABLE messages (id INTEGER PRIMARY KEY, conversation_id INTEGER, idx INTEGER, role TEXT, content TEXT);
             CREATE INDEX messages_by_conversation ON messages(conversation_id, idx);",
        )?;
        writer.execute_compat(
            "INSERT INTO conversations VALUES (1, ?1, 'local')",
            params![SOURCE],
        )?;
        for &(id, idx, text) in rows {
            writer.execute_compat(
                "INSERT INTO messages VALUES (?1, 1, ?2, 'agent', ?3)",
                params![id, idx, text],
            )?;
        }
        writer.close()?;
        Ok(Self { root, db })
    }
}

fn selection() -> Value {
    json!({"source_path": SOURCE, "source_id": "local", "conversation_id": 1})
}

/// Compare every file/directory in this tiny fixture, not just the main DB.
fn snapshot(root: &Path) -> Result<BTreeMap<PathBuf, Option<Vec<u8>>>> {
    let mut image = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            let relative = path.strip_prefix(root)?.to_path_buf();
            let kind = entry.file_type()?;
            ensure!(!kind.is_symlink(), "unexpected fixture symlink");
            if kind.is_dir() {
                image.insert(relative, None);
                pending.push(path);
            } else {
                ensure!(kind.is_file(), "unexpected fixture file type");
                image.insert(relative, Some(std::fs::read(path)?));
            }
        }
    }
    Ok(image)
}

struct Child(std::process::Child);

impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Leave stdin open until exit. A transcript request incorrectly entering the
/// frame loop must fail the external deadline instead of succeeding on EOF.
/// File-backed capture avoids pipe deadlock; bound output before materializing.
fn execute(
    root: &Path,
    db: &Path,
    selector: &str,
    stdio: bool,
) -> Result<(ExitStatus, Vec<u8>, Vec<u8>)> {
    let capture = tempfile::tempdir()?;
    let out = capture.path().join("stdout");
    let err = capture.path().join("stderr");
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    command.env_clear();
    for key in ["PATH", "SystemRoot", "SystemDrive", "TEMP", "TMP", "TMPDIR"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command
        .args(["serve", "--data-dir"])
        .arg(root.join("no-lexical-index"))
        .arg("--db")
        .arg(db)
        .args(["--request-timeout-ms", "10000"])
        .current_dir(root)
        .env("HOME", root)
        .env("USERPROFILE", root)
        .env("XDG_CONFIG_HOME", root.join("no-config"))
        .env("CASS_AUTO_REFRESH", "0")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("CASS_EXTERNAL_EMBEDDINGS", "0")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("RUST_MIN_STACK", "134217728")
        .env("RUST_LOG", "error")
        .stdin(Stdio::piped())
        .stdout(std::fs::File::create(&out)?)
        .stderr(std::fs::File::create(&err)?);
    if stdio {
        command.arg("--stdio");
    } else {
        command.args(["--transcript", selector]);
    }
    let mut child = Child(command.spawn()?);
    let mut input = child.0.stdin.take().context("child stdin")?;
    if stdio {
        let mut frame: Value = serde_json::from_str(selector)?;
        frame["op"] = json!("transcript");
        frame["id"] = json!(1);
        writeln!(input, "{frame}")?;
        writeln!(input, "{}", json!({"op":"shutdown", "id":2}))?;
        input.flush()?;
    }
    let status = child
        .0
        .wait_timeout(Duration::from_secs(30))?
        .context("canonical one-shot waited for stdin or exceeded its deadline")?;
    drop(input);
    for path in [&out, &err] {
        ensure!(
            std::fs::metadata(path)?.len() <= 2 * 1024 * 1024,
            "oversized test output"
        );
    }
    Ok((status, std::fs::read(out)?, std::fs::read(err)?))
}

fn read_page(fixture: &Fixture, selector: &Value) -> Result<Value> {
    let (status, out, err) = execute(
        fixture.root.path(),
        &fixture.db,
        &selector.to_string(),
        false,
    )?;
    ensure!(
        status.success(),
        "{status}: {}",
        String::from_utf8_lossy(&err)
    );
    ensure!(
        err.is_empty(),
        "unexpected diagnostic: {}",
        String::from_utf8_lossy(&err)
    );
    let reply: Value = serde_json::from_slice(&out)?;
    assert_eq!(reply["id"], 1);
    assert_eq!(reply["ok"], true);
    assert!(out.ends_with(b"\n"));
    Ok(reply["result"].clone())
}

#[test]
fn transcript_one_shot_matches_stdio_without_lexical_assets_or_raw_files() -> Result<()> {
    let fixture = Fixture::new(&[(1, 0, "first"), (2, 7, "雪\0second"), (3, 19, "last")])?;
    let before = snapshot(fixture.root.path())?;
    let mut selected = selection();
    selected["limit"] = json!(2);
    let page = read_page(&fixture, &selected)?;
    assert_eq!(page["count"], 2);
    assert_eq!(page["messages"][1]["message_index"], 8);
    assert_eq!(page["messages"][1]["content"], "雪\0second");
    assert_eq!(page["messages"][0]["role"], "assistant");
    assert_eq!(page["preview_only"], false);
    assert_eq!(page["maintenance_performed"], false);
    assert_eq!(page["cross_page_snapshot"], false);
    let (status, out, err) = execute(
        fixture.root.path(),
        &fixture.db,
        &selected.to_string(),
        true,
    )?;
    ensure!(
        status.success(),
        "{status}: {}",
        String::from_utf8_lossy(&err)
    );
    assert!(err.is_empty());
    let replies = std::str::from_utf8(&out)?
        .lines()
        .map(serde_json::from_str)
        .collect::<std::result::Result<Vec<Value>, _>>()?;
    assert_eq!(replies.len(), 2);
    assert_eq!(replies[0]["result"], page);
    assert_eq!(replies[1]["result"]["shutdown"], true);
    assert_eq!(snapshot(fixture.root.path())?, before);
    Ok(())
}

#[test]
fn transcript_one_shot_cursor_reassembles_large_messages_exactly_across_processes() -> Result<()> {
    let large = format!(
        "{}😀{}\0tail",
        "a".repeat(PAGE_BYTES - 1),
        "雪".repeat(PAGE_BYTES)
    );
    let fixture = Fixture::new(&[(1, 0, ""), (2, 7, &large), (3, 19, "after\0message")])?;
    let before = snapshot(fixture.root.path())?;
    let mut selected = selection();
    selected["limit"] = json!(2);
    let mut bodies: BTreeMap<i64, String> = BTreeMap::new();
    let mut partial = false;
    let mut finished = false;
    for _ in 0..12 {
        let page = read_page(&fixture, &selected)?;
        assert!(page["content_bytes"].as_u64().context("page byte count")? <= PAGE_BYTES as u64);
        assert_eq!(page["cross_page_snapshot"], false);
        for message in page["messages"].as_array().context("page messages")? {
            let body = bodies
                .entry(message["message_id"].as_i64().context("message identity")?)
                .or_default();
            assert_eq!(message["byte_offset"].as_u64(), Some(body.len() as u64));
            body.push_str(message["content"].as_str().context("message chunk")?);
            assert_eq!(message["end_byte_offset"].as_u64(), Some(body.len() as u64));
            partial |= message["partial_message"] == true;
        }
        if page["next_cursor"].is_null() {
            assert_eq!(page["end_of_conversation"], true);
            finished = true;
            break;
        }
        assert_eq!(page["end_of_conversation"], false);
        selected["cursor"] = page["next_cursor"].clone();
    }
    assert!(partial && finished);
    assert_eq!(bodies.get(&1).map(String::as_str), Some(""));
    assert_eq!(bodies.get(&2), Some(&large));
    assert_eq!(bodies.get(&3).map(String::as_str), Some("after\0message"));
    assert_eq!(snapshot(fixture.root.path())?, before);
    Ok(())
}

#[test]
fn transcript_one_shot_distinguishes_empty_from_missing_and_mismatched_identity() -> Result<()> {
    let fixture = Fixture::new(&[])?;
    let before = snapshot(fixture.root.path())?;
    let empty = read_page(&fixture, &selection())?;
    assert_eq!(empty["count"], 0);
    assert_eq!(empty["end_of_conversation"], true);
    for (field, value, kind) in [
        ("source_id", json!("other"), "canonical_identity_mismatch"),
        (
            "source_path",
            json!("/wrong"),
            "canonical_identity_mismatch",
        ),
        ("conversation_id", json!(2), "canonical_not_found"),
    ] {
        let mut selected = selection();
        selected[field] = value;
        let (status, out, err) = execute(
            fixture.root.path(),
            &fixture.db,
            &selected.to_string(),
            false,
        )?;
        assert_eq!(status.code(), Some(1));
        assert!(out.is_empty());
        let reply: Value = serde_json::from_slice(&err)?;
        assert_eq!(reply["ok"], false);
        assert_eq!(reply["error"]["kind"], kind);
    }
    assert_eq!(snapshot(fixture.root.path())?, before);
    Ok(())
}

#[test]
fn transcript_one_shot_refuses_a_resized_cursor_instead_of_substituting_a_neighbour() -> Result<()>
{
    let fixture = Fixture::new(&[(1, 0, &"x".repeat(PAGE_BYTES + 20)), (2, 7, "neighbour")])?;
    let first = read_page(&fixture, &selection())?;
    assert!(!first["next_cursor"].is_null());
    let writer = Connection::open(fixture.db.to_str().context("fixture path")?)?;
    writer.execute("UPDATE messages SET content = 'changed' WHERE id = 1")?;
    writer.close()?;
    let before = snapshot(fixture.root.path())?;
    let mut selected = selection();
    selected["cursor"] = first["next_cursor"].clone();
    let (status, out, err) = execute(
        fixture.root.path(),
        &fixture.db,
        &selected.to_string(),
        false,
    )?;
    assert_eq!(status.code(), Some(1));
    assert!(out.is_empty());
    let reply: Value = serde_json::from_slice(&err)?;
    assert_eq!(reply["error"]["kind"], "canonical_cursor_changed");
    assert!(reply.get("result").is_none());
    assert_eq!(snapshot(fixture.root.path())?, before);
    Ok(())
}

#[test]
fn transcript_one_shot_rejects_invalid_selection_before_creating_storage() -> Result<()> {
    let root = tempfile::tempdir()?;
    let missing = root.path().join("missing.db");
    let mut wrong_limit = selection();
    wrong_limit["limit"] = json!(33);
    let mut wrong_cursor = selection();
    wrong_cursor["cursor"] = json!({"message_id":1,"message_index":1,"byte_offset":4});
    for selected in [
        "{".into(),
        wrong_limit.to_string(),
        wrong_cursor.to_string(),
    ] {
        let (status, out, err) = execute(root.path(), &missing, &selected, false)?;
        assert_eq!(status.code(), Some(2));
        assert!(out.is_empty());
        let reply: Value = serde_json::from_slice(&err)?;
        assert_eq!(reply["error"]["kind"], "invalid_request");
        assert_eq!(std::fs::read_dir(root.path())?.count(), 0);
    }
    Ok(())
}
