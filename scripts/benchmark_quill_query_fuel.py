#!/usr/bin/env python3
"""Bounded, fresh-process CASS query measurements for GH518 (POSIX only).

Run against a DISPOSABLE, consistent copy: CASS search may perform maintenance.
This driver never rebuilds, retries with more fuel, or claims that a search_start
log names the backend that ultimately served results. Successful queries do not
expose work counters through the current CLI; their work measurement is null.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import re
import selectors
import signal
import statistics
import subprocess
import time
from collections import Counter
from typing import Any

FUEL_ENV = "CASS_QUILL_QUERY_FUEL_BUDGET"
QUERIES = ("git fixture", "test isolation")
OUTPUT_LIMIT = 256 * 1024  # Combined stdout + stderr, per invocation.
ANSI = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]")
FUEL = re.compile(
    r"query fuel exhausted after (?P<consumed>\d+)/(?P<budget>\d+) units\s*"
    r"\(segments=(?P<segments_touched>\d+),\s*"
    r"dictionary_blocks=(?P<dictionary_blocks>\d+),\s*"
    r"posting_blocks=(?P<posting_blocks>\d+),\s*"
    r"position_docs=(?P<position_docs>\d+)\)"
)
WORK_FIELDS = ("consumed", "budget", "segments_touched", "dictionary_blocks", "posting_blocks", "position_docs")
WORK_LOG = re.compile(r"\b(" + "|".join(WORK_FIELDS) + r")=(\d+)")
BACKEND = re.compile(r'\bbackend=(?:"([\w.-]{1,64})"|([\w.-]{1,64}))')


def diagnostics(stderr: bytes) -> dict[str, Any]:
    text = ANSI.sub("", stderr.decode("utf-8", errors="replace"))
    # Error context often repeats the same refusal. Keep distinct receipts,
    # rather than summing counters or inferring physical dictionary probes.
    work: list[dict[str, int]] = []
    for match in FUEL.finditer(text):
        receipt = {key: int(value) for key, value in match.groupdict().items()}
        if receipt not in work:
            work.append(receipt)
    attempts: list[str] = []
    for line in text.splitlines():
        # --json may put the human error inside stdout rather than stderr;
        # the structured warning still supplies the same authoritative tuple.
        if "quill.argus.query_fuel_exhausted" in line:
            fields = {key: int(value) for key, value in WORK_LOG.findall(line)}
            if all(key in fields for key in WORK_FIELDS):
                receipt = {key: fields[key] for key in WORK_FIELDS}
                if receipt not in work:
                    work.append(receipt)
        if "search_start" in line:
            match = BACKEND.search(line)
            if match:
                attempts.append(match.group(1) or match.group(2))
    return {
        "fuel_refusals": work or None,
        "observed_search_start_backends": attempts,
        "serving_backend": None,
    }


def _kill_group(process: subprocess.Popen[bytes]) -> None:
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass


def run_bounded(
    argv: list[str],
    *,
    env: dict[str, str],
    timeout_s: float,
    output_limit: int = OUTPUT_LIMIT,
) -> dict[str, Any]:
    """Kill/reap a private process group on timeout or output overflow.

    No shell is used. Pipes are drained concurrently, and retained output is
    bounded even for a noisy child. This is an external safety boundary, NOT
    proof that Quill or the SQLite fallback implements an internal deadline.
    """
    if os.name != "posix":
        raise ValueError("process-group containment requires POSIX")
    if not math.isfinite(timeout_s) or timeout_s <= 0 or output_limit < 1:
        raise ValueError("timeout and output limit must be positive and finite")
    started = time.monotonic()
    stdout = bytearray()
    stderr = bytearray()
    try:
        process = subprocess.Popen(
            argv, env=env, stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True,
        )
    except OSError as error:
        return {
            "termination": "spawn_error", "returncode": None, "error_errno": error.errno,
            "elapsed_ms": round((time.monotonic() - started) * 1000.0, 3),
            "stdout": b"", "stderr": b"",
        }
    termination = "completed"
    try:
        with selectors.DefaultSelector() as selector:
            assert process.stdout is not None and process.stderr is not None
            for stream, destination in ((process.stdout, stdout), (process.stderr, stderr)):
                os.set_blocking(stream.fileno(), False)
                selector.register(stream, selectors.EVENT_READ, destination)
            while selector.get_map() or process.poll() is None:
                remaining = timeout_s - (time.monotonic() - started)
                if remaining <= 0:
                    termination = "deadline"
                    break
                if not selector.get_map():
                    try:
                        process.wait(timeout=remaining)
                    except subprocess.TimeoutExpired:
                        termination = "deadline"
                    break
                for key, _ in selector.select(min(remaining, 0.05)):
                    try:
                        chunk = os.read(key.fd, 16384)
                    except BlockingIOError:
                        continue
                    if not chunk:
                        selector.unregister(key.fileobj)
                        continue
                    room = output_limit - len(stdout) - len(stderr)
                    key.data.extend(chunk[:room])
                    if len(chunk) > room:
                        termination = "output_limit"
                        break
                if termination != "completed":
                    break
    finally:
        # Also contain a child that exits but leaves descendants holding pipes.
        _kill_group(process)
        process.wait()
        if process.stdout is not None:
            process.stdout.close()
        if process.stderr is not None:
            process.stderr.close()
    elapsed_ms = (time.monotonic() - started) * 1000.0
    return {
        "termination": termination,
        "returncode": process.returncode,
        "elapsed_ms": round(elapsed_ms, 3),
        "stdout": bytes(stdout),
        "stderr": bytes(stderr),
    }


def hit_count(stdout: bytes) -> int | None:
    """Do not confuse CLI exit success or malformed output with search hits."""
    try:
        payload = json.loads(stdout)
    except (ValueError, UnicodeDecodeError, RecursionError):
        return None
    if isinstance(payload, dict):
        payload = payload.get("hits")
    if isinstance(payload, list) and all(isinstance(hit, dict) for hit in payload):
        return len(payload)
    return None


def sample_record(result: dict[str, Any], query: str, repetition: int) -> dict[str, Any]:
    stdout, stderr = result["stdout"], result["stderr"]
    count = hit_count(stdout)
    receipt = diagnostics(stderr)
    if result["termination"] != "completed":
        outcome = result["termination"]
    elif result["returncode"] != 0:
        outcome = "fuel_exhausted" if receipt["fuel_refusals"] else "command_error"
    elif count is None:
        outcome = "invalid_json_results"
    else:
        outcome = "success"
    return {
        "query": query, "repetition": repetition, "outcome": outcome,
        "returncode": result["returncode"], "elapsed_ms": result["elapsed_ms"],
        "spawn_error_errno": result.get("error_errno"),
        "hit_count": count, "stdout_bytes_retained": len(stdout),
        "stderr_bytes_retained": len(stderr), **receipt,
    }


def summarize(samples: list[dict[str, Any]]) -> dict[str, Any]:
    counts = dict(Counter(sample["outcome"] for sample in samples))
    successes = sorted(sample["elapsed_ms"] for sample in samples if sample["outcome"] == "success")
    return {
        "outcomes": counts,
        "successful_samples": len(successes),
        "success_p50_ms": statistics.median(successes) if successes else None,
        "success_p95_ms": successes[math.ceil(0.95 * len(successes)) - 1] if successes else None,
        "percentile_method": "p50 median; p95 nearest rank; failures excluded and counted separately",
    }


def positive_int(raw: str) -> int:
    try:
        value = int(raw)
    except ValueError as error:
        raise argparse.ArgumentTypeError("expected a positive integer") from error
    if not 1 <= value <= (1 << 64) - 1:
        raise argparse.ArgumentTypeError("expected an integer in 1..=u64::MAX")
    return value


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cass", type=Path, required=True)
    parser.add_argument("--data-dir", type=Path, required=True)
    parser.add_argument("--db", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True, help="new JSON receipt; never overwritten")
    parser.add_argument("--disposable-copy", action="store_true", help="confirm these paths are a disposable archive/index copy")
    parser.add_argument("--query", action="append", help="repeat up to eight times; default: GH518's two queries")
    parser.add_argument("--repetitions", type=positive_int, default=5)
    parser.add_argument("--timeout-seconds", type=float, default=10.0)
    parser.add_argument("--fuel-budget", type=positive_int, help="one explicit budget; no adaptive increases or retries")
    parser.add_argument("--reported-segment-count", type=positive_int, help="operator-supplied live count, not independently verified")
    parser.add_argument("--label", default="", help="operator label, e.g. exact source revision and corpus identifier")
    args = parser.parse_args()
    if os.name != "posix":
        parser.error("this driver requires POSIX process groups")
    if not args.disposable_copy:
        parser.error("use a disposable consistent copy and acknowledge it with --disposable-copy")
    if not 1 <= args.repetitions <= 30:
        parser.error("repetitions must be in 1..30")
    if not math.isfinite(args.timeout_seconds) or not 0 < args.timeout_seconds <= 120:
        parser.error("timeout-seconds must be finite and in (0, 120]")
    queries = args.query if args.query else list(QUERIES)
    if not 1 <= len(queries) <= 8 or any(not query.strip() or len(query) > 2048 for query in queries):
        parser.error("supply 1..8 nonempty queries, at most 2048 characters each")
    try:
        cass = args.cass.resolve(strict=True)
        data_dir = args.data_dir.resolve(strict=True)
        db = args.db.resolve(strict=True)
    except OSError as error:
        parser.error(str(error))
    if not cass.is_file() or not os.access(cass, os.X_OK) or not data_dir.is_dir() or not db.is_file():
        parser.error("cass must be executable; data-dir and db must be existing directory/file")
    # Reserve the receipt before running anything. A typo or existing output
    # cannot waste a benchmark or overwrite earlier evidence.
    with args.output.open("x", encoding="utf-8") as output:
        with cass.open("rb") as binary:
            binary_sha256 = hashlib.file_digest(binary, "sha256").hexdigest()
        env = dict(os.environ)
        env.pop(FUEL_ENV, None)
        if args.fuel_budget is not None:
            env[FUEL_ENV] = str(args.fuel_budget)
        env["RUST_LOG"] = "info"
        env["NO_COLOR"] = "1"
        samples: list[dict[str, Any]] = []
        for repetition in range(args.repetitions):
            # Alternate query order to reduce fixed warm-cache ordering bias.
            ordered = queries if repetition % 2 == 0 else list(reversed(queries))
            for query in ordered:
                argv = [str(cass), "--db", str(db), "search", "--mode", "lexical",
                        "--limit", "1", "--json", "--data-dir", str(data_dir), "--", query]
                result = run_bounded(argv, env=env, timeout_s=args.timeout_seconds)
                record = sample_record(result, query, repetition)
                samples.append(record)
                print(f"{query!r}: {record['outcome']} {record['elapsed_ms']:.3f} ms", flush=True)
        report = {
            "schema_version": 1, "label": args.label,
            "binary_sha256": binary_sha256, "binary_path": str(cass),
            "data_dir": str(data_dir), "db": str(db),
            "fuel_budget_override": args.fuel_budget,
            "reported_segment_count_unverified": args.reported_segment_count,
            "timeout_seconds_per_invocation": args.timeout_seconds,
            "output_bytes_limit_per_invocation": OUTPUT_LIMIT,
            "measurement": "fresh CLI process; includes open/search/hydration/shutdown and driver overhead; OS caches not flushed",
            "work_note": "fuel_refusals are admitted work, not physical probes; absent counters are unknown, not zero",
            "backend_note": "search_start records attempts, not a serving receipt; legacy tantivy labels may execute Quill",
            "samples": samples,
            "by_query": {query: summarize([sample for sample in samples if sample["query"] == query]) for query in queries},
        }
        json.dump(report, output, indent=2, allow_nan=False)
        output.write("\n")
    return 0 if all(sample["outcome"] == "success" for sample in samples) else 2


if __name__ == "__main__":
    raise SystemExit(main())
