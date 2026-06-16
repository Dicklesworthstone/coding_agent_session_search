//! Subsystem coverage matrix and closeout gate for every report failure-mode file.
//!
//! Bead: coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.15.5
//! ("Add subsystem coverage matrix and closeout gate for all report failure-mode
//! files").
//!
//! The mining report
//! (`COMPREHENSIVE_ISSUES_AND_PROBLEMS_WITH_CASS_BASED_ON_COMPLETE_SESSION_HISTORY_ANALYSIS.md`)
//! enumerates fifteen generated subsystem failure-mode files: `analytics`,
//! `bookmarks`, `cache`, `cli_robot`, `connectors`, `daemon`, `html_export`,
//! `indexer`, `models`, `pages`, `search`, `sources`, `storage`, `tui`, and
//! `update_check`. The whole point of converting that report into beads was to
//! make the Beads graph self-contained so a future agent never has to reopen the
//! markdown plan. That only holds if *every* subsystem file has a named owner,
//! a real proof path, a logging/redaction expectation, and a closure-evidence
//! format — not prose.
//!
//! [`RESILIENCE_TEST_MATRIX.md`](../../docs/RESILIENCE_TEST_MATRIX.md) (bead
//! `.12.1`) is the prose map of *which proof each family owes*; this module is
//! its **executable** counterpart: the same coverage, encoded as data that a
//! test can check against the real repository. The closeout gate fails when a
//! report subsystem has no owning bead, no mandatory proof level, no proof
//! artifact, no logging expectation, or cites a proof artifact that does not
//! exist on disk (the "only prose evidence" failure). The integrated resilience
//! gate (bead `.11.5`) consumes [`matrix_gaps`] so subsystem coverage cannot be
//! silently skipped during closeout.
//!
//! Pure, side-effect-free logic. The on-disk existence check is expressed as a
//! caller-supplied predicate ([`missing_artifacts`]) so the logic stays pure and
//! testable; the `#[cfg(test)]` gate supplies the real filesystem.

use serde::Serialize;

/// Stable schema version for the subsystem-coverage wire format.
pub const SUBSYSTEM_COVERAGE_SCHEMA_VERSION: u32 = 1;

/// The fifteen report subsystem failure-mode files, in canonical (alphabetical)
/// order. The closeout gate requires exactly one matrix row per name.
pub const REPORT_SUBSYSTEM_FILES: [&str; 15] = [
    "analytics",
    "bookmarks",
    "cache",
    "cli_robot",
    "connectors",
    "daemon",
    "html_export",
    "indexer",
    "models",
    "pages",
    "search",
    "sources",
    "storage",
    "tui",
    "update_check",
];

/// The precursor design docs this matrix builds on. The gate asserts both still
/// exist so the executable matrix never drifts away from its prose authority.
pub const PRECURSOR_DOCS: [&str; 2] = ["docs/RESILIENCE_TEST_MATRIX.md", "docs/PROOF_RECIPE.md"];

/// Proof levels (weakest to strongest), mirroring the legend in
/// `RESILIENCE_TEST_MATRIX.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProofLevel {
    /// In-crate `#[cfg(test)]` over pure schema/classifier logic; no I/O.
    Unit,
    /// Real types across modules / `tests/` with isolated data dirs; no live net.
    Integration,
    /// A pinned artifact (JSON/JSONL/snapshot/markdown) a change must update.
    Golden,
    /// A bounded run of the real `cass` binary asserting stdout/stderr/exit.
    E2e,
    /// A structured proof-log/artifact manifest distinguishing pass from timeout.
    Logs,
}

impl ProofLevel {
    /// Stable kebab-case label.
    pub const fn as_str(self) -> &'static str {
        match self {
            ProofLevel::Unit => "unit",
            ProofLevel::Integration => "integration",
            ProofLevel::Golden => "golden",
            ProofLevel::E2e => "e2e",
            ProofLevel::Logs => "logs",
        }
    }
}

/// What may be shared from a subsystem's evidence without leaking user content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RedactionExpectation {
    /// Diagnostics are structured counts/kinds/paths only — no user content.
    NoUserContent,
    /// Content stays on the local machine; only kinds/counts may be shared.
    LocalOnly,
    /// User content is present and must be redacted before any sharing/export.
    RedactRequired,
}

impl RedactionExpectation {
    /// Stable kebab-case label.
    pub const fn as_str(self) -> &'static str {
        match self {
            RedactionExpectation::NoUserContent => "no-user-content",
            RedactionExpectation::LocalOnly => "local-only",
            RedactionExpectation::RedactRequired => "redact-required",
        }
    }
}

/// One subsystem's coverage row: who owns it, which failure modes it represents,
/// which proof is mandatory, which real artifacts prove it, and how closure is
/// evidenced. Borrowed `&'static` fields: the matrix is a source-of-truth
/// constant rendered to JSON / markdown, never deserialized.
#[derive(Debug, Clone, Serialize)]
pub struct SubsystemCoverage {
    /// Canonical report subsystem file name (a member of [`REPORT_SUBSYSTEM_FILES`]).
    pub subsystem: &'static str,
    /// Owning bead suffixes (e.g. `.15.4`) within the fleet-resilience epic.
    pub owning_beads: &'static [&'static str],
    /// Representative `fm-*` finding ids this subsystem must keep covered.
    pub failure_modes: &'static [&'static str],
    /// Proof levels that are mandatory for closure of this subsystem's beads.
    pub mandatory_proofs: &'static [ProofLevel],
    /// Repo-relative paths of the proofs — every one must exist on disk.
    pub proof_artifacts: &'static [&'static str],
    /// Optional/live diagnostics that strengthen but never gate closure.
    pub optional_diagnostics: &'static [&'static str],
    /// What the fixtures represent and which data is synthetic.
    pub fixture_provenance: &'static str,
    /// The structured-log expectation for runs against this subsystem.
    pub log_expectation: &'static str,
    /// Privacy/redaction expectation for shared evidence.
    pub redaction: RedactionExpectation,
    /// The closure-evidence format a closing bead must cite (command + result).
    pub closure_evidence: &'static str,
}

/// A way a coverage row (or the matrix as a whole) fails the closeout gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "gap", rename_all = "kebab-case")]
pub enum CoverageGap {
    /// A report subsystem file has no matrix row.
    Unmapped { subsystem: String },
    /// A report subsystem file has more than one matrix row.
    Duplicate { subsystem: String },
    /// A matrix row names something that is not a report subsystem file.
    UnknownSubsystem { subsystem: String },
    /// The row has no owning bead.
    NoOwningBead { subsystem: String },
    /// The row names no mandatory proof level.
    NoMandatoryProof { subsystem: String },
    /// The row cites no proof artifact path.
    NoProofArtifact { subsystem: String },
    /// The row states no logging expectation.
    NoLoggingExpectation { subsystem: String },
    /// The row states no fixture provenance.
    NoFixtureProvenance { subsystem: String },
    /// The row states no closure-evidence format.
    NoClosureEvidence { subsystem: String },
    /// A cited proof artifact does not exist on disk (prose-only evidence).
    MissingArtifact { subsystem: String, artifact: String },
}

/// The fifteen-row subsystem coverage matrix. Order matches
/// [`REPORT_SUBSYSTEM_FILES`].
pub fn subsystem_coverage_matrix() -> Vec<SubsystemCoverage> {
    vec![
        SubsystemCoverage {
            subsystem: "analytics",
            owning_beads: &[".15.4", ".9.4"],
            failure_modes: &[
                "fm-analytics-rebuild-grouped-aggregate",
                "fm-analytics-charts-saturating-zero",
            ],
            mandatory_proofs: &[ProofLevel::Unit, ProofLevel::Golden],
            proof_artifacts: &[
                "src/metric_integrity.rs",
                "tests/analytics_cost_pricing_table_contract.rs",
            ],
            optional_diagnostics: &["live analytics rollup tail under doctor --fix"],
            fixture_provenance: "synthetic usage-ledger rows; no real session content",
            log_expectation: "deterministic --lib result line; proof-log when wired into doctor rebuild",
            redaction: RedactionExpectation::NoUserContent,
            closure_evidence: "cargo test --lib metric_integrity + analytics contract test path/count",
        },
        SubsystemCoverage {
            subsystem: "bookmarks",
            owning_beads: &[".15.4"],
            failure_modes: &[
                "fm-bookmarks-silent-row-decode",
                "fm-bookmarks-import-non-atomic",
            ],
            mandatory_proofs: &[ProofLevel::Unit, ProofLevel::Integration],
            proof_artifacts: &["src/bookmarks.rs", "src/metric_integrity.rs"],
            optional_diagnostics: &[],
            fixture_provenance: "synthetic bookmark rows; labels may embed workspace paths",
            log_expectation: "deterministic --lib result line; no structured log required for pure core",
            redaction: RedactionExpectation::LocalOnly,
            closure_evidence: "cargo test --lib bookmarks result line",
        },
        SubsystemCoverage {
            subsystem: "cache",
            owning_beads: &[".15.2"],
            failure_modes: &["fm-cache-tantivy-searcher-stale"],
            mandatory_proofs: &[ProofLevel::Integration],
            proof_artifacts: &[
                "src/daemon_runtime_state.rs",
                "tests/search_caching.rs",
                "tests/regex_cache.rs",
            ],
            optional_diagnostics: &["live searcher generation/reload tail"],
            fixture_provenance: "synthetic generation counters; no user content",
            log_expectation: "deterministic --lib result line for SearcherCacheOutcome",
            redaction: RedactionExpectation::NoUserContent,
            closure_evidence: "cargo test --lib daemon_runtime_state + search_caching",
        },
        SubsystemCoverage {
            subsystem: "cli_robot",
            owning_beads: &[".2.4", ".11.1", ".11.5", ".12.6"],
            failure_modes: &[
                "fm-cli-robot-schema-drift",
                "fm-cli-exit-code-regression",
                "fm-cli-golden-snapshot-drift",
            ],
            mandatory_proofs: &[ProofLevel::E2e, ProofLevel::Golden, ProofLevel::Logs],
            proof_artifacts: &[
                "tests/e2e_robot_smoke_gate.rs",
                "tests/cli_robot.rs",
                "tests/cli_robot_log_hygiene.rs",
            ],
            optional_diagnostics: &[],
            fixture_provenance: "isolated empty data dir; real cass binary dispatch",
            log_expectation: "PhaseTracker structured log + manifest per .12.3 (E2E_LOG=1)",
            redaction: RedactionExpectation::NoUserContent,
            closure_evidence: "cargo test --test e2e_robot_smoke_gate + golden robot JSON diff",
        },
        SubsystemCoverage {
            subsystem: "connectors",
            owning_beads: &[".15.1", ".3.1", ".14.1"],
            failure_modes: &[
                "fm-connectors-jsonl-parse-error",
                "fm-connectors-cursor-vscdb-locked",
                "fm-connectors-chatgpt-encrypted-undecipherable",
                "fm-connectors-amp-stem-prefix-unsafe",
                "fm-indexer-aider-external-id-collision",
            ],
            mandatory_proofs: &[
                ProofLevel::Unit,
                ProofLevel::Integration,
                ProofLevel::Golden,
            ],
            proof_artifacts: &[
                "src/connector_ingest_diagnostics.rs",
                "tests/connector_cursor.rs",
                "tests/connector_chatgpt.rs",
                "tests/connector_aider.rs",
                "tests/connector_amp.rs",
            ],
            optional_diagnostics: &["per-provider live ingest probe"],
            fixture_provenance: "per-provider synthetic session files; encrypted blob is non-decipherable by design",
            log_expectation: "deterministic --lib result line; per-provider conformance fixtures",
            redaction: RedactionExpectation::LocalOnly,
            closure_evidence: "cargo test --lib connector_ingest_diagnostics + per-provider conformance",
        },
        SubsystemCoverage {
            subsystem: "daemon",
            owning_beads: &[".15.2", ".2.2", ".4.1"],
            failure_modes: &[
                "fm-daemon-stale-pidfile-socket",
                "fm-daemon-fd-leak-on-tryclone",
            ],
            mandatory_proofs: &[ProofLevel::Integration],
            proof_artifacts: &[
                "src/daemon_runtime_state.rs",
                "tests/daemon_client_integration.rs",
            ],
            optional_diagnostics: &["live socket bind/stale-cleanup observation"],
            fixture_provenance: "synthetic runtime artifacts; flock .spawnlock, no pidfile",
            log_expectation: "deterministic --lib result line for DaemonRuntimeState",
            redaction: RedactionExpectation::NoUserContent,
            closure_evidence: "cargo test --lib daemon_runtime_state + daemon_client_integration",
        },
        SubsystemCoverage {
            subsystem: "html_export",
            owning_beads: &[".15.3", ".10.5", ".12.3", ".13.3"],
            failure_modes: &[
                "fm-html-export-markdown-injection",
                "fm-html-export-encryption-failure",
                "fm-encryption-nonce-type-safety",
                "fm-encryption-utf8-byte-slicing",
            ],
            mandatory_proofs: &[ProofLevel::Integration, ProofLevel::Golden, ProofLevel::E2e],
            proof_artifacts: &[
                "tests/html_export_sanitization_security.rs",
                "tests/html_export_integration.rs",
                "tests/html_export_e2e.rs",
            ],
            optional_diagnostics: &[],
            fixture_provenance: "synthetic malicious markdown/script inputs; encrypted body is ciphertext",
            log_expectation: "structured proof-log for the real-binary export run; encrypted bodies stay ciphertext",
            redaction: RedactionExpectation::RedactRequired,
            closure_evidence: "cargo test --test html_export_sanitization_security + golden html_export diff",
        },
        SubsystemCoverage {
            subsystem: "indexer",
            owning_beads: &[".1", ".4", ".11.2", ".12.5", ".14.4"],
            failure_modes: &[
                "fm-indexer-stale-lexical-publish-backups",
                "fm-indexer-tantivy-corrupt-or-stale",
                "fm-indexer-fsvi-vector-orphan",
                "fm-indexer-zero-results-regression",
                "fm-indexer-edge-ngram-mismatch",
                "fm-indexer-double-saturating-sub",
            ],
            mandatory_proofs: &[ProofLevel::Integration, ProofLevel::Golden],
            proof_artifacts: &[
                "tests/indexer_tantivy.rs",
                "tests/atomic_swap_publish_crash_window.rs",
                "src/search/regression_corpus.rs",
            ],
            optional_diagnostics: &["live publish/backup retention tail"],
            fixture_provenance: "synthetic index trees + crash-window fixtures; SQLite is source of truth",
            log_expectation: "proof-log over publish/atomic-swap runs; deterministic regression-corpus replay",
            redaction: RedactionExpectation::NoUserContent,
            closure_evidence: "cargo test --lib search::regression_corpus + atomic_swap_publish_crash_window",
        },
        SubsystemCoverage {
            subsystem: "models",
            owning_beads: &[".5", ".5.1", ".5.5"],
            failure_modes: &[
                "fm-models-fastembed-missing",
                "fm-models-onnx-load-failure",
                "fm-models-checksum-mismatch",
            ],
            mandatory_proofs: &[ProofLevel::Integration],
            proof_artifacts: &[
                "tests/cli_model_lifecycle_contract.rs",
                "tests/e2e_analytics_models.rs",
            ],
            optional_diagnostics: &["live model download (opt-in; never CI-required)"],
            fixture_provenance: "missing/mismatched model dirs; no network at test time",
            log_expectation: "deterministic contract test result line; live download is opt-in only",
            redaction: RedactionExpectation::NoUserContent,
            closure_evidence: "cargo test --test cli_model_lifecycle_contract",
        },
        SubsystemCoverage {
            subsystem: "pages",
            owning_beads: &[".15.4", ".13.2", ".13.4", ".12.6"],
            failure_modes: &[
                "fm-pages-render-parity-drift",
                "fm-pages-export-sanitization",
            ],
            mandatory_proofs: &[ProofLevel::Integration, ProofLevel::Golden],
            proof_artifacts: &[
                "tests/pages_export_golden.rs",
                "tests/pages_pipeline_e2e.rs",
                "tests/pages_error_handling_e2e.rs",
            ],
            optional_diagnostics: &["CI-only browser E2E (never local)"],
            fixture_provenance: "synthetic conversations; exported page content is sanitized",
            log_expectation: "golden page artifact diff; CI browser logs for render fidelity",
            redaction: RedactionExpectation::RedactRequired,
            closure_evidence: "cargo test --test pages_export_golden result line + golden diff",
        },
        SubsystemCoverage {
            subsystem: "search",
            owning_beads: &[".1", ".4", ".5", ".7", ".11.2", ".15.2"],
            failure_modes: &[
                "fm-search-rrf-fast-unwrap-panic",
                "fm-search-regex-pipe-in-charclass",
            ],
            mandatory_proofs: &[
                ProofLevel::Unit,
                ProofLevel::Integration,
                ProofLevel::Golden,
            ],
            proof_artifacts: &[
                "src/search/regression_corpus.rs",
                "tests/search_pipeline.rs",
                "tests/spec_search_determinism.rs",
            ],
            optional_diagnostics: &["live semantic refinement tail"],
            fixture_provenance: "synthetic corpus; query/results stay local",
            log_expectation: "deterministic regression-corpus + determinism spec result lines",
            redaction: RedactionExpectation::LocalOnly,
            closure_evidence: "cargo test --lib search::regression_corpus + spec_search_determinism",
        },
        SubsystemCoverage {
            subsystem: "sources",
            owning_beads: &[".8", ".8.1", ".8.2", ".8.4", ".8.5"],
            failure_modes: &[
                "fm-sources-rsync-not-on-path",
                "fm-sources-toml-malformed",
                "fm-sources-toctou-existence-race",
            ],
            mandatory_proofs: &[
                ProofLevel::Unit,
                ProofLevel::Integration,
                ProofLevel::Golden,
            ],
            proof_artifacts: &[
                "src/source_doctor_health.rs",
                "tests/e2e_sources.rs",
                "tests/setup_workflow.rs",
            ],
            optional_diagnostics: &["live SSH source probe (opt-in)"],
            fixture_provenance: "synthetic sources.toml + mirror dirs; no SSH session opened at test time",
            log_expectation: "deterministic --lib result line; structured logs for e2e source flows",
            redaction: RedactionExpectation::LocalOnly,
            closure_evidence: "cargo test --lib source_doctor_health + e2e_sources",
        },
        SubsystemCoverage {
            subsystem: "storage",
            owning_beads: &[".14", ".14.1", ".14.4", ".9.4"],
            failure_modes: &[
                "fm-storage-frankensqlite-openread-cursor",
                "fm-storage-pragma-integrity-fail",
                "fm-storage-wal-multiprocess-corruption",
                "fm-storage-rusqlite-frankensqlite-incompat",
                "fm-storage-schema-version-drift",
                "fm-storage-busy-lock-timeout",
                "fm-storage-stale-wal-orphan",
                "fm-storage-sql-fmt-injection-risk",
            ],
            mandatory_proofs: &[ProofLevel::Integration, ProofLevel::Golden, ProofLevel::E2e],
            proof_artifacts: &[
                "tests/e2e_storage_failure_fixture_gate.rs",
                "tests/storage.rs",
                "tests/storage_migration_safety.rs",
            ],
            optional_diagnostics: &["live integrity-check sweep"],
            fixture_provenance: "deterministic raw-byte corrupt fixtures; DB preserved byte-identical",
            log_expectation: "structured proof-log over the real-binary storage-failure gate",
            redaction: RedactionExpectation::NoUserContent,
            closure_evidence: "cargo test --test e2e_storage_failure_fixture_gate result line",
        },
        SubsystemCoverage {
            subsystem: "tui",
            owning_beads: &[".15.4", ".13.2", ".13.4", ".12.6"],
            failure_modes: &["fm-tui-render-parity-drift", "fm-tui-human-robot-mismatch"],
            mandatory_proofs: &[ProofLevel::Integration, ProofLevel::Golden, ProofLevel::E2e],
            proof_artifacts: &[
                "tests/e2e_human_robot_parity_gate.rs",
                "tests/tui_smoke.rs",
                "tests/e2e_tui_smoke_flows.rs",
            ],
            optional_diagnostics: &["live TUI asciicast capture"],
            fixture_provenance: "headless TUI fixtures; human summaries carry redacted fields verbatim",
            log_expectation: "structured proof-log over the human/robot parity gate",
            redaction: RedactionExpectation::RedactRequired,
            closure_evidence: "cargo test --test e2e_human_robot_parity_gate result line",
        },
        SubsystemCoverage {
            subsystem: "update_check",
            owning_beads: &[".15.4"],
            failure_modes: &["fm-update-shell-injection", "fm-update-clock-rollback"],
            mandatory_proofs: &[ProofLevel::Unit],
            proof_artifacts: &["src/update_check.rs", "src/metric_integrity.rs"],
            optional_diagnostics: &["live update-channel probe (opt-in)"],
            fixture_provenance: "synthetic version strings + clock values; no network at test time",
            log_expectation: "deterministic --lib result line for sanitized-arg and clock-rollback paths",
            redaction: RedactionExpectation::NoUserContent,
            closure_evidence: "cargo test --lib update_check result line",
        },
    ]
}

/// Per-row structural completeness gaps (no I/O). A row is complete when it has
/// a known subsystem name, at least one owning bead, at least one mandatory
/// proof level, at least one proof artifact, a non-empty logging expectation,
/// fixture provenance, and a closure-evidence format.
pub fn row_gaps(row: &SubsystemCoverage) -> Vec<CoverageGap> {
    let mut gaps = Vec::new();
    let name = row.subsystem.to_string();
    if !REPORT_SUBSYSTEM_FILES.contains(&row.subsystem) {
        gaps.push(CoverageGap::UnknownSubsystem {
            subsystem: name.clone(),
        });
    }
    if row.owning_beads.is_empty() {
        gaps.push(CoverageGap::NoOwningBead {
            subsystem: name.clone(),
        });
    }
    if row.mandatory_proofs.is_empty() {
        gaps.push(CoverageGap::NoMandatoryProof {
            subsystem: name.clone(),
        });
    }
    if row.proof_artifacts.is_empty() {
        gaps.push(CoverageGap::NoProofArtifact {
            subsystem: name.clone(),
        });
    }
    if row.log_expectation.trim().is_empty() {
        gaps.push(CoverageGap::NoLoggingExpectation {
            subsystem: name.clone(),
        });
    }
    if row.fixture_provenance.trim().is_empty() {
        gaps.push(CoverageGap::NoFixtureProvenance {
            subsystem: name.clone(),
        });
    }
    if row.closure_evidence.trim().is_empty() {
        gaps.push(CoverageGap::NoClosureEvidence {
            subsystem: name.clone(),
        });
    }
    gaps
}

/// All structural gaps across the matrix: every report subsystem must have
/// exactly one row, no row may name a non-report subsystem, and every row must
/// be structurally complete. This is the pure half of the closeout gate (no I/O).
pub fn matrix_gaps() -> Vec<CoverageGap> {
    let rows = subsystem_coverage_matrix();
    let mut gaps = Vec::new();

    // Every report subsystem file is represented exactly once.
    for canonical in REPORT_SUBSYSTEM_FILES.iter() {
        let count = rows.iter().filter(|r| r.subsystem == *canonical).count();
        if count == 0 {
            gaps.push(CoverageGap::Unmapped {
                subsystem: (*canonical).to_string(),
            });
        } else if count > 1 {
            gaps.push(CoverageGap::Duplicate {
                subsystem: (*canonical).to_string(),
            });
        }
    }

    // Each row is structurally complete (and names a real report subsystem).
    for row in rows.iter() {
        gaps.extend(row_gaps(row));
    }

    gaps
}

/// Proof artifacts that fail the on-disk existence check, given a caller-supplied
/// existence predicate. A cited artifact that does not exist is the "only prose
/// evidence" failure. Kept pure (predicate-injected) so it is testable without a
/// real filesystem; the `#[cfg(test)]` gate passes the real one.
pub fn missing_artifacts(
    row: &SubsystemCoverage,
    exists: impl Fn(&str) -> bool,
) -> Vec<CoverageGap> {
    row.proof_artifacts
        .iter()
        .filter(|artifact| !exists(artifact))
        .map(|artifact| CoverageGap::MissingArtifact {
            subsystem: row.subsystem.to_string(),
            artifact: (*artifact).to_string(),
        })
        .collect()
}

/// Whether the matrix passes the pure (structural) closeout gate.
pub fn matrix_is_complete() -> bool {
    matrix_gaps().is_empty()
}

/// Owned, serializable view of the whole matrix for robot output.
#[derive(Debug, Clone, Serialize)]
pub struct MatrixReport {
    pub schema_version: u32,
    pub subsystem_count: usize,
    pub complete: bool,
    pub subsystems: Vec<SubsystemCoverage>,
}

/// Build the robot-readable matrix report (consumed by the `.11.5` integrated gate).
pub fn matrix_report() -> MatrixReport {
    let subsystems = subsystem_coverage_matrix();
    MatrixReport {
        schema_version: SUBSYSTEM_COVERAGE_SCHEMA_VERSION,
        subsystem_count: subsystems.len(),
        complete: matrix_gaps().is_empty(),
        subsystems,
    }
}

/// Render one subsystem row as a markdown block (flat helper: keeps `format!`
/// out of the loop in [`render_markdown`]).
fn render_block(row: &SubsystemCoverage) -> String {
    let proofs = row
        .mandatory_proofs
        .iter()
        .map(|p| p.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let artifacts = row
        .proof_artifacts
        .iter()
        .map(|a| format!("`{a}`"))
        .collect::<Vec<_>>()
        .join(", ");
    let optional = if row.optional_diagnostics.is_empty() {
        "—".to_string()
    } else {
        row.optional_diagnostics.join(", ")
    };
    format!(
        "### {subsystem}\n\n\
         - **Owning beads:** {beads}\n\
         - **Failure modes:** {fms}\n\
         - **Mandatory proofs:** {proofs}\n\
         - **Proof artifacts:** {artifacts}\n\
         - **Optional diagnostics:** {optional}\n\
         - **Fixture provenance:** {provenance}\n\
         - **Log expectation:** {logs}\n\
         - **Redaction:** {redaction}\n\
         - **Closure evidence:** {closure}\n",
        subsystem = row.subsystem,
        beads = row.owning_beads.join(", "),
        fms = row.failure_modes.join(", "),
        proofs = proofs,
        artifacts = artifacts,
        optional = optional,
        provenance = row.fixture_provenance,
        logs = row.log_expectation,
        redaction = row.redaction.as_str(),
        closure = row.closure_evidence,
    )
}

/// Render the durable human-readable matrix that mirrors this module's data.
/// Pinned as `docs/SUBSYSTEM_COVERAGE_MATRIX.md` by the golden test below so the
/// doc can never drift from the executable matrix.
pub fn render_markdown() -> String {
    let blocks = subsystem_coverage_matrix()
        .iter()
        .map(render_block)
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "# Subsystem Coverage Matrix & Closeout Gate\n\
         \n\
         Bead: `coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.15.5`.\n\
         \n\
         > Generated from `src/subsystem_coverage_matrix.rs`. Do not edit by hand —\n\
         > run `UPDATE_GOLDENS=1 cargo test --lib subsystem_coverage_matrix` to\n\
         > regenerate after changing the matrix, and review the diff.\n\
         \n\
         This is the **executable** counterpart of\n\
         [`RESILIENCE_TEST_MATRIX.md`](RESILIENCE_TEST_MATRIX.md) (bead `.12.1`):\n\
         the same coverage encoded as data that a test checks against the real\n\
         repository. The closeout gate fails when any of the {count} report\n\
         subsystem failure-mode files has no owning bead, no mandatory proof\n\
         level, no proof artifact, no logging expectation, or cites a proof\n\
         artifact that does not exist on disk (the \"only prose evidence\"\n\
         failure). The integrated resilience gate (bead `.11.5`) consumes\n\
         `subsystem_coverage_matrix::matrix_gaps()` so subsystem coverage cannot\n\
         be silently skipped during closeout.\n\
         \n\
         ## Proof levels\n\
         \n\
         | Level | Meaning |\n\
         |-------|---------|\n\
         | `unit` | In-crate `#[cfg(test)]` over pure logic; no I/O. |\n\
         | `integration` | Real types across modules / `tests/` with isolated data dirs. |\n\
         | `golden` | A pinned artifact a change must deliberately update. |\n\
         | `e2e` | A bounded run of the real `cass` binary. |\n\
         | `logs` | A structured proof-log/manifest distinguishing pass from timeout. |\n\
         \n\
         ## Redaction expectations\n\
         \n\
         | Value | Meaning |\n\
         |-------|---------|\n\
         | `no-user-content` | Diagnostics are structured counts/kinds/paths only. |\n\
         | `local-only` | Content stays local; only kinds/counts may be shared. |\n\
         | `redact-required` | User content present; redact before any sharing/export. |\n\
         \n\
         ## Subsystems\n\
         \n\
         {blocks}\
         \n\
         ---\n\
         \n\
         Precursor docs (asserted to exist by the gate): {docs}.\n",
        count = REPORT_SUBSYSTEM_FILES.len(),
        blocks = blocks,
        docs = PRECURSOR_DOCS
            .iter()
            .map(|d| format!("`{d}`"))
            .collect::<Vec<_>>()
            .join(", "),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn repo_root() -> &'static Path {
        Path::new(env!("CARGO_MANIFEST_DIR"))
    }

    // --- the matrix covers every report subsystem, exactly once ------------

    #[test]
    fn every_report_subsystem_is_covered_exactly_once() {
        let rows = subsystem_coverage_matrix();
        assert_eq!(
            rows.len(),
            REPORT_SUBSYSTEM_FILES.len(),
            "matrix must have one row per report subsystem file"
        );
        for canonical in REPORT_SUBSYSTEM_FILES.iter() {
            let count = rows.iter().filter(|r| r.subsystem == *canonical).count();
            assert_eq!(count, 1, "{canonical} must appear exactly once");
        }
    }

    #[test]
    fn matrix_row_order_matches_canonical_order() {
        let rows = subsystem_coverage_matrix();
        let names: Vec<&str> = rows.iter().map(|r| r.subsystem).collect();
        assert_eq!(
            names,
            REPORT_SUBSYSTEM_FILES.to_vec(),
            "matrix order must match canonical (alphabetical) order"
        );
    }

    #[test]
    fn canonical_subsystem_names_are_sorted_and_unique() {
        let mut sorted = REPORT_SUBSYSTEM_FILES.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted,
            REPORT_SUBSYSTEM_FILES.to_vec(),
            "REPORT_SUBSYSTEM_FILES must be sorted and unique"
        );
    }

    // --- the pure (structural) closeout gate passes ------------------------

    #[test]
    fn structural_closeout_gate_passes_with_no_gaps() {
        let gaps = matrix_gaps();
        assert!(
            gaps.is_empty(),
            "subsystem coverage gate must be gap-free, found: {gaps:?}"
        );
        assert!(matrix_is_complete());
    }

    #[test]
    fn every_row_is_structurally_complete() {
        for row in subsystem_coverage_matrix().iter() {
            let gaps = row_gaps(row);
            assert!(
                gaps.is_empty(),
                "{} has structural gaps: {gaps:?}",
                row.subsystem
            );
            // Every row must keep its failure-mode list non-empty too: the matrix
            // exists to keep the report's findings owned.
            assert!(
                !row.failure_modes.is_empty(),
                "{} must list at least one fm-* finding",
                row.subsystem
            );
        }
    }

    // --- the on-disk gate: no prose-only evidence --------------------------

    #[test]
    fn every_cited_proof_artifact_exists_on_disk() {
        let root = repo_root();
        let exists = |p: &str| root.join(p).exists();
        let mut missing = Vec::new();
        for row in subsystem_coverage_matrix().iter() {
            missing.extend(missing_artifacts(row, &exists));
        }
        assert!(
            missing.is_empty(),
            "matrix cites proof artifacts that do not exist (prose-only evidence): {missing:?}"
        );
    }

    #[test]
    fn precursor_docs_exist() {
        let root = repo_root();
        for doc in PRECURSOR_DOCS.iter() {
            assert!(
                root.join(doc).exists(),
                "precursor doc {doc} must exist on disk"
            );
        }
    }

    // --- negative tests: each gap rule actually fires ----------------------

    fn complete_row() -> SubsystemCoverage {
        SubsystemCoverage {
            subsystem: "analytics",
            owning_beads: &[".15.4"],
            failure_modes: &["fm-analytics-charts-saturating-zero"],
            mandatory_proofs: &[ProofLevel::Unit],
            proof_artifacts: &["src/metric_integrity.rs"],
            optional_diagnostics: &[],
            fixture_provenance: "synthetic rows",
            log_expectation: "deterministic --lib result line",
            redaction: RedactionExpectation::NoUserContent,
            closure_evidence: "cargo test --lib metric_integrity",
        }
    }

    #[test]
    fn a_complete_row_has_no_gaps() {
        assert!(row_gaps(&complete_row()).is_empty());
    }

    #[test]
    fn unknown_subsystem_name_is_a_gap() {
        let mut row = complete_row();
        row.subsystem = "not_a_report_file";
        let gaps = row_gaps(&row);
        assert!(gaps.iter().any(|g| matches!(
            g,
            CoverageGap::UnknownSubsystem { subsystem } if subsystem == "not_a_report_file"
        )));
    }

    #[test]
    fn missing_owner_proof_logs_provenance_closure_are_each_gaps() {
        let mut no_owner = complete_row();
        no_owner.owning_beads = &[];
        assert!(
            row_gaps(&no_owner)
                .iter()
                .any(|g| matches!(g, CoverageGap::NoOwningBead { .. }))
        );

        let mut no_proof = complete_row();
        no_proof.mandatory_proofs = &[];
        assert!(
            row_gaps(&no_proof)
                .iter()
                .any(|g| matches!(g, CoverageGap::NoMandatoryProof { .. }))
        );

        let mut no_artifact = complete_row();
        no_artifact.proof_artifacts = &[];
        assert!(
            row_gaps(&no_artifact)
                .iter()
                .any(|g| matches!(g, CoverageGap::NoProofArtifact { .. }))
        );

        let mut no_logs = complete_row();
        no_logs.log_expectation = "   ";
        assert!(
            row_gaps(&no_logs)
                .iter()
                .any(|g| matches!(g, CoverageGap::NoLoggingExpectation { .. }))
        );

        let mut no_provenance = complete_row();
        no_provenance.fixture_provenance = "";
        assert!(
            row_gaps(&no_provenance)
                .iter()
                .any(|g| matches!(g, CoverageGap::NoFixtureProvenance { .. }))
        );

        let mut no_closure = complete_row();
        no_closure.closure_evidence = "";
        assert!(
            row_gaps(&no_closure)
                .iter()
                .any(|g| matches!(g, CoverageGap::NoClosureEvidence { .. }))
        );
    }

    #[test]
    fn a_phantom_artifact_is_caught_by_the_disk_gate() {
        let mut row = complete_row();
        row.proof_artifacts = &["src/this_file_does_not_exist_zzz.rs"];
        // Predicate that says nothing exists: every artifact is "prose only".
        let missing = missing_artifacts(&row, |_| false);
        assert_eq!(missing.len(), 1);
        assert!(matches!(
            &missing[0],
            CoverageGap::MissingArtifact { artifact, .. }
            if artifact == "src/this_file_does_not_exist_zzz.rs"
        ));
        // And with a predicate that says everything exists, there is no gap.
        assert!(missing_artifacts(&row, |_| true).is_empty());
    }

    // --- robot-readable report + stable serialization ----------------------

    #[test]
    fn matrix_report_is_complete_and_serializes() {
        let report = matrix_report();
        assert_eq!(report.subsystem_count, REPORT_SUBSYSTEM_FILES.len());
        assert!(report.complete);
        assert_eq!(report.schema_version, SUBSYSTEM_COVERAGE_SCHEMA_VERSION);
        let json = serde_json::to_value(&report).expect("matrix report serializes");
        assert_eq!(json["subsystem_count"], REPORT_SUBSYSTEM_FILES.len());
        assert_eq!(json["subsystems"][0]["subsystem"], "analytics");
        // Proof levels serialize as their kebab labels.
        assert_eq!(json["subsystems"][0]["mandatory_proofs"][0], "unit");
    }

    #[test]
    fn proof_level_and_redaction_labels_are_stable_kebab() {
        assert_eq!(ProofLevel::E2e.as_str(), "e2e");
        assert_eq!(ProofLevel::Logs.as_str(), "logs");
        assert_eq!(
            RedactionExpectation::NoUserContent.as_str(),
            "no-user-content"
        );
        assert_eq!(
            serde_json::to_string(&RedactionExpectation::RedactRequired).expect("ser"),
            "\"redact-required\""
        );
    }

    #[test]
    fn coverage_gap_serializes_with_tagged_kebab_kind() {
        let gap = CoverageGap::NoOwningBead {
            subsystem: "analytics".to_string(),
        };
        let json = serde_json::to_value(&gap).expect("gap serializes");
        assert_eq!(json["gap"], "no-owning-bead");
        assert_eq!(json["subsystem"], "analytics");
    }

    // --- the durable doc is a golden of the executable matrix ---------------

    #[test]
    fn rendered_markdown_matches_committed_doc() {
        let doc_path = repo_root().join("docs/SUBSYSTEM_COVERAGE_MATRIX.md");
        let rendered = render_markdown();
        if std::env::var("UPDATE_GOLDENS").is_ok() {
            std::fs::write(&doc_path, &rendered).expect("write subsystem coverage doc");
            return;
        }
        let on_disk = std::fs::read_to_string(&doc_path).expect(
            "docs/SUBSYSTEM_COVERAGE_MATRIX.md missing; \
             regenerate with UPDATE_GOLDENS=1 cargo test --lib subsystem_coverage_matrix",
        );
        assert_eq!(
            on_disk, rendered,
            "docs/SUBSYSTEM_COVERAGE_MATRIX.md is stale; \
             regenerate with UPDATE_GOLDENS=1 cargo test --lib subsystem_coverage_matrix"
        );
    }

    #[test]
    fn rendered_markdown_names_every_subsystem() {
        let rendered = render_markdown();
        for name in REPORT_SUBSYSTEM_FILES.iter() {
            assert!(
                rendered.contains(&format!("### {name}")),
                "rendered matrix must have a section for {name}"
            );
        }
    }
}
