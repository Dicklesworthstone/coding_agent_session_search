//! Command-level completion for an index run with retained partial work.
//!
//! Source-local rejection must not discard successful commits, but it must not
//! become a success event, exit zero or an idempotency success either. This
//! boundary consumes the indexer's existing observations; it never opens a DB.

use crate::indexer::ConnectorStats;
use crate::model::cli_error_kind::ErrorKind;
use crate::{CliError, CliResult};
use serde_json::{Value, json};
use std::collections::BTreeSet;

fn incomplete(scan_had_errors: bool, connectors: &[ConnectorStats]) -> bool {
    // Keep legacy connector errors authoritative even when an older caller
    // has not populated the aggregate flag. Discovered-only/skipped sources
    // and warning diagnostics are not, by themselves, failed scans.
    scan_had_errors || connectors.iter().any(|connector| connector.error.is_some())
}

pub(crate) fn require_complete_scan(
    scan_had_errors: bool,
    connectors: &[ConnectorStats],
) -> CliResult {
    if !incomplete(scan_had_errors, connectors) {
        return Ok(());
    }
    Err(CliError {
        code: 9,
        kind: ErrorKind::Index.kind_str(),
        message: "indexing retained completed work, but one or more source scans were incomplete".into(),
        hint: Some("Inspect indexing_stats.connectors for rejected sources and their reasons. Resolve the reported source limitation and rerun indexing; completed source observations remain available for reuse. An oversized rollout is not fixed by retrying unchanged bytes.".into()),
        // The invocation can be retried without rolling back committed work.
        // Individual source errors retain their own remediation requirements.
        retryable: true,
    })
}

/// Extend the existing error envelope without replacing its primary error.
/// `partial` describes incomplete coverage, not a claim that any row committed.
/// Returns true when the caller should attach its original indexing statistics.
pub(crate) fn annotate_partial_scan(
    payload: &mut Value,
    scan_had_errors: bool,
    connectors: &[ConnectorStats],
) -> bool {
    if !incomplete(scan_had_errors, connectors) {
        return false;
    }
    let Some(fields) = payload.as_object_mut() else {
        return false;
    };
    let failed: BTreeSet<_> = connectors
        .iter()
        .filter(|connector| connector.error.is_some())
        .map(|connector| connector.name.as_str())
        .collect();
    fields.insert("success".into(), json!(false));
    fields.insert("partial".into(), json!(true));
    fields.insert("coverage_status".into(), json!("incomplete"));
    fields.insert("scan_had_errors".into(), json!(true));
    fields.insert("failed_connectors".into(), json!(failed));
    true
}

/// Detect false-success receipts produced before partial coverage reached the
/// command boundary. Legacy successful object shapes remain compatible.
pub(crate) fn cached_scan_is_incomplete(payload: &serde_json::Map<String, Value>) -> bool {
    if payload.get("success") == Some(&Value::Bool(false))
        || payload.get("partial") == Some(&Value::Bool(true))
        || payload.get("scan_had_errors") == Some(&Value::Bool(true))
        || payload.get("coverage_status").and_then(Value::as_str) == Some("incomplete")
    {
        return true;
    }
    let Some(stats) = payload.get("indexing_stats") else {
        return false;
    };
    stats.get("scan_had_errors") == Some(&Value::Bool(true))
        || stats
            .get("connectors")
            .and_then(Value::as_array)
            .is_some_and(|connectors| {
                connectors
                    .iter()
                    .any(|connector| connector.get("error").is_some_and(|error| !error.is_null()))
            })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connector(name: &str, error: Option<&str>) -> ConnectorStats {
        ConnectorStats {
            name: name.into(),
            conversations: 2,
            messages: 7,
            error: error.map(str::to_owned),
            ..Default::default()
        }
    }

    #[test]
    fn successful_and_discovered_only_scans_remain_successful() {
        assert!(require_complete_scan(false, &[]).is_ok());
        assert!(require_complete_scan(false, &[connector("codex", None)]).is_ok());
        let mut payload = json!({"success": false, "code": 8});
        let original = payload.clone();
        assert!(!annotate_partial_scan(&mut payload, false, &[]));
        assert_eq!(payload, original);
    }

    #[test]
    fn aggregate_failure_is_not_hidden_by_empty_connector_diagnostics() {
        let error = require_complete_scan(true, &[]).unwrap_err();
        assert_eq!(error.code, 9);
        assert_eq!(error.kind, "index");
        assert!(error.retryable);
        let mut payload = crate::cli_error_json_payload(&error, 17);
        assert!(annotate_partial_scan(&mut payload, true, &[]));
        assert_eq!(payload["success"], false);
        assert_eq!(payload["partial"], true);
        assert_eq!(payload["coverage_status"], "incomplete");
        assert_eq!(payload["failed_connectors"], json!([]));
        assert_eq!(payload["elapsed_ms"], 17);
    }

    #[test]
    fn connector_error_is_authoritative_without_the_new_aggregate_flag() {
        let connectors = [connector("codex", Some("source rejected"))];
        assert!(require_complete_scan(false, &connectors).is_err());
        let mut payload = json!({"success": true});
        assert!(annotate_partial_scan(&mut payload, false, &connectors));
        assert_eq!(payload["success"], false);
        assert_eq!(payload["failed_connectors"], json!(["codex"]));
    }

    #[test]
    fn partial_context_preserves_the_primary_failure_and_escapes_names() {
        let connectors = [connector(
            "codex\nnot-a-log-line",
            Some("private error detail"),
        )];
        let mut payload =
            json!({"success":false,"code":130,"error":"interrupted","retryable":true});
        assert!(annotate_partial_scan(&mut payload, true, &connectors));
        assert_eq!(payload["code"], 130);
        assert_eq!(payload["error"], "interrupted");
        assert_eq!(payload["retryable"], true);
        let wire = serde_json::to_string(&payload).unwrap();
        assert!(!wire.contains('\n'));
        assert!(!wire.contains("private error detail"));
        assert_eq!(serde_json::from_str::<Value>(&wire).unwrap(), payload);
    }

    #[test]
    fn failed_connectors_are_sorted_deduplicated_and_not_inferred_from_counts() {
        let connectors = [
            connector("codex", Some("rejected b")),
            connector("claude", Some("read failure")),
            connector("codex", Some("rejected d")),
            connector("amp", None),
        ];
        let mut payload = json!({});
        assert!(annotate_partial_scan(&mut payload, true, &connectors));
        assert_eq!(payload["failed_connectors"], json!(["claude", "codex"]));
        assert!(payload.get("messages").is_none());
        assert!(payload.get("conversations").is_none());
    }

    #[test]
    fn retry_after_a_clean_scan_is_not_poisoned_by_previous_failure() {
        assert!(require_complete_scan(true, &[connector("codex", Some("large"))]).is_err());
        let clean = [connector("codex", None)];
        assert!(require_complete_scan(false, &clean).is_ok());
        let mut payload = json!({"success":true,"cached":false});
        assert!(!annotate_partial_scan(&mut payload, false, &clean));
        assert_eq!(payload["success"], true);
        assert!(payload.get("partial").is_none());
    }

    #[test]
    fn malformed_internal_envelope_cannot_panic() {
        let mut payload = Value::Null;
        assert!(!annotate_partial_scan(&mut payload, true, &[]));
        assert!(payload.is_null());
    }
    #[test]
    fn old_false_success_cache_receipts_are_misses() {
        for payload in [
            json!({"success":true,"indexing_stats":{"connectors":[{"name":"codex","error":"over budget"}]}}),
            json!({"success":true,"indexing_stats":{"scan_had_errors":true}}),
            json!({"success":true,"partial":true}),
            json!({"success":true,"scan_had_errors":true}),
            json!({"success":true,"coverage_status":"incomplete"}),
            json!({"success":false}),
        ] {
            assert!(crate::cached_index_payload(&payload.to_string(), "same-key").is_none());
        }
    }

    #[test]
    fn cache_keeps_clean_current_and_legacy_success_shapes() {
        for payload in [
            json!({"success":true,"indexing_stats":{"scan_had_errors":false,"connectors":[{"name":"codex","error":null}]}}),
            json!({"success":true,"messages":2}),
            json!({"ok":true,"messages":2}),
        ] {
            let cached = crate::cached_index_payload(&payload.to_string(), "same-key").unwrap();
            assert_eq!(cached["cached"], true);
            assert_eq!(cached["idempotency_key"], "same-key");
            assert_eq!(cached["messages"], payload["messages"]);
        }
    }
}
