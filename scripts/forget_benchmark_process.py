#!/usr/bin/env python3
"""Real Linux child-process measurements for the isolated forget benchmark.

No child result is called a qualification pass here. Output, observations, and a
terminal receipt are retained, including failed commands. A requested memory cap
requires an already delegated cgroup v2 parent. Parent settings are never changed.
Empty owned cgroups remain available for inspecting their kernel counters.

wait4 RSS is per invocation, not cumulative benchmark-child accounting. Its
process-lifetime accounting can retain the controller's inherited pre-exec RSS
as a startup floor. The parent RSS at launch and child-PID /proc HWM samples are
recorded separately; no baseline is subtracted from a kernel peak. Cgroup
memory.peak includes descendants and charged file cache. Phase times and sampled
RSS are stderr-arrival observations, not exact internal phase instrumentation.
"""
from __future__ import annotations

import datetime as dt
import hashlib
import json
import math
import os
from pathlib import Path
import re
import selectors
import signal
import subprocess
import sys
import time
from typing import Any, Mapping, Sequence
import uuid

PHASE = re.compile(rb"^forget:\s+(?:phase=)?([a-z][a-z0-9-]*)\s*$")
_ENTER = ("import os,sys; fd=os.open(sys.argv[1],os.O_WRONLY); "
          "os.write(fd,str(os.getpid()).encode()); os.close(fd); "
          "os.execvpe(sys.argv[2],sys.argv[2:],os.environ)")


class ProcessSetupError(RuntimeError):
    def __init__(self, message: str, receipt: dict[str, Any]):
        super().__init__(message)
        self.receipt = receipt


def parse_bytes(value: str) -> int:
    """Parse bytes, decimal KB/MB/GB/TB, or binary K/M/G/T and KiB/MiB/GiB/TiB."""
    match = re.fullmatch(r"([0-9]+)\s*(B|[KMGT](?:iB|B)?)?", value.strip(), re.I)
    if match is None:
        raise ValueError(f"invalid memory limit: {value!r}")
    amount, suffix = int(match[1]), (match[2] or "B").upper()
    exponent = "KMGT".index(suffix[0]) + 1 if suffix[0] in "KMGT" else 0
    base = 1000 if len(suffix) == 2 and suffix.endswith("B") else 1024
    result = amount * base ** exponent
    if result <= 0:
        raise ValueError("memory limit must be positive")
    return result


def _digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def _counters(path: Path) -> dict[str, int]:
    return {key: int(value) for key, value in
            (line.split() for line in path.read_text(encoding="ascii").splitlines())}


def _is_cgroup2(path: Path) -> bool:
    for line in Path("/proc/self/mountinfo").read_text().splitlines():
        left, sep, right = line.partition(" - ")
        if sep and right.split()[0] == "cgroup2":
            raw = left.split()[4]
            mount = Path(re.sub(r"\\([0-7]{3})", lambda m: chr(int(m[1], 8)), raw)).resolve()
            if path == mount or mount in path.parents:
                return True
    return False


class _Cgroup:
    def __init__(self, parent: Path, maximum: int):
        parent = parent.resolve(strict=True)
        if not parent.is_dir() or not _is_cgroup2(parent):
            raise ValueError(f"not a delegated cgroup v2 directory: {parent}")
        if "memory" not in (parent / "cgroup.subtree_control").read_text().split():
            raise ValueError(f"memory controller is not delegated below {parent}")
        self.path = parent / f"cass-forget-bench-{os.getpid()}-{uuid.uuid4().hex[:12]}"
        self.path.mkdir()
        for name, value in (("memory.swap.max", 0), ("memory.max", maximum), ("memory.oom.group", 1)):
            target = self.path / name
            with target.open("w", encoding="ascii") as stream:
                stream.write(str(value))
            if target.read_text().strip() != str(value):
                raise ValueError(f"kernel rejected {target}={value}")
        for name in ("cgroup.procs", "cgroup.kill", "memory.current", "memory.peak", "memory.events"):
            if not (self.path / name).is_file():
                raise ValueError(f"required cgroup counter/control unavailable: {self.path / name}")
        self.initial = self.snapshot()

    def snapshot(self) -> dict[str, Any]:
        return {"current_bytes": int((self.path / "memory.current").read_text()),
                "peak_bytes": int((self.path / "memory.peak").read_text()),
                "events": _counters(self.path / "memory.events")}

    def pids(self) -> list[int]:
        return [int(value) for value in (self.path / "cgroup.procs").read_text().split()]

    def kill(self) -> None:
        with (self.path / "cgroup.kill").open("w") as stream:
            stream.write("1")


def _rss(pid: int) -> tuple[int, int]:
    try:
        values = {}
        for line in Path(f"/proc/{pid}/status").read_text().splitlines():
            if line.startswith(("VmRSS:", "VmHWM:")):
                key, value, unit = line.split()
                if unit != "kB":
                    raise ValueError(f"unexpected RSS unit {unit}")
                values[key[:-1]] = int(value) * 1024
        return values.get("VmRSS", 0), values.get("VmHWM", 0)
    except (FileNotFoundError, ProcessLookupError):
        return 0, 0


def _group_pids(group: int) -> list[int]:
    pids = []
    with os.scandir("/proc") as entries:
        for entry in entries:
            if not entry.name.isdecimal():
                continue
            try:
                stat = Path(entry.path, "stat").read_text()
                fields = stat[stat.rfind(")") + 2:].split()
                if int(fields[2]) == group and fields[0] not in ("Z", "X"):
                    pids.append(int(entry.name))
            except (FileNotFoundError, ProcessLookupError, PermissionError):
                continue
    return pids


def run(argv: Sequence[str], *, label: str, env: Mapping[str, str], cwd: Path,
        log_dir: Path, timeout: float, memory_max: int | None = None,
        cgroup_parent: Path | None = None, interrupt_phase: str | None = None,
        interrupt_after_seconds: float | None = None, sample_interval: float = 0.1,
        allow_uncapped: bool = False, max_output_bytes: int = 64 * 1024 * 1024) -> dict[str, Any]:
    """Run and observe a command; the caller asserts the required outcome.

    Interruption delay is relative to first receipt of the selected phase, or
    launch if no phase was selected. Omit the delay for an immediate phase kill.
    Require interruption.sent AND exit_code == -9 before treating it as executed.
    Nonzero exits, timeout, requested kill, and OOM return receipts. Setup or
    observation errors raise ProcessSetupError after writing their receipt.
    """
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]*", label):
        raise ValueError("label must be a simple filename component")
    log_dir = Path(log_dir).resolve()
    log_dir.mkdir(parents=True, exist_ok=True)
    paths = {key: log_dir / f"{label}.{suffix}" for key, suffix in
             (("stdout", "stdout"), ("stderr", "stderr"), ("telemetry", "telemetry.jsonl"),
              ("receipt", "receipt.json"))}
    if any(path.exists() for path in paths.values()):
        raise ValueError(f"refusing to overwrite logs for {label}")
    receipt: dict[str, Any] = {
        "schema_version": 1, "label": label, "argv": list(argv), "cwd": str(Path(cwd).resolve()),
        "started_at_utc": dt.datetime.now(dt.timezone.utc).isoformat(), "status": "starting",
        "exit_code": None, "timed_out": False, "process_wall_ms": None, "peak_rss_kib": None,
        "sampled_root_peak_rss_bytes": 0, "sampled_process_tree_peak_rss_bytes": 0,
        "sampled_root_hwm_peak_bytes": 0,
        "parent_rss_bytes_at_launch": None, "parent_hwm_bytes_at_launch": None,
        "sample_interval_seconds": sample_interval, "phases": [],
        **{f"{key}_path": str(path) for key, path in paths.items()},
        "output_bytes": {"stdout": 0, "stderr": 0}, "output_limit_bytes": max_output_bytes,
        "output_limit_exceeded": False,
        "memory": {"limited": False, "requested_max_bytes": memory_max, "swap_max_bytes": None,
                   "cgroup_path": None, "peak_bytes": None, "events_delta": None},
        "interruption": {"requested_phase": interrupt_phase, "delay_seconds": interrupt_after_seconds,
                         "phase_observed": False, "sent": False},
        "measurement_notes": {
            "rss": "Linux wait4 rusage.ru_maxrss in KiB for this invocation; may include inherited pre-exec startup RSS; not a summed tree peak",
            "startup_floor": "Parent RSS/HWM at launch are recorded, never subtracted; child-PID /proc HWM samples cover the launcher then exec and may miss a short final peak",
            "cgroup": "memory.peak includes charged file cache and launch overhead; empty cgroup retained",
            "phases": "stderr-arrival intervals and sampled RSS lower bounds, not internal phase timers",
            "tree_rss": "sampled process RSS sum; shared mappings may be counted more than once",
            "uncapped_scope": "uncapped containment and RSS observations cover the child process group"}}
    process = group = usage = pidfd = start = exited = phase = due = stop_at = None
    stop_reason = None
    error = None
    pending = bytearray()
    dropping_line = False

    with paths["stdout"].open("xb") as out, paths["stderr"].open("xb") as err, \
            paths["telemetry"].open("x", encoding="utf-8") as telemetry:
        def event(kind: str, **fields: Any) -> None:
            telemetry.write(json.dumps({"event": kind, "elapsed_seconds": time.perf_counter() - start
                                        if start is not None else 0, **fields}, sort_keys=True) + "\n")
            telemetry.flush()

        def kill(reason: str) -> bool:
            nonlocal stop_reason, stop_at
            sent = False
            if process is not None:
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                    sent = True
                except ProcessLookupError:
                    pass
                if group is not None:
                    group.kill()
            stop_reason = stop_reason or reason
            stop_at = stop_at or time.perf_counter()
            event("signal", signal="SIGKILL", reason=reason, sent=sent)
            return sent

        def reap(block: bool = False) -> bool:
            nonlocal usage, exited
            if process is None or process.returncode is not None:
                return True
            pid, status, result = os.wait4(process.pid, 0 if block else os.WNOHANG)
            if pid:
                process.returncode = os.waitstatus_to_exitcode(status)
                usage, exited = result, time.perf_counter()
                event("process_exit", exit_code=process.returncode)
            return process.returncode is not None

        def parse(chunk: bytes) -> None:
            nonlocal phase, due, dropping_line
            for i, part in enumerate(chunk.split(b"\n")):
                if i:
                    match = None if dropping_line else PHASE.fullmatch(bytes(pending).rstrip(b"\r"))
                    if match is not None:
                        observed = time.perf_counter() - start
                        if phase is not None:
                            phase["observed_interval_ms"] = (observed - phase["observed_at_seconds"]) * 1000
                        name = match[1].decode("ascii")
                        phase = {"phase": name, "observed_at_seconds": observed,
                                 "observed_interval_ms": None, "samples": 0,
                                 "sampled_root_peak_rss_bytes": 0, "sampled_process_tree_peak_rss_bytes": 0,
                                 "sampled_cgroup_current_peak_bytes": None}
                        receipt["phases"].append(phase)
                        event("phase", phase=name)
                        if name == interrupt_phase and not receipt["interruption"]["phase_observed"]:
                            receipt["interruption"].update(phase_observed=True,
                                                           phase_observed_at_seconds=observed)
                            due = start + observed + (interrupt_after_seconds or 0)
                    pending.clear()
                    dropping_line = False
                if not dropping_line:
                    if len(pending) + len(part) <= 65536:
                        pending.extend(part)
                    else:
                        pending.clear()
                        dropping_line = True
                        event("phase_parser_long_line", maximum_bytes=65536)

        def sample() -> None:
            rss, hwm = _rss(process.pid)
            receipt["sampled_root_hwm_peak_bytes"] = max(receipt["sampled_root_hwm_peak_bytes"], hwm)
            pids = group.pids() if group is not None else _group_pids(process.pid)
            tree = sum(_rss(pid)[0] for pid in pids)
            memory = group.snapshot() if group is not None else None
            for target in (receipt, phase):
                if target is not None:
                    target["sampled_root_peak_rss_bytes"] = max(target["sampled_root_peak_rss_bytes"], rss)
                    target["sampled_process_tree_peak_rss_bytes"] = max(
                        target["sampled_process_tree_peak_rss_bytes"], tree)
            if phase is not None:
                phase["samples"] += 1
                if memory is not None:
                    phase["sampled_cgroup_current_peak_bytes"] = max(
                        phase["sampled_cgroup_current_peak_bytes"] or 0, memory["current_bytes"])
            event("sample", root_rss_bytes=rss, root_hwm_bytes=hwm, process_tree_rss_bytes=tree,
                  process_count=len(pids), phase=phase["phase"] if phase else None, cgroup=memory)

        try:
            if sys.platform != "linux" or not hasattr(os, "wait4"):
                raise ValueError("Linux with wait4 is required")
            if not argv or any(not isinstance(value, str) for value in argv):
                raise ValueError("argv must contain an executable and string arguments")
            if not all(math.isfinite(value) and value > 0 for value in (timeout, sample_interval)):
                raise ValueError("timeout and sample interval must be finite and positive")
            if max_output_bytes < 1:
                raise ValueError("output ceiling must be positive")
            if interrupt_after_seconds is not None and (
                    not math.isfinite(interrupt_after_seconds) or interrupt_after_seconds < 0):
                raise ValueError("interruption delay must be finite and nonnegative")
            if interrupt_phase is not None and not re.fullmatch(r"[a-z][a-z0-9-]*", interrupt_phase):
                raise ValueError("invalid requested phase name")
            if memory_max is None:
                if cgroup_parent is not None or not allow_uncapped:
                    raise ValueError("uncapped execution requires explicit allow_uncapped=True and no cgroup parent")
                command = list(argv)
            else:
                if isinstance(memory_max, bool) or not isinstance(memory_max, int) or memory_max <= 0:
                    raise ValueError("memory_max must be a positive integer byte count")
                if cgroup_parent is None:
                    raise ValueError("a requested cap requires an explicitly delegated cgroup_parent")
                group = _Cgroup(Path(cgroup_parent), memory_max)
                receipt["memory"].update(limited=True, swap_max_bytes=0, cgroup_path=str(group.path),
                                         initial=group.initial, enter_before_exec=True)
                command = [sys.executable, "-I", "-S", "-c", _ENTER,
                           str(group.path / "cgroup.procs"), *argv]
            receipt["parent_rss_bytes_at_launch"], receipt["parent_hwm_bytes_at_launch"] = _rss(os.getpid())
            start = time.perf_counter()
            process = subprocess.Popen(command, cwd=Path(cwd), env=dict(env), stdin=subprocess.DEVNULL,
                                       stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
            receipt.update(pid=process.pid, status="running")
            event("process_start", pid=process.pid)
            if interrupt_phase is None and interrupt_after_seconds is not None:
                due = start + interrupt_after_seconds
            next_sample = start
            with selectors.DefaultSelector() as selector:
                for pipe, name, log in ((process.stdout, "stdout", out), (process.stderr, "stderr", err)):
                    os.set_blocking(pipe.fileno(), False)
                    selector.register(pipe, selectors.EVENT_READ, (name, log))
                try:
                    pidfd = os.pidfd_open(process.pid)
                    selector.register(pidfd, selectors.EVENT_READ, ("pidfd", None))
                    receipt["exit_observer"] = "pidfd readiness plus wait4"
                except (AttributeError, OSError):
                    if pidfd is not None:
                        os.close(pidfd)
                        pidfd = None
                    receipt["exit_observer"] = "wait4 polling at most every 10 ms"
                while True:
                    now = time.perf_counter()
                    if now >= next_sample:
                        sample()
                        next_sample = now + sample_interval
                    done = reap()
                    if not done and stop_reason is None and due is not None and now >= due:
                        receipt["interruption"].update(sent=kill("requested-interruption"),
                            sent_at_seconds=time.perf_counter() - start,
                            last_observed_phase=phase["phase"] if phase else None)
                    if not done and stop_reason is None and now >= start + timeout:
                        receipt["timed_out"] = True
                        kill("timeout")
                    pipes = [key for key in selector.get_map().values() if key.data[0] != "pidfd"]
                    if done and not pipes:
                        remaining = group.pids() if group is not None else _group_pids(process.pid)
                        if remaining and stop_reason is None:
                            receipt["descendants_terminated_after_root_exit"] = True
                            kill("descendants-after-exit")
                        break
                    if done and now - exited > 0.25 and stop_reason is None:
                        receipt["descendants_terminated_after_root_exit"] = True
                        kill("descendants-after-exit")
                    if done and stop_at is not None and now - stop_at > 2:
                        receipt["output_pipes_remained_open_after_kill"] = True
                        break
                    waits = [max(0, next_sample - time.perf_counter())]
                    if stop_reason is None:
                        waits.append(max(0, start + timeout - time.perf_counter()))
                        if due is not None:
                            waits.append(max(0, due - time.perf_counter()))
                    if pidfd is None:
                        waits.append(0.01)
                    for key, _ in selector.select(min(waits)):
                        name, log = key.data
                        if name == "pidfd":
                            reap()
                            selector.unregister(key.fileobj)
                            continue
                        try:
                            chunk = os.read(key.fd, 65536)
                        except BlockingIOError:
                            continue
                        if not chunk:
                            selector.unregister(key.fileobj)
                            continue
                        offset = receipt["output_bytes"][name]
                        receipt["output_bytes"][name] += len(chunk)
                        if not receipt["output_limit_exceeded"]:
                            log.write(chunk)
                            event("output", stream=name, byte_offset=offset, byte_count=len(chunk))
                            if name == "stderr":
                                parse(chunk)
                        if sum(receipt["output_bytes"].values()) > max_output_bytes:
                            receipt["output_limit_exceeded"] = True
                            if stop_reason is None:
                                kill("output-limit")
                if pending and not dropping_line:
                    parse(b"\n")
                sample()
            receipt["status"] = stop_reason or "exited"
        except BaseException as caught:
            error = caught
            receipt.update(status="setup-error" if process is None else "observer-error",
                           error=f"{type(caught).__name__}: {caught}")
            if process is not None:
                try:
                    kill("observer-error")
                finally:
                    reap(block=True)
            event("error", error=receipt["error"])
        finally:
            if pidfd is not None:
                os.close(pidfd)
            if process is not None:
                for pipe in (process.stdout, process.stderr):
                    if pipe is not None:
                        pipe.close()
                receipt["exit_code"] = process.returncode
            if start is not None:
                ended = exited if exited is not None else time.perf_counter()
                receipt["process_wall_ms"] = (ended - start) * 1000
                if phase is not None:
                    phase["observed_interval_ms"] = max(0, (ended - start - phase["observed_at_seconds"]) * 1000)
            if usage is not None:
                receipt.update(peak_rss_kib=int(usage.ru_maxrss), cpu_user_seconds=usage.ru_utime,
                               cpu_system_seconds=usage.ru_stime)
            receipt["oom_killed"] = None
            if group is not None:
                try:
                    final = group.snapshot()
                    changes = {key: value - group.initial["events"].get(key, 0)
                               for key, value in final["events"].items()}
                    receipt["memory"].update(final=final, peak_bytes=final["peak_bytes"], events_delta=changes)
                    receipt["oom_killed"] = changes.get("oom_kill", 0) > 0
                except (OSError, ValueError) as caught:
                    receipt.update(status="observer-error", error=f"cgroup final observation: {caught}")
                    error = error or caught
            event("receipt", status=receipt["status"], exit_code=receipt["exit_code"])
    for name in ("stdout", "stderr", "telemetry"):
        receipt[f"{name}_sha256"] = _digest(paths[name])
    with paths["receipt"].open("x", encoding="utf-8") as stream:
        json.dump(receipt, stream, indent=2, sort_keys=True)
        stream.write("\n")
    if error is not None:
        if isinstance(error, (KeyboardInterrupt, SystemExit)):
            raise error
        raise ProcessSetupError(str(error), receipt) from error
    return receipt
