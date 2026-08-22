//! E2E: `cass import chatgpt` → `cass index --full` → `cass search`.
//!
//! GH #378: on Linux/Windows the ChatGPT connector has no default root and the
//! indexer only scans default roots for detected agents, so imported files used
//! to sit in a directory nothing scanned. The import now registers its output
//! directory as an explicit local source in `sources.toml`; this test proves
//! the whole chain through the real binary with an isolated HOME, config dir,
//! and data dir.

use std::fs;

mod util;
use util::e2e_log::PhaseTracker;

fn tracker_for(test_name: &str) -> PhaseTracker {
    PhaseTracker::new("e2e_import_chatgpt", test_name)
}

fn chatgpt_export_json() -> serde_json::Value {
    serde_json::json!([
        {
            "id": "e2e-import-conv-001",
            "title": "Import chain fixture",
            "mapping": {
                "node-user": {
                    "parent": null,
                    "message": {
                        "author": { "role": "user" },
                        "content": { "parts": ["How do I make importchainproof searchable?"] },
                        "create_time": 1701000000.25
                    }
                },
                "node-assistant": {
                    "parent": "node-user",
                    "message": {
                        "author": { "role": "assistant" },
                        "content": { "parts": ["Register the output directory as a scan root."] },
                        "create_time": 1701000001.75,
                        "metadata": { "model_slug": "gpt-4o" }
                    }
                }
            }
        },
        {
            "id": "../not-a-safe-id",
            "title": "Unsafe id fixture",
            "mapping": {
                "node-user": {
                    "parent": null,
                    "message": {
                        "author": { "role": "user" },
                        "content": { "parts": ["unsafeidproof must still import"] },
                        "create_time": 1701000100.0
                    }
                }
            }
        }
    ])
}

#[test]
fn import_then_index_then_search_finds_imported_conversations() {
    let tracker = tracker_for("import_then_index_then_search_finds_imported_conversations");

    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let config_home = home.join(".config");
    let data_dir = home.join("cass_data");
    let output_dir = home.join("chatgpt-out");
    fs::create_dir_all(&config_home).unwrap();
    fs::create_dir_all(&data_dir).unwrap();
    let export_path = home.join("conversations.json");
    fs::write(
        &export_path,
        serde_json::to_vec_pretty(&chatgpt_export_json()).unwrap(),
    )
    .unwrap();

    let command_env = tracker
        .command_environment()
        .with_home(home)
        .with_var("XDG_CONFIG_HOME", &config_home);

    // 1) Import with an explicit output dir; must register a scan root.
    let ps = tracker.start("import", Some("cass import chatgpt --json"));
    let output = command_env
        .cass_assert_command()
        .args(["import", "chatgpt"])
        .arg(&export_path)
        .arg("--output-dir")
        .arg(&output_dir)
        .arg("--json")
        .output()
        .expect("import command");
    tracker.end("import", Some("cass import chatgpt --json"), ps);
    assert!(
        output.status.success(),
        "import failed: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let import_json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("import json");
    assert_eq!(import_json["imported"], 2);
    assert_eq!(import_json["skipped"], 0);
    let conv_dir = output_dir.join("conversations-web-export");
    assert_eq!(
        import_json["output_dir"].as_str(),
        Some(conv_dir.to_string_lossy().as_ref())
    );
    assert_eq!(import_json["scan_root"]["source"], "chatgpt-import");
    assert_eq!(import_json["scan_root"]["registration"], "created");

    // The unsafe id must not have escaped the output directory.
    let mut written: Vec<String> = fs::read_dir(&conv_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    written.sort();
    assert_eq!(written.len(), 2, "{written:?}");
    assert!(
        written.contains(&"e2e-import-conv-001.json".to_string()),
        "{written:?}"
    );
    assert!(
        written
            .iter()
            .any(|name| name.starts_with("conv-") && name.ends_with(".json")),
        "the unsafe id must fall back to a digest stem: {written:?}"
    );
    assert!(!home.join("not-a-safe-id.json").exists());

    let sources_toml = fs::read_to_string(config_home.join("cass").join("sources.toml"))
        .expect("sources.toml written");
    assert!(sources_toml.contains("chatgpt-import"), "{sources_toml}");
    assert!(sources_toml.contains("type = \"local\""), "{sources_toml}");

    // 2) A re-import is idempotent: nothing new written, registration reused.
    let output = command_env
        .cass_assert_command()
        .args(["import", "chatgpt"])
        .arg(&export_path)
        .arg("--output-dir")
        .arg(&output_dir)
        .arg("--json")
        .output()
        .expect("re-import command");
    assert!(output.status.success());
    let reimport: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(reimport["imported"], 0);
    assert_eq!(reimport["skipped"], 2);
    assert_eq!(reimport["scan_root"]["registration"], "already_registered");

    // 3) Index: the explicit scan root is scanned regardless of detection.
    let ps = tracker.start("index", Some("cass index --full"));
    command_env
        .cass_assert_command()
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .assert()
        .success();
    tracker.end("index", Some("cass index --full"), ps);

    // 4) Search finds both imported conversations under the chatgpt agent.
    let ps = tracker.start("search", Some("cass search --robot"));
    for (term, expected_title) in [
        ("importchainproof", "Import chain fixture"),
        ("unsafeidproof", "Unsafe id fixture"),
    ] {
        let output = command_env
            .cass_assert_command()
            .args(["search", term, "--robot", "--data-dir"])
            .arg(&data_dir)
            .output()
            .expect("search command");
        assert!(
            output.status.success(),
            "search failed: {}\nstderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let hits = json["hits"].as_array().expect("hits array");
        assert!(
            hits.iter().any(|hit| {
                hit["agent"].as_str() == Some("chatgpt")
                    && hit["title"].as_str() == Some(expected_title)
            }),
            "expected an imported chatgpt hit titled {expected_title:?} for {term:?}: {json}"
        );
    }
    tracker.end("search", Some("cass search --robot"), ps);
}
