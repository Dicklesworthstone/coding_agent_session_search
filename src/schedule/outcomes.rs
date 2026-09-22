//! Scheduled-run outcomes are distinct from scheduler registration/probe status.
//!
//! In particular, `launchctl print` exiting successfully only establishes that
//! the job is registered; it says nothing about the last indexing attempt.

use super::{JobReport, ScheduleJob, ScheduleState, StepReport};

fn is_busy_index(step: &StepReport) -> bool {
    matches!(step.name.as_str(), "index" | "index-full") && step.exit_code == Some(7)
}

fn is_deferred(step: &StepReport) -> bool {
    is_busy_index(step)
        || (step.name == "schedule-admission"
            && step.result.as_ref().is_some_and(|result| {
                result.get("reason").and_then(serde_json::Value::as_str) == Some("schedule_busy")
            }))
}

/// Older receipts converted index-busy into `ok: true`. Interpret those receipts
/// truthfully when reading them, without writing to state or history during reads.
pub(super) fn normalize_state(mut state: ScheduleState) -> ScheduleState {
    for report in [&mut state.last_incremental, &mut state.last_nightly]
        .into_iter()
        .flatten()
    {
        for step in &mut report.steps {
            if is_deferred(step) {
                step.ok = false;
            }
        }
        // Never erase an independently recorded job-level failure.
        report.ok &= report.steps.iter().all(|step| step.ok);
    }
    state
}

fn run_detail(report: &JobReport) -> String {
    // Check the steps as well as `ok`, so even a legacy receipt that has not
    // passed through load_state cannot turn a lost lock race into fresh work.
    let failures: Vec<&StepReport> = report
        .steps
        .iter()
        .filter(|step| !step.ok || is_deferred(step))
        .collect();
    if !failures.is_empty() {
        let deferred = failures.iter().all(|step| is_deferred(step));
        let outcome = if deferred { "deferred" } else { "failed" };
        let steps = failures
            .iter()
            .map(|step| match step.exit_code {
                Some(code) => format!("{} (exit {code})", step.name),
                None => format!("{} (no exit status)", step.name),
            })
            .collect::<Vec<_>>()
            .join(", ");
        return format!("last run {outcome}: {steps}");
    }
    if !report.ok {
        return "last run failed".to_string();
    }
    if let Some(reason) = &report.skipped_reason {
        return format!("last run skipped: {reason}");
    }
    "last run completed".to_string()
}

/// Prefer the persisted run outcome; keep scheduler-probe diagnostics explicitly
/// labeled so a successful probe is never displayed as successful indexing.
pub(super) fn unit_detail(
    job: ScheduleJob,
    state: &ScheduleState,
    scheduler_detail: Option<String>,
) -> Option<String> {
    let report = match job {
        ScheduleJob::Incremental => state.last_incremental.as_ref(),
        ScheduleJob::Nightly => state.last_nightly.as_ref(),
    };
    match (report, scheduler_detail) {
        (Some(report), Some(probe)) => {
            Some(format!("{}; scheduler probe: {probe}", run_detail(report)))
        }
        (Some(report), None) => Some(run_detail(report)),
        (None, Some(probe)) => Some(format!("no recorded run; scheduler probe: {probe}")),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests;
