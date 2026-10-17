#!/usr/bin/env python3
"""Same-binary GH501 admission comparison; never report an ignored policy as fast.

Each fresh sample launches cass serve, makes ONE search, then shuts down. This
isolates fresh native lexical admission; it is not an end-to-end cass search CLI
measurement. Retained samples make many searches in ONE new service process.
Requires a natively built candidate and a quiescent, existing index. No index is
created, compacted or modified by this harness. Cache writes are explicitly opt-in.
"""
from __future__ import annotations
import argparse
import hashlib
import json
import os
from pathlib import Path
import resource
import statistics
import subprocess
import time


def fingerprint(index: Path) -> dict:
    manifest = index / "MANIFEST"
    return {"manifest_sha256": hashlib.sha256(manifest.read_bytes()).hexdigest(),
            "segments": {p.name: [s.st_dev, s.st_ino, s.st_size, s.st_mtime_ns, s.st_ctime_ns]
                         for p in sorted(index.glob("seg-*.fslx")) for s in [p.stat()]}}


def run_session(args, mode: str, count: int, *, expect_error: bool = False) -> dict:
    env = os.environ.copy()
    env.update(CASS_LEXICAL_VERIFY=mode, CASS_LEXICAL_RECEIPT_DIR=str(args.cache))
    requests = [{"op": "search", "id": i+1, "query": args.query, "limit": args.limit}
                for i in range(count)]
    requests.append({"op": "shutdown", "id": count+1})
    payload = "".join(json.dumps(request) + "\n" for request in requests)
    cpu0 = resource.getrusage(resource.RUSAGE_CHILDREN)
    wall0 = time.perf_counter()
    result = subprocess.run([str(args.cass), "serve", "--stdio", "--index", str(args.index)],
                            input=payload, text=True, capture_output=True, env=env,
                            timeout=args.timeout, check=False)
    wall = time.perf_counter() - wall0
    cpu1 = resource.getrusage(resource.RUSAGE_CHILDREN)
    if result.returncode != 0:
        raise RuntimeError(f"{mode}: worker exited {result.returncode}: {result.stderr[-2000:]}")
    replies = [json.loads(line) for line in result.stdout.splitlines()]
    if len(replies) != len(requests):
        raise RuntimeError(f"{mode}: response count differs from requests")
    for request, reply in zip(requests, replies, strict=True):
        if reply.get("id") != request["id"]:
            raise RuntimeError(f"{mode}: response ID mismatch")
    if expect_error:
        if replies[0].get("ok") is not False or "CASS_LEXICAL_VERIFY" not in json.dumps(replies[0]):
            raise RuntimeError("This binary/service path does not enforce the new admission selector")
        return {}
    if any(reply.get("ok") is not True for reply in replies):
        raise RuntimeError(f"{mode}: request failed: {replies}")
    searches = [reply["result"] for reply in replies[:-1]]
    if not searches[0].get("hits"):
        raise RuntimeError("An empty-hit workload cannot establish useful result equivalence")
    for i, search in enumerate(searches):
        if search.get("reader_reused") is not (i > 0):
            raise RuntimeError(f"{mode}: reader reuse contract failed at request {i+1}")
        if search["hits"] != searches[0]["hits"]:
            raise RuntimeError(f"{mode}: retained hit/score/order mismatch")
    return {"mode": mode, "queries": count, "wall_s": wall,
            "child_user_s": cpu1.ru_utime - cpu0.ru_utime,
            "child_system_s": cpu1.ru_stime - cpu0.ru_stime,
            "setup_ms": [r.get("setup_ms") for r in searches],
            "search_ms": [r.get("search_ms") for r in searches],
            "hits": searches[0]["hits"]}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cass", type=Path, required=True)
    parser.add_argument("--index", type=Path, required=True)
    parser.add_argument("--cache", type=Path, required=True,
                        help="existing dedicated owner-only (0700) directory, outside index")
    parser.add_argument("--query", required=True)
    parser.add_argument("--limit", type=int, default=10)
    parser.add_argument("--rounds", type=int, default=7)
    parser.add_argument("--retained-queries", type=int, default=20)
    parser.add_argument("--timeout", type=float, default=60)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if not 1 <= args.limit <= 100 or args.rounds < 2 or args.retained_queries < 2 or args.timeout <= 0:
        parser.error("limit must be 1..100, rounds and retained-queries >= 2, timeout > 0")
    args.cass, args.index, args.cache = (p.resolve(strict=True) for p in (args.cass, args.index, args.cache))
    if args.cache.is_relative_to(args.index):
        parser.error("receipt directory must be outside index")
    cache_stat = args.cache.stat()
    if cache_stat.st_uid != os.geteuid() or cache_stat.st_mode & 0o777 != 0o700:
        parser.error("receipt directory must belong to the effective user and be mode 0700")
    before = fingerprint(args.index)
    binary_hash = hashlib.sha256(args.cass.read_bytes()).hexdigest()
    run_session(args, "deliberately-invalid-gh501-policy", 1, expect_error=True)
    warm = run_session(args, "local-receipts", 1)
    proof = args.cache / "read-open-receipts-v2"
    if not proof.is_file():
        raise RuntimeError("No receipt was minted: index/cache ineligible or segments younger than 60 seconds; no fast-open claim")
    raw = proof.read_bytes()
    if len(raw) > 1 << 20 or len(raw) < 76 or raw[:8] != b"FSQORC02" or hashlib.sha256(raw[:-32]).digest() != raw[-32:]:
        raise RuntimeError("Receipt warmup did not produce a valid candidate proof book")
    records = int.from_bytes(raw[40:44], "little")
    if records == 0 or len(raw) != 76 + 96 * records:
        raise RuntimeError("Receipt book has no well-formed records")
    expected_hits = warm.pop("hits")
    samples = []
    for workload, count in [("fresh_process_one_query", 1), ("retained_reader", args.retained_queries)]:
        for round_number in range(args.rounds):
            modes = ["full", "local-receipts"] if round_number % 2 == 0 else ["local-receipts", "full"]
            for mode in modes:
                if fingerprint(args.index) != before:
                    raise RuntimeError("Index changed during qualification")
                sample = run_session(args, mode, count)
                if sample.pop("hits") != expected_hits:
                    raise RuntimeError(f"{workload}/{mode}: hit/score/order differs from baseline")
                sample.update(workload=workload, round=round_number)
                samples.append(sample)
    if fingerprint(args.index) != before or hashlib.sha256(args.cass.read_bytes()).hexdigest() != binary_hash:
        raise RuntimeError("Index or executable changed during qualification")
    summary = {}
    for workload in ("fresh_process_one_query", "retained_reader"):
        for mode in ("full", "local-receipts"):
            selected = [s for s in samples if s["workload"] == workload and s["mode"] == mode]
            summary[f"{workload}/{mode}"] = {
                "median_wall_s": statistics.median(s["wall_s"] for s in selected),
                "median_child_cpu_s": statistics.median(s["child_user_s"] + s["child_system_s"] for s in selected)}
    report = {"schema_version": 1, "cass_sha256": binary_hash, "index": before,
              "query": args.query, "limit": args.limit, "warmup": warm,
              "proof_records_after_warmup": records, "samples": samples, "summary": summary,
              "limits": ["Warm filesystem cache, not cold disk", "Fresh service process, not cass search CLI",
                         "No per-segment hit counters are inferred from timings; native hash-counter regressions must pass separately",
                         "Metadata receipts are an explicitly weaker optional policy than full-byte verification"]}
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(summary, indent=2))

if __name__ == "__main__":
    main()
