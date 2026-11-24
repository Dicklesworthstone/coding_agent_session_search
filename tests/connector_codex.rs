use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

use coding_agent_search::connectors::{Connector, ScanContext, ScanRoot, codex::CodexConnector};
use serde_json::Value;

fn codex_real_fixture_home() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex_real")
}

#[test]
fn codex_indexes_native_web_actions_and_pairs_completion_by_call_id() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("rollout-web-search.jsonl");
    let body = r#"{"type":"session_meta","payload":{"id":"native-web","cwd":"/synthetic/work"}}
{"type":"event_msg","payload":{"type":"user_message","message":"Find the API guide"}}
{"type":"event_msg","payload":{"type":"web_search_end","call_id":"ws_one","query":"API guide","results":[{"content":"OPAQUE_FIXTURE_RESULT"}]}}
{"type":"response_item","payload":{"type":"web_search_call","id":"ws_one","action":{"type":"search","query":"API guide","queries":["API guide","café API reference"]},"encrypted_content":"OPAQUE_FIXTURE_CIPHER"}}
{"type":"response_item","payload":{"type":"web_search_call","id":"ws_two","action":{"type":"search","query":"API guide"}}}
{"type":"event_msg","payload":{"type":"web_search_end","call_id":"ws_open","action":{"type":"open_page","url":"https://example.org/docs"}}}
{"type":"response_item","payload":{"type":"web_search_call","action":{"type":"find_in_page","url":"https://example.org/docs","pattern":"request timeout","internal":"OPAQUE_FIXTURE_ACTION"}}}
{"type":"response_item","payload":{"type":"web_search_call","action":{"type":"find_in_page","url":"https://example.org/docs","pattern":"request timeout"}}}
{"type":"response_item","payload":{"type":"web_search_call","id":"ws_find","action":{"type":"find_in_page","url":"https://example.org/docs","pattern":"request timeout"}}}
{"type":"response_item","payload":{"type":"web_search_call","id":"ws_pending","status":"in_progress"}}
{"type":"response_item","payload":{"type":"web_search_call","action":{"type":"future","opaque":"OPAQUE_FIXTURE_ACTION"}}}
"#;
    fs::write(&path, body).unwrap();
    let ctx = ScanContext::with_roots(
        root.path().to_path_buf(),
        vec![ScanRoot::local(path.clone())],
        None,
    );
    let conversations = CodexConnector::new().scan(&ctx).unwrap();
    assert_eq!(conversations.len(), 1);
    let messages = &conversations[0].messages;
    let calls: Vec<_> = messages
        .iter()
        .filter(|message| {
            message
                .invocations
                .iter()
                .any(|call| call.name == "web_search")
        })
        .collect();
    assert_eq!(calls.len(), 6);
    let find = |id| {
        calls
            .iter()
            .copied()
            .find(|message| message.invocations[0].call_id.as_deref() == Some(id))
            .expect("native call ID")
    };
    assert!(find("ws_one").content.contains("café API reference"));
    assert!(find("ws_two").content.contains("API guide"));
    assert!(find("ws_open").content.contains("https://example.org/docs"));
    assert_eq!(
        calls
            .iter()
            .filter(|message| message.content.contains("request timeout"))
            .count(),
        3,
        "both ID-less occurrences and the separate native call survive"
    );
    assert!(
        !serde_json::to_string(messages)
            .unwrap()
            .contains("OPAQUE_FIXTURE")
    );
    assert_eq!(fs::read_to_string(path).unwrap(), body);
}

#[test]
fn codex_tool_output_rows_survive_the_real_large_session_compaction_threshold() {
    use franken_agent_detection::connectors::{SourceCompletion, SourceScanHooks};
    use std::io::Write;

    const COMPACT_THRESHOLD_BYTES: u64 = 32 * 1024 * 1024;
    let root = TempDir::new().unwrap();
    let path = root.path().join("rollout-compacted-outputs.jsonl");
    let output = " \tunchanged café 🦀\r\n ";
    let long_output = format!("  {}\n", "🦀".repeat(128 * 1024 + 1));
    let repeated = serde_json::json!({
        "type": "response_item", "timestamp": "2026-06-28T10:00:02Z",
        "payload": {"type": "function_call_output", "call_id": "first", "output": output}
    });
    let records = [
        serde_json::json!({"type":"session_meta","timestamp":"2026-06-28T10:00:00Z","payload":{"id":"compacted-outputs","cwd":"/synthetic/compacted-outputs"}}),
        serde_json::json!({"type":"response_item","timestamp":"2026-06-28T10:00:01Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"retain every tool result"}]}}),
        repeated.clone(),
        repeated,
        serde_json::json!({"type":"response_item","timestamp":"2026-06-28T10:00:02Z","payload":{"type":"function_call_output","call_id":"distinct","output":output}}),
        serde_json::json!({"type":"response_item","timestamp":"2026-06-28T10:00:03Z","payload":{"type":"function_call_output","call_id":"long","output":long_output}}),
        serde_json::json!({"type":"event_msg","timestamp":"2026-06-28T10:00:04Z","payload":{"type":"tool_result","call_id":"event-first","output":output}}),
        serde_json::json!({"type":"event_msg","timestamp":"2026-06-28T10:00:04Z","payload":{"type":"tool_result","call_id":"event-second","output":output}}),
    ];
    let body = records
        .iter()
        .map(|raw| format!("{raw}\n"))
        .collect::<String>();
    fs::write(&path, &body).unwrap();
    let context = ScanContext::with_roots(
        root.path().to_path_buf(),
        vec![ScanRoot::local(path.clone())],
        None,
    );
    let connector = CodexConnector::new();
    let small = connector.scan(&context).unwrap();
    assert_eq!(small.len(), 1);
    let messages = &small[0].messages;
    assert_eq!(messages.len(), 7);
    assert_eq!(messages[0].content, "retain every tool result");
    for message in &messages[1..4] {
        assert_eq!(message.content, output);
    }
    assert_eq!(messages[4].content, long_output);
    for message in &messages[1..5] {
        assert_eq!(
            message
                .extra
                .pointer("/payload/type")
                .and_then(Value::as_str),
            Some("function_call_output")
        );
    }
    for (index, call_id) in [(5, "event-first"), (6, "event-second")] {
        assert_eq!(
            messages[index].content,
            format!("[Tool output: {call_id}]\n{}", output.trim())
        );
    }

    // Cross the upstream threshold without one giant JSON value or changing
    // any message. Unknown records are ignored by both parser passes.
    let small_len = fs::metadata(&path).unwrap().len();
    assert!(small_len < COMPACT_THRESHOLD_BYTES);
    let padding = format!(
        "{{\"type\":\"fixture_padding\",\"padding\":\"{}\"}}\n",
        "x".repeat(64 * 1024)
    );
    let padding_count = (COMPACT_THRESHOLD_BYTES - small_len).div_ceil(padding.len() as u64);
    let mut writer = fs::OpenOptions::new().append(true).open(&path).unwrap();
    for _ in 0..padding_count {
        writer.write_all(padding.as_bytes()).unwrap();
    }
    drop(writer);
    let before = fs::metadata(&path).unwrap();
    assert!(before.len() >= COMPACT_THRESHOLD_BYTES);
    let mut large = Vec::new();
    let mut completions = 0;
    {
        let mut complete = |completion: &SourceCompletion| {
            assert_eq!(completion.source.source_path, path);
            completions += 1;
            Ok(())
        };
        let mut hooks = SourceScanHooks {
            should_scan_source: None,
            on_source_complete: Some(&mut complete),
        };
        connector
            .scan_with_source_boundaries(&context, &mut hooks, &mut |conversation| {
                large.push(conversation);
                Ok(())
            })
            .unwrap();
    }
    assert_eq!(large.len(), 1);
    assert_eq!(completions, 1);
    assert_eq!(large[0].source_path, path);
    assert_eq!(large[0].messages.len(), 7);
    for message in &large[0].messages[1..5] {
        assert_eq!(
            message.extra,
            serde_json::json!({}),
            "exercise real primary-parser compaction, not a simulated extra"
        );
    }
    for message in &large[0].messages[5..] {
        assert_eq!(
            message
                .extra
                .pointer("/payload/type")
                .and_then(Value::as_str),
            Some("tool_result")
        );
    }
    let without_extra = |mut messages: Value| {
        for message in messages.as_array_mut().unwrap() {
            message.as_object_mut().unwrap().remove("extra");
        }
        messages
    };
    assert_eq!(
        without_extra(serde_json::to_value(&large[0].messages).unwrap()),
        without_extra(serde_json::to_value(&small[0].messages).unwrap()),
        "compaction must preserve exact text, occurrence count, and message identity"
    );
    let after = fs::metadata(&path).unwrap();
    assert_eq!(after.len(), before.len());
    assert_eq!(after.modified().unwrap(), before.modified().unwrap());
}

#[test]
fn gh560_inactive_cut_tail_keeps_complete_turns_and_can_later_finish() {
    use franken_agent_detection::connectors::{SourceCompletion, SourceScanHooks};
    use std::io::Write;
    use std::time::{Duration, SystemTime};

    let root = TempDir::new().unwrap();
    let sessions = root.path().join("sessions");
    fs::create_dir(&sessions).unwrap();
    let path = sessions.join("rollout-gh560.jsonl");
    let records = [
        serde_json::json!({"type":"session_meta","payload":{"id":"gh560","cwd":"/synthetic/gh560"}}),
        serde_json::json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"kilo marker five"}]}}),
        serde_json::json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"lima marker six"}]}}),
    ];
    let tail = serde_json::json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"mike marker seven"}]}}).to_string();
    let split = tail.len() / 2;
    let bytes = format!(
        "{}\n{}\n{}\n{}",
        records[0],
        records[1],
        records[2],
        &tail[..split]
    );
    fs::write(&path, &bytes).unwrap();
    fs::File::open(&path)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(7_200))
        .unwrap();
    let before = fs::metadata(&path).unwrap();
    let context = ScanContext::with_roots(
        root.path().to_path_buf(),
        vec![ScanRoot::local(path.clone())],
        None,
    );
    let connector = CodexConnector::new();
    // All public routes must publish the complete history and a successful
    // boundary; a repair that only fixes collected scans is insufficient.
    for route in 0..3 {
        let mut conversations = Vec::new();
        let mut completions = 0;
        match route {
            0 => conversations = connector.scan(&context).unwrap(),
            1 => connector
                .scan_with_callback(&context, &mut |conversation| {
                    conversations.push(conversation);
                    Ok(())
                })
                .unwrap(),
            _ => {
                let mut complete = |completion: &SourceCompletion| {
                    assert_eq!(completion.source.source_path, path);
                    completions += 1;
                    Ok(())
                };
                let mut hooks = SourceScanHooks {
                    should_scan_source: None,
                    on_source_complete: Some(&mut complete),
                };
                connector
                    .scan_with_source_boundaries(&context, &mut hooks, &mut |conversation| {
                        conversations.push(conversation);
                        Ok(())
                    })
                    .unwrap();
            }
        }
        assert_eq!(conversations.len(), 1, "route {route}");
        let messages = &conversations[0].messages;
        assert_eq!(messages.len(), 2, "route {route}");
        assert_eq!(messages[0].content, "kilo marker five");
        assert_eq!(messages[1].content, "lima marker six");
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(conversations[0].source_path, path);
        if route == 2 {
            assert_eq!(completions, 1);
        }
        assert_eq!(fs::read_to_string(&path).unwrap(), bytes);
        assert_eq!(
            fs::metadata(&path).unwrap().modified().unwrap(),
            before.modified().unwrap()
        );
    }

    // A later writer can complete that same record. Recovery never rewrites
    // the source or fabricates a newline, and the completed turn is retained.
    let mut writer = fs::OpenOptions::new().append(true).open(&path).unwrap();
    writer.write_all(&tail.as_bytes()[split..]).unwrap();
    writer.write_all(b"\n").unwrap();
    drop(writer);
    let completed = fs::read(&path).unwrap();
    let conversations = connector.scan(&context).unwrap();
    assert_eq!(conversations.len(), 1);
    assert_eq!(conversations[0].messages.len(), 3);
    assert_eq!(conversations[0].messages[2].content, "mike marker seven");
    assert_eq!(fs::read(&path).unwrap(), completed);
}

#[test]
fn gh553_codex_recovers_a_complete_turn_after_a_torn_prefix() {
    let root = tempfile::tempdir().unwrap().keep();
    let sessions = root.join("sessions");
    fs::create_dir(&sessions).unwrap();
    let path = sessions.join("rollout-gh553.jsonl");
    let body = concat!(
        "{\"type\":\"session_meta\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"payload\":{\"id\":\"gh553\",\"cwd\":\"/synthetic/gh553\"}}\n",
        "{\"payload\":\"torn",
        "{\"type\":\"response_item\",\"timestamp\":\"2026-01-01T00:00:01Z\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"bravo recovered codex\"}]}}\n",
        "{\"payload\":{\"role\":\"user\",\"content\":\"phantom marker\"}\n",
    );
    fs::write(&path, body).unwrap();
    let context = ScanContext::with_roots(root.clone(), vec![ScanRoot::local(root)], None);
    let conversations = CodexConnector::new().scan(&context).unwrap();
    assert_eq!(conversations.len(), 1);
    assert_eq!(conversations[0].messages.len(), 1);
    assert_eq!(
        conversations[0].messages[0].content,
        "bravo recovered codex"
    );
    assert_eq!(conversations[0].messages[0].role, "user");
    assert_eq!(fs::read_to_string(path).unwrap(), body);
}

#[test]
fn gh553_codex_enrichment_recovers_modern_records_after_a_torn_prefix() {
    use franken_agent_detection::connectors::{SourceCompletion, SourceScanHooks};

    let patch =
        "*** Begin Patch\n*** Add File: recovered-caf\u{e9}.rs\n+fn recovered() {}\n*** End Patch";
    let records = [
        serde_json::json!({"type":"session_meta","timestamp":"2026-01-01T00:00:00Z","payload":{"id":"gh553-enrichment","cwd":"/synthetic/gh553"}}),
        serde_json::json!({"type":"response_item","timestamp":"2026-01-01T00:00:01Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"alpha retained user"}]}}),
        serde_json::json!({"type":"event_msg","timestamp":"2026-01-01T00:00:02Z","payload":{"type":"agent_message","message":"charlie recovered assistant"}}),
        serde_json::json!({"type":"event_msg","timestamp":"2026-01-01T00:00:03Z","payload":{"type":"tool_result","call_id":"call-1","output":"delta recovered tool result"}}),
        serde_json::json!({"type":"response_item","timestamp":"2026-01-01T00:00:04Z","payload":{"type":"custom_tool_call","name":"apply_patch","call_id":"patch-1","input":patch}}),
    ];

    // In a settled rollout each suffix object is complete even when its damaged
    // physical line has no final newline. Active prefixes separately require
    // LF (codex_stable_prefix.rs). The primary parser retains the custom call;
    // enrichment must add its input and the two event-only messages once.
    for ending in ["\n", "\r\n", ""] {
        let root = TempDir::new().unwrap();
        let path = root.path().join("rollout-gh553-enrichment.jsonl");
        let body = format!(
            "{}\n{}\n{{\"payload\":\"torn{}{}{}{ending}",
            records[0], records[1], records[2], records[3], records[4]
        );
        fs::write(&path, &body).unwrap();
        fs::File::open(&path)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(7200))
            .unwrap();
        let before = fs::metadata(&path).unwrap().modified().unwrap();
        let context = ScanContext::with_roots(
            root.path().to_path_buf(),
            vec![ScanRoot::local(path.clone())],
            None,
        );
        let connector = CodexConnector::new();
        for route in 0..3 {
            let mut conversations = Vec::new();
            let mut completions = 0;
            match route {
                0 => conversations = connector.scan(&context).unwrap(),
                1 => connector
                    .scan_with_callback(&context, &mut |conversation| {
                        conversations.push(conversation);
                        Ok(())
                    })
                    .unwrap(),
                _ => {
                    let mut complete = |completion: &SourceCompletion| {
                        assert_eq!(completion.source.source_path, path);
                        completions += 1;
                        Ok(())
                    };
                    let mut hooks = SourceScanHooks {
                        should_scan_source: None,
                        on_source_complete: Some(&mut complete),
                    };
                    connector
                        .scan_with_source_boundaries(&context, &mut hooks, &mut |conversation| {
                            conversations.push(conversation);
                            Ok(())
                        })
                        .unwrap();
                }
            }
            assert_eq!(conversations.len(), 1, "ending {ending:?}, route {route}");
            let conversation = &conversations[0];
            assert_eq!(conversation.source_path, path);
            let messages = &conversation.messages;
            assert_eq!(messages.len(), 4, "ending {ending:?}, route {route}");
            assert_eq!(messages[0].content, "alpha retained user");
            assert_eq!(messages[0].role, "user");
            assert_eq!(messages[1].content, "charlie recovered assistant");
            assert_eq!(messages[1].role, "assistant");
            assert_eq!(
                messages[2].content,
                "[Tool output: call-1]\ndelta recovered tool result"
            );
            assert_eq!(messages[2].role, "tool");
            assert_eq!(messages[3].content, format!("[Tool: apply_patch]\n{patch}"));
            assert_eq!(messages[3].role, "assistant");
            assert_eq!(messages[3].invocations.len(), 1);
            let invocation = &messages[3].invocations[0];
            assert_eq!(invocation.name, "apply_patch");
            assert_eq!(invocation.call_id.as_deref(), Some("patch-1"));
            assert_eq!(
                invocation.arguments.as_ref(),
                Some(&Value::String(patch.to_string()))
            );
            for (index, message) in messages.iter().enumerate() {
                assert_eq!(message.idx, index as i64);
            }
            if route == 2 {
                assert_eq!(completions, 1);
            }
            assert_eq!(fs::read_to_string(&path).unwrap(), body);
            assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), before);
        }
    }
}

#[test]
fn gh553_codex_enrichment_rejects_nested_or_incomplete_suffix_records() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("rollout-gh553-rejected.jsonl");
    let user = serde_json::json!({"type":"response_item","timestamp":"2026-01-01T00:00:01Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"retained user"}]}});
    let assistant = serde_json::json!({"type":"event_msg","timestamp":"2026-01-01T00:00:02Z","payload":{"type":"agent_message","message":"retained intact assistant"}});
    let phantom = serde_json::json!({"type":"event_msg","timestamp":"2026-01-01T00:00:03Z","payload":{"type":"agent_message","message":"phantom assistant"}}).to_string();
    let rejected = [
        format!("{{\"nested\":{phantom}"),
        format!("[{phantom}"),
        format!("torn{phantom} trailing garbage"),
        format!("torn{phantom}{{\"unknown\":true}}"),
        format!("torn{}", &phantom[..phantom.len() / 2]),
    ];
    let body = format!("{user}\n{}\n{assistant}\n", rejected.join("\n"));
    fs::write(&path, &body).unwrap();
    let context = ScanContext::with_roots(
        root.path().to_path_buf(),
        vec![ScanRoot::local(path.clone())],
        None,
    );
    let conversations = CodexConnector::new().scan(&context).unwrap();
    assert_eq!(conversations.len(), 1);
    assert_eq!(conversations[0].messages.len(), 2);
    assert_eq!(conversations[0].messages[0].content, "retained user");
    assert_eq!(
        conversations[0].messages[1].content,
        "retained intact assistant"
    );
    assert_eq!(fs::read_to_string(path).unwrap(), body);
}

#[test]
fn gh553_codex_enrichment_keeps_whitespace_in_the_recovery_budget() {
    use franken_agent_detection::connectors::jsonl::MAX_JSONL_RECOVERY_LINE_BYTES;

    let root = TempDir::new().unwrap();
    let path = root.path().join("rollout-gh553-over-budget.jsonl");
    let phantom = serde_json::json!({"type":"event_msg","timestamp":"2026-01-01T00:00:01Z","payload":{"type":"agent_message","message":"over-budget phantom"}});
    let user = serde_json::json!({"type":"response_item","timestamp":"2026-01-01T00:00:02Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"retained user"}]}});
    let body = format!(
        "\u{feff}{}torn{phantom}\n{user}\n",
        " ".repeat(MAX_JSONL_RECOVERY_LINE_BYTES)
    );
    fs::write(&path, &body).unwrap();
    let context = ScanContext::with_roots(
        root.path().to_path_buf(),
        vec![ScanRoot::local(path.clone())],
        None,
    );
    let conversations = CodexConnector::new().scan(&context).unwrap();
    assert_eq!(conversations.len(), 1);
    assert_eq!(conversations[0].messages.len(), 1);
    assert_eq!(conversations[0].messages[0].content, "retained user");
    assert_eq!(fs::read_to_string(path).unwrap(), body);
}

#[test]
fn codex_connector_reads_modern_envelope_jsonl() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/2025/11/21");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-1.jsonl");

    // Modern envelope format with {type, timestamp, payload}
    let sample = r#"{"timestamp":"2025-09-30T15:42:34.559Z","type":"session_meta","payload":{"id":"test-id","cwd":"/test/workspace","cli_version":"0.42.0"}}
{"timestamp":"2025-09-30T15:42:36.190Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"write a hello program"}]}}
{"timestamp":"2025-09-30T15:42:43.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"text","text":"here is code"}]}}
"#;
    fs::write(&file, sample).unwrap();

    let connector = CodexConnector::new();
    // qu81y: the tempdir codex home is passed as an EXPLICIT scan root
    // instead of mutating process-global CODEX_HOME.
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    let c = &convs[0];
    assert_eq!(c.agent_slug, "codex");
    assert_eq!(c.messages.len(), 2);
    assert!(c.title.as_ref().unwrap().contains("write a hello program"));
    // Verify workspace was extracted from session_meta
    assert_eq!(c.workspace, Some(PathBuf::from("/test/workspace")));
    // Bead 7k7pl: pin timestamps parsed from the 2025-09-30 ISO-8601
    // fixture — both must be plausible ms-epoch values (>= 2020) and
    // started must be strictly before ended. A regression that
    // dropped ISO parsing (emitting 0 / default) would slip past
    // `.is_some()`.
    let started = c
        .started_at
        .expect("started_at must be parsed from ISO-8601");
    let ended = c.ended_at.expect("ended_at must be parsed from ISO-8601");
    // ms-epoch floor: 2020-01-01 (1_577_836_800_000 ms). Fixture is
    // 2025 so this is well below the real value but catches 0/MIN.
    assert!(
        started >= 1_577_836_800_000,
        "started_at must be parsed from the 2025 fixture (>= 2020); got {started}"
    );
    assert!(
        started < ended,
        "started_at must be strictly before ended_at; got started={started}, ended={ended}"
    );
}

#[test]
fn codex_connector_includes_agent_reasoning() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/2025/11/22");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-reasoning.jsonl");

    let sample = r#"{"timestamp":"2025-09-30T15:42:34.559Z","type":"session_meta","payload":{"id":"test-id","cwd":"/test"}}
{"timestamp":"2025-09-30T15:42:36.190Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"solve this problem"}]}}
{"timestamp":"2025-09-30T15:42:40.000Z","type":"event_msg","payload":{"type":"agent_reasoning","text":"Let me think about this carefully..."}}
{"timestamp":"2025-09-30T15:42:43.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"text","text":"here is solution"}]}}
{"timestamp":"2025-09-30T15:42:45.000Z","type":"event_msg","payload":{"type":"token_count","input_tokens":100,"output_tokens":200}}
"#;
    fs::write(&file, sample).unwrap();

    let connector = CodexConnector::new();
    // qu81y: the tempdir codex home is passed as an EXPLICIT scan root
    // instead of mutating process-global CODEX_HOME.
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    let c = &convs[0];

    // Should have 3 messages: user, reasoning, assistant
    // (token_count does not create a synthetic message)
    assert_eq!(c.messages.len(), 3);

    // Check reasoning is included with correct author tag
    let reasoning = c
        .messages
        .iter()
        .find(|m| m.author.as_deref() == Some("reasoning"));
    assert!(reasoning.is_some());
    assert!(
        reasoning
            .unwrap()
            .content
            .contains("think about this carefully")
    );

    let assistant = c.messages.iter().find(|m| {
        m.role == "assistant" && m.author.is_none() && m.content.contains("here is solution")
    });
    assert!(assistant.is_some());
    let assistant = assistant.unwrap();
    assert_eq!(
        assistant
            .extra
            .pointer("/cass/token_usage/input_tokens")
            .and_then(|v| v.as_i64()),
        Some(100)
    );
    assert_eq!(
        assistant
            .extra
            .pointer("/cass/token_usage/output_tokens")
            .and_then(|v| v.as_i64()),
        Some(200)
    );
}

#[test]
fn codex_connector_parses_real_tool_call_fixture() {
    let fixture_home = codex_real_fixture_home();
    let expected_path = fixture_home.join("sessions/2025/11/26/rollout-tool-call.jsonl");

    let connector = CodexConnector::new();
    // qu81y: committed fixture home passed as an explicit scan root.
    let ctx = ScanContext::with_roots(
        fixture_home.clone(),
        vec![ScanRoot::local(fixture_home.clone())],
        None,
    );
    let convs = connector.scan(&ctx).unwrap();
    let conv = convs
        .into_iter()
        .find(|conv| conv.source_path == expected_path)
        .expect("real Codex tool-call fixture should be discoverable");

    assert_eq!(conv.agent_slug, "codex");
    assert_eq!(conv.workspace, Some(PathBuf::from("/test/soldier/project")));
    assert_eq!(
        conv.external_id,
        Some("2025/11/26/rollout-tool-call".to_string())
    );
    assert_eq!(
        conv.title,
        Some(
            "Please trace the tool_call branch in the Codex connector and confirm invocation extraction."
                .to_string()
        )
    );
    assert_eq!(conv.messages.len(), 3);

    let tool_msg = &conv.messages[1];
    assert_eq!(tool_msg.idx, 1);
    assert_eq!(tool_msg.role, "assistant");
    assert_eq!(tool_msg.author, None);
    assert_eq!(tool_msg.content, "[Tool: bash]");
    assert_eq!(tool_msg.invocations.len(), 1);
    assert!(tool_msg.extra.get("payload").is_some());

    let invocation = &tool_msg.invocations[0];
    assert_eq!(invocation.kind, "tool");
    assert_eq!(invocation.name, "bash");
    assert_eq!(invocation.call_id.as_deref(), Some("call_codex_tool_001"));
    assert_eq!(
        invocation
            .arguments
            .as_ref()
            .and_then(|args| args.get("cmd"))
            .and_then(|value| value.as_str()),
        Some("rg -n tool_call src/connectors/codex.rs")
    );
    assert!(
        tool_msg.extra.pointer("/cass/token_usage").is_none(),
        "token usage should attach to the later assistant turn, not the tool_call message"
    );

    let assistant = &conv.messages[2];
    assert_eq!(assistant.idx, 2);
    assert_eq!(assistant.role, "assistant");
    assert!(assistant.content.contains("invocation is emitted"));
    assert_eq!(
        assistant
            .extra
            .pointer("/cass/token_usage/input_tokens")
            .and_then(|v| v.as_i64()),
        Some(120)
    );
    assert_eq!(
        assistant
            .extra
            .pointer("/cass/token_usage/output_tokens")
            .and_then(|v| v.as_i64()),
        Some(45)
    );
}

#[test]
fn codex_connector_indexes_modern_response_items() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/2026/05/08");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-modern.jsonl");

    let sample = r#"{"timestamp":"2026-05-08T23:09:00.000Z","type":"session_meta","payload":{"id":"modern-id","cwd":"/data/projects/ntm","cli_version":"0.49.0"}}
{"timestamp":"2026-05-08T23:09:01.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"investigate cass"}]}}
{"timestamp":"2026-05-08T23:09:02.000Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"git log --grep='bd-2mb03'\"}","call_id":"call-modern-1"}}
{"timestamp":"2026-05-08T23:09:03.000Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call-modern-1","output":"Output:\ncommit abc123 bd-2mb03.6.5 add CASS-backed handoff context enrichment\n"}}
{"timestamp":"2026-05-08T23:09:04.000Z","type":"event_msg","payload":{"type":"agent_message","message":"The raw session contains bd-2mb03."}}
{"timestamp":"2026-05-08T23:09:04.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"The raw session contains bd-2mb03."}],"phase":"commentary"}}
"#;
    fs::write(&file, sample).unwrap();

    let connector = CodexConnector::new();
    // qu81y: the tempdir codex home is passed as an EXPLICIT scan root
    // instead of mutating process-global CODEX_HOME.
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let conv = &convs[0];
    let tool_calls: Vec<_> = conv
        .messages
        .iter()
        .filter(|message| {
            message
                .invocations
                .iter()
                .any(|invocation| invocation.call_id.as_deref() == Some("call-modern-1"))
        })
        .collect();
    assert_eq!(
        tool_calls.len(),
        1,
        "function_call should not be duplicated"
    );
    let tool_call = tool_calls[0];
    assert_eq!(
        tool_call.invocations.len(),
        1,
        "function_call should produce exactly one invocation"
    );
    assert!(
        tool_call.content.contains("git log --grep='bd-2mb03'"),
        "function_call arguments should be searchable"
    );
    let invocation = &tool_call.invocations[0];
    assert_eq!(invocation.name, "exec_command");
    assert_eq!(invocation.call_id.as_deref(), Some("call-modern-1"));
    assert_eq!(
        invocation
            .arguments
            .as_ref()
            .and_then(|arguments| arguments.get("cmd"))
            .and_then(Value::as_str),
        Some("git log --grep='bd-2mb03'")
    );
    assert!(
        conv.messages
            .iter()
            .any(|message| message.role == "tool" && message.content.contains("bd-2mb03.6.5")),
        "function_call_output text should be searchable"
    );
    assert_eq!(
        conv.messages
            .iter()
            .filter(|message| message.content == "The raw session contains bd-2mb03.")
            .count(),
        1,
        "agent_message and output_text duplicates should collapse to one searchable message"
    );
}

#[test]
fn codex_connector_scans_explicit_rollout_file_root() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join(".codex/sessions/2026/05/08");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-explicit.jsonl");

    let sample = r#"{"timestamp":"2026-05-08T23:09:00.000Z","type":"session_meta","payload":{"id":"explicit-id","cwd":"/data/projects/ntm","cli_version":"0.49.0"}}
{"timestamp":"2026-05-08T23:09:01.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"explicit watch once bd-2mb03"}]}}
{"timestamp":"2026-05-08T23:09:02.000Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call-explicit","output":"bd-2mb03 direct file root output\n"}}
"#;
    fs::write(&file, sample).unwrap();

    let connector = CodexConnector::new();
    let ctx = ScanContext {
        data_dir: dir.path().join(".codex"),
        scan_roots: vec![ScanRoot::local(file.clone())],
        since_ts: None,
        progress_tick: None,
    };
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let conv = &convs[0];
    assert_eq!(conv.source_path, file);
    assert_eq!(conv.workspace, Some(PathBuf::from("/data/projects/ntm")));
    assert!(
        conv.messages
            .iter()
            .any(|message| message.content.contains("bd-2mb03 direct file root")),
        "explicit file-root scans must index modern response items"
    );
}

#[test]
fn codex_connector_ignores_unmatched_token_count() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/2025/11/23");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-filter.jsonl");

    let sample = r#"{"timestamp":"2025-09-30T15:42:34.559Z","type":"session_meta","payload":{"id":"test-id","cwd":"/test"}}
{"timestamp":"2025-09-30T15:42:36.190Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}}
{"timestamp":"2025-09-30T15:42:37.000Z","type":"event_msg","payload":{"type":"token_count","input_tokens":10,"output_tokens":20}}
{"timestamp":"2025-09-30T15:42:38.000Z","type":"turn_context","payload":{"turn":1}}
{"timestamp":"2025-09-30T15:42:39.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"text","text":"world"}]}}
"#;
    fs::write(&file, sample).unwrap();

    let connector = CodexConnector::new();
    // qu81y: the tempdir codex home is passed as an EXPLICIT scan root
    // instead of mutating process-global CODEX_HOME.
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    let c = &convs[0];

    // Should only have 2 messages (user, assistant).
    // token_count and turn_context do not create searchable messages.
    assert_eq!(c.messages.len(), 2);

    for msg in &c.messages {
        assert!(!msg.content.contains("token_count"));
        assert!(!msg.content.contains("turn_context"));
        assert!(!msg.content.trim().is_empty());
    }

    // token_count occurs before the first assistant turn and must not attach forward.
    let assistant = c.messages.iter().find(|m| m.role == "assistant").unwrap();
    assert!(
        assistant.extra.pointer("/cass/token_usage").is_none(),
        "unmatched token_count should be ignored"
    );
}

/// Test that since_ts uses FILE-LEVEL filtering, not message-level.
///
/// NOTE: We intentionally removed message-level timestamp filtering because
/// it caused data loss during incremental re-indexing. When a file is modified,
/// ALL messages in that file are ingested, regardless of individual timestamps.
/// The since_ts is ONLY used to decide whether to process the file at all
/// (based on file mtime vs since_ts).
#[test]
fn codex_connector_respects_since_ts_at_file_level_only() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/2025/11/24");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-since.jsonl");

    // Two messages with different timestamps - both should be included
    // since since_ts filtering happens at the FILE level, not message level.
    let sample = r#"{"timestamp":"2025-09-30T15:42:34.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"old msg"}]}}
{"timestamp":1700000100000,"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"text","text":"new msg"}]}}
"#;
    fs::write(&file, sample).unwrap();

    let connector = CodexConnector::new();
    // since_ts does NOT filter individual messages anymore - only whole files
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        Some(1_700_000_000_000),
    );
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    let c = &convs[0];

    // BOTH messages should be present - we don't filter by message timestamp
    assert_eq!(
        c.messages.len(),
        2,
        "file-level filtering means all messages in a processed file are included"
    );
    // Messages should have correct roles
    assert_eq!(c.messages[0].role, "user");
    assert!(c.messages[0].content.contains("old msg"));
    assert_eq!(c.messages[1].role, "assistant");
    assert!(c.messages[1].content.contains("new msg"));
}

/// Test legacy .json format parsing
#[test]
fn codex_connector_reads_legacy_json_format() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/2025/11/25");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-legacy.json");

    // Legacy format: single JSON object with session and items
    let sample = r#"{
        "session": {
            "id": "legacy-session",
            "cwd": "/legacy/workspace"
        },
        "items": [
            {
                "role": "user",
                "timestamp": "2025-09-30T15:42:36.190Z",
                "content": "legacy user message"
            },
            {
                "role": "assistant",
                "timestamp": "2025-09-30T15:42:43.000Z",
                "content": "legacy assistant response"
            }
        ]
    }"#;
    fs::write(&file, sample).unwrap();

    let connector = CodexConnector::new();
    // qu81y: the tempdir codex home is passed as an EXPLICIT scan root
    // instead of mutating process-global CODEX_HOME.
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    assert_eq!(c.agent_slug, "codex");
    assert_eq!(c.messages.len(), 2);
    assert_eq!(c.workspace, Some(PathBuf::from("/legacy/workspace")));

    // Verify metadata indicates legacy format
    assert_eq!(
        c.metadata.get("source").and_then(|v| v.as_str()),
        Some("rollout_json")
    );

    // Check messages
    assert_eq!(c.messages[0].role, "user");
    assert!(c.messages[0].content.contains("legacy user message"));
    assert_eq!(c.messages[1].role, "assistant");
}

/// Test detection with existing sessions directory
#[test]
fn codex_detect_with_sessions_dir() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    // qu81y: detection is exercised through FAD's explicit root-override
    // input (the same root CODEX_HOME=<dir> used to resolve to, i.e.
    // <dir>/sessions) instead of mutating process-global env.
    let report = franken_agent_detection::detect_installed_agents(
        &franken_agent_detection::AgentDetectOptions {
            only_connectors: Some(vec!["codex".to_string()]),
            include_undetected: true,
            root_overrides: vec![franken_agent_detection::AgentDetectRootOverride {
                slug: "codex".to_string(),
                root: sessions,
            }],
        },
    )
    .expect("codex detection report");
    let entry = &report.installed_agents[0];
    assert!(entry.detected);
    assert!(!entry.evidence.is_empty());
}

/// Test detection without sessions directory
#[test]
fn codex_detect_without_sessions_dir() {
    let dir = TempDir::new().unwrap();
    // Don't create the sessions directory: the overridden probe root does
    // not exist, so detection must report not-found.
    let report = franken_agent_detection::detect_installed_agents(
        &franken_agent_detection::AgentDetectOptions {
            only_connectors: Some(vec!["codex".to_string()]),
            include_undetected: true,
            root_overrides: vec![franken_agent_detection::AgentDetectRootOverride {
                slug: "codex".to_string(),
                root: dir.path().join("sessions"),
            }],
        },
    )
    .expect("codex detection report");
    let entry = &report.installed_agents[0];
    assert!(!entry.detected);
}

/// Test `user_message` event type
#[test]
fn codex_connector_handles_user_message_event() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/2025/11/26");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-user-event.jsonl");

    let sample = r#"{"timestamp":"2025-09-30T15:42:34.559Z","type":"session_meta","payload":{"id":"test-id","cwd":"/test"}}
{"timestamp":"2025-09-30T15:42:36.190Z","type":"event_msg","payload":{"type":"user_message","message":"user event message"}}
{"timestamp":"2025-09-30T15:42:43.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"text","text":"assistant reply"}]}}
"#;
    fs::write(&file, sample).unwrap();

    let connector = CodexConnector::new();
    // qu81y: the tempdir codex home is passed as an EXPLICIT scan root
    // instead of mutating process-global CODEX_HOME.
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    assert_eq!(c.messages.len(), 2);

    // First message should be the user event
    assert_eq!(c.messages[0].role, "user");
    assert!(c.messages[0].content.contains("user event message"));
}

/// Test malformed JSONL lines are skipped gracefully
#[test]
fn codex_connector_skips_malformed_lines() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/2025/11/27");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-malformed.jsonl");

    let sample = r#"{"timestamp":"2025-09-30T15:42:34.559Z","type":"session_meta","payload":{"id":"test-id","cwd":"/test"}}
{ this is not valid json
{"timestamp":"2025-09-30T15:42:36.190Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"valid message"}]}}
also not valid
{"timestamp":"2025-09-30T15:42:43.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"text","text":"valid response"}]}}
"#;
    fs::write(&file, sample).unwrap();

    let connector = CodexConnector::new();
    // qu81y: the tempdir codex home is passed as an EXPLICIT scan root
    // instead of mutating process-global CODEX_HOME.
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    // Should have 2 valid messages, malformed lines skipped
    assert_eq!(c.messages.len(), 2);
}

/// Test multiple sessions in separate files
#[test]
fn codex_connector_handles_multiple_sessions() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/2025/11/28");
    fs::create_dir_all(&sessions).unwrap();

    for i in 1..=3 {
        let file = sessions.join(format!("rollout-{i}.jsonl"));
        let sample = format!(
            r#"{{"timestamp":"2025-09-30T15:42:34.559Z","type":"session_meta","payload":{{"id":"session-{i}","cwd":"/test/{i}"}}}}
{{"timestamp":"2025-09-30T15:42:36.190Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"message {i}"}}]}}}}
"#
        );
        fs::write(&file, sample).unwrap();
    }

    let connector = CodexConnector::new();
    // qu81y: the tempdir codex home is passed as an EXPLICIT scan root
    // instead of mutating process-global CODEX_HOME.
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 3);
}

/// Test empty content messages are filtered
///
/// (qu81y: this suite no longer mutates CODEX_HOME — every test passes its
/// tempdir as an explicit ScanContext scan root, so parallel scheduling is
/// safe without serial_test.)
#[test]
fn codex_connector_filters_empty_content() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/2025/11/29");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-empty.jsonl");

    let sample = r#"{"timestamp":"2025-09-30T15:42:34.559Z","type":"session_meta","payload":{"id":"test-id","cwd":"/test"}}
{"timestamp":"2025-09-30T15:42:36.190Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"   "}]}}
{"timestamp":"2025-09-30T15:42:37.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"valid content"}]}}
{"timestamp":"2025-09-30T15:42:43.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[]}}
"#;
    fs::write(&file, sample).unwrap();

    let connector = CodexConnector::new();
    // qu81y: the tempdir codex home is passed as an EXPLICIT scan root
    // instead of mutating process-global CODEX_HOME.
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    // Only the message with "valid content" should be included
    assert_eq!(c.messages.len(), 1);
    assert!(c.messages[0].content.contains("valid content"));
}

/// Test title extraction from first user message
///
#[test]
fn codex_connector_extracts_title() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/2025/11/30");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-title.jsonl");

    let sample = r#"{"timestamp":"2025-09-30T15:42:34.559Z","type":"session_meta","payload":{"id":"test-id","cwd":"/test"}}
{"timestamp":"2025-09-30T15:42:35.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"text","text":"assistant first"}]}}
{"timestamp":"2025-09-30T15:42:36.190Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"This is the user's question\nWith a second line"}]}}
"#;
    fs::write(&file, sample).unwrap();

    let connector = CodexConnector::new();
    // qu81y: the tempdir codex home is passed as an EXPLICIT scan root
    // instead of mutating process-global CODEX_HOME.
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    // Title should be first line of first user message
    assert_eq!(c.title, Some("This is the user's question".to_string()));
}

/// Test sequential index assignment
#[test]
fn codex_connector_assigns_sequential_indices() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/2025/12/01");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-idx.jsonl");

    let sample = r#"{"timestamp":"2025-09-30T15:42:34.559Z","type":"session_meta","payload":{"id":"test-id","cwd":"/test"}}
{"timestamp":"2025-09-30T15:42:36.190Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"first"}]}}
{"timestamp":"2025-09-30T15:42:37.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"text","text":"second"}]}}
{"timestamp":"2025-09-30T15:42:38.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"third"}]}}
"#;
    fs::write(&file, sample).unwrap();

    let connector = CodexConnector::new();
    // qu81y: the tempdir codex home is passed as an EXPLICIT scan root
    // instead of mutating process-global CODEX_HOME.
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    assert_eq!(c.messages.len(), 3);
    assert_eq!(c.messages[0].idx, 0);
    assert_eq!(c.messages[1].idx, 1);
    assert_eq!(c.messages[2].idx, 2);
}

/// Test `external_id` comes from filename
#[test]
fn codex_connector_sets_external_id_from_filename() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/2025/12/02");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-unique-id-123.jsonl");

    let sample = r#"{"timestamp":"2025-09-30T15:42:34.559Z","type":"session_meta","payload":{"id":"test-id","cwd":"/test"}}
{"timestamp":"2025-09-30T15:42:36.190Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"test"}]}}
"#;
    fs::write(&file, sample).unwrap();

    let connector = CodexConnector::new();
    // qu81y: the tempdir codex home is passed as an EXPLICIT scan root
    // instead of mutating process-global CODEX_HOME.
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    // external_id is now the relative path from sessions dir for uniqueness across directories
    assert_eq!(
        c.external_id,
        Some("2025/12/02/rollout-unique-id-123".to_string())
    );
}

/// Test empty sessions directory returns no conversations
#[test]
fn codex_connector_handles_empty_sessions() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    // No files in sessions

    let connector = CodexConnector::new();
    // qu81y: the tempdir codex home is passed as an EXPLICIT scan root
    // instead of mutating process-global CODEX_HOME.
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    let convs = connector.scan(&ctx).unwrap();
    assert!(convs.is_empty());
}

/// Test integer (milliseconds) timestamp format
#[test]
fn codex_connector_parses_millis_timestamp() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/2025/12/03");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-millis.jsonl");

    // Timestamps as i64 milliseconds instead of ISO-8601 strings
    let sample = r#"{"timestamp":1700000000000,"type":"session_meta","payload":{"id":"millis-test","cwd":"/millis"}}
{"timestamp":1700000001000,"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"millis timestamp test"}]}}
{"timestamp":1700000002000,"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"text","text":"response with millis"}]}}
"#;
    fs::write(&file, sample).unwrap();

    let connector = CodexConnector::new();
    // qu81y: the tempdir codex home is passed as an EXPLICIT scan root
    // instead of mutating process-global CODEX_HOME.
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    assert_eq!(c.messages.len(), 2);
    // Verify timestamps were parsed from i64 millis
    // started_at comes from session_meta timestamp (1700000000000)
    assert_eq!(c.started_at, Some(1700000000000));
    // ended_at comes from the last message timestamp (1700000002000)
    assert_eq!(c.ended_at, Some(1700000002000));
}

/// Test `tool_use` blocks in content are flattened properly
#[test]
fn codex_connector_flattens_tool_use_blocks() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/2025/12/04");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-tools.jsonl");

    let sample = r#"{"timestamp":"2025-09-30T15:42:34.559Z","type":"session_meta","payload":{"id":"tool-test","cwd":"/tools"}}
{"timestamp":"2025-09-30T15:42:36.190Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"read a file"}]}}
{"timestamp":"2025-09-30T15:42:43.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"text","text":"Let me read that file"},{"type":"tool_use","name":"Read","input":{"file_path":"/test/file.rs"}}]}}
"#;
    fs::write(&file, sample).unwrap();

    let connector = CodexConnector::new();
    // qu81y: the tempdir codex home is passed as an EXPLICIT scan root
    // instead of mutating process-global CODEX_HOME.
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    assert_eq!(c.messages.len(), 2);

    // Assistant message should contain flattened tool_use
    let assistant = &c.messages[1];
    assert!(assistant.content.contains("Let me read that file"));
    assert!(assistant.content.contains("[Tool: Read"));
    assert!(assistant.content.contains("/test/file.rs"));
}

/// Test missing cwd in `session_meta` results in None workspace
#[test]
fn codex_connector_handles_missing_cwd() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/2025/12/05");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-no-cwd.jsonl");

    // session_meta without cwd field
    let sample = r#"{"timestamp":"2025-09-30T15:42:34.559Z","type":"session_meta","payload":{"id":"no-cwd","cli_version":"0.42.0"}}
{"timestamp":"2025-09-30T15:42:36.190Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"test without cwd"}]}}
"#;
    fs::write(&file, sample).unwrap();

    let connector = CodexConnector::new();
    // qu81y: the tempdir codex home is passed as an EXPLICIT scan root
    // instead of mutating process-global CODEX_HOME.
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    assert!(
        c.workspace.is_none(),
        "workspace should be None when cwd missing"
    );
}

/// Test files without rollout- prefix are ignored
#[test]
fn codex_connector_ignores_non_rollout_files() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/2025/12/06");
    fs::create_dir_all(&sessions).unwrap();

    // Valid rollout file
    let rollout = sessions.join("rollout-valid.jsonl");
    let sample = r#"{"timestamp":"2025-09-30T15:42:34.559Z","type":"session_meta","payload":{"id":"valid","cwd":"/test"}}
{"timestamp":"2025-09-30T15:42:36.190Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"valid"}]}}
"#;
    fs::write(&rollout, sample).unwrap();

    // Non-rollout files that should be ignored
    let other1 = sessions.join("session-123.jsonl");
    let other2 = sessions.join("backup.json");
    let other3 = sessions.join("config.jsonl");
    fs::write(&other1, sample).unwrap();
    fs::write(&other2, sample).unwrap();
    fs::write(&other3, sample).unwrap();

    let connector = CodexConnector::new();
    // qu81y: the tempdir codex home is passed as an EXPLICIT scan root
    // instead of mutating process-global CODEX_HOME.
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    let convs = connector.scan(&ctx).unwrap();
    // Only the rollout- prefixed file should be processed
    assert_eq!(convs.len(), 1);
    // external_id is now the relative path from sessions dir for uniqueness across directories
    assert_eq!(
        convs[0].external_id,
        Some("2025/12/06/rollout-valid".to_string())
    );
}

/// Test legacy JSON with missing optional fields
#[test]
fn codex_connector_handles_legacy_json_missing_session() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/2025/12/07");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-minimal.json");

    // Minimal legacy format without session object
    let sample = r#"{
        "items": [
            {
                "role": "user",
                "content": "minimal legacy message"
            }
        ]
    }"#;
    fs::write(&file, sample).unwrap();

    let connector = CodexConnector::new();
    // qu81y: the tempdir codex home is passed as an EXPLICIT scan root
    // instead of mutating process-global CODEX_HOME.
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    assert!(c.workspace.is_none());
    assert_eq!(c.messages.len(), 1);
    assert!(c.messages[0].content.contains("minimal legacy message"));
}

/// Test title fallback to first message when no user message exists
#[test]
fn codex_connector_title_fallback_to_first_message() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/2025/12/08");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-no-user.jsonl");

    // Only assistant messages, no user message
    let sample = r#"{"timestamp":"2025-09-30T15:42:34.559Z","type":"session_meta","payload":{"id":"no-user","cwd":"/test"}}
{"timestamp":"2025-09-30T15:42:36.190Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"text","text":"Assistant first line\nSecond line"}]}}
"#;
    fs::write(&file, sample).unwrap();

    let connector = CodexConnector::new();
    // qu81y: the tempdir codex home is passed as an EXPLICIT scan root
    // instead of mutating process-global CODEX_HOME.
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    // Title should fallback to first line of first message
    assert_eq!(c.title, Some("Assistant first line".to_string()));
}

/// Test deeply nested directory structure
#[test]
fn codex_connector_handles_nested_directories() {
    let dir = TempDir::new().unwrap();
    let deep_sessions = dir.path().join("sessions/2025/12/09/sub1/sub2");
    fs::create_dir_all(&deep_sessions).unwrap();
    let file = deep_sessions.join("rollout-nested.jsonl");

    let sample = r#"{"timestamp":"2025-09-30T15:42:34.559Z","type":"session_meta","payload":{"id":"nested","cwd":"/nested"}}
{"timestamp":"2025-09-30T15:42:36.190Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"deeply nested"}]}}
"#;
    fs::write(&file, sample).unwrap();

    let connector = CodexConnector::new();
    // qu81y: the tempdir codex home is passed as an EXPLICIT scan root
    // instead of mutating process-global CODEX_HOME.
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);
    assert!(convs[0].source_path.to_string_lossy().contains("sub2"));
}

/// Test `turn_aborted` event is filtered out
#[test]
fn codex_connector_filters_turn_aborted() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/2025/12/10");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-aborted.jsonl");

    let sample = r#"{"timestamp":"2025-09-30T15:42:34.559Z","type":"session_meta","payload":{"id":"test-id","cwd":"/test"}}
{"timestamp":"2025-09-30T15:42:36.190Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"test"}]}}
{"timestamp":"2025-09-30T15:42:37.000Z","type":"event_msg","payload":{"type":"turn_aborted","reason":"user cancelled"}}
{"timestamp":"2025-09-30T15:42:38.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"text","text":"response"}]}}
"#;
    fs::write(&file, sample).unwrap();

    let connector = CodexConnector::new();
    // qu81y: the tempdir codex home is passed as an EXPLICIT scan root
    // instead of mutating process-global CODEX_HOME.
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    // Should only have 2 messages (user, assistant) - turn_aborted filtered
    assert_eq!(c.messages.len(), 2);
    for msg in &c.messages {
        assert!(!msg.content.contains("turn_aborted"));
    }
}

/// Test long title is truncated to 100 chars
#[test]
fn codex_connector_truncates_long_title() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/2025/12/11");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-long-title.jsonl");

    let long_text = "A".repeat(200);
    let sample = format!(
        r#"{{"timestamp":"2025-09-30T15:42:34.559Z","type":"session_meta","payload":{{"id":"long","cwd":"/test"}}}}
{{"timestamp":"2025-09-30T15:42:36.190Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"{long_text}"}}]}}}}
"#
    );
    fs::write(&file, sample).unwrap();

    let connector = CodexConnector::new();
    // qu81y: the tempdir codex home is passed as an EXPLICIT scan root
    // instead of mutating process-global CODEX_HOME.
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    // Bead 7k7pl: collapse `.is_some()` + `.unwrap().len() == 100`
    // into a single pin on the title's length (truncation contract)
    // plus a content pin — the truncated title must still be 100 x
    // 'A' (no bytes lost to multi-byte boundary confusion).
    assert_eq!(
        c.title.as_ref().map(|t| t.len()),
        Some(100),
        "title must be truncated to exactly 100 chars; got {:?}",
        c.title
    );
    assert_eq!(
        c.title.as_deref(),
        Some("A".repeat(100).as_str()),
        "title must be 100 'A's from the truncation of 200; got {:?}",
        c.title
    );
}

/// Test `source_path` matches actual file path
#[test]
fn codex_connector_sets_source_path() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/2025/12/12");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-source-path.jsonl");

    let sample = r#"{"timestamp":"2025-09-30T15:42:34.559Z","type":"session_meta","payload":{"id":"path-test","cwd":"/test"}}
{"timestamp":"2025-09-30T15:42:36.190Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"test source path"}]}}
"#;
    fs::write(&file, sample).unwrap();

    let connector = CodexConnector::new();
    // qu81y: the tempdir codex home is passed as an EXPLICIT scan root
    // instead of mutating process-global CODEX_HOME.
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 1);

    let c = &convs[0];
    assert_eq!(c.source_path, file);
}

/// Test metadata indicates correct source format
#[test]
fn codex_connector_metadata_indicates_format() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join("sessions/2025/12/13");
    fs::create_dir_all(&sessions).unwrap();

    // Create both JSONL and JSON files
    let jsonl_file = sessions.join("rollout-jsonl.jsonl");
    let json_file = sessions.join("rollout-json.json");

    let jsonl_sample = r#"{"timestamp":"2025-09-30T15:42:34.559Z","type":"session_meta","payload":{"id":"jsonl","cwd":"/test"}}
{"timestamp":"2025-09-30T15:42:36.190Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"jsonl format"}]}}
"#;
    fs::write(&jsonl_file, jsonl_sample).unwrap();

    let json_sample = r#"{"session":{"id":"json","cwd":"/test"},"items":[{"role":"user","content":"json format"}]}"#;
    fs::write(&json_file, json_sample).unwrap();

    let connector = CodexConnector::new();
    // qu81y: the tempdir codex home is passed as an EXPLICIT scan root
    // instead of mutating process-global CODEX_HOME.
    let ctx = ScanContext::with_roots(
        dir.path().to_path_buf(),
        vec![ScanRoot::local(dir.path().to_path_buf())],
        None,
    );
    let convs = connector.scan(&ctx).unwrap();
    assert_eq!(convs.len(), 2);

    // Find each conversation and verify metadata
    let jsonl_conv = convs.iter().find(|c| c.source_path == jsonl_file).unwrap();
    let json_conv = convs.iter().find(|c| c.source_path == json_file).unwrap();

    assert_eq!(
        jsonl_conv.metadata.get("source").and_then(|v| v.as_str()),
        Some("rollout")
    );
    assert_eq!(
        json_conv.metadata.get("source").and_then(|v| v.as_str()),
        Some("rollout_json")
    );
}
