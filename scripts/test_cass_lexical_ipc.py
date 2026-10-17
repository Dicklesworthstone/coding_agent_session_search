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
from unittest import mock

MODULE_PATH = Path(__file__).with_name("cass_lexical_ipc.py")
spec = importlib.util.spec_from_file_location("cass_lexical_ipc", MODULE_PATH)
ipc = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ipc)

FAKE = r"""#!/usr/bin/env python3
import hashlib, json, os, pathlib, sys, time
index = pathlib.Path(sys.argv[sys.argv.index('--index') + 1])
home = pathlib.Path(__file__).parent
mode = (home / 'mode').read_text() if (home / 'mode').exists() else 'normal'
if mode == 'root_probe':
    # Capture at process startup, while the test has redirected an ancestor.
    retained_bytes = (index / 'segment.qseg').read_bytes()
    retained_manifest = (index / 'quill.manifest.json').read_text()
    root_info = index.stat()
    inherited = []
    for name in os.listdir('/proc/self/fd'):
        try:
            os.fstat(int(name))
        except OSError:
            continue
        if int(name) > 2:
            inherited.append(int(name))
    root_probe = {'identity': [root_info.st_dev, root_info.st_ino],
                  'sha256': hashlib.sha256(retained_bytes).hexdigest(),
                  'inherited': sorted(inherited)}
    (home / 'root_ready').write_text(json.dumps(root_probe))
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
    if mode == 'drop_reader':
        loaded = False
    if not loaded:
        with (home / 'opens').open('a') as log:
            log.write(str(os.getpid()) + '\n')
        expected = json.loads(retained_manifest if mode == 'root_probe' else
                              (index / 'quill.manifest.json').read_text())
        actual = hashlib.sha256(retained_bytes if mode == 'root_probe' else
                                (index / 'segment.qseg').read_bytes()).hexdigest()
        if actual != expected['hash']:
            print(json.dumps(dict(schema_version=1, id=request['id'], ok=False,
                                  error={'kind':'corrupt', 'message':'full hash failed'})), flush=True)
            continue
    if mode == 'mutate':
        with (index / 'segment.qseg').open('r+b') as changed:
            changed.write(b'Z')
    result = {'hits':[{'content': 'alpha', 'score':1.0}], 'reader_reused':loaded}
    if mode == 'forged_reuse':
        result['reader_reused'] = True
    if mode == 'non_boolean_reuse':
        result['reader_reused'] = 1
    if mode == 'missing_reuse':
        del result['reader_reused']
    if mode == 'mixed_outcome':
        print(json.dumps(dict(schema_version=1, id=request['id'], ok=True,
                              result=result, error={'kind':'corrupt'})), flush=True)
        continue
    if mode == 'forged_admission':
        print(json.dumps(dict(schema_version=1, id=request['id'], ok=True,
                              result=result, admission={'mode':'strict_full'})), flush=True)
        continue
    if mode == 'root_probe':
        result['root_probe'] = root_probe
        result['current_root'] = [index.stat().st_dev, index.stat().st_ino]
    print(json.dumps(dict(schema_version=1, id=request['id'], ok=True, result=result)), flush=True)
    loaded = True
"""


@unittest.skipUnless(sys.platform.startswith("linux"), "Linux peer credentials required")
class AdmissionTests(unittest.TestCase):
    def setUp(self):
        # Positive reuse must exercise real supported storage, not silently skip
        # when CI's normal temporary directory lives on overlayfs. Keep the fake
        # executable outside tmpfs because /dev/shm is commonly mounted noexec.
        self.temp = tempfile.TemporaryDirectory(dir="/dev/shm")
        self.executable_temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.executable_temp.cleanup)
        self.home = Path(self.temp.name)
        self.index = self.home / "index"
        self.index.mkdir()
        self.segment = self.index / "segment.qseg"
        self.segment.write_bytes(b"alpha beta " * 1024)
        self.publish_manifest()
        self.cass = Path(self.executable_temp.name) / "fake-cass"
        fake = FAKE.replace("#!/usr/bin/env python3", "#!" + sys.executable + " -S")
        self.cass.write_text(fake.replace("home = pathlib.Path(__file__).parent",
                                         f"home = pathlib.Path({str(self.home)!r})"))
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

    def test_full_verify_rejects_a_fresh_worker_claiming_reuse(self):
        self.search()
        self.mode("forged_reuse")
        with self.assertRaises(ipc.Failure) as raised:
            self.search(full_verify=True)
        self.assertEqual(raised.exception.kind, "worker_protocol")
        self.assertEqual(len(self.opens()), 2)
        self.assertIsNone(self.owner.process)
        self.assertIsNone(self.owner.guard)

    def test_native_reader_reclamation_is_reported_as_strict_admission(self):
        self.mode("drop_reader")
        first = self.search()
        pid = self.owner.process.pid
        second = self.search()
        self.assertEqual(self.owner.process.pid, pid)
        self.assertFalse(second["result"]["reader_reused"])
        self.assertEqual(first["admission"]["mode"], "strict_full")
        self.assertEqual(second["admission"]["mode"], "strict_full")
        self.assertEqual(second["admission"]["owner_epoch"], 2)
        self.assertEqual(len(self.opens()), 2)

    def test_unsupported_filesystems_force_strict_readers(self):
        table = ipc.kernel_bytes("/proc/self/mountinfo", ipc.MOUNTINFO_LIMIT)
        for filesystem in (b"nfs", b"nfs4", b"cifs", b"fuse.sshfs", b"overlay", b"unknown"):
            with self.subTest(filesystem=filesystem):
                self.owner.close()
                previous = len(self.opens())
                records = ipc.mount_records(table)
                for key, record in tuple(records.items()):
                    records[key] = (*record[:2], filesystem, record[3])
                with mock.patch.object(ipc, "mount_records", return_value=records):
                    first, second = self.search(), self.search()
                self.assertEqual(first["admission"]["mode"], "strict_full")
                self.assertEqual(second["admission"]["mode"], "strict_full")
                self.assertFalse(second["result"]["reader_reused"])
                self.assertEqual(len(self.opens()), previous + 2)
                self.assertIsNone(self.owner.guard.local_mounts)

    def test_unknown_kernel_metadata_disables_reuse_without_breaking_search(self):
        for error in (PermissionError("proc denied"), FileNotFoundError("proc absent"),
                      ValueError("corrupt proof metadata")):
            with self.subTest(error=error):
                self.owner.close()
                before = len(self.opens())
                with mock.patch.object(ipc, "kernel_bytes", side_effect=error):
                    first, second = self.search(), self.search()
                self.assertTrue(first["ok"] and second["ok"])
                self.assertEqual(second["admission"]["mode"], "strict_full")
                self.assertEqual(len(self.opens()), before + 2)

    def test_unsupported_nested_mount_and_device_mismatch_cannot_reuse(self):
        self.search()
        guard = self.owner.guard
        self.assertTrue(guard.reusable())
        real_mount = ipc.descriptor_mount
        segment_inode = self.segment.stat().st_ino
        table = ipc.mount_records(ipc.kernel_bytes("/proc/self/mountinfo", ipc.MOUNTINFO_LIMIT))
        new_id = max(table) + 1
        segment_device = self.segment.stat().st_dev
        record = (os.major(segment_device), os.minor(segment_device), b"nfs", b"nested")
        table[new_id] = record

        def mount_for(fd):
            return new_id if os.fstat(fd).st_ino == segment_inode else real_mount(fd)

        with mock.patch.object(ipc, "mount_records", return_value=table), \
                mock.patch.object(ipc, "descriptor_mount", side_effect=mount_for):
            self.assertIsNone(ipc.LocalMounts.capture(guard.nodes))
            table[new_id] = (record[0] + 1, record[1], b"tmpfs", b"wrong-device")
            self.assertIsNone(ipc.LocalMounts.capture(guard.nodes))
            del table[new_id]
            self.assertIsNone(ipc.LocalMounts.capture(guard.nodes))
        self.assertTrue(guard.reusable())

    def test_namespace_and_mount_changes_invalidate_warm_admission(self):
        self.search()
        namespace = self.owner.guard.local_mounts.namespace
        with mock.patch.object(ipc, "namespace_identity", return_value=(namespace[0], namespace[1] + 1)):
            changed = self.search()
        self.assertEqual(changed["admission"]["mode"], "strict_full")
        self.assertIsNone(self.owner.guard.local_mounts)
        self.search()
        self.assertTrue(self.owner.guard.reusable())
        table = ipc.mount_records(ipc.kernel_bytes("/proc/self/mountinfo", ipc.MOUNTINFO_LIMIT))
        watched = next(iter(self.owner.guard.local_mounts.records))
        record = table[watched]
        table[watched] = (*record[:3], record[3] + b" changed-options")
        with mock.patch.object(ipc, "mount_records", return_value=table):
            changed = self.search()
        self.assertEqual(changed["admission"]["mode"], "strict_full")
        self.assertEqual(len(self.opens()), 4)

    def test_unrelated_mount_record_changes_do_not_evict_reader(self):
        self.search()
        table = ipc.mount_records(ipc.kernel_bytes("/proc/self/mountinfo", ipc.MOUNTINFO_LIMIT))
        unrelated = max(table) + 1
        table[unrelated] = (0, 99, b"tmpfs", b"unrelated mount")
        with mock.patch.object(ipc, "mount_records", return_value=table):
            response = self.search()
        self.assertEqual(response["admission"]["mode"], "retained_guarded")
        self.assertEqual(len(self.opens()), 1)

    def test_reuse_eligibility_loss_during_query_discards_result(self):
        self.search()
        with mock.patch.object(ipc.LocalMounts, "unchanged", side_effect=[True, False]):
            with self.assertRaises(ipc.Failure) as raised:
                self.search()
        self.assertEqual(raised.exception.kind, "index_changed")
        self.assertIsNone(self.owner.process)
        self.assertIsNone(self.owner.guard)

    def test_ancestor_aba_cannot_redirect_native_admission(self):
        # Moving the ancestor away and back does not change the guarded root's
        # metadata. Pathname checks alone therefore cannot catch this ABA.
        live = self.home / "live"
        alternate = self.home / "alternate"
        saved = self.home / "saved"
        live.mkdir()
        alternate.mkdir()
        self.index.rename(live / "index")
        self.index = live / "index"
        self.segment = self.index / "segment.qseg"
        other = alternate / "index"
        other.mkdir()
        other_bytes = b"different but valid published index"
        (other / "segment.qseg").write_bytes(other_bytes)
        (other / "quill.manifest.json").write_text(json.dumps({
            "hash": hashlib.sha256(other_bytes).hexdigest()}))
        expected = ipc.stamp(self.index.stat())
        expected_hash = hashlib.sha256(self.segment.read_bytes()).hexdigest()
        self.owner = ipc.Owner(str(self.cass), self.index, 2)
        self.mode("root_probe")
        real_popen = subprocess.Popen

        def redirected_spawn(*args, **kwargs):
            live.rename(saved)
            alternate.rename(live)
            process = None
            try:
                process = real_popen(*args, **kwargs)
                deadline = time.monotonic() + 2
                while not (self.home / "root_ready").exists():
                    if process.poll() is not None or time.monotonic() >= deadline:
                        self.fail("worker did not sample the redirected root")
                    time.sleep(0.005)
                return process
            except BaseException:
                if process is not None:
                    process.kill()
                    process.communicate(timeout=2)
                raise
            finally:
                live.rename(alternate)
                saved.rename(live)

        with mock.patch.object(ipc.subprocess, "Popen", side_effect=redirected_spawn):
            first = self.search()
        self.assertEqual(ipc.stamp(self.index.stat()), expected)
        self.assertTrue(self.owner.guard.unchanged())
        self.assertEqual(first["result"]["root_probe"]["identity"], list(expected[:2]))
        self.assertEqual(first["result"]["root_probe"]["sha256"], expected_hash)
        second = self.search()
        self.assertEqual(second["admission"]["mode"], "retained_guarded")
        self.assertEqual(second["result"]["root_probe"], first["result"]["root_probe"])
        self.assertEqual(len(self.opens()), 1)

    def test_worker_retains_only_the_guarded_root_descriptor(self):
        self.mode("root_probe")
        unrelated = os.open(self.cass, os.O_RDONLY)
        os.set_inheritable(unrelated, True)
        try:
            first, second = self.search(), self.search()
        finally:
            os.close(unrelated)
        root_fd = self.owner.guard.nodes[0][0]
        identity = list(ipc.stamp(self.index.stat())[:2])
        self.assertEqual(first["result"]["root_probe"]["inherited"], [root_fd])
        self.assertEqual(second["result"]["current_root"], identity)
        self.assertEqual(len(self.opens()), 1)

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
        for mode in ("bad_json", "duplicate", "oversized", "wrong_id", "bad_schema", "exit",
                     "forged_reuse", "non_boolean_reuse", "missing_reuse", "mixed_outcome",
                     "forged_admission"):
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


class MetadataAndProtocolTests(unittest.TestCase):
    RECORD = b"17 1 0:42 / /index rw,nosuid shared:2 future-tag:x - tmpfs none rw\n"

    def test_mount_records_validate_binding_and_ignore_optional_fields(self):
        records = ipc.mount_records(self.RECORD)
        self.assertEqual(records[17], (0, 42, b"tmpfs", self.RECORD.rstrip(b"\n")))
        # Escaped whitespace in a mount path does not affect descriptor matching.
        self.assertEqual(ipc.mount_records(self.RECORD.replace(b"/index", b"/a\\040b"))[17][:3],
                         (0, 42, b"tmpfs"))

    def test_corrupt_truncated_oversized_and_duplicate_mount_state_is_refused(self):
        for bad in (b"", self.RECORD[:-1], self.RECORD + self.RECORD,
                    self.RECORD.replace(b"17 1", b"-1 1"),
                    self.RECORD.replace(b"17 1", b"9" * 21 + b" 1"),
                    self.RECORD.replace(b"0:42", b"0:bad"),
                    self.RECORD.replace(b" - ", b" "),
                    self.RECORD.replace(b" - ", b" - - "),
                    self.RECORD.replace(b"/index", b"relative"),
                    b"x" * (ipc.MOUNTINFO_LIMIT + 1)):
            with self.subTest(bad=bad[:80]), self.assertRaises(ValueError):
                ipc.mount_records(bad)
        with mock.patch.object(ipc, "MAX_MOUNTS", 1), self.assertRaises(ValueError):
            ipc.mount_records(self.RECORD + self.RECORD.replace(b"17 1", b"18 1"))

    def test_descriptor_mount_requires_one_bounded_kernel_id(self):
        for bad in (b"pos:\t0\n", b"mnt_id: 1\nmnt_id: 1\n", b"mnt_id: -1\n",
                    b"mnt_id: nonsense\n", b"mnt_id: " + b"9" * 21 + b"\n"):
            with mock.patch.object(ipc, "kernel_bytes", return_value=bad), self.assertRaises(ValueError):
                ipc.descriptor_mount(7)
        with mock.patch.object(ipc, "kernel_bytes", return_value=b"pos: 0\nmnt_id:\t17\n") as read:
            self.assertEqual(ipc.descriptor_mount(7), 17)
            read.assert_called_once_with("/proc/self/fdinfo/7", ipc.FDINFO_LIMIT)

    def test_kernel_reads_are_bounded_and_require_complete_frames(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "kernel-fixture"
            for bad in (b"", b"abc", b"abcde\n"):
                path.write_bytes(bad)
                with self.assertRaises(ValueError):
                    ipc.kernel_bytes(path, 4)
            path.write_bytes(b"abc\n")
            self.assertEqual(ipc.kernel_bytes(path, 4), b"abc\n")

    @staticmethod
    def response(reused=False, full=False):
        return {"schema_version": 1, "id": 1, "ok": True,
                "result": {"hits": [], "reader_reused": reused},
                "admission": {"mode": "retained_guarded" if reused else "strict_full",
                              "owner_epoch": 1, "full_verify_requested": full,
                              "persistent_proof": False, "file_identity_checked": True,
                              "immutable_generation_certified": False}}

    def test_client_accepts_only_the_requested_admission_contract(self):
        for reused, full in ((False, False), (False, True), (True, False)):
            ipc.validate_response(self.response(reused, full), 1, full)
        for key, value in (("mode", "unchecked"), ("owner_epoch", True), ("owner_epoch", 0),
                           ("owner_epoch", 2**64), ("full_verify_requested", 0),
                           ("persistent_proof", 0), ("persistent_proof", True),
                           ("file_identity_checked", False), ("immutable_generation_certified", True)):
            bad = self.response()
            bad["admission"][key] = value
            with self.subTest(key=key, value=value), self.assertRaises(ipc.Failure):
                ipc.validate_response(bad, 1, False)
        with self.assertRaises(ipc.Failure):
            ipc.validate_response(self.response(True, True), 1, True)
        bad = self.response()
        bad["admission"]["unrecognized_proof"] = True
        with self.assertRaises(ipc.Failure):
            ipc.validate_response(bad, 1, False)

    def test_mixed_error_and_success_proof_envelopes_are_rejected(self):
        error = {"schema_version": 1, "id": 1, "ok": False, "error": {"kind": "corrupt"}}
        ipc.validate_response(error, 1, True)
        for extra in ({"admission": self.response()["admission"]}, {"result": {}}, {"unknown": 1}):
            with self.assertRaises(ipc.Failure):
                ipc.validate_response({**error, **extra}, 1, False)
        good = self.response()
        with self.assertRaises(ipc.Failure):
            ipc.validate_response({**good, "error": {"kind": "corrupt"}}, 1, False)
        with self.assertRaises(ipc.Failure):
            ipc.validate_response(good, 1)  # The native worker cannot mint the owner's proof.

    def test_python_client_validates_proof_on_the_actual_socket_path(self):
        # Socket transport is isolated here; the real client function, envelope
        # validation and full-verify request all execute without a helper bypass.
        request = {"op": "search", "id": 1, "query": "alpha", "full_verify": True}
        with tempfile.TemporaryDirectory() as directory:
            endpoint = Path(directory) / "owner.sock"
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as listener:
                listener.bind(str(endpoint))
                endpoint.chmod(0o600)
                with mock.patch.object(ipc.socket, "socket"), mock.patch.object(ipc, "check_peer"), \
                        mock.patch.object(ipc, "receive", return_value=self.response(True, True)):
                    with self.assertRaises(ipc.Failure):
                        ipc.query(endpoint, request, 1)


if __name__ == "__main__":
    unittest.main()
