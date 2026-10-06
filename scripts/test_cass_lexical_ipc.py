#!/usr/bin/env python3
"""Exercise the real IPC/guard implementation with a fake strict CASS process.

This validates transport, lifecycle and filesystem guard behavior; it does not
qualify Quill or claim CASS timings. Run: python3 -m unittest discover -s scripts
-p 'test_cass_lexical_ipc.py' -v
"""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import time
import unittest

MODULE_PATH = Path(__file__).with_name("cass_lexical_ipc.py")
spec = importlib.util.spec_from_file_location("cass_lexical_ipc", MODULE_PATH)
ipc = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ipc)

FAKE = r"""#!/usr/bin/env python3
import hashlib, json, os, pathlib, sys, time
index = pathlib.Path(sys.argv[sys.argv.index('--index') + 1])
home = pathlib.Path(__file__).parent
mode = (home / 'mode').read_text() if (home / 'mode').exists() else 'normal'
loaded = False
for line in sys.stdin:
    request = json.loads(line)
    if mode == 'hang':
        time.sleep(60)
    if mode == 'exit':
        sys.exit(7)
    if mode == 'bad_json':
        print('not json', flush=True)
        continue
    if mode == 'duplicate':
        print('{"schema_version":1,"id":1,"ok":true,"ok":false}', flush=True)
        continue
    if mode == 'oversized':
        print('x' * (1024*1024+1), flush=True)
        continue
    if mode == 'wrong_id':
        print(json.dumps(dict(schema_version=1, id=request['id'] + 1, ok=True, result={})), flush=True)
        continue
    if mode == 'bad_schema':
        print(json.dumps(dict(schema_version=True, id=request['id'], ok=True, result={})), flush=True)
        continue
    if not loaded:
        with (home / 'opens').open('a') as log:
            log.write(str(os.getpid()) + '\n')
        expected = json.loads((index / 'quill.manifest.json').read_text())
        actual = hashlib.sha256((index / 'segment.qseg').read_bytes()).hexdigest()
        if actual != expected['hash']:
            print(json.dumps(dict(schema_version=1, id=request['id'], ok=False,
                                  error={'kind':'corrupt', 'message':'full hash failed'})), flush=True)
            continue
    if mode == 'mutate':
        with (index / 'segment.qseg').open('r+b') as changed:
            changed.write(b'Z')
    result = {'hits':[{'content': 'alpha', 'score':1.0}], 'reader_reused':loaded}
    print(json.dumps(dict(schema_version=1, id=request['id'], ok=True, result=result)), flush=True)
    loaded = True
"""


@unittest.skipUnless(sys.platform.startswith("linux"), "Linux peer credentials required")
class AdmissionTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.home = Path(self.temp.name)
        self.index = self.home / "index"
        self.index.mkdir()
        self.segment = self.index / "segment.qseg"
        self.segment.write_bytes(b"alpha beta " * 1024)
        self.publish_manifest()
        self.cass = self.home / "fake-cass"
        self.cass.write_text(FAKE.replace("#!/usr/bin/env python3", "#!" + sys.executable + " -S"))
        self.cass.chmod(0o700)
        self.owner = ipc.Owner(str(self.cass), self.index, 2)
        self.server = None

    def tearDown(self):
        self.owner.close()
        if self.server is not None:
            self.server.terminate()
            try:
                self.server.communicate(timeout=5)
            except subprocess.TimeoutExpired:
                self.server.kill()
                self.server.communicate(timeout=5)
        self.temp.cleanup()

    def publish_manifest(self):
        (self.index / "quill.manifest.json").write_text(json.dumps({
            "hash": hashlib.sha256(self.segment.read_bytes()).hexdigest()}))

    def request(self, **extra):
        return {"op": "search", "id": 1, "query": "alpha", **extra}

    def search(self, **extra):
        return self.owner.search(self.request(**extra))

    def opens(self):
        log = self.home / "opens"
        return log.read_text().splitlines() if log.exists() else []

    def mode(self, value):
        (self.home / "mode").write_text(value)

    def start_server(self):
        private = self.home / "private"
        private.mkdir(mode=0o700)
        endpoint = private / "lexical.sock"
        self.server = subprocess.Popen(
            [sys.executable, "-S", str(MODULE_PATH), "serve", "--socket", str(endpoint),
             "--index", str(self.index), "--cass", str(self.cass), "--timeout", "2"],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            if self.server.poll() is not None:
                self.fail(self.server.communicate()[1].decode())
            if endpoint.exists() and endpoint.stat().st_mode & 0o777 == 0o600:
                return endpoint
            time.sleep(0.01)
        self.fail("IPC owner did not start")

    def test_first_open_and_reuse(self):
        first, second = self.search(), self.search()
        self.assertEqual(first["admission"]["mode"], "strict_full")
        self.assertEqual(second["admission"]["mode"], "retained_guarded")
        self.assertEqual(first["result"]["hits"], second["result"]["hits"])
        self.assertEqual(len(self.opens()), 1)
        self.assertTrue(second["result"]["reader_reused"])

    def test_full_verify_releases_and_reopens(self):
        self.search()
        first_pid = self.owner.process.pid
        response = self.search(full_verify=True)
        self.assertNotEqual(self.owner.process.pid, first_pid)
        self.assertEqual(response["admission"]["mode"], "strict_full")
        self.assertEqual(response["admission"]["owner_epoch"], 2)
        self.assertEqual(len(self.opens()), 2)

    def test_adversarial_segment_changes_force_strict_failure(self):
        cases = ("replacement", "truncation", "same_length", "restored_mtime")
        original = self.segment.read_bytes()
        for kind in cases:
            with self.subTest(kind=kind):
                self.owner.close()
                self.segment.write_bytes(original)
                self.publish_manifest()
                self.assertTrue(self.search()["ok"])
                info = self.segment.stat()
                if kind == "replacement":
                    new = self.index / "replacement"
                    new.write_bytes(b"x" * len(original))
                    os.utime(new, ns=(info.st_atime_ns, info.st_mtime_ns))
                    new.replace(self.segment)
                elif kind == "truncation":
                    self.segment.write_bytes(original[:128])
                else:
                    with self.segment.open("r+b") as changed:
                        changed.write(b"x")
                    if kind == "restored_mtime":
                        os.utime(self.segment, ns=(info.st_atime_ns, info.st_mtime_ns))
                response = self.search()
                self.assertFalse(response["ok"])
                self.assertEqual(response["error"]["kind"], "corrupt")
                self.assertIsNone(self.owner.process)
                self.assertIsNone(self.owner.guard)

    def test_valid_publication_reopens(self):
        self.search()
        self.segment.write_bytes(b"new contents")
        self.publish_manifest()
        response = self.search()
        self.assertTrue(response["ok"])
        self.assertEqual(response["admission"]["mode"], "strict_full")
        self.assertEqual(len(self.opens()), 2)

    def test_directory_replacement_reopens(self):
        self.search()
        self.index.rename(self.home / "old")
        self.index.mkdir()
        self.segment.write_bytes(b"corruption")
        (self.index / "quill.manifest.json").write_text(
            (self.home / "old" / "quill.manifest.json").read_text())
        self.assertFalse(self.search()["ok"])
        self.assertEqual(len(self.opens()), 2)

    def test_mutation_during_query_discards_result(self):
        self.mode("mutate")
        with self.assertRaises(ipc.Failure) as raised:
            self.search()
        self.assertEqual(raised.exception.kind, "index_changed")
        self.assertIsNone(self.owner.process)

    def test_corrupted_worker_protocol_never_admits(self):
        for mode in ("bad_json", "duplicate", "oversized", "wrong_id", "bad_schema", "exit"):
            with self.subTest(mode=mode):
                self.mode(mode)
                with self.assertRaises(ipc.Failure):
                    self.search()
                self.assertIsNone(self.owner.process)
                self.assertIsNone(self.owner.guard)

    def test_timeout_reaps_worker(self):
        self.mode("hang")
        self.owner.timeout = 0.15
        start = time.monotonic()
        with self.assertRaises(ipc.Failure) as raised:
            self.search()
        self.assertEqual(raised.exception.kind, "timeout")
        self.assertLess(time.monotonic() - start, 3)
        self.assertIsNone(self.owner.process)

    def test_invalid_requests_do_not_start_worker(self):
        for extra in ({"query": ""}, {"id": True}, {"full_verify": 1},
                      {"limit": 101}, {"offset": -1}, {"receipt": {}},
                      {"op": "reload"}, {"query": "\ud800"}):
            with self.subTest(extra=extra):
                with self.assertRaises(ipc.Failure):
                    self.search(**extra)
                self.assertIsNone(self.owner.process)
        self.assertEqual(self.opens(), [])

    def test_index_symlink_refused(self):
        target = self.home / "outside"
        target.write_text("data")
        (self.index / "linked").symlink_to(target)
        with self.assertRaises(OSError):
            self.search()
        self.assertIsNone(self.owner.process)

    def test_fifo_refused_without_blocking(self):
        os.mkfifo(self.index / "fifo")
        start = time.monotonic()
        with self.assertRaises(ipc.Failure):
            self.search()
        self.assertLess(time.monotonic() - start, 1)

    def test_guard_detects_new_files(self):
        self.search()
        (self.index / "added").write_text("new sidecar")
        response = self.search()
        self.assertEqual(response["admission"]["mode"], "strict_full")

    def test_file_descriptor_cleanup(self):
        before = len(os.listdir("/proc/self/fd"))
        for _ in range(12):
            self.search(full_verify=True)
        self.owner.close()
        self.assertLessEqual(len(os.listdir("/proc/self/fd")), before)

    def test_fresh_clients_share_one_admission(self):
        endpoint = self.start_server()
        command = [sys.executable, "-S", str(MODULE_PATH), "search", "alpha",
                   "--socket", str(endpoint)]
        responses = []
        for extra in ([], [], ["--full-verify"]):
            result = subprocess.run(command + extra, capture_output=True, timeout=5)
            self.assertEqual(result.returncode, 0, result.stderr.decode())
            responses.append(json.loads(result.stdout))
        self.assertEqual([r["admission"]["mode"] for r in responses],
                         ["strict_full", "retained_guarded", "strict_full"])
        self.assertEqual(len(self.opens()), 2)
        self.assertEqual(responses[0]["result"]["hits"], responses[1]["result"]["hits"])

    def test_direct_full_verify_does_not_need_socket(self):
        result = subprocess.run(
            [sys.executable, "-S", str(MODULE_PATH), "search", "alpha",
             "--index", str(self.index), "--cass", str(self.cass)],
            capture_output=True, timeout=5)
        self.assertEqual(result.returncode, 0, result.stderr.decode())
        self.assertTrue(json.loads(result.stdout)["admission"]["full_verify_requested"])

    def test_insecure_and_symlink_socket_parent_refused(self):
        insecure = self.home / "insecure"
        insecure.mkdir(mode=0o755)
        with self.assertRaises(ipc.Failure):
            ipc.query(insecure / "s", self.request(), 1)
        private = self.home / "private"
        private.mkdir(mode=0o700)
        linked = self.home / "linked"
        linked.symlink_to(private, target_is_directory=True)
        with self.assertRaises(OSError):
            ipc.query(linked / "s", self.request(), 1)

    def test_existing_endpoint_is_not_deleted(self):
        endpoint = self.start_server()
        original = endpoint.stat().st_ino
        result = subprocess.run(
            [sys.executable, "-S", str(MODULE_PATH), "serve", "--socket", str(endpoint),
             "--index", str(self.index), "--cass", str(self.cass)],
            capture_output=True, timeout=5)
        self.assertEqual(result.returncode, 1)
        self.assertEqual(endpoint.stat().st_ino, original)
        self.assertTrue(ipc.query(endpoint, self.request(), 2)["ok"])

    def test_bad_frames_do_not_poison_next_connection(self):
        endpoint = self.start_server()
        for raw in (b"not json\n", b'{"id":1,"id":2}\n',
                    b"x" * (ipc.REQUEST_LIMIT + 1) + b"\n"):
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
                client.settimeout(2)
                client.connect(str(endpoint))
                client.sendall(raw)
                with __import__("contextlib").suppress(ConnectionResetError):
                    response = client.recv(ipc.RESPONSE_LIMIT)
                    if response:
                        self.assertFalse(json.loads(response)["ok"])
        self.assertTrue(ipc.query(endpoint, self.request(), 2)["ok"])

    def test_corrupted_lock_contents_are_not_an_admission_proof(self):
        endpoint = self.start_server()
        endpoint.with_name(endpoint.name + ".lock").write_text('{"verified":true}')
        self.assertTrue(ipc.query(endpoint, self.request(), 2)["ok"])
        self.assertEqual(len(self.opens()), 1)
        self.segment.write_bytes(b"bad")
        response = ipc.query(endpoint, self.request(), 2)
        self.assertFalse(response["ok"])
        self.assertEqual(len(self.opens()), 2)

    def test_sigterm_cleans_socket_and_child(self):
        endpoint = self.start_server()
        ipc.query(endpoint, self.request(), 2)
        child = int(self.opens()[0])
        self.server.terminate()
        self.server.communicate(timeout=5)
        self.assertFalse(endpoint.exists())
        with self.assertRaises(ProcessLookupError):
            os.kill(child, 0)


if __name__ == "__main__":
    unittest.main()
