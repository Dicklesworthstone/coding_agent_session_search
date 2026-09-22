//! Bounded publication metadata, deliberately not a searchable generation.
//!
//! Reading this value authenticates current.json, the selected manifest and
//! the caller's corpus identity. It does NOT open or validate artifact bytes.
//! Only retained readers may use it for preflight and selection rechecks;
//! ordinary callers still use load_current_semantic_generation for disk audit.

use super::*;
use std::fs::File;

/// Distinct from ValidatedSemanticGeneration: never a readiness receipt.
pub(crate) struct SemanticSelectionMetadata {
    pub(crate) pointer: SemanticCurrentPointerV1,
    pub(crate) manifest: SemanticGenerationManifestV1,
    pub(crate) generation_dir: PathBuf,
}

/// Metadata and opened-file identities selected for retained-reader admission.
/// This is NOT a content-validation or readiness receipt: the serving owner
/// must authenticate every image against the complete manifest before use.
/// Optional ANN files are neither opened nor certified by this selection.
pub(crate) struct SelectedSemanticVectors {
    pub(crate) pointer: SemanticCurrentPointerV1,
    pub(crate) manifest: SemanticGenerationManifestV1,
    pub(crate) generation_dir: PathBuf,
    files: Vec<VectorFilePreflight>,
}

impl SelectedSemanticVectors {
    /// Close the path/owner handoff after all sealed images were authenticated.
    /// Keeping each original file open makes even a same-byte pathname swap
    /// detectable. These handles are dropped when admission finishes, not kept
    /// by queries.
    pub(crate) fn verify_paths(&self) -> Result<(), SemanticGenerationError> {
        for file in &self.files {
            file.verify(&self.generation_dir)?;
        }
        Ok(())
    }
}

struct VectorFilePreflight {
    path: PathBuf,
    canonical: PathBuf,
    file: File,
    metadata: fs::Metadata,
    role: SemanticArtifactRole,
    size_bytes: u64,
}

impl VectorFilePreflight {
    fn verify(&self, root: &Path) -> Result<(), SemanticGenerationError> {
        let io = |error: std::io::Error| SemanticGenerationError::ArtifactIo {
            role: self.role,
            source: error.to_string(),
        };
        reject_existing_path_links(root, &self.path).map_err(io)?;
        reject_vector_wal(&self.path, self.role)?;
        let original = portable_file_identity(&self.file, &self.metadata).map_err(io)?;
        let metadata = self.file.metadata().map_err(io)?;
        let current = portable_file_identity(&self.file, &metadata).map_err(io)?;
        if current != original || current.is_some_and(|identity| identity.link_count != 1) {
            return invalid_manifest(
                SemanticManifestInvariantClass::Path,
                "selected vector file identity changed during admission",
            );
        }
        if metadata.len() != self.size_bytes {
            return Err(SemanticGenerationError::ArtifactSizeMismatch {
                role: self.role,
                expected: self.size_bytes,
                actual: metadata.len(),
            });
        }
        if self.path.canonicalize().map_err(io)? != self.canonical {
            return invalid_manifest(
                SemanticManifestInvariantClass::Path,
                "selected vector path changed during admission",
            );
        }
        verify_path_still_names_open_file(&self.canonical, original).map_err(io)
    }
}

fn reject_vector_wal(
    path: &Path,
    role: SemanticArtifactRole,
) -> Result<(), SemanticGenerationError> {
    match fs::symlink_metadata(wal_path_for(path)) {
        Ok(_) => Err(SemanticGenerationError::ArtifactIo {
            role,
            source: "immutable vector artifact has a WAL sidecar".to_owned(),
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(SemanticGenerationError::ArtifactIo {
            role,
            source: format!("failed inspecting immutable WAL sidecar: {error}"),
        }),
    }
}

/// Reuse the full auditor's path, file-identity and WAL rules without reading
/// any vector contents. The retained FSVI owner is the ONE content validator.
fn preflight_vector_file(
    root: &Path,
    canonical_root: &Path,
    relative: &str,
    role: SemanticArtifactRole,
    size_bytes: u64,
) -> Result<VectorFilePreflight, SemanticGenerationError> {
    let io = |error: std::io::Error| SemanticGenerationError::ArtifactIo {
        role,
        source: error.to_string(),
    };
    let relative = normalized_manifest_relative_path(relative).map_err(|reason| {
        SemanticGenerationError::InvalidManifest {
            class: SemanticManifestInvariantClass::Path,
            reason,
        }
    })?;
    let path = root.join(relative);
    reject_existing_path_links(root, &path).map_err(io)?;
    reject_vector_wal(&path, role)?;
    // Reject special files before open; O_NONBLOCK also contains a FIFO swap
    // between this check and open on Unix. No data is read from this handle.
    if !fs::symlink_metadata(&path).map_err(io)?.is_file() {
        return Err(SemanticGenerationError::ArtifactIo {
            role,
            source: "manifest-declared artifact is not a regular file".to_owned(),
        });
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(&path).map_err(io)?;
    let metadata = file.metadata().map_err(io)?;
    if !metadata.is_file() {
        return Err(SemanticGenerationError::ArtifactIo {
            role,
            source: "manifest-declared artifact is not a regular file".to_owned(),
        });
    }
    let canonical = path.canonicalize().map_err(io)?;
    if !canonical.starts_with(canonical_root) {
        return Err(SemanticGenerationError::ArtifactIo {
            role,
            source: "manifest-declared artifact escapes the generation root".to_owned(),
        });
    }
    let selected = VectorFilePreflight {
        path,
        canonical,
        file,
        metadata,
        role,
        size_bytes,
    };
    selected.verify(root)?;
    Ok(selected)
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

    /// Select all mandatory vector paths after the caller's byte-budget check.
    /// This preserves the full manifest identity, but deliberately does not
    /// hash or parse vector contents: only the subsequently retained FSVI
    /// owners may establish those content claims. Full disk audits and sealing
    /// continue to use validate_artifacts_on_disk, including its SHA-256 pass.
    pub(crate) fn preflight_vectors(
        self,
        data_dir: &Path,
    ) -> Result<SelectedSemanticVectors, SemanticGenerationError> {
        let started = Instant::now();
        let result = (|| {
            validate_existing_generation_directory(data_dir, &self.generation_dir)?;
            let canonical_root = self.generation_dir.canonicalize().map_err(|error| {
                SemanticGenerationError::ManifestIo {
                    generation_id: self.manifest.generation_id.clone(),
                    source: error.to_string(),
                }
            })?;
            let mut canonical_artifacts = BTreeSet::new();
            let mut physical_artifacts = BTreeSet::new();
            let mut files = Vec::new();
            for artifact in self
                .manifest
                .artifacts
                .iter()
                .filter(|item| item.role.is_vector())
            {
                let file = preflight_vector_file(
                    &self.generation_dir,
                    &canonical_root,
                    &artifact.relative_path,
                    artifact.role,
                    artifact.size_bytes,
                )?;
                let identity =
                    portable_file_identity(&file.file, &file.metadata).map_err(|error| {
                        SemanticGenerationError::ArtifactIo {
                            role: artifact.role,
                            source: error.to_string(),
                        }
                    })?;
                if !canonical_artifacts.insert(file.canonical.clone())
                    || identity.is_some_and(|identity| !physical_artifacts.insert(identity))
                {
                    return Err(SemanticGenerationError::InvalidManifest {
                        class: SemanticManifestInvariantClass::Path,
                        reason: "multiple vector artifacts resolve to the same file".to_owned(),
                    });
                }
                files.push(file);
            }
            Ok(files)
        })();
        let files = result.inspect_err(|error| {
            log_generation_validation_failure(
                Some(&self.pointer),
                Some(&self.manifest),
                error,
                started,
            );
        })?;
        Ok(SelectedSemanticVectors {
            pointer: self.pointer,
            manifest: self.manifest,
            generation_dir: self.generation_dir,
            files,
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
                if metadata_is_link_or_reparse(&metadata) || (current != path && !metadata.is_dir())
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

#[cfg(test)]
mod tests;
