#!/usr/bin/env python3
"""Failure-aware workload harness tests, not real CASS performance results."""
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

import test_cass_lexical_ipc as guards

MODULE_PATH = guards.MODULE_PATH


@unittest.skipUnless(sys.platform.startswith("linux"), "Linux IPC required")
class BenchmarkTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.home = Path(self.temp.name)
        self.index = self.home / "index"
        self.index.mkdir()
        payload = b"alpha beta " * 1024
        (self.index / "segment.qseg").write_bytes(payload)
        (self.index / "MANIFEST").write_text(json.dumps({
            "hash": hashlib.sha256(payload).hexdigest()}))
        fake = guards.FAKE.replace("quill.manifest.json", "MANIFEST")
        fake = fake.replace("#!/usr/bin/env python3", "#!" + sys.executable + " -S")
        fake = fake.replace(
            "    print(json.dumps(dict(schema_version=1, id=request['id'], ok=True, result=result)), flush=True)",
            "    if mode == 'different_reuse' and loaded:\n"
            "        result['hits'][0]['score'] = 9.0\n"
            "    print(json.dumps(dict(schema_version=1, id=request['id'], ok=True, result=result)), flush=True)")
        self.cass = self.home / "fake-cass"
        self.cass.write_text(fake)
        self.cass.chmod(0o700)

    def tearDown(self):
        self.temp.cleanup()

    def mode(self, value):
        (self.home / "mode").write_text(value)


    def benchmark(self, *extra):
        evidence = self.home / "measurement.json"
        result = subprocess.run(
            [sys.executable, "-S", str(MODULE_PATH.with_name("benchmark_lexical_ipc.py")),
             "--cass", str(self.cass), "--index", str(self.index),
             "--query", "alpha", "--rounds", "2", "--timeout", "2",
             "--output", str(evidence), *extra],
            capture_output=True, timeout=10)
        return result, evidence

    def test_benchmark_separates_fresh_retained_and_client_cpu(self):
        result, evidence = self.benchmark()
        self.assertEqual(result.returncode, 0, result.stderr.decode())
        report = json.loads(evidence.read_text())
        workloads = report["workloads"]
        self.assertEqual(len(workloads["strict_fresh_process"]["samples"]), 2)
        self.assertEqual(len(workloads["retained_native"]["retained_queries"]), 2)
        clients = workloads["fresh_ipc_clients"]
        self.assertEqual([sample["phase"] for sample in clients["samples"]],
                         ["initial_admission", "fresh_client_reuse", "fresh_client_reuse",
                          "full_verify_control", "post_verify_reuse"])
        self.assertIn("complete_group_cpu_ms", clients)
        self.assertIn("client_cpu_ms", clients["samples"][0])
        self.assertEqual(clients["client_kind"], "python_frontend")
        self.assertTrue(report["identical_hits_scores_and_order"])

    def test_benchmark_refuses_missing_explicit_native_client(self):
        result, evidence = self.benchmark("--client", str(self.home / "missing"))
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(evidence.exists())

    def test_benchmark_refuses_reuse_score_mismatch(self):
        self.mode("different_reuse")
        result, evidence = self.benchmark()
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(evidence.exists())

    def test_benchmark_refuses_changed_index(self):
        self.mode("mutate")
        result, evidence = self.benchmark()
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(evidence.exists())

    def test_benchmark_refuses_corrupted_worker_output(self):
        self.mode("bad_json")
        result, evidence = self.benchmark()
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(evidence.exists())

    def test_benchmark_does_not_overwrite_existing_evidence(self):
        evidence = self.home / "measurement.json"
        evidence.write_text("previous evidence")
        result, _ = self.benchmark()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(evidence.read_text(), "previous evidence")


if __name__ == "__main__":
    unittest.main()
