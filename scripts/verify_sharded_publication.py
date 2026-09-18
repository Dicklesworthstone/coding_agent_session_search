#!/usr/bin/env python3
"""Compile the actual immutable manifest and publication reader in isolation.

Copies complete production modules with their normal child-module layout. Only
CASS's DB-independent vector ID/filter definitions are extracted verbatim. This
separate dependency graph is not a full CASS, model, or archive-scale gate.
"""
from __future__ import annotations
import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import shutil
import subprocess

FS_REVISION = '6852e79a830c402ea4e55eb79eda3373c618dfc3'


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('cass_root', type=Path)
    parser.add_argument('upstream_root', type=Path)
    parser.add_argument('destination', type=Path)
    args = parser.parse_args()
    root, upstream, dest = (p.resolve() for p in (args.cass_root, args.upstream_root, args.destination))
    actual = subprocess.check_output(['git', '-C', str(upstream), 'rev-parse', 'HEAD'], text=True).strip()
    if actual != FS_REVISION:
        raise SystemExit('upstream revision does not match the component check pin')
    dest.mkdir(parents=True, exist_ok=False)
    target = dest / 'src/search'
    target.mkdir(parents=True)
    sources = []
    for name in ['policy', 'semantic_manifest', 'semantic_reader']:
        source = root / f'src/search/{name}.rs'
        shutil.copy2(source, target / source.name)
        sources.append(source)
        child = root / f'src/search/{name}'
        if child.is_dir():
            shutil.copytree(child, target / name)
            sources.extend(child.rglob('*.rs'))
    spec = importlib.util.spec_from_file_location('extract', root / 'scripts/verify_message_topk_fsvi.py')
    if spec is None or spec.loader is None:
        raise RuntimeError('cannot load existing source extractor')
    helper = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(helper)
    vector_file = root / 'src/search/vector_index.rs'
    vector = vector_file.read_text()
    parts = ['use std::collections::HashSet;']
    parts.extend(line for line in vector.splitlines() if line.startswith('pub const ROLE_'))
    for name in ['SemanticDocId', 'SemanticDocIdFilterView', 'SemanticFilter']:
        derive = '#[derive(Debug, Clone, Default)]' if name == 'SemanticFilter' else '#[derive(Debug, Clone, Copy, PartialEq, Eq)]'
        parts.append(derive + '\n' + helper.block(vector, r'pub(?:\([^)]*\))? struct ' + name + r'\b'))
    parts.append(helper.block(vector, r'impl SemanticDocId\b'))
    for name in ['parse_semantic_doc_id', 'parse_semantic_doc_id_filter_view']:
        parts.append(helper.function(vector, name))
    parts.append(helper.block(vector, r'impl frankensearch::core::filter::SearchFilter for SemanticFilter\b'))
    (target / 'vector_index.rs').write_text('\n'.join(parts))
    (dest / 'src/lib.rs').write_text('pub mod search { pub mod policy; pub mod vector_index; pub mod semantic_manifest; pub mod semantic_reader; }\n')
    (dest / 'Cargo.toml').write_text(
        '[package]\nname="cass-sharded-publication-check"\nversion="0.0.0"\nedition="2024"\n'
        '[dependencies]\nfrankensearch={path=' + json.dumps(str(upstream / 'frankensearch')) + ', default-features=false, features=["hash", "ann"]}\n'
        'serde={version="1", features=["derive"]}\nserde_json="1"\nthiserror="2"\nsha2="0.10"\nhex="0.4"\nring="0.17"\nfs2="0.4"\ntempfile="3"\ndotenvy="0.15"\ntracing="0.1"\nitoa="1"\n'
        '[dev-dependencies]\nproptest="1"\ntracing-subscriber={version="0.3", features=["env-filter"]}\nwait-timeout="0.2"\n'
        '[profile.test]\ndebug=0\n')
    receipt = {
        'scope': 'complete production manifest, reader, ANN and publication modules; exact ID/filter definitions; not full CASS',
        'upstream': actual,
        'source_sha256': {str(p.relative_to(root)): hashlib.sha256(p.read_bytes()).hexdigest() for p in sources + [vector_file]},
        'extracted_vector_sha256': hashlib.sha256((target / 'vector_index.rs').read_bytes()).hexdigest(),
    }
    (dest / 'source-receipt.json').write_text(json.dumps(receipt, indent=2) + '\n')
    print(json.dumps(receipt, indent=2))


if __name__ == '__main__':
    main()
