#!/usr/bin/env python3
"""Run the real CASS Codex module and its tests without the storage/TUI build.

    python3 scripts/test_codex_contract.py

The temporary crate stages byte-identical production Rust files with their
normal module layout; there is no maintained fork or mock connector.
Dependency versions and the initial lockfile come from CASS itself. Only
FAD's unrelated SQLite/crypto features are omitted. The integration tests
also remain normal CASS cargo test targets.
Requires Python 3.11+ and the repository's Rust toolchain.
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
    dependencies = manifest["dependencies"]
    names = (
        "anyhow", "blake3", "dotenvy", "franken-agent-detection", "serde",
        "serde_json", "tempfile", "thiserror", "tracing",
    )
    tests = ("connector_codex_exclusions", "codex_source_containment")
    lines = [
        "[package]", 'name = "cass-codex-contract"', 'version = "0.0.0"',
        'edition = "2024"', "publish = false", "", "[lib]",
        'name = "coding_agent_search"', 'path = "lib.rs"',
    ]
    for test in tests:
        lines.extend([
            "", "[[test]]", f'name = "{test}"',
            "path = " + json.dumps((root / f"tests/{test}.rs").as_posix(), ensure_ascii=False),
        ])
    lines.extend(["", "[dependencies]"])
    for name in names:
        spec = dependencies[name]
        version = spec if isinstance(spec, str) else spec["version"]
        if name == "franken-agent-detection":
            lines.append(
                f'{name} = {{ version = {json.dumps(version)}, '
                'default-features = false, features = ["connectors"] }'
            )
        elif name == "serde":
            lines.append(f'{name} = {{ version = {json.dumps(version)}, features = ["derive"] }}')
        else:
            lines.append(f"{name} = {json.dumps(version)}")
    lines.extend(["", "[lints.rust]", 'unsafe_code = "forbid"', ""])

    library = '''// Re-exports only; the Codex implementation is unmodified production code.
pub use franken_agent_detection::{
    Connector, DetectionResult, DiscoveredSourceFile, NormalizedConversation,
    NormalizedMessage, ScanContext, ScanRoot, parse_timestamp, reindex_messages,
};
pub mod codex;
pub mod connectors {
    pub use super::{
        Connector, DetectionResult, DiscoveredSourceFile, NormalizedConversation,
        NormalizedMessage, ScanContext, ScanRoot, codex,
    };
}
'''
    cargo = shutil.which("cargo")
    if cargo is None:
        raise SystemExit("cargo is required; install the repository's Rust toolchain")
    source_dir = root / "src/connectors"
    source_files = [source_dir / "codex.rs", *sorted((source_dir / "codex").rglob("*.rs"))]
    with tempfile.TemporaryDirectory(prefix="cass-codex-contract-") as directory:
        work = Path(directory)
        project = work / "Cargo.toml"
        project.write_text("\n".join(lines), encoding="utf-8")
        (work / "lib.rs").write_text(library, encoding="utf-8")
        # Normal `mod codex` preserves the nested lookup rules that #[path]
        # overrides. Check equality rather than rewriting source to fit a stub.
        for original in source_files:
            staged = work / original.relative_to(source_dir)
            staged.parent.mkdir(parents=True, exist_ok=True)
            source = original.read_bytes()
            staged.write_bytes(source)
            if staged.read_bytes() != source:
                raise RuntimeError(f"staged source differs from {original}")
        for name in ("Cargo.lock", "rust-toolchain.toml", "rustfmt.toml", ".rustfmt.toml"):
            original = root / name
            if original.is_file():
                shutil.copyfile(original, work / name)
        env = os.environ.copy()
        # The ordinary unit tests must not inherit operator scan exclusions.
        # Integration tests supply each child's own real exclusion value.
        env["CASS_EXCLUDE_PATHS"] = ""
        commands = [
            [cargo, "test", "--manifest-path", str(project), "--all-targets"],
            [cargo, "clippy", "--manifest-path", str(project), "--all-targets", "--", "-D", "warnings"],
        ]
        for command in commands:
            print("+ " + " ".join(command), flush=True)
            subprocess.run(command, cwd=root, env=env, check=True)
        # Each file is checked individually, so do not resolve its children as
        # if that file were a crate root. Never reformat the actual working tree.
        rustfmt = shutil.which("rustfmt")
        if rustfmt is None:
            raise SystemExit("rustfmt is required")
        command = [
            rustfmt, "--edition", "2024", "--check", "--config", "skip_children=true",
            *map(str, source_files),
            *(str(root / f"tests/{test}.rs") for test in tests),
        ]
        print("+ " + " ".join(command), flush=True)
        subprocess.run(command, cwd=root, env=env, check=True)


if __name__ == "__main__":
    main()
