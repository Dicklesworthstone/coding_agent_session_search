#!/usr/bin/env python3
"""Compile exact CASS shard builder/manifest methods against real FSVI/HNSW.

The harness uses the actual CASS hash producer and real storage components.
It does not qualify the full CASS dependency graph, native ML, or CLI routing.
No source checkout is edited. Optional --indexer-source supports RED evidence.
"""
from __future__ import annotations
import argparse, hashlib, importlib.util, json, re, subprocess, tomllib
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('cass_root', type=Path)
    parser.add_argument('frankensearch_root', type=Path)
    parser.add_argument('destination', type=Path)
    parser.add_argument('--indexer-source', type=Path)
    args = parser.parse_args()
    root, upstream, dest = (x.resolve() for x in (args.cass_root, args.frankensearch_root, args.destination))
    spec = importlib.util.spec_from_file_location('extract', root / 'scripts/verify_message_topk_fsvi.py')
    if spec is None or spec.loader is None: raise RuntimeError('source extractor unavailable')
    helper = importlib.util.module_from_spec(spec); spec.loader.exec_module(helper)
    revision = subprocess.check_output(['git','-C',str(upstream),'rev-parse','HEAD'], text=True).strip()
    if revision != helper.FS_REVISION: raise ValueError('Unexpected upstream revision')
    block, function = helper.block, helper.function
    indexer_path = args.indexer_source or root / 'src/indexer/semantic.rs'
    indexer = indexer_path.read_text()
    manifest = (root / 'src/search/semantic_manifest.rs').read_text()
    vector = (root / 'src/search/vector_index.rs').read_text()
    policy = (root / 'src/search/policy.rs').read_text()
    snippets = {}
    def retain(name, source):
        snippets[name] = source
        return source
    def const(text, name):
        matches = re.findall(r'^pub const ' + re.escape(name) + r':[^;]+;', text, re.M)
        if len(matches) != 1: raise ValueError('Missing/ambiguous constant '+name)
        return matches[0]+'\n'
    preamble = '''#![allow(dead_code, unused_imports)]
extern crate self as frankensearch;
pub use frankensearch_core::{SearchError, SearchResult, SyncEmbed, ModelCategory, ModelTier};
pub use frankensearch_embed::{HashAlgorithm, HashEmbedder};
pub mod core { pub use frankensearch_core::*; }
mod search { pub mod embedder {
    pub use frankensearch_core::{SyncEmbed as Embedder, SearchError as EmbedderError, SearchResult as EmbedderResult};
}
'''
    preamble += '#[path = '+json.dumps(str(root/'src/search/hash_embedder.rs'))+'] pub mod hash_embedder;\n}\n'
    m = '''mod manifest {
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf, Component};
use std::time::{SystemTime, UNIX_EPOCH};
use serde::{Serialize, Deserialize};
use ring::rand::{SecureRandom, SystemRandom};
'''
    for name in ['MANIFEST_FORMAT_VERSION','MANIFEST_FILENAME','SHARD_MANIFEST_FILENAME']:
        m += retain(name, const(manifest,name))
    for name in ['SEMANTIC_SCHEMA_VERSION','CHUNKING_STRATEGY_VERSION']:
        m += retain(name, const(policy,name))
    m += retain('TierKind', '#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]\n#[serde(rename_all="snake_case")]\n'+block(manifest,r'pub enum TierKind\b')+block(manifest,r'impl TierKind\b'))
    for name in ['SemanticShardRecord','SemanticShardSummary','SemanticShardManifest']:
        derive = '#[derive(Debug, Clone, PartialEq, Serialize, Deserialize'+(', Default' if name=='SemanticShardSummary' else '')+')]\n'
        m += retain(name,derive+block(manifest,r'pub struct '+name+r'\b'))
    m += retain('manifest_default',block(manifest,r'impl Default for SemanticShardManifest\b'))
    m += 'impl SemanticShardRecord {\n'+retain('matches_generation',function(block(manifest,r'impl SemanticShardRecord\b'),'matches_generation','    '))+'}\n'
    methods = block(manifest,r'impl SemanticShardManifest\b')
    m += 'impl SemanticShardManifest {\n'
    for name in ['path','load','load_or_default','save','replace_shards_for_generation','summary','mark_shards_stale_for_embedder']:
        m += retain('manifest_'+name,function(methods,name,'    '))
    m += '}\n'
    m += retain('ManifestError','#[derive(Debug)]\n'+block(manifest,r'pub enum ManifestError\b')+block(manifest,r'impl std::fmt::Display for ManifestError\b')+'impl std::error::Error for ManifestError {}\n')
    for name in ['semantic_shard_artifact_path_is_safe','now_ms','unique_manifest_temp_path','create_unique_manifest_temp_file','random_manifest_path_nonce','replace_file_from_temp','sync_parent_directory']:
        m += retain('manifest_'+name,function(manifest,name))
    # Linux qualification uses the exact Unix implementation; no shim is used.
    unix = manifest[:manifest.index('#[cfg(windows)]\nfn sync_directory')]
    m += retain('manifest_sync_directory',function(unix,'sync_directory'))
    m += '}\n'
    source = preamble+m+'''mod production {
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use anyhow::{Context, Result, bail};
use frankensearch_index::{VectorIndex as FsVectorIndex, VectorIndexWriter as FsVectorIndexWriter, Quantization as FsQuantization, HnswIndex as FsHnswIndex, HnswConfig as FsHnswConfig, HNSW_DEFAULT_M as FS_HNSW_DEFAULT_M, HNSW_DEFAULT_EF_CONSTRUCTION as FS_HNSW_DEFAULT_EF_CONSTRUCTION};
use ring::digest::{self, SHA256};
use crate::search::hash_embedder::HashEmbedder;
use crate::search::embedder::Embedder;
use crate::manifest::*;
'''
    for name in ['VECTOR_INDEX_DIR','ROLE_USER']: source += retain(name,const(vector,name))
    for name in ['SEMANTIC_SCHEMA_VERSION','CHUNKING_STRATEGY_VERSION']: source += const(policy,name)
    source += retain('SemanticDocId','#[derive(Debug, Clone, Copy)]\n'+block(vector,r'pub struct SemanticDocId\b')+block(vector,r'impl SemanticDocId\b'))
    for name in ['EmbeddedMessage','SemanticShardBuildPlan','SemanticShardBuildOutcome']:
        source += retain(name,'#[derive(Debug, Clone)]\n'+block(indexer,r'pub struct '+name+r'\b'))
    source += retain('SemanticIndexer',block(indexer,r'pub struct SemanticIndexer\b'))
    names = ['now_ms','safe_path_component','semantic_generation_fingerprint_component','semantic_shard_index_path','semantic_shard_ann_index_path','semantic_doc_id_for_embedded','validate_embedding_vector','expected_vector_space_revision']
    names += ['create_semantic_shard_generation_dir' if 'fn create_semantic_shard_generation_dir(' in indexer else 'semantic_shard_generation_dir']
    for name in names: source += retain('indexer_'+name,function(indexer,name))
    unix = indexer[:indexer.index('#[cfg(windows)]\nfn sync_parent_directory')]
    source += retain('indexer_sync_parent_directory',function(unix,'sync_parent_directory'))
    source += retain('HASH_VECTOR_SPACE_REVISION',const(indexer,'HASH_VECTOR_SPACE_REVISION'))
    native = (root/'src/search/fastembed_embedder.rs').read_text()
    for name in ['MINILM_VECTOR_SPACE_REVISION','MULTILINGUAL_MINILM_VECTOR_SPACE_REVISION']: source += retain(name,const(native,name))
    source += retain('content_hash',function((root/'src/search/canonicalize.rs').read_text(),'content_hash'))
    source += 'impl SemanticIndexer {\n'
    for name in ['embedder_id','embedder_dimension','vector_space_revision','build_and_save_index_shards','write_semantic_shard','build_and_save_index_at_path','build_and_save_index_at_path_with_progress']:
        source += retain('indexer_'+name,function(indexer,name,'    '))
    source += '}\n#[cfg(test)]\n#[path = '+json.dumps(str(root/'src/indexer/semantic/shard_publication_tests.rs'))+']\nmod shard_publication_tests;\n}\n'
    dest.mkdir(parents=True,exist_ok=False); (dest/'src').mkdir()
    (dest/'src/lib.rs').write_text(source)
    dependencies = f'''[package]
name = "cass-shard-publication-check"
version = "0.0.0"
edition = "2024"
[dependencies]
anyhow = "=1.0.102"
tracing = "=0.1.44"
serde = {{ version = "1", features = ["derive"] }}
serde_json = "1"
ring = "0.17"
blake3 = "1"
hex = "0.4"
itoa = "1"
tempfile = "=3.27.0"
frankensearch-core = {{ path = {json.dumps(str(upstream/'crates/frankensearch-core'))}, default-features = false }}
frankensearch-embed = {{ path = {json.dumps(str(upstream/'crates/frankensearch-embed'))}, default-features = false }}
frankensearch-index = {{ path = {json.dumps(str(upstream/'crates/frankensearch-index'))}, default-features = false, features = ["ann"] }}
[profile.test]
debug = 0
[profile.test.package."*"]
opt-level = 1
'''
    (dest/'Cargo.toml').write_text(dependencies)
    channel = tomllib.loads((root/'rust-toolchain.toml').read_text())['toolchain']['channel']
    (dest/'rust-toolchain.toml').write_text('[toolchain]\nchannel = '+json.dumps(channel)+'\n')
    receipt = {'scope':'exact CASS shard writer/manifest methods, CASS hash control producer, real pinned FSVI/HNSW; not full CASS or native ML', 'upstream':revision,'indexer_source_sha256':hashlib.sha256(indexer.encode()).hexdigest(),'snippets_sha256':{k:hashlib.sha256(v.encode()).hexdigest() for k,v in snippets.items()},'harness_sha256':hashlib.sha256(source.encode()).hexdigest()}
    (dest/'source-receipt.json').write_text(json.dumps(receipt,indent=2)+'\n'); print(json.dumps(receipt,indent=2))

if __name__ == '__main__': main()
