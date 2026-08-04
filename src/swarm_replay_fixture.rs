//! Redaction-first replay-fixture generator for swarm coordination tests.
//!
//! Converts a raw swarm timeline (Beads, Agent Mail, git, rch, proof-artifact
//! events) into a *scrubbed* replay fixture that captures event shape, timing,
//! ownership, dependency, and proof relationships — without any raw private
//! session text. The generated fixture can drive work-packet, lint, proof-debt,
//! context-pack, and outcome-analytics tests together.
//!
//! Guarantees:
//! * Read-only and deterministic: the fixture hash is a blake3 over a *canonical*
//!   (recursively key-sorted) JSON encoding of the scrubbed events, so replay is
//!   stable across machines. No system clock or randomness is used.
//! * Privacy-first: every event payload is passed through the strict
//!   swarm-evidence redactor, and any free-text field that could carry prompt or
//!   session content is dropped to an omission marker before hashing. No raw
//!   prompt/session payload can leak.

use chrono::Utc;
use serde_json::{Map, Value, json};

/// Schema identifier for the replay fixture payload.
pub const SCHEMA_VERSION: &str = "cass.swarm.replay_fixture.v1";

/// Marker substituted for any dropped free-text field.
const OMITTED: &str = "[OMITTED_SESSION_TEXT]";

/// Recognized event kinds, in stable reporting order. Unknown kinds are kept
/// verbatim but prefixed so histograms stay explicit.
const KNOWN_KINDS: &[&str] = &[
    "bead_close",
    "bead_open",
    "mail_send",
    "git_commit",
    "rch_build",
    "proof_artifact",
    "reservation",
    "lint_finding",
];

/// Field names that may carry raw prompt/session/body text and must be dropped
/// (not merely redacted) before the payload leaves this module.
const DROP_TEXT_FIELDS: &[&str] = &[
    "text",
    "body",
    "prompt",
    "session_text",
    "raw",
    "raw_text",
    "content",
    "message_body",
    "transcript",
];

#[derive(Debug, Clone)]
struct Event {
    seq: u64,
    ts_ms: Option<i64>,
    kind: String,
    actor: Option<String>,
    bead: Option<String>,
    payload: Value,
}

#[derive(Debug, Clone, Default)]
struct RedactionTally {
    fields_scrubbed: usize,
    text_fields_dropped: usize,
    string_values_seen: usize,
}

#[derive(Debug, Clone)]
struct ReplayFacts {
    fixture_problem: Option<String>,
    replay_id: String,
    events: Vec<Event>,
}

/// Render the live payload. With no caller-supplied timeline it reports an empty,
/// fully-documented envelope; live capture is the caller's responsibility.
#[must_use]
pub fn render_replay_fixture_live() -> Value {
    render_payload("live", "live", live_facts())
}

/// Render the scrubbed replay fixture from a raw-timeline swarm fixture source.
#[must_use]
pub fn render_replay_fixture_fixture(fixture_id: &str, source: Option<&Value>) -> Value {
    render_payload(fixture_id, "fixture", fixture_facts(source))
}

fn render_payload(fixture_id: &str, source_kind: &str, facts: ReplayFacts) -> Value {
    let mut tally = RedactionTally::default();
    let mut scrubbed_events: Vec<Value> = facts
        .events
        .iter()
        .map(|event| scrub_event(event, &mut tally))
        .collect();
    // Deterministic order: by seq, then by the canonical encoding as a tiebreak.
    scrubbed_events.sort_by(|a, b| {
        let sa = a.get("seq").and_then(Value::as_u64).unwrap_or(0);
        let sb = b.get("seq").and_then(Value::as_u64).unwrap_or(0);
        sa.cmp(&sb)
            .then_with(|| canonical_string(a).cmp(&canonical_string(b)))
    });

    let kind_histogram = histogram(&facts.events, |event| event.kind.clone());
    let actor_histogram = histogram(&facts.events, |event| {
        event
            .actor
            .clone()
            .unwrap_or_else(|| "unspecified".to_string())
    });
    let (first_ts, last_ts) = time_span(&facts.events);
    let events_value = Value::Array(scrubbed_events.clone());
    let deterministic_hash = canonical_hash(&events_value);
    let assertions = replay_assertions(&facts.events, &scrubbed_events, &tally);
    let summary = summarize(&facts, &tally, &assertions);
    let status = summary
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("partial")
        .to_string();
    let next_step = summary
        .get("recommended_action")
        .cloned()
        .unwrap_or_else(|| json!("review-scrubbed-fixture"));

    json!({
        "schema_version": SCHEMA_VERSION,
        "status": status,
        "_meta": {
            "generated_at": Utc::now().to_rfc3339(),
            "source": source_kind,
            "fixture_id": fixture_id,
            "contract": "deterministic redaction-first swarm replay fixture"
        },
        "summary": summary,
        "manifest": {
            "replay_id": facts.replay_id,
            "schema_version": SCHEMA_VERSION,
            "event_count": facts.events.len(),
            "deterministic_hash": deterministic_hash,
            "kind_histogram": kind_histogram,
            "actor_histogram": actor_histogram,
            "timeline": {
                "first_ts_ms": first_ts,
                "last_ts_ms": last_ts,
                "span_ms": match (first_ts, last_ts) {
                    (Some(first), Some(last)) => json!(last.saturating_sub(first)),
                    _ => Value::Null
                }
            }
        },
        "redaction_report": {
            "policy": "strict-swarm-evidence + drop-free-text",
            "events": facts.events.len(),
            "string_values_seen": tally.string_values_seen,
            "fields_scrubbed": tally.fields_scrubbed,
            "text_fields_dropped": tally.text_fields_dropped,
            "raw_payload_text_retained": false
        },
        "before_after": {
            "before": {
                "raw_payload_present": facts.events.iter().any(|event| has_text_field(&event.payload)),
                "event_count": facts.events.len()
            },
            "after": {
                "raw_payload_present": false,
                "event_count": scrubbed_events.len()
            }
        },
        "events": events_value,
        "replay_assertions": assertions,
        "mutation_contract": {
            "read_only": true,
            "schedules_work": false,
            "mutates_files": false,
            "mutates_db": false,
            "touches_network": false
        },
        "privacy": {
            "contains_session_text": false,
            "contains_raw_secrets": false,
            "redaction_applied": true
        },
        "guided_workflow": {
            "surface": "cass swarm replay-fixture --json",
            "bead_id": "coding_agent_session_search-swarm-coordination-intelligence-gnrxb.9",
            "apply_mode_available": false,
            "next_step": next_step
        }
    })
}

fn scrub_event(event: &Event, tally: &mut RedactionTally) -> Value {
    let scrubbed_payload = scrub_payload(&event.payload, tally);
    json!({
        "seq": event.seq,
        "ts_ms": event.ts_ms,
        "kind": normalize_kind(&event.kind),
        "actor": event.actor.as_ref().map(|actor| redact_text(actor)),
        "bead": event.bead,
        "payload": scrubbed_payload
    })
}

/// Recursively scrub a payload: drop free-text fields entirely, redact every
/// remaining string value.
fn scrub_payload(value: &Value, tally: &mut RedactionTally) -> Value {
    match value {
        Value::String(text) => {
            tally.string_values_seen += 1;
            let redacted = redact_text(text);
            if redacted != *text {
                tally.fields_scrubbed += 1;
            }
            Value::String(redacted)
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| scrub_payload(item, tally))
                .collect(),
        ),
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, val) in map {
                if DROP_TEXT_FIELDS.contains(&key.as_str()) {
                    tally.text_fields_dropped += 1;
                    out.insert(key.clone(), json!(OMITTED));
                } else {
                    out.insert(key.clone(), scrub_payload(val, tally));
                }
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

fn has_text_field(value: &Value) -> bool {
    match value {
        Value::Object(map) => map
            .iter()
            .any(|(key, val)| DROP_TEXT_FIELDS.contains(&key.as_str()) || has_text_field(val)),
        Value::Array(items) => items.iter().any(has_text_field),
        _ => false,
    }
}

fn redact_text(text: &str) -> String {
    crate::pages::redact::redact_swarm_text(text)
}

fn normalize_kind(kind: &str) -> String {
    if KNOWN_KINDS.contains(&kind) {
        kind.to_string()
    } else {
        format!("other:{kind}")
    }
}

fn histogram<F>(events: &[Event], key_fn: F) -> Value
where
    F: Fn(&Event) -> String,
{
    let mut keys: Vec<String> = Vec::new();
    let mut counts: Vec<usize> = Vec::new();
    for event in events {
        let key = key_fn(event);
        match keys.iter().position(|existing| existing == &key) {
            Some(idx) => counts[idx] += 1,
            None => {
                keys.push(key);
                counts.push(1);
            }
        }
    }
    // Deterministic order: count desc, then key asc.
    let mut pairs: Vec<(String, usize)> = keys.into_iter().zip(counts).collect();
    pairs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let mut map = Map::new();
    for (key, count) in pairs {
        map.insert(normalize_kind_for_key(&key), json!(count));
    }
    Value::Object(map)
}

/// Histogram keys are not secret, but redact defensively (an actor label could
/// be a mislabelled path).
fn normalize_kind_for_key(key: &str) -> String {
    redact_text(key)
}

fn time_span(events: &[Event]) -> (Option<i64>, Option<i64>) {
    let mut first: Option<i64> = None;
    let mut last: Option<i64> = None;
    for event in events {
        if let Some(ts) = event.ts_ms {
            first = Some(first.map_or(ts, |cur| cur.min(ts)));
            last = Some(last.map_or(ts, |cur| cur.max(ts)));
        }
    }
    (first, last)
}

fn replay_assertions(raw: &[Event], scrubbed: &[Value], tally: &RedactionTally) -> Value {
    let ts: Vec<i64> = scrubbed
        .iter()
        .filter_map(|event| event.get("ts_ms").and_then(Value::as_i64))
        .collect();
    let ts_monotonic = ts.windows(2).all(|pair| pair[0] <= pair[1]);
    let no_raw_payload = !scrubbed
        .iter()
        .any(|event| event.get("payload").is_some_and(retains_droppable_text));
    json!([
        {"assertion": "event_count_preserved", "ok": raw.len() == scrubbed.len()},
        {"assertion": "events_sorted_by_seq", "ok": seqs_sorted(scrubbed)},
        {"assertion": "timestamps_monotonic", "ok": ts_monotonic},
        {"assertion": "session_payload_omitted", "ok": no_raw_payload && tally.string_values_seen >= tally.fields_scrubbed},
        {"assertion": "ownership_preserved", "ok": ownership_preserved(raw, scrubbed)}
    ])
}

fn retains_droppable_text(payload: &Value) -> bool {
    match payload {
        Value::Object(map) => map.iter().any(|(key, val)| {
            // ubs:ignore — marker-string comparison against the omission sentinel, not a secret/token check.
            (DROP_TEXT_FIELDS.contains(&key.as_str()) && val.as_str() != Some(OMITTED))
                || retains_droppable_text(val)
        }),
        Value::Array(items) => items.iter().any(retains_droppable_text),
        _ => false,
    }
}

fn seqs_sorted(scrubbed: &[Value]) -> bool {
    let seqs: Vec<u64> = scrubbed
        .iter()
        .filter_map(|event| event.get("seq").and_then(Value::as_u64))
        .collect();
    seqs.windows(2).all(|pair| pair[0] <= pair[1])
}

fn ownership_preserved(raw: &[Event], scrubbed: &[Value]) -> bool {
    let raw_beads: Vec<&str> = raw
        .iter()
        .filter_map(|event| event.bead.as_deref())
        .collect();
    let scrubbed_beads: Vec<&str> = scrubbed
        .iter()
        .filter_map(|event| event.get("bead").and_then(Value::as_str))
        .collect();
    let mut a = raw_beads;
    let mut b = scrubbed_beads;
    a.sort_unstable();
    b.sort_unstable();
    a == b
}

fn summarize(facts: &ReplayFacts, tally: &RedactionTally, assertions: &Value) -> Value {
    let all_ok = assertions
        .as_array()
        .map(|items| {
            items
                .iter()
                .all(|item| item.get("ok").and_then(Value::as_bool).unwrap_or(false))
        })
        .unwrap_or(false);
    let status = if facts.fixture_problem.is_some() {
        "partial"
    } else if facts.events.is_empty() || !all_ok {
        "warning"
    } else {
        "ok"
    };
    let recommended_action = if facts.fixture_problem.is_some() {
        "supply-replay-timeline-fixture"
    } else if facts.events.is_empty() {
        "no-events-to-replay"
    } else if !all_ok {
        "investigate-failed-replay-assertions"
    } else {
        "review-scrubbed-fixture"
    };
    json!({
        "status": status,
        "event_count": facts.events.len(),
        "fields_scrubbed": tally.fields_scrubbed,
        "text_fields_dropped": tally.text_fields_dropped,
        "all_assertions_pass": all_ok,
        "recommended_action": recommended_action
    })
}

// ---- canonical JSON + hashing (cross-machine stable) ----

fn canonical_hash(value: &Value) -> String {
    let canonical = canonical_string(value);
    format!("blake3:{}", blake3::hash(canonical.as_bytes()).to_hex())
}

/// Serialize `value` with object keys recursively sorted, so the encoding (and
/// thus its hash) is identical regardless of map iteration order or the
/// `preserve_order` serde feature.
fn canonical_string(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable();
            out.push('{');
            for (idx, key) in keys.iter().enumerate() {
                if idx > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String((*key).clone()).to_string());
                out.push(':');
                write_canonical(&map[*key], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (idx, item) in items.iter().enumerate() {
                if idx > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

fn live_facts() -> ReplayFacts {
    ReplayFacts {
        fixture_problem: None,
        replay_id: "live".to_string(),
        events: Vec::new(),
    }
}

fn fixture_facts(source: Option<&Value>) -> ReplayFacts {
    let Some(source) = source else {
        return ReplayFacts {
            fixture_problem: Some("replay_fixture source is missing".to_string()),
            replay_id: "unknown".to_string(),
            events: Vec::new(),
        };
    };

    let events = source
        .get("events")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .enumerate()
                .map(|(idx, item)| parse_event(item, idx))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    ReplayFacts {
        fixture_problem: None,
        replay_id: source
            .get("replay_id")
            .and_then(Value::as_str)
            .unwrap_or("unspecified")
            .to_string(),
        events,
    }
}

fn parse_event(value: &Value, idx: usize) -> Event {
    Event {
        seq: value
            .get("seq")
            .and_then(Value::as_u64)
            .unwrap_or(idx as u64 + 1),
        ts_ms: value.get("ts_ms").and_then(Value::as_i64),
        kind: value
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("unspecified")
            .to_string(),
        actor: value
            .get("actor")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_string),
        bead: value
            .get("bead")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_string),
        payload: value.get("payload").cloned().unwrap_or(Value::Null),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> Value {
        json!({
            "replay_id": "swarm-2026-06-08",
            "events": [
                {
                    "seq": 2,
                    "ts_ms": 1_749_456_000_000i64,
                    "kind": "mail_send",
                    "actor": "cc",
                    "bead": "demo-bead",
                    "payload": {
                        "to": "cod",
                        "body": "private discussion: my key is sk-ant-supersecret1234567890",
                        "path": "/home/alice/.claude/projects/x"
                    }
                },
                {
                    "seq": 1,
                    "ts_ms": 1_749_455_000_000i64,
                    "kind": "git_commit",
                    "actor": "cod",
                    "bead": "demo-bead",
                    "payload": {
                        "sha": "abc123",
                        "message": "fix things; contact alice@example.com"
                    }
                }
            ]
        })
    }

    fn assert_no_leak(value: &Value) {
        let text = serde_json::to_string(value).expect("serialize");
        for needle in [
            "/home/",
            "sk-ant-",
            "supersecret",
            "alice@example.com",
            "private discussion",
        ] {
            assert!(!text.contains(needle), "replay fixture leaked: {needle}");
        }
    }

    #[test]
    fn scrubs_all_secrets_paths_and_drops_body() {
        let out = render_replay_fixture_fixture("replay", Some(&source()));
        assert_no_leak(&out);
        // The free-text body must be dropped to the omission marker.
        let events = out["events"].as_array().unwrap();
        let mail = events
            .iter()
            .find(|e| e["kind"] == json!("mail_send"))
            .unwrap();
        assert_eq!(mail["payload"]["body"], json!(OMITTED));
        assert_eq!(
            out["redaction_report"]["raw_payload_text_retained"],
            json!(false)
        );
    }

    #[test]
    fn events_sorted_by_seq_and_assertions_pass() {
        let out = render_replay_fixture_fixture("replay", Some(&source()));
        let events = out["events"].as_array().unwrap();
        assert_eq!(events[0]["seq"], json!(1));
        assert_eq!(events[1]["seq"], json!(2));
        assert_eq!(out["summary"]["all_assertions_pass"], json!(true));
        assert_eq!(out["status"], json!("ok"));
    }

    #[test]
    fn deterministic_hash_is_stable_across_runs() {
        let a = render_replay_fixture_fixture("replay", Some(&source()));
        let b = render_replay_fixture_fixture("replay", Some(&source()));
        assert_eq!(
            a["manifest"]["deterministic_hash"],
            b["manifest"]["deterministic_hash"]
        );
        // Hash is independent of input event order (sorted by seq before hashing).
        let mut reordered = source();
        let ev = reordered["events"].as_array().unwrap().clone();
        reordered["events"] = json!([ev[1], ev[0]]);
        let c = render_replay_fixture_fixture("replay", Some(&reordered));
        assert_eq!(
            a["manifest"]["deterministic_hash"],
            c["manifest"]["deterministic_hash"]
        );
    }

    #[test]
    fn manifest_has_counts_and_timeline() {
        let out = render_replay_fixture_fixture("replay", Some(&source()));
        assert_eq!(out["manifest"]["event_count"], json!(2));
        assert_eq!(
            out["manifest"]["timeline"]["first_ts_ms"],
            json!(1_749_455_000_000i64)
        );
        assert_eq!(
            out["manifest"]["timeline"]["last_ts_ms"],
            json!(1_749_456_000_000i64)
        );
        assert!(
            out["redaction_report"]["text_fields_dropped"]
                .as_u64()
                .unwrap()
                >= 1
        );
    }

    #[test]
    fn missing_source_is_partial_not_panic() {
        let out = render_replay_fixture_fixture("empty", None);
        assert_eq!(out["status"], json!("partial"));
        assert_eq!(out["mutation_contract"]["read_only"], json!(true));
        assert_eq!(out["privacy"]["redaction_applied"], json!(true));
    }

    #[test]
    fn live_is_empty_and_read_only() {
        let out = render_replay_fixture_live();
        assert_eq!(out["manifest"]["event_count"], json!(0));
        assert_eq!(
            out["summary"]["recommended_action"],
            json!("no-events-to-replay")
        );
        assert_eq!(out["mutation_contract"]["touches_network"], json!(false));
    }
}
