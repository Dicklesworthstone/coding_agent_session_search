// The live miner uses classification and stable ids; descriptor lookups and
// the `Other` forward-compatibility branch remain contract/test surfaces that
// intentionally are not all called by production dispatch yet.
#![allow(dead_code)]

//! Incident-mining category schema (bead
//! cass-fleet-resilience-20260608-uojcg.10.1).
//!
//! `cass` incident/history triage needs a stable category vocabulary so a
//! mined candidate can be classified, attributed to a root-cause family, and
//! handled under the right privacy tier. This module freezes the report's
//! fourth-pass classifier as that vocabulary: a fixed set of categories, each
//! with a stable id, description, detection signals, example terms, a
//! baseline detection confidence, the associated
//! [`RootCauseFamily`](crate::root_cause_taxonomy::RootCauseFamily), a privacy
//! tier, and a recommended next probe.
//!
//! Forward-compatibility: an explicit [`IncidentCategory::Other`] variant and
//! `from_id` returning `None` for unrecognised ids mean new categories can be
//! added without breaking consumers that match the stable ids. All enums
//! serialize as snake_case.

use serde::{Deserialize, Serialize};

use crate::root_cause_taxonomy::RootCauseFamily;

/// How reliably a category's detection signals indicate it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DetectionConfidence {
    Low,
    Medium,
    High,
}

/// Privacy tier governing how a mined incident of this category may be
/// surfaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PrivacyTier {
    /// Operational signals only (no user content or identifying paths).
    Operational,
    /// May reference paths/workspaces/hosts; redact before surfacing.
    Redacted,
    /// May include session content; gated behind explicit consent.
    Sensitive,
}

/// The stable incident categories from the report's fourth-pass classifier.
/// `Other` is the explicit extensibility escape hatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum IncidentCategory {
    CassStatusHealth,
    IndexStaleMissing,
    IndexStallProgress,
    SearchZeroWorkspace,
    QuarantineOom,
    StorageBusyCorrupt,
    RemoteSyncAuth,
    Semantic,
    WatchSalvageIssues,
    HostPressure,
    DependencyAttribution,
    /// A future / unclassified category. Never breaks the contract.
    Other,
}

/// The schema for one incident category.
// `&'static [&'static str]` fields can be serialized but not deserialized, so this
// static catalog descriptor is Serialize-only (matches RootCauseDescriptor).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct IncidentCategoryDef {
    pub category: IncidentCategory,
    /// Stable snake_case id (matches the serialized `category`).
    pub id: &'static str,
    pub description: &'static str,
    /// Signals a miner looks for (log markers, err.kind, status fields).
    pub detection_signals: &'static [&'static str],
    /// Example query terms that surface this category in history.
    pub example_terms: &'static [&'static str],
    /// Baseline confidence that the signals indicate this category.
    pub confidence: DetectionConfidence,
    /// The root-cause family most often implicated (best-effort a-priori).
    pub root_cause_family: RootCauseFamily,
    pub privacy_tier: PrivacyTier,
    /// The recommended next bounded probe when this category is suspected.
    pub recommended_next_probe: &'static str,
}

impl IncidentCategory {
    /// Stable snake_case id for this category.
    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::CassStatusHealth => "cass_status_health",
            Self::IndexStaleMissing => "index_stale_missing",
            Self::IndexStallProgress => "index_stall_progress",
            Self::SearchZeroWorkspace => "search_zero_workspace",
            Self::QuarantineOom => "quarantine_oom",
            Self::StorageBusyCorrupt => "storage_busy_corrupt",
            Self::RemoteSyncAuth => "remote_sync_auth",
            Self::Semantic => "semantic",
            Self::WatchSalvageIssues => "watch_salvage_issues",
            Self::HostPressure => "host_pressure",
            Self::DependencyAttribution => "dependency_attribution",
            Self::Other => "other",
        }
    }

    /// Resolve a category from its stable id. Returns `None` for unknown ids
    /// so a future category surfaces as unrecognised rather than silently
    /// mapping to an existing one.
    pub(crate) fn from_id(id: &str) -> Option<Self> {
        CATEGORIES
            .iter()
            .map(|d| d.category)
            .chain(std::iter::once(Self::Other))
            .find(|c| c.id() == id)
    }
}

/// The frozen category schema, in a stable order. `Other` is intentionally
/// excluded from the seeded set (it is the open extension point).
static CATEGORIES: &[IncidentCategoryDef] = &[
    IncidentCategoryDef {
        category: IncidentCategory::CassStatusHealth,
        id: "cass_status_health",
        description: "cass status/health reports degraded, unhealthy, or contradictory readiness",
        detection_signals: &[
            "health_class",
            "recommended_action",
            "unhealthy",
            "degraded",
        ],
        example_terms: &["health", "status", "unhealthy", "recommended action"],
        confidence: DetectionConfidence::High,
        root_cause_family: RootCauseFamily::CassDerivedState,
        privacy_tier: PrivacyTier::Operational,
        recommended_next_probe: "cass health --json",
    },
    IncidentCategoryDef {
        category: IncidentCategory::IndexStaleMissing,
        id: "index_stale_missing",
        description: "the lexical index is stale, missing, or not initialized",
        detection_signals: &["index_freshness", "not_initialized", "stale", "OpenRead"],
        example_terms: &["stale index", "no index", "reindex", "index missing"],
        confidence: DetectionConfidence::High,
        root_cause_family: RootCauseFamily::CassDerivedState,
        privacy_tier: PrivacyTier::Operational,
        recommended_next_probe: "cass status --json",
    },
    IncidentCategoryDef {
        category: IncidentCategory::IndexStallProgress,
        id: "index_stall_progress",
        description: "an index/rebuild emits heartbeats but makes no forward progress",
        detection_signals: &[
            "stalled",
            "no forward progress",
            "last_progress_at_ms",
            "rebuild",
        ],
        example_terms: &["stalled", "stuck index", "no progress", "rebuild hang"],
        confidence: DetectionConfidence::Medium,
        root_cause_family: RootCauseFamily::CassDerivedState,
        privacy_tier: PrivacyTier::Operational,
        recommended_next_probe: "cass status --json",
    },
    IncidentCategoryDef {
        category: IncidentCategory::SearchZeroWorkspace,
        id: "search_zero_workspace",
        description: "a workspace-filtered search returns zero hits due to a path/workspace mismatch",
        detection_signals: &[
            "zero_result_diagnosis",
            "candidate_workspaces",
            "workspace mismatch",
        ],
        example_terms: &[
            "no results",
            "empty workspace",
            "wrong workspace",
            "moved checkout",
        ],
        confidence: DetectionConfidence::Medium,
        root_cause_family: RootCauseFamily::WorkspaceProvenance,
        privacy_tier: PrivacyTier::Redacted,
        recommended_next_probe: "cass sources list --json",
    },
    IncidentCategoryDef {
        category: IncidentCategory::QuarantineOom,
        id: "quarantine_oom",
        description: "ingest hit an irreducible streaming OOM and quarantined a conversation",
        detection_signals: &[
            "quarantined_conversations",
            "index-ingest-out-of-memory",
            "ingest_oom",
        ],
        example_terms: &["quarantine", "out of memory", "oom", "poison session"],
        confidence: DetectionConfidence::High,
        root_cause_family: RootCauseFamily::CassDerivedState,
        privacy_tier: PrivacyTier::Redacted,
        recommended_next_probe: "cass diag --json --quarantine",
    },
    IncidentCategoryDef {
        category: IncidentCategory::StorageBusyCorrupt,
        id: "storage_busy_corrupt",
        description: "the storage engine reports busy locks, integrity failures, or WAL sidecar issues",
        detection_signals: &["database is locked", "integrity", "OpenRead", "WAL", "busy"],
        example_terms: &["db locked", "corrupt", "integrity check", "busy timeout"],
        confidence: DetectionConfidence::High,
        root_cause_family: RootCauseFamily::FrankensqliteStorage,
        privacy_tier: PrivacyTier::Operational,
        recommended_next_probe: "cass doctor --json",
    },
    IncidentCategoryDef {
        category: IncidentCategory::RemoteSyncAuth,
        id: "remote_sync_auth",
        description: "remote source sync failed on transport or authentication",
        detection_signals: &[
            "ssh",
            "rsync",
            "permission denied",
            "host key",
            "auth",
            "timeout",
        ],
        example_terms: &[
            "sync failed",
            "ssh error",
            "auth",
            "permission denied",
            "host unreachable",
        ],
        confidence: DetectionConfidence::High,
        root_cause_family: RootCauseFamily::RemoteTransportAuth,
        privacy_tier: PrivacyTier::Redacted,
        recommended_next_probe: "cass sources list --json",
    },
    IncidentCategoryDef {
        category: IncidentCategory::Semantic,
        id: "semantic",
        description: "semantic search is unavailable, backfilling, or has stale/missing model or vector assets",
        detection_signals: &[
            "semantic_fallback_lexical",
            "fallback_mode",
            "model",
            "vector",
            "embedder",
        ],
        example_terms: &[
            "semantic unavailable",
            "model missing",
            "backfill",
            "hybrid fallback",
        ],
        confidence: DetectionConfidence::High,
        root_cause_family: RootCauseFamily::SemanticAssets,
        privacy_tier: PrivacyTier::Operational,
        recommended_next_probe: "cass status --json",
    },
    IncidentCategoryDef {
        category: IncidentCategory::WatchSalvageIssues,
        id: "watch_salvage_issues",
        description: "watch-mode exits, OOM-kill restart loops, or historical salvage re-scans",
        detection_signals: &[
            "--watch",
            "exit code 9",
            "drop_close",
            "salvage",
            "deferred_authoritative_db_rebuild",
        ],
        example_terms: &["watch crash", "exit 9", "salvage loop", "watch restart"],
        confidence: DetectionConfidence::Medium,
        root_cause_family: RootCauseFamily::CassDerivedState,
        privacy_tier: PrivacyTier::Operational,
        recommended_next_probe: "cass status --json",
    },
    IncidentCategoryDef {
        category: IncidentCategory::HostPressure,
        id: "host_pressure",
        description: "host memory/load/disk pressure (OOM kills, high load, low free space) drives the incident",
        detection_signals: &["oomd", "load average", "no space left", "swap", "ballast"],
        example_terms: &["out of disk", "oom killed", "high load", "swap thrash"],
        confidence: DetectionConfidence::Medium,
        root_cause_family: RootCauseFamily::HostOomLoad,
        privacy_tier: PrivacyTier::Operational,
        recommended_next_probe: "cass doctor --json",
    },
    IncidentCategoryDef {
        category: IncidentCategory::DependencyAttribution,
        id: "dependency_attribution",
        description: "the incident is plausibly attributable to a pinned sibling dependency vs an upstream fix",
        detection_signals: &[
            "pin_state",
            "upstream_fix_possibly_missing",
            "known_issue_ids",
            "frankensqlite",
            "frankensearch",
        ],
        example_terms: &[
            "dependency",
            "pinned rev",
            "upstream fix",
            "regression after bump",
        ],
        // Attribution is the task; no single family a-priori, so Unknown is the
        // honest seed until discovery narrows it.
        confidence: DetectionConfidence::Low,
        root_cause_family: RootCauseFamily::Unknown,
        privacy_tier: PrivacyTier::Operational,
        recommended_next_probe: "cass diag --json",
    },
];

/// The frozen incident category schema, in stable order (excludes `Other`).
pub(crate) fn categories() -> &'static [IncidentCategoryDef] {
    CATEGORIES
}

/// Look up a category definition by stable id.
pub(crate) fn category_def(id: &str) -> Option<&'static IncidentCategoryDef> {
    CATEGORIES.iter().find(|d| d.id == id)
}

fn has_cass_anchor(text: &str) -> bool {
    [
        "cass ",
        "cass_",
        "coding-agent-search",
        "coding_agent_search",
        "agent_search.db",
        "frankensqlite",
        "frankensearch",
        "fts_messages",
    ]
    .iter()
    .any(|anchor| text.contains(anchor))
}

fn contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

fn structured_field_values<'a>(text: &'a str, field: &str) -> Vec<&'a str> {
    text.match_indices(field)
        .filter_map(|(offset, _)| {
            let mut rest = &text[offset + field.len()..];
            rest = rest.trim_start_matches(|character: char| character.is_ascii_whitespace());
            if let Some(stripped) = rest.strip_prefix(['"', '\'']) {
                rest =
                    stripped.trim_start_matches(|character: char| character.is_ascii_whitespace());
            }
            let separator = rest.chars().next()?;
            if !matches!(separator, ':' | '=') {
                return None;
            }
            rest = rest[separator.len_utf8()..]
                .trim_start_matches(|character: char| character.is_ascii_whitespace());
            if let Some(stripped) = rest.strip_prefix(['"', '\'']) {
                rest = stripped;
            }
            let value_end = rest
                .find(|character: char| {
                    character.is_ascii_whitespace()
                        || matches!(character, '"' | '\'' | ',' | '}' | ']' | ';')
                })
                .unwrap_or(rest.len());
            Some(&rest[..value_end])
        })
        .collect()
}

fn structured_field_has_value(text: &str, field: &str, expected: &[&str]) -> bool {
    structured_field_values(text, field)
        .into_iter()
        .any(|value| expected.contains(&value))
}

fn structured_field_is_positive(text: &str, field: &str) -> bool {
    structured_field_values(text, field)
        .into_iter()
        .filter_map(|value| value.parse::<u64>().ok())
        .any(|count| count > 0)
}

fn structured_flag_is_true_or_unstructured(text: &str, field: &str) -> bool {
    text.match_indices(field).any(|(offset, _)| {
        let Some(mut rest) = text.get(offset.saturating_add(field.len())..) else {
            return false;
        };
        rest = rest.trim_start_matches(|character: char| character.is_ascii_whitespace());
        if let Some(stripped) = rest.strip_prefix(['"', '\'']) {
            rest = stripped.trim_start_matches(|character: char| character.is_ascii_whitespace());
        }
        let Some(separator) = rest.chars().next() else {
            return true;
        };
        if !matches!(separator, ':' | '=') {
            return true;
        }
        let Some(stripped) = rest.strip_prefix(separator) else {
            return false;
        };
        rest = stripped.trim_start_matches(|character: char| character.is_ascii_whitespace());
        if let Some(stripped) = rest.strip_prefix(['"', '\'']) {
            rest = stripped;
        }
        let value_end = rest
            .find(|character: char| {
                character.is_ascii_whitespace()
                    || matches!(character, '"' | '\'' | ',' | '}' | ']' | ';')
            })
            .unwrap_or(rest.len());
        rest.get(..value_end)
            .is_some_and(|value| matches!(value, "true" | "1"))
    })
}

fn strong_signal_matches(category: IncidentCategory, text: &str) -> bool {
    match category {
        IncidentCategory::CassStatusHealth => structured_field_has_value(
            text,
            "health_class",
            &["degraded", "unhealthy", "not_ready", "critical", "error"],
        ),
        IncidentCategory::IndexStaleMissing => {
            structured_field_has_value(
                text,
                "index_freshness",
                &["stale", "missing", "not_initialized", "outdated", "corrupt"],
            ) || text.contains("err.kind=openread")
                || text.contains("err.kind\":\"openread")
        }
        IncidentCategory::IndexStallProgress => {
            text.contains("no forward progress")
                || (text.contains("last_progress_at_ms")
                    && contains_any(text, &["stalled", "stuck", "timed out", "timeout"]))
        }
        IncidentCategory::SearchZeroWorkspace => structured_field_has_value(
            text,
            "zero_result_diagnosis",
            &[
                "workspace_mismatch",
                "no_candidates",
                "zero_results",
                "missing_workspace",
            ],
        ),
        IncidentCategory::QuarantineOom => {
            structured_field_is_positive(text, "quarantined_conversations")
                || contains_any(text, &["index-ingest-out-of-memory", "ingest_oom"])
        }
        IncidentCategory::Semantic => {
            structured_flag_is_true_or_unstructured(text, "semantic_fallback_lexical")
                || structured_field_has_value(text, "fallback_mode", &["lexical"])
        }
        IncidentCategory::WatchSalvageIssues => {
            contains_any(text, &["drop_close", "deferred_authoritative_db_rebuild"])
        }
        IncidentCategory::DependencyAttribution => {
            contains_any(text, &["upstream_fix_possibly_missing", "known_issue_ids"])
        }
        IncidentCategory::StorageBusyCorrupt
        | IncidentCategory::RemoteSyncAuth
        | IncidentCategory::HostPressure
        | IncidentCategory::Other => false,
    }
}

fn matches_anchored_category(category: IncidentCategory, text: &str) -> bool {
    match category {
        IncidentCategory::CassStatusHealth => {
            contains_any(text, &["cass health", "cass status"])
                && contains_any(text, &["degraded", "unhealthy", "not ready", "critical"])
        }
        IncidentCategory::IndexStaleMissing => {
            contains_any(text, &["index", "fts_messages"])
                && contains_any(
                    text,
                    &["stale", "missing", "not initialized", "outdated", "corrupt"],
                )
        }
        IncidentCategory::IndexStallProgress => {
            contains_any(text, &["index", "rebuild"])
                && contains_any(text, &["stalled", "stuck", "no progress", "timed out"])
        }
        IncidentCategory::SearchZeroWorkspace => {
            contains_any(text, &["search", "workspace"])
                && contains_any(text, &["zero results", "no results", "mismatch", "missing"])
        }
        IncidentCategory::QuarantineOom => {
            contains_any(text, &["quarantine", "ingest"])
                && contains_any(text, &["oom", "out of memory", "failed"])
        }
        IncidentCategory::StorageBusyCorrupt => {
            contains_any(
                text,
                &[
                    "database",
                    "sqlite",
                    "frankensqlite",
                    "wal",
                    "openread",
                    "integrity",
                ],
            ) && contains_any(
                text,
                &["locked", "busy", "corrupt", "integrity", "wal", "openread"],
            )
        }
        IncidentCategory::RemoteSyncAuth => {
            contains_any(
                text,
                &["ssh", "rsync", "remote sync", "source sync", "host key"],
            ) && contains_any(
                text,
                &[
                    "permission denied",
                    "auth",
                    "timeout",
                    "failed",
                    "unreachable",
                    "host key",
                ],
            )
        }
        IncidentCategory::Semantic => contains_any(
            text,
            &[
                "semantic missing",
                "semantic unavailable",
                "semantic stale",
                "semantic backfill",
                "semantic fallback",
                "semantic failed",
                "semantic error",
                "model missing",
                "model unavailable",
                "model stale",
                "model failed",
                "model error",
                "vector missing",
                "vector unavailable",
                "vector stale",
                "vector backfill",
                "vector failed",
                "vector error",
                "embedder missing",
                "embedder unavailable",
                "embedder failed",
                "embedder error",
                "hybrid fallback",
                "hybrid failed",
                "hybrid error",
            ],
        ),
        IncidentCategory::WatchSalvageIssues => contains_any(
            text,
            &[
                "exit code 9",
                "drop_close",
                "salvage",
                "deferred_authoritative_db_rebuild",
                "watch crash",
                "watch restart",
            ],
        ),
        IncidentCategory::DependencyAttribution => {
            contains_any(text, &["frankensqlite", "frankensearch"])
                && contains_any(
                    text,
                    &[
                        "pinned",
                        "pin state",
                        "upstream",
                        "known issue",
                        "regression",
                    ],
                )
        }
        IncidentCategory::HostPressure => CATEGORIES
            .iter()
            .find(|definition| definition.category == category)
            .is_some_and(|definition| {
                definition
                    .detection_signals
                    .iter()
                    .any(|signal| text.contains(&signal.to_ascii_lowercase()))
            }),
        IncidentCategory::Other => false,
    }
}

/// Classify one bounded message fragment into zero or more canonical incident
/// categories. Stable category order is preserved. Generic coding words such as
/// "model", "auth", "busy", and "timeout" require a CASS-specific anchor in
/// the same message; strong structured markers classify on their own.
pub(crate) fn classify_text(text: &str) -> Vec<IncidentCategory> {
    let normalized = text.to_ascii_lowercase();
    let anchored = has_cass_anchor(&normalized);
    CATEGORIES
        .iter()
        .filter(|definition| {
            strong_signal_matches(definition.category, &normalized)
                || (anchored && matches_anchored_category(definition.category, &normalized))
        })
        .map(|definition| definition.category)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn require_categories(
        actual: Vec<IncidentCategory>,
        expected: &[IncidentCategory],
        context: &str,
    ) -> Result<(), String> {
        if actual.as_slice().cmp(expected).is_eq() {
            Ok(())
        } else {
            Err(format!(
                "{context}: expected {expected:?}, found {actual:?}"
            ))
        }
    }

    /// The eleven required category ids from the report, in order.
    const REQUIRED: &[&str] = &[
        "cass_status_health",
        "index_stale_missing",
        "index_stall_progress",
        "search_zero_workspace",
        "quarantine_oom",
        "storage_busy_corrupt",
        "remote_sync_auth",
        "semantic",
        "watch_salvage_issues",
        "host_pressure",
        "dependency_attribution",
    ];

    #[test]
    fn schema_lists_the_required_categories_in_stable_order() {
        let ids: Vec<&str> = categories().iter().map(|d| d.id).collect();
        assert_eq!(ids, REQUIRED, "category set and order must be stable");
    }

    #[test]
    fn every_def_id_matches_its_serialized_category() {
        for d in categories() {
            // The struct `id` and the enum's snake_case serialization agree.
            assert_eq!(d.id, d.category.id(), "id mismatch for {:?}", d.category);
            let json = serde_json::to_string(&d.category).unwrap();
            assert_eq!(json, format!("\"{}\"", d.id));
        }
    }

    #[test]
    fn every_def_has_signals_terms_and_a_probe() {
        for d in categories() {
            assert!(!d.detection_signals.is_empty(), "{} needs signals", d.id);
            assert!(!d.example_terms.is_empty(), "{} needs terms", d.id);
            assert!(!d.description.is_empty(), "{} needs a description", d.id);
            assert!(
                !d.recommended_next_probe.is_empty(),
                "{} needs a next probe",
                d.id
            );
            // Probes are concrete cass commands, never a bare `cass`.
            assert!(
                d.recommended_next_probe.starts_with("cass "),
                "{} probe should be a concrete cass command: {}",
                d.id,
                d.recommended_next_probe
            );
        }
    }

    #[test]
    fn from_id_resolves_known_categories_and_other() {
        for id in REQUIRED {
            assert_eq!(IncidentCategory::from_id(id).map(|c| c.id()), Some(*id));
        }
        assert_eq!(
            IncidentCategory::from_id("other"),
            Some(IncidentCategory::Other)
        );
    }

    #[test]
    fn unknown_category_id_is_unrecognised_not_silently_mapped() {
        // Forward-compat: a future/unknown id must NOT resolve to an existing
        // category; consumers can then treat it as Other explicitly.
        assert_eq!(IncidentCategory::from_id("brand_new_category_v2"), None);
        assert!(category_def("brand_new_category_v2").is_none());
    }

    #[test]
    fn root_cause_families_are_assigned_consistently() {
        // Spot-check the contract-critical associations.
        assert_eq!(
            category_def("storage_busy_corrupt")
                .unwrap()
                .root_cause_family,
            RootCauseFamily::FrankensqliteStorage
        );
        assert_eq!(
            category_def("remote_sync_auth").unwrap().root_cause_family,
            RootCauseFamily::RemoteTransportAuth
        );
        assert_eq!(
            category_def("semantic").unwrap().root_cause_family,
            RootCauseFamily::SemanticAssets
        );
        assert_eq!(
            category_def("search_zero_workspace")
                .unwrap()
                .root_cause_family,
            RootCauseFamily::WorkspaceProvenance
        );
        // Attribution category seeds Unknown by design.
        assert_eq!(
            category_def("dependency_attribution")
                .unwrap()
                .root_cause_family,
            RootCauseFamily::Unknown
        );
    }

    #[test]
    fn privacy_tiers_redact_path_and_host_bearing_categories() {
        assert_eq!(
            category_def("search_zero_workspace").unwrap().privacy_tier,
            PrivacyTier::Redacted
        );
        assert_eq!(
            category_def("remote_sync_auth").unwrap().privacy_tier,
            PrivacyTier::Redacted
        );
        assert_eq!(
            category_def("quarantine_oom").unwrap().privacy_tier,
            PrivacyTier::Redacted
        );
    }

    #[test]
    fn category_def_serializes_with_stable_field_wire_forms() {
        // The schema is serialize-only (static borrowed fields); assert the
        // projected JSON shape rather than a deserialize round-trip.
        let d = category_def("semantic").unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(d).unwrap()).unwrap();
        assert_eq!(v["category"], "semantic");
        assert_eq!(v["root_cause_family"], "semantic-assets");
        assert_eq!(v["privacy_tier"], "operational");
        assert_eq!(v["confidence"], "high");
        assert_eq!(v["id"], "semantic");
        assert!(v["detection_signals"].is_array());
        assert!(
            v["recommended_next_probe"]
                .as_str()
                .unwrap()
                .starts_with("cass ")
        );
    }

    #[test]
    fn classifier_matches_multiple_categories_once_in_stable_order() {
        let found = classify_text(
            "cass health_class=degraded recommended_action=repair; \
             semantic_fallback_lexical model missing model missing; database is locked busy",
        );
        assert_eq!(
            found,
            vec![
                IncidentCategory::CassStatusHealth,
                IncidentCategory::StorageBusyCorrupt,
                IncidentCategory::Semantic,
            ]
        );
    }

    #[test]
    fn classifier_rejects_unanchored_generic_application_discussion() {
        assert!(
            classify_text("our app model auth timeout busy status rebuild is flaky").is_empty(),
            "generic coding conversation must not become a CASS incident"
        );
    }

    #[test]
    fn classifier_requires_category_specific_failure_context() -> Result<(), String> {
        assert!(
            classify_text("cass uses frankensqlite and frankensearch").is_empty(),
            "dependency names alone are not an attribution incident"
        );
        assert!(
            classify_text("cass completed the model and vector migration").is_empty(),
            "healthy semantic discussion is not an incident"
        );
        assert!(
            classify_text("cass command timed out while rendering a report").is_empty(),
            "a generic timeout is not remote authentication failure"
        );
        assert!(
            classify_text(
                r#"cass status {"health_class":"healthy","recommended_action":"none","index_freshness":"fresh","last_progress_at_ms":1770000000000,"candidate_workspaces":["/tmp/demo"],"quarantined_conversations":0,"semantic_fallback_lexical":false,"fallback_mode":"hybrid"}"#,
            )
            .is_empty(),
            "healthy status field names and benign values must not become incidents"
        );
        require_categories(
            classify_text(r#"cass semantic model missing; {"fallback_mode":"hybrid"}"#),
            &[IncidentCategory::Semantic],
            "healthy structured fields must not hide an adjacent prose incident",
        )?;
        require_categories(
            classify_text("cass semantic_fallback_lexical; search stayed available"),
            &[IncidentCategory::Semantic],
            "the report-derived bare fallback marker remains a strong signal",
        )?;
        Ok(())
    }

    #[test]
    fn classifier_accepts_anchored_compound_failure_signals() {
        assert_eq!(
            classify_text("cass remote sync over ssh failed: permission denied"),
            vec![IncidentCategory::RemoteSyncAuth]
        );
        assert_eq!(
            classify_text("cass frankensqlite pinned revision may miss upstream fix"),
            vec![IncidentCategory::DependencyAttribution]
        );
    }

    #[test]
    fn classifier_accepts_report_derived_strong_markers_without_anchor() {
        assert_eq!(
            classify_text("index-ingest-out-of-memory quarantined_conversations=1"),
            vec![IncidentCategory::QuarantineOom]
        );
        assert_eq!(
            classify_text("deferred_authoritative_db_rebuild drop_close"),
            vec![IncidentCategory::WatchSalvageIssues]
        );
    }

    #[test]
    fn classifier_empty_and_unknown_inputs_have_no_false_category() {
        assert!(classify_text("").is_empty());
        assert!(classify_text("ordinary refactor completed successfully").is_empty());
    }
}
