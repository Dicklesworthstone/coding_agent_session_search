use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use coding_agent_search::model::types::{
    Agent, AgentKind, Conversation, Message, MessageRole, Snippet,
};
use coding_agent_search::pages::bundle::{BundleBuilder, BundleConfig};
use coding_agent_search::pages::encrypt::EncryptionEngine;
use coding_agent_search::pages::export::{ExportEngine, ExportFilter, PathMode};
use coding_agent_search::storage::sqlite::SqliteStorage;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Preset {
    Small,
    Medium,
    Large,
    Xlarge,
}

impl Preset {
    fn message_target(self) -> usize {
        match self {
            Preset::Small => 1_000,
            Preset::Medium => 10_000,
            Preset::Large => 50_000,
            Preset::Xlarge => 100_000,
        }
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "cass-pages-perf-bundle",
    about = "Generate synthetic pages bundle for perf testing"
)]
struct Args {
    /// Output directory for perf bundle assets
    #[arg(long)]
    output: PathBuf,

    /// Size preset (small, medium, large, xlarge)
    #[arg(long, value_enum, default_value_t = Preset::Small)]
    preset: Preset,

    /// Override total message count (defaults to preset)
    #[arg(long, default_value_t = 0)]
    messages: usize,

    /// Number of conversations to generate
    #[arg(long, default_value_t = 100)]
    conversations: usize,

    /// Approximate message length (characters)
    #[arg(long, default_value_t = 256)]
    message_len: usize,

    /// Password for encryption
    #[arg(long, default_value = "test-password")]
    password: String,

    /// Optional recovery secret (string)
    #[arg(long)]
    recovery_secret: Option<String>,

    /// Chunk size for encryption in bytes
    #[arg(long, default_value_t = 1024 * 1024)]
    chunk_bytes: usize,

    /// Bundle title
    #[arg(long, default_value = "cass Perf Archive")]
    title: String,

    /// Bundle description
    #[arg(long, default_value = "Synthetic performance fixture")]
    description: String,

    /// Hide metadata in bundle
    #[arg(long, default_value_t = false)]
    hide_metadata: bool,

    /// Output JSON summary to stdout
    #[arg(long, default_value_t = false)]
    json: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();

    let total_messages = if args.messages > 0 {
        args.messages
    } else {
        args.preset.message_target()
    };

    if total_messages == 0 {
        bail!("message count must be > 0");
    }

    let conversation_count = args.conversations.min(total_messages).max(1);
    let messages_per_conv = total_messages / conversation_count;
    let remainder = total_messages % conversation_count;

    let output_root = args.output.clone();
    let db_dir = output_root.join("db");
    let export_dir = output_root.join("export");
    let encrypt_dir = output_root.join("encrypt");
    let bundle_dir = output_root.join("bundle");

    fs::create_dir_all(&db_dir)?;
    fs::create_dir_all(&export_dir)?;
    fs::create_dir_all(&encrypt_dir)?;
    fs::create_dir_all(&bundle_dir)?;

    let db_path = db_dir.join("agent_search.db");
    let export_db_path = export_dir.join("export.db");

    eprintln!(
        "[perf-bundle] generating {} messages across {} conversations",
        total_messages, conversation_count
    );
    generate_db(
        &db_path,
        conversation_count,
        messages_per_conv,
        remainder,
        args.message_len,
    )?;

    eprintln!("[perf-bundle] exporting database...");
    let filter = ExportFilter {
        agents: None,
        workspaces: None,
        since: None,
        until: None,
        path_mode: PathMode::Relative,
    };
    let export_engine = ExportEngine::new(&db_path, &export_db_path, filter);
    let export_stats = export_engine.execute(|_, _| {}, None)?;

    eprintln!("[perf-bundle] encrypting export...");
    let mut enc_engine = EncryptionEngine::new(args.chunk_bytes)?;
    enc_engine.add_password_slot(&args.password)?;
    if let Some(secret) = &args.recovery_secret {
        enc_engine.add_recovery_slot(secret.as_bytes())?;
    }
    let _config = enc_engine.encrypt_file(&export_db_path, &encrypt_dir, |_, _| {})?;

    eprintln!("[perf-bundle] building bundle...");
    let bundle_config = BundleConfig {
        title: args.title.clone(),
        description: args.description.clone(),
        hide_metadata: args.hide_metadata,
        recovery_secret: args.recovery_secret.as_ref().map(|s| s.as_bytes().to_vec()),
        generate_qr: false,
        generated_docs: Vec::new(),
    };

    let builder = BundleBuilder::with_config(bundle_config);
    let bundle_result = builder.build(&encrypt_dir, &bundle_dir, |_, _| {})?;

    let summary = serde_json::json!({
        "messages": total_messages,
        "conversations": conversation_count,
        "export": {
            "conversations_processed": export_stats.conversations_processed,
            "messages_processed": export_stats.messages_processed
        },
        "paths": {
            "output": output_root,
            "db": db_path,
            "export_db": export_db_path,
            "encrypt": encrypt_dir,
            "bundle": bundle_dir,
            "site": bundle_result.site_dir,
            "private": bundle_result.private_dir
        }
    });

    if args.json {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        println!("Generated bundle at {}", bundle_result.site_dir.display());
    }

    Ok(())
}

fn generate_db(
    db_path: &Path,
    conversation_count: usize,
    messages_per_conv: usize,
    remainder: usize,
    message_len: usize,
) -> Result<()> {
    let mut storage = SqliteStorage::open(db_path).context("open sqlite storage")?;

    let agent = Agent {
        id: None,
        slug: "perf_agent".to_string(),
        name: "Perf Agent".to_string(),
        version: None,
        kind: AgentKind::Cli,
    };

    let agent_id = storage.ensure_agent(&agent).context("ensure agent")?;
    let workspace_id = storage
        .ensure_workspace(Path::new("/perf/workspace"), None)
        .context("ensure workspace")?;

    let filler = build_filler(message_len);

    for conv_idx in 0..conversation_count {
        let extra = if conv_idx < remainder { 1 } else { 0 };
        let msg_count = messages_per_conv + extra;
        let base_ts = 1_700_000_000_000i64 + (conv_idx as i64 * 60_000);

        let mut messages = Vec::with_capacity(msg_count);
        for msg_idx in 0..msg_count {
            let role = if msg_idx % 2 == 0 {
                MessageRole::User
            } else {
                MessageRole::Agent
            };
            let content = format!("conv={} msg={} {}", conv_idx, msg_idx, filler);
            let msg = Message {
                id: None,
                idx: msg_idx as i64,
                role,
                author: None,
                created_at: Some(base_ts + (msg_idx as i64 * 1000)),
                content,
                extra_json: empty_json(),
                snippets: Vec::<Snippet>::new(),
            };
            messages.push(msg);
        }

        let conv = Conversation {
            id: None,
            agent_slug: agent.slug.clone(),
            workspace: Some(Path::new("/perf/workspace").to_path_buf()),
            external_id: Some(format!("perf-conv-{conv_idx}")),
            title: Some(format!("Perf Conversation {conv_idx}")),
            source_path: PathBuf::from(format!("/perf/session-{conv_idx}.jsonl")),
            started_at: Some(base_ts),
            ended_at: Some(base_ts + (msg_count as i64 * 1000)),
            approx_tokens: None,
            metadata_json: empty_json(),
            messages,
            source_id: "local".to_string(),
            origin_host: None,
        };

        storage
            .insert_conversation_tree(agent_id, Some(workspace_id), &conv)
            .context("insert conversation")?;
    }

    Ok(())
}

fn build_filler(target_len: usize) -> String {
    if target_len == 0 {
        return String::new();
    }
    let mut s = String::with_capacity(target_len);
    while s.len() < target_len {
        s.push_str("lorem ipsum dolor sit amet ");
    }
    s.truncate(target_len);
    s
}

fn empty_json() -> Value {
    Value::Object(serde_json::Map::new())
}
