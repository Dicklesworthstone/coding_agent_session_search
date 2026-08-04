// Dead-code tolerated module-wide: the pure assembler + redaction posture
// (bead cass-fleet-resilience-20260608-uojcg.13.3) is now wired live by
// `cass support-bundle` (bead 6f1lm; see src/lib.rs::run_support_bundle, which
// gathers the readiness summary, command envelope, source provenance,
// quarantine, and root-cause attribution and emits the shareable bundle). Two
// pieces remain forward API the local single-host surface does not yet
// populate: the proof-log links (`ProofLogKind`/`ProofLogLink` — the bundle
// passes an empty Vec) and multi-host `FleetSummary` (passed `None`), so the
// module-wide allow stays until those are wired.
#![allow(dead_code)]

//! Redacted recovery / support evidence bundle (bead
//! cass-fleet-resilience-20260608-uojcg.13.3).
//!
//! When cass is degraded an operator (or agent) needs to share or inspect
//! enough evidence to recover safely — but raw session text and private tool
//! payloads must never leak by default. Today that evidence is scattered across
//! ad hoc artifacts (`health`/`status`/`doctor` JSON, proof logs, fleet
//! probes). This module is the single convergence point: it COMPOSES the
//! existing canonical contracts into one supportable, redacted bundle rather
//! than inventing a parallel story.
//!
//! It embeds, verbatim, the contracts other surfaces already emit — so the
//! bundle can never contradict robot JSON:
//! - [`ReadinessSummary`] (the canonical readiness projection),
//! - [`NextCommandEnvelope`] (the safe/unsafe next-command guidance),
//! - [`SourceDoctorReport`] (source/archive provenance + reachability),
//! - [`QuarantineSummary`] (advisory exclusions),
//! - [`FleetSummary`] (fleet reachability rollup, when applicable),
//! - [`RootCauseAttribution`] (root-cause family + evidence + confidence).
//!
//! On top it adds a [`SupportBundleManifest`] (cass version, host/source id,
//! data/config/model dirs, command provenance, fixture-vs-live marker, and a
//! generated-at timestamp), proof-log links, and a [`BundleRedactionReport`]
//! that states exactly which fields were included, suppressed, hashed, or
//! truncated.
//!
//! Redaction posture (the privacy invariant the tests pin): the default policy
//! ([`default_redaction_policy`]) suppresses all raw private payloads, hashes
//! fingerprints rather than surfacing content, and truncates filesystem paths
//! to basenames. Richer evidence (full paths, raw snippets) is explicit opt-in,
//! recorded in `opt_in_flags`. A bundle built under the default policy is
//! `is_share_safe()`. `ExecutionMode` distinguishes deterministic fixture mode
//! from opt-in live mode so logs and consumers always know the provenance.

use serde::{Deserialize, Serialize};

use crate::fleet_doctor_schema::FleetSummary;
use crate::root_cause_taxonomy::RootCauseAttribution;
use crate::search::command_envelope::NextCommandEnvelope;
use crate::search::incident_redaction::{HashStrategy, PrivateTextPolicy, RedactionPolicy};
use crate::search::readiness::QuarantineSummary;
use crate::search::readiness_projection::ReadinessSummary;
use crate::source_doctor_health::SourceDoctorReport;

/// Manifest schema version for the recovery/support bundle.
pub(crate) const SUPPORT_BUNDLE_MANIFEST_VERSION: u32 = 1;

/// Whether the bundle was assembled from a deterministic fixture (CI/golden,
/// reproducible) or from an opt-in live diagnostic run against real state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ExecutionMode {
    /// Built from live, real on-host state (opt-in).
    Live,
    /// Built from a deterministic fixture (carries a `fixture_id`).
    Fixture,
}

/// The kind of out-of-band log a [`ProofLogLink`] points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProofLogKind {
    Stdout,
    Stderr,
    /// A structured proof artifact (see `proof_artifact`).
    ProofArtifact,
    /// A diagnostic / NDJSON event log.
    DiagnosticLog,
}

/// A link to an out-of-band proof/diagnostic log. The bundle references logs by
/// path rather than inlining them, keeping it bounded and shareable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProofLogLink {
    pub label: String,
    pub kind: ProofLogKind,
    pub path: String,
}

/// The bundle manifest: who/what/when produced it, and from where.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SupportBundleManifest {
    pub manifest_version: u32,
    pub bundle_id: String,
    pub cass_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
    /// Canonical data dir (basename-only unless full paths are opted in).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_dir: Option<String>,
    /// The command that generated the bundle (provenance), e.g. `cass doctor`.
    pub command_provenance: String,
    pub mode: ExecutionMode,
    /// Present (and required) when `mode == Fixture`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixture_id: Option<String>,
    pub generated_at_ms: i64,
}

impl SupportBundleManifest {
    /// Whether every required manifest field is present, and a fixture-mode
    /// bundle carries its `fixture_id`.
    pub(crate) fn is_complete(&self) -> bool {
        self.manifest_version >= 1
            && !self.bundle_id.is_empty()
            && !self.cass_version.is_empty()
            && !self.command_provenance.is_empty()
            && self.generated_at_ms > 0
            && match self.mode {
                ExecutionMode::Fixture => self.fixture_id.is_some(),
                ExecutionMode::Live => true,
            }
    }
}

/// The auditable record of what the bundle redacted — which fields were
/// included, suppressed (raw private content), hashed (fingerprint instead of
/// content), or truncated (paths to basenames), plus the opt-in flags that
/// would unlock richer evidence. Reuses the [`PrivateTextPolicy`] /
/// [`HashStrategy`] vocabulary from `incident_redaction`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BundleRedactionReport {
    pub private_text_policy: PrivateTextPolicy,
    pub hash_strategy: HashStrategy,
    /// Whether full filesystem paths were emitted (vs basenames).
    pub full_paths: bool,
    pub fields_included: Vec<String>,
    pub fields_suppressed: Vec<String>,
    pub fields_hashed: Vec<String>,
    pub fields_truncated: Vec<String>,
    pub opt_in_flags: Vec<String>,
}

/// The composed, redacted recovery/support evidence bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RecoverySupportBundle {
    pub manifest: SupportBundleManifest,
    pub readiness: ReadinessSummary,
    pub command_envelope: NextCommandEnvelope,
    pub source_provenance: SourceDoctorReport,
    pub quarantine: QuarantineSummary,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fleet: Option<FleetSummary>,
    pub root_cause: RootCauseAttribution,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub proof_logs: Vec<ProofLogLink>,
    pub redaction: BundleRedactionReport,
}

impl RecoverySupportBundle {
    /// Whether this bundle is safe to share as-is: no raw private payload was
    /// embedded, paths are basename-only, and no full-path/raw opt-in was used.
    pub(crate) fn is_share_safe(&self) -> bool {
        !self.redaction.full_paths
            && !matches!(
                self.redaction.private_text_policy,
                PrivateTextPolicy::RawOptIn
            )
            && !self.embeds_full_paths()
    }

    /// Whether any manifest directory still carries a full (slash-bearing) path.
    fn embeds_full_paths(&self) -> bool {
        [
            &self.manifest.data_dir,
            &self.manifest.config_dir,
            &self.manifest.model_dir,
        ]
        .into_iter()
        .flatten()
        .any(|p| p.contains('/'))
    }
}

/// The raw pieces a caller supplies to assemble a bundle. The composed reports
/// are the canonical contracts other surfaces already built (passed in so this
/// stays pure and fixture-testable); the scalars populate the manifest.
#[derive(Debug, Clone)]
pub(crate) struct SupportBundleInputs {
    pub bundle_id: String,
    pub cass_version: String,
    pub host_id: Option<String>,
    pub source_id: Option<String>,
    pub data_dir: Option<String>,
    pub config_dir: Option<String>,
    pub model_dir: Option<String>,
    pub command_provenance: String,
    pub generated_at_ms: i64,
    /// Required when `mode == Fixture`.
    pub fixture_id: Option<String>,
    pub readiness: ReadinessSummary,
    pub command_envelope: NextCommandEnvelope,
    pub source_provenance: SourceDoctorReport,
    pub quarantine: QuarantineSummary,
    pub fleet: Option<FleetSummary>,
    pub root_cause: RootCauseAttribution,
    pub proof_logs: Vec<ProofLogLink>,
}

/// The default support-bundle redaction posture: suppress all raw private
/// text, hash fingerprints (never content), basename-only paths. A bundle
/// built under this policy is `is_share_safe()`.
pub(crate) fn default_redaction_policy() -> RedactionPolicy {
    RedactionPolicy {
        private_text: PrivateTextPolicy::SuppressAll,
        hash: HashStrategy::Blake3_256V1,
        allow_full_paths: false,
    }
}

/// Reduce a filesystem path to its basename (last component), trimming a
/// trailing slash first. Used to keep directory paths from leaking the host's
/// home/user layout in a shared bundle.
fn basename(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    trimmed.rsplit('/').next().unwrap_or(trimmed).to_string()
}

/// Assemble a redacted recovery/support bundle. Pure: no I/O, no clock, no
/// network. The redaction report is derived from `policy`; under the default
/// policy raw payloads are suppressed and paths are truncated to basenames.
pub(crate) fn build_support_bundle(
    inputs: SupportBundleInputs,
    policy: RedactionPolicy,
    mode: ExecutionMode,
) -> RecoverySupportBundle {
    let full_paths = policy.allow_full_paths;

    // Directory paths are truncated to basenames unless full paths are
    // explicitly opted in.
    let redact_dir = |p: &Option<String>| -> Option<String> {
        match p {
            Some(path) if !full_paths => Some(basename(path)),
            other => other.clone(),
        }
    };
    let data_dir = redact_dir(&inputs.data_dir);
    let config_dir = redact_dir(&inputs.config_dir);
    let model_dir = redact_dir(&inputs.model_dir);

    // ---- redaction-report accounting ----
    let mut fields_included: Vec<String> = vec![
        "manifest".into(),
        "readiness".into(),
        "command_envelope".into(),
        "source_provenance".into(),
        "quarantine".into(),
        "root_cause".into(),
    ];
    if inputs.fleet.is_some() {
        fields_included.push("fleet".into());
    }
    if !inputs.proof_logs.is_empty() {
        fields_included.push("proof_log_links".into());
    }

    let mut fields_truncated: Vec<String> = Vec::new();
    let mut fields_hashed: Vec<String> = Vec::new();
    let mut fields_suppressed: Vec<String> = Vec::new();
    let mut opt_in_flags: Vec<String> = Vec::new();

    if full_paths {
        fields_included.push("manifest.full_paths".into());
    } else {
        for (name, present) in [
            ("manifest.data_dir", inputs.data_dir.is_some()),
            ("manifest.config_dir", inputs.config_dir.is_some()),
            ("manifest.model_dir", inputs.model_dir.is_some()),
        ] {
            if present {
                fields_truncated.push(name.into());
            }
        }
        opt_in_flags.push("--include-full-paths".into());
    }

    // Raw private session/tool payloads are never embedded by default; an
    // operator must opt in to attach them.
    match policy.private_text {
        PrivateTextPolicy::SuppressAll => {
            fields_suppressed.push("raw_session_payload".into());
            fields_suppressed.push("raw_tool_payload".into());
            opt_in_flags.push("--include-raw-evidence".into());
        }
        PrivateTextPolicy::RedactedSnippets => {
            fields_included.push("redacted_snippets".into());
            fields_suppressed.push("raw_session_payload".into());
            opt_in_flags.push("--include-raw-evidence".into());
        }
        PrivateTextPolicy::RawOptIn => {
            fields_included.push("raw_session_payload".into());
            fields_included.push("raw_tool_payload".into());
        }
    }

    if matches!(policy.hash, HashStrategy::Blake3_256V1) {
        fields_hashed.push("content_fingerprints".into());
    }

    fields_included.sort();
    fields_truncated.sort();
    fields_hashed.sort();
    fields_suppressed.sort();
    opt_in_flags.sort();

    let redaction = BundleRedactionReport {
        private_text_policy: policy.private_text,
        hash_strategy: policy.hash,
        full_paths,
        fields_included,
        fields_suppressed,
        fields_hashed,
        fields_truncated,
        opt_in_flags,
    };

    let manifest = SupportBundleManifest {
        manifest_version: SUPPORT_BUNDLE_MANIFEST_VERSION,
        bundle_id: inputs.bundle_id,
        cass_version: inputs.cass_version,
        host_id: inputs.host_id,
        source_id: inputs.source_id,
        data_dir,
        config_dir,
        model_dir,
        command_provenance: inputs.command_provenance,
        mode,
        fixture_id: inputs.fixture_id,
        generated_at_ms: inputs.generated_at_ms,
    };

    RecoverySupportBundle {
        manifest,
        readiness: inputs.readiness,
        command_envelope: inputs.command_envelope,
        source_provenance: inputs.source_provenance,
        quarantine: inputs.quarantine,
        fleet: inputs.fleet,
        root_cause: inputs.root_cause,
        proof_logs: inputs.proof_logs,
        redaction,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet_doctor_schema::ArchiveRisk;
    use crate::root_cause_taxonomy::{AttributionConfidence, RootCauseFamily};
    use crate::search::readiness::{DerivedAssetTruthTable, fleet_fixtures};
    use crate::search::readiness_projection::{SurfaceKind, project};
    use crate::source_doctor_health::{
        SOURCE_DOCTOR_SCHEMA_VERSION, SourceDoctorEntry, SourceDoctorReport, SourceDoctorState,
        SourceDoctorSummary,
    };
    use std::collections::BTreeMap;

    const NOW_MS: i64 = 1_749_350_000_000;

    fn table(name: &str) -> DerivedAssetTruthTable {
        fleet_fixtures()
            .into_iter()
            .find(|(n, _)| *n == name)
            .expect("missing fleet fixture (name not present in fleet_fixtures)")
            .1
    }

    fn sample_source_report() -> SourceDoctorReport {
        SourceDoctorReport {
            schema_version: SOURCE_DOCTOR_SCHEMA_VERSION,
            sources: vec![SourceDoctorEntry {
                source_id: "laptop".into(),
                host: Some("user@laptop".into()),
                state: SourceDoctorState::Reachable,
                host_reached: true,
                connection_error: None,
                safe_next_command: None,
            }],
            summary: SourceDoctorSummary {
                total: 1,
                healthy: 1,
                unhealthy: 0,
                unreached: 0,
            },
            mutation_free: true,
        }
    }

    fn sample_fleet_summary() -> FleetSummary {
        FleetSummary {
            total_hosts: 3,
            ok: 2,
            degraded: 0,
            timed_out: 0,
            unreachable: 1,
            cancelled: 0,
            stale_data: 0,
            highest_archive_risk: ArchiveRisk::Low,
            root_cause_distribution: BTreeMap::new(),
            dominant_root_cause: None,
        }
    }

    /// Build representative inputs from the canonical `css_stale_existing_index`
    /// fleet fixture so the embedded readiness/envelope are real projections.
    fn sample_inputs(fleet: bool, fixture_id: Option<&str>) -> SupportBundleInputs {
        let t = table("css_stale_existing_index");
        SupportBundleInputs {
            bundle_id: "rsb-0001".into(),
            cass_version: "0.6.15".into(),
            host_id: Some("csd".into()),
            source_id: Some("laptop".into()),
            data_dir: Some("/home/user/.local/share/coding-agent-search".into()),
            config_dir: Some("/home/user/.config/cass".into()),
            model_dir: Some("/home/user/.local/share/coding-agent-search/models".into()),
            command_provenance: "cass doctor --json".into(),
            generated_at_ms: NOW_MS,
            fixture_id: fixture_id.map(str::to_string),
            readiness: project(&t, SurfaceKind::Status),
            command_envelope: t.next_command_envelope(Some("/home/user/.local/share/cass")),
            source_provenance: sample_source_report(),
            quarantine: t.quarantine.clone(),
            fleet: fleet.then(sample_fleet_summary),
            root_cause: RootCauseAttribution::new(
                RootCauseFamily::CassDerivedState,
                AttributionConfidence::Probable,
                "lexical index stale; refresh recommended",
            ),
            proof_logs: vec![ProofLogLink {
                label: "doctor stdout".into(),
                kind: ProofLogKind::Stdout,
                path: "/tmp/cass-proof/doctor.stdout".into(),
            }],
        }
    }

    fn default_bundle() -> RecoverySupportBundle {
        build_support_bundle(
            sample_inputs(true, None),
            default_redaction_policy(),
            ExecutionMode::Live,
        )
    }

    // ---- manifest completeness ----

    #[test]
    fn manifest_is_complete_for_live_default_bundle() {
        let b = default_bundle();
        assert!(b.manifest.is_complete());
        assert_eq!(b.manifest.manifest_version, SUPPORT_BUNDLE_MANIFEST_VERSION);
        assert!(!b.manifest.cass_version.is_empty());
        assert!(!b.manifest.command_provenance.is_empty());
        assert!(b.manifest.generated_at_ms > 0);
    }

    #[test]
    fn fixture_mode_requires_a_fixture_id() {
        // Fixture mode without an id is incomplete...
        let missing = build_support_bundle(
            sample_inputs(false, None),
            default_redaction_policy(),
            ExecutionMode::Fixture,
        );
        assert!(!missing.manifest.is_complete(), "fixture needs an id");
        // ...with an id it is complete.
        let present = build_support_bundle(
            sample_inputs(false, Some("css_stale_existing_index")),
            default_redaction_policy(),
            ExecutionMode::Fixture,
        );
        assert!(present.manifest.is_complete());
        assert_eq!(
            present.manifest.fixture_id.as_deref(),
            Some("css_stale_existing_index")
        );
    }

    // ---- redaction defaults (privacy fixtures) ----

    #[test]
    fn default_policy_suppresses_raw_and_truncates_paths() {
        let b = default_bundle();
        // Paths are basename-only: no leaked home/user layout.
        assert_eq!(b.manifest.data_dir.as_deref(), Some("coding-agent-search"));
        assert_eq!(b.manifest.config_dir.as_deref(), Some("cass"));
        assert_eq!(b.manifest.model_dir.as_deref(), Some("models"));
        assert!(!b.embeds_full_paths());
        // Raw payloads are suppressed and the redaction report says so.
        assert!(
            b.redaction
                .fields_suppressed
                .contains(&"raw_session_payload".to_string())
        );
        assert!(
            b.redaction
                .fields_suppressed
                .contains(&"raw_tool_payload".to_string())
        );
        // Truncated dirs are recorded.
        for dir in [
            "manifest.data_dir",
            "manifest.config_dir",
            "manifest.model_dir",
        ] {
            assert!(
                b.redaction.fields_truncated.contains(&dir.to_string()),
                "{dir} should be recorded as truncated"
            );
        }
        // Fingerprints are hashed, never raw content.
        assert!(
            b.redaction
                .fields_hashed
                .contains(&"content_fingerprints".to_string())
        );
        // Opt-in flags tell the operator how to unlock richer evidence.
        assert!(
            b.redaction
                .opt_in_flags
                .contains(&"--include-full-paths".to_string())
        );
        assert!(
            b.redaction
                .opt_in_flags
                .contains(&"--include-raw-evidence".to_string())
        );
        // The whole thing is share-safe by default.
        assert!(b.is_share_safe());
    }

    #[test]
    fn opt_in_full_paths_keeps_paths_and_is_not_share_safe() {
        let policy = RedactionPolicy {
            allow_full_paths: true,
            ..default_redaction_policy()
        };
        let b = build_support_bundle(sample_inputs(true, None), policy, ExecutionMode::Live);
        assert_eq!(
            b.manifest.data_dir.as_deref(),
            Some("/home/user/.local/share/coding-agent-search")
        );
        assert!(b.embeds_full_paths());
        assert!(
            b.redaction
                .fields_included
                .contains(&"manifest.full_paths".to_string())
        );
        assert!(b.redaction.fields_truncated.is_empty());
        assert!(!b.is_share_safe(), "full paths are not share-safe");
    }

    #[test]
    fn raw_opt_in_text_marks_bundle_unsafe_to_share() {
        let policy = RedactionPolicy {
            private_text: PrivateTextPolicy::RawOptIn,
            ..default_redaction_policy()
        };
        let b = build_support_bundle(sample_inputs(false, None), policy, ExecutionMode::Live);
        assert!(
            b.redaction
                .fields_included
                .contains(&"raw_session_payload".to_string())
        );
        assert!(!b.is_share_safe(), "raw opt-in is not share-safe");
    }

    // ---- fleet present / absent ----

    #[test]
    fn fleet_is_optional_and_reflected_in_redaction_report() {
        let with_fleet = build_support_bundle(
            sample_inputs(true, None),
            default_redaction_policy(),
            ExecutionMode::Live,
        );
        assert!(with_fleet.fleet.is_some());
        assert!(
            with_fleet
                .redaction
                .fields_included
                .contains(&"fleet".to_string())
        );

        let without_fleet = build_support_bundle(
            sample_inputs(false, None),
            default_redaction_policy(),
            ExecutionMode::Live,
        );
        assert!(without_fleet.fleet.is_none());
        assert!(
            !without_fleet
                .redaction
                .fields_included
                .contains(&"fleet".to_string())
        );
    }

    // ---- consistency with robot JSON (the embedded contracts are verbatim) ----

    #[test]
    fn embedded_contracts_match_their_standalone_robot_projections() {
        let t = table("css_stale_existing_index");
        let b = default_bundle();
        // The bundle embeds the canonical readiness projection verbatim — it
        // cannot contradict `cass status --json`.
        assert_eq!(b.readiness, project(&t, SurfaceKind::Status));
        // ...and the canonical next-command envelope verbatim.
        assert_eq!(
            b.command_envelope,
            t.next_command_envelope(Some("/home/user/.local/share/cass"))
        );
        // ...and the quarantine summary verbatim.
        assert_eq!(b.quarantine, t.quarantine);

        // Structured parity proof (visible with --nocapture).
        let proof = serde_json::json!({
            "event": "bundle_robot_parity",
            "fixture": "css_stale_existing_index",
            "readiness_class": serde_json::to_value(b.readiness.class).unwrap(),
            "safe_next_action": serde_json::to_value(b.readiness.safe_next_action).unwrap(),
            "share_safe": b.is_share_safe(),
            "parity": true,
        });
        println!("{proof}");
    }

    // ---- fixture vs live provenance logging ----

    #[test]
    fn fixture_and_live_modes_are_distinguishable_in_logs() {
        let live = build_support_bundle(
            sample_inputs(false, None),
            default_redaction_policy(),
            ExecutionMode::Live,
        );
        let fixture = build_support_bundle(
            sample_inputs(false, Some("css_stale_existing_index")),
            default_redaction_policy(),
            ExecutionMode::Fixture,
        );
        assert_eq!(live.manifest.mode, ExecutionMode::Live);
        assert_eq!(fixture.manifest.mode, ExecutionMode::Fixture);

        for b in [&live, &fixture] {
            let log = serde_json::json!({
                "event": "support_bundle_generated",
                "bundle_id": b.manifest.bundle_id,
                "mode": serde_json::to_value(b.manifest.mode).unwrap(),
                "fixture_id": b.manifest.fixture_id,
                "share_safe": b.is_share_safe(),
            });
            println!("{log}");
        }
        // The wire form is unambiguous.
        let live_json = serde_json::to_string(&live.manifest).unwrap();
        let fixture_json = serde_json::to_string(&fixture.manifest).unwrap();
        assert!(live_json.contains("\"mode\":\"live\""));
        assert!(fixture_json.contains("\"mode\":\"fixture\""));
    }

    // ---- serialization / bounds ----

    #[test]
    fn bundle_round_trips_through_json_with_snake_case_own_fields() {
        let b = default_bundle();
        let json = serde_json::to_string(&b).unwrap();
        assert!(json.contains("\"manifest_version\":1"));
        assert!(json.contains("\"mode\":\"live\""));
        assert!(json.contains("\"full_paths\":false"));
        let parsed: RecoverySupportBundle = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, b);
    }

    #[test]
    fn bundle_is_bounded() {
        // The bundle references logs by path and embeds compact contracts, so a
        // single-node bundle stays small. Pin a generous cap to catch
        // accidental payload inlining.
        let b = default_bundle();
        let json = serde_json::to_string(&b).unwrap();
        assert!(
            json.len() < 16_384,
            "bundle unexpectedly large ({} bytes) — is a raw payload inlined?",
            json.len()
        );
        // Redaction field lists are bounded.
        assert!(b.redaction.fields_included.len() <= 12);
        assert!(b.redaction.opt_in_flags.len() <= 4);
    }

    #[test]
    fn execution_mode_and_proof_kind_serialize_snake_case() {
        assert_eq!(
            serde_json::to_string(&ExecutionMode::Fixture).unwrap(),
            "\"fixture\""
        );
        assert_eq!(
            serde_json::to_string(&ProofLogKind::ProofArtifact).unwrap(),
            "\"proof_artifact\""
        );
        assert_eq!(
            serde_json::to_string(&ProofLogKind::DiagnosticLog).unwrap(),
            "\"diagnostic_log\""
        );
    }
}
