#!/usr/bin/env python3
"""Run the real CASS Codex module and its tests without the storage/TUI build.

    python3 scripts/test_codex_contract.py
    python3 scripts/test_codex_contract.py --fad-checkout ../franken_agent_detection

The temporary crate stages byte-identical production Rust files with their
normal module layout; there is no maintained fork or mock connector.
By default dependency sources, versions and the initial lockfile come from CASS.
The resolved FAD source must match CASS's lockfile before tests may start.
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


# APIs that exist only in the full CASS crate. A staged test that uses one can
# never build here; 11fcce89 added such a module and left this lane red for
# days behind a bare compile error (bead coding_agent_session_search-2l1b0.62).
FULL_CRATE_ONLY = ("assert_cmd::", "cargo_bin!(", "get_connector_factories(")


def toml_value(value: object) -> str:
    """Encode the string/bool/list values used by Cargo dependency tables."""
    if isinstance(value, str):
        # JSON's ASCII surrogate pairs are not legal TOML Unicode escapes.
        return json.dumps(value, ensure_ascii=False)
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, list):
        return "[" + ", ".join(toml_value(item) for item in value) + "]"
    raise ValueError(f"unsupported Cargo dependency value type: {type(value).__name__}")


def dependency_for_harness(
    name: str, specification: str | dict, root: Path,
    fad_checkout: Path | None, fad_version: str,
) -> str:
    spec = {"version": specification} if isinstance(specification, str) else dict(specification)
    if "workspace" in spec:
        raise ValueError(f"{name}: resolve workspace inheritance before staging this dependency")
    if name == "franken-agent-detection":
        if fad_checkout is not None:
            for key in ("git", "rev", "tag", "branch", "path", "registry", "registry-index"):
                spec.pop(key, None)
            spec.update(version="=" + fad_version, path=str(fad_checkout))
        # The only deliberate feature reduction: unrelated FAD SQLite/crypto.
        # Keep its git/rev (or registry/path) source, not just its version.
        spec.update({"default-features": False, "features": ["connectors", "codex-zstd"]})
    elif name == "serde":
        spec["features"] = list(dict.fromkeys([*spec.get("features", []), "derive"]))
    if "path" in spec:
        spec["path"] = str((root / spec["path"]).resolve(strict=True))
    return name + " = { " + ", ".join(
        f"{key} = {toml_value(value)}" for key, value in spec.items()
    ) + " }"


def verify_fad_selection(metadata: dict, lockfile: dict, fad_checkout: Path | None) -> None:
    selected = [package for package in metadata["packages"] if package["name"] == "franken-agent-detection"]
    if len(selected) != 1:
        raise RuntimeError("Cargo must select exactly one franken-agent-detection package")
    package = selected[0]
    if fad_checkout is not None:
        if package.get("source") is not None or Path(package["manifest_path"]).resolve() != fad_checkout / "Cargo.toml":
            raise RuntimeError("Cargo did not select the requested local FAD checkout")
    else:
        locked = [entry for entry in lockfile["package"] if entry["name"] == "franken-agent-detection"]
        if len(locked) != 1 or (
            package["version"], package.get("source")
        ) != (locked[0]["version"], locked[0].get("source")):
            raise RuntimeError("Cargo's FAD version/source differs from CASS's committed lockfile")
    print(f"Verified FAD {package['version']}: {package.get('source') or package['manifest_path']}", flush=True)


def refuse_full_crate_tests(root: Path, tests: tuple[str, ...]) -> None:
    for test in tests:
        path = root / f"tests/{test}.rs"
        found = [token for token in FULL_CRATE_ONLY if token in path.read_text(encoding="utf-8")]
        if found:
            raise SystemExit(
                f"tests/{test}.rs uses full-crate APIs ({', '.join(found)}), which this slim "
                "consumer crate cannot build; put those tests in their own integration target "
                "(as tests/codex_source_budget_override.rs does)"
            )


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
    lockfile = tomllib.loads((root / "Cargo.lock").read_text(encoding="utf-8"))
    dependencies = manifest["dependencies"]
    names = (
        "anyhow", "blake3", "chrono", "dirs", "dotenvy", "franken-agent-detection", "walkdir",
        "serde", "serde_json", "tempfile", "thiserror", "tracing", "zstd",
    )
    tests = ("connector_codex_exclusions", "codex_source_containment")
    refuse_full_crate_tests(root, tests)
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
        spec = dependencies[name] if name in dependencies else manifest["dev-dependencies"][name]
        lines.append(dependency_for_harness(name, spec, root, fad_checkout, fad_version))
    lines.extend(["", "[lints.rust]", 'unsafe_code = "forbid"', ""])

    library = '''// Re-exports only; the Codex implementation is unmodified production code.
// Crate-internal entry points used only by unstaged CASS modules (such as
// ingest diagnostics) are unused here; the full crate's clippy checks them.
#![allow(dead_code)]
pub use franken_agent_detection::{
    Connector, DetectionResult, DiscoveredSourceFile, DiscoveredSourceRole, NormalizedConversation,
    NormalizedMessage, Origin, ScanContext, ScanRoot, SourceKind, parse_timestamp, reindex_messages,
};
pub mod codex;
pub mod connectors {
    pub use super::{
        Connector, DetectionResult, DiscoveredSourceFile, DiscoveredSourceRole, NormalizedConversation,
        NormalizedMessage, Origin, ScanContext, ScanRoot, SourceKind, codex,
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
        # The reduced crate needs its own root entry in the copied lockfile.
        # Normalize once, verify the actual source (including the resolved Git
        # commit), then disallow resolution changes during tests and Clippy.
        metadata = json.loads(subprocess.check_output(
            [cargo, "metadata", "--manifest-path", str(project), "--format-version", "1"],
            cwd=root, env=env, text=True,
        ))
        verify_fad_selection(metadata, lockfile, fad_checkout)
        commands = [
            [cargo, "test", "--locked", "--manifest-path", str(project), "--all-targets"],
            [cargo, "clippy", "--locked", "--manifest-path", str(project), "--all-targets", "--", "-D", "warnings"],
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
