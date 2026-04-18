# CASS Indexing Historical Benchmark Results

## Corpus
- Date: 2026-04-17
- Canonical DB: `/home/ubuntu/.local/share/coding-agent-search/agent_search.db`
- Conversations: `51,185`
- Messages: `4,703,804`
- DB size: `22,396,870,656` bytes (`~20.86 GiB`)
- Message content bytes: `1,938,100,433` bytes (`~1.80 GiB`)
- Benchmark harness: `/tmp/cass_real_index_benchmark.py`
- Benchmark command shape:

```bash
cass --db /home/ubuntu/.local/share/coding-agent-search/agent_search.db   index --json --force-rebuild --data-dir <fresh-temp-dir>
```

## Notes
- All runs below used a fresh temporary `--data-dir`, so they measure a full lexical Tantivy rebuild from the canonical SQLite DB.
- `--force-rebuild` on an already-populated canonical DB intentionally takes the canonical-only rebuild path.
- The machine is shared, so CPU availability varies somewhat across runs. Repeated runs are recorded explicitly instead of relying on a single sample.
- The later code-default runs used the source-built release binary at `/data/projects/coding_agent_session_search/target-optbench/release/cass`.

## Results

| Label | Code State | Wall s | Conv/s | Msg/s | DB MB/s | Avg Proc CPU % | Peak RSS GiB |
|---|---|---:|---:|---:|---:|---:|---:|
| `opt3-frankensearch8e07` | pinned `frankensearch` baseline after earlier rebuild-streaming fixes | 99.820 | 512.774 | 47122.951 | 213.979 | 778.495 | 20.804 |
| `opt3-frankensearch8e07-singleopen` | single-open current-schema fast path | 93.718 | 546.161 | 50191.139 | 227.911 | 743.708 | 20.782 |
| `opt3-frankensearch8e07-singleopen-addbatch16k` | single-open plus larger Tantivy add batches via env (`16384` messages / `64 MiB`) | 88.668 | 577.267 | 53049.762 | 240.892 | 990.834 | 21.579 |
| `opt3-frankensearch8e07-singleopen-batchconv1024` | single-open plus larger outer conversation batch (`1024`) | 96.741 | 529.093 | 48622.639 | 220.789 | 910.474 | 21.011 |
| `opt3-frankensearch8e07-singleopen-codedefault-addbatch` | single-open plus code-default parallelism-aware Tantivy add batches | 88.659 | 577.322 | 53054.771 | 240.914 | 994.472 | 21.592 |
| `opt3-frankensearch8e07-singleopen-codedefault-addbatch-override32k` | code-default run plus `32768` messages / `128 MiB` override | 88.662 | 577.304 | 53053.151 | 240.907 | 1009.430 | 21.959 |

## Phase Timing Breakdown

### `opt3-frankensearch8e07`
- preparing -> indexing: `24.718s`
- indexing start -> `current=51185`: `56.041s`
- phase reset after indexing: `82.160s`
- completed payload emitted: `97.071s`
- observed shutdown tail after phase reset: `14.911s`

### `opt3-frankensearch8e07-singleopen`
- preparing -> indexing: `23.917s`
- indexing start -> `current=51185`: `52.036s`
- phase reset after indexing: `76.754s`
- completed payload emitted: `91.148s`
- observed shutdown tail after phase reset: `14.394s`

### `opt3-frankensearch8e07-singleopen-addbatch16k`
- preparing -> indexing: `24.117s`
- indexing start -> `current=51185`: `46.033s`
- phase reset after indexing: `72.051s`
- completed payload emitted: `86.062s`
- observed shutdown tail after phase reset: `14.011s`

### `opt3-frankensearch8e07-singleopen-batchconv1024`
- preparing -> indexing: `24.017s`
- indexing start -> `current=51185`: `54.039s`
- phase reset after indexing: `80.057s`
- completed payload emitted: `94.143s`
- observed shutdown tail after phase reset: `14.086s`

### `opt3-frankensearch8e07-singleopen-codedefault-addbatch`
- preparing -> indexing: `24.216s`
- indexing start -> `current=51185`: `46.034s`
- phase reset after indexing: `72.352s`
- completed payload emitted: `86.062s`
- observed shutdown tail after phase reset: `13.710s`

### `opt3-frankensearch8e07-singleopen-codedefault-addbatch-override32k`
- preparing -> indexing: `24.116s`
- indexing start -> `current=51185`: `46.033s`
- phase reset after indexing: `72.350s`
- completed payload emitted: `86.143s`
- observed shutdown tail after phase reset: `13.793s`

## Takeaways
- The single-open storage fast path delivered a real win over the pinned baseline: `99.820s -> 93.718s` (`~6.1%` faster wall clock).
- The bigger Tantivy add-batch lever delivered the next real win: `93.718s -> 88.659s` (`~5.4%` faster wall clock) once promoted from env-only tuning into code defaults.
- The net improvement across this optimization cycle is `99.820s -> 88.659s` (`~11.2%` faster wall clock).
- Enlarging the outer conversation batch to `1024` was a regression and should not be kept.
- Pushing the Tantivy add-batch ceiling even higher (`32768` messages / `128 MiB`) produced no meaningful speedup and increased memory, so the smaller code-default setting is the better default.
- Even at the current best run, the process is still not saturating the machine. Average process CPU was about `994%`, which is only about `9.9` fully busy cores on average.
- The remaining dominant fixed costs are still the pre-index prepare phase (`~24.2s`) and the post-index shutdown tail (`~13.7s`).

## Artifacts
- `/tmp/cass-real-bench-20260417-opt3-frankensearch8e07/logs/summary.json`
- `/tmp/cass-real-bench-20260417-opt3-frankensearch8e07-singleopen/logs/summary.json`
- `/tmp/cass-real-bench-20260417-opt3-frankensearch8e07-singleopen-addbatch16k/logs/summary.json`
- `/tmp/cass-real-bench-20260417-opt3-frankensearch8e07-singleopen-batchconv1024/logs/summary.json`
- `/tmp/cass-real-bench-20260417-opt3-frankensearch8e07-singleopen-codedefault-addbatch/logs/summary.json`
- `/tmp/cass-real-bench-20260417-opt3-frankensearch8e07-singleopen-codedefault-addbatch-override32k/logs/summary.json`
