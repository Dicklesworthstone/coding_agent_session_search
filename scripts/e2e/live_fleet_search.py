#!/usr/bin/env python3
"""Opt-in real SSH fleet test. Requires Python 3 on the runner and remotes.

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
    event={'timestamp':'2026-09-01T00:00:03Z','type':'response_item','payload':{'type':'message','role':'user','content':[{'type':'input_text','text':request['marker']+' appended'}]}}
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


class FleetRun:
    def __init__(self, inventory, binary):
        self.repo = Path(__file__).resolve().parents[2]
        inventory = Path(inventory).resolve(strict=True)
        if inventory.is_relative_to(self.repo):
            raise ValueError("inventory must be outside the repository")
        if inventory.stat().st_mode & 0o077:
            raise ValueError("inventory must have private permissions (0600)")
        self.inventory = json.loads(inventory.read_text())
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
        self.root = Path(tempfile.mkdtemp(prefix="cass-live-fleet-"))
        self.root.chmod(0o700)
        if self.root.is_relative_to(self.repo):
            raise ValueError("TMPDIR must be outside the repository")
        self.write("inventory.json", self.inventory)
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
        self.outcomes = []
        self.ready = []

    def write(self, name, value):
        path = self.root / name
        with path.open("x") as stream:
            json.dump(value, stream, indent=2)
        path.chmod(0o600)

    def command(self, label, argv, payload=None, env=None, timeout=120):
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
        if record["exit"]:
            raise RuntimeError("command failed; see private artifact " + label)
        return record["stdout"]

    def cass(self, label, args, timeout=180):
        return self.command(label, [self.binary, *args], env=self.env, timeout=timeout)

    def seed(self, ordinal_host):
        ordinal, host = ordinal_host
        label = f"node-{ordinal:02}"
        request = {"phase": "initial", "session": self.token + label, "marker": self.token + " " + label}
        try:
            output = self.remote(label + "-seed", host, request)
            return {"label": label, "host": host, "request": request, **json.loads(output)}
        except (RuntimeError, ValueError, OSError) as error:
            return {"label": label, "failed": type(error).__name__}

    def remote(self, label, host, request):
        return self.command(label, ["ssh", "-F", str(self.ssh_config), "-o", "BatchMode=yes",
                            "-o", "StrictHostKeyChecking=yes", "-o", "ConnectTimeout=8",
                            "-o", "ServerAliveInterval=5", "-o", "ServerAliveCountMax=1",
                            host["ssh"], "python3 -c " + shlex.quote(REMOTE_SESSION)],
                            payload=json.dumps(request), timeout=35)

    def search(self, label, query, source="all"):
        documents = json_documents(self.cass(label, ["search", query, "--robot", "--mode", "lexical",
            "--no-maintenance", "--no-daemon", "--source", source, "--limit", "1000",
            "--fields", "source_path,line_number,agent,source_id,origin_host,content", "--timeout", "30000"]))
        if len(documents) != 1 or documents[0].get("budget", {}).get("timed_out"):
            raise AssertionError("search did not complete")
        return documents[0]["hits"]

    def verify(self, phase, expected_per_host):
        hits = self.search(phase + "-all", self.token)
        assert len(hits) == len(self.ready) * expected_per_host, "fleet hit count mismatch"
        identities = {(h["source_id"], h["source_path"], h["line_number"]) for h in hits}
        assert len(identities) == len(hits), "duplicate search identities"
        for host in self.ready:
            label = host["label"]
            selected = self.search(phase + "-" + label, self.token, label)
            assert len(selected) == expected_per_host, "source-scoped hit count mismatch"
            assert all(h["source_id"] == label and h.get("origin_host") and host["request"]["marker"] in h["content"] for h in selected), "source provenance mismatch"
        assert not self.search(phase + "-local-negative", self.token, "local"), "remote sessions leaked into local scope"
        assert not self.search(phase + "-missing-negative", self.token, "nonexistent-source"), "unknown source broadened query"
        return identities

    def run(self):
        self.cass("version", ["--version"])
        with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
            seeded = list(pool.map(self.seed, enumerate(self.hosts, 1)))
        self.write("remote-directories.json", seeded)
        for host in seeded:
            if "failed" in host:
                self.outcomes.append({"host": host["label"], "phase": "ssh-seed", "passed": False})
                continue
            self.cass(host["label"] + "-add", ["sources", "add", host["host"]["ssh"], "--name",
                      host["label"], "--path", host["path"], "--no-test"])
            self.ready.append(host)
        if not self.ready:
            raise RuntimeError("no reachable hosts")
        self.cass("initial-sync", ["sources", "sync", "--all", "--json"], timeout=600)
        initial = self.verify("initial", 2)
        self.cass("replay-sync", ["sources", "sync", "--all", "--json"], timeout=600)
        assert self.verify("replay", 2) == initial, "repeat sync changed identity"
        for host in self.ready:
            request = {**host["request"], "phase": "append", "root": host["root"]}
            self.remote(host["label"] + "-append", host["host"], request)
        self.cass("append-sync", ["sources", "sync", "--all", "--json"], timeout=600)
        appended = self.verify("append", 3)
        assert initial <= appended, "append lost existing messages"
        for host in self.ready:
            self.outcomes.append({"host": host["label"], "phase": "sync-search-replay-append", "passed": True})
        return len(self.ready) == len(self.hosts)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--inventory", required=True)
    parser.add_argument("--cass-bin", required=True)
    options = parser.parse_args()
    os.umask(0o077)
    run = FleetRun(options.inventory, options.cass_bin)
    passed = False
    try:
        passed = run.run()
    except (RuntimeError, AssertionError, ValueError, OSError) as error:
        # Exception details can include private paths; retain them privately.
        run.write("failure.json", {"type": type(error).__name__, "message": str(error)})
    report = {"passed": passed, "requested_hosts": len(run.hosts), "reachable_hosts": len(run.ready),
              "outcomes": run.outcomes, "artifacts": str(run.root)}
    run.write("summary.json", report)
    print(json.dumps(report, indent=2))
    return 0 if passed else 1


if __name__ == "__main__":
    raise SystemExit(main())
