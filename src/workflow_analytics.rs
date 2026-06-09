//! Workflow outcome analytics for swarm coordination.
//!
//! Extends analytics from tokens/tools into swarm *outcomes*: which skills,
//! commands, proof gates, file areas, and agents correlate with fast clean
//! closure versus reopen, failed gate, or proof debt.
//!
//! Operates on already-redacted, metadata-first workflow records (fixture- or
//! caller-supplied). It reports aggregate metrics only — never raw prompt text —
//! and attaches confidence, sample size, and stale-data markers so operators do
//! not overfit small or aging samples.
//!
//! Guarantees:
//! * Read-only and deterministic: rates are integer per-mille, group ordering is
//!   by (sample_size desc, key asc), and time math uses the fixture `now_ms`,
//!   never the system clock.
//! * Privacy-first: any free-text note is passed through the strict
//!   swarm-evidence redactor; file areas are redacted as paths. No raw excerpt
//!   or secret can leak.
//! * Rollup/slow-path parity: per-dimension group counts always sum back to the
//!   overall outcome counts (asserted by tests).

use chrono::Utc;
use serde_json::{Value, json};

/// Schema identifier for the workflow analytics payload.
pub const SCHEMA_VERSION: &str = "cass.swarm.workflow_analytics.v1";

/// Default lookback window when the fixture does not pin one.
pub const DEFAULT_WINDOW_DAYS: u64 = 30;

const MS_PER_DAY: i64 = 86_400_000;

/// Sample-size thresholds for confidence labelling.
const CONFIDENCE_HIGH_N: usize = 20;
const CONFIDENCE_MEDIUM_N: usize = 8;

/// The closure outcomes we track, in stable reporting order.
const OUTCOMES: &[&str] = &["clean_close", "reopen", "failed_gate", "proof_debt"];

/// Dimensions we group by, in stable reporting order.
const DIMENSIONS: &[&str] = &["skill", "command", "proof_gate", "file_area", "agent"];

/// Metadata fields required for a record to count as "fully covered".
const COVERAGE_FIELDS: &[&str] = &["skill", "command", "proof_gate", "file_area", "outcome"];

#[derive(Debug, Clone)]
struct Record {
    ts_ms: Option<i64>,
    agent: Option<String>,
    source: Option<String>,
    workspace: Option<String>,
    skill: Option<String>,
    command: Option<String>,
    proof_gate: Option<String>,
    file_area: Option<String>,
    outcome: String,
    duration_ms: Option<u64>,
}

#[derive(Debug, Clone)]
struct Filters {
    agent: Option<String>,
    source: Option<String>,
    workspace: Option<String>,
}

#[derive(Debug, Clone)]
struct AnalyticsFacts {
    fixture_problem: Option<String>,
    now_ms: Option<i64>,
    window_days: u64,
    filters: Filters,
    records: Vec<Record>,
}

/// Outcome tallies for a record set.
#[derive(Debug, Clone, Default)]
struct OutcomeCounts {
    clean_close: usize,
    reopen: usize,
    failed_gate: usize,
    proof_debt: usize,
    other: usize,
    duration_samples: Vec<u64>,
    newest_ts_ms: Option<i64>,
}

impl OutcomeCounts {
    fn add(&mut self, record: &Record) {
        match record.outcome.as_str() {
            "clean_close" => self.clean_close += 1,
            "reopen" => self.reopen += 1,
            "failed_gate" => self.failed_gate += 1,
            "proof_debt" => self.proof_debt += 1,
            _ => self.other += 1,
        }
        if let Some(duration) = record.duration_ms {
            self.duration_samples.push(duration);
        }
        if let Some(ts) = record.ts_ms {
            self.newest_ts_ms = Some(self.newest_ts_ms.map_or(ts, |cur| cur.max(ts)));
        }
    }

    fn total(&self) -> usize {
        self.clean_close + self.reopen + self.failed_gate + self.proof_debt + self.other
    }

    fn count_for(&self, outcome: &str) -> usize {
        match outcome {
            "clean_close" => self.clean_close,
            "reopen" => self.reopen,
            "failed_gate" => self.failed_gate,
            "proof_debt" => self.proof_debt,
            _ => 0,
        }
    }
}

/// Render the live payload. Conservative: with no caller-supplied records it
/// reports an empty, fully-degraded analytics envelope documenting the contract.
#[must_use]
pub fn render_workflow_analytics_live() -> Value {
    render_payload("live", "live", live_facts())
}

/// Render the payload from a checked-in swarm fixture source value.
#[must_use]
pub fn render_workflow_analytics_fixture(fixture_id: &str, source: Option<&Value>) -> Value {
    render_payload(fixture_id, "fixture", fixture_facts(source))
}

fn redact(text: &str) -> String {
    crate::pages::redact::redact_swarm_text(text)
}

fn render_payload(fixture_id: &str, source_kind: &str, facts: AnalyticsFacts) -> Value {
    let window_ms = (facts.window_days.max(1) as i64).saturating_mul(MS_PER_DAY);
    // In-window filter + attribute filters. Records without a timestamp are kept
    // (treated as undated) so missing-metadata fixtures still aggregate.
    let in_scope: Vec<&Record> = facts
        .records
        .iter()
        .filter(|record| in_window(record, facts.now_ms, window_ms))
        .filter(|record| matches_filters(record, &facts.filters))
        .collect();

    let mut overall = OutcomeCounts::default();
    let mut covered = 0usize;
    for record in &in_scope {
        overall.add(record);
        if is_fully_covered(record) {
            covered += 1;
        }
    }

    let dimensions = DIMENSIONS
        .iter()
        .map(|dimension| render_dimension(dimension, &in_scope, &facts, window_ms))
        .collect::<Vec<_>>();

    let summary = summarize(&facts, &in_scope, &overall, covered);
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
            "contract": "read-only aggregate workflow outcome analytics"
        },
        "window": {
            "window_days": facts.window_days,
            "now_ms": facts.now_ms,
            "filters": {
                "agent": facts.filters.agent,
                "source": facts.filters.source,
                "workspace": facts.filters.workspace
            }
        },
        "summary": summary,
        "overall_outcomes": render_outcome_block(&overall),
        "dimensions": dimensions,
        "mutation_contract": {
            "read_only": true,
            "schedules_work": false,
            "mutates_files": false,
            "mutates_db": false,
            "touches_network": false
        },
        "privacy": {
            "aggregate_only": true,
            "contains_prompt_text": false,
            "redaction_applied": true
        },
        "guided_workflow": {
            "surface": "cass swarm workflow-analytics --json",
            "bead_id": "coding_agent_session_search-swarm-coordination-intelligence-gnrxb.5",
            "apply_mode_available": false,
            "next_step": summary.get("recommended_action").cloned().unwrap_or_else(|| json!("review-outcome-rates"))
        }
    })
}

fn render_dimension(
    dimension: &str,
    in_scope: &[&Record],
    facts: &AnalyticsFacts,
    window_ms: i64,
) -> Value {
    // Group records by the dimension key (redacted), preserving first-seen order
    // only as a fallback; final ordering is deterministic below.
    let mut keys: Vec<String> = Vec::new();
    let mut groups: Vec<OutcomeCounts> = Vec::new();
    for record in in_scope {
        let key = dimension_key(dimension, record);
        let idx = match keys.iter().position(|existing| existing == &key) {
            Some(idx) => idx,
            None => {
                keys.push(key);
                groups.push(OutcomeCounts::default());
                groups.len() - 1
            }
        };
        groups[idx].add(record);
    }

    let mut rows: Vec<Value> = keys
        .iter()
        .zip(groups.iter())
        .map(|(key, counts)| render_group(key, counts, facts.now_ms, window_ms))
        .collect();
    // Deterministic order: sample size desc, then key asc.
    rows.sort_by(|a, b| {
        let an = a.get("sample_size").and_then(Value::as_u64).unwrap_or(0);
        let bn = b.get("sample_size").and_then(Value::as_u64).unwrap_or(0);
        bn.cmp(&an).then_with(|| {
            let ak = a.get("key").and_then(Value::as_str).unwrap_or("");
            let bk = b.get("key").and_then(Value::as_str).unwrap_or("");
            ak.cmp(bk)
        })
    });

    json!({
        "dimension": dimension,
        "group_count": rows.len(),
        "groups": rows
    })
}

fn render_group(key: &str, counts: &OutcomeCounts, now_ms: Option<i64>, window_ms: i64) -> Value {
    let n = counts.total();
    let stale = is_stale(counts.newest_ts_ms, now_ms, window_ms);
    let mut block = render_outcome_block(counts);
    if let Value::Object(map) = &mut block {
        map.insert("key".to_string(), json!(key));
        map.insert("sample_size".to_string(), json!(n));
        map.insert("confidence".to_string(), json!(confidence_for(n)));
        map.insert("stale".to_string(), json!(stale));
        map.insert(
            "newest_ts_ms".to_string(),
            counts.newest_ts_ms.map_or(Value::Null, |ts| json!(ts)),
        );
    }
    block
}

fn render_outcome_block(counts: &OutcomeCounts) -> Value {
    let n = counts.total();
    let rates: serde_json::Map<String, Value> = OUTCOMES
        .iter()
        .map(|outcome| {
            (
                format!("{outcome}_per_mille"),
                json!(per_mille(counts.count_for(outcome), n)),
            )
        })
        .collect();
    json!({
        "sample_size": n,
        "counts": {
            "clean_close": counts.clean_close,
            "reopen": counts.reopen,
            "failed_gate": counts.failed_gate,
            "proof_debt": counts.proof_debt,
            "unclassified": counts.other
        },
        "rates_per_mille": rates,
        "proof_debt_per_mille": per_mille(counts.proof_debt, n),
        "clean_close_per_mille": per_mille(counts.clean_close, n),
        "median_duration_ms": median(&counts.duration_samples),
        "mean_duration_ms": mean(&counts.duration_samples)
    })
}

fn summarize(
    facts: &AnalyticsFacts,
    in_scope: &[&Record],
    overall: &OutcomeCounts,
    covered: usize,
) -> Value {
    let n = in_scope.len();
    let total_records = facts.records.len();
    let evidence_coverage_per_mille = per_mille(covered, n);
    let low_sample = n < CONFIDENCE_MEDIUM_N;
    let stale_present = facts.now_ms.is_some()
        && in_scope.iter().all(|record| {
            record.ts_ms.is_some_and(|ts| {
                facts.now_ms.is_some_and(|now| {
                    now.saturating_sub(ts) > (facts.window_days.max(1) as i64) * MS_PER_DAY / 2
                })
            })
        })
        && n > 0;

    let status = if facts.fixture_problem.is_some() {
        "partial"
    } else if n == 0 || low_sample || stale_present {
        "warning"
    } else {
        "ok"
    };
    let recommended_action = if facts.fixture_problem.is_some() {
        "supply-analytics-fixture"
    } else if n == 0 {
        "no-records-in-window"
    } else if low_sample {
        "collect-more-samples-before-acting"
    } else if overall.proof_debt + overall.reopen + overall.failed_gate > overall.clean_close {
        "investigate-high-failure-dimensions"
    } else {
        "review-outcome-rates"
    };

    json!({
        "status": status,
        "total_records": total_records,
        "records_in_scope": n,
        "evidence_coverage_per_mille": evidence_coverage_per_mille,
        "low_sample_size": low_sample,
        "stale_data_present": stale_present,
        "overall_clean_close_per_mille": per_mille(overall.clean_close, n),
        "overall_proof_debt_per_mille": per_mille(overall.proof_debt, n),
        "recommended_action": recommended_action
    })
}

fn in_window(record: &Record, now_ms: Option<i64>, window_ms: i64) -> bool {
    match (record.ts_ms, now_ms) {
        (Some(ts), Some(now)) => {
            let age = now.saturating_sub(ts);
            (0..=window_ms).contains(&age) || age < 0
        }
        // Undated records or an undated "now" are kept (can't window-filter).
        _ => true,
    }
}

fn matches_filters(record: &Record, filters: &Filters) -> bool {
    filter_matches(&record.agent, &filters.agent)
        && filter_matches(&record.source, &filters.source)
        && filter_matches(&record.workspace, &filters.workspace)
}

fn filter_matches(value: &Option<String>, filter: &Option<String>) -> bool {
    match filter {
        None => true,
        Some(want) => value.as_deref() == Some(want.as_str()),
    }
}

fn is_fully_covered(record: &Record) -> bool {
    COVERAGE_FIELDS.iter().all(|field| match *field {
        "skill" => record.skill.is_some(),
        "command" => record.command.is_some(),
        "proof_gate" => record.proof_gate.is_some(),
        "file_area" => record.file_area.is_some(),
        "outcome" => !record.outcome.is_empty() && record.outcome != "unspecified",
        _ => true,
    })
}

fn dimension_key(dimension: &str, record: &Record) -> String {
    let raw = match dimension {
        "skill" => record.skill.as_deref(),
        "command" => record.command.as_deref(),
        "proof_gate" => record.proof_gate.as_deref(),
        "file_area" => record.file_area.as_deref(),
        "agent" => record.agent.as_deref(),
        _ => None,
    };
    match raw {
        Some(value) if !value.is_empty() => redact(value),
        _ => "unspecified".to_string(),
    }
}

fn confidence_for(n: usize) -> &'static str {
    if n >= CONFIDENCE_HIGH_N {
        "high"
    } else if n >= CONFIDENCE_MEDIUM_N {
        "medium"
    } else {
        "low"
    }
}

fn is_stale(newest_ts_ms: Option<i64>, now_ms: Option<i64>, window_ms: i64) -> bool {
    match (newest_ts_ms, now_ms) {
        (Some(newest), Some(now)) => now.saturating_sub(newest) > window_ms / 2,
        _ => false,
    }
}

fn per_mille(count: usize, total: usize) -> u64 {
    ((count as u64) * 1000)
        .checked_div(total as u64)
        .unwrap_or(0)
}

fn median(samples: &[u64]) -> Option<u64> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let mid = sorted.len() / 2;
    if sorted.len() % 2 == 1 {
        Some(sorted[mid])
    } else {
        Some((sorted[mid - 1] + sorted[mid]) / 2)
    }
}

fn mean(samples: &[u64]) -> Option<u64> {
    if samples.is_empty() {
        return None;
    }
    let sum: u64 = samples.iter().sum();
    Some(sum / samples.len() as u64)
}

fn live_facts() -> AnalyticsFacts {
    AnalyticsFacts {
        fixture_problem: None,
        now_ms: None,
        window_days: DEFAULT_WINDOW_DAYS,
        filters: Filters {
            agent: None,
            source: None,
            workspace: None,
        },
        records: Vec::new(),
    }
}

fn fixture_facts(source: Option<&Value>) -> AnalyticsFacts {
    let Some(source) = source else {
        return AnalyticsFacts {
            fixture_problem: Some("workflow_analytics fixture source is missing".to_string()),
            ..live_facts()
        };
    };

    let records = source
        .get("records")
        .and_then(Value::as_array)
        .map(|items| items.iter().map(parse_record).collect::<Vec<_>>())
        .unwrap_or_default();

    AnalyticsFacts {
        fixture_problem: None,
        now_ms: source.get("now_ms").and_then(Value::as_i64),
        window_days: source
            .get("window_days")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_WINDOW_DAYS),
        filters: Filters {
            agent: filter_field(source, "agent"),
            source: filter_field(source, "source"),
            workspace: filter_field(source, "workspace"),
        },
        records,
    }
}

fn filter_field(source: &Value, field: &str) -> Option<String> {
    source
        .get("filters")
        .and_then(|filters| filters.get(field))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn parse_record(value: &Value) -> Record {
    let opt_str = |field: &str| {
        value
            .get(field)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    Record {
        ts_ms: value.get("ts_ms").and_then(Value::as_i64),
        agent: opt_str("agent"),
        source: opt_str("source"),
        workspace: opt_str("workspace"),
        skill: opt_str("skill"),
        command: opt_str("command"),
        proof_gate: opt_str("proof_gate"),
        file_area: opt_str("file_area"),
        outcome: value
            .get("outcome")
            .and_then(Value::as_str)
            .unwrap_or("unspecified")
            .to_string(),
        duration_ms: value.get("duration_ms").and_then(Value::as_u64),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW_MS: i64 = 1_749_456_000_000;
    const DAY_MS: i64 = 86_400_000;

    fn record(
        ts_offset_days: i64,
        agent: &str,
        skill: &str,
        outcome: &str,
        duration_ms: u64,
    ) -> Value {
        json!({
            "ts_ms": NOW_MS - ts_offset_days * DAY_MS,
            "agent": agent,
            "source": "local",
            "workspace": "cass",
            "skill": skill,
            "command": "cargo clippy",
            "proof_gate": "clippy",
            "file_area": "/home/alice/src/swarm",
            "outcome": outcome,
            "duration_ms": duration_ms
        })
    }

    fn source() -> Value {
        json!({
            "now_ms": NOW_MS,
            "window_days": 30,
            "records": [
                record(1, "cc", "ubs", "clean_close", 1000),
                record(2, "cc", "ubs", "clean_close", 1200),
                record(3, "cc", "ubs", "reopen", 5000),
                record(1, "cod", "rch", "failed_gate", 8000),
                record(40, "cod", "rch", "proof_debt", 9000) // out of 30-day window
            ]
        })
    }

    #[test]
    fn windows_out_old_records() {
        let out = render_workflow_analytics_fixture("wf", Some(&source()));
        // 5 records, 1 outside the 30-day window -> 4 in scope.
        assert_eq!(out["summary"]["total_records"], json!(5));
        assert_eq!(out["summary"]["records_in_scope"], json!(4));
    }

    #[test]
    fn rollup_slow_path_parity_agent_groups_sum_to_overall() {
        let out = render_workflow_analytics_fixture("wf", Some(&source()));
        let overall = &out["overall_outcomes"]["counts"];
        // The "agent" dimension partitions all in-scope records, so its group
        // counts must sum back to the overall counts.
        let agent_dim = out["dimensions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["dimension"] == json!("agent"))
            .unwrap();
        let mut sum = (0u64, 0u64, 0u64, 0u64);
        for g in agent_dim["groups"].as_array().unwrap() {
            sum.0 += g["counts"]["clean_close"].as_u64().unwrap();
            sum.1 += g["counts"]["reopen"].as_u64().unwrap();
            sum.2 += g["counts"]["failed_gate"].as_u64().unwrap();
            sum.3 += g["counts"]["proof_debt"].as_u64().unwrap();
        }
        assert_eq!(sum.0, overall["clean_close"].as_u64().unwrap());
        assert_eq!(sum.1, overall["reopen"].as_u64().unwrap());
        assert_eq!(sum.2, overall["failed_gate"].as_u64().unwrap());
        assert_eq!(sum.3, overall["proof_debt"].as_u64().unwrap());
    }

    #[test]
    fn deterministic_and_low_sample_flagged() {
        let out = render_workflow_analytics_fixture("wf", Some(&source()));
        let out2 = render_workflow_analytics_fixture("wf", Some(&source()));
        assert_eq!(out["dimensions"], out2["dimensions"]);
        // 4 in-scope records < medium threshold -> low sample, warning.
        assert_eq!(out["summary"]["low_sample_size"], json!(true));
        assert_eq!(out["status"], json!("warning"));
    }

    #[test]
    fn filters_restrict_scope() {
        let mut src = source();
        src["filters"] = json!({"agent": "cc"});
        let out = render_workflow_analytics_fixture("wf", Some(&src));
        // Only cc's 3 in-window records remain.
        assert_eq!(out["summary"]["records_in_scope"], json!(3));
    }

    #[test]
    fn missing_metadata_lowers_coverage_and_no_path_leak() {
        let src = json!({
            "now_ms": NOW_MS,
            "window_days": 30,
            "records": [
                {"ts_ms": NOW_MS - DAY_MS, "outcome": "clean_close", "file_area": "/home/alice/secret"},
                record(1, "cc", "ubs", "clean_close", 1000)
            ]
        });
        let out = render_workflow_analytics_fixture("wf", Some(&src));
        // One record missing skill/command/proof_gate -> coverage < 1000 per-mille.
        let coverage = out["summary"]["evidence_coverage_per_mille"].as_u64().unwrap();
        assert!(coverage < 1000, "coverage should reflect missing metadata");
        // No absolute path may leak (file_area redacted as a dimension key).
        let text = serde_json::to_string(&out).unwrap();
        assert!(!text.contains("/home/"), "file path leaked");
    }

    #[test]
    fn missing_source_is_partial_not_panic() {
        let out = render_workflow_analytics_fixture("empty", None);
        assert_eq!(out["status"], json!("partial"));
        assert_eq!(out["mutation_contract"]["read_only"], json!(true));
        assert_eq!(out["privacy"]["aggregate_only"], json!(true));
    }

    #[test]
    fn live_is_empty_and_read_only() {
        let out = render_workflow_analytics_live();
        assert_eq!(out["summary"]["records_in_scope"], json!(0));
        assert_eq!(out["summary"]["recommended_action"], json!("no-records-in-window"));
        assert_eq!(out["mutation_contract"]["touches_network"], json!(false));
    }
}
