//! GH#520: real transcripts must survive CASS's hashed SSH mirror layout.
//!
//! The indexer's construction tests cover config/database source metadata. This
//! suite exercises its pinned connector dependency using the same safe-name
//! function, actual transcripts and a stock SQLite Cursor database, without
//! needing SSH, installed agents or a writable CASS archive.

use std::fs;
use std::path::Path;

use coding_agent_search::sources::sync::path_to_safe_dirname;
use franken_agent_detection::{
    Connector, CursorConnector, FactoryConnector, GeminiConnector, NormalizedConversation,
    Origin, Platform, PrimeAgentConnector, ScanContext, ScanRoot, VibeConnector,
};

include!("support/remote_mirror_cursor_fixture.rs");

struct Case {
    connector: Box<dyn Connector>,
    slug: &'static str,
    source: &'static str,
    store: &'static str,
    payload_store: &'static str,
    file: &'static str,
    bytes: Vec<u8>,
}

fn cases() -> Vec<Case> {
    let factory = concat!(
        "{\"type\":\"session_start\",\"id\":\"mirror-factory\",\"cwd\":\"/srv/project\"}\n",
        "{\"type\":\"message\",\"timestamp\":\"2024-01-01T00:00:00Z\",\"message\":{\"role\":\"user\",\"content\":\"mirror question\"}}\n",
        "{\"type\":\"message\",\"timestamp\":\"2024-01-01T00:00:01Z\",\"message\":{\"role\":\"assistant\",\"content\":\"mirror answer\"}}\n"
    );
    let gemini = br#"{"sessionId":"mirror-gemini","projectHash":"project-hash","messages":[{"id":"u1","type":"user","content":"mirror question","timestamp":"2024-01-01T00:00:00Z"},{"id":"a1","type":"gemini","content":"mirror answer","timestamp":"2024-01-01T00:00:01Z"}]}"#;
    let vibe = concat!(
        "{\"role\":\"user\",\"content\":\"mirror question\",\"timestamp\":\"2024-01-01T00:00:00Z\"}\n",
        "{\"role\":\"assistant\",\"content\":\"mirror answer\",\"timestamp\":\"2024-01-01T00:00:01Z\"}\n"
    );
    let prime = concat!(
        "{\"type\":\"session\",\"version\":1,\"id\":\"mirror-prime\",\"cwd\":\"/srv/project\",\"timestamp\":\"2024-01-01T00:00:00Z\"}\n",
        "{\"type\":\"message\",\"timestamp\":\"2024-01-01T00:00:00Z\",\"message\":{\"role\":\"user\",\"content\":\"mirror question\"}}\n",
        "{\"type\":\"message\",\"timestamp\":\"2024-01-01T00:00:01Z\",\"message\":{\"role\":\"assistant\",\"content\":\"mirror answer\"}}\n"
    );
    let mut out = Vec::new();
    for (source, payload_store) in [
        ("~/.factory/sessions", ""),
        ("/home/alice/.factory", "sessions"),
    ] {
        out.push(Case {
            connector: Box::new(FactoryConnector::new()),
            slug: "factory",
            source,
            store: ".factory/sessions",
            payload_store,
            file: "-srv-project/factory-test.jsonl",
            bytes: factory.as_bytes().to_vec(),
        });
    }
    for (source, payload_store) in [("~/.gemini/tmp", ""), ("~/.gemini", "tmp")] {
        out.push(Case {
            connector: Box::new(GeminiConnector::new()),
            slug: "gemini",
            source,
            store: ".gemini/tmp",
            payload_store,
            file: "project-hash/chats/session-gemini.json",
            bytes: gemini.to_vec(),
        });
    }
    for (source, payload_store) in [
        ("~/.vibe/logs/session", ""),
        ("~/.vibe/logs", "session"),
        ("~/.vibe", "logs/session"),
    ] {
        out.push(Case {
            connector: Box::new(VibeConnector::new()),
            slug: "vibe",
            source,
            store: ".vibe/logs/session",
            payload_store,
            file: "fixture-session/messages.jsonl",
            bytes: vibe.as_bytes().to_vec(),
        });
    }
    for (source, payload_store) in [
        ("~/.prime/agent/sessions", ""),
        ("~/.prime/agent", "sessions"),
        ("~/.prime", "agent/sessions"),
    ] {
        out.push(Case {
            connector: Box::new(PrimeAgentConnector::new()),
            slug: "prime_agent",
            source,
            store: ".prime/agent/sessions",
            payload_store,
            file: "prime-test.jsonl",
            bytes: prime.as_bytes().to_vec(),
        });
    }
    for (source, payload_store) in [
        ("~/.config/Cursor/User", ""),
        ("~/Library/Application Support/Cursor/User", ""),
        ("~/AppData/Roaming/Cursor/User", ""),
        ("~/.config/Cursor", "User"),
    ] {
        out.push(Case {
            connector: Box::new(CursorConnector::new()),
            slug: "cursor",
            source,
            store: ".config/Cursor/User",
            payload_store,
            file: "globalStorage/state.vscdb",
            bytes: cursor_fixture(),
        });
    }
    out
}

fn put(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

fn signature(conversations: &[NormalizedConversation]) -> serde_json::Value {
    serde_json::Value::Array(
        conversations
            .iter()
            .map(|conversation| {
                serde_json::json!({
                    "agent": conversation.agent_slug,
                    "id": conversation.external_id,
                    "messages": conversation.messages.iter().map(|message| {
                        serde_json::json!([message.role, message.content])
                    }).collect::<Vec<_>>(),
                })
            })
            .collect(),
    )
}

#[test]
fn hashed_mirrors_parse_like_local_stores_across_copy_layouts() {
    for case in cases() {
        // rsync without '/' and scp/SFTP preserve the basename. rsync with
        // '/' copies contents; scp/SFTP can still retain it for that spelling.
        for (trailing_slash, retain_basename) in [(false, true), (true, true), (true, false)] {
            let tmp = tempfile::tempdir().unwrap();
            let native = tmp.path().join("native").join(case.store);
            let native_file = native.join(case.file);
            put(&native_file, &case.bytes);
            let local = ScanContext::with_roots(
                tmp.path().join("state"),
                vec![ScanRoot::local(native)],
                None,
            );
            let expected = case.connector.scan(&local).unwrap();
            assert_eq!(expected.len(), 1, "native {}", case.source);
            assert_eq!(expected[0].messages.len(), 2, "native {}", case.source);
            assert!(expected[0].external_id.is_some());

            let source = format!(
                "{}{}",
                case.source,
                if trailing_slash { "/" } else { "" }
            );
            let wrapper = tmp
                .path()
                .join("remotes/mirror-host/mirror")
                .join(path_to_safe_dirname(&source));
            let payload = if retain_basename {
                wrapper.join(Path::new(case.source).file_name().unwrap())
            } else {
                wrapper.clone()
            };
            let mirrored_file = payload.join(case.payload_store).join(case.file);
            put(&mirrored_file, &case.bytes);
            let origin = Origin::remote_with_host("mirror-host", "alice@remote");
            let root = ScanRoot::remote(wrapper, origin.clone(), Some(Platform::Linux))
                .with_mirrored_source_path(&source)
                .with_rewrite("/srv", "/mapped");
            assert_eq!(
                root.rewrite_workspace("/srv/project", Some(case.slug)),
                "/mapped/project"
            );
            let mut ctx = ScanContext::with_roots(
                tmp.path().join("state"),
                vec![root.clone(), root],
                None,
            );
            let label = format!("{source}, retained={retain_basename}");
            let sources = case.connector.discover_source_files(&ctx).unwrap();
            assert_eq!(sources.len(), 1, "{label}");
            assert_eq!(sources[0].source_path, mirrored_file, "{label}");
            assert_eq!(sources[0].origin, origin, "{label}");
            assert_eq!(sources[0].platform, Some(Platform::Linux));
            assert_eq!(sources[0].provider_slug, case.slug);

            let actual = case.connector.scan(&ctx).unwrap();
            assert_eq!(signature(&actual), signature(&expected), "{label}");
            assert_eq!(actual[0].source_path, mirrored_file, "{label}");
            let mut streamed = Vec::new();
            case.connector
                .scan_with_callback(&ctx, &mut |conversation| {
                    streamed.push(conversation);
                    Ok(())
                })
                .unwrap();
            assert_eq!(signature(&streamed), signature(&expected), "{label}");
            assert_eq!(fs::read(&mirrored_file).unwrap(), case.bytes, "{label}");

            ctx.since_ts = Some(i64::MAX);
            assert!(case.connector.discover_source_files(&ctx).unwrap().is_empty());
            assert!(case.connector.scan(&ctx).unwrap().is_empty());
        }
    }
}

#[test]
fn prime_mirrors_require_prime_ownership_not_a_shared_wire_format() {
    let tmp = tempfile::tempdir().unwrap();
    let prime = cases()
        .into_iter()
        .find(|case| case.slug == "prime_agent")
        .unwrap();
    for source in [
        "~/.pi/agent/sessions",
        "~/.omp/agent/sessions",
        "~/sessions",
    ] {
        let wrapper = tmp.path().join(path_to_safe_dirname(source));
        put(&wrapper.join("sessions/prime-test.jsonl"), &prime.bytes);
        let ctx = ScanContext::with_roots(
            tmp.path().join("state"),
            vec![ScanRoot::remote(wrapper, Origin::remote("mirror-host"), None)
                .with_mirrored_source_path(source)],
            None,
        );
        assert!(prime.connector.discover_source_files(&ctx).unwrap().is_empty());
        assert!(prime.connector.scan(&ctx).unwrap().is_empty());
    }
}
