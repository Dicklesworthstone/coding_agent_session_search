# Cass Bake-off Validation

## Purpose
Validate bake-off winners against the cass benchmark corpus with a focused end-to-end run.
This checks search quality (NDCG@10) and latency (p50/p95) on a subset of the xf
benchmark corpus, using cass indexing + search paths.

## Inputs
- Corpus: `xf/tests/fixtures/benchmark_corpus.json`
- Generated sessions: Factory connector JSONL under `<data_dir>/.factory/sessions/<slug>/...`
- Queries: corpus `queries[]` subset

## How to run
```bash
./scripts/bakeoff/cass_validation_e2e.sh
```

## Common overrides
```bash
# Smaller run for quick smoke validation
MAX_DOCS=200 MAX_QUERIES=20 ./scripts/bakeoff/cass_validation_e2e.sh

# Smoke preset (overrides sizes + disables doc append)
SMOKE=1 ./scripts/bakeoff/cass_validation_e2e.sh

# Force hybrid mode + reranking
MODE=hybrid RERANK=1 ./scripts/bakeoff/cass_validation_e2e.sh

# Use a specific model + daemon
MODE=semantic MODEL=minilm DAEMON=1 ./scripts/bakeoff/cass_validation_e2e.sh
```

## Environment variables
- `CASS_BIN`: cass binary or command (defaults to release/debug/cass, then `cass`, then `cargo run -q --`)
- `CORPUS_PATH`: corpus JSON path
- `DATA_DIR`: cass data dir for the run
- `MAX_DOCS`, `MAX_QUERIES`: corpus/query subset sizes
- `LIMIT`: search limit (default 10)
- `MODE`: `semantic`, `hybrid`, or `lexical`
- `MODEL`: embedder model (optional)
- `RERANK`: set `1` to enable reranking
- `RERANKER`: reranker model name
- `DAEMON`: set `1` to enable daemon
- `NO_DAEMON`: set `1` to disable daemon
- `NDCG_MIN`: minimum acceptable NDCG@10 (default 0.25)
- `LATENCY_P95_MAX_MS`: max acceptable p95 latency (default 500)
- `STRICT`: set `1` to fail if thresholds are not met
- `SMOKE`: set `1` to use a quick smoke preset (reduces sizes, disables doc append)
- `REPORT_JSON`: output report path
- `REPORT_DOC`: doc to append summary
- `APPEND_DOCS`: set `1` to append summary to this file

## Outputs
- JSON report: `<data_dir>/validation_report.json`
- Per-query diagnostics: `<data_dir>/per_query_scores.json`
- Log file: `<data_dir>/validation.log`

## Notes
- The script isolates indexing to the generated Factory sessions by running `cass index`
  with `HOME=<data_dir>` and `CASS_IGNORE_SOURCES_CONFIG=1`.
- If `RERANK=1` but the reranker model files are missing under
  `<data_dir>/models/ms-marco-MiniLM-L-6-v2`, rerank is auto-disabled and a warning
  is recorded in the report.

## Report schema
```json
{
  "model_id": "minilm",
  "corpus_hash": "<sha256>",
  "ndcg_at_10": 0.42,
  "latency_ms_p50": 12,
  "latency_ms_p95": 30,
  "eligible": true,
  "warnings": ["..."],
  "run_id": "20260126T021000Z",
  "timestamp": "2026-01-26T02:10:00Z",
  "query_count": 50,
  "mode": "semantic",
  "limit": 10,
  "data_dir": "/path/to/run",
  "rerank": false,
  "reranker": null,
  "daemon": false,
  "no_daemon": false
}
```

## Eligibility rules
- `eligible = (ndcg_at_10 >= NDCG_MIN) && (latency_ms_p95 <= LATENCY_P95_MAX_MS)`
- If `STRICT=0`, eligibility failures are allowed but recorded as warnings

## Run history
## Run 20260126T040812Z
- Timestamp: 2026-01-26T04:12:00.384740Z
- Model: minilm
- Mode: semantic
- Rerank: false (reranker: null)
- Daemon: false (no_daemon: true)
- NDCG@10: 0.070061
- Latency p50: 2960 ms
- Latency p95: 13156 ms
- Eligible: false
- Warnings:
  - ndcg_at_10 below threshold (0.0701 < 0.25)
  - latency_p95 above threshold (13155.56ms > 500.0ms)
- Notes: reranker model not installed; rerank disabled for this run

## Run 20260130T030942Z
- Timestamp: 2026-01-30T03:10:05.196868Z
- Model: auto
- Mode: lexical
- Rerank: False (reranker: None)
- Daemon: False (no_daemon: False)
- NDCG@10: 0.0
- Latency p50: 339 ms
- Latency p95: 432 ms
- Eligible: False
- Warnings:
  - ndcg_at_10 below threshold (0.0000 < 0.25)
  - cutoff exception: STRICT=0

## Run 20260130T031632Z
- Timestamp: 2026-01-30T03:16:52.362967Z
- Model: auto
- Mode: lexical
- Rerank: False (reranker: None)
- Daemon: False (no_daemon: False)
- NDCG@10: 0.0
- Latency p50: 928 ms
- Latency p95: 1091 ms
- Eligible: False
- Warnings:
  - ndcg_at_10 below threshold (0.0000 < 0.25)
  - latency_p95 above threshold (1091.17ms > 500.0ms)
  - cutoff exception: STRICT=0

## Run 20260130T031904Z
- Timestamp: 2026-01-30T03:19:27.285658Z
- Model: auto
- Mode: lexical
- Rerank: False (reranker: None)
- Daemon: False (no_daemon: False)
- NDCG@10: 0.0
- Latency p50: 810 ms
- Latency p95: 884 ms
- Eligible: False
- Warnings:
  - ndcg_at_10 below threshold (0.0000 < 0.25)
  - latency_p95 above threshold (883.81ms > 500.0ms)
  - cutoff exception: STRICT=0

## Run 20260130T032108Z
- Timestamp: 2026-01-30T03:21:24.076651Z
- Model: auto
- Mode: lexical
- Rerank: False (reranker: None)
- Daemon: False (no_daemon: False)
- NDCG@10: 0.041154
- Latency p50: 224 ms
- Latency p95: 270 ms
- Eligible: False
- Warnings:
  - ndcg_at_10 below threshold (0.0412 < 0.25)
  - cutoff exception: STRICT=0

## Run 20260130T032714Z
- Timestamp: 2026-01-30T03:27:24.220879Z
- Model: auto
- Mode: semantic
- Rerank: False (reranker: None)
- Daemon: False (no_daemon: True)
- NDCG@10: 0.117723
- Latency p50: 255 ms
- Latency p95: 454 ms
- Eligible: False
- Warnings:
  - ndcg_at_10 below threshold (0.1177 < 0.25)
  - cutoff exception: STRICT=0
