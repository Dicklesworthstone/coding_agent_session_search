#!/usr/bin/env python3
"""Exercise CASS's actual OpenClaw surface and SQLite bridge without the TUI.

Normal mode uses the production dependency declarations unchanged. Qualification
mode enables only openclaw-sqlite in a candidate manifest, never the checkout.
It is for validating the feature change before landing it, not for release use.
"""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import tomllib


def declaration(source: str, name: str) -> str:
    section = re.search(r"(?ms)^\[dependencies\]\n(.*?)(?=^\[)", source)
    if section is None:
        raise ValueError("missing normal dependency section")
    matches = re.findall(r"(?m)^" + re.escape(name) + r"\s*=.*$", section[1])
    if len(matches) != 1:
        raise ValueError(f"missing or ambiguous declaration: {name}")
    if tomllib.loads(matches[0])[name] != tomllib.loads(source)["dependencies"][name]:
        raise ValueError(f"incomplete declaration: {name}")
    return matches[0]


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--qualify-feature", action="store_true")
    parser.add_argument("--candidate-output", type=Path)
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    manifest = (root / "Cargo.toml").read_text(encoding="utf-8")
    fad = declaration(manifest, "franken-agent-detection")
    features = tomllib.loads(fad)["franken-agent-detection"].get("features", [])
    if "openclaw-sqlite" not in features:
        if not args.qualify_feature:
            raise SystemExit("standard CASS dependency is missing openclaw-sqlite")
        candidate = fad.replace("features = [", 'features = ["openclaw-sqlite", ', 1)
        if candidate == fad:
            raise SystemExit("unrecognized dependency declaration; not modifying it")
        manifest = manifest.replace(fad, candidate, 1)
    names = ["anyhow", "asupersync", "frankensqlite", "fsqlite-types", "franken-agent-detection", "serde_json", "tempfile"]
    dependencies = [declaration(manifest, name) for name in names]
    cargo = shutil.which("cargo")
    if cargo is None:
        raise SystemExit("cargo is required")
    test = root / "tests/connector_openclaw_sqlite.rs"
    with tempfile.TemporaryDirectory(prefix="cass-openclaw-contract-") as directory:
        work = Path(directory)
        (work / "connectors").mkdir()
        for relative, destination in [
            ("src/connectors/openclaw.rs", "connectors/openclaw.rs"),
            ("src/franken_sync.rs", "franken_sync.rs"),
        ]:
            shutil.copyfile(root / relative, work / destination)
        (work / "lib.rs").write_text('''pub mod franken_sync;
pub mod connectors {
    pub use franken_agent_detection::{Connector, ScanContext, ScanRoot};
    pub mod openclaw;
}
// The full application calls this private production teardown. Keep it used
// in this small crate too, without changing or suppressing its diagnostics.
pub fn shutdown_contract_driver() -> bool { franken_sync::shutdown_driver() }
''', encoding="utf-8")
        project = work / "Cargo.toml"
        project.write_text('\n'.join([
            '[package]', 'name = "cass-openclaw-contract"', 'version = "0.0.0"',
            'edition = "2024"', 'publish = false', '[lib]',
            'name = "coding_agent_search"', 'path = "lib.rs"',
            '[[test]]', 'name = "connector_openclaw_sqlite"',
            'path = ' + json.dumps(test.as_posix(), ensure_ascii=False),
            '[dependencies]', *dependencies, '[profile.dev.package."*"]', 'opt-level = 1',
            '[lints.rust]', 'unsafe_code = "forbid"', '',
        ]), encoding="utf-8")
        for name in ["Cargo.lock", "rust-toolchain.toml"]:
            if (root / name).is_file():
                shutil.copyfile(root / name, work / name)
        env = os.environ.copy()
        env["RUST_MIN_STACK"] = "16777216"
        env["CASS_EXCLUDE_PATHS"] = ""
        metadata = json.loads(subprocess.check_output([
            cargo, "metadata", "--manifest-path", str(project), "--format-version", "1",
        ], cwd=root, env=env))
        expected = tomllib.loads((root / "Cargo.lock").read_text(encoding="utf-8"))["package"]
        expected = {(p["name"], p["version"], p.get("source")) for p in expected}
        for package in metadata["packages"]:
            name = package["name"]
            if name in {"franken-agent-detection", "asupersync"} or name.startswith("fsqlite"):
                key = (name, package["version"], package.get("source"))
                if key not in expected:
                    raise RuntimeError(f"dependency is not from the CASS lockfile: {key}")
                print("LOCKED_CONSUMER", key, flush=True)
        for command in [
            [cargo, "test", "--locked", "--manifest-path", str(project), "--test", "connector_openclaw_sqlite"],
            [cargo, "clippy", "--locked", "--manifest-path", str(project), "--test", "connector_openclaw_sqlite", "--", "-D", "warnings"],
        ]:
            print("+", " ".join(command), flush=True)
            subprocess.run(command, cwd=root, env=env, check=True)
    if args.candidate_output is not None:
        args.candidate_output.parent.mkdir(parents=True, exist_ok=True)
        args.candidate_output.write_text(manifest, encoding="utf-8")


if __name__ == "__main__":
    main()
