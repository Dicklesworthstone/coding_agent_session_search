//! Shipped entry point for the opt-in Codex layout migration (GH #524).
//! The storage proof owns locks, validation and receipt publication. This
//! adapter never indexes a source, repairs an archive or rewrites canonical IDs.

use std::io::{self, Write};
use std::path::PathBuf;

use clap::{Args, CommandFactory, Parser, Subcommand};
use coding_agent_search::storage::codex_layout_migration::{
    MigrationOptions, MigrationReport, migrate,
};
use coding_agent_search::{CliError, CliResult};

#[derive(Parser)]
#[command(name = "cass", disable_version_flag = true)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Prove a same-source flat/dated Codex transition before indexing its second layout.
    /// Dry-run by default. --apply registers a receipt without rewriting archive rows.
    CodexLayoutMigrate(MigrationArgs),
}

#[derive(Args)]
struct MigrationArgs {
    /// Explicit CASS directory containing the existing agent_search.db; no custom DB.
    #[arg(long, value_name = "PATH")]
    data_dir: PathBuf,
    /// Exact named local source configured in sources.toml and the archive.
    #[arg(long, value_name = "SOURCE")]
    source: String,
    /// Existing flat or native dated rollout. Both inputs must remain accessible.
    #[arg(long, value_name = "ROLLOUT")]
    from: PathBuf,
    /// Matching rollout in the other layout, belonging to the same local source.
    #[arg(long, value_name = "ROLLOUT")]
    to: PathBuf,
    /// Register the proven identity before indexing the second layout; default is dry-run.
    #[arg(long)]
    apply: bool,
    /// Emit the migration report as JSON. Fatal errors use stderr, not stdout.
    #[arg(long, alias = "robot")]
    json: bool,
}

pub fn clap_commands() -> Vec<clap::Command> {
    Cli::command()
        .find_subcommand("codex-layout-migrate")
        .cloned()
        .into_iter()
        .collect()
}

fn output_failure(error: io::Error) -> CliError {
    CliError {
        code: 9,
        kind: "codex-layout-output",
        message: error.to_string(),
        hint: None,
        retryable: false,
    }
}

fn render(report: &MigrationReport, json: bool, output: &mut impl Write) -> io::Result<()> {
    // Prepare the complete report before writing. Output failure after apply
    // cannot roll back an already durable receipt and must not invite blind retry.
    let mut bytes = if json {
        serde_json::to_vec_pretty(report).map_err(io::Error::other)?
    } else {
        let mut text = format!(
            "{}: retain Codex conversation {} ({}) and {} existing messages for source {}.",
            if report.applied { "Registered" } else { "Dry-run verified" },
            report.canonical_conversation_id,
            report.canonical_external_id,
            report.retained_messages,
            report.source_id,
        );
        if !report.applied {
            text.push_str("\nRe-run with --apply before indexing the second layout.");
        }
        text.into_bytes()
    };
    bytes.push(b'\n');
    output.write_all(&bytes)?;
    output.flush()
}

fn render_completed(
    report: &MigrationReport,
    json: bool,
    output: &mut impl Write,
) -> CliResult<()> {
    render(report, json, output).map_err(|error| CliError {
        code: 9,
        kind: "codex-layout-output",
        message: format!(
            "Codex migration completed (applied={}), but report output failed: {error}",
            report.applied,
        ),
        hint: Some(format!(
            "Inspect the receipt at {} before retrying; an applied receipt is not rolled back by an output failure.",
            report.receipt_path.display(),
        )),
        retryable: false,
    })
}

pub fn run(args: Vec<String>) -> CliResult<()> {
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(error) => {
            let code = error.exit_code();
            error.print().map_err(output_failure)?;
            return if code == 0 {
                Ok(())
            } else {
                Err(CliError::already_reported(code, "argument_parsing", false))
            };
        }
    };
    let Command::CodexLayoutMigrate(args) = cli.command;
    let report = migrate(&MigrationOptions {
        data_dir: args.data_dir,
        source_id: args.source,
        from: args.from,
        to: args.to,
        apply: args.apply,
    })
    .map_err(|error| CliError {
        code: 5,
        kind: "codex-layout-migration",
        message: format!("Codex layout migration failed: {error:#}"),
        hint: Some(
            "Stop indexing and inspect both rollouts, same-source ownership and any existing receipt before retrying. This command never rewrites canonical rows or source files."
                .into(),
        ),
        retryable: false,
    })?;
    render_completed(&report, args.json, &mut io::stdout().lock())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments() -> Vec<&'static str> {
        vec![
            "cass", "codex-layout-migrate", "--data-dir", "/archive",
            "--source", "history", "--from", "/flat/rollout.jsonl",
            "--to", "/dated/rollout.jsonl",
        ]
    }

    fn report(applied: bool) -> MigrationReport {
        MigrationReport {
            applied,
            already_registered: false,
            source_id: "history".into(),
            canonical_conversation_id: 7,
            canonical_external_id: "retained-id".into(),
            retained_messages: 3,
            receipt_path: "/archive/receipt.json".into(),
            flat_path: "/flat/rollout.jsonl".into(),
            dated_path: "/dated/rollout.jsonl".into(),
        }
    }

    #[test]
    fn explicit_scope_and_opt_in_are_required_before_migration() {
        Cli::command().debug_assert();
        assert_eq!(clap_commands()[0].get_name(), "codex-layout-migrate");
        let Command::CodexLayoutMigrate(args) = Cli::try_parse_from(arguments()).unwrap().command;
        assert!(!args.apply && !args.json);
        for option in ["--data-dir", "--source", "--from", "--to"] {
            let mut args = arguments();
            let index = args.iter().position(|value| *value == option).unwrap();
            drop(args.drain(index..index + 2));
            assert!(Cli::try_parse_from(args).is_err(), "missing {option}");
        }
        let mut args = arguments();
        args.extend(["--apply", "--robot"]);
        let Command::CodexLayoutMigrate(args) = Cli::try_parse_from(args).unwrap().command;
        assert!(args.apply && args.json);
        for flag in ["--force", "--full", "--semantic", "--apply=true", "--db"] {
            let mut args = arguments();
            args.push(flag);
            assert!(Cli::try_parse_from(args).is_err(), "unsupported {flag}");
        }
    }

    #[test]
    fn report_keeps_the_companion_schema_and_dry_run_guidance() {
        for applied in [false, true] {
            let report = report(applied);
            let mut json = Vec::new();
            render(&report, true, &mut json).unwrap();
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&json).unwrap(),
                serde_json::to_value(&report).unwrap(),
            );
            let mut text = Vec::new();
            render(&report, false, &mut text).unwrap();
            let text = String::from_utf8(text).unwrap();
            assert_eq!(text.contains("Re-run with --apply"), !applied);
        }
    }

    struct FailedOutput;
    impl Write for FailedOutput {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed consumer"))
        }
        fn flush(&mut self) -> io::Result<()> { Ok(()) }
    }

    #[test]
    fn output_failure_does_not_misrepresent_an_applied_receipt() {
        let error = render_completed(&report(true), true, &mut FailedOutput).unwrap_err();
        assert_eq!(error.code, 9);
        assert!(!error.retryable);
        assert!(error.message.contains("applied=true"));
        assert!(error.hint.unwrap().contains("not rolled back"));
    }
}
