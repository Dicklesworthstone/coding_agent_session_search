//! Conformance harness for the CASS-local Kiro connector.
//!
//! Kiro has no `franken_agent_detection` implementation, so [`KiroConnector`]
//! is defined CASS-side (mirroring the Codex local-connector pattern) and
//! parses Kiro CLI session logs at `~/.kiro/sessions/cli/`:
//!   - `<session_uuid>.jsonl` — append-only event log (`{version, kind, data}`;
//!     kinds `Prompt`/`AssistantMessage`/`ToolResults`; content blocks
//!     `text`/`thinking`/`toolUse`/`toolResult`). This is the primary parse
//!     target.
//!   - `<session_uuid>.json` — session-state snapshot read as a metadata
//!     sidecar (`session_id`, `cwd`, `title`, `created_at`, `updated_at`,
//!     `session_state.model_info.model_id`).
//!
//! `Prompt` records carry `data.meta.timestamp` as epoch **seconds**; CASS
//! normalizes to epoch **milliseconds**. `AssistantMessage`/`ToolResults`
//! carry no timestamp and inherit the previous value forward so the sequence
//! stays monotonically nondecreasing.
//!
//! See the README's Kiro connector section for the observed contract these tests pin.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use coding_agent_search::connectors::{
    Connector, DiscoveredSourceRole, ScanContext, kiro::KiroConnector,
};
use serde_json::Value;
use serial_test::serial;
use tempfile::TempDir;

/// Build a `~/.kiro/sessions/cli` store inside a temp dir and return its path.
/// The `.kiro` path segment is what the connector keys default detection on,
/// so temp-dir scans stay hermetic (they never fall back to the real
/// `~/.kiro/sessions/cli`).
fn kiro_cli_dir(tmp: &TempDir) -> PathBuf {
    let dir = tmp.path().join(".kiro").join("sessions").join("cli");
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
}

/// The committed real-shape Kiro CLI fixture store (`tests/fixtures/kiro/cli`).
fn kiro_fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/kiro/cli")
}

/// Current wall-clock in epoch milliseconds (for file-level `since_ts` tests).
fn now_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

// ---------------------------------------------------------------------------
// Parsing + metadata extraction
// ---------------------------------------------------------------------------

#[test]
fn kiro_connector_parses_cli_session_with_metadata() {
    let tmp = TempDir::new().unwrap();
    let cli = kiro_cli_dir(&tmp);
    let uuid = "aaaa1111-bbbb-2222-cccc-333344445555";

    write(
        &cli.join(format!("{uuid}.jsonl")),
        &[
            r#"{"version":"v1","kind":"Prompt","data":{"message_id":"m1","content":[{"kind":"text","data":"Wire up the Kiro connector"}],"meta":{"timestamp":1785939877}}}"#,
            r#"{"version":"v1","kind":"AssistantMessage","data":{"message_id":"m2","content":[{"kind":"thinking","data":{"text":"reasoning that must stay private","signature":"s"}},{"kind":"text","data":"On it -- here is the plan."}]}}"#,
        ]
        .join("\n"),
    );
    write(
        &cli.join(format!("{uuid}.json")),
        r#"{"session_id":"aaaa1111-bbbb-2222-cccc-333344445555","cwd":"/work/repo","title":"Kiro connector work","created_at":"2026-08-06T08:44:02.921363Z","updated_at":"2026-08-06T08:59:00.000000Z","session_state":{"version":"v1","model_info":{"model_name":"claude-opus-4.8","model_id":"claude-opus-4.8"}}}"#,
    );

    let connector = KiroConnector::new();
    let convs = connector
        .scan(&ScanContext::local_default(cli, None))
        .unwrap();

    assert_eq!(convs.len(), 1);
    let conv = &convs[0];
    assert_eq!(conv.agent_slug, "kiro");
    // Title comes from the sidecar, not the first user message.
    assert_eq!(conv.title.as_deref(), Some("Kiro connector work"));
    assert_eq!(conv.workspace.as_deref(), Some(Path::new("/work/repo")));
    assert_eq!(conv.messages.len(), 2);

    // Roles.
    assert_eq!(conv.messages[0].role, "user");
    assert_eq!(conv.messages[1].role, "assistant");

    // epoch-seconds -> epoch-ms normalization + monotonic, present timestamps.
    assert_eq!(conv.messages[0].created_at, Some(1_785_939_877_000));
    let t0 = conv.messages[0].created_at.unwrap();
    let t1 = conv.messages[1].created_at.expect("assistant ts present");
    assert!(t1 >= t0, "timestamps must be nondecreasing: {t0} then {t1}");
    assert_eq!(conv.started_at, conv.messages[0].created_at);
    assert_eq!(conv.ended_at, conv.messages[1].created_at);

    // Contiguous 0-based indices.
    assert_eq!(conv.messages[0].idx, 0);
    assert_eq!(conv.messages[1].idx, 1);

    // `thinking` excluded; visible text preserved.
    assert!(conv.messages[1].content.contains("here is the plan"));
    assert!(!conv.messages[1].content.contains("must stay private"));

    // Model id lifted from the sidecar's model_info.
    assert_eq!(
        conv.metadata.get("model_id").and_then(Value::as_str),
        Some("claude-opus-4.8")
    );
    assert_eq!(
        conv.metadata.get("source").and_then(Value::as_str),
        Some("kiro")
    );
}

#[test]
fn kiro_connector_extracts_tool_use_and_tool_result() {
    let tmp = TempDir::new().unwrap();
    let cli = kiro_cli_dir(&tmp);

    write(
        &cli.join("session.jsonl"),
        &[
            r#"{"version":"v1","kind":"Prompt","data":{"content":[{"kind":"text","data":"run the tests"}],"meta":{"timestamp":1785939900}}}"#,
            r#"{"version":"v1","kind":"AssistantMessage","data":{"content":[{"kind":"toolUse","data":{"toolUseId":"tool-1","name":"execute_bash","input":{"command":"cargo test"}}}]}}"#,
            r#"{"version":"v1","kind":"ToolResults","data":{"content":[{"kind":"toolResult","data":{"toolUseId":"tool-1","content":[{"kind":"text","data":"test result: ok. 39 passed"}]}}]}}"#,
        ]
        .join("\n"),
    );

    let connector = KiroConnector::new();
    let convs = connector
        .scan(&ScanContext::local_default(cli, None))
        .unwrap();

    assert_eq!(convs.len(), 1);
    let conv = &convs[0];
    assert_eq!(conv.messages.len(), 3);

    let tool_use = &conv.messages[1];
    assert_eq!(tool_use.role, "assistant");
    assert!(tool_use.content.contains("[Tool: execute_bash]"));
    assert!(tool_use.content.contains("cargo test"));
    assert_eq!(tool_use.invocations.len(), 1);
    assert_eq!(tool_use.invocations[0].kind, "tool");
    assert_eq!(tool_use.invocations[0].name, "execute_bash");
    assert_eq!(tool_use.invocations[0].call_id.as_deref(), Some("tool-1"));
    assert_eq!(
        tool_use.invocations[0]
            .arguments
            .as_ref()
            .and_then(|a| a.get("command"))
            .and_then(Value::as_str),
        Some("cargo test")
    );

    let tool_result = &conv.messages[2];
    assert_eq!(tool_result.role, "tool");
    assert!(tool_result.content.contains("[Tool output: tool-1]"));
    assert!(tool_result.content.contains("39 passed"));
}

#[test]
fn kiro_connector_skips_empty_logs_and_tolerates_unknown_kinds() {
    let tmp = TempDir::new().unwrap();
    let cli = kiro_cli_dir(&tmp);

    // 0-byte log: must be skipped entirely (never yields a conversation).
    write(&cli.join("empty.jsonl"), "");

    // Blank line, malformed line, and an unknown record kind -- only the valid
    // Prompt should survive.
    write(
        &cli.join("mixed.jsonl"),
        &[
            "",
            "not-json",
            r#"{"version":"v1","kind":"SystemPrompt","data":{"content":[{"kind":"text","data":"system"}]}}"#,
            r#"{"version":"v1","kind":"Prompt","data":{"content":[{"kind":"text","data":"real user message"}],"meta":{"timestamp":1785939999}}}"#,
        ]
        .join("\n"),
    );

    let connector = KiroConnector::new();
    let convs = connector
        .scan(&ScanContext::local_default(cli, None))
        .unwrap();

    assert_eq!(convs.len(), 1, "empty log must not yield a conversation");
    assert_eq!(convs[0].messages.len(), 1);
    assert_eq!(convs[0].messages[0].role, "user");
    assert!(convs[0].messages[0].content.contains("real user message"));
}

#[test]
fn kiro_connector_title_falls_back_to_first_user_message() {
    let tmp = TempDir::new().unwrap();
    let cli = kiro_cli_dir(&tmp);

    // No sidecar => title must fall back to the first user message's first line.
    write(
        &cli.join("no-sidecar.jsonl"),
        &[
            r#"{"version":"v1","kind":"AssistantMessage","data":{"content":[{"kind":"text","data":"assistant speaks first"}]}}"#,
            r#"{"version":"v1","kind":"Prompt","data":{"content":[{"kind":"text","data":"This is the real question\nwith a second line"}],"meta":{"timestamp":1785939877}}}"#,
        ]
        .join("\n"),
    );

    let connector = KiroConnector::new();
    let convs = connector
        .scan(&ScanContext::local_default(cli, None))
        .unwrap();

    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].title.as_deref(), Some("This is the real question"));
}

#[test]
fn kiro_connector_external_id_is_relative_stem() {
    let tmp = TempDir::new().unwrap();
    let cli = kiro_cli_dir(&tmp);
    let uuid = "dddd4444-eeee-5555-ffff-666677778888";
    write(
        &cli.join(format!("{uuid}.jsonl")),
        r#"{"version":"v1","kind":"Prompt","data":{"content":[{"kind":"text","data":"hi"}],"meta":{"timestamp":1785939877}}}"#,
    );

    let connector = KiroConnector::new();
    let convs = connector
        .scan(&ScanContext::local_default(cli.clone(), None))
        .unwrap();

    assert_eq!(convs.len(), 1);
    // external_id is the session path relative to the scan root, extension
    // stripped -- here the bare uuid.
    assert_eq!(convs[0].external_id.as_deref(), Some(uuid));
    assert_eq!(convs[0].source_path, cli.join(format!("{uuid}.jsonl")));
}

#[test]
fn kiro_connector_handles_multiple_sessions() {
    let tmp = TempDir::new().unwrap();
    let cli = kiro_cli_dir(&tmp);

    for i in 1..=3 {
        let record = serde_json::json!({
            "version": "v1",
            "kind": "Prompt",
            "data": {
                "content": [{"kind": "text", "data": format!("message {i}")}],
                "meta": {"timestamp": 1_785_939_900 + i},
            },
        })
        .to_string();
        write(&cli.join(format!("sess-{i}.jsonl")), &record);
    }

    let connector = KiroConnector::new();
    let convs = connector
        .scan(&ScanContext::local_default(cli, None))
        .unwrap();

    assert_eq!(convs.len(), 3);
    // Deterministic (sorted) traversal.
    assert_eq!(
        convs[0].external_id.as_deref(),
        Some("sess-1"),
        "sessions must be enumerated in deterministic sorted order"
    );
}

#[test]
fn kiro_connector_parses_millis_and_seconds_timestamps() {
    let tmp = TempDir::new().unwrap();
    let cli = kiro_cli_dir(&tmp);

    // First prompt uses epoch seconds, second already looks like epoch ms.
    write(
        &cli.join("mixed-ts.jsonl"),
        &[
            r#"{"version":"v1","kind":"Prompt","data":{"content":[{"kind":"text","data":"seconds ts"}],"meta":{"timestamp":1785939877}}}"#,
            r#"{"version":"v1","kind":"Prompt","data":{"content":[{"kind":"text","data":"millis ts"}],"meta":{"timestamp":1785939999000}}}"#,
        ]
        .join("\n"),
    );

    let connector = KiroConnector::new();
    let convs = connector
        .scan(&ScanContext::local_default(cli, None))
        .unwrap();

    assert_eq!(convs.len(), 1);
    let conv = &convs[0];
    // seconds -> scaled to ms; ms -> passed through unscaled.
    assert_eq!(conv.messages[0].created_at, Some(1_785_939_877_000));
    assert_eq!(conv.messages[1].created_at, Some(1_785_939_999_000));
}

// ---------------------------------------------------------------------------
// Discovery + incremental (`since_ts`) behavior
// ---------------------------------------------------------------------------

#[test]
fn kiro_connector_discovers_log_and_sidecar() {
    let tmp = TempDir::new().unwrap();
    let cli = kiro_cli_dir(&tmp);
    let uuid = "bbbb2222-cccc-3333-dddd-444455556666";
    write(
        &cli.join(format!("{uuid}.jsonl")),
        r#"{"version":"v1","kind":"Prompt","data":{"content":[{"kind":"text","data":"hi"}],"meta":{"timestamp":1785939877}}}"#,
    );
    write(&cli.join(format!("{uuid}.json")), r#"{"session_id":"s"}"#);

    let connector = KiroConnector::new();
    let ctx = ScanContext::local_default(cli, None);
    let discovered = connector.discover_source_files(&ctx).unwrap();

    assert_eq!(discovered.len(), 2);
    assert!(discovered.iter().all(|d| d.provider_slug == "kiro"));
    assert!(
        discovered
            .iter()
            .any(|d| d.role == DiscoveredSourceRole::PrimarySessionLog
                && d.required_for_reconstruction),
        "the .jsonl log must be the primary, required source"
    );
    assert!(
        discovered
            .iter()
            .any(|d| d.role == DiscoveredSourceRole::MetadataSidecar
                && !d.required_for_reconstruction),
        "the .json sidecar must be an optional metadata source"
    );

    // Discovery must cover every scanned source path.
    for conv in connector.scan(&ctx).unwrap() {
        assert!(
            discovered.iter().any(|d| d.source_path == conv.source_path),
            "every scanned conversation path must be discoverable"
        );
    }
}

#[test]
fn kiro_connector_does_not_treat_substring_only_path_as_storage() {
    let tmp = TempDir::new().unwrap();
    let misleading = tmp.path().join("kiroshi-data");
    fs::create_dir_all(&misleading).unwrap();
    write(
        &misleading.join("unrelated.jsonl"),
        r#"{"version":"v1","kind":"Prompt","data":{"content":[{"kind":"text","data":"must not scan"}],"meta":{"timestamp":1785939877}}}"#,
    );

    let connector = KiroConnector::new();
    let ctx = ScanContext {
        data_dir: tmp.path().join("cass-data"),
        scan_roots: vec![coding_agent_search::connectors::ScanRoot::local(misleading)],
        since_ts: None,
        progress_tick: None,
    };
    assert!(connector.scan(&ctx).unwrap().is_empty());
}

#[test]
fn kiro_connector_parent_root_does_not_duplicate_nested_store() {
    let tmp = TempDir::new().unwrap();
    let cli = kiro_cli_dir(&tmp);
    write(
        &cli.join("one.jsonl"),
        r#"{"version":"v1","kind":"Prompt","data":{"content":[{"kind":"text","data":"once"}],"meta":{"timestamp":1785939877}}}"#,
    );

    let connector = KiroConnector::new();
    let ctx = ScanContext {
        data_dir: tmp.path().join("cass-data"),
        scan_roots: vec![coding_agent_search::connectors::ScanRoot::local(
            tmp.path().to_path_buf(),
        )],
        since_ts: None,
        progress_tick: None,
    };
    assert_eq!(connector.scan(&ctx).unwrap().len(), 1);
}

#[test]
fn kiro_connector_explicit_file_root_scans_only_that_file() {
    let tmp = TempDir::new().unwrap();
    let cli = kiro_cli_dir(&tmp);
    for name in ["selected", "sibling"] {
        write(
            &cli.join(format!("{name}.jsonl")),
            &format!(
                r#"{{"version":"v1","kind":"Prompt","data":{{"content":[{{"kind":"text","data":"{name}"}}],"meta":{{"timestamp":1785939877}}}}}}"#
            ),
        );
    }

    let selected = cli.join("selected.jsonl");
    let connector = KiroConnector::new();
    let ctx = ScanContext {
        data_dir: tmp.path().join("cass-data"),
        scan_roots: vec![coding_agent_search::connectors::ScanRoot::local(
            selected.clone(),
        )],
        since_ts: None,
        progress_tick: None,
    };
    let conversations = connector.scan(&ctx).unwrap();
    assert_eq!(conversations.len(), 1);
    assert_eq!(conversations[0].source_path, selected);
}

#[test]
fn kiro_connector_respects_since_ts_at_file_level() {
    let tmp = TempDir::new().unwrap();
    let cli = kiro_cli_dir(&tmp);
    write(
        &cli.join("session.jsonl"),
        r#"{"version":"v1","kind":"Prompt","data":{"content":[{"kind":"text","data":"old"}],"meta":{"timestamp":1000}}}"#,
    );

    let connector = KiroConnector::new();

    // A since_ts far in the future => the file's mtime predates it => skipped.
    let future = now_ms().saturating_add(60_000);
    assert!(
        connector
            .scan(&ScanContext::local_default(cli.clone(), Some(future)))
            .unwrap()
            .is_empty(),
        "a file older than since_ts must be skipped"
    );

    // since_ts in the distant past => the file is (re-)ingested wholesale.
    let convs = connector
        .scan(&ScanContext::local_default(cli, Some(1)))
        .unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].messages.len(), 1);
}

// ---------------------------------------------------------------------------
// Committed real-shape fixture
// ---------------------------------------------------------------------------

#[test]
fn kiro_connector_parses_committed_fixture() {
    let fixture = kiro_fixture_dir();
    let expected_log = fixture.join("7c9e6a10-1111-2222-3333-444455556666.jsonl");

    let connector = KiroConnector::new();
    let convs = connector
        .scan(&ScanContext::local_default(fixture, None))
        .unwrap();

    let conv = convs
        .into_iter()
        .find(|c| c.source_path == expected_log)
        .expect("committed Kiro fixture should be discoverable");

    assert_eq!(conv.agent_slug, "kiro");
    assert_eq!(
        conv.external_id.as_deref(),
        Some("7c9e6a10-1111-2222-3333-444455556666")
    );
    assert_eq!(conv.title.as_deref(), Some("Add a Kiro connector"));
    assert_eq!(
        conv.workspace.as_deref(),
        Some(Path::new("/Users/dev/coding_agent_session_search"))
    );
    assert_eq!(
        conv.metadata.get("model_id").and_then(Value::as_str),
        Some("claude-opus-4.8")
    );

    // user prompt, assistant (thinking excluded), assistant tool-use, tool result.
    assert_eq!(conv.messages.len(), 4);
    assert_eq!(conv.messages[0].role, "user");
    assert_eq!(conv.messages[1].role, "assistant");
    assert!(
        !conv.messages[1]
            .content
            .contains("private chain of thought"),
        "thinking blocks must never be indexed"
    );
    assert!(conv.messages[2].content.contains("[Tool: execute_bash]"));
    assert_eq!(conv.messages[2].invocations.len(), 1);
    assert_eq!(conv.messages[3].role, "tool");
    assert!(conv.messages[3].content.contains("12 passed"));

    // Timestamps present and nondecreasing across the whole conversation.
    let mut prev = i64::MIN;
    for m in &conv.messages {
        let ts = m.created_at.expect("every message must carry a timestamp");
        assert!(ts >= prev, "timestamps must be monotonically nondecreasing");
        prev = ts;
    }
}

// ---------------------------------------------------------------------------
// Detection (probes the real `~/.kiro/sessions/cli` via $HOME) + registration
// ---------------------------------------------------------------------------

/// Run `body` with `$HOME` pointed at `home`, restoring the prior value after.
fn with_home<T>(home: &Path, body: impl FnOnce() -> T) -> T {
    let prev = std::env::var_os("HOME");
    // Safe in test scope: serialized via #[serial], restored below.
    unsafe {
        std::env::set_var("HOME", home);
    }
    let out = body();
    unsafe {
        match prev {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }
    out
}

#[test]
#[serial]
fn kiro_connector_detect_with_session_store() {
    let tmp = TempDir::new().unwrap();
    let cli = tmp.path().join(".kiro").join("sessions").join("cli");
    fs::create_dir_all(&cli).unwrap();
    write(
        &cli.join("s.jsonl"),
        r#"{"version":"v1","kind":"Prompt","data":{"content":[{"kind":"text","data":"hi"}],"meta":{"timestamp":1785939877}}}"#,
    );

    let result = with_home(tmp.path(), || KiroConnector::new().detect());
    assert!(result.detected, "a populated CLI store must be detected");
    assert!(
        !result.evidence.is_empty(),
        "detection must surface human-readable evidence"
    );
    assert!(result.root_paths.iter().any(|p| p == &cli));
}

#[test]
#[serial]
fn kiro_connector_detect_without_session_store() {
    let tmp = TempDir::new().unwrap();
    // No ~/.kiro/sessions/cli under this HOME.
    let result = with_home(tmp.path(), || KiroConnector::new().detect());
    assert!(!result.detected);
    assert!(result.root_paths.is_empty());
}

#[test]
fn kiro_connector_is_registered_in_factory_registry() {
    let slugs: Vec<&str> = coding_agent_search::indexer::get_connector_factories()
        .into_iter()
        .map(|(slug, _)| slug)
        .collect();
    assert!(
        slugs.contains(&"kiro"),
        "kiro must be a registered ingest connector, got: {slugs:?}"
    );
}
