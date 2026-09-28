#!/usr/bin/env python3
"""Compile the actual CASS shard admission/merge against pinned native HNSW.

This extends the existing exact-FSVI component harness, not the shipping CASS
lockfile. It never edits either source checkout. Full CASS remains a separate
qualification command. No fixture graph replaces the native implementation.
"""
from __future__ import annotations
import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tomllib


def once(text: str, old: str, new: str) -> str:
    if text.count(old) != 1:
        raise ValueError(f"Expected one extraction anchor: {old!r}")
    return text.replace(old, new, 1)


def locked_version(root: Path, name: str) -> str:
    """The version CASS's own lockfile resolves, so the harness links the same crate."""
    lock = tomllib.loads((root / "Cargo.lock").read_text(encoding="utf-8"))
    versions = {package["version"] for package in lock["package"] if package["name"] == name}
    if len(versions) != 1:
        raise ValueError(f"Expected one locked {name} version, got {sorted(versions)}")
    return versions.pop()


def mirror_module_tree(module: Path, destination: Path) -> list[Path]:
    """Preserve normal Rust child-module lookup without changing source bytes.

    An absolute #[path] include changed the lookup base of the split ANN
    module, so rustc searched query/execution.rs instead of
    query/ann_shards/execution.rs. Mirror the actual tree under the harness's
    inline `extracted` module and use a normal `mod ann_shards` declaration.
    """
    paths = [module, *sorted(module.with_suffix("").rglob("*.rs"))]
    for path in paths:
        target = destination / path.relative_to(module.parent)
        target.parent.mkdir(parents=True, exist_ok=True)
        content = path.read_bytes()
        target.write_bytes(content)
        if target.read_bytes() != content:
            raise RuntimeError(f"Source mirror differs from {path}")
    return paths


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("cass_root", type=Path)
    parser.add_argument("frankensearch_root", type=Path)
    parser.add_argument("destination", type=Path)
    args = parser.parse_args()
    root, upstream, dest = (
        path.resolve()
        for path in (args.cass_root, args.frankensearch_root, args.destination)
    )
    if any(dest == checkout or checkout in dest.parents for checkout in (root, upstream)):
        parser.error("destination must be outside both source checkouts")
    generator = root / "scripts/verify_message_topk_fsvi.py"
    subprocess.run(
        [sys.executable, str(generator), str(root), str(upstream), str(dest)],
        check=True,
    )
    spec = importlib.util.spec_from_file_location("exact_fsvi", generator)
    if spec is None or spec.loader is None:
        raise RuntimeError("Cannot load existing source extractor")
    helper = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(helper)
    query = (root / "src/search/query.rs").read_text()
    reporting = root / "src/search/ann_index.rs"
    source_path = dest / "src/lib.rs"
    source = source_path.read_text()
    vector = (root / "src/search/vector_index.rs").read_text()
    directory = next(
        line for line in vector.splitlines()
        if line.startswith("pub const VECTOR_INDEX_DIR:")
    )
    # Include the complete production reporting module, including Serialize and
    # its exact fallback receipt. Do not substitute harness-only reporting types.
    source = once(
        source,
        "mod search { pub mod vector_index {",
        "mod search { #[path = " + json.dumps(str(reporting))
        + "] pub mod ann_index;\npub mod vector_index {\n" + directory + "\n",
    )
    # ann_index.rs imports `frankensearch::index` at module scope, where a
    # crate-root alias module is not in scope. Link the real upstream facade
    # (see the manifest below) instead, so every `frankensearch::` path
    # resolves exactly as it does in CASS.
    source = once(
        source,
        "// Namespace aliases point at the real upstream trait, not a fixture trait.\n"
        "mod frankensearch { pub mod core { pub use frankensearch_core::filter; } }\n",
        "",
    )
    source = once(
        source,
        "struct SemanticCandidateRetryState {",
        "#[derive(Debug, Default)]\nstruct SemanticCandidateRetryState {",
    )
    source = once(
        source,
        "mod extracted {\nuse super::*;",
        "mod extracted {\nuse super::*;\nuse frankensearch_index::{HnswIndex as FsHnswIndex, HNSW_DEFAULT_EF_SEARCH as FS_HNSW_DEFAULT_EF_SEARCH};",
    )
    failure = "#[derive(Debug)]\n" + helper.block(query, r"struct SemanticAnnOpenFailure\b")
    loader = helper.function(query, "open_fs_semantic_ann_index")
    multiplier = next(
        line for line in query.splitlines()
        if line.startswith("const ANN_CANDIDATE_MULTIPLIER:")
    )
    module = root / "src/search/query/ann_shards.rs"
    module_paths = mirror_module_tree(module, dest / "src/extracted")
    extra = failure + loader + multiplier + "\nmod ann_shards;\n"
    if not source.endswith("}\n"):
        raise ValueError("Expected the existing extracted module's closing brace")
    source = source[:-2] + extra + "}\n"
    source_path.write_text(source)
    manifest_path = dest / "Cargo.toml"
    manifest = manifest_path.read_text()
    old = next(
        line for line in manifest.splitlines()
        if line.startswith("frankensearch-index =")
    )
    facade = (
        "frankensearch = { path = " + json.dumps(str(upstream / "frankensearch"))
        + ', default-features = false, features = ["ann"] }'
    )
    manifest = once(
        manifest, old,
        old.replace("default-features = false", 'default-features = false, features = ["ann"]')
        + "\n" + facade
        + f'\nwalkdir = "={locked_version(root, "walkdir")}"',
    )
    manifest_path.write_text(
        manifest + '\n[dependencies.serde]\nversion = "1"\nfeatures = ["derive"]\n'
    )
    recorded_paths = [
        *module_paths,
        root / "src/search/query.rs",
        root / "src/search/vector_index.rs",
        root / "src/search/ann_index.rs",
    ]
    receipt = {
        "scope": "actual CASS all-shard admission and merge with native HNSW; not full CASS or model/CLI qualification",
        "source_sha256": {
            str(path.relative_to(root)): hashlib.sha256(path.read_bytes()).hexdigest()
            for path in recorded_paths
        },
        "loader_sha256": hashlib.sha256(loader.encode()).hexdigest(),
        "harness_sha256": hashlib.sha256(source.encode()).hexdigest(),
    }
    (dest / "ann-source-receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
    print(json.dumps(receipt, indent=2))


if __name__ == "__main__":
    main()
