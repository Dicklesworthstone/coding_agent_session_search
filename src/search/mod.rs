//! Search layer facade.
//!
//! This module provides the search infrastructure for cass, including:
//!
//! - **[`query`]**: Query parsing, execution, and caching for Tantivy-based full-text search.
//! - **[`tantivy`]**: Tantivy index creation, schema management, and document indexing.
//! - **[`embedder`]**: Embedder trait for semantic search (hash and ML implementations).
//! - **[`hash_embedder`]**: FNV-1a feature hashing embedder (deterministic fallback).
//! - **[`canonicalize`]**: Text preprocessing for consistent embedding input.

pub mod canonicalize;
pub mod embedder;
pub mod hash_embedder;
pub mod query;
pub mod tantivy;
// pub mod vector_index; // Missing from repo
