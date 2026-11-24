//! The active prefix router must not delegate a nonempty zero-boundary source
//! to the legacy EOF parser. Exercise real connector delivery and completion.

use super::*;
use super::super::{
    CodexConnector, Connector, DiscoveredSourceFile, ScanContext, ScanRoot,
};
use franken_agent_detection::{SourceCompletion, SourceScanHooks};
use serde_json::json;
use std::time::{Duration, SystemTime};

struct Attempt {
    result: Result<()>,
    conversations: Vec<NormalizedConversation>,
    completions: Vec<Option<StablePrefixReceipt>>,
}

fn response(text: &str) -> String {
    json!({
        "type": "response_item",
        "timestamp": "2026-11-23T12:00:02Z",
        "payload": {"type": "message", "role": "assistant", "content": text}
    })
    .to_string()
}

fn user_event() -> String {
    json!({
        "type": "event_msg",
        "timestamp": "2026-11-23T12:00:01Z",
        "payload": {
            "type": "user_message", "message": "  continue\n", "turn_id": "turn-one"
        }
    })
    .to_string()
}

fn fixture(body: &str) -> Result<(tempfile::TempDir, PathBuf, ScanContext)> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("rollout-eof-boundary.jsonl");
    fs::write(&path, body)?;
    let context = ScanContext::with_roots(
        directory.path().join("archive"),
        vec![ScanRoot::local(path.clone())],
        None,
    );
    Ok((directory, path, context))
}

fn scan(context: &ScanContext, previous: Option<&StablePrefixReceipt>) -> Attempt {
    let observation = previous.map(|receipt| json!({"codex_prefix": receipt}).to_string());
    let mut before = |source: &DiscoveredSourceFile| {
        set_previous_prefix_receipt(&source.source_path, observation.as_deref()).unwrap();
        true
    };
    let mut conversations = Vec::new();
    let mut completions = Vec::new();
    let mut complete = |done: &SourceCompletion| {
        completions.push(current_prefix_receipt(&done.source.source_path));
        Ok(())
    };
    let result = CodexConnector::new().scan_with_source_boundaries(
        context,
        &mut SourceScanHooks {
            should_scan_source: Some(&mut before),
            on_source_complete: Some(&mut complete),
        },
        &mut |conversation| {
            conversations.push(conversation);
            Ok(())
        },
    );
    Attempt {
        result,
        conversations,
        completions,
    }
}

fn assert_deferred(attempt: &Attempt) {
    assert!(
        attempt.result.is_err(),
        "zero-boundary source succeeded: {} conversations, {} completions",
        attempt.conversations.len(),
        attempt.completions.len()
    );
    assert!(attempt.conversations.is_empty());
    assert!(attempt.completions.is_empty());
}

#[test]
fn recent_complete_first_json_value_without_lf_is_not_delivered_or_completed() -> Result<()> {
    for body in [response("first complete value"), user_event()] {
        for held in [false, true] {
            let (_directory, path, context) = fixture(&body)?;
            let _writer = if held {
                Some(OpenOptions::new().append(true).open(&path)?)
            } else {
                None
            };
            assert!(!crate::indexer::source_can_recover_incomplete_tail(&path));
            assert_deferred(&scan(&context, None));
            let error = StablePrefixSnapshot::capture(&path, 1024 * 1024, None, None)
                .err()
                .context("an active nonempty source must not fall back to strict EOF parsing")?;
            assert_eq!(
                error.downcast_ref::<io::Error>().map(io::Error::kind),
                Some(io::ErrorKind::UnexpectedEof)
            );
            assert_eq!(fs::read_to_string(&path)?, body);
        }
    }
    Ok(())
}

#[test]
fn first_prompt_waits_for_lf_and_neighbor_then_preserves_the_response_projection() -> Result<()> {
    let event = user_event();
    let (_directory, path, context) = fixture(&event)?;
    let mut writer = OpenOptions::new().append(true).open(&path)?;
    assert_deferred(&scan(&context, None));

    writer.write_all(b"\n")?;
    assert_deferred(&scan(&context, None));

    let paired = json!({
        "type": "response_item",
        "timestamp": "2026-11-23T12:00:01.001Z",
        "payload": {
            "type": "message", "role": "user", "content": "continue", "turn_id": "turn-one"
        }
    })
    .to_string();
    let appended = format!("{paired}\n{}\n", response("after prompt"));
    writer.write_all(appended.as_bytes())?;

    let attempt = scan(&context, None);
    attempt.result?;
    assert_eq!(attempt.conversations.len(), 1);
    let conversation = &attempt.conversations[0];
    assert_eq!(conversation.messages.len(), 2);
    assert_eq!(conversation.messages[0].idx, 0);
    assert_eq!(conversation.messages[0].role, "user");
    assert_eq!(conversation.messages[0].content, "continue");
    assert_eq!(conversation.messages[0].created_at, Some(1_795_435_201_001));
    assert_eq!(conversation.messages[1].idx, 1);
    assert_eq!(conversation.messages[1].content, "after prompt");
    assert_eq!(attempt.completions.len(), 1);
    let receipt = attempt.completions[0].as_ref().context("typed prefix completion")?;
    assert!(receipt.partial);
    assert!(!receipt.inactive_eof);
    assert_eq!(receipt.message_count, 2);
    assert_eq!(receipt.committed_bytes, fs::metadata(&path)?.len());

    let replay = scan(&context, Some(receipt));
    replay.result?;
    assert_eq!(replay.conversations.len(), 1);
    assert_eq!(replay.conversations[0].messages.len(), 2);
    assert_eq!(replay.completions.len(), 1);
    let repeated = replay.completions[0].as_ref().context("typed replay completion")?;
    assert_eq!(repeated.messages_blake3, receipt.messages_blake3);
    assert_eq!(repeated.committed_bytes, receipt.committed_bytes);
    assert_eq!(fs::read_to_string(&path)?, format!("{event}\n{appended}"));
    Ok(())
}

#[test]
fn inactive_complete_eof_and_abandoned_suffix_keep_the_existing_recovery_policy() -> Result<()> {
    let complete = response("kept");
    for body in [complete.clone(), format!("{complete}\n{{\"cut\":")] {
        let (_directory, path, context) = fixture(&body)?;
        File::open(&path)?.set_modified(SystemTime::now() - Duration::from_secs(7200))?;
        assert!(crate::indexer::source_can_recover_incomplete_tail(&path));
        let attempt = scan(&context, None);
        attempt.result?;
        assert_eq!(attempt.conversations.len(), 1);
        assert_eq!(attempt.conversations[0].messages.len(), 1);
        assert_eq!(attempt.conversations[0].messages[0].content, "kept");
        assert_eq!(attempt.completions.len(), 1);
        let receipt = attempt.completions[0].as_ref().context("typed inactive completion")?;
        assert!(receipt.inactive_eof);
        assert!(!receipt.partial);
        assert_eq!(receipt.admitted_bytes, body.len() as u64);
        assert_eq!(receipt.committed_bytes, body.len() as u64);
        assert_eq!(fs::read_to_string(&path)?, body);
    }
    Ok(())
}
