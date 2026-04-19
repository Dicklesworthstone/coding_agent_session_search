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


## Prepare-Phase Optimization Cycle

### Goal
- Eliminate the remaining `~24s` serialized prepare cost in the canonical-only `--force-rebuild` path without changing the final lexical checkpoint contract.

### Measured Rounds
1. Replaced the combined `MAX(id)` fingerprint query with `ORDER BY id DESC LIMIT 1` subqueries. No measurable startup win.
2. Added prep-stage instrumentation around DB-state construction. This proved path normalization was effectively free.
3. Split the lexical content fingerprint into separate `conversations` and `messages` queries. Result: `conversations` took `~0.8s`, while `messages` consumed the remaining `~40s` startup stall.
4. Accepted code change: canonical-only fresh-start rebuilds now defer the expensive initial `messages` fingerprint instead of blocking startup on it, while still persisting the exact final `content-v1` fingerprint synthesized from the streamed rebuild observations.
5. Verified the change with a fresh release-build full-corpus benchmark on the real canonical DB.

### Code Changes Kept
- `src/indexer/mod.rs`
  - Added a deferred-startup fingerprint mode for canonical-only fresh-start rebuilds.
  - Fresh-start rebuilds now skip the blocking initial `messages` high-water fingerprint query during prepare.
  - The completed lexical checkpoint still lands with the exact `content-v1:{total_conversations}:{max_conversation_id}:{max_message_id}` fingerprint by deriving the max IDs from the authoritative streamed rebuild itself.
  - Added a regression test proving the deferred-startup path still persists the exact completed fingerprint.

### Result

| Label | Change | Wall s | Conv/s | Msg/s | DB MiB/s | Avg Proc CPU % | Peak RSS GiB | Outcome |
|---|---|---:|---:|---:|---:|---:|---:|---|
| `r16-deferred-startup-fingerprint` | defer initial canonical-only force-rebuild content fingerprint; persist exact completed fingerprint from streamed observations | 71.645 | 714.429 | 65654.627 | 298.129 | 1085.203 | 20.888 | accepted |

### Phase Breakdown

| Label | Prepare s | Index Window s | Post-Index Tail s |
|---|---:|---:|---:|
| `r16-deferred-startup-fingerprint` | 0.600 | 66.047 | 1.397 |

### Takeaways
- The prepare-phase bottleneck was real and specific: the `messages` side of the lexical content fingerprint was consuming roughly `40s` before indexing even began.
- Deferring that work on the explicit fresh-start canonical-only rebuild path collapsed prepare time from about `24.0s` to about `0.6s` in the real release benchmark.
- Overall wall clock improved from the previous accepted best `75.557s` to `71.645s` (`~5.2%` faster overall on the same corpus and harness).
- The final completed checkpoint contract was preserved. The optimization changes startup behavior, not the settled lexical checkpoint semantics.

### Artifacts
- `/tmp/cass-real-bench-20260417-r16-deferred-startup-fingerprint/logs/summary.json`
- `/tmp/cass-real-bench-20260417-r16-deferred-startup-fingerprint/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260417-r16-deferred-startup-fingerprint/logs/index.stdout.json`


## Profile-Guided Commit-Cadence Optimization Cycle

### Goal
- Use explicit rebuild-stage telemetry to find the dominant remaining service center in the canonical-only force-rebuild path, then retune that lever without regressing correctness.

### Alien-Artifact Framing
- Treat the streamed lexical rebuild as a tandem queue. Measure each service center directly before tuning thresholds.
- Use cliff detection rather than monotonicity assumptions: a larger checkpoint interval should help until segment merge or writer flush costs cross a threshold, then it will sharply regress.

### Code Changes Kept
- `src/indexer/mod.rs`
  - Added opt-in rebuild-stage telemetry behind `CASS_TANTIVY_REBUILD_PROFILE`.
  - The profiler records flush count, commit count, heartbeat persists, and cumulative prepare/add/commit/progress durations, then emits a single `CASS_REBUILD_PROFILE ...` summary line on stderr.
  - Raised the steady-state lexical rebuild commit threshold from `200_000` messages to `800_000` messages.
  - Raised the initial lexical rebuild message threshold from `50_000` to `800_000` messages because the initial slice is already bounded by conversation and byte ceilings.
  - Reworked the streamed conversation closure lifetime into a lexical scope so strict clippy stays clean without any artificial `drop(...)` hack.
  - Updated the interval tests to match the new accepted default.

### Measured Rounds

| Label | Change | Wall s | Conv/s | Msg/s | DB MiB/s | Commits | Commit ms | Outcome |
|---|---|---:|---:|---:|---:|---:|---:|---|
| `r17-profile-baseline` | profiling on; prior code-default commit cadence | 77.712 | 658.652 | 60528.875 | 274.853 | 25 | 13798.996 | baseline |
| `r18-commit400k` | env override `CASS_TANTIVY_REBUILD_COMMIT_EVERY_MESSAGES=400000` | 71.674 | 714.140 | 65628.100 | 298.008 | 13 | 7991.407 | improved |
| `r19-commit800k` | env override `CASS_TANTIVY_REBUILD_COMMIT_EVERY_MESSAGES=800000` | 70.748 | 723.482 | 66486.597 | 301.906 | 8 | 6857.534 | improved |
| `r20-commit1200k` | env override `CASS_TANTIVY_REBUILD_COMMIT_EVERY_MESSAGES=1200000` | 95.824 | 534.159 | 49088.192 | 222.903 | 7 | 25784.196 | rejected cliff |
| `r21-commit800k-initial800k` | env override `800000` for steady and initial message thresholds | 68.648 | 745.620 | 68521.021 | 311.144 | 8 | 6866.053 | best env-only |
| `r22-commit800k-initmsg800k-initconv10k` | also raise initial conversation threshold to `10000` | 69.642 | 734.977 | 67543.018 | 306.703 | 8 | 7310.640 | rejected |
| `r23-default800k` | code-default `800000` steady and initial message thresholds, no env override | 69.650 | 734.890 | 67535.035 | 306.667 | 8 | 6845.780 | accepted |

### Accepted Profile Breakdown

| Label | Flushes | Heartbeat Persists | Prepare ms | Add ms | Commit ms | Pending Progress ms | Heartbeat Progress ms | Checkpoint Persist ms | Meta Fingerprint ms |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `r23-default800k` | 105 | 21 | 10674.415 | 10474.053 | 6845.780 | 28.714 | 46.044 | 11.613 | 0.355 |

### Takeaways
- The new profiler made the next lever obvious: commit fences were still the biggest remaining serialized service center after the earlier prepare/startup fixes.
- Raising the commit cadence from the old default to `800k` messages cut commit overhead from `~13.8s` in the profiled baseline to `~6.85s` in the accepted code-default run.
- There is a real cliff. Pushing to `1.2M` messages looked attractive on paper but detonated commit cost to `~25.8s` and regressed wall clock badly.
- The accepted code-default run improved wall clock from the previous accepted best `71.645s` to `69.650s` (`~2.8%` faster overall).
- Relative to this cycle's measured profiled baseline, the accepted code-default run improved wall clock from `77.712s` to `69.650s` (`~10.4%` faster).
- On the accepted run, the remaining measured service centers are still substantial: prepare `~10.7s`, add `~10.5s`, commit `~6.85s`.
- The next plausible frontier is no longer commit cadence. It is the unmeasured stream-assembly path between ordered DB rows and prepared Tantivy batches, plus any remaining per-batch preparation overhead hidden inside the `prepare` bucket.

### Artifacts
- `/tmp/cass-real-bench-20260418-r17-profile-baseline/logs/summary.json`
- `/tmp/cass-real-bench-20260418-r17-profile-baseline/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260418-r18-commit400k/logs/summary.json`
- `/tmp/cass-real-bench-20260418-r18-commit400k/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260418-r19-commit800k/logs/summary.json`
- `/tmp/cass-real-bench-20260418-r19-commit800k/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260418-r20-commit1200k/logs/summary.json`
- `/tmp/cass-real-bench-20260418-r20-commit1200k/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260418-r21-commit800k-initial800k/logs/summary.json`
- `/tmp/cass-real-bench-20260418-r21-commit800k-initial800k/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260418-r22-commit800k-initmsg800k-initconv10k/logs/summary.json`
- `/tmp/cass-real-bench-20260418-r22-commit800k-initmsg800k-initconv10k/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260418-r23-default800k/logs/summary.json`
- `/tmp/cass-real-bench-20260418-r23-default800k/logs/index.stderr.log`



## Debug Current-Tree Queue-Geometry Rejection Cycle

### Goal
- Re-test the suspected stream-assembly bottleneck on the live corpus using a current-tree debug binary fetched from a warm remote worker, then separate real wins from deceptive mid-run progress improvements.

### Environment Notes
- These rounds used `/tmp/cass-remote-debug`, a current-tree debug binary built remotely on `ts2` and copied back with `scp`.
- A full optimized remote build was also started, but it remained in cold dependency compilation long enough that it stopped being the critical path for this cycle.
- Because these are debug-binary measurements, use them to rank local hypotheses, not to replace the accepted release-profile history above.

### Measured Rounds

| Label | Change | Wall s | Conv/s | Msg/s | DB MiB/s | Outcome |
|---|---|---:|---:|---:|---:|---|
| `r31-batch256-debug` | env override `CASS_TANTIVY_REBUILD_BATCH_FETCH_CONVERSATIONS=256` | 486.567 | 105.196 | 9666.912 | 44.074 | baseline for outer-batch sweep |
| `r32-batch384-debug` | env override `CASS_TANTIVY_REBUILD_BATCH_FETCH_CONVERSATIONS=384` | 481.607 | 106.280 | 9766.487 | 44.528 | best in initial debug outer-batch sweep |
| `r33-batch512-debug` | env override `CASS_TANTIVY_REBUILD_BATCH_FETCH_CONVERSATIONS=512` | 486.576 | 105.194 | 9666.740 | 44.073 | rejected |
| `r35-patched-default-debug` | experimental shard-flatten removal in streamed rebuild path; default queue settings | 488.574 | 104.764 | 9627.619 | 43.718 | rejected |
| `r36-patched-batch384-debug` | shard-flatten removal plus `batch_fetch_conversations=384` | 497.716 | 102.840 | 9450.773 | 42.915 | rejected |
| `r37-inner8192-32m-debug` | reverted code path; smaller inner Tantivy add batches via `CASS_TANTIVY_ADD_BATCH_MAX_MESSAGES=8192` and `CASS_TANTIVY_ADD_BATCH_MAX_CHARS=33554432` | 496.674 | 103.055 | 9470.603 | 43.005 | rejected |

### Profile Highlights

| Label | Flushes | Commits | Prepare ms | Add ms | Commit ms | Outcome |
|---|---:|---:|---:|---:|---:|---|
| `r31-batch256-debug` | 205 | 8 | 43024.856 | 41193.042 | 71299.504 | baseline |
| `r32-batch384-debug` | 140 | 8 | 42326.307 | 40683.233 | 69395.433 | best initial sweep |
| `r33-batch512-debug` | 105 | 8 | 39905.467 | 38428.934 | 73655.906 | rejected |
| `r35-patched-default-debug` | 105 | 8 | 39479.486 | 38680.342 | 73528.180 | rejected |
| `r36-patched-batch384-debug` | 140 | 8 | 42291.806 | 41356.877 | 69321.667 | rejected |
| `r37-inner8192-32m-debug` | 105 | 8 | 54022.954 | 53226.469 | 66625.334 | rejected |

### Takeaways
- The outer-batch sweep alone was real but small: `384` conversations per outer chunk beat `256` and `512`, but only by about `1.0%` in this debug matrix. That is not the kind of leverage that justifies large code churn by itself.
- The streamed shard-flatten removal was a deceptive non-win. It made some mid-run progress windows look faster, but end-to-end wall clock stayed flat or regressed. The change was reverted.
- Shrinking the inner Tantivy add-batch amplitude reduced peak RSS slightly and trimmed commit time, but it exploded prepare/add overhead badly enough to lose overall.
- The long flat spots in progress are still present after these queue-geometry variants. That keeps the dominant suspicion on Tantivy-side commit/segment behavior, not on one extra user-space flatten or on simply making batches smaller.
- The best result in this cycle remains `r32-batch384-debug`, and even that is only a modest improvement over the surrounding debug variants. No new code-default optimization was accepted from this cycle.

### Artifacts
- `/tmp/cass-real-bench-20260418-r31-batch256-debug/logs/summary.json`
- `/tmp/cass-real-bench-20260418-r31-batch256-debug/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260418-r32-batch384-debug/logs/summary.json`
- `/tmp/cass-real-bench-20260418-r32-batch384-debug/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260418-r33-batch512-debug/logs/summary.json`
- `/tmp/cass-real-bench-20260418-r33-batch512-debug/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260418-r35-patched-default-debug/logs/summary.json`
- `/tmp/cass-real-bench-20260418-r35-patched-default-debug/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260418-r36-patched-batch384-debug/logs/summary.json`
- `/tmp/cass-real-bench-20260418-r36-patched-batch384-debug/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260418-r37-inner8192-32m-debug/logs/summary.json`
- `/tmp/cass-real-bench-20260418-r37-inner8192-32m-debug/logs/index.stderr.log`


## Local-Override Lexical Writer Control And Rejection Cycle

### Goal
- Re-check the real lexical writer bottleneck with a clean, optimized profiling binary and use that tighter control to test the remaining high-EV levers: merge suppression, writer-thread count, relaxed early commit fencing, and the one outer batch size that had shown a weak positive hint.

### Environment Notes
- `Cargo.toml` already pins `frankensearch` rev `8e07d082`, and the local checkout used for the temporary override was at the same `HEAD`.
- The local override was only used to make it easy to benchmark a temporary `no_merge` experiment and to build a fresh profiling binary on `ts2`.
- The accepted control run below is therefore behaviorally equivalent to the current effective dependency path. The temporary `no_merge` knob was later reverted from the sibling `frankensearch` checkout.

### Measured Rounds

| Label | Change | Wall s | Conv/s | Msg/s | DB MiB/s | Flushes | Commits | Prepare ms | Add ms | Commit ms | Outcome |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| `r38-localfs-default-debug` | local override debug control | 503.834 | 101.591 | 9336.013 | 42.394 | 105 | 8 | 40665.826 | 39869.950 | 75391.760 | baseline for override cycle |
| `r39-localfs-no-merge-debug` | temporary `CASS_TANTIVY_DISABLE_BULK_LOAD_MERGES=1` | 515.105 | 99.368 | 9131.745 | 41.466 | 105 | 8 | 40923.727 | 40124.853 | 81082.569 | rejected |
| `r40-localfs-w16-debug` | `CASS_TANTIVY_MAX_WRITER_THREADS=16` | 580.127 | 88.231 | 8108.235 | 36.818 | 105 | 8 | 52056.020 | 51258.220 | 144175.618 | rejected hard |
| `r41-localfs-default-profiling` | fresh optimized profiling control | 63.582 | 805.025 | 73980.243 | 335.934 | 105 | 8 | 7494.415 | 7221.018 | 7186.305 | accepted fresh best |
| `r42-localfs-w20-profiling` | `CASS_TANTIVY_MAX_WRITER_THREADS=20` | 67.632 | 756.814 | 69549.805 | 315.816 | 105 | 8 | 9002.267 | 8701.574 | 7136.817 | rejected |
| `r43-localfs-w32-profiling` | `CASS_TANTIVY_MAX_WRITER_THREADS=32` | 68.693 | 745.131 | 68476.142 | 310.941 | 105 | 8 | 8079.636 | 7770.994 | 7171.754 | rejected |
| `r44-localfs-commit1m-profiling` | relax initial fence and raise message commit target to `1_000_000` | 67.637 | 756.760 | 69544.827 | 315.793 | 104 | 6 | 7848.833 | 7564.444 | 8795.677 | rejected |
| `r45-localfs-batch384-profiling` | `CASS_TANTIVY_REBUILD_BATCH_FETCH_CONVERSATIONS=384` | 66.635 | 768.136 | 70590.234 | 320.540 | 140 | 8 | 8218.176 | 7856.156 | 7337.777 | rejected |

### Accepted Control Breakdown

| Label | Flushes | Heartbeat Persists | Prepare ms | Add ms | Commit ms | Pending Progress ms | Heartbeat Progress ms | Checkpoint Persist ms | Meta Fingerprint ms |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `r41-localfs-default-profiling` | 105 | 21 | 7494.415 | 7221.018 | 7186.305 | 24.703 | 50.732 | 13.898 | 0.367 |

### Takeaways
- The strongest new datum in this cycle is the fresh optimized control itself: `r41-localfs-default-profiling` completed in `63.582s`, materially faster than the prior documented best `69.650s` (`~8.7%` faster).
- Suppressing bulk-load merges entirely was a clean loser. `r39` increased commit cost from `75.392s` to `81.083s` in the debug matrix, so the problem is not “too many merges, just turn them off.”
- Lowering writer threads was also a loser. `r42` and especially `r40` showed that reducing writer parallelism hurt `prepare` and `add` much more than it helped `commit`.
- Raising writer threads to `32` was also worse than the control. `r43` kept commit cost roughly flat while regressing both `prepare` and `add`, which implies the current default writer-thread cap is already close to the local optimum.
- Relaxing the initial restartability fence did reduce commit count (`8 -> 6` in `r44`), but it still lost overall because per-commit cost rose sharply and the larger slices made `prepare`/`add` worse. “Fewer commits” is not the objective function by itself.
- Re-testing the old outer-batch `384` hint on the optimized control binary also lost. It increased flush count from `105` to `140` and regressed wall clock despite a superficially good debug hint earlier.
- No new code-default optimization was accepted from this cycle. The only durable artifact kept in this repo is the benchmark history update documenting the new control and the rejected levers around it.

### Artifacts
- `/tmp/cass-real-bench-20260418-r38-localfs-default-debug/logs/summary.json`
- `/tmp/cass-real-bench-20260418-r38-localfs-default-debug/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260418-r39-localfs-no-merge-debug/logs/summary.json`
- `/tmp/cass-real-bench-20260418-r39-localfs-no-merge-debug/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260418-r40-localfs-w16-debug/logs/summary.json`
- `/tmp/cass-real-bench-20260418-r40-localfs-w16-debug/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260418-r41-localfs-default-profiling/logs/summary.json`
- `/tmp/cass-real-bench-20260418-r41-localfs-default-profiling/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260418-r42-localfs-w20-profiling/logs/summary.json`
- `/tmp/cass-real-bench-20260418-r42-localfs-w20-profiling/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260418-r43-localfs-w32-profiling/logs/summary.json`
- `/tmp/cass-real-bench-20260418-r43-localfs-w32-profiling/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260418-r44-localfs-commit1m-profiling/logs/summary.json`
- `/tmp/cass-real-bench-20260418-r44-localfs-commit1m-profiling/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260418-r45-localfs-batch384-profiling/logs/summary.json`
- `/tmp/cass-real-bench-20260418-r45-localfs-batch384-profiling/logs/index.stderr.log`

## Streamed Rebuild Queueing Experiments: Rejected

### Goal
- Test whether removing the outer flattened-doc materialization, and then overlapping prepare with add via a bounded ordered pipeline, could reduce the effective `prepare + add` portion of lexical rebuild without hurting commit behavior.

### Measured Rounds

| Label | Change | Wall s | Conv/s | Msg/s | DB MiB/s | Prepare ms | Add ms | Commit ms | Outcome |
|---|---|---:|---:|---:|---:|---:|---:|---:|---|
| `r46-localdebug-pipeline` | shard streaming plus bounded ordered prepare→add pipeline, debug | 499.811 | 102.409 | 9411.169 | 42.735 | 917.130 | 39825.243 | 73981.459 | rejected |
| `r47-profiling-pipeline` | same bounded ordered pipeline, optimized profiling build | 63.582 | 805.020 | 73979.837 | 335.932 | 382.818 | 7386.208 | 7066.819 | rejected as benchmark-neutral |
| `r48-profiling-shardstream` | shard streaming only, no pipeline, optimized profiling build | 63.587 | 804.962 | 73974.492 | 335.908 | 407.439 | 7414.252 | 7057.664 | rejected as slightly slower |

### Takeaways
- The bounded ordered pipeline did exactly what the internal counters said it would do: it nearly eliminated standalone `prepare_ms`. That did **not** translate into end-to-end wall-clock improvement on the real corpus.
- On the trusted profiling run, the pipeline path (`r47`) ended effectively tied with the standing best control (`r41-localfs-default-profiling` at `63.582s`). The saved prepare time simply reappeared inside the broader add/consumer critical path.
- The debug run for the same pipeline path (`r46`) was a clearer loser at `499.811s` versus the prior nearby debug point `481.607s`, confirming that the more complex queueing path was not a robust improvement.
- The simpler shard-streaming-only variant (`r48`) was also not good enough. It landed at `63.587s`, fractionally slower than the standing best control, so the extra hot-path complexity was not justified.
- Final decision from this cycle: revert both experiments. The repo should not keep either the bounded ordered pipeline or the shard-streaming-only hot-path change based on this evidence.

### Artifacts
- `/tmp/cass-real-bench-20260418-r46-localdebug-pipeline/logs/summary.json`
- `/tmp/cass-real-bench-20260418-r46-localdebug-pipeline/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260418-r47-profiling-pipeline/logs/summary.json`
- `/tmp/cass-real-bench-20260418-r47-profiling-pipeline/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260418-r48-profiling-shardstream/logs/summary.json`
- `/tmp/cass-real-bench-20260418-r48-profiling-shardstream/logs/index.stderr.log`


## Rebuild Scan-Path Breakdown And Grouped Message Streaming

### Goal
- Measure the previously unaccounted lexical rebuild wall time inside the post-`ready_to_index` scan path, then attack the real hotspot instead of continuing speculative queue-shape experiments.

### Measured Rounds

| Label | Change | Wall s | Conv/s | Msg/s | DB MiB/s | Total ms | Conversation List ms | Message Stream ms | Finish Conversation ms | Prepare ms | Add ms | Commit ms | Outcome |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| `r49-prep-scan-control` | control run with new internal prep + rebuild breakdown enabled | 64.604 | 792.289 | 72809.885 | 330.620 | 60512.503 | 309.320 | 58307.003 | 14969.626 | 7635.771 | 7350.405 | 6382.053 | diagnostic baseline |
| `r50-profile-scan-breakdown` | same control on current-tree profiling binary with retained scan timers | 64.597 | 792.379 | 72818.079 | 330.657 | 60512.503 | 309.320 | 58307.003 | 14969.626 | 8202.398 | 7016.905 | 7201.437 | confirms hotspot location |
| `r51-grouped-message-stream` | storage streams one callback per conversation instead of one per message row | 63.565 | 805.234 | 73999.505 | 336.021 | 60219.208 | 315.846 | 58084.419 | 15053.749 | 8180.393 | 7029.454 | 7339.476 | accepted |
| `r52-grouped-plus-move` | consume preloaded conversation rows via iterator instead of cloning each envelope | 63.574 | 805.131 | 73990.004 | 335.978 | 59920.119 | 318.789 | 57818.331 | 14710.652 | 7948.957 | 6828.904 | 7198.409 | retained, effectively tied |
| `r53-grouped-plus-move-repeat` | repeat of retained current tree | 63.588 | 804.948 | 73973.232 | 335.902 | 59714.698 | 312.742 | 57637.359 | 14639.930 | 7908.375 | 6825.523 | 7055.011 | repeat confirms tie-stable behavior |

### Takeaways
- The earlier external “prepare” bucket was misleading. Internal prep before `ready_to_index` is only about `0.4s`; the real fixed cost was the scan path after startup.
- The dominant hotspot was not conversation listing. `conversation_list_ms` stayed around `0.31s`, so materializing the conversation envelope vector was never the main problem.
- The real cost center was the message scan/merge window itself: roughly `58.3s` on the control run. That made the highest-EV lever clear: cut outer per-message callback and merge overhead.
- Grouped message streaming did that. `r51` improved wall clock from `64.597s` to `63.565s`, about `1.6%` faster, while also increasing throughput to about `74.0k` messages/sec.
- The follow-on move-based conversation iterator was basically wall-clock neutral versus grouped-only, but it did reduce the internal scan and finish counters slightly on both retained samples. The current tree keeps it because it removes pointless conversation-envelope clone churn without evidence of regression.
- The core scan path is still the frontier. Even after the accepted grouped-stream change, `message_stream_ms` is still about `57.6-58.1s`, which remains much larger than `prepare + add + commit`.

### Artifacts
- `/tmp/cass-real-bench-20260418-r49-prep-scan-control/logs/summary.json`
- `/tmp/cass-real-bench-20260418-r49-prep-scan-control/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260418-r50-profile-scan-breakdown/logs/summary.json`
- `/tmp/cass-real-bench-20260418-r50-profile-scan-breakdown/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260418-r51-grouped-message-stream/logs/summary.json`
- `/tmp/cass-real-bench-20260418-r51-grouped-message-stream/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260418-r52-grouped-plus-move/logs/summary.json`
- `/tmp/cass-real-bench-20260418-r52-grouped-plus-move/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260418-r53-grouped-plus-move-repeat/logs/summary.json`
- `/tmp/cass-real-bench-20260418-r53-grouped-plus-move-repeat/logs/index.stderr.log`

## Grouped Stream Late-Materialization Sweep

### Goal
- Continue attacking the measured `message_stream_ms` hotspot inside the authoritative canonical-DB lexical rebuild by stripping per-row work that the grouped scan path did not actually need.

### Measured Rounds

| Label | Change | Wall s | Conv/s | Msg/s | DB MiB/s | Total ms | Conversation List ms | Message Stream ms | Finish Conversation ms | Prepare ms | Add ms | Commit ms | Outcome |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| `r55-precompute-message-bytes` | precompute message-byte totals during grouped scan and pass them into `finish_conversation` | 64.628 | 791.992 | 72782.587 | 330.496 | 60521.117 | 296.913 | 58495.152 | 15302.675 | 8567.230 | 7448.500 | 7058.829 | rejected |
| `r56-grouped-null-author` | grouped scan projects `NULL AS author` instead of decoding unused author text | 63.593 | 804.885 | 73967.407 | 335.876 | 60051.317 | 311.745 | 58010.494 | 14689.677 | 8109.158 | 6959.775 | 6846.188 | effectively neutral |
| `r57-grouped-lite-row` | grouped scan emits a lite row (`idx`, `created_at`, `content`, tool-bit) plus per-conversation last message id | 62.586 | 817.830 | 75157.021 | 341.278 | 58814.138 | 307.244 | 56816.733 | 14415.829 | 7660.826 | 6557.957 | 7170.979 | accepted fresh best |

### Takeaways
- Moving the message-byte summation earlier was a clean loser. `r55` regressed wall clock and worsened every important internal bucket (`message_stream`, `prepare`, and `add`). That lever was reverted.
- Simply pruning grouped `author` decode was too small to matter by itself. `r56` landed essentially tied with the retained grouped-stream baseline, so it was only useful as a proof that narrow projection pruning in this area is behaviorally safe.
- The stronger late-materialization lever did pay off. `r57` cut wall time to `62.586s`, improving on the prior retained `r53` repeat (`63.588s`) by about `1.6%`.
- The accepted win is visible in the right internal buckets: `message_stream_ms` dropped from `57637.359` on `r53` to `56816.733` on `r57`, while `prepare_ms` and `add_ms` also improved materially.
- The canonical corpus currently has zero `tool` rows (`SELECT count(*) FROM messages WHERE role='tool'` returned `0`), which made it especially clear that carrying full role strings through this grouped rebuild path was unnecessary overhead on this workload.
- After `r57`, the dominant remaining cost is still the raw grouped scan and content transfer itself. The next frontier is likely the unavoidable `content` payload movement, not another tiny metadata field.

### Artifacts
- `/tmp/cass-real-bench-20260418-r55-precompute-message-bytes/logs/summary.json`
- `/tmp/cass-real-bench-20260418-r55-precompute-message-bytes/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260418-r56-grouped-null-author/logs/summary.json`
- `/tmp/cass-real-bench-20260418-r56-grouped-null-author/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260418-r57-grouped-lite-row/logs/summary.json`
- `/tmp/cass-real-bench-20260418-r57-grouped-lite-row/logs/index.stderr.log`


## Corrected Runner Harness + Micro-Lever Sweep

### Goal
- Remove the full-CLI link step from the optimization loop without changing the authoritative indexer code path, then re-test two micro-levers on the real canonical corpus with a same-harness control.

### Harness Notes
- A tiny `/tmp/cass_runner_r59` wrapper was compiled directly against the freshly built `coding_agent_search` profiling rlib with `panic=abort`.
- The first wrapper attempt was invalid because it omitted the normal `IndexingProgress`, which forced a slow final lexical fingerprint refresh (`fingerprint_messages step_ms=12887`). That measurement is retained only as a harness-debug artifact and is not used for decision-making.
- The corrected runner uses `IndexingProgress::default()` so it matches the normal `cass index --force-rebuild` fast path closely enough for same-harness comparisons.

### Measured Rounds

| Label | Change | Wall s | Conv/s | Msg/s | DB MiB/s | Rebuild ms | Message Stream ms | Prepare ms | Add ms | Commit ms | Outcome |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| `r58-exact-flatten` | replace flattened-doc `collect()` with exact-capacity append | 64.589 | 792.466 | 72826.152 | 330.693 | 60.486 | 58.328 | 7.623 | 6.658 | 7.269 | rejected |
| `r59-grouped-reserve64-invalid` | fixed grouped-message reserve of `64` on the first wrapper harness | 72.479 | 706.203 | 64898.708 | 294.696 | 54.819 | 52.781 | 7.982 | 6.842 | 7.139 | invalid harness; wrapper paid final fingerprint tail |
| `r60-grouped-reserve64-corrected` | same reserve `64`, corrected wrapper harness | 59.431 | 861.247 | 79146.927 | 359.395 | 55.692 | 53.624 | 8.040 | 6.829 | 7.118 | tied / not a real win |
| `r61-noise-fast-reject` | skip lowercase work for messages already rejected by the existing `>200` ack cutoff | 59.421 | 861.390 | 79160.104 | 359.455 | 55.829 | 53.800 | 8.011 | 6.809 | 7.030 | effectively neutral, kept as harmless cleanup |
| `r62-corrected-control-noreserve` | corrected wrapper harness control with reserve removed, fast-reject still present | 59.436 | 861.184 | 79141.168 | 359.369 | 55.589 | 53.561 | 7.918 | 6.873 | 6.940 | control for reserve verdict |

### Takeaways
- The exact-capacity flattened-doc append looked plausible but was a clean loser on the real corpus and was reverted.
- The first wrapper harness run surfaced an important control-plane issue: omitting `IndexingProgress` from the standalone runner disabled the existing fast-path that skips final lexical checkpoint refresh, which injected a bogus `~12.9s` fingerprint tail into wall clock.
- Once the harness was corrected, the grouped reserve experiment collapsed to noise. `r60` and `r62` are effectively identical, so the reserve change was reverted.
- The long-message fast-reject before lowercase is also basically noise on end-to-end wall time, but it is behavior-preserving and directionally sensible because it avoids useless lowercase allocation for messages that the existing rule would reject immediately anyway.
- The corrected same-harness plateau is now about `59.42-59.44s`, which is materially faster than the older `62.586s` CLI-binary result, but this cycle should still be treated mainly as a harness-corrected micro-lever sweep rather than a big architectural breakthrough.

### Artifacts
- `/tmp/cass-real-bench-20260418-r58-exact-flatten/logs/summary.json`
- `/tmp/cass-real-bench-20260418-r58-exact-flatten/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260418-r59-grouped-reserve64/logs/summary.json`
- `/tmp/cass-real-bench-20260418-r59-grouped-reserve64/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260418-r60-grouped-reserve64-corrected/logs/summary.json`
- `/tmp/cass-real-bench-20260418-r60-grouped-reserve64-corrected/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260418-r61-noise-fast-reject/logs/summary.json`
- `/tmp/cass-real-bench-20260418-r61-noise-fast-reject/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260418-r62-corrected-control-noreserve/logs/summary.json`
- `/tmp/cass-real-bench-20260418-r62-corrected-control-noreserve/logs/index.stderr.log`


## SmallVec Grouped-Stream Capacity Sweep

### Goal
- Attack the still-dominant grouped message-stream hot path by removing heap-first allocation for the common case: many conversations are small, so the grouped row buffer should start stack-first and spill only when needed.

### Alien/Queueing Framing
- This is a small-object allocation optimization, not a batching change. The queueing model stays the same; only the per-conversation buffer representation changes.
- With median conversation size `38`, p90 `194`, p95 `379`, and p99 `1029`, a spillable inline buffer should capture a large fraction of conversations without paying the struct-bloat cost of a very large inline capacity.

### Measured Rounds

| Label | Change | Wall s | Conv/s | Msg/s | DB MiB/s | Rebuild ms | Message Stream ms | Prepare ms | Add ms | Commit ms | Outcome |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| `r63-smallvec48` | grouped message rows stored in `SmallVec<[row; 48]>` | 58.430 | 876.000 | 80502.742 | 365.552 | 55.271 | 53.163 | 8.014 | 6.876 | 7.103 | accepted candidate |
| `r64-smallvec64` | widen inline capacity to `64` | 58.418 | 876.187 | 80519.918 | 365.630 | 54.386 | 52.313 | 7.739 | 6.597 | 6.970 | plateau / tied |
| `r65-smallvec32` | shrink inline capacity to `32` | 58.417 | 876.194 | 80520.544 | 365.633 | 54.456 | 52.312 | 7.832 | 6.650 | 6.911 | plateau / retained best-by-size |
| `r66-smallvec16` | shrink inline capacity further to `16` | 60.457 | 846.629 | 77803.579 | 353.295 | 56.311 | 54.086 | 8.065 | 6.869 | 7.008 | rejected |

### Takeaways
- `SmallVec` itself is the real win. Moving from the old heap-first grouped buffer path (`r62` corrected control at `59.436s`) to the stack-first path cut wall time by about `1.0s`, roughly `1.7%`.
- The gain is visible in the right service center: `message_stream_ms` fell from `53665.752` on the corrected no-`SmallVec` control to about `52312-53163` on the `SmallVec` variants, with `prepare`, `add`, and `commit` also trending down.
- The inline-capacity sweep found a flat optimum band rather than a sharp point. `32`, `48`, and `64` all beat the old control; `32` and `64` are essentially tied on wall time.
- Pushing the inline buffer below that band breaks the win. `r66-smallvec16` regressed sharply to `60.457s`, so the common-case stack capture needs more than `16` inline slots on this corpus.
- The retained setting should therefore bias toward the smaller inline footprint on the plateau. `32` keeps the common-case stack win while minimizing per-conversation struct bulk inside pending batches.

### Artifacts
- `/tmp/cass-real-bench-20260418-r63-smallvec48/logs/summary.json`
- `/tmp/cass-real-bench-20260418-r63-smallvec48/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260418-r64-smallvec64/logs/summary.json`
- `/tmp/cass-real-bench-20260418-r64-smallvec64/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260418-r65-smallvec32/logs/summary.json`
- `/tmp/cass-real-bench-20260418-r65-smallvec32/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260418-r66-smallvec16/logs/summary.json`
- `/tmp/cass-real-bench-20260418-r66-smallvec16/logs/index.stderr.log`

## Rejected Byte-Pass Fusion Follow-Up

### Goal
- Test a pass-fusion idea from the grouped lexical rebuild hot path: carry per-conversation `message_bytes` forward from the SQLite grouping scan so `finish_conversation` no longer walks every grouped message again just to total `content.len()`.

### Alien/Graveyard Framing
- This was a straight pass-fusion experiment: eliminate a redundant per-conversation walk and keep the observable rebuild behavior identical.
- The measured hotspot evidence for trying it was the still-large `finish_conversation_ms` bucket inside the corrected runner harness.

### Measured Round

| Label | Change | Wall s | Conv/s | Msg/s | DB MiB/s | Rebuild ms | Message Stream ms | Finish Conversation ms | Prepare ms | Add ms | Commit ms | Outcome |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| `r67-bytepass-fused` | compute grouped `message_bytes` during SQLite scan and pass them into `finish_conversation` | 59.445 | 861.053 | 79129.163 | 359.315 | 55.490 | 53.390 | 14.889 | 7.970 | 6.813 | 7.229 | rejected |

### Takeaways
- The idea was clean but it was not a win. `r67` lost by about `1.0s` against the retained `r65-smallvec32` baseline (`58.417s`).
- The internal counters did not justify keeping it either: `message_stream_ms` rose back to `53390.355`, far above the retained `52311.599` on `r65`.
- That means the extra byte-summing pass in `finish_conversation` is not the binding constraint here. The real remaining cost is still deeper in the grouped scan / lexical ingest path, not this tiny per-conversation fold.

### Artifacts
- `/tmp/cass-real-bench-20260418-r67-bytepass-fused/logs/summary.json`
- `/tmp/cass-real-bench-20260418-r67-bytepass-fused/logs/index.stderr.log`

## High-Thread Tantivy Add-Batch Sweep

### Goal
- Re-test the outer Tantivy add-batch geometry on the corrected same-harness runner now that the retained `SmallVec<[row; 32]>` path is in place and the machine was materially less loaded than during the older plateau measurements.

### Alien/Queueing Framing
- This was a queue/service tuning pass, not a semantics change. The hypothesis was that the default outer `add_prebuilt_documents` batch cap was underfeeding the lexical writer on the 26-thread path.
- Because batching changes are notoriously noisy, this sweep used a sequential-validation rule: do not keep a candidate unless the improvement survives a repeat or a code-default reproduction.

### Measured Rounds

| Label | Change | Wall s | Conv/s | Msg/s | DB MiB/s | Rebuild ms | Message Stream ms | Prepare ms | Add ms | Commit ms | Outcome |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| `r68-control-repeat` | current retained tree, no add-batch override | 57.413 | 891.515 | 81928.571 | 372.026 | 53.953 | 51.905 | 7.745 | 6.619 | 6.946 | fresh control |
| `r69-addbatch16384` | `CASS_TANTIVY_ADD_BATCH_MAX_MESSAGES=16384` | 57.422 | 891.378 | 81915.930 | 371.969 | 53.991 | 52.158 | 7.372 | 6.255 | 6.392 | effectively tied |
| `r70-addbatch24576` | `CASS_TANTIVY_ADD_BATCH_MAX_MESSAGES=24576` | 56.412 | 907.342 | 83382.974 | 378.630 | 53.344 | 51.305 | 6.474 | 5.336 | 6.843 | promising outlier |
| `r71-addbatch32768` | `CASS_TANTIVY_ADD_BATCH_MAX_MESSAGES=32768` | 78.505 | 651.998 | 59917.336 | 272.076 | 53.027 | 50.791 | 5.967 | 4.842 | 7.496 | hard regression / cliff |
| `r73-code-default-addbatch24576` | compiled code default changed to `24576` on high-thread path, no env override | 57.425 | 891.330 | 81911.507 | 371.949 | not separately inspected | not separately inspected | not separately inspected | not separately inspected | not separately inspected | failed reproduction |
| `r74-addbatch24576-repeat` | repeat of the `24576` env override after the code-default non-repro | 57.418 | 891.451 | 81922.692 | 372.000 | not separately inspected | not separately inspected | not separately inspected | not separately inspected | not separately inspected | failed reproduction |

### Takeaways
- The new clean-machine control (`r68`) was already materially faster than the older `58.417s` plateau, so this whole sweep had to be judged against `57.413s`, not against stale noisier runs.
- `16384` was just noise. It improved some internal `prepare`/`add` counters but did not move wall clock.
- `24576` produced one excellent run (`56.412s`) with much better internal `prepare_ms` and `add_ms`, but it failed both validation checks: the compiled code-default run (`r73`) and a straight env repeat (`r74`) fell back to `~57.42s`.
- `32768` exposed a real cliff: the internal rebuild profile still looked superficially fine, but end-to-end wall time exploded to `78.505s`, which strongly suggests large outer batches can trigger delayed downstream costs outside the profiled rebuild buckets.
- Conclusion: do not keep any add-batch default change from this sweep. The `24576` result was a false positive under sequential validation, not a stable optimization.

### Artifacts
- `/tmp/cass-real-bench-20260419-r68-control-repeat/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r68-control-repeat/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260419-r69-addbatch16384/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r69-addbatch16384/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260419-r70-addbatch24576/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r70-addbatch24576/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260419-r71-addbatch32768/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r71-addbatch32768/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260419-r73-code-default-addbatch24576/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r73-code-default-addbatch24576/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260419-r74-addbatch24576-repeat/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r74-addbatch24576-repeat/logs/index.stderr.log`



## Rejected Prepare-Chunking Sweep

### Goal
- Test a morsel-style prepare-stage rewrite in the lexical rebuild hot path: replace one `Vec<CassDocument>` allocation per conversation with chunked worker-local accumulation and a single final extend pass.

### Alien/Queueing Framing
- This was a queue-shape and allocation-churn experiment inspired by morsel-driven parallelism, not a semantic change. Ordering was intentionally preserved by keeping contiguous conversation chunks, preserving in-chunk order, and flattening chunk outputs in chunk order.
- The EV case for trying it was the still-material `prepare_ms` bucket on the fresh corrected control (`r68`), which suggested that per-conversation allocation and flatten overhead might still be worth attacking locally.

### Measured Rounds

| Label | Change | Wall s | Conv/s | Msg/s | DB MiB/s | Rebuild ms | Message Stream ms | Finish Conversation ms | Prepare ms | Add ms | Commit ms | Outcome |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| `r75-chunked-prepare` | chunked worker-local doc accumulation in `prepare_lexical_rebuild_batch` | 57.416 | 891.473 | 81924.629 | 372.008 | 53.954 | 51.845 | 14.185 | 7.649 | 6.665 | 6.928 | tie / rejected |
| `r76-chunked-prepare-repeat` | repeat of the same chunked worker-local prepare path | 57.420 | 891.407 | 81918.571 | 371.981 | 53.926 | 51.913 | 14.376 | 7.754 | 6.748 | 6.976 | failed repeat |

### Takeaways
- This lever did not earn its complexity. Both measured rounds were fractionally slower than the fresh retained control (`r68-control-repeat` at `57.413s`).
- `r75` briefly looked directionally interesting because `prepare_ms` fell from the control's `7.745s` to `7.649s`, but the savings were too small and were mostly given back in `add_ms`; end-to-end wall time did not move.
- The repeat removed any doubt. `r76` drifted the wrong way in every interesting internal bucket: `message_stream_ms`, `finish_conversation_ms`, `prepare_ms`, `add_ms`, and `commit_ms` all worsened versus `r75`.
- Conclusion: revert the chunked prepare rewrite. The simpler per-conversation prepare path remains the correct retained implementation until there is evidence for a materially larger prepare-stage lever.

### Artifacts
- `/tmp/cass-real-bench-20260419-r75-chunked-prepare/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r75-chunked-prepare/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260419-r76-chunked-prepare-repeat/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r76-chunked-prepare-repeat/logs/index.stderr.log`


## Accepted Slice-Based Prebuilt-Doc Add Fast Path

### Goal
- Remove the extra wrapper-side buffering pass in the lexical rebuild add path. The rebuild already prepares a full `Vec<CassDocument>` per flush, but `TantivyIndex::add_prebuilt_documents` was moving every document into a second staging `Vec` before borrowing it back into `frankensearch`'s slice API.

### Alien/Queueing Framing
- This is a zero-copy ownership-transfer style optimization at the wrapper boundary, not a batching-policy change. The batch geometry stays the same; the change is that the rebuild path now submits contiguous slices of the already-prepared document vector instead of rebuilding a second buffer just to recover those same contiguous slices.
- The relevant graveyard pattern is ownership-preserving zero-copy handoff: keep the hot path on existing contiguous buffers and avoid gratuitous queue copies when the downstream consumer already accepts borrowed slices.

### Behavior Preservation Proof
- Ordering preserved: yes. Batches are contiguous windows over the original prepared-doc vector, and those windows are submitted in original order.
- Tie-breaking unchanged: yes. No sort keys or batch thresholds changed.
- Floating-point: N/A.
- RNG seeds: unchanged / N/A.
- Golden/replay verification: compile gates green plus targeted lexical-add and streamed-rebuild tests green.

### Measured Rounds

| Label | Change | Wall s | Conv/s | Msg/s | DB MiB/s | Rebuild ms | Message Stream ms | Finish Conversation ms | Prepare ms | Add ms | Commit ms | Outcome |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| `r77-slice-add-fastpath` | rebuild path calls new slice-based prebuilt-doc fast path; no second staging vec | 55.395 | 924.002 | 84914.020 | 385.583 | 52.342 | 50.421 | 13.213 | 5.813 | 4.714 | 6.492 | accepted candidate |
| `r78-slice-add-fastpath-repeat` | repeat of the same retained slice-based fast path | 56.409 | 907.398 | 83388.187 | 378.654 | 52.607 | 50.535 | 13.249 | 5.675 | 4.551 | 6.792 | accepted repeat |

### Takeaways
- This is a real retained win. Both rounds beat the fresh retained control (`r68-control-repeat` at `57.413s`).
- The repeat held with a clear margin: `57.413s -> 56.409s`, about `1.7%` faster, while the first round reached `55.395s`.
- The internal profile moved in the right service centers. Against `r68`, the repeat cut `prepare_ms` from `7.745s` to `5.675s` and `add_ms` from `6.619s` to `4.551s`, with `message_stream_ms` also dropping by about `1.37s`.
- That matches the hypothesis: the wrapper-side rebuffering pass was expensive enough to matter on the real corpus, and deleting it improved both the handoff into frankensearch and the apparent upstream critical path.

### Artifacts
- `/tmp/cass-real-bench-20260419-r77-slice-add-fastpath/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r77-slice-add-fastpath/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260419-r78-slice-add-fastpath-repeat/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r78-slice-add-fastpath-repeat/logs/index.stderr.log`


## Accepted Prebuilt Add-Batch Default Retune

### Goal
- Re-evaluate the lexical rebuild add-batch default after the accepted slice-based prebuilt-doc fast path changed the hot-path balance. The earlier add-batch sweep was taken on an older tree, so its conclusions were no longer trustworthy for the retained slice fast path.

### Alien/Queueing Framing
- This is a queue geometry retune, but only at the prebuilt-doc boundary where the retained slice fast path now hands already-prepared contiguous documents directly into Tantivy/frankensearch.
- The relevant queueing rule is local retuning after a service-center change: once the wrapper-side copy stage was removed, the old batch-size optimum became stale and needed to be re-measured on the new pipeline rather than inherited by folklore.
- Scope stays intentionally narrow. Regular per-message indexing keeps the older default; only the prebuilt lexical rebuild slice path gets the raised floor.

### Behavior Preservation Proof
- Ordering preserved: yes. Only batch window size changed; document order within and across batches is unchanged.
- Tie-breaking unchanged: yes. Search fields, batch submission order, and commit cadence all stay the same.
- Floating-point: N/A.
- RNG seeds: unchanged / N/A.
- Golden/replay verification: compile gates green plus targeted lexical-add and streamed-rebuild tests green.

### Measured Rounds

| Label | Change | Wall s | Conv/s | Msg/s | Rebuild ms | Message Stream ms | Finish Conversation ms | Prepare ms | Add ms | Commit ms | Outcome |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| `r79-control-after-slice` | fresh retained control on the slice fast-path tree; code default unchanged | 56.402 | 907.501 | 83397.578 | 52.663 | 50.546 | 13.286 | 5.667 | 4.540 | 6.758 | control |
| `r80-addbatch16384-after-slice` | env-only retune with `CASS_TANTIVY_ADD_BATCH_MAX_MESSAGES=16384` on the retained slice tree | 55.404 | 923.855 | 84900.539 | 52.122 | 50.258 | 12.945 | 5.300 | 4.155 | 6.608 | promising |
| `r81-addbatch16384-repeat-after-slice` | repeat of the same env-only `16384` retune | 55.404 | 923.858 | 84900.830 | 51.903 | 50.078 | 12.591 | 5.331 | 4.200 | 6.158 | accepted env repeat |
| `r82-code-default-prebuilt16384` | code default raised to `16384` for the prebuilt-doc slice path only | 56.409 | 907.395 | 83387.895 | 52.736 | 50.745 | 13.169 | 5.402 | 4.191 | 6.888 | noisy miss / inconclusive |
| `r83-code-default-prebuilt16384-repeat` | no-env repeat of the retained code-default prebuilt-only `16384` change | 55.384 | 924.187 | 84931.058 | 51.884 | 50.180 | 12.799 | 5.387 | 4.242 | 6.269 | accepted |

### Takeaways
- The old add-batch conclusions were stale once the slice fast path removed the wrapper-side rebuffering pass. Re-measuring on the retained tree found a different stable point.
- `16384` is the new retained default floor for the prebuilt lexical rebuild slice path. The accepted code-default repeat (`r83`) improved the fresh retained control from `56.402s` to `55.384s`, about `1.8%` faster.
- The internal profile moved in the right places. Against `r79`, the accepted `r83` cut `prepare_ms` from `5.667s` to `5.387s`, `add_ms` from `4.540s` to `4.242s`, `commit_ms` from `6.758s` to `6.269s`, and `finish_conversation_ms` from `13.286s` to `12.799s`.
- `r82` is deliberately kept in the history even though it lost, because a single no-env miss after two strong env repeats was not enough evidence to throw away the lever. The second no-env repeat settled the question.
- Scope discipline mattered. The retained code raises the default only for prebuilt rebuild batches; the regular per-message indexing path stays untouched because it was not the measured winner.

### Artifacts
- `/tmp/cass-real-bench-20260419-r79-control-after-slice/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r79-control-after-slice/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260419-r80-addbatch16384-after-slice/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r80-addbatch16384-after-slice/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260419-r81-addbatch16384-repeat-after-slice/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r81-addbatch16384-repeat-after-slice/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260419-r82-code-default-prebuilt16384/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r82-code-default-prebuilt16384/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260419-r83-code-default-prebuilt16384-repeat/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r83-code-default-prebuilt16384-repeat/logs/index.stderr.log`
