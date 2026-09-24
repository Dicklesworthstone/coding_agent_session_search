# Swarm Operations Robot Contract

**Bead:** `coding_agent_session_search-oh96l.1`
**Status:** Contract for implementation beads
**Date:** 2026-05-08

> **Implementation status (checked 2026-09-24 against `cass swarm --help` and
> src/lib.rs; bead 2l1b0.59).** This contract is the target. The shipped
> `cass swarm status` accepts only `--json`, `--robot-format`, `--fixture`,
> `--fixture-dir` and `--fixture-id`. The flags in the Inputs table below
> (`--repo`, `--project-key`, `--stale-threshold-seconds`, the six `--max-*`
> limits, `--include-evidence`, `--no-process-scan`, `--request-id`) do not
> exist, and `swarm-unsupported-format` is never emitted. The shipped surface
> also has subcommands this contract does not describe: `work-packet`, `lint`,
> `evidence`, `proof-debt`, `failure-patterns`, `dependency-drift`,
> `resource-plan`, `privacy-preview`, `context-pack`, `workflow-analytics`,
> `replay-fixture`, `macros`, `repro-capsule` and `dashboard`. Live `lint`,
> `evidence`, `proof-debt` and `failure-patterns` report every provider
> `live-provider-unimplemented`; fixtures are their tested path.

This document defines the first robot contract for a read-only swarm operations
surface in cass. The surface composes existing coordination truth sources into
one stable status payload for agents and operators who are managing a busy shared
repository.

The initial command name is:

```bash
cass swarm status --json
```

The first advisory planning command built on that status payload is:

```bash
cass swarm work-packet --json
cass swarm work-packet --json --bead coding_agent_session_search-example
```

The command is advisory. It does not claim beads, release reservations, kill
processes, run builds, repair indexes, mutate git state, or scrape raw private
session content. It reports what it can prove, names the source of each section,
and marks unavailable providers as partial data.

## Operator Workflow

Use the swarm surface as a cockpit, not as an ownership system. Beads remains
the source of truth for issue status, Agent Mail remains the coordination
channel, rch remains the required build/test execution path, and cass
search/pack remains the evidence lookup and handoff path.

1. Start with a read-only snapshot:

   ```bash
   cass swarm status --json
   ```

   Read `summary.recommended_action`, `providers[]`, `beads.ready[]`,
   `reservations[]`, `build_pressure`, and `evidence.proof_gaps[]`. Partial
   provider status is a warning to inspect, not permission to override another
   agent.

2. If a bead looks safe, ask for a scoped packet before editing:

   ```bash
   cass swarm work-packet --json --bead coding_agent_session_search-example
   ```

   The packet may suggest file reservations, Agent Mail copy, and rch proof
   commands. It does not create those reservations, send the mail, update Beads,
   or run the commands. Agents still use the real tools:
   `br update ... --status in_progress --json`, Agent Mail reservations and
   messages, and `rch exec -- env CARGO_TARGET_DIR=... cargo ...`.

3. If `stale_candidate_count` is non-zero, treat it as a review queue:

   ```bash
   cass swarm status --json --stale-threshold-seconds 3600
   br show coding_agent_session_search-example --json
   ```

   Stale detection is advisory only. Do not reopen, force-release, or take over
   work until Beads, Agent Mail, reservations, dirty paths, and recent commits
   all agree that the holder is inactive or explicit coordination has happened.

4. If status points at prior proof or discussion, build a bounded cited handoff:

   ```bash
   cass pack "coding_agent_session_search-example closeout proof" --robot --max-tokens 8000
   ```

   Packs are token-budgeted evidence bundles. They are useful for handoff and
   review, but they do not replace Beads close reasons, Agent Mail closeout, or
   command transcripts.

5. Before closeout or takeover review, lint coordination hygiene:

   ```bash
   cass swarm lint --json --bead coding_agent_session_search-example
   ```

   Findings are suggestions. The lint surface never acknowledges messages,
   sends mail, edits files, releases reservations, updates beads, or runs git.

## Goals

- Show one robot-safe view of active swarm work in the current repository.
- Explain which beads are ready, in progress, blocked, stale-looking, or unsafe
  to claim.
- Surface active file reservations, recent Agent Mail activity, recent commits,
  and rch/build pressure without mutating any of them.
- Include cass health/status readiness so operators know whether search and
  answer-pack evidence are fresh enough to trust.
- Emit stable JSON that can be pinned by fixture and golden tests before any TUI
  or automation consumes it.

## Non-Goals

- No replacement for Beads, Agent Mail, git, rch, cass search, or cass pack.
- No automatic bead claiming, reopening, closing, or stale takeover.
- No automatic Agent Mail force release.
- No automatic `git fetch`, push, reset, checkout, stash, or cleanup.
- No cargo/build execution and no rch job creation.
- No bare `cass`, bare `bv`, interactive TUI launch, or browser test execution.
- No inclusion of raw private session transcripts unless a future explicit
  opt-in evidence command adds redacted excerpts.

## Command Surface

```bash
cass swarm status --json
cass swarm status --json --repo /data/projects/coding_agent_session_search
cass swarm status --json --include-evidence
cass swarm status --json --stale-threshold-seconds 1800
cass swarm status --json --max-commits 20 --max-messages 50
```

The command supports only structured output at first. `--robot-format json`,
`--robot-format compact`, and `--robot-format toon` may be added by the
implementation if they can reuse the existing structured-output helpers.
`--robot-format sessions` is not meaningful for this command and should return a
JSON error envelope with `err.kind="swarm-unsupported-format"`.

## Inputs

| Flag | Type | Default | Contract |
|------|------|---------|----------|
| `--repo <path>` | path | current working directory | Repository root used for Beads, git, and provider fixtures. |
| `--project-key <path>` | path | same as `--repo` | Agent Mail project identity. Must be an absolute path. |
| `--stale-threshold-seconds <n>` | integer | `1800` | Advisory age threshold for quiet in-progress work. |
| `--max-ready <n>` | integer | `20` | Maximum ready beads to include. |
| `--max-in-progress <n>` | integer | `50` | Maximum in-progress beads to include. |
| `--max-agents <n>` | integer | `50` | Maximum active agents to include. |
| `--max-reservations <n>` | integer | `100` | Maximum file reservations to include. |
| `--max-commits <n>` | integer | `20` | Maximum recent git commits to include. |
| `--max-messages <n>` | integer | `50` | Maximum recent Agent Mail messages to include. |
| `--include-evidence` | flag | false | Include redacted proof summaries and source references where available. |
| `--no-process-scan` | flag | false | Skip local process/rch pressure inspection. |
| `--fixture-dir <path>` | path | none | Read deterministic provider fixtures instead of live local providers. |
| `--request-id <id>` | string | none | Echoed in `_meta.request_id`. |

Provider fixtures must never contact live Agent Mail, run git commands against a
remote, or inspect private session logs. They are for deterministic tests and
golden generation.

## Work Packet Surface

`cass swarm work-packet --json` is a read-only projection over
`cass swarm status --json`. It selects the first safe ready bead, or a requested
`--bead <id>`, and returns a scoped advisory packet for one agent. It must not
claim the bead, reserve files, send Agent Mail, or run verification commands.

Top-level shape:

```json
{
  "schema_version": "cass.swarm.work_packet.v1",
  "status": "ok",
  "_meta": {
    "source_schema_version": "cass.swarm.status.v1"
  },
  "filter": {"bead_id": null},
  "summary": {
    "bead_id": "coding_agent_session_search-example",
    "safe_to_start": true,
    "readiness_state": "ready",
    "recommended_action": "claim-ready-bead",
    "requires_coordination": false,
    "claim_blocker_count": 0,
    "suggested_reservation_count": 2,
    "proof_command_count": 3
  },
  "work_packet": {
    "bead": {},
    "readiness": {},
    "suggested_reservations": [],
    "coordination": {},
    "verification": {},
    "closeout": {},
    "fallback_actions": []
  },
  "source_status": {},
  "privacy": {}
}
```

`readiness_state` is branchable: `ready`, `blocked`,
`build-pressure-high`, `provider-partial`, `bead-not-found`, or
`no-ready-work`. Only `ready` sets `summary.safe_to_start=true`. Suggested file
reservations are heuristics derived from labels and existing status evidence;
they are not leases. Operators and agents still create real reservations through
Agent Mail. Verification commands must use the project rch command shapes, and
high build pressure changes the recommendation to `wait-for-rch-capacity`
without deleting the later proof plan.

The work packet closeout section may include commands such as `br close ...`,
but these remain suggestions. Agents still need an artifact-backed proof summary,
an Agent Mail closeout, and the normal Beads state transition.

## Coordination Lint Surface

`cass swarm lint --json` is a read-only protocol checker over the same Beads,
Agent Mail, git, reservation, and evidence snapshots used by status and
work-packet. It detects hygiene problems before an agent starts or closes work:
missing start mail, missing closeout mail, unacknowledged `ack_required`
messages, stale or dead-agent reservations, bead status mismatches, missing
close reasons, missing proof references, dirty peer files, unsafe takeover
language, and unavailable Agent Mail.

The lint command never acknowledges messages, sends mail, updates Beads,
releases reservations, edits files, or runs git commands. Its `safe_next_action`
fields are suggestions only.

```json
{
  "schema_version": "cass.swarm.coordination_lint.v1",
  "status": "ok",
  "summary": {
    "finding_count": 0,
    "error_count": 0,
    "warning_count": 0,
    "info_count": 0,
    "mutation_performed": false,
    "recommended_action": "coordination-clean"
  },
  "findings": [],
  "mutation_contract": {
    "read_only": true,
    "agent_mail_mutations": false,
    "bead_mutations": false,
    "reservation_mutations": false,
    "git_mutations": false,
    "safe_next_actions_are_suggestions": true
  }
}
```

Findings are deterministic and metadata-only:

| Field | Type | Contract |
|-------|------|----------|
| `id` | string | Stable `code:subject_kind:subject_id` identifier. |
| `severity` | enum | `error`, `warning`, or `info`. |
| `code` | string | Branchable finding code such as `missing-closeout-mail`. |
| `subject_kind` | string | `bead`, `message`, `reservation`, `provider`, or `git`. |
| `subject_id` | string | Bead id, message id/thread id, reservation reason, or provider name. |
| `evidence_refs` | array[string] | Provider paths used for the finding. |
| `safe_next_action` | string | Suggested non-mutating or explicitly operator-chosen next step. |
| `redacted_snippet` | string/null | Optional redacted subject/snippet only; full mail bodies stay omitted. |

## Exit Semantics

| Exit | Meaning | JSON shape |
|------|---------|------------|
| `0` | Payload generated. Some provider sections may still be partial. | `status="ok"` or `status="partial"` |
| `1` | Repository is not usable for swarm status. | JSON error envelope |
| `2` | Usage or parsing error. | JSON error envelope |
| `7` | A local advisory lock is busy and no snapshot can be read. | JSON error envelope with `err.kind="lock-busy"` |
| `9` | Unknown internal error. | JSON error envelope |

Partial provider failures do not fail the command when at least one core source
is readable. They set `status="partial"`, add `providers[].status="unavailable"`,
and append a machine-readable warning.

## Top-Level JSON

All object keys are stable and snake_case.

```json
{
  "schema_version": "cass.swarm.status.v1",
  "status": "ok",
  "_meta": {
    "request_id": "optional-client-id",
    "generated_at_ms": 1778219200000,
    "elapsed_ms": 25,
    "repo": "/data/projects/coding_agent_session_search",
    "project_key": "/data/projects/coding_agent_session_search",
    "hostname": "host",
    "partial": false,
    "warnings": []
  },
  "providers": [],
  "summary": {},
  "beads": {},
  "agents": [],
  "reservations": [],
  "build_pressure": {},
  "git": {},
  "cass": {},
  "evidence": {},
  "recommendations": [],
  "privacy": {}
}
```

## Provider Model

Every provider contributes a status record:

| Field | Type | Contract |
|-------|------|----------|
| `name` | string | `beads`, `bv`, `agent_mail`, `git`, `process`, `cass_health`, `cass_status`, or `evidence`. |
| `source` | string | Exact command/API/fixture used, for example `br ready --json`. |
| `status` | enum | `ok`, `partial`, `unavailable`, or `skipped`. |
| `freshness_ms` | integer/null | Age of the source snapshot when known. |
| `elapsed_ms` | integer/null | Provider collection time. |
| `error_kind` | string/null | Kebab-case failure kind when unavailable. |
| `warning` | string/null | Short operator-readable detail. |

Provider errors must not be inferred from prose. Each error needs a branchable
`error_kind` such as `missing-command`, `parse-error`, `lock-busy`,
`permission-denied`, `fixture-missing`, or `timeout`.

## Summary

`summary` is the fast first screen:

| Field | Type | Contract |
|-------|------|----------|
| `ready_count` | integer | Count from Beads ready set. |
| `in_progress_count` | integer | Count from Beads in-progress set. |
| `blocked_count` | integer | Count from Beads blocked set. |
| `active_agent_count` | integer | Agents active inside the configured recency window. |
| `active_reservation_count` | integer | Unexpired file reservations. |
| `dirty_worktree` | boolean | True if git status reports any modified/untracked tracked surface. |
| `build_pressure` | enum | `none`, `light`, `moderate`, `high`, or `unknown`. |
| `stale_candidate_count` | integer | Advisory stale candidates, never automatic takeovers. |
| `proof_gap_count` | integer | Recent commits/beads missing machine-readable proof evidence. |
| `recommended_action` | string/null | One safe next action, not a mutation unless the user explicitly chooses it. |

## Beads Section

`beads` contains Beads and bv-derived issue state:

```json
{
  "ready": [],
  "in_progress": [],
  "blocked": [],
  "stale_candidates": [],
  "graph": {
    "node_count": 0,
    "edge_count": 0,
    "has_cycles": false,
    "source": "bv --robot-insights"
  }
}
```

Each bead item:

| Field | Type | Contract |
|-------|------|----------|
| `id` | string | Bead id. |
| `title` | string | Bead title. |
| `status` | string | Beads status. |
| `priority` | integer/null | Beads priority. |
| `issue_type` | string/null | Beads issue type. |
| `labels` | array[string] | Labels, sorted for determinism. |
| `updated_at` | string/null | Source timestamp. |
| `age_seconds` | integer/null | Snapshot age if known. |
| `owners` | array[string] | Agents inferred from status, reservations, or messages. |
| `safe_to_claim` | boolean | False if in progress, reserved, dirty peer files, or conflicting evidence. |
| `claim_blockers` | array[string] | Machine-readable blockers such as `active-reservation`, `dirty-peer-work`, `recent-mail`, `dependency-blocked`. |
| `recommended_action` | string/null | Example: `coordinate-before-claim`, `claim-with-br-update`, `wait-for-owner`, `manual-review`. |

`stale_candidates[]` uses the same fields plus:

| Field | Type | Contract |
|-------|------|----------|
| `stale_state` | enum | `active`, `recently_quiet`, `likely_stale`, `conflicting_evidence`, or `manual_review_required`. |
| `evidence` | array[object] | Exact evidence records used for classification. |
| `confidence` | enum | `low`, `medium`, or `high`; high still does not mutate. |

## Agents And Reservations

`agents[]` summarizes Agent Mail participants:

| Field | Type | Contract |
|-------|------|----------|
| `name` | string | Agent Mail name. |
| `program` | string/null | Registered program. |
| `model` | string/null | Registered model. |
| `task_description` | string/null | Registered task. |
| `last_active_ts` | string/null | Agent Mail activity timestamp. |
| `active` | boolean | Based on the command recency window. |
| `current_threads` | array[string] | Recent thread ids, not message bodies. |

`reservations[]` reports active file leases:

| Field | Type | Contract |
|-------|------|----------|
| `id` | integer/null | Agent Mail reservation id when available. |
| `holder` | string | Agent name. |
| `path_pattern` | string | Repository-relative path or glob. |
| `exclusive` | boolean | Reservation exclusivity. |
| `reason` | string/null | Holder-provided reason. |
| `expires_ts` | string/null | Expiry timestamp. |
| `state` | enum | `active`, `expired`, `conflicting`, or `unknown`. |
| `overlaps_dirty_worktree` | boolean | True when git status touches this path. |

The command must not force-release or renew reservations. It may recommend the
existing Agent Mail stale-release workflow only when the evidence is high
confidence and no recent dirty work or mail suggests live ownership.

## Build Pressure

`build_pressure` is local and advisory:

| Field | Type | Contract |
|-------|------|----------|
| `status` | enum | `none`, `light`, `moderate`, `high`, or `unknown`. |
| `active_rch_jobs` | integer/null | Count inferred from local process samples or fixture data. |
| `active_cargo_jobs` | integer/null | Count of cargo processes; should be zero for agents using rch. |
| `load_average_1m` | number/null | Local sample if available. |
| `cpu_count` | integer/null | Local logical CPU count if sampled. |
| `recommended_action` | string/null | Example: `wait-for-rch-slot`, `avoid-local-cargo`, `safe-to-run-focused-rch-test`. |

The provider may read local process metadata, but it must not kill or reprioritize
processes.

## Git Section

`git` reports local repository state:

| Field | Type | Contract |
|-------|------|----------|
| `branch` | string/null | Current branch. |
| `upstream` | string/null | Upstream ref when configured. |
| `ahead` | integer/null | Ahead count if known. |
| `behind` | integer/null | Behind count if known. |
| `dirty` | boolean | Any local worktree/index changes. |
| `dirty_paths` | array[object] | Status code and path, capped by input limit. |
| `recent_commits` | array[object] | Recent local commits with hash, subject, author time, and touched paths when included. |
| `legacy_branch_mirror_required` | boolean/null | True only when local refs prove the legacy compatibility branch lags `origin/main`. |

The command must not run `git fetch`, `git pull`, `git push`, checkout, reset, or
stash. Mirror advice is informational and should name the exact command only in
`recommended_action` after proving the refs locally.

## Cass Readiness

`cass` embeds compact health and status facts:

| Field | Type | Contract |
|-------|------|----------|
| `health_status` | string/null | From `cass health --json` or fixture. |
| `healthy` | boolean/null | Health boolean. |
| `initialized` | boolean/null | Whether data is initialized. |
| `recommended_action` | string/null | Existing cass health/status recommendation. |
| `search_ready` | boolean/null | Lexical readiness. |
| `semantic_fallback_mode` | string/null | Expected semantic fallback, if any. |
| `active_rebuild` | boolean/null | Existing rebuild flag. |

The swarm command must not run indexing, doctor repair, model install, or pack by
default. It may include a pack/search handoff recommendation when evidence is
fresh enough.

## Evidence

`evidence` is a summary, not raw transcript content:

| Field | Type | Contract |
|-------|------|----------|
| `recent_threads` | array[object] | Thread id, subject, sender, created timestamp, and redaction status. |
| `recent_proofs` | array[object] | Bead id, commit hash, command shape, exit status, and source reference. |
| `proof_gaps` | array[object] | Missing or conflicting proof for recent closeouts. |
| `redaction_applied` | boolean | True if any text was omitted or summarized for privacy. |

`include_evidence=false` should keep this section compact: counts, ids, and
source references only. `include_evidence=true` may include short redacted
snippets from Agent Mail closeouts, never raw private session logs.

## Recommendations

Each recommendation is branchable:

| Field | Type | Contract |
|-------|------|----------|
| `kind` | string | `claim-ready-bead`, `coordinate`, `wait`, `verify-proof`, `inspect-stale`, `reduce-build-pressure`, or `no-ready-work`. |
| `confidence` | enum | `low`, `medium`, or `high`. |
| `summary` | string | Short human-readable sentence. |
| `commands` | array[string] | Safe command suggestions only. |
| `requires_human_confirmation` | boolean | True for any takeover, force-release, mirror push, or repair-like action. |
| `evidence_refs` | array[string] | References into provider sections, not prose scraping. |

Commands must use robot-safe forms such as `br ready --json`,
`bv --robot-triage`, `cass health --json`, or `cass pack ... --robot`. The status
surface must not recommend destructive commands.

## Privacy And Redaction

`privacy` records the data boundary:

| Field | Type | Contract |
|-------|------|----------|
| `raw_session_content_included` | boolean | Must be false for this command version. |
| `mail_body_snippets_included` | boolean | True only with `--include-evidence`. |
| `redaction_policy` | string | `strict` for the first version. |
| `redaction_applied` | boolean | Whether any provider content was reduced. |
| `sensitive_paths_scrubbed` | integer | Count of path-like fields scrubbed in fixture/golden output. |
| `command_arguments_scrubbed` | integer | Count of command arguments reduced because they carried private paths, secrets, or host/user material. |
| `env_values_scrubbed` | integer | Count of environment values replaced before serialization. Environment names may remain when useful; values do not. |
| `mailbox_snippets_omitted` | integer | Count of mail body snippets omitted because the caller did not explicitly opt in. |
| `evidence_references_scrubbed` | integer | Count of pack/search/evidence references reduced before output. |
| `opt_in_boundary` | string | Human-readable boundary for evidence opt-ins. For v1, mail body snippets require `--include-evidence`; raw session content is unsupported. |

Golden tests must scrub host paths, timestamps, UUIDs, commit hashes where needed,
and any content-like mail fields. Provider fixtures should include hostile
private-looking input to prove it does not leak.

All swarm status and evidence-broker output should route through the shared
strict swarm-evidence redaction path before JSON serialization. Partial provider
failures do not bypass this rule: degraded providers may omit fields, but any
field they do emit must already be redacted. `--include-evidence` may allow
redacted mail body snippets in a future implementation; it must not enable raw
private session text for `cass.swarm.status.v1`.

## Threat Model

The primary threats are false authority, unsafe takeover, privacy leakage, and
resource surprise.

1. **False authority:** The command reports sources and freshness for every
   section. It must not invent state when Beads, Agent Mail, git, or cass health
   is unavailable.
2. **Unsafe takeover:** Stale classification is advisory. Dirty worktree files,
   recent messages, active reservations, or recent commits force `safe_to_claim`
   to false or `manual_review_required`.
3. **Privacy leakage:** Default output contains ids, paths, counts, hashes,
   command shapes, and redaction status. It excludes raw session content and
   full mail bodies.
4. **Resource surprise:** The command is read-only and bounded. It must avoid
   expensive scans by default, cap all arrays, and expose provider timings.
5. **Command injection:** Command suggestions are static templates assembled
   from validated ids and paths. They are not shell-concatenated from untrusted
   provider prose.
6. **Stale fixtures:** Fixture mode must be explicit and reported in
   `providers[].source`; fixture output should never be mistaken for live state.

## Test And Golden Plan

Implementation follow-up tasks should add:

- `tests/swarm_status_contract.rs` for schema-level unit coverage.
- `tests/golden_swarm_status.rs` or new cases in `tests/golden_robot_json.rs` for
  deterministic robot payloads.
- Fixtures under `tests/fixtures/swarm_status/` for:
  - clean dry queue;
  - active multi-agent work;
  - dirty peer work;
  - reservation conflict;
  - likely stale bead;
  - conflicting stale evidence;
  - rch build pressure;
  - recent commit with complete proof;
  - recent commit with proof gaps;
  - Agent Mail unavailable.
- `cass introspect --json` schema coverage for `swarm-status` once the command is
  implemented.
- `cass robot-docs` golden coverage once the command is documented.

Golden regeneration should use the existing rch pattern:

```bash
UPDATE_GOLDENS=1 rch exec -- env CARGO_TARGET_DIR=${TMPDIR:-/tmp}/rch_target_cass_swarm_status_goldens \
  cargo test --test golden_robot_json --test golden_robot_docs
```

Every golden diff must be reviewed before commit. No test may contact live Agent
Mail, run real rch jobs, inspect private session logs, or depend on current local
git history unless the data is fixture-backed and scrubbed.

## Acceptance Checklist

- Every top-level field has a named source of truth.
- Every provider has explicit stale/failure behavior.
- The command is read-only and mutation-free by default.
- Stale detection is advisory and evidence-backed.
- Privacy defaults exclude raw session content and full mail bodies.
- Recommended commands are robot-safe and non-destructive.
- Fixture and golden test targets are named for follow-up implementation.
