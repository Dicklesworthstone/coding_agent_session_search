"""Real Python-child tests of measurement plumbing, never CASS performance proof.

Artifacts are retained under the printed temporary directory. The cgroup test
needs CASS_BENCH_TEST_CGROUP_PARENT pointing to an already delegated parent;
without that explicit authorization it is reported as skipped, never passed.
"""
from __future__ import annotations

import importlib.util
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import textwrap
import time
import unittest
from unittest import mock


SCRIPT = Path(__file__).resolve().parents[1] / "forget_benchmark_process.py"
SPEC = importlib.util.spec_from_file_location("forget_benchmark_process", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
runner = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(runner)
ARTIFACTS = Path(tempfile.mkdtemp(prefix="cass-forget-process-tests-"))


class ByteLimitTests(unittest.TestCase):
    def test_explicit_decimal_and_binary_units(self) -> None:
        for source, expected in (("1", 1), ("40G", 40 * 1024**3), ("76GiB", 76 * 1024**3),
                                 ("11GB", 11 * 1000**3), ("512 MiB", 512 * 1024**2)):
            self.assertEqual(runner.parse_bytes(source), expected)
        for source in ("0", "-1G", "1.5G", "max", "GB", "40Gigabytes"):
            with self.subTest(source=source), self.assertRaises(ValueError):
                runner.parse_bytes(source)


@unittest.skipUnless(sys.platform == "linux", "Linux process groups are required")
class GroupInventoryTests(unittest.TestCase):
    def scan_unreadable(self, ownership: int | BaseException) -> list[int]:
        entry = mock.Mock()
        entry.name = "12345"
        entry.path = "/proc/12345"
        with mock.patch.object(runner.os, "scandir") as scan, \
                mock.patch.object(Path, "read_text", side_effect=PermissionError("stat unreadable")), \
                mock.patch.object(runner.os, "getpgid", return_value=ownership,
                                  side_effect=ownership if isinstance(ownership, BaseException) else None) as lookup:
            scan.return_value.__enter__.return_value = [entry]
            try:
                return runner._group_pids(777)
            finally:
                lookup.assert_called_once_with(12345)

    def test_unreadable_owned_member_is_not_an_empty_group(self) -> None:
        with self.assertRaisesRegex(PermissionError, "stat unreadable"):
            self.scan_unreadable(777)

    def test_unreadable_confirmed_outside_member_can_be_skipped(self) -> None:
        self.assertEqual(self.scan_unreadable(888), [])

    def test_unreadable_member_that_disappeared_can_be_skipped(self) -> None:
        self.assertEqual(self.scan_unreadable(ProcessLookupError("process exited")), [])

    def test_unknown_group_ownership_cannot_be_skipped(self) -> None:
        with self.assertRaisesRegex(PermissionError, "ownership unreadable"):
            self.scan_unreadable(PermissionError("ownership unreadable"))


@unittest.skipUnless(sys.platform == "linux", "Linux wait4 and /proc are required")
class ProcessMeasurementTests(unittest.TestCase):
    def setUp(self) -> None:
        self.directory = ARTIFACTS / self.id().split(".")[-1]
        self.directory.mkdir()
        self.env = {"PATH": os.defpath, "LANG": "C.UTF-8"}

    def child(self, source: str, label: str = "child", **kwargs):
        defaults = {"timeout": 5.0, "sample_interval": 0.01, "allow_uncapped": True}
        defaults.update(kwargs)
        receipt = runner.run([sys.executable, "-I", "-S", "-c", source], label=label,
                             env=self.env, cwd=self.directory, log_dir=self.directory, **defaults)
        self.assertTrue(receipt["containment"]["quiescent"])
        self.assertFalse(receipt["containment"]["cleanup_timed_out"])
        self.assertEqual(receipt["containment"]["remaining_pids"], [])
        self.assertTrue(receipt["root_reaped"])
        with self.assertRaises(ChildProcessError):
            os.waitpid(receipt["pid"], os.WNOHANG)
        if receipt["memory"]["limited"]:
            self.assertEqual(receipt["containment"]["scope"], "cgroup-v2")
            self.assertEqual(receipt["containment"]["cgroup_populated"], 0)
            self.assertTrue(receipt["memory"]["final_observed_after_quiescence"])
        else:
            self.assertEqual(receipt["containment"]["scope"], "process-group")
        return receipt

    def assert_not_live(self, pid: int) -> None:
        try:
            stat = Path(f"/proc/{pid}/stat").read_text()
        except FileNotFoundError:
            return
        self.assertIn(stat[stat.rfind(")") + 2:].split()[0], ("Z", "X"))

    def test_drains_large_outputs_and_captures_split_phase_lines(self) -> None:
        receipt = self.child(
            "import os,time; os.write(1,b'x'*170000); os.write(2,b'y'*170000+b'\\n'); "
            "os.write(2,b'forget: ana'); time.sleep(.03); "
            "os.write(2,b'lytics\\n'); time.sleep(.04); "
            "os.write(2,b'forget: phase=lexical-index\\n'); time.sleep(.04); "
            "os.write(1,b'z'*170000)"
        )
        self.assertEqual(receipt["exit_code"], 0)
        self.assertEqual(receipt["status"], "exited")
        self.assertEqual(Path(receipt["stdout_path"]).read_bytes(), b"x" * 170000 + b"z" * 170000)
        self.assertEqual([phase["phase"] for phase in receipt["phases"]], ["analytics", "lexical-index"])
        self.assertTrue(all(phase["observed_interval_ms"] > 0 for phase in receipt["phases"]))
        self.assertTrue(all(phase["samples"] > 0 for phase in receipt["phases"]))
        telemetry = [json.loads(line) for line in Path(receipt["telemetry_path"]).read_text().splitlines()]
        self.assertTrue(any(row["event"] == "phase_parser_long_line" for row in telemetry))
        self.assertTrue(any(row["event"] == "sample" for row in telemetry))
        self.assertEqual(json.loads(Path(receipt["receipt_path"]).read_text()), receipt)

    def test_wait4_peak_is_per_invocation_not_prior_child_maximum(self) -> None:
        # wait4 can preserve fork/exec startup RSS. Running this test after the
        # large fixture planner in a combined suite otherwise gives the small
        # child the suite controller's 40+ MiB floor. Keep the original 64 MiB
        # workload and >32 MiB discrimination gate, isolating unrelated suite
        # allocations in a fresh controller that performs BOTH measurements.
        controller = textwrap.dedent("""
            import importlib.util, os, sys
            from pathlib import Path
            spec = importlib.util.spec_from_file_location('runner', sys.argv[1])
            runner = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(runner)
            root = Path(sys.argv[2])
            for label, amount, pause in [('large', 64 * 1024**2, .06), ('small', 1024**2, .03)]:
                runner.run([sys.executable, '-I', '-S', '-c',
                            f'import time; x=bytearray({amount}); time.sleep({pause})'],
                           label=label, env={'PATH': os.defpath, 'LANG': 'C.UTF-8'},
                           cwd=root, log_dir=root, timeout=5, sample_interval=.01, allow_uncapped=True)
        """)
        with (self.directory / "controller.stdout").open("xb") as output, \
                (self.directory / "controller.stderr").open("xb") as errors:
            subprocess.run([sys.executable, "-I", "-S", "-c", controller,
                            str(SCRIPT), str(self.directory)], env=self.env,
                           cwd=self.directory, stdout=output, stderr=errors,
                           timeout=10, check=True)
        large = json.loads((self.directory / "large.receipt.json").read_text())
        small = json.loads((self.directory / "small.receipt.json").read_text())
        self.assertGreater(large["peak_rss_kib"], 64 * 1024)
        self.assertGreater(large["peak_rss_kib"] - small["peak_rss_kib"], 32 * 1024)
        self.assertGreater(large["sampled_root_peak_rss_bytes"], 64 * 1024 * 1024)
        self.assertGreater(large["sampled_root_hwm_peak_bytes"], 64 * 1024 * 1024)
        self.assertGreater(small["parent_rss_bytes_at_launch"], 0)
        self.assertIsNone(large["memory"]["peak_bytes"])

    def test_kills_at_observed_phase_and_keeps_receipt(self) -> None:
        receipt = self.child("import os,time; os.write(2,b'forget: analytics\\n'); time.sleep(30)",
                             interrupt_phase="analytics")
        self.assertEqual(receipt["exit_code"], -9)
        self.assertEqual(receipt["status"], "requested-interruption")
        self.assertTrue(receipt["interruption"]["phase_observed"])
        self.assertTrue(receipt["interruption"]["sent"])
        self.assertFalse(receipt["timed_out"])
        self.assertIsNone(receipt["oom_killed"])
        self.assertGreater(receipt["peak_rss_kib"], 0)

    def test_silent_timeout_is_recorded(self) -> None:
        receipt = self.child("import time; time.sleep(30)", timeout=0.15)
        self.assertTrue(receipt["timed_out"])
        self.assertEqual(receipt["status"], "timeout")
        self.assertEqual(receipt["exit_code"], -9)
        self.assertGreaterEqual(receipt["process_wall_ms"], 150)
        self.assertLess(receipt["process_wall_ms"], 2000)
        self.assertTrue(Path(receipt["receipt_path"]).is_file())

    def test_timeout_contains_descendant_process_group(self) -> None:
        # Give interpreter startup a realistic budget. A pipe acknowledgement
        # proves the descendant reached its own body before the parent emits
        # its PID; a short import/startup timeout must not masquerade as a
        # descendant-containment test.
        receipt = self.child(
            textwrap.dedent("""
                import os, time
                ready_read, ready_write = os.pipe()
                pid = os.fork()
                if pid == 0:
                    os.close(ready_read)
                    os.write(ready_write, b'1')
                    os.close(ready_write)
                    time.sleep(30)
                    os._exit(0)
                os.close(ready_write)
                assert os.read(ready_read, 1) == b'1'
                os.close(ready_read)
                print(pid, flush=True)
                time.sleep(30)
            """), timeout=3.0)
        output = Path(receipt["stdout_path"]).read_text().strip()
        self.assertTrue(output.isdecimal(), "descendant did not acknowledge readiness before timeout")
        pid = int(output)
        self.assertTrue(receipt["timed_out"])
        self.assertEqual(receipt["status"], "timeout")
        self.assertEqual(receipt["exit_code"], -9)
        telemetry = [json.loads(line) for line in Path(receipt["telemetry_path"]).read_text().splitlines()]
        self.assertTrue(any(row["event"] == "sample" and row["process_count"] >= 2
                            for row in telemetry), "descendant was never observed in the process group")
        status = Path(f"/proc/{pid}/stat")
        deadline = time.monotonic() + 2.0
        while True:
            try:
                stat = status.read_text()
            except FileNotFoundError:
                break
            if stat[stat.rfind(")") + 2:].split()[0] == "Z":
                break
            self.assertLess(time.monotonic(), deadline, "descendant was not contained")
            time.sleep(0.01)
        self.assertGreaterEqual(receipt["sampled_process_tree_peak_rss_bytes"],
                                receipt["sampled_root_peak_rss_bytes"])

    def test_nonzero_is_evidence_not_an_automatic_pass(self) -> None:
        receipt = self.child("import sys; print('failed child'); sys.exit(23)")
        self.assertEqual(receipt["exit_code"], 23)
        self.assertEqual(receipt["status"], "exited")
        self.assertEqual(json.loads(Path(receipt["receipt_path"]).read_text())["exit_code"], 23)

    def test_zero_exit_cannot_leave_a_descendant_with_closed_outputs(self) -> None:
        receipt = self.child(
            "import subprocess,sys; "
            "p=subprocess.Popen([sys.executable,'-I','-S','-c','import time; time.sleep(30)'], "
            "stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL); print(p.pid,flush=True)")
        self.assertEqual(receipt["exit_code"], 0)
        self.assertEqual(receipt["status"], "descendants-after-exit")
        self.assertTrue(receipt["descendants_terminated_after_root_exit"])
        pid = int(Path(receipt["stdout_path"]).read_text().strip())
        deadline = time.monotonic() + 1.0
        while True:
            try:
                stat = Path(f"/proc/{pid}/stat").read_text()
            except FileNotFoundError:
                break
            if stat[stat.rfind(")") + 2:].split()[0] == "Z":
                break
            self.assertLess(time.monotonic(), deadline, "descendant was not contained")
            time.sleep(0.01)

    def test_missing_phase_does_not_claim_interruption(self) -> None:
        receipt = self.child("print('no selected phase')", interrupt_phase="analytics")
        self.assertEqual(receipt["exit_code"], 0)
        self.assertFalse(receipt["interruption"]["phase_observed"])
        self.assertFalse(receipt["interruption"]["sent"])

    def test_interruption_quiesces_a_ready_descendant_before_returning(self) -> None:
        receipt = self.child(textwrap.dedent("""
            import os, time
            read_end, write_end = os.pipe()
            child = os.fork()
            if child == 0:
                os.close(read_end)
                os.close(1)
                os.close(2)
                os.write(write_end, b'1')
                os.close(write_end)
                time.sleep(30)
                os._exit(0)
            os.close(write_end)
            assert os.read(read_end, 1) == b'1'
            os.close(read_end)
            print(child, flush=True)
            os.write(2, b'forget: analytics\\n')
            time.sleep(30)
        """), interrupt_phase="analytics")
        self.assertEqual(receipt["status"], "requested-interruption")
        self.assertEqual(receipt["exit_code"], -9)
        self.assertTrue(receipt["interruption"]["sent"])
        self.assert_not_live(int(Path(receipt["stdout_path"]).read_text()))
        events = [json.loads(line)["event"]
                  for line in Path(receipt["telemetry_path"]).read_text().splitlines()]
        self.assertLess(events.index("process_exit"), events.index("containment_quiescent"))
        self.assertLess(events.index("containment_quiescent"), events.index("root_reaped"))

    def test_root_remains_waitable_until_inherited_pipes_drain(self) -> None:
        receipt = self.child(textwrap.dedent("""
            import os, time
            parent = os.getpid()
            def state(pid):
                try:
                    with open(f'/proc/{pid}/stat') as stream:
                        stat = stream.read()
                    return stat[stat.rfind(')') + 2:].split()[0]
                except FileNotFoundError:
                    return 'missing'
            read_end, write_end = os.pipe()
            child = os.fork()
            if child == 0:
                os.close(read_end)
                os.setsid()
                os.write(write_end, b'1')
                os.close(write_end)
                deadline = time.monotonic() + 3
                while state(parent) not in ('Z', 'missing') and time.monotonic() < deadline:
                    time.sleep(.01)
                time.sleep(.35)
                with open('root-reservation', 'w') as stream:
                    stream.write(state(parent))
                os._exit(0)
            os.close(write_end)
            assert os.read(read_end, 1) == b'1'
            os.close(read_end)
            print(child, flush=True)
            os._exit(0)
        """))
        self.assertEqual((self.directory / "root-reservation").read_text(), "Z")
        self.assertEqual(receipt["exit_code"], 0)
        self.assert_not_live(int(Path(receipt["stdout_path"]).read_text()))
        events = [json.loads(line) for line in Path(receipt["telemetry_path"]).read_text().splitlines()]
        exited = next(row["elapsed_seconds"] for row in events if row["event"] == "process_exit")
        self.assertLessEqual(receipt["process_wall_ms"], exited * 1000)
        self.assertLessEqual(exited, receipt["containment"]["observed_at_seconds"])

    def test_escaped_pipe_holder_cannot_certify_an_interruption(self) -> None:
        # This child deliberately leaves the uncapped run's owned group. The
        # harness must fail closed without signalling that other group. Release
        # it cooperatively in finally; the fixture also has its own time bound.
        started = time.monotonic()
        try:
            with self.assertRaises(runner.ProcessSetupError) as raised:
                self.child(textwrap.dedent("""
                    import os, time
                    read_end, write_end = os.pipe()
                    child = os.fork()
                    if child == 0:
                        os.close(read_end)
                        os.setsid()
                        with open('detached-pid', 'w') as stream:
                            stream.write(str(os.getpid()))
                        os.write(write_end, b'1')
                        os.close(write_end)
                        deadline = time.monotonic() + 10
                        while not os.path.exists('release-detached') and time.monotonic() < deadline:
                            time.sleep(.01)
                        os._exit(0)
                    os.close(write_end)
                    assert os.read(read_end, 1) == b'1'
                    os.close(read_end)
                    os.write(2, b'forget: analytics\\n')
                    time.sleep(30)
                """), interrupt_phase="analytics", cleanup_timeout=0.2)
            self.assertLess(time.monotonic() - started, 5)
            receipt = raised.exception.receipt
            self.assertEqual(receipt["status"], "cleanup-timeout")
            self.assertEqual(receipt["exit_code"], -9)
            self.assertTrue(receipt["interruption"]["sent"])
            self.assertTrue(receipt["containment"]["cleanup_timed_out"])
            self.assertFalse(receipt["containment"]["quiescent"])
            self.assertTrue(receipt["output_pipes_remained_open_after_kill"])
            self.assertFalse(receipt["memory"]["final_observed_after_quiescence"])
            self.assertEqual(json.loads(Path(receipt["receipt_path"]).read_text()), receipt)
            pid = int((self.directory / "detached-pid").read_text())
            stat = Path(f"/proc/{pid}/stat").read_text()
            self.assertNotIn(stat[stat.rfind(")") + 2:].split()[0], ("Z", "X"))
        finally:
            (self.directory / "release-detached").write_text("release")
            pid_file = self.directory / "detached-pid"
            if pid_file.exists():
                pid = int(pid_file.read_text())
                deadline = time.monotonic() + 2
                while time.monotonic() < deadline:
                    try:
                        self.assert_not_live(pid)
                        break
                    except AssertionError:
                        time.sleep(.01)
                self.assert_not_live(pid)

    def test_observer_failure_retains_receipt_and_cleans_owned_root(self) -> None:
        real_rss = runner._rss
        def fail_child_sample(pid):
            if pid != os.getpid():
                raise OSError("injected RSS observation failure")
            return real_rss(pid)
        with mock.patch.object(runner, "_rss", side_effect=fail_child_sample):
            with self.assertRaises(runner.ProcessSetupError) as raised:
                self.child("import time; time.sleep(30)")
        receipt = raised.exception.receipt
        self.assertEqual(receipt["status"], "observer-error")
        self.assertIn("injected RSS observation failure", receipt["error"])
        self.assertTrue(receipt["containment"]["quiescent"])
        self.assertTrue(receipt["root_reaped"])
        self.assert_not_live(receipt["pid"])
        self.assertEqual(json.loads(Path(receipt["receipt_path"]).read_text()), receipt)

    def test_unknown_containment_never_becomes_an_empty_scope(self) -> None:
        with mock.patch.object(runner, "_group_pids", side_effect=PermissionError("scope unreadable")):
            with self.assertRaises(runner.ProcessSetupError) as raised:
                self.child("import time; time.sleep(30)", cleanup_timeout=0.05)
        receipt = raised.exception.receipt
        self.assertFalse(receipt["containment"]["quiescent"])
        self.assertTrue(receipt["containment"]["cleanup_timed_out"])
        self.assertTrue(receipt["root_reaped"])
        self.assert_not_live(receipt["pid"])
        self.assertEqual(json.loads(Path(receipt["receipt_path"]).read_text()), receipt)

    def test_unreadable_owned_process_stat_retains_failed_receipt(self) -> None:
        launched = {}
        real_popen, real_read = runner.subprocess.Popen, Path.read_text

        def launch(*args, **kwargs):
            child = real_popen(*args, **kwargs)
            launched["pid"] = child.pid
            return child

        def denied_read(path, *args, **kwargs):
            if str(path) == f"/proc/{launched.get('pid')}/stat":
                raise PermissionError("owned stat unreadable")
            return real_read(path, *args, **kwargs)

        with mock.patch.object(runner.subprocess, "Popen", side_effect=launch), \
                mock.patch.object(Path, "read_text", new=denied_read):
            with self.assertRaises(runner.ProcessSetupError) as raised:
                self.child("import time; time.sleep(30)", timeout=0.3, cleanup_timeout=0.05)
        receipt = raised.exception.receipt
        self.assertEqual(receipt["pid"], launched["pid"])
        self.assertIn("owned stat unreadable", receipt["error"])
        self.assertEqual(receipt["status"], "cleanup-timeout")
        self.assertFalse(receipt["containment"]["quiescent"])
        self.assertTrue(receipt["root_reaped"])
        self.assert_not_live(receipt["pid"])
        self.assertEqual(json.loads(Path(receipt["receipt_path"]).read_text()), receipt)

    def test_auto_reaping_sigchld_is_refused_before_launch(self) -> None:
        previous = signal.signal(signal.SIGCHLD, signal.SIG_IGN)
        try:
            with self.assertRaises(runner.ProcessSetupError) as raised:
                self.child("open('unexpected-launch', 'w').write('unsafe')")
        finally:
            signal.signal(signal.SIGCHLD, previous)
        self.assertEqual(raised.exception.receipt["status"], "setup-error")
        self.assertFalse((self.directory / "unexpected-launch").exists())

    def test_uncapped_run_requires_explicit_opt_in(self) -> None:
        with self.assertRaises(runner.ProcessSetupError) as raised:
            self.child("raise Exception('must not launch')", allow_uncapped=False)
        receipt = raised.exception.receipt
        self.assertEqual(receipt["status"], "setup-error")
        self.assertIsNone(receipt["exit_code"])
        self.assertTrue(Path(receipt["receipt_path"]).is_file())

    def test_ordinary_directory_cannot_substitute_for_cgroup(self) -> None:
        with self.assertRaises(runner.ProcessSetupError) as raised:
            self.child("open('unexpected-launch','w').write('unsafe')", memory_max=1024**3,
                       cgroup_parent=self.directory)
        receipt = raised.exception.receipt
        self.assertEqual(receipt["status"], "setup-error")
        self.assertFalse((self.directory / "unexpected-launch").exists())
        self.assertIsNone(receipt["exit_code"])
        self.assertFalse(receipt["memory"]["limited"])

    def test_output_limit_kills_and_preserves_failure(self) -> None:
        receipt = self.child("import os,time; os.write(1,b'x'*65536); time.sleep(30)",
                             max_output_bytes=1024)
        self.assertTrue(receipt["output_limit_exceeded"])
        self.assertEqual(receipt["status"], "output-limit")
        self.assertEqual(receipt["exit_code"], -9)

    @unittest.skipUnless(os.environ.get("CASS_BENCH_TEST_CGROUP_PARENT"),
                         "no explicitly delegated CASS_BENCH_TEST_CGROUP_PARENT; native cgroup OOM unmeasured")
    def test_real_cgroup_limit_reports_kernel_oom_kill(self) -> None:
        parent = Path(os.environ["CASS_BENCH_TEST_CGROUP_PARENT"])
        receipt = self.child("import time; x=bytearray(256*1024*1024); time.sleep(30)",
                             memory_max=64 * 1024 * 1024, cgroup_parent=parent,
                             allow_uncapped=False, timeout=10)
        self.assertEqual(receipt["exit_code"], -9)
        self.assertTrue(receipt["memory"]["limited"])
        self.assertEqual(receipt["memory"]["swap_max_bytes"], 0)
        self.assertGreater(receipt["memory"]["peak_bytes"], 0)
        self.assertGreaterEqual(receipt["memory"]["events_delta"]["oom_kill"], 1)
        self.assertTrue(receipt["oom_killed"])
        self.assertFalse(receipt["timed_out"])

    @unittest.skipUnless(os.environ.get("CASS_BENCH_TEST_CGROUP_PARENT"),
                         "no explicitly delegated CASS_BENCH_TEST_CGROUP_PARENT; nested cgroup cleanup unmeasured")
    def test_nested_cgroup_descendant_is_killed_and_observed_empty(self) -> None:
        parent = Path(os.environ["CASS_BENCH_TEST_CGROUP_PARENT"]).resolve()
        self.env["CASS_BENCH_TEST_PARENT"] = str(parent)
        receipt = self.child(textwrap.dedent("""
            import os, time
            from pathlib import Path
            parent = Path(os.environ['CASS_BENCH_TEST_PARENT'])
            owned = next(path for path in parent.glob(f'cass-forget-bench-{os.getppid()}-*')
                         if str(os.getpid()) in (path / 'cgroup.procs').read_text().split())
            nested = owned / 'worker'
            nested.mkdir()
            read_end, write_end = os.pipe()
            child = os.fork()
            if child == 0:
                os.close(read_end)
                os.setsid()
                (nested / 'cgroup.procs').write_text(str(os.getpid()))
                memory = bytearray(8 * 1024 * 1024)
                os.close(1)
                os.close(2)
                os.write(write_end, b'1')
                os.close(write_end)
                time.sleep(30)
                os._exit(0)
            os.close(write_end)
            assert os.read(read_end, 1) == b'1'
            os.close(read_end)
            print(child, flush=True)
            time.sleep(.1)
            os._exit(0)
        """), memory_max=64 * 1024 * 1024, cgroup_parent=parent,
             allow_uncapped=False, timeout=10)
        self.assertEqual(receipt["exit_code"], 0)
        self.assertEqual(receipt["status"], "descendants-after-exit")
        self.assert_not_live(int(Path(receipt["stdout_path"]).read_text()))
        owned = Path(receipt["memory"]["cgroup_path"])
        populated = dict(line.split() for line in (owned / "cgroup.events").read_text().splitlines())
        self.assertEqual(populated["populated"], "0")
        self.assertEqual((owned / "worker" / "cgroup.procs").read_text().strip(), "")
        self.assertGreaterEqual(receipt["memory"]["peak_bytes"], 8 * 1024 * 1024)
        self.assertEqual(receipt["memory"]["events_delta"]["oom_kill"], 0)


if __name__ == "__main__":
    print(f"Harness-only child process artifacts: {ARTIFACTS}", file=sys.stderr)
    unittest.main()
