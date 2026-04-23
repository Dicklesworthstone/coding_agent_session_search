use coding_agent_search::connectors::clawdbot::ClawdbotConnector;
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

const CLAWDBOT_NORMALIZATION_SPEC: &[ConnectorRequirement] = &[
    ConnectorRequirement {
        id: "CB-MUST-001",
        level: RequirementLevel::Must,
        description: "each emitted conversation uses the clawdbot agent slug",
    },
    ConnectorRequirement {
        id: "CB-MUST-002",
        level: RequirementLevel::Must,
        description: "each emitted conversation has a source path and filename-derived external id",
    },
    ConnectorRequirement {
        id: "CB-MUST-003",
        level: RequirementLevel::Must,
        description: "each emitted conversation contains at least one normalized message",
    },
    ConnectorRequirement {
        id: "CB-MUST-004",
        level: RequirementLevel::Must,
        description: "message indices are contiguous and start at zero",
    },
    ConnectorRequirement {
        id: "CB-MUST-005",
        level: RequirementLevel::Must,
        description: "message roles are non-empty members of the normalized role enum",
    },
    ConnectorRequirement {
        id: "CB-MUST-006",
        level: RequirementLevel::Must,
        description: "message content is non-empty after connector filtering",
    },
    ConnectorRequirement {
        id: "CB-MUST-007",
        level: RequirementLevel::Must,
        description: "message timestamps are present and monotonically nondecreasing",
    },
    ConnectorRequirement {
        id: "CB-MUST-008",
        level: RequirementLevel::Must,
        description: "conversation start and end timestamps bound emitted messages",
    },
    ConnectorRequirement {
        id: "CB-SHOULD-001",
        level: RequirementLevel::Should,
        description: "conversation title is derived from the first user message",
    },
];

fn write_session(root: &Path, name: &str, lines: &[&str]) -> std::path::PathBuf {
    let path = root.join(name);
    fs::write(&path, lines.join("\n")).unwrap();
    path
}

fn scan_fixture() -> Vec<NormalizedConversation> {
    let tmp = TempDir::new().unwrap();
    let sessions = tmp.path().join(".clawdbot/sessions");
    fs::create_dir_all(&sessions).unwrap();

    write_session(
        &sessions,
        "alpha.jsonl",
        &[
            r#"{"role":"user","content":"Plan a conformance harness","timestamp":"2025-06-15T10:00:00.000Z"}"#,
            r#"{"role":"assistant","content":"Use a fixture-driven contract matrix.","timestamp":"2025-06-15T10:00:02.000Z"}"#,
            r#"{"role":"user","content":"Also validate timestamps.","timestamp":"2025-06-15T10:00:04.000Z"}"#,
        ],
    );
    write_session(
        &sessions,
        "beta.jsonl",
        &[
            r#"{"role":"user","content":"Second session","timestamp":"2025-06-15T11:00:00.000Z"}"#,
            r#"{"role":"assistant","content":"Still normalized.","timestamp":"2025-06-15T11:00:01.000Z"}"#,
        ],
    );

    let connector = ClawdbotConnector::new();
    let ctx = ScanContext::local_default(sessions, None);
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
    let idx_requirement = &CLAWDBOT_NORMALIZATION_SPEC[3];
    let role_requirement = &CLAWDBOT_NORMALIZATION_SPEC[4];
    let content_requirement = &CLAWDBOT_NORMALIZATION_SPEC[5];
    let timestamp_requirement = &CLAWDBOT_NORMALIZATION_SPEC[6];

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

#[test]
fn clawdbot_connector_output_conforms_to_normalized_contract() {
    let conversations = scan_fixture();
    assert_eq!(conversations.len(), 2);

    let must_count = CLAWDBOT_NORMALIZATION_SPEC
        .iter()
        .filter(|req| req.level == RequirementLevel::Must)
        .count();
    let should_count = CLAWDBOT_NORMALIZATION_SPEC
        .iter()
        .filter(|req| req.level == RequirementLevel::Should)
        .count();
    assert_eq!(must_count, 8, "coverage matrix drifted");
    assert_eq!(should_count, 1, "coverage matrix drifted");

    for conversation in &conversations {
        let slug_requirement = &CLAWDBOT_NORMALIZATION_SPEC[0];
        assert_eq!(
            conversation.agent_slug, "clawdbot",
            "{} {}",
            slug_requirement.id, slug_requirement.description
        );

        let source_requirement = &CLAWDBOT_NORMALIZATION_SPEC[1];
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
                .is_some_and(|id| !id.is_empty()),
            "{} {}",
            source_requirement.id,
            source_requirement.description
        );

        let messages_requirement = &CLAWDBOT_NORMALIZATION_SPEC[2];
        assert!(
            !conversation.messages.is_empty(),
            "{} {}",
            messages_requirement.id,
            messages_requirement.description
        );
        assert_message_contracts(conversation);

        let bounds_requirement = &CLAWDBOT_NORMALIZATION_SPEC[7];
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

        let title_requirement = &CLAWDBOT_NORMALIZATION_SPEC[8];
        let first_user_content = conversation
            .messages
            .iter()
            .find(|msg| msg.role == "user")
            .map(|msg| msg.content.as_str());
        assert_eq!(
            conversation.title.as_deref(),
            first_user_content,
            "{} {}",
            title_requirement.id,
            title_requirement.description
        );
    }
}
