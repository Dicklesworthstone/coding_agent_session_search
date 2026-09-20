//! Bounded publication metadata, deliberately not a searchable generation.
//!
//! Reading this value authenticates current.json, the selected manifest and
//! the caller's corpus identity. It does NOT open or validate artifact bytes.
//! Only retained readers may use it for preflight and selection rechecks;
//! ordinary callers still use load_current_semantic_generation for disk audit.

use super::*;

/// Distinct from ValidatedSemanticGeneration: never a readiness receipt.
pub(crate) struct SemanticSelectionMetadata {
    pub(crate) pointer: SemanticCurrentPointerV1,
    pub(crate) manifest: SemanticGenerationManifestV1,
    pub(crate) generation_dir: PathBuf,
}

/// Vector-only disk validation for retained serving. This is intentionally NOT
/// ValidatedSemanticGeneration: optional ANN bytes have not been opened, hashed,
/// or certified healthy. Keep the complete authenticated manifest for selection
/// identity and for a separate, budgeted graph admission against exact owners.
pub(crate) struct ValidatedSemanticVectors {
    pub(crate) pointer: SemanticCurrentPointerV1,
    pub(crate) manifest: SemanticGenerationManifestV1,
    pub(crate) generation_dir: PathBuf,
}

impl SemanticSelectionMetadata {
    pub(crate) fn read(
        data_dir: &Path,
        expected_corpus: Option<&SemanticCorpusSnapshotIdentity>,
    ) -> Result<Self, SemanticGenerationError> {
        let started = Instant::now();
        let mut pointer = None;
        let mut manifest = None;
        let result = read_observed(data_dir, expected_corpus, &mut pointer, &mut manifest);
        if let Err(error) = &result {
            log_generation_validation_failure(pointer.as_ref(), manifest.as_ref(), error, started);
        }
        result
    }

    /// Validate every mandatory vector through the SAME disk validator used by
    /// sealing and full audits, without accessing optional graph bytes. A lost
    /// graph must not prevent exact serving or defeat an ANN admission budget.
    /// This does not produce the public full-generation validation receipt.
    pub(crate) fn validate_vectors(
        self,
        data_dir: &Path,
    ) -> Result<ValidatedSemanticVectors, SemanticGenerationError> {
        let started = Instant::now();
        // read_observed already authenticated and structurally validated the
        // complete manifest, including every ANN-to-base binding. This private
        // projection selects disk work only; never publish or return it as the
        // manifest identity, and never certify its optional graphs as healthy.
        let mut vectors = self.manifest.clone();
        vectors.artifacts.retain(|artifact| artifact.role.is_vector());
        vectors
            .validate_artifacts_on_disk(data_dir, false)
            .inspect_err(|error| {
                log_generation_validation_failure(
                    Some(&self.pointer),
                    Some(&self.manifest),
                    error,
                    started,
                );
            })?;
        Ok(ValidatedSemanticVectors {
            pointer: self.pointer,
            manifest: self.manifest,
            generation_dir: self.generation_dir,
        })
    }
}

// The public full loader shares these exact checks and keeps its historical
// diagnostic context. No second parser or weaker filename discovery exists.
pub(super) fn read_observed(
    data_dir: &Path,
    expected_corpus: Option<&SemanticCorpusSnapshotIdentity>,
    pointer_for_log: &mut Option<SemanticCurrentPointerV1>,
    manifest_for_log: &mut Option<SemanticGenerationManifestV1>,
) -> Result<SemanticSelectionMetadata, SemanticGenerationError> {
    let pointer_path = SemanticCurrentPointerV1::path(data_dir);
    match fs::symlink_metadata(&pointer_path) {
        Ok(metadata) if metadata_is_link_or_reparse(&metadata) => {
            return Err(SemanticGenerationError::InvalidPointer {
                reason: "current pointer must not be a symlink or reparse point".to_owned(),
            });
        }
        Ok(metadata) if !metadata.is_file() => {
            return Err(SemanticGenerationError::InvalidPointer {
                reason: "current pointer is not a regular file".to_owned(),
            });
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(SemanticGenerationError::MissingPointer);
        }
        Err(error) => {
            return Err(SemanticGenerationError::PointerIo {
                source: error.to_string(),
            });
        }
    }
    let pointer_bytes =
        read_bounded_file(&pointer_path, MAX_SEMANTIC_POINTER_BYTES).map_err(|error| {
            if error.kind() == std::io::ErrorKind::InvalidData {
                SemanticGenerationError::PointerParse {
                    source: error.to_string(),
                }
            } else {
                SemanticGenerationError::PointerIo {
                    source: error.to_string(),
                }
            }
        })?;
    let pointer = parse_current_pointer_bytes(&pointer_bytes)?;
    *pointer_for_log = Some(pointer.clone());
    pointer.validate()?;
    let loaded = load_manifest_selected_by_pointer(data_dir, &pointer)?;
    *manifest_for_log = Some(loaded.manifest.clone());
    if let Some(expected) = expected_corpus
        && expected != &loaded.manifest.corpus
    {
        return Err(SemanticGenerationError::StaleCorpus {
            expected: corpus_identity_sha256(expected)?,
            actual: corpus_identity_sha256(&loaded.manifest.corpus)?,
        });
    }
    Ok(SemanticSelectionMetadata {
        pointer,
        manifest: loaded.manifest,
        generation_dir: loaded.generation_dir,
    })
}

/// Read-only path preflight for optional ANN loading, after its byte budget.
/// The manifest already requires safe relative paths. Recheck each actual path
/// component here because exact serving deliberately did not touch ANN files.
/// Missing entries are left to the sidecar loader's unavailable diagnostic.
/// This is not race-free: cryptographic/structural graph admission still runs.
pub(crate) fn optional_ann_path_is_safe(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    if relative.as_os_str().is_empty()
        || !relative
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        return false;
    }
    let Ok(root_metadata) = fs::symlink_metadata(root) else {
        return false;
    };
    if !root_metadata.is_dir() || metadata_is_link_or_reparse(&root_metadata) {
        return false;
    }
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata_is_link_or_reparse(&metadata)
                    || (current != path && !metadata.is_dir())
                {
                    return false;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return true,
            Err(_) => return false,
        }
    }
    true
}
