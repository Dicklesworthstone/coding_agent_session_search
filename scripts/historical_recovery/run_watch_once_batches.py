#!/usr/bin/env python3

import argparse
from collections import deque
import hashlib
import json
import os
import sqlite3
import subprocess
import sys
import threading
import time
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple

LEGACY_STATE_BASENAME = "watch_once_batches_state.json"
LEGACY_LOG_BASENAME = "watch_once_batches_log.jsonl"
MIN_MEMORY_BUDGET_KB = 256 * 1024
TOTAL_MEMORY_SOFT_CAP_FRACTION = 0.10
TOTAL_MEMORY_HARD_CAP_FRACTION = 0.18
STREAM_TAIL_CHARS = 20_000


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Run native 'cass index --watch-once' in resumable batches so large raw "
            "session trees can be reconciled without whole-root OOM failures."
        )
    )
    parser.add_argument(
        "--cass-binary",
        default="/data/projects/.cargo-target-cass-release/release/cass",
        help="Path to the cass binary to invoke.",
    )
    parser.add_argument(
        "--data-dir",
        required=True,
        help="cass data dir that contains agent_search.db.",
    )
    parser.add_argument(
        "--root",
        action="append",
        default=[],
        help="Root to scan for raw session files. Repeatable.",
    )
    parser.add_argument(
        "--pattern",
        action="append",
        default=[],
        help="Glob pattern relative to each root, e.g. '**/*.jsonl'. Repeatable.",
    )
    parser.add_argument(
        "--paths-file",
        action="append",
        default=[],
        help="Newline-delimited file containing exact paths to process. Repeatable.",
    )
    parser.add_argument(
        "--batch-size",
        type=int,
        default=32,
        help="Initial number of files to pass to each watch-once invocation.",
    )
    parser.add_argument(
        "--max-batch-size",
        type=int,
        default=256,
        help="Largest batch size the autotuner is allowed to reach.",
    )
    parser.add_argument(
        "--min-batch-size",
        type=int,
        default=1,
        help="Smallest batch size allowed when shrinking after failures.",
    )
    parser.add_argument(
        "--max-batches",
        type=int,
        default=None,
        help="Optional cap on successful batches for a single run.",
    )
    parser.add_argument(
        "--start-index",
        type=int,
        default=None,
        help="Override the resume position instead of using the saved state file.",
    )
    parser.add_argument(
        "--state-file",
        default=None,
        help="JSON file that stores resume state. Defaults under <data-dir>/recovery_state/.",
    )
    parser.add_argument(
        "--log-file",
        default=None,
        help="JSONL log file for per-batch results. Defaults alongside the state file.",
    )
    parser.add_argument(
        "--serial-chunk-size",
        type=int,
        default=32,
        help="Sets CASS_INDEXER_SERIAL_CHUNK_SIZE for the cass subprocess.",
    )
    parser.add_argument(
        "--sample-interval-ms",
        type=int,
        default=100,
        help="How often to sample the cass subprocess memory usage from /proc.",
    )
    parser.add_argument(
        "--memory-soft-fraction",
        type=float,
        default=0.20,
        help="Soft RSS budget as a fraction of MemAvailable at batch start.",
    )
    parser.add_argument(
        "--memory-hard-fraction",
        type=float,
        default=0.35,
        help="Hard RSS budget as a fraction of MemAvailable at batch start.",
    )
    parser.add_argument(
        "--memory-soft-cap-gb",
        type=float,
        default=8.0,
        help="Absolute soft RSS cap in GiB, applied in addition to the MemAvailable fraction.",
    )
    parser.add_argument(
        "--memory-hard-cap-gb",
        type=float,
        default=12.0,
        help="Absolute hard RSS cap in GiB, applied in addition to the MemAvailable fraction.",
    )
    parser.add_argument(
        "--growth-factor",
        type=float,
        default=1.5,
        help="Multiplicative batch-size increase when throughput and memory headroom are both good.",
    )
    parser.add_argument(
        "--defer-lexical-updates",
        action="store_true",
        default=True,
        help="Set CASS_DEFER_LEXICAL_UPDATES=1 for DB-only reconciliation passes.",
    )
    parser.add_argument(
        "--no-defer-lexical-updates",
        dest="defer_lexical_updates",
        action="store_false",
        help="Do not set CASS_DEFER_LEXICAL_UPDATES.",
    )
    args = parser.parse_args()
    if not args.paths_file and (not args.root or not args.pattern):
        parser.error(
            "provide either at least one --paths-file or both --root and --pattern"
        )
    return args


def read_explicit_paths(paths_files: List[str]) -> List[Path]:
    explicit_paths: List[Path] = []
    for path_file_text in paths_files:
        path_file = Path(path_file_text).expanduser().resolve()
        if not path_file.exists():
            continue
        for raw_line in path_file.read_text(encoding="utf-8").splitlines():
            entry = raw_line.strip()
            if not entry or entry.startswith("#"):
                continue
            path = Path(entry).expanduser()
            if not path.is_absolute():
                path = (path_file.parent / path).resolve()
            else:
                path = path.resolve()
            if path.is_file():
                explicit_paths.append(path)
    return explicit_paths


def collect_paths(roots: List[str], patterns: List[str], paths_files: List[str]) -> List[Path]:
    seen: Dict[str, Path] = {}
    for path in read_explicit_paths(paths_files):
        seen[str(path)] = path
    for root_text in roots:
        root = Path(root_text).expanduser().resolve()
        if not root.exists():
            continue
        for pattern in patterns:
            for path in root.glob(pattern):
                if path.is_file():
                    seen[str(path)] = path
    return [seen[key] for key in sorted(seen.keys())]


def config_signature(
    roots: List[str], patterns: List[str], paths_files: List[str]
) -> Tuple[Dict[str, List[str]], str]:
    payload = {
        "roots": sorted(str(Path(root).expanduser().resolve()) for root in roots),
        "patterns": sorted(patterns),
        "paths_files": sorted(
            str(Path(path_file).expanduser().resolve()) for path_file in paths_files
        ),
    }
    encoded = json.dumps(payload, sort_keys=True, separators=(",", ":")).encode("utf-8")
    return payload, hashlib.sha256(encoded).hexdigest()[:16]


def state_paths(args: argparse.Namespace) -> tuple[Path, Path, Dict[str, List[str]], str, Path, Path]:
    data_dir = Path(args.data_dir).expanduser().resolve()
    recovery_dir = data_dir / "recovery_state"
    recovery_dir.mkdir(parents=True, exist_ok=True)
    signature_payload, signature_id = config_signature(
        args.root, args.pattern, args.paths_file
    )
    if args.state_file is not None:
        state_file = Path(args.state_file).expanduser().resolve()
    else:
        state_file = recovery_dir / f"watch_once_batches_state_{signature_id}.json"
    if args.log_file is not None:
        log_file = Path(args.log_file).expanduser().resolve()
    else:
        log_file = recovery_dir / f"watch_once_batches_log_{signature_id}.jsonl"
    legacy_state_file = recovery_dir / LEGACY_STATE_BASENAME
    legacy_log_file = recovery_dir / LEGACY_LOG_BASENAME
    return (
        state_file,
        log_file,
        signature_payload,
        signature_id,
        legacy_state_file,
        legacy_log_file,
    )


def load_state(state_file: Path) -> Dict[str, object]:
    if not state_file.exists():
        return {}
    return json.loads(state_file.read_text())


def save_state(state_file: Path, state: Dict[str, object]) -> None:
    state_file.parent.mkdir(parents=True, exist_ok=True)
    state_file.write_text(json.dumps(state, indent=2, sort_keys=True) + "\n")


def append_log(log_file: Path, payload: Dict[str, object]) -> None:
    log_file.parent.mkdir(parents=True, exist_ok=True)
    with log_file.open("a", encoding="utf-8") as handle:
        handle.write(json.dumps(payload, sort_keys=True) + "\n")


def maybe_migrate_legacy_state(
    state_file: Path,
    log_file: Path,
    legacy_state_file: Path,
    legacy_log_file: Path,
    signature_payload: Dict[str, List[str]],
) -> Dict[str, Any]:
    if state_file.exists():
        return load_state(state_file)
    if not legacy_state_file.exists():
        return {}
    legacy_state = load_state(legacy_state_file)
    if legacy_state.get("roots") != signature_payload["roots"]:
        return {}
    if legacy_state.get("patterns") != signature_payload["patterns"]:
        return {}
    save_state(state_file, legacy_state)
    if legacy_log_file.exists() and not log_file.exists():
        log_file.write_text(legacy_log_file.read_text())
    return legacy_state


def normalize_state_metadata(
    state: Dict[str, Any],
    signature_payload: Dict[str, List[str]],
    signature_id: str,
    state_file: Path,
    log_file: Path,
    fallback_batch_size: int,
) -> Dict[str, Any]:
    normalized = dict(state)
    normalized["signature"] = signature_payload
    normalized["signature_id"] = signature_id
    normalized["roots"] = signature_payload["roots"]
    normalized["patterns"] = signature_payload["patterns"]
    normalized["state_file"] = str(state_file)
    normalized["log_file"] = str(log_file)
    normalized.setdefault("current_batch_size", fallback_batch_size)
    tuning = dict(normalized.get("tuning", {}))
    tuning.setdefault("best_batch_size", int(normalized["current_batch_size"]))
    normalized["tuning"] = tuning
    return normalized


def db_counts(data_dir: Path) -> Dict[str, int]:
    db_path = data_dir / "agent_search.db"
    conn = sqlite3.connect(db_path)
    try:
        cur = conn.cursor()
        conversations = cur.execute("SELECT count(*) FROM conversations").fetchone()[0]
        messages = cur.execute("SELECT count(*) FROM messages").fetchone()[0]
        return {
            "conversations": int(conversations),
            "messages": int(messages),
        }
    finally:
        conn.close()


def read_meminfo_kb() -> Dict[str, int]:
    values: Dict[str, int] = {}
    with open("/proc/meminfo", encoding="utf-8") as handle:
        for line in handle:
            key, raw = line.split(":", 1)
            parts = raw.strip().split()
            if not parts:
                continue
            try:
                values[key] = int(parts[0])
            except ValueError:
                continue
    return values


def read_proc_status_kb(pid: int) -> Dict[str, int]:
    values: Dict[str, int] = {}
    status_path = Path(f"/proc/{pid}/status")
    if not status_path.exists():
        return values
    with status_path.open(encoding="utf-8") as handle:
        for line in handle:
            if line.startswith(("VmRSS:", "VmHWM:")):
                key, raw = line.split(":", 1)
                parts = raw.strip().split()
                if not parts:
                    continue
                try:
                    values[key] = int(parts[0])
                except ValueError:
                    continue
    return values


def compute_memory_budgets_kb(
    mem_total_kb: int,
    mem_available_kb: int,
    soft_fraction: float,
    hard_fraction: float,
    soft_cap_gb: float,
    hard_cap_gb: float,
) -> Tuple[int, int]:
    soft_budget = min(
        int(mem_available_kb * soft_fraction),
        int(mem_total_kb * TOTAL_MEMORY_SOFT_CAP_FRACTION),
        int(soft_cap_gb * 1024 * 1024),
    )
    hard_budget = min(
        int(mem_available_kb * hard_fraction),
        int(mem_total_kb * TOTAL_MEMORY_HARD_CAP_FRACTION),
        int(hard_cap_gb * 1024 * 1024),
    )
    soft_budget = max(MIN_MEMORY_BUDGET_KB, soft_budget)
    hard_budget = max(soft_budget, max(MIN_MEMORY_BUDGET_KB, hard_budget))
    return soft_budget, hard_budget


class TailBuffer:
    def __init__(self, max_chars: int) -> None:
        self.max_chars = max_chars
        self.chunks: deque[str] = deque()
        self.total_chars = 0

    def append(self, chunk: str) -> None:
        if not chunk:
            return
        self.chunks.append(chunk)
        self.total_chars += len(chunk)
        while self.total_chars > self.max_chars and self.chunks:
            removed = self.chunks.popleft()
            self.total_chars -= len(removed)

    def text(self) -> str:
        combined = "".join(self.chunks)
        if len(combined) <= self.max_chars:
            return combined
        return combined[-self.max_chars :]


def drain_stream(stream: Optional[Any], tail: TailBuffer) -> None:
    if stream is None:
        return
    try:
        while True:
            chunk = stream.read(4096)
            if not chunk:
                break
            tail.append(chunk)
    finally:
        stream.close()


def run_batch(
    cass_binary: Path,
    data_dir: Path,
    batch_paths: List[Path],
    defer_lexical_updates: bool,
    serial_chunk_size: int,
    sample_interval_ms: int,
) -> Dict[str, Any]:
    cmd = [
        str(cass_binary),
        "--color=never",
        "index",
        "--watch-once",
        *[str(path) for path in batch_paths],
        "--data-dir",
        str(data_dir),
        "--json",
    ]
    env = os.environ.copy()
    env["CASS_INDEXER_SERIAL_CHUNK_SIZE"] = str(serial_chunk_size)
    if defer_lexical_updates:
        env["CASS_DEFER_LEXICAL_UPDATES"] = "1"
    else:
        env.pop("CASS_DEFER_LEXICAL_UPDATES", None)
    mem_before = read_meminfo_kb()
    proc = subprocess.Popen(
        cmd,
        env=env,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    stdout_tail = TailBuffer(STREAM_TAIL_CHARS)
    stderr_tail = TailBuffer(STREAM_TAIL_CHARS)
    stdout_thread = threading.Thread(
        target=drain_stream,
        args=(proc.stdout, stdout_tail),
        daemon=True,
    )
    stderr_thread = threading.Thread(
        target=drain_stream,
        args=(proc.stderr, stderr_tail),
        daemon=True,
    )
    stdout_thread.start()
    stderr_thread.start()
    peak_rss_kb = 0
    peak_hwm_kb = 0
    samples = 0
    while True:
        status = read_proc_status_kb(proc.pid)
        peak_rss_kb = max(peak_rss_kb, status.get("VmRSS", 0))
        peak_hwm_kb = max(peak_hwm_kb, status.get("VmHWM", 0))
        samples += 1
        if proc.poll() is not None:
            break
        time.sleep(max(sample_interval_ms, 10) / 1000.0)
    stdout_thread.join()
    stderr_thread.join()
    mem_after = read_meminfo_kb()
    return {
        "proc": subprocess.CompletedProcess(
            args=cmd,
            returncode=proc.returncode,
            stdout=stdout_tail.text(),
            stderr=stderr_tail.text(),
        ),
        "peak_rss_kb": peak_rss_kb,
        "peak_hwm_kb": peak_hwm_kb,
        "sample_count": samples,
        "mem_total_kb": mem_before.get("MemTotal", 0),
        "mem_available_start_kb": mem_before.get("MemAvailable", 0),
        "mem_available_end_kb": mem_after.get("MemAvailable", 0),
    }


def failure_text(proc: subprocess.CompletedProcess[str]) -> str:
    return "\n".join(part for part in [proc.stdout, proc.stderr] if part).lower()


def has_masked_watch_failure(proc: subprocess.CompletedProcess[str]) -> bool:
    return "watch reindex failed" in failure_text(proc)


def autotune_after_success(
    args: argparse.Namespace,
    batch_size: int,
    elapsed_ms: int,
    peak_memory_kb: int,
    soft_budget_kb: int,
    hard_budget_kb: int,
    remaining_paths: int,
    tuning: Dict[str, Any],
) -> Tuple[int, Dict[str, Any], str]:
    elapsed_seconds = max(elapsed_ms / 1000.0, 0.001)
    throughput = batch_size / elapsed_seconds
    best_throughput = float(tuning.get("best_throughput_paths_per_sec", 0.0))
    best_batch_size = int(tuning.get("best_batch_size", batch_size))
    if throughput >= best_throughput:
        best_throughput = throughput
        best_batch_size = batch_size

    near_hard_limit = peak_memory_kb > 0 and peak_memory_kb >= hard_budget_kb
    above_soft_limit = peak_memory_kb > 0 and peak_memory_kb > soft_budget_kb
    very_safe = peak_memory_kb == 0 or peak_memory_kb <= int(soft_budget_kb * 0.60)
    comfortably_safe = peak_memory_kb == 0 or peak_memory_kb <= int(soft_budget_kb * 0.75)
    throughput_regressed = best_throughput > 0 and throughput < (best_throughput * 0.85)

    if near_hard_limit:
        next_batch_size = max(args.min_batch_size, batch_size // 2)
        reason = "decrease_high_rss"
    elif above_soft_limit and batch_size > args.min_batch_size:
        next_batch_size = max(args.min_batch_size, batch_size - max(1, batch_size // 4))
        reason = "decrease_soft_rss"
    elif comfortably_safe and batch_size < args.max_batch_size:
        growth_step = max(1, min(32, batch_size // 2))
        grown = batch_size + growth_step
        next_batch_size = min(args.max_batch_size, grown)
        reason = "increase_safe_headroom"
    elif very_safe and batch_size < args.max_batch_size:
        growth_step = max(1, min(16, batch_size // 4))
        grown = batch_size + growth_step
        next_batch_size = min(args.max_batch_size, grown)
        reason = "increase_safe_headroom"
    elif throughput_regressed and batch_size > best_batch_size and peak_memory_kb >= int(soft_budget_kb * 0.85):
        next_batch_size = max(args.min_batch_size, best_batch_size)
        reason = "return_to_best_throughput"
    else:
        next_batch_size = batch_size
        reason = "hold_steady"

    next_batch_size = min(next_batch_size, max(remaining_paths, args.min_batch_size))
    tuning.update(
        {
            "best_batch_size": best_batch_size,
            "best_throughput_paths_per_sec": best_throughput,
            "last_batch_throughput_paths_per_sec": throughput,
            "last_peak_memory_kb": peak_memory_kb,
            "last_memory_soft_budget_kb": soft_budget_kb,
            "last_memory_hard_budget_kb": hard_budget_kb,
            "last_autotune_reason": reason,
        }
    )
    return next_batch_size, tuning, reason


def main() -> int:
    args = parse_args()
    cass_binary = Path(args.cass_binary).expanduser().resolve()
    data_dir = Path(args.data_dir).expanduser().resolve()
    (
        state_file,
        log_file,
        signature_payload,
        signature_id,
        legacy_state_file,
        legacy_log_file,
    ) = state_paths(args)

    paths = collect_paths(args.root, args.pattern, args.paths_file)
    if not paths:
        print(json.dumps({"status": "no_paths", "roots": args.root, "patterns": args.pattern}))
        return 0

    state = maybe_migrate_legacy_state(
        state_file=state_file,
        log_file=log_file,
        legacy_state_file=legacy_state_file,
        legacy_log_file=legacy_log_file,
        signature_payload=signature_payload,
    )
    state = normalize_state_metadata(
        state=state,
        signature_payload=signature_payload,
        signature_id=signature_id,
        state_file=state_file,
        log_file=log_file,
        fallback_batch_size=args.batch_size,
    )
    next_index = int(state.get("next_index", 0))
    current_batch_size = int(state.get("current_batch_size", args.batch_size))
    tuning: Dict[str, Any] = dict(state.get("tuning", {}))
    if args.start_index is not None:
        next_index = args.start_index
        current_batch_size = args.batch_size
    current_batch_size = max(args.min_batch_size, min(args.max_batch_size, current_batch_size))

    baseline_counts = db_counts(data_dir)
    run_started_at = int(time.time())
    successful_batches = 0

    while next_index < len(paths):
        if args.max_batches is not None and successful_batches >= args.max_batches:
            break

        batch_size = max(args.min_batch_size, current_batch_size)
        batch = paths[next_index : next_index + batch_size]
        started_at = time.time()
        batch_result = run_batch(
            cass_binary=cass_binary,
            data_dir=data_dir,
            batch_paths=batch,
            defer_lexical_updates=args.defer_lexical_updates,
            serial_chunk_size=args.serial_chunk_size,
            sample_interval_ms=args.sample_interval_ms,
        )
        proc = batch_result["proc"]
        elapsed_ms = int((time.time() - started_at) * 1000)
        combined_failure = failure_text(proc)
        masked_watch_failure = has_masked_watch_failure(proc)
        effective_returncode = proc.returncode if not masked_watch_failure else 90
        peak_memory_kb = max(
            int(batch_result.get("peak_rss_kb", 0)),
            int(batch_result.get("peak_hwm_kb", 0)),
        )
        soft_budget_kb, hard_budget_kb = compute_memory_budgets_kb(
            mem_total_kb=int(batch_result.get("mem_total_kb", 0)),
            mem_available_kb=max(1, int(batch_result.get("mem_available_start_kb", 0))),
            soft_fraction=args.memory_soft_fraction,
            hard_fraction=args.memory_hard_fraction,
            soft_cap_gb=args.memory_soft_cap_gb,
            hard_cap_gb=args.memory_hard_cap_gb,
        )

        log_entry: Dict[str, object] = {
            "ts": int(time.time()),
            "start_index": next_index,
            "end_index": next_index + len(batch),
            "batch_size": len(batch),
            "first_path": str(batch[0]),
            "last_path": str(batch[-1]),
            "exit_code": proc.returncode,
            "effective_exit_code": effective_returncode,
            "elapsed_ms": elapsed_ms,
            "stdout_tail": proc.stdout[-2000:],
            "stderr_tail": proc.stderr[-2000:],
            "masked_watch_failure": masked_watch_failure,
            "peak_rss_kb": int(batch_result.get("peak_rss_kb", 0)),
            "peak_hwm_kb": int(batch_result.get("peak_hwm_kb", 0)),
            "mem_total_kb": int(batch_result.get("mem_total_kb", 0)),
            "mem_available_start_kb": int(batch_result.get("mem_available_start_kb", 0)),
            "mem_available_end_kb": int(batch_result.get("mem_available_end_kb", 0)),
            "memory_soft_budget_kb": soft_budget_kb,
            "memory_hard_budget_kb": hard_budget_kb,
            "sample_count": int(batch_result.get("sample_count", 0)),
        }

        if effective_returncode == 0:
            counts = db_counts(data_dir)
            remaining_paths = len(paths) - (next_index + len(batch))
            current_batch_size, tuning, tune_reason = autotune_after_success(
                args=args,
                batch_size=len(batch),
                elapsed_ms=elapsed_ms,
                peak_memory_kb=peak_memory_kb,
                soft_budget_kb=soft_budget_kb,
                hard_budget_kb=hard_budget_kb,
                remaining_paths=remaining_paths,
                tuning=tuning,
            )
            log_entry["db_counts"] = counts
            log_entry["next_batch_size"] = current_batch_size
            log_entry["autotune_reason"] = tune_reason
            append_log(log_file, log_entry)
            next_index += len(batch)
            successful_batches += 1
            state = {
                "signature": signature_payload,
                "signature_id": signature_id,
                "roots": signature_payload["roots"],
                "patterns": signature_payload["patterns"],
                "total_paths": len(paths),
                "next_index": next_index,
                "current_batch_size": current_batch_size,
                "successful_batches_this_run": successful_batches,
                "run_started_at": run_started_at,
                "updated_at": int(time.time()),
                "baseline_counts": baseline_counts,
                "latest_counts": counts,
                "tuning": tuning,
                "last_batch": {
                    "size": len(batch),
                    "first_path": str(batch[0]),
                    "last_path": str(batch[-1]),
                    "elapsed_ms": elapsed_ms,
                    "peak_memory_kb": peak_memory_kb,
                    "next_batch_size": current_batch_size,
                    "autotune_reason": tune_reason,
                },
            }
            save_state(state_file, state)
            print(
                json.dumps(
                    {
                        "status": "batch_ok",
                        "next_index": next_index,
                        "total_paths": len(paths),
                        "batch_size": len(batch),
                        "elapsed_ms": elapsed_ms,
                        "peak_memory_kb": peak_memory_kb,
                        "next_batch_size": current_batch_size,
                        "autotune_reason": tune_reason,
                        "db_counts": counts,
                    },
                    sort_keys=True,
                ),
                flush=True,
            )
            continue

        append_log(log_file, log_entry)
        if "out of memory" in combined_failure and batch_size > args.min_batch_size:
            current_batch_size = max(args.min_batch_size, batch_size // 2)
            tuning.update(
                {
                    "last_peak_memory_kb": peak_memory_kb,
                    "last_memory_soft_budget_kb": soft_budget_kb,
                    "last_memory_hard_budget_kb": hard_budget_kb,
                    "last_autotune_reason": "shrink_after_oom",
                }
            )
            state = {
                "signature": signature_payload,
                "signature_id": signature_id,
                "roots": signature_payload["roots"],
                "patterns": signature_payload["patterns"],
                "total_paths": len(paths),
                "next_index": next_index,
                "current_batch_size": current_batch_size,
                "successful_batches_this_run": successful_batches,
                "run_started_at": run_started_at,
                "updated_at": int(time.time()),
                "baseline_counts": baseline_counts,
                "latest_counts": db_counts(data_dir),
                "tuning": tuning,
                "last_failure": {
                    "reason": "out_of_memory",
                    "failed_batch_size": batch_size,
                    "retry_batch_size": current_batch_size,
                    "first_path": str(batch[0]),
                    "last_path": str(batch[-1]),
                },
            }
            save_state(state_file, state)
            print(
                json.dumps(
                    {
                        "status": "shrinking_batch_after_oom",
                        "failed_batch_size": batch_size,
                        "retry_batch_size": current_batch_size,
                        "next_index": next_index,
                    },
                    sort_keys=True,
                ),
                flush=True,
            )
            continue

        print(proc.stdout, end="", file=sys.stdout)
        print(proc.stderr, end="", file=sys.stderr)
        state = {
            "signature": signature_payload,
            "signature_id": signature_id,
            "roots": signature_payload["roots"],
            "patterns": signature_payload["patterns"],
            "total_paths": len(paths),
            "next_index": next_index,
            "current_batch_size": current_batch_size,
            "successful_batches_this_run": successful_batches,
            "run_started_at": run_started_at,
            "updated_at": int(time.time()),
            "baseline_counts": baseline_counts,
            "latest_counts": db_counts(data_dir),
            "tuning": tuning,
            "last_failure": {
                "reason": "masked_watch_failure"
                if masked_watch_failure
                else "subprocess_failed",
                "exit_code": proc.returncode,
                "effective_exit_code": effective_returncode,
                "batch_size": batch_size,
                "first_path": str(batch[0]),
                "last_path": str(batch[-1]),
            },
        }
        save_state(state_file, state)
        return effective_returncode

    final_counts = db_counts(data_dir)
    summary = {
        "status": "done",
        "successful_batches_this_run": successful_batches,
        "next_index": next_index,
        "total_paths": len(paths),
        "baseline_counts": baseline_counts,
        "final_counts": final_counts,
        "state_file": str(state_file),
        "log_file": str(log_file),
        "current_batch_size": current_batch_size,
        "tuning": tuning,
    }
    save_state(state_file, {**state, **summary, "updated_at": int(time.time())})
    print(json.dumps(summary, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
