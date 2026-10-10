//! GH#517: one large message must not make the entire canonical backup unusable.
//! These tests use Cargo's real cass binary, not a PATH installation or a mock.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::Path;
use std::process::Output;
use std::time::Duration;

use assert_cmd::Command;
use coding_agent_search::franken_sync::compat::{OpenFlags, RowExt, open_with_flags};
use coding_agent_search::franken_sync::{Connection, SqliteValue};
use coding_agent_search::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
use coding_agent_search::storage::sqlite::SqliteStorage;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const WIRE_LIMIT: usize = 8 * 1024 * 1024;

fn command(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cass"));
    command
        .current_dir(home)
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("CASS_DATA_DIR", home.join("unused-default"))
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("CASS_AUTO_REFRESH", "0")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env_remove("CASS_OUTPUT_FORMAT")
        .env_remove("TOON_DEFAULT_FORMAT")
        .timeout(Duration::from_secs(180));
    command
}

fn receipt(output: Output) -> Value {
    assert!(
        output.status.success(),
        "archive command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("one JSON success receipt")
}

fn export(home: &Path, source: &Path, output: &Path) -> Value {
    receipt(
        command(home)
            .args(["archive", "export", "--db"])
            .arg(source)
            .args([
                "--archive-id",
                "large-row-proof",
                "--include-private",
                "--format-version",
                "2",
                "--output",
            ])
            .arg(output)
            .output()
            .unwrap(),
    )
}

fn import(home: &Path, input: &Path, output: &Path, identical: bool) -> Output {
    let mut cmd = command(home);
    cmd.args(["archive", "import"])
        .arg(input)
        .args([
            "--archive-id",
            "large-row-proof",
            "--include-private",
            "--output",
        ])
        .arg(output);
    if identical {
        cmd.arg("--if-identical");
    }
    cmd.output().unwrap()
}

fn bundle_digest(path: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut result = BTreeMap::new();
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        let mut file = match File::open(Path::new(&name)) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => panic!("cannot fingerprint test database: {error}"),
        };
        let mut digest = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let count = file.read(&mut buffer).unwrap();
            if count == 0 {
                break;
            }
            digest.update(&buffer[..count]);
        }
        result.insert(suffix.to_owned(), digest.finalize().to_vec());
    }
    result
}

fn oversized_source(path: &Path) -> (i64, i64, String, Vec<u8>) {
    let storage = SqliteStorage::open(path).unwrap();
    let agent_id = storage
        .ensure_agent(&Agent {
            id: None,
            slug: "codex".to_owned(),
            name: "Codex".to_owned(),
            version: None,
            kind: AgentKind::Cli,
        })
        .unwrap();
    storage
        .insert_conversation_tree(
            agent_id,
            None,
            &Conversation {
                id: None,
                agent_slug: "codex".to_owned(),
                workspace: None,
                external_id: Some("oversized-row-fixture".to_owned()),
                title: Some("Large canonical backup fixture".to_owned()),
                source_path: path.with_extension("absent.jsonl"),
                started_at: Some(1_733_000_000_000),
                ended_at: None,
                approx_tokens: None,
                metadata_json: json!({}),
                messages: (0..2)
                    .map(|idx| Message {
                        id: None,
                        idx,
                        role: MessageRole::Agent,
                        author: Some("codex".to_owned()),
                        created_at: Some(1_733_000_000_000 + idx),
                        content: format!("small retained message {idx}"),
                        extra_json: json!({}),
                        snippets: Vec::new(),
                    })
                    .collect(),
                source_id: "local".to_owned(),
                origin_host: None,
            },
        )
        .unwrap();
    drop(storage);

    let connection = Connection::open(path.to_str().unwrap()).unwrap();
    let ids: Vec<i64> = connection
        .query("SELECT id FROM messages ORDER BY id")
        .unwrap()
        .iter()
        .map(|row| row.get_typed(0).unwrap())
        .collect();
    assert_eq!(ids.len(), 2);
    // Reproduce the observed large-row shape: both content and extra_bin are ~9.9 MB.
    // Include Unicode, NUL and line breaks; binary bytes are not interpreted as text.
    let mut body = String::from("OVERSIZED-NEEDLE 雪\0\n");
    body.push_str(&"0123456789abcdef\n".repeat(9_909_057 / 17 + 1));
    body.truncate(9_909_057); // The tail is ASCII, so this is a UTF-8 boundary.
    let mut raw = [0_u8, 1, 127, 128, 254, 255].repeat(9_909_176 / 6 + 1);
    raw.truncate(9_909_176);
    connection
        .execute_with_params(
            "UPDATE messages SET content = ?1, extra_bin = ?2 WHERE id = ?3",
            &[
                SqliteValue::Text(body.clone().into()),
                SqliteValue::Blob(raw.clone().into()),
                SqliteValue::Integer(ids[0]),
            ],
        )
        .unwrap();
    connection.close().unwrap();
    (ids[0], ids[1], body, raw)
}

fn assert_integrity_failure(output: Output) {
    assert_eq!(output.status.code(), Some(5));
    assert!(
        output.stdout.is_empty(),
        "failed imports must not publish success data"
    );
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["error"]["kind"], "logical-archive-integrity");
    assert_eq!(error["error"]["retryable"], false);
    assert!(!String::from_utf8_lossy(&output.stderr).contains("OVERSIZED-NEEDLE"));
}

#[test]
fn oversized_message_export_verify_restore_search_and_reimport_are_lossless() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source.db");
    let (large_id, small_id, body, raw) = oversized_source(&source);
    let source_before = bundle_digest(&source);
    let legacy_output = root.path().join("refused-v1.jsonl");
    let refused = command(root.path())
        .args(["archive", "export", "--db"])
        .arg(&source)
        .args([
            "--archive-id",
            "large-row-proof",
            "--include-private",
            "--format-version",
            "1",
            "--output",
        ])
        .arg(&legacy_output)
        .output()
        .unwrap();
    assert_eq!(refused.status.code(), Some(9));
    assert!(refused.stdout.is_empty());
    let error: Value = serde_json::from_slice(&refused.stderr).unwrap();
    assert!(error["error"]["message"].as_str().unwrap().contains("8 MiB"));
    assert!(
        !legacy_output.exists(),
        "v1 must not silently drop the large row"
    );
    assert_eq!(source_before, bundle_digest(&source));

    let backup = root.path().join("history.jsonl");
    let exported = export(root.path(), &source, &backup);
    assert_eq!(exported["schema_version"], 2);
    assert_eq!(exported["tables"]["messages"], 2);
    assert_eq!(source_before, bundle_digest(&source));
    let verified = receipt(
        command(root.path())
            .args(["archive", "verify"])
            .arg(&backup)
            .output()
            .unwrap(),
    );
    assert_eq!(verified["schema_version"], 2);
    assert_eq!(verified["content_sha256"], exported["content_sha256"]);

    // Check actual framing and create a corrupted copy by omitting a middle
    // chunk. Never alter the good input; every normal physical line remains bounded.
    let corrupt = root.path().join("missing-chunk.jsonl");
    let mut altered = BufWriter::new(File::create(&corrupt).unwrap());
    let mut reader = BufReader::new(File::open(&backup).unwrap());
    let mut line = Vec::new();
    let mut skipped = false;
    let mut starts = 0;
    while reader.read_until(b'\n', &mut line).unwrap() != 0 {
        assert!(line.len() <= WIRE_LIMIT);
        let record: Value = serde_json::from_slice(&line).unwrap();
        if record["type"] == "row_start" {
            starts += 1;
        }
        if !skipped && record["type"] == "row_chunk" && record["sequence"] == 1 {
            skipped = true;
        } else {
            altered.write_all(&line).unwrap();
        }
        line.clear();
    }
    altered.flush().unwrap();
    assert!(
        starts > 0 && skipped,
        "the oversized message was not transported in chunks"
    );

    let search = receipt(
        command(root.path())
            .args(["archive", "search"])
            .arg(&backup)
            .args(["--contains", "OVERSIZED-NEEDLE", "--include-private"])
            .output()
            .unwrap(),
    );
    assert_eq!(search["schema_version"], 2);
    assert_eq!(search["matches"], 1);
    assert_eq!(search["hits"][0]["message_id"], large_id);
    assert_eq!(search["hits"][0]["content_bytes"], body.len());
    assert_eq!(search["database_opened"], false);
    assert_eq!(search["content_sha256"], exported["content_sha256"]);
    // An unselected oversized row must not consume the selected view's 64 KiB budget.
    let viewed = receipt(
        command(root.path())
            .args(["archive", "view"])
            .arg(&backup)
            .args([
                "--message-id",
                &small_id.to_string(),
                "--content-sha256",
            ])
            .arg(exported["content_sha256"].as_str().unwrap())
            .args(["-C", "0", "--include-private"])
            .output()
            .unwrap(),
    );
    assert_eq!(viewed["schema_version"], 2);
    assert_eq!(viewed["messages"][0]["content"], "small retained message 1");

    let target = root.path().join("restored.db");
    let imported = receipt(import(root.path(), &backup, &target, false));
    assert_eq!(imported["schema_version"], 2);
    assert_eq!(imported["content_sha256"], exported["content_sha256"]);
    let connection =
        open_with_flags(target.to_str().unwrap(), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let row = connection
        .query_row_with_params(
            "SELECT content, extra_bin FROM messages WHERE id = ?1",
            &[SqliteValue::Integer(large_id)],
        )
        .unwrap();
    assert_eq!(row.get_typed::<String>(0).unwrap(), body);
    assert_eq!(row.get_typed::<Vec<u8>>(1).unwrap(), raw);
    connection.close_without_checkpoint().unwrap();
    let before = bundle_digest(&target);
    let repeated = receipt(import(root.path(), &backup, &target, true));
    assert_eq!(repeated["destination_status"], "unchanged");
    assert_eq!(repeated["content_sha256"], exported["content_sha256"]);
    assert_eq!(before, bundle_digest(&target));
    let reexported = export(root.path(), &target, &root.path().join("restored.jsonl"));
    assert_eq!(reexported["content_sha256"], exported["content_sha256"]);

    let rejected = root.path().join("must-not-publish.db");
    assert_integrity_failure(import(root.path(), &corrupt, &rejected, false));
    assert!(!rejected.exists());
    assert_integrity_failure(import(root.path(), &corrupt, &target, true));
    assert_eq!(before, bundle_digest(&target));
    assert_eq!(source_before, bundle_digest(&source));
    assert!(!source.with_extension("absent.jsonl").exists());
    assert!(!root.path().join("unused-default").exists());
}

#[test]
fn legacy_v1_backup_restores_and_compares_against_current_v2_snapshots() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source.db");
    drop(SqliteStorage::open(&source).unwrap());
    let current = root.path().join("current.jsonl");
    let exported = export(root.path(), &source, &current);
    let old = root.path().join("legacy-v1.jsonl");
    // Request v1 explicitly for an older receiver; the default remains v2.
    let original = receipt(
        command(root.path())
            .args(["archive", "export", "--db"])
            .arg(&source)
            .args([
                "--archive-id",
                "large-row-proof",
                "--include-private",
                "--format-version",
                "1",
                "--output",
            ])
            .arg(&old)
            .output()
            .unwrap(),
    );
    assert_eq!(original["schema_version"], 1);
    assert_eq!(original["content_sha256"], exported["content_sha256"]);
    let verified = receipt(
        command(root.path())
            .args(["archive", "verify"])
            .arg(&old)
            .output()
            .unwrap(),
    );
    assert_eq!(verified["schema_version"], 1);
    assert_eq!(verified["content_sha256"], exported["content_sha256"]);
    let target = root.path().join("restored.db");
    let imported = receipt(import(root.path(), &old, &target, false));
    assert_eq!(imported["schema_version"], 1);
    assert_eq!(imported["content_sha256"], exported["content_sha256"]);
    let before = bundle_digest(&target);
    let repeated = receipt(import(root.path(), &old, &target, true));
    assert_eq!(repeated["destination_status"], "unchanged");
    assert_eq!(before, bundle_digest(&target));
    let reexported = export(root.path(), &target, &root.path().join("reexported.jsonl"));
    assert_eq!(reexported["schema_version"], 2);
    assert_eq!(reexported["content_sha256"], exported["content_sha256"]);
}
