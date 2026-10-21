//! Native doctor entry for the existing offline rebuild-quarantine capability.
//! Clone the execution parser, not its options or its deletion policy. Only the
//! explicit two-word surface is intercepted; ordinary doctor stays unchanged.

use std::path::PathBuf;

use coding_agent_search::{CliError, CliResult};

/// Deliberately recognize command words, not occurrences in paths, search terms
/// or options. The documented spelling puts `doctor quarantine` before options.
/// Clap still validates every token, including unknown/conflicting apply flags.
pub fn is_invocation(args: &[String]) -> bool {
    args.get(1).map(String::as_str) == Some("doctor")
        && args.get(2).map(String::as_str) == Some("quarantine")
}

fn command() -> clap::Command {
    let quarantine = super::clap_commands()
        .into_iter()
        .next()
        .expect("lexical quarantine command is registered")
        .name("quarantine")
        .about("Inspect or explicitly reclaim failed lexical rebuild evidence")
        .after_help(
            "Read-only by default. --plan previews; --apply requires its exact fingerprint.\n\
             This surface does not open the archive, repair indexes or run doctor --fix.\n\
             Use: cass doctor quarantine --data-dir PATH [--json] [--plan | --apply FINGERPRINT]",
        );
    clap::Command::new("cass")
        .disable_version_flag(true)
        .disable_help_subcommand(true)
        .subcommand_required(true)
        .subcommand(
            clap::Command::new("doctor")
                .disable_help_subcommand(true)
                .subcommand_required(true)
                .subcommand(quarantine),
        )
}

pub(super) fn run(args: Vec<String>) -> CliResult<()> {
    let mut matches = match command().try_get_matches_from(args) {
        Ok(matches) => matches,
        Err(error) => {
            let code = error.exit_code();
            error.print().map_err(super::failure)?;
            return if code == 0 {
                Ok(())
            } else {
                Err(CliError::already_reported(code, "argument_parsing", false))
            };
        }
    };
    // Required subcommands and the required data_dir were checked by the same
    // Clap tree that defines the standalone command. No tokens are discarded.
    let (_, mut doctor) = matches.remove_subcommand().expect("required doctor command");
    let (_, mut options) = doctor.remove_subcommand().expect("required quarantine command");
    let data_dir = options
        .remove_one::<PathBuf>("data_dir")
        .expect("required quarantine data directory");
    let json = options.get_flag("json");
    let plan = options.get_flag("plan");
    let apply = options.remove_one::<String>("apply");
    super::execute(data_dir, json, plan, apply)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }

    #[test]
    fn dispatch_only_claims_the_explicit_doctor_quarantine_surface() {
        for args in [
            &["cass", "doctor", "quarantine"][..],
            &["cass", "doctor", "quarantine", "--help"][..],
            &["cass", "doctor", "quarantine", "--unknown"][..],
        ] {
            assert!(is_invocation(&argv(args)));
        }
        for args in [
            &["cass", "doctor"][..],
            &["cass", "doctor", "check"][..],
            &["cass", "doctor", "cleanup", "--yes"][..],
            &["cass", "doctor", "--data-dir", "quarantine"][..],
            &["cass", "search", "doctor", "quarantine"][..],
            &["cass", "--db", "doctor", "quarantine"][..],
        ] {
            assert!(!is_invocation(&argv(args)), "{args:?}");
        }
    }

    #[test]
    fn parser_reuses_scope_approval_and_conflict_rules_without_overrides() {
        command().debug_assert();
        let approval = format!("lq-cleanup-v1-{}", "a".repeat(64));
        let base = ["cass", "doctor", "quarantine", "--data-dir", "."];
        for args in [
            vec!["--apply"],
            vec!["--apply", "yes"],
            vec!["--plan", "--apply", approval.as_str()],
            vec!["--apply", approval.as_str(), "--apply", approval.as_str()],
            vec!["--yes"],
            vec!["--fix"],
            vec!["--force-rebuild"],
            vec!["--db", "somewhere-else"],
            vec!["--data-dir", "elsewhere"],
        ] {
            assert!(command().try_get_matches_from(base.iter().copied().chain(args)).is_err());
        }
        assert!(command().try_get_matches_from(["cass", "doctor", "quarantine"]).is_err());
        for args in [vec!["--plan", "--robot"], vec!["--apply", approval.as_str(), "--json"]] {
            assert!(command().try_get_matches_from(base.iter().copied().chain(args)).is_ok());
        }
    }

    #[test]
    fn nested_help_names_the_actual_command_and_shared_approval_contract() {
        let help = command()
            .try_get_matches_from(["cass", "doctor", "quarantine", "--help"])
            .unwrap_err();
        assert_eq!(help.exit_code(), 0);
        let text = help.to_string();
        assert!(text.contains("cass doctor quarantine"));
        assert!(text.contains("--data-dir"));
        assert!(text.contains("--plan"));
        assert!(text.contains("--apply"));
        assert!(text.contains("fingerprint"));
    }
}
