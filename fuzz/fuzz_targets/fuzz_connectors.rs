//! Fuzz target for compiled connector scanners.
//!
//! The connector trait does not expose every provider's private
//! `parse_session_file` helper, so this target feeds arbitrary bytes through the
//! public scan surface after placing the payload in the selected connector's
//! expected on-disk layout. That keeps the harness representative of production
//! discovery while still reaching per-provider parsers.

#![no_main]

use std::path::{Path, PathBuf};

use arbitrary::Arbitrary;
use coding_agent_search::connectors::{get_connector_factories, ScanContext, ScanRoot};
use libfuzzer_sys::fuzz_target;
use tempfile::TempDir;

const MAX_PAYLOAD_BYTES: usize = 128 * 1024;

#[derive(Arbitrary, Debug)]
struct ConnectorInput {
    connector_index: u8,
    payload: Vec<u8>,
}

fn write_payload(path: PathBuf, payload: &[u8]) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, payload);
}

fn write_connector_layout(root: &Path, slug: &str, payload: &[u8]) {
    match slug {
        "aider" => {
            write_payload(root.join("project/.aider.chat.history.md"), payload);
        }
        "amp" => {
            write_payload(root.join(".local/share/amp/fuzz-log.jsonl"), payload);
            write_payload(
                root.join(".config/Code/User/globalStorage/sourcegraph.amp/fuzz-log.json"),
                payload,
            );
        }
        "chatgpt" => {
            write_payload(root.join("conversation-fuzz.json"), payload);
        }
        "claude" => {
            write_payload(root.join("projects/fuzz/session.jsonl"), payload);
            write_payload(root.join("projects/fuzz/session.json"), payload);
        }
        "clawdbot" => {
            write_payload(root.join(".clawdbot/sessions/session.jsonl"), payload);
        }
        "cline" => {
            write_payload(
                root.join("taskHistory.json/fuzz-task/ui_messages.json"),
                payload,
            );
            write_payload(
                root.join("taskHistory.json/fuzz-task/api_conversation_history.json"),
                payload,
            );
        }
        "codex" => {
            write_payload(root.join("sessions/rollout-fuzz.jsonl"), payload);
            write_payload(root.join(".codex/sessions/rollout-fuzz.json"), payload);
        }
        "copilot" => {
            write_payload(root.join("conversations.json"), payload);
            write_payload(
                root.join("session-state/fuzz-session/events.jsonl"),
                payload,
            );
            write_payload(
                root.join("history-session-state/fuzz-session.json"),
                payload,
            );
        }
        "copilot_cli" => {
            write_payload(
                root.join(".copilot/session-state/fuzz/events.jsonl"),
                payload,
            );
        }
        "cursor" => {
            write_payload(root.join("globalStorage/state.vscdb"), payload);
            write_payload(root.join("workspaceStorage/fuzz/state.vscdb"), payload);
        }
        "factory" => {
            write_payload(root.join("factory-session.jsonl"), payload);
        }
        "gemini" => {
            write_payload(root.join("tmp/fuzz-session.json"), payload);
            write_payload(root.join(".gemini/tmp/fuzz-session.json"), payload);
        }
        "goose" => {
            write_payload(root.join(".local/share/goose/sessions/fuzz.jsonl"), payload);
        }
        "hermes" => {
            write_payload(root.join(".hermes/sessions/fuzz.jsonl"), payload);
        }
        "kimi" => {
            write_payload(
                root.join(".kimi/sessions/workspace/session/wire.jsonl"),
                payload,
            );
            write_payload(
                root.join(".kimi/sessions/workspace/session/state.json"),
                payload,
            );
        }
        "openclaw" => {
            write_payload(root.join(".openclaw/sessions/session.jsonl"), payload);
        }
        "opencode" => {
            write_payload(root.join("storage/session/project/session.json"), payload);
            write_payload(root.join("storage/message/session/message.json"), payload);
            write_payload(root.join("storage/part/message/part.json"), payload);
            write_payload(root.join("opencode.db"), payload);
        }
        "pi_agent" => {
            write_payload(
                root.join(".pi/agent/sessions/project/2025-12-01T10-00-00_fuzz.jsonl"),
                payload,
            );
            write_payload(
                root.join(".omp/agent/sessions/project/2025-12-01T10-00-00_fuzz.jsonl"),
                payload,
            );
        }
        "qwen" => {
            write_payload(
                root.join(".qwen/tmp/project/chats/session-1731107950138-fuzz.json"),
                payload,
            );
        }
        "vibe" => {
            write_payload(root.join(".vibe/logs/session/fuzz/messages.jsonl"), payload);
            write_payload(root.join("logs/session/fuzz/messages.jsonl"), payload);
        }
        _ => {
            write_payload(root.join("session.jsonl"), payload);
            write_payload(root.join("session.json"), payload);
        }
    }
}

fuzz_target!(|input: ConnectorInput| {
    let factories = get_connector_factories();
    if factories.is_empty() {
        return;
    }

    let (slug, build_connector) = factories[input.connector_index as usize % factories.len()];
    let payload = if input.payload.len() > MAX_PAYLOAD_BYTES {
        &input.payload[..MAX_PAYLOAD_BYTES]
    } else {
        &input.payload
    };

    let Ok(temp_dir) = TempDir::new() else {
        return;
    };
    let root = temp_dir.path().join("scan-root");
    let data_dir = temp_dir.path().join("data");

    write_connector_layout(&root, slug, payload);

    let ctx = ScanContext::with_roots(data_dir, vec![ScanRoot::local(root)], None);
    let connector = build_connector();
    let _ = connector.scan_with_callback(&ctx, &mut |conversation| {
        let _ = serde_json::to_value(&conversation);
        Ok(())
    });
});
