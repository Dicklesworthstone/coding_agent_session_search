//! Redacted repro-capsule generator for failures and search hits.
//!
//! Turns a failing command, doctor incident, search miss, session view, or CI
//! failure into a minimal, scrubbed reproduction artifact: a manifest, the
//! (redacted) command transcript, an environment summary, cass version, relevant
//! health/diag excerpts, evidence refs, a redaction report, expected/actual
//! behavior, and a one-command rerun template that accepts a recipient-local
//! shared fixture path — never a live data dir.
//!
//! Privacy is the default: raw private prompt/session text is dropped to an
//! omission marker unless the operator explicitly opts into the `full` privacy
//! tier, and every emitted string passes the strict swarm-evidence redactor.
//! Deterministic: the capsule id is a blake3 over a canonical key-sorted
//! encoding of the scrubbed capsule body (stable across machines).

use chrono::Utc;
use serde_json::{Map, Value, json};

/// Schema identifier for the repro capsule payload.
pub const SCHEMA_VERSION: &str = "cass.swarm.repro_capsule.v2";

/// Marker substituted for dropped private session/prompt text.
const OMITTED: &str = "[OMITTED_PRIVATE_SESSION_TEXT]";

/// Recognized incident kinds, in stable order.
const INCIDENT_KINDS: &[&str] = &[
    "search-miss",
    "panic",
    "doctor-incident",
    "model-install-failure",
    "stale-index",
    "ci-failure",
];

#[derive(Debug, Clone)]
struct CapsuleFacts {
    fixture_problem: Option<String>,
    incident_kind: String,
    cass_version: String,
    command: String,
    transcript: String,
    env: Value,
    health_excerpt: Value,
    evidence_refs: Vec<String>,
    expected: String,
    actual: String,
    private_session_text: Option<String>,
    privacy_tier: String,
}

#[derive(Debug, Clone, Default)]
struct RedactionTally {
    fields_scrubbed: usize,
    strings_seen: usize,
    private_text_dropped: bool,
}

/// Render the live capsule. Conservative: with no incident input it documents
/// the contract and a partial status; live capture is the caller's job.
#[must_use]
pub fn render_repro_capsule_live() -> Value {
    render_payload("live", "live", live_facts())
}

/// Render a scrubbed repro capsule from a swarm fixture source value.
#[must_use]
pub fn render_repro_capsule_fixture(fixture_id: &str, source: Option<&Value>) -> Value {
    render_payload(fixture_id, "fixture", fixture_facts(source))
}

fn redact(text: &str, tally: &mut RedactionTally) -> String {
    tally.strings_seen += 1;
    let out = crate::pages::redact::redact_swarm_text(text);
    if out != text {
        tally.fields_scrubbed += 1;
    }
    out
}

/// Recursively redact every string value in an arbitrary JSON value.
fn redact_value(value: &Value, tally: &mut RedactionTally) -> Value {
    match value {
        Value::String(text) => Value::String(redact(text, tally)),
        Value::Array(items) => {
            Value::Array(items.iter().map(|item| redact_value(item, tally)).collect())
        }
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, val)| (key.clone(), redact_value(val, tally)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn render_payload(fixture_id: &str, source_kind: &str, facts: CapsuleFacts) -> Value {
    let mut tally = RedactionTally::default();
    let share_safe_fixture_id = redact(fixture_id, &mut RedactionTally::default());

    let opted_into_full = facts.privacy_tier.eq_ignore_ascii_case("full");
    // Private session text is dropped to a marker unless the operator opts into
    // the full tier; even then it is redacted, never emitted raw.
    let session_text = match (&facts.private_session_text, opted_into_full) {
        (Some(text), true) => redact(text, &mut tally),
        (Some(_), false) => {
            tally.private_text_dropped = true;
            OMITTED.to_string()
        }
        (None, _) => OMITTED.to_string(),
    };

    let command = redact(&facts.command, &mut tally);
    let transcript = redact(&facts.transcript, &mut tally);
    let env = redact_value(&facts.env, &mut tally);
    let health_excerpt = redact_value(&facts.health_excerpt, &mut tally);
    let evidence_refs: Vec<String> = facts
        .evidence_refs
        .iter()
        .map(|reference| redact(reference, &mut tally))
        .collect();
    let expected = redact(&facts.expected, &mut tally);
    let actual = redact(&facts.actual, &mut tally);

    let body = json!({
        "incident_kind": normalize_kind(&facts.incident_kind),
        "command": command,
        "transcript": transcript,
        "env_summary": env,
        "health_excerpt": health_excerpt,
        "evidence_refs": evidence_refs,
        "expected": expected,
        "actual": actual,
        "session_text": session_text,
    });
    let capsule_id = canonical_hash(&body);
    let rerun = rerun_script(&capsule_id);
    let replay_value = |key: &str| body.get(key).cloned().unwrap_or(Value::Null);
    let replay_source = json!({
        "incident_kind": replay_value("incident_kind"),
        "cass_version": redact(&facts.cass_version, &mut RedactionTally::default()),
        "command": replay_value("command"),
        "transcript": replay_value("transcript"),
        "env": replay_value("env_summary"),
        "health_excerpt": replay_value("health_excerpt"),
        "evidence_refs": replay_value("evidence_refs"),
        "expected": replay_value("expected"),
        "actual": replay_value("actual"),
        "private_session_text": replay_value("session_text"),
        "privacy_tier": if opted_into_full { "full" } else { "redacted" }
    });

    let summary = summarize(&facts, &tally);
    let status = summary
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("partial");

    json!({
        "schema_version": SCHEMA_VERSION,
        "status": status,
        "_meta": {
            "generated_at": Utc::now().to_rfc3339(),
            "source": source_kind,
            "fixture_id": share_safe_fixture_id,
            "contract": "redacted reproduction capsule"
        },
        "fixture_id": share_safe_fixture_id,
        "description": "Self-contained redacted input for cass swarm repro-capsule",
        "sources": {
            "repro_capsule": replay_source
        },
        "manifest": {
            "capsule_id": capsule_id,
            "schema_version": SCHEMA_VERSION,
            "incident_kind": normalize_kind(&facts.incident_kind),
            "cass_version": redact(&facts.cass_version, &mut RedactionTally::default()),
            "privacy_tier": facts.privacy_tier,
            "evidence_ref_count": facts.evidence_refs.len()
        },
        "summary": summary,
        "capsule": body,
        "redaction_report": {
            "policy": "strict-swarm-evidence + drop-private-session-text",
            "strings_seen": tally.strings_seen,
            "fields_scrubbed": tally.fields_scrubbed,
            "private_session_text_dropped": tally.private_text_dropped,
            "raw_session_content_included": opted_into_full && facts.private_session_text.is_some()
        },
        "rerun": rerun,
        "mutation_contract": {
            "read_only": true,
            "schedules_work": false,
            "mutates_files": false,
            "mutates_db": false,
            "touches_network": false
        },
        "privacy": {
            "contains_raw_session_text": false,
            "contains_raw_secrets": false,
            "redaction_applied": true,
            "session_text_opt_in": opted_into_full
        },
        "guided_workflow": {
            "surface": "cass swarm repro-capsule --json",
            "bead_id": "coding_agent_session_search-guided-ops-repro-trust-5u82n.2",
            "apply_mode_available": false,
            "next_step": summary.get("recommended_action").cloned().unwrap_or_else(|| json!("review-capsule"))
        }
    })
}

/// The one-command rerun template uses the real read-only CLI surface while
/// deliberately omitting the operator's local fixture path. The emitted payload
/// is itself a valid fixture; save it under the fixed safe filename and run the
/// command verbatim from that directory.
fn rerun_script(capsule_id: &str) -> Value {
    json!({
        "targets_live_data": false,
        "no_live_data_guard": true,
        "capsule_id": capsule_id,
        "command_template": "cass swarm repro-capsule --json --fixture repro-capsule.fixture.json",
        "note": "Save this capsule as repro-capsule.fixture.json, then run the command from that directory; the operator's original path is intentionally omitted."
    })
}

fn summarize(facts: &CapsuleFacts, tally: &RedactionTally) -> Value {
    let known_kind = INCIDENT_KINDS.contains(&facts.incident_kind.as_str());
    let status = if facts.fixture_problem.is_some() {
        "partial"
    } else if !known_kind {
        "warning"
    } else {
        "ok"
    };
    let recommended_action = if facts.fixture_problem.is_some() {
        "supply-incident-fixture"
    } else if !known_kind {
        "classify-incident-kind"
    } else {
        "review-capsule"
    };
    json!({
        "status": status,
        "incident_kind_known": known_kind,
        "fields_scrubbed": tally.fields_scrubbed,
        "private_session_text_dropped": tally.private_text_dropped,
        "evidence_ref_count": facts.evidence_refs.len(),
        "recommended_action": recommended_action
    })
}

fn normalize_kind(kind: &str) -> String {
    if INCIDENT_KINDS.contains(&kind) || kind.starts_with("other:") {
        kind.to_string()
    } else {
        format!("other:{kind}")
    }
}

// ---- canonical JSON + hashing (cross-machine stable) ----

fn canonical_hash(value: &Value) -> String {
    let mut canonical = String::new();
    write_canonical(value, &mut canonical);
    canonical.push_str("::");
    canonical.push_str(SCHEMA_VERSION);
    format!(
        "capsule-blake3:{}",
        blake3::hash(canonical.as_bytes()).to_hex()
    )
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

fn live_facts() -> CapsuleFacts {
    CapsuleFacts {
        fixture_problem: None,
        incident_kind: "unspecified".to_string(),
        cass_version: env!("CARGO_PKG_VERSION").to_string(),
        command: String::new(),
        transcript: String::new(),
        env: Value::Object(Map::new()),
        health_excerpt: Value::Object(Map::new()),
        evidence_refs: Vec::new(),
        expected: String::new(),
        actual: String::new(),
        private_session_text: None,
        privacy_tier: "redacted".to_string(),
    }
}

fn fixture_facts(source: Option<&Value>) -> CapsuleFacts {
    let Some(source) = source else {
        return CapsuleFacts {
            fixture_problem: Some("repro_capsule fixture source is missing".to_string()),
            ..live_facts()
        };
    };

    let string_field = |field: &str, default: &str| {
        source
            .get(field)
            .and_then(Value::as_str)
            .unwrap_or(default)
            .to_string()
    };

    CapsuleFacts {
        fixture_problem: None,
        incident_kind: string_field("incident_kind", "unspecified"),
        cass_version: string_field("cass_version", env!("CARGO_PKG_VERSION")),
        command: string_field("command", ""),
        transcript: string_field("transcript", ""),
        env: source
            .get("env")
            .cloned()
            .unwrap_or(Value::Object(Map::new())),
        health_excerpt: source
            .get("health_excerpt")
            .cloned()
            .unwrap_or(Value::Object(Map::new())),
        evidence_refs: source
            .get("evidence_refs")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        expected: string_field("expected", ""),
        actual: string_field("actual", ""),
        private_session_text: source
            .get("private_session_text")
            .and_then(Value::as_str)
            .map(str::to_string),
        privacy_tier: string_field("privacy_tier", "redacted"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn risky_source(privacy_tier: &str) -> Value {
        let synthetic_anthropic_key = ["sk-ant-", "api03-", "AAAABBBBCCCCDDDDEEEE"].concat();
        json!({
            "incident_kind": "search-miss",
            "cass_version": "0.6.13",
            "command": "search for sk-ant-supersecret1234567890 under /home/alice/.claude",
            "transcript": "no hits; tried /home/alice/.claude/projects and TOKEN=supersecretvalue123456",
            "env": {"os": "linux", "home": "/home/alice", "email": "alice@example.com"},
            "health_excerpt": {"index_present": true, "path": "/home/alice/.cass/index"},
            "evidence_refs": ["/home/alice/.claude/s.jsonl:42"],
            "expected": "at least one hit",
            "actual": "zero hits",
            "private_session_text": format!(
                "see /home/alice/notes, contact alice@example.com, key {synthetic_anthropic_key}"
            ),
            "privacy_tier": privacy_tier
        })
    }

    fn assert_no_leak(value: &Value) {
        let text = serde_json::to_string(value).expect("serialize");
        for needle in [
            "/home/",
            "sk-ant-",
            "supersecret",
            "alice@example.com",
            "hunter2",
            "TOKEN=supersecret",
        ] {
            assert!(!text.contains(needle), "repro capsule leaked: {needle}");
        }
    }

    #[test]
    fn redacted_tier_drops_private_text_and_scrubs_everything() {
        let out = render_repro_capsule_fixture("capsule", Some(&risky_source("redacted")));
        assert_no_leak(&out);
        assert_eq!(out["capsule"]["session_text"], json!(OMITTED));
        assert_eq!(
            out["redaction_report"]["private_session_text_dropped"],
            json!(true)
        );
        assert_eq!(
            out["redaction_report"]["raw_session_content_included"],
            json!(false)
        );
        assert_eq!(out["privacy"]["session_text_opt_in"], json!(false));
    }

    #[test]
    fn full_tier_keeps_but_still_redacts_session_text() {
        let out = render_repro_capsule_fixture("capsule", Some(&risky_source("full")));
        // Even opted-in, secrets/paths in the session text are scrubbed.
        assert_no_leak(&out);
        assert_ne!(out["capsule"]["session_text"], json!(OMITTED));
        assert_eq!(out["privacy"]["session_text_opt_in"], json!(true));
    }

    #[test]
    fn rerun_uses_real_share_safe_fixture_surface() {
        let out = render_repro_capsule_fixture("capsule", Some(&risky_source("redacted")));
        assert_eq!(out["rerun"]["targets_live_data"], json!(false));
        assert_eq!(out["rerun"]["no_live_data_guard"], json!(true));
        assert!(out["rerun"].get("data_dir").is_none());
        let cmd = out["rerun"]["command_template"].as_str().unwrap();
        assert_eq!(
            cmd,
            "cass swarm repro-capsule --json --fixture repro-capsule.fixture.json"
        );
        let fictional = ["cass-repro", "--incident", "--capsule-id", "--data-dir"]
            .into_iter()
            .find(|fragment| cmd.contains(fragment));
        assert!(
            fictional.is_none(),
            "fictional rerun fragment: {fictional:?}"
        );
    }

    #[test]
    fn capsule_id_is_deterministic_across_runs() {
        let a = render_repro_capsule_fixture("capsule", Some(&risky_source("redacted")));
        let b = render_repro_capsule_fixture("capsule", Some(&risky_source("redacted")));
        assert_eq!(a["manifest"]["capsule_id"], b["manifest"]["capsule_id"]);
        assert!(
            a["manifest"]["capsule_id"]
                .as_str()
                .unwrap()
                .starts_with("capsule-blake3:")
        );
    }

    #[test]
    fn emitted_capsule_is_its_own_redacted_replay_fixture() -> Result<(), String> {
        let first = render_repro_capsule_fixture("capsule", Some(&risky_source("redacted")));
        if first.pointer("/fixture_id").and_then(Value::as_str) != Some("capsule") {
            return Err("emitted capsule lost source fixture provenance".to_string());
        }
        let Some(replay_source) = first.pointer("/sources/repro_capsule") else {
            return Err("emitted capsule omitted replay source".to_string());
        };
        let second = render_repro_capsule_fixture("repro-capsule", Some(replay_source));
        if first.pointer("/manifest/capsule_id") != second.pointer("/manifest/capsule_id") {
            return Err("self-replay changed the capsule id".to_string());
        }
        assert_no_leak(&first);
        assert_no_leak(&second);
        Ok(())
    }

    #[test]
    fn all_incident_kinds_classify_known() {
        for kind in INCIDENT_KINDS {
            let src = json!({"incident_kind": kind, "command": "x"});
            let out = render_repro_capsule_fixture("capsule", Some(&src));
            assert_eq!(
                out["summary"]["incident_kind_known"],
                json!(true),
                "kind {kind}"
            );
            assert_eq!(out["status"], json!("ok"), "kind {kind}");
        }
    }

    #[test]
    fn unknown_kind_is_warning() {
        let src = json!({"incident_kind": "meteor-strike", "command": "x"});
        let out = render_repro_capsule_fixture("capsule", Some(&src));
        assert_eq!(out["summary"]["incident_kind_known"], json!(false));
        assert_eq!(out["status"], json!("warning"));
        assert_eq!(
            out["capsule"]["incident_kind"],
            json!("other:meteor-strike")
        );
    }

    #[test]
    fn missing_source_is_partial_not_panic() {
        let out = render_repro_capsule_fixture("empty", None);
        assert_eq!(out["status"], json!("partial"));
        assert_eq!(out["mutation_contract"]["read_only"], json!(true));
        assert_eq!(out["privacy"]["redaction_applied"], json!(true));
    }

    #[test]
    fn live_is_read_only_and_empty() {
        let out = render_repro_capsule_live();
        assert_eq!(out["mutation_contract"]["touches_network"], json!(false));
        assert_eq!(out["capsule"]["session_text"], json!(OMITTED));
    }
}
