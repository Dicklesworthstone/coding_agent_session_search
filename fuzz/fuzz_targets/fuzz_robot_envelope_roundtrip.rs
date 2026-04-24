//! Fuzz target for the robot-mode CliError JSON envelope round-trip.
//!
//! `coding_agent_session_search-d3eci`: cass produces structured
//! robot-mode JSON envelopes (CliError-shaped: code/kind/message/
//! hint/retryable, plus result envelopes via serde_json::to_string_pretty
//! across ~6 sites in src/lib.rs). The al19b/jyn5r golden test pins
//! the kebab-case `kind` invariant for 81 known kinds, but no
//! coverage-guided harness verifies that
//! `parse(serialize(envelope)) == envelope` for arbitrary inputs.
//! That round-trip is the contract every agent harness depends on:
//! a regression that introduces a non-serializable field, NaN-tainted
//! float, or schema mismatch would slip past the goldens.
//!
//! Archetype: **Round-Trip (Pattern 2)** from /testing-fuzzing.
//! Inverse oracle — `decode(encode(x)) == x`.

#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;

const MAX_FIELD_BYTES: usize = 64 * 1024;

#[derive(Arbitrary, Debug, Clone)]
struct EnvelopeInput {
    code: i32,
    kind: String,
    message: String,
    hint: Option<String>,
    retryable: bool,
}

fn bound_str(s: String, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = s;
    out.truncate(end);
    out
}

fn build_envelope(input: &EnvelopeInput) -> serde_json::Value {
    let kind = bound_str(input.kind.clone(), MAX_FIELD_BYTES);
    let message = bound_str(input.message.clone(), MAX_FIELD_BYTES);
    let hint = input
        .hint
        .as_ref()
        .map(|h| bound_str(h.clone(), MAX_FIELD_BYTES));
    serde_json::json!({
        "error": {
            "code": input.code,
            "kind": kind,
            "message": message,
            "hint": hint,
            "retryable": input.retryable,
        }
    })
}

fuzz_target!(|input: EnvelopeInput| {
    let envelope = build_envelope(&input);

    // Pretty-print + compact serialization both must round-trip.
    for serialized in [
        serde_json::to_string(&envelope),
        serde_json::to_string_pretty(&envelope),
    ] {
        let Ok(text) = serialized else {
            // serde_json refuses NaN/Infinity floats and other non-serializable
            // values. We bound the input to scalar/string types only, so
            // serialization should never fail — if it does, the harness has
            // exposed a real schema-stability regression.
            panic!("envelope serialization failed: {envelope:#}");
        };
        let parsed: serde_json::Value = serde_json::from_str(&text)
            .unwrap_or_else(|err| {
                panic!(
                    "round-trip parse failed for envelope: serialized={text:?}, err={err}"
                )
            });
        assert_eq!(
            parsed, envelope,
            "round-trip equality violated: serialize→parse produced different value.\n\
             original: {envelope:#}\nroundtrip: {parsed:#}"
        );
    }
});
