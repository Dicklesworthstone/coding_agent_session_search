//! Repairable evidence-bundle manifests for derived search artifacts.
//!
//! This module is deliberately producer-neutral: lexical generations, semantic
//! shards, and database backups can all describe their files as content-addressed
//! chunks, then ask the same verifier whether the bundle is complete,
//! partially repairable from parity metadata, or unsafe to use.

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

pub const EVIDENCE_BUNDLE_MANIFEST_VERSION: u32 = 1;
pub const EVIDENCE_BUNDLE_MANIFEST_FILE: &str = "evidence-bundle-manifest.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceBundleKind {
    LexicalGeneration,
    SemanticShard,
    DatabaseBackup,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceBundleChunkRole {
    Manifest,
    LexicalShard,
    SemanticShard,
    DatabaseMain,
    DatabaseWal,
    Metadata,
    Parity,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceBundleVerificationStatus {
    Complete,
    PartiallyRepairable,
    Unsafe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceBundleIssueKind {
    CorruptManifest,
    UnsupportedManifestVersion,
    EmptyManifest,
    DuplicateChunkPath,
    UnsafeChunkPath,
    MissingChunk,
    SizeMismatch,
    DigestMismatch,
    InvalidWalStateChunk,
    WalMainMismatch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceBundleChunk {
    pub path: String,
    pub role: EvidenceBundleChunkRole,
    pub size_bytes: u64,
    pub blake3: String,
    #[serde(default = "default_required_chunk")]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parity_group: Option<String>,
}

impl EvidenceBundleChunk {
    pub fn from_file(
        bundle_root: &Path,
        relative_path: impl Into<String>,
        role: EvidenceBundleChunkRole,
        required: bool,
        parity_group: Option<String>,
    ) -> Result<Self> {
        let path = relative_path.into();
        let resolved = resolve_existing_bundle_path(bundle_root, &path)?;
        let (size_bytes, blake3) = digest_file(&resolved)
            .with_context(|| format!("digesting bundle chunk {}", resolved.display()))?;
        Ok(Self {
            path,
            role,
            size_bytes,
            blake3,
            required,
            parity_group,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceBundleParityGroup {
    pub group_id: String,
    #[serde(default)]
    pub chunk_paths: Vec<String>,
    pub repairable_failed_chunks: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatabaseWalStateEvidence {
    pub main_chunk_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wal_chunk_path: Option<String>,
    pub main_state_fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wal_base_fingerprint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceBundleManifest {
    pub manifest_version: u32,
    pub bundle_id: String,
    pub kind: EvidenceBundleKind,
    pub created_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_db_fingerprint: Option<String>,
    #[serde(default)]
    pub chunks: Vec<EvidenceBundleChunk>,
    #[serde(default)]
    pub parity_groups: Vec<EvidenceBundleParityGroup>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database_wal_state: Option<DatabaseWalStateEvidence>,
    #[serde(default = "default_explicit_delete_required")]
    pub explicit_delete_required: bool,
}

impl EvidenceBundleManifest {
    pub fn new(bundle_id: impl Into<String>, kind: EvidenceBundleKind, created_at_ms: i64) -> Self {
        Self {
            manifest_version: EVIDENCE_BUNDLE_MANIFEST_VERSION,
            bundle_id: bundle_id.into(),
            kind,
            created_at_ms,
            source_db_fingerprint: None,
            chunks: Vec::new(),
            parity_groups: Vec::new(),
            database_wal_state: None,
            explicit_delete_required: true,
        }
    }

    pub fn path(bundle_root: &Path) -> PathBuf {
        bundle_root.join(EVIDENCE_BUNDLE_MANIFEST_FILE)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let bytes = fs::read(path)
            .with_context(|| format!("reading evidence bundle manifest {}", path.display()))?;
        serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing evidence bundle manifest {}", path.display()))
    }

    pub fn save(&self, bundle_root: &Path) -> Result<PathBuf> {
        fs::create_dir_all(bundle_root)
            .with_context(|| format!("creating evidence bundle root {}", bundle_root.display()))?;
        let path = Self::path(bundle_root);
        let tmp_path = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(self)
            .with_context(|| "serializing evidence bundle manifest")?;
        fs::write(&tmp_path, bytes)
            .with_context(|| format!("writing evidence bundle manifest {}", tmp_path.display()))?;
        fs::rename(&tmp_path, &path).with_context(|| {
            format!(
                "publishing evidence bundle manifest {} -> {}",
                tmp_path.display(),
                path.display()
            )
        })?;
        Ok(path)
    }

    pub fn verify(&self, bundle_root: &Path) -> EvidenceBundleVerificationReport {
        verify_manifest(self, bundle_root)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceBundleIssue {
    pub kind: EvidenceBundleIssueKind,
    pub path: Option<String>,
    pub message: String,
    pub repairable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceBundleGcDryRun {
    pub dry_run: bool,
    pub explicit_delete_required: bool,
    pub deletion_allowed: bool,
    pub retained_chunk_count: usize,
    pub retained_bytes: u64,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceBundleVerificationReport {
    pub manifest_version: Option<u32>,
    pub bundle_id: Option<String>,
    pub kind: Option<EvidenceBundleKind>,
    pub status: EvidenceBundleVerificationStatus,
    pub issues: Vec<EvidenceBundleIssue>,
    pub verified_chunk_count: usize,
    pub repairable_issue_count: usize,
    pub unsafe_issue_count: usize,
    pub expected_chunk_count: usize,
    pub expected_bytes: u64,
    pub verified_bytes: u64,
    pub gc_dry_run: EvidenceBundleGcDryRun,
}

impl EvidenceBundleVerificationReport {
    pub fn is_complete(&self) -> bool {
        self.status == EvidenceBundleVerificationStatus::Complete
    }

    pub fn is_partially_repairable(&self) -> bool {
        self.status == EvidenceBundleVerificationStatus::PartiallyRepairable
    }

    pub fn is_unsafe(&self) -> bool {
        self.status == EvidenceBundleVerificationStatus::Unsafe
    }
}

pub fn verify_evidence_bundle_manifest_file(
    bundle_root: &Path,
    manifest_path: &Path,
) -> EvidenceBundleVerificationReport {
    match EvidenceBundleManifest::load(manifest_path) {
        Ok(manifest) => manifest.verify(bundle_root),
        Err(err) => unsafe_report(
            EvidenceBundleIssueKind::CorruptManifest,
            None,
            format!("manifest could not be loaded: {err}"),
        ),
    }
}

fn verify_manifest(
    manifest: &EvidenceBundleManifest,
    bundle_root: &Path,
) -> EvidenceBundleVerificationReport {
    let mut issues = Vec::new();
    let mut chunk_failures = Vec::new();
    let mut verified_chunk_count = 0usize;
    let mut verified_bytes = 0u64;
    let expected_bytes = manifest
        .chunks
        .iter()
        .fold(0u64, |sum, chunk| sum.saturating_add(chunk.size_bytes));

    if manifest.manifest_version != EVIDENCE_BUNDLE_MANIFEST_VERSION {
        issues.push(issue(
            EvidenceBundleIssueKind::UnsupportedManifestVersion,
            None,
            format!(
                "manifest version {} is not supported by verifier version {}",
                manifest.manifest_version, EVIDENCE_BUNDLE_MANIFEST_VERSION
            ),
            false,
        ));
    }
    if manifest.chunks.is_empty() {
        issues.push(issue(
            EvidenceBundleIssueKind::EmptyManifest,
            None,
            "manifest contains no chunks".to_string(),
            false,
        ));
    }

    let parity_index = parity_index(manifest);
    let mut verified_parity_groups = BTreeSet::new();
    let mut seen_paths = BTreeSet::new();
    for chunk in &manifest.chunks {
        if !seen_paths.insert(chunk.path.clone()) {
            chunk_failures.push(raw_chunk_failure(
                EvidenceBundleIssueKind::DuplicateChunkPath,
                chunk.path.clone(),
                "duplicate chunk path in manifest".to_string(),
            ));
            continue;
        }

        let resolved = match resolve_bundle_path(bundle_root, &chunk.path) {
            Ok(path) => path,
            Err(err) => {
                chunk_failures.push(raw_chunk_failure(
                    EvidenceBundleIssueKind::UnsafeChunkPath,
                    chunk.path.clone(),
                    err.to_string(),
                ));
                continue;
            }
        };
        if !resolved.exists() {
            if !chunk.required {
                continue;
            }
            chunk_failures.push(raw_chunk_failure(
                EvidenceBundleIssueKind::MissingChunk,
                chunk.path.clone(),
                format!("required bundle chunk {} is missing", chunk.path),
            ));
            continue;
        }
        let resolved = match resolve_existing_bundle_path(bundle_root, &chunk.path) {
            Ok(path) => path,
            Err(err) => {
                chunk_failures.push(raw_chunk_failure(
                    EvidenceBundleIssueKind::UnsafeChunkPath,
                    chunk.path.clone(),
                    err.to_string(),
                ));
                continue;
            }
        };

        match digest_file(&resolved) {
            Ok((actual_size, actual_digest)) => {
                if actual_size != chunk.size_bytes {
                    chunk_failures.push(raw_chunk_failure(
                        EvidenceBundleIssueKind::SizeMismatch,
                        chunk.path.clone(),
                        format!(
                            "chunk {} has size {}, expected {}",
                            chunk.path, actual_size, chunk.size_bytes
                        ),
                    ));
                    continue;
                }
                if actual_digest != chunk.blake3 {
                    chunk_failures.push(raw_chunk_failure(
                        EvidenceBundleIssueKind::DigestMismatch,
                        chunk.path.clone(),
                        format!("chunk {} digest does not match manifest", chunk.path),
                    ));
                    continue;
                }
                verified_chunk_count = verified_chunk_count.saturating_add(1);
                verified_bytes = verified_bytes.saturating_add(actual_size);
                if chunk.role == EvidenceBundleChunkRole::Parity
                    && let Some(group) = parity_index.get(&chunk.path)
                {
                    verified_parity_groups.insert(group.group_id.clone());
                }
            }
            Err(err) => chunk_failures.push(raw_chunk_failure(
                EvidenceBundleIssueKind::MissingChunk,
                chunk.path.clone(),
                format!("chunk {} could not be read: {err}", chunk.path),
            )),
        }
    }

    let failure_counts = chunk_failure_counts_by_parity_group(&chunk_failures, &parity_index);
    for failure in chunk_failures {
        let repairable = chunk_failure_is_repairable(
            failure.kind,
            &failure.path,
            &parity_index,
            &verified_parity_groups,
            &failure_counts,
        );
        issues.push(issue(
            failure.kind,
            Some(failure.path),
            failure.message,
            repairable,
        ));
    }

    if let Some(wal_state) = &manifest.database_wal_state {
        validate_wal_state_chunk_declaration(
            &mut issues,
            manifest,
            &wal_state.main_chunk_path,
            "main DB",
        );
        if let Some(wal_chunk_path) = wal_state.wal_chunk_path.as_deref() {
            validate_wal_state_chunk_declaration(&mut issues, manifest, wal_chunk_path, "WAL");
            if wal_state.wal_base_fingerprint.as_deref()
                != Some(wal_state.main_state_fingerprint.as_str())
            {
                issues.push(issue(
                    EvidenceBundleIssueKind::WalMainMismatch,
                    wal_state.wal_chunk_path.clone(),
                    format!(
                        "WAL base fingerprint {:?} does not match main DB fingerprint {}",
                        wal_state.wal_base_fingerprint, wal_state.main_state_fingerprint
                    ),
                    false,
                ));
            }
        }
    }

    let repairable_issue_count = issues.iter().filter(|issue| issue.repairable).count();
    let unsafe_issue_count = issues.len().saturating_sub(repairable_issue_count);
    let status = if unsafe_issue_count > 0 {
        EvidenceBundleVerificationStatus::Unsafe
    } else if repairable_issue_count > 0 {
        EvidenceBundleVerificationStatus::PartiallyRepairable
    } else {
        EvidenceBundleVerificationStatus::Complete
    };

    EvidenceBundleVerificationReport {
        manifest_version: Some(manifest.manifest_version),
        bundle_id: Some(manifest.bundle_id.clone()),
        kind: Some(manifest.kind),
        status,
        issues,
        verified_chunk_count,
        repairable_issue_count,
        unsafe_issue_count,
        expected_chunk_count: manifest.chunks.len(),
        expected_bytes,
        verified_bytes,
        gc_dry_run: EvidenceBundleGcDryRun {
            dry_run: true,
            explicit_delete_required: manifest.explicit_delete_required,
            deletion_allowed: false,
            retained_chunk_count: manifest.chunks.len(),
            retained_bytes: expected_bytes,
            reason: "evidence bundle verifier is read-only; deletion requires a separate explicit operator-approved GC path".to_string(),
        },
    }
}

#[derive(Debug)]
struct RawChunkFailure {
    kind: EvidenceBundleIssueKind,
    path: String,
    message: String,
}

fn raw_chunk_failure(
    kind: EvidenceBundleIssueKind,
    path: String,
    message: String,
) -> RawChunkFailure {
    RawChunkFailure {
        kind,
        path,
        message,
    }
}

fn parity_index(manifest: &EvidenceBundleManifest) -> BTreeMap<String, &EvidenceBundleParityGroup> {
    let mut index = BTreeMap::new();
    for group in &manifest.parity_groups {
        for path in &group.chunk_paths {
            index.insert(path.clone(), group);
        }
    }
    index
}

fn chunk_failure_counts_by_parity_group(
    failures: &[RawChunkFailure],
    parity_index: &BTreeMap<String, &EvidenceBundleParityGroup>,
) -> BTreeMap<String, u32> {
    let mut counts = BTreeMap::new();
    for failure in failures {
        if let Some(group) = parity_index.get(&failure.path) {
            *counts.entry(group.group_id.clone()).or_insert(0) += 1;
        }
    }
    counts
}

fn chunk_failure_is_repairable(
    kind: EvidenceBundleIssueKind,
    path: &str,
    parity_index: &BTreeMap<String, &EvidenceBundleParityGroup>,
    verified_parity_groups: &BTreeSet<String>,
    failure_counts: &BTreeMap<String, u32>,
) -> bool {
    if !matches!(
        kind,
        EvidenceBundleIssueKind::MissingChunk
            | EvidenceBundleIssueKind::SizeMismatch
            | EvidenceBundleIssueKind::DigestMismatch
    ) {
        return false;
    }
    let Some(group) = parity_index.get(path) else {
        return false;
    };
    if !verified_parity_groups.contains(&group.group_id) {
        return false;
    }
    let failures_in_group = failure_counts
        .get(&group.group_id)
        .copied()
        .unwrap_or_default();
    failures_in_group > 0 && failures_in_group <= group.repairable_failed_chunks
}

fn validate_wal_state_chunk_declaration(
    issues: &mut Vec<EvidenceBundleIssue>,
    manifest: &EvidenceBundleManifest,
    path: &str,
    label: &str,
) {
    let Some(chunk) = manifest.chunks.iter().find(|chunk| chunk.path == path) else {
        issues.push(issue(
            EvidenceBundleIssueKind::InvalidWalStateChunk,
            Some(path.to_string()),
            format!("database_wal_state {label} chunk {path} is not declared in manifest chunks"),
            false,
        ));
        return;
    };

    if !chunk.required {
        issues.push(issue(
            EvidenceBundleIssueKind::InvalidWalStateChunk,
            Some(path.to_string()),
            format!("database_wal_state {label} chunk {path} must be declared as required"),
            false,
        ));
    }
}

fn issue(
    kind: EvidenceBundleIssueKind,
    path: Option<String>,
    message: String,
    repairable: bool,
) -> EvidenceBundleIssue {
    EvidenceBundleIssue {
        kind,
        path,
        message,
        repairable,
    }
}

fn unsafe_report(
    kind: EvidenceBundleIssueKind,
    path: Option<String>,
    message: String,
) -> EvidenceBundleVerificationReport {
    EvidenceBundleVerificationReport {
        manifest_version: None,
        bundle_id: None,
        kind: None,
        status: EvidenceBundleVerificationStatus::Unsafe,
        issues: vec![issue(kind, path, message, false)],
        verified_chunk_count: 0,
        repairable_issue_count: 0,
        unsafe_issue_count: 1,
        expected_chunk_count: 0,
        expected_bytes: 0,
        verified_bytes: 0,
        gc_dry_run: EvidenceBundleGcDryRun {
            dry_run: true,
            explicit_delete_required: true,
            deletion_allowed: false,
            retained_chunk_count: 0,
            retained_bytes: 0,
            reason: "corrupt or unreadable evidence bundle manifest cannot authorize deletion"
                .to_string(),
        },
    }
}

fn digest_file(path: &Path) -> Result<(u64, String)> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = blake3::Hasher::new();
    let mut size = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("reading {}", path.display()))?;
        if read == 0 {
            break;
        }
        size = size.saturating_add(read as u64);
        hasher.update(&buffer[..read]);
    }
    Ok((size, hasher.finalize().to_hex().to_string()))
}

fn resolve_bundle_path(bundle_root: &Path, relative_path: &str) -> Result<PathBuf> {
    let path = Path::new(relative_path);
    if path.is_absolute() {
        bail!("bundle chunk path must be relative: {relative_path}");
    }
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                bail!("bundle chunk path contains unsafe component: {relative_path}");
            }
        }
    }
    if relative_path.is_empty() {
        return Err(anyhow!("bundle chunk path must not be empty"));
    }
    Ok(bundle_root.join(path))
}

fn resolve_existing_bundle_path(bundle_root: &Path, relative_path: &str) -> Result<PathBuf> {
    let resolved = resolve_bundle_path(bundle_root, relative_path)?;
    let canonical_root = fs::canonicalize(bundle_root)
        .with_context(|| format!("canonicalizing bundle root {}", bundle_root.display()))?;
    let canonical_resolved = fs::canonicalize(&resolved)
        .with_context(|| format!("canonicalizing bundle chunk {}", resolved.display()))?;
    if !canonical_resolved.starts_with(&canonical_root) {
        bail!("bundle chunk path resolves outside bundle root: {relative_path}");
    }
    Ok(canonical_resolved)
}

fn default_required_chunk() -> bool {
    true
}

fn default_explicit_delete_required() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_chunk(root: &Path, path: &str, bytes: &[u8]) {
        let full_path = root.join(path);
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(full_path, bytes).unwrap();
    }

    fn chunk(root: &Path, path: &str, role: EvidenceBundleChunkRole) -> EvidenceBundleChunk {
        EvidenceBundleChunk::from_file(root, path, role, true, None).unwrap()
    }

    #[test]
    fn verifier_proves_complete_lexical_generation_bundle() {
        let tmp = TempDir::new().unwrap();
        write_chunk(tmp.path(), "manifest.json", br#"{"docs":2}"#);
        write_chunk(tmp.path(), "shards/segment-a", b"lexical shard bytes");

        let mut manifest = EvidenceBundleManifest::new(
            "lexical-generation-1",
            EvidenceBundleKind::LexicalGeneration,
            1_700_000_000_000,
        );
        manifest.chunks = vec![
            chunk(
                tmp.path(),
                "manifest.json",
                EvidenceBundleChunkRole::Manifest,
            ),
            chunk(
                tmp.path(),
                "shards/segment-a",
                EvidenceBundleChunkRole::LexicalShard,
            ),
        ];
        manifest.save(tmp.path()).unwrap();

        let report = verify_evidence_bundle_manifest_file(
            tmp.path(),
            &EvidenceBundleManifest::path(tmp.path()),
        );
        assert!(report.is_complete(), "{report:?}");
        assert_eq!(report.verified_chunk_count, 2);
        assert_eq!(report.unsafe_issue_count, 0);
        assert!(!report.gc_dry_run.deletion_allowed);
    }

    #[test]
    fn corrupt_manifest_sidecar_is_unsafe_to_use() {
        let tmp = TempDir::new().unwrap();
        let manifest_path = EvidenceBundleManifest::path(tmp.path());
        fs::write(&manifest_path, b"{not-json").unwrap();

        let report = verify_evidence_bundle_manifest_file(tmp.path(), &manifest_path);
        assert!(report.is_unsafe(), "{report:?}");
        assert_eq!(
            report.issues[0].kind,
            EvidenceBundleIssueKind::CorruptManifest
        );
        assert!(!report.issues[0].repairable);
        assert!(!report.gc_dry_run.deletion_allowed);
    }

    #[test]
    fn missing_semantic_shard_with_parity_is_partially_repairable() {
        let tmp = TempDir::new().unwrap();
        write_chunk(tmp.path(), "semantic/shard-0.f16", b"semantic shard zero");
        write_chunk(tmp.path(), "semantic/parity-0.bin", b"parity bytes");

        let mut shard = chunk(
            tmp.path(),
            "semantic/shard-0.f16",
            EvidenceBundleChunkRole::SemanticShard,
        );
        shard.parity_group = Some("semantic-parity-0".to_string());
        let mut missing = shard.clone();
        missing.path = "semantic/shard-1.f16".to_string();
        missing.size_bytes = 19;
        missing.blake3 = blake3::hash(b"semantic shard one").to_hex().to_string();
        let mut parity = chunk(
            tmp.path(),
            "semantic/parity-0.bin",
            EvidenceBundleChunkRole::Parity,
        );
        parity.parity_group = Some("semantic-parity-0".to_string());

        let mut manifest = EvidenceBundleManifest::new(
            "semantic-tier-fast-0",
            EvidenceBundleKind::SemanticShard,
            1_700_000_000_001,
        );
        manifest.chunks = vec![shard, missing, parity];
        manifest.parity_groups = vec![EvidenceBundleParityGroup {
            group_id: "semantic-parity-0".to_string(),
            chunk_paths: vec![
                "semantic/shard-0.f16".to_string(),
                "semantic/shard-1.f16".to_string(),
                "semantic/parity-0.bin".to_string(),
            ],
            repairable_failed_chunks: 1,
        }];

        let report = manifest.verify(tmp.path());
        assert!(report.is_partially_repairable(), "{report:?}");
        assert_eq!(report.repairable_issue_count, 1);
        assert_eq!(report.unsafe_issue_count, 0);
        assert_eq!(report.issues[0].kind, EvidenceBundleIssueKind::MissingChunk);
        assert!(report.issues[0].repairable);
    }

    #[test]
    fn declared_parity_without_verified_parity_artifact_is_unsafe() {
        let tmp = TempDir::new().unwrap();
        write_chunk(tmp.path(), "semantic/shard-0.f16", b"semantic shard zero");

        let mut shard = chunk(
            tmp.path(),
            "semantic/shard-0.f16",
            EvidenceBundleChunkRole::SemanticShard,
        );
        shard.parity_group = Some("semantic-parity-0".to_string());
        let mut missing = shard.clone();
        missing.path = "semantic/shard-1.f16".to_string();
        missing.size_bytes = 19;
        missing.blake3 = blake3::hash(b"semantic shard one").to_hex().to_string();

        let mut manifest = EvidenceBundleManifest::new(
            "semantic-missing-parity-artifact",
            EvidenceBundleKind::SemanticShard,
            1_700_000_000_002,
        );
        manifest.chunks = vec![shard, missing];
        manifest.parity_groups = vec![EvidenceBundleParityGroup {
            group_id: "semantic-parity-0".to_string(),
            chunk_paths: vec![
                "semantic/shard-0.f16".to_string(),
                "semantic/shard-1.f16".to_string(),
                "semantic/parity-0.bin".to_string(),
            ],
            repairable_failed_chunks: 1,
        }];

        let report = manifest.verify(tmp.path());
        assert!(report.is_unsafe(), "{report:?}");
        assert_eq!(report.repairable_issue_count, 0);
        assert_eq!(report.unsafe_issue_count, 1);
        assert_eq!(report.issues[0].kind, EvidenceBundleIssueKind::MissingChunk);
        assert!(
            !report.issues[0].repairable,
            "a parity declaration without a verified parity artifact must not claim repairability"
        );
    }

    #[test]
    fn parity_does_not_repair_manifest_structure_errors() {
        let tmp = TempDir::new().unwrap();
        write_chunk(tmp.path(), "semantic/shard-0.f16", b"semantic shard zero");

        let mut shard = chunk(
            tmp.path(),
            "semantic/shard-0.f16",
            EvidenceBundleChunkRole::SemanticShard,
        );
        shard.parity_group = Some("semantic-parity-0".to_string());

        let mut manifest = EvidenceBundleManifest::new(
            "semantic-duplicate-path",
            EvidenceBundleKind::SemanticShard,
            1_700_000_000_002,
        );
        manifest.chunks = vec![shard.clone(), shard];
        manifest.parity_groups = vec![EvidenceBundleParityGroup {
            group_id: "semantic-parity-0".to_string(),
            chunk_paths: vec!["semantic/shard-0.f16".to_string()],
            repairable_failed_chunks: 1,
        }];

        let report = manifest.verify(tmp.path());
        assert!(report.is_unsafe(), "{report:?}");
        assert_eq!(
            report.issues[0].kind,
            EvidenceBundleIssueKind::DuplicateChunkPath
        );
        assert!(!report.issues[0].repairable);
    }

    #[test]
    fn mismatched_database_wal_state_is_unsafe_even_when_files_hash() {
        let tmp = TempDir::new().unwrap();
        write_chunk(tmp.path(), "db/cass.db", b"main db bytes");
        write_chunk(tmp.path(), "db/cass.db-wal", b"wal bytes");

        let mut manifest = EvidenceBundleManifest::new(
            "db-backup-1",
            EvidenceBundleKind::DatabaseBackup,
            1_700_000_000_003,
        );
        manifest.chunks = vec![
            chunk(
                tmp.path(),
                "db/cass.db",
                EvidenceBundleChunkRole::DatabaseMain,
            ),
            chunk(
                tmp.path(),
                "db/cass.db-wal",
                EvidenceBundleChunkRole::DatabaseWal,
            ),
        ];
        manifest.database_wal_state = Some(DatabaseWalStateEvidence {
            main_chunk_path: "db/cass.db".to_string(),
            wal_chunk_path: Some("db/cass.db-wal".to_string()),
            main_state_fingerprint: "main-fp".to_string(),
            wal_base_fingerprint: Some("other-main-fp".to_string()),
        });

        let report = manifest.verify(tmp.path());
        assert!(report.is_unsafe(), "{report:?}");
        assert_eq!(report.verified_chunk_count, 2);
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.kind == EvidenceBundleIssueKind::WalMainMismatch)
        );
    }

    #[test]
    fn database_wal_state_rejects_undeclared_wal_chunk() {
        let tmp = TempDir::new().unwrap();
        write_chunk(tmp.path(), "db/cass.db", b"main db bytes");

        let mut manifest = EvidenceBundleManifest::new(
            "db-backup-undeclared-wal",
            EvidenceBundleKind::DatabaseBackup,
            1_700_000_000_003,
        );
        manifest.chunks = vec![chunk(
            tmp.path(),
            "db/cass.db",
            EvidenceBundleChunkRole::DatabaseMain,
        )];
        manifest.database_wal_state = Some(DatabaseWalStateEvidence {
            main_chunk_path: "db/cass.db".to_string(),
            wal_chunk_path: Some("db/cass.db-wal".to_string()),
            main_state_fingerprint: "main-fp".to_string(),
            wal_base_fingerprint: Some("main-fp".to_string()),
        });

        let report = manifest.verify(tmp.path());
        assert!(report.is_unsafe(), "{report:?}");
        assert!(
            report.issues.iter().any(|issue| {
                issue.kind == EvidenceBundleIssueKind::InvalidWalStateChunk
                    && issue.path.as_deref() == Some("db/cass.db-wal")
            }),
            "database_wal_state must not certify an undeclared WAL chunk: {report:?}"
        );
    }

    #[test]
    fn database_wal_state_rejects_optional_wal_chunk() {
        let tmp = TempDir::new().unwrap();
        write_chunk(tmp.path(), "db/cass.db", b"main db bytes");

        let mut manifest = EvidenceBundleManifest::new(
            "db-backup-optional-wal",
            EvidenceBundleKind::DatabaseBackup,
            1_700_000_000_003,
        );
        manifest.chunks = vec![
            chunk(
                tmp.path(),
                "db/cass.db",
                EvidenceBundleChunkRole::DatabaseMain,
            ),
            EvidenceBundleChunk {
                path: "db/cass.db-wal".to_string(),
                role: EvidenceBundleChunkRole::DatabaseWal,
                size_bytes: 0,
                blake3: blake3::hash(b"").to_hex().to_string(),
                required: false,
                parity_group: None,
            },
        ];
        manifest.database_wal_state = Some(DatabaseWalStateEvidence {
            main_chunk_path: "db/cass.db".to_string(),
            wal_chunk_path: Some("db/cass.db-wal".to_string()),
            main_state_fingerprint: "main-fp".to_string(),
            wal_base_fingerprint: Some("main-fp".to_string()),
        });

        let report = manifest.verify(tmp.path());
        assert!(report.is_unsafe(), "{report:?}");
        assert!(
            report.issues.iter().any(|issue| {
                issue.kind == EvidenceBundleIssueKind::InvalidWalStateChunk
                    && issue.path.as_deref() == Some("db/cass.db-wal")
            }),
            "database_wal_state WAL chunks must not be optional: {report:?}"
        );
    }

    #[test]
    fn verifier_gc_surface_is_dry_run_and_does_not_delete_files() {
        let tmp = TempDir::new().unwrap();
        write_chunk(tmp.path(), "db/cass.db", b"main db bytes");

        let mut manifest = EvidenceBundleManifest::new(
            "db-backup-retained",
            EvidenceBundleKind::DatabaseBackup,
            1_700_000_000_004,
        );
        manifest.chunks = vec![chunk(
            tmp.path(),
            "db/cass.db",
            EvidenceBundleChunkRole::DatabaseMain,
        )];

        let report = manifest.verify(tmp.path());
        assert!(report.is_complete(), "{report:?}");
        assert!(report.gc_dry_run.dry_run);
        assert!(report.gc_dry_run.explicit_delete_required);
        assert!(!report.gc_dry_run.deletion_allowed);
        assert!(tmp.path().join("db/cass.db").exists());
    }

    #[test]
    fn missing_optional_chunk_does_not_make_bundle_unsafe() {
        let tmp = TempDir::new().unwrap();
        write_chunk(tmp.path(), "db/cass.db", b"main db bytes");

        let mut manifest = EvidenceBundleManifest::new(
            "db-backup-with-optional-sidecar",
            EvidenceBundleKind::DatabaseBackup,
            1_700_000_000_005,
        );
        manifest.chunks = vec![
            chunk(
                tmp.path(),
                "db/cass.db",
                EvidenceBundleChunkRole::DatabaseMain,
            ),
            EvidenceBundleChunk {
                path: "db/cass.db-shm".to_string(),
                role: EvidenceBundleChunkRole::Metadata,
                size_bytes: 0,
                blake3: blake3::hash(b"").to_hex().to_string(),
                required: false,
                parity_group: None,
            },
        ];

        let report = manifest.verify(tmp.path());
        assert!(report.is_complete(), "{report:?}");
        assert_eq!(report.verified_chunk_count, 1);
        assert!(report.issues.is_empty());
    }

    #[test]
    fn unsafe_relative_paths_are_rejected() {
        let tmp = TempDir::new().unwrap();
        let mut manifest = EvidenceBundleManifest::new(
            "bad-path",
            EvidenceBundleKind::LexicalGeneration,
            1_700_000_000_006,
        );
        manifest.chunks = vec![EvidenceBundleChunk {
            path: "../outside".to_string(),
            role: EvidenceBundleChunkRole::LexicalShard,
            size_bytes: 1,
            blake3: blake3::hash(b"x").to_hex().to_string(),
            required: true,
            parity_group: None,
        }];

        let report = manifest.verify(tmp.path());
        assert!(report.is_unsafe(), "{report:?}");
        assert_eq!(
            report.issues[0].kind,
            EvidenceBundleIssueKind::UnsafeChunkPath
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_chunk_that_escapes_bundle_root_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let outside_chunk = outside.path().join("segment-a");
        fs::write(&outside_chunk, b"outside shard bytes").unwrap();
        fs::create_dir_all(tmp.path().join("shards")).unwrap();
        std::os::unix::fs::symlink(&outside_chunk, tmp.path().join("shards/segment-a")).unwrap();

        let mut manifest = EvidenceBundleManifest::new(
            "symlink-escape",
            EvidenceBundleKind::LexicalGeneration,
            1_700_000_000_007,
        );
        manifest.chunks = vec![EvidenceBundleChunk {
            path: "shards/segment-a".to_string(),
            role: EvidenceBundleChunkRole::LexicalShard,
            size_bytes: b"outside shard bytes".len() as u64,
            blake3: blake3::hash(b"outside shard bytes").to_hex().to_string(),
            required: true,
            parity_group: None,
        }];

        let report = manifest.verify(tmp.path());

        assert!(report.is_unsafe(), "{report:?}");
        assert_eq!(report.verified_chunk_count, 0);
        assert_eq!(
            report.issues[0].kind,
            EvidenceBundleIssueKind::UnsafeChunkPath
        );
    }
}
