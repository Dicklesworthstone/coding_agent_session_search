//! Fuzz target for the human-readable time parser.
//!
//! `parse_time_input` is exercised by config wizards, dashboard filters,
//! and other UI surfaces that accept free-text time expressions
//! ("-7d", "yesterday", "2024-11-25", "30 days ago", unix timestamps).
//! Bead `coding_agent_session_search-vmtms` pinned the totality
//! invariant via a finite regression vector list; this target uses
//! coverage-guided fuzzing to explore the prefix-stripping,
//! char-iteration, and chrono parser interactions far more
//! exhaustively. The function MUST NEVER panic — only return None
//! or Some(i64). Bead: `coding_agent_session_search-4znjn`.

#![no_main]

use coding_agent_search::ui::time_parser::parse_time_input;
use libfuzzer_sys::fuzz_target;

const MAX_INPUT_BYTES: usize = 8 * 1024;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    let Ok(input) = std::str::from_utf8(data) else {
        return;
    };

    let _ = parse_time_input(input);
});
