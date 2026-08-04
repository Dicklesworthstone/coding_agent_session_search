//! Read-only privacy exposure preview.
//!
//! Shows what CASS *would* read, index, export, or include in support/repro
//! capsules before an operator proceeds. It reports provider roots (redacted),
//! file counts, size ranges, source classes, secret-looking samples **after
//! redaction**, exclusion rules, raw-mirror policy, export/capsule privacy
//! tier, and the exact opt-in flags required for more sensitive modes.
//!
//! Hard guarantees enforced by this module:
//! * It never mutates files, the database, or the network.
//! * It never emits a raw secret value or raw absolute path: every sample is
//!   passed through both the ingestion secret redactor and the strict swarm
//!   evidence redactor before it leaves this module.
//! * Higher-risk reads (live secret scanning, export decryption, support/repro
//!   capsule capture, source-mirror capture) are reported as opt-in only and
//!   are never performed by the preview itself.

use chrono::Utc;
use serde_json::{Value, json};

/// Schema identifier for the privacy exposure preview payload.
pub const SCHEMA_VERSION: &str = "cass.swarm.privacy_exposure.v1";

/// Catalog of sensitive modes that require explicit operator opt-in.
///
/// Tuple shape: `(mode, opt_in_flag, severity, reason)`.
const OPT_IN_MODES: &[(&str, &str, &str, &str)] = &[
    (
        "live-secret-scan",
        "--scan-secrets",
        "review",
        "Scanning live files for secret-like strings reads raw file contents.",
    ),
    (
        "decrypt-export",
        "--allow-decrypt-export",
        "high",
        "Decrypting ChatGPT exports exposes plaintext conversation content.",
    ),
    (
        "support-capsule",
        "--allow-support-capsule",
        "review",
        "Support capsules bundle redacted diagnostics intended for sharing.",
    ),
    (
        "repro-capsule",
        "--allow-repro-capsule",
        "review",
        "Repro capsules embed redacted session excerpts for failure reproduction.",
    ),
    (
        "source-mirror-capture",
        "--allow-source-mirror",
        "high",
        "Source-mirror capture stores raw copies of source files on disk.",
    ),
];

#[derive(Debug, Clone)]
struct ProviderExposure {
    name: String,
    source_class: String,
    roots_redacted: Vec<String>,
    enabled: bool,
    file_count: Option<u64>,
    total_bytes: Option<u64>,
    min_file_bytes: Option<u64>,
    max_file_bytes: Option<u64>,
    symlink_count: u64,
    unreadable_count: u64,
    secret_sample_count: u64,
    redacted_samples: Vec<RedactedSample>,
}

#[derive(Debug, Clone)]
struct RedactedSample {
    kind: &'static str,
    redacted: String,
    original_len: u64,
}

#[derive(Debug, Clone)]
struct ExposureFacts {
    fixture_problem: Option<String>,
    live_enumeration: bool,
    providers: Vec<ProviderExposure>,
    excluded_agents: Vec<String>,
    raw_mirror_enabled: Option<bool>,
    raw_mirror_manifest_count: Option<u64>,
    raw_mirror_storage_bytes: Option<u64>,
    encrypted_export_present: bool,
    html_export_tier: String,
    support_capsule_requested: bool,
    repro_capsule_requested: bool,
    source_mirror_requested: bool,
}

/// Render the live preview. Conservative and fast by default: it does not scan
/// the corpus or sample secrets (those are opt-in higher-risk reads). It only
/// surfaces the static policy and opt-in catalog so operators can see what each
/// action would require before turning it on.
#[must_use]
pub fn render_privacy_exposure_live() -> Value {
    render_payload("live", "live", live_facts())
}

/// Render the preview from a checked-in swarm fixture source value.
#[must_use]
pub fn render_privacy_exposure_fixture(fixture_id: &str, source: Option<&Value>) -> Value {
    render_payload(fixture_id, "fixture", fixture_facts(source))
}

/// Redact a single secret-looking sample. Belt-and-suspenders: applies the
/// ingestion secret redactor (PEM keys, AWS/GitHub/OpenAI/Anthropic tokens,
/// JWTs, Slack/Stripe keys, DB URLs) and then the strict swarm evidence
/// redactor (absolute paths, emails, hostnames, env secret assignments). The
/// raw value is never returned.
fn redact_sample(raw: &str) -> String {
    let stage1 = crate::indexer::redact_secrets::redact_text(raw).into_owned();
    crate::pages::redact::redact_swarm_text(&stage1)
}

fn classify_sample(raw: &str) -> &'static str {
    let lower = raw.to_ascii_lowercase();
    if raw.contains("BEGIN") && raw.contains("PRIVATE KEY") {
        "pem-private-key"
    } else if lower.contains("sk-ant-") {
        "anthropic-key"
    } else if lower.starts_with("sk-") || lower.contains(" sk-") {
        "openai-key"
    } else if lower.contains("ghp_") || lower.contains("github_pat_") {
        "github-token"
    } else if lower.contains("bearer ") {
        "bearer-token"
    } else if raw.contains("AKIA") || raw.contains("ASIA") {
        "aws-access-key"
    } else if raw.contains('@') && raw.contains('.') {
        "email"
    } else if raw.contains('=') {
        "env-assignment"
    } else {
        "secret-like-string"
    }
}

fn render_payload(fixture_id: &str, source_kind: &str, facts: ExposureFacts) -> Value {
    let provider_payloads = facts
        .providers
        .iter()
        .map(render_provider)
        .collect::<Vec<_>>();
    let risk_categories = risk_categories(&facts);
    let required_opt_ins = required_opt_ins(&facts);
    let summary = summarize(&facts, &risk_categories, &required_opt_ins);
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
            "fixture_id": fixture_id,
            "contract": "read-only privacy exposure preview"
        },
        "summary": summary,
        "providers": provider_payloads,
        "exclusions": {
            "excluded_agents": facts.excluded_agents,
            "policy": "excluded agents are never read, indexed, exported, or captured"
        },
        "raw_mirror": {
            "enabled": facts.raw_mirror_enabled,
            "manifest_count": facts.raw_mirror_manifest_count,
            "stored_bytes": facts.raw_mirror_storage_bytes,
            "policy": "raw-mirror stores verbatim source copies; treat as sensitive at rest"
        },
        "exports": {
            "html_export_privacy_tier": facts.html_export_tier,
            "chatgpt_encrypted_present": facts.encrypted_export_present,
            "support_capsule_requested": facts.support_capsule_requested,
            "repro_capsule_requested": facts.repro_capsule_requested,
            "source_mirror_capture_requested": facts.source_mirror_requested,
            "policy": "export and capsule outputs are redaction-first; decryption and raw capture require opt-in"
        },
        "risk_categories": risk_categories,
        "required_opt_ins": required_opt_ins,
        "live_enumeration": {
            "performed": facts.live_enumeration,
            "note": if facts.live_enumeration {
                "live provider enumeration performed without scanning file contents"
            } else {
                "live preview is policy-only by default; pass --fixture <file> for a populated preview or opt in to a scan"
            }
        },
        "mutation_contract": {
            "read_only": true,
            "schedules_work": false,
            "apply_mode": false,
            "mutates_files": false,
            "mutates_db": false,
            "runs_builds": false,
            "touches_network": false,
            "reads_file_contents": false
        },
        "privacy": {
            "contains_session_content": false,
            "contains_raw_secrets": false,
            "contains_raw_paths": false,
            "redaction_applied": true,
            "redaction_policy": "ingestion-secret + strict-swarm-evidence",
            "sampling_is_opt_in": true
        },
        "guided_workflow": {
            "surface": "cass swarm privacy-preview --json",
            "bead_id": "coding_agent_session_search-guided-ops-repro-trust-5u82n.5",
            "apply_mode_available": false,
            "next_step": summary.get("recommended_action").cloned().unwrap_or_else(|| json!("review-exposure"))
        }
    })
}

fn render_provider(provider: &ProviderExposure) -> Value {
    json!({
        "name": provider.name,
        "source_class": provider.source_class,
        "enabled": provider.enabled,
        "roots_redacted": provider.roots_redacted,
        "file_count": provider.file_count,
        "size_range_bytes": {
            "total": provider.total_bytes,
            "min_file": provider.min_file_bytes,
            "max_file": provider.max_file_bytes
        },
        "symlink_count": provider.symlink_count,
        "unreadable_count": provider.unreadable_count,
        "secret_sample_count": provider.secret_sample_count,
        "redacted_samples": provider.redacted_samples.iter().map(|sample| json!({
            "kind": sample.kind,
            "redacted": sample.redacted,
            "original_len": sample.original_len
        })).collect::<Vec<_>>(),
        "flags": provider_flags(provider)
    })
}

fn provider_flags(provider: &ProviderExposure) -> Vec<&'static str> {
    let mut flags = Vec::new();
    if !provider.enabled {
        flags.push("disabled");
    }
    if provider.secret_sample_count > 0 {
        flags.push("secrets-detected");
    }
    if provider.unreadable_count > 0 {
        flags.push("unreadable-paths");
    }
    if provider.symlink_count > 0 {
        flags.push("symlinks-present");
    }
    if provider.file_count.is_none() {
        flags.push("counts-unknown");
    }
    flags
}

fn risk_categories(facts: &ExposureFacts) -> Vec<Value> {
    let mut categories = Vec::new();

    let secret_total: u64 = facts
        .providers
        .iter()
        .map(|provider| provider.secret_sample_count)
        .sum();
    if secret_total > 0 {
        categories.push(json!({
            "category": "secrets-detected",
            "severity": "high",
            "count": secret_total,
            "mitigation": "redaction-applied-automatically",
            "blocking": false
        }));
    }

    let unreadable_total: u64 = facts
        .providers
        .iter()
        .map(|provider| provider.unreadable_count)
        .sum();
    if unreadable_total > 0 {
        categories.push(json!({
            "category": "unreadable-paths",
            "severity": "review",
            "count": unreadable_total,
            "mitigation": "skipped-during-read",
            "blocking": false
        }));
    }

    let symlink_total: u64 = facts
        .providers
        .iter()
        .map(|provider| provider.symlink_count)
        .sum();
    if symlink_total > 0 {
        categories.push(json!({
            "category": "symlinks-present",
            "severity": "review",
            "count": symlink_total,
            "mitigation": "not-followed-outside-provider-root",
            "blocking": false
        }));
    }

    if facts.encrypted_export_present {
        categories.push(json!({
            "category": "encrypted-export-present",
            "severity": "opt-in",
            "count": 1,
            "mitigation": "decryption-requires-opt-in",
            "blocking": false
        }));
    }

    if facts.source_mirror_requested {
        categories.push(json!({
            "category": "source-mirror-capture-requested",
            "severity": "opt-in",
            "count": 1,
            "mitigation": "raw-capture-requires-opt-in",
            "blocking": false
        }));
    }

    if facts.raw_mirror_enabled == Some(true) {
        categories.push(json!({
            "category": "raw-mirror-at-rest",
            "severity": "info",
            "count": facts.raw_mirror_manifest_count.unwrap_or(0),
            "mitigation": "stored-locally-treat-as-sensitive",
            "blocking": false
        }));
    }

    if facts.fixture_problem.is_some() {
        categories.push(json!({
            "category": "incomplete-input",
            "severity": "review",
            "count": 1,
            "mitigation": "fall-back-to-policy-only-preview",
            "blocking": false
        }));
    }

    categories
}

fn required_opt_ins(facts: &ExposureFacts) -> Vec<Value> {
    OPT_IN_MODES
        .iter()
        .map(|(mode, flag, severity, reason)| {
            let requested = match *mode {
                "decrypt-export" => facts.encrypted_export_present,
                "support-capsule" => facts.support_capsule_requested,
                "repro-capsule" => facts.repro_capsule_requested,
                "source-mirror-capture" => facts.source_mirror_requested,
                "live-secret-scan" => false,
                _ => false,
            };
            json!({
                "mode": mode,
                "flag": flag,
                "severity": severity,
                "default_state": "off",
                "currently_requested": requested,
                "reason": reason
            })
        })
        .collect()
}

fn summarize(
    facts: &ExposureFacts,
    risk_categories: &[Value],
    required_opt_ins: &[Value],
) -> Value {
    let provider_count = facts.providers.len();
    let enabled_count = facts
        .providers
        .iter()
        .filter(|provider| provider.enabled)
        .count();
    let secret_total: u64 = facts
        .providers
        .iter()
        .map(|provider| provider.secret_sample_count)
        .sum();
    let high_risk_count = risk_categories
        .iter()
        .filter(|category| category.get("severity").and_then(Value::as_str) == Some("high"))
        .count();
    let review_count = risk_categories
        .iter()
        .filter(|category| category.get("severity").and_then(Value::as_str) == Some("review"))
        .count();
    let opt_in_pending = required_opt_ins
        .iter()
        .filter(|opt_in| opt_in.get("currently_requested").and_then(Value::as_bool) == Some(true))
        .count();

    let readiness = if opt_in_pending > 0 {
        "opt-in-required"
    } else if high_risk_count > 0 || review_count > 0 {
        "review-required"
    } else {
        "ready"
    };
    let status = if facts.fixture_problem.is_some() {
        "partial"
    } else if high_risk_count > 0 || review_count > 0 || opt_in_pending > 0 {
        "warning"
    } else {
        "ok"
    };
    let recommended_action = if opt_in_pending > 0 {
        "confirm-required-opt-ins"
    } else if secret_total > 0 {
        "review-redacted-secret-samples"
    } else if review_count > 0 {
        "review-flagged-providers"
    } else if provider_count == 0 {
        "supply-fixture-or-enable-scan"
    } else {
        "proceed-with-redaction"
    };

    json!({
        "status": status,
        "readiness": readiness,
        "provider_count": provider_count,
        "enabled_provider_count": enabled_count,
        "excluded_agent_count": facts.excluded_agents.len(),
        "secret_sample_count": secret_total,
        "high_risk_count": high_risk_count,
        "review_count": review_count,
        "opt_in_pending_count": opt_in_pending,
        "recommended_action": recommended_action
    })
}

fn live_facts() -> ExposureFacts {
    ExposureFacts {
        fixture_problem: None,
        live_enumeration: false,
        providers: Vec::new(),
        excluded_agents: Vec::new(),
        raw_mirror_enabled: None,
        raw_mirror_manifest_count: None,
        raw_mirror_storage_bytes: None,
        encrypted_export_present: false,
        html_export_tier: "redacted".to_string(),
        support_capsule_requested: false,
        repro_capsule_requested: false,
        source_mirror_requested: false,
    }
}

fn fixture_facts(source: Option<&Value>) -> ExposureFacts {
    let Some(source) = source else {
        return ExposureFacts {
            fixture_problem: Some("privacy_exposure fixture source is missing".to_string()),
            ..live_facts()
        };
    };

    let providers = source
        .get("providers")
        .and_then(Value::as_array)
        .map(|items| items.iter().map(parse_provider).collect::<Vec<_>>())
        .unwrap_or_default();

    let excluded_agents = source
        .get("excluded_agents")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                // Agent names are not sensitive, but redact defensively in case
                // an operator mislabels a path as an agent name.
                .map(redact_sample)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    ExposureFacts {
        fixture_problem: None,
        live_enumeration: false,
        providers,
        excluded_agents,
        raw_mirror_enabled: value_bool(source, &["raw_mirror", "enabled"]),
        raw_mirror_manifest_count: value_u64(source, &["raw_mirror", "manifest_count"]),
        raw_mirror_storage_bytes: value_u64(source, &["raw_mirror", "total_storage_bytes"]),
        encrypted_export_present: value_bool(source, &["exports", "chatgpt_encrypted_present"])
            .unwrap_or(false),
        html_export_tier: value_str(source, &["exports", "html_export_tier"])
            .unwrap_or("redacted")
            .to_string(),
        support_capsule_requested: value_bool(source, &["support_capsule", "requested"])
            .unwrap_or(false),
        repro_capsule_requested: value_bool(source, &["repro_capsule", "requested"])
            .unwrap_or(false),
        source_mirror_requested: value_bool(source, &["source_mirror_capture", "requested"])
            .unwrap_or(false),
    }
}

fn parse_provider(value: &Value) -> ProviderExposure {
    let roots_redacted = value
        .get("roots")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(redact_sample)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let raw_samples = value
        .get("secret_samples")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).collect::<Vec<_>>())
        .unwrap_or_default();
    let secret_sample_count =
        value_u64(value, &["secret_sample_count"]).unwrap_or(raw_samples.len() as u64);
    let redacted_samples = raw_samples
        .iter()
        .map(|raw| RedactedSample {
            kind: classify_sample(raw),
            redacted: redact_sample(raw),
            original_len: raw.chars().count() as u64,
        })
        .collect::<Vec<_>>();

    ProviderExposure {
        name: value
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string(),
        source_class: value
            .get("source_class")
            .and_then(Value::as_str)
            .unwrap_or("unspecified")
            .to_string(),
        enabled: value
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        roots_redacted,
        file_count: value_u64(value, &["file_count"]),
        total_bytes: value_u64(value, &["total_bytes"]),
        min_file_bytes: value_u64(value, &["min_file_bytes"]),
        max_file_bytes: value_u64(value, &["max_file_bytes"]),
        symlink_count: value_u64(value, &["symlink_count"]).unwrap_or(0),
        unreadable_count: value_u64(value, &["unreadable_count"]).unwrap_or(0),
        secret_sample_count,
        redacted_samples,
    }
}

fn value_u64(value: &Value, path: &[&str]) -> Option<u64> {
    get_value(value, path).and_then(Value::as_u64)
}

fn value_bool(value: &Value, path: &[&str]) -> Option<bool> {
    get_value(value, path).and_then(Value::as_bool)
}

fn value_str<'a>(value: &'a Value, path: &[&str]) -> Option<&'a str> {
    get_value(value, path).and_then(Value::as_str)
}

fn get_value<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    Some(current)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_no_raw_leaks(value: &Value) {
        let text = serde_json::to_string(value).expect("serialize");
        for needle in [
            "/home/",
            "/Users/",
            "/data/projects/",
            "sk-ant-",
            "ghp_",
            "AKIA",
            "alice@example.com",
            "BEGIN RSA PRIVATE KEY",
            "Bearer abcd",
            "TOKEN=supersecret",
        ] {
            assert!(
                !text.contains(needle),
                "privacy preview leaked forbidden text: {needle}"
            );
        }
    }

    fn risky_source() -> Value {
        // Preserve a realistic runtime token shape for redaction/classification
        // coverage without storing a contiguous key-shaped literal that static
        // secret scanners can mistake for a live credential.
        let synthetic_anthropic_key =
            ["sk-ant-", "api03-", "AAAABBBBCCCCDDDDEEEEFFFFGGGG"].concat();
        json!({
            "providers": [
                {
                    "name": "claude-code",
                    "source_class": "local-agent-history",
                    "enabled": true,
                    "roots": ["/home/ubuntu/.claude/projects"],
                    "file_count": 1280,
                    "total_bytes": 524288000u64,
                    "min_file_bytes": 128,
                    "max_file_bytes": 8388608u64,
                    "symlink_count": 2,
                    "unreadable_count": 1,
                    "secret_samples": [
                        synthetic_anthropic_key,
                        "contact alice@example.com for access",
                        "TOKEN=supersecretvalue123456",
                        "-----BEGIN RSA PRIVATE KEY-----abcdef"
                    ]
                }
            ],
            "excluded_agents": ["aider", "cursor"],
            "raw_mirror": {"enabled": true, "manifest_count": 42, "total_storage_bytes": 104857600u64},
            "exports": {"chatgpt_encrypted_present": true, "html_export_tier": "redacted"},
            "support_capsule": {"requested": true},
            "source_mirror_capture": {"requested": true}
        })
    }

    #[test]
    fn redacts_all_secret_samples_and_paths() {
        let output = render_privacy_exposure_fixture("privacy-risky", Some(&risky_source()));
        assert_no_raw_leaks(&output);
    }

    #[test]
    fn flags_secrets_and_requires_opt_in() {
        let output = render_privacy_exposure_fixture("privacy-risky", Some(&risky_source()));
        assert_eq!(output["status"], json!("warning"));
        assert_eq!(output["summary"]["readiness"], json!("opt-in-required"));
        assert_eq!(output["summary"]["secret_sample_count"], json!(4));
        // Encrypted export and source-mirror capture must be requested opt-ins.
        let pending = output["required_opt_ins"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|opt_in| opt_in["currently_requested"] == json!(true))
            .count();
        assert!(pending >= 2, "expected at least two pending opt-ins");
    }

    #[test]
    fn redacted_samples_classify_kinds() {
        let output = render_privacy_exposure_fixture("privacy-risky", Some(&risky_source()));
        let samples = output["providers"][0]["redacted_samples"]
            .as_array()
            .unwrap();
        let kinds: Vec<&str> = samples
            .iter()
            .map(|sample| sample["kind"].as_str().unwrap())
            .collect();
        assert!(kinds.contains(&"anthropic-key"));
        assert!(kinds.contains(&"email"));
        assert!(kinds.contains(&"pem-private-key"));
        // Every redacted sample must differ from a raw secret marker.
        for sample in samples {
            let redacted = sample["redacted"].as_str().unwrap();
            assert!(!redacted.contains("sk-ant-"));
            assert!(!redacted.contains("alice@example.com"));
        }
    }

    #[test]
    fn missing_source_is_partial_not_panic() {
        let output = render_privacy_exposure_fixture("privacy-empty", None);
        assert_eq!(output["status"], json!("partial"));
        assert_eq!(output["mutation_contract"]["read_only"], json!(true));
        assert_eq!(output["privacy"]["redaction_applied"], json!(true));
    }

    #[test]
    fn live_preview_is_policy_only_and_read_only() {
        let output = render_privacy_exposure_live();
        assert_eq!(output["live_enumeration"]["performed"], json!(false));
        assert_eq!(
            output["mutation_contract"]["reads_file_contents"],
            json!(false)
        );
        assert_eq!(
            output["summary"]["recommended_action"],
            json!("supply-fixture-or-enable-scan")
        );
        // The opt-in catalog is always present, even with no providers.
        assert_eq!(
            output["required_opt_ins"].as_array().unwrap().len(),
            OPT_IN_MODES.len()
        );
    }
}
