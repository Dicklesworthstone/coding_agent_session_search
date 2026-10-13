# Migrating a flattened Codex archive to native dated logs

CASS normally preserves directory-scoped Codex external IDs. A filename or
native UUID alone is not permission to merge conversations from different
roots. For the snapshot-to-live transition reported in #524, use the explicit
`cass-codex-layout-migrate` companion command **before indexing the second
layout**. It retains whichever external ID is already indexed, including a
dated ID when the dated layout was indexed first.

This is an opt-in identity registration, not destructive deduplication. It
supports an existing `agent_search.db` in the selected data directory, a named
local source, and two plain JSONL files: one directly below `sessions/`, the
other below `sessions/YYYY/MM/DD/`, with the same native rollout filename.
The two `sessions` directories may be different snapshot/live roots.

## Procedure

Build both binaries from this checkout (the companion is not part of older
published CASS installers):

```sh
cargo build --release --bin cass --bin cass-codex-layout-migrate
```

Pause the watcher and session writer during registration. Keep the original
file accessible until registration succeeds. Configure both roots under one
named local source in `$XDG_CONFIG_HOME/cass/sources.toml` (normally
`~/.config/cass/sources.toml`):

```toml
[[sources]]
name = "full-history-plan"
type = "local"
paths = ["/tmp/cass-identity/flat/sessions", "/tmp/cass-identity/dated/sessions"]
```

Exactly one corresponding Codex conversation must already be indexed. Neither
root may also be claimed by another configured local source. Preview the proof:

```sh
name=rollout-2026-10-13T16-20-00-00000000-0000-4000-8000-000000000001.jsonl
flat="/tmp/cass-identity/flat/sessions/$name"
dated="/tmp/cass-identity/dated/sessions/2026/10/13/$name"
data=/path/to/existing/cass-data

target/release/cass-codex-layout-migrate \
  --data-dir "$data" --source full-history-plan \
  --from "$flat" --to "$dated" --json
```

Review the reported canonical conversation ID, external ID, message count and
paths. Publish the receipt only after the dry run succeeds:

```sh
target/release/cass-codex-layout-migrate \
  --data-dir "$data" --source full-history-plan \
  --from "$flat" --to "$dated" --apply --json

target/release/cass index --watch-once "$dated" --data-dir "$data" --json
```

`--from` and `--to` select the pair; argument order does not choose a new
canonical identity. The existing archive row decides. Use the modified `cass`
binary for subsequent indexing; an older binary does not understand receipts.
Resume the watcher normally. The original snapshot no longer needs to remain
in the active scan roots once registration and indexing succeed. Keep ordinary
backups rather than deleting source data as part of migration.

## Proof and failure behavior

The command takes the same archive writer lease as indexing, opens the database
read-only, checks exact agent/source ownership, and refuses zero or multiple
candidate identities. It compares the stored message projection, including
roles, timestamps, authors, content, extra data and snippets, against parsed
source history. Both raw rollouts must have matching native session metadata;
the filename UUID must match `session_meta.payload.id`. Every overlapping raw
byte must match. A longer destination may contain already-appended messages.

No database rows, message IDs, search artifacts, source files or configuration
entries are rewritten by registration. Dry-run creates no identity receipt;
it may create/open the ordinary permanent archive lock file. `--apply` writes a
bounded, versioned receipt under `codex-layout-identities-v1/` in the data
directory. Receipts contain hashes and identity/path information, not message
bodies. Reapplying an unchanged migration is idempotent.

During later aliased scans, CASS rechecks the exact source, path pair, native
metadata and complete overlapping prefix before delivering the existing
external ID to normal ingestion. A stronger prefix proof is atomically written
**before** delivery. If ingestion fails after partially committing, retrying the
same file is safe; switching to a divergent branch is not accepted. A shorter
known snapshot is permitted without weakening the high-water proof. An unknown
shorter snapshot requires a still-accessible proven longer peer.

Conflicting metadata/content, incomplete JSONL, unsupported directory scope,
missing evidence, source races, corrupt receipts, lock contention and ambiguous
ownership are errors, not permission to merge. Do not delete or hand-edit a
receipt to suppress a conflict; preserve the files and investigate it.

## Boundaries

This command deliberately refuses an archive that already contains both
identities. It does not choose a survivor, delete duplicates, or invalidate
existing document IDs. Such an archive needs a separately reviewed repair;
registration is the supported transition path while one canonical row exists.
It also refuses unverifiable projections rather than assuming a parser-version
change or extra-data difference is harmless.

Custom database paths sharing a data directory, remote/SSH sources, compressed
or legacy JSON rollouts, arbitrary custom directory scopes, and moving the
receipt to another archive are outside this command's supported scope. Retain
the data directory and its receipts together in backups. A full archive rebuild
has its usual ID-reallocation semantics and is not an identity-preserving
migration.

## Regression coverage and validation commands

Unit regressions exercise bounded proofs, both directions, appends, metadata
and body conflicts, source/agent isolation, missing old copies, corruption,
locking and write-ahead retries. The CLI suite uses the reporter's separate
roots and named local source in streaming and non-streaming modes. It checks
conversation/message counts and IDs, production-encoded semantic document IDs,
lexical search counts, shorter-copy replay, repeated registration/indexing,
already-extended destinations, deliberate conflicts, existing duplicates and
archive writer exclusion. Encoding semantic IDs is not a model-backed semantic
search test.

```sh
cargo test --lib connectors::codex::layout_identity::tests
cargo test --test codex_layout_migration
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

These Rust commands were not executed in the implementation environment, which
had no Rust toolchain or package-network access. The committed tests are
regression specifications pending native execution, not claimed passing runs.
