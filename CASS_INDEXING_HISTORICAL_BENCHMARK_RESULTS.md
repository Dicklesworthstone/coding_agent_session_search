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


## Follow-up Cycle — Prepare/Shutdown Focus

### Goal
- Focus the next optimization pass on the serialized pre-index and post-index work around the streamed rebuild, then keep only changes that survive real full-corpus benchmarking.

### Fresh-Eyes Fix
- While re-reading the new code, one test bug turned up: `rebuild_tantivy_from_db_resume_reports_total_observed_messages` was asserting a nonexistent `checkpoint.total_messages` field. The test now loads the full lexical rebuild state and asserts `state.db.total_messages` instead.

### Code Changes Kept
- `src/main.rs`: apply a code-level default `CASS_TANTIVY_MAX_WRITER_THREADS=26` when the user has not explicitly configured it.
- `src/search/tantivy.rs`: keep the same `26`-thread fallback in the writer parallelism heuristic so the in-process default and library fallback stay aligned.
- `src/indexer/mod.rs`: keep the fresh-eyes test fix only. A more invasive writer-storage reopen experiment was benchmarked and rejected.

### Follow-up Results

| Label | Change | Wall s | Conv/s | Msg/s | DB MiB/s | Avg Proc CPU % | Peak RSS GiB | Outcome |
|---|---|---:|---:|---:|---:|---:|---:|---|
| `r7-reopenstorage` | close and reopen writer storage around authoritative rebuild | 83.664 | 611.796 | 56222.878 | 255.300 | 1058.284 | 20.597 | rejected |
| `r8-rebaseline` | rebaseline after reverting the reopen experiment | 81.677 | 626.679 | 57590.626 | 261.511 | 1114.162 | 21.572 | baseline |
| `r9-writer24` | env override `CASS_TANTIVY_MAX_WRITER_THREADS=24` | 78.618 | 651.055 | 59830.755 | 271.683 | 825.335 | 20.522 | improved |
| `r10-writer20` | env override `CASS_TANTIVY_MAX_WRITER_THREADS=20` | 119.056 | 429.925 | 39509.276 | 179.406 | 685.558 | 20.377 | rejected |
| `r11-writer28` | env override `CASS_TANTIVY_MAX_WRITER_THREADS=28` | 76.588 | 668.314 | 61416.743 | 278.885 | 1066.166 | 21.062 | improved |
| `r12-writer30` | env override `CASS_TANTIVY_MAX_WRITER_THREADS=30` | 80.630 | 634.814 | 58338.211 | 264.906 | 831.660 | 21.633 | rejected |
| `r13-writer26` | env override `CASS_TANTIVY_MAX_WRITER_THREADS=26` | 76.574 | 668.438 | 61428.135 | 278.937 | 1038.264 | 21.105 | improved |
| `r14-default26` | code-default `26` writer threads, no env override | 75.557 | 677.440 | 62255.422 | 282.693 | 1031.633 | 21.102 | accepted best |
| `r15-default26-repeat` | repeat of code-default `26` writer threads | 76.561 | 668.550 | 61438.517 | 278.984 | 1027.329 | 20.997 | accepted repeat |

### Follow-up Phase Breakdown

| Label | Prepare s | Index Window s | Post-Index Tail s |
|---|---:|---:|---:|
| `r8-rebaseline` | 24.619 | 52.642 | 0.801 |
| `r9-writer24` | 25.720 | 49.737 | 0.603 |
| `r11-writer28` | 24.317 | 49.636 | 0.100 |
| `r13-writer26` | 24.117 | 49.735 | 0.200 |
| `r14-default26` | 24.016 | 48.735 | 0.300 |
| `r15-default26-repeat` | 24.017 | 49.235 | 0.915 |

### Follow-up Takeaways
- The earlier exact-checkpoint work already collapsed the authoritative rebuild shutdown tail from double-digit seconds to sub-second territory. There was no hidden remaining post-index bug to unlock.
- The reopen-storage experiment looked plausible on paper but was a real regression and should stay reverted.
- The only clean win from this pass was retuning Tantivy writer parallelism. On this machine and corpus, `26` writer threads consistently beat the previous effective `32`-thread default.
- The accepted code-default run improved wall clock from the rebaseline `81.677s` to `75.557s` (`~7.5%` faster in this pass) and from the previous documented best `88.659s` to `75.557s` (`~14.8%` faster overall).
- The dominant remaining fixed cost is still the prepare leg at about `24.0s`. The index-window work is now under `50s`, and the shutdown tail is no longer the problem.
- Even after the latest tuning, the process is still far from saturating a 128-core host. Average process CPU in the accepted runs was about `1030%`, or roughly `10.3` fully busy cores.

### Follow-up Artifacts
- `/tmp/cass-real-bench-20260417-r7-reopenstorage/logs/summary.json`
- `/tmp/cass-real-bench-20260417-r8-rebaseline/logs/summary.json`
- `/tmp/cass-real-bench-20260417-r9-writer24/logs/summary.json`
- `/tmp/cass-real-bench-20260417-r10-writer20/logs/summary.json`
- `/tmp/cass-real-bench-20260417-r11-writer28/logs/summary.json`
- `/tmp/cass-real-bench-20260417-r12-writer30/logs/summary.json`
- `/tmp/cass-real-bench-20260417-r13-writer26/logs/summary.json`
- `/tmp/cass-real-bench-20260417-r14-default26/logs/summary.json`
- `/tmp/cass-real-bench-20260417-r15-default26-repeat/logs/summary.json`
