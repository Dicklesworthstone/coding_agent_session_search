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


## Rejected Frankensearch Internal Add-Plan Batch Sweep

### Goal
- Re-test the inner `frankensearch` parallel-add chunk geometry after the retained slice fast path and prebuilt `16384` outer-batch retune changed the upstream handoff shape.

### Alien/Queueing Framing
- This is a second-stage queueing probe: the retained outer batch now hands the writer a different workload, so the internal `cass_parallel_add_target_batch_docs` constant inside `frankensearch` might have become stale even if the outer retune held.
- The relevant graveyard rule is coupled service-center retuning: once an upstream boundary changes, a downstream chunking heuristic must be revalidated rather than assumed.

### Behavior Preservation Proof
- Ordering preserved: yes. Only internal chunk geometry changed via env overrides.
- Tie-breaking unchanged: yes. No schema, sort, or query behavior changed.
- Floating-point: N/A.
- RNG seeds: unchanged / N/A.
- Golden/replay verification: env-only benchmark sweep; retained source tree unchanged.

### Measured Rounds

| Label | Change | Wall s | Conv/s | Msg/s | Rebuild ms | Message Stream ms | Finish Conversation ms | Prepare ms | Add ms | Commit ms | Outcome |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| `r84-paraddocs1024` | env-only `CASS_TANTIVY_PARALLEL_ADD_BATCH_DOCS=1024` | 58.291 | 878.092 | 80695.006 | 55.035 | 53.135 | 14.644 | 7.427 | 6.303 | 6.211 | rejected |
| `r85-paraddocs256` | env-only `CASS_TANTIVY_PARALLEL_ADD_BATCH_DOCS=256` | 57.080 | 896.723 | 82407.160 | 53.697 | 51.770 | 12.930 | 4.737 | 3.583 | 7.101 | rejected |

### Takeaways
- The whole inner-add batch-doc sweep is a non-winner on the retained tree. `1024` was a hard regression, and `256` also lost despite looking directionally nicer in `prepare_ms` and `add_ms`.
- The `256` round is especially informative: it cut `prepare_ms` and `add_ms` substantially, but the savings came back as a much worse `commit_ms` bill. That is exactly the kind of coupled-queue trap that makes isolated thermostat tuning unreliable.
- Conclusion: keep the default `512` internal add-plan target inside `frankensearch`. The retained tree is better served by a structural lever than by further inner-batch folklore.

### Artifacts
- `/tmp/cass-real-bench-20260419-r84-paraddocs1024/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r84-paraddocs1024/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260419-r85-paraddocs256/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r85-paraddocs256/logs/index.stderr.log`


## Accepted Borrowed Prebuilt-Doc Refs via Local Frankensearch Override

### Goal
- Remove the intermediate owned `CassDocument` layer from the lexical rebuild path. The grouped rebuild batch already owns the conversation rows and message strings, so cloning them into `CassDocument` and then cloning again into Tantivy documents was redundant.

### Alien/Queueing Framing
- This is an ownership-transport optimization using the exact pinned `frankensearch` rev already declared by cass (`8e07d082`). The local patch override only changes source resolution so the sibling checkout can expose a borrowed-doc API; it does not switch cass to a different upstream revision.
- The relevant graveyard pattern is ownership-preserving zero-copy handoff plus local-to-global queue repair. The winning shape was not merely “borrowed refs”; it was “borrowed refs while preserving the old parallel shard prep geometry.”
- That distinction mattered. The first serial borrowed-ref prototype (`r86`) regressed badly because it deleted clone work and parallel fanout at the same time. Restoring parallel shard construction on borrowed refs produced the actual win.

### Behavior Preservation Proof
- Ordering preserved: yes. Each borrowed ref is emitted in the same `(conversation_id, idx)` order as the old owned-doc path, and the slice batching logic is unchanged.
- Tie-breaking unchanged: yes. Same schema fields, same commit cadence, same batch boundaries, same search semantics.
- Floating-point: N/A.
- RNG seeds: unchanged / N/A.
- Golden/replay verification: compile gates green plus targeted lexical-add and streamed-rebuild tests green.

### Measured Rounds

| Label | Change | Wall s | Conv/s | Msg/s | Rebuild ms | Message Stream ms | Finish Conversation ms | Prepare ms | Add ms | Commit ms | Outcome |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| `r86-borrowed-docrefs` | first borrowed-ref prototype with serial ref construction | 59.469 | 860.706 | 79097.240 | 56.175 | 54.111 | 13.858 | 6.145 | 5.087 | 7.232 | rejected prototype |
| `r87-borrowed-docrefs-parallel` | borrowed refs plus restored parallel shard construction | 55.258 | 926.288 | 85124.121 | 52.357 | 50.513 | 12.007 | 4.985 | 4.324 | 6.558 | accepted candidate |
| `r88-borrowed-docrefs-parallel-repeat` | repeat of the retained parallel borrowed-ref path | 55.200 | 927.264 | 85213.782 | 52.372 | 50.456 | 12.301 | 4.840 | 4.189 | 7.194 | accepted repeat |

### Takeaways
- This is a real but modest retained win. The repeat held against the previous retained baseline (`r83-code-default-prebuilt16384-repeat` at `55.384s`): `55.384s -> 55.200s`, about `0.3%` faster.
- The improvement is small enough that the failed `r86` prototype matters. Without the parallel shard construction, the borrowed-ref idea was decisively wrong. With parallel shard construction restored, the clone-elimination becomes net positive.
- The repeat profile supports the story. Against `r83`, `r88` cut `prepare_ms` from `5.387s` to `4.840s` and `finish_conversation_ms` from `12.799s` to `12.301s`, while `add_ms` stayed effectively tied (`4.242s -> 4.189s`). `commit_ms` drifted up, which is why the total win stayed small.
- Conclusion: keep the borrowed prebuilt-doc ref path and the local `frankensearch` override, but describe it honestly as a narrow structural win rather than a breakthrough.

### Artifacts
- `/tmp/cass-real-bench-20260419-r86-borrowed-docrefs/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r86-borrowed-docrefs/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260419-r87-borrowed-docrefs-parallel/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r87-borrowed-docrefs-parallel/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260419-r88-borrowed-docrefs-parallel-repeat/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r88-borrowed-docrefs-parallel-repeat/logs/index.stderr.log`


## Rejected Edge-Ngram and Preview Micro-Optimizations

### Goal
- Probe two remaining per-document string hot paths inside `frankensearch` lexical ingest after the retained borrowed-ref handoff win: edge-ngram generation and preview construction.

### Alien/Queueing Framing
- Both ideas targeted the same measured symptom: residual per-message transform cost after ownership transport had already been tightened.
- The graveyard lesson here is that local micro-allocation reductions can still lose globally once branch behavior, UTF-8 scanning shape, and downstream writer overlap are accounted for. “Fewer obvious allocations” is not itself a proof of lower end-to-end service time.

### Behavior Preservation Proof
- Ordering preserved: yes. Both experiments only rewrote pure string-preparation helpers.
- Tie-breaking unchanged: yes. No query, schema, commit, or batch-boundary changes.
- Floating-point: N/A.
- RNG seeds: unchanged / N/A.
- Golden/replay verification: helper behavior pinned with direct unit tests; real-corpus benchmark rejected both runtime changes.

### Measured Rounds

| Label | Change | Wall s | Conv/s | Msg/s | Outcome |
|---|---|---:|---:|---:|---|
| `r89-edgegrams-streaming` | remove per-word edge-ngram index `Vec` and stream prefixes directly | 56.400 | 907.535 | 83400.780 | rejected |
| `r90-preview-slice` | build previews from one bounded UTF-8 slice instead of char-by-char pushes | 57.250 | 894.061 | 82162.515 | rejected |

### Takeaways
- Both ideas lost cleanly against the retained baseline `r88-borrowed-docrefs-parallel-repeat = 55.200s`.
- `r89` shows the classic micro-optimization trap: deleting a small heap allocation inside a tight loop changed the local work shape, but the real corpus still got slower end-to-end.
- `r90` was even worse. The slice-based preview path looked cheaper on paper, but on the real workload it produced the slowest result of the pass.
- Conclusion: keep the previous retained runtime path unchanged. The remaining high-EV frontier is still deeper lexical ingest structure, not isolated helper rewrites of already-bounded string transforms.

### Artifacts
- `/tmp/cass-real-bench-20260418-r89-edgegrams-streaming/logs/summary.json`
- `/tmp/cass-real-bench-20260418-r90-preview-slice/logs/summary.json`


## Rejected SmallVec Doc-Ref Shard Buffer

### Goal
- Test whether per-conversation `CassDocumentRef` shard construction in the retained borrowed-ref rebuild path was still paying enough heap-allocation tax to justify an inline buffer.

### Alien/Queueing Framing
- This was a local buffer-shape probe on the post-`r88` path: each conversation still builds a temporary doc-ref shard before the batch is flattened and handed to Tantivy.
- The expected value case was simple and valid: previous `SmallVec` use in the storage scan path had already produced one real win, so it was reasonable to re-test the same primitive at the next queue boundary.

### Behavior Preservation Proof
- Ordering preserved: yes. Only the temporary per-conversation shard container changed.
- Tie-breaking unchanged: yes. Same conversation order, message order, batch boundaries, and lexical fields.
- Floating-point: N/A.
- RNG seeds: unchanged / N/A.
- Golden/replay verification: compile-gated before the benchmark; runtime change rejected on the real corpus and reverted.

### Measured Round

| Label | Change | Wall s | Conv/s | Msg/s | Outcome |
|---|---|---:|---:|---:|---|
| `r91-docref-smallvec16` | `SmallVec<[CassDocumentRef; 16]>` for per-conversation borrowed-ref shards | 57.270 | 893.749 | 82133.822 | rejected |

### Takeaways
- This was not a marginal loss; it was a clear regression against the retained baseline `r88 = 55.200s`.
- The result reinforces the current frontier diagnosis: once the big ownership-transport win landed, more local container tweaking in the rebuild-prep layer stopped paying for itself.
- Conclusion: keep plain `Vec` for the temporary doc-ref shards and continue looking deeper than local buffer microstructure.

### Artifacts
- `/tmp/cass-real-bench-20260418-r91-docref-smallvec16/logs/summary.json`

## Rejected Streamed Message-Byte Carry

### Goal
- Test whether the grouped lexical rebuild stream could cheaply carry per-conversation `message_bytes` forward and avoid the second `messages.iter().map(|m| m.content.len()).sum()` pass inside `finish_conversation`.

### Alien/Queueing Framing
- This was a one-pass sufficient-statistics probe on the dominant grouped stream path.
- The hypothesis was orthodox and high-EV: the storage scan already touches every row, so it should be cheaper to accumulate byte totals there than to walk every grouped message slice again on the indexer side.

### Behavior Preservation Proof
- Ordering preserved: yes. Row order, conversation boundaries, and batch/commit decisions were unchanged.
- Tie-breaking unchanged: yes. Only an extra aggregate was threaded through the existing callback contract.
- Floating-point: N/A.
- RNG seeds: unchanged / N/A.
- Golden/replay verification: compile-gated before benchmark; runtime change rejected on the real corpus and reverted.

### Measured Round

| Label | Change | Wall s | Conv/s | Msg/s | `message_stream_ms` | `finish_conversation_ms` | `prepare_ms` | `add_ms` | `commit_ms` | Outcome |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---|
| `r92-streamed-message-bytes` | carry grouped `message_bytes` from SQLite scan into `finish_conversation` | 59.475 | 860.612 | 79088.625 | 53.779 | 13.497 | 5.911 | 5.198 | 7.216 | rejected |

### Takeaways
- This did not merely fail to help wall clock; it regressed the local target bucket too. `finish_conversation_ms` worsened from the retained `r88` value of `12.301s` to `13.497s`.
- `prepare_ms` and `add_ms` also moved in the wrong direction, so the extra callback plumbing and per-row accumulation cost more than the removed second pass.
- Conclusion: keep the simpler existing callback contract and continue hunting deeper than this local sufficient-statistics tweak.

### Artifacts
- `/tmp/cass-real-bench-20260418-r92-streamed-message-bytes/logs/summary.json`
- `/tmp/cass-real-bench-20260418-r92-streamed-message-bytes/logs/index.stderr.log`



## Clean 048d5fa8 Baseline Reset, Rejected Repin, and Retained Edge-Ngram Helper Reversal

### Goal
- Re-establish the true control on the current clean tree, then test whether the faster older `cass_generate_edge_ngrams` helper was a real lever or just another edge-ngram false positive.

### Alien/Queueing Framing
- This pass corrected the control plane before changing the data plane. The repo had drifted back to the clean git-pinned `frankensearch` `048d5fa8` state, so the old `r88` local-override number was no longer the right baseline.
- The graveyard lesson was the same constants-over-asymptotics warning that already showed up in the rejected `r89` streaming rewrite: fewer obvious temporaries does not guarantee a lower service-time envelope. Here, the older per-word index-vector helper won decisively once measured against the true clean control.

### Behavior Preservation Proof
- Ordering preserved: yes. The retained change only swaps the `frankensearch` source from the pinned git checkout to the local sibling checkout containing the older `cass_generate_edge_ngrams` helper; document order, batch order, commit cadence, and lexical schema are unchanged.
- Tie-breaking unchanged: yes. No query, scoring, or shard-order changes.
- Floating-point: N/A.
- RNG seeds: unchanged / N/A.
- Golden/replay verification: compile-gated and re-benchmarked on the real corpus; the bad `3c486a1d` repin was rejected and reverted.

### Measured Rounds

| Label | Change | Wall s | Conv/s | Msg/s | `message_stream_ms` | `prepare_ms` | `add_ms` | `commit_ms` | Outcome |
|---|---|---:|---:|---:|---:|---:|---:|---:|---|
| `r93-clean-048d5fa8` | fresh control on current clean git-pinned `frankensearch` `048d5fa8` | 60.447 | 846.706 | 77815.221 | 54.886 | 5.288 | 4.608 | 7.270 | control |
| `r94-repin-3c486a1d` | repin `frankensearch` git rev to `3c486a1d` | 61.466 | 832.662 | 76524.167 | 55.693 | 5.605 | 4.924 | 9.791 | rejected |
| `r95-048d5fa8-old-edgegrams` | scratch dependency checkout: restore older `cass_generate_edge_ngrams` helper | 58.431 | 876.018 | 80503.520 | 52.655 | 5.249 | 4.582 | 7.572 | candidate |
| `r96-048d5fa8-old-edgegrams-repeat` | repeat same scratch helper reversal | 57.420 | 891.407 | 81918.620 | 51.514 | 5.053 | 4.407 | 7.162 | candidate-repeat |
| `r97-local-frankensearch-patch` | retained tree: enable local `frankensearch` `[patch]` override using sibling checkout with older helper | 56.410 | 907.375 | 83386.041 | 51.082 | 5.010 | 4.347 | 7.219 | kept |

### Takeaways
- The clean current baseline is `r93 = 60.447s`. That is the correct control for this pass, not the earlier local-override `r88` number.
- The simple git repin to `3c486a1d` was wrong. `r94 = 61.466s` lost cleanly and was reverted.
- Restoring the older helper inside `frankensearch-lexical::cass_generate_edge_ngrams` is a real win in the current environment. The retained-tree confirmation `r97 = 56.410s` improves on the clean control by about `6.7%` (`60.447s -> 56.410s`).
- The internal buckets support the wall-clock result. Against `r93`, `r97` cut `message_stream_ms` from `54.886s` to `51.082s`, `prepare_ms` from `5.288s` to `5.010s`, and `add_ms` from `4.608s` to `4.347s`.
- Conclusion: keep the local `frankensearch` override in `Cargo.toml` and the older edge-ngram helper in the sibling `frankensearch` checkout. The remaining frontier is still deeper lexical ingest structure, but this pass produced another retained real-corpus win.

### Artifacts
- `/tmp/cass-real-bench-20260419-r93-clean-048d5fa8/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r93-clean-048d5fa8/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260419-r94-repin-3c486a1d/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r94-repin-3c486a1d/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260419-r95-048d5fa8-old-edgegrams/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r95-048d5fa8-old-edgegrams/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260419-r96-048d5fa8-old-edgegrams-repeat/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r96-048d5fa8-old-edgegrams-repeat/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260419-r97-local-frankensearch-patch/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r97-local-frankensearch-patch/logs/index.stderr.log`


## Retained Stack-Buffered Edge-Ngram Helper

### Goal
- Keep the faster old edge-ngram semantics from `r97`, but remove the remaining per-word heap `Vec<usize>` allocation inside `cass_generate_edge_ngrams`.

### Alien/Queueing Framing
- This is a bounded automaton/state-buffer rewrite rather than a new algorithm. The helper only ever needs the first 21 boundary indices, so a fixed stack buffer is enough to preserve the exact prefix envelope while avoiding heap traffic on every token.
- The earlier streaming rewrite failed because it changed the work shape too much. This version keeps the winning old semantics and only changes the storage substrate for the bounded index set.

### Behavior Preservation Proof
- Ordering preserved: yes. Prefixes are emitted in the same per-word order as before.
- Tie-breaking unchanged: yes. No schema, query, commit, or batch-boundary changes.
- Floating-point: N/A.
- RNG seeds: unchanged / N/A.
- Golden/replay verification: helper unit tests for expected prefixes and 20-char cap passed in local `frankensearch`; retained-tree cass rebuild tests also passed.

### Measured Rounds

| Label | Change | Wall s | Conv/s | Msg/s | `message_stream_ms` | `finish_conversation_ms` | `prepare_ms` | `add_ms` | `commit_ms` | Outcome |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---|
| `r97-local-frankensearch-patch` | retained control with old helper + local sibling override | 56.410 | 907.375 | 83386.041 | 51.082 | 12.597 | 5.010 | 4.347 | 7.219 | control |
| `r98-stack-edgegrams` | replace per-word `Vec<usize>` with fixed `[usize; 21]` buffer | 55.385 | 924.172 | 84929.680 | 49.419 | 11.002 | 3.021 | 2.390 | 7.590 | candidate |
| `r99-stack-edgegrams-repeat` | repeat same stack-buffer helper | 55.401 | 923.901 | 84904.713 | 49.438 | 11.013 | 3.120 | 2.460 | 7.595 | kept |

### Takeaways
- The repeat held almost exactly: `55.385s` and `55.401s`.
- Against the retained `r97` control, the kept helper improves wall clock by about `1.8%` (`56.410s -> 55.401s`).
- The hot buckets moved sharply in the expected direction. Against `r97`, `r99` reduced `prepare_ms` from `5.010s` to `3.120s`, `add_ms` from `4.347s` to `2.460s`, and `finish_conversation_ms` from `12.597s` to `11.013s`.
- Conclusion: keep the stack-buffered helper in the sibling `frankensearch` checkout along with the existing local override. The remaining frontier is now even less about prefix-helper folklore and more about deeper lexical document-build structure.

### Artifacts
- `/tmp/cass-real-bench-20260419-r98-stack-edgegrams/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r98-stack-edgegrams/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260419-r99-stack-edgegrams-repeat/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r99-stack-edgegrams-repeat/logs/index.stderr.log`


## Retained Fused Content Prefix + Preview Scan

### Goal
- Remove the second scan over message content during lexical document build by fusing `content_prefix` generation and preview extraction into one pass, while preserving the exact outputs of `cass_generate_edge_ngrams(content)` and `cass_build_preview(content, 400)`.

### Alien/Queueing Framing
- This is a deterministic finite-state scanner on the hottest per-document path. The content string was being traversed once for edge-ngram generation and again for preview extraction; the fused helper keeps the same output contract but collapses those traversals into a single bounded state machine.
- The risk gate was constants-sensitive rather than algorithmic. Prior preview-only rewrites had lost, so this version kept the old winning prefix semantics and differential-tested the fused helper directly against the existing standalone helpers before benchmarking.

### Behavior Preservation Proof
- Ordering preserved: yes. Token boundary handling, per-word prefix ordering, and preview truncation semantics are unchanged.
- Tie-breaking unchanged: yes. No query, schema, commit, or batch-boundary changes.
- Floating-point: N/A.
- RNG seeds: unchanged / N/A.
- Golden/replay verification: local `frankensearch-lexical` test confirms the fused helper matches `cass_generate_edge_ngrams` plus `cass_build_preview(..., 400)` on representative ASCII, Unicode, punctuation, CJK, and long-input samples; retained-tree cass rebuild tests also passed.

### Measured Rounds

| Label | Change | Wall s | Conv/s | Msg/s | `message_stream_ms` | `finish_conversation_ms` | `prepare_ms` | `add_ms` | `commit_ms` | Outcome |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---|
| `r99-stack-edgegrams-repeat` | retained control with stack-buffered edge-ngram helper | 55.401 | 923.901 | 84904.713 | 49.438 | 11.013 | 3.120 | 2.460 | 7.595 | control |
| `r100-fused-content-scan` | one-pass fused `content_prefix + preview` builder | 54.411 | 940.717 | 86450.119 | 48.770 | 10.503 | 3.224 | 2.565 | 6.803 | candidate |
| `r101-fused-content-scan-repeat` | repeat same fused content scan | 54.377 | 941.292 | 86502.946 | 48.432 | 10.628 | 2.972 | 2.347 | 7.399 | kept |

### Takeaways
- The repeat held: `54.411s` and `54.377s`.
- Against the retained `r99` control, the kept fused helper improves wall clock by about `1.8%` (`55.401s -> 54.377s`).
- Against the clean `r93` baseline (`60.447s`), the current retained tree is now about `10.0%` faster.
- The hot buckets moved in the right direction again. Against `r99`, `r101` reduced `message_stream_ms` from `49.438s` to `48.432s`, `finish_conversation_ms` from `11.013s` to `10.628s`, `prepare_ms` from `3.120s` to `2.972s`, and `add_ms` from `2.460s` to `2.347s`.
- Conclusion: keep the fused content builder in the sibling `frankensearch` checkout together with the existing local override. The next frontier is no longer obvious string rescans; it is deeper document materialization and writer interaction structure.

### Artifacts
- `/tmp/cass-real-bench-20260419-r100-fused-content-scan/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r100-fused-content-scan/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260419-r101-fused-content-scan-repeat/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r101-fused-content-scan-repeat/logs/index.stderr.log`


## Corrected Actual Cass Tokenizer Retest

### Goal
- Re-run the custom `CassTokenizer` change on a freshly relinked runner after discovering that the earlier `r102`/`r103` numbers were produced by a stale binary that still contained `RegexTokenStream`.

### Alien/Queueing Framing
- The original hotspot diagnosis was still valid: the retained `r101` tree spent a dominant share of ingest CPU inside Tantivy regex tokenization (`regex_automata::dfa::search::find_fwd` at `38.17%` children / `34.73%` self, `RegexTokenStream::advance` at `29.29%` children in `/tmp/cass-r101-control.perf.data`).
- The correction was procedural rather than conceptual. After forcing a fresh cass relink and confirming `/tmp/cass_runner_r59` actually contained `CassTokenizer`, the same finite-state rewrite could be measured honestly.

### Behavior Preservation Proof
- Ordering preserved: yes. Token order, offsets, and positions are still differential-tested against the legacy regex tokenizer.
- Tie-breaking unchanged: yes. No schema, query, commit cadence, or batch geometry changes.
- Floating-point: N/A.
- RNG seeds: unchanged / N/A.
- Golden/replay verification: `cass_tokenizer_matches_legacy_regex_boundaries` passed on the sibling `frankensearch` checkout, and the retained-tree cass rebuild tests still passed.

### Measured Rounds

| Label | Change | Wall s | Conv/s | Msg/s | `message_stream_ms` | `finish_conversation_ms` | `prepare_ms` | `add_ms` | `commit_ms` | Outcome |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---|
| `r101-fused-content-scan-repeat` | retained control with fused content-prefix + preview scan | 54.377 | 941.292 | 86502.946 | 48.432 | 10.628 | 2.972 | 2.347 | 7.399 | control |
| `r104-actual-cass-tokenizer` | fresh relink, actual `CassTokenizer` binary confirmed | 53.226 | 961.655 | 88374.253 | 48.050 | 9.793 | 2.964 | 2.322 | 6.314 | candidate |
| `r105-actual-cass-tokenizer-repeat` | repeat same actual tokenizer build | 54.262 | 943.287 | 86686.240 | 48.840 | 9.837 | 3.047 | 2.383 | 6.181 | candidate-repeat |
| `r106-actual-cass-tokenizer-tiebreak` | third run on same actual tokenizer build | 54.169 | 944.918 | 86836.170 | 48.664 | 9.960 | 3.147 | 2.473 | 6.478 | kept-control |

### Takeaways
- `r102` and `r103` were invalid and should not be used. They came from a stale runner that still contained Tantivy's regex tokenizer.
- On the correctly relinked binary, the tokenizer rewrite still stays on the positive side, but it is a small edge rather than the earlier claimed clean repeat-held win. Against `r101`, the corrected candidate mean is `53.886s` (`0.492s`, about `0.90%` faster) and the median is `54.169s` (`0.209s`, about `0.38%` faster).
- A fresh `perf` sample on the actual tokenizer tree (`/tmp/cass-r106-actual.perf.data`) confirmed that regex DFA cost was removed and the dominant ingest CPU shifted to the custom token stream plus Tantivy postings work. That made the next frontier clear.
- Conclusion: keep the custom tokenizer in the local working tree as the live control for the next round, but downgrade the historical claim. The real value here is modest; the large retained win came from the follow-on postings change below.

### Artifacts
- `/tmp/cass-r101-control.perf.data`
- `/tmp/cass-r106-actual.perf.data`
- `/tmp/cass-real-bench-20260419-r104-actual-cass-tokenizer/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r104-actual-cass-tokenizer/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260419-r105-actual-cass-tokenizer-repeat/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r105-actual-cass-tokenizer-repeat/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260419-r106-actual-cass-tokenizer-tiebreak/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r106-actual-cass-tokenizer-tiebreak/logs/index.stderr.log`

## Retained Prefix-Field Freq-Only Postings

### Goal
- Reduce Tantivy postings write cost by storing term frequencies but not positions for `title_prefix` and `content_prefix`, because those fields are only used by `TermQuery` paths and never by `PhraseQuery`.

### Alien/Queueing Framing
- The actual `r106` profile moved the bottleneck from regex DFA into Tantivy postings (`SpecializedPostingsWriter<TfAndPositionRecorder>::subscribe` and related serialization paths) while token-prep time stayed comparatively smaller.
- This made the highest-EV move a structural schema reduction rather than another tokenizer trick: keep BM25 term-frequency scoring on prefix fields, but stop paying to record positions that no query path ever reads.

### Behavior Preservation Proof
- Ordering preserved: yes. Document order, token order, batch order, and commit cadence are unchanged.
- Tie-breaking unchanged: yes in the intended scoring contract. `title_prefix` and `content_prefix` still store frequencies, and phrase queries continue to target only `title` and `content`.
- Floating-point: N/A.
- RNG seeds: unchanged / N/A.
- Golden/replay verification: new `frankensearch-lexical` test asserts that both prefix fields now store `IndexRecordOption::WithFreqs`; the tokenizer differential test and fused-helper differential test still pass; retained-tree cass rebuild tests also passed.

### Measured Rounds

| Label | Change | Wall s | Conv/s | Msg/s | `message_stream_ms` | `finish_conversation_ms` | `prepare_ms` | `add_ms` | `commit_ms` | Outcome |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---|
| `r106-actual-cass-tokenizer-tiebreak` | live control with actual custom tokenizer | 54.169 | 944.918 | 86836.170 | 48.664 | 9.960 | 3.147 | 2.473 | 6.478 | control |
| `r107-prefix-freqs-candidate` | `title_prefix` / `content_prefix` store freqs without positions | 51.367 | 996.448 | 91571.686 | 45.986 | 7.434 | 3.033 | 2.370 | 3.837 | candidate |
| `r108-prefix-freqs-repeat` | repeat same freq-only prefix postings schema | 52.362 | 977.519 | 89832.120 | 46.525 | 7.401 | 3.039 | 2.381 | 3.778 | kept |

### Takeaways
- This is a real retained win. Against the live `r106` control, the two runs improved wall clock by `2.801s` (`5.17%`) and `1.807s` (`3.34%`); the candidate mean is `51.865s`, about `4.25%` faster than control.
- The bucket movement matches the hypothesis almost perfectly. Against `r106`, the repeat `r108` cut `message_stream_ms` from `48.664s` to `46.525s`, `finish_conversation_ms` from `9.960s` to `7.401s`, and `commit_ms` from `6.478s` to `3.778s`, while `prepare_ms` and `add_ms` stayed essentially flat.
- Relative to the clean `r93 = 60.447s` baseline, the current retained tree is now about `13.4%` faster on the real corpus using the conservative repeated `r108` number.
- Conclusion: keep the freq-only prefix postings schema in the sibling `frankensearch` checkout together with the existing local override. The next frontier is now even deeper inside Tantivy ingest and writer interaction rather than in token-boundary work.

### Artifacts
- `/tmp/cass-real-bench-20260419-r107-prefix-freqs-candidate/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r107-prefix-freqs-candidate/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260419-r108-prefix-freqs-repeat/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r108-prefix-freqs-repeat/logs/index.stderr.log`


## Retained Preview Stored-Only Field

### Goal
- Remove useless tokenization and postings work for `preview`, which cass stores as a fallback display field but never targets in lexical query construction.

### Alien/Queueing Framing
- The control profile after `r108` still showed Tantivy tokenization and postings dominating rebuild cost, so the highest-EV next lever was to cut write amplification from a field that did not contribute to retrieval.
- This lines up with the graveyard guidance to eliminate needless index work before chasing more exotic local hot-loop tricks: if a field only needs stored retrieval semantics, indexing it is pure ingest tax.

### Behavior Preservation Proof
- Ordering preserved: yes. Document build order, flush cadence, commit cadence, and query result ordering are unchanged.
- Tie-breaking unchanged: yes. `preview` is still stored and retrievable, but lexical query construction does not target it; the fallback read path in `src/search/query.rs` still reads the stored field.
- Floating-point: N/A.
- RNG seeds: unchanged / N/A.
- Golden/replay verification: new `frankensearch-lexical` test asserts that `preview` is stored-only; prefix-field and fused-helper tests still pass; retained-tree cass rebuild tests also passed.

### Measured Rounds

| Label | Change | Wall s | Conv/s | Msg/s | `message_stream_ms` | `finish_conversation_ms` | `prepare_ms` | `add_ms` | `commit_ms` | Outcome |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---|
| `r108-prefix-freqs-repeat` | live control with freq-only prefix postings | 52.362 | 977.519 | 89832.120 | 46.525 | 7.401 | 3.039 | 2.381 | 3.778 | control |
| `r109-preview-stored-only` | `preview` changed from `TEXT | STORED` to `STORED` | 51.365 | 996.493 | 91575.771 | 46.188 | 7.464 | 3.178 | 2.542 | 3.597 | candidate |
| `r110-preview-stored-only-repeat` | repeat same stored-only preview schema | 52.379 | 977.213 | 89804.033 | 47.100 | 7.868 | 3.301 | 2.639 | 3.752 | statistical tie |
| `r111-preview-stored-only-tiebreak` | tiebreak repeat on same schema | 51.378 | 996.248 | 91553.265 | 46.606 | 7.665 | 3.184 | 2.543 | 3.867 | kept |

### Takeaways
- This is a modest but retained win. Against the live `r108` control, two of three runs improved wall clock by `0.997s` (`1.90%`) and `0.984s` (`1.88%`); the middle repeat was effectively flat at `-0.016s` (`-0.03%`). The three-run candidate mean is `51.707s`, about `1.25%` faster than control.
- The strongest deterministic gain is index size. Removing `preview` postings shrank the produced index from `3,452,397,136` bytes to `3,236,213,871` bytes on `r111`, a reduction of `216,183,265` bytes (`6.26%`).
- The rebuild stage buckets were noisier than the wall clock on this round, so the keep decision is based primarily on repeated end-to-end time plus the structural proof that `preview` indexing was unused work.
- Relative to the clean `r93 = 60.447s` baseline, the current retained tree is now about `15.0%` faster on the real corpus using the conservative repeated `r111` number.
- Conclusion: keep `preview` as stored-only in the sibling `frankensearch` checkout together with the existing local override. The next frontier is likely deeper Tantivy writer/postings structure again, not more unused-field cleanup.

### Artifacts
- `/tmp/cass-real-bench-20260419-r109-preview-stored-only/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r109-preview-stored-only/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260419-r110-preview-stored-only-repeat/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r110-preview-stored-only-repeat/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260419-r111-preview-stored-only-tiebreak/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r111-preview-stored-only-tiebreak/logs/index.stderr.log`


## Rejected Prefix Direct-Term Stream

### Goal
- Eliminate the hot `hyphen_normalize` tokenizer stack on `title_prefix` and `content_prefix` by precomputing the exact legacy prefix-field term stream up front, then indexing those fields with a cheap whitespace-plus-lowercase analyzer.

### Alien/Queueing Framing
- The fresh `r111` profile still showed `RemoveLongFilterStream<...CassTokenStream>::advance` and Tantivy postings subscription dominating ingest CPU, so the highest-EV hypothesis was to remove analyzer-layer work from the already-precomputed prefix fields.
- The design used a proof-carrying direct term stream: preserve the indexed prefix terms exactly, but move their construction out of Tantivy’s tokenizer pipeline.

### Behavior Preservation Proof
- Ordering preserved: yes in the attempted design. Document order, flush cadence, commit cadence, and field/query structure were unchanged.
- Tie-breaking unchanged: yes by construction in the proof harness. A differential test showed the new direct prefix stream produced the same analyzed token sequence as the legacy prefix analyzer on representative ASCII, CJK, accented, and mixed-script samples.
- Floating-point: N/A.
- RNG seeds: unchanged / N/A.
- Golden/replay verification: the new `frankensearch-lexical` differential test passed locally before the benchmark run.

### Measured Rounds

| Label | Change | Wall s | Conv/s | Msg/s | `message_stream_ms` | `finish_conversation_ms` | `prepare_ms` | `add_ms` | `commit_ms` | Outcome |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---|
| `r111-preview-stored-only-tiebreak` | live retained control | 51.378 | 996.248 | 91553.265 | 46.606 | 7.665 | 3.184 | 2.543 | 3.867 | control |
| `r112-prefix-direct-terms` | precompute exact prefix-field terms + whitespace tokenizer | 53.383 | 958.818 | 88113.512 | 47.855 | 8.161 | 3.733 | 3.038 | 3.665 | rejected |

### Takeaways
- This candidate lost clearly on the first real-corpus run: `+2.006s` slower than control (`+3.90%`). That is too large to justify a repeat.
- The failure mode is visible in the stage buckets. Relative to `r111`, `message_stream_ms` rose from `46.606s` to `47.855s`, `finish_conversation_ms` rose from `7.665s` to `8.161s`, and both `prepare_ms` and `add_ms` got materially worse. `commit_ms` improved slightly, but nowhere near enough to offset the added precompute cost.
- The direct term-stream proof was not the problem; the cost model was. Moving the exact token construction out of Tantivy and into cass’s prep path simply shifted too much work into the single-threaded prebuild side.
- Conclusion: reject the direct-prefix-term / dedicated-prefix-tokenizer rewrite and restore the prior retained tree.

### Artifacts
- `/tmp/cass-r111-control.perf.data`
- `/tmp/cass-real-bench-20260419-r112-prefix-direct-terms/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r112-prefix-direct-terms/logs/index.stderr.log`


## Rejected CJK Bigram Fast Path

### Goal
- Remove the unconditional per-token `Vec<char>` allocation inside `CjkBigramDecompose` by scanning tokens allocation-free first and only materializing bigrams for all-CJK multi-character tokens.

### Alien/Queueing Framing
- The fresh retained-tree profile still had `RemoveLongFilterStream<...CassTokenStream>::advance` dominating rebuild CPU, and the CJK filter was one of the last remaining analyzer stages doing per-token heap work even for plain ASCII tokens.
- This was a classic graveyard hot-loop candidate: keep semantics fixed, eliminate useless allocation on the common path, and let the writer stage stay unchanged.

### Behavior Preservation Proof
- Ordering preserved: yes in the attempted design. Token order, document order, flush cadence, and commit cadence were unchanged.
- Tie-breaking unchanged: yes. The attempted rewrite emitted the same bigram sequence for CJK tokens and passed through non-CJK tokens unchanged.
- Floating-point: N/A.
- RNG seeds: unchanged / N/A.
- Golden/replay verification: focused lexical tests passed before benchmarking, including existing CJK bigram tests and a temporary differential proof against the legacy helper.

### Measured Rounds

| Label | Change | Wall s | Conv/s | Msg/s | `message_stream_ms` | `finish_conversation_ms` | `prepare_ms` | `add_ms` | `commit_ms` | Outcome |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---|
| `r113-cjk-fastpath-control` | retained control on rebuilt real profiling binary | 56.513 | 905.714 | 83233.432 | 50.738 | 7.505 | 3.092 | 2.445 | 3.731 | control |
| `r114-cjk-fastpath-candidate` | allocation-free CJK scan + reverse-slice bigram build | 56.508 | 905.806 | 83241.865 | 50.639 | 7.361 | 3.136 | 2.429 | 3.555 | candidate |
| `r115-cjk-fastpath-repeat` | repeat same fast-path build | 56.506 | 905.830 | 83244.053 | 51.011 | 7.481 | 3.139 | 2.437 | 3.761 | statistical tie |

### Takeaways
- This is noise, not a keeper. The candidate mean (`56.507s`) beat control (`56.513s`) by only `0.006s`, about `0.01%`, which is far below the threshold worth retaining.
- The first candidate run looked mildly encouraging in stage buckets, but the repeat gave that back: `message_stream_ms` rose from `50.639s` to `51.011s`, `finish_conversation_ms` rose from `7.361s` to `7.481s`, and `commit_ms` overshot control.
- The attempted proof was sound; the economics were not. Removing this small analyzer allocation simply does not move enough real end-to-end work on the current retained tree.
- Conclusion: reject the CJK fast-path rewrite and restore the prior retained source state.

### Artifacts
- `/tmp/cass-real-bench-20260419-r113-cjk-fastpath-control/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r113-cjk-fastpath-control/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260419-r114-cjk-fastpath-candidate/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r114-cjk-fastpath-candidate/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260419-r115-cjk-fastpath-repeat/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r115-cjk-fastpath-repeat/logs/index.stderr.log`


## Retained Tantivy Content Externalization

### Goal
- Stop storing full `content` in the lexical Tantivy documents, keep it indexed-only, and hydrate missing content from the authoritative SQLite database at search time.

### Alien/Queueing Framing
- This was a direct write-amplification attack from the graveyard playbook: remove a large duplicated payload from the inverted-index write path, keep query semantics intact, and pay the recovery cost only on the relatively colder read path that actually needs full content.
- The highest-EV hypothesis was that Tantivy segment build and commit would materially benefit from deleting a multi-gigabyte stored-field stream, especially after the earlier prefix/postings wins had already squeezed easier analyzer hot loops.

### Behavior Preservation Proof
- Indexed search semantics preserved: yes. `content` remains indexed, so lexical matching, prefix terms, BM25 scoring, and ranking inputs are unchanged.
- Result payload semantics preserved: yes. cass now hydrates missing content by `(conversation_id, msg_idx)` when available, with a compatibility fallback keyed by `(source_id, source_path, msg_idx)` for ad hoc indexes built without embedded `conversation_id`.
- Snippet behavior preserved: yes. When Tantivy no longer stores `content`, cass synthesizes a snippet document from the hydrated content and reuses the existing snippet renderer.
- Harness correction: the new regression proof uses a dedicated `search-index/` subdir because `TantivyIndex::open_or_create(dir.path())` rebuild semantics are allowed to clear the target directory, which would invalidate a sibling temp `cass.db` in the same root.
- Golden/replay verification before benchmarking:
  - `cass_content_field_is_indexed_not_stored`
  - `tantivy_search_hydrates_long_content_when_content_field_is_not_stored`
  - `add_prebuilt_documents_streams_large_payloads_without_dropping_docs`
  - `rebuild_tantivy_from_db_logs_streamed_batch_stats`

### Measured Rounds

| Label | Change | Wall s | Conv/s | Msg/s | `message_stream_ms` | `finish_conversation_ms` | `prepare_ms` | `add_ms` | `commit_ms` | Outcome |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---|
| `r113-cjk-fastpath-control` | retained control on rebuilt real profiling binary | 56.513 | 905.714 | 83233.432 | 50.738 | 7.505 | 3.092 | 2.445 | 3.731 | control |
| `r116-content-externalized` | `content` changed from indexed+stored to indexed-only; search hydrates from SQLite | 54.233 | 943.802 | 86733.614 | — | — | — | — | — | candidate |
| `r117-content-externalized-repeat` | repeat on same retained tree | 55.142 | 928.232 | 85302.775 | — | — | — | — | — | repeat |
| `r118-content-externalized-profile` | profiled confirmation run with `CASS_TANTIVY_REBUILD_PROFILE=1` | 54.025 | 947.429 | 87066.916 | 49.067 | 6.674 | 2.971 | 2.333 | 3.106 | kept |

### Takeaways
- This is a retained win. The conservative repeated result improved from `56.513s` to `55.142s`, about `2.43%` faster. The three-run candidate mean was `54.467s`, about `3.62%` faster than control.
- The profiled run moved the right buckets:
  - `message_stream_ms`: `50.738s -> 49.067s` (`-3.29%`)
  - `finish_conversation_ms`: `7.505s -> 6.674s` (`-11.08%`)
  - `prepare_ms`: `3.092s -> 2.971s` (`-3.90%`)
  - `add_ms`: `2.445s -> 2.333s` (`-4.59%`)
  - `commit_ms`: `3.731s -> 3.106s` (`-16.75%`)
- The index-size effect is large and deterministic. The rebuilt index dropped from `3,215,812,765` bytes on control to `2,399,327,541` bytes on the profiled kept run, a reduction of `816,485,224` bytes (`25.39%`).
- Conclusion: keep the content-externalization change. It is a real end-to-end ingest win, not just a size-only cleanup.

### Artifacts
- `/tmp/cass-real-bench-20260419-r116-content-externalized/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r116-content-externalized/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260419-r117-content-externalized-repeat/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r117-content-externalized-repeat/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260419-r118-content-externalized-profile/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r118-content-externalized-profile/logs/index.stderr.log`


## Rejected Authoritative Rebuild `source_path` Externalization

### Goal
- Stop storing `source_path` on the authoritative DB rebuild path only, and hydrate it from SQLite by `conversation_id` at lexical search time.

### Alien/Queueing Framing
- This was the next obvious write-amplification lever after full `content` externalization: `source_path` was still a large repeated stored payload on every lexical message document, but unlike `content` it is not indexed for matching.
- The high-EV hypothesis was that removing that repeated stored field from the hot authoritative rebuild stream would shave segment-write and commit work while keeping ad hoc index behavior unchanged.

### Behavior Preservation Proof
- Indexed search semantics preserved in the candidate: yes. `source_path` is not part of lexical matching, so ranking and BM25 clause construction were unchanged.
- Result payload semantics preserved in the candidate: yes. Missing stored `source_path` values were backfilled from SQLite by `conversation_id` before `SearchHit` construction.
- Post-search `session_paths` filtering preserved in the candidate: yes. A dedicated regression test proved that authoritative rebuild docs with omitted `source_path` still matched `session_paths` filters after hydration.
- Golden/replay verification before benchmarking:
  - `cass_document_refs_may_omit_source_path`
  - `tantivy_search_hydrates_source_path_when_authoritative_rebuild_omits_it`
  - `tantivy_search_hydrates_long_content_when_content_field_is_not_stored`
  - `add_prebuilt_documents_streams_large_payloads_without_dropping_docs`
  - `rebuild_tantivy_from_db_logs_streamed_batch_stats`

### Measured Rounds

| Label | Change | Wall s | Conv/s | Msg/s | `message_stream_ms` | `finish_conversation_ms` | `prepare_ms` | `add_ms` | `commit_ms` | `index_size_bytes` | Outcome |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| `r118-content-externalized-profile` | retained control | 54.025 | 947.429 | 87066.916 | 49.067 | 6.674 | 2.971 | 2.333 | 3.106 | 2399327541 | control |
| `r119-source-path-externalized` | authoritative rebuild omits stored `source_path`; search hydrates by `conversation_id` | 54.004 | 947.807 | 87101.646 | 48.919 | 6.604 | 2.966 | 2.312 | 3.058 | 2367926029 | candidate |
| `r120-source-path-externalized-repeat` | repeat on same tree | 54.121 | 945.749 | 86912.510 | 49.562 | 6.907 | 3.116 | 2.453 | 3.135 | 2368123163 | reject |

### Takeaways
- This is not a keeper. `r119` beat the retained control by only `0.0215s` (`0.04%`), and the repeat `r120` lost by `0.0960s` (`0.18%`).
- The two candidate runs average `54.062s`, which is slightly slower than the retained `r118 = 54.025s`. That is noise at best, and not a repeat-held win.
- The candidate did shrink the rebuilt index by about `31.3 MB` (`1.30%`) versus `r118`, but that size-only improvement is too small to justify the added search-time hydration path and extra complexity.
- Conclusion: reject the authoritative-rebuild-only `source_path` externalization and restore the prior retained tree.

### Artifacts
- `/tmp/cass-real-bench-20260419-r119-source-path-externalized/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r119-source-path-externalized/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260419-r120-source-path-externalized-repeat/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r120-source-path-externalized-repeat/logs/index.stderr.log`


## Retained Cass-Specific Fused Normalize+Limit Filter

### Goal
- Replace the generic `LowerCaser + RemoveLongFilter::limit(256)` pair in the cass Tantivy analyzer with a cass-specific fused filter that performs the same normalization in one pass.

### Alien/Queueing Framing
- This is a certified data-plane rewrite rather than a schema trick: keep the exact token stream contract, but collapse two generic analyzer stages into one cass-specific fast path.
- The saved `perf` evidence already showed the analyzer chain dominating authoritative rebuild CPU, so the highest-EV next lever was a proof-backed specialization of the remaining generic normalization layer.

### Behavior Preservation Proof
- Token boundary semantics preserved: yes. `CassTokenizer`, `HyphenDecompose`, and `CjkBigramDecompose` are unchanged.
- Lowercasing semantics preserved: yes. `CassTokenizer` only emits ASCII alphanumeric runs and CJK runs, so `String::make_ascii_lowercase()` is behaviorally equivalent to Tantivy's generic `LowerCaser` on the emitted token language.
- Long-token filtering preserved: yes. The fused filter drops tokens whose UTF-8 byte length exceeds `256`, matching `RemoveLongFilter::limit(256)`.
- Schema/index compatibility preserved: yes. Field definitions, analyzer name, and lexical query construction are unchanged, so no schema/version bump was needed.
- Golden/replay verification before benchmarking:
  - `cass_tokenizer_matches_legacy_regex_boundaries`
  - `cass_normalize_and_limit_matches_legacy_pipeline`
  - `cass_build_content_prefix_and_preview_matches_existing_helpers`
  - `tantivy_search_hydrates_long_content_when_content_field_is_not_stored`
  - `add_prebuilt_documents_streams_large_payloads_without_dropping_docs`
  - `rebuild_tantivy_from_db_logs_streamed_batch_stats`

### Measured Rounds

| Label | Change | Wall s | Conv/s | Msg/s | `message_stream_ms` | `finish_conversation_ms` | `prepare_ms` | `add_ms` | `commit_ms` | `index_size_bytes` | Outcome |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| `r118-content-externalized-profile` | retained control | 54.025 | 947.429 | 87066.916 | 49.067 | 6.674 | 2.971 | 2.333 | 3.106 | 2399327541 | control |
| `r121-fast-normalize-filter` | replace generic lowercase + long-token filters with fused cass-specific normalizer | 53.051 | 964.826 | 88665.643 | 48.676 | 6.589 | 2.889 | 2.258 | 3.112 | 2398780869 | candidate |
| `r122-fast-normalize-filter-repeat` | repeat on same retained tree | 53.123 | 963.526 | 88546.247 | 48.719 | 6.665 | 2.895 | 2.254 | 3.126 | 2399846232 | kept |

### Takeaways
- This is a retained win. The conservative repeated result improved from `54.025s` to `53.123s`, about `1.67%` faster.
- The two candidate runs averaged `53.087s`, about `1.74%` faster than the retained control.
- The hot rebuild buckets moved in the right direction on both runs:
  - `message_stream_ms`: `49.067s -> 48.676s / 48.719s` (`-0.71%` to `-0.80%`)
  - `finish_conversation_ms`: `6.674s -> 6.589s / 6.665s` (`-0.13%` to `-1.28%`)
  - `prepare_ms`: `2.971s -> 2.889s / 2.895s` (`-2.56%` to `-2.76%`)
  - `add_ms`: `2.333s -> 2.258s / 2.254s` (`-3.18%` to `-3.41%`)
  - `commit_ms`: essentially flat/noise (`3.106s -> 3.112s / 3.126s`)
- Index size stayed effectively unchanged, which is what we want for a pure CPU-path rewrite. This is an ingest-speed win without a schema tradeoff.
- Conclusion: keep the fused cass-specific normalize+limit filter. It trims the remaining generic analyzer overhead while preserving the exact lexical token stream.

### Artifacts
- `/tmp/cass-real-bench-20260419-r121-fast-normalize-filter/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r121-fast-normalize-filter/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260419-r122-fast-normalize-filter-repeat/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r122-fast-normalize-filter-repeat/logs/index.stderr.log`


## Rejected ASCII-Byte DFA Tokenizer Scan

### Goal
- Replace the hot ASCII path in `CassTokenStream::advance` with a byte-wise DFA so UTF-8 decoding only happens on non-ASCII bytes.

### Alien/Parser Framing
- This was a certified parser-kernel rewrite: keep the tokenizer language and offsets identical, but switch the dominant ASCII scan from per-character decoding to a deterministic byte machine.
- The idea was directly motivated by the retained `perf` evidence that `CassTokenStream::advance` still dominated authoritative rebuild CPU after the fused normalize+limit win.

### Behavior Preservation Proof
- Token boundary semantics preserved in the candidate: yes. `cass_tokenizer_matches_legacy_regex_boundaries` still matched the legacy regex tokenizer on the existing adversarial fixture set.
- Analyzer output semantics preserved in the candidate: yes. `cass_normalize_and_limit_matches_legacy_pipeline` still held because the downstream analyzer stages were unchanged.
- Golden/replay verification before benchmarking:
  - `cass_tokenizer_matches_legacy_regex_boundaries`
  - `cass_normalize_and_limit_matches_legacy_pipeline`
  - `cass_build_content_prefix_and_preview_matches_existing_helpers`

### Measured Rounds

| Label | Change | Wall s | Conv/s | Msg/s | `message_stream_ms` | `finish_conversation_ms` | `prepare_ms` | `add_ms` | `commit_ms` | Outcome |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---|
| `r122-fast-normalize-filter-repeat` | retained control | 53.123 | 963.526 | 88546.247 | 48.719 | 6.665 | 2.895 | 2.254 | 3.126 | control |
| `r123-ascii-dfa-tokenizer` | byte-wise ASCII DFA in `CassTokenStream::advance` | 53.192 | 962.277 | 88431.379 | 48.651 | 6.592 | 2.896 | 2.247 | 3.120 | reject |
| `r124-ascii-dfa-tokenizer-repeat` | attempted repeat | — | — | — | — | — | — | — | — | harness failure |

### Takeaways
- This is not a keeper. The only completed corpus run, `r123`, was slightly slower than the retained `r122` control (`53.192s` vs `53.123s`, about `0.13%` worse).
- The internal rebuild buckets moved slightly in the right direction, but not enough to overcome wall-time noise. That is exactly the kind of near-tie that should be rejected, not rationalized into a win.
- The attempted repeat `r124` failed before launch because the profiling binary path used by the local harness disappeared (`target-optscan/profiling/cass` missing). Since the first run already failed to beat control, the missing repeat is not worth re-running via another full profiling rebuild cycle.
- Conclusion: reject the ASCII-byte DFA tokenizer scan and restore the prior retained tree.

### Artifacts
- `/tmp/cass-real-bench-20260419-r123-ascii-dfa-tokenizer/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r123-ascii-dfa-tokenizer/logs/index.stderr.log`


## Retained Prefix Presence-Only Postings

### Goal
- Reduce Tantivy write amplification on the cass prefix fields by storing only term presence for `title_prefix` and `content_prefix`, instead of term frequencies.

### Alien/Postings Framing
- The retained `perf` evidence after the tokenizer and content-externalization wins left a clear remaining hotspot family: Tantivy postings subscription work, especially the `TermFrequencyRecorder` path that prefix fields still paid for on every emitted edge n-gram.
- The high-EV hypothesis was that cass only needs prefix-field membership to satisfy prefix matching; exact `title` and `content` fields still carry the full BM25 term-frequency and positional signal. That makes frequency-tracked prefix postings redundant cost on the hot rebuild path.

### Behavior Preservation Proof
- Exact lexical semantics preserved: yes. `title` and `content` remain indexed with `WithFreqsAndPositions`, so exact-term BM25 scoring and phrase matching are unchanged.
- Prefix matching semantics preserved: yes. Prefix fields remain indexed and queryable; only their posting detail drops from `WithFreqs` to `Basic`, which keeps term presence while removing redundant per-doc frequency tracking.
- Title-only lexical retrieval preserved: yes. `title` field storage and indexing are unchanged.
- Golden/replay verification before benchmarking:
  - `cass_prefix_fields_store_basic_without_freqs_or_positions`
  - `prefix_wildcard_matches_start_of_term`
  - `edge_ngram_enables_prefix_search`
  - `title_field_is_searchable`
  - `wildcard_fallback_short_query_triggers_prefix`
  - `add_prebuilt_documents_streams_large_payloads_without_dropping_docs`
  - `rebuild_tantivy_from_db_logs_streamed_batch_stats`

### Measured Rounds

| Label | Change | Wall s | Conv/s | Msg/s | `message_stream_ms` | `finish_conversation_ms` | `prepare_ms` | `add_ms` | `commit_ms` | `index_size_bytes` | Outcome |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| `r122-fast-normalize-filter-repeat` | retained control | 53.123 | 963.526 | 88546.247 | 48.719 | 6.665 | 2.895 | 2.254 | 3.126 | 2399846232 | control |
| `r125-prefix-basic-postings` | switch `title_prefix` / `content_prefix` postings from `WithFreqs` to `Basic` | 51.716 | 989.732 | 90954.487 | 47.672 | 6.126 | 2.873 | 2.260 | 2.666 | 2062700780 | candidate |
| `r126-prefix-basic-postings-repeat` | repeat on same retained tree | 51.528 | 993.347 | 91286.726 | 47.533 | 6.146 | 2.862 | 2.261 | 2.693 | 2063387147 | kept |

### Takeaways
- This is a retained win. The conservative repeated result improved from `53.123s` to `51.528s`, about `3.00%` faster.
- The two candidate runs averaged `51.622s`, about `2.82%` faster than the retained control.
- The rebuilt index also got materially smaller: `2,399,846,232` bytes on `r122` down to `2,063,387,147` bytes on repeated `r126`, a reduction of `336,459,085` bytes (`14.02%`).
- The hot rebuild buckets moved in the right direction overall:
  - `message_stream_ms`: `48.719s -> 47.533s` (`-2.44%`)
  - `finish_conversation_ms`: `6.665s -> 6.146s` (`-7.79%`)
  - `prepare_ms`: `2.895s -> 2.862s` (`-1.13%`)
  - `commit_ms`: `3.126s -> 2.693s` (`-13.86%`)
  - `add_ms`: essentially flat/noise (`2.254s -> 2.261s`)
- Conclusion: keep the prefix-field `Basic` postings change. cass still gets prefix matching, but no longer pays per-document frequency bookkeeping for edge n-gram fields.

### Artifacts
- `/tmp/cass-real-bench-20260419-r125-prefix-basic-postings/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r125-prefix-basic-postings/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260419-r126-prefix-basic-postings-repeat/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r126-prefix-basic-postings-repeat/logs/index.stderr.log`


## Rejected Prefix-Term Dedup Under Basic Postings

### Goal
- Exploit the fact that `title_prefix` and `content_prefix` now use `IndexRecordOption::Basic` by deduplicating repeated prefix terms within a document before handing them to Tantivy.

### Alien/Set-Semantics Framing
- This was the obvious algebraic follow-on to the retained prefix-postings win: once prefix fields are presence-only, duplicate per-document prefix terms become semantically idempotent.
- A direct sample against the authoritative cass database suggested a plausible opportunity. On 1,000 recent conversations, the generated prefix stream averaged `191.005` total prefix terms, `126.334` unique prefix terms, and `64.671` duplicates per document (`33.86%` duplicate ratio).
- The candidate therefore introduced exact per-document deduplication in the prefix builders using an `AHashSet<&str>`, while preserving first-occurrence order so the emitted term stream remained deterministic.

### Behavior Preservation Proof
- Prefix matching semantics preserved in the candidate: yes. Prefix fields already use presence-only postings, so duplicate term suppression does not change the logical per-document term set.
- Deterministic output ordering preserved in the candidate: yes. The first occurrence of each prefix term was still emitted in encounter order.
- Golden/replay verification before benchmarking:
  - `cass_generate_edge_ngrams_deduplicates_repeated_prefix_terms`
  - `cass_build_content_prefix_and_preview_matches_existing_helpers`
  - `prefix_wildcard_matches_start_of_term`
  - `edge_ngram_enables_prefix_search`
  - `wildcard_fallback_short_query_triggers_prefix`

### Measured Rounds

| Label | Change | Wall s | Conv/s | Msg/s | `message_stream_ms` | `finish_conversation_ms` | `prepare_ms` | `add_ms` | `commit_ms` | `index_size_bytes` | Outcome |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| `r126-prefix-basic-postings-repeat` | retained control | 51.528 | 993.347 | 91286.726 | 47.533 | 6.146 | 2.862 | 2.261 | 2.693 | 2063387147 | control |
| `r127-prefix-dedup-basic-postings` | exact per-document dedup of repeated prefix terms via `AHashSet<&str>` | 53.484 | 957.008 | 87947.197 | 49.199 | 7.379 | 4.186 | 3.589 | 2.534 | 2063387344 | reject |

### Takeaways
- This is not a keeper. `r127` regressed from `51.528s` to `53.484s`, about `3.80%` slower than the retained control.
- The candidate barely changed the rebuilt index size (`+197` bytes), so the dedup bookkeeping cost was pure overhead on the hot rebuild path.
- The algebra was correct but the implementation economics were wrong: hashing and probing every candidate prefix term cost more than simply feeding the duplicates through Tantivy's already-cheap presence-only postings path.
- The stage buckets make that failure mode explicit: `prepare_ms` and `add_ms` both ballooned, and `finish_conversation_ms` got materially worse too, even though `commit_ms` improved slightly.
- Conclusion: reject exact prefix-term dedup under `Basic` postings and restore the prior retained tree.

### Artifacts
- `/tmp/cass-real-bench-20260419-r127-prefix-dedup-basic-postings/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r127-prefix-dedup-basic-postings/logs/index.stderr.log`


## Rejected Preview Field Elision

### Goal
- Stop materializing the stored `preview` payload into Tantivy documents during authoritative cass rebuilds, relying on the existing SQLite content-hydration path for lexical snippets instead.

### Alien/Compression Framing
- After the retained content-hydration keeper, `preview` looked like a classic stale cache artifact: snippets already hydrate full content from SQLite whenever `content` or `snippet` is requested, so the stored preview field had become a residual duplicate of the first 400 content characters.
- A direct corpus probe on the authoritative cass database suggested a large apparent byte opportunity: `4,703,804` messages with average preview length `112.23` characters, or about `527,918,461` raw preview characters total.
- The candidate therefore elided preview materialization from the hot lexical document-build path while keeping the schema field and query-side fallback machinery intact.

### Behavior Preservation Proof
- Lexical content hydration preserved in the candidate: yes. `tantivy_search_hydrates_long_content_when_content_field_is_not_stored` still passed, proving full content and snippets could render from SQLite without stored Tantivy content.
- Prefix generation semantics preserved in the candidate: yes. `cass_build_content_prefix_and_preview_matches_existing_helpers` still held for the retained helper logic, and the candidate only removed preview emission from built docs.
- Direct preview-elision proof before benchmarking:
  - `cass_built_documents_do_not_materialize_preview_field`
  - `tantivy_search_hydrates_long_content_when_content_field_is_not_stored`

### Measured Rounds

| Label | Change | Wall s | Conv/s | Msg/s | `message_stream_ms` | `finish_conversation_ms` | `prepare_ms` | `add_ms` | `commit_ms` | `index_size_bytes` | Outcome |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| `r126-prefix-basic-postings-repeat` | retained control | 51.528 | 993.347 | 91286.726 | 47.533 | 6.146 | 2.862 | 2.261 | 2.693 | 2063387147 | control |
| `r128-preview-elided` | omit stored `preview` payload from built Tantivy docs | 53.243 | 961.351 | 88346.359 | 49.222 | 7.405 | 4.201 | 3.598 | 2.599 | 2063298121 | reject |

### Takeaways
- This is not a keeper. `r128` regressed from `51.528s` to `53.243s`, about `3.33%` slower than the retained control.
- The rebuilt index barely moved: `2,063,387,147` bytes down to `2,063,298,121`, a reduction of only `89,026` bytes (`0.0043%`). The huge raw preview-character count compressed away so effectively inside Tantivy's stored-field path that it did not translate into real index-size savings.
- The stage profile shows the failure clearly: `prepare_ms` and `add_ms` both got much worse, and `finish_conversation_ms` regressed heavily too. The tiny `commit_ms` improvement was nowhere near enough to compensate.
- Conclusion: reject preview-field elision. In this workload, the stored preview path is not the real write-amplification lever anymore.

### Artifacts
- `/tmp/cass-real-bench-20260419-r128-preview-elided/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r128-preview-elided/logs/index.stderr.log`


## Rejected Conversation-Level Title-Prefix Precompute

### Goal
- Precompute `title_prefix` once per conversation and reuse it for every message document in that conversation, instead of regenerating the same edge-ngram string for each message during lexical rebuild.

### Alien/Common-Subexpression Framing
- This was a classic conversation-constant memoization probe. In cass, `title` is constant across all messages in a conversation, so `cass_generate_edge_ngrams(title)` looked like repeated deterministic work on the hot rebuild path.
- The relevant graveyard/optimization pattern is common-subexpression elimination at the ownership boundary: lift a pure derived artifact from per-message work into per-conversation context, then transport it through the existing borrowed-doc pipeline.
- The EV case looked strong on paper because the corpus averages about `91.9` messages per conversation, so each surviving conversation title could have been reused many times.

### Behavior Preservation Proof
- Title search semantics preserved in the candidate: yes. The title text itself was unchanged, and the precomputed prefix payload was built with the exact same `cass_generate_edge_ngrams` helper as the old per-message path.
- Prefix-field bytes preserved in the candidate: yes. `cass_precomputed_title_prefix_matches_runtime_generation` proved that a precomputed `title_prefix` payload matched the runtime-generated payload exactly.
- Golden/replay verification before benchmarking:
  - `cass_precomputed_title_prefix_matches_runtime_generation`
  - `cass_content_field_is_indexed_not_stored`
  - `add_prebuilt_documents_streams_large_payloads_without_dropping_docs`
  - `rebuild_tantivy_from_db_logs_streamed_batch_stats`
  - `title_field_is_searchable`

### Measured Rounds

| Label | Change | Wall s | Conv/s | Msg/s | `message_stream_ms` | `finish_conversation_ms` | `prepare_ms` | `add_ms` | `commit_ms` | `index_size_bytes` | Outcome |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| `r126-prefix-basic-postings-repeat` | retained control | 51.528 | 993.347 | 91286.726 | 47.533 | 6.146 | 2.862 | 2.261 | 2.693 | 2063387147 | control |
| `r131-title-prefix-precompute` | precompute `title_prefix` once per conversation and reuse it across message docs | 52.610 | 972.910 | 89408.600 | 48.005 | 6.095 | 2.788 | 2.147 | 2.704 | 2062980960 | reject |

### Takeaways
- This is not a keeper. `r131` regressed from `51.528s` to `52.610s`, about `2.10%` slower than the retained control, so it was rejected without spending another full repeat run.
- The candidate did improve some local buckets: `prepare_ms` fell from `2.862s` to `2.788s`, `add_ms` from `2.261s` to `2.147s`, and `finish_conversation_ms` edged down from `6.146s` to `6.095s`.
- But the dominant service center moved the wrong way: `message_stream_ms` rose from `47.533s` to `48.005s`. That means the saved per-message prefix generation work was overpaid by the new per-conversation precompute and transport overhead inside the broader rebuild pipeline.
- The rebuilt index shrank slightly (`-406,187` bytes, about `0.02%`), but not enough to matter.
- Conclusion: reject conversation-level `title_prefix` precompute and keep the simpler retained tree.

### Artifacts
- `/tmp/cass-real-bench-20260419-r131-title-prefix-precompute/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r131-title-prefix-precompute/logs/index.stderr.log`


## Rejected Retained-Tree Writer-Thread Retune

### Goal
- Re-test the Tantivy writer-thread count on the current retained tree after the later content externalization, fused normalization, and prefix-postings wins materially changed the per-document service demand.

### Alien/Queueing Framing
- This was a straightforward queueing-theory retune on the write service center. The old `26`-writer default was chosen on a much heavier tree, so the natural hypothesis was that the new retained tree might want a different concurrency point.
- Two adjacent probes were worth real money: `24` as the lower-contention candidate, and `28` as the only nearby higher-throughput neighbor that had ever looked competitive on older baselines.

### Behavior Preservation Proof
- Ordering preserved: yes. This was env-only thread-count tuning; document order, schema, batch boundaries, and query behavior were unchanged.
- Tie-breaking unchanged: yes. No ranking or retrieval semantics changed.
- Floating-point: N/A.
- RNG seeds: unchanged / N/A.
- Golden/replay verification: env-only benchmark sweep on the retained tree; source restored untouched.

### Measured Rounds

| Label | Change | Wall s | Conv/s | Msg/s | `message_stream_ms` | `finish_conversation_ms` | `prepare_ms` | `add_ms` | `commit_ms` | Outcome |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---|
| `r126-prefix-basic-postings-repeat` | retained control (`26` writer threads) | 51.528 | 993.347 | 91286.726 | 47.533 | 6.146 | 2.862 | 2.261 | 2.693 | control |
| `r132-writer24-retained` | env-only `CASS_TANTIVY_MAX_WRITER_THREADS=24` | 52.415 | 976.526 | 89742.968 | 47.903 | 6.161 | 2.781 | 2.140 | 2.772 | reject |
| `r133-writer28-retained` | env-only `CASS_TANTIVY_MAX_WRITER_THREADS=28` | 51.466 | 994.539 | 91396.371 | 47.807 | 6.093 | 2.816 | 2.172 | 2.646 | inconclusive first hit |
| `r134-writer28-retained-repeat` | repeat of the same `28`-writer env override | 52.636 | 972.434 | 89366.477 | 48.192 | 6.056 | 2.839 | 2.174 | 2.604 | reject |

### Takeaways
- `24` is a clean loser on the current retained tree: `52.415s` versus `51.528s`, about `1.72%` slower than control.
- `28` looked like a tiny single-run win (`51.466s`, about `0.12%` faster), but the repeat lost hard enough (`52.636s`, about `2.15%` slower) that the two-run mean is still worse than control by about `1.02%`.
- The profile shape explains why this branch is not the frontier anymore. `24` reduced `prepare_ms` and `add_ms`, but paid it back in worse `message_stream_ms` and `commit_ms`. `28` slightly improved `commit_ms`, but both runs worsened `message_stream_ms` and `prepare_ms` relative to the retained control.
- Conclusion: keep the existing `26`-writer retained tree. On the current workload, writer-pool sizing is not the next keeper.

### Artifacts
- `/tmp/cass-real-bench-20260419-r132-writer24-retained/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r132-writer24-retained/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260419-r133-writer28-retained/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r133-writer28-retained/logs/index.stderr.log`
- `/tmp/cass-real-bench-20260419-r134-writer28-retained-repeat/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r134-writer28-retained-repeat/logs/index.stderr.log`

## Rejected HyphenDecompose State-Machine Rewrite

- Date: 2026-04-19
- Labels: `r126-prefix-basic-postings-repeat` (retained control), `r135-hyphen-decompose-state` (candidate)

### Goal

- Remove the remaining avoidable allocation churn inside `HyphenDecompose` by replacing the `contains('-') + split('-') + collect::<Vec<_>>() + token.clone()` path with a direct reverse byte scan plus an explicit compound/parts state machine.

### Alien / Optimization Framing

- `extreme-software-optimization`: this was a classic buffer-reuse / allocation-elision probe on a still-hot analyzer stack.
- `alien-artifact-coding`: the proof obligation was exact token-stream isomorphism, especially preserving compound-first emission followed by left-to-right sub-parts at the same position.
- `alien-graveyard`: the relevant primitive was simple hot-loop allocation suppression, not a heavier data-structure swap.

### Behavior Preservation Proof

- Added a focused analyzer test for the exact expected stream order: `cass_hyphen_decompose_emits_compound_then_parts`.
- Re-ran the broader retained tokenizer/analyzer guards before benchmarking:
  - `cargo fmt --check` in `/data/projects/frankensearch`
  - `cargo test -p frankensearch-lexical cass_hyphen_decompose_emits_compound_then_parts -- --nocapture`
  - `cargo test -p frankensearch-lexical cass_tokenizer_matches_legacy_regex_boundaries -- --nocapture`
  - `cargo test -p frankensearch-lexical cass_normalize_and_limit_matches_legacy_pipeline -- --nocapture`

### Benchmark Result

| Label | Change | Wall s | Conv/s | Msg/s | `message_stream_ms` | `finish_conversation_ms` | `prepare_ms` | `add_ms` | `commit_ms` | `index_size_bytes` | Outcome |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| `r126-prefix-basic-postings-repeat` | retained control | 51.528 | 993.347 | 91286.726 | 47.533 | 6.146 | 2.862 | 2.261 | 2.693 | 2063387147 | control |
| `r135-hyphen-decompose-state` | no-`Vec`, no-compound-clone `HyphenDecompose` state machine | 52.548 | 974.054 | 89513.723 | 47.907 | 6.289 | 2.999 | 2.392 | 2.669 | 2063082645 | reject |

### Interpretation

- The candidate lost clearly enough that it did not earn a repeat: `52.548s` versus retained `51.528s`, about `1.98%` slower.
- The tiny index-size reduction (`-304,502` bytes, about `0.015%`) was noise relative to the runtime loss.
- The loss shows up in the hot buckets that matter most to rebuild throughput:
  - `message_stream_ms`: `47.533s -> 47.907s`
  - `finish_conversation_ms`: `6.146s -> 6.289s`
  - `prepare_ms`: `2.862s -> 2.999s`
  - `add_ms`: `2.261s -> 2.392s`
  - `commit_ms` improved slightly (`2.693s -> 2.669s`) but nowhere near enough to pay for the extra work elsewhere.
- Conclusion: reject the `HyphenDecompose` state-machine rewrite and restore the prior retained tree.

### Artifacts

- `/tmp/cass-real-bench-20260419-r135-hyphen-decompose-state/logs/summary.json`
- `/tmp/cass-real-bench-20260419-r135-hyphen-decompose-state/logs/index.stderr.log`
