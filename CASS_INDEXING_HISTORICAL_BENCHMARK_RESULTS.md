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
- All runs below used a fresh temporary `--data-dir`, so they measure full lexical Tantivy rebuild from the canonical SQLite DB.
- `--force-rebuild` on an already-populated canonical DB intentionally takes the canonical-only rebuild path.
- The machine is shared, so CPU availability varies somewhat across runs. For that reason, repeated runs are recorded explicitly instead of relying on a single sample.

## Results

| Label | Binary/Profile | Code State | Wall s | Conv/s | Msg/s | DB MB/s | Avg Proc CPU % | Peak RSS GiB |
|---|---|---|---:|---:|---:|---:|---:|---:|
| `current-baseline` | release `opt-level="z"` | streamed rebuild path before speed-profile change | 164.269 | 311.592 | 28634.704 | 130.026 | 875.380 | 19.27 |
| `opt3-baseline` | release `opt-level=3` | speed-first release profile | 151.269 | 338.372 | 31095.704 | 141.201 | 710.115 | 18.94 |
| `opt3-lazy-canonicalonly` | release `opt-level=3` | defer source sync + scan-root discovery; skip historical-bundle probe on canonical-only rebuild | 123.906 | 413.096 | 37962.769 | 172.384 | 767.015 | 19.06 |
| `opt3-lazy-canonicalonly-r2` | release `opt-level=3` | repeat of previous row for stability check | 123.922 | 413.042 | 37957.790 | 172.361 | 763.924 | 19.06 |
| `opt3-lazy-canonicalonly-rwprobefix` | release `opt-level=3` | plus readonly/no-checkpoint fast schema probe | 124.926 | 409.721 | 37652.574 | 170.975 | 762.290 | 19.09 |

## Phase Timing Breakdown

### `opt3-baseline`
- preparing -> indexing: `24.018s`
- indexing start -> `current=51185`: `108.080s`
- phase reset after indexing: `132.399s`
- completed payload emitted: `148.111s`
- observed shutdown tail after phase reset: `15.712s`

### `opt3-lazy-canonicalonly`
- preparing -> indexing: `23.416s`
- indexing start -> `current=51185`: `82.059s`
- phase reset after indexing: `107.277s`
- completed payload emitted: `121.087s`
- observed shutdown tail after phase reset: `13.810s`

### `opt3-lazy-canonicalonly-r2`
- preparing -> indexing: `23.218s`
- indexing start -> `current=51185`: `82.059s`
- phase reset after indexing: `107.178s`
- completed payload emitted: `121.190s`
- observed shutdown tail after phase reset: `14.012s`

### `opt3-lazy-canonicalonly-rwprobefix`
- preparing -> indexing: `23.817s`
- indexing start -> `current=51185`: `84.062s`
- phase reset after indexing: `108.280s`
- completed payload emitted: `122.089s`
- observed shutdown tail after phase reset: `13.809s`

## Takeaways
- Changing the shipping release profile from size-optimized (`opt-level="z"`) to speed-optimized (`opt-level=3`) produced a clear improvement: `164.269s -> 151.269s` (`~7.9%` faster wall clock).
- The later canonical-only fast-path cleanup produced a much larger observed improvement versus the original `opt3` baseline: `151.269s -> ~123.914s` (`~18.1%` faster wall clock across the two stable repeat runs).
- The readonly/no-checkpoint schema-probe cleanup is behaviorally cleaner, but benchmark impact was effectively neutral. It should be treated as a correctness/cleanliness improvement, not a meaningful throughput win.
- After the stable `~123.9s` runs, the remaining dominant fixed costs are still:
  - pre-index prepare time: `~23.2s`
  - post-index shutdown tail: `~13.8-14.0s`
- Even at `~123.9s`, the process is not saturating the machine. Average process CPU stayed around `~764-767%`, which is only about `7.6` fully busy cores on average.

## Artifacts
- `/tmp/cass-real-bench-20260417-current-baseline/logs/summary.json`
- `/tmp/cass-real-bench-20260417-opt3-baseline/logs/summary.json`
- `/tmp/cass-real-bench-20260417-opt3-lazy-canonicalonly/logs/summary.json`
- `/tmp/cass-real-bench-20260417-opt3-lazy-canonicalonly-r2/logs/summary.json`
- `/tmp/cass-real-bench-20260417-opt3-lazy-canonicalonly-rwprobefix/logs/summary.json`
