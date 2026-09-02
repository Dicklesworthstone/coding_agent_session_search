//! `cass pages key …` through the real binary (WS-G.4, bead ctigq).
//!
//! The unit tests in `pages::key_cli` exercise the verbs directly; this file
//! proves the clap wiring under `cass pages`, the `--password-stdin` contract
//! (current password on line 1, new password on line 2), the JSON document
//! shape, and the exit codes an agent branches on. The bundle is a real
//! encrypted export built with the same engine `cass pages` uses.

use assert_cmd::Command;
use coding_agent_search::pages::bundle::BundleBuilder;
use coding_agent_search::pages::encrypt::EncryptionEngine;
use serde_json::Value;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

const PASSWORD: &str = "correct horse battery staple";

fn encrypted_bundle(root: &Path) -> PathBuf {
    let input = root.join("input.txt");
    let encrypted = root.join("encrypted");
    let bundle = root.join("bundle");
    std::fs::write(&input, b"pages key cli fixture").expect("write input");
    let mut engine = EncryptionEngine::new(1024).expect("engine");
    engine.add_password_slot(PASSWORD).expect("password slot");
    engine
        .encrypt_file(&input, &encrypted, |_, _| {})
        .expect("encrypt");
    BundleBuilder::new()
        .build(&encrypted, &bundle, |_, _| {})
        .expect("bundle");
    bundle
}

fn cass(home: &Path) -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("CASS_AUTO_REFRESH", "0")
        .current_dir(home);
    cmd
}

fn json(output: &std::process::Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
        panic!(
            "stdout is not one JSON document: {err}\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

/// Positive observable: list → add-password (stdin: current, new) →
/// revoke round-trip through the binary, each step reflected by the next
/// `list`. Planted negatives: a wrong current password is exit 1 (the engine
/// refused, nothing changed), and a path that is not a bundle is exit 3.
/// No-claim: the interactive prompt path is not exercised (no TTY).
#[test]
fn pages_key_verbs_round_trip_through_the_binary_with_stdin_passwords() {
    let temp = TempDir::new().expect("tempdir");
    let home = temp.path();
    let bundle = encrypted_bundle(home);

    let listed = cass(home)
        .args(["pages", "key", "list", "--archive"])
        .arg(&bundle)
        .arg("--json")
        .output()
        .expect("key list");
    assert!(
        listed.status.success(),
        "list must succeed without a password: stderr={}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let listed = json(&listed);
    assert_eq!(listed["success"], Value::Bool(true), "{listed}");
    assert_eq!(listed["action"], Value::String("list".into()), "{listed}");
    assert_eq!(listed["active_slots"], Value::from(1), "{listed}");

    let added = cass(home)
        .args(["pages", "key", "add-password", "--archive"])
        .arg(&bundle)
        .args(["--password-stdin", "--json"])
        .write_stdin(format!("{PASSWORD}\nsecond password 42\n"))
        .output()
        .expect("key add-password");
    assert!(
        added.status.success(),
        "add-password failed: stdout={} stderr={}",
        String::from_utf8_lossy(&added.stdout),
        String::from_utf8_lossy(&added.stderr)
    );
    let added = json(&added);
    assert_eq!(
        added["action"],
        Value::String("add-password".into()),
        "{added}"
    );
    assert_eq!(added["slot_id"], Value::from(1), "{added}");
    assert_eq!(added["active_slots"], Value::from(2), "{added}");

    // Planted negative: the wrong current password must be a typed refusal
    // and must not change the archive.
    let refused = cass(home)
        .args(["pages", "key", "add-password", "--archive"])
        .arg(&bundle)
        .args(["--password-stdin", "--json"])
        .write_stdin("not the password\nwhatever password\n")
        .output()
        .expect("key add-password with wrong password");
    assert_eq!(
        refused.status.code(),
        Some(1),
        "wrong password must exit 1: stdout={} stderr={}",
        String::from_utf8_lossy(&refused.stdout),
        String::from_utf8_lossy(&refused.stderr)
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&refused.stdout),
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(
        combined.contains("add-password failed"),
        "the refusal must say which verb failed: {combined}"
    );

    let revoked = cass(home)
        .args(["pages", "key", "revoke", "--archive"])
        .arg(&bundle)
        .args(["--slot", "1", "--password-stdin", "--json"])
        .write_stdin(format!("{PASSWORD}\n"))
        .output()
        .expect("key revoke");
    assert!(
        revoked.status.success(),
        "revoke failed: stdout={} stderr={}",
        String::from_utf8_lossy(&revoked.stdout),
        String::from_utf8_lossy(&revoked.stderr)
    );
    let revoked = json(&revoked);
    assert_eq!(revoked["revoked_slot_id"], Value::from(1), "{revoked}");
    assert_eq!(revoked["remaining_slots"], Value::from(1), "{revoked}");

    let listed = json(
        &cass(home)
            .args(["pages", "key", "list", "--archive"])
            .arg(&bundle)
            .arg("--json")
            .output()
            .expect("key list after revoke"),
    );
    assert_eq!(listed["active_slots"], Value::from(1), "{listed}");

    // Planted negative: not a bundle.
    let missing = cass(home)
        .args(["pages", "key", "list", "--archive"])
        .arg(home.join("definitely-not-a-bundle"))
        .arg("--json")
        .output()
        .expect("key list on a missing path");
    assert_eq!(
        missing.status.code(),
        Some(3),
        "a path that is not a bundle must exit 3: stdout={} stderr={}",
        String::from_utf8_lossy(&missing.stdout),
        String::from_utf8_lossy(&missing.stderr)
    );
}

/// A password verb without `--password-stdin` and without a terminal must
/// refuse with exit 6 (`password-required`) instead of hanging on a prompt.
#[test]
fn pages_key_password_verbs_refuse_without_stdin_or_a_terminal() {
    let temp = TempDir::new().expect("tempdir");
    let home = temp.path();
    let bundle = encrypted_bundle(home);
    let output = cass(home)
        .args(["pages", "key", "add-recovery", "--archive"])
        .arg(&bundle)
        .arg("--json")
        .write_stdin("")
        .output()
        .expect("key add-recovery without a password source");
    assert_eq!(
        output.status.code(),
        Some(6),
        "no password source must exit 6: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
