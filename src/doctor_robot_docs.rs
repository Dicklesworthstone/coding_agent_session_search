//! `cass robot-docs doctor` topic content.
//!
//! Pass-1 of world-class-doctor adds a doctor-specific entry to the
//! existing `cass robot-docs` topic dispatcher. The topic is paste-ready
//! agent documentation: when, why, and how to invoke `cass doctor` from
//! a non-interactive context.
//!
//! The text is exposed via [`doctor_robot_docs_body`] which returns the
//! topic body. The dispatch site in `src/lib.rs` reads this and
//! emits it on stdout when `cass robot-docs doctor` is requested.
//!
//! Pass-2 wires `doctor_robot_docs_body()` into the `RobotTopic::Doctor`
//! dispatcher in `src/lib.rs::print_robot_docs`. The constant id/title
//! exports remain available for future capabilities-surface integration.

#![allow(dead_code)]

/// Stable topic id (kebab-case). Used as the URL-style id in robot-docs JSON.
pub(crate) const DOCTOR_ROBOT_DOCS_TOPIC_ID: &str = "doctor";

/// Title shown in the topic index.
pub(crate) const DOCTOR_ROBOT_DOCS_TITLE: &str = "cass doctor — agent handbook";

/// Returns the canonical doctor topic body. Plain text (no ANSI). Stable
/// across `doctor_contract_version` 1.x; bumped only on breaking surface change.
pub(crate) fn doctor_robot_docs_body() -> &'static str {
    DOCTOR_ROBOT_DOCS_BODY
}

const DOCTOR_ROBOT_DOCS_BODY: &str = r#"# cass doctor — agent handbook

## TL;DR

If you are an agent and `cass <something>` reported `recommended_action: ...`, run:

    cass doctor --json

This is read-only and ≤2s on a healthy system. Branch on `health_class`:

  * "ok"        — proceed with your original task
  * "degraded"  — surface the issue, optionally apply safe repairs
  * "unhealthy" — STOP; require operator review before mutating

Never run bare `cass doctor` (it has no TUI in this surface, but other cass
subcommands do — always pass `--json` or `--robot`).

## Read-only surface (safe to invoke any time)

    cass doctor --json                   # full check
    cass doctor --json --check           # explicit read-only-check mode
    cass doctor --json --verbose         # show passed checks too
    cass doctor --json --quarantine      # include quarantine inventory

Exit codes:

  0   no failed check. Findings may still be present: read `status`,
      `issues_found` and `operation_outcome.exit_code_kind` (health-failure
      means diagnosed issues), then `recommended_action`
  4   refused-unsafe — a precondition failed; inspect `checks[].name` for which
  5   doctor — failed checks remain; the full report is still on stdout and
      `operation_outcome.exit_code_kind` says health-failure or repair-failure
  7   index-busy — a repair was blocked by an active operation lock; read
      `operation_state.owners` and retry after that owner finishes

`cass doctor --emit-capabilities --json` lists every (code, kind) pair doctor
and its subcommands return.

## Plan a repair (no mutation)

    cass doctor repair --dry-run --json

Returns a `plan_fingerprint`. The fingerprint is content-addressed over the set
of planned actions; it changes if the underlying state changes between dry-run
and apply.

## Apply a repair (mutation, gated)

    cass doctor repair --yes --plan-fingerprint=<fp> --json

The mutation is applied only if the fingerprint still matches. If the state
changed under your feet, exit 4 (refused-unsafe: plan-fingerprint-mismatch);
re-plan and re-apply.

## NEW pass-1 subcommands

### `cass doctor ls --json`

Lists all `.doctor/runs/<run-id>/` entries with status. Use this to find the
run-id for a recent invocation.

    cass doctor ls --json | jq '.runs[0]'

Each run entry has `run_id`, `started_at_ms`, `ended_at_ms`, `mode`,
`exit_code`, `action_count`, `status` (one of completed|incomplete|unknown).

### `cass doctor undo <run-id> --json`

Restores the on-disk state from the per-run backups, byte-identically.
Verifies hashes at each step — refuses to proceed if any tampering is
detected.

    cass doctor undo 2026-05-09T20-07-01Z__a3f9b2 --json
    cass doctor undo latest --json

Exit codes:

  0   all mutations restored
  3   undo failed (hash mismatch on a backup); state is at the failing step
  4   refused-unsafe (e.g., post-mutation file changed out of band)

### `cass doctor diff [<ref>] --json`

Read-only. Shows the diff between (a) the current state and (b) what a
`--fix` would produce (no `<ref>`), or between two prior runs (with `<ref>`).

### `cass doctor gc --before <iso8601> --yes --json`

Quarantines all runs older than `<iso8601>` into `.doctor/quarantine/runs/`.
Never deletes. Operator owns final disk reclamation. Both `--before` and
`--yes` are required.

### `cass doctor capabilities --json`

Self-describes the doctor surface: contract version, every detector name,
every fixer name, every exit code with kebab-case kind, every env var,
every artifact path. Always present in the JSON output:

    {
      "doctor_contract_version": 1,
      "schema_version": 1,
      "binary_version": "x.y.z",
      "detectors": [...],
      "fixers": [...],
      "exit_codes": [...],
      "data_paths": [...],
      "env_vars": [...]
    }

### `cass doctor --robot-triage`

The mega-command. Returns one envelope with everything you need to make a
decision in a single call:

    {
      "summary": {...},
      "findings": [...],
      "actions_planned": [...],
      "recommended_command": "cass doctor repair --yes --plan-fingerprint=...",
      "capabilities_url": "cass doctor capabilities --json"
    }

## JSON envelope

Pass-1 introduces top-level `schema_version: 2`. Existing fields preserved.

    {
      "schema_version": 2,                    # NEW (root level)
      "doctor_contract_version": 1,           # NEW
      "run_id": "...",                        # NEW (in repair/cleanup modes)
      "started_at_ms": <i64>,
      "ended_at_ms": <i64>,                   # NEW
      "exit_code": 0,
      "exit_code_kind": "success|...",
      "checks": [...],                        # unchanged
      "operation_outcome": {...},             # unchanged
      "next_command": "...",                  # NEW (when applicable)
      "capabilities_url": "cass doctor capabilities --json",  # NEW
      "report_url": ".doctor/runs/<run-id>/report.json"        # NEW
    }

Always branch on `exit_code_kind` (kebab-case string), not on the numeric
`exit_code`. Codes ≥4 are domain-specific and may map to multiple kinds.

## What the doctor will refuse

Per AGENTS.md and the world-class-doctor safety envelope (S1-S12):

  * Delete files. Cleanup is via quarantine + `gc --before <ts> --yes`.
  * Run destructive shell. No `rm -rf`, `git reset --hard`, etc.
  * Bypass the mutate() chokepoint. New writes go through it (Phase 4+).
  * Auto-fix P0 anomalies. Operator-only.
  * Auto-update goldens. Reports the diff; operator owns regeneration.
  * Touch .env, rust-toolchain.toml, scripts/git-hooks/pre-push.sh.
  * Make network calls offline. `--online` is opt-in.
  * Repair while another doctor repair holds the mutation lock. The outcome
    is repair-blocked (exit 7 index-busy while failed checks remain).

## Common gotchas

* Two `cass doctor --fix` invocations race → the second mutates nothing,
  reports `operation_outcome.kind` repair-blocked and exits 7 (index-busy)
  while failed checks remain; rerun it after the first finishes.
* `cass doctor undo <run-id>` against a run created by an older cass version
  may fail — the actions.jsonl schema is versioned; bump implies undo
  refusal.
* `cass doctor capabilities --json` lists detectors that are compiled in,
  not detectors enabled at runtime; some detectors require `--online`.
* The data dir is `~/.local/share/coding-agent-search/` by default; override
  with `--data-dir <path>` everywhere.

## When to call which

| You want to                                              | Run                                          |
|----------------------------------------------------------|----------------------------------------------|
| Find out if cass is healthy (≤50ms)                      | cass health --json                           |
| Get the recommended action for the current state         | cass triage --json                           |
| Comprehensive checks with explanation                    | cass doctor --json --verbose                 |
| Plan a repair, then apply                                | cass doctor repair --dry-run; --yes ...      |
| Inspect history of doctor runs                           | cass doctor ls --json                        |
| Compare what a fix would change                          | cass doctor diff --json                      |
| Reclaim disk space from old runs                         | cass doctor gc --before <ts> --yes --json    |
| Undo a recent repair                                     | cass doctor undo <run-id> --json             |
| Self-describe to a fresh agent                           | cass doctor capabilities --json              |
| One-shot triage decision                                 | cass doctor --robot-triage                   |
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_is_non_empty_and_starts_with_title() {
        let body = doctor_robot_docs_body();
        assert!(body.len() > 1000);
        assert!(body.starts_with("# cass doctor"));
    }

    #[test]
    fn body_documents_new_subcommands() {
        let body = doctor_robot_docs_body();
        for needle in [
            "cass doctor ls",
            "cass doctor undo",
            "cass doctor diff",
            "cass doctor gc",
            "cass doctor capabilities",
            "cass doctor --robot-triage",
            "schema_version: 2",
            "exit_code_kind",
        ] {
            assert!(
                body.contains(needle),
                "robot-docs body missing required marker: {needle}"
            );
        }
    }

    #[test]
    fn body_warns_against_destructive_actions() {
        let body = doctor_robot_docs_body();
        for marker in [
            "Delete files",
            "rm -rf",
            "Auto-fix P0",
            "Auto-update goldens",
        ] {
            assert!(
                body.contains(marker),
                "robot-docs missing safety marker: {marker}"
            );
        }
    }

    #[test]
    fn topic_id_and_title_constants_are_stable() {
        // These are the agent contract — they MUST NOT change in a backward
        // incompatible way without bumping doctor_contract_version.
        assert_eq!(DOCTOR_ROBOT_DOCS_TOPIC_ID, "doctor");
        assert_eq!(DOCTOR_ROBOT_DOCS_TITLE, "cass doctor — agent handbook");
    }
}
