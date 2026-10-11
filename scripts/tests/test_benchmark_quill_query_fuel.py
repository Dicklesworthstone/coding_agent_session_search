"""Driver tests only: these do not run or qualify the Rust search engine."""
from __future__ import annotations

import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "benchmark_quill_query_fuel.py"
spec = importlib.util.spec_from_file_location("quill_fuel_bench", SCRIPT)
assert spec is not None and spec.loader is not None
bench = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bench)

REFUSAL = (
    b"executing a Quill lexical query: scalar Quill query fuel exhausted after "
    b"10000000/10000000 units (segments=206, dictionary_blocks=9987317, "
    b"posting_blocks=12477, position_docs=0)"
)


class DiagnosticsTests(unittest.TestCase):
    def test_reported_206_segment_error_is_parsed(self):
        work = bench.diagnostics(REFUSAL)["fuel_refusals"][0]
        self.assertEqual(work, {
            "consumed": 10_000_000, "budget": 10_000_000,
            "segments_touched": 206, "dictionary_blocks": 9_987_317,
            "posting_blocks": 12_477, "position_docs": 0,
        })

    def test_repeated_context_is_not_counted_twice(self):
        self.assertEqual(len(bench.diagnostics(REFUSAL + b"\n" + REFUSAL)["fuel_refusals"]), 1)

    def test_json_mode_structured_warning_retains_counters(self):
        warning = (b'WARN event="quill.argus.query_fuel_exhausted" phase="search" '
                   b'budget=10000000 consumed=10000000 segments_touched=206 '
                   b'dictionary_blocks=9987317 posting_blocks=12477 position_docs=0')
        self.assertEqual(bench.diagnostics(warning)["fuel_refusals"],
                         bench.diagnostics(REFUSAL)["fuel_refusals"])
        self.assertEqual(len(bench.diagnostics(warning + b"\n" + REFUSAL)["fuel_refusals"]), 1)

    def test_incomplete_or_unrelated_work_fields_are_not_invented(self):
        self.assertIsNone(bench.diagnostics(
            b'event="quill.argus.query_fuel_exhausted" budget=10000000'
        )["fuel_refusals"])
        self.assertIsNone(bench.diagnostics(
            b'budget=10000000 consumed=10000000 segments_touched=206 '
            b'dictionary_blocks=9987317 posting_blocks=12477 position_docs=0'
        )["fuel_refusals"])

    def test_missing_work_is_unknown_not_zero(self):
        self.assertIsNone(bench.diagnostics(b"success")["fuel_refusals"])

    def test_backend_attempt_is_not_serving_receipt(self):
        data = bench.diagnostics(
            b'\x1b[32mINFO\x1b[0m search_start backend="tantivy" query="git fixture"\n'
            b'INFO search_start backend="sqlite-fts5"\n'
            b'INFO unrelated backend="not-a-search"\n'
        )
        self.assertEqual(data["observed_search_start_backends"], ["tantivy", "sqlite-fts5"])
        self.assertIsNone(data["serving_backend"])

    def test_valid_json_empty_is_distinct_from_non_json(self):
        self.assertEqual(bench.hit_count(b'{"hits": []}'), 0)
        self.assertEqual(bench.hit_count(b'[{"content": "fixture"}]'), 1)
        for invalid in (b'{}', b'{"hits": "bad"}', b'[1]', b'not JSON'):
            self.assertIsNone(bench.hit_count(invalid))

    def test_fuel_classification_requires_failed_exit(self):
        data = {"stdout": b'{"hits": [{}]}', "stderr": REFUSAL,
                "termination": "completed", "returncode": 0, "elapsed_ms": 4.0}
        self.assertEqual(bench.sample_record(data, "git fixture", 0)["outcome"], "success")
        data["returncode"] = 1
        self.assertEqual(bench.sample_record(data, "git fixture", 0)["outcome"], "fuel_exhausted")

    def test_deadline_is_not_reported_as_fuel_exhaustion(self):
        data = {"stdout": b'', "stderr": REFUSAL, "termination": "deadline",
                "returncode": -9, "elapsed_ms": 100.0}
        self.assertEqual(bench.sample_record(data, "test isolation", 0)["outcome"], "deadline")

    def test_failure_does_not_improve_success_percentile(self):
        report = bench.summarize([
            {"outcome": "deadline", "elapsed_ms": 1.0},
            {"outcome": "success", "elapsed_ms": 20.0},
            {"outcome": "success", "elapsed_ms": 40.0},
        ])
        self.assertEqual(report["success_p50_ms"], 30.0)
        self.assertEqual(report["success_p95_ms"], 40.0)
        self.assertEqual(report["outcomes"]["deadline"], 1)
        self.assertIsNone(bench.summarize([])["success_p95_ms"])

    def test_invalid_limits_are_rejected(self):
        for value in ("0", "-1", str(1 << 64), "NaN"):
            with self.assertRaises(Exception):
                bench.positive_int(value)
        self.assertEqual(bench.positive_int("10000000"), 10_000_000)


@unittest.skipUnless(os.name == "posix", "POSIX containment tests")
class ProcessTests(unittest.TestCase):
    def run_code(self, code, timeout=2.0, cap=4096):
        return bench.run_bounded([sys.executable, "-c", code], env=dict(os.environ),
                                 timeout_s=timeout, output_limit=cap)

    def test_successful_process_and_both_pipes(self):
        result = self.run_code('import sys; print("fixture"); print("trace", file=sys.stderr)')
        self.assertEqual(result["termination"], "completed")
        self.assertEqual(result["returncode"], 0)
        self.assertEqual(result["stdout"], b"fixture\n")
        self.assertEqual(result["stderr"], b"trace\n")

    def test_timeout_reaps_direct_child(self):
        result = self.run_code('import time; time.sleep(30)', timeout=0.15)
        self.assertEqual(result["termination"], "deadline")
        self.assertLess(result["elapsed_ms"], 3000)
        self.assertNotEqual(result["returncode"], 0)

    def test_closed_pipes_do_not_remove_deadline(self):
        result = self.run_code('import os, time; os.close(1); os.close(2); time.sleep(30)', timeout=0.15)
        self.assertEqual(result["termination"], "deadline")
        self.assertLess(result["elapsed_ms"], 3000)

    def test_spawn_failure_is_a_receipt(self):
        with tempfile.TemporaryDirectory() as root:
            result = bench.run_bounded([str(Path(root) / "missing")],
                                       env=dict(os.environ), timeout_s=1)
        self.assertEqual(result["termination"], "spawn_error")
        self.assertIsNotNone(result["error_errno"])
        self.assertIsNone(result["returncode"])

    def test_output_cap_kills_noisy_process(self):
        result = self.run_code('import os;\nwhile True: os.write(1, b"x" * 65536)', cap=1024)
        self.assertEqual(result["termination"], "output_limit")
        self.assertLessEqual(len(result["stdout"]) + len(result["stderr"]), 1024)

    def test_descendant_holding_pipes_does_not_escape_deadline(self):
        with tempfile.TemporaryDirectory() as root:
            marker = Path(root) / "escaped"
            child = f'import time; from pathlib import Path; time.sleep(0.8); Path({str(marker)!r}).write_text("bad")'
            parent = f'import subprocess, sys; subprocess.Popen([sys.executable, "-c", {child!r}])'
            result = self.run_code(parent, timeout=0.15)
            self.assertEqual(result["termination"], "deadline")
            time.sleep(0.9)
            self.assertFalse(marker.exists(), "descendant survived process-group cancellation")

    def test_nonfinite_timeout_rejected_before_spawning(self):
        for timeout in (float("nan"), float("inf"), 0, -1):
            with self.assertRaises(ValueError):
                self.run_code('raise SystemExit("must not run")', timeout=timeout)

    def test_complete_cli_receipt_and_ambient_fuel_is_not_inherited(self):
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            executable = root / "fake-cass"
            executable.write_text(
                '#!' + sys.executable + '\nimport json, os, sys\n'
                'assert "CASS_QUILL_QUERY_FUEL_BUDGET" not in os.environ\n'
                'assert sys.argv[1:3] == ["--db", ' + repr(str(root / "archive.db")) + ']\n'
                'assert sys.argv[3] == "search" and sys.argv[4:6] == ["--mode", "lexical"] and sys.argv[-2] == "--"\n'
                'print(json.dumps({"hits": [{"content": "DO_NOT_PERSIST_PRIVATE_CONTENT"}]}))\n'
                'print(\'INFO search_start backend="tantivy"\', file=sys.stderr)\n'
            )
            executable.chmod(0o700)
            (root / "archive.db").touch()
            output = root / "receipt.json"
            argv = [sys.executable, str(SCRIPT), "--cass", str(executable),
                    "--data-dir", str(root), "--db", str(root / "archive.db"),
                    "--output", str(output), "--disposable-copy", "--repetitions", "1"]
            env = dict(os.environ, CASS_QUILL_QUERY_FUEL_BUDGET="999999999")
            completed = subprocess.run(argv, env=env, capture_output=True, timeout=10)
            self.assertEqual(completed.returncode, 0, completed.stderr.decode())
            report = json.loads(output.read_text())
            self.assertEqual([s["query"] for s in report["samples"]], list(bench.QUERIES))
            self.assertTrue(all(s["hit_count"] == 1 for s in report["samples"]))
            self.assertNotIn("DO_NOT_PERSIST_PRIVATE_CONTENT", output.read_text())
            self.assertIsNone(report["fuel_budget_override"])
            self.assertEqual(report["by_query"]["git fixture"]["successful_samples"], 1)
            # A second run cannot replace an earlier receipt.
            previous = output.read_bytes()
            repeated = subprocess.run(argv, env=env, capture_output=True, timeout=10)
            self.assertNotEqual(repeated.returncode, 0)
            self.assertEqual(output.read_bytes(), previous)


if __name__ == "__main__":
    unittest.main()
