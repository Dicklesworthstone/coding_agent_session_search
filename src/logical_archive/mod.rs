//! Explicit logical-archive commands. Kept outside the ordinary search startup
//! path. Only explicit import flags opt into canonical-only lexical rebuilding
//! or compatible additive schema migration; export and verification never do so.

mod codec;
mod export;
mod import;
mod migrate;
mod query;
mod reimport;

use std::path::PathBuf;

use anyhow::{Result, anyhow, ensure};
use clap::{Parser, Subcommand};
use coding_agent_search::search::archive_rebuild::ArchiveIndexPlan;

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
    /// Search verified backup message bodies without restoring a DB or index.
    Search {
        input: PathBuf,
        /// Case-sensitive literal substring, not indexed query syntax.
        #[arg(long)]
        contains: String,
        /// Maximum retained matches (1..100); the entire backup is verified.
        #[arg(long, default_value_t = 25)]
        limit: usize,
        /// Restrict matching to this exact positive canonical conversation ID.
        #[arg(long)]
        conversation_id: Option<i64>,
        /// Continue with next_cursor from the same backup and query/filter.
        #[arg(long)]
        cursor: Option<String>,
        /// Acknowledge that result excerpts contain private session content.
        #[arg(long)]
        include_private: bool,
    },
    /// Read complete bounded message bodies directly from a verified backup.
    View {
        input: PathBuf,
        /// Exact canonical message ID from archive search, not a physical line.
        #[arg(long)]
        message_id: i64,
        /// Bind the selected ID to the content_sha256 returned by search/verify.
        #[arg(long)]
        content_sha256: String,
        /// Actual messages on each side (0..20); complete bodies must fit 64 KiB.
        #[arg(long, short = 'C', default_value_t = 2)]
        context: usize,
        /// Acknowledge that complete private session text will be displayed.
        #[arg(long)]
        include_private: bool,
    },
    /// Restore canonical rows into a NEW database; never replace an archive.
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
        /// Permit a structurally compatible older archive to be replayed into
        /// today's initializer schema. Renamed/removed fields and PK drift fail.
        #[arg(long)]
        allow_compatible_schema: bool,
        /// Rebuild lexical search from the restored DB, without scanning providers.
        /// Output must be <existing-data-directory>/agent_search.db, and input
        /// must be outside that directory. The DB is retained if indexing fails.
        #[arg(long)]
        rebuild_index: bool,
    },
}

pub fn run(args: Vec<String>) -> Result<()> {
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(error)
            if matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) =>
        {
            error.print()?;
            return Ok(());
        }
        Err(error) => return Err(anyhow!(error.to_string())),
    };
    let _ = cli.json;
    let Root::Archive { command } = cli.command;
    let mut destination_status = None;
    let mut lexical_rebuild = None;
    let mut schema_migration = None;
    let (operation, header, completion) = match command {
        Operation::Export {
            output,
            archive_id,
            include_private,
        } => {
            ensure!(
                include_private,
                "full-fidelity export contains private session data; pass --include-private to acknowledge this"
            );
            let source = cli
                .db
                .or_else(|| {
                    cli.data_dir
                        .map(|directory| directory.join("agent_search.db"))
                })
                .ok_or_else(|| {
                    anyhow!(
                        "export requires an explicit --db or --data-dir (or CASS_DATA_DIR)"
                    )
                })?;
            let (header, completion) = export::export_file(&source, &output, archive_id)?;
            ("export", header, completion)
        }
        Operation::Verify { input } => {
            let (header, completion) = export::verify_file(&input)?;
            ("verify", header, completion)
        }
        Operation::Search {
            input,
            contains,
            limit,
            conversation_id,
            cursor,
            include_private,
        } => {
            ensure!(
                include_private,
                "backup search emits private session excerpts; pass --include-private to acknowledge this"
            );
            let result =
                query::search(&input, &contains, limit, conversation_id, cursor.as_deref())?;
            println!("{result}");
            return Ok(());
        }
        Operation::View {
            input,
            message_id,
            content_sha256,
            context,
            include_private,
        } => {
            ensure!(
                include_private,
                "backup view emits private session text; pass --include-private to acknowledge this"
            );
            let result = query::view(&input, message_id, context, &content_sha256)?;
            println!("{result}");
            return Ok(());
        }
        Operation::Import {
            input,
            output,
            archive_id,
            include_private,
            if_identical,
            allow_compatible_schema,
            rebuild_index,
        } => {
            ensure!(
                include_private,
                "restoration writes private session data; pass --include-private to acknowledge this"
            );
            ensure!(
                !(allow_compatible_schema && if_identical),
                "--allow-compatible-schema currently requires a new destination; omit --if-identical"
            );
            let plan = rebuild_index
                .then(|| ArchiveIndexPlan::prepare(&output, &input))
                .transpose()?;
            let output = plan
                .as_ref()
                .map_or(output.as_path(), ArchiveIndexPlan::database);
            let (header, completion, created) = if allow_compatible_schema {
                let outcome = migrate::import_compatible(&input, output, &archive_id)?;
                schema_migration = outcome.migration;
                (outcome.header, outcome.completion, outcome.created)
            } else if if_identical {
                import::import_file_with_policy(&input, output, &archive_id, true)?
            } else {
                let (header, completion) = import::import_file(&input, output, &archive_id)?;
                (header, completion, true)
            };
            destination_status = Some(if created { "created" } else { "unchanged" });
            if let Some(plan) = plan {
                lexical_rebuild = Some(plan.rebuild().map_err(|error| {
                    anyhow!(
                        "canonical archive was {} and is retained at {}; lexical rebuild failed: {}; retry the same import with --if-identical --rebuild-index",
                        if created { "created" } else { "verified unchanged" },
                        plan.database().display(),
                        error,
                    )
                })?);
            }
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
    if let Some(migration) = schema_migration {
        receipt["schema_migration"] = serde_json::to_value(migration)?;
    }
    if let Some(rebuild) = lexical_rebuild {
        receipt["derived_search_assets"] =
            serde_json::json!("lexical_rebuilt_semantic_not_built");
        receipt["lexical_rebuild"] = serde_json::to_value(rebuild)?;
    }
    println!("{receipt}");
    Ok(())
}
