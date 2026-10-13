//! CLI projection of the shared approved-cleanup API. No second policy or
//! deletion implementation; default inventory keeps its existing JSON schema.

use std::io::{self, Write};
use std::path::Path;

use coding_agent_search::search::lexical_quarantine::cleanup::{self as core, CleanupError, CleanupReport};
use coding_agent_search::{CliError, CliResult};

pub(super) fn parse_approval(value: &str) -> Result<String, String> {
    if core::valid_approval(value) {
        Ok(value.to_owned())
    } else {
        Err("use the complete lq-cleanup-v1 fingerprint returned by --plan".into())
    }
}

fn error(error: CleanupError) -> CliError {
    let (code, kind, retryable) = match &error {
        CleanupError::InvalidApproval => (2, "quarantine-invalid-approval", false),
        CleanupError::Disabled => (2, "quarantine-cleanup-disabled", false),
        CleanupError::ApprovalChanged => (6, "quarantine-approval-changed", true),
        CleanupError::Busy => (7, "quarantine-cleanup-busy", true),
        CleanupError::Incomplete => (8, "quarantine-incomplete", true),
        CleanupError::Io(_) => return match error {
            CleanupError::Io(error) => super::failure(error),
            _ => unreachable!(),
        },
    };
    CliError {
        code, kind, message: error.to_string(), retryable,
        hint: Some("Run lexical-quarantine --plan again for the same explicit --data-dir; never reuse approval after its inventory changes".into()),
    }
}

fn json(value: &impl serde::Serialize, output: &mut impl Write) -> io::Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    bytes.push(b'\n');
    output.write_all(&bytes)?;
    output.flush()
}

pub(super) fn plan(data_dir: &Path, as_json: bool, output: &mut impl Write) -> CliResult<()> {
    let plan = core::plan(data_dir).map_err(super::failure)?;
    if as_json {
        json(&plan, output).map_err(super::failure)?;
    } else {
        super::render(&plan.inventory, false, output).map_err(super::failure)?;
        match &plan.approval_fingerprint {
            Some(approval) => writeln!(output, "Approval fingerprint: {approval}\nUse --apply with this exact fingerprint only after reviewing the eligible bundles."),
            None => writeln!(output, "No cleanup approval: retention is disabled or the inventory is incomplete."),
        }.map_err(super::failure)?;
        output.flush().map_err(super::failure)?;
    }
    if !plan.inventory.complete {
        return Err(CliError::already_reported(8, "quarantine-incomplete", true));
    }
    Ok(())
}

fn render(report: &CleanupReport, as_json: bool, output: &mut impl Write) -> io::Result<()> {
    if as_json { return json(report, output); }
    writeln!(output, "Lexical quarantine cleanup complete: {}", report.complete)?;
    writeln!(output, "Selected: {}; attempted: {}; fully removed: {}; logical bytes removed: {}; protected bundles retained: {}",
        report.selected_bundles, report.attempted_bundles, report.removed_bundles,
        report.removed_logical_bytes, report.retained_bundles)?;
    for error in &report.errors { writeln!(output, "Cleanup stopped: {error}")?; }
    if !report.complete {
        writeln!(output, "Earlier mutations were not rolled back. Inspect the remaining inventory and obtain a new plan before retrying.")?;
    }
    output.flush()
}

pub(super) fn apply(data_dir: &Path, approval: &str, as_json: bool, output: &mut impl Write) -> CliResult<()> {
    let report = core::apply(data_dir, approval).map_err(error)?;
    // Execution has already completed or stopped. Even a broken output sink
    // cannot imply that cleanup did not happen or that retry is idempotent.
    render(&report, as_json, output).map_err(|failure| CliError {
        code: 9,
        kind: "quarantine-cleanup-output-failed",
        message: format!("cleanup reached {} attempted and {} fully removed bundles, but its report could not be written: {failure}",
            report.attempted_bundles, report.removed_bundles),
        hint: Some("Inspect lexical-quarantine before retrying; cleanup may already have changed files".into()),
        retryable: false,
    })?;
    if !report.complete {
        return Err(CliError::already_reported(8, "quarantine-cleanup-incomplete", true));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parser_requires_explicit_fingerprint_and_separates_plan_from_apply() {
        let approval = format!("lq-cleanup-v1-{}", "a".repeat(64));
        let base = ["cass", "lexical-quarantine", "--data-dir", "."];
        for args in [
            vec!["--apply"], vec!["--apply", "yes"], vec!["--plan", "--apply", &approval],
        ] {
            assert!(super::super::Cli::try_parse_from(base.iter().copied().chain(args)).is_err());
        }
        assert!(super::super::Cli::try_parse_from(base.iter().copied().chain(["--plan", "--robot"])).is_ok());
        let parsed = super::super::Cli::try_parse_from(base.iter().copied().chain(["--apply", approval.as_str()])).unwrap();
        let super::super::Command::LexicalQuarantine { apply, plan, .. } = parsed.command;
        assert_eq!(apply.as_deref(), Some(approval.as_str()));
        assert!(!plan);
    }

    #[test]
    fn refusals_have_distinct_machine_readable_codes() {
        for (cause, code, kind) in [
            (CleanupError::InvalidApproval, 2, "quarantine-invalid-approval"),
            (CleanupError::Disabled, 2, "quarantine-cleanup-disabled"),
            (CleanupError::ApprovalChanged, 6, "quarantine-approval-changed"),
            (CleanupError::Busy, 7, "quarantine-cleanup-busy"),
            (CleanupError::Incomplete, 8, "quarantine-incomplete"),
        ] {
            let failure = error(cause);
            assert_eq!(failure.code, code);
            assert_eq!(failure.kind, kind);
        }
    }

    #[test]
    fn partial_cleanup_report_does_not_claim_complete_or_read_only() {
        let report = CleanupReport {
            schema_version: 1, data_dir: ".".into(), approval_fingerprint: "approved fixture".into(),
            complete: false, selected_bundles: 3, retained_bundles: 1,
            attempted_bundles: 2, removed_bundles: 1, removed_logical_bytes: 25,
            errors: vec!["later target changed".into()],
        };
        let mut encoded = Vec::new();
        render(&report, true, &mut encoded).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(value["complete"], false);
        assert_eq!(value["attempted_bundles"], 2);
        assert_eq!(value["removed_bundles"], 1);
        let mut human = Vec::new();
        render(&report, false, &mut human).unwrap();
        assert!(String::from_utf8(human).unwrap().contains("not rolled back"));
    }
}
