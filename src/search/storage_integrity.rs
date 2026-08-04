// Some contract helpers are projected only by fleet/support-bundle follow-ons;
// retain them while the live doctor/status/search probes share this module.
#![allow(dead_code)]

//! Storage-integrity diagnostic taxonomy and JSON contract (bead
//! cass-fleet-resilience-20260608-uojcg.14.1).
//!
//! Storage failures surface today as scattered symptoms — OpenRead cursor
//! errors, integrity-check failures, stale WAL/SHM sidecars, schema-version
//! drift, busy locks, FTS metadata mismatch, legacy-DB readability problems,
//! unsafe SQL construction, and zero-result regressions — with no shared
//! vocabulary. Without one, doctor/status give generic "stale index" advice
//! when the operator actually needs archive-risk handling.
//!
//! This module defines the single contract every storage surface (health,
//! status, doctor, triage, fleet, search metadata, support bundles) projects:
//! a [`StorageState`], a [`SourceOfTruthRisk`], an [`ArchiveReadability`],
//! and the [`StorageCheck`]s attempted (each carrying `elapsed_ms`,
//! `timed_out`, an optional `skipped_reason`, and whether it is read-only).
//! [`StorageIntegrityReport::derive`] computes the source-of-truth risk from
//! the state so robot JSON and human summaries agree.
//!
//! The schema and its dedicated read-only refinements live together here so
//! doctor/status/search cannot drift into different classifications. Probe SQL
//! uses bound parameters for variable values and adds no new rusqlite code.
//! All enums serialize as snake_case, matching the readiness vocabulary; the
//! associated root-cause family reuses
//! [`crate::root_cause_taxonomy::RootCauseFamily`].

use serde::{Deserialize, Serialize};

use crate::root_cause_taxonomy::RootCauseFamily;

/// Results from the dedicated, read-only probes required to distinguish the
/// four storage states that db-open/index-readiness signals alone cannot prove.
///
/// In particular, `busy_or_locked` is set only after observing a typed
/// `FrankenError` classified by `contention_diagnostics`; a generic CLI
/// `retryable` hint is deliberately not an input. Likewise, sidecar presence
/// is not enough for `wal_sidecar_suspect`: the probe requires an orphaned SHM
/// or a structurally malformed non-empty WAL.
#[derive(Debug, Clone, Default)]
pub(crate) struct DedicatedStorageProbe {
    pub busy_or_locked: bool,
    pub schema_drift: bool,
    pub legacy_interop_failed: bool,
    pub wal_sidecar_suspect: bool,
    /// The main file has a structurally plausible SQLite header. This is not
    /// an integrity verdict; it only prevents a broken arbitrary file from
    /// being relabeled when a malformed sidecar happens to coexist with it.
    pub main_db_header_plausible: bool,
    pub checks_attempted: Vec<StorageCheck>,
}

/// The storage-engine integrity state. `Ok` and the failure modes the report
/// enumerated; `UnknownDeferred` is the explicit fallback when a check could
/// not run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StorageState {
    /// All attempted checks passed.
    Ok,
    /// Only derived assets drifted; the canonical DB itself is intact.
    DerivedOnlyDrift,
    /// The DB is busy or locked by another writer.
    BusyOrLocked,
    /// A WAL/SHM sidecar is suspect (stale, orphaned, or size-inconsistent).
    WalSidecarSuspect,
    /// The on-disk schema version drifted from the expected contract.
    SchemaDrift,
    /// A cursor/OpenRead operation failed.
    OpenreadFailed,
    /// An integrity / `PRAGMA integrity_check`-class check failed.
    IntegrityFailed,
    /// A legacy database could not be read by the current engine.
    LegacyInteropFailed,
    /// FTS metadata is missing or inconsistent.
    FtsMetadataFailed,
    /// An unsafe SQL construction / query shape (bind-risk) was detected.
    UnsafeSqlShape,
    /// A check could not run and the verdict is deferred — never omit it.
    UnknownDeferred,
    /// The archive opened and bounded reads worked, but NO structural
    /// integrity probe ran on this surface and no still-valid cached
    /// attestation exists (#331). Distinct from `Ok`, which is reserved for
    /// a passed structural check (live or fingerprint-matched cached): a
    /// lightweight surface that only proved "db_open succeeded" must not
    /// synthesize a definitive `ok` verdict from openability alone.
    Unchecked,
}

/// Risk to the canonical source of truth implied by the storage state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SourceOfTruthRisk {
    None,
    Low,
    Medium,
    High,
    Unknown,
}

/// Whether the canonical archive could be read during the diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ArchiveReadability {
    Readable,
    PartiallyReadable,
    Unreadable,
    NotChecked,
    TimedOut,
}

/// One diagnostic check that was attempted (or skipped).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StorageCheck {
    /// Stable check name (snake_case), e.g. `open_read`, `integrity_check`.
    pub name: String,
    pub elapsed_ms: i64,
    pub timed_out: bool,
    /// Why the check was skipped, when it was. `None` when it ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped_reason: Option<String>,
    /// Whether the check only reads (never mutates) the archive — true for
    /// every diagnostic probe; repairs are not checks.
    pub read_only: bool,
}

impl StorageCheck {
    /// A read-only check that ran to completion.
    pub(crate) fn ran(name: impl Into<String>, elapsed_ms: i64) -> Self {
        Self {
            name: name.into(),
            elapsed_ms,
            timed_out: false,
            skipped_reason: None,
            read_only: true,
        }
    }

    /// A read-only check that timed out.
    pub(crate) fn timed_out(name: impl Into<String>, elapsed_ms: i64) -> Self {
        Self {
            name: name.into(),
            elapsed_ms,
            timed_out: true,
            skipped_reason: None,
            read_only: true,
        }
    }

    /// A check that was skipped with a reason.
    pub(crate) fn skipped(name: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            elapsed_ms: 0,
            timed_out: false,
            skipped_reason: Some(reason.into()),
            read_only: true,
        }
    }
}

impl StorageState {
    /// The default source-of-truth risk implied by this state. Conservative:
    /// anything that prevents trusting/reading the canonical rows is high;
    /// derived-only / FTS issues are low because the canonical rows survive.
    pub(crate) fn default_risk(self) -> SourceOfTruthRisk {
        match self {
            Self::Ok => SourceOfTruthRisk::None,
            Self::FtsMetadataFailed | Self::DerivedOnlyDrift | Self::BusyOrLocked => {
                SourceOfTruthRisk::Low
            }
            Self::WalSidecarSuspect
            | Self::SchemaDrift
            | Self::LegacyInteropFailed
            | Self::UnsafeSqlShape => SourceOfTruthRisk::Medium,
            Self::OpenreadFailed | Self::IntegrityFailed => SourceOfTruthRisk::High,
            Self::UnknownDeferred | Self::Unchecked => SourceOfTruthRisk::Unknown,
        }
    }

    /// The root-cause family this state attributes to. Storage states are
    /// frankensqlite-storage except the explicit deferred fallback.
    pub(crate) fn root_cause_family(self) -> RootCauseFamily {
        match self {
            Self::UnknownDeferred | Self::Unchecked => RootCauseFamily::Unknown,
            _ => RootCauseFamily::FrankensqliteStorage,
        }
    }

    /// Whether ordinary search can still trust the canonical rows.
    pub(crate) fn canonical_trustworthy(self) -> bool {
        !matches!(self, Self::OpenreadFailed | Self::IntegrityFailed)
    }
}

/// One diagnostic check that a surface deliberately did NOT attempt, with the
/// reason — the explicit probe-coverage complement of `checks_attempted`
/// (#331: "a false definitive success is worse than an explicit unchecked").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StorageCheckNotAttempted {
    pub name: String,
    pub reason: String,
}

/// The storage-integrity report every surface projects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StorageIntegrityReport {
    pub storage_state: StorageState,
    pub source_of_truth_risk: SourceOfTruthRisk,
    pub archive_readability: ArchiveReadability,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks_attempted: Vec<StorageCheck>,
    /// Checks this surface knows about but deliberately did not run (#331),
    /// e.g. `quick_check` on the lightweight status budget.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks_not_attempted: Vec<StorageCheckNotAttempted>,
    /// Where the structural-integrity verdict came from: `live` (a probe ran
    /// on this surface), `cached` (a fingerprint-matched attestation from an
    /// earlier doctor/index probe), or `none` (no structural evidence).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attestation_source: Option<String>,
    /// Wall-clock ms timestamp of the cached attestation, when one was used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attested_at_ms: Option<i64>,
    /// Depth of the attested structural check (`quick_check` /
    /// `integrity_check`), when a cached attestation was used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attestation_check_depth: Option<String>,
    /// Stable, non-sensitive digest of the DB/WAL stat tuple the attestation
    /// covers. This lets operators correlate a cached verdict with the exact
    /// archive generation without exposing archive paths or content (#331).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attested_db_fingerprint: Option<String>,
}

impl StorageIntegrityReport {
    /// Build a report, deriving `source_of_truth_risk` from the state so
    /// robot JSON and human summaries never disagree.
    pub(crate) fn derive(
        state: StorageState,
        archive_readability: ArchiveReadability,
        checks_attempted: Vec<StorageCheck>,
    ) -> Self {
        Self {
            storage_state: state,
            source_of_truth_risk: state.default_risk(),
            archive_readability,
            checks_attempted,
            checks_not_attempted: Vec::new(),
            attestation_source: None,
            attested_at_ms: None,
            attestation_check_depth: None,
            attested_db_fingerprint: None,
        }
    }

    /// A one-line human summary built from the SAME enum vocabulary the
    /// robot JSON serializes, so the two surfaces stay in lockstep.
    pub(crate) fn human_summary(&self) -> String {
        format!(
            "storage {} (source-of-truth risk {}, archive {})",
            serde_plain_label(self.storage_state),
            serde_plain_label(self.source_of_truth_risk),
            serde_plain_label(self.archive_readability),
        )
    }

    /// Whether every attempted check was read-only (a pure diagnostic pass
    /// never mutated the archive).
    pub(crate) fn all_checks_read_only(&self) -> bool {
        self.checks_attempted.iter().all(|c| c.read_only)
    }
}

/// The common read-only signals `cass doctor --check` gathers about the
/// canonical archive. This base classifier covers db-open, integrity, and
/// derived-index drift; [`probe_dedicated_storage_state`] supplies the four
/// probe-dependent refinements and [`apply_dedicated_storage_probe`] overlays
/// them with explicit precedence.
///
/// `FtsMetadataFailed` is not derived from these signals. A doctor run that
/// only sees a generic open/integrity failure still honestly reports the coarser
/// [`StorageState::OpenreadFailed`] / [`StorageState::IntegrityFailed`] rather
/// than over-claiming a precise cause it cannot prove. (`FtsMetadataFailed` is
/// deferred because doctor's `fts_table` probe cannot distinguish a *benign*
/// absent in-DB `fts_messages` shadow — which it reports as `pass`, since
/// lexical search falls back to the Tantivy index — from a genuinely corrupt
/// one; deriving a failure from the benign case would contradict that `pass`.)
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct DoctorStorageSignals {
    /// The canonical `agent_search.db` file is present on disk.
    pub db_file_present: bool,
    /// The data dir has never been indexed (no archive is expected yet).
    pub not_initialized: bool,
    /// The read-only opener could not open the present DB file.
    pub db_open_failed: bool,
    /// The bounded archive probe hit its deadline, so the verdict is deferred.
    pub probe_timed_out: bool,
    /// A `PRAGMA quick_check` / `integrity_check`-class probe reported failure.
    pub integrity_failed: bool,
    /// The DB opened but its row/integrity probe could not complete (a read
    /// failed), so integrity could not be confirmed either way.
    pub integrity_unverified: bool,
    /// The derived lexical (Tantivy) index drifted from an intact DB — empty,
    /// missing, or unreadable while the canonical rows survive.
    pub lexical_index_drifted: bool,
}

impl DoctorStorageSignals {
    /// Derive the `(StorageState, ArchiveReadability)` pair these signals imply.
    /// Conservative and total: read failures dominate, an unverifiable probe is
    /// `UnknownDeferred` (never silently "ok"), and a healthy canonical DB whose
    /// only problem is a drifted *derived* asset stays low-risk
    /// `DerivedOnlyDrift`. Precedence runs most-severe first so a DB that fails
    /// to open is never masked by a downstream derived-asset signal.
    pub(crate) fn classify(self) -> (StorageState, ArchiveReadability) {
        if self.db_file_present {
            if self.db_open_failed {
                (StorageState::OpenreadFailed, ArchiveReadability::Unreadable)
            } else if self.probe_timed_out {
                (StorageState::UnknownDeferred, ArchiveReadability::TimedOut)
            } else if self.integrity_failed {
                (
                    StorageState::IntegrityFailed,
                    ArchiveReadability::PartiallyReadable,
                )
            } else if self.integrity_unverified {
                // The DB opened but a read failed mid-probe; treat as an
                // integrity failure with an unreadable verdict rather than
                // claiming health we never confirmed.
                (
                    StorageState::IntegrityFailed,
                    ArchiveReadability::Unreadable,
                )
            } else if self.lexical_index_drifted {
                (StorageState::DerivedOnlyDrift, ArchiveReadability::Readable)
            } else {
                // Canonical DB opened, integrity passed, derived assets in sync.
                // An absent in-DB `fts_messages` shadow is intentionally NOT
                // escalated here (see the struct docs): doctor reports it as a
                // benign `pass` because lexical search falls back to Tantivy, so
                // claiming `FtsMetadataFailed` would contradict that verdict.
                (StorageState::Ok, ArchiveReadability::Readable)
            }
        } else if self.not_initialized {
            // No archive yet — nothing is broken; a from-scratch index will
            // create it. Vacuously ok, but nothing was read.
            (StorageState::Ok, ArchiveReadability::NotChecked)
        } else {
            // Expected but missing — missing != corrupt, so do not claim a
            // failure state. The verdict is deferred until an archive exists.
            (
                StorageState::UnknownDeferred,
                ArchiveReadability::NotChecked,
            )
        }
    }
}

/// Build the storage-integrity report `cass doctor --check --json` projects from
/// the signals its database + lexical-index checks already gathered, recording
/// the read-only checks attempted so the report carries its own provenance and
/// `source_of_truth_risk` stays derived from the state (never hand-set).
pub(crate) fn build_doctor_storage_integrity(
    signals: DoctorStorageSignals,
    checks_attempted: Vec<StorageCheck>,
) -> StorageIntegrityReport {
    let (state, readability) = signals.classify();
    StorageIntegrityReport::derive(state, readability, checks_attempted)
}

/// Build the storage-integrity report a *lightweight readiness surface* —
/// `cass status --json` and `cass search --robot-meta` — projects from the
/// db-open + index-drift signals it already gathered while serving the request
/// (bead `…-qfswx`, follow-on to `vl1cj`'s doctor wiring).
///
/// Unlike [`build_doctor_storage_integrity`], these surfaces do NOT run the
/// deep PRAGMA integrity probe the doctor owns, so the recorded checks honestly
/// say only `db_open` ran (never `archive_integrity`). #331: a successful open
/// alone therefore projects `unchecked` (risk `unknown`) — never a synthesized
/// `ok` — unless a fingerprint-still-valid cached attestation from an earlier
/// deep probe upgrades it to `ok` (attested pass) or `integrity_failed`
/// (attested fail). Both surfaces call this one function and feed it the same
/// [`DoctorStorageSignals`] shape, so they project the SAME [`StorageState`]
/// vocabulary by construction (the "all truth surfaces agree" invariant). The
/// `source_of_truth_risk` stays derived from the state, never hand-set.
pub(crate) fn build_readiness_storage_integrity(
    signals: DoctorStorageSignals,
    attestation: Option<&IntegrityAttestation>,
) -> StorageIntegrityReport {
    let mut checks: Vec<StorageCheck> = Vec::new();
    if signals.db_file_present {
        // The readiness surface opened (or attempted to open) the canonical DB
        // while serving the request; that open is the only check it ran — it
        // never runs the deep integrity PRAGMA the doctor owns. Recorded with
        // `elapsed_ms = 0` because the open was timed inside the shared
        // state-meta probe, not separately here.
        checks.push(StorageCheck::ran("db_open", 0));
    } else {
        let reason = if signals.not_initialized {
            "database not initialized"
        } else {
            "no archive present to probe"
        };
        checks.push(StorageCheck::skipped("db_open", reason));
    }
    let (state, readability) = signals.classify();
    // #331: openability alone never proves structural integrity. When the
    // classifier's verdict rests only on a successful open (state `Ok` with
    // the DB file present), consult the fingerprint-matched attestation an
    // earlier deep probe (doctor / index preflight) persisted:
    //   - matched PASS  → project `ok` with cached provenance;
    //   - matched FAIL  → project `integrity_failed` (known-bad archives must
    //     not read as healthy just because they still open);
    //   - none/stale    → honest `unchecked` + an explicit not-attempted
    //     `quick_check` entry, never a synthesized `ok`.
    if signals.db_file_present && state == StorageState::Ok {
        if let Some(att) = attestation {
            let projected_state = match att.verdict {
                IntegrityAttestationVerdict::Pass => StorageState::Ok,
                IntegrityAttestationVerdict::Fail => StorageState::IntegrityFailed,
            };
            let projected_readability = match att.verdict {
                IntegrityAttestationVerdict::Pass => readability,
                IntegrityAttestationVerdict::Fail => ArchiveReadability::PartiallyReadable,
            };
            let mut report =
                StorageIntegrityReport::derive(projected_state, projected_readability, checks);
            report.attestation_source = Some("cached".to_string());
            report.attested_at_ms = Some(att.checked_at_ms);
            report.attestation_check_depth = Some(att.check_depth.clone());
            report.attested_db_fingerprint = Some(integrity_attestation_fingerprint(att));
            return report;
        }
        let mut report =
            StorageIntegrityReport::derive(StorageState::Unchecked, readability, checks);
        report.attestation_source = Some("none".to_string());
        report.checks_not_attempted.push(StorageCheckNotAttempted {
            name: "quick_check".to_string(),
            reason: "outside_status_budget".to_string(),
        });
        return report;
    }
    // A known-bad cached attestation also outranks a derived-only verdict:
    // canonical structural damage dominates a drifted derived asset.
    if signals.db_file_present
        && state == StorageState::DerivedOnlyDrift
        && let Some(att) = attestation
        && att.verdict == IntegrityAttestationVerdict::Fail
    {
        let mut report = StorageIntegrityReport::derive(
            StorageState::IntegrityFailed,
            ArchiveReadability::PartiallyReadable,
            checks,
        );
        report.attestation_source = Some("cached".to_string());
        report.attested_at_ms = Some(att.checked_at_ms);
        report.attestation_check_depth = Some(att.check_depth.clone());
        report.attested_db_fingerprint = Some(integrity_attestation_fingerprint(att));
        return report;
    }
    StorageIntegrityReport::derive(state, readability, checks)
}

/// Run the dedicated, bounded, non-mutating probes that distinguish contention,
/// schema drift, legacy interoperability, and suspect WAL/SHM sidecars.
///
/// The schema leg queries a capped isolated snapshot instead of opening the
/// canonical pager or a migration-aware storage wrapper. Main DB plus present
/// sidecars must total at most 16 MiB; larger archives defer schema/legacy
/// classification with an explicit skip reason. The contention leg inspects
/// the native VFS reservation state without opening the pager or acquiring a
/// transaction; neither leg changes canonical rows.
/// That preserves the archive and lets an old but structurally openable schema
/// be classified without upgrading it. Any open/query error is inspected by
/// concrete `FrankenError` type; text and the generic CLI retryability flag
/// never imply contention.
pub(crate) fn probe_dedicated_storage_state(
    db_path: &std::path::Path,
    timeout: std::time::Duration,
) -> DedicatedStorageProbe {
    let mut probe = DedicatedStorageProbe::default();
    if !db_path.is_file() {
        probe.checks_attempted.push(StorageCheck::skipped(
            "contention_classification",
            "database file absent",
        ));
        probe.checks_attempted.push(StorageCheck::skipped(
            "schema_version",
            "database file absent",
        ));
        probe.checks_attempted.push(StorageCheck::skipped(
            "wal_sidecar_shape",
            "database file absent",
        ));
        return probe;
    }
    probe.main_db_header_plausible = sqlite_main_db_header_is_plausible(db_path);

    let sidecar_started = std::time::Instant::now();
    probe.wal_sidecar_suspect = wal_sidecars_are_structurally_suspect(db_path);
    probe.checks_attempted.push(StorageCheck::ran(
        "wal_sidecar_shape",
        elapsed_millis(sidecar_started),
    ));

    if probe.wal_sidecar_suspect {
        // The sidecar verdict already explains why the archive is deferred.
        // Avoid any additional database handle so this diagnostic preserves
        // the orphan/malformed evidence byte-for-byte.
        probe.checks_attempted.push(StorageCheck::skipped(
            "contention_classification",
            "structurally suspect sidecar already explains the deferred archive",
        ));
    } else {
        let contention_started = std::time::Instant::now();
        let contention_timed_out = match probe_writer_lock_state(db_path, timeout) {
            LockAdmissionProbe::Available => false,
            LockAdmissionProbe::BusyOrLocked => {
                probe.busy_or_locked = true;
                false
            }
            LockAdmissionProbe::TimedOut => {
                probe.checks_attempted.push(StorageCheck::timed_out(
                    "contention_classification",
                    elapsed_millis(contention_started),
                ));
                true
            }
            LockAdmissionProbe::Unclassified => false,
        };
        if !contention_timed_out {
            probe.checks_attempted.push(StorageCheck::ran(
                "contention_classification",
                elapsed_millis(contention_started),
            ));
        }
    }

    if probe.wal_sidecar_suspect || probe.busy_or_locked {
        probe.checks_attempted.push(StorageCheck::skipped(
            "schema_version",
            "sidecar or writer evidence already requires deferring canonical schema reads",
        ));
    } else {
        let schema_started = std::time::Instant::now();
        match probe_schema_state_from_isolated_snapshot(db_path, timeout) {
            SchemaSnapshotProbe::Observed(observation) => {
                probe.schema_drift = observation.schema_drift;
                probe.legacy_interop_failed = observation.legacy_interop_failed;
                probe.checks_attempted.push(StorageCheck::ran(
                    "schema_version",
                    elapsed_millis(schema_started),
                ));
            }
            SchemaSnapshotProbe::TimedOut => {
                probe.checks_attempted.push(StorageCheck::timed_out(
                    "schema_version",
                    elapsed_millis(schema_started),
                ));
            }
            SchemaSnapshotProbe::SkippedOversized => {
                probe.checks_attempted.push(StorageCheck::skipped(
                    "schema_version",
                    SCHEMA_SNAPSHOT_OVERSIZED_REASON,
                ));
            }
            SchemaSnapshotProbe::Unclassified => {
                probe.checks_attempted.push(StorageCheck::skipped(
                    "schema_version",
                    "isolated snapshot could not be opened or queried",
                ));
            }
        }
    }

    probe
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LockAdmissionProbe {
    Available,
    BusyOrLocked,
    TimedOut,
    Unclassified,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct SchemaSnapshotObservation {
    schema_drift: bool,
    legacy_interop_failed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SchemaSnapshotProbe {
    Observed(SchemaSnapshotObservation),
    TimedOut,
    SkippedOversized,
    Unclassified,
}

const SCHEMA_SNAPSHOT_MAX_BYTES: u64 = 16 * 1024 * 1024;
const SCHEMA_SNAPSHOT_OVERSIZED_REASON: &str = "isolated_snapshot_exceeds_16_mib_budget";

/// Query schema metadata only on an isolated copy. FrankenSQLite's current
/// pager-backed read-only open can perform WAL recovery while establishing a
/// readable view, so opening the canonical pathname would violate the
/// diagnostic contract even when the SQL itself is read-only.
///
/// Copying costs O(main DB + present sidecars) I/O. The caller's wait is
/// bounded; a worker that reaches the deadline finishes and removes its temp
/// snapshot in the background without ever opening the canonical pager.
fn probe_schema_state_from_isolated_snapshot(
    db_path: &std::path::Path,
    timeout: std::time::Duration,
) -> SchemaSnapshotProbe {
    match isolated_snapshot_preflight_bytes(db_path) {
        Ok(total) if total <= SCHEMA_SNAPSHOT_MAX_BYTES => {}
        Ok(_) => return SchemaSnapshotProbe::SkippedOversized,
        Err(()) => return SchemaSnapshotProbe::Unclassified,
    }

    let path = db_path.to_path_buf();
    let (tx, rx) = std::sync::mpsc::channel();
    let _worker = std::thread::spawn(move || {
        let outcome = inspect_schema_state_from_isolated_snapshot(&path, timeout);
        let _ = tx.send(outcome);
    });

    match rx.recv_timeout(timeout) {
        Ok(outcome) => outcome,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => SchemaSnapshotProbe::TimedOut,
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => SchemaSnapshotProbe::Unclassified,
    }
}

fn inspect_schema_state_from_isolated_snapshot(
    db_path: &std::path::Path,
    timeout: std::time::Duration,
) -> SchemaSnapshotProbe {
    use frankensqlite::compat::{ConnectionExt as _, RowExt as _};

    let snapshot_dir = match tempfile::Builder::new()
        .prefix("cass-storage-schema-probe-")
        .tempdir()
    {
        Ok(dir) => dir,
        Err(_) => return SchemaSnapshotProbe::Unclassified,
    };
    let file_name = match db_path.file_name() {
        Some(name) => name,
        None => return SchemaSnapshotProbe::Unclassified,
    };
    let snapshot_db = snapshot_dir.path().join(file_name);
    let mut copied_bytes = 0_u64;
    match copy_snapshot_file_bounded(db_path, &snapshot_db, &mut copied_bytes) {
        SnapshotCopyOutcome::Copied => {}
        SnapshotCopyOutcome::Oversized => return SchemaSnapshotProbe::SkippedOversized,
        SnapshotCopyOutcome::Failed => return SchemaSnapshotProbe::Unclassified,
    }
    for source in isolated_snapshot_sidecar_paths(db_path) {
        let Ok(metadata) = std::fs::symlink_metadata(&source) else {
            continue;
        };
        if !metadata.file_type().is_file() {
            return SchemaSnapshotProbe::Unclassified;
        }
        let Some(name) = source.file_name() else {
            return SchemaSnapshotProbe::Unclassified;
        };
        match copy_snapshot_file_bounded(
            &source,
            &snapshot_dir.path().join(name),
            &mut copied_bytes,
        ) {
            SnapshotCopyOutcome::Copied => {}
            SnapshotCopyOutcome::Oversized => return SchemaSnapshotProbe::SkippedOversized,
            SnapshotCopyOutcome::Failed => return SchemaSnapshotProbe::Unclassified,
        }
    }

    let mut conn = match crate::storage::sqlite::open_franken_raw_readonly_connection_with_timeout(
        &snapshot_db,
        timeout,
    ) {
        Ok(conn) => conn,
        Err(_) => return SchemaSnapshotProbe::Unclassified,
    };
    let table_count = conn.query_row_map(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table'",
        &[],
        |row: &frankensqlite::Row| row.get_typed::<i64>(0),
    );
    let meta_count = conn.query_row_map(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'meta'",
        &[],
        |row: &frankensqlite::Row| row.get_typed::<i64>(0),
    );

    let outcome = match (table_count, meta_count) {
        (Ok(table_count), Ok(0)) if table_count > 0 => {
            SchemaSnapshotProbe::Observed(SchemaSnapshotObservation {
                legacy_interop_failed: true,
                ..SchemaSnapshotObservation::default()
            })
        }
        (Ok(_), Ok(_)) => match conn.query_row_map(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            &[],
            |row: &frankensqlite::Row| row.get_typed::<String>(0),
        ) {
            Ok(raw_version) => match raw_version.trim().parse::<i64>() {
                Ok(version)
                    if (1..crate::storage::sqlite::MIN_IN_PLACE_MIGRATION_SCHEMA_VERSION)
                        .contains(&version) =>
                {
                    SchemaSnapshotProbe::Observed(SchemaSnapshotObservation {
                        legacy_interop_failed: true,
                        ..SchemaSnapshotObservation::default()
                    })
                }
                Ok(version) => SchemaSnapshotProbe::Observed(SchemaSnapshotObservation {
                    schema_drift: version != crate::storage::sqlite::CURRENT_SCHEMA_VERSION,
                    legacy_interop_failed: false,
                }),
                Err(_) => SchemaSnapshotProbe::Observed(SchemaSnapshotObservation {
                    legacy_interop_failed: true,
                    ..SchemaSnapshotObservation::default()
                }),
            },
            Err(_) => SchemaSnapshotProbe::Unclassified,
        },
        (Err(_), _) | (_, Err(_)) => SchemaSnapshotProbe::Unclassified,
    };
    if conn.close_without_checkpoint_in_place().is_err() {
        conn.close_best_effort_in_place();
    }
    outcome
}

fn isolated_snapshot_sidecar_paths(db_path: &std::path::Path) -> [std::path::PathBuf; 3] {
    [
        wal_sidecar_path(db_path),
        shm_sidecar_path(db_path),
        suffixed_db_path(db_path, "-journal"),
    ]
}

fn isolated_snapshot_preflight_bytes(db_path: &std::path::Path) -> Result<u64, ()> {
    let main = std::fs::symlink_metadata(db_path).map_err(|_| ())?;
    if !main.file_type().is_file() {
        return Err(());
    }
    let mut total = main.len();
    for sidecar in isolated_snapshot_sidecar_paths(db_path) {
        match std::fs::symlink_metadata(sidecar) {
            Ok(metadata) if metadata.file_type().is_file() => {
                total = total.checked_add(metadata.len()).ok_or(())?;
            }
            Ok(_) => return Err(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(()),
        }
    }
    Ok(total)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SnapshotCopyOutcome {
    Copied,
    Oversized,
    Failed,
}

fn copy_snapshot_file_bounded(
    source: &std::path::Path,
    destination: &std::path::Path,
    copied_bytes: &mut u64,
) -> SnapshotCopyOutcome {
    use std::io::{Read as _, Write as _};

    let mut input = match std::fs::File::open(source) {
        Ok(file) => file,
        Err(_) => return SnapshotCopyOutcome::Failed,
    };
    let mut output = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
    {
        Ok(file) => file,
        Err(_) => return SnapshotCopyOutcome::Failed,
    };
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = match input.read(&mut buffer) {
            Ok(0) => return SnapshotCopyOutcome::Copied,
            Ok(read) => read,
            Err(_) => return SnapshotCopyOutcome::Failed,
        };
        let Ok(read_u64) = u64::try_from(read) else {
            return SnapshotCopyOutcome::Failed;
        };
        let Some(next_total) = copied_bytes.checked_add(read_u64) else {
            return SnapshotCopyOutcome::Oversized;
        };
        if next_total > SCHEMA_SNAPSHOT_MAX_BYTES {
            return SnapshotCopyOutcome::Oversized;
        }
        if output.write_all(&buffer[..read]).is_err() {
            return SnapshotCopyOutcome::Failed;
        }
        *copied_bytes = next_total;
    }
}

/// Inspect the native VFS reservation state without acquiring a transaction or
/// opening the pager. On Unix, `check_reserved_lock` delegates to `F_GETLK` on
/// SQLite's reserved byte, so it neither changes the main file nor recovers or
/// checkpoints sidecars. The work runs on a bounded thread because a storage
/// diagnosis must never inherit an engine-level wait indefinitely.
fn probe_writer_lock_state(
    db_path: &std::path::Path,
    timeout: std::time::Duration,
) -> LockAdmissionProbe {
    let path = db_path.to_path_buf();
    let (tx, rx) = std::sync::mpsc::channel();
    let _worker = std::thread::spawn(move || {
        let outcome = inspect_native_writer_lock(&path);
        let _ = tx.send(outcome);
    });

    match rx.recv_timeout(timeout) {
        Ok(outcome) => outcome,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => LockAdmissionProbe::TimedOut,
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => LockAdmissionProbe::Unclassified,
    }
}

#[cfg(unix)]
fn inspect_native_writer_lock(db_path: &std::path::Path) -> LockAdmissionProbe {
    use frankensqlite::fsqlite_vfs::{UnixVfs, Vfs as _, VfsFile as _};
    use fsqlite_types::cx::Cx;
    use fsqlite_types::flags::VfsOpenFlags;

    let cx = Cx::new();
    let vfs = UnixVfs::new();
    let flags = VfsOpenFlags::MAIN_DB | VfsOpenFlags::READONLY;
    let (mut file, _) = match vfs.open(&cx, Some(db_path), flags) {
        Ok(opened) => opened,
        Err(err) => return lock_admission_outcome_from_error(&err),
    };
    let outcome = match file.check_reserved_lock(&cx) {
        Ok(true) => LockAdmissionProbe::BusyOrLocked,
        Ok(false) => LockAdmissionProbe::Available,
        Err(err) => lock_admission_outcome_from_error(&err),
    };
    if file.close(&cx).is_err() && outcome == LockAdmissionProbe::Available {
        LockAdmissionProbe::Unclassified
    } else {
        outcome
    }
}

#[cfg(not(unix))]
fn inspect_native_writer_lock(_db_path: &std::path::Path) -> LockAdmissionProbe {
    // The Windows VFS materializes advisory-lock sidecars when opened. Keep
    // this diagnostic non-mutating there and rely on the typed raw read/query
    // errors below until a side-effect-free lock-inspection API is available.
    LockAdmissionProbe::Unclassified
}

#[cfg(unix)]
fn lock_admission_outcome_from_error(err: &frankensqlite::FrankenError) -> LockAdmissionProbe {
    use crate::search::contention_diagnostics::{ContentionClass, classify_franken_error};

    if classify_franken_error(err).is_some_and(|class| {
        matches!(
            class,
            ContentionClass::BusyLocked
                | ContentionClass::BusyRecovery
                | ContentionClass::SnapshotConflict
        )
    }) {
        LockAdmissionProbe::BusyOrLocked
    } else {
        LockAdmissionProbe::Unclassified
    }
}

fn elapsed_millis(started: std::time::Instant) -> i64 {
    i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX)
}

fn observe_anyhow_contention(err: &anyhow::Error, probe: &mut DedicatedStorageProbe) {
    for cause in err.chain() {
        if let Some(franken) = cause.downcast_ref::<frankensqlite::FrankenError>() {
            observe_typed_contention(franken, probe);
        }
    }
}

fn observe_typed_contention(err: &frankensqlite::FrankenError, probe: &mut DedicatedStorageProbe) {
    use crate::search::contention_diagnostics::{ContentionClass, classify_franken_error};

    probe.busy_or_locked |= classify_franken_error(err).is_some_and(|class| {
        matches!(
            class,
            ContentionClass::BusyLocked
                | ContentionClass::BusyRecovery
                | ContentionClass::SnapshotConflict
        )
    });
}

fn shm_sidecar_path(db_path: &std::path::Path) -> std::path::PathBuf {
    let mut name = db_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    name.push_str("-shm");
    db_path.with_file_name(name)
}

fn suffixed_db_path(db_path: &std::path::Path, suffix: &str) -> std::path::PathBuf {
    let mut name = db_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    name.push_str(suffix);
    db_path.with_file_name(name)
}

/// Check only the immutable SQLite file-header signature needed for sidecar
/// attribution. This deliberately does not claim that the database is
/// readable or passes integrity checks.
fn sqlite_main_db_header_is_plausible(db_path: &std::path::Path) -> bool {
    use std::io::Read as _;

    let mut header = [0_u8; 18];
    let mut file = match std::fs::File::open(db_path) {
        Ok(file) => file,
        Err(_) => return false,
    };
    if file.read_exact(&mut header).is_err() || !header.starts_with(b"SQLite format 3\0") {
        return false;
    }
    let raw_page_size = u16::from_be_bytes([header[16], header[17]]);
    raw_page_size == 1
        || ((512..=32_768).contains(&raw_page_size) && raw_page_size.is_power_of_two())
}

/// Require structural evidence before calling a WAL/SHM sidecar suspect.
/// Healthy WAL-mode databases routinely have sidecars while open, so presence
/// alone is deliberately a non-signal.
fn wal_sidecars_are_structurally_suspect(db_path: &std::path::Path) -> bool {
    use std::io::Read as _;

    let wal_path = wal_sidecar_path(db_path);
    let shm_path = shm_sidecar_path(db_path);
    let wal_meta = std::fs::symlink_metadata(&wal_path).ok();
    let shm_meta = std::fs::symlink_metadata(&shm_path).ok();

    // SHM is derived from a WAL. SHM with no WAL is therefore orphaned. A
    // symlink/non-file sidecar is also off-contract and must not be followed.
    if shm_meta.is_some() && wal_meta.is_none() {
        return true;
    }
    if wal_meta
        .as_ref()
        .is_some_and(|meta| !meta.file_type().is_file())
        || shm_meta
            .as_ref()
            .is_some_and(|meta| !meta.file_type().is_file())
    {
        return true;
    }

    let Some(wal_meta) = wal_meta else {
        return false;
    };
    let wal_len = wal_meta.len();
    // A zero-length WAL may be a freshly created healthy sidecar. Non-empty
    // WALs must carry the 32-byte header plus whole page frames.
    if wal_len == 0 {
        return false;
    }
    if wal_len < 32 {
        return true;
    }

    let mut header = [0_u8; 12];
    let mut file = match std::fs::File::open(&wal_path) {
        Ok(file) => file,
        Err(_) => return true,
    };
    if file.read_exact(&mut header).is_err() {
        return true;
    }
    let [
        magic_0,
        magic_1,
        magic_2,
        magic_3,
        _,
        _,
        _,
        _,
        page_0,
        page_1,
        page_2,
        page_3,
    ] = header;
    let magic = u32::from_be_bytes([magic_0, magic_1, magic_2, magic_3]);
    if !matches!(magic, 0x377f_0682 | 0x377f_0683) {
        return true;
    }
    let raw_page_size = u32::from_be_bytes([page_0, page_1, page_2, page_3]);
    let page_size = if raw_page_size == 1 {
        65_536_u64
    } else {
        u64::from(raw_page_size)
    };
    if !(512..=65_536).contains(&page_size) || !page_size.is_power_of_two() {
        return true;
    }
    let frame_size = 24_u64.saturating_add(page_size);
    !(wal_len - 32).is_multiple_of(frame_size)
}

/// Overlay the dedicated-probe verdict on the report derived from the common
/// db-open/index/integrity signals. Busy is checked before generic open failure
/// because it is typed, low-risk contention. Generic open/integrity failures
/// otherwise retain precedence so broken headers are never relabeled merely
/// because a sidecar happens to exist. A structurally suspect sidecar may
/// explain an open failure only when the main file still has a plausible
/// SQLite header; this preserves open-failure precedence for arbitrary broken
/// canonical files while making an orphaned/malformed sidecar diagnosable.
pub(crate) fn apply_dedicated_storage_probe(
    mut report: StorageIntegrityReport,
    dedicated: DedicatedStorageProbe,
) -> StorageIntegrityReport {
    let state = if dedicated.busy_or_locked {
        Some(StorageState::BusyOrLocked)
    } else if report.storage_state == StorageState::OpenreadFailed
        && dedicated.wal_sidecar_suspect
        && dedicated.main_db_header_plausible
    {
        Some(StorageState::WalSidecarSuspect)
    } else if matches!(
        report.storage_state,
        StorageState::OpenreadFailed | StorageState::IntegrityFailed
    ) {
        None
    } else if dedicated.legacy_interop_failed {
        Some(StorageState::LegacyInteropFailed)
    } else if dedicated.schema_drift {
        Some(StorageState::SchemaDrift)
    } else if dedicated.wal_sidecar_suspect {
        Some(StorageState::WalSidecarSuspect)
    } else {
        None
    };

    if let Some(state) = state {
        report.storage_state = state;
        report.source_of_truth_risk = state.default_risk();
        if state == StorageState::BusyOrLocked {
            report.archive_readability = ArchiveReadability::NotChecked;
        }
    }
    report.checks_attempted.extend(dedicated.checks_attempted);
    report
}

/// Verdict of a persisted structural-integrity attestation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum IntegrityAttestationVerdict {
    Pass,
    Fail,
}

/// A persisted record of the most recent deep structural-integrity probe
/// (`PRAGMA quick_check` / `integrity_check`-class) run against the canonical
/// archive, fingerprinted so lightweight surfaces only reuse it while the
/// archive bytes it attested are still plausibly current (#331).
///
/// The fingerprint is deliberately conservative: it covers the main DB file's
/// size + mtime AND the WAL sidecar's size + mtime, so any checkpoint or WAL
/// append invalidates the attestation and status honestly degrades to
/// `unchecked` rather than projecting a stale verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct IntegrityAttestation {
    /// Schema version for forward compatibility; readers reject other values.
    pub version: u32,
    pub verdict: IntegrityAttestationVerdict,
    /// Depth of the probe that produced the verdict (`quick_check` /
    /// `integrity_check`).
    pub check_depth: String,
    /// Wall-clock ms when the probe completed.
    pub checked_at_ms: i64,
    pub db_size_bytes: u64,
    pub db_mtime_ns: i64,
    /// 0 when no WAL sidecar existed at attestation time.
    pub wal_size_bytes: u64,
    pub wal_mtime_ns: i64,
    /// Optional short human diagnostic (first integrity error line, etc.).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

const INTEGRITY_ATTESTATION_VERSION: u32 = 1;
const INTEGRITY_ATTESTATION_FILE: &str = "integrity_attestation.json";
/// Cached verdicts older than this are ignored even when the fingerprint
/// still matches (a quiet archive should still be re-proven eventually).
const INTEGRITY_ATTESTATION_MAX_AGE_MS: i64 = 7 * 24 * 60 * 60 * 1000;

pub(crate) fn integrity_attestation_fingerprint(attestation: &IntegrityAttestation) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"cass-integrity-attestation-fingerprint-v1\0");
    hasher.update(&attestation.db_size_bytes.to_le_bytes());
    hasher.update(&attestation.db_mtime_ns.to_le_bytes());
    hasher.update(&attestation.wal_size_bytes.to_le_bytes());
    hasher.update(&attestation.wal_mtime_ns.to_le_bytes());
    hasher.finalize().to_hex().to_string()
}

pub(crate) fn integrity_attestation_path(data_dir: &std::path::Path) -> std::path::PathBuf {
    data_dir.join(INTEGRITY_ATTESTATION_FILE)
}

fn file_size_and_mtime_ns(path: &std::path::Path) -> (u64, i64) {
    match std::fs::metadata(path) {
        Ok(meta) => {
            let mtime_ns = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
                .unwrap_or(0);
            (meta.len(), mtime_ns)
        }
        Err(_) => (0, 0),
    }
}

/// Capture the outcome of a completed deep integrity probe against the CURRENT
/// db/WAL stat. This is read-only; callers may project the live evidence without
/// violating `doctor check`'s no-mutation contract.
pub(crate) fn capture_integrity_attestation(
    db_path: &std::path::Path,
    verdict: IntegrityAttestationVerdict,
    check_depth: &str,
    detail: Option<String>,
) -> Option<IntegrityAttestation> {
    let (db_size_bytes, db_mtime_ns) = file_size_and_mtime_ns(db_path);
    if db_size_bytes == 0 {
        return None;
    }
    let wal_path = wal_sidecar_path(db_path);
    let (wal_size_bytes, wal_mtime_ns) = file_size_and_mtime_ns(&wal_path);
    Some(IntegrityAttestation {
        version: INTEGRITY_ATTESTATION_VERSION,
        verdict,
        check_depth: check_depth.to_string(),
        checked_at_ms: chrono::Utc::now().timestamp_millis(),
        db_size_bytes,
        db_mtime_ns,
        wal_size_bytes,
        wal_mtime_ns,
        detail,
    })
}

/// Persist a captured attestation. Best-effort: an IO failure only logs a
/// debug line (the attestation is a cache, never a source of truth). Callers
/// must only invoke this from an explicitly mutating command surface; ordinary
/// `doctor check` is contractually read-only.
pub(crate) fn persist_integrity_attestation(
    data_dir: &std::path::Path,
    attestation: &IntegrityAttestation,
) {
    let path = integrity_attestation_path(data_dir);
    let write_outcome = serde_json::to_vec_pretty(attestation)
        .map_err(std::io::Error::other)
        // A torn cache write is safe: readers reject malformed JSON and
        // degrade to `unchecked`. Writing in place is intentionally more
        // portable than rename-over-existing, which fails on Windows and
        // would otherwise prevent later doctor probes from refreshing the
        // attestation.
        .and_then(|bytes| std::fs::write(&path, bytes));
    if let Err(err) = write_outcome {
        tracing::debug!(
            error = %err,
            path = %path.display(),
            "failed to persist integrity attestation (non-fatal cache write)"
        );
    }
}

/// Capture and persist an attestation for mutating command surfaces and test
/// fixtures. Keeping this composition here prevents callers from persisting a
/// fingerprint assembled from stale metadata.
pub(crate) fn store_integrity_attestation(
    data_dir: &std::path::Path,
    db_path: &std::path::Path,
    verdict: IntegrityAttestationVerdict,
    check_depth: &str,
    detail: Option<String>,
) -> Option<IntegrityAttestation> {
    let attestation = capture_integrity_attestation(db_path, verdict, check_depth, detail)?;
    persist_integrity_attestation(data_dir, &attestation);
    Some(attestation)
}

fn wal_sidecar_path(db_path: &std::path::Path) -> std::path::PathBuf {
    let mut name = db_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    name.push_str("-wal");
    db_path.with_file_name(name)
}

/// Load the persisted attestation and return it only when its fingerprint
/// still matches the archive's current db/WAL stat AND it is not older than
/// [`INTEGRITY_ATTESTATION_MAX_AGE_MS`]. Any mismatch (checkpoint, WAL
/// append, replaced file, clock skew) yields `None` so callers degrade to the
/// honest `unchecked` verdict.
pub(crate) fn load_matching_integrity_attestation(
    data_dir: &std::path::Path,
    db_path: &std::path::Path,
) -> Option<IntegrityAttestation> {
    let raw = std::fs::read(integrity_attestation_path(data_dir)).ok()?;
    let attestation: IntegrityAttestation = serde_json::from_slice(&raw).ok()?;
    if attestation.version != INTEGRITY_ATTESTATION_VERSION {
        return None;
    }
    let now_ms = chrono::Utc::now().timestamp_millis();
    let age_ms = now_ms.saturating_sub(attestation.checked_at_ms);
    if !(0..=INTEGRITY_ATTESTATION_MAX_AGE_MS).contains(&age_ms) {
        return None;
    }
    let (db_size_bytes, db_mtime_ns) = file_size_and_mtime_ns(db_path);
    if db_size_bytes == 0
        || db_size_bytes != attestation.db_size_bytes
        || db_mtime_ns != attestation.db_mtime_ns
    {
        return None;
    }
    let (wal_size_bytes, wal_mtime_ns) = file_size_and_mtime_ns(&wal_sidecar_path(db_path));
    if wal_size_bytes != attestation.wal_size_bytes || wal_mtime_ns != attestation.wal_mtime_ns {
        return None;
    }
    Some(attestation)
}

/// Render an enum's snake_case wire label for human summaries (shared
/// vocabulary). Falls back to `unknown` if serialization is somehow not a
/// bare string (never expected for these unit enums).
fn serde_plain_label<T: Serialize>(value: T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_STATES: &[StorageState] = &[
        StorageState::Ok,
        StorageState::DerivedOnlyDrift,
        StorageState::BusyOrLocked,
        StorageState::WalSidecarSuspect,
        StorageState::SchemaDrift,
        StorageState::OpenreadFailed,
        StorageState::IntegrityFailed,
        StorageState::LegacyInteropFailed,
        StorageState::FtsMetadataFailed,
        StorageState::UnsafeSqlShape,
        StorageState::UnknownDeferred,
        StorageState::Unchecked,
    ];

    #[test]
    fn storage_state_values_serialize_snake_case_and_are_stable() {
        let pairs: &[(StorageState, &str)] = &[
            (StorageState::Ok, "ok"),
            (StorageState::DerivedOnlyDrift, "derived_only_drift"),
            (StorageState::BusyOrLocked, "busy_or_locked"),
            (StorageState::WalSidecarSuspect, "wal_sidecar_suspect"),
            (StorageState::SchemaDrift, "schema_drift"),
            (StorageState::OpenreadFailed, "openread_failed"),
            (StorageState::IntegrityFailed, "integrity_failed"),
            (StorageState::LegacyInteropFailed, "legacy_interop_failed"),
            (StorageState::FtsMetadataFailed, "fts_metadata_failed"),
            (StorageState::UnsafeSqlShape, "unsafe_sql_shape"),
            (StorageState::UnknownDeferred, "unknown_deferred"),
            (StorageState::Unchecked, "unchecked"),
        ];
        for (v, want) in pairs {
            assert_eq!(serde_json::to_string(v).unwrap(), format!("\"{want}\""));
        }
        // Every variant is in the pinned list (count guard catches additions).
        assert_eq!(pairs.len(), ALL_STATES.len());
    }

    #[test]
    fn risk_and_readability_serialize_snake_case() {
        let risk: &[(SourceOfTruthRisk, &str)] = &[
            (SourceOfTruthRisk::None, "none"),
            (SourceOfTruthRisk::Low, "low"),
            (SourceOfTruthRisk::Medium, "medium"),
            (SourceOfTruthRisk::High, "high"),
            (SourceOfTruthRisk::Unknown, "unknown"),
        ];
        for (v, want) in risk {
            assert_eq!(serde_json::to_string(v).unwrap(), format!("\"{want}\""));
        }
        let read: &[(ArchiveReadability, &str)] = &[
            (ArchiveReadability::Readable, "readable"),
            (ArchiveReadability::PartiallyReadable, "partially_readable"),
            (ArchiveReadability::Unreadable, "unreadable"),
            (ArchiveReadability::NotChecked, "not_checked"),
            (ArchiveReadability::TimedOut, "timed_out"),
        ];
        for (v, want) in read {
            assert_eq!(serde_json::to_string(v).unwrap(), format!("\"{want}\""));
        }
    }

    #[test]
    fn every_state_has_a_defined_risk_and_storage_family() {
        for &s in ALL_STATES {
            // default_risk is total; Ok is the only None.
            let risk = s.default_risk();
            if s == StorageState::Ok {
                assert_eq!(risk, SourceOfTruthRisk::None);
            }
            // Every non-deferred, non-unchecked state attributes to
            // frankensqlite-storage.
            let fam = s.root_cause_family();
            if matches!(s, StorageState::UnknownDeferred | StorageState::Unchecked) {
                assert_eq!(fam, RootCauseFamily::Unknown);
            } else {
                assert_eq!(fam, RootCauseFamily::FrankensqliteStorage);
            }
        }
    }

    #[test]
    fn read_failures_are_high_risk_and_untrustworthy() {
        for s in [StorageState::OpenreadFailed, StorageState::IntegrityFailed] {
            assert_eq!(s.default_risk(), SourceOfTruthRisk::High, "{s:?}");
            assert!(!s.canonical_trustworthy(), "{s:?}");
        }
        // Derived-only / FTS / busy keep the canonical rows trustworthy.
        for s in [
            StorageState::DerivedOnlyDrift,
            StorageState::FtsMetadataFailed,
            StorageState::BusyOrLocked,
        ] {
            assert!(s.canonical_trustworthy(), "{s:?}");
            assert_eq!(s.default_risk(), SourceOfTruthRisk::Low, "{s:?}");
        }
    }

    /// Fixtures for the report's named failure modes.
    fn fixture(state: StorageState) -> StorageIntegrityReport {
        let (readability, checks) = match state {
            StorageState::OpenreadFailed => (
                ArchiveReadability::Unreadable,
                vec![StorageCheck::ran("open_read", 12)],
            ),
            StorageState::IntegrityFailed => (
                ArchiveReadability::PartiallyReadable,
                vec![StorageCheck::ran("integrity_check", 240)],
            ),
            StorageState::SchemaDrift => (
                ArchiveReadability::Readable,
                vec![StorageCheck::ran("schema_version", 3)],
            ),
            StorageState::BusyOrLocked => (
                ArchiveReadability::NotChecked,
                vec![StorageCheck::skipped(
                    "integrity_check",
                    "database is locked",
                )],
            ),
            StorageState::WalSidecarSuspect => (
                ArchiveReadability::Readable,
                vec![StorageCheck::ran("wal_sidecar", 5)],
            ),
            StorageState::UnsafeSqlShape => (
                ArchiveReadability::Readable,
                vec![StorageCheck::ran("sql_shape_lint", 1)],
            ),
            StorageState::FtsMetadataFailed => (
                ArchiveReadability::Readable,
                vec![StorageCheck::ran("fts_metadata", 8)],
            ),
            StorageState::LegacyInteropFailed => (
                ArchiveReadability::Unreadable,
                vec![StorageCheck::ran("legacy_open", 40)],
            ),
            _ => (
                ArchiveReadability::Readable,
                vec![StorageCheck::ran("open_read", 2)],
            ),
        };
        StorageIntegrityReport::derive(state, readability, checks)
    }

    #[test]
    fn fixtures_cover_the_named_failure_modes_with_consistent_risk() {
        let cases = [
            (StorageState::OpenreadFailed, SourceOfTruthRisk::High),
            (StorageState::IntegrityFailed, SourceOfTruthRisk::High),
            (StorageState::SchemaDrift, SourceOfTruthRisk::Medium),
            (StorageState::BusyOrLocked, SourceOfTruthRisk::Low),
            (StorageState::WalSidecarSuspect, SourceOfTruthRisk::Medium),
            (StorageState::UnsafeSqlShape, SourceOfTruthRisk::Medium),
            (StorageState::FtsMetadataFailed, SourceOfTruthRisk::Low),
            (StorageState::LegacyInteropFailed, SourceOfTruthRisk::Medium),
        ];
        for (state, risk) in cases {
            let r = fixture(state);
            assert_eq!(r.storage_state, state);
            assert_eq!(r.source_of_truth_risk, risk, "{state:?} risk");
            // Diagnostics never mutate the archive.
            assert!(
                r.all_checks_read_only(),
                "{state:?} checks must be read-only"
            );
        }
    }

    #[test]
    fn busy_lock_fixture_skips_with_a_reason() {
        let r = fixture(StorageState::BusyOrLocked);
        let check = &r.checks_attempted[0];
        assert!(check.skipped_reason.is_some());
        assert_eq!(r.archive_readability, ArchiveReadability::NotChecked);
    }

    #[test]
    fn human_summary_shares_the_robot_vocabulary() {
        let r = fixture(StorageState::OpenreadFailed);
        let summary = r.human_summary();
        // The human one-liner uses the exact serialized enum labels.
        assert!(summary.contains("openread_failed"), "{summary}");
        assert!(summary.contains("high"), "{summary}");
        assert!(summary.contains("unreadable"), "{summary}");
    }

    #[test]
    fn report_round_trips_through_json() {
        let r = fixture(StorageState::IntegrityFailed);
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"storage_state\":\"integrity_failed\""));
        assert!(json.contains("\"source_of_truth_risk\":\"high\""));
        assert!(json.contains("\"archive_readability\":\"partially_readable\""));
        assert!(json.contains("\"read_only\":true"));
        let parsed: StorageIntegrityReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, r);
    }

    #[test]
    fn doctor_signals_classify_each_derivable_state() {
        // Healthy canonical DB, healthy derived assets.
        let ok = DoctorStorageSignals {
            db_file_present: true,
            ..Default::default()
        };
        assert_eq!(
            ok.classify(),
            (StorageState::Ok, ArchiveReadability::Readable)
        );

        // DB file present but the read-only opener failed.
        let open_failed = DoctorStorageSignals {
            db_file_present: true,
            db_open_failed: true,
            ..Default::default()
        };
        assert_eq!(
            open_failed.classify(),
            (StorageState::OpenreadFailed, ArchiveReadability::Unreadable)
        );

        // Opened but the integrity probe reported failure.
        let integrity = DoctorStorageSignals {
            db_file_present: true,
            integrity_failed: true,
            ..Default::default()
        };
        assert_eq!(
            integrity.classify(),
            (
                StorageState::IntegrityFailed,
                ArchiveReadability::PartiallyReadable
            )
        );

        // Opened but a read failed mid-probe so integrity is unconfirmed.
        let unverified = DoctorStorageSignals {
            db_file_present: true,
            integrity_unverified: true,
            ..Default::default()
        };
        assert_eq!(
            unverified.classify(),
            (
                StorageState::IntegrityFailed,
                ArchiveReadability::Unreadable
            )
        );

        // Healthy DB, drifted derived lexical index.
        let drift = DoctorStorageSignals {
            db_file_present: true,
            lexical_index_drifted: true,
            ..Default::default()
        };
        assert_eq!(
            drift.classify(),
            (StorageState::DerivedOnlyDrift, ArchiveReadability::Readable)
        );
    }

    #[test]
    fn doctor_signals_classify_deferred_and_absent_cases() {
        // A bounded probe timeout never claims health.
        let timed_out = DoctorStorageSignals {
            db_file_present: true,
            probe_timed_out: true,
            // Even with a downstream integrity_failed signal set, the timeout
            // (deferred verdict) wins so we never over-claim corruption.
            integrity_failed: true,
            ..Default::default()
        };
        assert_eq!(
            timed_out.classify(),
            (StorageState::UnknownDeferred, ArchiveReadability::TimedOut)
        );

        // Fresh install: no DB file, never indexed → vacuously ok, not_checked.
        let fresh = DoctorStorageSignals {
            db_file_present: false,
            not_initialized: true,
            ..Default::default()
        };
        assert_eq!(
            fresh.classify(),
            (StorageState::Ok, ArchiveReadability::NotChecked)
        );

        // Expected-but-missing archive: missing != corrupt → deferred, not a
        // failure state.
        let missing = DoctorStorageSignals {
            db_file_present: false,
            not_initialized: false,
            ..Default::default()
        };
        assert_eq!(
            missing.classify(),
            (
                StorageState::UnknownDeferred,
                ArchiveReadability::NotChecked
            )
        );
    }

    #[test]
    fn doctor_signal_precedence_is_most_severe_first() {
        // Open failure dominates every downstream derived-asset signal.
        let everything = DoctorStorageSignals {
            db_file_present: true,
            db_open_failed: true,
            integrity_failed: true,
            lexical_index_drifted: true,
            ..Default::default()
        };
        assert_eq!(everything.classify().0, StorageState::OpenreadFailed);

        // Real DB-level integrity failure outranks a derived-only drift.
        let integ_over_drift = DoctorStorageSignals {
            db_file_present: true,
            integrity_failed: true,
            lexical_index_drifted: true,
            ..Default::default()
        };
        assert_eq!(integ_over_drift.classify().0, StorageState::IntegrityFailed);
    }

    #[test]
    fn build_doctor_report_derives_risk_from_state() {
        let signals = DoctorStorageSignals {
            db_file_present: true,
            integrity_failed: true,
            ..Default::default()
        };
        let report = build_doctor_storage_integrity(
            signals,
            vec![StorageCheck::ran("archive_integrity", 7)],
        );
        assert_eq!(report.storage_state, StorageState::IntegrityFailed);
        // Risk is the state's default, never hand-set.
        assert_eq!(
            report.source_of_truth_risk,
            StorageState::IntegrityFailed.default_risk()
        );
        assert_eq!(report.source_of_truth_risk, SourceOfTruthRisk::High);
        assert!(report.all_checks_read_only());
    }

    #[test]
    fn readiness_builder_records_db_open_not_archive_integrity() {
        // #331: a healthy open WITHOUT a cached attestation projects
        // `unchecked` (risk unknown) — never a synthesized `ok` — and the
        // ONLY recorded check is `db_open`; the deliberately-skipped
        // `quick_check` is listed under checks_not_attempted.
        let unchecked = build_readiness_storage_integrity(
            DoctorStorageSignals {
                db_file_present: true,
                ..Default::default()
            },
            None,
        );
        assert_eq!(unchecked.storage_state, StorageState::Unchecked);
        assert_eq!(unchecked.source_of_truth_risk, SourceOfTruthRisk::Unknown);
        assert_eq!(unchecked.archive_readability, ArchiveReadability::Readable);
        assert_eq!(unchecked.checks_attempted.len(), 1);
        assert_eq!(unchecked.checks_attempted[0].name, "db_open");
        assert!(unchecked.checks_attempted[0].read_only);
        assert!(
            unchecked
                .checks_attempted
                .iter()
                .all(|c| c.name != "archive_integrity"),
            "readiness surfaces never claim a deep integrity probe ran"
        );
        assert_eq!(unchecked.attestation_source.as_deref(), Some("none"));
        assert_eq!(unchecked.checks_not_attempted.len(), 1);
        assert_eq!(unchecked.checks_not_attempted[0].name, "quick_check");
        assert_eq!(
            unchecked.checks_not_attempted[0].reason,
            "outside_status_budget"
        );
    }

    fn test_attestation(verdict: IntegrityAttestationVerdict) -> IntegrityAttestation {
        IntegrityAttestation {
            version: 1,
            verdict,
            check_depth: "quick_check".to_string(),
            checked_at_ms: 1_700_000_000_000,
            db_size_bytes: 4096,
            db_mtime_ns: 1_700_000_000_000_000_000,
            wal_size_bytes: 0,
            wal_mtime_ns: 0,
            detail: None,
        }
    }

    #[test]
    fn readiness_builder_projects_cached_pass_attestation_as_ok() {
        let att = test_attestation(IntegrityAttestationVerdict::Pass);
        let ok = build_readiness_storage_integrity(
            DoctorStorageSignals {
                db_file_present: true,
                ..Default::default()
            },
            Some(&att),
        );
        assert_eq!(ok.storage_state, StorageState::Ok);
        assert_eq!(ok.source_of_truth_risk, SourceOfTruthRisk::None);
        assert_eq!(ok.attestation_source.as_deref(), Some("cached"));
        assert_eq!(ok.attested_at_ms, Some(att.checked_at_ms));
        assert_eq!(ok.attestation_check_depth.as_deref(), Some("quick_check"));
        assert_eq!(
            ok.attested_db_fingerprint.as_deref(),
            Some(integrity_attestation_fingerprint(&att).as_str())
        );
    }

    #[test]
    fn readiness_builder_projects_cached_fail_attestation_as_integrity_failed() {
        // The #331 scenario: the archive still opens and bounded reads work,
        // but a deep probe proved committed corruption. Status must NOT
        // report ok/none; the cached failure projects integrity_failed/high.
        let att = test_attestation(IntegrityAttestationVerdict::Fail);
        for signals in [
            DoctorStorageSignals {
                db_file_present: true,
                ..Default::default()
            },
            DoctorStorageSignals {
                db_file_present: true,
                lexical_index_drifted: true,
                ..Default::default()
            },
        ] {
            let failed = build_readiness_storage_integrity(signals, Some(&att));
            assert_eq!(failed.storage_state, StorageState::IntegrityFailed);
            assert_eq!(failed.source_of_truth_risk, SourceOfTruthRisk::High);
            assert_eq!(
                failed.archive_readability,
                ArchiveReadability::PartiallyReadable
            );
            assert_eq!(failed.attestation_source.as_deref(), Some("cached"));
        }
    }

    #[test]
    fn attestation_round_trips_and_fingerprint_gates_reuse() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data_dir = dir.path();
        let db_path = data_dir.join("agent_search.db");
        std::fs::write(&db_path, b"not empty").expect("seed db file");

        // No attestation stored yet.
        assert!(load_matching_integrity_attestation(data_dir, &db_path).is_none());

        let captured = capture_integrity_attestation(
            &db_path,
            IntegrityAttestationVerdict::Pass,
            "quick_check",
            None,
        )
        .expect("capture live attestation");
        assert_eq!(captured.check_depth, "quick_check");
        assert!(
            !integrity_attestation_path(data_dir).exists(),
            "capturing live doctor evidence must remain read-only"
        );

        store_integrity_attestation(
            data_dir,
            &db_path,
            IntegrityAttestationVerdict::Pass,
            "quick_check",
            None,
        )
        .expect("capture and persist pass attestation");
        let loaded = load_matching_integrity_attestation(data_dir, &db_path)
            .expect("fingerprint-matched attestation loads");
        assert_eq!(loaded.verdict, IntegrityAttestationVerdict::Pass);
        assert_eq!(loaded.check_depth, "quick_check");

        // Any archive byte change (size differs) invalidates the attestation.
        std::fs::write(&db_path, b"not empty but longer now").expect("mutate db file");
        assert!(
            load_matching_integrity_attestation(data_dir, &db_path).is_none(),
            "a changed archive must not reuse the stale attestation"
        );

        // A fresh probe over the changed bytes re-arms it, including FAIL.
        store_integrity_attestation(
            data_dir,
            &db_path,
            IntegrityAttestationVerdict::Fail,
            "quick_check",
            Some("wrong # of entries in index sqlite_autoindex_conversations_1".to_string()),
        )
        .expect("capture and persist fail attestation");
        let failed = load_matching_integrity_attestation(data_dir, &db_path)
            .expect("fail attestation loads while fingerprint matches");
        assert_eq!(failed.verdict, IntegrityAttestationVerdict::Fail);
        assert!(failed.detail.is_some());

        // WAL sidecar activity also invalidates.
        std::fs::write(data_dir.join("agent_search.db-wal"), b"wal bytes").expect("seed wal");
        assert!(
            load_matching_integrity_attestation(data_dir, &db_path).is_none(),
            "WAL sidecar activity must invalidate the attestation"
        );
    }

    #[test]
    fn readiness_builder_skips_open_when_no_archive_present() {
        // No DB file but never initialized → vacuously ok, the db_open check is
        // recorded as skipped with the initialization reason.
        let fresh = build_readiness_storage_integrity(
            DoctorStorageSignals {
                db_file_present: false,
                not_initialized: true,
                ..Default::default()
            },
            None,
        );
        assert_eq!(fresh.storage_state, StorageState::Ok);
        assert_eq!(fresh.archive_readability, ArchiveReadability::NotChecked);
        assert_eq!(fresh.checks_attempted[0].name, "db_open");
        assert!(fresh.checks_attempted[0].skipped_reason.is_some());
    }

    #[test]
    fn readiness_builder_agrees_with_doctor_classification_on_shared_signals() {
        // For every non-Ok signal BOTH surfaces can observe (open failure,
        // derived-only drift, missing/uninitialized, expected-but-missing), the
        // readiness builder yields the SAME storage state the shared
        // classifier does — the "all truth surfaces agree" invariant. Without
        // an attestation the readiness builder never produces
        // `integrity_failed` because it never sets the integrity signals; a
        // clean open is downgraded to the honest `unchecked` (#331) and is
        // asserted separately above.
        let cases = [
            DoctorStorageSignals {
                db_file_present: true,
                db_open_failed: true,
                ..Default::default()
            },
            DoctorStorageSignals {
                db_file_present: true,
                lexical_index_drifted: true,
                ..Default::default()
            },
            DoctorStorageSignals {
                db_file_present: false,
                not_initialized: true,
                ..Default::default()
            },
            DoctorStorageSignals {
                db_file_present: false,
                ..Default::default()
            },
        ];
        for signals in cases {
            // DoctorStorageSignals is Copy, so classify() after the move is fine.
            let report = build_readiness_storage_integrity(signals, None);
            let (state, _) = signals.classify();
            assert_eq!(report.storage_state, state, "{signals:?}");
            // Risk is always derived from the state, never hand-set.
            assert_eq!(report.source_of_truth_risk, state.default_risk());
            assert!(report.all_checks_read_only());
            assert_ne!(report.storage_state, StorageState::IntegrityFailed);
        }
    }

    #[test]
    fn timed_out_check_is_recorded() {
        let r = StorageIntegrityReport::derive(
            StorageState::UnknownDeferred,
            ArchiveReadability::TimedOut,
            vec![StorageCheck::timed_out("integrity_check", 5000)],
        );
        assert_eq!(r.source_of_truth_risk, SourceOfTruthRisk::Unknown);
        assert!(r.checks_attempted[0].timed_out);
        assert_eq!(r.archive_readability, ArchiveReadability::TimedOut);
    }

    #[test]
    fn dedicated_probe_precedence_keeps_typed_busy_above_generic_open_failure() {
        let open_failed = StorageIntegrityReport::derive(
            StorageState::OpenreadFailed,
            ArchiveReadability::Unreadable,
            Vec::new(),
        );
        let busy = apply_dedicated_storage_probe(
            open_failed,
            DedicatedStorageProbe {
                busy_or_locked: true,
                ..Default::default()
            },
        );
        assert_eq!(busy.storage_state, StorageState::BusyOrLocked);
        assert_eq!(busy.source_of_truth_risk, SourceOfTruthRisk::Low);
        assert_eq!(busy.archive_readability, ArchiveReadability::NotChecked);

        let open_failed = StorageIntegrityReport::derive(
            StorageState::OpenreadFailed,
            ArchiveReadability::Unreadable,
            Vec::new(),
        );
        let schema = apply_dedicated_storage_probe(
            open_failed,
            DedicatedStorageProbe {
                schema_drift: true,
                ..Default::default()
            },
        );
        assert_eq!(schema.storage_state, StorageState::OpenreadFailed);

        let plausible_main_with_orphan = apply_dedicated_storage_probe(
            StorageIntegrityReport::derive(
                StorageState::OpenreadFailed,
                ArchiveReadability::Unreadable,
                Vec::new(),
            ),
            DedicatedStorageProbe {
                wal_sidecar_suspect: true,
                main_db_header_plausible: true,
                ..Default::default()
            },
        );
        assert_eq!(
            plausible_main_with_orphan.storage_state,
            StorageState::WalSidecarSuspect
        );

        let broken_main_with_sidecar = apply_dedicated_storage_probe(
            StorageIntegrityReport::derive(
                StorageState::OpenreadFailed,
                ArchiveReadability::Unreadable,
                Vec::new(),
            ),
            DedicatedStorageProbe {
                wal_sidecar_suspect: true,
                main_db_header_plausible: false,
                ..Default::default()
            },
        );
        assert_eq!(
            broken_main_with_sidecar.storage_state,
            StorageState::OpenreadFailed
        );
    }

    fn seed_schema_version(path: &std::path::Path, version: i64) -> anyhow::Result<()> {
        use frankensqlite::compat::{ConnectionExt as _, ParamValue};

        let storage = crate::storage::sqlite::FrankenStorage::open(path)?;
        storage.raw().execute_compat(
            "UPDATE meta SET value = ?1 WHERE key = 'schema_version'",
            &[ParamValue::from(version.to_string())],
        )?;
        storage.close_without_checkpoint()?;
        Ok(())
    }

    #[test]
    fn dedicated_probe_distinguishes_openable_schema_drift_from_legacy_layout() -> anyhow::Result<()>
    {
        let schema_dir = tempfile::tempdir()?;
        let schema_db = schema_dir.path().join("agent_search.db");
        seed_schema_version(
            &schema_db,
            crate::storage::sqlite::CURRENT_SCHEMA_VERSION + 1,
        )?;
        let schema = probe_dedicated_storage_state(&schema_db, std::time::Duration::from_secs(1));
        assert!(schema.schema_drift);
        assert!(!schema.legacy_interop_failed);
        assert!(!schema.busy_or_locked);

        let legacy_dir = tempfile::tempdir()?;
        let legacy_db = legacy_dir.path().join("agent_search.db");
        seed_schema_version(
            &legacy_db,
            crate::storage::sqlite::MIN_IN_PLACE_MIGRATION_SCHEMA_VERSION - 1,
        )?;
        let legacy = probe_dedicated_storage_state(&legacy_db, std::time::Duration::from_secs(1));
        assert!(legacy.legacy_interop_failed);
        assert!(!legacy.schema_drift);
        assert!(!legacy.busy_or_locked);
        Ok(())
    }

    #[test]
    fn dedicated_probe_skips_oversized_snapshot_without_mutating_canonical_bytes()
    -> anyhow::Result<()> {
        use std::io::{Seek as _, SeekFrom, Write as _};

        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("agent_search.db");
        let mut db = std::fs::File::create(&db_path)?;
        db.write_all(b"SQLite format 3\0")?;
        db.seek(SeekFrom::Start(16))?;
        db.write_all(&4096_u16.to_be_bytes())?;
        db.set_len(SCHEMA_SNAPSHOT_MAX_BYTES + 1)?;
        drop(db);

        let before = blake3::hash(&std::fs::read(&db_path)?);
        let probe = probe_dedicated_storage_state(&db_path, std::time::Duration::from_secs(1));
        let after = blake3::hash(&std::fs::read(&db_path)?);

        assert_eq!(before, after, "oversized preflight must not rewrite the DB");
        assert!(!probe.schema_drift);
        assert!(!probe.legacy_interop_failed);
        let schema_check = probe
            .checks_attempted
            .iter()
            .find(|check| check.name == "schema_version")
            .expect("schema check must report why it was skipped");
        assert_eq!(
            schema_check.skipped_reason.as_deref(),
            Some(SCHEMA_SNAPSHOT_OVERSIZED_REASON)
        );
        assert!(!schema_check.timed_out);
        Ok(())
    }

    #[test]
    fn dedicated_probe_requires_orphan_or_malformed_sidecar_evidence() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("agent_search.db");
        std::fs::write(&db_path, b"SQLite format 3\0diagnostic")?;
        let wal_path = wal_sidecar_path(&db_path);
        let shm_path = shm_sidecar_path(&db_path);

        std::fs::write(&wal_path, [])?;
        assert!(
            !wal_sidecars_are_structurally_suspect(&db_path),
            "mere WAL presence is not suspect"
        );

        // An SHM without its source WAL is an actual orphan signal.
        std::fs::write(&shm_path, [0_u8; 32])?;
        let renamed_wal = dir.path().join("retained-wal-fixture");
        std::fs::rename(&wal_path, renamed_wal)?;
        assert!(wal_sidecars_are_structurally_suspect(&db_path));
        Ok(())
    }
}
