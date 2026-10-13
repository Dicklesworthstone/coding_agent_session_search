//! Guarded flat/native Codex identity migration (GH #524).
use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use coding_agent_search::storage::codex_layout_migration::{MigrationOptions, migrate};

#[derive(Parser)]
#[command(about = "Prove and register a same-source Codex layout migration; dry-run by default")]
struct Args {
    /// CASS directory containing the existing agent_search.db (not a custom DB).
    #[arg(long)]
    data_dir: PathBuf,
    /// Exact named local source ID in sources.toml and the archive.
    #[arg(long)]
    source: String,
    /// One existing rollout (flat or native dated).
    #[arg(long)]
    from: PathBuf,
    /// The matching rollout in the other layout. Both files must exist.
    #[arg(long)]
    to: PathBuf,
    /// Publish the receipt. Does not rewrite the DB or either source file.
    #[arg(long)]
    apply: bool,
    /// Machine-readable validation/migration report.
    #[arg(long)]
    json: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let report = migrate(&MigrationOptions {
        data_dir: args.data_dir,
        source_id: args.source,
        from: args.from,
        to: args.to,
        apply: args.apply,
    })?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "{}: retain Codex conversation {} ({}) and {} existing messages for source {}.",
            if report.applied { "Registered" } else { "Dry-run verified" },
            report.canonical_conversation_id,
            report.canonical_external_id,
            report.retained_messages,
            report.source_id,
        );
        if !report.applied {
            println!("Re-run with --apply before indexing the second layout.");
        }
    }
    Ok(())
}
