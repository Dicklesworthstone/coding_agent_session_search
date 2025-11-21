pub mod config;
pub mod connectors;
pub mod indexer;
pub mod model;
pub mod search;
pub mod storage;
pub mod ui;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

use connectors::{cline::ClineConnector, codex::CodexConnector, Connector, NormalizedConversation};
use directories::ProjectDirs;
use model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
use storage::sqlite::SqliteStorage;

/// Command-line interface.
#[derive(Parser, Debug)]
#[command(
    name = "coding-agent-search",
    version,
    about = "Unified TUI search over coding agent histories"
)]
pub struct Cli {
    /// Path to the SQLite database (defaults to platform data dir)
    #[arg(long)]
    pub db: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Launch interactive TUI
    Tui,
    /// Run indexer (stub)
    Index {
        /// Perform full rebuild
        #[arg(long)]
        full: bool,
    },
}

pub async fn run() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Tui => ui::tui::run_tui(),
        Commands::Index { full } => run_index(cli.db, full),
    }
}

fn run_index(db_override: Option<PathBuf>, full: bool) -> Result<()> {
    let db_path = db_override.unwrap_or_else(default_db_path);
    let mut storage = SqliteStorage::open(&db_path)?;

    let connectors: Vec<Box<dyn Connector>> = vec![
        Box::new(CodexConnector::new()),
        Box::new(ClineConnector::new()),
    ];

    for conn in connectors {
        let detect = conn.detect();
        if !detect.detected {
            continue;
        }

        let ctx = connectors::ScanContext {
            data_root: dirs::home_dir().unwrap_or_default(),
            since_ts: None,
        };

        let convs = conn.scan(&ctx)?;
        for conv in convs {
            persist_conversation(&mut storage, &conv)?;
        }
    }

    if full {
        tracing::info!(target: "index", "full index run complete (partial connectors)");
    }

    Ok(())
}

fn persist_conversation(storage: &mut SqliteStorage, conv: &NormalizedConversation) -> Result<()> {
    let agent = Agent {
        slug: conv.agent_slug.clone(),
        name: conv.agent_slug.clone(),
        version: None,
        kind: AgentKind::Cli,
    };
    let agent_id = storage.ensure_agent(&agent)?;

    let workspace_id = if let Some(ws) = &conv.workspace {
        Some(storage.ensure_workspace(ws, None)?)
    } else {
        None
    };

    let messages = conv
        .messages
        .iter()
        .map(|m| Message {
            idx: m.idx,
            role: map_role(&m.role),
            author: m.author.clone(),
            created_at: m.created_at,
            content: m.content.clone(),
            extra_json: m.extra.clone(),
            snippets: Vec::new(),
        })
        .collect();

    let conversation = Conversation {
        id: 0,
        agent_slug: conv.agent_slug.clone(),
        workspace: conv.workspace.clone(),
        external_id: conv.external_id.clone(),
        title: conv.title.clone(),
        source_path: conv.source_path.clone(),
        started_at: conv.started_at,
        ended_at: conv.ended_at,
        approx_tokens: None,
        metadata_json: conv.metadata.clone(),
        messages,
    };

    let _ = storage.insert_conversation_tree(agent_id, workspace_id, &conversation)?;
    Ok(())
}

fn map_role(role: &str) -> MessageRole {
    match role {
        "user" => MessageRole::User,
        "assistant" | "agent" => MessageRole::Agent,
        "tool" => MessageRole::Tool,
        "system" => MessageRole::System,
        other => MessageRole::Other(other.to_string()),
    }
}

fn default_db_path() -> PathBuf {
    let dirs = directories::ProjectDirs::from("com", "coding-agent-search", "coding-agent-search")
        .expect("project dirs available");
    dirs.data_dir().join("agent_search.db")
}
