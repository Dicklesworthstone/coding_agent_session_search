#!/usr/bin/env python3
"""Test the actual raw-mirror storage and Codex exclusion boundary in isolation.

Stages byte-identical production modules, not mocks; omits unrelated DB/TUI
builds. Dependency versions and the initial lockfile are taken from CASS.
Requires Python 3.11+ and the repository's pinned Rust toolchain.
"""
from __future__ import annotations

import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import tomllib


def main() -> None:
    root = Path(__file__).resolve().parents[1]
    manifest = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))
    names = (
        "anyhow", "blake3", "chrono", "dirs", "dotenvy", "franken-agent-detection",
        "fs2", "libc", "serde", "serde_json", "tempfile", "thiserror", "tracing",
    )
    lines = [
        "[package]", 'name = "cass-raw-mirror-contract"', 'version = "0.0.0"',
        'edition = "2024"', 'publish = false', "", "[lib]", 'path = "lib.rs"',
        "", "[dependencies]",
    ]
    for name in names:
        spec = manifest["dependencies"].get(name, manifest.get("dev-dependencies", {}).get(name))
        if spec is None:
            raise SystemExit(f"missing CASS dependency: {name}")
        version = spec if isinstance(spec, str) else spec["version"]
        if name == "franken-agent-detection":
            lines.append(f'{name} = {{ version = {json.dumps(version)}, default-features = false, features = ["connectors"] }}')
        elif name == "serde":
            lines.append(f'{name} = {{ version = {json.dumps(version)}, features = ["derive"] }}')
        else:
            lines.append(f"{name} = {json.dumps(version)}")
    library = '''// Only module wiring differs; every staged implementation is unchanged.
// Full-application-only entry points are unused in this focused consumer.
#![allow(dead_code)]
pub use franken_agent_detection::{
    Connector, DetectionResult, DiscoveredSourceFile, NormalizedConversation,
    NormalizedMessage, ScanContext, ScanRoot, parse_timestamp, reindex_messages,
};
pub mod codex;
pub mod connectors {
    pub use super::{Connector, DiscoveredSourceFile, ScanContext, ScanRoot, codex};
}
pub mod raw_mirror;
'''
    cargo = shutil.which("cargo")
    if cargo is None:
        raise SystemExit("cargo is required")
    env = os.environ.copy()
    env["CASS_EXCLUDE_PATHS"] = ""
    with tempfile.TemporaryDirectory(prefix="cass-raw-mirror-") as directory:
        work = Path(directory)
        project = work / "Cargo.toml"
        project.write_text("\n".join(lines) + "\n", encoding="utf-8")
        (work / "lib.rs").write_text(library, encoding="utf-8")
        for source_base, module in [(root / "src/connectors", "codex"), (root / "src", "raw_mirror")]:
            sources = [source_base / f"{module}.rs", *sorted((source_base / module).rglob("*.rs"))]
            for source in sources:
                target = work / source.relative_to(source_base)
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(source.read_bytes())
                if target.read_bytes() != source.read_bytes():
                    raise RuntimeError(f"staged source differs: {source}")
        for name in ["Cargo.lock", "rust-toolchain.toml", "rustfmt.toml", ".rustfmt.toml"]:
            source = root / name
            if source.is_file():
                shutil.copyfile(source, work / name)
        for command in [
            [cargo, "test", "--manifest-path", str(project), "--lib", "raw_mirror::", "--", "--nocapture"],
            [cargo, "clippy", "--manifest-path", str(project), "--all-targets", "--", "-D", "warnings"],
        ]:
            print("+ " + " ".join(command), flush=True)
            subprocess.run(command, cwd=root, env=env, check=True)


if __name__ == "__main__":
    main()
