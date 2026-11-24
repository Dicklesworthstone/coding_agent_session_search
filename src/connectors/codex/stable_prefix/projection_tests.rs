//! Append-stability regressions against the public CASS connector and pinned
//! primary parser. These retain complete byte receipts across actual rescans.

use super::*;
use super::super::{
    CodexConnector, Connector, DiscoveredSourceFile, ScanContext, ScanRoot,
};
use franken_agent_detection::{SourceCompletion, SourceScanHooks};
use serde_json::{Value, json};

const EARLY: &str = "2026-11-23T12:00:00Z";
const SAME: &str = "2026-11-23T12:00:01Z";
const LATE: &str = "2026-11-23T12:00:02Z";

fn line(kind: &str, timestamp: &str, payload: Value) -> String {
    format!("{}\n", json!({"type":kind, "timestamp":timestamp, "payload":payload}))
}

fn response(text: &str, timestamp: &str) -> String {
    line("response_item", timestamp, json!({
        "type":"message", "role":"assistant", "content":text
    }))
}

fn user_event(text: &str, timestamp: &str) -> String {
    line("event_msg", timestamp, json!({
        "type":"user_message", "message":text, "turn_id":"turn-one"
    }))
}

fn user_response(text: &str, timestamp: &str) -> String {
    line("response_item", timestamp, json!({
        "type":"message", "role":"user", "content":text, "turn_id":"turn-one"
    }))
}

fn fixture(bytes: &str) -> Result<(tempfile::TempDir, PathBuf, ScanContext)> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("rollout-projection.jsonl");
    fs::write(&path, bytes)?;
    let ctx = ScanContext::with_roots(
        directory.path().join("archive"),
        vec![ScanRoot::local(path.clone())],
        None,
    );
    Ok((directory, path, ctx))
}

fn append(path: &Path, text: &str) -> Result<()> {
    OpenOptions::new().append(true).open(path)?.write_all(text.as_bytes())?;
    Ok(())
}

fn append_compaction_padding(path: &Path) -> Result<()> {
    let mut file = OpenOptions::new().append(true).open(path)?;
    file.write_all(b"{\"type\":\"padding\",\"payload\":{\"text\":\"")?;
    let block = [b'x'; 64 * 1024];
    for _ in 0..528 {
        file.write_all(&block)?;
    }
    file.write_all(b"\"}}\n")?;
    Ok(())
}

fn scan(
    ctx: &ScanContext,
    previous: Option<&StablePrefixReceipt>,
) -> Result<(NormalizedConversation, StablePrefixReceipt)> {
    let observation = previous.map(|receipt| json!({"codex_prefix":receipt}).to_string());
    let mut before = |source: &DiscoveredSourceFile| {
        set_previous_prefix_receipt(&source.source_path, observation.as_deref()).unwrap();
        true
    };
    let mut receipt = None;
    let mut complete = |done: &SourceCompletion| {
        assert!(receipt.is_none(), "one physical source emits one completion");
        receipt = current_prefix_receipt(&done.source.source_path);
        Ok(())
    };
    let mut conversations = Vec::new();
    CodexConnector::new().scan_with_source_boundaries(
        ctx,
        &mut SourceScanHooks {
            should_scan_source: Some(&mut before),
            on_source_complete: Some(&mut complete),
        },
        &mut |conversation| {
            conversations.push(conversation);
            Ok(())
        },
    )?;
    assert_eq!(conversations.len(), 1);
    Ok((conversations.remove(0), receipt.context("typed prefix completion")?))
}

fn contents(conversation: &NormalizedConversation) -> Vec<&str> {
    conversation.messages.iter().enumerate().map(|(index, message)| {
        assert_eq!(message.idx, i64::try_from(index).unwrap());
        message.content.as_str()
    }).collect()
}

#[test]
fn active_user_event_frontier_can_finish_as_a_paired_response_on_the_next_scan() -> Result<()> {
    let stable = response("before prompt", EARLY);
    let first_bytes = stable.clone() + &user_event("  continue\n", SAME);
    let (_directory, path, ctx) = fixture(&first_bytes)?;
    let (first, prior) = scan(&ctx, None)?;
    assert_eq!(contents(&first), ["before prompt"]);
    assert_eq!(prior.committed_bytes, stable.len() as u64);
    assert!(prior.partial);

    let appended = user_response("continue", "2026-11-23T12:00:01.001Z")
        + &response("after prompt", LATE);
    append(&path, &appended)?;
    let (second, next) = scan(&ctx, Some(&prior))?;
    assert_eq!(contents(&second), ["before prompt", "continue", "after prompt"]);
    assert_eq!(second.messages[1].created_at, Some(1_795_435_201_001));
    assert_eq!(next.message_count, 3);
    let (_, replay) = scan(&ctx, Some(&next))?;
    assert_eq!(replay.messages_blake3, next.messages_blake3);
    assert_eq!(fs::read_to_string(&path)?, first_bytes + &appended);
    Ok(())
}

#[test]
fn enrichment_occurrences_keep_source_order_across_equal_and_older_timestamps() -> Result<()> {
    for timestamp in [SAME, EARLY] {
        let (_directory, path, ctx) =
            fixture(&(response("first", SAME) + &response("second", SAME)))?;
        let (_, prior) = scan(&ctx, None)?;
        append(&path, &line("event_msg", timestamp, json!({
            "type":"agent_message", "message":"event-only append"
        })))?;
        let (second, next) = scan(&ctx, Some(&prior))?;
        assert_eq!(contents(&second), ["first", "second", "event-only append"]);
        let expected_start = if timestamp == EARLY {
            1_795_435_200_000
        } else {
            1_795_435_201_000
        };
        assert_eq!(second.started_at, Some(expected_start));
        assert_eq!(second.ended_at, Some(1_795_435_201_000));
        append(&path, &response("native append after event", timestamp))?;
        let (third, final_receipt) = scan(&ctx, Some(&next))?;
        assert_eq!(contents(&third), [
            "first", "second", "event-only append", "native append after event"
        ]);
        assert_eq!(final_receipt.message_count, 4);
        assert_eq!(third.started_at, Some(expected_start));
        assert_eq!(third.ended_at, Some(1_795_435_201_000));
        assert_eq!(
            messages_digest(&third.messages[..next.message_count])?,
            next.messages_blake3
        );
    }
    Ok(())
}

#[test]
fn inactive_policy_can_finalize_an_unpaired_user_event_without_altering_earlier_messages()
-> Result<()> {
    let bytes = response("before prompt", EARLY) + &user_event("standalone prompt", SAME);
    let (_directory, path, ctx) = fixture(&bytes)?;
    let (first, prior) = scan(&ctx, None)?;
    assert_eq!(contents(&first), ["before prompt"]);
    File::open(&path)?.set_modified(
        std::time::SystemTime::now() - std::time::Duration::from_secs(7200)
    )?;
    let (second, next) = scan(&ctx, Some(&prior))?;
    assert_eq!(contents(&second), ["before prompt", "standalone prompt"]);
    assert!(!next.partial);
    assert_eq!(next.committed_bytes, bytes.len() as u64);
    Ok(())
}

#[test]
fn prompt_frontier_preserves_reversed_pairs_and_repeated_occurrences() -> Result<()> {
    let event = user_event("continue", SAME);
    let native = user_response("continue", SAME);
    for (prompts, expected_retained) in [
        (event.clone() + &native + &event, 2),
        (native.clone() + &event, 1),
        (event.clone() + &event, 2),
    ] {
        let (_directory, path, ctx) = fixture(&(response("before", EARLY) + &prompts))?;
        let (first, prior) = scan(&ctx, None)?;
        append(&path, &response("after", LATE))?;
        let (second, next) = scan(&ctx, Some(&prior))?;
        assert_eq!(second.messages.len(), expected_retained + 2);
        assert_eq!(
            messages_digest(&second.messages[..first.messages.len()])?,
            prior.messages_blake3
        );
        let (_, replay) = scan(&ctx, Some(&next))?;
        assert_eq!(replay.messages_blake3, next.messages_blake3);
    }
    Ok(())
}

#[test]
fn prompt_frontier_keeps_crlf_recovery_and_zero_progress_retry() -> Result<()> {
    let stable = response("before", EARLY).replace('\n', "\r\n");
    let event = user_event("recovered prompt", SAME).replace('\n', "\r\n");
    let bytes = stable.clone() + "{\"torn\":\"" + &event;
    let (_directory, path, ctx) = fixture(&bytes)?;
    let (first, prior) = scan(&ctx, None)?;
    assert_eq!(contents(&first), ["before"]);
    assert_eq!(prior.committed_bytes, stable.len() as u64);
    append(&path, &response("after", LATE))?;
    let (second, _) = scan(&ctx, Some(&prior))?;
    assert_eq!(contents(&second), ["before", "recovered prompt", "after"]);

    let (_directory, path, ctx) = fixture(&("\u{feff}".to_owned() + &event))?;
    let error = StablePrefixSnapshot::capture(&path, 1024 * 1024, None, None)
        .err().context("pending prompt must wait")?;
    assert_eq!(
        error.downcast_ref::<io::Error>().context("retry error")?.kind(),
        io::ErrorKind::UnexpectedEof
    );
    // Exercise the connector, not a constructed receipt: zero committed bytes
    // must defer rather than falling back to a strict parse of the pending row.
    let mut delivered = 0;
    let result = CodexConnector::new().scan_with_source_boundaries(
        &ctx,
        &mut SourceScanHooks::default(),
        &mut |_| {
            delivered += 1;
            Ok(())
        },
    );
    assert!(result.is_err());
    assert_eq!(delivered, 0);
    File::open(&path)?.set_modified(
        std::time::SystemTime::now() - std::time::Duration::from_secs(7200)
    )?;
    let (inactive, _) = scan(&ctx, None)?;
    assert_eq!(contents(&inactive), ["recovered prompt"]);
    Ok(())
}

#[test]
fn compaction_threshold_keeps_native_call_and_event_occurrences_append_stable() -> Result<()> {
    let mut bytes = response("before", EARLY);
    for call_id in ["patch-one", "patch-two", "patch-one"] {
        bytes += &line("response_item", SAME, json!({
            "type":"custom_tool_call", "name":"apply_patch",
            "call_id":call_id, "input":"same patch"
        }));
    }
    for call_id in ["function-one", "function-two"] {
        bytes += &line("response_item", SAME, json!({
            "type":"function_call", "name":"exec_command",
            "call_id":call_id, "arguments":"{\"cmd\":\"pwd\"}"
        }));
    }
    bytes += &line("response_item", SAME, json!({"content":"legacy default-role text"}));
    bytes += &line("response_item", SAME, json!({
        "type":"alternate_plaintext", "content":"generic default-role text"
    }));
    for kind in ["function_call_output", "custom_tool_call_output"] {
        bytes += &line("response_item", SAME, json!({
            "type":kind, "call_id":"same-output-id",
            "output":{"content":[{"type":"output_text","text":"structured exact"}]}
        }));
    }
    let event = line("event_msg", SAME, json!({
        "type":"agent_message", "message":"repeated event"
    }));
    bytes += &event;
    bytes += &event;
    let (_directory, path, ctx) = fixture(&bytes)?;
    let (first, prior) = scan(&ctx, None)?;
    assert_eq!(first.messages.len(), 12);
    assert_eq!(first.messages[6].role, "agent");
    assert_eq!(first.messages[7].role, "agent");
    assert_eq!(first.messages[8].content, "structured exact");
    assert_eq!(first.messages[9].content, "structured exact");
    let call_ids: Vec<_> = first.messages[1..6].iter()
        .map(|message| message.invocations[0].call_id.as_deref().unwrap())
        .collect();
    assert_eq!(call_ids, [
        "patch-one", "patch-two", "patch-one", "function-one", "function-two"
    ]);

    append_compaction_padding(&path)?;
    append(&path, &line("response_item", SAME, json!({
        "type":"custom_tool_call", "name":"apply_patch",
        "call_id":"patch-three", "input":"same patch"
    })))?;
    append(&path, &event)?;
    let (second, next) = scan(&ctx, Some(&prior))?;
    assert_eq!(second.messages.len(), 14);
    assert_eq!(
        messages_digest(&second.messages[..prior.message_count])?,
        prior.messages_blake3
    );
    assert_eq!(second.messages[7].extra, json!({}), "the real 32 MiB compaction ran");
    assert_eq!(second.messages[12].invocations[0].call_id.as_deref(), Some("patch-three"));
    assert_eq!(second.messages.iter().filter(|message| message.content == "repeated event").count(), 3);
    let (_, replay) = scan(&ctx, Some(&next))?;
    assert_eq!(replay.messages_blake3, next.messages_blake3);
    Ok(())
}

#[test]
fn paired_user_event_cannot_steal_a_later_identical_response_occurrence() -> Result<()> {
    for compact in [false, true] {
        for timestamp in [json!(SAME), Value::Null] {
            let (_directory, path, ctx) = fixture("")?;
            if compact {
                append_compaction_padding(&path)?;
            }
            let event = json!({"type":"event_msg", "timestamp":timestamp,
                "payload":{"type":"user_message", "message":"continue", "turn_id":"one"}});
            let first_response = json!({"type":"response_item", "timestamp":timestamp,
                "payload":{"type":"message", "role":"user", "content":"continue", "turn_id":"one"}});
            let agent = json!({"type":"event_msg", "timestamp":timestamp,
                "payload":{"type":"agent_message", "message":"between"}});
            append(&path, &format!("{event}\n{first_response}\n{agent}\n"))?;
            let (first, prior) = scan(&ctx, None)?;
            assert_eq!(contents(&first), ["continue", "between"]);
            let later = json!({"type":"response_item", "timestamp":timestamp,
                "payload":{"type":"message", "role":"user", "content":"continue", "turn_id":"two"}});
            append(&path, &format!("{later}\n"))?;
            let (second, next) = scan(&ctx, Some(&prior))?;
            assert_eq!(contents(&second), ["continue", "between", "continue"]);
            assert_eq!(messages_digest(&second.messages[..2])?, prior.messages_blake3);
            let (_, replay) = scan(&ctx, Some(&next))?;
            assert_eq!(replay.messages_blake3, next.messages_blake3);
        }
    }
    Ok(())
}

#[test]
fn recovered_ignored_suffixes_preserve_physical_prompt_adjacency() -> Result<()> {
    let stable = response("before", EARLY);
    let event = user_event("continue", SAME);
    let ignored = line("event_msg", SAME, json!({"type":"token_count"}));
    let enriched = line("event_msg", SAME, json!({
        "type":"agent_message", "message":"event on recovered line"
    }));
    let recovered_line = format!("{{\"torn\":\"{}{}{}\n",
        event.trim_end(), ignored.trim_end(), enriched.trim_end());
    let (_directory, path, ctx) = fixture(&(stable.clone() + &recovered_line))?;
    let (first, prior) = scan(&ctx, None)?;
    assert_eq!(contents(&first), ["before"]);
    assert_eq!(prior.committed_bytes, stable.len() as u64);
    append(&path, &(user_response("continue", SAME) + &response("after", LATE)))?;
    let (second, next) = scan(&ctx, Some(&prior))?;
    assert_eq!(contents(&second), [
        "before", "event on recovered line", "continue", "after"
    ]);
    assert_eq!(next.message_count, 4);

    // Blank or unrecoverable physical lines increment FAD's physical index
    // and therefore prevent a later response from pairing across that gap.
    for separator in ["\n", "not-json\n"] {
        let bytes = stable.clone() + &event + separator;
        let (_directory, path, ctx) = fixture(&bytes)?;
        let (first, prior) = scan(&ctx, None)?;
        assert_eq!(contents(&first), ["before", "continue"]);
        append(&path, &user_response("continue", SAME))?;
        let (second, _) = scan(&ctx, Some(&prior))?;
        assert_eq!(contents(&second), ["before", "continue", "continue"]);
    }
    Ok(())
}

#[test]
fn source_order_scope_requires_a_live_private_snapshot() -> Result<()> {
    let (_directory, path, _ctx) = fixture(&response("before", EARLY))?;
    let snapshot = StablePrefixSnapshot::capture(&path, 1024 * 1024, None, None)?
        .context("private prefix")?;
    assert!(!guarded_parser_view(&path));
    assert!(!guarded_parser_view(snapshot.path()));
    {
        let _guard = PrefixParserViewGuard::enter(&snapshot);
        assert!(guarded_parser_view(snapshot.path()));
        assert!(!guarded_parser_view(&path));
    }
    assert!(!guarded_parser_view(snapshot.path()));
    Ok(())
}

#[test]
fn semantic_frontier_cannot_regress_previously_admitted_nonprojected_bytes() -> Result<()> {
    // The event in a response/event pair has no canonical message of its own,
    // but its physical bytes remain part of the durable source proof.
    let bytes = response("before", EARLY)
        + &user_response("continue", SAME)
        + &user_event("continue", SAME);
    let (_directory, path, ctx) = fixture(&bytes)?;
    File::open(&path)?.set_modified(
        std::time::SystemTime::now() - std::time::Duration::from_secs(7200)
    )?;
    let (first, prior) = scan(&ctx, None)?;
    assert_eq!(contents(&first), ["before", "continue"]);
    assert_eq!(prior.committed_bytes, bytes.len() as u64);
    File::open(&path)?.set_modified(std::time::SystemTime::now())?;
    let error = StablePrefixSnapshot::capture(&path, 1024 * 1024, Some(&prior), None)
        .err().context("byte proof must not regress")?;
    assert_eq!(
        error.downcast_ref::<io::Error>().context("source-change error")?.kind(),
        io::ErrorKind::Interrupted
    );
    assert!(error.to_string().contains("boundary moved backwards"));
    assert!(scan(&ctx, Some(&prior)).is_err());
    append(&path, &response("after", LATE))?;
    let (second, next) = scan(&ctx, Some(&prior))?;
    assert_eq!(contents(&second), ["before", "continue", "after"]);
    assert!(next.committed_bytes >= prior.committed_bytes);
    assert_eq!(messages_digest(&second.messages[..2])?, prior.messages_blake3);
    Ok(())
}

#[test]
fn native_web_call_then_completion_remains_append_stable() -> Result<()> {
    let call = line("response_item", SAME, json!({
        "type":"web_search_call", "id":"web-one",
        "action":{"type":"search", "query":"stable query"}
    }));
    let (_directory, path, ctx) = fixture(&(response("before", EARLY) + &call))?;
    let (first, prior) = scan(&ctx, None)?;
    assert_eq!(first.messages.len(), 2);
    assert_eq!(first.messages[1].invocations[0].call_id.as_deref(), Some("web-one"));
    append(&path, &line("event_msg", LATE, json!({
        "type":"web_search_end", "call_id":"web-one", "query":"stable query"
    })))?;
    append(&path, &response("after", LATE))?;
    let (second, next) = scan(&ctx, Some(&prior))?;
    assert_eq!(second.messages.len(), 3);
    assert_eq!(messages_digest(&second.messages[..2])?, prior.messages_blake3);
    assert_eq!(second.messages[1].invocations.len(), 1);
    let (_, replay) = scan(&ctx, Some(&next))?;
    assert_eq!(replay.messages_blake3, next.messages_blake3);
    Ok(())
}

#[test]
fn later_richer_web_response_requires_reconciliation_of_published_completion() -> Result<()> {
    let completion = line("event_msg", SAME, json!({
        "type":"web_search_end", "call_id":"web-one", "query":"display query"
    }));
    let (_directory, path, ctx) =
        fixture(&(response("before", EARLY) + &completion))?;
    let (first, prior) = scan(&ctx, None)?;
    assert_eq!(first.messages.len(), 2);
    append(&path, &line("response_item", LATE, json!({
        "type":"web_search_call", "id":"web-one",
        "action":{"type":"search", "queries":["first query", "second query"]}
    })))?;
    // The original bytes remain valid. It is the upstream richer projection,
    // not a source rewrite, that cannot replace already published row content.
    prior.validate_source()?;
    assert!(scan(&ctx, Some(&prior)).is_err());
    let (whole, _) = scan(&ctx, None)?;
    assert_eq!(whole.messages.len(), 2, "within one prefix, retain the upstream merge");
    assert_ne!(whole.messages[1].content, first.messages[1].content);
    assert_eq!(
        whole.messages[1].invocations[0].arguments.as_ref().unwrap()["queries"],
        json!(["first query", "second query"])
    );
    let snapshot = StablePrefixSnapshot::capture(
        &path, 1024 * 1024, Some(&prior), None
    )?.context("unchanged physical prefix")?;
    let error = snapshot.receipt_for(Some(&whole), Some(&prior))
        .err().context("changed published projection must fail")?;
    assert!(error.to_string().contains("message projection changed"));
    Ok(())
}
