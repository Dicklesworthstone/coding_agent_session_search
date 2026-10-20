//! Read-only rebuild-quarantine projection for diag and doctor (GH #523).
//!
//! This is not the generation-cleanup plan. Eligibility is policy information,
//! never permission for doctor/diag cleanup to delete rebuild evidence.

use super::lexical_quarantine::{self, Inventory};
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub schema_version: u32,
    pub complete: bool,
    pub cleanup_apply_allowed: bool,
    pub repeated_failure_evidence: bool,
    pub inventory: Option<Inventory>,
    pub warnings: Vec<String>,
    /// Argument vector, not shell text; absent for non-UTF-8 data paths.
    pub next_safe_argv: Option<Vec<String>>,
}

/// Use the existing bounded, metadata-only inventory. An I/O/configuration
/// failure is represented as incomplete, not as a successful empty inventory.
pub fn collect(data_dir: &Path) -> Report {
    project(data_dir, lexical_quarantine::inspect(data_dir))
}

fn project(data_dir: &Path, result: std::io::Result<Inventory>) -> Report {
    let mut report = Report {
        schema_version: 1,
        complete: false,
        cleanup_apply_allowed: false,
        repeated_failure_evidence: false,
        inventory: None,
        warnings: Vec::new(),
        next_safe_argv: data_dir.to_str().map(|path| {
            ["cass", "lexical-quarantine", "--data-dir", path, "--plan", "--json"]
                .into_iter()
                .map(str::to_owned)
                .collect()
        }),
    };
    match result {
        Ok(inventory) => {
            report.complete = inventory.complete;
            report.repeated_failure_evidence = inventory
                .artifacts
                .iter()
                .filter(|artifact| artifact.has_evidence && !artifact.interrupted_reclaim)
                .take(2)
                .count()
                > 1;
            report.warnings.clone_from(&inventory.warnings);
            if !inventory.complete {
                report.warnings.push(
                    "Rebuild-quarantine inventory is incomplete; sizes may be partial and no cleanup is authorized."
                        .to_owned(),
                );
            }
            if report.repeated_failure_evidence {
                report.warnings.push(
                    "Repeated failed rebuild evidence is present; retention does not repair rebuild certification."
                        .to_owned(),
                );
            }
            report.inventory = Some(inventory);
        }
        Err(error) => {
            report.warnings.push(format!(
                "Rebuild-quarantine inventory unavailable ({:?}): {error}",
                error.kind()
            ));
        }
    }
    report
}

impl Report {
    /// Paths and reasons use debug escaping, so evidence cannot inject terminal
    /// controls. No diagnostic contents or independently scanned sizes appear.
    pub fn human_lines(&self) -> Vec<String> {
        let mut lines = vec![format!(
            "Lexical rebuild quarantines (complete: {}; cleanup apply allowed: false):",
            self.complete
        )];
        if let Some(inventory) = &self.inventory {
            let label = if inventory.complete {
                "Retained logical bytes"
            } else {
                "Observed logical bytes (incomplete; not a total)"
            };
            lines.push(format!(
                "  Bundles: {}; {label}: {}; policy-eligible logical bytes: {}",
                inventory.artifacts.len(),
                inventory.total_bytes,
                inventory.eligible_bytes
            ));
            for artifact in &inventory.artifacts {
                let size = if artifact.size_complete {
                    format!("{} bytes", artifact.size_bytes)
                } else {
                    "unknown (scan incomplete)".to_owned()
                };
                let age = artifact.age_seconds.map_or_else(
                    || "unknown/future".to_owned(),
                    |seconds| format!("{seconds}s"),
                );
                lines.push(format!(
                    "  {:?}: {size}; age={age}; {:?}; {:?}",
                    artifact.path, artifact.disposition, artifact.reason
                ));
                for evidence in &artifact.failure_evidence {
                    lines.push(format!(
                        "    evidence: {:?} {:?} ({} bytes; contents not inspected)",
                        evidence.kind, evidence.relative_path, evidence.size_bytes
                    ));
                }
                if !artifact.size_complete {
                    lines.push("    Evidence inventory incomplete; absence is not established.".to_owned());
                }
            }
        }
        for warning in &self.warnings {
            lines.push(format!("  Warning: {warning:?}"));
        }
        if let Some(argv) = &self.next_safe_argv {
            lines.push(format!("  Read-only cleanup preview arguments: {argv:?}"));
        }
        lines
    }
}

/// Generation manifests inside a rebuild quarantine are failure evidence, not
/// published generations. Skip the entire direct-child namespace, including
/// malformed names, before the legacy walker can parse them or propose cleanup.
/// This predicate DOES NOT expand the retention module's ownership allowlist.
pub fn generation_walk_entry(entry: &walkdir::DirEntry) -> bool {
    if entry.depth() != 1 {
        return true;
    }
    let name = entry.file_name().to_string_lossy();
    !name.starts_with(".lexical-rebuild-quarantine-")
        && !name.starts_with(".lexical-rebuild-reclaim-")
}

#[cfg(test)]
mod tests;
