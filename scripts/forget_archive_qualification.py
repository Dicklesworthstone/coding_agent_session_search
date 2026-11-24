#!/usr/bin/env python3
"""GH565 native archive qualification. Only newly generated synthetic archives.

This driver never accepts a pre-existing archive, builds CASS, changes a cgroup
parent, dispatches CI, downloads models, or writes to a Git remote. See
docs/perf/forget-archive-qualification.md for build and evidence contracts.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import platform
import shutil
import statistics
import subprocess
import sys
import time
from pathlib import Path
from typing import Any

import benchmark_forget as legacy
import forget_archive_fixture as fixture
import forget_benchmark_process as process

INSPECTED_APPLICATION_BASELINE = "710435869716073a3b5aa015ad3f9872426e2e4a"
PROFILES = {"smoke": fixture.SMOKE, "small": fixture.SMALL,
            "representative": fixture.REPRESENTATIVE}
PHASES = ("fallback-fts-preflight", "canonical-delete", "fallback-fts",
          "analytics", "daily-stats", "token-daily-stats", "lexical-index")
SENTINEL = {"schema_version": 1, "synthetic": True}
ROW_FIELDS = ("id", "source_path", "external_id", "source_id", "message_count",
              "content_bytes", "conversation_sha256", "messages_sha256",
              "token_usage_count", "token_usage_total_tokens", "token_usage_sha256")
JSON_LIMIT = 64 * 1024 * 1024


def require(condition: bool, message: str) -> None:
    if not condition:
        raise RuntimeError(message)


def sha256(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def save(path: Path, value: Any) -> None:
    with path.open("x", encoding="utf-8") as stream:
        json.dump(value, stream, sort_keys=True, indent=2, allow_nan=False)
        stream.write("\n")


def no_links(path: Path) -> None:
    absolute = path.absolute()
    for part in (absolute, *absolute.parents):
        require(not part.is_symlink(), f"symlink path refused: {part}")


def new_root(path: Path) -> Path:
    no_links(path)
    root = path.absolute()
    require(root.parent.is_dir(), "output parent must already exist")
    root.mkdir(mode=0o700)
    save(root / "gh565-owned.json", SENTINEL)
    return root


def tree_bytes(path: Path) -> int:
    total = 0
    for directory, dirs, files in os.walk(path, followlinks=False):
        for name in dirs + files:
            child = Path(directory) / name
            require(not child.is_symlink(), f"fixture link refused: {child}")
        total += sum((Path(directory) / name).stat().st_size for name in files)
    return total


def read_json(path: Path) -> dict[str, Any]:
    require(path.stat().st_size <= JSON_LIMIT, f"JSON exceeds {JSON_LIMIT} bytes: {path}")
    with path.open(encoding="utf-8") as stream:
        value = json.load(stream)
    require(isinstance(value, dict), f"not a JSON object: {path}")
    return value


def phase_names(receipt: dict[str, Any]) -> list[str]:
    return [row["phase"] for row in receipt.get("phases", [])]


def oom_kills(receipt: dict[str, Any]) -> int:
    return int((receipt.get("memory", {}).get("events_delta") or {}).get("oom_kill", 0))


def successful(receipt: dict[str, Any]) -> bool:
    return clean_observation(receipt) and receipt.get("exit_code") == 0


def clean_observation(receipt: dict[str, Any]) -> bool:
    return (receipt.get("status") == "exited"
            and not receipt.get("timed_out") and oom_kills(receipt) == 0
            and not receipt.get("output_limit_exceeded")
            and not receipt.get("descendants_terminated_after_root_exit")
            and not receipt.get("output_pipes_remained_open_after_kill"))


class ProbeUnavailable(RuntimeError):
    """The strict reader produced no usable snapshot, with its receipt retained.

    Parsed canonical/privacy invariant failures are ordinary RuntimeErrors and
    must never be treated as this missing-evidence case.
    """
    def __init__(self, message: str, receipt: dict[str, Any]):
        super().__init__(message)
        self.receipt = receipt


class Commands:
    def __init__(self, root: Path, home: Path, data: Path, args: argparse.Namespace,
                 memory_max: int | None) -> None:
        self.root, self.home, self.data = root, home, data
        self.args, self.memory_max = args, memory_max
        self.verification_max = (args.verification_memory_max
                                 or (max(args.memory_max) if args.memory_max else None))
        self.logs = root / "command-logs"
        self.logs.mkdir(parents=True)
        self.env = legacy.fixture_environment(home, data, 1)
        # These are the production defaults/profile choices, not page-budget
        # reductions intended to make an RSS result look better.
        self.env["CASS_TANTIVY_REBUILD_PROFILE"] = "bounded"
        if args.fts_shadow_max_messages is not None:
            self.env["CASS_FTS_SHADOW_MAX_MESSAGES"] = str(args.fts_shadow_max_messages)
        self.sequence = 0

    def invoke(self, binary: Path, argv: list[str], label: str, *,
               measured: bool = False,
               interrupt_phase: str | None = None,
               interrupt_after_seconds: float | None = None) -> dict[str, Any]:
        self.sequence += 1
        return process.run(
            [str(binary), *argv], label=f"{self.sequence:04d}-{label}",
            env=self.env, cwd=self.home, log_dir=self.logs,
            timeout=self.args.timeout,
            memory_max=self.memory_max if measured else self.verification_max,
            cgroup_parent=self.args.cgroup_parent,
            allow_uncapped=self.args.allow_uncapped,
            interrupt_phase=interrupt_phase,
            interrupt_after_seconds=interrupt_after_seconds,
            sample_interval=self.args.sample_interval,
        )

    def cass(self, argv: list[str], label: str, **kwargs: Any) -> dict[str, Any]:
        return self.invoke(self.args.cass, argv, label, **kwargs)

    def json(self, argv: list[str], label: str) -> tuple[dict[str, Any], dict[str, Any]]:
        receipt = self.cass(argv, label)
        require(successful(receipt), f"{label}: failed; inspect command receipt/logs")
        return read_json(Path(receipt["stdout_path"])), receipt

    def probe(self, label: str) -> dict[str, Any]:
        argv = [str(self.data)]
        if self.args.quick_check:
            argv.append("--quick-check")
        try:
            receipt = self.invoke(self.args.probe, argv, label)
        except process.ProcessSetupError as error:
            raise ProbeUnavailable(f"{label}: canonical probe could not run", error.receipt) from error
        if not successful(receipt):
            raise ProbeUnavailable(f"{label}: canonical read-only probe failed", receipt)
        try:
            result = read_json(Path(receipt["stdout_path"]))
        except (OSError, ValueError, RuntimeError) as error:
            raise ProbeUnavailable(f"{label}: canonical probe returned no usable JSON: {error}",
                                   receipt) from error
        # Keep all checks on an admitted snapshot outside the unavailable path.
        # A failed integrity/orphan assertion is evidence of a failure, not a
        # reason to downgrade the trial to inconclusive.
        require(result.get("schema_version") == 1, "unsupported probe receipt")
        require(result["canonical"]["orphan_messages"] == 0, "orphan canonical messages")
        require(result["canonical"]["orphan_token_usage_conversations"] == 0,
                "orphan canonical token ledger rows")
        if self.args.quick_check:
            require(result.get("quick_check", {}).get("ok") is True, "quick_check failed")
        return result


def rows_by_path(snapshot: dict[str, Any]) -> dict[str, dict[str, Any]]:
    rows = snapshot["conversations"]
    require(isinstance(rows, list), "probe omitted conversation inventory")
    paths = {row["source_path"]: row for row in rows}
    require(len(paths) == len(rows), "fixture source path maps to multiple conversations")
    require(len(rows) == snapshot["canonical"]["conversations"], "probe count mismatch")
    for row in rows:
        require(all(key in row for key in ROW_FIELDS), "probe omitted canonical digest fields")
    return paths


def tombstone_paths(snapshot: dict[str, Any], table: str) -> set[str]:
    return {row["source_path"] for row in snapshot["tombstones"][table]["rows"]}


def validate_seed(snapshot: dict[str, Any], manifest: dict[str, Any]) -> None:
    actual = rows_by_path(snapshot)
    expected = {row["path"]: row for row in manifest["conversations"]}
    require(set(actual) == set(expected), "index admitted missing or ambient conversations")
    for path, plan in expected.items():
        require(actual[path]["message_count"] == plan["message_count"],
                f"connector changed expected message count: {path}")
        require(actual[path]["content_bytes"] == plan["content_bytes"],
                f"connector changed expected UTF-8 content bytes: {path}")
    require(not snapshot["pending"]["effective"], "seed has a pending privacy repair")
    require(not tombstone_paths(snapshot, "forgotten_conversations")
            and not tombstone_paths(snapshot, "forgotten_sources"), "seed already has exclusions")
    validate_analytics(snapshot)
    require(snapshot["canonical"]["token_usage_rows"] > 0,
            "seed has an empty token ledger; token repair was not populated")
    require(snapshot["canonical"]["token_usage_rows"] == snapshot["canonical"]["messages"],
            "inline seed ingestion did not populate the complete per-message token ledger")


def validate_analytics(snapshot: dict[str, Any]) -> None:
    n = snapshot["canonical"]["messages"]
    analytics = snapshot["analytics"]
    require(analytics["message_metrics"]["row_count"] == n,
            "message_metrics is incomplete or duplicated")
    for name in ("daily_stats", "usage_daily", "usage_hourly", "usage_models_daily"):
        require(analytics[name]["exists"] and analytics[name]["message_count_sum"] == n,
                f"{name} does not cover every canonical message")
    require(analytics["token_usage"]["row_count"] == snapshot["canonical"]["token_usage_rows"],
            "token ledger inventory disagrees")
    # Both totals come from the probe's independent scans; 'all/all' filtering
    # on token_daily_stats is part of the helper's explicit schema contract.
    require(analytics["token_daily_stats"]["exists"]
            and analytics["token_daily_stats"]["row_count"] > 0,
            "token rollup is empty")
    require(analytics["token_daily_stats"]["total_tokens_sum"]
            == analytics["token_usage"]["total_tokens_sum"],
            "token rollup sum differs from canonical ledger")
    require(analytics["token_daily_stats"]["message_count_sum"]
            == snapshot["canonical"]["token_usage_rows"],
            "token rollup message count differs from canonical ledger")


def verify_snapshot(before: dict[str, Any], after: dict[str, Any],
                    selected_paths: set[str], *, partial: bool = False,
                    require_repaired: bool = True) -> dict[str, Any]:
    original, current = rows_by_path(before), rows_by_path(after)
    require(selected_paths <= original.keys(), "selector is outside the synthetic seed")
    removed = set(original) - set(current)
    require(set(current) <= set(original), "unexpected new canonical history")
    require(removed <= selected_paths, "forget removed unrelated history")
    if not partial:
        require(removed == selected_paths, "forget did not remove the exact selected cohort")
    for path, row in current.items():
        require(all(row[key] == original[path][key] for key in ROW_FIELDS),
                f"canonical identity/content/ledger changed for retained history: {path}")
    for field, per_row in (("messages", "message_count"), ("content_bytes", "content_bytes"),
                           ("token_usage_rows", "token_usage_count")):
        expected = before["canonical"][field] - sum(original[path][per_row] for path in removed)
        require(after["canonical"][field] == expected, f"wrong canonical {field}")
    # Ordinary one-session Codex files use source stamps. Per-session identity
    # tombstones belong to shared stores, which this fixture does not simulate.
    require(tombstone_paths(after, "forgotten_sources") == removed,
            "source tombstones do not durably cover exactly committed deletions")
    require(after["tombstones"]["forgotten_conversations"]
            == before["tombstones"]["forgotten_conversations"],
            "unexpected shared-store session tombstones in a Codex file fixture")
    if require_repaired:
        require(not after["pending"]["effective"], "pending-repair marker did not clear")
        require(not after["pending"]["sql_marker_present"], "SQL pending marker still exists")
        require(not after["pending"]["reader_pending"], "reader privacy fence still pending")
        validate_analytics(after)
    return {"deleted_conversations": len(removed),
            "deleted_messages": sum(original[path]["message_count"] for path in removed),
            "retained_conversations_hashed": len(current),
            "retained_messages_hashed": after["canonical"]["messages"],
            "retained_content_bytes_hashed": after["canonical"]["content_bytes"],
            "tombstones": {name: after["tombstones"][name]["sha256"]
                           for name in ("forgotten_sources", "forgotten_conversations")}}


def error_kind(receipt: dict[str, Any]) -> str | None:
    stderr = Path(receipt["stderr_path"])
    require(stderr.stat().st_size <= JSON_LIMIT, "oversized error diagnostics")
    for line in reversed(stderr.read_text(encoding="utf-8", errors="replace").splitlines()):
        try:
            value = json.loads(line)
        except ValueError:
            continue
        if isinstance(value, dict) and isinstance(value.get("error"), dict):
            return value["error"].get("kind")
    return None


def search(commands: Commands, marker: str, mode: str, label: str,
           *, pending: bool = False) -> list[dict[str, Any]] | None:
    receipt = commands.cass(["search", marker, "--robot", "--limit", "5",
                             "--robot-meta", *legacy.SURFACES[mode]], label)
    require(clean_observation(receipt), f"{mode}: search observation was not cleanly completed")
    if pending:
        require(receipt.get("exit_code") == 9 and error_kind(receipt) == "forget-repair-pending",
                f"{mode}: pending privacy repair did not reject the reader")
        require(Path(receipt["stdout_path"]).stat().st_size == 0,
                "pending refusal emitted data before failing")
        return None
    if receipt.get("exit_code") == 15 and mode.startswith("semantic"):
        require(error_kind(receipt) == "semantic-unavailable",
                "unexpected semantic error, not a fail-closed unavailable result")
        require(Path(receipt["stdout_path"]).stat().st_size == 0,
                "semantic refusal emitted data before failing")
        return None
    require(successful(receipt), f"{mode}: search failed, absence was not verified")
    value = read_json(Path(receipt["stdout_path"]))
    require(not value.get("budget", {}).get("timed_out"), "search timed out instead of proving absence")
    hits = value.get("hits")
    require(isinstance(hits, list), "search omitted hit list")
    return hits


def search_checks(commands: Commands, marker: str, selected: set[str],
                  manifest: dict[str, Any], *, pending: bool = False) -> dict[str, Any]:
    outcomes: dict[str, Any] = {}
    for mode in legacy.SURFACES:
        hits = search(commands, marker, mode, f"search-forgotten-{mode}", pending=pending)
        if pending:
            outcomes[mode] = "refused:forget-repair-pending"
        elif hits is None:
            outcomes[mode] = "refused:semantic-unavailable"
        else:
            require(all(hit.get("source_path") not in selected
                        and marker not in hit.get("content", "") for hit in hits),
                    f"{mode}: forgotten content remains searchable")
            if mode == "lexical":
                require(not hits, "forgotten lexical marker still returns a result")
            outcomes[mode] = {"returned_hits": len(hits), "forgotten_hits": 0}
    if pending:
        keep_hits = search(commands, fixture.KEEP_MARKER, "lexical",
                           "search-kept-while-pending", pending=True)
        require(keep_hits is None, "pending search unexpectedly completed")
        return outcomes
    # All unrelated canonical rows are hashed. A fixed deterministic sample
    # checks search serving without returning millions of bodies to Python.
    kept = [row for row in manifest["conversations"] if row["cohort"] == "keep"]
    sample = sorted({0, len(kept) // 2, len(kept) - 1})
    for index in sample:
        row = kept[index]
        hits = search(commands, row["witness"], "lexical", f"kept-witness-{index}")
        require(hits is not None and any(hit.get("source_path") == row["path"] for hit in hits),
                "unrelated search witness is unavailable")
    outcomes["unrelated_search_witnesses"] = len(sample)
    return outcomes


def selectors(manifest: dict[str, Any], case: str) -> list[str]:
    cohort = manifest["cohorts"][case]
    # Match the reporter's exact single-source-directory selector shape.
    return ["--source-glob", legacy.exact_glob(cohort["source_directory"]) + "/*.jsonl"]


def positive_target_control(commands: Commands, manifest: dict[str, Any], case: str) -> None:
    cohort = manifest["cohorts"][case]
    hits = search(commands, cohort["marker"], "lexical", "target-positive-control")
    require(hits and any(hit.get("source_path") in set(cohort["paths"]) for hit in hits),
            "selected marker was not searchable before forgetting")


def checkpoint(root: Path, report: dict[str, Any], sequence: int) -> None:
    directory = root / "checkpoints"
    directory.mkdir(exist_ok=True)
    save(directory / f"{sequence:04d}.json", report)


def preserve_interrupted_archive(live: Path, trial_root: Path) -> dict[str, Any]:
    """Retain the quiescent pre-recovery bytes without opening the database.

    Neither strict probes nor recovery commands use the saved directory. A
    reflink shares CoW extents, never writable inodes; ordinary copy is the
    fallback. Keep partial copies and copy diagnostics on every failure.
    """
    destination = trial_root / "interrupted-archive"
    no_links(live)
    no_links(destination)
    require(live.is_dir(), "interrupted fixture directory is missing")
    require(not destination.exists(), "interrupted evidence destination already exists")
    required = tree_bytes(live)
    source_files = sorted((path for path in live.rglob("*") if not path.is_dir()),
                          key=lambda path: str(path.relative_to(live)))
    require(all(path.is_file() and not path.is_symlink() for path in source_files),
            "interrupted fixture contains a non-regular file")
    free = shutil.disk_usage(trial_root).free
    require(free >= 3 * required,
            "insufficient disk to preserve the interrupted archive and retain repair output")
    stdout, stderr = trial_root / "interrupted-copy.stdout", trial_root / "interrupted-copy.stderr"
    argv = ["cp", "-a", "--reflink=auto", "--", str(live), str(destination)]
    receipt: dict[str, Any] = {
        "status": "copying", "path": str(destination), "argv": argv,
        "logical_bytes": required, "free_bytes_before": free,
        "stdout_path": str(stdout), "stderr_path": str(stderr),
        "receipt_path": str(trial_root / "interrupted-copy.json"),
        "method": "independent reflink-auto copy, never hardlinks to the working archive",
    }
    try:
        with stdout.open("xb") as out, stderr.open("xb") as err:
            copied = subprocess.run(argv, stdin=subprocess.DEVNULL, stdout=out, stderr=err)
        receipt["exit_code"] = copied.returncode
        require(copied.returncode == 0, "interrupted archive copy failed; inspect copy diagnostics")
        require(tree_bytes(destination) == required, "interrupted archive copy changed byte totals")
        source_names = [str(path.relative_to(live)) for path in source_files]
        copied_names = sorted(str(path.relative_to(destination))
                              for path in destination.rglob("*") if not path.is_dir())
        require(source_names == copied_names, "interrupted archive copy changed the file inventory")
        files = []
        for source, relative in zip(source_files, source_names):
            saved = destination / relative
            require(saved.is_file() and not saved.is_symlink(),
                    "interrupted evidence contains a non-regular file")
            left, right = source.stat(), saved.stat()
            require((left.st_dev, left.st_ino) != (right.st_dev, right.st_ino),
                    "interrupted evidence is hardlinked to the working archive")
            require(left.st_size == right.st_size, "interrupted archive copy changed a file size")
            digest = sha256(saved)
            require(digest == sha256(source), "interrupted archive copy changed file content")
            files.append({"path": relative, "bytes": right.st_size, "sha256": digest})
        receipt.update(status="preserved", files=files,
                       contents_sha256=legacy.json_digest(files), verified_distinct_inodes=True)
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
        receipt.update(status="failed", error=f"{type(error).__name__}: {error}")
        raise
    finally:
        save(Path(receipt["receipt_path"]), receipt)
    return receipt


def run_trial(profile_root: Path, home: Path, manifest: dict[str, Any],
              seed: dict[str, Any], args: argparse.Namespace, cap: int | None,
              case: str, name: str, interrupt: str | None) -> dict[str, Any]:
    trial_root = profile_root / "trials" / name
    trial_root.mkdir(parents=True)
    live, seed_dir = profile_root / "live-data", profile_root / "seed-data"
    require(not live.exists(), "owned live fixture from earlier trial was not parked")
    required = tree_bytes(seed_dir)
    require(shutil.disk_usage(profile_root).free >= required + 2 * required,
            "insufficient disk for a trial copy plus repair output; evidence is retained")
    # No delete/cleanup. A reflink is an independent CoW archive; GNU cp falls
    # back to a normal byte copy and never hardlinks writable database files.
    subprocess.run(["cp", "-a", "--reflink=auto", "--", str(seed_dir), str(live)],
                   check=True, stdin=subprocess.DEVNULL)
    commands = Commands(trial_root, home, live, args, cap)
    result: dict[str, Any] = {"name": name, "case": case, "memory_max": cap,
                              "interruption_phase": interrupt, "status": "running"}
    chosen = set(manifest["cohorts"][case]["paths"])
    marker = manifest["cohorts"][case]["marker"]
    argv = ["forget", "--apply", "--json", *selectors(manifest, case)]
    try:
        # Copying creates a new database inode while preserving the old
        # FrankenSQLite namespace sidecars. Admit that owned copy through the
        # ordinary writable index route before a strict evidence read. This is
        # recorded setup, outside forget timing; the probe remains non-repairing.
        admission = commands.cass(
            ["index", "--rebuild-profile", "bounded", "--json", "--no-progress-events"],
            "admit-restored-seed", measured=False)
        result["restored_seed_admission"] = {
            "included_in_forget_timing": False, "measurement": admission}
        require(successful(admission), "copied seed index admission did not complete")
        result["restored_seed_admission"]["application_report"] = read_json(Path(admission["stdout_path"]))
        restored = commands.probe("restored-seed")
        seed_rows = rows_by_path(seed)
        require(restored["canonical"] == seed["canonical"], "copied seed canonical totals changed")
        require(rows_by_path(restored) == seed_rows, "copied seed canonical digests changed")
        require(restored["analytics"] == seed["analytics"], "copied seed analytics baseline changed")
        require(restored["tombstones"] == seed["tombstones"], "copied seed tombstones changed")
        require(not restored["pending"]["effective"], "copied seed has pending repair")
        positive_target_control(commands, manifest, case)
        preview, _ = commands.json(["forget", "--json", *selectors(manifest, case)], "preview")
        expected_messages = sum(seed_rows[path]["message_count"] for path in chosen)
        require(preview.get("conversations_matched") == len(chosen)
                and preview.get("messages_matched") == expected_messages,
                "preview selected the wrong cohort")
        if interrupt:
            delay = args.canonical_interrupt_delay if interrupt == "canonical-delete" else 0.0
            measurement = commands.cass(argv, "interrupted-forget", interrupt_phase=interrupt,
                                        interrupt_after_seconds=delay, measured=True)
        else:
            measurement = commands.cass(argv, "forget", measured=True)
        result["measurement"] = measurement
        if interrupt:
            require(measurement.get("interruption", {}).get("sent") is True
                    and measurement.get("exit_code") == -9
                    and oom_kills(measurement) == 0,
                    "scheduled SIGKILL did not occur independently of OOM")
            # Preserve the killed process's DB/WAL/SHM/fence bytes before a
            # reader or recovery command can alter any working-copy sidecar.
            result["interrupted_archive"] = preserve_interrupted_archive(live, trial_root)
            committed: int | None = None
            partial_commit_proved = False
            try:
                interrupted = commands.probe("interrupted-canonical")
            except ProbeUnavailable as error:
                # A strict reader may correctly refuse stale SHM/dirty-WAL
                # state. Keep that failed receipt and exercise the application
                # recovery anyway; do not repair through the evidence probe.
                result["pre_recovery_probe"] = {
                    "status": "unavailable", "error": str(error), "measurement": error.receipt}
                result["inconclusive"] = (
                    "strict pre-recovery snapshot unavailable; committed-prefix and pending-reader "
                    "proof are missing even if application recovery succeeds")
                result["pending_reader_checks"] = {"status": "not_exercised",
                    "reason": "strict snapshot unavailable; pending state was not inferred"}
            else:
                # Invariant/privacy failures remain fatal. They are deliberately
                # outside the missing-snapshot handler above.
                partial = verify_snapshot(seed, interrupted, chosen, partial=True, require_repaired=False)
                result["pre_recovery_probe"] = {"status": "verified"}
                result["interrupted_state"] = partial
                committed = partial["deleted_conversations"]
                if committed == 0:
                    result["inconclusive"] = "SIGKILL preceded the first durable batch; adjust delay and repeat"
                else:
                    require(interrupted["pending"]["effective"],
                            "committed interrupted deletion lost its pending repair fence")
                if interrupted["pending"]["effective"]:
                    result["pending_reader_checks"] = search_checks(
                        commands, marker, chosen, manifest, pending=True)
                else:
                    result["pending_reader_checks"] = {"status": "not_exercised",
                        "reason": "signal arrived before any pending repair was created"}
                partial_commit_proved = 0 < committed < len(chosen)
            result["partial_delete_commit_proved"] = partial_commit_proved
            if measurement["interruption"].get("last_observed_phase") != interrupt:
                result.setdefault("inconclusive", "signal arrived after the requested phase; repeat this case")
            # Same selectors resume partial deletion. The index route is tested
            # only after the entire deletion is independently proven committed.
            # An unavailable strict snapshot always takes same-selector forget.
            if interrupt == "lexical-index" and committed == len(chosen):
                recovery_argv = ["index", "--full", "--rebuild-profile", "bounded",
                                 "--json", "--no-progress-events"]
                route = "index-full"
            else:
                recovery_argv, route = argv, "same-selector-forget"
            recovery = commands.cass(recovery_argv, "recovery", measured=True)
            result["recovery"] = {"route": route, "measurement": recovery}
            require(successful(recovery), "recovery command did not complete")
            read_json(Path(recovery["stdout_path"]))
            if interrupt == "canonical-delete" and not partial_commit_proved:
                result.setdefault("inconclusive", "SIGKILL missed the partial-batch window; adjust delay and repeat")
        else:
            require(successful(measurement), "forget failed; RSS/time retained without a correctness pass")
            applied = read_json(Path(measurement["stdout_path"]))
            require(applied.get("applied") is True
                    and applied.get("conversations_deleted") == len(chosen)
                    and applied.get("messages_matched") == expected_messages,
                    "forget report does not match independent cohort")
            require(applied.get("derived_rebuilt") is True
                    and applied.get("rebuild_profile", {}).get("name") == "bounded",
                    "repair did not complete with the bounded lexical profile")
            require(set(PHASES) <= set(phase_names(measurement)), "repair-phase telemetry missing")
            result["application_report"] = applied
        after = commands.probe("after-repair")
        result["canonical_checks"] = verify_snapshot(seed, after, chosen)
        result["search_checks"] = search_checks(commands, marker, chosen, manifest)
        # Fresh-process retry proves tombstone durability and idempotence.
        no_op, _ = commands.json(argv, "same-selector-noop")
        require(no_op.get("conversations_deleted") == 0 and no_op.get("derived_rebuilt") is False,
                "same-selector completed retry is not idempotent")
        reopened = commands.probe("reopened-after-noop")
        verify_snapshot(seed, reopened, chosen)
        require(reopened["tombstones"] == after["tombstones"], "no-op changed tombstones")
        # Original sources are intentionally still present. Real connector
        # rediscovery must honor the durable exclusions.
        _, rescan = commands.json(["index", "--full", "--rebuild-profile", "bounded",
                                    "--json", "--no-progress-events"], "full-rescan")
        result["rescan_measurement"] = rescan
        rescanned = commands.probe("after-full-rescan")
        # Routine indexing can defer analytics: canonical preservation and
        # erasure are asserted separately from the completed repair totals.
        result["rescan_canonical_checks"] = verify_snapshot(
            seed, rescanned, chosen, require_repaired=False)
        require(not rescanned["pending"]["effective"], "full rescan left a pending repair")
        require(rescanned["tombstones"] == after["tombstones"], "full rescan changed exclusions")
        result["rescan_search_checks"] = search_checks(commands, marker, chosen, manifest)
        result["status"] = "inconclusive" if "inconclusive" in result else "passed"
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
        result.update(status="failed", error=f"{type(error).__name__}: {error}")
    finally:
        # Park only this owned copy, including failures and WAL/journal evidence.
        if live.exists():
            live.rename(trial_root / "archive")
        save(trial_root / "trial.json", result)
    return result


def scaling_summary(profiles: list[dict[str, Any]]) -> dict[str, Any]:
    groups: dict[tuple[str, int | None], list[dict[str, Any]]] = {}
    failures = []
    for profile in profiles:
        for trial in profile.get("trials", []):
            if trial.get("interruption_phase") is not None:
                continue
            if trial.get("status") != "passed":
                failures.append({"profile": profile["profile"], "name": trial["name"],
                                 "status": trial["status"], "measurement": trial.get("measurement")})
                continue
            measured = trial["measurement"]
            parent_rss = measured.get("parent_rss_bytes_at_launch")
            # wait4 can retain inherited pre-exec RSS. This conservative 10%
            # guard is an interpretation filter, not a corrected measurement.
            # Keep every raw peak; never subtract the controller's footprint.
            startup_floor = ("unknown" if not parent_rss else
                             "possible" if measured["peak_rss_kib"] * 1024 <= parent_rss * 1.10
                             else "clear")
            groups.setdefault((trial["case"], trial["memory_max"]), []).append({
                "profile": profile["profile"], "messages": profile["seed"]["canonical"]["messages"],
                "database_bytes": profile["database_bytes"], "rss_kib": measured["peak_rss_kib"],
                "wall_ms": measured["process_wall_ms"],
                "parent_rss_bytes_at_launch": parent_rss,
                "parent_hwm_bytes_at_launch": measured.get("parent_hwm_bytes_at_launch"),
                "sampled_root_hwm_peak_bytes": measured.get("sampled_root_hwm_peak_bytes"),
                "rss_startup_floor": startup_floor,
                "cgroup_peak_bytes": measured.get("memory", {}).get("peak_bytes")})
    comparisons = []
    for (case, cap), rows in groups.items():
        by_profile: dict[str, list[dict[str, Any]]] = {}
        for row in rows:
            by_profile.setdefault(row["profile"], []).append(row)
        points = []
        for name, samples in by_profile.items():
            point = {"profile": name, "messages": samples[0]["messages"],
                     "database_bytes": samples[0]["database_bytes"], "samples": len(samples),
                     "rss_startup_floor_possible_samples": sum(
                         row["rss_startup_floor"] == "possible" for row in samples),
                     "rss_startup_floor_unknown_samples": sum(
                         row["rss_startup_floor"] == "unknown" for row in samples)}
            for metric in ("rss_kib", "wall_ms", "cgroup_peak_bytes", "parent_rss_bytes_at_launch",
                           "parent_hwm_bytes_at_launch", "sampled_root_hwm_peak_bytes"):
                values = [row[metric] for row in samples if row[metric] is not None]
                point[metric] = statistics.median(values) if values else None
            points.append(point)
        points.sort(key=lambda row: row["messages"])
        comparison: dict[str, Any] = {"case": case, "memory_max": cap, "points": points,
                                     "status": "measured" if len(points) > 1 else "insufficient_sizes"}
        if len(points) > 1:
            if any(point["rss_startup_floor_possible_samples"]
                   or point["rss_startup_floor_unknown_samples"] for point in points):
                comparison["status"] = "rss_startup_floor_uncertain"
            a, b = points[0], points[-1]
            comparison["largest_over_smallest"] = {
                key: b[key] / a[key] for key in
                ("messages", "database_bytes", "rss_kib", "wall_ms", "cgroup_peak_bytes")
                if a[key] and b[key] is not None}
            if (comparison["status"] == "measured" and a["rss_kib"] and b["rss_kib"]
                    and b["messages"] > a["messages"]):
                comparison["empirical_rss_log_slope"] = (
                    math.log(b["rss_kib"] / a["rss_kib"]) / math.log(b["messages"] / a["messages"]))
        comparisons.append(comparison)
    return {"comparisons": comparisons, "failed_or_inconclusive_trials": failures,
            "interpretation": "Observed fixed-deletion-workload scaling only; no asymptotic or hard RSS guarantee.",
            "rss_startup_floor_rule": "A peak at most 1.10 times parent launch RSS, or missing launch RSS, makes RSS scaling uncertain; raw peaks are retained without subtraction."}


def native_identity(args: argparse.Namespace) -> dict[str, Any]:
    require(sys.platform.startswith("linux"), "native qualification requires Linux")
    require(isinstance(args.source_revision, str) and len(args.source_revision) == 40
            and all(c in "0123456789abcdef" for c in args.source_revision),
            "native runs require --source-revision with the actual full build commit SHA")
    for name in ("cass", "probe"):
        binary = getattr(args, name)
        require(binary is not None, f"--{name} is required for a native run")
        binary = binary.absolute()
        no_links(binary)
        require(binary.is_file() and os.access(binary, os.X_OK), f"not an executable: {binary}")
        setattr(args, name, binary)
    require(args.build_description, "--build-description is required for native runs")
    identity = {"source_revision": args.source_revision, "cass_sha256": sha256(args.cass),
                "probe_sha256": sha256(args.probe), "build_description": args.build_description,
                "source_binding": "operator_asserted"}
    if args.build_receipt:
        receipt = read_json(args.build_receipt)
        for field in ("source_revision", "cass_sha256", "probe_sha256"):
            require(receipt.get(field) == identity[field], f"build receipt mismatch: {field}")
        identity.update(build_receipt_sha256=sha256(args.build_receipt),
                        source_binding="supplied_build_receipt_matches_hashes")
    else:
        require(args.profile == ["smoke"], "representative runs require --build-receipt")
    return identity


def run(args: argparse.Namespace) -> dict[str, Any]:
    identity = None if args.plan_only else native_identity(args)
    root = new_root(args.output_dir)
    report: dict[str, Any] = {
        "schema_version": 1, "purpose": "gh565-large-archive-qualification",
        "inspected_application_baseline": INSPECTED_APPLICATION_BASELINE,
        "status": "running", "platform": platform.platform(), "python": sys.version,
        "identity": identity, "profiles": [],
        "measurement_contract": {
            "rss": "Linux wait4 ru_maxrss KiB for the CLI process lifetime, including native threads and possible inherited pre-exec RSS; startup-floor evidence recorded separately",
            "cgroup": "Fresh cgroup v2 child; swap disabled; peak includes charged cache and descendants",
            "phase_times": "stderr-line arrival boundaries; sampled RSS within boundaries is a lower bound",
            "cache": "warm/uncontrolled OS cache after copying and baseline verification; no cache drops",
            "excluded_from_forget_timing": ["source generation", "index/analytics setup", "copy",
                                          "copied-seed admission",
                                          "read-only probes", "search validation", "rescan"],
            "source_distribution": "Synthetic heavy-tail assumption; reporter's exact histogram is unknown",
            "scope": "Current-binary synthetic qualification; no v0.11.0 speedup claim"},
    }
    save(root / "invocation.json", {key: str(value) if isinstance(value, Path) else value
                                   for key, value in vars(args).items()})
    try:
        # Exercise the actual delegated controller before writing multi-GB
        # sources. This is a launcher preflight, never a CASS measurement.
        if not args.plan_only and args.memory_max:
            report["cgroup_preflights"] = []
            for cap in sorted(set(args.memory_max + ([args.verification_memory_max]
                                                    if args.verification_memory_max else []))):
                checked = process.run(
                    [sys.executable, "-I", "-S", "-c", "print('gh565 cgroup launcher ready')"],
                    label=f"cap-{cap}", env={"PATH": os.defpath}, cwd=root,
                    log_dir=root / "preflight", timeout=10, memory_max=cap,
                    cgroup_parent=args.cgroup_parent)
                report["cgroup_preflights"].append(checked)
                require(successful(checked), "cgroup launcher preflight failed before source generation")
        for profile_name in args.profile:
            values = dict(PROFILES[profile_name])
            values["seed"] = args.seed
            values["oversized_message_bytes"] = args.oversized_message_bytes
            if profile_name in args.content_overrides:
                values["content_bytes"] = args.content_overrides[profile_name]
            plan = fixture.make_plan(**values)
            profile_root = root / profile_name
            profile_root.mkdir()
            save(profile_root / "plan.json", plan)
            profile: dict[str, Any] = {"profile": profile_name, "status": "planned", "trials": []}
            report["profiles"].append(profile)
            if args.plan_only:
                profile["plan_sha256"] = plan["plan_sha256"]
                continue
            try:
                needed = values["content_bytes"] * 6
                require(shutil.disk_usage(profile_root).free >= needed,
                        f"insufficient free disk: need initial estimate {needed} bytes")
                home = profile_root / "fixture-home"
                home.mkdir()
                manifest = fixture.generate_sources(home, plan)
                save(profile_root / "fixture.json", manifest)
                data = profile_root / "live-data"
                data.mkdir()
                save(data / ".gh565-synthetic-fixture.json", SENTINEL)
                setup_cap = max(args.memory_max) if args.memory_max else None
                commands = Commands(profile_root / "setup", home, data, args, setup_cap)
                version = commands.cass(["--version"], "cass-version")
                require(successful(version), "CASS version probe failed")
                profile["cass_version_output"] = Path(version["stdout_path"]).read_text().strip()
                # Routine indexing otherwise defers both analytics and the token
                # ledger. Populate the seed through the existing public opt-in;
                # measured forget commands do not inherit this override.
                commands.env["CASS_INLINE_ANALYTICS_UPDATES"] = "1"
                profile["setup_environment_overrides"] = {"CASS_INLINE_ANALYTICS_UPDATES": "1"}
                index, _ = commands.json(
                    ["index", "--full", "--semantic", "--embedder", "hash", "--rebuild-profile", "bounded",
                     "--json", "--no-progress-events"], "seed-index")
                del commands.env["CASS_INLINE_ANALYTICS_UPDATES"]
                analytics, _ = commands.json(
                    ["analytics", "rebuild", "--track", "a", "--json"], "populate-analytics-a")
                # Track B rebuild exercises actual token ledger and rollup contracts.
                tokens, _ = commands.json(
                    ["analytics", "rebuild", "--track", "b", "--json"], "populate-analytics-b")
                seed = commands.probe("seed-probe")
                validate_seed(seed, manifest)
                profile["seed"] = seed
                database_bytes = (data / "agent_search.db").stat().st_size
                profile["database_bytes"] = database_bytes
                profile["archive_bytes"] = tree_bytes(data)
                profile["shape"] = {"target_database_bytes": args.database_target_bytes,
                                    "database_size_within_tolerance": None}
                if profile_name == "representative":
                    relative_error = abs(database_bytes / args.database_target_bytes - 1.0)
                    profile["shape"].update(database_size_relative_error=relative_error,
                        database_size_within_tolerance=relative_error <= args.database_size_tolerance)
                data.rename(profile_root / "seed-data")
                for cap in args.memory_max or [None]:
                    for repetition in range(args.repetitions):
                        for case in args.case:
                            name = f"{case}-cap-{cap}-repeat-{repetition + 1}"
                            trial = run_trial(profile_root, home, manifest, seed, args, cap, case, name, None)
                            profile["trials"].append(trial)
                            checkpoint(root, report, sum(len(p["trials"]) for p in report["profiles"]))
                recovery_cap = max(args.memory_max) if args.memory_max else None
                for phase in args.recovery_phase:
                    trial = run_trial(profile_root, home, manifest, seed, args, recovery_cap, "batch",
                                      f"recovery-{phase}", phase)
                    profile["trials"].append(trial)
                    checkpoint(root, report, sum(len(p["trials"]) for p in report["profiles"]))
                for source in manifest["conversations"]:
                    require(sha256(Path(source["path"])) == source["source_sha256"],
                            "source transcript changed during qualification")
                profile["status"] = (
                    "passed" if all(t["status"] == "passed" for t in profile["trials"])
                    and profile["shape"]["database_size_within_tolerance"] is not False else "incomplete")
            except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
                profile.update(status="failed", error=f"{type(error).__name__}: {error}")
        if args.plan_only:
            report.update(status="plan_only", application_performance="unmeasured")
        else:
            require(sha256(args.cass) == identity["cass_sha256"]
                    and sha256(args.probe) == identity["probe_sha256"],
                    "binary bytes changed during the run")
            report["scaling"] = scaling_summary(report["profiles"])
            complete = (
                "small" in args.profile and "representative" in args.profile
                and set(args.case) == {"single", "batch"} and bool(args.memory_max)
                and set(args.recovery_phase) == {"canonical-delete", "analytics", "lexical-index"}
                and bool(report["scaling"]["comparisons"])
                and all(c["status"] == "measured" for c in report["scaling"]["comparisons"])
                and all(p["status"] == "passed" for p in report["profiles"]))
            if complete:
                report["status"] = "qualified_synthetic_profiles"
            elif args.profile == ["smoke"] and all(p["status"] == "passed" for p in report["profiles"]):
                report["status"] = "smoke_passed"
            else:
                report["status"] = "incomplete"
            report["application_performance"] = "measured_only_for_trials_with_retained_receipts"
        save(root / "report.json", report)
        return report
    except BaseException as error:
        report.update(status="failed", error=f"{type(error).__name__}: {error}")
        save(root / "failure.json", report)
        raise


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--cass", type=Path)
    parser.add_argument("--probe", type=Path)
    parser.add_argument("--source-revision",
                        help="actual full build commit SHA; required for native runs")
    parser.add_argument("--build-description")
    parser.add_argument("--build-receipt", type=Path)
    parser.add_argument("--profile", action="append", choices=tuple(PROFILES))
    parser.add_argument("--case", action="append", choices=("single", "batch"))
    parser.add_argument("--memory-max", action="append", type=process.parse_bytes)
    parser.add_argument("--verification-memory-max", type=process.parse_bytes,
                        help="setup/probe/search cap; defaults to the largest trial cap")
    parser.add_argument("--cgroup-parent", type=Path)
    parser.add_argument("--allow-uncapped", action="store_true", help="smoke only; never a cap qualification")
    parser.add_argument("--plan-only", action="store_true")
    parser.add_argument("--content-bytes", action="append", default=[], metavar="PROFILE=BYTES")
    parser.add_argument("--database-target-bytes", type=process.parse_bytes, default=11_300_000_000)
    parser.add_argument("--database-size-tolerance", type=float, default=0.20)
    parser.add_argument("--oversized-message-bytes", type=process.parse_bytes, default=0)
    parser.add_argument("--fts-shadow-max-messages", type=int)
    parser.add_argument("--repetitions", type=int, default=2)
    parser.add_argument("--recovery-phase", action="append",
                        choices=("canonical-delete", "analytics", "lexical-index"))
    parser.add_argument("--canonical-interrupt-delay", type=float, default=1.0)
    parser.add_argument("--sample-interval", type=float, default=0.1)
    parser.add_argument("--timeout", type=float, default=7200)
    parser.add_argument("--quick-check", action="store_true")
    parser.add_argument("--seed", type=int, default=565)
    args = parser.parse_args(argv)
    args.profile = args.profile or ["small", "representative"]
    require("smoke" not in args.profile or args.profile == ["smoke"],
            "smoke cannot be combined with archive-scale profiles")
    args.case = args.case or ["single", "batch"]
    # Eight small smoke deletions fit one atomic batch. A partial-batch recovery
    # claim is impossible there; exercise both post-commit repair interruptions.
    args.recovery_phase = args.recovery_phase or (
        ["analytics", "lexical-index"] if args.profile == ["smoke"]
        else ["canonical-delete", "analytics", "lexical-index"])
    require(len(set(args.profile)) == len(args.profile), "duplicate profiles refused")
    require(len(set(args.case)) == len(args.case), "duplicate cases refused")
    require(len(set(args.recovery_phase)) == len(args.recovery_phase), "duplicate recovery phases refused")
    require(not args.memory_max or len(set(args.memory_max)) == len(args.memory_max),
            "duplicate memory limits refused")
    require(args.repetitions > 0 and math.isfinite(args.timeout) and args.timeout > 0,
            "positive repetitions and finite timeout required")
    require(math.isfinite(args.sample_interval) and 0.01 <= args.sample_interval <= 5,
            "sample interval must be 0.01..5 seconds")
    require(math.isfinite(args.canonical_interrupt_delay) and args.canonical_interrupt_delay >= 0,
            "invalid canonical interruption delay")
    require(math.isfinite(args.database_size_tolerance)
            and 0 < args.database_size_tolerance < 1, "invalid database size tolerance")
    require(args.database_target_bytes > 0, "database target must be positive")
    require(args.plan_only or args.source_revision is not None,
            "native runs require --source-revision with the actual build commit SHA")
    if args.source_revision is not None:
        require(len(args.source_revision) == 40
                and all(c in "0123456789abcdef" for c in args.source_revision),
                "source revision must be a full lowercase Git SHA")
    require(args.fts_shadow_max_messages is None or args.fts_shadow_max_messages >= 0,
            "FTS shadow bound must be nonnegative")
    if not args.plan_only:
        if args.allow_uncapped:
            require(args.profile == ["smoke"] and not args.memory_max and not args.cgroup_parent
                    and not args.verification_memory_max,
                    "uncapped execution is supported only for explicit smoke profile")
        else:
            require(args.memory_max and args.cgroup_parent,
                    "native qualification requires --memory-max and delegated --cgroup-parent")
    args.content_overrides = {}
    for value in args.content_bytes:
        name, separator, raw = value.partition("=")
        require(separator == "=" and name in args.profile and name not in args.content_overrides,
                "--content-bytes must be a unique selected PROFILE=BYTES")
        args.content_overrides[name] = process.parse_bytes(raw)
    return args


def main(argv: list[str] | None = None) -> int:
    try:
        args = parse_args(argv)
        report = run(args)
        print(json.dumps({"status": report["status"],
                          "report": str(args.output_dir.absolute() / "report.json")}, indent=2))
        return 0 if report["status"] in ("plan_only", "smoke_passed", "qualified_synthetic_profiles") else 1
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
        print(f"qualification failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
