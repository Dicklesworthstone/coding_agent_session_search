//! Run the production canary's real-index regressions without compiling the
//! unrelated CLI unit-test monolith. No alternate query or fake engine is used.
pub use coding_agent_search::search;

#[path = "../src/indexer/lexical_reconcile/canary.rs"]
mod canary;
