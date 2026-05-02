# CASS lexical rebuild finalization perf slice

Date: 2026-05-02

## Workload

- Command shape: `cass index --watch-once <nonexistent> --data-dir <seeded-data-dir> --json --progress-interval-ms 10000`
- Seed database: `/home/ubuntu/cass-post-tokenizer-hotspot-20260502T035907Z/agent_search.db*`
- Corpus: 51,214 conversations / 4,711,686 messages
- Binary: `/tmp/cass_perf_opt_target/profiling/cass`

## Baseline

Artifact: `tests/artifacts/perf/cass-ascii-byte-scan-pinned-ab-20260502T042728Z`

- `elapsed_ms`: 95,166
- Wall time: 1:36.17
- Full corpus reached: 34,724 ms
- Max RSS: 60,302,852 KB
- Staged merge workers max: 4

## Final candidate

Artifact: `tests/artifacts/perf/cass-final-default-summary-merge8-seeded-20260502T053345Z`

- `elapsed_ms`: 90,863
- Wall time: 1:31.67
- Full corpus reached: 34,624 ms
- Max RSS: 60,317,720 KB
- Staged merge workers max: 8

## Delta

- Total `elapsed_ms`: 4.7% faster (`95,166 -> 90,863`)
- Wall time: 4.7% faster (`96.17s -> 91.67s`)
- Full-corpus handoff: essentially unchanged (`34.724s -> 34.624s`)
- RSS: essentially unchanged (`60,302,852 KB -> 60,317,720 KB`)

## Interpretation

The win is in the post-handoff rebuild tail, not message ingestion. The shipped
change carries already-validated shard summaries through staged artifacts so the
final federated publish can avoid redundant shard summary opens, and raises the
default staged merge worker cap from 4 to the measured best point, 8.

Rejected points:

- ASCII byte scanner in frankensearch: regressed to `elapsed_ms=109,085`.
- Hyphen no-`Vec<&str>` tokenizer tweak: behaviorally safe but noise-level result (`95,165` vs pinned `95,166`).
- 16 staged merge workers: regressed relative to 8 (`94,368` / wall `1:35.37`).
