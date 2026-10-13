//! Shared offline recovery command for `cass semantic-reclaim` and the legacy
//! `cass-semantic-reclaim` companion. No database, model, runtime, or default
//! archive is initialized. Both forms retain the same approval and JSON contract.
#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Command, CommandFactory, FromArgMatches, Parser, error::ErrorKind};
use coding_agent_search::indexer::semantic::{
    BackfillArtifactReclaimPlan, BackfillArtifactReclaimReport, apply_backfill_artifact_plan,
    plan_backfill_artifacts,
};
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(
    name = "cass-semantic-reclaim",
    version,
    about = "Preview or reclaim unreferenced semantic backfill scratch",
    after_help = "Preview is the default. Apply requires the exact preview fingerprint.\nOnly backfill staging/WAL files and reuse directories are eligible; live\nmanifest references, generations, shards and quarantine evidence are retained.\nNo database or model is opened. Stop active index/backfill writers first."
)]
struct Args {
    /// Existing CASS data directory; no default archive is guessed.
    #[arg(long, value_name = "DIRECTORY")]
    data_dir: PathBuf,
    /// Explicitly preview without deleting artifacts (also the default).
    #[arg(long, conflicts_with = "apply")]
    dry_run: bool,
    /// Delete only the candidates in a still-current approved preview.
    #[arg(long, requires = "plan_fingerprint", conflicts_with = "dry_run")]
    apply: bool,
    /// Exact plan_fingerprint from a prior preview of this archive.
    #[arg(long, requires = "apply", value_name = "FINGERPRINT", value_parser = fingerprint)]
    plan_fingerprint: Option<String>,
    /// Emit one JSON result on stdout and structured failures on stderr.
    #[arg(long, visible_alias = "robot")]
    json: bool,
}

fn fingerprint(value: &str) -> std::result::Result<String, String> {
    if value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(value.to_owned())
    } else {
        Err("expected the exact 64-character lowercase plan fingerprint".into())
    }
}

#[derive(Serialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
enum Outcome {
    Preview {
        schema_version: u32,
        status: &'static str,
        plan: BackfillArtifactReclaimPlan,
    },
    Apply {
        schema_version: u32,
        status: &'static str,
        report: BackfillArtifactReclaimReport,
    },
}

impl Outcome {
    fn code(&self) -> u8 {
        match self {
            Self::Preview { plan, .. } if plan.checkpoint_missing => 3,
            Self::Apply { report, .. }
                if report.checkpoint_missing || !report.failed_paths.is_empty() =>
            {
                3
            }
            _ => 0,
        }
    }

    fn write(&self, json: bool, output: &mut impl Write) -> Result<()> {
        if json {
            serde_json::to_writer(&mut *output, self)?;
            writeln!(output)?;
        } else {
            match self {
                Self::Preview { plan, .. } => {
                    writeln!(output, "Archive: {}", plan.data_dir.display())?;
                    if plan.checkpoint_missing {
                        writeln!(
                            output,
                            "BLOCKED: resumable checkpoint is missing; all fallback artifacts retained."
                        )?;
                    } else {
                        writeln!(
                            output,
                            "Reclaimable: {} logical bytes in {} files and {} reuse directories",
                            plan.reclaimable_bytes,
                            plan.reclaimable_files,
                            plan.reclaimable_directories
                        )?;
                        for candidate in &plan.candidates {
                            writeln!(
                                output,
                                "  {} bytes  {}",
                                candidate.size_bytes,
                                candidate.path.display()
                            )?;
                        }
                        writeln!(output, "Plan fingerprint: {}", plan.plan_fingerprint)?;
                        writeln!(
                            output,
                            "No artifacts removed. To apply, repeat with the same --data-dir and:"
                        )?;
                        writeln!(
                            output,
                            "  --apply --plan-fingerprint {}",
                            plan.plan_fingerprint
                        )?;
                    }
                }
                Self::Apply { report, .. } => {
                    writeln!(
                        output,
                        "Reclaimed: {} logical bytes in {} files and {} reuse directories",
                        report.reclaimed_bytes, report.removed_files, report.removed_directories
                    )?;
                    for path in &report.failed_paths {
                        writeln!(output, "  Could not completely remove: {}", path.display())?;
                    }
                }
            }
        }
        output.flush()?;
        Ok(())
    }
}

fn run(args: &Args) -> Result<Outcome> {
    match (args.apply, args.dry_run) {
        (true, true) => bail!("apply and dry-run are mutually exclusive"),
        (true, false) => {
            let expected = args
                .plan_fingerprint
                .as_deref()
                .context("apply requires a plan fingerprint")?;
            let report = apply_backfill_artifact_plan(&args.data_dir, expected)?;
            let status = if report.failed_paths.is_empty() && !report.checkpoint_missing {
                "complete"
            } else {
                "partial"
            };
            Ok(Outcome::Apply {
                schema_version: 1,
                status,
                report,
            })
        }
        (false, _) => {
            let plan = plan_backfill_artifacts(&args.data_dir)?;
            let status = if plan.checkpoint_missing {
                "blocked"
            } else {
                "ready"
            };
            Ok(Outcome::Preview {
                schema_version: 1,
                status,
                plan,
            })
        }
    }
}

fn report_error(json: bool, kind: &str, message: &str) {
    if json {
        eprintln!(
            "{}",
            serde_json::json!({"error": {"kind": kind, "message": message}})
        );
    } else {
        eprintln!("{message}");
    }
}

/// The exact tree used by execution also feeds CASS help, completions,
/// capabilities, and introspection through its external-command registry.
pub fn clap_commands() -> Vec<Command> {
    vec![Args::command().name("semantic-reclaim")]
}

#[derive(Clone, Copy)]
enum Invocation {
    Standalone,
    Cass,
}

impl Invocation {
    fn command(self) -> Command {
        match self {
            Self::Standalone => Args::command(),
            Self::Cass => Command::new("cass")
                .disable_version_flag(true)
                .disable_help_subcommand(true)
                .subcommand_required(true)
                .subcommands(clap_commands()),
        }
    }

    fn parse(self, raw: Vec<OsString>) -> std::result::Result<Args, clap::Error> {
        let raw = match self {
            Self::Cass => cass_args(&raw).unwrap_or(raw),
            Self::Standalone => raw,
        };
        let matches = self.command().try_get_matches_from(raw)?;
        let arguments = match self {
            Self::Standalone => &matches,
            Self::Cass => matches.subcommand_matches("semantic-reclaim").ok_or_else(|| {
                clap::Error::raw(ErrorKind::MissingSubcommand, "expected semantic-reclaim")
            })?,
        };
        Args::from_arg_matches(arguments)
    }
}

/// Recognize only the explicit recovery command, not a search term or an option
/// value. Permit its own options before the command as well as after it. Move
/// only the command token: Clap still validates EVERY supplied argument, so
/// this dispatch cannot drop unknown flags, conflicts, or duplicate approvals.
/// Keep OS strings intact until the recovery parser handles paths.
pub fn cass_args(raw: &[OsString]) -> Option<Vec<OsString>> {
    let mut index = 1;
    while let Some(argument) = raw.get(index) {
        match argument.to_str() {
            Some("--json" | "--robot" | "--dry-run" | "--apply") => index += 1,
            Some("--data-dir" | "--plan-fingerprint") => {
                raw.get(index + 1)?;
                index += 2;
            }
            Some(value)
                if value.starts_with("--data-dir=")
                    || value.starts_with("--plan-fingerprint=") =>
            {
                index += 1;
            }
            Some("semantic-reclaim") => {
                let mut result = raw.to_vec();
                let command = result.remove(index);
                result.insert(1, command);
                return Some(result);
            }
            _ => return None,
        }
    }
    None
}

/// Execute the standalone spelling, returning its actual process exit code.
pub fn run_standalone(raw: Vec<OsString>) -> u8 {
    run_cli(raw, Invocation::Standalone)
}

/// Execute `cass semantic-reclaim` before ordinary CASS startup. All leases and
/// output streams have left their scopes before this function returns.
pub fn run_cass(raw: Vec<OsString>) -> u8 {
    run_cli(raw, Invocation::Cass)
}

fn run_cli(raw: Vec<OsString>, invocation: Invocation) -> u8 {
    let json = raw
        .iter()
        .any(|value| matches!(value.to_str(), Some("--json" | "--robot")));
    let args = match invocation.parse(raw) {
        Ok(args) => args,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            return if error.print().is_ok() { 0 } else { 1 };
        }
        Err(error) => {
            report_error(json, "usage", &error.to_string());
            return 2;
        }
    };
    match run(&args) {
        Ok(outcome) => {
            // A failed output write must not falsely claim that apply did not
            // run: deletion may already have completed before a broken pipe.
            if let Err(error) = outcome.write(args.json, &mut io::stdout().lock()) {
                report_error(
                    args.json,
                    "output_failed",
                    &format!("operation finished, but result output failed: {error:#}"),
                );
                return 1;
            }
            outcome.code()
        }
        Err(error) => {
            report_error(args.json, "recovery_failed", &format!("{error:#}"));
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_requires_explicit_matching_approval_shape() {
        let base = ["cass-semantic-reclaim", "--data-dir", "/unused"];
        let valid = "a".repeat(64);
        for suffix in [
            vec!["--apply"],
            vec!["--plan-fingerprint", &valid],
            vec!["--apply", "--plan-fingerprint", "bad"],
            vec!["--dry-run", "--apply", "--plan-fingerprint", &valid],
            vec!["--force"],
        ] {
            assert!(Args::try_parse_from(base.iter().copied().chain(suffix)).is_err());
        }
        let args = Args::try_parse_from(base.iter().copied().chain([
            "--apply",
            "--plan-fingerprint",
            valid.as_str(),
        ]))
        .unwrap();
        assert!(args.apply);
        assert!(!Args::try_parse_from(base).unwrap().apply);
    }

    #[test]
    fn partial_cleanup_and_missing_checkpoint_are_not_success_exit_codes() {
        let partial = Outcome::Apply {
            schema_version: 1,
            status: "partial",
            report: BackfillArtifactReclaimReport {
                failed_paths: vec![PathBuf::from("retained")],
                ..Default::default()
            },
        };
        assert_eq!(partial.code(), 3);
        let mut bytes = Vec::new();
        partial.write(true, &mut bytes).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["status"], "partial");
        assert_eq!(value["report"]["failed_paths"][0], "retained");
        let blocked = Outcome::Preview {
            schema_version: 1,
            status: "blocked",
            plan: BackfillArtifactReclaimPlan {
                schema_version: 1,
                data_dir: PathBuf::from("archive"),
                candidates: Vec::new(),
                reclaimable_files: 0,
                reclaimable_directories: 0,
                reclaimable_bytes: 0,
                checkpoint_missing: true,
                plan_fingerprint: "a".repeat(64),
            },
        };
        assert_eq!(blocked.code(), 3);
        let mut bytes = Vec::new();
        blocked.write(true, &mut bytes).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["status"], "blocked");
        assert_eq!(value["plan"]["checkpoint_missing"], true);
    }
    fn raw(values: &[&str]) -> Vec<OsString> {
        values.iter().map(|value| OsString::from(*value)).collect()
    }

    #[test]
    fn cass_parser_keeps_the_standalone_approval_contract() {
        Invocation::Cass.command().debug_assert();
        Invocation::Standalone.command().debug_assert();
        let valid = "b".repeat(64);
        for suffix in [
            vec![],
            vec!["--apply"],
            vec!["--data-dir", "/unused", "--apply"],
            vec!["--data-dir", "/unused", "--plan-fingerprint", &valid],
            vec!["--data-dir", "/unused", "--apply", "--plan-fingerprint", "bad"],
            vec!["--data-dir", "/unused", "--apply", "--dry-run", "--plan-fingerprint", &valid],
            vec!["--data-dir", "/unused", "--force"],
            vec!["--data-dir", "/unused", "--data-dir", "/other"],
        ] {
            for (invocation, prefix) in [
                (Invocation::Cass, vec!["cass", "semantic-reclaim"]),
                (Invocation::Standalone, vec!["cass-semantic-reclaim"]),
            ] {
                let args = prefix.into_iter().chain(suffix.iter().copied())
                    .map(OsString::from).collect();
                assert!(invocation.parse(args).is_err());
            }
        }
        let args = Invocation::Cass.parse(raw(&[
            "cass", "semantic-reclaim", "--data-dir", "/unused",
            "--apply", "--plan-fingerprint", &valid, "--robot",
        ])).unwrap();
        assert!(args.apply && args.json);
        assert_eq!(args.plan_fingerprint.as_deref(), Some(valid.as_str()));
    }

    #[test]
    fn dispatch_never_treats_an_argument_value_as_a_recovery_command() {
        for args in [
            vec!["cass", "search", "semantic-reclaim", "--json"],
            vec!["cass", "--data-dir", "semantic-reclaim", "storage"],
            vec!["cass", "--data-dir=semantic-reclaim", "storage"],
            vec!["cass", "--plan-fingerprint", "semantic-reclaim"],
            vec!["cass", "--help"],
            vec!["cass", "--json"],
        ] {
            assert!(cass_args(&raw(&args)).is_none());
        }
        let original = raw(&[
            "cass", "--data-dir", "/archive with spaces", "--json",
            "semantic-reclaim", "--dry-run", "--force",
        ]);
        let normalized = cass_args(&original).unwrap();
        assert_eq!(normalized, raw(&[
            "cass", "semantic-reclaim", "--data-dir", "/archive with spaces",
            "--json", "--dry-run", "--force",
        ]));
        // Routing must not discard the unknown flag to manufacture approval.
        assert!(Invocation::Cass.parse(normalized).is_err());
        let args = Invocation::Cass.parse(raw(&[
            "cass", "--data-dir=/explicit", "--robot", "semantic-reclaim", "--dry-run",
        ])).unwrap();
        assert_eq!(args.data_dir, PathBuf::from("/explicit"));
        assert!(args.json && args.dry_run && !args.apply);
    }

}
