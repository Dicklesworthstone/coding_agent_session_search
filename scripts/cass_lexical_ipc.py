#!/usr/bin/env python3
"""Opt-in Linux IPC for fresh lexical clients backed by one strict CASS reader.

No persistent verification receipt and no alternate search engine. The owner is
foreground-only. Index files must follow Quill's immutable-publication contract;
identity guards detect ordinary filesystem changes, not metadata-invisible
writes or media faults. --full-verify discards the reader before each admission.
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


class Guard:
    """Pin every inode, and check both the handles and their directory names."""
    def __init__(self, path):
        self.path = os.path.abspath(path)
        self.nodes = []
        try:
            fd = open_directory(self.path)
            self._capture(fd, None, None, 0)
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

    def close(self):
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
                        if (type(response.get("id")) is not int or
                                response["id"] != request["id"] or
                                type(response.get("ok")) is not bool or
                                type(response.get("schema_version")) is not int or
                                response["schema_version"] != 1):
                            raise Failure("worker_protocol", "Mismatched worker envelope.")
                        if response["ok"] and not isinstance(response.get("result"), dict):
                            raise Failure("worker_protocol", "Missing worker result.")
                        if not response["ok"] and not isinstance(response.get("error"), dict):
                            raise Failure("worker_protocol", "Missing worker error.")
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
                  self.process.poll() is None and self.guard.unchanged())
        try:
            if not reused:
                self.close()
                self.guard = Guard(self.index)
                remaining(deadline)
                self.process = subprocess.Popen(
                    [self.cass, "serve", "--stdio", "--index", self.index,
                     "--request-timeout-ms", str(int(self.timeout * 1000))],
                    stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                    stderr=None, close_fds=True)
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
            if not response["ok"]:
                self.close()
                return response
            if not reused:
                self.epoch += 1
            response["admission"] = {
                "mode": "retained_guarded" if reused else "strict_full",
                "owner_epoch": self.epoch, "full_verify_requested": full,
                "persistent_proof": False, "file_identity_checked": True,
                "immutable_generation_certified": False,
            }
            # Validate the augmented envelope before allowing any output.
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
            if (type(response.get("id")) is not int or response["id"] != request["id"] or
                    type(response.get("ok")) is not bool or
                    type(response.get("schema_version")) is not int or
                    response["schema_version"] != 1):
                raise Failure("worker_protocol", "Mismatched IPC response envelope.")
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
