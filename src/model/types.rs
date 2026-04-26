//! Normalized entity structs.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Roles seen across source agents.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum MessageRole {
    User,
    Agent,
    Tool,
    System,
    Other(String),
}

impl std::fmt::Display for MessageRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MessageRole::User => write!(f, "User"),
            MessageRole::Agent => write!(f, "Agent"),
            MessageRole::Tool => write!(f, "Tool"),
            MessageRole::System => write!(f, "System"),
            MessageRole::Other(s) => write!(f, "{}", s),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub id: Option<i64>,
    pub slug: String,
    pub name: String,
    pub version: Option<String>,
    pub kind: AgentKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum AgentKind {
    Cli,
    VsCode,
    Hybrid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub id: Option<i64>,
    pub path: PathBuf,
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conversation {
    pub id: Option<i64>,
    pub agent_slug: String,
    pub workspace: Option<PathBuf>,
    pub external_id: Option<String>,
    pub title: Option<String>,
    pub source_path: PathBuf,
    pub started_at: Option<i64>,
    pub ended_at: Option<i64>,
    pub approx_tokens: Option<i64>,
    pub metadata_json: serde_json::Value,
    pub messages: Vec<Message>,
    /// Source ID for provenance tracking (e.g., "local", "work-laptop").
    /// Defaults to "local" for backward compatibility.
    #[serde(default = "default_source_id")]
    pub source_id: String,
    /// Origin host label for remote sources.
    #[serde(default)]
    pub origin_host: Option<String>,
}

fn default_source_id() -> String {
    "local".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: Option<i64>,
    pub idx: i64,
    pub role: MessageRole,
    pub author: Option<String>,
    pub created_at: Option<i64>,
    pub content: String,
    pub extra_json: serde_json::Value,
    pub snippets: Vec<Snippet>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snippet {
    pub id: Option<i64>,
    pub file_path: Option<PathBuf>,
    pub start_line: Option<i64>,
    pub end_line: Option<i64>,
    pub language: Option<String>,
    pub snippet_text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tag {
    pub id: Option<i64>,
    pub name: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{from_value, json, to_value};

    fn message_fixture(content: impl Into<String>) -> Message {
        Message {
            id: None,
            idx: 0,
            role: MessageRole::User,
            author: None,
            created_at: None,
            content: content.into(),
            extra_json: json!(null),
            snippets: vec![],
        }
    }

    fn conversation_fixture(agent_slug: &str, source_path: &str) -> Conversation {
        Conversation {
            id: None,
            agent_slug: agent_slug.to_string(),
            workspace: None,
            external_id: None,
            title: None,
            source_path: PathBuf::from(source_path),
            started_at: None,
            ended_at: None,
            approx_tokens: None,
            metadata_json: json!(null),
            messages: vec![],
            source_id: "local".to_string(),
            origin_host: None,
        }
    }

    // =========================
    // MessageRole Tests
    // =========================

    #[test]
    fn message_role_display() {
        let cases = [
            (MessageRole::User, "User"),
            (MessageRole::Agent, "Agent"),
            (MessageRole::Tool, "Tool"),
            (MessageRole::System, "System"),
            (MessageRole::Other("Custom".to_string()), "Custom"),
            (MessageRole::Other("".to_string()), ""),
            (MessageRole::Other("日本語".to_string()), "日本語"),
        ];

        for (role, expected_display) in cases {
            let actual_display = role.to_string();
            assert_eq!(actual_display, expected_display, "role: {role:?}");
        }
    }

    #[test]
    fn message_role_serde_roundtrip() {
        let roles = vec![
            MessageRole::User,
            MessageRole::Agent,
            MessageRole::Tool,
            MessageRole::System,
            MessageRole::Other("CustomRole".to_string()),
        ];

        for role in roles {
            let serialized = to_value(&role).unwrap();
            let deserialized: MessageRole = from_value(serialized).unwrap();
            assert_eq!(role, deserialized);
        }
    }

    #[test]
    fn message_role_equality() {
        assert_eq!(MessageRole::User, MessageRole::User);
        assert_ne!(MessageRole::User, MessageRole::Agent);
        assert_eq!(
            MessageRole::Other("x".to_string()),
            MessageRole::Other("x".to_string())
        );
        assert_ne!(
            MessageRole::Other("x".to_string()),
            MessageRole::Other("y".to_string())
        );
    }

    // =========================
    // AgentKind Tests
    // =========================

    #[test]
    fn agent_kind_serde_roundtrip() {
        let kinds = vec![AgentKind::Cli, AgentKind::VsCode, AgentKind::Hybrid];

        for kind in kinds {
            let serialized = to_value(&kind).unwrap();
            let deserialized: AgentKind = from_value(serialized).unwrap();
            assert_eq!(kind, deserialized);
        }
    }

    #[test]
    fn agent_kind_equality() {
        assert_eq!(AgentKind::Cli, AgentKind::Cli);
        assert_ne!(AgentKind::Cli, AgentKind::VsCode);
        assert_ne!(AgentKind::VsCode, AgentKind::Hybrid);
    }

    // =========================
    // Agent Tests
    // =========================

    #[test]
    fn agent_serde_roundtrip() {
        let agent = Agent {
            id: Some(42),
            slug: "claude-code".to_string(),
            name: "Claude Code".to_string(),
            version: Some("1.0.0".to_string()),
            kind: AgentKind::Cli,
        };

        let json = serde_json::to_string(&agent).unwrap();
        let deserialized: Agent = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.id, Some(42));
        assert_eq!(deserialized.slug, "claude-code");
        assert_eq!(deserialized.name, "Claude Code");
        assert_eq!(deserialized.version, Some("1.0.0".to_string()));
        assert_eq!(deserialized.kind, AgentKind::Cli);
    }

    #[test]
    fn agent_with_none_fields() {
        let agent = Agent {
            id: None,
            slug: "test".to_string(),
            name: "Test".to_string(),
            version: None,
            kind: AgentKind::VsCode,
        };

        let json = serde_json::to_string(&agent).unwrap();
        let deserialized: Agent = serde_json::from_str(&json).unwrap();

        assert!(deserialized.id.is_none());
        assert!(deserialized.version.is_none());
    }

    // =========================
    // Workspace Tests
    // =========================

    #[test]
    fn workspace_serde_roundtrip() {
        let workspace = Workspace {
            id: Some(1),
            path: PathBuf::from("/home/user/project"),
            display_name: Some("My Project".to_string()),
        };

        let json = serde_json::to_string(&workspace).unwrap();
        let deserialized: Workspace = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.id, Some(1));
        assert_eq!(deserialized.path, PathBuf::from("/home/user/project"));
        assert_eq!(deserialized.display_name, Some("My Project".to_string()));
    }

    #[test]
    fn workspace_with_unicode_path() {
        let workspace = Workspace {
            id: None,
            path: PathBuf::from("/home/用户/プロジェクト"),
            display_name: Some("日本語プロジェクト".to_string()),
        };

        let json = serde_json::to_string(&workspace).unwrap();
        let deserialized: Workspace = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.path, PathBuf::from("/home/用户/プロジェクト"));
        assert_eq!(
            deserialized.display_name,
            Some("日本語プロジェクト".to_string())
        );
    }

    // =========================
    // Tag Tests
    // =========================

    #[test]
    fn tag_serde_roundtrip() {
        let tag = Tag {
            id: Some(100),
            name: "important".to_string(),
        };

        let json = serde_json::to_string(&tag).unwrap();
        let deserialized: Tag = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.id, Some(100));
        assert_eq!(deserialized.name, "important");
    }

    #[test]
    fn tag_with_empty_name() {
        let tag = Tag {
            id: None,
            name: "".to_string(),
        };

        let json = serde_json::to_string(&tag).unwrap();
        let deserialized: Tag = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.name, "");
    }

    // =========================
    // Snippet Tests
    // =========================

    #[test]
    fn snippet_serde_roundtrip() {
        let snippet = Snippet {
            id: Some(1),
            file_path: Some(PathBuf::from("src/main.rs")),
            start_line: Some(10),
            end_line: Some(20),
            language: Some("rust".to_string()),
            snippet_text: Some("fn main() {}".to_string()),
        };

        let json = serde_json::to_string(&snippet).unwrap();
        let deserialized: Snippet = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.id, Some(1));
        assert_eq!(deserialized.file_path, Some(PathBuf::from("src/main.rs")));
        assert_eq!(deserialized.start_line, Some(10));
        assert_eq!(deserialized.end_line, Some(20));
        assert_eq!(deserialized.language, Some("rust".to_string()));
        assert_eq!(deserialized.snippet_text, Some("fn main() {}".to_string()));
    }

    #[test]
    fn snippet_all_none() {
        let snippet = Snippet {
            id: None,
            file_path: None,
            start_line: None,
            end_line: None,
            language: None,
            snippet_text: None,
        };

        let json = serde_json::to_string(&snippet).unwrap();
        let deserialized: Snippet = serde_json::from_str(&json).unwrap();

        assert!(deserialized.id.is_none());
        assert!(deserialized.file_path.is_none());
        assert!(deserialized.start_line.is_none());
        assert!(deserialized.end_line.is_none());
        assert!(deserialized.language.is_none());
        assert!(deserialized.snippet_text.is_none());
    }

    // =========================
    // Message Tests
    // =========================

    #[test]
    fn message_serde_roundtrip() {
        let message = Message {
            id: Some(42),
            idx: 0,
            role: MessageRole::User,
            author: Some("human".to_string()),
            created_at: Some(1700000000000),
            content: "Hello, world!".to_string(),
            extra_json: json!({"key": "value"}),
            snippets: vec![],
        };

        let json = serde_json::to_string(&message).unwrap();
        let deserialized: Message = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.id, Some(42));
        assert_eq!(deserialized.idx, 0);
        assert_eq!(deserialized.role, MessageRole::User);
        assert_eq!(deserialized.author, Some("human".to_string()));
        assert_eq!(deserialized.created_at, Some(1700000000000));
        assert_eq!(deserialized.content, "Hello, world!");
        assert_eq!(deserialized.extra_json, json!({"key": "value"}));
        assert!(deserialized.snippets.is_empty());
    }

    #[test]
    fn message_with_snippets() {
        let snippet = Snippet {
            id: None,
            file_path: Some(PathBuf::from("test.rs")),
            start_line: Some(1),
            end_line: Some(5),
            language: Some("rust".to_string()),
            snippet_text: Some("code".to_string()),
        };

        let mut message = message_fixture("Here's some code");
        message.idx = 1;
        message.role = MessageRole::Agent;
        message.snippets = vec![snippet];

        let json = serde_json::to_string(&message).unwrap();
        let deserialized: Message = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.snippets.len(), 1);
        assert_eq!(deserialized.snippets[0].language, Some("rust".to_string()));
    }

    #[test]
    fn message_with_unicode_content() {
        let mut message = message_fixture("こんにちは世界！🌍");
        message.author = Some("ユーザー".to_string());
        message.extra_json = json!({"emoji": "🎉"});

        let json = serde_json::to_string(&message).unwrap();
        let deserialized: Message = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.content, "こんにちは世界！🌍");
        assert_eq!(deserialized.author, Some("ユーザー".to_string()));
    }

    // =========================
    // Conversation Tests
    // =========================

    #[test]
    fn conversation_serde_roundtrip() {
        let conversation = Conversation {
            id: Some(1),
            agent_slug: "claude-code".to_string(),
            workspace: Some(PathBuf::from("/project")),
            external_id: Some("ext-123".to_string()),
            title: Some("Test Conversation".to_string()),
            source_path: PathBuf::from("/path/to/session.jsonl"),
            started_at: Some(1700000000000),
            ended_at: Some(1700003600000),
            approx_tokens: Some(1000),
            metadata_json: json!({"model": "claude-3"}),
            messages: vec![],
            source_id: "local".to_string(),
            origin_host: None,
        };

        let json = serde_json::to_string(&conversation).unwrap();
        let deserialized: Conversation = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.id, Some(1));
        assert_eq!(deserialized.agent_slug, "claude-code");
        assert_eq!(deserialized.workspace, Some(PathBuf::from("/project")));
        assert_eq!(deserialized.external_id, Some("ext-123".to_string()));
        assert_eq!(deserialized.title, Some("Test Conversation".to_string()));
        assert_eq!(
            deserialized.source_path,
            PathBuf::from("/path/to/session.jsonl")
        );
        assert_eq!(deserialized.started_at, Some(1700000000000));
        assert_eq!(deserialized.ended_at, Some(1700003600000));
        assert_eq!(deserialized.approx_tokens, Some(1000));
        assert_eq!(deserialized.source_id, "local");
        assert!(deserialized.origin_host.is_none());
    }

    #[test]
    fn conversation_source_id_default() {
        // Test that source_id defaults to "local" when not present
        let json = json!({
            "agent_slug": "test",
            "source_path": "/test.jsonl",
            "metadata_json": {},
            "messages": []
        });

        let conversation: Conversation = from_value(json).unwrap();
        assert_eq!(conversation.source_id, "local");
    }

    #[test]
    fn conversation_with_remote_source() {
        let mut conversation = conversation_fixture("codex", "/remote/session.jsonl");
        conversation.source_id = "work-laptop".to_string();
        conversation.origin_host = Some("laptop.local".to_string());

        let json = serde_json::to_string(&conversation).unwrap();
        let deserialized: Conversation = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.source_id, "work-laptop");
        assert_eq!(deserialized.origin_host, Some("laptop.local".to_string()));
    }

    #[test]
    fn conversation_with_messages() {
        let mut conversation = conversation_fixture("test", "/test.jsonl");
        conversation.messages = vec![message_fixture("Hello")];

        let json = serde_json::to_string(&conversation).unwrap();
        let deserialized: Conversation = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.messages.len(), 1);
        assert_eq!(deserialized.messages[0].content, "Hello");
    }

    // =========================
    // Edge Cases
    // =========================

    #[test]
    fn empty_strings_are_valid() {
        let tag = Tag {
            id: None,
            name: "".to_string(),
        };
        let agent = Agent {
            id: None,
            slug: "".to_string(),
            name: "".to_string(),
            version: Some("".to_string()),
            kind: AgentKind::Cli,
        };

        // Both should serialize/deserialize without error
        let tag_json = serde_json::to_string(&tag).unwrap();
        let _: Tag = serde_json::from_str(&tag_json).unwrap();

        let agent_json = serde_json::to_string(&agent).unwrap();
        let _: Agent = serde_json::from_str(&agent_json).unwrap();
    }

    #[test]
    fn large_content_strings() {
        let large_content = "x".repeat(100_000);
        let mut message = message_fixture(large_content.clone());
        message.role = MessageRole::Agent;

        let json = serde_json::to_string(&message).unwrap();
        let deserialized: Message = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.content.len(), 100_000);
    }

    #[test]
    fn special_characters_in_strings() {
        let content = "Hello\nWorld\t\"quoted\"\r\nbackslash\\end";
        let message = message_fixture(content);

        let json = serde_json::to_string(&message).unwrap();
        let deserialized: Message = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.content, content);
    }

    #[test]
    fn negative_line_numbers() {
        // While semantically odd, the type allows negative numbers
        let snippet = Snippet {
            id: Some(-1),
            file_path: None,
            start_line: Some(-10),
            end_line: Some(-5),
            language: None,
            snippet_text: None,
        };

        let json = serde_json::to_string(&snippet).unwrap();
        let deserialized: Snippet = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.start_line, Some(-10));
        assert_eq!(deserialized.end_line, Some(-5));
    }

    #[test]
    fn complex_metadata_json() {
        let metadata = json!({
            "nested": {
                "array": [1, 2, 3],
                "object": {"key": "value"},
                "null": null,
                "bool": true,
                "number": 42.5
            }
        });

        let mut conversation = conversation_fixture("test", "/test.jsonl");
        conversation.metadata_json = metadata.clone();

        let json = serde_json::to_string(&conversation).unwrap();
        let deserialized: Conversation = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.metadata_json, metadata);
    }
}
