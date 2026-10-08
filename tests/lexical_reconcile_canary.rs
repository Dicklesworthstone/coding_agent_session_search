//! Run the production canary's real-index regressions without compiling the
//! unrelated CLI unit-test monolith. No alternate query or fake engine is used.
pub use coding_agent_search::search;

// Preserve both the production module ancestry used by pub(in ...) and the
// ordinary file-module lookup that finds canary/exact.rs. Loading canary.rs
// directly with #[path] made its child resolve beside, not inside, canary/.
#[path = "../src/indexer"]
mod indexer {
    mod lexical_reconcile {
        mod canary;
    }
}
