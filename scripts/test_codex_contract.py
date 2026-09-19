#!/usr/bin/env python3
"""Run the real CASS Codex module and its tests without the storage/TUI build.

    python3 scripts/test_codex_contract.py
    python3 scripts/test_codex_contract.py --fad-checkout ../franken_agent_detection

The temporary crate stages byte-identical production Rust files with their
normal module layout; there is no maintained fork or mock connector.
By default dependency versions and the initial lockfile come from CASS.
--fad-checkout tests an upstream change before publication, verifies Cargo
selected that checkout, and changes only the temporary crate's dependency.
Only FAD's unrelated SQLite/crypto features are omitted. The integration
tests also remain normal CASS cargo test targets.
Requires Python 3.11+ and the repository's Rust toolchain.
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


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fad-checkout", type=Path, help="Test this local FAD checkout without changing CASS's published dependency pin")
    args = parser.parse_args()
    fad_checkout = None
    fad_version = ""
    if args.fad_checkout is not None:
        try:
            fad_checkout = args.fad_checkout.resolve(strict=True)
            package = tomllib.loads((fad_checkout / "Cargo.toml").read_text(encoding="utf-8"))["package"]
            if package.get("name") != "franken-agent-detection" or not isinstance(package.get("version"), str):
                raise ValueError("expected the franken-agent-detection package with an explicit version")
            fad_version = package["version"]
        except (OSError, KeyError, ValueError) as error:
            parser.error(f"invalid --fad-checkout: {error}")

    root = Path(__file__).resolve().parents[1]
    manifest = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))
    dependencies = manifest["dependencies"]
    names = (
        "anyhow", "blake3", "chrono", "dirs", "dotenvy", "franken-agent-detection",
        "serde", "serde_json", "tempfile", "thiserror", "tracing",
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
            source = f'version = {json.dumps(version)}'
            if fad_checkout is not None:
                source = f'version = {json.dumps("=" + fad_version)}, path = {json.dumps(fad_checkout.as_posix(), ensure_ascii=False)}'
            lines.append(
                f'{name} = {{ {source}, '
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
        if fad_checkout is not None:
            metadata = json.loads(subprocess.check_output(
                [cargo, "metadata", "--manifest-path", str(project), "--format-version", "1"],
                cwd=root, env=env, text=True,
            ))
            selected = [package for package in metadata["packages"] if package["name"] == "franken-agent-detection"]
            if len(selected) != 1 or Path(selected[0]["manifest_path"]).resolve() != fad_checkout / "Cargo.toml":
                raise RuntimeError("Cargo did not select the requested local FAD checkout")
            print(f"Verified local FAD {fad_version}: {selected[0]['manifest_path']}", flush=True)
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
