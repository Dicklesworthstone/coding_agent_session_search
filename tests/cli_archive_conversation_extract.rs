//! A real-binary recovery journey using an independent, database-free v1 fixture.
//! The fixture's canonical SHA-256 was computed outside the Rust codec.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;
use sha2::{Digest, Sha256};

const SOURCE: &[u8] = include_bytes!("fixtures/logical_archive/conversation_recovery_v1.jsonl");
const SOURCE_DIGEST: &str = "bed004624c6ef745e883b5ce3924f0cff4d125b19ab68d53a826dbfb9aeff816";

fn command(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cass"));
    command
        .current_dir(root)
        .env("CASS_AUTO_REFRESH", "0")
        .env("CASS_DATA_DIR", root.join("never-open-data"))
        .arg("--db")
        .arg(root.join("never-open.db"));
    command
}

fn extract(root: &Path, input: &Path, output: &Path, private: bool) -> Output {
    let mut command = command(root);
    command
        .args(["archive", "extract-conversation"])
        .arg(input)
        .args(["--conversation-id", "7", "--content-sha256", SOURCE_DIGEST, "--output"])
        .arg(output)
        .arg("--json");
    if private {
        command.arg("--include-private");
    }
    command.output().expect("run cass conversation extraction")
}

#[test]
fn real_cli_recovers_a_whole_conversation_without_database_setup_and_refuses_unsafe_publication() {
    let root = tempfile::tempdir().unwrap();
    let input = root.path().join("backup.jsonl");
    let output = root.path().join("conversation.jsonl");
    fs::write(&input, SOURCE).unwrap();
    let verified = command(root.path())
        .args(["archive", "verify"])
        .arg(&input)
        .arg("--json")
        .output()
        .unwrap();
    assert!(verified.status.success(), "{}", String::from_utf8_lossy(&verified.stderr));
    let verification: Value = serde_json::from_slice(&verified.stdout).unwrap();
    assert_eq!(verification["content_sha256"], SOURCE_DIGEST);

    let refusal = extract(root.path(), &input, &output, false);
    assert_eq!(refusal.status.code(), Some(2));
    assert!(!output.exists());
    let recovered = extract(root.path(), &input, &output, true);
    assert!(recovered.status.success(), "{}", String::from_utf8_lossy(&recovered.stderr));
    let receipt: Value = serde_json::from_slice(&recovered.stdout).unwrap();
    let bytes = fs::read(&output).unwrap();
    assert_eq!(receipt["operation"], "extract-conversation");
    assert_eq!(receipt["message_rows"], 2);
    assert_eq!(receipt["content_sha256"], SOURCE_DIGEST);
    assert_eq!(receipt["output_sha256"], hex::encode(Sha256::digest(&bytes)));
    assert_eq!(receipt["database_opened"], false);
    assert_eq!(receipt["provider_files_opened"], false);
    assert_eq!(receipt["restorable_as_archive"], false);
    let text = std::str::from_utf8(&bytes).unwrap();
    assert!(text.contains("PRIVATE-TARGET late evidence"));
    assert!(!text.contains("PRIVATE-UNRELATED"));

    let repeated = extract(root.path(), &input, &output, true);
    assert!(!repeated.status.success());
    assert_eq!(fs::read(&output).unwrap(), bytes);
    let corrupt = root.path().join("corrupt.jsonl");
    fs::write(&corrupt, &SOURCE[..SOURCE.len() - 1]).unwrap();
    let unpublished = root.path().join("never-publish.jsonl");
    let failed = extract(root.path(), &corrupt, &unpublished, true);
    assert_eq!(failed.status.code(), Some(5));
    assert!(!unpublished.exists());
    assert_eq!(fs::read(&input).unwrap(), SOURCE);
    assert!(!root.path().join("never-open.db").exists());
    assert!(!root.path().join("never-open-data").exists());
}
