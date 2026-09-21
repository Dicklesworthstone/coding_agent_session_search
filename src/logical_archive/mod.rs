//! Explicit logical-archive commands. Kept outside the ordinary search startup
//! path: these commands do not construct search indexes, models or maintenance.

mod codec;
mod export;
mod import;
mod reimport;

use std::path::PathBuf;

use anyhow::{Result, anyhow, ensure};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "cass", disable_version_flag = true)]
struct Cli {
    /// Existing canonical archive. Export never creates or repairs this file.
    #[arg(long, global = true)]
    db: Option<PathBuf>,
    /// Explicit canonical archive directory (also accepts CASS_DATA_DIR).
    #[arg(long, global = true, env = "CASS_DATA_DIR")]
    data_dir: Option<PathBuf>,
    /// Receipts are JSON regardless of this compatibility flag.
    #[arg(long, global = true, visible_alias = "robot")]
    json: bool,
    #[command(subcommand)]
    command: Root,
}

#[derive(Subcommand)]
enum Root {
    /// Export, verify or restore a bounded, versioned logical canonical archive.
    Archive {
        #[command(subcommand)]
        command: Operation,
    },
}

#[derive(Subcommand)]
enum Operation {
    /// Stream a read-only snapshot to a NEW private JSONL file.
    Export {
        #[arg(long)]
        output: PathBuf,
        /// Stable source archive identity; reuse it for subsequent snapshots.
        /// It is supplied explicitly, never inferred from a filesystem path.
        #[arg(long)]
        archive_id: String,
        /// Acknowledge that full session bodies and metadata are exported.
        #[arg(long)]
        include_private: bool,
    },
    /// Verify framing, identities, counts and digest without opening a database.
    Verify {
        input: PathBuf,
    },
    /// Restore all canonical rows into a NEW database; never replace an archive.
    Import {
        input: PathBuf,
        /// New database file, not a data directory or an existing live archive.
        #[arg(long)]
        output: PathBuf,
        /// Require this exact source identity before creating a restore candidate.
        #[arg(long)]
        archive_id: String,
        /// Acknowledge that full private session bodies will be restored.
        #[arg(long)]
        include_private: bool,
        /// Accept an existing destination only after a read-only full-digest match.
        #[arg(long)]
        if_identical: bool,
    },
}

pub fn run(args: Vec<String>) -> Result<()> {
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(error) if matches!(error.kind(), clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion) => {
            error.print()?;
            return Ok(());
        }
        Err(error) => return Err(anyhow!(error.to_string())),
    };
    let _ = cli.json;
    let Root::Archive { command } = cli.command;
    let mut destination_status = None;
    let (operation, header, completion) = match command {
        Operation::Export { output, archive_id, include_private } => {
            ensure!(include_private, "full-fidelity export contains private session data; pass --include-private to acknowledge this");
            let source = cli.db.or_else(|| cli.data_dir.map(|directory| directory.join("agent_search.db")))
                .ok_or_else(|| anyhow!("export requires an explicit --db or --data-dir (or CASS_DATA_DIR)"))?;
            let (header, completion) = export::export_file(&source, &output, archive_id)?;
            ("export", header, completion)
        }
        Operation::Verify { input } => {
            let (header, completion) = export::verify_file(&input)?;
            ("verify", header, completion)
        }
        Operation::Import { input, output, archive_id, include_private, if_identical } => {
            ensure!(include_private, "restoration writes private session data; pass --include-private to acknowledge this");
            let (header, completion, created) = if if_identical {
                import::import_file_with_policy(&input, &output, &archive_id, true)?
            } else {
                let (header, completion) = import::import_file(&input, &output, &archive_id)?;
                (header, completion, true)
            };
            destination_status = Some(if created { "created" } else { "unchanged" });
            ("import", header, completion)
        }
    };
    let mut receipt = serde_json::json!({
        "operation": operation,
        "format": codec::FORMAT,
        "schema_version": codec::VERSION,
        "archive_id": header.archive_id,
        "records": completion.records,
        "tables": completion.tables,
        "content_sha256": completion.content_sha256,
        "contains_private_data": header.contains_private_data,
        "derived_search_assets": "omitted_rebuild_required",
        "integrity_verified": true
    });
    if let Some(status) = destination_status {
        receipt["destination_status"] = serde_json::Value::String(status.to_owned());
    }
    println!("{receipt}");
    Ok(())
}
