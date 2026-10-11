# Archive-First Doctor Recovery Runbook

This runbook documents the operator workflow for `cass doctor` after the
archive-first doctor v2 work. The core rule is preservation before repair:
`cass` treats the SQLite archive, raw mirror metadata, raw mirror blobs, backup
bundles, source ledgers, receipts, failure contexts, and support-bundle manifests
as evidence. Derived lexical, semantic, and report assets may be rebuilt through
fingerprinted plans, but recovery must not discard the only remaining copy of a
coding-agent session.

Use robot-safe commands for automation. Never run bare `cass` from an agent or
script because it launches the interactive TUI.

## Mental Model

- SQLite is the archive of record once a conversation is indexed.
- Provider session logs under harness-owned locations such as `~/.codex`,
  `~/.claude`, Cursor, Gemini, OpenCode, and remote sources are upstream inputs,
  not guaranteed long-term backups. Those harnesses may prune old session logs.
- The raw-session mirror is cass-owned evidence captured before parsing. It is
  content-addressed, path-safe, privacy-aware, and verified by hashes.
- Lexical and semantic indexes are derived assets. Rebuild them when the doctor
  plan says so; do not treat an index rebuild as proof that archived sessions are
  recoverable from upstream logs.
- Repairs build candidates first, compare coverage, verify integrity, and only
  promote a candidate when the approved plan proves equal-or-better archive
  coverage.

## What Doctor Must Never Delete

Doctor workflows must preserve:

- canonical archive databases such as `agent_search.db` and required sidecars
- raw mirror blobs, manifests, provenance ledgers, and source coverage ledgers
- backup bundles, restore rehearsal receipts, promotion receipts, and rollback
  references
- bookmarks, TUI state, source configuration, remote mirror metadata, and user
  configs
- provider source logs and private raw sessions unless an operator explicitly
  opts into a sensitive support artifact

Do not hand-remove cass data directories, index directories, raw mirrors, backup
bundles, WAL/SHM sidecars, or provider session trees as a repair step. If disk
pressure is the problem, run the storage-pressure and archive export flows below
and inspect their fingerprints before any mutation.

## First Response Checklist

1. Capture the read-only truth surface:

   ```bash
   cass doctor check --json
   ```

2. Branch on JSON fields, not prose:

   - `status`
   - `risk_level`
   - `recommended_action`
   - `operation_outcome.kind`
   - `operation_outcome.exit_code_kind`
   - `coverage_risk.status`
   - `source_authority.authority_level`
   - `raw_mirror.status`
   - `remote_source_sync.status`
   - `storage_pressure.status`
   - `repair_failure_marker.status`

3. If the command reports active locks, wait or retry later. Do not remove lock
   files by hand.

4. If the command reports sole-copy or source-pruned risk, preserve the cass
   archive first. Do not run source-only rebuild recipes.

5. If support is needed, collect the support handoff bundle near the end of this
   runbook.

## Source Pruning and Sole-Copy Warnings

Run the archive scan when the question is "does cass still have enough evidence
if upstream logs disappeared?":

```bash
cass doctor archive-scan --json
```

Important fields:

- `source_inventory` inventories current provider paths and FAD-backed sources.
- `coverage_summary` compares archived rows, current sources, raw mirror links,
  and legacy DB-only rows.
- `sole_copy_warnings` identifies conversations where cass may be the only
  remaining archival copy.
- `source_authority` explains whether the live source, raw mirror, verified
  backup, or current DB is the authority for a repair.
- `remote_source_sync` classifies remote gaps such as unavailable hosts, pruned
  upstream paths, local archive ahead of remote, and verified remote copies.

Treat missing upstream files as a preservation warning, not proof that the cass
archive is bad. If `sole_copy_warnings` is non-empty, take an archive export or
backup verification path before any repair that could replace canonical state.

## Low-Risk Auto-Run

The legacy command remains available for low-risk derived repairs:

```bash
cass doctor --fix --json
```

For a derived refresh request:

```bash
cass doctor --fix --force-rebuild --json
```

Safe auto-run is intentionally narrow. It can apply only predeclared safe actions
and must emit receipts for every mutation. It must fail closed when archive
coverage, source authority, prior repair failure markers, or storage-pressure
evidence make the action unsafe.

## Fingerprinted Repair Flow

Use explicit repair for candidate-based archive work.

1. Generate a read-only plan:

   ```bash
   cass doctor repair --dry-run --json
   ```

2. Inspect:

   - `repair_plan.plan_fingerprint`
   - `repair_plan.apply_command`
   - `candidate_staging`
   - `coverage_summary`
   - `source_authority`
   - `safety_gates`
   - `forensic_bundle.artifact_manifest_path`

3. Apply only the exact inspected fingerprint:

   ```bash
   cass doctor repair --yes --plan-fingerprint <plan_fingerprint> --json
   ```

4. Confirm:

   - `operation_outcome.kind` is an applied or no-op outcome, not blocked
   - `post_repair_probes` show successful read/write verification
   - `receipt.path` or `receipts[]` exists
   - `coverage_summary` did not shrink
   - rollback or restore guidance is present if promotion failed

If `repair_failure_marker.status` shows a previous failed repair, do not loop
blindly. Re-run the dry-run and use `--allow-repeated-repair` only when the
reported failure marker is part of the plan you inspected.

## Reconstruct Candidates

Reconstruction builds an isolated candidate from verified authority, such as the
raw mirror or a verified backup. It must not mutate the live archive while the
candidate is being built.

Current operator entry point:

```bash
cass doctor repair --dry-run --json
```

Inspect `candidate_staging` and `source_authority`. When a completed candidate is
eligible for promotion, the repair dry-run emits the fingerprinted apply command.
Promotion must still pass non-decreasing coverage, integrity checks, and
post-repair probes.

The `doctor-reconstruct-dry-run` schema in `cass introspect --json` documents the
candidate contract for automation. Treat any example path in documentation as
illustrative unless it comes from an actual `scripts/e2e/doctor_v2.sh` run
artifact.

## Backups, Restore Rehearsal, and Restore Apply

List backups before trusting a restore target:

```bash
cass doctor backups list --json
```

Verify a specific backup:

```bash
cass doctor backups verify <backup_id> --json
```

Run the restore rehearsal first. This is the default no-mutation restore mode:

```bash
cass doctor backups restore <backup_id> --json
```

Inspect `restore_plan.plan_fingerprint`, `restore_rehearsal.status`, manifest
hashes, sidecar completeness, and the rehearsal receipt. Apply only the matching
fingerprint:

```bash
cass doctor backups restore <backup_id> --yes --plan-fingerprint <plan_fingerprint> --json
```

The database and its write-ahead log (WAL) form one backup. Committed messages
may exist only in the WAL until a checkpoint writes them into the database.
Verification requires the declared files to match their recorded checksums and
rejects missing, changed, unrecorded, or ambiguously assigned bundle components.
A WAL failure must never downgrade the backup to a verified single-file database.
Keep the original bundle for inspection; changing its manifest checksums does not
repair missing or damaged data.

SHM is a derived index of the WAL. If the database engine needs to rebuild it,
verification reports `storage_probe_status=requires_private_wal_index_recovery`:
the manifest checks passed, but storage admission still requires rehearsal.
Rehearsal rebuilds SHM only in the private candidate and runs a database check.
The original backup remains unchanged, and the candidate database and WAL must
still match their recorded checksums after the probe. Apply runs the same check
before promotion.

Rehearsal copies sidecars beside the candidate using the candidate database's
actual filename, so its database probe sees the verified WAL. Apply checks the
recorded checksums again while copying and requires the completed candidate's
database and WAL inventory before changing the live archive. A WAL change also
invalidates derived search assets even when the main database bytes match; inspect
the promotion receipt's lexical and semantic rebuild requirements.

Restore apply must capture a pre-restore backup, build a candidate, verify the
candidate, promote atomically, and emit a restore receipt. If any verification
fails, stop and keep all artifacts for inspection.

## Cleanup and Archive Normalize

Cleanup is for derived or explicitly reclaimable assets, not archive evidence.

```bash
cass doctor cleanup --json
cass doctor cleanup --yes --plan-fingerprint <plan_fingerprint> --json
```

Archive normalize may add metadata annotations for hygiene findings. It must not
rewrite raw session blobs or canonical archive rows.

```bash
cass doctor archive-normalize --dry-run --json
cass doctor archive-normalize --yes --plan-fingerprint <plan_fingerprint> --json
```

If either command routes a finding to repair, reconstruct, or restore, leave it
out of cleanup and follow the higher-authority workflow.

## Storage Pressure and Archive Export

Use archive export or relocation planning when the archive is precious and the
current filesystem is under pressure.

Plan export:

```bash
cass doctor archive export /absolute/target/cass-archive-export --json
```

Apply export:

```bash
cass doctor archive export /absolute/target/cass-archive-export --yes --plan-fingerprint <plan_fingerprint> --json
```

Verify a copied bundle:

```bash
cass doctor archive export verify /absolute/target/cass-archive-export --json
```

For relocation planning, use:

```bash
cass doctor archive relocate /absolute/target/cass-archive --json
```

Important fields:

- `archive_export_plan.plan_fingerprint`
- `required_bytes`
- `copied_bytes`
- `verified_asset_classes`
- `skipped_asset_classes`
- `privacy_mode`
- `compression`
- `encryption`
- `config_update_status`
- `old_archive_retained`
- `will_delete_old_archive`
- `receipts`
- `event_log_path`
- `verify_status`

The old archive is retained. If `will_delete_old_archive` is ever true, stop and
audit the implementation before proceeding.

## Diagnostic Baselines

Save a baseline before risky investigation or after a known-good state:

```bash
cass doctor baseline save --json
```

Diff a later state against it:

```bash
cass doctor baseline diff <baseline_id> --json
```

Update a baseline only when the new state is intentionally the new known-good
reference:

```bash
cass doctor baseline update <baseline_id> --json
```

Baseline outputs are diagnostic-only. They should include artifact manifests and
redacted paths; they should not mutate archive evidence.

## Support Bundle Handoff

Create a scrubbed support bundle:

```bash
cass doctor support-bundle --json
```

Verify the manifest before sending or attaching it:

```bash
cass doctor support-bundle verify <bundle_or_manifest_path> --json
```

Default bundles are redacted diagnostic handoffs, not backups. They exclude raw
session content, raw mirror blobs, the full SQLite archive, encrypted payloads,
environment secrets, private source snippets, and full home-directory paths.

Support checklist:

- `cass doctor check --json` output
- latest `failure_context.json`, if present
- support bundle `manifest.json`
- `artifact_manifest_path` values referenced by doctor outputs
- `event_log_path` values referenced by doctor outputs
- baseline diff JSON, if a baseline exists
- backup verification JSON, if restore is being discussed
- exact command line and exit code for the failing command
- no raw session logs, no full SQLite archive copy, and no private source files
  unless the user explicitly opts into sensitive evidence attachment

## Troubleshooting Recipes

### Lock Contention

Symptom: `err.kind` is `lock-busy`, or `operation_state` reports an active
doctor, index, or watch owner.

Action: wait for the owner, retry `cass doctor check --json`, and attach lock
diagnostics if it stays busy. Do not remove lock files by hand.

### Storage Pressure

Symptom: `storage_pressure.status` is degraded or
`recommended_action` mentions archive export, relocation, or cleanup.

Action: run archive export planning first, verify the target has enough space,
then run cleanup only for derived/reclaimable assets with a matching
fingerprint.

### Missing Semantic Models

Symptom: `fallback_mode` is `lexical`, semantic model fields report absent
models, or search remains lexical-only.

Action: this is usually not an archive repair issue. Run:

```bash
cass models status --json
```

Install models only when the operator consents:

```bash
cass models install --json
```

### Remote Sync Gaps

Symptom: `remote_source_sync.status` or `source_inventory` reports unavailable
hosts, pruned paths, or local archive ahead of remote.

Action: preserve local cass evidence, then run:

```bash
cass sources sync --all --json
```

If the remote harness pruned logs, do not assume a local archive row is invalid.
Use `source_authority` and `coverage_summary`.

### Failed Post-Repair Probes

Symptom: repair apply wrote a receipt but `post_repair_probes` reports failed
read/write checks.

Action: stop. Collect the receipt, failure context, candidate manifest,
pre-mutation backup manifest, and event log. Use backup verify/restore rehearsal
before any further apply.

### Repeated Repair Markers

Symptom: `repair_failure_marker.status` reports a previous failed repair.

Action: do not re-run apply with the old fingerprint. Re-run the dry-run, inspect
the marker, and use `--allow-repeated-repair` only when the new plan explicitly
accounts for the marker.

### Support Bundle Verification Fails

Symptom: support bundle verify reports `missing_artifact`,
`checksum_mismatch`, `extra_file`, or `unsafe_path`.

Action: regenerate the bundle from the same cass data directory and verify the
new manifest. Do not edit manifests by hand.

## E2E Evidence

The doctor v2 scripted runner is the source for reproducible journey artifacts:

```bash
scripts/e2e/doctor_v2.sh list --json
scripts/e2e/doctor_v2.sh describe <scenario_id> --json
scripts/e2e/doctor_v2.sh run <scenario_id> --json --artifact-dir /tmp/cass-doctor-e2e
```

Each run writes an artifact directory with `run-summary.json`, command stdout and
stderr, JSON snapshots, file-tree diffs, checksums, receipts, failure contexts,
and rerun commands. Any artifact path shown in docs should either come from one
of those fixture runs or be clearly marked illustrative.

## Stop Conditions

Stop and inspect manually when:

- a plan would reduce coverage
- `source_authority` is ambiguous
- sole-copy warnings are present and no verified export or backup exists
- a target path is inside a source path, symlinked unexpectedly, or not absolute
- a plan fingerprint does not match the inspected dry-run
- restore rehearsal fails
- post-repair probes fail
- support-bundle verification fails
- a command suggests deleting, hand-removing, or overwriting archive evidence

The safe default is to keep every artifact and gather a support bundle.
