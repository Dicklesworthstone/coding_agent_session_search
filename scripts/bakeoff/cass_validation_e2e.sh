#!/usr/bin/env bash
# cass_validation_e2e.sh — End-to-end validation for cass bake-off winners (bd-39th)
#
# This script:
#   1) Generates a temporary Factory connector dataset from the xf benchmark corpus
#   2) Indexes it with cass
#   3) Runs semantic/hybrid searches to compute NDCG@10 and latency
#   4) Emits a JSON report and optionally appends a summary to docs
#
# Usage:
#   ./scripts/bakeoff/cass_validation_e2e.sh
#
# Environment:
#   CASS_BIN             - cass binary/command (default: target/release/cass, target/debug/cass, or cass on PATH)
#   CORPUS_PATH          - benchmark corpus JSON (default: /data/projects/xf/tests/fixtures/benchmark_corpus.json)
#   DATA_DIR             - cass data dir to use (default: tmp/bakeoff_validation_<timestamp>)
#   MAX_DOCS             - number of corpus docs to index (default: 500)
#   MAX_QUERIES          - number of queries to evaluate (default: 50)
#   LIMIT                - search limit (default: 10)
#   MODE                 - search mode: semantic|hybrid|lexical (default: semantic)
#   EMBEDDER             - embedder for index: hash, fastembed (default: fastembed)
#   MODEL                - embedder model name for semantic search (default: empty/auto)
#   RERANK               - set to 1 to enable reranking (default: 0)
#   RERANKER             - reranker model name (optional)
#   DAEMON               - set to 1 to enable daemon (default: 0)
#   NO_DAEMON            - set to 1 to disable daemon (default: 0)
#   NDCG_MIN             - minimum acceptable NDCG@10 (default: 0.25)
#   LATENCY_P95_MAX_MS   - max acceptable p95 latency (default: 500)
#   STRICT               - set to 1 to fail if thresholds not met (default: 1)
#   SMOKE                - set to 1 for quick smoke run (overrides MAX_DOCS/MAX_QUERIES/LIMIT, disables doc append)
#   REPORT_JSON          - path for JSON report (default: <DATA_DIR>/validation_report.json)
#   REPORT_DOC           - doc path to append summary (default: docs/reference/cass_bakeoff_validation.md)
#   APPEND_DOCS          - set to 1 to append summary to docs (default: 1)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
RUN_ID="$(date -u +%Y%m%dT%H%M%SZ)"

CORPUS_PATH="${CORPUS_PATH:-/data/projects/xf/tests/fixtures/benchmark_corpus.json}"
DATA_DIR="${DATA_DIR:-$REPO_ROOT/tmp/bakeoff_validation_$RUN_ID}"
MAX_DOCS="${MAX_DOCS:-500}"
MAX_QUERIES="${MAX_QUERIES:-50}"
LIMIT="${LIMIT:-10}"
MODE="${MODE:-semantic}"
EMBEDDER="${EMBEDDER:-fastembed}"
MODEL="${MODEL:-}"
RERANK="${RERANK:-0}"
RERANKER="${RERANKER:-}"
DAEMON="${DAEMON:-0}"
NO_DAEMON="${NO_DAEMON:-0}"
NDCG_MIN="${NDCG_MIN:-0.25}"
LATENCY_P95_MAX_MS="${LATENCY_P95_MAX_MS:-500}"
STRICT="${STRICT:-1}"
SMOKE="${SMOKE:-0}"
REPORT_JSON="${REPORT_JSON:-$DATA_DIR/validation_report.json}"
REPORT_DOC="${REPORT_DOC:-$REPO_ROOT/docs/reference/cass_bakeoff_validation.md}"
APPEND_DOCS="${APPEND_DOCS:-1}"

LOG_FILE="$DATA_DIR/validation.log"
WORKSPACE_SLUG="${WORKSPACE_SLUG:--tmp-cass-validation}"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

log_info() { echo -e "${BLUE}[INFO]${NC} $*" | tee -a "$LOG_FILE"; }
log_warn() { echo -e "${YELLOW}[WARN]${NC} $*" | tee -a "$LOG_FILE"; }
log_fail() { echo -e "${RED}[FAIL]${NC} $*" | tee -a "$LOG_FILE"; }
log_pass() { echo -e "${GREEN}[PASS]${NC} $*" | tee -a "$LOG_FILE"; }

mkdir -p "$DATA_DIR"
: > "$LOG_FILE"

if [[ "$SMOKE" == "1" ]]; then
    MAX_DOCS=200
    MAX_QUERIES=20
    LIMIT=5
    APPEND_DOCS=0
fi

export CORPUS_PATH DATA_DIR MAX_DOCS MAX_QUERIES LIMIT MODE EMBEDDER MODEL RERANK RERANKER DAEMON NO_DAEMON
export NDCG_MIN LATENCY_P95_MAX_MS STRICT REPORT_JSON REPORT_DOC APPEND_DOCS RUN_ID WORKSPACE_SLUG SMOKE

if [[ ! -f "$CORPUS_PATH" ]]; then
    log_fail "Corpus file not found: $CORPUS_PATH"
    exit 1
fi

if [[ -z "${CASS_BIN:-}" ]]; then
    if [[ -x "$REPO_ROOT/target/release/cass" ]]; then
        CASS_BIN="$REPO_ROOT/target/release/cass"
    elif [[ -x "$REPO_ROOT/target/debug/cass" ]]; then
        CASS_BIN="$REPO_ROOT/target/debug/cass"
    elif command -v cass >/dev/null 2>&1; then
        CASS_BIN="$(command -v cass)"
    else
        log_fail "cass binary not found. Build via rch, then rerun this script with CASS_BIN=/path/to/cass"
        exit 1
    fi
fi
export CASS_BIN

cass_cmd() {
    # shellcheck disable=SC2086
    $CASS_BIN "$@"
}

log_info "Cass validation run: $RUN_ID"
log_info "Corpus: $CORPUS_PATH"
log_info "Data dir: $DATA_DIR"
log_info "Mode: $MODE"
log_info "Embedder: $EMBEDDER"
log_info "Model: ${MODEL:-auto}"
log_info "Rerank: $RERANK (reranker: ${RERANKER:-auto})"
log_info "Daemon: $DAEMON (no_daemon: $NO_DAEMON)"
log_info "Docs: $REPORT_DOC"
if [[ "$SMOKE" == "1" ]]; then
    log_warn "Smoke mode enabled (MAX_DOCS=$MAX_DOCS, MAX_QUERIES=$MAX_QUERIES, LIMIT=$LIMIT, APPEND_DOCS=0)"
fi
log_info "Index isolation: HOME=$DATA_DIR (local-only), CASS_IGNORE_SOURCES_CONFIG=1"

# If reranking is requested, ensure the local model bundle exists.
RERANK_MISSING=0
if [[ "$RERANK" == "1" ]]; then
    RERANK_DIR="$DATA_DIR/models/ms-marco-MiniLM-L-6-v2"
    REQUIRED_RERANK_FILES=(
        "model.safetensors"
        "tokenizer.json"
        "config.json"
        "special_tokens_map.json"
        "tokenizer_config.json"
    )
    MISSING=()
    for f in "${REQUIRED_RERANK_FILES[@]}"; do
        if [[ ! -f "$RERANK_DIR/$f" ]]; then
            MISSING+=("$f")
        fi
    done
    if [[ ${#MISSING[@]} -gt 0 ]]; then
        log_warn "Reranker model missing in $RERANK_DIR (${MISSING[*]}). Disabling rerank for this run."
        RERANK=0
        RERANKER=""
        RERANK_MISSING=1
    fi
fi
export RERANK_MISSING RERANK RERANKER

# Generate Factory sessions from corpus
log_info "Generating Factory sessions (max docs: $MAX_DOCS)"
python3 - <<'PY' 2>&1 | tee -a "$LOG_FILE"
import json
import os
from pathlib import Path
from datetime import datetime, timezone

corpus_path = Path(os.environ["CORPUS_PATH"])
data_dir = Path(os.environ["DATA_DIR"])
max_docs = int(os.environ.get("MAX_DOCS", "500"))
workspace_slug = os.environ.get("WORKSPACE_SLUG", "-tmp-cass-validation")

with corpus_path.open("r", encoding="utf-8") as f:
    corpus = json.load(f)

docs = corpus.get("corpus", [])[:max_docs]

sessions_root = data_dir / ".factory" / "sessions" / workspace_slug
sessions_root.mkdir(parents=True, exist_ok=True)

now = datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")

for doc in docs:
    doc_id = str(doc.get("id", "unknown"))
    text = doc.get("text", "")
    session_path = sessions_root / f"{doc_id}.jsonl"

    session_start = {
        "type": "session_start",
        "id": doc_id,
        "title": f"cass-validation {doc_id}",
        "owner": "cass_validation",
        "cwd": "/tmp/cass_validation",
    }
    message = {
        "type": "message",
        "timestamp": now,
        "message": {
            "role": "assistant",
            "content": text,
        },
    }

    with session_path.open("w", encoding="utf-8") as out:
        out.write(json.dumps(session_start, ensure_ascii=False))
        out.write("\n")
        out.write(json.dumps(message, ensure_ascii=False))
        out.write("\n")

print(f"Wrote {len(docs)} session files to {sessions_root}")
PY

# Index data
log_info "Running cass index"
INDEX_ARGS=("index" "--full" "--data-dir" "$DATA_DIR" "--json")
if [[ "$MODE" == "semantic" || "$MODE" == "hybrid" ]]; then
    INDEX_ARGS+=("--semantic" "--embedder" "$EMBEDDER")
fi

INDEX_OUTPUT="$DATA_DIR/index_output.json"
if HOME="$DATA_DIR" CASS_IGNORE_SOURCES_CONFIG=1 cass_cmd "${INDEX_ARGS[@]}" >"$INDEX_OUTPUT" 2>>"$LOG_FILE"; then
    log_pass "Index completed"
else
    log_fail "Index failed (see $LOG_FILE)"
    exit 1
fi

# Compute metrics with python (runs cass search for each query)
log_info "Running validation queries (max queries: $MAX_QUERIES, limit: $LIMIT)"
python3 - <<'PY' 2>&1 | tee -a "$LOG_FILE"
import json
import os
import shlex
import subprocess
import time
from pathlib import Path
from datetime import datetime, timezone

corpus_path = Path(os.environ["CORPUS_PATH"])
data_dir = os.environ["DATA_DIR"]
limit = int(os.environ.get("LIMIT", "10"))
mode = os.environ.get("MODE", "semantic")
model = os.environ.get("MODEL", "").strip()
rerank = os.environ.get("RERANK", "0") == "1"
reranker = os.environ.get("RERANKER", "").strip()
rerank_missing = os.environ.get("RERANK_MISSING", "0") == "1"
daemon = os.environ.get("DAEMON", "0") == "1"
no_daemon = os.environ.get("NO_DAEMON", "0") == "1"
max_queries = int(os.environ.get("MAX_QUERIES", "50"))
ndcg_min = float(os.environ.get("NDCG_MIN", "0.25"))
latency_p95_max_ms = float(os.environ.get("LATENCY_P95_MAX_MS", "500"))
strict = os.environ.get("STRICT", "1") == "1"
report_json = Path(os.environ["REPORT_JSON"])
run_id = os.environ.get("RUN_ID", "unknown")

base_cmd = shlex.split(os.environ.get("CASS_BIN", "cass"))

with corpus_path.open("r", encoding="utf-8") as f:
    corpus = json.load(f)

queries = corpus.get("queries", [])[:max_queries]
doc_ids = {str(d.get("id")) for d in corpus.get("corpus", [])[:int(os.environ.get("MAX_DOCS", "500"))]}

latencies = []
ndcg_scores = []
per_query = []
errors = []


def dcg_at_k(rels, k):
    out = 0.0
    import math
    for i, rel in enumerate(rels[:k]):
        rel = rel if (isinstance(rel, (int, float)) and rel == rel) else 0.0
        if rel < 0:
            rel = 0.0
        denom = math.log2(i + 2)
        out += (2.0 ** rel - 1.0) / denom
    return out


def ndcg_at_k(ranked_rels, ideal_rels, k):
    dcg = dcg_at_k(ranked_rels, k)
    if dcg == 0.0:
        return 0.0
    ideal_sorted = sorted([r for r in ideal_rels if r == r and r > 0], reverse=True)
    idcg = dcg_at_k(ideal_sorted, k)
    return 0.0 if idcg == 0.0 else dcg / idcg


def percentile(values, pct):
    if not values:
        return 0.0
    values = sorted(values)
    if len(values) == 1:
        return float(values[0])
    k = (len(values) - 1) * (pct / 100.0)
    f = int(k)
    c = min(f + 1, len(values) - 1)
    if f == c:
        return float(values[f])
    return float(values[f] + (values[c] - values[f]) * (k - f))


for q in queries:
    q_raw = str(q.get("text", "")).strip()
    if not q_raw:
        continue
    # Extract the topic keyword (first word) from synthetic queries like "metrics search query 0"
    # This matches how the corpus generator creates queries: f"{topic} search query {i}"
    q_text = q_raw.split()[0] if q_raw else q_raw
    relevants = q.get("relevants", {}) or {}
    relevants = {k: v for k, v in relevants.items() if str(k) in doc_ids}
    ideal_rels = list(relevants.values())

    cmd = base_cmd + [
        "search",
        q_text,
        "--limit",
        str(limit),
        "--robot",
        "--fields",
        "minimal",
        "--data-dir",
        data_dir,
        "--mode",
        mode,
    ]

    if model:
        cmd += ["--model", model]
    if rerank:
        cmd += ["--rerank"]
    if reranker:
        cmd += ["--reranker", reranker]
    if daemon:
        cmd += ["--daemon"]
    if no_daemon:
        cmd += ["--no-daemon"]

    # Pass CASS_DATA_DIR to ensure cass finds the correct index
    env = os.environ.copy()
    env["CASS_DATA_DIR"] = data_dir

    start = time.perf_counter()
    proc = subprocess.run(cmd, capture_output=True, text=True, env=env)
    elapsed_ms = (time.perf_counter() - start) * 1000.0

    if proc.returncode != 0:
        errors.append({"query": q_text, "error": proc.stderr.strip()})
        per_query.append({
            "query": q_text,
            "ndcg_at_10": 0.0,
            "latency_ms": elapsed_ms,
            "hits": 0,
            "error": proc.stderr.strip(),
        })
        latencies.append(elapsed_ms)
        ndcg_scores.append(0.0)
        continue

    try:
        payload = json.loads(proc.stdout.strip() or "{}")
    except json.JSONDecodeError as e:
        errors.append({"query": q_text, "error": f"json parse error: {e}"})
        per_query.append({
            "query": q_text,
            "ndcg_at_10": 0.0,
            "latency_ms": elapsed_ms,
            "hits": 0,
            "error": f"json parse error: {e}",
        })
        latencies.append(elapsed_ms)
        ndcg_scores.append(0.0)
        continue

    hits = payload.get("hits", []) or []
    ranked_rels = []
    for hit in hits[:limit]:
        source_path = str(hit.get("source_path", ""))
        doc_id = Path(source_path).stem
        ranked_rels.append(float(relevants.get(doc_id, 0.0)))

    score = ndcg_at_k(ranked_rels, ideal_rels, 10)
    latencies.append(elapsed_ms)
    ndcg_scores.append(score)

    per_query.append({
        "query": q_text,
        "ndcg_at_10": score,
        "latency_ms": elapsed_ms,
        "hits": len(hits),
    })

avg_ndcg = sum(ndcg_scores) / len(ndcg_scores) if ndcg_scores else 0.0
p50 = percentile(latencies, 50)
p95 = percentile(latencies, 95)

warnings = []
eligible = True
if avg_ndcg < ndcg_min:
    eligible = False
    warnings.append(f"ndcg_at_10 below threshold ({avg_ndcg:.4f} < {ndcg_min})")
if p95 > latency_p95_max_ms:
    eligible = False
    warnings.append(f"latency_p95 above threshold ({p95:.2f}ms > {latency_p95_max_ms}ms)")
if errors:
    warnings.append(f"query errors: {len(errors)}")
if rerank_missing:
    warnings.append("reranker model missing; rerank disabled")
if not strict:
    warnings.append("cutoff exception: STRICT=0")

report = {
    "model_id": model or "auto",
    "corpus_hash": __import__("hashlib").sha256(corpus_path.read_bytes()).hexdigest(),
    "ndcg_at_10": round(avg_ndcg, 6),
    "latency_ms_p50": int(round(p50)),
    "latency_ms_p95": int(round(p95)),
    "eligible": eligible,
    "warnings": warnings,
    "run_id": run_id,
    "timestamp": datetime.now(timezone.utc).isoformat().replace("+00:00", "Z"),
    "query_count": len(ndcg_scores),
    "mode": mode,
    "limit": limit,
    "data_dir": data_dir,
    "rerank": rerank,
    "reranker": reranker or None,
    "daemon": daemon,
    "no_daemon": no_daemon,
}

report_json.parent.mkdir(parents=True, exist_ok=True)
report_json.write_text(json.dumps(report, indent=2), encoding="utf-8")

# Write per-query diagnostics
per_query_path = report_json.with_name("per_query_scores.json")
per_query_path.write_text(json.dumps(per_query, indent=2), encoding="utf-8")

print(f"Report written to: {report_json}")
print(f"Per-query scores: {per_query_path}")
print(f"NDCG@10: {avg_ndcg:.4f} | p50: {p50:.2f}ms | p95: {p95:.2f}ms | eligible: {eligible}")

if strict and not eligible:
    raise SystemExit(2)
PY

# Append to docs if requested
if [[ "$APPEND_DOCS" == "1" ]]; then
    if [[ -f "$REPORT_DOC" ]]; then
        log_info "Appending summary to $REPORT_DOC"
        python3 - <<'PY' >> "$REPORT_DOC"
import json
import os
from datetime import datetime, timezone

report_path = os.environ["REPORT_JSON"]
run_id = os.environ.get("RUN_ID", "unknown")

with open(report_path, "r", encoding="utf-8") as f:
    report = json.load(f)

stamp = datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")

print("\n## Run " + run_id)
print(f"- Timestamp: {stamp}")
print(f"- Model: {report.get('model_id')}")
print(f"- Mode: {report.get('mode')}")
print(f"- Rerank: {report.get('rerank')} (reranker: {report.get('reranker')})")
print(f"- Daemon: {report.get('daemon')} (no_daemon: {report.get('no_daemon')})")
print(f"- NDCG@10: {report.get('ndcg_at_10')}")
print(f"- Latency p50: {report.get('latency_ms_p50')} ms")
print(f"- Latency p95: {report.get('latency_ms_p95')} ms")
print(f"- Eligible: {report.get('eligible')}")

warnings = report.get("warnings", []) or []
if warnings:
    print("- Warnings:")
    for w in warnings:
        print(f"  - {w}")
PY
    else
        log_warn "Report doc not found: $REPORT_DOC (skipping append)"
    fi
fi

log_info "Validation complete"
log_warn "Temp data dir preserved: $DATA_DIR (remove manually if desired)"
