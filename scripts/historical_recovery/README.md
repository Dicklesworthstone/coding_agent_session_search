# Historical Recovery Scripts

One-off Bash/Python salvage helpers for reconstructing the canonical `cass` session database on this machine without pushing more recovery logic through Rust first.

These scripts are for:

- inventorying large numbers of historical SQLite bundles
- classifying which bundles are directly readable vs damaged
- recovering damaged bundles with SQLite-native tooling
- extracting only canonical session tables rather than treating FTS or Tantivy data as authoritative

## Scripts

`inventory_sqlite_sources.py`

- Walk one or more roots and classify likely SQLite/session bundles
- Records file size, sidecars, header state, schema readability, core table presence, and row counts when readable
- Default output is JSONL so it can be piped into `jq`, `sqlite-utils`, or further Python processing

`recover_historical_bundle.py`

- Runs `sqlite3 .recover` against a damaged bundle
- Filters the stream down to canonical session tables only
- Imports the filtered recovery stream into a fresh SQLite database with just the core schema

`merge_historical_bundle.py`

- Reconciles one historical cass SQLite bundle into a canonical DB
- Preserves existing canonical rows on same-idx conflicts
- Only touches core tables (`sources`, `agents`, `workspaces`, `conversations`, `messages`, `snippets`)

`import_codex_rollouts.py`

- Scans raw Codex rollout files under `~/.codex/sessions`
- Reconciles both missing sessions and already-known sessions that have grown more messages on disk
- Uses `state_5.sqlite` only as metadata fallback for workspace/title/timestamps when the rollout file is incomplete

`run_watch_once_batches.py`

- Drives native `cass index --watch-once` over large raw session trees in resumable batches
- Keeps all parsing/insertion logic inside `cass`; the Python only chunks paths and records progress
- Learns a per-root batch size automatically by growing on safe headroom and shrinking on OOM or high RSS
- Treats stderr-side `watch reindex failed` diagnostics as hard failures even if an older `cass` binary exits `0`
- Persists per-root/per-pattern state so Claude, Codex, Gemini, and backup roots can all resume independently
- Unsets `CASS_INDEXER_BEGIN_CONCURRENT*` by default so historical recovery stays on the safer serial writer path; pass `--allow-begin-concurrent` only if you explicitly want to benchmark that mode

## Typical usage

Inventory the main `cass` data directory:

```bash
python3 scripts/historical_recovery/inventory_sqlite_sources.py \
  --root /home/ubuntu/.local/share/coding-agent-search
```

Recover one damaged bundle into a clean staging DB:

```bash
python3 scripts/historical_recovery/recover_historical_bundle.py \
  /home/ubuntu/.local/share/coding-agent-search/agent_search.corrupt.20260324_212907 \
  /tmp/agent_search.corrupt.recovered.db
```

Merge one readable historical bundle into the clean canonical clone:

```bash
python3 scripts/historical_recovery/merge_historical_bundle.py \
  /home/ubuntu/.local/share/coding-agent-search/agent_search.db.backup.253251.1774560539243940632.0 \
  --canonical-db /home/ubuntu/.local/share/coding-agent-search/repair-lab/agent_search.canonical_dumpclone_1774564525.db
```

Reconcile raw Codex rollout sessions into the clean canonical clone:

```bash
python3 scripts/historical_recovery/import_codex_rollouts.py \
  --canonical-db /home/ubuntu/.local/share/coding-agent-search/repair-lab/agent_search.canonical_dumpclone_1774564525.db \
  --sessions-root /home/ubuntu/.codex/sessions \
  --state-db /home/ubuntu/.codex/state_5.sqlite
```

Reconcile a large Gemini raw-session tree through native `cass` in resumable batches:

```bash
python3 scripts/historical_recovery/run_watch_once_batches.py \
  --data-dir /home/ubuntu/.local/share/coding-agent-search/repair-lab/reconcile_from_native_full_20260328 \
  --root /home/ubuntu/.gemini/tmp \
  --pattern '**/session-*.json' \
  --batch-size 32
```

Save the filtered SQL stream too:

```bash
python3 scripts/historical_recovery/recover_historical_bundle.py \
  /path/to/bundle.db \
  /tmp/recovered.db \
  --filtered-sql /tmp/recovered.filtered.sql
```

## Notes

- These scripts do not delete or overwrite source bundles.
- Recovered DBs should be treated as staging inputs for later dedup/merge work.
- The source of truth should remain the final canonical `agent_search.db`, rebuilt only after staged salvage is complete.
- For large recovery passes, leave BEGIN CONCURRENT disabled unless you have already proven that specific DB + workload is stable with `--allow-begin-concurrent`.
