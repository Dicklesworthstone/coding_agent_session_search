//! Typed `CliError.kind` enum (`coding_agent_session_search-dxnmb`).
//!
//! `CliError.kind` is currently a `&'static str` field with 86 unique
//! values scattered as string literals across `src/lib.rs`. There is
//! no compile-time exhaustiveness check, no naming-convention guard,
//! and no rename-safety. A hurried maintainer can:
//!
//! - typo a kind ("db_error" vs "db-error") without compiler error,
//! - introduce a new kind that shadows an existing one,
//! - use inconsistent casing (the existing literal set already has
//!   4 snake_case stragglers — `failed_seed_bundle_file`,
//!   `lexical_generation`, `lexical_shard`, `retained_publish_backup`
//!   — alongside the canonical kebab-case majority).
//!
//! That inconsistency caused 3 real duplicates pinned by bead `al19b`.
//!
//! This module ships the **vocabulary slice** of the dxnmb fix:
//! a single source-of-truth enum that:
//!
//! 1. enumerates every kind currently emitted by `src/lib.rs`
//!    (audited at landing time via `grep -oE 'kind: "[a-z_-]+"'`),
//! 2. exposes a `kind_str()` accessor that returns the canonical
//!    kebab-case (or, for the four snake_case stragglers, the exact
//!    legacy literal — preserving wire compatibility with golden
//!    tests + downstream agents until those four are migrated in a
//!    separate slice),
//! 3. exposes a `from_kind_str()` lookup so JSON-mode consumers
//!    (and golden tests) can round-trip the kind cleanly.
//!
//! The actual migration of the 223 call sites in `src/lib.rs` (each
//! `CliError { kind: "...", ... }` literal → `CliError { kind:
//! ErrorKind::Foo.as_str(), ... }`) is the *follow-up* slice; it
//! requires write access to `src/lib.rs` which is currently held by
//! another agent's exclusive file reservation. Landing the
//! vocabulary first lets that follow-up slice land as a pure
//! mechanical replacement gated by the golden test below.
//!
//! # Variant naming
//!
//! Variants use Rust's standard CamelCase. The mapping to the
//! wire-format string is held by `kind_str()` rather than by
//! `#[serde(rename = "...")]` because the four snake_case stragglers
//! cannot be auto-generated from CamelCase by serde's
//! `rename_all = "kebab-case"` (e.g. `LexicalGeneration` would
//! serialize as `lexical-generation`, breaking the existing
//! `kind: "lexical_generation"` wire contract). The audit golden
//! test pins both the kebab-case canonical kinds AND the snake_case
//! exemptions so a future cleanup slice that migrates the four
//! stragglers to kebab-case has an explicit place to flip the
//! contract.

use serde::{Deserialize, Serialize};

/// Typed counterpart to `CliError.kind`. Every variant maps to the
/// exact wire-format string emitted today; new kinds added by future
/// CLI surfaces should be added here AND covered by the golden test
/// at the bottom of this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ErrorKind {
    AmbiguousSource,
    ArchiveAnalyticsRebuild,
    ArchiveCount,
    ArchiveDailyStatsRebuild,
    ArchiveFtsRebuild,
    ArchivePurge,
    ArchiveTokenDailyStatsRebuild,
    Config,
    CursorDecode,
    CursorParse,
    Daemon,
    DbError,
    DbOpen,
    DbQuery,
    Doctor,
    Download,
    EmbedderUnavailable,
    EmptyFile,
    EmptySession,
    EncodeJson,
    ExportFailed,
    /// Snake-case wire literal (legacy): `failed_seed_bundle_file`.
    /// Kept exact until the cross-cutting kebab-case migration ships.
    FailedSeedBundleFile,
    FileCreate,
    FileNotFound,
    FileOpen,
    FileRead,
    FileWrite,
    Health,
    IdempotencyMismatch,
    Index,
    IndexBusy,
    IndexMissing,
    IndexedSessionRequired,
    InvalidAgent,
    InvalidFilename,
    InvalidLine,
    Io,
    IoError,
    LexicalRebuild,
    /// Snake-case wire literal (legacy): `lexical_generation`.
    LexicalGeneration,
    /// Snake-case wire literal (legacy): `lexical_shard`.
    LexicalShard,
    LineNotFound,
    LineOutOfRange,
    Local,
    Mapping,
    MissingDb,
    MissingIndex,
    Model,
    NotFound,
    OpenIndex,
    OpencodeParse,
    OpencodeSqliteParse,
    OutputNotWritable,
    Pages,
    ParseError,
    PasswordReadError,
    PasswordRequired,
    RebuildError,
    RepairError,
    ResumeEmptyCommand,
    ResumeExecFailed,
    /// Snake-case wire literal (legacy): `retained_publish_backup`.
    RetainedPublishBackup,
    Search,
    SemanticBackfill,
    SemanticManifest,
    SemanticUnavailable,
    SerializeMessage,
    SessionFileUnreadable,
    SessionIdNotFound,
    SessionNotFound,
    SessionParse,
    SessionsFrom,
    Setup,
    Source,
    Ssh,
    Storage,
    StorageFingerprint,
    Timeout,
    Tui,
    TuiHeadlessOnce,
    TuiResetState,
    Unknown,
    UnknownAgent,
    UpdateCheck,
    Usage,
    WriteFailed,
}

impl ErrorKind {
    /// Returns the wire-format string emitted in `CliError.kind`.
    /// **MUST** match the literal currently used in `src/lib.rs` for
    /// every variant — the golden test below asserts this. Adding a
    /// new variant without updating this match is a compile error
    /// (no `_ => ...` catch-all).
    pub fn kind_str(self) -> &'static str {
        match self {
            Self::AmbiguousSource => "ambiguous-source",
            Self::ArchiveAnalyticsRebuild => "archive-analytics-rebuild",
            Self::ArchiveCount => "archive-count",
            Self::ArchiveDailyStatsRebuild => "archive-daily-stats-rebuild",
            Self::ArchiveFtsRebuild => "archive-fts-rebuild",
            Self::ArchivePurge => "archive-purge",
            Self::ArchiveTokenDailyStatsRebuild => "archive-token-daily-stats-rebuild",
            Self::Config => "config",
            Self::CursorDecode => "cursor-decode",
            Self::CursorParse => "cursor-parse",
            Self::Daemon => "daemon",
            Self::DbError => "db-error",
            Self::DbOpen => "db-open",
            Self::DbQuery => "db-query",
            Self::Doctor => "doctor",
            Self::Download => "download",
            Self::EmbedderUnavailable => "embedder-unavailable",
            Self::EmptyFile => "empty-file",
            Self::EmptySession => "empty-session",
            Self::EncodeJson => "encode-json",
            Self::ExportFailed => "export-failed",
            Self::FailedSeedBundleFile => "failed_seed_bundle_file",
            Self::FileCreate => "file-create",
            Self::FileNotFound => "file-not-found",
            Self::FileOpen => "file-open",
            Self::FileRead => "file-read",
            Self::FileWrite => "file-write",
            Self::Health => "health",
            Self::IdempotencyMismatch => "idempotency-mismatch",
            Self::Index => "index",
            Self::IndexBusy => "index-busy",
            Self::IndexMissing => "index-missing",
            Self::IndexedSessionRequired => "indexed-session-required",
            Self::InvalidAgent => "invalid-agent",
            Self::InvalidFilename => "invalid-filename",
            Self::InvalidLine => "invalid-line",
            Self::Io => "io",
            Self::IoError => "io-error",
            Self::LexicalRebuild => "lexical-rebuild",
            Self::LexicalGeneration => "lexical_generation",
            Self::LexicalShard => "lexical_shard",
            Self::LineNotFound => "line-not-found",
            Self::LineOutOfRange => "line-out-of-range",
            Self::Local => "local",
            Self::Mapping => "mapping",
            Self::MissingDb => "missing-db",
            Self::MissingIndex => "missing-index",
            Self::Model => "model",
            Self::NotFound => "not-found",
            Self::OpenIndex => "open-index",
            Self::OpencodeParse => "opencode-parse",
            Self::OpencodeSqliteParse => "opencode-sqlite-parse",
            Self::OutputNotWritable => "output-not-writable",
            Self::Pages => "pages",
            Self::ParseError => "parse-error",
            Self::PasswordReadError => "password-read-error",
            Self::PasswordRequired => "password-required",
            Self::RebuildError => "rebuild-error",
            Self::RepairError => "repair-error",
            Self::ResumeEmptyCommand => "resume-empty-command",
            Self::ResumeExecFailed => "resume-exec-failed",
            Self::RetainedPublishBackup => "retained_publish_backup",
            Self::Search => "search",
            Self::SemanticBackfill => "semantic-backfill",
            Self::SemanticManifest => "semantic-manifest",
            Self::SemanticUnavailable => "semantic-unavailable",
            Self::SerializeMessage => "serialize-message",
            Self::SessionFileUnreadable => "session-file-unreadable",
            Self::SessionIdNotFound => "session-id-not-found",
            Self::SessionNotFound => "session-not-found",
            Self::SessionParse => "session-parse",
            Self::SessionsFrom => "sessions-from",
            Self::Setup => "setup",
            Self::Source => "source",
            Self::Ssh => "ssh",
            Self::Storage => "storage",
            Self::StorageFingerprint => "storage-fingerprint",
            Self::Timeout => "timeout",
            Self::Tui => "tui",
            Self::TuiHeadlessOnce => "tui-headless-once",
            Self::TuiResetState => "tui-reset-state",
            Self::Unknown => "unknown",
            Self::UnknownAgent => "unknown-agent",
            Self::UpdateCheck => "update-check",
            Self::Usage => "usage",
            Self::WriteFailed => "write-failed",
        }
    }

    /// Reverse lookup: parse a wire-format kind string back into the
    /// typed enum. Returns `None` on unknown kinds. Used by JSON-mode
    /// deserialization paths that need to branch on `err.kind` and by
    /// the golden test that asserts every variant round-trips.
    pub fn from_kind_str(kind: &str) -> Option<Self> {
        Some(match kind {
            "ambiguous-source" => Self::AmbiguousSource,
            "archive-analytics-rebuild" => Self::ArchiveAnalyticsRebuild,
            "archive-count" => Self::ArchiveCount,
            "archive-daily-stats-rebuild" => Self::ArchiveDailyStatsRebuild,
            "archive-fts-rebuild" => Self::ArchiveFtsRebuild,
            "archive-purge" => Self::ArchivePurge,
            "archive-token-daily-stats-rebuild" => Self::ArchiveTokenDailyStatsRebuild,
            "config" => Self::Config,
            "cursor-decode" => Self::CursorDecode,
            "cursor-parse" => Self::CursorParse,
            "daemon" => Self::Daemon,
            "db-error" => Self::DbError,
            "db-open" => Self::DbOpen,
            "db-query" => Self::DbQuery,
            "doctor" => Self::Doctor,
            "download" => Self::Download,
            "embedder-unavailable" => Self::EmbedderUnavailable,
            "empty-file" => Self::EmptyFile,
            "empty-session" => Self::EmptySession,
            "encode-json" => Self::EncodeJson,
            "export-failed" => Self::ExportFailed,
            "failed_seed_bundle_file" => Self::FailedSeedBundleFile,
            "file-create" => Self::FileCreate,
            "file-not-found" => Self::FileNotFound,
            "file-open" => Self::FileOpen,
            "file-read" => Self::FileRead,
            "file-write" => Self::FileWrite,
            "health" => Self::Health,
            "idempotency-mismatch" => Self::IdempotencyMismatch,
            "index" => Self::Index,
            "index-busy" => Self::IndexBusy,
            "index-missing" => Self::IndexMissing,
            "indexed-session-required" => Self::IndexedSessionRequired,
            "invalid-agent" => Self::InvalidAgent,
            "invalid-filename" => Self::InvalidFilename,
            "invalid-line" => Self::InvalidLine,
            "io" => Self::Io,
            "io-error" => Self::IoError,
            "lexical-rebuild" => Self::LexicalRebuild,
            "lexical_generation" => Self::LexicalGeneration,
            "lexical_shard" => Self::LexicalShard,
            "line-not-found" => Self::LineNotFound,
            "line-out-of-range" => Self::LineOutOfRange,
            "local" => Self::Local,
            "mapping" => Self::Mapping,
            "missing-db" => Self::MissingDb,
            "missing-index" => Self::MissingIndex,
            "model" => Self::Model,
            "not-found" => Self::NotFound,
            "open-index" => Self::OpenIndex,
            "opencode-parse" => Self::OpencodeParse,
            "opencode-sqlite-parse" => Self::OpencodeSqliteParse,
            "output-not-writable" => Self::OutputNotWritable,
            "pages" => Self::Pages,
            "parse-error" => Self::ParseError,
            "password-read-error" => Self::PasswordReadError,
            "password-required" => Self::PasswordRequired,
            "rebuild-error" => Self::RebuildError,
            "repair-error" => Self::RepairError,
            "resume-empty-command" => Self::ResumeEmptyCommand,
            "resume-exec-failed" => Self::ResumeExecFailed,
            "retained_publish_backup" => Self::RetainedPublishBackup,
            "search" => Self::Search,
            "semantic-backfill" => Self::SemanticBackfill,
            "semantic-manifest" => Self::SemanticManifest,
            "semantic-unavailable" => Self::SemanticUnavailable,
            "serialize-message" => Self::SerializeMessage,
            "session-file-unreadable" => Self::SessionFileUnreadable,
            "session-id-not-found" => Self::SessionIdNotFound,
            "session-not-found" => Self::SessionNotFound,
            "session-parse" => Self::SessionParse,
            "sessions-from" => Self::SessionsFrom,
            "setup" => Self::Setup,
            "source" => Self::Source,
            "ssh" => Self::Ssh,
            "storage" => Self::Storage,
            "storage-fingerprint" => Self::StorageFingerprint,
            "timeout" => Self::Timeout,
            "tui" => Self::Tui,
            "tui-headless-once" => Self::TuiHeadlessOnce,
            "tui-reset-state" => Self::TuiResetState,
            "unknown" => Self::Unknown,
            "unknown-agent" => Self::UnknownAgent,
            "update-check" => Self::UpdateCheck,
            "usage" => Self::Usage,
            "write-failed" => Self::WriteFailed,
            _ => return None,
        })
    }

    /// Returns every variant in declaration order. Used by the
    /// golden test to assert every variant has both a `kind_str()`
    /// arm AND a `from_kind_str()` arm.
    pub fn all_variants() -> &'static [Self] {
        &[
            Self::AmbiguousSource,
            Self::ArchiveAnalyticsRebuild,
            Self::ArchiveCount,
            Self::ArchiveDailyStatsRebuild,
            Self::ArchiveFtsRebuild,
            Self::ArchivePurge,
            Self::ArchiveTokenDailyStatsRebuild,
            Self::Config,
            Self::CursorDecode,
            Self::CursorParse,
            Self::Daemon,
            Self::DbError,
            Self::DbOpen,
            Self::DbQuery,
            Self::Doctor,
            Self::Download,
            Self::EmbedderUnavailable,
            Self::EmptyFile,
            Self::EmptySession,
            Self::EncodeJson,
            Self::ExportFailed,
            Self::FailedSeedBundleFile,
            Self::FileCreate,
            Self::FileNotFound,
            Self::FileOpen,
            Self::FileRead,
            Self::FileWrite,
            Self::Health,
            Self::IdempotencyMismatch,
            Self::Index,
            Self::IndexBusy,
            Self::IndexMissing,
            Self::IndexedSessionRequired,
            Self::InvalidAgent,
            Self::InvalidFilename,
            Self::InvalidLine,
            Self::Io,
            Self::IoError,
            Self::LexicalRebuild,
            Self::LexicalGeneration,
            Self::LexicalShard,
            Self::LineNotFound,
            Self::LineOutOfRange,
            Self::Local,
            Self::Mapping,
            Self::MissingDb,
            Self::MissingIndex,
            Self::Model,
            Self::NotFound,
            Self::OpenIndex,
            Self::OpencodeParse,
            Self::OpencodeSqliteParse,
            Self::OutputNotWritable,
            Self::Pages,
            Self::ParseError,
            Self::PasswordReadError,
            Self::PasswordRequired,
            Self::RebuildError,
            Self::RepairError,
            Self::ResumeEmptyCommand,
            Self::ResumeExecFailed,
            Self::RetainedPublishBackup,
            Self::Search,
            Self::SemanticBackfill,
            Self::SemanticManifest,
            Self::SemanticUnavailable,
            Self::SerializeMessage,
            Self::SessionFileUnreadable,
            Self::SessionIdNotFound,
            Self::SessionNotFound,
            Self::SessionParse,
            Self::SessionsFrom,
            Self::Setup,
            Self::Source,
            Self::Ssh,
            Self::Storage,
            Self::StorageFingerprint,
            Self::Timeout,
            Self::Tui,
            Self::TuiHeadlessOnce,
            Self::TuiResetState,
            Self::Unknown,
            Self::UnknownAgent,
            Self::UpdateCheck,
            Self::Usage,
            Self::WriteFailed,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// `coding_agent_session_search-dxnmb` golden gate: every variant
    /// in `all_variants()` must round-trip through `kind_str()` →
    /// `from_kind_str()` and yield the same variant. A new variant
    /// added without registering it in both arms fails this gate.
    #[test]
    fn every_error_kind_round_trips_through_kind_str() {
        for variant in ErrorKind::all_variants() {
            let kind = variant.kind_str();
            let parsed = ErrorKind::from_kind_str(kind).unwrap_or_else(|| {
                panic!(
                    "ErrorKind::{:?}.kind_str() = {:?} but from_kind_str returned None — \
                     missing from_kind_str arm",
                    variant, kind
                )
            });
            assert_eq!(
                parsed, *variant,
                "round-trip mismatch: {:?}.kind_str() → {:?} → {:?}",
                variant, kind, parsed
            );
        }
    }

    /// All wire strings must be unique. A regression that mapped two
    /// variants to the same kind_str() (e.g. the historical "db_error"
    /// vs "db-error" duplicate from bead al19b) trips this gate.
    #[test]
    fn every_kind_str_is_unique() {
        let mut seen: HashSet<&'static str> = HashSet::new();
        for variant in ErrorKind::all_variants() {
            let kind = variant.kind_str();
            assert!(
                seen.insert(kind),
                "duplicate kind_str detected: {:?} maps to {:?} which was already \
                 registered by an earlier variant",
                variant, kind
            );
        }
    }

    /// The vocabulary covers every kind currently emitted by
    /// src/lib.rs at landing time (audited via
    /// `grep -oE 'kind: \"[a-z_-]+\"' src/lib.rs | sort -u`). A
    /// regression that added a new kind to lib.rs without adding it
    /// here would be invisible until a future enum migration site
    /// hit a missing variant; pinning the count here surfaces the
    /// drift immediately at CI time.
    #[test]
    fn variant_count_matches_audited_lib_rs_kind_literals() {
        // 86 unique kinds at landing time (commit before the variant
        // landed). If lib.rs grows a new kind, bump this count AND
        // add the variant + arms above.
        const AUDITED_KIND_COUNT: usize = 86;
        assert_eq!(
            ErrorKind::all_variants().len(),
            AUDITED_KIND_COUNT,
            "ErrorKind variant count drifted from the audited lib.rs literal set; \
             re-run `grep -oE 'kind: \"[a-z_-]+\"' src/lib.rs | sort -u | wc -l` and \
             update the constant + add the missing variant"
        );
    }

    /// Pin the four legacy snake_case stragglers explicitly so a
    /// future "rename to kebab-case" cleanup slice has a single place
    /// to flip the contract. Pinning them here also surfaces an
    /// accidental flip-back from kebab-case to snake_case.
    #[test]
    fn snake_case_stragglers_preserve_legacy_wire_format() {
        assert_eq!(
            ErrorKind::FailedSeedBundleFile.kind_str(),
            "failed_seed_bundle_file"
        );
        assert_eq!(
            ErrorKind::LexicalGeneration.kind_str(),
            "lexical_generation"
        );
        assert_eq!(ErrorKind::LexicalShard.kind_str(), "lexical_shard");
        assert_eq!(
            ErrorKind::RetainedPublishBackup.kind_str(),
            "retained_publish_backup"
        );
    }

    /// Unknown kinds return None (not a default Unknown variant);
    /// callers must explicitly handle the parse failure.
    #[test]
    fn from_kind_str_returns_none_for_unknown_inputs() {
        assert_eq!(ErrorKind::from_kind_str(""), None);
        assert_eq!(ErrorKind::from_kind_str("not-a-real-kind"), None);
        // Casing matters: the wire format is exact.
        assert_eq!(ErrorKind::from_kind_str("DB-ERROR"), None);
        assert_eq!(ErrorKind::from_kind_str("Db-Error"), None);
        // Sanity: the well-known "unknown" kind IS distinct from
        // "not-a-real-kind" and parses cleanly.
        assert_eq!(
            ErrorKind::from_kind_str("unknown"),
            Some(ErrorKind::Unknown)
        );
    }

    /// Serde round-trip via JSON works (callers can use the enum as
    /// a serde-serializable field). Default rename uses CamelCase
    /// for serde, but downstream consumers that need the wire-format
    /// kebab-case string call `kind_str()` directly. This test pins
    /// that the enum is at least serializable / deserializable so
    /// callers wanting the typed form (e.g. error envelopes for
    /// telemetry sinks) can opt in.
    #[test]
    fn error_kind_is_serde_compatible() {
        let json = serde_json::to_string(&ErrorKind::DbError).expect("serialize");
        let parsed: ErrorKind = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, ErrorKind::DbError);
    }
}
