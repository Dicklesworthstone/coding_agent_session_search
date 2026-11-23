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
use std::io::{self, BufRead, Read};
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
    /// The source changed during its bounded read, so that observation was
    /// withheld. A subsequent stable observation can be indexed.
    SourceChanged,
    /// A source read ended before a complete observation could be produced.
    /// Unlike TruncatedSession, no partial conversation is certified here.
    UnfinishedSource,
    /// A discovered source disappeared before its read could complete.
    SourceMissing,
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
    /// The supplemental JSONL inspection reached its own resource bound.
    /// This limits diagnostic coverage, not the connector's ingestion.
    DiagnosticBudgetExhausted,
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
            IngestFailureKind::SourceChanged => "source-changed",
            IngestFailureKind::UnfinishedSource => "unfinished-source",
            IngestFailureKind::SourceMissing => "source-missing",
            IngestFailureKind::EncryptedUnsupported => "encrypted-unsupported",
            IngestFailureKind::KeychainUnavailable => "keychain-unavailable",
            IngestFailureKind::FilenameAssumptionViolated => "filename-assumption-violated",
            IngestFailureKind::UnreadableSource => "unreadable-source",
            IngestFailureKind::UnparseableSource => "unparseable-source",
            IngestFailureKind::SourceOverReadBudget => "source-over-read-budget",
            IngestFailureKind::DiagnosticBudgetExhausted => "diagnostic-budget-exhausted",
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
        IngestFailureKind::SourceChanged => (
            IngestSeverity::Warning,
            true,
            SourceIngestDisposition::Skipped,
            format!(
                "{provider} source changed while being read; this observation was withheld. \
                 Retry after the writer settles; completed sibling sources remain indexed"
            ),
        ),
        IngestFailureKind::UnfinishedSource => (
            IngestSeverity::Warning,
            true,
            SourceIngestDisposition::Skipped,
            format!(
                "{provider} source has an unfinished record or became shorter while being read; \
                 this observation was withheld. Retry after the writer settles; preserve the \
                 original source if recovery is needed"
            ),
        ),
        IngestFailureKind::SourceMissing => (
            IngestSeverity::Warning,
            true,
            SourceIngestDisposition::Skipped,
            format!(
                "{provider} source disappeared after discovery; restore or resync that source, \
                 then re-index. Completed sibling sources remain indexed"
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
        IngestFailureKind::DiagnosticBudgetExhausted => (
            IngestSeverity::Warning,
            false,
            SourceIngestDisposition::Discovered,
            "supplemental JSONL inspection reached its bounded budget; connector ingestion \
             proceeds independently. Diagnostic coverage is incomplete; preserve the original \
             source for deeper inspection and consult the connector's indexing outcome"
                .to_string(),
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

fn is_jsonl_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
}

/// Supplemental diagnostics must not bypass a connector's bounded parser or
/// allocate a whole transcript-sized record. These are inspection limits, not
/// source-admission limits: a valid larger source can still be fully indexed.
#[derive(Clone, Copy)]
struct JsonlInspectionLimits {
    source_bytes: u64,
    record_bytes: usize,
    malformed_findings: usize,
}

impl Default for JsonlInspectionLimits {
    fn default() -> Self {
        Self {
            source_bytes: 100 * 1024 * 1024,
            record_bytes: 1024 * 1024,
            malformed_findings: 128,
        }
    }
}

fn open_jsonl_for_inspection(path: &Path) -> io::Result<(std::fs::File, std::fs::Metadata)> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Discovery is not a lock: a regular file can become a FIFO before
        // this open. Inspect the actual handle before reading any bytes.
        options.custom_flags(libc::O_NONBLOCK);
    }
    #[cfg(not(unix))]
    if !std::fs::metadata(path)?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "JSONL diagnostic source is not a regular file",
        ));
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "JSONL diagnostic source is not a regular file",
        ));
    }
    Ok((file, metadata))
}

struct InspectedJsonlLine {
    bytes_read: u64,
    had_newline: bool,
    oversized: bool,
}

/// Retain at most `max_bytes`, including the line ending. Once a record is
/// too large, drain it through the same bounded reader so the next complete
/// record can still be inspected; never interpret its retained prefix as JSON.
fn read_inspection_line(
    reader: &mut impl BufRead,
    line: &mut Vec<u8>,
    max_bytes: usize,
) -> io::Result<InspectedJsonlLine> {
    line.clear();
    let mut result = InspectedJsonlLine {
        bytes_read: 0,
        had_newline: false,
        oversized: false,
    };
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(result);
        }
        let newline = available.iter().position(|&byte| byte == b'\n');
        let consumed = newline.map_or(available.len(), |position| position + 1);
        let retained = consumed.min(max_bytes.saturating_sub(line.len()));
        line.extend_from_slice(&available[..retained]);
        result.oversized |= retained < consumed;
        result.bytes_read = result.bytes_read.saturating_add(consumed as u64);
        reader.consume(consumed);
        if newline.is_some() {
            result.had_newline = true;
            return Ok(result);
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ObservedSource {
    disposition: SourceIngestDisposition,
    malformed: bool,
    /// A read or scan failure remains relevant after later content delivery.
    failed: bool,
    /// Cleared only by inspection or an actual source-ledger reuse event.
    pending_jsonl: bool,
}

/// Mutable observation of one connector scan scope. It consumes the same
/// discovery results used by raw-mirror capture, avoiding a second filesystem
/// walk, and contains no conversation payloads.
pub struct ConnectorIngestRun {
    provider: String,
    sources: BTreeMap<PathBuf, ObservedSource>,
    diagnostics: Vec<ConnectorIngestDiagnostic>,
    /// Preserve native source identities until their final disposition is
    /// known. The display paths in serialized diagnostics can be lossy.
    inspection_budget_diagnostics: Vec<(PathBuf, usize)>,
    /// Quarantine output belongs to CASS, even when the scan data dir is a
    /// remote mirror or an explicit source root.
    quarantine_data_dir: PathBuf,
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
        Self::begin_with_jsonl_policy(provider, data_dir, ctx, sources, false)
    }

    /// Defer JSONL body reads until the connector's actual source decision.
    /// An admitted or unresolved source is inspected; a ledger-reused source
    /// is not read again. These diagnostics describe this run, not historical
    /// findings from a source that the connector did not reread (GH #555).
    pub fn begin_deferred_jsonl(
        provider: &str,
        data_dir: &Path,
        ctx: &ScanContext,
        sources: &[DiscoveredSourceFile],
    ) -> Self {
        Self::begin_with_jsonl_policy(provider, data_dir, ctx, sources, true)
    }

    fn begin_with_jsonl_policy(
        provider: &str,
        data_dir: &Path,
        ctx: &ScanContext,
        sources: &[DiscoveredSourceFile],
        defer_jsonl: bool,
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
                            failed: false,
                            pending_jsonl: defer_jsonl && is_jsonl_path(&source.source_path),
                        },
                    )
                })
                .collect(),
            diagnostics: Vec::new(),
            inspection_budget_diagnostics: Vec::new(),
            quarantine_data_dir: data_dir.to_path_buf(),
            scan_data_dir: ctx.data_dir.clone(),
            durable_stores,
        };
        // Reports retain registry names (Claude registers as `claude`), while
        // parsing policy follows the conversations' provider (`claude_code`).
        let parser_provider = crate::connectors::archive_provider_slug(provider);
        for source in sources {
            if !defer_jsonl {
                run.inspect_jsonl(data_dir, &source.source_path, parser_provider);
            }
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

    fn inspect_pending_jsonl_source(&mut self, path: &Path) {
        let pending = self
            .sources
            .get_mut(path)
            .is_some_and(|source| std::mem::take(&mut source.pending_jsonl));
        if !pending {
            return;
        }
        // Clear the flag before reading: an I/O failure calls observe_scan_error
        // below, which must not recursively inspect the same source.
        let data_dir = self.quarantine_data_dir.clone();
        let parser_provider = crate::connectors::archive_provider_slug(&self.provider).to_owned();
        self.inspect_jsonl(&data_dir, path, &parser_provider);
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
                failed: false,
                pending_jsonl: false,
            });
            source.pending_jsonl = false;
            if source.disposition == SourceIngestDisposition::Discovered {
                source.disposition = SourceIngestDisposition::Indexed;
            }
        }
    }

    /// Account for a parsed source rejected by the operator's path policy.
    /// Virtual database/session handles are distinct source identities here,
    /// just as they are in `observe_conversation`. Repeated callbacks must not
    /// inflate the count or erase content or a failure already observed.
    pub fn observe_excluded_source(&mut self, path: &Path) {
        self.inspect_pending_jsonl_source(path);
        let source = self
            .sources
            .entry(path.to_path_buf())
            .or_insert(ObservedSource {
                disposition: SourceIngestDisposition::Discovered,
                malformed: false,
                failed: false,
                pending_jsonl: false,
            });
        if source.disposition == SourceIngestDisposition::Discovered {
            source.disposition = SourceIngestDisposition::Skipped;
        }
    }

    /// Record and harden one successfully parsed conversation.
    pub fn observe_conversation(&mut self, conversation: &mut NormalizedConversation) {
        self.inspect_pending_jsonl_source(&conversation.source_path);
        let state = self
            .sources
            .entry(conversation.source_path.clone())
            .or_insert(ObservedSource {
                disposition: SourceIngestDisposition::Discovered,
                malformed: false,
                failed: false,
                pending_jsonl: false,
            });
        state.disposition = if state.malformed || state.failed {
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
        self.observe_source_failure(source_path, kind);
    }

    /// Record a failed connector scan using typed per-source evidence when
    /// available. A moving, missing, unfinished, or oversized Codex rollout
    /// must not turn into a permissions error naming the whole scan root.
    pub fn observe_connector_scan_error(&mut self, fallback_path: &Path, error: &anyhow::Error) {
        if let Some(over) = crate::connectors::codex::over_budget_sources(error) {
            for path in &over.sampled_paths {
                self.observe_source_failure(path, IngestFailureKind::SourceOverReadBudget);
            }
            return;
        }
        // GH #559: the first cause cannot classify an aggregate containing
        // moving, missing, malformed and oversized rollouts. Each sample has
        // its own typed cause and original path; never parse its Display text.
        if self.provider == "codex"
            && crate::connectors::codex::for_each_source_failure(error, |path, reason| {
                use crate::connectors::codex::SourceFailureReason;
                let kind = match reason {
                    SourceFailureReason::ReadBudgetExceeded => {
                        IngestFailureKind::SourceOverReadBudget
                    }
                    SourceFailureReason::UnfinishedSource => IngestFailureKind::UnfinishedSource,
                    SourceFailureReason::SourceChanged => IngestFailureKind::SourceChanged,
                    SourceFailureReason::SourceMissing => IngestFailureKind::SourceMissing,
                    SourceFailureReason::InvalidSourceData => IngestFailureKind::UnparseableSource,
                    SourceFailureReason::SourceUnreadable
                    | SourceFailureReason::SourceReadFailed => IngestFailureKind::UnreadableSource,
                };
                self.observe_source_failure(path, kind);
            })
        {
            return;
        }
        // Codebuff retains bounded typed failures for independently scanned
        // inputs. Classify EACH cause at its recorded path, not the first cause
        // at a fallback root. Never parse paths or copy payloads from Display.
        if self.provider == "codebuff"
            && crate::connectors::codebuff::CodebuffConnector::for_each_source_failure(
                error,
                |path, cause| {
                    self.observe_source_failure(path, connector_scan_failure_kind(cause));
                },
            )
        {
            return;
        }
        // GH #511: FAD's top message includes the failing path. Classify the
        // cause, so a project called "busy" cannot turn a parse or permission
        // failure into a retryable database lock.
        self.observe_source_failure(fallback_path, connector_scan_failure_kind(error));
    }

    fn observe_source_failure(&mut self, path: &Path, kind: IngestFailureKind) {
        // Keep the eager ordering: line findings precede the connector's final
        // typed failure, so a late inspection cannot overwrite that failure.
        self.inspect_pending_jsonl_source(path);
        let mut diagnostic = classify_path(&self.provider, path, kind);
        let state = self
            .sources
            .entry(path.to_path_buf())
            .or_insert(ObservedSource {
                disposition: SourceIngestDisposition::Discovered,
                malformed: false,
                failed: false,
                pending_jsonl: false,
            });
        // A post-delivery observation error cannot erase content already
        // retained from this source. Nor can a failure at one path replace a
        // successfully indexed sibling with a synthetic failed root entry.
        if matches!(
            state.disposition,
            SourceIngestDisposition::Indexed | SourceIngestDisposition::PartiallyIndexed
        ) {
            diagnostic.disposition = SourceIngestDisposition::PartiallyIndexed;
        }
        state.failed = true;
        state.disposition = diagnostic.disposition;
        self.diagnostics.push(diagnostic);
    }

    /// Close the scope. Each Pi durable store that yielded no conversation
    /// and was not reused is asked once why, so an unsupported schema or an
    /// unreadable store is reported rather than counted as an empty source.
    /// Stores that yielded conversations are not read again.
    #[must_use]
    pub fn finish(mut self) -> ConnectorIngestReport {
        // Sources yielding no conversation or failure still need diagnostics.
        // Reuse clears the flag only after the real ledger admission decided.
        let pending: Vec<_> = self
            .sources
            .iter()
            .filter(|(_, source)| source.pending_jsonl)
            .map(|(path, _)| path.clone())
            .collect();
        for path in pending {
            self.inspect_pending_jsonl_source(&path);
        }
        self.inspect_unproductive_durable_stores();
        for (path, diagnostic_index) in &self.inspection_budget_diagnostics {
            if let Some(source) = self.sources.get(path)
                && let Some(diagnostic) = self.diagnostics.get_mut(*diagnostic_index)
            {
                // Inspection can happen before the connector delivers its
                // conversation. Project its final outcome without treating an
                // incomplete supplemental inspection as an ingestion failure.
                diagnostic.disposition = source.disposition;
            }
        }
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
                    failed: false,
                    pending_jsonl: false,
                },
            );
            self.diagnostics.push(diagnostic);
        }
    }

    fn inspect_jsonl(&mut self, data_dir: &Path, path: &Path, parser_provider: &str) {
        self.inspect_jsonl_with_limits(
            data_dir,
            path,
            parser_provider,
            JsonlInspectionLimits::default(),
        );
    }

    fn inspect_jsonl_with_limits(
        &mut self,
        data_dir: &Path,
        path: &Path,
        parser_provider: &str,
        limits: JsonlInspectionLimits,
    ) {
        if !is_jsonl_path(path) {
            return;
        }
        let (file, metadata) = match open_jsonl_for_inspection(path) {
            Ok(opened) => opened,
            Err(error) => {
                self.observe_scan_error(path, error.to_string());
                return;
            }
        };
        if parser_provider == "codex" {
            // Diagnostics may run before or after source admission. Do not
            // read a body beyond the connector budget or invent corruption
            // evidence when its policy is invalid. Its scan reports the cause.
            let Ok(limit) = crate::connectors::codex::source_read_budget(path) else {
                return;
            };
            if metadata.len() > limit {
                return;
            }
        }
        // Every provider is bounded to the opened prefix. An append must not
        // keep this supplemental pass alive indefinitely, and a parser's own
        // budget must not be defeated by its diagnostic observer.
        let read_bytes = metadata.len().min(limits.source_bytes);
        let mut reader = std::io::BufReader::new(file.take(read_bytes));
        let mut raw_line = Vec::new();
        let mut line_number = 0u64;
        let mut inspected_bytes = 0u64;
        let mut oversized_records = 0u64;
        let mut first_uninspected_line = None;
        let mut malformed_findings = 0usize;
        let mut findings_limited = false;
        let source_path = path.display().to_string();
        let supports_suffix_recovery = matches!(
            parser_provider,
            "pi_agent" | "pi_agent_rust" | "omp" | "codex" | "claude_code"
        );
        let accepts_leading_bom = supports_suffix_recovery
            || matches!(
                parser_provider,
                "clawdbot"
                    | "factory"
                    | "gemini"
                    | "kimi"
                    | "pi_durable"
                    | "prime_agent"
                    | "qwen"
                    | "vibe"
            );
        loop {
            if malformed_findings >= limits.malformed_findings && inspected_bytes < read_bytes {
                findings_limited = true;
                first_uninspected_line.get_or_insert(line_number.saturating_add(1));
                break;
            }
            let line = match read_inspection_line(&mut reader, &mut raw_line, limits.record_bytes) {
                Ok(line) => line,
                Err(error) => {
                    self.observe_scan_error(path, error);
                    break;
                }
            };
            if line.bytes_read == 0 {
                break;
            }
            line_number = line_number.saturating_add(1);
            inspected_bytes = inspected_bytes.saturating_add(line.bytes_read);
            if line.oversized {
                oversized_records = oversized_records.saturating_add(1);
                first_uninspected_line.get_or_insert(line_number);
                continue;
            }
            let had_newline = line.had_newline;
            if !had_newline && inspected_bytes == read_bytes && metadata.len() > read_bytes {
                // This is the observer's window boundary, not evidence that
                // the provider wrote a truncated or malformed record.
                first_uninspected_line.get_or_insert(line_number);
                break;
            }
            if had_newline {
                raw_line.pop();
                if raw_line.last() == Some(&b'\r') {
                    raw_line.pop();
                }
            }
            let line = raw_line.as_slice();
            let text = std::str::from_utf8(line).ok();
            // Match FAD's ordinary JSONL normalization without changing the raw
            // recovery budget or the bytes retained as quarantine evidence.
            let ordinary_text = text.map(|text| {
                if parser_provider == "gemini" {
                    text.trim().trim_start_matches('\u{feff}')
                } else if matches!(parser_provider, "codex" | "pi_durable" | "prime_agent") {
                    text.trim_start_matches('\u{feff}').trim()
                } else if accepts_leading_bom {
                    text.trim_start_matches('\u{feff}')
                } else {
                    text
                }
            });
            if ordinary_text.is_some_and(|text| {
                text.trim().is_empty() || serde_json::from_str::<serde_json::Value>(text).is_ok()
            }) {
                continue;
            }
            let kind = if !had_newline {
                IngestFailureKind::TruncatedSession
            } else {
                IngestFailureKind::MalformedJsonLine
            };
            malformed_findings = malformed_findings.saturating_add(1);
            // These providers use FAD's shared suffix recovery. Other JSONL
            // connectors retain their existing whole-line quarantine policy.
            let recovery = text
                .filter(|_| supports_suffix_recovery)
                .map(franken_agent_detection::connectors::jsonl::recover_jsonl_suffix)
                .filter(|recovery| !recovery.records.is_empty());
            let mut diagnostic =
                classify(&self.provider, &source_path, kind).with_line(line_number);
            let quarantined = if let Some(recovery) = recovery {
                diagnostic.detail = Some(format!(
                    "recovered complete records: {}; quarantined prefix bytes: {}",
                    recovery.records.len(),
                    recovery.discarded_bytes
                ));
                diagnostic.safe_next_action = "the malformed prefix is quarantined; complete suffix records continue through the connector; inspect the original source if the missing prefix is needed".to_string();
                &line[..recovery.discarded_bytes]
            } else {
                line
            };
            self.diagnostics.push(diagnostic);
            self.sources.entry(path.to_path_buf()).and_modify(|source| {
                source.disposition = SourceIngestDisposition::Quarantined;
                source.malformed = true;
            });
            if let Err(error) = crate::indexer::quarantine::record_connector_line(
                data_dir,
                &self.provider,
                path,
                line_number,
                quarantined,
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
        if oversized_records > 0 || findings_limited || metadata.len() > read_bytes {
            let mut diagnostic = classify(
                &self.provider,
                &source_path,
                IngestFailureKind::DiagnosticBudgetExhausted,
            )
            .with_detail(format!(
                "source_bytes={}; read_prefix_bytes={inspected_bytes}; \
                 source_limit_bytes={}; record_limit_bytes={}; \
                 oversized_records_not_validated={oversized_records}; \
                 malformed_findings_limit={}; findings_limit_reached={findings_limited}",
                metadata.len(),
                limits.source_bytes,
                limits.record_bytes,
                limits.malformed_findings,
            ));
            diagnostic.line_or_row = first_uninspected_line;
            // Diagnostic limits do not certify malformed input, withhold a
            // source, or change a successfully delivered conversation's status.
            if let Some(source) = self.sources.get(path) {
                diagnostic.disposition = source.disposition;
            }
            self.inspection_budget_diagnostics
                .push((path.to_path_buf(), self.diagnostics.len()));
            self.diagnostics.push(diagnostic);
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
                            failed: false,
                            pending_jsonl: false,
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
            IngestFailureKind::DiagnosticBudgetExhausted,
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
            IngestFailureKind::SourceChanged,
            IngestFailureKind::UnfinishedSource,
            IngestFailureKind::SourceMissing,
            IngestFailureKind::DiagnosticBudgetExhausted,
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
                "source-over-read-budget",
                "source-changed",
                "unfinished-source",
                "source-missing",
                "diagnostic-budget-exhausted"
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
            (IngestFailureKind::SourceChanged, "source-changed"),
            (IngestFailureKind::UnfinishedSource, "unfinished-source"),
            (IngestFailureKind::SourceMissing, "source-missing"),
            (
                IngestFailureKind::DiagnosticBudgetExhausted,
                "diagnostic-budget-exhausted",
            ),
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
    fn jsonl_inspection_limits_preserve_real_claude_ingestion_and_later_findings() -> TestResult {
        for malformed_sibling in [false, true] {
            let temp = tempdir()?;
            let project = temp.path().join("projects/bounded");
            std::fs::create_dir_all(&project)?;
            let path = project.join("session.jsonl");
            let content = format!(
                "boundeddiagnosticcanary{}",
                "x".repeat(JsonlInspectionLimits::default().record_bytes),
            );
            let record = json!({
                "type": "user", "uuid": "bounded-message", "sessionId": "bounded-session",
                "timestamp": "2026-11-23T10:00:00Z", "cwd": "/work/bounded",
                "message": {"role": "user", "content": content},
            });
            let body = format!(
                "{record}\n{}",
                if malformed_sibling { "{broken}\n" } else { "" }
            );
            std::fs::write(&path, &body)?;
            let ctx = ScanContext::with_roots(
                temp.path().to_path_buf(),
                vec![ScanRoot::local(temp.path().to_path_buf())],
                None,
            );
            let connector = crate::connectors::claude_code::ClaudeCodeConnector::new();
            let sources = connector.discover_source_files(&ctx)?;
            verify_eq!(sources.len(), 1);
            let mut run = ConnectorIngestRun::begin("claude", temp.path(), &ctx, &sources);
            let mut conversations = connector.scan(&ctx)?;
            verify_eq!(conversations.len(), 1);
            verify_eq!(conversations[0].messages.len(), 1);
            verify_eq!(conversations[0].messages[0].content, content);
            run.observe_conversation(&mut conversations[0]);
            let report = run.finish();
            verify_eq!(report.summary.total(), 1);
            verify_eq!(report.summary.indexed, u64::from(!malformed_sibling));
            verify_eq!(
                report.summary.partially_indexed,
                u64::from(malformed_sibling)
            );
            verify_eq!(report.summary.skipped, 0);
            let limited = report
                .diagnostics
                .iter()
                .find(|finding| {
                    finding.failure_kind == IngestFailureKind::DiagnosticBudgetExhausted
                })
                .ok_or("missing incomplete-inspection warning")?;
            verify_eq!(limited.line_or_row, Some(1));
            verify_eq!(limited.severity, IngestSeverity::Warning);
            verify!(!limited.retryable);
            verify_eq!(
                limited.disposition,
                if malformed_sibling {
                    SourceIngestDisposition::PartiallyIndexed
                } else {
                    SourceIngestDisposition::Indexed
                },
            );
            verify!(
                limited
                    .detail
                    .as_deref()
                    .is_some_and(|detail| { detail.contains("oversized_records_not_validated=1") })
            );
            let quarantine = temp.path().join("quarantine/connector_ingest_lines.json");
            if malformed_sibling {
                verify_eq!(report.diagnostics.len(), 2);
                let malformed = &report.diagnostics[0];
                verify_eq!(malformed.failure_kind, IngestFailureKind::MalformedJsonLine);
                verify_eq!(malformed.line_or_row, Some(2));
                let state: serde_json::Value = serde_json::from_slice(&std::fs::read(quarantine)?)?;
                let entries = state["entries"]
                    .as_object()
                    .ok_or("missing quarantine entries")?;
                verify_eq!(entries.len(), 1);
                let entry = entries.values().next().ok_or("missing malformed line")?;
                verify_eq!(entry["line_number"], 2);
                verify_eq!(entry["payload_bytes"], b"{broken}".len());
                verify_eq!(
                    entry["payload_blake3"],
                    blake3::hash(b"{broken}").to_hex().to_string(),
                );
            } else {
                verify_eq!(report.diagnostics.len(), 1);
                verify!(
                    !quarantine.exists(),
                    "an uninspected valid record is not quarantined"
                );
            }
            verify!(!serde_json::to_string(&report)?.contains("boundeddiagnosticcanary"));
            verify_eq!(std::fs::read_to_string(&path)?, body);
        }
        Ok(())
    }

    #[test]
    fn jsonl_inspection_source_window_does_not_invent_a_truncated_record() -> TestResult {
        let temp = tempdir()?;
        let path = temp.path().join("window.jsonl");
        let body = b"{\"ok\":1}\n{\"still_valid\":2}\n";
        std::fs::write(&path, body)?;
        let ctx = ScanContext::with_roots(temp.path().to_path_buf(), Vec::new(), None);
        for source_bytes in [9, 10, 13, body.len() as u64] {
            let mut run = ConnectorIngestRun::begin("claude", temp.path(), &ctx, &[]);
            run.inspect_jsonl_with_limits(
                temp.path(),
                &path,
                "claude_code",
                JsonlInspectionLimits {
                    source_bytes,
                    record_bytes: 64,
                    malformed_findings: 4,
                },
            );
            if source_bytes == body.len() as u64 {
                verify!(run.diagnostics.is_empty());
            } else {
                verify_eq!(run.diagnostics.len(), 1);
                verify_eq!(
                    run.diagnostics[0].failure_kind,
                    IngestFailureKind::DiagnosticBudgetExhausted,
                );
                verify!(run.diagnostics[0].detail.as_deref().is_some_and(|detail| {
                    detail.contains(&format!("read_prefix_bytes={source_bytes};"))
                }));
            }
            verify!(
                !temp
                    .path()
                    .join("quarantine/connector_ingest_lines.json")
                    .exists()
            );
        }
        verify_eq!(std::fs::read(&path)?, body);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn jsonl_inspection_budget_outcomes_preserve_native_source_identity() -> TestResult {
        use std::os::unix::ffi::OsStringExt;

        let temp = tempdir()?;
        let indexed = temp
            .path()
            .join(std::ffi::OsString::from_vec(b"native-\x80.jsonl".to_vec()));
        let skipped = temp
            .path()
            .join(std::ffi::OsString::from_vec(b"native-\x81.jsonl".to_vec()));
        verify_ne!(indexed, skipped);
        verify_eq!(indexed.display().to_string(), skipped.display().to_string());
        let body = format!(
            "{{\"message\":\"{}\"}}\n",
            "x".repeat(JsonlInspectionLimits::default().record_bytes),
        );
        let root = ScanRoot::local(temp.path().to_path_buf());
        let sources: Vec<_> = [&indexed, &skipped]
            .into_iter()
            .map(|path| {
                DiscoveredSourceFile::new(
                    "claude_code",
                    &root,
                    path.clone(),
                    DiscoveredSourceRole::PrimarySessionLog,
                    true,
                )
            })
            .collect();
        for path in [&indexed, &skipped] {
            std::fs::write(path, &body)?;
        }
        let ctx = ScanContext::with_roots(temp.path().to_path_buf(), vec![root], None);
        let mut run = ConnectorIngestRun::begin("claude", temp.path(), &ctx, &sources);
        run.observe_reused_sources([indexed]);
        run.observe_excluded_source(&skipped);
        let report = run.finish();
        verify_eq!(report.summary.indexed, 1);
        verify_eq!(report.summary.skipped, 1);
        verify_eq!(report.diagnostics.len(), 2);
        verify_eq!(
            report.diagnostics[0].disposition,
            SourceIngestDisposition::Indexed
        );
        verify_eq!(
            report.diagnostics[1].disposition,
            SourceIngestDisposition::Skipped
        );
        verify!(report.diagnostics.iter().all(|finding| {
            finding.failure_kind == IngestFailureKind::DiagnosticBudgetExhausted
        }));
        Ok(())
    }

    #[test]
    fn jsonl_inspection_caps_malformed_findings_without_hiding_the_limit() -> TestResult {
        let temp = tempdir()?;
        let path = temp.path().join("many-malformed.jsonl");
        let body = b"{first}\n{second}\n{third}\n{fourth}\n{fifth}\n";
        std::fs::write(&path, body)?;
        let ctx = ScanContext::with_roots(temp.path().to_path_buf(), Vec::new(), None);
        let mut run = ConnectorIngestRun::begin("claude", temp.path(), &ctx, &[]);
        run.inspect_jsonl_with_limits(
            temp.path(),
            &path,
            "claude_code",
            JsonlInspectionLimits {
                source_bytes: 1024,
                record_bytes: 64,
                malformed_findings: 3,
            },
        );
        verify_eq!(run.diagnostics.len(), 4);
        for (index, finding) in run.diagnostics[..3].iter().enumerate() {
            verify_eq!(finding.failure_kind, IngestFailureKind::MalformedJsonLine);
            verify_eq!(finding.line_or_row, Some(index as u64 + 1));
        }
        let limited = &run.diagnostics[3];
        verify_eq!(
            limited.failure_kind,
            IngestFailureKind::DiagnosticBudgetExhausted
        );
        verify_eq!(limited.line_or_row, Some(4));
        verify!(limited.detail.as_deref().is_some_and(|detail| {
            detail.contains("malformed_findings_limit=3; findings_limit_reached=true")
        }));
        let state: serde_json::Value = serde_json::from_slice(&std::fs::read(
            temp.path().join("quarantine/connector_ingest_lines.json"),
        )?)?;
        verify_eq!(
            state["entries"]
                .as_object()
                .ok_or("missing quarantine entries")?
                .len(),
            3
        );
        verify_eq!(std::fs::read(&path)?, body);
        Ok(())
    }

    #[test]
    fn jsonl_inspection_reader_bounds_memory_and_preserves_line_boundaries() -> TestResult {
        let temp = tempdir()?;
        let path = temp.path().join("reader.jsonl");
        let body = b"1234567\n1234567890123456789012345678901234567890\n{}\nlast";
        std::fs::write(&path, body)?;
        let file = std::fs::File::open(&path)?;
        let mut reader = std::io::BufReader::with_capacity(3, file);
        let mut line = Vec::new();
        for (bytes_read, had_newline, oversized, retained) in [
            (8, true, false, b"1234567\n".as_slice()),
            (41, true, true, b"12345678".as_slice()),
            (3, true, false, b"{}\n".as_slice()),
            (4, false, false, b"last".as_slice()),
            (0, false, false, b"".as_slice()),
        ] {
            let result = read_inspection_line(&mut reader, &mut line, 8)?;
            verify_eq!(result.bytes_read, bytes_read);
            verify_eq!(result.had_newline, had_newline);
            verify_eq!(result.oversized, oversized);
            verify_eq!(line, retained);
            verify!(line.capacity() <= 8, "record allocation exceeded the bound");
        }
        verify_eq!(std::fs::read(&path)?, body);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn jsonl_inspection_rejects_non_regular_replacements_without_blocking() -> TestResult {
        use std::os::unix::fs::FileTypeExt;

        const CHILD: &str = "CASS_TEST_JSONL_DIAGNOSTIC_FIFO_CHILD";
        if dotenvy::var(CHILD).as_deref() != Ok("1") {
            let output = assert_cmd::Command::new(std::env::current_exe()?)
                .args([
                    "jsonl_inspection_rejects_non_regular_replacements_without_blocking",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .timeout(std::time::Duration::from_secs(10))
                .output()?;
            verify!(
                output.status.success(),
                "FIFO inspection child failed: {output:?}"
            );
            verify!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
            return Ok(());
        }
        for (deferred, use_alias) in [(false, false), (false, true), (true, false), (true, true)] {
            let temp = tempdir()?;
            let path = temp.path().join("replaced.jsonl");
            let body = b"{\"ok\":true}\n";
            std::fs::write(&path, body)?;
            let root = ScanRoot::local(temp.path().to_path_buf());
            let source = DiscoveredSourceFile::new(
                "claude_code",
                &root,
                path.clone(),
                DiscoveredSourceRole::PrimarySessionLog,
                true,
            );
            let ctx = ScanContext::with_roots(temp.path().to_path_buf(), vec![root], None);
            let retained = temp.path().join("retained.jsonl");
            std::fs::rename(&path, &retained)?;
            let fifo = if use_alias {
                temp.path().join("pipe")
            } else {
                path.clone()
            };
            verify!(
                std::process::Command::new("mkfifo")
                    .arg(&fifo)
                    .status()?
                    .success()
            );
            if use_alias {
                std::os::unix::fs::symlink(&fifo, &path)?;
            }
            let run = if deferred {
                ConnectorIngestRun::begin_deferred_jsonl("claude", temp.path(), &ctx, &[source])
            } else {
                ConnectorIngestRun::begin("claude", temp.path(), &ctx, &[source])
            };
            let report = run.finish();
            verify_eq!(report.summary.skipped, 1);
            verify_eq!(report.diagnostics.len(), 1);
            verify_eq!(
                report.diagnostics[0].failure_kind,
                IngestFailureKind::UnreadableSource
            );
            verify_eq!(std::fs::read(&retained)?, body);
            verify!(std::fs::symlink_metadata(&fifo)?.file_type().is_fifo());
        }
        Ok(())
    }

    #[test]
    fn gh553_intact_bom_lines_do_not_create_quarantine_evidence() -> TestResult {
        for provider in [
            "pi_agent",
            "pi_agent_rust",
            "omp",
            "codex",
            "claude",
            "claude_code",
            "clawdbot",
            "factory",
            "gemini",
            "kimi",
            "pi_durable",
            "prime_agent",
            "qwen",
            "vibe",
        ] {
            let root = tempdir()?.keep();
            let path = root.join("intact-bom.jsonl");
            let body = concat!(
                "\u{feff}{\"type\":\"session\",\"id\":\"bom\"}\r\n",
                "\u{feff}\u{feff}{\"wrapper\":{\"role\":\"user\",\"content\":\"nested\"}}\n",
                "\u{feff} \t\r\n",
                "\u{2003}\r\n",
            );
            let body = if provider == "gemini" {
                format!("{body} \t\u{feff}{{\"type\":\"session\",\"id\":\"spaced-bom\"}}\n")
            } else if matches!(provider, "codex" | "pi_durable" | "prime_agent") {
                format!(
                    "{body}\u{feff}\u{2003}{{\"type\":\"session\",\"id\":\"unicode-space\"}}\u{2003}\n\u{feff}\u{2003}\n"
                )
            } else {
                body.to_string()
            };
            std::fs::write(&path, &body)?;
            let scan_root = ScanRoot::local(root.clone());
            let source = DiscoveredSourceFile::new(
                provider,
                &scan_root,
                path.clone(),
                DiscoveredSourceRole::PrimarySessionLog,
                true,
            );
            let context = ScanContext::with_roots(root.clone(), vec![scan_root], None);
            let run = ConnectorIngestRun::begin(provider, &root, &context, &[source]);
            verify!(run.diagnostics.is_empty());
            let observed = run.sources.get(&path).ok_or("missing inspected source")?;
            verify!(!observed.malformed);
            let error = std::fs::metadata(root.join("quarantine/connector_ingest_lines.json"))
                .err()
                .ok_or("intact records created quarantine evidence")?;
            verify_eq!(error.kind(), std::io::ErrorKind::NotFound);
            verify_eq!(std::fs::read_to_string(path)?, body);
        }
        Ok(())
    }

    #[test]
    fn gh553_unrecoverable_lines_keep_complete_quarantine_evidence() -> TestResult {
        let cases: [(&str, &[u8]); 6] = [
            (
                "pi_agent",
                br#"{"payload":{"role":"user","content":"nested"}"#,
            ),
            (
                "pi_agent",
                br#"torn{"role":"user","content":"complete"}trailing"#,
            ),
            ("vibe", br#"torn{"role":"user","content":"complete"}"#),
            ("unknown_provider", b"\xef\xbb\xbf{\"intact\":true}"),
            (
                "vibe",
                "\u{feff}\u{2003}{\"intact\":true}\u{2003}".as_bytes(),
            ),
            (
                "pi_agent",
                b"invalid\xff{\"role\":\"user\",\"content\":\"complete\"}",
            ),
        ];
        for (provider, line) in cases {
            let root = tempdir()?.keep();
            let path = root.join("bad.jsonl");
            let mut bytes = line.to_vec();
            bytes.push(b'\n');
            std::fs::write(&path, &bytes)?;
            let scan_root = ScanRoot::local(root.clone());
            let source = DiscoveredSourceFile::new(
                provider,
                &scan_root,
                path.clone(),
                DiscoveredSourceRole::PrimarySessionLog,
                true,
            );
            let context = ScanContext::with_roots(root.clone(), vec![scan_root], None);
            let report = ConnectorIngestRun::begin(provider, &root, &context, &[source]).finish();
            verify_eq!(report.diagnostics.len(), 1);
            verify_eq!(report.diagnostics[0].detail, None);
            let state: serde_json::Value = serde_json::from_slice(&std::fs::read(
                root.join("quarantine/connector_ingest_lines.json"),
            )?)?;
            let entries = state["entries"]
                .as_object()
                .ok_or("missing quarantine entries")?;
            verify_eq!(entries.len(), 1);
            let entry = entries.values().next().ok_or("missing quarantine record")?;
            verify_eq!(entry["payload_bytes"], json!(line.len()));
            verify_eq!(
                entry["payload_blake3"],
                json!(blake3::hash(line).to_hex().to_string())
            );
            verify_eq!(std::fs::read(path)?, bytes);
        }
        Ok(())
    }

    #[test]
    fn gh553_quarantine_hashes_only_the_unrecovered_prefix() -> TestResult {
        for provider in ["pi_agent", "omp"] {
            let root = tempdir()?.keep();
            let path = root.join("torn.jsonl");
            let prefix = "{\"type\":\"message\",\"message\":{\"content\":\"署名";
            let record = r#"{"type":"message","id":"tool","message":{"role":"toolResult","content":"recovered marker"}}"#;
            std::fs::write(&path, format!("{prefix}{record}\r\n"))?;
            let scan_root = ScanRoot::local(root.clone());
            let source = DiscoveredSourceFile::new(
                provider,
                &scan_root,
                path.clone(),
                DiscoveredSourceRole::PrimarySessionLog,
                true,
            );
            let context = ScanContext::with_roots(root.clone(), vec![scan_root], None);
            let run = ConnectorIngestRun::begin(provider, &root, &context, &[source]);
            let report = run.finish();
            verify_eq!(report.diagnostics.len(), 1);
            let diagnostic = &report.diagnostics[0];
            verify_eq!(diagnostic.line_or_row, Some(1));
            verify_eq!(
                diagnostic.failure_kind,
                IngestFailureKind::MalformedJsonLine
            );
            let expected_detail = format!(
                "recovered complete records: 1; quarantined prefix bytes: {}",
                prefix.len()
            );
            verify_eq!(diagnostic.detail.as_deref(), Some(expected_detail.as_str()));
            let state: serde_json::Value = serde_json::from_slice(&std::fs::read(
                root.join("quarantine/connector_ingest_lines.json"),
            )?)?;
            let entries = state["entries"]
                .as_object()
                .ok_or("missing quarantine entries")?;
            verify_eq!(entries.len(), 1);
            let entry = entries
                .values()
                .next()
                .ok_or("missing quarantined prefix")?;
            verify_eq!(entry["payload_bytes"], json!(prefix.len()));
            verify_eq!(
                entry["payload_blake3"],
                json!(blake3::hash(prefix.as_bytes()).to_hex().to_string())
            );
            verify_eq!(
                std::fs::read_to_string(path)?,
                format!("{prefix}{record}\r\n")
            );
        }
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
    fn gh555_reused_jsonl_is_not_opened_but_unresolved_siblings_are_inspected() -> TestResult {
        let temp = tempdir()?;
        let reused = temp.path().join("not-opened.jsonl");
        let unresolved = temp.path().join("unresolved.jsonl");
        std::fs::write(&unresolved, b"{broken}\n")?;
        let root = ScanRoot::local(temp.path().to_path_buf());
        let sources: Vec<_> = [&reused, &unresolved]
            .into_iter()
            .map(|path| {
                DiscoveredSourceFile::new(
                    "claude_code",
                    &root,
                    path.to_path_buf(),
                    DiscoveredSourceRole::PrimarySessionLog,
                    true,
                )
            })
            .collect();
        let ctx = ScanContext::with_roots(temp.path().to_path_buf(), vec![root], None);
        let mut run =
            ConnectorIngestRun::begin_deferred_jsonl("claude", temp.path(), &ctx, &sources);
        // The absent path is an I/O sentinel for this observer test. The
        // indexer integration tests separately exercise actual ledger reuse.
        run.observe_reused_sources([reused.clone(), reused.clone()]);
        let report = run.finish();
        verify_eq!(report.summary.total(), 2);
        verify_eq!(report.summary.indexed, 1);
        verify_eq!(report.summary.quarantined, 1);
        verify_eq!(report.diagnostics.len(), 1);
        verify_eq!(
            report.diagnostics[0].source_path,
            unresolved.display().to_string()
        );
        verify_eq!(
            report.diagnostics[0].failure_kind,
            IngestFailureKind::MalformedJsonLine
        );
        let control =
            ConnectorIngestRun::begin("claude", temp.path(), &ctx, &sources[..1]).finish();
        verify_eq!(control.summary.skipped, 1);
        verify_eq!(control.diagnostics.len(), 1);
        verify_eq!(
            control.diagnostics[0].failure_kind,
            IngestFailureKind::UnreadableSource
        );
        Ok(())
    }

    #[test]
    fn gh555_deferred_diagnostics_preserve_unreused_outcome_precedence() -> TestResult {
        for outcome in [
            "unproductive",
            "productive",
            "source_changed",
            "scan_error",
            "excluded",
            "productive_then_failure",
        ] {
            let temp = tempdir()?;
            let path = temp.path().join("mixed.jsonl");
            std::fs::write(&path, b"{\"ok\":1}\n{broken}\n{\"ok\":2}\n")?;
            let root = ScanRoot::local(temp.path().to_path_buf());
            let source = DiscoveredSourceFile::new(
                "claude_code",
                &root,
                path.clone(),
                DiscoveredSourceRole::PrimarySessionLog,
                true,
            );
            let ctx = ScanContext::with_roots(temp.path().to_path_buf(), vec![root], None);
            let mut reports = Vec::new();
            for deferred in [false, true] {
                let mut run = if deferred {
                    ConnectorIngestRun::begin_deferred_jsonl(
                        "claude",
                        temp.path(),
                        &ctx,
                        std::slice::from_ref(&source),
                    )
                } else {
                    ConnectorIngestRun::begin(
                        "claude",
                        temp.path(),
                        &ctx,
                        std::slice::from_ref(&source),
                    )
                };
                if matches!(outcome, "productive" | "productive_then_failure") {
                    let mut conversation = NormalizedConversation {
                        agent_slug: "claude_code".into(),
                        external_id: Some("mixed".into()),
                        title: None,
                        workspace: None,
                        source_path: path.clone(),
                        started_at: None,
                        ended_at: None,
                        metadata: json!({}),
                        messages: Vec::new(),
                    };
                    run.observe_conversation(&mut conversation);
                }
                match outcome {
                    "source_changed" | "productive_then_failure" => {
                        run.observe_source_failure(&path, IngestFailureKind::SourceChanged);
                    }
                    "scan_error" => run.observe_scan_error(&path, "source read failed"),
                    "excluded" => run.observe_excluded_source(&path),
                    _ => {}
                }
                reports.push(run.finish());
            }
            verify_eq!(
                serde_json::to_value(&reports[0])?,
                serde_json::to_value(&reports[1])?,
                "deferred inspection changed the {outcome} report"
            );
            verify_eq!(reports[1].diagnostics[0].line_or_row, Some(2));
            verify_eq!(
                reports[1].diagnostics[0].failure_kind,
                IngestFailureKind::MalformedJsonLine
            );
            let expected_findings = if matches!(
                outcome,
                "source_changed" | "scan_error" | "productive_then_failure"
            ) {
                2
            } else {
                1
            };
            verify_eq!(reports[1].diagnostics.len(), expected_findings);
            if matches!(outcome, "productive" | "productive_then_failure") {
                verify_eq!(reports[1].summary.partially_indexed, 1);
            }
        }
        Ok(())
    }

    #[test]
    fn gh555_deferred_read_errors_do_not_recurse_or_replace_connector_failure() -> TestResult {
        let temp = tempdir()?;
        let path = temp.path().join("missing.jsonl");
        let root = ScanRoot::local(temp.path().to_path_buf());
        let source = DiscoveredSourceFile::new(
            "codex",
            &root,
            path.clone(),
            DiscoveredSourceRole::PrimarySessionLog,
            true,
        );
        let ctx = ScanContext::with_roots(temp.path().to_path_buf(), vec![root], None);
        let mut run =
            ConnectorIngestRun::begin_deferred_jsonl("codex", temp.path(), &ctx, &[source]);
        run.observe_source_failure(&path, IngestFailureKind::SourceMissing);
        let report = run.finish();
        verify_eq!(report.summary.total(), 1);
        verify_eq!(report.summary.skipped, 1);
        verify_eq!(report.diagnostics.len(), 2);
        verify_eq!(
            report.diagnostics[0].failure_kind,
            IngestFailureKind::UnreadableSource
        );
        verify_eq!(
            report.diagnostics[1].failure_kind,
            IngestFailureKind::SourceMissing
        );
        Ok(())
    }

    #[test]
    fn gh555_deferred_read_failure_keeps_delivered_batches_partial() -> TestResult {
        let temp = tempdir()?;
        let path = temp.path().join("disappeared.jsonl");
        std::fs::write(&path, b"{\"ok\":1}\n")?;
        let root = ScanRoot::local(temp.path().to_path_buf());
        let source = DiscoveredSourceFile::new(
            "codex",
            &root,
            path.clone(),
            DiscoveredSourceRole::PrimarySessionLog,
            true,
        );
        let ctx = ScanContext::with_roots(temp.path().to_path_buf(), vec![root], None);
        let mut run =
            ConnectorIngestRun::begin_deferred_jsonl("codex", temp.path(), &ctx, &[source]);
        // The connector can have decoded content before diagnostics opens the
        // file. Keep the original bytes while making that later open fail.
        std::fs::rename(&path, temp.path().join("retained.jsonl"))?;
        let mut conversation = NormalizedConversation {
            agent_slug: "codex".into(),
            external_id: Some("delivered-before-diagnostic-read".into()),
            title: None,
            workspace: None,
            source_path: path,
            started_at: None,
            ended_at: None,
            metadata: json!({}),
            messages: Vec::new(),
        };
        run.observe_conversation(&mut conversation);
        run.observe_conversation(&mut conversation);
        let report = run.finish();
        verify_eq!(report.summary.total(), 1);
        verify_eq!(report.summary.partially_indexed, 1);
        verify_eq!(report.summary.indexed, 0);
        verify_eq!(report.summary.skipped, 0);
        verify_eq!(report.diagnostics.len(), 1);
        verify_eq!(
            report.diagnostics[0].failure_kind,
            IngestFailureKind::UnreadableSource
        );
        Ok(())
    }

    #[test]
    fn gh555_scan_failure_keeps_coverage_partial_across_delivery_order() -> TestResult {
        for typed in [false, true] {
            for failure_before_delivery in [false, true] {
                let temp = tempdir()?;
                let path = temp.path().join("delivered.jsonl");
                std::fs::write(&path, b"{\"ok\":1}\n")?;
                let root = ScanRoot::local(temp.path().to_path_buf());
                let source = DiscoveredSourceFile::new(
                    "codex",
                    &root,
                    path.clone(),
                    DiscoveredSourceRole::PrimarySessionLog,
                    true,
                );
                let ctx = ScanContext::with_roots(temp.path().to_path_buf(), vec![root], None);
                let mut run =
                    ConnectorIngestRun::begin_deferred_jsonl("codex", temp.path(), &ctx, &[source]);
                let mut conversation = NormalizedConversation {
                    agent_slug: "codex".into(),
                    external_id: Some("delivered-with-scan-failure".into()),
                    title: None,
                    workspace: None,
                    source_path: path.clone(),
                    started_at: None,
                    ended_at: None,
                    metadata: json!({}),
                    messages: Vec::new(),
                };
                let record_failure = |run: &mut ConnectorIngestRun| {
                    if typed {
                        run.observe_source_failure(&path, IngestFailureKind::SourceChanged);
                    } else {
                        run.observe_scan_error(&path, "source read failed");
                    }
                };
                if failure_before_delivery {
                    record_failure(&mut run);
                }
                run.observe_conversation(&mut conversation);
                if !failure_before_delivery {
                    record_failure(&mut run);
                }
                run.observe_conversation(&mut conversation);
                let report = run.finish();
                verify_eq!(report.summary.total(), 1);
                verify_eq!(report.summary.partially_indexed, 1);
                verify_eq!(report.summary.indexed, 0);
                verify_eq!(report.summary.skipped, 0);
                verify_eq!(report.diagnostics.len(), 1);
                verify_eq!(
                    report.diagnostics[0].failure_kind,
                    if typed {
                        IngestFailureKind::SourceChanged
                    } else {
                        IngestFailureKind::UnreadableSource
                    }
                );
                if !failure_before_delivery {
                    verify_eq!(
                        report.diagnostics[0].disposition,
                        SourceIngestDisposition::PartiallyIndexed
                    );
                }
            }
        }
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
    fn gh554_excluded_virtual_sources_are_counted_once_without_payloads() -> TestResult {
        let temp = tempdir()?;
        let database = temp.path().join("opencode.db");
        let root = ScanRoot::local(temp.path().to_path_buf());
        let source = DiscoveredSourceFile::new(
            "opencode",
            &root,
            database.clone(),
            DiscoveredSourceRole::SqliteDatabase,
            true,
        );
        let ctx = ScanContext::with_roots(temp.path().to_path_buf(), vec![root], None);
        let mut run = ConnectorIngestRun::begin("opencode", temp.path(), &ctx, &[source]);
        let first = database.join("ses_first");
        let second = database.join("ses_second");
        run.observe_excluded_source(&first);
        run.observe_excluded_source(&first);
        run.observe_excluded_source(&second);
        let report = run.finish();
        verify_eq!(report.summary.discovered, 1);
        verify_eq!(report.summary.skipped, 2);
        verify_eq!(report.summary.with_content(), 0);
        verify!(report.diagnostics.is_empty());
        let json = serde_json::to_value(report)?;
        verify_eq!(json["summary"]["skipped"], json!(2));
        verify!(!json.to_string().contains("ses_first"));
        Ok(())
    }

    #[test]
    fn gh554_exclusion_observations_preserve_content_and_failure_evidence() -> TestResult {
        let temp = tempdir()?;
        let ctx = ScanContext::local_default(temp.path().to_path_buf(), None);
        let mut run = ConnectorIngestRun::begin("opencode", temp.path(), &ctx, &[]);
        let mut conversation = NormalizedConversation {
            agent_slug: "opencode".into(),
            external_id: Some("ses_indexed".into()),
            title: None,
            workspace: None,
            source_path: temp.path().join("opencode.db/ses_indexed"),
            started_at: None,
            ended_at: None,
            metadata: json!({}),
            messages: Vec::new(),
        };
        // Later content supersedes a prior exclusion observation, while a
        // repeated exclusion must not erase an already observed success.
        run.observe_excluded_source(&conversation.source_path);
        run.observe_conversation(&mut conversation);
        run.observe_excluded_source(&conversation.source_path);
        let locked = temp.path().join("locked.db");
        run.observe_scan_error(&locked, "database is locked (SQLITE_BUSY)");
        run.observe_excluded_source(&locked);
        let report = run.finish();
        verify_eq!(report.summary.indexed, 1);
        verify_eq!(report.summary.locked, 1);
        verify_eq!(report.summary.skipped, 0);
        verify_eq!(report.diagnostics.len(), 1);
        verify_eq!(
            report.diagnostics[0].failure_kind,
            IngestFailureKind::SourceLocked
        );
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

    /// Exercise actual file changes through the pinned parser and snapshot
    /// guards. The first error must not hide the other files' different causes
    /// or turn the healthy sibling into a failure at the scan root.
    #[test]
    fn gh559_mixed_codex_read_failures_keep_source_identity_and_retry() -> TestResult {
        use std::io::Write;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let temp = tempdir()?;
        let sessions = temp.path().join("sessions");
        std::fs::create_dir(&sessions)?;
        let moving = sessions.join("rollout-a-growing.jsonl");
        let invalid = sessions.join("rollout-b-invalid.jsonl");
        let missing = sessions.join("rollout-c-missing.jsonl");
        let parked = sessions.join("retained-source.txt");
        let healthy = sessions.join("rollout-d-healthy.jsonl");
        let record = format!(
            "{}\n",
            json!({"type":"response_item","payload":{"role":"user","content":"gh559privatebody"}})
        );
        let appended_record = format!(
            "{}\n",
            json!({"type":"response_item","payload":{"role":"assistant","content":"gh559privatetail"}})
        );
        for path in [&moving, &missing, &healthy] {
            std::fs::write(path, &record)?;
        }
        std::fs::write(&invalid, b"\xff\n")?;

        let admitted = Arc::new(AtomicBool::new(false));
        let appended = Arc::new(AtomicBool::new(false));
        let ctx = ScanContext::with_roots(
            temp.path().join("data"),
            vec![ScanRoot::local(sessions.clone())],
            None,
        )
        .with_progress_tick(Arc::new({
            let admitted = Arc::clone(&admitted);
            let appended = Arc::clone(&appended);
            let moving = moving.clone();
            move || {
                // The primary reader's first progress tick runs after it has
                // opened its bounded snapshot. No thread timing is required.
                if admitted.swap(false, Ordering::SeqCst) {
                    std::fs::OpenOptions::new()
                        .append(true)
                        .open(&moving)
                        .expect("open growing fixture")
                        .write_all(appended_record.as_bytes())
                        .expect("append to admitted source");
                    appended.store(true, Ordering::SeqCst);
                }
            }
        }));
        let connector = crate::connectors::codex::CodexConnector::new();
        let mut run = ConnectorIngestRun::begin("codex", temp.path(), &ctx, &[]);
        let mut should_scan = |source: &DiscoveredSourceFile| {
            admitted.store(source.source_path == moving, Ordering::SeqCst);
            if source.source_path == missing {
                std::fs::rename(&missing, &parked).expect("park selected fixture");
            }
            true
        };
        let error = connector
            .scan_with_source_boundaries(
                &ctx,
                &mut franken_agent_detection::SourceScanHooks {
                    should_scan_source: Some(&mut should_scan),
                    on_source_complete: None,
                },
                &mut |mut conversation| {
                    run.observe_conversation(&mut conversation);
                    Ok(())
                },
            )
            .expect_err("moving and invalid inputs must leave incomplete coverage")
            .context("outer caller context");
        verify!(appended.load(Ordering::SeqCst));
        run.observe_connector_scan_error(&sessions, &error);
        let report = run.finish();
        verify_eq!(report.summary.indexed, 1);
        verify_eq!(report.summary.skipped, 3);
        verify_eq!(report.summary.discovered, 0);
        verify_eq!(report.summary.total(), 4);
        verify_eq!(report.diagnostics.len(), 3);
        for (path, kind, retryable) in [
            (&moving, IngestFailureKind::SourceChanged, true),
            (&invalid, IngestFailureKind::UnparseableSource, false),
            (&missing, IngestFailureKind::SourceMissing, true),
        ] {
            let diagnostic = report
                .diagnostics
                .iter()
                .find(|diagnostic| diagnostic.source_path == path.to_string_lossy())
                .ok_or("missing source-specific diagnostic")?;
            verify_eq!(diagnostic.failure_kind, kind);
            verify_eq!(diagnostic.retryable, retryable);
            verify!(!diagnostic.safe_next_action.contains("check permissions"));
            verify_eq!(diagnostic.disposition, SourceIngestDisposition::Skipped);
        }
        verify!(!serde_json::to_string(&report)?.contains("gh559private"));

        // Settling the writer and restoring the bad sources clears the next
        // invocation, while keeping all complete records from the append.
        std::fs::write(&invalid, &record)?;
        std::fs::rename(&parked, &missing)?;
        let mut settled = ctx;
        settled.progress_tick = None;
        let conversations = connector.scan(&settled)?;
        verify_eq!(conversations.len(), 4);
        verify_eq!(
            conversations
                .iter()
                .find(|conversation| conversation.source_path == moving)
                .ok_or("growing source did not recover")?
                .messages
                .len(),
            2
        );
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
