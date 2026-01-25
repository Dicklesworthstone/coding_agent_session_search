//! Search layer facade.
//!
//! This module provides the search infrastructure for cass, including:
//!
//! - **[`query`]**: Query parsing, execution, and caching for Tantivy-based full-text search.
//! - **[`tantivy`]**: Tantivy index creation, schema management, and document indexing.
//! - **[`embedder`]**: Embedder trait for semantic search (hash and ML implementations).
//! - **[`hash_embedder`]**: FNV-1a feature hashing embedder (deterministic fallback).
//! - **[`fastembed_embedder`]**: FastEmbed-backed ML embedder (MiniLM).
//! - **[`reranker`]**: Reranker trait for cross-encoder reranking of search results.
//! - **[`fastembed_reranker`]**: FastEmbed-backed cross-encoder reranker (ms-marco-MiniLM-L-6-v2).
//! - **[`model_manager`]**: Semantic model detection + context wiring (no downloads).
//! - **[`model_download`]**: Model download system with consent, verification, and atomic install.
//! - **[`canonicalize`]**: Text preprocessing for consistent embedding input.

pub mod canonicalize;
pub mod embedder;
pub mod fastembed_embedder;
pub mod fastembed_reranker;
pub mod hash_embedder;
pub mod model_download;
pub mod model_manager;
pub mod query;
pub mod reranker;
pub mod tantivy;
pub mod vector_index;
