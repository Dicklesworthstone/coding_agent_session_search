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


def once(text: str, old: str, new: str) -> str:
    if text.count(old) != 1:
        raise ValueError(f"Expected one extraction anchor: {old!r}")
    return text.replace(old, new, 1)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("cass_root", type=Path)
    parser.add_argument("frankensearch_root", type=Path)
    parser.add_argument("destination", type=Path)
    args = parser.parse_args()
    root, upstream, dest = (path.resolve() for path in (args.cass_root, args.frankensearch_root, args.destination))
    generator = root / "scripts/verify_message_topk_fsvi.py"
    subprocess.run([sys.executable, str(generator), str(root), str(upstream), str(dest)], check=True)
    spec = importlib.util.spec_from_file_location("exact_fsvi", generator)
    if spec is None or spec.loader is None:
        raise RuntimeError("Cannot load existing source extractor")
    helper = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(helper)
    query = (root / "src/search/query.rs").read_text()
    reporting = (root / "src/search/ann_index.rs").read_text()
    source_path = dest / "src/lib.rs"
    source = source_path.read_text()
    stats = "#[derive(Debug, Clone, Default)]\n" + helper.block(reporting, r"pub struct AnnSearchStats\b")
    source = once(source, "mod search { pub mod vector_index {", "mod search { pub mod ann_index {\n" + stats + "}\npub mod vector_index {")
    source = once(source, "mod frankensearch { pub mod core {", "mod frankensearch { pub mod index { pub use frankensearch_index::*; } pub mod core {")
    source = once(source, "struct SemanticCandidateRetryState {", "#[derive(Debug, Default)]\nstruct SemanticCandidateRetryState {")
    source = once(source, "mod extracted {\nuse super::*;", "mod extracted {\nuse super::*;\nuse frankensearch_index::{HnswIndex as FsHnswIndex, HNSW_DEFAULT_EF_SEARCH as FS_HNSW_DEFAULT_EF_SEARCH};")
    failure = "#[derive(Debug)]\n" + helper.block(query, r"struct SemanticAnnOpenFailure\b")
    loader = helper.function(query, "open_fs_semantic_ann_index")
    multiplier = next(line for line in query.splitlines() if line.startswith("const ANN_CANDIDATE_MULTIPLIER:"))
    module = root / "src/search/query/ann_shards.rs"
    extra = failure + loader + multiplier + "\n#[path = " + json.dumps(str(module)) + "]\nmod ann_shards;\n"
    assert source.endswith("}\n")
    source = source[:-2] + extra + "}\n"
    source_path.write_text(source)
    manifest_path = dest / "Cargo.toml"
    manifest = manifest_path.read_text()
    old = next(line for line in manifest.splitlines() if line.startswith("frankensearch-index ="))
    manifest_path.write_text(once(manifest, old, old.replace("default-features = false", 'default-features = false, features = ["ann"]')))
    receipt = {"scope": "actual CASS all-shard admission and merge with native HNSW; not full CASS or model/CLI qualification",
               "source_sha256": {str(path.relative_to(root)): hashlib.sha256(path.read_bytes()).hexdigest()
                                 for path in [module, root / "src/search/query.rs", root / "src/search/vector_index.rs", root / "src/search/ann_index.rs"]},
               "loader_sha256": hashlib.sha256(loader.encode()).hexdigest(),
               "harness_sha256": hashlib.sha256(source.encode()).hexdigest()}
    (dest / "ann-source-receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
    print(json.dumps(receipt, indent=2))


if __name__ == "__main__":
    main()
