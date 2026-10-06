#!/usr/bin/env python3
"""Measure strict fresh CASS, retained CASS, and fresh IPC clients separately.

No cache dropping, source-index writes, timing estimates, or implicit speedup
claims. Failed requests, empty/mismatched hits, policy downgrades, changed index
identities, and unclean exits abort without publishing a result file.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import resource
import selectors
import shutil
import subprocess
import sys
import tempfile
import time

import cass_lexical_ipc as ipc

SCRIPT = Path(__file__).with_name("cass_lexical_ipc.py")


def children_cpu_ms():
    usage = resource.getrusage(resource.RUSAGE_CHILDREN)
    return 1000 * (usage.ru_utime + usage.ru_stime)


def fingerprint(path):
    digest = hashlib.sha256()
    with open(path, "rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def successful(response, reused, reference=None, mode=None, full=False):
    if response.get("ok") is not True:
        raise RuntimeError(f"search failed: {response.get('error')}")
    result = response.get("result", {})
    hits = result.get("hits")
    if not isinstance(hits, list) or not hits:
        raise RuntimeError("benchmark query must return at least one hit")
    if result.get("reader_reused") is not reused:
        raise RuntimeError("unexpected native reader lifecycle")
    if reference is not None and hits != reference:
        raise RuntimeError("hit, identity, score or ordering mismatch")
    if mode is not None:
        admission = response.get("admission", {})
        if (admission.get("mode") != mode or
                admission.get("full_verify_requested") is not full or
                admission.get("persistent_proof") is not False or
                admission.get("file_identity_checked") is not True or
                admission.get("immutable_generation_certified") is not False):
            raise RuntimeError("IPC verification policy was not honored")
    return hits


def cleanup(process):
    if process.poll() is None:
        process.kill()
    process.wait(timeout=5)
    for stream in (process.stdin, process.stdout, process.stderr):
        if stream is not None and not stream.closed:
            stream.close()


def run_client(command, timeout):
    """Include client startup/exit CPU, and bound both output pipes."""
    cpu_before, start = children_cpu_ms(), time.monotonic()
    process = subprocess.Popen(command, stdin=subprocess.DEVNULL,
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    output, errors = bytearray(), bytearray()
    deadline = start + timeout
    try:
        with selectors.DefaultSelector() as selector:
            for stream, target, limit in (
                    (process.stdout, output, ipc.RESPONSE_LIMIT),
                    (process.stderr, errors, 64 * 1024)):
                os.set_blocking(stream.fileno(), False)
                selector.register(stream, selectors.EVENT_READ, (target, limit))
            while selector.get_map():
                for key, _ in selector.select(ipc.remaining(deadline)):
                    try:
                        data = os.read(key.fd, 65536)
                    except BlockingIOError:
                        continue
                    if not data:
                        selector.unregister(key.fileobj)
                        continue
                    target, limit = key.data
                    target.extend(data)
                    if len(target) > limit:
                        raise RuntimeError("client exceeded its output bound")
        code = process.wait(timeout=ipc.remaining(deadline))
        if code != 0:
            raise RuntimeError(f"client exited {code}: {errors[:4096].decode(errors='replace')}")
        if not output.endswith(b"\n") or output.count(b"\n") != 1:
            raise RuntimeError("client did not return exactly one complete response")
        response = ipc.decode(output)
        return response, {
            "wall_ms": 1000 * (time.monotonic() - start),
            "client_cpu_ms": children_cpu_ms() - cpu_before,
        }
    finally:
        cleanup(process)


def start_native(cass, index, timeout):
    process = subprocess.Popen(
        [cass, "serve", "--stdio", "--index", str(index),
         "--request-timeout-ms", str(int(timeout * 1000))],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    os.set_blocking(process.stdin.fileno(), False)
    os.set_blocking(process.stdout.fileno(), False)
    return process


def finish_native(process, timeout):
    process.stdin.close()
    code = process.wait(timeout=timeout)
    if code != 0:
        raise RuntimeError(f"native worker exited {code}")
    if os.read(process.stdout.fileno(), ipc.RESPONSE_LIMIT + 1):
        raise RuntimeError("unsolicited native output after query")
    process.stdout.close()


def strict_fresh(args, reference):
    samples = []
    for _ in range(args.rounds):
        cpu_before, start = children_cpu_ms(), time.monotonic()
        process = start_native(args.cass, args.index, args.timeout)
        try:
            response = ipc.exchange_pipes(process, args.request, start + args.timeout)
            query_end = time.monotonic()
            reference = successful(response, False, reference)
            finish_native(process, args.timeout)
            samples.append({
                "response_wall_ms": 1000 * (query_end - start),
                "complete_process_wall_ms": 1000 * (time.monotonic() - start),
                "complete_native_cpu_ms": children_cpu_ms() - cpu_before,
            })
        finally:
            cleanup(process)
    return {"samples": samples}, reference


def retained_native(args, reference):
    cpu_before, start = children_cpu_ms(), time.monotonic()
    process = start_native(args.cass, args.index, args.timeout)
    samples = []
    try:
        for index in range(args.rounds + 1):
            before = start if index == 0 else time.monotonic()
            response = ipc.exchange_pipes(process, {**args.request, "id": index + 1},
                                          before + args.timeout)
            successful(response, index != 0, reference)
            samples.append({"response_wall_ms": 1000 * (time.monotonic() - before)})
        finish_native(process, args.timeout)
        return {
            "initial_admission": samples[0], "retained_queries": samples[1:],
            "complete_group_wall_ms": 1000 * (time.monotonic() - start),
            "complete_native_cpu_ms": children_cpu_ms() - cpu_before,
        }
    finally:
        cleanup(process)


def ipc_clients(args, reference):
    with tempfile.TemporaryDirectory(prefix="cass-ipc-benchmark-") as directory:
        endpoint = Path(directory) / "owner.sock"
        cpu_before, start = children_cpu_ms(), time.monotonic()
        owner = subprocess.Popen(
            [sys.executable, "-S", str(SCRIPT), "serve", "--cass", args.cass,
             "--index", str(args.index), "--socket", str(endpoint),
             "--timeout", str(args.timeout), "--idle-seconds", "300"],
            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            deadline = start + args.timeout
            while not endpoint.exists() or endpoint.stat().st_mode & 0o777 != 0o600:
                ipc.remaining(deadline)
                if owner.poll() is not None:
                    raise RuntimeError(f"owner exited before readiness: {owner.returncode}")
                time.sleep(0.01)
            if args.client:
                command = [args.client, "--socket", str(endpoint),
                           "--timeout-ms", str(int(args.timeout * 1000))]
            else:
                command = [sys.executable, "-S", str(SCRIPT), "search",
                           "--socket", str(endpoint), "--timeout", str(args.timeout)]
            samples = []
            # Initial admission, warm fresh clients, forced verification, and
            # post-verification reuse are distinct, asserted lifecycle phases.
            phases = [("initial_admission", False, False)]
            phases += [("fresh_client_reuse", True, False)] * args.rounds
            phases += [("full_verify_control", False, True), ("post_verify_reuse", True, False)]
            for phase, reused, full in phases:
                arguments = command + (["--full-verify"] if full else []) + ["--", args.query]
                response, sample = run_client(arguments, args.timeout)
                successful(response, reused, reference,
                           "retained_guarded" if reused else "strict_full", full)
                samples.append({"phase": phase, **sample})
            owner.terminate()
            code = owner.wait(timeout=args.timeout)
            if code != 130:
                raise RuntimeError(f"owner did not perform expected SIGTERM cleanup: {code}")
            if endpoint.exists():
                raise RuntimeError("owner left its socket behind")
            return {
                "client_kind": "native_cass_query" if args.client else "python_frontend",
                "samples": samples,
                "complete_group_wall_ms": 1000 * (time.monotonic() - start),
                "complete_group_cpu_ms": children_cpu_ms() - cpu_before,
                "cpu_scope": "All reaped clients, owner and native descendants for this whole group; "
                             "per-sample CPU is client-only, not total query CPU.",
            }
        finally:
            cleanup(owner)


def measure(args):
    guard = ipc.Guard(args.index)
    try:
        report = {
            "schema_version": 1,
            "phase_order": ["strict_fresh_process", "retained_native", "fresh_ipc_clients"],
            "cache_policy": "Filesystem caches are not flushed. Process freshness is not cold storage.",
            "query": args.query, "rounds": args.rounds,
            "machine": {"platform": platform.platform(), "cpu_count": os.cpu_count()},
            "binaries": {"cass": args.cass, "cass_sha256": fingerprint(args.cass),
                         "client": args.client,
                         "client_sha256": fingerprint(args.client) if args.client else None},
            "owner_source_sha256": fingerprint(SCRIPT),
            "manifest_sha256": fingerprint(args.index / "MANIFEST"),
            "environment_revision": os.environ.get("GITHUB_SHA"),
        }
        fresh, reference = strict_fresh(args, None)
        retained = retained_native(args, reference)
        clients = ipc_clients(args, reference)
        if not guard.unchanged():
            raise RuntimeError("index changed during benchmark; results refused")
        if (fingerprint(args.cass) != report["binaries"]["cass_sha256"] or
                (args.client and fingerprint(args.client) != report["binaries"]["client_sha256"])):
            raise RuntimeError("benchmark binary changed; results refused")
        report["workloads"] = {
            "strict_fresh_process": fresh, "retained_native": retained,
            "fresh_ipc_clients": clients,
        }
        report["hit_count"] = len(reference)
        report["identical_hits_scores_and_order"] = True
        return report
    finally:
        guard.close()


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cass", required=True)
    parser.add_argument("--client", help="Built cass-query binary; omitted means Python frontend.")
    parser.add_argument("--index", type=Path, required=True)
    parser.add_argument("--query", default="performance")
    parser.add_argument("--rounds", type=int, default=10)
    parser.add_argument("--timeout", type=float, default=30)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args(argv)
    if not 1 <= args.rounds <= 1000 or not 0.001 <= args.timeout <= 300:
        parser.error("rounds must be 1..1000 and timeout 0.001..300 seconds")
    requested_client = args.client
    args.cass = shutil.which(args.cass)
    args.client = shutil.which(requested_client) if requested_client else None
    if not args.cass:
        parser.error("CASS executable not found")
    if requested_client and not args.client:
        parser.error("explicit cass-query executable not found; no frontend fallback")
    args.cass = os.path.abspath(args.cass)
    args.client = os.path.abspath(args.client) if args.client else None
    if args.output.exists():
        parser.error("--output already exists; choose a new evidence path")
    # Resolve the index path without accepting symlinks (Guard enforces this).
    args.index = Path(os.path.abspath(args.index))
    args.request = {"op": "search", "id": 1, "query": args.query}
    ipc.validate(args.request)
    report = measure(args)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile("w", dir=args.output.parent, delete=False) as target:
        temporary = Path(target.name)
        json.dump(report, target, indent=2, allow_nan=False)
        target.write("\n")
    try:
        os.replace(temporary, args.output)
    finally:
        temporary.unlink(missing_ok=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
