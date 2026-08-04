# Resilience Proof Recipe & Log-Completeness Gate

Bead: `coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.12.6`.

The single named proof suite that implementers run **identically locally and
in CI** for the resilience rollout, plus the log-completeness gate that makes
the integrated resilience gate (`.11.5`) unable to pass by doing nothing.

Pairs with:
- **`.12.1`** — [`RESILIENCE_TEST_MATRIX.md`](RESILIENCE_TEST_MATRIX.md): which proof each family owes.
- **`.12.3`** — `src/search/proof_log.rs`: the proof-log record + retention.
- **`.12.4`** — `UNIT_TEST_HARNESS_REQUIREMENTS.md`: unit cases per family.
- **`.12.5`** — `src/search/e2e_scenarios.rs`: the CI/live scenarios.
- **`.12.2`** — `src/e2e_runner.rs`: the bounded runner.

All commands follow `AGENTS.md`: remote compilation via `rch`, an isolated
`CARGO_TARGET_DIR`, and `-D warnings` on clippy.

## 0. Conventions

- **Remote build/test:** prefix cargo with
  `rch exec -- env CARGO_TARGET_DIR=<isolated-dir> ...`. Use a per-agent dir
  (e.g. `/data/tmp/cass-check-target`) so concurrent agents don't collide.
- **Success signal:** grep the output for `Finished` and the
  `test result: ok.` line — do **not** trust a piped exit code (a `| tail`
  pipeline masks cargo's status).
- **Format:** edition 2024 (`rustfmt --edition 2024 <file>` /
  `cargo fmt --check`).

## 1. Compile / lint / format gate (after substantive code changes)

```sh
# Full compile of every target (lib, tests, benches):
rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-check-target cargo check --all-targets
# Lint, warnings-as-errors, crate-wide:
rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-check-target cargo clippy --all-targets -- -D warnings
# Format check:
cargo fmt --check
# Bug scan on changed files (exit 0 required; #[cfg(test)] helper panics and
# intentional fake-secret fixtures are acceptable, triaged, criticals):
ubs <changed-files> --ci
```

## 2. Targeted unit/integration tests by feature family

Run the family you touched (fast; isolated target dir optional). The
resilience contract cores live under `src/search/` and `src/indexer/`:

```sh
# Readiness & archive-risk (.1.x):           cargo test --lib search::readiness
# Readiness fixtures (.1.5):                  cargo test --lib search::readiness_fixtures
# Liveness: progress/stall (.4.1):           cargo test --lib search::progress_contract
# Liveness: salvage ledger (.4.2):           cargo test --lib search::salvage_ledger
# Liveness: watch recovery (.4.3):           cargo test --lib search::watch_recovery
# Liveness: watch-exit envelope (.4.4):      cargo test --lib search::watch_exit_envelope
# Liveness fixtures (.4.5):                   cargo test --lib search::liveness_fixtures
# Semantic readiness (.5.1):                  cargo test --lib search::semantic_readiness
# Semantic progress sink (.5.2):              cargo test --lib indexer::semantic_progress
# Semantic publish safety (.5.3):             cargo test --lib search::semantic_publish_safety
# Workspace zero-result (.7.1):               cargo test --lib search::zero_result_diagnosis
# Source provenance (.7.2):                   cargo test --lib search::source_provenance
# Drill-down (.7.3):                          cargo test --lib search::drill_down
# Workspace/source fixtures (.7.4):           cargo test --lib search::workspace_source_fixtures
# Quarantine compat (.3.4):                   cargo test --lib indexer::quarantine
# Incident categories (.10.1):                cargo test --lib search::incident_categories
# Incident redaction (.10.5):                 cargo test --lib search::incident_redaction
# Storage integrity (.14.1):                  cargo test --lib search::storage_integrity
# Proof-log schema (.12.3):                   cargo test --lib search::proof_log
# Regression corpus (.11.2):                  cargo test --lib search::regression_corpus
# Recovery journeys (.13.1):                  cargo test --lib search::recovery_journeys
# E2E scenarios (.12.5):                      cargo test --lib search::e2e_scenarios
```

Each prefixed with `rch exec -- env CARGO_TARGET_DIR=<dir>`. A green run is
`test result: ok. N passed; 0 failed`.

## 3. Golden update flow

Golden artifacts (pinned JSON/JSONL wire forms) change **only** through a
reviewed run:

```sh
UPDATE_GOLDENS=1 rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-check-target cargo test <golden-target>
```

A `UPDATE_GOLDENS=1` diff must be reviewed as an intentional contract change
(it is a wire-format break otherwise). The default (unset) run asserts
goldens unchanged.

## 4. Shared E2E runner (quick / full)

The `.12.2` runner executes the `.12.5` scenarios against the real `cass`
binary into an artifact directory under a bounded timeout, emitting one
`.12.3` `ProofLogRecord` per command:

```sh
# quick: the CI scenario set (no live host) — the default gate.
cass-e2e-runner --mode quick --artifacts <dir> --timeout-ms <budget>
# full: quick + opt-in live-host scenarios (operator only; never CI-required).
cass-e2e-runner --mode full --live-hosts <hosts> --artifacts <dir>
```

`--mode quick` runs exactly `e2e_scenarios::ci_scenarios()` (every named
fleet/archive state, deterministically, no live host). Live scenarios
(`requires_live_host=true`) run only under `--mode full`.

## 5. Log-completeness gate (the integrated gate cannot pass by doing nothing)

After a runner pass, the gate asserts the artifact directory's
`ProofLogRecord`s are **complete**, not merely "no failures observed":

1. **Coverage:** the set of `scenario_id`s with a record equals
   `e2e_scenarios::ci_scenarios()` (quick) — a missing scenario is a gate
   failure, not a silent skip.
2. **No empty pass:** the record count is ≥ the expected scenario×command
   count; zero records fails the gate (cannot pass by doing nothing).
3. **Outcome integrity:** every record's `outcome` is `passed`. Any
   `timed_out_partial`, `stale_artifact_reused`, `invalid_json`,
   `did_not_run`, or `failed` fails the gate — these are distinguished by the
   `.12.3` schema precisely so a timeout/stale/skip can never read as a pass.
4. **Freshness:** records are from this run (`finished_at_ms` within the run
   window), not a reused stale artifact.
5. **Redaction:** `RetentionPolicy::is_redaction_safe` holds for every
   retained record (no secret-bearing `sanitized_env` keys).

A closure report cites the artifact directory + the per-scenario
`ProofLogRecord` outcomes; "tests pass" prose without cited artifacts does
not satisfy the closure checklist in `RESILIENCE_TEST_MATRIX.md`.

## 6. The named suite (one command surface)

Implementers and CI invoke the same logical suite:

1. §1 compile/lint/format gate.
2. §2 targeted family tests for changed families (or all, in CI).
3. §4 `--mode quick` E2E runner into an artifact dir.
4. §5 log-completeness gate over the artifacts.

Local and CI differ only in scope (`--mode quick` vs a nightly `--mode full`)
and target-dir isolation — never in the assertions. This is the recipe a
closure must cite by exact command + artifact path.

## 7. Real-binary robot dispatch smoke gate (`.2.4`)

Bead `…uojcg.2.4`. Where §4's runner exercises fleet/archive **state**
scenarios, this gate exercises **dispatch correctness** of the critical robot
surfaces — the gap that let **pass-12** (`doctor --json` returning the agent
handbook) slip past every golden/unit check, because those checked the right
*emitter* while real dispatch pointed at the wrong one.

```sh
# Routine (sub-second per surface against an isolated empty data dir):
rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-check-target \
  cargo test --test e2e_robot_smoke_gate
# CI proof artifacts (.12.3 structured log + manifest via PhaseTracker):
E2E_LOG=1 rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-check-target \
  cargo test --test e2e_robot_smoke_gate
```

`tests/e2e_robot_smoke_gate.rs` runs the real `cass` binary across
`api-version`, `capabilities`, `introspect`, `diag`, `health`, `status`,
`doctor`, `triage`, `search`, `pack`, `stats`, and `view`, asserting per
surface: **success payloads are pure JSON on stdout** (parse consumes the
whole trimmed stdout), **surface-identity keys** (e.g. doctor →
`checks`+`doctor_command`; the dispatch proof), and — for the error surfaces —
**the `{error:{...}}` envelope on stderr with stdout empty** (the stdout=data /
stderr=diagnostics hygiene) carrying a **stable kebab error kind** with the
process exit code mirroring `error.code`. Every surface also asserts **no
ANSI/TUI escape on stdout** and **bounded completion**.

Interpreting outcomes:

| Outcome | Meaning |
|---------|---------|
| **PASS** | process exited (not signal-killed) within the bound, stdout was pure JSON, identity/error-kind assertions held. |
| **FAIL (assertion)** | a surface dispatched wrong, leaked stdout, drifted an error kind, or mis-mirrored its exit code — the panic names the surface, argv, and payload head; every surface is evaluated and logged **before** the panic, so the proof log shows the full picture without a rerun. |
| **TIMEOUT (≠ pass, ≠ fail)** | a surface exceeded the per-surface bound (a hang, e.g. an accidental bare-TUI launch blocking on closed stdin) — the `TIMEOUT DIAGNOSTIC` block on stderr (phase, pid, elapsed, data-dir listing, stdout/stderr tails) is the timeout-vs-pass discriminator. |

Surface signatures are pinned against the golden robot JSON under
`tests/golden/robot/`; an intentional contract change updates both together.

## 8. Lightweight proof artifacts (`.11.4`)

Bead `…uojcg.11.4`. Where §4's `.12.3` `ProofLogRecord` is the heavyweight
record emitted by the bounded runner, `src/proof_artifact.rs` is the **lightweight
classifier** that any test, gauntlet, or smoke gate can emit without standing up
the full runner. It exists so the five-plus confusable outcomes — `pass`,
`fail`, `timeout`, `skipped`, `stale-artifact`, `generated-only`,
`partial-proof` — are recorded distinctly, with the safety-first precedence that
**a timeout outranks a zero exit** (the 7200s-timeout-before-tests trap can never
read as a pass) and **assertions that did not run are `generated-only`, never a
pass**.

Emitting an artifact:

```rust
use coding_agent_search::proof_artifact::{ProofRun, emit_proof_artifact, ProofManifest};

// Record the run's facts (timestamps in; no clock read inside), classify, and
// write `<dir>/<label>.proof.json`:
let emitted = emit_proof_artifact(proof_dir, "repro-capsule-search-miss", run)?;

// Aggregate into a manifest whose verdict cannot pass by doing nothing:
let mut manifest = ProofManifest::new();
manifest.record(emitted);
assert!(manifest.is_clean_pass());          // false when empty or any non-pass entry
manifest.write_jsonl(&proof_dir.join("proof-manifest.jsonl"))?;
```

`ProofManifest::is_clean_pass()` is the log-completeness verdict for this layer:
it is `true` only when there is **at least one** entry and **every** entry is a
trustworthy `pass`. An empty manifest (a gate that ran nothing) and any single
`timeout` / `stale-artifact` / `generated-only` / `fail` / `partial-proof` /
`skipped` entry both return `false`. `worst_status()` surfaces the single most
severe outcome for a one-line rollup.

Wiring into a gate (reference adopter):

```sh
# Each scenario emits one `<label>.proof.json` into $CASS_PROOF_DIR:
CASS_PROOF_DIR=<dir> rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-check-target \
  cargo test --test e2e_repro_capsule_gate
```

`tests/e2e_repro_capsule_gate.rs` is the reference adopter: its `run_capsule`
emits a proof artifact per invocation when `CASS_PROOF_DIR` is set, and
`proof_artifacts_emit_and_distinguish_pass_from_timeout` proves end-to-end that a
real passing run emits a `pass` artifact while a timeout-before-assertions emits
`timeout` (and sinks the manifest). A closure citing this layer points at the
`$CASS_PROOF_DIR` artifact files + the exact `cargo test` command above — not
prose. The lib classifier itself is proven by
`cargo test --lib proof_artifact` (`test result: ok`).

## 9. Local guided-operations dashboard (`5u82n.12`)

The dashboard is a human-readable, read-only projection over existing robot
contracts. Render its self-contained offline HTML to stdout:

```sh
cass swarm dashboard --html > /tmp/cass-operations-dashboard.html
```

The report has no JavaScript, network dependency, form, apply mode, or file/DB
mutation path. The shell redirection above is the operator's explicit choice;
`cass` itself never writes the report. For deterministic test or support
fixtures, pass `--fixture <path>` (or `--fixture-dir <dir> --fixture-id <id>`).

Agents must not parse the HTML. Consume the underlying JSON contracts directly:

```sh
cass guide <intent> --json
cass swarm macros --json
cass swarm resource-plan --json
cass swarm privacy-preview --json
cass swarm repro-capsule --json
cass search <query> --robot --robot-meta
cass swarm evidence --json
```

The capsule's `rerun.command_template` is the real read-only surface:
`cass swarm repro-capsule --json --fixture repro-capsule.fixture.json`.
The emitted capsule is itself a valid, redacted input fixture. Save it under
that fixed filename and run the command from the same directory. The operator's
original path is intentionally omitted, and the capsule id remains provenance
metadata rather than a command-line flag.
Replay guarantees the same scrubbed capsule body and capsule id. Run-local
timestamps, redaction counters, and diagnostic status are regenerated and may
differ from the first render.

Use `cass swarm dashboard --json` only when automation needs the normalized
cross-surface rollup (current goal, blockers, warning counts, recent capsule
metadata, and the next proof command). Search/session content is never copied
into that model; trust is metadata-only, paths/secrets are redacted, and local
capsule links must be safe relative paths.

Proof the fixture adapter, byte-for-byte deterministic HTML, hostile-input
escaping, long-path behavior, empty/partial/blocked states, redaction, and the
no-network/no-mutation contract with:

```sh
rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-dashboard-target \
  cargo test --lib operations_dashboard
rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-dashboard-target \
  cargo test --test operations_dashboard_contract
```

## 10. Integrated guided-operations golden gate (`5u82n.13`)

The capstone gate runs the guided surfaces together against checked-in,
synthetic fixtures and freezes their composed contract. It writes runtime audit
artifacts into an isolated temporary directory: stdout, stderr, and parsed JSON
for each scenario plus redaction, fixture-manifest, timing, and assertion
summaries. The temporary artifact set is validated during the test; the stable,
scrubbed robot rollup and reviewed command matrix live under
`tests/golden/guided_ops/`.

```sh
rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-guided-golden-target \
  cargo test --test e2e_guided_ops_golden_gate
```

The pass contract requires an unchanged before/after HOME and data-directory
snapshot, zero private-marker leaks, robot-safe recommendations, every command
inside its timing budget, and a full `bv --robot-insights` result with zero
dependency cycles. Regeneration is opt-in via `UPDATE_GUIDED_OPS_GOLDENS=1` and
must be followed by byte-level review of both goldens and a second non-update
run.

## 11. Gated guide apply/run mode (`5u82n.17`)

`cass guide` remains a dry-run unless `--apply` (alias `--run`) is explicit.
Every recognized plan now includes a deterministic `execution` transcript with
tokenized argv, allowlist decision, mutation class, rch decision, proof-gate
result/source, and per-step confirmation evidence. Macro command identifiers are
never passed to a shell.

Before a mutating adapter can run, apply mode requires ready prerequisites,
clear stop conditions, exact privacy/cost-tier acceptance where applicable, the
macro's rch grant, all preceding proof gates, and `--confirm-step <N>` for that
exact mutation. Confirmations do not carry between steps. Fixture apply is
permanently non-mutating, even when every grant is present; this is the stable
way to exercise a fully authorized transcript in tests.

```sh
# Read-only plan and transcript.
cass guide support-capsule --json

# Gated live apply. Review the dry-run and resource/privacy surfaces first.
cass guide support-capsule --apply \
  --confirm-fact db_present \
  --accept-privacy-tier redacted \
  --accept-cost-risk medium \
  --confirm-stop-conditions-clear \
  --confirm-step 3 \
  --json
```

An apply request can still return `overall_status=blocked` or
`awaiting-confirmation`; inspect `execution.global_gates[]` and
`execution.transcript[]` rather than inferring success from the request flag.
Only the closed, parameter-complete support-capsule mutation adapter is live in
this slice. Other mutation identifiers remain honestly `adapter-unavailable`
until they have equally strict typed inputs and rollback/proof coverage.

Proof the real CLI contract, fixture no-mutation guarantee, deterministic
transcript, and the readiness/privacy/cost/rch/stop/confirmation gates with:

```sh
RCH_REQUIRE_REMOTE=1 rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-guide-apply-target \
  cargo test --locked --test e2e_guide_apply_gate
```
