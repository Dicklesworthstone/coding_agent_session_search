#!/usr/bin/env python3
"""Build an isolated check of exact CASS selection against pinned real FSVI.

Copies exact source definitions, not a rewritten selection model. The complete
CASS dependency graph, model setup, hydration, and CLI remain a separate gate.
No files under either source checkout are edited by this extraction.
"""
from __future__ import annotations
import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import tomllib

FS_REVISION = '6852e79a830c402ea4e55eb79eda3373c618dfc3'


def block(text: str, header: str, indent: str = '') -> str:
    pattern = '^' + re.escape(indent) + header
    matches = list(re.finditer(pattern, text, re.MULTILINE))
    if len(matches) != 1:
        raise ValueError(f'Expected one source definition for {header!r}, got {len(matches)}')
    match = matches[0]
    closing = re.search('^' + re.escape(indent) + r'}$', text[match.end():], re.MULTILINE)
    if closing is None:
        raise ValueError(f'No source definition terminator for {header!r}')
    return text[match.start():match.end() + closing.end()] + '\n'


def function(text: str, name: str, indent: str = '') -> str:
    return block(text, r'(?:pub(?:\([^)]*\))? )?fn ' + re.escape(name) + r'\b', indent)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('cass_root', type=Path)
    parser.add_argument('frankensearch_root', type=Path)
    parser.add_argument('destination', type=Path)
    args = parser.parse_args()
    root = args.cass_root.resolve()
    upstream = args.frankensearch_root.resolve()
    destination = args.destination.resolve()
    actual = subprocess.check_output(['git', '-C', str(upstream), 'rev-parse', 'HEAD'], text=True).strip()
    if actual != FS_REVISION:
        raise SystemExit(f'Expected pinned FSVI source {FS_REVISION}, got {actual}')
    query = (root / 'src/search/query.rs').read_text()
    vector = (root / 'src/search/vector_index.rs').read_text()
    scope = (root / 'src/search/query/session_scope.rs').read_text()
    snippets: dict[str, str] = {}
    for name in ['VectorSearchResult', 'SemanticDocId', 'SemanticDocIdFilterView',
                 'SemanticFilter', 'SemanticFilterMaps', 'SemanticIndexArtifact']:
        # Only derives needed by these original definitions are added here;
        # the struct fields and every executable body are copied verbatim.
        snippets[name] = '#[derive(Debug, Clone)]\n' + block(vector, r'pub(?:\([^)]*\))? struct ' + name + r'\b')
    snippets['ann_reason'] = '#[derive(Debug, Clone, Copy)]\n' + block(vector, r'pub enum SemanticAnnUnavailableReason\b')
    snippets['ann_codes'] = block(vector, r'impl SemanticAnnUnavailableReason\b')
    snippets['doc_encode'] = block(vector, r'impl SemanticDocId\b')
    snippets['artifact'] = block(vector, r'impl SemanticIndexArtifact\b')
    for name in ['reject_final_component_symlink', 'parse_semantic_doc_id', 'parse_semantic_doc_id_filter_view']:
        snippets[name] = function(vector, name)
    snippets['metadata_filter'] = block(vector, r'impl frankensearch::core::filter::SearchFilter for SemanticFilter\b')
    maps_impl = block(vector, r'impl SemanticFilterMaps\b')
    snippets['filter_maps_fixture_constructor'] = 'impl SemanticFilterMaps {\n' + function(maps_impl, 'for_tests', '    ') + '}\n'
    snippets['candidate_context'] = block(query, r'struct SemanticCandidateContext\b')
    snippets['retry_state'] = block(query, r'struct SemanticCandidateRetryState\b')
    snippets['refill_filter'] = block(query, r'struct ExactMessageRefillFilter\b') + block(query, r'impl FsSearchFilter for ExactMessageRefillFilter')
    snippets['session_filter'] = block(scope, r'pub\(super\) struct SessionScopedSemanticFilter\b') + block(scope, r'impl FsSearchFilter for SessionScopedSemanticFilter')
    methods = ['collapse_semantic_results', 'semantic_exact_candidate_limit',
               'semantic_window_may_omit_competitor', 'record_fs_semantic_hit',
               'search_exact_semantic_indexes_initial_window', 'search_exact_semantic_indexes']
    for name in methods:
        snippets[name] = function(query, name, '    ')
    multiplier = re.findall(r'^const SEMANTIC_EXACT_CHUNK_OVERFETCH_MULTIPLIER: usize = [0-9_]+;$', query, re.MULTILINE)
    if len(multiplier) != 1:
        raise ValueError('Missing exact source candidate multiplier')
    source = '''#![allow(dead_code, unused_imports)]
use anyhow::{Context, Result, anyhow, bail};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use frankensearch_index::VectorIndex;
use frankensearch_index::VectorIndex as FsVectorIndex;
use frankensearch_core::{VectorHit as FsVectorHit, filter::SearchFilter as FsSearchFilter};
// Namespace aliases point at the real upstream trait, not a fixture trait.
mod frankensearch { pub mod core { pub use frankensearch_core::filter; } }
mod search { pub mod vector_index {
    pub(crate) use crate::extracted::parse_semantic_doc_id_filter_view;
    pub use frankensearch_index::Quantization;
} }
mod extracted {
use super::*;
'''
    source += '\n'.join(value for key, value in snippets.items() if key not in methods)
    source += multiplier[0] + '\nstruct SearchClient;\nimpl SearchClient {\n'
    source += ''.join(snippets[name] for name in methods) + '}\n'
    for name in ['message_topk', 'message_topk_integration']:
        path = root / f'src/search/query/{name}.rs'
        source += ('#[cfg(test)]\n' if name == 'message_topk_integration' else '') + '#[path = ' + json.dumps(str(path)) + f']\nmod {name};\n'
    source += '}\n'
    manifest = f'''[package]
name = "cass-exact-message-fsvi-check"
version = "0.0.0"
edition = "2024"
[dependencies]
anyhow = "=1.0.102"
tracing = "=0.1.44"
serde_json = "1"
hex = "=0.4.3"
itoa = "=1.0.18"
same-file = "=1.0.6"
tempfile = "=3.27.0"
frankensearch-index = {{ path = {json.dumps(str(upstream / 'crates/frankensearch-index'))}, default-features = false }}
frankensearch-core = {{ path = {json.dumps(str(upstream / 'crates/frankensearch-core'))}, default-features = false }}
[profile.test]
debug = 0
[profile.test.package."*"]
opt-level = 1
'''
    destination.mkdir(parents=True, exist_ok=False)
    (destination / 'src').mkdir()
    (destination / 'src/lib.rs').write_text(source)
    (destination / 'Cargo.toml').write_text(manifest)
    channel = tomllib.loads((root / 'rust-toolchain.toml').read_text())['toolchain']['channel']
    (destination / 'rust-toolchain.toml').write_text(f'[toolchain]\nchannel = "{channel}"\n')
    receipt = {'upstream_revision': actual, 'toolchain': channel,
               'scope': 'exact CASS static selection methods, original filters and retained artifact constructor with real upstream FSVI; not the full CASS binary',
               'snippets_sha256': {name: hashlib.sha256(value.encode()).hexdigest() for name, value in snippets.items()},
               'harness_sha256': hashlib.sha256(source.encode()).hexdigest()}
    (destination / 'source-receipt.json').write_text(json.dumps(receipt, indent=2) + '\n')
    print(json.dumps(receipt, indent=2))


if __name__ == '__main__':
    main()
