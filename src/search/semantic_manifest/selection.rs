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

    /// Preflight callers must explicitly cross the full artifact-validation
    /// boundary before this value can become a disk-validation receipt.
    pub(crate) fn validate_artifacts(
        self,
        data_dir: &Path,
    ) -> Result<ValidatedSemanticGeneration, SemanticGenerationError> {
        let started = Instant::now();
        let artifact_paths = self
            .manifest
            .validate_artifacts_on_disk(data_dir, false)
            .inspect_err(|error| {
                log_generation_validation_failure(
                    Some(&self.pointer),
                    Some(&self.manifest),
                    error,
                    started,
                );
            })?;
        Ok(ValidatedSemanticGeneration {
            pointer: self.pointer,
            manifest: self.manifest,
            generation_dir: self.generation_dir,
            artifact_paths,
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
