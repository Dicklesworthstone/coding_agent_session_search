//! Search layer facade.
//!
//! This module provides the search infrastructure for cass, including:
//!
//! - **[`query`]**: Query parsing, execution, and caching for Tantivy-based full-text search.
//! - **[`tantivy`]**: Tantivy index creation, schema management, and document indexing.
//! - **[`embedder`]**: Embedder trait for semantic search (hash and ML implementations).
//! - **[`embedder_registry`]**: Embedder registry for model selection (bd-2mbe).
//! - **[`hash_embedder`]**: FNV-1a feature hashing embedder (deterministic fallback).
//! - **[`fastembed_embedder`]**: FastEmbed-backed ML embedder (MiniLM).
//! - **[`reranker`]**: Reranker trait for cross-encoder reranking of search results.
//! - **[`reranker_registry`]**: Reranker registry for model selection with bake-off support.
//! - **[`fastembed_reranker`]**: FastEmbed-backed cross-encoder reranker (ms-marco-MiniLM-L-6-v2).
//! - **[`daemon_client`]**: Daemon client wrappers for warm embedder/reranker (bd-1lps).
//! - **[`model_manager`]**: Semantic model detection + context wiring (no downloads).
//! - **[`model_download`]**: Model download system with consent, verification, and atomic install.
//! - **[`canonicalize`]**: Text preprocessing for consistent embedding input.
//! - **[`ann_index`]**: HNSW-based approximate nearest neighbor index (Opt 9).
//! - **[`two_tier_search`]**: Two-tier progressive search with fast/quality embeddings (bd-3dcw).

pub mod ann_index;
pub mod canonicalize;
pub mod daemon_client;
pub mod embedder;
pub mod embedder_registry;
pub mod fastembed_embedder;
pub mod fastembed_reranker;
pub mod hash_embedder;
pub mod model_download;
pub mod model_manager;
pub mod query;
pub mod reranker;
pub mod reranker_registry;
pub mod tantivy;
pub mod two_tier_search;
pub mod vector_index;
