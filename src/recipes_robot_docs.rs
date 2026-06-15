//! `cass robot-docs recipes` topic content.
//!
//! Bead `coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.11.3`
//! (epic 11 — "Real-binary proof gates, regression corpus, and canonical
//! workflow docs"). The canonical, machine-first recovery + workflow recipes
//! distilled from the 2026-06-08 fleet session-history analysis
//! (`COMPREHENSIVE_ISSUES_AND_PROBLEMS_WITH_CASS_BASED_ON_COMPLETE_SESSION_HISTORY_ANALYSIS.md`,
//! "Skill Updates Recommended For `$cass`").
//!
//! Every command in this topic is read-only or backup-gated. The topic NEVER
//! recommends a destructive repair (no `rm -rf`, no `git reset`, no
//! hand-deletion of a data/index directory, no force rebuild) and NEVER tells
//! an agent to run bare `cass`/`bv` (both launch an interactive TUI). Those
//! invariants are pinned by `tests/spec_robot_docs_topics.rs`
//! (`recipes_topic_is_safe_and_recommends_only_bounded_json_commands`) and the
//! golden in `tests/golden/robot_docs/recipes.txt.golden`.
//!
//! The text is exposed via [`recipes_robot_docs_body`]; the dispatch site in
//! `src/lib.rs::print_robot_docs` reads it and emits it on stdout when
//! `cass robot-docs recipes` is requested.

#![allow(dead_code)]

/// Stable topic id (kebab-case). Used as the URL-style id in robot-docs JSON.
pub(crate) const RECIPES_ROBOT_DOCS_TOPIC_ID: &str = "recipes";

/// Title shown in the topic index.
pub(crate) const RECIPES_ROBOT_DOCS_TITLE: &str =
    "cass recovery & workflow recipes — agent handbook";

/// Returns the canonical recipes topic body. Plain text (no ANSI). Stable
/// across the resilience-contract surface; regenerate the golden on change.
pub(crate) fn recipes_robot_docs_body() -> &'static str {
    RECIPES_ROBOT_DOCS_BODY
}

const RECIPES_ROBOT_DOCS_BODY: &str = r#"# cass recovery & workflow recipes — agent handbook

Canonical, machine-first recipes distilled from the 2026-06-08 fleet
session-history analysis. Every command here is read-only or backup-gated.

## Global safety rules (apply to every recipe)

  * NEVER run bare `cass` or bare `bv` in automation — both launch an
    interactive TUI that blocks the session. Always pass `--json` / `--robot`
    and a subcommand.
  * cass QUARANTINES; it never silently deletes. NEVER hand-remove a data
    directory, index directory, lock file, or WAL/SHM sidecar. There is no safe
    "wipe and re-index" recipe.
  * Prefer the tool's own guidance: follow `next_command`,
    `recommended_action`, and `recommended_commands[]` from `cass triage --json`
    / `cass status --json` instead of hand-rolled repair rituals.
  * Keep every probe bounded. `cass status --json` / `cass health --json` /
    `cass doctor --check --json` are read-only and finish within a few seconds;
    treat a stall as a finding, not a reason to wait indefinitely.

## 1. Stale vs missing vs archive-risk — decision tree

These are THREE different states with three different responses. Read
`cass status --json` (or `cass triage --json`) and branch:

  * STALE  — `index.exists == true` AND `index.stale == true`.
             The archive is still searchable; results may lag a recent write.
             ACTION: continue with lexical results; a background refresh catches
             up. Do not rebuild.
  * MISSING — `index.exists == false`, or search returns `missing-index`
             (exit 3, "not initialized"). The lexical metadata is absent, not
             corrupt.
             ACTION: run `cass index --full` once to initialize the archive.
  * ARCHIVE RISK — `coverage_risk.status` is elevated / `archive_risk: high` /
             sole-copy rows present.
             ACTION: BACK UP the data dir first. Do NOT repair, rebuild, or
             prune until a backup exists or the user explicitly approves.
             `cass triage --json` will say "back up this data dir first".

Corrupt is distinct from missing: a present-but-broken archive reports a storage
error (exit 5), NOT `missing-index` (exit 3). Never treat a corrupt archive as a
fresh install — that would discard recoverable rows. Run `cass doctor --check
--json` and follow its backup-first plan.

## 2. Bounded fleet preflight (per host)

For each configured host, run a bounded, read-only preflight and record gaps —
never silently omit a host that timed out or was unreachable:

    cass --version
    cass status --json          # bounded; readiness, index freshness, db stats
    cass doctor --check --json   # bounded read-only truth surface, when available

  * Record an unreachable / timed-out host as an explicit access gap (do not
    drop it from the report).
  * A host that returns `missing-index` needs `cass index --full`; a host with
    `archive_risk: high` needs backup-first handling (recipe 1).
  * Remote source diagnostics are local-only: `remote_source_sync` is computed
    from cass-owned evidence and never opens SSH while classifying. Run
    `cass sources sync --all --json` only after reviewing the reported gaps.

## 3. Raw-log fallback (bounded; avoid huge JSONL lines)

When a cass surface is unavailable and you must read provider logs directly:

  * Use exact strings and counts first (`rg -c <pattern> <dir>`); reach for
    `rg -l <pattern>` for path discovery.
  * Do NOT dump raw JSONL lines into context — they can be megabytes each. Use
    `cass view <session> -n <line> --json` for a bounded, structured slice.
  * If `cass view` is slow or noisy, record that as a cass problem and fall back
    to direct file reads only for files that are present on disk.

## 4. Quarantine decision tree

`cass status --json` (`ingest_quarantine`) and `cass diag --json --quarantine`
enumerate quarantined work. The `safe_to_gc` flag is advisory only — no path
auto-deletes. Branch on the quarantine reason:

  * Current irreducible OOM quarantine — the host ran out of memory mid-ingest;
    re-run after relieving memory pressure; do not force a full rebuild.
  * Legacy entry missing `cass_version_at_quarantine` — eligible for a one-time
    retry under a newer cass (see issue #258).
  * Same-version retry suppressed — already retried under this version; wait for
    a version bump rather than looping.
  * Version-bump retry eligible — a newer cass may parse it; retry is allowed.
  * Source path missing — the original file moved/vanished; the archived row may
    be the only copy (see recipe 6).

## 5. Issue-class recipes

  * #110 — chunk FTS rebuilds; never wrap one giant FTS transaction. If a
    rebuild stalls, read `cass status --json` `rebuild.phase`; do not force one
    monolithic transaction.
  * #120 — remote sync must prefer system OpenSSH / rsync / scp over the `ssh2`
    SFTP fallback. Verify with `cass sources doctor --json`; a transport problem
    is an SSH/rsync issue, not a cass-archive loss.
  * #137 / #196 — `current: 0` with no forward progress means STALL DIAGNOSIS,
    not blind waiting. Inspect `cass status --json`
    `rebuild_progress.{processed_conversations,phase,stalled}`; if `stalled` is
    true, follow the reported next probe instead of waiting.
  * #247 — historical salvage should not rescan whole backup bundles for minutes
    when every batch imports zero new conversations. If `cass index --watch`
    spends minutes per batch with zero new imports, capture
    `cass status --json` `rebuild` and `cass doctor --check --json`; report it
    rather than re-triggering salvage.
  * #248 — watch-mode restart after OOM must not repeatedly pick a full deferred
    rebuild without a checkpointed recovery path. If you see repeated
    `deferred_authoritative_db_rebuild`, relieve memory pressure and let the
    checkpoint resume; do not delete state.
  * #250 — watch-mode exit code 9 must carry a parseable reason and next action.
    On a watch exit, read the structured error envelope (`err.kind`, `err.hint`)
    for the failing subsystem and next command; a bare destructor warning is a
    cass bug to report, not a reason to wipe the archive.
  * #257 — for semantic backfill, follow `CASS_SEMANTIC_PROGRESS_JSONL`,
    `last_message_id`, and the quality-tier status in `cass status --json`
    `semantic` instead of guessing progress.
  * #258 — legacy quarantine retry eligibility and stalled-rebuild reporting:
    use `cass status --json` `ingest_quarantine` + `rebuild` to decide retry vs
    wait (see recipe 4).

## 6. Do not overtrust source_path

  * An indexed `source_path` can point to a file that moved or vanished; the
    archived rows are still useful for search.
  * Treat `cass view` as suspect when it cannot resolve an archive-only path —
    that is a known limitation, not proof the conversation is lost.
  * Prefer searching the archive (`cass search ... --robot`) over chasing a
    stale `source_path` on disk.

## 7. Dependency attribution (do not blame cass for a dependency)

Robot/JSON output must stay parseable even when a dependency logs. When a
failure surfaces, attribute it before reacting:

  * storage / "database is busy" / OpenRead -> likely `frankensqlite`.
  * lexical / vector / hybrid search -> likely `frankensearch` (Tantivy/vector).
  * runtime / scheduler / load stalls -> likely `asupersync` or host load.
  * remote transport -> SSH / rsync / scp / SFTP.

Dependency tracing belongs behind an explicit `--trace-file` or env flag, not in
normal robot output. If a robot command leaks dependency logs onto stdout/stderr
such that you cannot parse it, that is a cass problem to report.

## See also

    cass triage --json          # one-shot readiness + next_command
    cass robot-docs doctor      # the cass doctor agent handbook
    cass robot-docs guide       # quick-start walkthrough
    cass robot-docs exit-codes  # error-handling / err.kind taxonomy
"#;
