#!/usr/bin/env python3
"""Compile the exact production filesystem matcher against CASS's dependency pins.

This is a native matcher test/benchmark, NOT connector, storage or CLI coverage.
Cargo may prune unused features/edges in this disposable graph, but every retained
package identity, checksum and edge must come from the repository lockfile before
any compilation. Native tests then run with --locked. The repository is read-only.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import tomllib
from typing import Any

PACKAGE = "cass-source-ledger-matcher"
VERSION = "0.0.0"
DIRECT = ("serde_json", "tempfile")


def identity(package: dict[str, Any]) -> tuple[str, str, str]:
    return package["name"], package["version"], package.get("source", "")


def resolve(spec: str, packages: list[dict[str, Any]]) -> dict[str, Any]:
    parts = spec.split()
    matches = [
        package for package in packages
        if package["name"] == parts[0]
        and (len(parts) < 2 or package["version"] == parts[1])
        and (len(parts) < 3 or package.get("source") == parts[2].strip("()"))
    ]
    if len(matches) != 1 or len(parts) > 3:
        raise RuntimeError(f"ambiguous or missing locked dependency: {spec}")
    return matches[0]


def dependency_closure(lock: dict[str, Any]) -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
    packages = lock["package"]
    root = resolve("coding-agent-search", packages)
    direct = [resolve(next(spec for spec in root["dependencies"] if spec.split()[0] == name),
                      packages) for name in DIRECT]
    selected = {}
    pending = list(direct)
    while pending:
        package = pending.pop()
        key = identity(package)
        if key in selected:
            continue
        if not key[2].startswith("registry+") or not package.get("checksum"):
            raise RuntimeError(f"matcher dependency is not checksum-pinned: {key}")
        selected[key] = package
        pending.extend(resolve(spec, packages) for spec in package.get("dependencies", []))
    return direct, list(selected.values())


def validate_normalized_lock(
    lock: dict[str, Any], allowed: list[dict[str, Any]], direct: list[dict[str, Any]],
) -> list[dict[str, Any]]:
    """Permit feature pruning, never a new version, source, checksum or edge."""
    packages = lock["package"]
    observed = {identity(package): package for package in packages}
    if len(observed) != len(packages):
        raise RuntimeError("duplicate package identity in matcher lockfile")
    root = resolve(f"{PACKAGE} {VERSION}", packages)
    if root.get("source") or root.get("checksum"):
        raise RuntimeError("matcher root is not the local harness")
    expected = {identity(package): package for package in allowed}
    root_edges = {identity(resolve(spec, packages)) for spec in root.get("dependencies", [])}
    if root_edges != {identity(package) for package in direct}:
        raise RuntimeError("matcher direct dependencies changed")
    retained = []
    for package in packages:
        if package is root:
            continue
        key = identity(package)
        original = expected.get(key)
        if original is None or package.get("checksum") != original.get("checksum"):
            raise RuntimeError(f"dependency pin drift: {key}")
        old_edges = {identity(resolve(spec, allowed)) for spec in original.get("dependencies", [])}
        new_edges = {identity(resolve(spec, packages)) for spec in package.get("dependencies", [])}
        if not new_edges <= old_edges:
            raise RuntimeError(f"dependency edge drift: {key}")
        retained.append(package)
    return retained


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bench", action="store_true", help="also run the 4159-source benchmark")
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    source = root / "src/connectors/source_dependencies/observation.rs"
    source_bytes = source.read_bytes()
    lock_path = root / "Cargo.lock"
    lock_bytes = lock_path.read_bytes()
    lock_text = lock_bytes.decode()
    lock = tomllib.loads(lock_text)
    direct, allowed = dependency_closure(lock)
    selected = {identity(package) for package in allowed}
    tables = []
    for block in lock_text.split("[[package]]")[1:]:
        package = tomllib.loads("[[package]]" + block)["package"][0]
        if identity(package) in selected:
            tables.append("[[package]]" + block)
    manifest = "\n".join([
        "[package]", f'name = "{PACKAGE}"', f'version = "{VERSION}"',
        'edition = "2024"', "[lib]", 'path = "observation.rs"', "[dependencies]",
        *(f'{package["name"]} = "={package["version"]}"' for package in direct), "",
    ])
    harness_lock = "\n".join([
        f'version = {lock["version"]}', "", "[[package]]",
        f'name = "{PACKAGE}"', f'version = "{VERSION}"',
        'dependencies = ["serde_json", "tempfile"]', "", *tables,
    ])
    receipt = {
        "source_sha256": hashlib.sha256(source_bytes).hexdigest(),
        "repository_lock_sha256": hashlib.sha256(lock_bytes).hexdigest(),
        "scope": "exact production filesystem matcher only; full integration is separate",
    }
    with tempfile.TemporaryDirectory(prefix="cass-source-ledger-") as directory:
        work = Path(directory)
        (work / "observation.rs").write_bytes(source_bytes)
        (work / "Cargo.toml").write_text(manifest)
        (work / "Cargo.lock").write_text(harness_lock)
        env = os.environ.copy()
        toolchain = tomllib.loads((root / "rust-toolchain.toml").read_text())["toolchain"]["channel"]
        env["RUSTUP_TOOLCHAIN"] = toolchain
        for variable in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS"):
            env.pop(variable, None)
        try:
            # Metadata resolves the smaller feature graph without compiling code.
            # The copied CASS graph can contain optional edges not used here; let
            # Cargo normalize only this temporary lock, then audit it before test.
            subprocess.run(
                ["cargo", "metadata", "--format-version", "1", "--manifest-path", str(work / "Cargo.toml")],
                cwd=work, env=env, stdout=subprocess.DEVNULL, check=True,
            )
            normalized = (work / "Cargo.lock").read_bytes()
            retained = validate_normalized_lock(tomllib.loads(normalized.decode()), allowed, direct)
            receipt["harness_lock_sha256"] = hashlib.sha256(normalized).hexdigest()
            receipt["dependencies"] = [
                {key: package[key] for key in ("name", "version", "source", "checksum")}
                for package in sorted(retained, key=identity)
            ]
            receipt["pruned_packages"] = len(allowed) - len(retained)
            print("GH512_MATCHER_SOURCE=" + json.dumps(receipt, sort_keys=True), flush=True)
            command = ["cargo", "test", "--release", "--locked", "--manifest-path", str(work / "Cargo.toml")]
            subprocess.run(command + ["--", "--nocapture"], cwd=work, env=env, check=True)
            if args.bench:
                subprocess.run(command + ["gh512_benchmark_4159", "--", "--ignored", "--nocapture"],
                               cwd=work, env=env, check=True)
            if (work / "Cargo.lock").read_bytes() != normalized:
                raise RuntimeError("matcher dependency pins changed during qualification")
            if (work / "observation.rs").read_bytes() != source_bytes:
                raise RuntimeError("tested matcher changed during qualification")
        finally:
            if source.read_bytes() != source_bytes or lock_path.read_bytes() != lock_bytes:
                raise RuntimeError("repository source or dependency lock changed during qualification")


if __name__ == "__main__":
    main()
