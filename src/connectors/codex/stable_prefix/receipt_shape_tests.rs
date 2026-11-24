//! Durable observations may be legacy objects, but a different JSON root
//! cannot establish that a previously recorded prefix is absent.

use super::{set_previous_prefix_receipt, take_previous_prefix_receipt};
use std::path::Path;

#[test]
fn non_object_observations_are_not_absent_prefix_evidence() {
    let path = Path::new("rollout-invalid-observation.jsonl");
    for observation in ["null", "false", "true", "0", r#""broken""#, "[]", "[{}]"] {
        let error = set_previous_prefix_receipt(path, Some(observation))
            .expect_err("a malformed observation root must not discard prior evidence");
        assert!(
            error.to_string().contains("must be a JSON object"),
            "unexpected rejection for {observation}: {error}"
        );
        assert!(take_previous_prefix_receipt(path).unwrap().is_none());
    }
}

#[test]
fn legacy_object_observations_without_prefix_evidence_remain_supported() {
    let path = Path::new("rollout-legacy-observation.jsonl");
    for observation in [
        None,
        Some("{}"),
        Some(r#"{"primary":{},"dependencies":[],"watch_published":false}"#),
    ] {
        set_previous_prefix_receipt(path, observation).unwrap();
        assert!(take_previous_prefix_receipt(path).unwrap().is_none());
    }
}
