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
import subprocess
import sys
import tempfile
import textwrap
import time
import unittest


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


@unittest.skipUnless(sys.platform == "linux", "Linux wait4 and /proc are required")
class ProcessMeasurementTests(unittest.TestCase):
    def setUp(self) -> None:
        self.directory = ARTIFACTS / self.id().split(".")[-1]
        self.directory.mkdir()
        self.env = {"PATH": os.defpath, "LANG": "C.UTF-8"}

    def child(self, source: str, label: str = "child", **kwargs):
        defaults = {"timeout": 5.0, "sample_interval": 0.01, "allow_uncapped": True}
        defaults.update(kwargs)
        return runner.run([sys.executable, "-I", "-S", "-c", source], label=label,
                          env=self.env, cwd=self.directory, log_dir=self.directory, **defaults)

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
        receipt = self.child(
            "import subprocess,sys,time; "
            "p=subprocess.Popen([sys.executable,'-I','-S','-c','import time; time.sleep(30)']); "
            "print(p.pid,flush=True); time.sleep(30)", timeout=0.3)
        pid = int(Path(receipt["stdout_path"]).read_text().strip())
        status = Path(f"/proc/{pid}/stat")
        if status.exists():
            stat = status.read_text()
            self.assertEqual(stat[stat.rfind(")") + 2:].split()[0], "Z")
        self.assertEqual(receipt["exit_code"], -9)
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


if __name__ == "__main__":
    print(f"Harness-only child process artifacts: {ARTIFACTS}", file=sys.stderr)
    unittest.main()
