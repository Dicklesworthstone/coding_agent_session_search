//! Provider-specific connector-ingest diagnostic taxonomy.
//!
//! Bead: coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.15.1
//! ("Harden connector ingest diagnostics and per-provider failure fixtures").
//!
//! Connectors are the front door for 20 local agents, so a connector failure
//! becomes lost, merged, duplicated, or misleading search history. The report
//! named concrete cases: an Aider external-id collision that merges distinct
//! project histories, malformed JSONL that could panic/abort a whole file, a
//! Cursor `state.vscdb` locked by the live app, ChatGPT encrypted stores that
//! are undecipherable on Linux/headless hosts, and Amp filename-prefix
//! assumptions that break on non-ASCII / future schemes.
//!
//! The failure these share is *silence*: a provider-specific limitation gets
//! turned into a fake zero-message import, or two unrelated conversations get
//! merged under one id. This module makes the failure **explicit,
//! provider-specific, retry-aware, and testable**: an [`IngestFailureKind`]
//! classifies what went wrong, [`classify`] turns it into a
//! [`ConnectorIngestDiagnostic`] carrying provider/source/provenance/line/id
//! context plus severity, retryability, the per-source
//! [`SourceIngestDisposition`], and a safe next action, and
//! [`ProviderIngestSummary`] distinguishes discovered / skipped / quarantined /
//! partially-indexed / locked / unsupported-encrypted / successfully-indexed
//! sources instead of collapsing them into one count.
//!
//! It also carries the one concrete behavioural fix that is pure logic:
//! [`disambiguated_external_id`] folds workspace/source identity into a
//! connector external id so two distinct project histories can never collide
//! and merge.
//!
//! The classification helpers remain pure. [`ConnectorIngestRun`] wires them to
//! live discovery and parsing, applies the Aider id hardening before persistence,
//! and records privacy-safe hashes for malformed lines while the surrounding
//! valid records continue through normal ingest.

use std::collections::BTreeMap;
use std::io::BufRead;
use std::path::{Path, PathBuf};

use crate::franken_sync::compat::{ConnectionExt, OpenFlags, open_with_flags};
use serde::{Deserialize, Serialize};

use crate::connectors::{
    DiscoveredSourceFile, DiscoveredSourceRole, NormalizedConversation, ScanContext, ScanRoot,
};

/// Stable schema version for the connector-ingest diagnostic wire format.
pub const CONNECTOR_INGEST_DIAGNOSTIC_SCHEMA_VERSION: u32 = 1;

/// How serious a single ingest finding is. Ordered low→high so a per-source or
/// per-run rollup can take the worst with `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IngestSeverity {
    /// Informational; ingestion proceeded normally.
    Info,
    /// A limitation or a quarantined record; the rest of the source still
    /// indexed. Not a hard failure.
    Warning,
    /// The source could not be ingested at all (and it is not merely an absent
    /// or encrypted-but-known limitation).
    Error,
}

impl IngestSeverity {
    /// Stable kebab-case wire value.
    pub const fn as_str(self) -> &'static str {
        match self {
            IngestSeverity::Info => "info",
            IngestSeverity::Warning => "warning",
            IngestSeverity::Error => "error",
        }
    }
}

/// What specifically went wrong while ingesting a connector source. Each kind is
/// the report's concrete connector failure mode, generalised across providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IngestFailureKind {
    /// A connector external id would collide across distinct workspaces/sources
    /// and merge unrelated histories (the Aider case) unless disambiguated.
    ExternalIdCollisionRisk,
    /// A single JSONL line/record was malformed; valid records around it are
    /// preserved and this one is quarantined (never a whole-file abort/panic).
    MalformedJsonLine,
    /// A session file was truncated mid-stream; records up to the truncation are
    /// preserved.
    TruncatedSession,
    /// The source is locked by the live app (Cursor `state.vscdb`); retryable.
    SourceLocked,
    /// The source is encrypted and cannot be decrypted on this host
    /// (ChatGPT v2/v3 on Linux/headless); legacy unencrypted data still indexes.
    EncryptedUnsupported,
    /// A required keychain/credential to decrypt is unavailable (headless host).
    KeychainUnavailable,
    /// A filename/path assumption did not hold (Amp prefix scheme, non-ASCII, or
    /// a future-looking name); the source is skipped rather than mis-parsed.
    FilenameAssumptionViolated,
    /// The source path exists but could not be read (permissions / I/O).
    UnreadableSource,
    /// The connector failed on content it could not interpret (an unsupported
    /// record shape or value); no I/O error caused it, so permissions and
    /// retries do not help (GH #511).
    UnparseableSource,
    /// The source is larger than the connector's per-source read budget, so it
    /// was not indexed; raising the budget admits it on the next run.
    SourceOverReadBudget,
    /// The store uses a layout or format version this build's connector does
    /// not read (a newer Pi durable schema, a Pi Session Store V2 sidecar).
    /// It was not indexed from that store, and the store is left untouched.
    UnsupportedStoreFormat,
}

impl IngestFailureKind {
    /// Stable kebab-case wire value.
    pub const fn as_str(self) -> &'static str {
        match self {
            IngestFailureKind::ExternalIdCollisionRisk => "external-id-collision-risk",
            IngestFailureKind::MalformedJsonLine => "malformed-json-line",
            IngestFailureKind::TruncatedSession => "truncated-session",
            IngestFailureKind::SourceLocked => "source-locked",
            IngestFailureKind::EncryptedUnsupported => "encrypted-unsupported",
            IngestFailureKind::KeychainUnavailable => "keychain-unavailable",
            IngestFailureKind::FilenameAssumptionViolated => "filename-assumption-violated",
            IngestFailureKind::UnreadableSource => "unreadable-source",
            IngestFailureKind::UnparseableSource => "unparseable-source",
            IngestFailureKind::SourceOverReadBudget => "source-over-read-budget",
            IngestFailureKind::UnsupportedStoreFormat => "unsupported-store-format",
        }
    }
}

/// The per-source outcome an ingest run reports, so a limitation is never
/// silently rendered as "indexed 0 messages". Ordered roughly best→worst.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceIngestDisposition {
    /// Fully ingested; every record indexed.
    Indexed,
    /// Discovered but not yet processed (enumerated, pending).
    Discovered,
    /// Some records indexed, others quarantined/truncated.
    PartiallyIndexed,
    /// One or more records quarantined; the source itself is otherwise indexed.
    Quarantined,
    /// Skipped (filename/path assumption violated, or deliberately excluded).
    Skipped,
    /// Locked by the live app; retry later (no data indexed this run).
    Locked,
    /// Encrypted and unsupported on this host (a known, explicit limitation —
    /// never a fake zero-message success).
    UnsupportedEncrypted,
}

impl SourceIngestDisposition {
    /// Stable kebab-case wire value.
    pub const fn as_str(self) -> &'static str {
        match self {
            SourceIngestDisposition::Indexed => "indexed",
            SourceIngestDisposition::Discovered => "discovered",
            SourceIngestDisposition::PartiallyIndexed => "partially-indexed",
            SourceIngestDisposition::Quarantined => "quarantined",
            SourceIngestDisposition::Skipped => "skipped",
            SourceIngestDisposition::Locked => "locked",
            SourceIngestDisposition::UnsupportedEncrypted => "unsupported-encrypted",
        }
    }

    /// Whether this disposition reflects an explicit, non-silent limitation
    /// (locked / unsupported-encrypted / skipped) — the thing that must never be
    /// reported as a successful zero-message import.
    pub const fn is_explicit_limitation(self) -> bool {
        matches!(
            self,
            SourceIngestDisposition::Locked
                | SourceIngestDisposition::UnsupportedEncrypted
                | SourceIngestDisposition::Skipped
        )
    }
}

/// A single connector-ingest diagnostic. Stable snake_case JSON with an embedded
/// [`schema_version`](Self::schema_version).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorIngestDiagnostic {
    /// Mirrors [`CONNECTOR_INGEST_DIAGNOSTIC_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// The provider/connector (e.g. `"aider"`, `"cursor"`, `"chatgpt"`).
    pub provider: String,
    /// The source path or root the finding came from.
    pub source_path: String,
    /// Workspace / source provenance identity, when known (distinguishes the
    /// same base id across projects/machines).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// Line/row identifier when the finding is record-scoped (e.g. a malformed
    /// JSONL line number).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_or_row: Option<u64>,
    /// The connector external id in context, when relevant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
    /// The canonical (post-disambiguation) id, when computed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical_id: Option<String>,
    /// What went wrong.
    pub failure_kind: IngestFailureKind,
    /// The connector's own explanation, when it gives one (e.g. the format
    /// version it found and the one it reads). Never message content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// How serious it is.
    pub severity: IngestSeverity,
    /// Whether retrying later may succeed (e.g. a transient lock).
    pub retryable: bool,
    /// The per-source disposition this finding implies.
    pub disposition: SourceIngestDisposition,
    /// A single, safe next action for an operator/agent. Always read-only or a
    /// safe re-run; never a destructive cleanup.
    pub safe_next_action: String,
}

/// The classification of a failure kind for a provider: severity, retryability,
/// the disposition it implies, and the safe next action. Pure.
fn classify_kind(
    provider: &str,
    kind: IngestFailureKind,
) -> (IngestSeverity, bool, SourceIngestDisposition, String) {
    match kind {
        IngestFailureKind::ExternalIdCollisionRisk => (
            IngestSeverity::Warning,
            false,
            SourceIngestDisposition::Indexed,
            format!(
                "{provider} external ids are disambiguated by workspace/source identity so distinct \
                 project histories cannot merge; verify the canonical id includes provenance"
            ),
        ),
        IngestFailureKind::MalformedJsonLine => (
            IngestSeverity::Warning,
            false,
            SourceIngestDisposition::PartiallyIndexed,
            "the malformed line is quarantined and the surrounding valid records are indexed; \
             inspect the quarantined line — the whole file is never aborted"
                .to_string(),
        ),
        IngestFailureKind::TruncatedSession => (
            IngestSeverity::Warning,
            false,
            SourceIngestDisposition::PartiallyIndexed,
            "the session was truncated; records up to the truncation are indexed — re-export the \
             session if the tail is needed"
                .to_string(),
        ),
        IngestFailureKind::SourceLocked => (
            IngestSeverity::Warning,
            true, // retryable: the lock is transient
            SourceIngestDisposition::Locked,
            format!(
                "{provider} source is locked by the live app (e.g. an open Cursor); this is \
                 reported as lock-busy, NOT a zero-message import — retry after closing the app or \
                 on the next sync"
            ),
        ),
        IngestFailureKind::EncryptedUnsupported => (
            IngestSeverity::Warning,
            false,
            SourceIngestDisposition::UnsupportedEncrypted,
            format!(
                "{provider} store is encrypted and unsupported on this host; legacy unencrypted \
                 conversations still index — this is an explicit limitation, never a fake success"
            ),
        ),
        IngestFailureKind::KeychainUnavailable => (
            IngestSeverity::Warning,
            true, // a keychain may become available on a non-headless run
            SourceIngestDisposition::UnsupportedEncrypted,
            format!(
                "{provider} decryption needs a keychain/credential unavailable on this headless \
                 host; index from an interactive session where the keychain is unlocked"
            ),
        ),
        IngestFailureKind::FilenameAssumptionViolated => (
            IngestSeverity::Warning,
            false,
            SourceIngestDisposition::Skipped,
            format!(
                "{provider} filename did not match the expected scheme (non-ASCII or a future \
                 naming variant); skipped rather than mis-parsed — file a fixture for the variant"
            ),
        ),
        IngestFailureKind::UnreadableSource => (
            IngestSeverity::Error,
            true, // permissions/IO may be fixed and retried
            SourceIngestDisposition::Skipped,
            format!("{provider} source path could not be read; check permissions, then re-index"),
        ),
        IngestFailureKind::UnparseableSource => (
            IngestSeverity::Error,
            false, // the same bytes fail the same way
            SourceIngestDisposition::Skipped,
            format!(
                "{provider} could not interpret a source's content (not a permissions or I/O \
                 problem); indexing_stats.connectors[].error names the record. Report its shape \
                 so the connector can parse it, then re-index"
            ),
        ),
        IngestFailureKind::SourceOverReadBudget => (
            IngestSeverity::Error,
            true, // a larger budget admits it on the next run
            SourceIngestDisposition::Skipped,
            format!(
                "{provider} session file is larger than the per-source read budget and was not \
                 indexed; raise the budget (Codex: CASS_CODEX_MAX_SOURCE_BYTES, up to 1073741824 \
                 bytes), then re-index"
            ),
        ),
        IngestFailureKind::UnsupportedStoreFormat => (
            IngestSeverity::Warning,
            false, // the same build reads the same store the same way
            SourceIngestDisposition::Skipped,
            format!(
                "{provider} found a store in a format or location this cass build does not \
                 read; nothing was indexed from that store and it was not modified. detail names \
                 the format and what admits it (a cass release whose connector reads it, or \
                 indexing on the store's own host), then re-index"
            ),
        ),
    }
}

/// Build a full [`ConnectorIngestDiagnostic`] for a provider failure. Pure.
pub fn classify(
    provider: &str,
    source_path: &str,
    kind: IngestFailureKind,
) -> ConnectorIngestDiagnostic {
    let (severity, retryable, disposition, safe_next_action) = classify_kind(provider, kind);
    ConnectorIngestDiagnostic {
        schema_version: CONNECTOR_INGEST_DIAGNOSTIC_SCHEMA_VERSION,
        provider: provider.to_string(),
        source_path: source_path.to_string(),
        workspace: None,
        line_or_row: None,
        external_id: None,
        canonical_id: None,
        failure_kind: kind,
        detail: None,
        severity,
        retryable,
        disposition,
        safe_next_action,
    }
}

fn classify_path(
    provider: &str,
    source_path: &Path,
    kind: IngestFailureKind,
) -> ConnectorIngestDiagnostic {
    classify(provider, source_path.to_string_lossy().as_ref(), kind)
}

/// Recognize backend error messages, not arbitrary occurrences of lock words
/// in source paths or parser context. Typed causes take precedence below.
fn is_database_lock_message(message: &str) -> bool {
    let lower = message.trim().to_ascii_lowercase();
    [
        "database is locked",
        "database is busy",
        "database table is locked",
        "database schema is locked",
        "sqlite_busy",
        "sqlite_busy_recovery",
        "sqlite_busy_snapshot",
        "sqlite_busy_timeout",
        "sqlite_locked",
        "sqlite_locked_sharedcache",
        "sqlite_locked_vtab",
    ]
    .iter()
    .any(|&prefix| {
        lower.strip_prefix(prefix).is_some_and(|suffix| {
            let suffix = suffix.trim_start();
            suffix.is_empty() || suffix.starts_with(':') || suffix.starts_with('(')
        })
    })
}

fn connector_scan_failure_kind(error: &anyhow::Error) -> IngestFailureKind {
    use crate::franken_sync::FrankenError;

    if let Some(io) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<std::io::Error>())
    {
        return if matches!(
            io.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::ResourceBusy
        ) {
            IngestFailureKind::SourceLocked
        } else {
            IngestFailureKind::UnreadableSource
        };
    }
    // A parser may quote source content that itself names a lock. Its type
    // proves this is still a content failure, regardless of that message.
    if error
        .chain()
        .any(|cause| cause.downcast_ref::<serde_json::Error>().is_some())
    {
        return IngestFailureKind::UnparseableSource;
    }
    let database_locked = error.chain().any(|cause| {
        cause.downcast_ref::<FrankenError>().is_some_and(|error| {
            matches!(
                error,
                FrankenError::Busy
                    | FrankenError::BusyRecovery
                    | FrankenError::BusySnapshot { .. }
                    | FrankenError::DatabaseLocked { .. }
                    | FrankenError::LockFailed { .. }
                    | FrankenError::WriteConflict { .. }
                    | FrankenError::SerializationFailure { .. }
            )
        })
    });
    if database_locked || is_database_lock_message(&error.root_cause().to_string()) {
        IngestFailureKind::SourceLocked
    } else {
        IngestFailureKind::UnparseableSource
    }
}

impl ConnectorIngestDiagnostic {
    /// Attach workspace/source provenance.
    pub fn with_workspace(mut self, workspace: impl Into<String>) -> Self {
        self.workspace = Some(workspace.into());
        self
    }

    /// Attach a record line/row identifier.
    pub fn with_line(mut self, line: u64) -> Self {
        self.line_or_row = Some(line);
        self
    }

    /// Attach the external id and its disambiguated canonical id.
    pub fn with_ids(
        mut self,
        external_id: impl Into<String>,
        canonical_id: impl Into<String>,
    ) -> Self {
        self.external_id = Some(external_id.into());
        self.canonical_id = Some(canonical_id.into());
        self
    }

    /// Attach the connector's own explanation.
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

/// Fold workspace/source identity into a connector external id so that two
/// distinct project histories sharing the same base id can never collide and
/// merge (the Aider external-id collision). Deterministic: the same
/// `(base_id, workspace)` always yields the same canonical id, and different
/// workspaces always diverge. An empty workspace returns the base id unchanged
/// (nothing to disambiguate by).
pub fn disambiguated_external_id(base_id: &str, workspace: &str) -> String {
    if workspace.is_empty() {
        return base_id.to_string();
    }
    let digest = blake3::hash(workspace.as_bytes());
    let short: String = digest.to_hex().to_string().chars().take(12).collect();
    format!("{base_id}@{short}")
}

/// A per-provider rollup of source dispositions for a run. Replaces a single
/// "indexed N" count with the full breakdown so a locked/encrypted source is
/// never invisible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ProviderIngestSummary {
    pub discovered: u64,
    pub indexed: u64,
    pub partially_indexed: u64,
    pub quarantined: u64,
    pub skipped: u64,
    pub locked: u64,
    pub unsupported_encrypted: u64,
}

impl ProviderIngestSummary {
    /// Record one source's disposition.
    pub fn record(&mut self, disposition: SourceIngestDisposition) {
        match disposition {
            SourceIngestDisposition::Discovered => self.discovered += 1,
            SourceIngestDisposition::Indexed => self.indexed += 1,
            SourceIngestDisposition::PartiallyIndexed => self.partially_indexed += 1,
            SourceIngestDisposition::Quarantined => self.quarantined += 1,
            SourceIngestDisposition::Skipped => self.skipped += 1,
            SourceIngestDisposition::Locked => self.locked += 1,
            SourceIngestDisposition::UnsupportedEncrypted => self.unsupported_encrypted += 1,
        }
    }

    /// Total sources accounted for.
    pub fn total(&self) -> u64 {
        self.discovered
            + self.indexed
            + self.partially_indexed
            + self.quarantined
            + self.skipped
            + self.locked
            + self.unsupported_encrypted
    }

    /// Sources that produced at least some searchable content (indexed or
    /// partially indexed). Used to assert a run is not a silent total miss.
    pub fn with_content(&self) -> u64 {
        self.indexed + self.partially_indexed
    }

    /// Merge another scan scope for the same provider into this run rollup.
    pub fn merge(&mut self, other: Self) {
        self.discovered = self.discovered.saturating_add(other.discovered);
        self.indexed = self.indexed.saturating_add(other.indexed);
        self.partially_indexed = self
            .partially_indexed
            .saturating_add(other.partially_indexed);
        self.quarantined = self.quarantined.saturating_add(other.quarantined);
        self.skipped = self.skipped.saturating_add(other.skipped);
        self.locked = self.locked.saturating_add(other.locked);
        self.unsupported_encrypted = self
            .unsupported_encrypted
            .saturating_add(other.unsupported_encrypted);
    }
}

/// One completed provider scan scope, ready to merge into `IndexingStats`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ConnectorIngestReport {
    pub provider: String,
    pub summary: ProviderIngestSummary,
    pub diagnostics: Vec<ConnectorIngestDiagnostic>,
}

#[derive(Debug, Clone, Copy)]
struct ObservedSource {
    disposition: SourceIngestDisposition,
    malformed: bool,
}

/// Mutable observation of one connector scan scope. It consumes the same
/// discovery results used by raw-mirror capture, avoiding a second filesystem
/// walk, and contains no conversation payloads.
pub struct ConnectorIngestRun {
    provider: String,
    sources: BTreeMap<PathBuf, ObservedSource>,
    diagnostics: Vec<ConnectorIngestDiagnostic>,
    /// The scan context's data dir, for the single-store contexts below.
    scan_data_dir: PathBuf,
    /// Each discovered Pi durable store as a single-store scan root carrying
    /// its discovery provenance. [`Self::finish`] asks the connector why a
    /// store that yielded no conversation was not read.
    durable_stores: Vec<ScanRoot>,
}

impl ConnectorIngestRun {
    /// Inspect discovered sources for failure modes that upstream connectors
    /// otherwise log-and-skip: malformed JSONL, locked Cursor databases,
    /// encrypted ChatGPT directories unavailable to this scan, and Pi stores
    /// in formats the Pi connector does not read.
    pub fn begin(
        provider: &str,
        data_dir: &Path,
        ctx: &ScanContext,
        sources: &[DiscoveredSourceFile],
    ) -> Self {
        let durable_stores = if provider == "pi_durable" {
            sources
                .iter()
                .filter(|source| {
                    matches!(
                        source.role,
                        DiscoveredSourceRole::PrimarySessionLog
                            | DiscoveredSourceRole::SqliteDatabase
                    )
                })
                .map(|source| {
                    let mut root = ScanRoot::local(source.source_path.clone());
                    root.origin = source.origin.clone();
                    root.platform = source.platform;
                    root
                })
                .collect()
        } else {
            Vec::new()
        };
        let mut run = Self {
            provider: provider.to_string(),
            sources: sources
                .iter()
                .map(|source| {
                    (
                        source.source_path.clone(),
                        ObservedSource {
                            disposition: SourceIngestDisposition::Discovered,
                            malformed: false,
                        },
                    )
                })
                .collect(),
            diagnostics: Vec::new(),
            scan_data_dir: ctx.data_dir.clone(),
            durable_stores,
        };
        for source in sources {
            run.inspect_jsonl(data_dir, &source.source_path);
            if provider == "cursor" {
                run.inspect_cursor_database(&source.source_path);
            }
        }
        if provider == "chatgpt" {
            run.inspect_chatgpt_encrypted_directories(ctx, sources);
        }
        if provider == "pi_agent" {
            run.inspect_unsupported_pi_stores(ctx);
        }
        run
    }

    /// Sources the scan skipped because the source ledger shows them
    /// unchanged since they were indexed. They count as indexed, and
    /// [`Self::finish`] does not re-read them.
    pub fn observe_reused_sources(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        for path in paths {
            // Self-contained connectors skip the redundant discovery
            // inventory, so a reused source may not be listed yet.
            let source = self.sources.entry(path).or_insert(ObservedSource {
                disposition: SourceIngestDisposition::Discovered,
                malformed: false,
            });
            if source.disposition == SourceIngestDisposition::Discovered {
                source.disposition = SourceIngestDisposition::Indexed;
            }
        }
    }

    /// Record and harden one successfully parsed conversation.
    pub fn observe_conversation(&mut self, conversation: &mut NormalizedConversation) {
        let state = self
            .sources
            .entry(conversation.source_path.clone())
            .or_insert(ObservedSource {
                disposition: SourceIngestDisposition::Discovered,
                malformed: false,
            });
        state.disposition = if state.malformed {
            SourceIngestDisposition::PartiallyIndexed
        } else {
            SourceIngestDisposition::Indexed
        };

        if self.provider == "aider"
            && let (Some(base_id), Some(workspace)) = (
                conversation.external_id.clone(),
                conversation
                    .workspace
                    .as_deref()
                    .or_else(|| conversation.source_path.parent()),
            )
        {
            let workspace = workspace.display().to_string();
            let canonical_id = disambiguated_external_id(&base_id, &workspace);
            conversation.external_id = Some(canonical_id.clone());
            self.diagnostics.push(
                classify(
                    "aider",
                    &conversation.source_path.display().to_string(),
                    IngestFailureKind::ExternalIdCollisionRisk,
                )
                .with_workspace(workspace)
                .with_ids(base_id, canonical_id),
            );
        }
    }

    /// Attach a scan-level error without converting it into a fake success.
    pub fn observe_scan_error(&mut self, source_path: &Path, error: impl std::fmt::Display) {
        let kind = if is_database_lock_message(&error.to_string()) {
            IngestFailureKind::SourceLocked
        } else {
            IngestFailureKind::UnreadableSource
        };
        let diagnostic = classify_path(&self.provider, source_path, kind);
        self.sources.insert(
            source_path.to_path_buf(),
            ObservedSource {
                disposition: diagnostic.disposition,
                malformed: false,
            },
        );
        self.diagnostics.push(diagnostic);
    }

    /// Record a failed connector scan. A Codex scan that stopped only because
    /// rollouts exceeded the per-source read budget is reported per rejected
    /// file, with the setting that admits it; it used to surface as one
    /// "unreadable, check permissions" entry naming the cass data directory.
    pub fn observe_connector_scan_error(&mut self, fallback_path: &Path, error: &anyhow::Error) {
        if let Some(over) = crate::connectors::codex::over_budget_sources(error) {
            for path in &over.sampled_paths {
                let diagnostic = classify_path(
                    &self.provider,
                    path,
                    IngestFailureKind::SourceOverReadBudget,
                );
                self.sources.insert(
                    path.clone(),
                    ObservedSource {
                        disposition: diagnostic.disposition,
                        malformed: false,
                    },
                );
                self.diagnostics.push(diagnostic);
            }
            return;
        }
        // Codebuff retains bounded typed failures for independently scanned
        // inputs. Classify EACH cause at its recorded path, not the first cause
        // at a fallback root. Never parse paths or copy payloads from Display.
        if self.provider == "codebuff"
            && crate::connectors::codebuff::CodebuffConnector::for_each_source_failure(
                error,
                |path, cause| {
                    let mut diagnostic =
                        classify_path(&self.provider, path, connector_scan_failure_kind(cause));
                    let state = self
                        .sources
                        .entry(path.to_path_buf())
                        .or_insert(ObservedSource {
                            disposition: SourceIngestDisposition::Discovered,
                            malformed: false,
                        });
                    // An observation can fail after delivery. Do not erase
                    // content that this run already observed for that source.
                    if matches!(
                        state.disposition,
                        SourceIngestDisposition::Indexed
                            | SourceIngestDisposition::PartiallyIndexed
                    ) {
                        diagnostic.disposition = SourceIngestDisposition::PartiallyIndexed;
                    }
                    state.disposition = diagnostic.disposition;
                    self.diagnostics.push(diagnostic);
                },
            )
        {
            return;
        }
        // GH #511: FAD's top message includes the failing path. Classify the
        // cause, so a project called "busy" cannot turn a parse or permission
        // failure into a retryable database lock.
        let diagnostic = classify_path(
            &self.provider,
            fallback_path,
            connector_scan_failure_kind(error),
        );
        self.sources.insert(
            fallback_path.to_path_buf(),
            ObservedSource {
                disposition: diagnostic.disposition,
                malformed: false,
            },
        );
        self.diagnostics.push(diagnostic);
    }

    /// Close the scope. Each Pi durable store that yielded no conversation
    /// and was not reused is asked once why, so an unsupported schema or an
    /// unreadable store is reported rather than counted as an empty source.
    /// Stores that yielded conversations are not read again.
    #[must_use]
    pub fn finish(mut self) -> ConnectorIngestReport {
        self.inspect_unproductive_durable_stores();
        let mut summary = ProviderIngestSummary::default();
        for source in self.sources.values() {
            summary.record(source.disposition);
        }
        ConnectorIngestReport {
            provider: self.provider,
            summary,
            diagnostics: self.diagnostics,
        }
    }

    fn inspect_unproductive_durable_stores(&mut self) {
        let connector = franken_agent_detection::PiDurableConnector::new();
        for root in std::mem::take(&mut self.durable_stores) {
            let unproductive = self
                .sources
                .get(&root.path)
                .is_some_and(|source| source.disposition == SourceIngestDisposition::Discovered);
            if !unproductive {
                continue;
            }
            let ctx = ScanContext::with_roots(self.scan_data_dir.clone(), vec![root], None);
            for found in connector.store_diagnostics(&ctx) {
                let kind = match found.code {
                    "unsupported_version"
                    | "sqlite_support_not_compiled"
                    | "remote_sqlite_unsupported" => IngestFailureKind::UnsupportedStoreFormat,
                    "unreadable" => IngestFailureKind::UnreadableSource,
                    // A file that is not a durable store, or a note on a store
                    // that was read; inspect_jsonl reports torn and malformed
                    // commit lines itself.
                    _ => continue,
                };
                let diagnostic = classify_path(&self.provider, &found.path, kind)
                    .with_detail(format!("{}: {}", found.code, found.detail));
                if let Some(source) = self.sources.get_mut(&found.path) {
                    source.disposition = diagnostic.disposition;
                }
                self.diagnostics.push(diagnostic);
            }
        }
    }

    fn inspect_unsupported_pi_stores(&mut self, ctx: &ScanContext) {
        for store in
            crate::connectors::pi_agent::PiAgentConnector::new().unsupported_store_diagnostics(ctx)
        {
            let detail = match store.store_format {
                "session_store_v2" => {
                    "session_store_v2: Pi Session Store V2 sidecars are not read; a JSONL \
                     transcript beside one is indexed and may lag it"
                }
                "sqlite_sessions" => {
                    "sqlite_sessions: Pi SQLite sessions under a remote root are not read, \
                     because a synced database and its WAL are not one snapshot; index them on \
                     their own host"
                }
                other => other,
            };
            let diagnostic = classify_path(
                &self.provider,
                &store.path,
                IngestFailureKind::UnsupportedStoreFormat,
            )
            .with_detail(detail);
            self.sources.insert(
                store.path,
                ObservedSource {
                    disposition: diagnostic.disposition,
                    malformed: false,
                },
            );
            self.diagnostics.push(diagnostic);
        }
    }

    fn inspect_jsonl(&mut self, data_dir: &Path, path: &Path) {
        let is_jsonl = path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"));
        if !is_jsonl {
            return;
        }
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(error) => {
                self.observe_scan_error(path, error.to_string());
                return;
            }
        };
        let mut reader = std::io::BufReader::new(file);
        let mut raw_line = Vec::new();
        let mut line_number = 0u64;
        let source_path = path.display().to_string();
        loop {
            raw_line.clear();
            let bytes_read = match reader.read_until(b'\n', &mut raw_line) {
                Ok(bytes_read) => bytes_read,
                Err(error) => {
                    self.observe_scan_error(path, error);
                    break;
                }
            };
            if bytes_read == 0 {
                break;
            }
            line_number = line_number.saturating_add(1);
            let had_newline = raw_line.last() == Some(&b'\n');
            if had_newline {
                raw_line.pop();
                if raw_line.last() == Some(&b'\r') {
                    raw_line.pop();
                }
            }
            let line = raw_line.as_slice();
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            if serde_json::from_slice::<serde_json::Value>(line).is_ok() {
                continue;
            }
            let kind = if !had_newline {
                IngestFailureKind::TruncatedSession
            } else {
                IngestFailureKind::MalformedJsonLine
            };
            self.diagnostics
                .push(classify(&self.provider, &source_path, kind).with_line(line_number));
            self.sources.entry(path.to_path_buf()).and_modify(|source| {
                source.disposition = SourceIngestDisposition::Quarantined;
                source.malformed = true;
            });
            if let Err(error) = crate::indexer::quarantine::record_connector_line(
                data_dir,
                &self.provider,
                path,
                line_number,
                line,
                kind.as_str(),
            ) {
                tracing::warn!(
                    provider = %self.provider,
                    source_path = %path.display(),
                    line_number,
                    error = %error,
                    "failed to persist connector line quarantine record"
                );
            }
        }
    }

    fn inspect_cursor_database(&mut self, path: &Path) {
        if path.file_name().and_then(|name| name.to_str()) != Some("state.vscdb") {
            return;
        }
        // Avoid doubling the cost of every cold Cursor DB open. A live Cursor
        // writer uses SQLite sidecars; only those potentially contended stores
        // need the explicit lock probe before the connector's normal read.
        let wal_path = path.with_file_name("state.vscdb-wal");
        let journal_path = path.with_file_name("state.vscdb-journal");
        if !wal_path.exists() && !journal_path.exists() {
            return;
        }
        let probe = open_with_flags(
            path.to_string_lossy().as_ref(),
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .and_then(|connection| {
            connection.query_row_map("SELECT 1 FROM sqlite_master LIMIT 1", &[], |_row| Ok(()))
        });
        if let Err(error) = probe {
            let error = anyhow::Error::new(error);
            if connector_scan_failure_kind(&error) == IngestFailureKind::SourceLocked {
                self.observe_connector_scan_error(path, &error);
            }
        }
    }

    fn inspect_chatgpt_encrypted_directories(
        &mut self,
        ctx: &ScanContext,
        sources: &[DiscoveredSourceFile],
    ) {
        let mut pending = if ctx.scan_roots.is_empty() {
            vec![(ctx.data_dir.clone(), 0usize)]
        } else {
            ctx.scan_roots
                .iter()
                .map(|root| (root.path.clone(), 0usize))
                .collect()
        };
        while let Some((path, depth)) = pending.pop() {
            let encrypted = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.starts_with("conversations-v2-") || name.starts_with("conversations-v3-")
                });
            if encrypted {
                let key_was_available = sources
                    .iter()
                    .any(|source| source.source_path.starts_with(&path));
                if !key_was_available {
                    let kind = if cfg!(target_os = "macos") {
                        IngestFailureKind::KeychainUnavailable
                    } else {
                        IngestFailureKind::EncryptedUnsupported
                    };
                    let diagnostic = classify_path("chatgpt", &path, kind);
                    self.sources.insert(
                        path,
                        ObservedSource {
                            disposition: diagnostic.disposition,
                            malformed: false,
                        },
                    );
                    self.diagnostics.push(diagnostic);
                }
                continue;
            }
            if depth >= 5 {
                continue;
            }
            if let Ok(entries) = std::fs::read_dir(&path) {
                pending.extend(
                    entries
                        .flatten()
                        .filter(|entry| entry.path().is_dir())
                        .map(|entry| (entry.path(), depth + 1)),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::Connector;
    use serde_json::json;
    use tempfile::tempdir;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    macro_rules! verify {
        ($condition:expr $(,)?) => {
            if !$condition {
                return Err(format!("verification failed: {}", stringify!($condition)).into());
            }
        };
        ($condition:expr, $($arg:tt)+) => {
            if !$condition {
                return Err(format!($($arg)+).into());
            }
        };
    }

    macro_rules! verify_eq {
        ($left:expr, $right:expr $(,)?) => {{
            let left = &$left;
            let right = &$right;
            if !std::cmp::PartialEq::eq(left, right) {
                return Err(format!(
                    "verification failed: {} equals {}; left={left:?}, right={right:?}",
                    stringify!($left),
                    stringify!($right),
                )
                .into());
            }
        }};
        ($left:expr, $right:expr, $($arg:tt)+) => {{
            let left = &$left;
            let right = &$right;
            if !std::cmp::PartialEq::eq(left, right) {
                return Err(format!($($arg)+).into());
            }
        }};
    }

    macro_rules! verify_ne {
        ($left:expr, $right:expr $(,)?) => {{
            let left = &$left;
            let right = &$right;
            if std::cmp::PartialEq::eq(left, right) {
                return Err(format!(
                    "verification failed: {} differs from {}; both={left:?}",
                    stringify!($left),
                    stringify!($right),
                )
                .into());
            }
        }};
        ($left:expr, $right:expr, $($arg:tt)+) => {{
            let left = &$left;
            let right = &$right;
            if std::cmp::PartialEq::eq(left, right) {
                return Err(format!($($arg)+).into());
            }
        }};
    }

    // --- per-provider classification --------------------------------------

    #[test]
    fn cursor_lock_is_retryable_lock_busy_not_a_zero_import() -> TestResult {
        let d = classify(
            "cursor",
            "~/Library/.../state.vscdb",
            IngestFailureKind::SourceLocked,
        );
        verify_eq!(d.disposition, SourceIngestDisposition::Locked);
        verify!(d.retryable, "a live-app lock is transient and retryable");
        verify!(d.disposition.is_explicit_limitation());
        verify!(
            d.safe_next_action.contains("NOT a zero-message import"),
            "a lock must never read as a successful empty import: {}",
            d.safe_next_action
        );
        Ok(())
    }

    #[test]
    fn chatgpt_encrypted_is_unsupported_not_silent_and_legacy_still_indexes() -> TestResult {
        let d = classify(
            "chatgpt",
            "~/.../com.openai.chat",
            IngestFailureKind::EncryptedUnsupported,
        );
        verify_eq!(d.disposition, SourceIngestDisposition::UnsupportedEncrypted);
        verify!(!d.retryable);
        verify!(d.disposition.is_explicit_limitation());
        verify!(d.safe_next_action.contains("legacy unencrypted"));
        verify!(d.safe_next_action.contains("never a fake success"));
        Ok(())
    }

    #[test]
    fn keychain_unavailable_is_retryable_from_interactive_session() -> TestResult {
        let d = classify(
            "chatgpt",
            "~/.../com.openai.chat",
            IngestFailureKind::KeychainUnavailable,
        );
        verify_eq!(d.disposition, SourceIngestDisposition::UnsupportedEncrypted);
        verify!(d.retryable);
        verify!(d.safe_next_action.contains("headless"));
        Ok(())
    }

    #[test]
    fn malformed_json_line_quarantines_the_line_and_preserves_the_rest() -> TestResult {
        let d = classify(
            "codex",
            "rollout-x.jsonl",
            IngestFailureKind::MalformedJsonLine,
        )
        .with_line(42);
        verify_eq!(d.disposition, SourceIngestDisposition::PartiallyIndexed);
        verify_eq!(d.line_or_row, Some(42));
        verify!(
            d.safe_next_action.contains("never aborted"),
            "a malformed line must not abort the whole file"
        );
        verify_eq!(d.severity, IngestSeverity::Warning);
        Ok(())
    }

    #[test]
    fn truncated_session_indexes_up_to_truncation() -> TestResult {
        let d = classify(
            "claude_code",
            "session.jsonl",
            IngestFailureKind::TruncatedSession,
        );
        verify_eq!(d.disposition, SourceIngestDisposition::PartiallyIndexed);
        Ok(())
    }

    #[test]
    fn amp_filename_assumption_violation_is_skipped_not_misparsed() -> TestResult {
        let d = classify(
            "amp",
            "weird-名前.json",
            IngestFailureKind::FilenameAssumptionViolated,
        );
        verify_eq!(d.disposition, SourceIngestDisposition::Skipped);
        verify!(d.disposition.is_explicit_limitation());
        verify!(d.safe_next_action.contains("non-ASCII"));
        Ok(())
    }

    #[test]
    fn unreadable_source_is_an_error_but_retryable() -> TestResult {
        let d = classify(
            "gemini",
            "~/.gemini/tmp",
            IngestFailureKind::UnreadableSource,
        );
        verify_eq!(d.severity, IngestSeverity::Error);
        verify_eq!(d.disposition, SourceIngestDisposition::Skipped);
        verify!(d.retryable);
        Ok(())
    }

    #[test]
    fn every_failure_kind_has_a_nonempty_safe_action_and_no_destructive_op() -> TestResult {
        let kinds = [
            IngestFailureKind::ExternalIdCollisionRisk,
            IngestFailureKind::MalformedJsonLine,
            IngestFailureKind::TruncatedSession,
            IngestFailureKind::SourceLocked,
            IngestFailureKind::EncryptedUnsupported,
            IngestFailureKind::KeychainUnavailable,
            IngestFailureKind::FilenameAssumptionViolated,
            IngestFailureKind::UnreadableSource,
            IngestFailureKind::UnparseableSource,
            IngestFailureKind::SourceOverReadBudget,
            IngestFailureKind::UnsupportedStoreFormat,
        ];
        let destructive = ["rm ", "--delete", "--purge", "reset", "drop ", "rm -rf"];
        for kind in kinds {
            let d = classify("aider", "/some/path", kind);
            verify!(
                !d.safe_next_action.is_empty(),
                "{} has no action",
                kind.as_str()
            );
            let lower = d.safe_next_action.to_ascii_lowercase();
            for bad in destructive {
                verify!(
                    !lower.contains(bad),
                    "{} safe action references a destructive op {bad:?}: {}",
                    kind.as_str(),
                    d.safe_next_action
                );
            }
        }
        Ok(())
    }

    // --- Aider external-id disambiguation ---------------------------------

    #[test]
    fn distinct_workspaces_never_collide_on_the_same_base_id() -> TestResult {
        let a = disambiguated_external_id("aider-history", "/home/me/project-a");
        let b = disambiguated_external_id("aider-history", "/home/me/project-b");
        verify_ne!(
            a,
            b,
            "distinct project workspaces must not merge under one id"
        );
        verify!(a.starts_with("aider-history@"));
        Ok(())
    }

    #[test]
    fn same_workspace_and_base_id_is_stable() -> TestResult {
        let a = disambiguated_external_id("aider-history", "/home/me/project-a");
        let b = disambiguated_external_id("aider-history", "/home/me/project-a");
        verify_eq!(a, b, "disambiguation must be deterministic");
        Ok(())
    }

    #[test]
    fn empty_workspace_returns_base_id_unchanged() -> TestResult {
        verify_eq!(disambiguated_external_id("base", ""), "base");
        Ok(())
    }

    #[test]
    fn diagnostic_can_carry_disambiguated_ids() -> TestResult {
        let canonical = disambiguated_external_id("h", "/ws/proj");
        let d = classify(
            "aider",
            "/ws/proj/.aider.chat.history.md",
            IngestFailureKind::ExternalIdCollisionRisk,
        )
        .with_workspace("/ws/proj")
        .with_ids("h", canonical.clone());
        verify_eq!(d.external_id.as_deref(), Some("h"));
        verify_eq!(d.canonical_id.as_deref(), Some(canonical.as_str()));
        verify_eq!(d.workspace.as_deref(), Some("/ws/proj"));
        Ok(())
    }

    // --- provider summary -------------------------------------------------

    #[test]
    fn summary_distinguishes_every_disposition() -> TestResult {
        let mut s = ProviderIngestSummary::default();
        for disp in [
            SourceIngestDisposition::Discovered,
            SourceIngestDisposition::Indexed,
            SourceIngestDisposition::Indexed,
            SourceIngestDisposition::PartiallyIndexed,
            SourceIngestDisposition::Quarantined,
            SourceIngestDisposition::Skipped,
            SourceIngestDisposition::Locked,
            SourceIngestDisposition::UnsupportedEncrypted,
        ] {
            s.record(disp);
        }
        verify_eq!(s.discovered, 1);
        verify_eq!(s.indexed, 2);
        verify_eq!(s.partially_indexed, 1);
        verify_eq!(s.quarantined, 1);
        verify_eq!(s.skipped, 1);
        verify_eq!(s.locked, 1);
        verify_eq!(s.unsupported_encrypted, 1);
        verify_eq!(s.total(), 8);
        verify_eq!(s.with_content(), 3); // indexed + partially-indexed
        Ok(())
    }

    #[test]
    fn a_locked_or_encrypted_only_run_has_no_content_but_is_not_silent() -> TestResult {
        // The anti-pattern guard: a run where every source is locked/encrypted
        // must report zero content AND surface the explicit limitations.
        let mut s = ProviderIngestSummary::default();
        s.record(SourceIngestDisposition::Locked);
        s.record(SourceIngestDisposition::UnsupportedEncrypted);
        verify_eq!(s.with_content(), 0, "no searchable content");
        verify!(
            s.total() > 0,
            "but the sources are accounted for, not silently dropped"
        );
        verify!(SourceIngestDisposition::Locked.is_explicit_limitation());
        verify!(SourceIngestDisposition::UnsupportedEncrypted.is_explicit_limitation());
        Ok(())
    }

    // --- serialization stability ------------------------------------------

    #[test]
    fn diagnostic_serializes_with_stable_fields_and_round_trips() -> TestResult {
        let d = classify("cursor", "/p/state.vscdb", IngestFailureKind::SourceLocked)
            .with_workspace("/p");
        let value = serde_json::to_value(&d)?;
        verify_eq!(
            value["schema_version"],
            CONNECTOR_INGEST_DIAGNOSTIC_SCHEMA_VERSION
        );
        verify_eq!(value["provider"], "cursor");
        verify_eq!(value["failure_kind"], "source-locked");
        verify_eq!(value["severity"], "warning");
        verify_eq!(value["disposition"], "locked");
        verify_eq!(value["retryable"], true);
        let back: ConnectorIngestDiagnostic = serde_json::from_value(value)?;
        verify_eq!(back, d);
        Ok(())
    }

    #[test]
    fn summary_serializes_with_stable_snake_case_keys() -> TestResult {
        let mut s = ProviderIngestSummary::default();
        s.record(SourceIngestDisposition::UnsupportedEncrypted);
        let value = serde_json::to_value(s)?;
        verify_eq!(value["unsupported_encrypted"], 1);
        verify_eq!(value["indexed"], 0);
        Ok(())
    }

    #[test]
    fn wire_labels_are_stable_kebab() -> TestResult {
        let failure_kinds = [
            IngestFailureKind::ExternalIdCollisionRisk,
            IngestFailureKind::MalformedJsonLine,
            IngestFailureKind::SourceLocked,
            IngestFailureKind::EncryptedUnsupported,
            IngestFailureKind::KeychainUnavailable,
            IngestFailureKind::FilenameAssumptionViolated,
            IngestFailureKind::UnreadableSource,
            IngestFailureKind::UnparseableSource,
            IngestFailureKind::TruncatedSession,
            IngestFailureKind::SourceOverReadBudget,
        ];
        verify_eq!(
            serde_json::to_value(failure_kinds)?,
            json!([
                "external-id-collision-risk",
                "malformed-json-line",
                "source-locked",
                "encrypted-unsupported",
                "keychain-unavailable",
                "filename-assumption-violated",
                "unreadable-source",
                "unparseable-source",
                "truncated-session",
                "source-over-read-budget"
            ])
        );
        for (k, w) in [
            (
                IngestFailureKind::ExternalIdCollisionRisk,
                "external-id-collision-risk",
            ),
            (IngestFailureKind::MalformedJsonLine, "malformed-json-line"),
            (IngestFailureKind::SourceLocked, "source-locked"),
            (
                IngestFailureKind::EncryptedUnsupported,
                "encrypted-unsupported",
            ),
            (
                IngestFailureKind::KeychainUnavailable,
                "keychain-unavailable",
            ),
            (
                IngestFailureKind::FilenameAssumptionViolated,
                "filename-assumption-violated",
            ),
            (IngestFailureKind::UnreadableSource, "unreadable-source"),
            (IngestFailureKind::UnparseableSource, "unparseable-source"),
            (IngestFailureKind::TruncatedSession, "truncated-session"),
            (
                IngestFailureKind::SourceOverReadBudget,
                "source-over-read-budget",
            ),
        ] {
            verify_eq!(k.as_str(), w);
        }
        let dispositions = [
            SourceIngestDisposition::Indexed,
            SourceIngestDisposition::PartiallyIndexed,
            SourceIngestDisposition::UnsupportedEncrypted,
            SourceIngestDisposition::Locked,
        ];
        verify_eq!(
            serde_json::to_value(dispositions)?,
            json!([
                "indexed",
                "partially-indexed",
                "unsupported-encrypted",
                "locked"
            ])
        );
        for (d, w) in [
            (SourceIngestDisposition::Indexed, "indexed"),
            (
                SourceIngestDisposition::PartiallyIndexed,
                "partially-indexed",
            ),
            (
                SourceIngestDisposition::UnsupportedEncrypted,
                "unsupported-encrypted",
            ),
            (SourceIngestDisposition::Locked, "locked"),
        ] {
            verify_eq!(d.as_str(), w);
        }
        verify_eq!(IngestSeverity::Warning.as_str(), "warning");
        Ok(())
    }

    #[test]
    fn severity_orders_for_a_max_rollup() -> TestResult {
        verify!(IngestSeverity::Info < IngestSeverity::Warning);
        verify!(IngestSeverity::Warning < IngestSeverity::Error);
        let worst = [
            IngestSeverity::Info,
            IngestSeverity::Error,
            IngestSeverity::Warning,
        ]
        .into_iter()
        .max()
        .ok_or_else(|| std::io::Error::other("severity sample must be non-empty"))?;
        verify_eq!(worst, IngestSeverity::Error);
        Ok(())
    }

    #[test]
    fn live_run_quarantines_only_bad_jsonl_line_and_indexes_valid_content() -> TestResult {
        let temp = tempdir()?;
        let path = temp.path().join("mixed.jsonl");
        std::fs::write(&path, b"{\"ok\":1}\n{broken}\n{\"ok\":2}\n")?;
        let root = ScanRoot::local(temp.path().to_path_buf());
        let source = DiscoveredSourceFile::new(
            "codex",
            &root,
            path.clone(),
            DiscoveredSourceRole::PrimarySessionLog,
            true,
        );
        let ctx = ScanContext::with_roots(temp.path().to_path_buf(), vec![root], None);
        let mut run = ConnectorIngestRun::begin("codex", temp.path(), &ctx, &[source]);
        let mut conversation = NormalizedConversation {
            agent_slug: "codex".into(),
            external_id: Some("mixed".into()),
            title: None,
            workspace: None,
            source_path: path,
            started_at: None,
            ended_at: None,
            metadata: json!({}),
            messages: Vec::new(),
        };
        run.observe_conversation(&mut conversation);
        let report = run.finish();
        verify_eq!(report.summary.partially_indexed, 1);
        verify_eq!(report.diagnostics.len(), 1);
        verify_eq!(report.diagnostics[0].line_or_row, Some(2));
        verify_eq!(
            report.diagnostics[0].failure_kind,
            IngestFailureKind::MalformedJsonLine
        );
        let quarantine =
            std::fs::read_to_string(temp.path().join("quarantine/connector_ingest_lines.json"))?;
        verify!(!quarantine.contains("{broken}"), "payload must never leak");
        verify!(quarantine.contains("payload_blake3"));
        Ok(())
    }

    #[test]
    fn live_aider_run_disambiguates_same_external_id_by_workspace() -> TestResult {
        fn conversation(workspace: &str) -> NormalizedConversation {
            NormalizedConversation {
                agent_slug: "aider".into(),
                external_id: Some("history".into()),
                title: None,
                workspace: Some(PathBuf::from(workspace)),
                source_path: PathBuf::from(workspace).join(".aider.chat.history.md"),
                started_at: None,
                ended_at: None,
                metadata: json!({}),
                messages: Vec::new(),
            }
        }
        let ctx = ScanContext::local_default(PathBuf::from("/tmp"), None);
        let mut first = ConnectorIngestRun::begin("aider", Path::new("/tmp"), &ctx, &[]);
        let mut second = ConnectorIngestRun::begin("aider", Path::new("/tmp"), &ctx, &[]);
        let mut a = conversation("/workspace/alpha");
        let mut b = conversation("/workspace/beta");
        first.observe_conversation(&mut a);
        second.observe_conversation(&mut b);
        verify_ne!(a.external_id, b.external_id);
        verify_eq!(first.finish().diagnostics.len(), 1);
        verify_eq!(second.finish().summary.indexed, 1);
        Ok(())
    }

    #[test]
    fn live_chatgpt_run_reports_encrypted_but_keeps_legacy_source() -> TestResult {
        let temp = tempdir()?;
        let legacy_dir = temp.path().join("conversations-legacy");
        let encrypted_dir = temp.path().join("conversations-v3-secret");
        std::fs::create_dir_all(&legacy_dir)?;
        std::fs::create_dir_all(&encrypted_dir)?;
        let legacy = legacy_dir.join("conversation.json");
        std::fs::write(&legacy, "{}")?;
        let root = ScanRoot::local(temp.path().to_path_buf());
        let source = DiscoveredSourceFile::new(
            "chatgpt",
            &root,
            legacy,
            DiscoveredSourceRole::PrimarySessionLog,
            true,
        );
        let ctx = ScanContext::with_roots(temp.path().to_path_buf(), vec![root], None);
        let report = ConnectorIngestRun::begin("chatgpt", temp.path(), &ctx, &[source]).finish();
        verify_eq!(report.summary.discovered, 1);
        verify_eq!(report.summary.unsupported_encrypted, 1);
        let expected_kind = if cfg!(target_os = "macos") {
            IngestFailureKind::KeychainUnavailable
        } else {
            IngestFailureKind::EncryptedUnsupported
        };
        let expected_kind_seen = report.diagnostics.iter().any(|diagnostic| {
            // ubs:ignore — compares a public diagnostic enum, never secret material.
            diagnostic.failure_kind == expected_kind
        });
        verify!(expected_kind_seen);
        Ok(())
    }

    #[test]
    fn amp_unicode_future_filename_is_scanned_without_byte_slicing_panic() -> TestResult {
        let temp = tempdir()?;
        let threads = temp.path().join("threads");
        std::fs::create_dir_all(&threads)?;
        std::fs::write(
            threads.join("未来-thread-名前.json"),
            r#"{"id":"future-unicode","messages":[{"role":"user","content":"searchable"}]}"#,
        )?;
        let connector = crate::connectors::amp::AmpConnector::new();
        let ctx = ScanContext::with_roots(threads.clone(), vec![ScanRoot::local(threads)], None);
        let conversations = connector.scan(&ctx)?;
        verify_eq!(conversations.len(), 1);
        verify_eq!(conversations[0].messages[0].content, "searchable");
        Ok(())
    }

    #[test]
    fn live_cursor_busy_error_is_retryable_and_not_zero_message_success() -> TestResult {
        let ctx = ScanContext::local_default(PathBuf::from("/tmp"), None);
        let mut run = ConnectorIngestRun::begin("cursor", Path::new("/tmp"), &ctx, &[]);
        run.observe_scan_error(
            Path::new("/cursor/state.vscdb"),
            "database is locked (SQLITE_BUSY)",
        );
        let report = run.finish();
        verify_eq!(report.summary.locked, 1);
        verify_eq!(report.summary.with_content(), 0);
        verify!(report.diagnostics[0].retryable);
        verify_eq!(
            report.diagnostics[0].failure_kind,
            IngestFailureKind::SourceLocked
        );
        Ok(())
    }

    #[test]
    fn indexing_stats_robot_shape_exposes_summary_and_diagnostics() -> TestResult {
        let value = serde_json::to_value(crate::indexer::IndexingStats::default())?;
        verify_eq!(value["connector_summary"], json!({}));
        verify_eq!(value["connector_diagnostics"], json!([]));
        Ok(())
    }

    /// A Codex scan that stopped only because rollouts exceeded the read budget
    /// is diagnosed per rejected rollout with the setting that admits it, even
    /// through added context; it used to read as one "unreadable, check
    /// permissions" entry naming the cass data directory. An I/O failure keeps
    /// the unreadable entry.
    #[test]
    fn codex_over_budget_scan_error_is_diagnosed_per_rollout_not_as_the_data_dir() -> TestResult {
        let error = crate::connectors::codex::over_budget_scan_error(&[(
            "/codex/sessions/rollout-big.jsonl",
            200 * 1024 * 1024,
        )])
        .context("scanning codex");
        let ctx = ScanContext::local_default(PathBuf::from("/data"), None);

        let mut run = ConnectorIngestRun::begin("codex", Path::new("/data"), &ctx, &[]);
        run.observe_connector_scan_error(Path::new("/data"), &error);
        let report = run.finish();
        verify_eq!(report.diagnostics.len(), 1);
        let diagnostic = &report.diagnostics[0];
        verify_eq!(
            diagnostic.failure_kind,
            IngestFailureKind::SourceOverReadBudget
        );
        verify_eq!(diagnostic.source_path, "/codex/sessions/rollout-big.jsonl");
        verify!(
            diagnostic
                .safe_next_action
                .contains("CASS_CODEX_MAX_SOURCE_BYTES")
        );
        verify_eq!(report.summary.skipped, 1);

        let mut run = ConnectorIngestRun::begin("codex", Path::new("/data"), &ctx, &[]);
        let disk_gone = anyhow::Error::new(std::io::Error::other("disk went away"));
        run.observe_connector_scan_error(Path::new("/data"), &disk_gone);
        let report = run.finish();
        verify_eq!(report.diagnostics.len(), 1);
        verify_eq!(
            report.diagnostics[0].failure_kind,
            IngestFailureKind::UnreadableSource
        );
        verify_eq!(report.diagnostics[0].source_path, "/data");
        Ok(())
    }

    /// GH #511: a connector that read a source but rejected a record value
    /// ("invalid shared CLI timestamp at record 0", with context and no I/O
    /// cause) is unparseable, not unreadable: no permissions advice, not
    /// retryable on the same bytes, at the root it scanned. An I/O cause still
    /// reads as unreadable and a lock as locked.
    #[test]
    fn content_failures_are_unparseable_and_io_failures_stay_unreadable() -> TestResult {
        let ctx = ScanContext::local_default(PathBuf::from("/data"), None);
        let root = Path::new("/home/me/.config/manicode");
        let parse_error = anyhow::anyhow!("premature end of input")
            .context("invalid shared CLI timestamp at record 0");

        let mut run = ConnectorIngestRun::begin("codebuff", Path::new("/data"), &ctx, &[]);
        run.observe_connector_scan_error(root, &parse_error);
        let report = run.finish();
        verify_eq!(report.diagnostics.len(), 1);
        let diagnostic = &report.diagnostics[0];
        verify_eq!(
            diagnostic.failure_kind,
            IngestFailureKind::UnparseableSource
        );
        verify_eq!(diagnostic.source_path, root.display().to_string());
        verify!(!diagnostic.retryable);
        verify_eq!(diagnostic.disposition, SourceIngestDisposition::Skipped);
        verify!(!diagnostic.safe_next_action.contains("check permissions"));
        verify!(
            diagnostic
                .safe_next_action
                .contains("indexing_stats.connectors[].error")
        );
        verify_eq!(report.summary.skipped, 1);

        let denied = anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "permission denied",
        ))
        .context("reading chat-messages.json");
        let mut run = ConnectorIngestRun::begin("codebuff", Path::new("/data"), &ctx, &[]);
        run.observe_connector_scan_error(root, &denied);
        let report = run.finish();
        verify_eq!(
            report.diagnostics[0].failure_kind,
            IngestFailureKind::UnreadableSource
        );
        verify!(report.diagnostics[0].retryable);

        let locked = anyhow::anyhow!("database is locked").context("scanning state.vscdb");
        let mut run = ConnectorIngestRun::begin("cursor", Path::new("/data"), &ctx, &[]);
        run.observe_connector_scan_error(Path::new("/cursor"), &locked);
        let report = run.finish();
        verify_eq!(
            report.diagnostics[0].failure_kind,
            IngestFailureKind::SourceLocked
        );
        Ok(())
    }

    /// FAD 0.3.5 includes the failing transcript path in its top message.
    /// Project names are not evidence of a database lock, even when a host
    /// has flattened the contextual error into one display string.
    #[test]
    fn gh511_project_names_cannot_turn_parse_failures_into_locks() -> TestResult {
        let ctx = ScanContext::local_default(PathBuf::from("/data"), None);
        for project in ["a-busy-project", "unlocked-project"] {
            let transcript = PathBuf::from("/home/me/.config/manicode/projects")
                .join(project)
                .join("chats/2026-03-21T17-14-03.768Z/chat-messages.json");
            let parse_error =
                serde_json::from_str::<serde_json::Value>(r#"[{"id":"user-1774113351457""#)
                    .expect_err("truncated native transcript must fail JSON parsing");
            let error = anyhow::Error::new(parse_error)
                .context("invalid Codebuff / Freebuff transcript JSON");
            let summary = format!("{}: {error:#}", transcript.display());
            let error = error.context(summary.clone());
            for error in [error, anyhow::anyhow!(summary)] {
                let mut run = ConnectorIngestRun::begin("codebuff", Path::new("/data"), &ctx, &[]);
                run.observe_connector_scan_error(&transcript, &error);
                let report = run.finish();
                verify_eq!(report.diagnostics.len(), 1);
                let diagnostic = &report.diagnostics[0];
                verify_eq!(
                    diagnostic.failure_kind,
                    IngestFailureKind::UnparseableSource
                );
                verify_eq!(diagnostic.source_path, transcript.display().to_string());
                verify!(!diagnostic.retryable);
                verify_eq!(report.summary.skipped, 1);
                verify_eq!(report.summary.locked, 0);
            }
        }
        Ok(())
    }

    #[test]
    fn gh511_permission_errors_under_busy_paths_stay_unreadable() -> TestResult {
        let ctx = ScanContext::local_default(PathBuf::from("/data"), None);
        let transcript = Path::new(
            "/home/me/.config/manicode/projects/a-busy-project/chats/unlocked/chat-messages.json",
        );
        let error = anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "permission denied",
        ));
        let summary = format!("{}: {error:#}", transcript.display());
        let error = error.context(summary);
        let mut run = ConnectorIngestRun::begin("codebuff", Path::new("/data"), &ctx, &[]);
        run.observe_connector_scan_error(transcript, &error);
        let report = run.finish();
        verify_eq!(
            report.diagnostics[0].failure_kind,
            IngestFailureKind::UnreadableSource
        );
        verify!(report.diagnostics[0].retryable);
        verify_eq!(report.summary.skipped, 1);
        verify_eq!(report.summary.locked, 0);
        Ok(())
    }

    #[test]
    fn gh511_json_error_content_cannot_claim_a_database_lock() -> TestResult {
        let ctx = ScanContext::local_default(PathBuf::from("/data"), None);
        let error = <serde_json::Error as serde::de::Error>::custom("database is locked");
        let error =
            anyhow::Error::new(error).context("invalid Codebuff / Freebuff transcript JSON");
        let mut run = ConnectorIngestRun::begin("codebuff", Path::new("/data"), &ctx, &[]);
        run.observe_connector_scan_error(Path::new("/codebuff/chat-messages.json"), &error);
        let report = run.finish();
        verify_eq!(
            report.diagnostics[0].failure_kind,
            IngestFailureKind::UnparseableSource
        );
        verify!(!report.diagnostics[0].retryable);
        verify_eq!(report.summary.locked, 0);
        Ok(())
    }

    #[test]
    fn gh511_database_lock_causes_remain_retryable() -> TestResult {
        use crate::franken_sync::FrankenError;

        let ctx = ScanContext::local_default(PathBuf::from("/data"), None);
        let path = Path::new("/cursor/state.vscdb");
        for error in [
            anyhow::Error::new(FrankenError::Busy),
            anyhow::Error::new(FrankenError::BusyRecovery),
            anyhow::Error::new(FrankenError::BusySnapshot {
                conflicting_pages: "12".to_string(),
            }),
            anyhow::Error::new(FrankenError::DatabaseLocked {
                path: path.to_path_buf(),
            }),
            anyhow::Error::new(FrankenError::LockFailed {
                detail: "F_SETLK contention".to_string(),
            }),
            anyhow::anyhow!("database is locked (SQLITE_BUSY)"),
            anyhow::anyhow!("database is busy (recovery in progress)"),
            anyhow::anyhow!("database table is locked"),
            anyhow::anyhow!("SQLITE_BUSY"),
            anyhow::anyhow!("SQLITE_BUSY_SNAPSHOT"),
            anyhow::anyhow!("SQLITE_LOCKED"),
            anyhow::anyhow!("SQLITE_LOCKED_SHAREDCACHE"),
        ] {
            let error = error.context("reading /cursor/state.vscdb");
            let mut run = ConnectorIngestRun::begin("cursor", Path::new("/data"), &ctx, &[]);
            run.observe_connector_scan_error(path, &error);
            let report = run.finish();
            verify_eq!(
                report.diagnostics[0].failure_kind,
                IngestFailureKind::SourceLocked
            );
            verify!(report.diagnostics[0].retryable);
            verify_eq!(report.summary.locked, 1);
        }
        Ok(())
    }

    /// A Pi durable JSONL store whose one commit marker declares `format`.
    fn durable_jsonl_store(
        dir: &Path,
        format: i64,
    ) -> std::io::Result<(ScanRoot, DiscoveredSourceFile)> {
        std::fs::create_dir_all(dir)?;
        let main = dir.join("main.jsonl");
        std::fs::write(
            &main,
            format!("{{\"type\":\"commit\",\"format\":{format},\"seq\":1,\"writes\":[]}}\n"),
        )?;
        let root = ScanRoot::local(dir.to_path_buf());
        let source = DiscoveredSourceFile::new(
            "pi_durable",
            &root,
            main,
            DiscoveredSourceRole::PrimarySessionLog,
            true,
        );
        Ok((root, source))
    }

    #[test]
    fn a_durable_store_in_an_unsupported_format_is_reported_not_counted_empty() -> TestResult {
        let temp = tempdir()?;
        let (root, source) = durable_jsonl_store(temp.path(), 2)?;
        let main = source.source_path.display().to_string();
        let ctx = ScanContext::with_roots(temp.path().to_path_buf(), vec![root], None);
        let report = ConnectorIngestRun::begin("pi_durable", temp.path(), &ctx, &[source]).finish();
        verify_eq!(report.diagnostics.len(), 1);
        let diagnostic = &report.diagnostics[0];
        verify_eq!(
            diagnostic.failure_kind,
            IngestFailureKind::UnsupportedStoreFormat
        );
        verify_eq!(diagnostic.provider, "pi_durable");
        verify_eq!(diagnostic.source_path, main);
        let detail = diagnostic.detail.as_deref().unwrap_or_default();
        verify!(
            detail.starts_with("unsupported_version: ") && detail.contains("format"),
            "{detail}"
        );
        verify_eq!(report.summary.skipped, 1);
        verify_eq!(report.summary.discovered, 0);
        Ok(())
    }

    #[test]
    fn a_readable_durable_store_without_conversations_is_not_reported() -> TestResult {
        let temp = tempdir()?;
        let (root, source) = durable_jsonl_store(temp.path(), 1)?;
        let ctx = ScanContext::with_roots(temp.path().to_path_buf(), vec![root], None);
        let report = ConnectorIngestRun::begin("pi_durable", temp.path(), &ctx, &[source]).finish();
        verify!(report.diagnostics.is_empty(), "{:?}", report.diagnostics);
        verify_eq!(report.summary.discovered, 1);
        Ok(())
    }

    #[test]
    fn reused_and_productive_durable_stores_are_not_read_again() -> TestResult {
        // Both stores would be reported if read: their format is unsupported.
        let temp = tempdir()?;
        let (reused_root, reused) = durable_jsonl_store(&temp.path().join("reused"), 2)?;
        let (productive_root, productive) =
            durable_jsonl_store(&temp.path().join("productive"), 2)?;
        let ctx = ScanContext::with_roots(
            temp.path().to_path_buf(),
            vec![reused_root, productive_root],
            None,
        );
        let mut run = ConnectorIngestRun::begin(
            "pi_durable",
            temp.path(),
            &ctx,
            &[reused.clone(), productive.clone()],
        );
        run.observe_reused_sources([reused.source_path]);
        let mut conversation = NormalizedConversation {
            agent_slug: "pi_durable".into(),
            external_id: Some("lead".into()),
            title: None,
            workspace: None,
            source_path: productive.source_path,
            started_at: None,
            ended_at: None,
            metadata: json!({}),
            messages: Vec::new(),
        };
        run.observe_conversation(&mut conversation);
        let report = run.finish();
        verify!(report.diagnostics.is_empty(), "{:?}", report.diagnostics);
        verify_eq!(report.summary.indexed, 2);
        Ok(())
    }

    #[test]
    fn a_reused_source_missing_from_the_discovery_inventory_counts_as_indexed() -> TestResult {
        // Self-contained connectors (Claude Code, Codex) reuse ledger rows
        // without listing the unchanged sources first.
        let temp = tempdir()?;
        let ctx = ScanContext::with_roots(temp.path().to_path_buf(), Vec::new(), None);
        let mut run = ConnectorIngestRun::begin("claude", temp.path(), &ctx, &[]);
        let reused = temp.path().join("projects/p/session-0.jsonl");
        run.observe_reused_sources([reused.clone(), reused]);
        let report = run.finish();
        verify!(report.diagnostics.is_empty(), "{:?}", report.diagnostics);
        verify_eq!(report.summary.indexed, 1);
        verify_eq!(report.summary.discovered, 0);
        Ok(())
    }

    fn v2_sidecar(agent_dir: &Path) -> std::io::Result<PathBuf> {
        // One component at a time: the diagnostic's path comes from a walk,
        // which spells Windows separators as `\`.
        let sidecar = agent_dir
            .join("sessions")
            .join("--work--")
            .join("2026-01-01T00-00-00-000Z_0123abcd.v2");
        std::fs::create_dir_all(&sidecar)?;
        std::fs::write(sidecar.join("manifest.json"), "{}")?;
        Ok(sidecar)
    }

    #[test]
    fn a_pi_session_store_v2_sidecar_is_reported() -> TestResult {
        let temp = tempdir()?;
        let agent_dir = temp.path().join("copied-home/.pi/agent");
        let sidecar = v2_sidecar(&agent_dir)?;
        let ctx = ScanContext::with_roots(
            temp.path().to_path_buf(),
            vec![ScanRoot::local(agent_dir)],
            None,
        );
        let report = ConnectorIngestRun::begin("pi_agent", temp.path(), &ctx, &[]).finish();
        verify_eq!(report.diagnostics.len(), 1, "{:?}", report.diagnostics);
        let diagnostic = &report.diagnostics[0];
        verify_eq!(
            diagnostic.failure_kind,
            IngestFailureKind::UnsupportedStoreFormat
        );
        verify_eq!(diagnostic.source_path, sidecar.display().to_string());
        verify!(
            diagnostic
                .detail
                .as_deref()
                .is_some_and(|detail| detail.starts_with("session_store_v2: ")),
            "{:?}",
            diagnostic.detail
        );
        verify_eq!(report.summary.skipped, 1);
        Ok(())
    }

    #[test]
    fn a_v2_sidecar_in_an_omp_store_is_not_reported_as_pi() -> TestResult {
        let temp = tempdir()?;
        let agent_dir = temp.path().join("copied-home/.omp/agent");
        v2_sidecar(&agent_dir)?;
        let ctx = ScanContext::with_roots(
            temp.path().to_path_buf(),
            vec![ScanRoot::local(agent_dir)],
            None,
        );
        // Pi's own permissive detection does claim it.
        verify_eq!(
            franken_agent_detection::PiAgentConnector::new()
                .unsupported_store_diagnostics(&ctx)
                .len(),
            1
        );
        let report = ConnectorIngestRun::begin("pi_agent", temp.path(), &ctx, &[]).finish();
        verify!(report.diagnostics.is_empty(), "{:?}", report.diagnostics);
        Ok(())
    }
}
