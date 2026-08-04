//! Root-cause attribution taxonomy for CASS diagnostics.
//!
//! Bead: coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.9.1
//! ("Define root-cause attribution taxonomy for CASS diagnostics").
//!
//! This module is the single, stable, machine-consumable vocabulary that lets
//! `status`, `doctor`, fleet probes, and incident mining attribute a failure to
//! a *family* of root causes **without parsing human prose**. The motivating
//! observation is that dependency and host issues routinely masquerade as CASS
//! failures — frankensqlite `OpenRead`/FTS noise reads like a CASS bug, host
//! disk pressure or OOM reads like a hang, an old binary reads like a missing
//! feature. Encoding the families as a closed enum with stable kebab-case wire
//! values, per-family descriptors (examples, typical evidence, first probe,
//! false-positive guidance), and a structured attribution record makes that
//! distinction explicit and actionable.
//!
//! Consumers (the dependents of this bead — `9.2` projects the family into
//! status/doctor/fleet output; `10.1` builds the incident-mining schema on top;
//! `14.1` layers a storage-integrity sub-taxonomy) should depend ONLY on the
//! stable string values and the structured fields here, never on the `Debug`
//! representation or any prose.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// Stable schema version for the taxonomy wire format. Bump only on a breaking
/// change to the family set or the attribution record shape; additive,
/// backward-compatible changes (new descriptor prose) do not require a bump.
pub const ROOT_CAUSE_TAXONOMY_VERSION: u32 = 1;

/// A closed set of root-cause *families* for CASS diagnostics.
///
/// The wire value of each variant is its kebab-case name (e.g.
/// [`RootCauseFamily::FrankensqliteStorage`] serializes as
/// `"frankensqlite-storage"`). These strings are a stable contract: renaming a
/// variant is a breaking change that requires bumping
/// [`ROOT_CAUSE_TAXONOMY_VERSION`]. [`RootCauseFamily::Unknown`] is the explicit
/// fallback — a diagnostic must never invent a family or leave one unset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RootCauseFamily {
    /// CASS's own derived state is wrong or stale (index, caches, summaries,
    /// quarantine, derived-asset truth-table drift).
    CassDerivedState,
    /// The frankensqlite storage engine is the proximate cause (OpenRead/FTS
    /// errors, WAL sidecar issues, busy locks, schema/migration failures).
    FrankensqliteStorage,
    /// The frankensearch search stack is the proximate cause (lexical/semantic
    /// fusion, tantivy index corruption, fail-open lexical fallback).
    FrankensearchSearch,
    /// The asupersync async runtime is the proximate cause (task starvation,
    /// blocking-on-runtime, cancellation/`Cx` propagation faults).
    AsupersyncRuntime,
    /// Remote transport or authentication for sources/mirrors (rsync/scp/ssh2,
    /// expired credentials, host-key/permission failures, network timeouts).
    RemoteTransportAuth,
    /// Semantic assets are missing, partial, or incompatible (embedding model
    /// download, ONNX runtime, vector index build artifacts).
    SemanticAssets,
    /// Workspace provenance / configuration is wrong (sources config, data-dir
    /// resolution, agent-detection mapping, env/`.env` mismatch).
    WorkspaceProvenance,
    /// Host disk pressure (low free space, full tmpfs, ballast eviction) is the
    /// proximate cause rather than any CASS defect.
    HostDiskPressure,
    /// Host OOM / load pressure (memory pressure, oomd kills, runaway load
    /// average, swap thrash) is the proximate cause.
    HostOomLoad,
    /// The running CASS binary is older than the on-disk state or the expected
    /// contract version (feature/skew mismatch, stale install).
    OldBinarySkew,
    /// No family could be attributed with even `Possible` confidence. The
    /// explicit, required fallback — never omit attribution entirely.
    Unknown,
}

impl RootCauseFamily {
    /// Every family, in a stable, exhaustive order. The compiler-checked match
    /// in [`RootCauseFamily::as_str`] keeps this array honest: adding a variant
    /// without extending both sites fails to build.
    pub const ALL: [RootCauseFamily; 11] = [
        RootCauseFamily::CassDerivedState,
        RootCauseFamily::FrankensqliteStorage,
        RootCauseFamily::FrankensearchSearch,
        RootCauseFamily::AsupersyncRuntime,
        RootCauseFamily::RemoteTransportAuth,
        RootCauseFamily::SemanticAssets,
        RootCauseFamily::WorkspaceProvenance,
        RootCauseFamily::HostDiskPressure,
        RootCauseFamily::HostOomLoad,
        RootCauseFamily::OldBinarySkew,
        RootCauseFamily::Unknown,
    ];

    /// The stable kebab-case wire value. This is the single source of truth for
    /// the string contract; a unit test asserts it matches serde's output so the
    /// two can never silently drift.
    pub const fn as_str(self) -> &'static str {
        match self {
            RootCauseFamily::CassDerivedState => "cass-derived-state",
            RootCauseFamily::FrankensqliteStorage => "frankensqlite-storage",
            RootCauseFamily::FrankensearchSearch => "frankensearch-search",
            RootCauseFamily::AsupersyncRuntime => "asupersync-runtime",
            RootCauseFamily::RemoteTransportAuth => "remote-transport-auth",
            RootCauseFamily::SemanticAssets => "semantic-assets",
            RootCauseFamily::WorkspaceProvenance => "workspace-provenance",
            RootCauseFamily::HostDiskPressure => "host-disk-pressure",
            RootCauseFamily::HostOomLoad => "host-oom-load",
            RootCauseFamily::OldBinarySkew => "old-binary-skew",
            RootCauseFamily::Unknown => "unknown",
        }
    }

    /// Where the fault most likely lives. Lets a consumer cheaply answer the
    /// central question — "is this actually CASS's fault, or a dependency/host
    /// issue wearing a CASS mask?" — without string matching.
    pub const fn locus(self) -> FaultLocus {
        match self {
            RootCauseFamily::CassDerivedState | RootCauseFamily::WorkspaceProvenance => {
                FaultLocus::Cass
            }
            RootCauseFamily::FrankensqliteStorage
            | RootCauseFamily::FrankensearchSearch
            | RootCauseFamily::AsupersyncRuntime
            | RootCauseFamily::RemoteTransportAuth
            | RootCauseFamily::SemanticAssets => FaultLocus::Dependency,
            RootCauseFamily::HostDiskPressure | RootCauseFamily::HostOomLoad => FaultLocus::Host,
            RootCauseFamily::OldBinarySkew => FaultLocus::BinarySkew,
            RootCauseFamily::Unknown => FaultLocus::Unknown,
        }
    }

    /// `true` when the fault is *not* inside CASS proper — the masquerade case.
    /// Dependency, host, and binary-skew families return `true`; `Unknown`
    /// returns `false` because it makes no claim either way.
    pub const fn is_external_to_cass(self) -> bool {
        matches!(
            self.locus(),
            FaultLocus::Dependency | FaultLocus::Host | FaultLocus::BinarySkew
        )
    }

    /// The structured, prose-free descriptor for this family: examples, typical
    /// evidence-ref kinds, a cheap first probe, and false-positive guidance.
    pub const fn descriptor(self) -> RootCauseDescriptor {
        match self {
            RootCauseFamily::CassDerivedState => RootCauseDescriptor {
                family: self,
                title: "CASS derived state is wrong or stale",
                examples: &[
                    "status reports healthy but search returns nothing after a reindex",
                    "quarantine count disagrees with the derived-asset truth table",
                    "summaries reference sessions that no longer exist on disk",
                ],
                typical_evidence: &[
                    "cass.derived_asset.truth_table_mismatch",
                    "cass.index.last_indexed_ms",
                    "cass.cache.generation",
                ],
                first_probe: "cass doctor check --json",
                false_positive_guidance: "A cold or never-indexed data dir is NOT this family: \
zero derived state is expected, not corrupt. Require evidence of a mismatch \
between two CASS-owned facts (e.g. index count vs. truth table), not merely \
emptiness.",
            },
            RootCauseFamily::FrankensqliteStorage => RootCauseDescriptor {
                family: self,
                title: "frankensqlite storage engine fault",
                examples: &[
                    "OpenRead error opening the main DB under noisy fsqlite tracing",
                    "FTS query fails while plain row reads succeed",
                    "WAL sidecar present but unreadable; busy-lock under concurrent writers",
                ],
                typical_evidence: &[
                    "fsqlite.error_code",
                    "fsqlite.open_read_failure",
                    "file:cass.db-wal",
                ],
                first_probe: "cass diag --json",
                false_positive_guidance: "fsqlite INFO/TRACE log lines are noise, not evidence — \
the robot hygiene chokepoint suppresses them in machine modes. Attribute here \
only on a structured fsqlite error code or an OpenRead/FTS failure, never on \
the presence of tracing output.",
            },
            RootCauseFamily::FrankensearchSearch => RootCauseDescriptor {
                family: self,
                title: "frankensearch search-stack fault",
                examples: &[
                    "semantic results empty while lexical fail-open still returns hits",
                    "tantivy segment corruption on the lexical index",
                    "RRF fusion panics or returns degenerate ordering",
                ],
                typical_evidence: &[
                    "frankensearch.fusion_error",
                    "frankensearch.lexical_fail_open",
                    "frankensearch.tantivy_segment_error",
                ],
                first_probe: "cass search --json --dry-run <query>",
                false_positive_guidance: "Lexical fail-open returning results is the designed \
graceful degradation, not a fault by itself. Require a search-stack error or a \
semantic/lexical divergence beyond the documented fail-open contract.",
            },
            RootCauseFamily::AsupersyncRuntime => RootCauseDescriptor {
                family: self,
                title: "asupersync runtime fault",
                examples: &[
                    "a read-only probe hangs because a blocking call ran on the runtime",
                    "task starvation under load; cancellation does not propagate",
                    "Cx propagation lost across a spawn boundary",
                ],
                typical_evidence: &[
                    "asupersync.task_stall_ms",
                    "asupersync.blocking_on_runtime",
                    "asupersync.cx_propagation_lost",
                ],
                first_probe: "cass status --json --timeout-ms 8000",
                false_positive_guidance: "A command that is merely slow because of real I/O is not \
a runtime fault. Attribute here only when work stalls with no underlying I/O \
progress, or cancellation/timeout fails to take effect.",
            },
            RootCauseFamily::RemoteTransportAuth => RootCauseDescriptor {
                family: self,
                title: "remote transport or authentication fault",
                examples: &[
                    "rsync/scp over system OpenSSH fails before the ssh2 fallback",
                    "expired credentials or host-key mismatch on a remote mirror",
                    "network timeout pulling a remote source",
                ],
                typical_evidence: &[
                    "transport.ssh_exit_code",
                    "transport.auth_failure",
                    "transport.connect_timeout_ms",
                ],
                first_probe: "cass sources probe --json",
                false_positive_guidance: "A locally-configured source with no remote does not \
belong here. Distinguish transport/auth failure from a missing or misconfigured \
source path (which is workspace-provenance).",
            },
            RootCauseFamily::SemanticAssets => RootCauseDescriptor {
                family: self,
                title: "semantic assets missing or incompatible",
                examples: &[
                    "embedding model not downloaded; ONNX runtime missing",
                    "vector index build artifacts absent or partial",
                    "embedding dimension mismatch vs. the stored index",
                ],
                typical_evidence: &[
                    "semantic.model_present",
                    "semantic.vector_index_built",
                    "semantic.embedding_dim_mismatch",
                ],
                first_probe: "cass diag --json",
                false_positive_guidance: "Semantic search being disabled by configuration is not a \
fault. Attribute here only when semantic mode is requested/enabled but its \
assets are missing, partial, or incompatible.",
            },
            RootCauseFamily::WorkspaceProvenance => RootCauseDescriptor {
                family: self,
                title: "workspace provenance or configuration fault",
                examples: &[
                    "sources config points at a stale or moved data dir",
                    "agent-detection mapping misclassifies a session source",
                    ".env / data-dir resolution disagrees with the actual layout",
                ],
                typical_evidence: &[
                    "config.data_dir",
                    "config.sources_config_path",
                    "provenance.agent_mapping",
                ],
                first_probe: "cass status --json",
                false_positive_guidance: "Default first-run configuration is not a fault. Require a \
concrete mismatch between configured provenance and the on-disk reality, not the \
mere absence of customization.",
            },
            RootCauseFamily::HostDiskPressure => RootCauseDescriptor {
                family: self,
                title: "host disk pressure",
                examples: &[
                    "writes fail or stall because the filesystem is near-full",
                    "tmpfs build/cache directory exhausted",
                    "ballast eviction triggered by an external disk-pressure guard",
                ],
                typical_evidence: &[
                    "host.disk_free_bytes",
                    "host.disk_free_pct",
                    "host.tmpfs_free_bytes",
                ],
                first_probe: "df -h (host) / cass diag --json",
                false_positive_guidance: "Plenty of free space rules this out — never attribute \
disk pressure without a free-space metric below threshold. A single ENOSPC from \
an unrelated bind mount is not host-wide pressure.",
            },
            RootCauseFamily::HostOomLoad => RootCauseDescriptor {
                family: self,
                title: "host OOM or load pressure",
                examples: &[
                    "process killed by systemd-oomd under memory pressure",
                    "runaway load average from competing builds starves the probe",
                    "swap thrash makes every operation time out",
                ],
                typical_evidence: &[
                    "host.mem_available_bytes",
                    "host.load_avg_1m",
                    "host.oom_kill_count",
                ],
                first_probe: "cass diag --json / dmesg oom scan",
                false_positive_guidance: "A normally-loaded host is not under OOM/load pressure. \
Require memory-available below threshold, an oom-kill record, or load average \
well above core count — not merely a busy machine.",
            },
            RootCauseFamily::OldBinarySkew => RootCauseDescriptor {
                family: self,
                title: "old binary / contract skew",
                examples: &[
                    "running binary predates the on-disk schema or contract version",
                    "a requested flag is missing because the installed build is stale",
                    "fleet nodes report different api-version values",
                ],
                typical_evidence: &[
                    "binary.api_version",
                    "binary.contract_version",
                    "state.schema_version",
                ],
                first_probe: "cass api-version --json",
                false_positive_guidance: "A version difference within the supported compatibility \
window is not skew. Attribute here only when the binary's contract/api version is \
behind what the on-disk state or the fleet requires.",
            },
            RootCauseFamily::Unknown => RootCauseDescriptor {
                family: self,
                title: "unattributed",
                examples: &[
                    "a failure with no evidence pointing at any specific family",
                    "conflicting signals across families with none dominant",
                ],
                typical_evidence: &["diagnostic.unattributed_reason"],
                first_probe: "cass diag --json && cass doctor check --json",
                false_positive_guidance: "Do not use Unknown to avoid investigation: if any family \
reaches `Possible` confidence, attribute to it instead. Unknown means evidence \
was gathered and still pointed nowhere — record why in the attribution summary.",
            },
        }
    }
}

impl fmt::Display for RootCauseFamily {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error returned when parsing an unknown family string. Carries the offending
/// value so callers can log it without re-deriving it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseRootCauseFamilyError(pub String);

impl fmt::Display for ParseRootCauseFamilyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unrecognized root-cause family: {:?}", self.0)
    }
}

impl std::error::Error for ParseRootCauseFamilyError {}

impl FromStr for RootCauseFamily {
    type Err = ParseRootCauseFamilyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        RootCauseFamily::ALL
            .into_iter()
            .find(|family| family.as_str() == s)
            .ok_or_else(|| ParseRootCauseFamilyError(s.to_string()))
    }
}

/// Coarse classification of *where* a fault lives, derived from the family.
/// Lets consumers separate genuine CASS defects from dependency/host/skew noise
/// without matching on individual families.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FaultLocus {
    /// The fault is in CASS proper.
    Cass,
    /// The fault is in a bundled dependency (fsqlite/frankensearch/asupersync/…).
    Dependency,
    /// The fault is in the host environment (disk/OOM/load).
    Host,
    /// The fault is a binary/contract version skew.
    BinarySkew,
    /// Locus could not be determined.
    Unknown,
}

/// Confidence with which a [`RootCauseFamily`] is attributed to an observation.
/// A diagnostic must always state a confidence; `Unknown` is distinct from a
/// low-but-nonzero `Possible`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AttributionConfidence {
    /// Direct, unambiguous evidence (e.g. an explicit fsqlite error code).
    Confirmed,
    /// Strong but circumstantial evidence; the most likely single explanation.
    Probable,
    /// Plausible given the evidence, but other families are not excluded.
    Possible,
    /// No confidence — pair only with [`RootCauseFamily::Unknown`].
    Unknown,
}

impl AttributionConfidence {
    /// Monotonic rank for comparison/selection (higher = more certain).
    /// Prefer this over the derived `Ord` when ranking, since it is explicit
    /// about direction and stable against future reordering of variants.
    pub const fn rank(self) -> u8 {
        match self {
            AttributionConfidence::Confirmed => 3,
            AttributionConfidence::Probable => 2,
            AttributionConfidence::Possible => 1,
            AttributionConfidence::Unknown => 0,
        }
    }

    /// `true` when the confidence is high enough to act on without further
    /// probing (`Confirmed` or `Probable`).
    pub const fn is_actionable(self) -> bool {
        self.rank() >= AttributionConfidence::Probable.rank()
    }
}

/// Prose-free, per-family reference card: examples, typical evidence-ref kinds,
/// a cheap first probe, and false-positive guidance. Returned by
/// [`RootCauseFamily::descriptor`] and safe to serialize directly into a robot
/// surface so an agent can act without consulting documentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct RootCauseDescriptor {
    /// The family this descriptor documents.
    pub family: RootCauseFamily,
    /// Short human title (UI/log convenience; not part of the machine contract).
    pub title: &'static str,
    /// Representative symptoms an operator or agent might observe.
    pub examples: &'static [&'static str],
    /// Stable evidence-ref *kinds* a diagnostic should populate
    /// ([`EvidenceRef::kind`]) when attributing to this family.
    pub typical_evidence: &'static [&'static str],
    /// The cheapest bounded probe to confirm or rule out this family.
    pub first_probe: &'static str,
    /// Guidance to avoid attributing this family on insufficient or misleading
    /// signals (the recurring "dependency/host noise looks like CASS" trap).
    pub false_positive_guidance: &'static str,
}

/// A single structured pointer to the evidence behind an attribution. Diagnostic
/// surfaces emit these instead of free-text so consumers can correlate, dedupe,
/// and act without parsing prose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceRef {
    /// Stable kind of evidence — typically one of the family's
    /// [`RootCauseDescriptor::typical_evidence`] keys (e.g.
    /// `"fsqlite.error_code"`, `"host.disk_free_pct"`).
    pub kind: String,
    /// Where the evidence lives: a file path, metric key, log span id, or
    /// exit-code reference.
    pub locator: String,
    /// Optional observed value or short detail (e.g. the actual error code).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl EvidenceRef {
    /// Construct an evidence ref with no extra detail.
    pub fn new(kind: impl Into<String>, locator: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            locator: locator.into(),
            detail: None,
        }
    }

    /// Attach an observed value / short detail.
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

/// The structured attribution record a diagnostic surface emits. This is the
/// machine contract consumed by status/doctor/fleet/incident mining: a family,
/// a confidence, the evidence behind it, a short summary, and the recommended
/// next bounded probe. Serializes with stable snake_case field names and an
/// embedded [`schema_version`](Self::schema_version).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootCauseAttribution {
    /// Taxonomy schema version, mirrors [`ROOT_CAUSE_TAXONOMY_VERSION`].
    pub schema_version: u32,
    /// The attributed family.
    pub family: RootCauseFamily,
    /// Where the fault lives, derived from `family` for consumer convenience.
    pub locus: FaultLocus,
    /// Confidence in the attribution.
    pub confidence: AttributionConfidence,
    /// Structured evidence supporting the attribution (may be empty only for
    /// [`RootCauseFamily::Unknown`]).
    #[serde(default)]
    pub evidence_refs: Vec<EvidenceRef>,
    /// One-line, action-oriented summary. Human-readable, but never the place
    /// for facts a consumer needs — those go in `evidence_refs`.
    pub summary: String,
    /// The recommended next bounded probe to confirm/refine, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommended_next_probe: Option<String>,
}

impl RootCauseAttribution {
    /// Build an attribution for `family` at `confidence` with the given summary.
    /// Pre-seeds `recommended_next_probe` from the family descriptor's cheap
    /// first probe; callers may override via [`Self::with_next_probe`].
    pub fn new(
        family: RootCauseFamily,
        confidence: AttributionConfidence,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            schema_version: ROOT_CAUSE_TAXONOMY_VERSION,
            family,
            locus: family.locus(),
            confidence,
            evidence_refs: Vec::new(),
            summary: summary.into(),
            recommended_next_probe: Some(family.descriptor().first_probe.to_string()),
        }
    }

    /// The explicit, evidence-free "we looked and found nothing" attribution.
    /// Pair with a summary explaining what was probed.
    pub fn unattributed(summary: impl Into<String>) -> Self {
        let mut attribution = Self::new(
            RootCauseFamily::Unknown,
            AttributionConfidence::Unknown,
            summary,
        );
        // Unknown's descriptor first_probe is a reasonable default next step.
        attribution.evidence_refs.clear();
        attribution
    }

    /// Replace the evidence set.
    pub fn with_evidence(mut self, evidence: Vec<EvidenceRef>) -> Self {
        self.evidence_refs = evidence;
        self
    }

    /// Append a single evidence ref.
    pub fn push_evidence(&mut self, evidence: EvidenceRef) {
        self.evidence_refs.push(evidence);
    }

    /// Override the recommended next probe (use `None` to clear it).
    pub fn with_next_probe(mut self, probe: Option<String>) -> Self {
        self.recommended_next_probe = probe;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_families_present_unique_and_include_unknown() {
        // The bead requires exactly these eleven families.
        let required = [
            "cass-derived-state",
            "frankensqlite-storage",
            "frankensearch-search",
            "asupersync-runtime",
            "remote-transport-auth",
            "semantic-assets",
            "workspace-provenance",
            "host-disk-pressure",
            "host-oom-load",
            "old-binary-skew",
            "unknown",
        ];
        let actual: Vec<&str> = RootCauseFamily::ALL.iter().map(|f| f.as_str()).collect();
        assert_eq!(
            actual, required,
            "family set/order must match the bead contract"
        );

        // No duplicates.
        let mut sorted = actual.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            RootCauseFamily::ALL.len(),
            "families must be unique"
        );

        // Unknown fallback is present.
        assert!(RootCauseFamily::ALL.contains(&RootCauseFamily::Unknown));
    }

    #[test]
    fn as_str_round_trips_through_from_str() {
        for family in RootCauseFamily::ALL {
            let parsed: RootCauseFamily = family.as_str().parse().expect("parse stable str");
            assert_eq!(parsed, family);
        }
    }

    #[test]
    fn from_str_rejects_unknown_value() {
        let err = "not-a-family".parse::<RootCauseFamily>().unwrap_err();
        assert_eq!(err, ParseRootCauseFamilyError("not-a-family".to_string()));
    }

    #[test]
    fn serde_wire_value_matches_as_str() {
        // Guards against silent drift between the manual `as_str` contract and
        // serde's `rename_all = "kebab-case"` derivation.
        for family in RootCauseFamily::ALL {
            let json = serde_json::to_string(&family).expect("serialize");
            assert_eq!(json, format!("\"{}\"", family.as_str()));
            let back: RootCauseFamily = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, family);
        }
    }

    #[test]
    fn every_family_has_a_complete_descriptor() {
        for family in RootCauseFamily::ALL {
            let d = family.descriptor();
            assert_eq!(d.family, family, "descriptor must report its own family");
            assert!(!d.title.is_empty(), "{family}: empty title");
            assert!(!d.examples.is_empty(), "{family}: must have examples");
            assert!(
                d.examples.iter().all(|e| !e.is_empty()),
                "{family}: empty example"
            );
            assert!(
                !d.typical_evidence.is_empty(),
                "{family}: must list typical evidence kinds"
            );
            assert!(
                !d.first_probe.is_empty(),
                "{family}: must have a first probe"
            );
            assert!(
                d.false_positive_guidance.len() > 20,
                "{family}: false-positive guidance must be substantive"
            );
        }
    }

    #[test]
    fn locus_classification_separates_cass_from_dependency_and_host() {
        assert_eq!(RootCauseFamily::CassDerivedState.locus(), FaultLocus::Cass);
        assert_eq!(
            RootCauseFamily::WorkspaceProvenance.locus(),
            FaultLocus::Cass
        );
        for dep in [
            RootCauseFamily::FrankensqliteStorage,
            RootCauseFamily::FrankensearchSearch,
            RootCauseFamily::AsupersyncRuntime,
            RootCauseFamily::RemoteTransportAuth,
            RootCauseFamily::SemanticAssets,
        ] {
            assert_eq!(
                dep.locus(),
                FaultLocus::Dependency,
                "{dep} should be a dependency"
            );
            assert!(dep.is_external_to_cass(), "{dep} masquerades as CASS");
        }
        assert_eq!(RootCauseFamily::HostDiskPressure.locus(), FaultLocus::Host);
        assert_eq!(RootCauseFamily::HostOomLoad.locus(), FaultLocus::Host);
        assert_eq!(
            RootCauseFamily::OldBinarySkew.locus(),
            FaultLocus::BinarySkew
        );
        assert_eq!(RootCauseFamily::Unknown.locus(), FaultLocus::Unknown);

        // CASS-proper families and Unknown are not "external".
        assert!(!RootCauseFamily::CassDerivedState.is_external_to_cass());
        assert!(!RootCauseFamily::Unknown.is_external_to_cass());
    }

    #[test]
    fn confidence_rank_is_monotonic_and_actionability_threshold_holds() {
        assert!(AttributionConfidence::Confirmed.rank() > AttributionConfidence::Probable.rank());
        assert!(AttributionConfidence::Probable.rank() > AttributionConfidence::Possible.rank());
        assert!(AttributionConfidence::Possible.rank() > AttributionConfidence::Unknown.rank());

        assert!(AttributionConfidence::Confirmed.is_actionable());
        assert!(AttributionConfidence::Probable.is_actionable());
        assert!(!AttributionConfidence::Possible.is_actionable());
        assert!(!AttributionConfidence::Unknown.is_actionable());
    }

    #[test]
    fn attribution_serializes_with_stable_fields_and_locus() {
        let attribution = RootCauseAttribution::new(
            RootCauseFamily::FrankensqliteStorage,
            AttributionConfidence::Confirmed,
            "OpenRead failed on main DB",
        )
        .with_evidence(vec![
            EvidenceRef::new("fsqlite.error_code", "cass.db").with_detail("SQLITE_CANTOPEN"),
        ]);

        let value = serde_json::to_value(&attribution).expect("serialize");
        assert_eq!(value["schema_version"], ROOT_CAUSE_TAXONOMY_VERSION);
        assert_eq!(value["family"], "frankensqlite-storage");
        assert_eq!(value["locus"], "dependency");
        assert_eq!(value["confidence"], "confirmed");
        assert_eq!(value["summary"], "OpenRead failed on main DB");
        assert_eq!(value["evidence_refs"][0]["kind"], "fsqlite.error_code");
        assert_eq!(value["evidence_refs"][0]["detail"], "SQLITE_CANTOPEN");
        // first_probe is pre-seeded as the recommended next probe.
        assert_eq!(value["recommended_next_probe"], "cass diag --json");

        // Round-trips back to an equal record.
        let back: RootCauseAttribution = serde_json::from_value(value).expect("deserialize");
        assert_eq!(back, attribution);
    }

    #[test]
    fn unattributed_is_unknown_with_no_evidence() {
        let attribution = RootCauseAttribution::unattributed("probed status+doctor, no signal");
        assert_eq!(attribution.family, RootCauseFamily::Unknown);
        assert_eq!(attribution.confidence, AttributionConfidence::Unknown);
        assert_eq!(attribution.locus, FaultLocus::Unknown);
        assert!(attribution.evidence_refs.is_empty());
    }

    #[test]
    fn evidence_ref_omits_detail_when_absent() {
        let value = serde_json::to_value(EvidenceRef::new("host.disk_free_pct", "/")).unwrap();
        assert!(
            value.get("detail").is_none(),
            "absent detail must be skipped"
        );
        assert_eq!(value["kind"], "host.disk_free_pct");
        assert_eq!(value["locator"], "/");
    }

    #[test]
    fn full_taxonomy_catalog_is_serializable_and_complete() {
        // A light golden: the whole taxonomy projected to JSON, as a consumer
        // (status/doctor/fleet) would embed it. Every family string must appear.
        let catalog: Vec<RootCauseDescriptor> = RootCauseFamily::ALL
            .iter()
            .map(|f| f.descriptor())
            .collect();
        let json = serde_json::to_string(&catalog).expect("serialize catalog");
        for family in RootCauseFamily::ALL {
            assert!(
                json.contains(family.as_str()),
                "catalog JSON missing family {family}"
            );
        }
    }
}
