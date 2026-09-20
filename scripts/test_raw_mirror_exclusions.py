#!/usr/bin/env python3
"""Test the real raw-mirror storage and Codex exclusion boundary.

Default: stage byte-identical capture, Codex and franken_sync implementations.
The small keep-tag database opener uses the real read-only FrankenSQLite API,
not the full CASS storage open/recovery policy. No capture or parser is mocked.
Use --full to run the actual CASS library and CLI acceptance test instead.
Requires Python 3.11+ and the repository's Rust toolchain. Dependency versions,
features and the initial lockfile are taken from CASS.
"""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import tomllib


def execute(command: list[str], root: Path, env: dict[str, str]) -> None:
    print("+ " + " ".join(command), flush=True)
    subprocess.run(command, cwd=root, env=env, check=True)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--full", action="store_true", help="run full CASS targets, without a harness")
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    cargo = shutil.which("cargo")
    if cargo is None:
        raise SystemExit("cargo is required")
    env = os.environ.copy()
    env["CASS_EXCLUDE_PATHS"] = ""
    if args.full:
        execute([cargo, "test", "--locked", "--lib", "raw_mirror::"], root, env)
        execute([cargo, "test", "--locked", "--test", "e2e_codex_exclusion_capture"], root, env)
        return

    manifest = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))
    names = (
        "anyhow", "asupersync", "blake3", "chrono", "dirs", "dotenvy",
        "franken-agent-detection", "frankensqlite", "fs2", "glob", "libc",
        "serde", "serde_json", "tempfile", "thiserror", "tracing",
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
        if isinstance(spec, str):
            spec = {"version": spec}
        else:
            spec = dict(spec)
        if name == "franken-agent-detection":
            spec = {"version": spec["version"], "default-features": False, "features": ["connectors"]}
        fields = []
        for key in ("version", "package", "default-features", "features"):
            if key in spec:
                fields.append(f"{key} = {json.dumps(spec[key], ensure_ascii=False)}")
        lines.append(name + " = { " + ", ".join(fields) + " }")
    library = '''// Capture, parser, and sync-bridge files are unmodified production code.
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
pub mod franken_sync;
pub mod raw_mirror;
pub mod storage {
    pub mod sqlite {
        // Only keep-tag prune fixtures use this helper. It is a real read-only
        // engine connection, but deliberately not the full application's
        // recovery, busy-retry, schema-admission, or canonical storage layer.
        pub fn open_franken_raw_readonly_connection_with_timeout(
            path: &std::path::Path,
            timeout: std::time::Duration,
        ) -> anyhow::Result<crate::franken_sync::Connection> {
            use crate::franken_sync::compat::{OpenFlags, open_with_flags};
            let path = path.to_str().ok_or_else(|| anyhow::anyhow!("non-UTF-8 fixture database path"))?;
            let conn = open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            conn.execute(&format!("PRAGMA busy_timeout = {};", timeout.as_millis()))?;
            Ok(conn)
        }
    }
}
'''
    with tempfile.TemporaryDirectory(prefix="cass-raw-mirror-") as directory:
        work = Path(directory)
        project = work / "Cargo.toml"
        project.write_text("\n".join(lines) + "\n", encoding="utf-8")
        (work / "lib.rs").write_text(library, encoding="utf-8")
        for source_base, module in [
            (root / "src/connectors", "codex"),
            (root / "src", "raw_mirror"),
            (root / "src", "franken_sync"),
        ]:
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
            execute(command, root, env)


if __name__ == "__main__":
    main()
