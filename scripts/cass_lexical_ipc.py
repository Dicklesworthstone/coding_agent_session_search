#!/usr/bin/env python3
"""Opt-in Linux IPC for fresh lexical clients backed by one strict CASS reader.

No persistent verification receipt and no alternate search engine. The owner is
foreground-only. Index files must follow Quill's immutable-publication contract;
identity guards detect ordinary filesystem changes, not metadata-invisible
writes or media faults. --full-verify discards the reader before each admission.
Reuse requires every guarded descriptor to belong to a supported local Linux
filesystem. Unknown, network, FUSE and overlay storage uses fresh strict readers.
"""
from __future__ import annotations

import argparse
import contextlib
import fcntl
import json
import os
from pathlib import Path
import selectors
import signal
import shutil
import socket
import stat
import struct
import subprocess
import sys
import time

REQUEST_LIMIT = 64 * 1024
RESPONSE_LIMIT = 1024 * 1024
MAX_FILES = 4096
MAX_DEPTH = 16
MOUNTINFO_LIMIT = 4 * 1024 * 1024
FDINFO_LIMIT = 16 * 1024
MAX_MOUNTS = 16384
LOCAL_FILESYSTEMS = frozenset((b"ext2", b"ext3", b"ext4", b"xfs", b"btrfs",
                               b"f2fs", b"tmpfs"))


class Failure(Exception):
    def __init__(self, kind, message, retryable=False):
        super().__init__(message)
        self.kind, self.retryable = kind, retryable


def directory_names(fd):
    names = []
    with os.scandir(fd) as entries:
        for entry in entries:
            if len(names) >= MAX_FILES:
                raise Failure("index_layout", "Index directory exceeds its entry limit.")
            names.append(entry.name)
    return tuple(sorted(names))


def stamp(info):
    return (info.st_dev, info.st_ino, info.st_mode, info.st_uid, info.st_gid,
            info.st_nlink, info.st_size, info.st_mtime_ns, info.st_ctime_ns)


def open_directory(path):
    """Walk without symlinks, retaining the final directory descriptor."""
    path = Path(os.path.abspath(path))
    fd = os.open("/", os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
    try:
        for part in path.parts[1:]:
            next_fd = os.open(part, os.O_RDONLY | os.O_DIRECTORY |
                              os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=fd)
            os.close(fd)
            fd = next_fd
        return fd
    except BaseException:
        os.close(fd)
        raise


def kernel_bytes(path, limit):
    """Bound advisory procfs metadata before parsing; failure disables reuse."""
    with open(path, "rb") as source:
        data = source.read(limit + 1)
    if not data or len(data) > limit or not data.endswith(b"\n"):
        raise ValueError("unavailable or oversized kernel metadata")
    return data


def kernel_number(word):
    if not word.isdigit() or len(word) > 20:
        raise ValueError("invalid kernel identifier")
    return int(word)


def mount_records(data):
    """Parse mount IDs, never infer a filesystem from a pathname prefix.

    Linux documents fdinfo's mnt_id as the mountinfo ID of the opened file.
    IDs are namespace-local and reusable after unmount; callers retain both
    the namespace descriptor and the exact records, not just those integers.
    Unknown optional fields before the separator are intentionally ignored.
    """
    if not data or len(data) > MOUNTINFO_LIMIT or not data.endswith(b"\n"):
        raise ValueError("invalid mount table size or framing")
    records = {}
    for line in data.splitlines():
        fields = line.split(b" - ")
        if len(fields) != 2 or len(records) >= MAX_MOUNTS:
            raise ValueError("invalid or oversized mount table")
        before, after = fields[0].split(), fields[1].split()
        if len(before) < 6 or len(after) != 3:
            raise ValueError("incomplete mount record")
        mount_id = kernel_number(before[0])
        kernel_number(before[1])
        device = before[2].split(b":")
        if len(device) != 2 or not mount_id or mount_id in records:
            raise ValueError("duplicate or invalid mount identifier")
        major, minor = map(kernel_number, device)
        if not before[3].startswith(b"/") or not before[4].startswith(b"/"):
            raise ValueError("invalid mount root")
        records[mount_id] = (major, minor, after[0], line)
    return records


def descriptor_mount(fd):
    data = kernel_bytes(f"/proc/self/fdinfo/{fd}", FDINFO_LIMIT)
    values = [line.partition(b":")[2].strip() for line in data.splitlines()
              if line.partition(b":")[0] == b"mnt_id"]
    if len(values) != 1:
        raise ValueError("missing or ambiguous descriptor mount")
    return kernel_number(values[0])


def namespace_identity():
    info = os.stat("/proc/self/ns/mnt")
    return info.st_dev, info.st_ino


class LocalMounts:
    """Ephemeral eligibility only: no byte witness and no persistent proof."""
    def __init__(self, nodes):
        self.namespace_fd = None
        self.records = {}
        try:
            self.namespace_fd = os.open("/proc/self/ns/mnt", os.O_RDONLY | os.O_CLOEXEC)
            info = os.fstat(self.namespace_fd)
            self.namespace = (info.st_dev, info.st_ino)
            table = mount_records(kernel_bytes("/proc/self/mountinfo", MOUNTINFO_LIMIT))
            for fd, _parent, _name, identity, _names in nodes:
                mount_id = descriptor_mount(fd)
                record = table.get(mount_id)
                device = (os.major(identity[0]), os.minor(identity[0]))
                if (record is None or record[:2] != device or
                        record[2] not in LOCAL_FILESYSTEMS):
                    raise ValueError("descriptor is not on supported local storage")
                self.records[mount_id] = record
            if not self.unchanged():
                raise ValueError("mount eligibility changed during capture")
        except BaseException:
            self.close()
            raise

    @classmethod
    def capture(cls, nodes):
        try:
            return cls(nodes)
        except (OSError, ValueError):
            # Advisory admission failure must not prevent an ordinary strict
            # search. In particular, procfs can be absent or permission-limited.
            return None

    def unchanged(self):
        if self.namespace_fd is None or not self.records:
            return False
        try:
            if namespace_identity() != self.namespace:
                return False
            current = mount_records(kernel_bytes("/proc/self/mountinfo", MOUNTINFO_LIMIT))
            return (all(current.get(key) == value for key, value in self.records.items())
                    and namespace_identity() == self.namespace)
        except (OSError, ValueError):
            return False

    def close(self):
        if self.namespace_fd is not None:
            os.close(self.namespace_fd)
            self.namespace_fd = None


class Guard:
    """Pin every inode, and check both the handles and their directory names."""
    def __init__(self, path):
        self.path = os.path.abspath(path)
        self.nodes = []
        self.local_mounts = None
        try:
            fd = open_directory(self.path)
            self._capture(fd, None, None, 0)
            self.local_mounts = LocalMounts.capture(self.nodes)
            if not self.unchanged():
                raise Failure("index_changed", "Index changed during admission.", True)
        except BaseException:
            self.close()
            raise

    def _capture(self, fd, parent, name, depth):
        # Register ownership immediately, including on any subsequent failure.
        node = [fd, parent, name, stamp(os.fstat(fd)), None]
        self.nodes.append(node)
        if len(self.nodes) > MAX_FILES or depth > MAX_DEPTH:
            raise Failure("index_layout", "Index exceeds guarded file/depth limits.")
        if stat.S_ISDIR(node[3][2]):
            names = directory_names(fd)
            node[4] = names
            for child in names:
                child_fd = os.open(child, os.O_RDONLY | os.O_NOFOLLOW |
                                   os.O_NONBLOCK | os.O_CLOEXEC, dir_fd=fd)
                self._capture(child_fd, fd, child, depth + 1)
        elif not stat.S_ISREG(node[3][2]):
            raise Failure("index_layout", "Index contains a non-regular file.")

    def unchanged(self):
        if not self.nodes:
            return False
        try:
            current = open_directory(self.path)
            try:
                if stamp(os.fstat(current)) != self.nodes[0][3]:
                    return False
            finally:
                os.close(current)
            for fd, parent, name, identity, names in self.nodes:
                if stamp(os.fstat(fd)) != identity:
                    return False
                if parent is not None and stamp(os.stat(
                        name, dir_fd=parent, follow_symlinks=False)) != identity:
                    return False
                if names is not None and directory_names(fd) != names:
                    return False
            return True
        except (OSError, Failure):
            return False

    def reusable(self):
        return (self.local_mounts is not None and self.local_mounts.unchanged()
                and self.unchanged())

    def close(self):
        if self.local_mounts is not None:
            self.local_mounts.close()
            self.local_mounts = None
        for node in reversed(self.nodes):
            os.close(node[0])
        self.nodes.clear()


def encode(value, limit):
    try:
        data = json.dumps(value, ensure_ascii=True, allow_nan=False,
                          separators=(",", ":")).encode() + b"\n"
    except (ValueError, RecursionError) as error:
        raise Failure("invalid_json", "Cannot encode protocol frame.") from error
    if len(data) > limit:
        raise Failure("frame_too_large", "Protocol frame exceeds its byte limit.")
    return data


def decode(data):
    try:
        def unique(pairs):
            value = {}
            for key, item in pairs:
                if key in value:
                    raise ValueError("duplicate key")
                value[key] = item
            return value
        def invalid_constant(_):
            raise ValueError("non-finite number")
        value = json.loads(data, object_pairs_hook=unique,
                           parse_constant=invalid_constant)
    except (ValueError, UnicodeError, RecursionError) as error:
        raise Failure("invalid_json", "Invalid JSON protocol frame.") from error
    if not isinstance(value, dict):
        raise Failure("invalid_request", "Protocol frame must be an object.")
    return value


def remaining(deadline):
    budget = deadline - time.monotonic()
    if budget <= 0:
        raise Failure("timeout", "Lexical request deadline expired.", True)
    return budget


def validate_response(response, request_id, full_verify=None):
    """Validate native lifecycle or the owner's stronger client envelope.

    A process being alive is not evidence that its native reader was reused.
    The caller must reconcile the native boolean with the process it started;
    clients additionally require an admission policy matching their request.
    """
    allowed = {"schema_version", "id", "ok", "result", "error"}
    if full_verify is not None:
        allowed.add("admission")
    if (set(response) - allowed or type(response.get("id")) is not int or
            response["id"] != request_id or type(response.get("ok")) is not bool or
            type(response.get("schema_version")) is not int or response["schema_version"] != 1):
        raise Failure("worker_protocol", "Mismatched response envelope.")
    if not response["ok"]:
        if (not isinstance(response.get("error"), dict) or response.get("result") is not None
                or response.get("admission") is not None):
            raise Failure("worker_protocol", "Malformed error response.")
        return
    result = response.get("result")
    if (not isinstance(result, dict) or response.get("error") is not None
            or type(result.get("reader_reused")) is not bool):
        raise Failure("worker_protocol", "Missing or contradictory native lifecycle.")
    if full_verify is None:
        return
    admission = response.get("admission")
    fields = {"mode", "owner_epoch", "full_verify_requested", "persistent_proof",
              "file_identity_checked", "immutable_generation_certified"}
    if (not isinstance(admission, dict) or set(admission) != fields or
            type(admission["owner_epoch"]) is not int or
            not 0 < admission["owner_epoch"] < 2**64 or
            admission["full_verify_requested"] is not full_verify or
            admission["persistent_proof"] is not False or
            admission["file_identity_checked"] is not True or
            admission["immutable_generation_certified"] is not False):
        raise Failure("worker_protocol", "Invalid admission contract.")
    expected = "retained_guarded" if result["reader_reused"] else "strict_full"
    if admission["mode"] != expected or (full_verify and result["reader_reused"]):
        raise Failure("worker_protocol", "Owner did not honor verification policy.")


def exchange_pipes(process, request, deadline):
    """Bound the complete write/read, not just one blocking read operation."""
    data = memoryview(encode(request, REQUEST_LIMIT))
    received = bytearray()
    with selectors.DefaultSelector() as selector:
        selector.register(process.stdin, selectors.EVENT_WRITE)
        selector.register(process.stdout, selectors.EVENT_READ)
        while True:
            events = selector.select(remaining(deadline))
            for key, _ in events:
                if key.fileobj is process.stdin:
                    try:
                        count = os.write(process.stdin.fileno(), data)
                    except BlockingIOError:
                        continue
                    if count == 0:
                        raise Failure("worker_io", "Worker closed its input.", True)
                    data = data[count:]
                    if not data:
                        selector.unregister(process.stdin)
                else:
                    try:
                        chunk = os.read(process.stdout.fileno(), 65536)
                    except BlockingIOError:
                        continue
                    if not chunk:
                        raise Failure("worker_exit", "Lexical worker exited.", True)
                    received.extend(chunk)
                    if len(received) > RESPONSE_LIMIT:
                        raise Failure("worker_protocol", "Oversized worker response.")
                    if b"\n" in received:
                        line, tail = received.split(b"\n", 1)
                        if tail or data:
                            raise Failure("worker_protocol", "Unexpected worker output.")
                        response = decode(line)
                        validate_response(response, request["id"])
                        return response


def validate(request):
    allowed = {"op", "id", "query", "limit", "offset", "filters", "full_verify"}
    if set(request) - allowed:
        raise Failure("invalid_request", "Unsupported request field.")
    if type(request.get("id")) is not int or not 0 <= request["id"] < 2**64:
        raise Failure("invalid_request", "id must be an unsigned 64-bit integer.")
    if request.get("op") != "search":
        raise Failure("invalid_request", "Only lexical search is supported.")
    query = request.get("query")
    if not isinstance(query, str) or not query.strip():
        raise Failure("invalid_request", "A nonempty query is required.")
    try:
        valid_query = len(query.encode("utf-8")) <= 4096
    except UnicodeError:
        valid_query = False
    limit, offset = request.get("limit", 10), request.get("offset", 0)
    if (not valid_query or type(limit) is not int or not 1 <= limit <= 100 or
            type(offset) is not int or offset < 0 or offset + limit + 1 > 1024):
        raise Failure("invalid_request", "Invalid query or pagination budget.")
    if type(request.get("full_verify", False)) is not bool:
        raise Failure("invalid_request", "full_verify must be a boolean.")
    if "filters" in request and not isinstance(request["filters"], dict):
        raise Failure("invalid_request", "filters must be an object.")


class Owner:
    def __init__(self, cass, index, timeout=30.0):
        self.cass, self.index, self.timeout = cass, os.path.abspath(index), timeout
        self.process = self.guard = None
        self.epoch = 0
        self.sequence = 0

    def close(self):
        process, self.process = self.process, None
        if process is not None:
            try:
                if process.poll() is None:
                    process.kill()
                process.wait(timeout=5)
            finally:
                process.stdin.close()
                process.stdout.close()
        if self.guard is not None:
            self.guard.close()
            self.guard = None

    def search(self, request, deadline=None):
        validate(request)
        deadline = min(deadline or float("inf"), time.monotonic() + self.timeout)
        full = request.get("full_verify", False)
        reused = (not full and self.process is not None and
                  self.process.poll() is None and self.guard.reusable())
        try:
            if not reused:
                self.close()
                self.guard = Guard(self.index)
                remaining(deadline)
                # Open exactly the directory captured by the guard, not a
                # pathname an ancestor rename could temporarily redirect.
                # Only this root capability crosses exec; the guard's other
                # descriptors and the owner's socket/lock remain private.
                root_fd = self.guard.nodes[0][0]
                self.process = subprocess.Popen(
                    [self.cass, "serve", "--stdio", "--index",
                     f"/proc/self/fd/{root_fd}",
                     "--request-timeout-ms", str(int(self.timeout * 1000))],
                    stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                    stderr=None, close_fds=True, pass_fds=(root_fd,))
                os.set_blocking(self.process.stdin.fileno(), False)
                os.set_blocking(self.process.stdout.fileno(), False)
            self.sequence += 1
            native_request = {key: value for key, value in request.items()
                              if key != "full_verify"}
            native_request["id"] = self.sequence
            response = exchange_pipes(self.process, native_request, deadline)
            response["id"] = request["id"]
            remaining(deadline)
            if not self.guard.unchanged():
                raise Failure("index_changed", "Index changed during the query.", True)
            if reused and (self.guard.local_mounts is None or
                           not self.guard.local_mounts.unchanged()):
                raise Failure("index_changed", "Local reuse eligibility changed during the query.", True)
            if not response["ok"]:
                self.close()
                return response
            native_reused = response["result"]["reader_reused"]
            if native_reused and not reused:
                raise Failure("worker_protocol", "Fresh worker claimed an existing reader.")
            # The native service may reclaim its own reader. Report its actual
            # strict re-admission rather than claiming a hit because the process
            # survived, and never accept reuse from a newly spawned worker.
            if not native_reused:
                self.epoch += 1
            response["admission"] = {
                "mode": "retained_guarded" if native_reused else "strict_full",
                "owner_epoch": self.epoch, "full_verify_requested": full,
                "persistent_proof": False, "file_identity_checked": True,
                "immutable_generation_certified": False,
            }
            # Validate the augmented envelope before allowing any output.
            validate_response(response, request["id"], full)
            encode(response, RESPONSE_LIMIT)
            return response
        except BaseException:
            self.close()
            raise


def private_endpoint(path):
    path = Path(os.path.abspath(path))
    fd = open_directory(path.parent)
    info = os.fstat(fd)
    if info.st_uid != os.geteuid() or info.st_mode & 0o077:
        os.close(fd)
        raise Failure("unsafe_socket", "Socket parent must be owned by you and mode 0700.")
    # Linux procfs gives bind/connect/unlink one pinned directory, even if an
    # ancestor is renamed. No directory is created and no symlink is followed.
    return fd, path.name, f"/proc/self/fd/{fd}/{path.name}"


def check_peer(stream):
    credentials = stream.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12)
    _, uid, _ = struct.unpack("3i", credentials)
    if uid != os.geteuid():
        raise Failure("unsafe_peer", "Socket peer does not have the same user ID.")


def receive(stream, limit, deadline):
    data = bytearray()
    while True:
        stream.settimeout(remaining(deadline))
        chunk = stream.recv(min(65536, limit + 1 - len(data)))
        if not chunk:
            raise Failure("truncated_frame", "Connection closed before a complete frame.")
        data.extend(chunk)
        if len(data) > limit:
            raise Failure("frame_too_large", "Protocol frame exceeds its byte limit.")
        if b"\n" in data:
            line, tail = data.split(b"\n", 1)
            if tail:
                raise Failure("invalid_request", "Only one frame per connection is allowed.")
            return decode(line)


def error_response(request, error):
    return {"schema_version": 1,
            "id": request.get("id") if type(request.get("id")) is int else None,
            "ok": False, "error": {"kind": error.kind, "message": str(error),
                                  "retryable": error.retryable}}


def serve(owner, endpoint, idle_seconds):
    directory, name, address = private_endpoint(endpoint)
    lock_fd = None
    bound = None
    try:
        lock_fd = os.open(name + ".lock", os.O_CREAT | os.O_RDWR |
                          os.O_NOFOLLOW | os.O_CLOEXEC, 0o600, dir_fd=directory)
        lock = os.fstat(lock_fd)
        if (not stat.S_ISREG(lock.st_mode) or lock.st_uid != os.geteuid() or
                lock.st_mode & 0o077 or lock.st_nlink != 1):
            raise Failure("unsafe_socket", "Unsafe worker lock file.")
        fcntl.flock(lock_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as listener:
            # Never delete an existing endpoint, even when it appears stale.
            listener.bind(address)
            bound = os.stat(name, dir_fd=directory, follow_symlinks=False)
            os.chmod(name, 0o600, dir_fd=directory, follow_symlinks=False)
            listener.listen(8)
            listener.settimeout(idle_seconds)
            print(f"Strict lexical owner listening at {endpoint}", file=sys.stderr)
            while True:
                try:
                    connection, _ = listener.accept()
                except socket.timeout:
                    return
                with connection:
                    request = {}
                    deadline = time.monotonic() + owner.timeout
                    try:
                        check_peer(connection)
                        request = receive(connection, REQUEST_LIMIT, deadline)
                        response = owner.search(request, deadline)
                    except Failure as error:
                        response = error_response(request, error)
                    except (OSError, subprocess.SubprocessError):
                        owner.close()
                        response = error_response(request, Failure(
                            "worker_io", "Lexical worker or filesystem operation failed.", True))
                    try:
                        connection.settimeout(remaining(deadline))
                        connection.sendall(encode(response, RESPONSE_LIMIT))
                    except (OSError, Failure):
                        # The next client cannot inherit framing or output state.
                        continue
    finally:
        owner.close()
        if bound is not None:
            with contextlib.suppress(OSError):
                current = os.stat(name, dir_fd=directory, follow_symlinks=False)
                if (current.st_dev, current.st_ino) == (bound.st_dev, bound.st_ino):
                    os.unlink(name, dir_fd=directory)
        if lock_fd is not None:
            os.close(lock_fd)
        os.close(directory)


def query(endpoint, request, timeout):
    validate(request)
    directory, name, address = private_endpoint(endpoint)
    try:
        info = os.stat(name, dir_fd=directory, follow_symlinks=False)
        if (not stat.S_ISSOCK(info.st_mode) or info.st_uid != os.geteuid() or
                info.st_mode & 0o077):
            raise Failure("unsafe_socket", "Unsafe lexical worker endpoint.")
        deadline = time.monotonic() + timeout
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as stream:
            stream.settimeout(remaining(deadline))
            stream.connect(address)
            check_peer(stream)
            stream.sendall(encode(request, REQUEST_LIMIT))
            response = receive(stream, RESPONSE_LIMIT, deadline)
            validate_response(response, request["id"], request.get("full_verify", False))
            return response
    finally:
        os.close(directory)


def filters_argument(text):
    try:
        return decode(text)
    except Failure as error:
        raise argparse.ArgumentTypeError(str(error)) from error


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    server = commands.add_parser("serve", help="Run a foreground lexical owner.")
    server.add_argument("--socket", required=True)
    server.add_argument("--index", required=True)
    server.add_argument("--cass", default="cass")
    server.add_argument("--idle-seconds", type=float, default=300)
    server.add_argument("--timeout", type=float, default=30)
    client = commands.add_parser("search", help="Run a fresh lexical client.")
    client.add_argument("query")
    location = client.add_mutually_exclusive_group(required=True)
    location.add_argument("--socket")
    location.add_argument("--index", help="Bypass IPC; fully verify with a fresh CASS worker.")
    client.add_argument("--cass", default="cass")
    client.add_argument("--limit", type=int, default=10)
    client.add_argument("--offset", type=int, default=0)
    client.add_argument("--filters", type=filters_argument, default={})
    client.add_argument("--full-verify", action="store_true")
    client.add_argument("--timeout", type=float, default=30)
    args = parser.parse_args(argv)
    if not sys.platform.startswith("linux") or not hasattr(socket, "SO_PEERCRED"):
        parser.error("This guarded IPC implementation requires Linux and procfs.")
    if not 0.001 <= args.timeout <= 300:
        parser.error("--timeout must be between 0.001 and 300 seconds.")
    if args.command == "serve" and not 0 < args.idle_seconds <= 86400:
        parser.error("--idle-seconds must be between 0 and 86400.")
    request = {}
    try:
        if args.command == "search":
            request = {"op": "search", "id": 1, "query": args.query,
                       "limit": args.limit, "offset": args.offset,
                       "filters": args.filters, "full_verify": args.full_verify}
            if args.socket:
                response = query(args.socket, request, args.timeout)
            else:
                cass = shutil.which(args.cass)
                if not cass:
                    raise Failure("binary_not_found", "CASS executable not found.")
                owner = Owner(os.path.abspath(cass), args.index, args.timeout)
                request["full_verify"] = True
                try:
                    response = owner.search(request)
                finally:
                    owner.close()
            sys.stdout.buffer.write(encode(response, RESPONSE_LIMIT))
            return 0 if response["ok"] else 1
        cass = shutil.which(args.cass)
        if not cass:
            raise Failure("binary_not_found", "CASS executable not found.")
        def terminate(_signum, _frame):
            raise KeyboardInterrupt
        previous = signal.signal(signal.SIGTERM, terminate)
        try:
            serve(Owner(os.path.abspath(cass), args.index, args.timeout),
                  args.socket, args.idle_seconds)
        finally:
            signal.signal(signal.SIGTERM, previous)
        return 0
    except (Failure, OSError, subprocess.SubprocessError) as error:
        if not isinstance(error, Failure):
            error = Failure("worker_io", "Lexical IPC operation failed.", True)
        print(json.dumps(error_response(request, error)), file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        return 130


if __name__ == "__main__":
    raise SystemExit(main())
