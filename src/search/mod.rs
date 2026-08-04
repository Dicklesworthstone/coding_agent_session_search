//! Search layer facade.
//!
//! This module provides the search infrastructure for cass, including:
//!
//! - **[`query`]**: Query parsing, execution, and caching for Tantivy-based full-text search.
//! - **[`tantivy`]**: Tantivy index creation, schema management, and document indexing.
//! - **[`embedder`]**: Embedder trait for semantic search (hash and ML implementations).
//! - **[`embedder_registry`]**: Embedder registry for model selection (bd-2mbe).
//! - **[`hash_embedder`]**: FNV-1a feature hashing embedder (explicit degraded mode).
//! - **[`fastembed_embedder`]**: Pure-Rust native MiniLM embedder (historical module name).
//! - **[`reranker`]**: Reranker trait for cross-encoder reranking of search results.
//! - **[`reranker_registry`]**: Reranker registry for model selection with bake-off support.
//! - **[`fastembed_reranker`]**: Pure-Rust native cross-encoder reranker (historical module name).
//! - **[`daemon_client`]**: Daemon client wrappers for warm embedder/reranker (bd-1lps).
//! - **[`model_manager`]**: Semantic model detection + context wiring (no downloads).
//! - **[`model_download`]**: Model download system with consent, verification, and atomic install.
//! - **[`policy`]**: Semantic policy contract: model defaults, tiers, budgets, invalidation.
//! - **[`semantic_manifest`]**: Durable semantic asset manifests, backlog ledger, and checkpoints.
//! - **[`canonicalize`]**: Text preprocessing for consistent embedding input.
//! - **[`ann_index`]**: HNSW-based approximate nearest neighbor index (Opt 9).
//! - **[`two_tier_search`]**: Two-tier progressive search with fast/quality embeddings (bd-3dcw).
//! - **[`pack_planner`]**: Deterministic answer-pack evidence selection core.

pub mod ann_index;
pub mod asset_state;
pub(crate) mod bounded_discovery;
pub mod canonicalize;
pub(crate) mod command_envelope;
pub(crate) mod contention_diagnostics;
pub mod daemon_client;
pub(crate) mod drill_down;
pub(crate) mod e2e_scenarios;
pub mod embedder;
pub mod embedder_registry;
pub mod fastembed_embedder;
pub mod fastembed_reranker;
pub(crate) mod fleet_cheap_probes;
pub mod hash_embedder;
pub(crate) mod human_readiness_summary;
pub(crate) mod incident_categories;
pub(crate) mod incident_redaction;
pub(crate) mod liveness_fixtures;
pub(crate) mod model_acquisition;
pub mod model_download;
pub mod model_manager;
pub mod pack_planner;
pub mod policy;
pub(crate) mod progress_contract;
pub(crate) mod proof_log;
pub(crate) mod quarantine_status;
pub mod query;
pub(crate) mod readiness;
pub(crate) mod readiness_fixtures;
pub(crate) mod readiness_projection;
pub(crate) mod recovery_journeys;
pub(crate) mod regression_corpus;
pub mod reranker;
pub mod reranker_registry;
pub mod runtime_optimizations;
pub(crate) mod salvage_ledger;
pub(crate) mod search_mode_metadata;
pub mod semantic_manifest;
pub(crate) mod semantic_publish_safety;
pub(crate) mod semantic_readiness;
pub(crate) mod source_provenance;
pub(crate) mod storage_integrity;
pub(crate) mod storage_salvage;
pub mod tantivy;
pub mod trust_correlation;
pub mod trust_scoring;
pub mod two_tier_search;
pub mod vector_index;
pub(crate) mod watch_exit_envelope;
pub(crate) mod watch_recovery;
pub(crate) mod workspace_source_fixtures;
pub(crate) mod zero_result_diagnosis;
