// Dead-code tolerated module-wide: this source-provenance classifier lands
// ahead of the hit/pack serializers that will attach it (and the field-policy
// inclusion in lib.rs). Downstream bead .7.4 (moved-workspace and
// stale-source fixture suite) consumes these types.
#![allow(dead_code)]

//! Source existence and archive provenance for hits and packs (bead
//! cass-fleet-resilience-20260608-uojcg.7.2).
//!
//! The 2026-06-08 report found stale `source_path` values: a hit's indexed
//! path may no longer back a live file, yet agents overtrust it. A result
//! must say whether its `source_path` currently exists, whether the content
//! is archive-only (backed by the DB, not a file), which source produced it,
//! and whether a path mapping was applied — so an agent knows whether it can
//! open the file or must read from the archive.
//!
//! This module derives that provenance from explicit signals, so the five
//! cases the report calls out — a local existing file, a deleted/moved file,
//! a remote mirror, a source-pruned archive row, and a path-mapped row — are
//! unit-testable without a filesystem or DB. The hit/pack struct fields and
//! field-policy (minimal/summary/custom) inclusion are the integration step;
//! this is the pure, reusable core. All enums serialize as snake_case and
//! the projected fields are redaction-friendly (ids and host labels only).

use serde::{Deserialize, Serialize};

/// The dominant provenance story for a result, strongest-evidence first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProvenanceKind {
    /// A local `source_path` that currently exists on disk.
    LocalPresent,
    /// A local `source_path` recorded in the index but no longer on disk
    /// (deleted or moved); content survives only in the archive.
    LocalMissing,
    /// Backed by a remote source/mirror identified by `origin_host`.
    RemoteMirror,
    /// The originating source was pruned; only the archive DB row remains.
    ArchiveOnlyPruned,
    /// A path mapping was applied to resolve the source_path (e.g.
    /// macOS↔Linux or a relocated checkout).
    PathMapped,
}

/// Signals a hit/pack carries about its origin. The serializer populates
/// these from the row + a (cheap, cached) existence probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProvenanceSignals {
    /// The indexed source path, if any.
    pub source_path: Option<String>,
    /// Whether `source_path` (after any mapping) currently exists.
    pub file_exists: bool,
    /// The source id that produced the row; `None` when the source was
    /// pruned.
    pub source_id: Option<i64>,
    /// Origin host label for remote sources; `None` for local.
    pub origin_host: Option<String>,
    /// Whether the source is local (vs a remote mirror).
    pub is_local_source: bool,
    /// Whether the content is present as an archive DB row.
    pub archive_row_present: bool,
    /// Whether a path mapping was applied to resolve `source_path`.
    pub path_mapping_applied: bool,
}

impl ProvenanceSignals {
    /// Whether a live (non-archive) source still backs this row.
    fn has_live_source(&self) -> bool {
        self.source_path.is_some() && self.source_id.is_some()
    }

    /// Derive the provenance projection a hit/pack should expose.
    pub(crate) fn provenance(&self) -> SourceProvenance {
        // Pruned source: only the archive row remains. Dominates, because
        // there is no live backing to open regardless of other flags.
        let kind = if self.archive_row_present && !self.has_live_source() {
            ProvenanceKind::ArchiveOnlyPruned
        } else if self.path_mapping_applied {
            ProvenanceKind::PathMapped
        } else if !self.is_local_source {
            ProvenanceKind::RemoteMirror
        } else if self.file_exists {
            ProvenanceKind::LocalPresent
        } else {
            ProvenanceKind::LocalMissing
        };

        // The live source exists when a file backs it now (mapped path
        // included); a pruned/archive-only row never has a live source.
        let source_exists = match kind {
            ProvenanceKind::ArchiveOnlyPruned => false,
            _ => self.file_exists,
        };
        // Archive-only when the content is in the archive but no live file
        // backs it.
        let archive_only = self.archive_row_present && !source_exists;

        SourceProvenance {
            source_exists,
            archive_only,
            source_id: self.source_id,
            origin_host: self.origin_host.clone(),
            path_mapped: self.path_mapping_applied,
            kind,
        }
    }
}

/// The redaction-friendly provenance fields attached to a hit/pack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SourceProvenance {
    /// Whether the `source_path` currently backs a live file.
    pub source_exists: bool,
    /// Whether the content is available only from the archive DB.
    pub archive_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_host: Option<String>,
    /// Whether a path mapping was applied to resolve the source.
    pub path_mapped: bool,
    pub kind: ProvenanceKind,
}

impl SourceProvenance {
    /// Whether an agent can open the result's file directly (vs needing to
    /// read it from the archive via `cass view`).
    pub(crate) fn is_openable_file(&self) -> bool {
        self.source_exists
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local_present() -> ProvenanceSignals {
        ProvenanceSignals {
            source_path: Some("/home/u/proj/session.jsonl".to_string()),
            file_exists: true,
            source_id: Some(7),
            origin_host: None,
            is_local_source: true,
            archive_row_present: true,
            path_mapping_applied: false,
        }
    }

    #[test]
    fn enums_serialize_as_snake_case() {
        assert_eq!(
            serde_json::to_string(&ProvenanceKind::ArchiveOnlyPruned).unwrap(),
            "\"archive_only_pruned\""
        );
    }

    #[test]
    fn local_existing_file_is_openable_and_not_archive_only() {
        let p = local_present().provenance();
        assert_eq!(p.kind, ProvenanceKind::LocalPresent);
        assert!(p.source_exists);
        assert!(!p.archive_only);
        assert!(p.is_openable_file());
        assert_eq!(p.source_id, Some(7));
    }

    #[test]
    fn deleted_or_moved_file_is_archive_only_and_not_openable() {
        let mut s = local_present();
        s.file_exists = false; // file gone, but still a known source + archive row
        let p = s.provenance();
        assert_eq!(p.kind, ProvenanceKind::LocalMissing);
        assert!(!p.source_exists);
        assert!(p.archive_only, "content survives only in the archive");
        assert!(!p.is_openable_file());
    }

    #[test]
    fn remote_mirror_is_classified_by_origin_host() {
        let s = ProvenanceSignals {
            source_path: Some("/srv/data/session.jsonl".to_string()),
            file_exists: true,
            source_id: Some(12),
            origin_host: Some("mac-mini-old".to_string()),
            is_local_source: false,
            archive_row_present: true,
            path_mapping_applied: false,
        };
        let p = s.provenance();
        assert_eq!(p.kind, ProvenanceKind::RemoteMirror);
        assert_eq!(p.origin_host.as_deref(), Some("mac-mini-old"));
        assert!(p.source_exists);
        assert!(!p.archive_only);
    }

    #[test]
    fn source_pruned_archive_row_is_archive_only() {
        let s = ProvenanceSignals {
            source_path: None, // source pruned
            file_exists: false,
            source_id: None,
            origin_host: None,
            is_local_source: true,
            archive_row_present: true,
            path_mapping_applied: false,
        };
        let p = s.provenance();
        assert_eq!(p.kind, ProvenanceKind::ArchiveOnlyPruned);
        assert!(!p.source_exists);
        assert!(p.archive_only);
        assert!(p.source_id.is_none());
    }

    #[test]
    fn path_mapped_row_records_the_mapping() {
        let mut s = local_present();
        s.path_mapping_applied = true;
        let p = s.provenance();
        assert_eq!(p.kind, ProvenanceKind::PathMapped);
        assert!(p.path_mapped);
        // A successfully mapped path that exists is still openable.
        assert!(p.source_exists);
    }

    #[test]
    fn pruned_dominates_path_mapping() {
        // Even if a mapping was attempted, a pruned source with only an
        // archive row is archive-only (nothing live to open).
        let s = ProvenanceSignals {
            source_path: None,
            file_exists: false,
            source_id: None,
            origin_host: None,
            is_local_source: true,
            archive_row_present: true,
            path_mapping_applied: true,
        };
        assert_eq!(s.provenance().kind, ProvenanceKind::ArchiveOnlyPruned);
    }

    #[test]
    fn provenance_round_trips_through_json() {
        let p = local_present().provenance();
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("\"source_exists\":true"));
        assert!(json.contains("\"kind\":\"local_present\""));
        assert!(json.contains("\"path_mapped\":false"));
        let parsed: SourceProvenance = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, p);
    }
}
