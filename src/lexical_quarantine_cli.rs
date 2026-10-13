//! Read-only rebuild evidence inventory, including when the archive cannot open.

use std::io::{self, Write};
use std::path::PathBuf;

use clap::{CommandFactory, Parser, Subcommand};
use coding_agent_search::search::lexical_quarantine::{self, Inventory};
use coding_agent_search::{CliError, CliResult};

#[derive(Parser)]
#[command(name = "cass", disable_version_flag = true)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Inspect retained failed lexical rebuilds without opening the database.
    /// No files are deleted. Normal index startup applies conservative retention.
    LexicalQuarantine {
        /// Explicit archive directory; never defaults to another user's archive.
        #[arg(long, value_name = "PATH")]
        data_dir: PathBuf,
        /// Emit one complete inventory; complete=false exits with code 8.
        #[arg(long, alias = "robot")]
        json: bool,
    },
}

pub fn clap_commands() -> Vec<clap::Command> {
    Cli::command().find_subcommand("lexical-quarantine").cloned().into_iter().collect()
}

fn failure(error: io::Error) -> CliError {
    let code = match error.kind() {
        io::ErrorKind::NotFound => 3,
        io::ErrorKind::InvalidInput => 2,
        _ => 9,
    };
    CliError {
        code, kind: "lexical-quarantine", message: error.to_string(),
        hint: Some("Use an existing, non-symlink --data-dir; this command never opens or repairs the archive".into()),
        retryable: false,
    }
}

fn render(inventory: &Inventory, json: bool, output: &mut impl Write) -> io::Result<()> {
    if json {
        // Serialize before publication: unrepresentable paths cannot leave half
        // a JSON document on stdout. The bounded inventory limits this allocation.
        let mut bytes = serde_json::to_vec_pretty(inventory).map_err(io::Error::other)?;
        bytes.push(b'\n');
        output.write_all(&bytes)?;
    } else {
        writeln!(output, "Lexical rebuild quarantines: {} (complete: {})",
            inventory.artifacts.len(), inventory.complete)?;
        writeln!(output, "Logical bytes retained: {}; eligible under index-run lock: {}",
            inventory.total_bytes, inventory.eligible_bytes)?;
        for artifact in &inventory.artifacts {
            writeln!(output, "{}\t{} bytes\t{:?}\t{}",
                artifact.path.display(), artifact.size_bytes, artifact.disposition, artifact.reason)?;
        }
        for warning in &inventory.warnings { writeln!(output, "Warning: {warning}")?; }
        writeln!(output, "Read-only inspection. The newest bundle and evidence newer than six hours are retained.")?;
    }
    output.flush()
}

pub fn run(args: Vec<String>) -> CliResult<()> {
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(error) => {
            let code = error.exit_code();
            error.print().map_err(failure)?;
            return if code == 0 { Ok(()) } else {
                Err(CliError::already_reported(code, "argument_parsing", false))
            };
        }
    };
    let Command::LexicalQuarantine { data_dir, json } = cli.command;
    let inventory = lexical_quarantine::inspect(&data_dir).map_err(failure)?;
    render(&inventory, json, &mut io::stdout().lock()).map_err(failure)?;
    if !inventory.complete {
        return Err(CliError::already_reported(8, "quarantine-incomplete", true));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inspection_requires_explicit_scope_and_has_no_mutating_flags() {
        assert!(Cli::try_parse_from(["cass", "lexical-quarantine"]).is_err());
        assert!(Cli::try_parse_from(["cass", "lexical-quarantine", "--data-dir", ".", "--apply"]).is_err());
        assert!(Cli::try_parse_from(["cass", "lexical-quarantine", "--data-dir", ".", "--robot"]).is_ok());
        let command = Cli::command();
        command.debug_assert();
        assert_eq!(clap_commands()[0].get_name(), "lexical-quarantine");
    }
}
