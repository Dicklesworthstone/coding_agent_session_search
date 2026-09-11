#!/usr/bin/env python3
"""Opt-in real SSH fleet test. Requires a Unix runner and Python 3.9+.

Usage: python3 scripts/e2e/live_fleet_search.py --inventory /private/fleet.json
       --cass-bin /path/to/cass

Inventory (keep OUTSIDE the repository, mode 0600):
  {"ssh_config": "/private/ssh_config", "hosts": [{"ssh": "workstation"}]}

Uses existing SSH authentication with strict host-key checks. Creates fresh,
retained test directories on each remote and a private local artifact directory;
never edits existing archives or removes files. Only synthetic sessions are
transferred. Raw commands/output/inventory stay outside git with mode 0600.
Console output contains ordinal host labels and verdicts, never host identities.
Every requested host must pass; unreachable hosts are failures, not skips.
"""

import argparse
import concurrent.futures
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import tempfile
import time
import uuid


REMOTE_SESSION = r'''
import datetime,json,os,pathlib,sys,tempfile
os.umask(0o077)
request=json.load(sys.stdin)
if request['phase']=='initial':
    root=pathlib.Path(tempfile.mkdtemp(prefix='cass-live-fleet-'))
    sessions=root/'.codex'/'sessions'
    sessions.mkdir(parents=True)
    path=sessions/'rollout-fleet.jsonl'
    events=[{'timestamp':'2026-09-01T00:00:00Z','type':'session_meta','payload':{'id':request['session'],'cwd':'/cass-live-fleet-project','cli_version':'0.42.0'}}]
    for index in range(2):
        events.append({'timestamp':f'2026-09-01T00:00:0{index+1}Z','type':'response_item','payload':{'type':'message','role':'user' if index==0 else 'assistant','content':[{'type':'input_text' if index==0 else 'output_text','text':request['marker']+' initial '+str(index)}]}})
    with path.open('x') as stream:
        for event in events:stream.write(json.dumps(event)+'\n')
else:
    root=pathlib.Path(request['root'])
    assert root.name.startswith('cass-live-fleet-') and root.is_absolute()
    path=root/'.codex'/'sessions'/'rollout-fleet.jsonl'
    with path.open() as stream:first=json.loads(stream.readline())
    assert first['payload']['id']==request['session']
    number=request.get('number',3)
    event={'timestamp':f'2026-09-01T00:00:0{number}Z','type':'response_item','payload':{'type':'message','role':'user','content':[{'type':'input_text','text':request['marker']+' appended '+str(number)}]}}
    with path.open('a') as stream:stream.write(json.dumps(event)+'\n')
print(json.dumps({'root':str(root),'path':str(path.parent)}))
'''


def json_documents(text):
    decoder = json.JSONDecoder()
    documents = []
    while text.strip():
        value, end = decoder.raw_decode(text.lstrip())
        documents.append(value)
        text = text.lstrip()[end:]
    return documents


def json_document(text):
    try:
        return json.loads(text)  # ubs:ignore -- UBS 5.3.13 misses the enclosing JSONDecodeError handler.
    except json.JSONDecodeError as error:
        # Keep malformed private command output in its existing receipt. An
        # exception should identify the location without echoing that content.
        raise ValueError(f"invalid JSON at line {error.lineno}, column {error.colno}") from None


class FleetRun:
    def __init__(self, inventory, binary, tailscale=False):
        self.tailscale = tailscale
        self.repo = Path(__file__).resolve().parents[2]
        inventory = Path(inventory).resolve(strict=True)
        if inventory.is_relative_to(self.repo):
            raise ValueError("inventory must be outside the repository")
        if inventory.stat().st_mode & 0o077:
            raise ValueError("inventory must have private permissions (0600)")
        self.inventory = json_document(inventory.read_text())
        self.hosts = self.inventory["hosts"]
        if not self.hosts or len({h["ssh"] for h in self.hosts}) != len(self.hosts):
            raise ValueError("inventory must contain distinct hosts")
        for host in self.hosts:
            if not re.fullmatch(r"[A-Za-z0-9_][A-Za-z0-9_.@-]*", host["ssh"]):
                raise ValueError("invalid SSH alias")
        self.ssh_config = Path(self.inventory["ssh_config"]).resolve(strict=True)
        if self.ssh_config.is_relative_to(self.repo):
            raise ValueError("SSH configuration must be outside the repository")
        self.binary = str(Path(binary).resolve(strict=True))
        self.root = Path(tempfile.mkdtemp(prefix="cass-live-fleet-")).resolve()
        self.root.chmod(0o700)
        if self.root.is_relative_to(self.repo):
            raise ValueError("TMPDIR must be outside the repository")
        self.write("inventory.json", self.inventory)
        self.write("harness.json", {"sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest()})
        supplied_config = self.ssh_config
        self.ssh_config = self.root / "ssh_config"
        with self.ssh_config.open("x") as stream:
            stream.write("Host cass-live-unreachable\n    HostName 127.0.0.1\n    Port 1\n"
                         "    ProxyCommand none\n    ProxyJump none\nHost *\nInclude "
                         + json.dumps(str(supplied_config)) + "\n")
        self.write("binary.json", {"path": self.binary, "sha256": hashlib.sha256(Path(self.binary).read_bytes()).hexdigest()})
        self.home = self.root / "home"
        self.home.mkdir()
        (self.home / ".env").write_text("")
        self.data = self.root / "data"
        self.env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": str(self.home),
                    "XDG_CONFIG_HOME": str(self.root / "config"), "XDG_DATA_HOME": str(self.root / "xdg"),
                    "CASS_DATA_DIR": str(self.data), "CASS_SSH_CONFIG": str(self.ssh_config),
                    "CODING_AGENT_SEARCH_NO_UPDATE_PROMPT": "1", "CASS_AUTO_REFRESH": "0",
                    "RUST_MIN_STACK": "134217728", "CASS_DAEMON_SOCKET": str(self.root / "unused.sock")}
        for name in ["SSH_AUTH_SOCK", "USER", "LOGNAME"]:
            if name in os.environ:
                self.env[name] = os.environ[name]
        self.token = "cassfleet" + uuid.uuid4().hex
        self.outcomes = [{"host": f"node-{ordinal:02}", "phase": "discovery-pending", "passed": None}
                         for ordinal in range(1, len(self.hosts) + 1)]
        self.ready = []

    def write(self, name, value):
        path = self.root / name
        with path.open("x") as stream:
            json.dump(value, stream, indent=2)
        path.chmod(0o600)

    def command(self, label, argv, payload=None, env=None, timeout=120, expected_exit=0):
        started = time.monotonic()
        try:
            result = subprocess.run(argv, input=payload, capture_output=True, text=True,
                                    cwd=self.home, env=env, timeout=timeout)
            record = {"argv": argv, "exit": result.returncode, "stdout": result.stdout, "stderr": result.stderr}
        except subprocess.TimeoutExpired as error:
            record = {"argv": argv, "exit": 124, "stdout": (error.stdout or b"").decode(errors="replace"),
                      "stderr": (error.stderr or b"").decode(errors="replace"), "timeout": True}
        record["elapsed_seconds"] = time.monotonic() - started
        self.write(label + ".json", record)
        if record["exit"] != expected_exit:
            raise RuntimeError("command failed; see private artifact " + label)
        return record["stdout"]

    def cass(self, label, args, timeout=180, expected_exit=0):
        return self.command(label, [self.binary, *args], env=self.env, timeout=timeout, expected_exit=expected_exit)

    def seed(self, ordinal_host):
        ordinal, host = ordinal_host
        label = f"node-{ordinal:02}"
        request = {"phase": "initial", "session": self.token + label, "marker": self.token + " " + label}
        try:
            output = self.remote(label + "-seed", host, request)
            return {"label": label, "host": host, "request": request, **json_document(output)}
        except (RuntimeError, ValueError, OSError) as error:
            return {"label": label, "failed": type(error).__name__}

    def remote(self, label, host, request):
        return self.command(label, ["ssh", "-F", str(self.ssh_config), "-o", "BatchMode=yes",
                            "-o", "StrictHostKeyChecking=yes", "-o", "ConnectTimeout=8",
                            "-o", "ServerAliveInterval=5", "-o", "ServerAliveCountMax=1",
                            host["ssh"], "python3 -c " + shlex.quote(REMOTE_SESSION)],
                            payload=json.dumps(request), timeout=35)

    def search(self, label, query, source="all", mode="lexical"):
        mode_args = ["--mode", mode] if mode else []
        documents = json_documents(self.cass(label, ["search", query, "--robot", *mode_args,
            "--no-maintenance", "--no-daemon", "--source", source, "--limit", "1000",
            "--fields", "source_path,line_number,agent,source_id,origin_host,content", "--timeout", "30000"]))
        if len(documents) != 1 or documents[0].get("budget", {}).get("timed_out"):
            raise AssertionError("search did not complete")
        return documents[0]["hits"]

    def sync(self, label, expected_exit=0):
        # --json promises one document, including the nested indexing result.
        result = json_document(self.cass(label, ["sources", "sync", "--all", "--json"],
                                      timeout=600, expected_exit=expected_exit))
        expected_status = {0: "complete", 7: "index_failed", 8: "partial"}[expected_exit]
        assert result["status"] == expected_status, "sync status disagrees with exit"
        if result["total_files"] and not expected_exit:
            assert result["indexing"]["success"] is True, "sync omitted completed indexing"  # ubs:ignore — require the JSON boolean; integer 1 is not success.
        return result

    def verify(self, phase, expected_per_host):
        hits = self.search(phase + "-all", self.token)
        assert len(hits) == len(self.ready) * expected_per_host, "fleet hit count mismatch"
        identities = {(h["source_id"], h["source_path"], h["line_number"]) for h in hits}
        assert len(identities) == len(hits), "duplicate search identities"
        scoped_identities = set()
        def evidence(hit):
            return (hit["source_id"], hit["source_path"], hit["line_number"], hit.get("origin_host"), hit["content"])
        global_evidence = {evidence(hit) for hit in hits}
        scoped_evidence = set()
        for host in self.ready:
            label = host["label"]
            selected = self.search(phase + "-" + label, self.token, label)
            assert len(selected) == expected_per_host, "source-scoped hit count mismatch"
            assert all(h["source_id"] == label and h.get("origin_host") == host["target"] and host["request"]["marker"] in h["content"] for h in selected), "source provenance mismatch"
            scoped_identities.update((h["source_id"], h["source_path"], h["line_number"]) for h in selected)
            scoped_evidence.update(evidence(hit) for hit in selected)
        assert scoped_identities == identities, "global search disagrees with source-scoped results"  # ubs:ignore — acceptance assertion; main rejects optimized Python before any workflow.
        assert scoped_evidence == global_evidence, "global search changed content or provenance"  # ubs:ignore — acceptance assertion; main rejects optimized Python before any workflow.
        assert not self.search(phase + "-local-negative", self.token, "local"), "remote sessions leaked into local scope"  # ubs:ignore — acceptance assertion; main rejects optimized Python before any workflow.
        assert not self.search(phase + "-missing-negative", self.token, "nonexistent-source"), "unknown source broadened query"  # ubs:ignore — acceptance assertion; main rejects optimized Python before any workflow.
        hybrid = self.search(phase + "-default-hybrid", self.token, mode=None)
        assert {(h["source_id"], h["source_path"], h["line_number"]) for h in hybrid} == identities, "default hybrid lost fleet evidence"
        assert {evidence(hit) for hit in hybrid} == global_evidence, "default hybrid changed content or provenance"
        return identities

    def run(self):
        self.cass("version", ["--version"])
        discovery_args = ["sources", "discover", "--json"]
        if self.tailscale:
            discovery_args.append("--tailscale")
        discovery = json_document(self.cass("discovery", discovery_args))
        if self.tailscale:
            assert not discovery.get("discovery_warning"), "Tailscale discovery unavailable"
        aliases = {host["name"] for host in discovery.get("hosts", [])}
        assert all(host["ssh"].rsplit("@", 1)[-1] in aliases for host in self.hosts), "SSH discovery omitted an inventory alias"
        if self.tailscale:
            baseline = json_document(self.cass("ssh-only-discovery", ["sources", "discover", "--json"]))
            configured = {host["name"] for host in baseline.get("hosts", [])}
            assert any(host["ssh"].rsplit("@", 1)[-1] not in configured for host in self.hosts), "no inventory target required Tailscale discovery"
        with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
            seeded = list(pool.map(self.seed, enumerate(self.hosts, 1)))
        self.write("remote-directories.json", seeded)
        self.outcomes = [{"host": host["label"], "phase": "ssh-seed" if "failed" in host else "workflow-pending",
                          "passed": False if "failed" in host else None} for host in seeded]
        self.ready = [host for host in seeded if "failed" not in host]
        for host in seeded:
            if "failed" in host:
                continue
            alias = host["host"]["ssh"]
            # sources add requires user@host in older releases. Resolve the
            # same user OpenSSH would use; never substitute the runner's user.
            config = self.command(host["label"] + "-ssh-config", ["ssh", "-G", "-F", str(self.ssh_config), alias])
            user = next(line.split(" ", 1)[1] for line in config.splitlines() if line.startswith("user "))
            target = alias if "@" in alias else user + "@" + alias
            host["target"] = target
            self.cass(host["label"] + "-add", ["sources", "add", target, "--name",
                      host["label"], "--path", host["path"]])
        if not self.ready:
            raise RuntimeError("no reachable hosts")
        self.sync("initial-sync")
        initial = self.verify("initial", 2)
        self.sync("replay-sync")
        assert self.verify("replay", 2) == initial, "repeat sync changed identity"
        for host in self.ready:
            request = {**host["request"], "phase": "append", "root": host["root"]}
            self.remote(host["label"] + "-append", host["host"], request)
        # Hold the real indexing lock: transfer must remain observable, and a
        # refused ingest must not be advertised as completed indexing.
        with (self.data / "index-run.lock").open("a+b") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            busy = self.sync("index-busy-sync", expected_exit=7)
            assert busy["indexing"].get("error"), "missing indexing failure detail"
            assert self.verify("index-busy", 2) == initial, "failed ingest changed canonical evidence"
        recovered = json_document(self.cass("mirror-reingest", ["sources", "reingest", "--from-mirror", "--json"], timeout=600))
        assert recovered["status"] == "complete" and recovered["indexing"]["success"], "mirror recovery did not complete"
        self.verify("mirror-recovered", 3)
        for host in self.ready:
            request = {**host["request"], "phase": "append", "root": host["root"], "number": 4}
            self.remote(host["label"] + "-second-append", host["host"], request)
        self.sync("append-sync")
        appended = self.verify("append", 4)
        assert initial <= appended, "append lost existing messages"
        # A real refused SSH connection must produce partial failure while the
        # already-synced real machines remain queryable. Never spoof ssh/rsync.
        self.cass("offline-add", ["sources", "add", "unreachable@cass-live-unreachable",
                  "--name", "unavailable-source", "--path", "/cass-live-unused", "--no-test"])
        partial = self.sync("offline-sync", expected_exit=8)
        assert partial.get("sources_with_failures") == 1, "unexpected source failure count"
        assert self.verify("offline", 4) == appended, "failed source changed searchable evidence"
        for outcome in self.outcomes:
            if outcome["passed"] is None:
                outcome.update(phase="sync-search-replay-append", passed=True)
        return len(self.ready) == len(self.hosts)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--inventory", required=True)
    parser.add_argument("--cass-bin", required=True)
    parser.add_argument("--tailscale", action="store_true",
                        help="Require live Tailscale discovery; inventory SSH targets should be tailnet IPs")
    options = parser.parse_args()
    if not __debug__:
        parser.error("assertions are required; run without -O or PYTHONOPTIMIZE")
    os.umask(0o077)
    try:
        run = FleetRun(options.inventory, options.cass_bin, options.tailscale)
    except (ValueError, OSError, KeyError, TypeError):
        # Invalid input can contain private paths; never print a traceback.
        parser.error("invalid private inventory, SSH configuration, binary, or artifact directory")
    passed = False
    try:
        passed = run.run()
    except Exception as error:
        # Exception details can include private paths; retain them privately.
        run.write("failure.json", {"type": type(error).__name__, "message": str(error)})
    report = {"passed": passed, "requested_hosts": len(run.hosts), "reachable_hosts": len(run.ready),
              "outcomes": run.outcomes, "artifacts": str(run.root)}
    run.write("summary.json", report)
    print(json.dumps(report, indent=2))
    return 0 if passed else 1


if __name__ == "__main__":
    raise SystemExit(main())
