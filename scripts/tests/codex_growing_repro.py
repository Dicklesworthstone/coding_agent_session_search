#!/usr/bin/env python3
"""GH #559's 20 MB + 0.5 MB still/reopen/held reproduction, with assertions.

Run against a built CASS binary in an authorized DSR execution environment:
    python3 scripts/tests/codex_growing_repro.py --cass target/release/cass

Synthetic inputs only. Every arm has isolated HOME, CODEX_HOME and data. All
inputs, stdout/stderr, search responses and a summary are retained for review.
The two live arms append every 20 ms; this is the reporter's free-running
matrix, complementing the deterministic Rust connector regressions.

The mid-scan search marker is appended AFTER the first index and BEFORE the
second. Finding an old first-run marker would not prove live ingestion. The
last append is checked after the writer stops to detect lost future discovery.
Default timing matches the reporter, including fresh closed files. Optional
--age-settled-controls is a separate qualification adaptation: it ages synthetic
initial/still/final mtimes past the existing inactivity window, requires exit 0
for those controls, and never alters source bytes or the live writer arms.
"""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import random
import subprocess
import tempfile
import threading
import time


def record(kind: str, payload: dict, day: int = 1) -> str:
    return json.dumps({
        "timestamp": f"2026-07-{day:02d}T12:00:00.000Z",
        "type": kind,
        "payload": payload,
    }, separators=(",", ":")) + "\n"


def message(text: str) -> str:
    return record("response_item", {
        "type": "message", "role": "assistant",
        "content": [{"type": "output_text", "text": text}],
    })


def mark_settled(path: Path) -> None:
    """Use the real inactivity policy without waiting minutes in a regression."""
    metadata = path.stat()
    os.utime(path, ns=(metadata.st_atime_ns, time.time_ns() - 7200 * 1_000_000_000))


def generate(home: Path, day: int, megabytes: float) -> Path:
    rng = random.Random(day)
    words = [f"w{index}" for index in range(20_000)]
    directory = home / "sessions" / "2026" / "07" / f"{day:02d}"
    directory.mkdir(parents=True)
    sid = f"01990000-{day:04x}-7000-8000-{rng.getrandbits(48):012x}"
    path = directory / f"rollout-2026-07-{day:02d}T12-00-00-{sid}.jsonl"
    with path.open("w", encoding="utf-8") as source:
        source.write(record("session_meta", {
            "id": sid, "cwd": "/synthetic/project", "cli_version": "0.149.0",
            "originator": "codex_cli_rs",
        }, day))
        source.write(record("response_item", {
            "type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "Synthetic growing rollout reproduction"}],
        }, day))
        size, index = source.tell(), 0
        while size < megabytes * 1_000_000:
            index += 1
            def text(count: int) -> str:
                return " ".join(rng.choice(words) for _ in range(count))
            rows = [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": text(50)}]},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text(100)}]},
                {"type": "custom_tool_call", "call_id": f"c{index}", "name": "shell", "input": text(20)},
                {"type": "custom_tool_call_output", "call_id": f"c{index}", "output": text(rng.choice([200, 800, 3000]))},
            ]
            for payload in rows:
                rendered = record("response_item", payload, day)
                source.write(rendered)
                size += len(rendered.encode("utf-8"))
    return path


class Writer:
    def __init__(self, path: Path, mode: str, arm: str):
        self.path, self.mode, self.arm = path, mode, arm
        self.stop_event = threading.Event()
        self.started = threading.Event()
        self.count = 0
        self.error: BaseException | None = None
        self.thread = threading.Thread(target=self.run, daemon=True)

    def marker(self, index: int) -> str:
        return f"liveappend{self.arm}{index:08d}"

    def run(self) -> None:
        held = None
        try:
            if self.mode == "held":
                held = self.path.open("a", encoding="utf-8")
            while not self.stop_event.is_set():
                rendered = message(self.marker(self.count) + " " + "z" * 2000)
                if held is not None:
                    held.write(rendered)
                    held.flush()
                else:
                    with self.path.open("a", encoding="utf-8") as source:
                        source.write(rendered)
                self.count += 1
                self.started.set()
                self.stop_event.wait(0.02)
        except BaseException as error:
            self.error = error
            self.started.set()
        finally:
            if held is not None:
                held.close()

    def start(self) -> None:
        self.thread.start()
        if not self.started.wait(10):
            raise RuntimeError("synthetic writer did not start")
        self.check()

    def check(self) -> None:
        if self.error is not None:
            raise RuntimeError("synthetic writer failed") from self.error

    def stop(self) -> None:
        self.stop_event.set()
        self.thread.join(10)
        if self.thread.is_alive():
            raise RuntimeError("synthetic writer did not stop")
        self.check()


def run(cass: Path, arm: Path, label: str, arguments: list[str]) -> tuple[int, dict]:
    environment = {
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
        "HOME": str(arm / "home"), "CODEX_HOME": str(arm / "codex"),
        "CASS_DATA_DIR": str(arm / "data"), "CASS_IGNORE_SOURCES_CONFIG": "1",
        "CASS_EXCLUDE_PATHS": "", "CASS_AUTO_REFRESH": "0", "NO_COLOR": "1",
    }
    command = [str(cass), *arguments, "--json"]
    completed = subprocess.run(command, cwd=arm, env=environment, capture_output=True, text=True, timeout=180)
    (arm / f"{label}.stdout").write_text(completed.stdout, encoding="utf-8")
    (arm / f"{label}.stderr").write_text(completed.stderr, encoding="utf-8")
    (arm / f"{label}.command.json").write_text(json.dumps({"argv": command, "exit": completed.returncode}) + "\n", encoding="utf-8")
    # Index progress can precede its final JSON object; preserve all raw output.
    decoder = json.JSONDecoder()
    remaining, values = completed.stdout.lstrip(), []
    while remaining:
        value, end = decoder.raw_decode(remaining)
        values.append(value)
        remaining = remaining[end:].lstrip()
    if not values or not isinstance(values[-1], dict):
        raise AssertionError(f"{label}: no final JSON object; see retained stdout/stderr")
    return completed.returncode, values[-1]


def require_hit(cass: Path, arm: Path, label: str, marker: str) -> int:
    code, result = run(cass, arm, label, ["search", marker, "--mode", "lexical", "--limit", "100"])
    hits = result.get("hits") or []
    if code != 0 or not hits:
        raise AssertionError(f"{label}: newly written marker was not searchable (exit {code})")
    return len(hits)


def experiment(cass: Path, root: Path, name: str, large_mb: float, small_mb: float,
               age_settled_controls: bool) -> dict:
    arm = root / name
    (arm / "home").mkdir(parents=True)
    source = generate(arm / "codex", 1, large_mb)
    sibling = generate(arm / "codex", 2, small_mb)
    if age_settled_controls:
        mark_settled(source)
        mark_settled(sibling)
    first_code, first = run(cass, arm, "first", ["index"])
    settled_codes = (0,) if age_settled_controls else (0, 9)
    if first_code not in settled_codes:
        raise AssertionError(f"{name}: initial index failed with exit {first_code}")
    admitted_marker = f"admittedprefix{name}"
    with source.open("a", encoding="utf-8") as target:
        target.write(message(admitted_marker))
    writer = Writer(source, "held" if name == "held" else "reopen", name)
    writer.start()
    try:
        if name == "still":
            time.sleep(4)
            writer.stop()
            time.sleep(2)
            if age_settled_controls:
                mark_settled(source)
        else:
            time.sleep(1)
        before = writer.count
        second_code, second = run(cass, arm, "second", ["index"])
        allowed_second = settled_codes if name == "still" else (0, 9)
        if second_code not in allowed_second:
            raise AssertionError(f"{name}: unexpected live-index exit {second_code}")
        if name != "still" and writer.count <= before:
            raise AssertionError(f"{name}: writer did not append during ingestion")
        mid_hits = require_hit(cass, arm, "search-while-writing", admitted_marker)
        writer.check()
    finally:
        writer.stop()
    if age_settled_controls:
        mark_settled(source)
    third_code, third = run(cass, arm, "third", ["index"])
    if third_code not in settled_codes:
        raise AssertionError(f"{name}: retry after writer stopped failed with exit {third_code}")
    final_hits = require_hit(cass, arm, "search-final-append", writer.marker(writer.count - 1))
    return {
        "arm": name, "source": str(source), "appended_records": writer.count,
        "exit_codes": [first_code, second_code, third_code],
        "new_admitted_marker_hits_while_writing": mid_hits,
        "last_append_hits_after_retry": final_hits,
        "runs": [first, second, third],
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cass", required=True, type=Path)
    parser.add_argument("--output", type=Path, help="new directory for retained evidence")
    parser.add_argument("--large-mb", type=float, default=20)
    parser.add_argument("--small-mb", type=float, default=0.5)
    parser.add_argument("--age-settled-controls", action="store_true",
                        help="qualification adaptation: age initial/still/final mtimes 2 hours; require exit 0 for settled controls")
    args = parser.parse_args()
    cass = args.cass.resolve(strict=True)
    root = args.output.resolve() if args.output else Path(tempfile.mkdtemp(prefix="cass-gh559-matrix-"))
    root.mkdir(parents=True, exist_ok=True)
    version = subprocess.check_output([str(cass), "--version"], text=True, timeout=15).strip()
    print(f"{version}\nevidence: {root}", flush=True)
    qualification = {
        "timing": "reporter timing with aged settled controls" if args.age_settled_controls else "reporter timing",
        "age_settled_controls": args.age_settled_controls,
        "settled_mtime_age_seconds": 7200 if args.age_settled_controls else None,
        "settled_exit_codes_allowed": [0] if args.age_settled_controls else [0, 9],
        "live_sources_aged_while_writing": False,
        "note": "Exit 9 is permitted for a still-recent source; newly admitted and final-append search assertions remain mandatory.",
    }
    print(json.dumps({"qualification": qualification}), flush=True)
    summary = {"binary": str(cass), "version": version, "qualification": qualification, "arms": []}
    for name in ("still", "reopen", "held"):
        observed = experiment(cass, root, name, args.large_mb, args.small_mb, args.age_settled_controls)
        summary["arms"].append(observed)
        (root / "summary.json").write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")
        print(json.dumps({key: value for key, value in observed.items() if key != "runs"}), flush=True)


if __name__ == "__main__":
    main()
