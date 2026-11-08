#!/usr/bin/env python3
r"""Compare real serial and batched forget operations on synthetic archives.

Both binaries must be built with the same toolchain, features and profile. This
driver does not build them, accept an existing archive, drop caches, or estimate
unmeasured timings. Run it through the repository's DSR quality route. Example:

    python3 scripts/benchmark_forget.py --baseline-cass /tmp/baseline/cass \
        --candidate-cass /tmp/candidate/cass --output-dir /tmp/forget-proof \
        --build-description 'same pinned nightly, release-perf, LTO disabled'

The output directory must not exist. It retains generated sources, every command
log, the seed archive, and every completed trial. The baseline uses one escaped
exact --source-glob per process. The candidate passes the same selector union in
one process, using literal --source-path arguments plus one overlapping exact
glob. Duplicate and absent paths are included in both workloads.

Rebuild counts come from newly retained lexical publication directories, with a
retention cap that prevents pruning during a trial. The candidate's report flag
is only a cross-check. These are measured lexical publications, not independent
timers or counters for the four database-derived rebuild routines.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import platform
import random
import selectors
import shutil
import signal
import statistics
import subprocess
import sys
import threading
import time
from pathlib import Path
from typing import Any

TARGET_MARKER = "cassforgetbenchtarget"
KEEP_MARKER = "cassforgetbenchkeep"
SURFACES = {
    "lexical": ["--mode", "lexical"],
    "semantic": ["--mode", "semantic", "--model", "hash"],
    "semantic-fast-only": ["--mode", "semantic", "--model", "hash", "--fast-only"],
    "hybrid": ["--mode", "hybrid", "--model", "hash"],
}
OUTPUT_LIMIT = 16 * 1024 * 1024


def require(condition: bool, message: str) -> None:
    if not condition:
        raise RuntimeError(message)


def digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def json_digest(value: Any) -> str:
    return hashlib.sha256(json.dumps(value, sort_keys=True).encode()).hexdigest()


def tree_digest(root: Path) -> str:
    entries = []
    for path in sorted(root.rglob("*")):
        require(not path.is_symlink(), f"unexpected symlink in owned fixture: {path}")
        if path.is_file():
            entries.append((str(path.relative_to(root)), digest(path)))
    return json_digest(entries)


def exact_glob(path: str) -> str:
    # Equivalent to glob::Pattern::escape(), including literal brackets.
    return "".join({"?": "[?]", "*": "[*]", "[": "[[]", "]": "[]]"}.get(c, c)
                   for c in path)


def write_json(path: Path, value: Any) -> None:
    with path.open("x", encoding="utf-8") as stream:
        json.dump(value, stream, indent=2, sort_keys=True)
        stream.write("\n")


def fixture_environment(home: Path, data: Path, retention: int) -> dict[str, str]:
    # Use an allowlist: ambient connector roots, exclusions, failpoints, and
    # configuration must never select real conversations or affect a trial.
    env = {name: os.environ[name] for name in ("PATH", "LD_LIBRARY_PATH", "DYLD_LIBRARY_PATH")
           if name in os.environ}
    env.update({
        "HOME": str(home),
        "CODEX_HOME": str(home / ".codex"),
        "CLAUDE_CONFIG_DIR": str(home / ".claude"),
        "XDG_DATA_HOME": str(home / ".local/share"),
        "XDG_CONFIG_HOME": str(home / ".config"),
        "XDG_CACHE_HOME": str(home / ".cache"),
        "XDG_STATE_HOME": str(home / ".local/state"),
        "OPENCODE_SQLITE_DB": str(home / ".local/share/opencode/opencode.db"),
        "CASS_DEVIN_DATA_ROOT": str(home / ".local/share/devin/cli"),
        "CASS_DATA_DIR": str(data),
        "CASS_AUTO_REFRESH": "0",
        "CASS_IGNORE_SOURCES_CONFIG": "1",
        "CASS_LEXICAL_PUBLISH_BACKUP_RETENTION": str(retention),
        "CODING_AGENT_SEARCH_NO_UPDATE_PROMPT": "1",
        "RUST_MIN_STACK": "134217728",
        "NO_COLOR": "1",
        "LANG": "C.UTF-8",
        "TZ": "UTC",
    })
    return env


class Runner:
    def __init__(self, root: Path, home: Path, env: dict[str, str], timeout: float):
        self.root, self.home, self.env, self.timeout = root, home, env, timeout
        self.logs = root / "command-logs"
        self.logs.mkdir()
        self.sequence = 0

    def run(self, binary: Path, args: list[str], label: str,
            allowed_exits: tuple[int, ...] = (0,)) -> tuple[bytes, bytes, dict[str, Any]]:
        self.sequence += 1
        stem = f"{self.sequence:05d}-{label}"
        stdout_path, stderr_path = self.logs / f"{stem}.stdout", self.logs / f"{stem}.stderr"
        argv = [str(binary), *args]
        with stdout_path.open("xb") as stdout, stderr_path.open("xb") as stderr:
            started = time.perf_counter_ns()
            process = subprocess.Popen(argv, cwd=self.home, env=self.env,
                                       stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                       stderr=subprocess.PIPE,
                                       start_new_session=True)
            expired = threading.Event()

            def expire() -> None:
                expired.set()
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass

            watchdog = threading.Timer(self.timeout, expire)
            watchdog.daemon = True
            watchdog.start()
            try:
                # Read readiness, rather than wait(timeout)'s exponentially
                # spaced polling, keeps short no-op timings from being rounded
                # up by the harness's polling sleeps.
                deadline = time.monotonic() + self.timeout
                total_output = 0
                with selectors.DefaultSelector() as selector:
                    for pipe, log in ((process.stdout, stdout), (process.stderr, stderr)):
                        require(pipe is not None, "subprocess pipe was not created")
                        os.set_blocking(pipe.fileno(), False)
                        selector.register(pipe, selectors.EVENT_READ, log)
                    while selector.get_map():
                        remaining = deadline - time.monotonic()
                        if remaining <= 0:
                            raise subprocess.TimeoutExpired(argv, self.timeout)
                        for key, _ in selector.select(remaining):
                            try:
                                chunk = os.read(key.fd, 65536)
                            except BlockingIOError:
                                continue
                            if not chunk:
                                selector.unregister(key.fileobj)
                                continue
                            key.data.write(chunk)
                            total_output += len(chunk)
                            require(total_output <= OUTPUT_LIMIT,
                                    f"{label}: output exceeded {OUTPUT_LIMIT} bytes")
                code = process.wait()
                elapsed_ms = (time.perf_counter_ns() - started) / 1_000_000
                watchdog.cancel()
                if expired.is_set():
                    raise subprocess.TimeoutExpired(argv, self.timeout)
            except BaseException:
                # Contain any children as well as the CLI on timeout/interruption.
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                process.wait()
                raise
            finally:
                watchdog.cancel()
                watchdog.join()
                for pipe in (process.stdout, process.stderr):
                    if pipe is not None:
                        pipe.close()
        stdout_bytes, stderr_bytes = stdout_path.read_bytes(), stderr_path.read_bytes()
        receipt = {"argv": argv, "exit_code": code, "process_wall_ms": elapsed_ms,
                   "stdout_path": str(stdout_path.relative_to(self.root)),
                   "stderr_path": str(stderr_path.relative_to(self.root)),
                   "stdout_sha256": digest(stdout_path), "stderr_sha256": digest(stderr_path)}
        write_json(self.logs / f"{stem}.receipt.json", receipt)
        require(code in allowed_exits,
                f"{label}: exit {code}; inspect {stderr_path} and {stdout_path}")
        return stdout_bytes, stderr_bytes, receipt

    def json(self, binary: Path, args: list[str], label: str) -> tuple[dict[str, Any], dict[str, Any]]:
        output, _, receipt = self.run(binary, args, label)
        value = json.loads(output)
        require(isinstance(value, dict), f"{label}: expected a JSON object")
        return value, receipt


def make_sources(home: Path, args: argparse.Namespace) -> dict[str, Any]:
    directory = home / ".codex/sessions/2026/09/20"
    directory.mkdir(parents=True)
    selected, kept = [], []
    for index in range(args.conversations):
        session = f"forget-bench-{index:04d}"
        # This file makes literal glob metacharacters part of the comparison.
        suffix = "-[literal]*?" if index == 0 else ""
        path = directory / f"rollout-{session}{suffix}.jsonl"
        marker = TARGET_MARKER if index < args.selected else KEEP_MARKER
        rows = [{"timestamp": "2026-09-20T10:00:00.000Z", "type": "session_meta",
                 "payload": {"id": session, "cwd": "/work/cass-forget-benchmark",
                             "cli_version": "0.42.0"}}]
        for message in range(args.messages):
            role = "user" if message % 2 == 0 else "assistant"
            text = (f"Synthetic conversation {session}, message {message:04d}: {marker}. "
                    "Verify exact archived conversation removal, persistent exclusions, "
                    "search consistency, and unchanged unrelated messages.")
            rows.append({"timestamp": f"2026-09-20T10:{message // 60:02d}:{message % 60:02d}.000Z",
                         "type": "response_item",
                         "payload": {"type": "message", "role": role,
                                     "content": [{"type": "input_text" if role == "user" else "text",
                                                  "text": text}]}})
        path.write_text("".join(json.dumps(row) + "\n" for row in rows), encoding="utf-8")
        (selected if index < args.selected else kept).append(str(path))
    absent = [str(directory / f"rollout-absent-{index:04d}.jsonl") for index in range(args.absent)]
    queue = [{"path": path, "kind": "path"} for path in selected + absent]
    random.Random(args.seed).shuffle(queue)
    queue.extend({"path": selected[index % len(selected)], "kind": "path"}
                 for index in range(args.duplicates))
    queue.append({"path": selected[0], "kind": "glob"})
    return {"selected_paths": selected, "kept_paths": kept, "absent_paths": absent,
            "queue": queue, "source_tree_sha256": tree_digest(directory),
            "source_bytes": sum(path.stat().st_size for path in directory.iterdir()),
            "source_directory": str(directory)}


def publication_witness(data: Path) -> dict[str, Any]:
    index = data / "index"
    manifests = [path for path in index.glob("*/MANIFEST")
                 if not path.parent.name.startswith(".")]
    require(len(manifests) == 1, f"expected one live lexical generation, found {manifests}")
    manifest = manifests[0]
    stat = manifest.stat()
    backup_root = index / ".lexical-publish-backups"
    backups = sorted(path.name for path in backup_root.iterdir()) if backup_root.exists() else []
    return {"live_path": str(manifest.relative_to(data)), "sha256": digest(manifest),
            "inode": stat.st_ino, "mtime_ns": stat.st_mtime_ns, "size": stat.st_size,
            "backups": backups}


def observed_publications(data: Path, before: dict[str, Any], after: dict[str, Any]) -> list[str]:
    previous, current = set(before["backups"]), set(after["backups"])
    require(previous <= current, "publication evidence was pruned during a trial")
    added = sorted(current - previous)
    require(len(added) <= 1, f"one forget invocation published {len(added)} generations")
    if added:
        retained = data / "index/.lexical-publish-backups" / added[0] / "MANIFEST"
        require(retained.is_file() and digest(retained) == before["sha256"],
                "new backup does not retain the observed prior live generation")
        require(after["sha256"] != before["sha256"], "publication did not replace live MANIFEST")
    else:
        require(after == before, "lexical generation changed without a retained publication")
    return added


def selectors_for(queue: list[dict[str, str]], batched: bool) -> list[str]:
    result = []
    for selector in queue:
        literal = batched and selector["kind"] == "path"
        result.extend(["--source-path" if literal else "--source-glob",
                       selector["path"] if literal else exact_glob(selector["path"])])
    return result


def forget_phase(runner: Runner, binary: Path, data: Path, queue: list[dict[str, str]],
                 selected: set[str], *, batched: bool, label: str,
                 candidate: bool) -> dict[str, Any]:
    groups = [queue] if batched else [[selector] for selector in queue]
    remaining = set(selected)
    measurements = []
    for index, group in enumerate(groups):
        before = publication_witness(data)
        report, receipt = runner.json(binary, ["forget", "--apply", "--json",
                                               *selectors_for(group, batched)], f"{label}-{index:04d}")
        after = publication_witness(data)
        added = observed_publications(data, before, after)
        expected = remaining.intersection(selector["path"] for selector in group)
        require(report.get("applied") is True, f"{label}: forget did not apply")
        require(report.get("conversations_matched") == len(expected)
                and report.get("conversations_deleted") == len(expected),
                f"{label}: exact selector union deleted an unexpected set: {report}")
        require(len(added) == int(bool(expected)), f"{label}: rebuild count disagrees with deletion")
        if candidate:
            require(report.get("derived_rebuilt") is bool(added),
                    f"{label}: derived_rebuilt disagrees with independent publication evidence")
        remaining.difference_update(expected)
        measurements.append({**receipt, "conversations_deleted": len(expected),
                             "lexical_publications": added,
                             "derived_rebuilt_reported": report.get("derived_rebuilt"),
                             "generation_before": before, "generation_after": after})
    require(not remaining, f"{label}: workload omitted selected conversations")
    result = {"invocations": len(measurements),
              "process_wall_ms": sum(row["process_wall_ms"] for row in measurements),
              "lexical_publications_observed": sum(len(row["lexical_publications"]) for row in measurements),
              "conversations_deleted": sum(row["conversations_deleted"] for row in measurements),
              "commands": measurements}
    print(json.dumps({"phase": label, **{key: value for key, value in result.items() if key != "commands"}}),
          file=sys.stderr, flush=True)
    return result


def search(runner: Runner, binary: Path, query: str, surface: str,
           limit: int, label: str, *, allow_unavailable: bool = False) -> list[tuple[str, str]] | None:
    stdout, stderr, receipt = runner.run(binary, ["search", query, "--robot", "--limit", str(limit),
                                                 *SURFACES[surface]], label,
                                        (0, 15) if allow_unavailable else (0,))
    if receipt["exit_code"] == 15:
        lines = stderr.decode("utf-8").splitlines()
        envelope = json.loads(next(line for line in reversed(lines) if line.strip()))
        require(envelope.get("error", {}).get("kind") == "semantic-unavailable",
                f"{label}: unexpected exit-15 failure")
        return None
    hits = json.loads(stdout).get("hits")
    require(isinstance(hits, list), f"{label}: search has no hit list")
    require(len(hits) < limit, f"{label}: verification was truncated at the requested limit")
    return sorted((hit["source_path"], hit["content"]) for hit in hits)


def archive_counts(runner: Runner, binary: Path, expected: int, messages: int,
                   label: str) -> dict[str, Any]:
    stats, _ = runner.json(binary, ["stats", "--json"], f"{label}-stats")
    require(stats.get("conversations") == expected and stats.get("messages") == expected * messages,
            f"{label}: wrong canonical counts: {stats}")
    analytics, _ = runner.json(binary, ["analytics", "status", "--json"], f"{label}-analytics")
    state = analytics.get("data", {})
    require(state.get("coverage", {}).get("total_messages") == expected * messages,
            f"{label}: analytics contains a different message count")
    # Keep persistent table observations, not time-dependent drift explanations
    # or the analytics envelope's per-command elapsed time.
    tables = state.get("tables")
    require(isinstance(tables, list), f"{label}: analytics has no table inventory")
    fields = ("table", "exists", "row_count", "min_day_id", "max_day_id", "last_updated")
    return {"conversations": stats["conversations"], "messages": stats["messages"],
            "analytics_message_count": state["coverage"]["total_messages"],
            "analytics_tables": sorted(({key: table.get(key) for key in fields} for table in tables),
                                       key=lambda table: table["table"])}


def exclusions(runner: Runner, binary: Path, label: str) -> dict[str, Any]:
    report, _ = runner.json(binary, ["forget", "--list", "--json"], label)
    require(isinstance(report.get("exclusions"), list) and isinstance(report.get("legacy_sources"), list),
            f"{label}: no durable exclusion inventory")
    return {key: report[key] for key in ("exclusions", "legacy_sources")}


def verify_deleted(runner: Runner, binary: Path, fixture: dict[str, Any],
                   kept_before: list[tuple[str, str]], limit: int, label: str,
                   *, allow_unavailable: bool) -> dict[str, Any]:
    selected = set(fixture["selected_paths"])
    surfaces = {}
    for surface in SURFACES:
        hits = search(runner, binary, TARGET_MARKER, surface, limit, f"{label}-{surface}",
                      allow_unavailable=allow_unavailable and surface.startswith("semantic"))
        if hits is None:
            surfaces[surface] = "failed-closed:semantic-unavailable"
        else:
            require(all(path not in selected and TARGET_MARKER not in text for path, text in hits),
                    f"{label}: {surface} exposed forgotten content")
            surfaces[surface] = {"hits": len(hits), "identities_sha256": json_digest(hits)}
    kept = search(runner, binary, KEEP_MARKER, "lexical", limit, f"{label}-kept")
    require(kept == kept_before, f"{label}: unrelated lexical hit identities/content changed")
    return {"surfaces": surfaces, "kept_hits": len(kept_before),
            "kept_identities_sha256": json_digest(kept_before)}


def trial(runner: Runner, binary: Path, root: Path, fixture: dict[str, Any],
          args: argparse.Namespace, seed_hash: str, name: str, arm: str) -> dict[str, Any]:
    data, seed = root / "live-data", root / "seed-data"
    trial_dir = root / "trials" / name
    trial_dir.mkdir(parents=True)
    shutil.copytree(seed, data)
    require(tree_digest(data) == seed_hash, f"{name}: restored archive differs from seed")
    is_candidate = arm == "candidate"
    # Measure legacy no-op calls on identical starting bytes, before either
    # arm's differing deletion/checkpoint/publication history exists.
    original_counts = archive_counts(runner, binary, args.conversations, args.messages,
                                     f"{name}-noop-seed")
    original_exclusions = exclusions(runner, binary, f"{name}-noop-seed-exclusions")
    absent = fixture["absent_paths"]
    noop_queue = [{"path": absent[index % len(absent)], "kind": "path"}
                  for index in range(args.noop_probes)]
    phases = {"serial_noop_control": forget_phase(
        runner, binary, data, noop_queue, set(), batched=False,
        label=f"{name}-serial-noop-control", candidate=is_candidate)}
    require(archive_counts(runner, binary, args.conversations, args.messages,
                           f"{name}-noop-seed-after") == original_counts
            and exclusions(runner, binary, f"{name}-noop-seed-exclusions-after") == original_exclusions,
            f"{name}: absent-only control changed canonical/analytics/exclusion state")
    data.rename(trial_dir / "noop-control-archive")
    shutil.copytree(seed, data)
    require(tree_digest(data) == seed_hash, f"{name}: deletion archive differs from seed")
    limit = args.conversations * args.messages + 1
    archive_counts(runner, binary, args.conversations, args.messages, f"{name}-before")
    kept = search(runner, binary, KEEP_MARKER, "lexical", limit, f"{name}-before-kept")
    targets = search(runner, binary, TARGET_MARKER, "lexical", limit, f"{name}-before-targets")
    require(kept is not None and {path for path, _ in kept} == set(fixture["kept_paths"]),
            f"{name}: the seed did not expose every unrelated conversation")
    require(targets is not None and {path for path, _ in targets} == set(fixture["selected_paths"]),
            f"{name}: the seed did not expose every selected conversation")
    phases["forget"] = forget_phase(runner, binary, data, fixture["queue"],
                                     set(fixture["selected_paths"]), batched=is_candidate,
                                     label=f"{name}-forget", candidate=is_candidate)
    counts = archive_counts(runner, binary, args.conversations - args.selected, args.messages,
                            f"{name}-after")
    tombstones = exclusions(runner, binary, f"{name}-exclusions-before-noop")
    phases["repeat_noop"] = forget_phase(runner, binary, data, fixture["queue"], set(),
                                          batched=is_candidate, label=f"{name}-repeat-noop",
                                          candidate=is_candidate)
    require(exclusions(runner, binary, f"{name}-exclusions-after-noop") == tombstones,
            f"{name}: zero-match operations changed durable exclusions")
    require(archive_counts(runner, binary, args.conversations - args.selected, args.messages,
                           f"{name}-after-noop") == counts,
            f"{name}: zero-match operations changed analytics or counts")
    after = verify_deleted(runner, binary, fixture, kept, limit, f"{name}-after", allow_unavailable=True)
    runner.json(binary, ["index", "--full", "--semantic", "--embedder", "hash", "--json",
                         "--no-progress-events"], f"{name}-rescan")
    archive_counts(runner, binary, args.conversations - args.selected, args.messages, f"{name}-rescan")
    rescanned = verify_deleted(runner, binary, fixture, kept, limit, f"{name}-rescan", allow_unavailable=False)
    require(tree_digest(Path(fixture["source_directory"])) == fixture["source_tree_sha256"],
            f"{name}: source files were modified")
    data.rename(trial_dir / "archive")
    result = {"name": name, "arm": arm, "seed_sha256": seed_hash, "phases": phases,
              "after": after, "after_full_rescan": rescanned,
              "durable_exclusions_sha256": json_digest(tombstones)}
    write_json(trial_dir / "trial.json", result)
    return result


def comparison(left: dict[str, Any], right: dict[str, Any]) -> dict[str, Any]:
    phases = {}
    for phase in left["phases"]:
        a, b = left["phases"][phase], right["phases"][phase]
        phases[phase] = {"left_ms": a["process_wall_ms"], "right_ms": b["process_wall_ms"],
                         "left_over_right": a["process_wall_ms"] / b["process_wall_ms"],
                         "left_publications": a["lexical_publications_observed"],
                         "right_publications": b["lexical_publications_observed"]}
    return {"left": left["name"], "right": right["name"], "phases": phases}


def run(args: argparse.Namespace) -> dict[str, Any]:
    root = args.output_dir.resolve()
    binaries = {arm: path.resolve(strict=True) for arm, path in
                (("baseline", args.baseline_cass), ("candidate", args.candidate_cass))}
    for binary in binaries.values():
        require(binary.is_file() and os.access(binary, os.X_OK), f"not an executable file: {binary}")
    hashes = {arm: digest(path) for arm, path in binaries.items()}
    require(hashes["baseline"] != hashes["candidate"], "baseline and candidate binaries are identical")
    root.mkdir(parents=True, exist_ok=False)
    home = root / "fixture-home"
    home.mkdir()
    fixture = make_sources(home, args)
    data = root / "live-data"
    retention = len(fixture["queue"]) + 16
    runner = Runner(root, home, fixture_environment(home, data, retention), args.timeout)
    report: dict[str, Any] = {
        "schema_version": 1, "status": "running", "platform": platform.platform(),
        "python": sys.version, "build_description": args.build_description,
        "binaries": {arm: {"path": str(path), "sha256": hashes[arm]} for arm, path in binaries.items()},
        "fixture": fixture, "parameters": {key: value for key, value in vars(args).items()
                                              if not isinstance(value, Path)},
        "measurement": {
            "wall_time": "sum of actual forget process startup-through-exit wall time; fixture copying, "
                         "witness inspection, and correctness verification are excluded",
            "timing_method": "perf_counter_ns around Popen through blocking wait; pipes drained by readiness; "
                             "a cancellable watchdog kills the process group at the deadline; no timed-wait polling",
            "cache_policy": "warm OS page cache; same seed bytes restored at the same path; no cache drop",
            "serial_noop_control": "identical absent-only legacy selector calls on a separate restoration "
                                   "of the exact seed archive, before deletion history can differ",
            "rust_min_stack": runner.env["RUST_MIN_STACK"],
            "rebuild_counter": "new retained prior-generation directories, checked against prior live MANIFEST",
            "database_rebuild_counts": "not independently instrumented; successful current forget code couples "
                                       "FTS, analytics, daily stats, and token daily stats rebuilding to publication",
            "retention_override": retention,
            "retention_effect": "both arms retain all prior generations; timings include retention bookkeeping "
                                "and do not represent default retention pruning",
        },
        "trials": [],
    }
    try:
        for arm, binary in binaries.items():
            version, _, _ = runner.run(binary, ["--version"], f"{arm}-version")
            report["binaries"][arm]["version"] = version.decode().strip()
        runner.json(binaries["baseline"], ["index", "--full", "--semantic", "--embedder", "hash",
                                           "--json", "--no-progress-events"], "seed-index")
        data.rename(root / "seed-data")
        seed_hash = tree_digest(root / "seed-data")
        report["seed_sha256"] = seed_hash
        for side in ("left", "right"):
            report["trials"].append(trial(runner, binaries["baseline"], root, fixture, args, seed_hash,
                                          f"null-{side}", "baseline"))
        report["null_control"] = comparison(report["trials"][0], report["trials"][1])
        pairs = []
        for iteration in range(args.rounds):
            arms = ("baseline", "candidate") if iteration % 2 == 0 else ("candidate", "baseline")
            pair = {}
            for arm in arms:
                result = trial(runner, binaries[arm], root, fixture, args, seed_hash,
                               f"round-{iteration + 1}-{arm}", arm)
                report["trials"].append(result)
                pair[arm] = result
            pairs.append(comparison(pair["baseline"], pair["candidate"]))
        report["paired_comparisons"] = pairs
        report["summary"] = {
            phase: {"median_baseline_ms": statistics.median(pair["phases"][phase]["left_ms"] for pair in pairs),
                    "median_candidate_ms": statistics.median(pair["phases"][phase]["right_ms"] for pair in pairs),
                    "paired_ratios": [pair["phases"][phase]["left_over_right"] for pair in pairs],
                    "null_ratio": report["null_control"]["phases"][phase]["left_over_right"]}
            for phase in pairs[0]["phases"]
        }
        require(all(digest(path) == hashes[arm] for arm, path in binaries.items()),
                "a measured binary changed during the benchmark")
        report["status"] = "passed"
        write_json(root / "report.json", report)
        return report
    except BaseException as error:
        report.update(status="failed", error=f"{type(error).__name__}: {error}")
        write_json(root / "failure.json", report)
        raise


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--baseline-cass", type=Path, required=True)
    parser.add_argument("--candidate-cass", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True, help="new directory; all synthetic evidence is retained")
    parser.add_argument("--build-description", required=True, help="record the identical toolchain/profile/features used")
    parser.add_argument("--conversations", type=int, default=64)
    parser.add_argument("--selected", type=int, default=8)
    parser.add_argument("--absent", type=int, default=72)
    parser.add_argument("--duplicates", type=int, default=2)
    parser.add_argument("--messages", type=int, default=4, help="messages per generated conversation")
    parser.add_argument("--rounds", type=int, default=2, help="at least two for baseline/candidate and reversed order")
    parser.add_argument("--noop-probes", type=int, default=8)
    parser.add_argument("--seed", type=int, default=544)
    parser.add_argument("--timeout", type=float, default=600, help="deadline in seconds for each actual CLI invocation")
    args = parser.parse_args()
    if (not 1 <= args.selected < args.conversations or args.absent < 1 or args.duplicates < 0
            or not 2 <= args.messages <= 60 or args.rounds < 2 or args.noop_probes < 1
            or not math.isfinite(args.timeout) or args.timeout <= 0):
        parser.error("require 1 <= selected < conversations, absent >= 1, duplicates >= 0, "
                     "2 <= messages <= 60, rounds >= 2, noop-probes >= 1, and a positive finite timeout")
    if os.name != "posix":
        parser.error("this harness requires POSIX process-group containment")
    return args


if __name__ == "__main__":
    arguments = parse_args()
    try:
        completed = run(arguments)
    except (OSError, ValueError, RuntimeError, subprocess.TimeoutExpired) as error:
        print(f"benchmark failed: {error}", file=sys.stderr)
        sys.exit(1)
    print(json.dumps({"status": completed["status"],
                      "report": str(arguments.output_dir.resolve() / "report.json"),
                      "summary": completed["summary"]}, indent=2))
