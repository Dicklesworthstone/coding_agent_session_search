use coding_agent_search::connectors::claude_code::ClaudeCodeConnector;
use coding_agent_search::connectors::{
    Connector, NormalizedConversation, NormalizedMessage, ScanContext,
};
use std::fs;
use std::path::Path;
use tempfile::TempDir;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequirementLevel {
    Must,
    Should,
}

#[derive(Debug, Clone, Copy)]
struct ConnectorRequirement {
    id: &'static str,
    level: RequirementLevel,
    description: &'static str,
}

const CLAUDE_CODE_NORMALIZATION_SPEC: &[ConnectorRequirement] = &[
    ConnectorRequirement {
        id: "CC-MUST-001",
        level: RequirementLevel::Must,
        description: "each emitted conversation uses the claude_code agent slug",
    },
    ConnectorRequirement {
        id: "CC-MUST-002",
        level: RequirementLevel::Must,
        description: "each emitted conversation has a source path and project-relative external id",
    },
    ConnectorRequirement {
        id: "CC-MUST-003",
        level: RequirementLevel::Must,
        description: "each emitted conversation contains at least one normalized message",
    },
    ConnectorRequirement {
        id: "CC-MUST-004",
        level: RequirementLevel::Must,
        description: "message indices are contiguous and start at zero",
    },
    ConnectorRequirement {
        id: "CC-MUST-005",
        level: RequirementLevel::Must,
        description: "message roles are non-empty members of the normalized role enum",
    },
    ConnectorRequirement {
        id: "CC-MUST-006",
        level: RequirementLevel::Must,
        description: "message content is non-empty after connector filtering",
    },
    ConnectorRequirement {
        id: "CC-MUST-007",
        level: RequirementLevel::Must,
        description: "message timestamps are present and monotonically nondecreasing",
    },
    ConnectorRequirement {
        id: "CC-MUST-008",
        level: RequirementLevel::Must,
        description: "conversation start and end timestamps bound emitted messages",
    },
    ConnectorRequirement {
        id: "CC-SHOULD-001",
        level: RequirementLevel::Should,
        description: "conversation title is derived from the first user message",
    },
    ConnectorRequirement {
        id: "CC-SHOULD-002",
        level: RequirementLevel::Should,
        description: "session metadata preserves session id and git branch",
    },
    ConnectorRequirement {
        id: "CC-SHOULD-003",
        level: RequirementLevel::Should,
        description: "assistant model and tool_use blocks are normalized",
    },
];

fn write_session(root: &Path, name: &str, lines: &[&str]) -> std::path::PathBuf {
    let path = root.join(name);
    fs::write(&path, lines.join("\n")).unwrap();
    path
}

fn scan_fixture() -> Vec<NormalizedConversation> {
    let tmp = TempDir::new().unwrap();
    let project = tmp.path().join("fixture-claude/projects/org/team");
    fs::create_dir_all(&project).unwrap();

    write_session(
        &project,
        "alpha.jsonl",
        &[
            r#"{"type":"user","cwd":"/workspace/alpha","sessionId":"session-alpha","gitBranch":"main","message":{"role":"user","content":"Plan conformance for Claude Code\nInclude timestamp checks."},"timestamp":"2025-11-12T18:31:18.000Z"}"#,
            r#"{"type":"summary","summary":"Summary should not surface","timestamp":"2025-11-12T18:31:19.000Z"}"#,
            r#"{"type":"assistant","message":{"role":"assistant","model":"claude-sonnet-4","content":[{"type":"text","text":"I will inspect the fixture."},{"type":"tool_use","id":"toolu_1","name":"Read","input":{"file_path":"/workspace/alpha/session.jsonl"}}]},"timestamp":"2025-11-12T18:31:20.000Z"}"#,
            r#"{"type":"file-history-snapshot","timestamp":"2025-11-12T18:31:21.000Z"}"#,
            r#"{"type":"user","message":{"role":"user","content":"Verify the normalized contract."},"timestamp":"2025-11-12T18:31:22.000Z"}"#,
        ],
    );
    write_session(
        &project,
        "beta.jsonl",
        &[
            r#"{"type":"user","cwd":"/workspace/beta","sessionId":"session-beta","gitBranch":"feature/conformance","message":{"role":"user","content":"Review metadata"},"timestamp":"2025-11-13T09:00:00.000Z"}"#,
            r#"{"type":"assistant","message":{"role":"assistant","model":"claude-opus-4","content":[{"type":"text","text":"Metadata is preserved."}]},"timestamp":"2025-11-13T09:00:01.000Z"}"#,
        ],
    );

    let connector = ClaudeCodeConnector::new();
    let ctx = ScanContext::local_default(tmp.path().join("fixture-claude"), None);
    connector.scan(&ctx).unwrap()
}

fn assert_valid_role(message: &NormalizedMessage, requirement: &ConnectorRequirement) {
    assert!(
        matches!(
            message.role.as_str(),
            "user" | "assistant" | "system" | "tool"
        ),
        "{} {}: invalid role {:?}",
        requirement.id,
        requirement.description,
        message.role
    );
}

fn assert_message_contracts(conversation: &NormalizedConversation) {
    let idx_requirement = &CLAUDE_CODE_NORMALIZATION_SPEC[3];
    let role_requirement = &CLAUDE_CODE_NORMALIZATION_SPEC[4];
    let content_requirement = &CLAUDE_CODE_NORMALIZATION_SPEC[5];
    let timestamp_requirement = &CLAUDE_CODE_NORMALIZATION_SPEC[6];

    let mut previous_created_at = None;
    for (expected_idx, message) in conversation.messages.iter().enumerate() {
        assert_eq!(
            message.idx, expected_idx as i64,
            "{} {}",
            idx_requirement.id, idx_requirement.description
        );
        assert_valid_role(message, role_requirement);
        assert!(
            !message.content.trim().is_empty(),
            "{} {}",
            content_requirement.id,
            content_requirement.description
        );

        let created_at = message.created_at.unwrap_or_else(|| {
            panic!(
                "{} {}",
                timestamp_requirement.id, timestamp_requirement.description
            )
        });
        if let Some(previous) = previous_created_at {
            assert!(
                created_at >= previous,
                "{} {}: {created_at} came after {previous}",
                timestamp_requirement.id,
                timestamp_requirement.description
            );
        }
        previous_created_at = Some(created_at);
    }
}

fn conversation_by_external_id<'a>(
    conversations: &'a [NormalizedConversation],
    external_id: &str,
) -> &'a NormalizedConversation {
    conversations
        .iter()
        .find(|conversation| conversation.external_id.as_deref() == Some(external_id))
        .unwrap_or_else(|| panic!("missing conversation with external_id {external_id}"))
}

#[test]
fn claude_code_connector_output_conforms_to_normalized_contract() {
    let conversations = scan_fixture();
    assert_eq!(conversations.len(), 2);

    let must_count = CLAUDE_CODE_NORMALIZATION_SPEC
        .iter()
        .filter(|req| req.level == RequirementLevel::Must)
        .count();
    let should_count = CLAUDE_CODE_NORMALIZATION_SPEC
        .iter()
        .filter(|req| req.level == RequirementLevel::Should)
        .count();
    assert_eq!(must_count, 8, "coverage matrix drifted");
    assert_eq!(should_count, 3, "coverage matrix drifted");

    for conversation in &conversations {
        let slug_requirement = &CLAUDE_CODE_NORMALIZATION_SPEC[0];
        assert_eq!(
            conversation.agent_slug, "claude_code",
            "{} {}",
            slug_requirement.id, slug_requirement.description
        );

        let source_requirement = &CLAUDE_CODE_NORMALIZATION_SPEC[1];
        assert!(
            conversation
                .source_path
                .extension()
                .is_some_and(|ext| ext == "jsonl"),
            "{} {}",
            source_requirement.id,
            source_requirement.description
        );
        assert!(
            conversation
                .external_id
                .as_deref()
                .is_some_and(|id| id.starts_with("projects/org/team/") && id.ends_with(".jsonl")),
            "{} {}",
            source_requirement.id,
            source_requirement.description
        );

        let messages_requirement = &CLAUDE_CODE_NORMALIZATION_SPEC[2];
        assert!(
            !conversation.messages.is_empty(),
            "{} {}",
            messages_requirement.id,
            messages_requirement.description
        );
        assert_message_contracts(conversation);

        let bounds_requirement = &CLAUDE_CODE_NORMALIZATION_SPEC[7];
        let first_message_ts = conversation.messages.first().and_then(|msg| msg.created_at);
        let last_message_ts = conversation.messages.last().and_then(|msg| msg.created_at);
        assert_eq!(
            conversation.started_at, first_message_ts,
            "{} {}",
            bounds_requirement.id, bounds_requirement.description
        );
        assert_eq!(
            conversation.ended_at, last_message_ts,
            "{} {}",
            bounds_requirement.id, bounds_requirement.description
        );
    }

    let alpha = conversation_by_external_id(&conversations, "projects/org/team/alpha.jsonl");
    assert_eq!(alpha.messages.len(), 3);
    assert!(
        alpha
            .messages
            .iter()
            .all(|message| !message.content.contains("Summary should not surface"))
    );

    let title_requirement = &CLAUDE_CODE_NORMALIZATION_SPEC[8];
    assert_eq!(
        alpha.title.as_deref(),
        Some("Plan conformance for Claude Code"),
        "{} {}",
        title_requirement.id,
        title_requirement.description
    );

    let metadata_requirement = &CLAUDE_CODE_NORMALIZATION_SPEC[9];
    assert_eq!(
        alpha
            .metadata
            .get("sessionId")
            .and_then(|value| value.as_str()),
        Some("session-alpha"),
        "{} {}",
        metadata_requirement.id,
        metadata_requirement.description
    );
    assert_eq!(
        alpha
            .metadata
            .get("gitBranch")
            .and_then(|value| value.as_str()),
        Some("main"),
        "{} {}",
        metadata_requirement.id,
        metadata_requirement.description
    );

    let assistant_requirement = &CLAUDE_CODE_NORMALIZATION_SPEC[10];
    let assistant = alpha
        .messages
        .iter()
        .find(|message| message.role == "assistant")
        .expect("fixture includes assistant message");
    assert_eq!(
        assistant.author.as_deref(),
        Some("claude-sonnet-4"),
        "{} {}",
        assistant_requirement.id,
        assistant_requirement.description
    );
    assert!(
        assistant.content.contains("[Tool: Read"),
        "{} {}",
        assistant_requirement.id,
        assistant_requirement.description
    );
    assert_eq!(
        assistant.invocations.len(),
        1,
        "{} {}",
        assistant_requirement.id,
        assistant_requirement.description
    );
    let invocation = &assistant.invocations[0];
    assert_eq!(invocation.kind, "tool");
    assert_eq!(invocation.name, "Read");
    assert_eq!(invocation.call_id.as_deref(), Some("toolu_1"));
    assert_eq!(
        invocation
            .arguments
            .as_ref()
            .and_then(|args| args.get("file_path"))
            .and_then(|value| value.as_str()),
        Some("/workspace/alpha/session.jsonl")
    );
}
