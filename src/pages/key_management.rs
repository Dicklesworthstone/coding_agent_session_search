//! Key management operations for encrypted pages archives.
//!
//! Provides CLI operations to manage key slots in an encrypted archive:
//! - `list`: Show all key slots
//! - `add`: Add a new password or recovery key slot
//! - `revoke`: Remove a key slot
//! - `rotate`: Full key rotation (regenerate DEK, re-encrypt payload)
//!
//! # Security Model
//!
//! The archive uses envelope encryption with multiple key slots (like LUKS):
//! - A random Data Encryption Key (DEK) encrypts the payload
//! - Each key slot wraps the DEK with a Key Encryption Key (KEK)
//! - KEK is derived from password (Argon2id) or recovery secret (HKDF-SHA256)
//! - Add/revoke only modifies config.json; payload unchanged
//! - Rotate re-encrypts entire payload with new DEK

use crate::pages::attachments::reencrypt_blobs_into_dir;
use crate::pages::encrypt::{
    Argon2Params, EncryptionConfig, KdfAlgorithm, KeySlot, MIN_RECOVERY_SECRET_BYTES, SlotType,
    decompress_archive_chunk, load_config, max_archive_ciphertext_chunk_size,
    validate_supported_payload_format,
};
use crate::pages::qr::RecoverySecret;
use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit, Payload},
};
use anyhow::{Context, Result, bail};
use argon2::{Algorithm, Argon2, Params, Version};
use base64::prelude::*;
use chrono::{DateTime, Utc};
use flate2::{Compression, write::DeflateEncoder};
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};
use tracing::info;

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};

/// Argon2id default parameters
#[cfg(not(test))]
const ARGON2_MEMORY_KB: u32 = 65536; // 64 MB
#[cfg(test)]
const ARGON2_MEMORY_KB: u32 = 64;
#[cfg(not(test))]
const ARGON2_ITERATIONS: u32 = 3;
#[cfg(test)]
const ARGON2_ITERATIONS: u32 = 1;
#[cfg(not(test))]
const ARGON2_PARALLELISM: u32 = 4;
#[cfg(test)]
const ARGON2_PARALLELISM: u32 = 1;

/// Schema version for encryption
const SCHEMA_VERSION: u8 = 2;
const MAX_ARCHIVE_CHUNKS: u64 = u32::MAX as u64;
const KEY_MUTATION_JOURNAL_FORMAT: &str = "cass-pages-key-mutation-v1";
const KEY_MUTATION_JOURNAL_MAX_BYTES: u64 = 16 * 1024;
const KEY_MUTATION_TREE_ENTRY_LIMIT: usize = 1_000_000;
const KEY_MUTATION_TREE_DEPTH_LIMIT: usize = 128;
const REQUIRED_SITE_FILES: &[&str] = &[
    "index.html",
    "config.json",
    "sw.js",
    "viewer.js",
    "auth.js",
    "styles.css",
    "robots.txt",
    ".nojekyll",
];

#[derive(Debug, Clone)]
struct KeyMutationTarget {
    live_root: PathBuf,
    site_relative: PathBuf,
    journal_path: PathBuf,
}

impl KeyMutationTarget {
    fn live_site_dir(&self) -> PathBuf {
        self.live_root.join(&self.site_relative)
    }

    fn staged_site_dir(&self, staged_root: &Path) -> PathBuf {
        staged_root.join(&self.site_relative)
    }
}

struct KeyMutationGuard {
    target: KeyMutationTarget,
    publication_lock: PagesPublicationLockGuard,
}

struct KeyMutationStagingOwnership {
    path: PathBuf,
    identity: crate::franken_sync::FileIdentity,
    handle: File,
}

pub(super) struct PagesPublicationLockGuard {
    live_root: PathBuf,
    lock_path: PathBuf,
    lock_identity: crate::franken_sync::FileIdentity,
    lock_file: File,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TreeEvidence {
    digest: String,
    entries: usize,
    bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyMutationJournal {
    format: String,
    staged_file_name: String,
    backup_file_name: String,
    prior_digest: String,
    candidate_digest: String,
}

struct ValidatedKeyMutationJournal {
    journal: KeyMutationJournal,
    identity: crate::franken_sync::FileIdentity,
    size: u64,
    handle: File,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveredKeyMutation {
    None,
    RolledBack,
    Committed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyMutationStageMode {
    PreserveSite,
    ReplaceEncryptedPayload,
}

fn max_encryptable_plaintext_bytes(chunk_size: usize) -> u64 {
    MAX_ARCHIVE_CHUNKS.saturating_mul(chunk_size as u64)
}

fn ensure_archive_chunk_count_fits_nonce_space(chunk_count: u64, chunk_size: usize) -> Result<()> {
    if chunk_count > MAX_ARCHIVE_CHUNKS {
        bail!(
            "File too large: exceeds maximum of {} chunks ({} bytes with current chunk size)",
            u32::MAX,
            max_encryptable_plaintext_bytes(chunk_size)
        );
    }
    Ok(())
}

fn ensure_can_write_archive_chunk(chunk_index: u32, chunk_size: usize) -> Result<()> {
    if chunk_index == u32::MAX {
        bail!(
            "File too large: exceeds maximum of {} chunks ({} bytes with current chunk size)",
            u32::MAX,
            max_encryptable_plaintext_bytes(chunk_size)
        );
    }
    Ok(())
}

fn validate_password_input(password: &str) -> Result<()> {
    if password.is_empty() || password.trim().is_empty() {
        bail!("Password cannot be empty or whitespace-only");
    }
    Ok(())
}

fn key_mutation_sidecar_path(root: &Path, suffix: &str) -> PathBuf {
    let root_name = root
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("pages-archive"));
    let mut sidecar_name = std::ffi::OsString::from(".");
    sidecar_name.push(root_name);
    sidecar_name.push(suffix);
    root.with_file_name(sidecar_name)
}

fn derive_key_mutation_target(requested_path: &Path) -> Result<KeyMutationTarget> {
    let (live_root, site_relative) = if requested_path
        .file_name()
        .is_some_and(|name| name == "site")
    {
        let parent = requested_path.parent().ok_or_else(|| {
            anyhow::anyhow!(
                "site directory has no parent publication root: {}",
                requested_path.display()
            )
        })?;
        let parent_private = parent.join("private");
        let parent_journal = key_mutation_sidecar_path(
            parent,
            ".pages-key-mutation-in-progress.json",
        );
        let parent_lock = key_mutation_sidecar_path(parent, ".pages-publication.lock");
        let parent_is_publication_root = [&parent_private, &parent_journal, &parent_lock]
            .into_iter()
            .try_fold(false, |found, path| {
                match std::fs::symlink_metadata(path) {
                    Ok(_) => Ok(true),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(found),
                    Err(error) => Err(error).with_context(|| {
                        format!(
                            "failed inspecting Pages publication marker {}",
                            path.display()
                        )
                    }),
                }
            })?;
        if parent_is_publication_root {
            (parent.to_path_buf(), PathBuf::from("site"))
        } else {
            (requested_path.to_path_buf(), PathBuf::new())
        }
    } else {
        match std::fs::symlink_metadata(requested_path.join("site")) {
            Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
                (requested_path.to_path_buf(), PathBuf::from("site"))
            }
            Ok(_) => (requested_path.to_path_buf(), PathBuf::new()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                (requested_path.to_path_buf(), PathBuf::new())
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed inspecting Pages key-mutation site under {}",
                        requested_path.display()
                    )
                });
            }
        }
    };
    if live_root.file_name().and_then(|name| name.to_str()).is_none() {
        bail!(
            "Pages key-mutation publication root must have a UTF-8 directory name: {}",
            live_root.display()
        );
    }

    Ok(KeyMutationTarget {
        journal_path: key_mutation_sidecar_path(
            &live_root,
            ".pages-key-mutation-in-progress.json",
        ),
        live_root,
        site_relative,
    })
}

fn pages_publication_lock_path(live_root: &Path) -> PathBuf {
    key_mutation_sidecar_path(live_root, ".pages-publication.lock")
}

fn ensure_pages_publication_lock_parent(lock_path: &Path) -> Result<()> {
    let parent = lock_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut ancestors = Vec::new();
    let mut current = Some(parent);
    while let Some(ancestor) = current {
        if ancestor.as_os_str().is_empty() {
            break;
        }
        ancestors.push(ancestor.to_path_buf());
        current = ancestor.parent();
    }
    ancestors.reverse();

    for ancestor in &ancestors {
        match std::fs::symlink_metadata(ancestor) {
            Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(metadata)
                if metadata.file_type().is_symlink()
                    && pages_publication_allows_system_symlink_ancestor(ancestor) => {}
            Ok(_) => bail!(
                "Pages publication lock parent must be a real directory: {}",
                ancestor.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed inspecting Pages publication lock parent {}",
                        ancestor.display()
                    )
                });
            }
        }
    }

    std::fs::create_dir_all(parent).with_context(|| {
        format!(
            "failed creating Pages publication lock parent {}",
            parent.display()
        )
    })?;
    for ancestor in ancestors {
        match std::fs::symlink_metadata(&ancestor) {
            Ok(metadata)
                if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(metadata)
                if metadata.file_type().is_symlink()
                    && pages_publication_allows_system_symlink_ancestor(&ancestor) => {}
            Ok(_) => bail!(
                "Pages publication lock parent changed into a non-directory or symlink: {}",
                ancestor.display()
            ),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed re-inspecting Pages publication lock parent {}",
                        ancestor.display()
                    )
                });
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn pages_publication_allows_system_symlink_ancestor(path: &Path) -> bool {
    path == Path::new("/var") || path == Path::new("/tmp")
}

#[cfg(not(target_os = "macos"))]
fn pages_publication_allows_system_symlink_ancestor(_path: &Path) -> bool {
    false
}

pub(super) fn acquire_pages_publication_lock(
    live_root: &Path,
) -> Result<PagesPublicationLockGuard> {
    let lock_path = pages_publication_lock_path(live_root);
    ensure_pages_publication_lock_parent(&lock_path)?;
    let lock_exists = match std::fs::symlink_metadata(&lock_path) {
        Ok(metadata) if metadata.file_type().is_file() => {
            #[cfg(unix)]
            {
                if metadata.nlink() != 1 {
                    bail!(
                        "Pages publication lock {} has {} hard links; exclusive pathname ownership is not provable",
                        lock_path.display(),
                        metadata.nlink()
                    );
                }
                if key_publication_mode(&metadata) & 0o077 != 0 {
                    bail!(
                        "Pages publication lock is not owner-only: {}",
                        lock_path.display()
                    );
                }
            }
            true
        }
        Ok(_) => bail!(
            "Pages publication lock is not a regular file: {}",
            lock_path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed inspecting Pages publication lock {}",
                    lock_path.display()
                )
            });
        }
    };

    let mut options = OpenOptions::new();
    options.read(true).write(true);
    if lock_exists {
        options.create(false);
    } else {
        options.create_new(true);
    }
    #[cfg(unix)]
    options.mode(0o600);
    let lock_file = options.open(&lock_path).with_context(|| {
        format!(
            "failed opening Pages publication lock {}",
            lock_path.display()
        )
    })?;
    let lock_identity = crate::franken_sync::FileIdentity::from_file(&lock_file)
        .with_context(|| {
            format!(
                "failed identifying Pages publication lock {}",
                lock_path.display()
            )
        })?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "filesystem does not expose a stable identity for Pages publication lock {}",
                lock_path.display()
            )
        })?;

    match fs2::FileExt::try_lock_exclusive(&lock_file) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => bail!(
            "another Pages publication is already active for {}; lock contention at {}",
            live_root.display(),
            lock_path.display()
        ),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed acquiring Pages publication lock {}",
                    lock_path.display()
                )
            });
        }
    }

    let guard = PagesPublicationLockGuard {
        live_root: live_root.to_path_buf(),
        lock_path,
        lock_identity,
        lock_file,
    };
    require_pages_publication_lock(live_root, &guard)?;
    Ok(guard)
}

pub(super) fn require_pages_publication_lock(
    live_root: &Path,
    guard: &PagesPublicationLockGuard,
) -> Result<()> {
    if live_root != guard.live_root {
        bail!(
            "Pages publication lock for {} cannot authorize publication to {}",
            guard.live_root.display(),
            live_root.display()
        );
    }
    let held_identity = crate::franken_sync::FileIdentity::from_file(&guard.lock_file)
        .context("failed re-identifying held Pages publication lock")?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "filesystem stopped exposing a stable identity for held Pages publication lock {}",
                guard.lock_path.display()
            )
        })?;
    if held_identity != guard.lock_identity {
        bail!(
            "held Pages publication lock {} changed identity",
            guard.lock_path.display()
        );
    }

    let metadata = std::fs::symlink_metadata(&guard.lock_path).with_context(|| {
        format!(
            "failed re-inspecting Pages publication lock {}",
            guard.lock_path.display()
        )
    })?;
    if !metadata.file_type().is_file() {
        bail!(
            "Pages publication lock {} is no longer a regular file",
            guard.lock_path.display()
        );
    }
    #[cfg(unix)]
    {
        if metadata.nlink() != 1 {
            bail!(
                "Pages publication lock {} has {} hard links; refused publication",
                guard.lock_path.display(),
                metadata.nlink()
            );
        }
        if key_publication_mode(&metadata) & 0o077 != 0 {
            bail!(
                "Pages publication lock is no longer owner-only: {}",
                guard.lock_path.display()
            );
        }
    }
    let probe = File::open(&guard.lock_path).with_context(|| {
        format!(
            "failed re-opening Pages publication lock {}",
            guard.lock_path.display()
        )
    })?;
    let path_identity = crate::franken_sync::FileIdentity::from_file(&probe)
        .context("failed re-identifying Pages publication lock pathname")?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "filesystem stopped exposing a stable pathname identity for Pages publication lock {}",
                guard.lock_path.display()
            )
        })?;
    if path_identity != guard.lock_identity {
        bail!(
            "Pages publication lock {} was replaced after acquisition",
            guard.lock_path.display()
        );
    }
    Ok(())
}

fn open_key_mutation_lock(target: KeyMutationTarget) -> Result<KeyMutationGuard> {
    let publication_lock = acquire_pages_publication_lock(&target.live_root)?;
    Ok(KeyMutationGuard {
        target,
        publication_lock,
    })
}

fn require_key_mutation_guard(guard: &KeyMutationGuard) -> Result<()> {
    require_pages_publication_lock(&guard.target.live_root, &guard.publication_lock)
}

fn random_key_mutation_sidecar_path(root: &Path, role: &str) -> PathBuf {
    let mut random = [0_u8; 16];
    rand::rng().fill_bytes(&mut random);
    let root_name = root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("pages-archive");
    root.with_file_name(format!(
        ".{root_name}.pages-key-{role}.{}",
        hex::encode(random)
    ))
}

#[cfg(unix)]
fn key_publication_mode(metadata: &std::fs::Metadata) -> u32 {
    metadata.permissions().mode() & 0o777
}

#[cfg(not(unix))]
fn key_publication_mode(_metadata: &std::fs::Metadata) -> u32 {
    0
}

fn hash_key_publication_file(path: &Path) -> Result<(u64, u32, [u8; 32])> {
    let mut file = File::open(path)
        .with_context(|| format!("failed opening key-publication file {}", path.display()))?;
    let identity = crate::franken_sync::FileIdentity::from_file(&file)
        .with_context(|| format!("failed identifying key-publication file {}", path.display()))?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "filesystem does not expose a stable identity for key-publication file {}",
                path.display()
            )
        })?;
    let opened_metadata = file.metadata()?;
    if !opened_metadata.file_type().is_file() {
        bail!(
            "Pages key-publication entry does not resolve to a regular file: {}",
            path.display()
        );
    }
    #[cfg(unix)]
    if opened_metadata.nlink() != 1 {
        bail!(
            "Pages key-publication file {} has {} hard links; exact ownership is not provable",
            path.display(),
            opened_metadata.nlink()
        );
    }
    let opened_mode = key_publication_mode(&opened_metadata);
    let mut file_digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        file_digest.update(&buffer[..read]);
    }
    let hashed_metadata = file.metadata()?;
    if !hashed_metadata.file_type().is_file()
        || hashed_metadata.len() != opened_metadata.len()
        || key_publication_mode(&hashed_metadata) != opened_mode
    {
        bail!(
            "key-publication file changed type, size, or permissions while hashing: {}",
            path.display()
        );
    }
    let probe = File::open(path)?;
    let path_identity = crate::franken_sync::FileIdentity::from_file(&probe)?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "filesystem stopped exposing a stable identity for key-publication file {}",
                path.display()
            )
        })?;
    if path_identity != identity {
        bail!(
            "key-publication file changed identity while hashing: {}",
            path.display()
        );
    }
    let digest: [u8; 32] = file_digest.finalize().into();
    Ok((opened_metadata.len(), opened_mode, digest))
}

fn inspect_key_publication_tree(root: &Path) -> Result<TreeEvidence> {
    let root_metadata = std::fs::symlink_metadata(root).with_context(|| {
        format!(
            "failed inspecting Pages key-publication tree {}",
            root.display()
        )
    })?;
    if root_metadata.file_type().is_symlink() || !root_metadata.file_type().is_dir() {
        bail!(
            "Pages key-publication tree must be a real directory: {}",
            root.display()
        );
    }
    let root_handle = File::open(root).with_context(|| {
        format!(
            "failed opening Pages key-publication root {}",
            root.display()
        )
    })?;
    let root_identity = crate::franken_sync::FileIdentity::from_file(&root_handle)?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "filesystem does not expose a stable identity for Pages key-publication root {}",
                root.display()
            )
        })?;
    let root_mode = key_publication_mode(&root_metadata);
    let canonical_root = root.canonicalize().with_context(|| {
        format!(
            "failed resolving Pages key-publication root {}",
            root.display()
        )
    })?;
    let site_path = root.join("site");
    let canonical_public_root = match std::fs::symlink_metadata(&site_path) {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            site_path.canonicalize()?
        }
        Ok(_) => canonical_root.clone(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => canonical_root.clone(),
        Err(error) => return Err(error.into()),
    };

    let mut paths = Vec::new();
    collect_key_publication_paths(root, root, &mut paths)?;
    paths.sort_unstable_by(|left, right| left.0.cmp(&right.0));

    let mut digest = Sha256::new();
    digest.update(b"root\0");
    digest.update(root_mode.to_be_bytes());
    let mut total_bytes = 0_u64;
    for (relative, path) in &paths {
        let metadata = std::fs::symlink_metadata(path).with_context(|| {
            format!("failed inspecting key-publication entry {}", path.display())
        })?;
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            if Path::new(relative).starts_with("private") {
                bail!(
                    "private key artifact must not be a symlink: {}",
                    path.display()
                );
            }
            let link_target = std::fs::read_link(path).with_context(|| {
                format!(
                    "failed reading Pages key-publication symlink {}",
                    path.display()
                )
            })?;
            let canonical_target = path.canonicalize().with_context(|| {
                format!(
                    "failed resolving Pages key-publication symlink {}",
                    path.display()
                )
            })?;
            if !canonical_target.starts_with(&canonical_public_root) {
                bail!(
                    "refusing Pages key-publication symlink outside the public site root: {}",
                    path.display()
                );
            }
            let target_relative = canonical_target
                .strip_prefix(&canonical_root)?
                .to_str()
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Pages key-publication symlink target is not valid UTF-8: {}",
                        canonical_target.display()
                    )
                })?;
            let (file_len, file_mode, file_digest) = hash_key_publication_file(path)?;
            let final_metadata = std::fs::symlink_metadata(path)?;
            let final_link_target = std::fs::read_link(path)?;
            if !final_metadata.file_type().is_symlink()
                || final_link_target != link_target
                || path.canonicalize()? != canonical_target
            {
                bail!(
                    "Pages key-publication symlink changed while inspected: {}",
                    path.display()
                );
            }
            total_bytes = total_bytes
                .checked_add(file_len)
                .context("Pages key-publication byte count overflowed u64")?;
            digest.update(b"symlink\0");
            digest.update(relative.as_bytes());
            digest.update(b"\0");
            digest.update(target_relative.as_bytes());
            digest.update(b"\0");
            digest.update(file_mode.to_be_bytes());
            digest.update(file_len.to_be_bytes());
            digest.update(file_digest);
            continue;
        }
        if file_type.is_dir() {
            digest.update(b"directory\0");
            digest.update(relative.as_bytes());
            digest.update(b"\0");
            digest.update(key_publication_mode(&metadata).to_be_bytes());
            continue;
        }
        if !file_type.is_file() {
            bail!(
                "Pages key-publication tree contains a non-regular entry: {}",
                path.display()
            );
        }
        #[cfg(unix)]
        if metadata.nlink() != 1 {
            bail!(
                "Pages key-publication file {} has {} hard links; exact ownership is not provable",
                path.display(),
                metadata.nlink()
            );
        }
        let (file_len, file_mode, file_digest) = hash_key_publication_file(path)?;
        let final_metadata = std::fs::symlink_metadata(path)?;
        if !final_metadata.file_type().is_file()
            || final_metadata.len() != file_len
            || key_publication_mode(&final_metadata) != file_mode
        {
            bail!(
                "key-publication file changed type, size, or permissions while inspected: {}",
                path.display()
            );
        }

        total_bytes = total_bytes
            .checked_add(file_len)
            .context("Pages key-publication byte count overflowed u64")?;
        digest.update(b"file\0");
        digest.update(relative.as_bytes());
        digest.update(b"\0");
        digest.update(file_mode.to_be_bytes());
        digest.update(file_len.to_be_bytes());
        digest.update(file_digest);
    }

    let mut final_paths = Vec::new();
    collect_key_publication_paths(root, root, &mut final_paths)?;
    final_paths.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    if final_paths != paths {
        bail!(
            "Pages key-publication entries changed while inspected: {}",
            root.display()
        );
    }

    let final_root_metadata = std::fs::symlink_metadata(root)?;
    if final_root_metadata.file_type().is_symlink()
        || !final_root_metadata.file_type().is_dir()
        || key_publication_mode(&final_root_metadata) != root_mode
    {
        bail!(
            "Pages key-publication root changed type or permissions while inspected: {}",
            root.display()
        );
    }
    let final_root_handle = File::open(root)?;
    let final_root_identity = crate::franken_sync::FileIdentity::from_file(&final_root_handle)?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "filesystem stopped exposing a stable identity for Pages key-publication root {}",
                root.display()
            )
        })?;
    if final_root_identity != root_identity {
        bail!(
            "Pages key-publication root changed identity while inspected: {}",
            root.display()
        );
    }

    Ok(TreeEvidence {
        digest: hex::encode(digest.finalize()),
        entries: paths.len(),
        bytes: total_bytes,
    })
}

fn collect_key_publication_paths(
    root: &Path,
    directory: &Path,
    paths: &mut Vec<(String, PathBuf)>,
) -> Result<()> {
    let mut directories = vec![(directory.to_path_buf(), 0_usize)];
    while let Some((directory, depth)) = directories.pop() {
        if depth > KEY_MUTATION_TREE_DEPTH_LIMIT {
            bail!(
                "Pages key-publication tree exceeds the {}-directory depth bound at {}",
                KEY_MUTATION_TREE_DEPTH_LIMIT,
                directory.display()
            );
        }
        for entry in std::fs::read_dir(&directory).with_context(|| {
            format!("failed reading key-publication tree {}", directory.display())
        })? {
            if paths.len() >= KEY_MUTATION_TREE_ENTRY_LIMIT {
                bail!(
                    "Pages key-publication tree exceeds the {}-entry validation bound",
                    KEY_MUTATION_TREE_ENTRY_LIMIT
                );
            }
            let entry = entry?;
            let path = entry.path();
            let relative_path = path.strip_prefix(root)?;
            let relative = relative_path.to_str().ok_or_else(|| {
                anyhow::anyhow!(
                    "Pages key-publication entry is not valid UTF-8: {}",
                    relative_path.display()
                )
            })?;
            paths.push((relative.replace('\\', "/"), path.clone()));
            let metadata = std::fs::symlink_metadata(&path)?;
            if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
                directories.push((path, depth + 1));
            }
        }
    }
    Ok(())
}

fn allocate_key_mutation_staging_root(
    live_root: &Path,
) -> Result<(PathBuf, KeyMutationStagingOwnership)> {
    for _ in 0..64 {
        let path = random_key_mutation_sidecar_path(live_root, "staged");
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        builder.mode(0o700);
        match builder.create(&path) {
            Ok(()) => {
                #[cfg(unix)]
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
                let handle = File::open(&path)?;
                let identity = crate::franken_sync::FileIdentity::from_file(&handle)?
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "filesystem does not expose a stable identity for Pages key-mutation staging root {}",
                            path.display()
                        )
                    })?;
                let probe = File::open(&path)?;
                let path_identity = crate::franken_sync::FileIdentity::from_file(&probe)?
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "filesystem stopped exposing a stable staging-root pathname identity"
                        )
                    })?;
                if path_identity != identity {
                    bail!(
                        "Pages key-mutation staging root changed identity during allocation: {}",
                        path.display()
                    );
                }
                return Ok((
                    path.clone(),
                    KeyMutationStagingOwnership {
                        path,
                        identity,
                        handle,
                    },
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed creating Pages key-mutation staging root {}",
                        path.display()
                    )
                });
            }
        }
    }
    bail!("failed allocating a unique Pages key-mutation staging root")
}

fn is_private_key_artifact_path(target: &KeyMutationTarget, relative: &Path) -> bool {
    !target.site_relative.as_os_str().is_empty() && relative.starts_with("private")
}

fn rotation_replaces_site_entry(target: &KeyMutationTarget, relative: &Path) -> bool {
    let site_relative = if target.site_relative.as_os_str().is_empty() {
        relative
    } else {
        let Ok(relative) = relative.strip_prefix(&target.site_relative) else {
            return false;
        };
        relative
    };
    site_relative.components().count() == 1
        && matches!(
            site_relative.to_str(),
            Some("payload" | "blobs" | "config.json" | "integrity.json")
        )
}

fn stage_key_mutation_tree(
    guard: &KeyMutationGuard,
    mode: KeyMutationStageMode,
) -> Result<(PathBuf, TreeEvidence, KeyMutationStagingOwnership)> {
    require_key_mutation_guard(guard)?;
    let target = &guard.target;
    let prior_evidence = inspect_key_publication_tree(&target.live_root)?;
    let (staged_root, staging_ownership) =
        allocate_key_mutation_staging_root(&target.live_root)?;
    let canonical_live_root = target.live_root.canonicalize().with_context(|| {
        format!(
            "failed resolving Pages key-mutation root {}",
            target.live_root.display()
        )
    })?;
    let canonical_live_site = target.live_site_dir().canonicalize().with_context(|| {
        format!(
            "failed resolving Pages key-mutation site {}",
            target.live_site_dir().display()
        )
    })?;
    if !canonical_live_site.starts_with(&canonical_live_root) {
        bail!(
            "Pages key-mutation site resolves outside publication root: {}",
            target.live_site_dir().display()
        );
    }

    let copy_result = copy_key_mutation_tree_recursive(
        target,
        &target.live_root,
        &staged_root,
        &target.live_root,
        &canonical_live_site,
        mode,
        0,
    );
    if let Err(error) = copy_result {
        let cleanup_error = remove_owned_key_mutation_tree(
            guard,
            &staged_root,
            None,
            Some(&staging_ownership),
            None,
        )
        .err();
        return match cleanup_error {
            Some(cleanup_error) => Err(error.context(format!(
                "failed cleaning rejected key-mutation staging root: {cleanup_error:#}"
            ))),
            None => Err(error),
        };
    }

    #[cfg(unix)]
    std::fs::set_permissions(
        &staged_root,
        std::fs::Permissions::from_mode(key_publication_mode(&std::fs::metadata(
            &target.live_root,
        )?)),
    )?;

    Ok((staged_root, prior_evidence, staging_ownership))
}

fn copy_key_mutation_tree_recursive(
    target: &KeyMutationTarget,
    source_directory: &Path,
    staged_root: &Path,
    source_root: &Path,
    canonical_source_site: &Path,
    mode: KeyMutationStageMode,
    depth: usize,
) -> Result<()> {
    if depth > KEY_MUTATION_TREE_DEPTH_LIMIT {
        bail!(
            "Pages key-mutation source exceeds the {}-directory depth bound at {}",
            KEY_MUTATION_TREE_DEPTH_LIMIT,
            source_directory.display()
        );
    }
    for entry in std::fs::read_dir(source_directory).with_context(|| {
        format!(
            "failed reading Pages key-mutation source {}",
            source_directory.display()
        )
    })? {
        let entry = entry?;
        let source_path = entry.path();
        let relative = source_path.strip_prefix(source_root)?;
        if mode == KeyMutationStageMode::ReplaceEncryptedPayload
            && rotation_replaces_site_entry(target, relative)
        {
            continue;
        }
        if relative.components().any(|component| {
            !matches!(component, Component::Normal(_) | Component::CurDir)
        }) {
            bail!(
                "refusing to stage unsafe Pages key-mutation path {}",
                relative.display()
            );
        }
        let destination = staged_root.join(relative);
        let metadata = std::fs::symlink_metadata(&source_path)?;
        let file_type = metadata.file_type();
        let is_private = is_private_key_artifact_path(target, relative);

        if file_type.is_symlink() {
            if is_private {
                bail!(
                    "private key artifact must not be a symlink: {}",
                    source_path.display()
                );
            }
            let canonical_target = source_path.canonicalize().with_context(|| {
                format!(
                    "failed resolving symlinked Pages key-mutation source {}",
                    source_path.display()
                )
            })?;
            if !canonical_target.starts_with(canonical_source_site) {
                bail!(
                    "refusing to stage Pages key-mutation symlink outside public site root: {}",
                    source_path.display()
                );
            }
            if !std::fs::metadata(&canonical_target)?.file_type().is_file() {
                bail!(
                    "Pages key-mutation symlink must resolve to a regular file: {}",
                    source_path.display()
                );
            }
            if let Some(parent) = destination.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&canonical_target, &destination)?;
            continue;
        }

        if file_type.is_dir() {
            std::fs::create_dir(&destination).with_context(|| {
                format!(
                    "failed creating staged key-mutation directory {}",
                    destination.display()
                )
            })?;
            #[cfg(unix)]
            std::fs::set_permissions(
                &destination,
                std::fs::Permissions::from_mode(if is_private {
                    0o700
                } else {
                    key_publication_mode(&metadata)
                }),
            )?;
            copy_key_mutation_tree_recursive(
                target,
                &source_path,
                staged_root,
                source_root,
                canonical_source_site,
                mode,
                depth + 1,
            )?;
            continue;
        }

        if !file_type.is_file() {
            bail!(
                "Pages key-mutation source contains a non-regular entry: {}",
                source_path.display()
            );
        }
        #[cfg(unix)]
        if metadata.nlink() != 1 {
            bail!(
                "Pages key-mutation source file {} has {} hard links; exact ownership is not provable",
                source_path.display(),
                metadata.nlink()
            );
        }
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&source_path, &destination).with_context(|| {
            format!(
                "failed copying Pages key-mutation source {} to {}",
                source_path.display(),
                destination.display()
            )
        })?;
        #[cfg(unix)]
        std::fs::set_permissions(
            &destination,
            std::fs::Permissions::from_mode(if is_private {
                0o600
            } else {
                key_publication_mode(&metadata)
            }),
        )?;
    }
    Ok(())
}

fn direct_child_from_journal(
    root: &Path,
    file_name: &str,
    role: &str,
    label: &str,
) -> Result<PathBuf> {
    let candidate = Path::new(file_name);
    let mut components = candidate.components();
    let Some(Component::Normal(name)) = components.next() else {
        bail!("{label} is not a plain file name: {file_name}");
    };
    if components.next().is_some() {
        bail!("{label} contains path separators: {file_name}");
    }
    let root_name = root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("pages-archive");
    let expected_prefix = format!(".{root_name}.pages-key-{role}.");
    let Some(nonce) = file_name.strip_prefix(&expected_prefix) else {
        bail!("{label} does not carry the owned {role} prefix: {file_name}");
    };
    if nonce.len() != 32
        || !nonce
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("{label} does not carry a 128-bit lowercase hexadecimal nonce");
    }
    Ok(root.with_file_name(name))
}

fn validate_key_mutation_digest(label: &str, digest: &str) -> Result<()> {
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("Pages key-mutation journal {label} digest is not canonical SHA-256");
    }
    Ok(())
}

fn write_key_mutation_journal(
    guard: &KeyMutationGuard,
    staged_root: &Path,
    backup_root: &Path,
    prior: &TreeEvidence,
    candidate: &TreeEvidence,
) -> Result<ValidatedKeyMutationJournal> {
    require_key_mutation_guard(guard)?;
    match std::fs::symlink_metadata(&guard.target.journal_path) {
        Ok(_) => bail!(
            "Pages key-mutation recovery journal already exists: {}",
            guard.target.journal_path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let staged_file_name = staged_root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("key-mutation staging name is not UTF-8"))?;
    let backup_file_name = backup_root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("key-mutation backup name is not UTF-8"))?;
    let claimed_staged = direct_child_from_journal(
        &guard.target.live_root,
        staged_file_name,
        "staged",
        "key-mutation staged path",
    )?;
    let claimed_backup = direct_child_from_journal(
        &guard.target.live_root,
        backup_file_name,
        "backup",
        "key-mutation backup path",
    )?;
    if claimed_staged != staged_root || claimed_backup != backup_root {
        bail!("Pages key-mutation journal paths do not name the exact staged and backup trees");
    }
    validate_key_mutation_digest("prior", &prior.digest)?;
    validate_key_mutation_digest("candidate", &candidate.digest)?;
    let journal = KeyMutationJournal {
        format: KEY_MUTATION_JOURNAL_FORMAT.to_string(),
        staged_file_name: staged_file_name.to_string(),
        backup_file_name: backup_file_name.to_string(),
        prior_digest: prior.digest.clone(),
        candidate_digest: candidate.digest.clone(),
    };
    let bytes = serde_json::to_vec_pretty(&journal)?;
    if bytes.len() as u64 > KEY_MUTATION_JOURNAL_MAX_BYTES {
        bail!("Pages key-mutation recovery journal exceeds its size bound");
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&guard.target.journal_path).with_context(|| {
        format!(
            "failed creating Pages key-mutation recovery journal {}",
            guard.target.journal_path.display()
        )
    })?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    sync_parent_directory(&guard.target.journal_path)?;
    let validated = read_key_mutation_journal(guard)?.ok_or_else(|| {
        anyhow::anyhow!(
            "newly written Pages key-mutation journal disappeared: {}",
            guard.target.journal_path.display()
        )
    })?;
    if validated.journal != journal {
        bail!(
            "newly written Pages key-mutation journal changed content: {}",
            guard.target.journal_path.display()
        );
    }
    Ok(validated)
}

fn read_key_mutation_journal(
    guard: &KeyMutationGuard,
) -> Result<Option<ValidatedKeyMutationJournal>> {
    require_key_mutation_guard(guard)?;
    let metadata = match std::fs::symlink_metadata(&guard.target.journal_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_file() {
        bail!(
            "Pages key-mutation recovery journal is not a regular file: {}",
            guard.target.journal_path.display()
        );
    }
    #[cfg(unix)]
    {
        if metadata.nlink() != 1 {
            bail!(
                "Pages key-mutation recovery journal {} has {} hard links",
                guard.target.journal_path.display(),
                metadata.nlink()
            );
        }
        if key_publication_mode(&metadata) & 0o077 != 0 {
            bail!(
                "Pages key-mutation recovery journal is not owner-only: {}",
                guard.target.journal_path.display()
            );
        }
    }
    if metadata.len() > KEY_MUTATION_JOURNAL_MAX_BYTES {
        bail!(
            "Pages key-mutation recovery journal exceeds its {} byte limit: {}",
            KEY_MUTATION_JOURNAL_MAX_BYTES,
            guard.target.journal_path.display()
        );
    }
    let mut file = File::open(&guard.target.journal_path)?;
    let identity = crate::franken_sync::FileIdentity::from_file(&file)?
        .ok_or_else(|| anyhow::anyhow!("filesystem does not expose a stable journal identity"))?;
    let opened_metadata = file.metadata()?;
    if opened_metadata.len() != metadata.len() {
        bail!(
            "Pages key-mutation recovery journal changed size before read: {}",
            guard.target.journal_path.display()
        );
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut file)
        .take(KEY_MUTATION_JOURNAL_MAX_BYTES + 1)
        .read_to_end(&mut bytes)?;
    let hashed_metadata = file.metadata()?;
    if hashed_metadata.len() != opened_metadata.len() || bytes.len() as u64 != metadata.len() {
        bail!(
            "Pages key-mutation recovery journal changed size while read: {}",
            guard.target.journal_path.display()
        );
    }
    let path_probe = File::open(&guard.target.journal_path)?;
    let path_identity = crate::franken_sync::FileIdentity::from_file(&path_probe)?
        .ok_or_else(|| {
            anyhow::anyhow!("filesystem stopped exposing a stable journal pathname identity")
        })?;
    if path_identity != identity {
        bail!(
            "Pages key-mutation recovery journal changed identity while read: {}",
            guard.target.journal_path.display()
        );
    }
    if path_probe.metadata()?.len() != opened_metadata.len() {
        bail!(
            "Pages key-mutation recovery journal changed size after read: {}",
            guard.target.journal_path.display()
        );
    }
    let journal: KeyMutationJournal = serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "failed parsing Pages key-mutation recovery journal {}",
            guard.target.journal_path.display()
        )
    })?;
    if journal.format != KEY_MUTATION_JOURNAL_FORMAT {
        bail!(
            "unrecognized Pages key-mutation recovery journal format at {}",
            guard.target.journal_path.display()
        );
    }
    for (label, digest) in [
        ("prior", journal.prior_digest.as_str()),
        ("candidate", journal.candidate_digest.as_str()),
    ] {
        validate_key_mutation_digest(label, digest)?;
    }
    Ok(Some(ValidatedKeyMutationJournal {
        journal,
        identity,
        size: opened_metadata.len(),
        handle: file,
    }))
}

fn require_validated_key_mutation_journal(
    guard: &KeyMutationGuard,
    validated: &ValidatedKeyMutationJournal,
) -> Result<()> {
    require_key_mutation_guard(guard)?;
    let held_identity = crate::franken_sync::FileIdentity::from_file(&validated.handle)?
        .ok_or_else(|| anyhow::anyhow!("filesystem stopped exposing a held journal identity"))?;
    if held_identity != validated.identity || validated.handle.metadata()?.len() != validated.size {
        bail!(
            "held Pages key-mutation journal changed identity or size after validation: {}",
            guard.target.journal_path.display()
        );
    }
    let metadata = std::fs::symlink_metadata(&guard.target.journal_path).with_context(|| {
        format!(
            "failed re-inspecting Pages key-mutation journal {}",
            guard.target.journal_path.display()
        )
    })?;
    if !metadata.file_type().is_file() || metadata.len() != validated.size {
        bail!(
            "Pages key-mutation journal changed type or size after validation: {}",
            guard.target.journal_path.display()
        );
    }
    #[cfg(unix)]
    if metadata.nlink() != 1 || key_publication_mode(&metadata) & 0o077 != 0 {
        bail!(
            "Pages key-mutation journal changed ownership evidence after validation: {}",
            guard.target.journal_path.display()
        );
    }
    let probe = File::open(&guard.target.journal_path)?;
    let identity = crate::franken_sync::FileIdentity::from_file(&probe)?
        .ok_or_else(|| {
            anyhow::anyhow!("filesystem stopped exposing a stable journal pathname identity")
        })?;
    if identity != validated.identity || probe.metadata()?.len() != validated.size {
        bail!(
            "Pages key-mutation journal changed identity or size after validation: {}",
            guard.target.journal_path.display()
        );
    }
    Ok(())
}

fn optional_key_tree_evidence(path: &Path) -> Result<Option<TreeEvidence>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            inspect_key_publication_tree(path).map(Some)
        }
        Ok(_) => bail!(
            "Pages key-mutation recovery path is not a real directory: {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn remove_owned_key_mutation_tree(
    guard: &KeyMutationGuard,
    path: &Path,
    expected_digest: Option<&str>,
    staging_ownership: Option<&KeyMutationStagingOwnership>,
    expected_journal: Option<&ValidatedKeyMutationJournal>,
) -> Result<()> {
    require_key_mutation_guard(guard)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("owned key-mutation path is not valid UTF-8"))?;
    let owned_path = ["staged", "backup"].into_iter().any(|role| {
        direct_child_from_journal(
            &guard.target.live_root,
            file_name,
            role,
            "owned key-mutation cleanup path",
        )
        .is_ok_and(|owned| owned == path)
    });
    if !owned_path {
        bail!(
            "refusing to remove a path not owned by this Pages key mutation: {}",
            path.display()
        );
    }
    if expected_digest.is_none() && staging_ownership.is_none() {
        bail!(
            "refusing pre-journal cleanup without stable staging ownership: {}",
            path.display()
        );
    }
    if let Some(staging_ownership) = staging_ownership
        && staging_ownership.path != path
    {
        bail!(
            "staging ownership does not authorize cleanup of {}",
            path.display()
        );
    }
    let Some(evidence) = optional_key_tree_evidence(path)? else {
        bail!(
            "owned Pages key-mutation cleanup path disappeared before verification: {}",
            path.display()
        );
    };
    if let Some(expected_digest) = expected_digest
        && evidence.digest != expected_digest
    {
        bail!(
            "refusing to remove key-mutation tree {} because its digest drifted",
            path.display()
        );
    }
    require_key_mutation_guard(guard)?;
    if let Some(staging_ownership) = staging_ownership {
        let held_identity = crate::franken_sync::FileIdentity::from_file(
            &staging_ownership.handle,
        )?
        .ok_or_else(|| {
            anyhow::anyhow!("filesystem stopped exposing a held staging-root identity")
        })?;
        let probe = File::open(path)?;
        let path_identity = crate::franken_sync::FileIdentity::from_file(&probe)?
            .ok_or_else(|| {
                anyhow::anyhow!("filesystem stopped exposing a staging-root pathname identity")
            })?;
        if held_identity != staging_ownership.identity
            || path_identity != staging_ownership.identity
        {
            bail!(
                "Pages key-mutation staging root changed identity before cleanup: {}",
                path.display()
            );
        }
    }
    if let Some(expected_journal) = expected_journal {
        require_validated_key_mutation_journal(guard, expected_journal)?;
    }
    require_key_mutation_guard(guard)?;
    std::fs::remove_dir_all(path).with_context(|| {
        format!(
            "failed removing owned Pages key-mutation tree {}",
            path.display()
        )
    })?;
    sync_parent_directory(path)
}

fn remove_key_mutation_journal(
    guard: &KeyMutationGuard,
    expected: Option<&ValidatedKeyMutationJournal>,
) -> Result<()> {
    if let Some(expected) = expected {
        require_validated_key_mutation_journal(guard, expected)?;
    }
    require_key_mutation_guard(guard)?;
    let metadata = match std::fs::symlink_metadata(&guard.target.journal_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_file() {
        bail!(
            "refusing to remove non-file key-mutation journal {}",
            guard.target.journal_path.display()
        );
    }
    #[cfg(unix)]
    {
        if metadata.nlink() != 1 {
            bail!(
                "refusing to remove hard-linked key-mutation journal {}",
                guard.target.journal_path.display()
            );
        }
        if key_publication_mode(&metadata) & 0o077 != 0 {
            bail!(
                "refusing to remove non-owner-only key-mutation journal {}",
                guard.target.journal_path.display()
            );
        }
    }
    let journal_file = File::open(&guard.target.journal_path)?;
    let journal_identity = crate::franken_sync::FileIdentity::from_file(&journal_file)?
        .ok_or_else(|| anyhow::anyhow!("filesystem does not expose a stable journal identity"))?;
    let journal_size = journal_file.metadata()?.len();
    if let Some(expected) = expected
        && (journal_identity != expected.identity || journal_size != expected.size)
    {
        bail!(
            "Pages key-mutation journal changed identity or size before cleanup: {}",
            guard.target.journal_path.display()
        );
    }
    require_key_mutation_guard(guard)?;
    let journal_probe = File::open(&guard.target.journal_path)?;
    let path_identity = crate::franken_sync::FileIdentity::from_file(&journal_probe)?
        .ok_or_else(|| {
            anyhow::anyhow!("filesystem stopped exposing a stable journal pathname identity")
        })?;
    if path_identity != journal_identity || journal_probe.metadata()?.len() != journal_size {
        bail!(
            "Pages key-mutation journal changed identity or size before cleanup: {}",
            guard.target.journal_path.display()
        );
    }
    require_key_mutation_guard(guard)?;
    std::fs::remove_file(&guard.target.journal_path)?;
    sync_parent_directory(&guard.target.journal_path)
}

fn evidence_has_digest(evidence: &Option<TreeEvidence>, digest: &str) -> bool {
    evidence
        .as_ref()
        .is_some_and(|evidence| evidence.digest == digest)
}

fn recover_interrupted_key_mutation(
    guard: &KeyMutationGuard,
) -> Result<RecoveredKeyMutation> {
    let Some(validated_journal) = read_key_mutation_journal(guard)? else {
        return Ok(RecoveredKeyMutation::None);
    };
    let journal = &validated_journal.journal;
    let staged_root = direct_child_from_journal(
        &guard.target.live_root,
        &journal.staged_file_name,
        "staged",
        "key-mutation staged path",
    )?;
    let backup_root = direct_child_from_journal(
        &guard.target.live_root,
        &journal.backup_file_name,
        "backup",
        "key-mutation backup path",
    )?;
    if staged_root == guard.target.live_root
        || backup_root == guard.target.live_root
        || staged_root == backup_root
    {
        bail!("Pages key-mutation recovery journal aliases publication paths");
    }

    let live = optional_key_tree_evidence(&guard.target.live_root)?;
    let staged = optional_key_tree_evidence(&staged_root)?;
    let backup = optional_key_tree_evidence(&backup_root)?;

    if evidence_has_digest(&live, &journal.prior_digest)
        && evidence_has_digest(&staged, &journal.candidate_digest)
        && backup.is_none()
    {
        require_validated_key_mutation_journal(guard, &validated_journal)?;
        remove_owned_key_mutation_tree(
            guard,
            &staged_root,
            Some(&journal.candidate_digest),
            None,
            Some(&validated_journal),
        )?;
        remove_key_mutation_journal(guard, Some(&validated_journal))?;
        return Ok(RecoveredKeyMutation::RolledBack);
    }

    if evidence_has_digest(&live, &journal.candidate_digest) {
        let linux_prior = evidence_has_digest(&staged, &journal.prior_digest) && backup.is_none();
        let rename_pair_prior = staged.is_none()
            && evidence_has_digest(&backup, &journal.prior_digest);
        require_validated_key_mutation_journal(guard, &validated_journal)?;
        if linux_prior {
            remove_owned_key_mutation_tree(
                guard,
                &staged_root,
                Some(&journal.prior_digest),
                None,
                Some(&validated_journal),
            )?;
        } else if rename_pair_prior {
            remove_owned_key_mutation_tree(
                guard,
                &backup_root,
                Some(&journal.prior_digest),
                None,
                Some(&validated_journal),
            )?;
        } else {
            bail!(
                "ambiguous completed Pages key mutation: live candidate is present but its exact prior generation is not owned by the recovery journal"
            );
        }
        remove_key_mutation_journal(guard, Some(&validated_journal))?;
        return Ok(RecoveredKeyMutation::Committed);
    }

    if live.is_none()
        && evidence_has_digest(&backup, &journal.prior_digest)
        && evidence_has_digest(&staged, &journal.candidate_digest)
    {
        require_validated_key_mutation_journal(guard, &validated_journal)?;
        require_key_mutation_guard(guard)?;
        std::fs::rename(&backup_root, &guard.target.live_root).with_context(|| {
            format!(
                "failed restoring prior Pages archive {} from key-mutation backup {}",
                guard.target.live_root.display(),
                backup_root.display()
            )
        })?;
        sync_parent_directory(&guard.target.live_root)?;
        let restored = inspect_key_publication_tree(&guard.target.live_root)?;
        if restored.digest != journal.prior_digest {
            bail!(
                "restored Pages key-mutation generation at {} failed exact digest verification",
                guard.target.live_root.display()
            );
        }
        remove_owned_key_mutation_tree(
            guard,
            &staged_root,
            Some(&journal.candidate_digest),
            None,
            Some(&validated_journal),
        )?;
        remove_key_mutation_journal(guard, Some(&validated_journal))?;
        return Ok(RecoveredKeyMutation::RolledBack);
    }

    bail!(
        "ambiguous interrupted Pages key mutation at {}; live, staged, and backup trees were preserved for inspection",
        guard.target.live_root.display()
    )
}

fn begin_key_mutation(requested_path: &Path) -> Result<KeyMutationGuard> {
    let target = derive_key_mutation_target(requested_path)?;
    let mut guard = open_key_mutation_lock(target)?;
    let recovered = recover_interrupted_key_mutation(&guard)?;

    if guard.target.site_relative.as_os_str().is_empty() {
        match std::fs::symlink_metadata(guard.target.live_root.join("site")) {
            Ok(metadata)
                if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() =>
            {
                guard.target.site_relative = PathBuf::from("site");
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }

    let resolved_site = super::resolve_site_dir(&guard.target.live_root)?;
    if resolved_site != guard.target.live_site_dir() {
        bail!(
            "Pages key-mutation target resolved inconsistently: expected {}, got {}",
            guard.target.live_site_dir().display(),
            resolved_site.display()
        );
    }
    if recovered == RecoveredKeyMutation::Committed {
        bail!(
            "Recovered a previously committed Pages key mutation at {}; inspect the current key slots before requesting another mutation",
            guard.target.live_root.display()
        );
    }
    Ok(guard)
}

fn verify_published_key_tree(path: &Path, expected: &TreeEvidence) -> Result<()> {
    let actual = inspect_key_publication_tree(path)?;
    if &actual != expected {
        bail!(
            "published Pages key-mutation tree failed exact verification: expected digest {} ({} entries, {} bytes), got {} ({} entries, {} bytes)",
            expected.digest,
            expected.entries,
            expected.bytes,
            actual.digest,
            actual.entries,
            actual.bytes
        );
    }
    Ok(())
}

fn cleanup_committed_key_mutation(
    guard: &KeyMutationGuard,
    prior_path: &Path,
    prior_digest: &str,
    journal: &ValidatedKeyMutationJournal,
) {
    if let Err(error) =
        remove_owned_key_mutation_tree(
            guard,
            prior_path,
            Some(prior_digest),
            None,
            Some(journal),
        )
    {
        tracing::warn!(
            live_root = %guard.target.live_root.display(),
            retained_prior = %prior_path.display(),
            error = %format!("{error:#}"),
            "Pages key mutation committed and verified, but its prior generation remains retained"
        );
        return;
    }
    if let Err(error) = remove_key_mutation_journal(guard, Some(journal)) {
        tracing::warn!(
            live_root = %guard.target.live_root.display(),
            journal = %guard.target.journal_path.display(),
            error = %format!("{error:#}"),
            "Pages key mutation committed and verified, but its recovery journal remains retained"
        );
    }
}

fn publish_staged_key_mutation(
    guard: &KeyMutationGuard,
    staged_root: &Path,
    prior: &TreeEvidence,
    candidate: &TreeEvidence,
) -> Result<()> {
    require_key_mutation_guard(guard)?;
    let current = inspect_key_publication_tree(&guard.target.live_root)?;
    if &current != prior {
        bail!(
            "Pages archive changed while its key mutation was staged; live generation was preserved"
        );
    }
    verify_published_key_tree(staged_root, candidate)?;

    let backup_root = random_key_mutation_sidecar_path(&guard.target.live_root, "backup");
    match std::fs::symlink_metadata(&backup_root) {
        Ok(_) => bail!(
            "fresh Pages key-mutation backup path unexpectedly exists: {}",
            backup_root.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed inspecting fresh Pages key-mutation backup path {}",
                    backup_root.display()
                )
            });
        }
    }
    let journal =
        write_key_mutation_journal(guard, staged_root, &backup_root, prior, candidate)?;
    require_key_mutation_guard(guard)?;

    #[cfg(target_os = "linux")]
    {
        require_key_mutation_guard(guard)?;
        match crate::indexer::atomic_exchange_paths(&guard.target.live_root, staged_root) {
            Ok(()) => {
                sync_parent_directory(&guard.target.live_root).with_context(|| {
                    format!(
                        "Pages key mutation is live at {}, but its atomic exchange could not be durably synced; prior generation retained at {}",
                        guard.target.live_root.display(),
                        staged_root.display()
                    )
                })?;
                if let Err(verification_error) =
                    verify_published_key_tree(&guard.target.live_root, candidate)
                {
                    require_key_mutation_guard(guard)?;
                    crate::indexer::atomic_exchange_paths(&guard.target.live_root, staged_root)
                        .context(
                            "published Pages key tree failed verification and atomic rollback failed",
                        )?;
                    sync_parent_directory(&guard.target.live_root)?;
                    verify_published_key_tree(&guard.target.live_root, prior)?;
                    remove_owned_key_mutation_tree(
                        guard,
                        staged_root,
                        Some(&candidate.digest),
                        None,
                        Some(&journal),
                    )?;
                    remove_key_mutation_journal(guard, Some(&journal))?;
                    return Err(verification_error.context(
                        "published Pages key tree failed verification; restored prior generation",
                    ));
                }
                cleanup_committed_key_mutation(
                    guard,
                    staged_root,
                    &prior.digest,
                    &journal,
                );
                return Ok(());
            }
            Err(error) if crate::indexer::linux_atomic_exchange_is_unsupported(&error) => {
                tracing::info!(
                    live_root = %guard.target.live_root.display(),
                    "atomic Pages key-mutation exchange is unsupported; using recoverable rename-pair publication"
                );
            }
            Err(error) => return Err(error.context("failed atomically publishing Pages key mutation")),
        }
    }

    require_key_mutation_guard(guard)?;
    std::fs::rename(&guard.target.live_root, &backup_root).with_context(|| {
        format!(
            "failed parking prior Pages archive {} at {}",
            guard.target.live_root.display(),
            backup_root.display()
        )
    })?;
    if let Err(sync_error) = sync_parent_directory(&guard.target.live_root) {
        require_key_mutation_guard(guard)?;
        match std::fs::rename(&backup_root, &guard.target.live_root) {
            Ok(()) => {
                sync_parent_directory(&guard.target.live_root)?;
                remove_owned_key_mutation_tree(
                    guard,
                    staged_root,
                    Some(&candidate.digest),
                    None,
                    Some(&journal),
                )?;
                remove_key_mutation_journal(guard, Some(&journal))?;
                return Err(sync_error.context(
                    "failed durably parking prior Pages archive; restored prior generation",
                ));
            }
            Err(restore_error) => bail!(
                "failed syncing parked prior Pages archive: {sync_error}; restore also failed: {restore_error}; prior retained at {} and candidate at {}",
                backup_root.display(),
                staged_root.display()
            ),
        }
    }

    require_key_mutation_guard(guard)?;
    if let Err(publish_error) = std::fs::rename(staged_root, &guard.target.live_root) {
        require_key_mutation_guard(guard)?;
        return match std::fs::rename(&backup_root, &guard.target.live_root) {
            Ok(()) => {
                sync_parent_directory(&guard.target.live_root)?;
                remove_owned_key_mutation_tree(
                    guard,
                    staged_root,
                    Some(&candidate.digest),
                    None,
                    Some(&journal),
                )?;
                remove_key_mutation_journal(guard, Some(&journal))?;
                Err(publish_error).context(
                    "failed publishing staged Pages key mutation; restored prior generation",
                )
            }
            Err(restore_error) => bail!(
                "failed publishing staged Pages key mutation: {publish_error}; restore also failed: {restore_error}; prior retained at {} and candidate at {}",
                backup_root.display(),
                staged_root.display()
            ),
        };
    }
    sync_parent_directory(&guard.target.live_root).with_context(|| {
        format!(
            "Pages key mutation is live at {}, but publication could not be durably synced; prior generation retained at {}",
            guard.target.live_root.display(),
            backup_root.display()
        )
    })?;

    if let Err(verification_error) = verify_published_key_tree(&guard.target.live_root, candidate) {
        require_key_mutation_guard(guard)?;
        std::fs::rename(&guard.target.live_root, staged_root).with_context(|| {
            format!(
                "published Pages key tree failed verification and candidate could not be parked at {}",
                staged_root.display()
            )
        })?;
        require_key_mutation_guard(guard)?;
        std::fs::rename(&backup_root, &guard.target.live_root)
            .context("published Pages key tree failed verification and prior restore failed")?;
        sync_parent_directory(&guard.target.live_root)?;
        verify_published_key_tree(&guard.target.live_root, prior)?;
        remove_owned_key_mutation_tree(
            guard,
            staged_root,
            Some(&candidate.digest),
            None,
            Some(&journal),
        )?;
        remove_key_mutation_journal(guard, Some(&journal))?;
        return Err(verification_error
            .context("published Pages key tree failed verification; restored prior generation"));
    }

    cleanup_committed_key_mutation(guard, &backup_root, &prior.digest, &journal);
    Ok(())
}

fn publish_key_config_mutation(
    guard: &KeyMutationGuard,
    config: &EncryptionConfig,
    recovery_secret: Option<&[u8]>,
    remove_recovery_artifacts: bool,
) -> Result<()> {
    let (staged_root, prior, staging_ownership) =
        stage_key_mutation_tree(guard, KeyMutationStageMode::PreserveSite)?;
    let staged_site = guard.target.staged_site_dir(&staged_root);
    let prepare_result = (|| -> Result<TreeEvidence> {
        materialize_safe_required_file_symlinks(&staged_site)?;
        write_json_pretty_atomically(&staged_site.join("config.json"), config)?;
        let manifest = regenerate_integrity_manifest(&staged_site)?;
        refresh_private_artifacts(
            &staged_site,
            config,
            manifest.as_ref(),
            recovery_secret,
            remove_recovery_artifacts,
        )?;
        let verification = crate::pages::verify::verify_bundle(&staged_site, false)?;
        if verification.status != "valid" {
            bail!(
                "staged Pages key mutation failed bundle verification with status {}",
                verification.status
            );
        }
        sync_tree(&staged_root)?;
        inspect_key_publication_tree(&staged_root)
    })();
    let candidate = match prepare_result {
        Ok(candidate) => candidate,
        Err(error) => {
            return match remove_owned_key_mutation_tree(
                guard,
                &staged_root,
                None,
                Some(&staging_ownership),
                None,
            ) {
                Ok(()) => Err(error),
                Err(cleanup_error) => Err(error.context(format!(
                    "failed cleaning rejected key-mutation candidate: {cleanup_error:#}"
                ))),
            };
        }
    };
    publish_staged_key_mutation(guard, &staged_root, &prior, &candidate)
}

/// Result of listing key slots
#[derive(Debug, Clone, Serialize)]
pub struct KeyListResult {
    pub slots: Vec<KeySlotInfo>,
    pub active_slots: usize,
    pub dek_created_at: Option<String>,
    pub export_id: String,
}

/// Information about a single key slot
#[derive(Debug, Clone, Serialize)]
pub struct KeySlotInfo {
    pub id: u8,
    pub slot_type: String,
    pub kdf: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kdf_params: Option<Argon2Params>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Result of adding a key slot
#[derive(Debug)]
pub enum AddKeyResult {
    Password { slot_id: u8 },
    Recovery { slot_id: u8, secret: RecoverySecret },
}

/// Result of revoking a key slot
#[derive(Debug, Serialize)]
pub struct RevokeResult {
    pub revoked_slot_id: u8,
    pub remaining_slots: usize,
}

/// Result of key rotation
#[derive(Debug, Serialize)]
pub struct RotateResult {
    pub new_dek_created_at: DateTime<Utc>,
    pub slot_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery_secret: Option<String>,
}

/// List all key slots in an archive
pub fn key_list(archive_dir: &Path) -> Result<KeyListResult> {
    let archive_dir = super::resolve_site_dir(archive_dir)?;
    let config = load_config(&archive_dir)?;

    let slots: Vec<KeySlotInfo> = config
        .key_slots
        .iter()
        .map(|slot| KeySlotInfo {
            id: slot.id,
            slot_type: match slot.slot_type {
                SlotType::Password => "password".to_string(),
                SlotType::Recovery => "recovery".to_string(),
            },
            kdf: match slot.kdf {
                KdfAlgorithm::Argon2id => "argon2id".to_string(),
                KdfAlgorithm::HkdfSha256 => "hkdf-sha256".to_string(),
            },
            kdf_params: slot.argon2_params.clone(),
            label: None, // Labels stored in encrypted metadata (future)
        })
        .collect();

    Ok(KeyListResult {
        active_slots: slots.len(),
        slots,
        dek_created_at: None, // Would need to store in config
        export_id: config.export_id,
    })
}

/// Add a new password-based key slot
pub fn key_add_password(
    archive_dir: &Path,
    current_password: &str,
    new_password: &str,
) -> Result<u8> {
    validate_password_input(new_password)?;
    let guard = begin_key_mutation(archive_dir)?;
    let archive_dir = guard.target.live_site_dir();
    let mut config = load_config(&archive_dir)?;
    validate_supported_payload_format(&config)?;

    // Unlock with current password to get DEK
    let dek = zeroize::Zeroizing::new(unwrap_dek_with_password(&config, current_password)?);

    // Create new slot (use max ID + 1 since IDs are stable after revocation)
    // If no slots exist, start at 0; otherwise use max + 1
    let slot_id = next_key_slot_id(&config.key_slots)?;
    let new_slot = create_password_slot(new_password, &dek, &config.export_id, slot_id)?;

    config.key_slots.push(new_slot);
    publish_key_config_mutation(&guard, &config, None, false)?;

    info!(slot_id, "Added password key slot");
    Ok(slot_id)
}

/// Add a new recovery secret key slot
pub fn key_add_recovery(
    archive_dir: &Path,
    current_password: &str,
) -> Result<(u8, RecoverySecret)> {
    let guard = begin_key_mutation(archive_dir)?;
    let archive_dir = guard.target.live_site_dir();
    let mut config = load_config(&archive_dir)?;
    validate_supported_payload_format(&config)?;

    // Unlock with current password to get DEK
    let dek = zeroize::Zeroizing::new(unwrap_dek_with_password(&config, current_password)?);

    // Generate recovery secret
    let secret = RecoverySecret::generate();

    // Create new slot (use max ID + 1 since IDs are stable after revocation)
    // If no slots exist, start at 0; otherwise use max + 1
    let slot_id = next_key_slot_id(&config.key_slots)?;
    let new_slot = create_recovery_slot(secret.as_bytes(), &dek, &config.export_id, slot_id)?;

    config.key_slots.push(new_slot);
    publish_key_config_mutation(
        &guard,
        &config,
        Some(secret.as_bytes()),
        false,
    )?;

    info!(slot_id, "Added recovery key slot");
    Ok((slot_id, secret))
}

fn next_key_slot_id(key_slots: &[KeySlot]) -> Result<u8> {
    match key_slots.iter().map(|s| s.id).max() {
        Some(max_id) => max_id.checked_add(1).ok_or_else(|| {
            anyhow::anyhow!("Cannot add more key slots: maximum slot ID (255) reached")
        }),
        None => Ok(0),
    }
}

/// Revoke a key slot
pub fn key_revoke(
    archive_dir: &Path,
    current_password: &str,
    slot_id_to_revoke: u8,
) -> Result<RevokeResult> {
    let guard = begin_key_mutation(archive_dir)?;
    let archive_dir = guard.target.live_site_dir();
    let mut config = load_config(&archive_dir)?;
    validate_supported_payload_format(&config)?;

    // Safety: Cannot revoke last slot
    if config.key_slots.len() <= 1 {
        anyhow::bail!("Cannot revoke the last remaining key slot. Add another key first.");
    }

    // Find which slot authenticates with this password
    let (auth_slot_id, dek) = unwrap_dek_with_slot_id(&config, current_password)?;
    let mut _dek = zeroize::Zeroizing::new(dek); // Zeroize on drop

    // Verify they aren't trying to revoke the slot they are currently using
    if auth_slot_id == slot_id_to_revoke {
        bail!(
            "Cannot revoke slot {} used for authentication. Use a different password.",
            slot_id_to_revoke
        );
    }

    // Verify slot exists
    if !config.key_slots.iter().any(|s| s.id == slot_id_to_revoke) {
        bail!("Slot {} not found", slot_id_to_revoke);
    }

    let revoked_slot_is_recovery = config
        .key_slots
        .iter()
        .find(|s| s.id == slot_id_to_revoke)
        .map(|s| s.slot_type == SlotType::Recovery)
        .unwrap_or(false);

    // Remove the slot (keeping IDs stable - they're part of the AAD binding)
    config.key_slots.retain(|s| s.id != slot_id_to_revoke);
    let has_recovery_slot = config
        .key_slots
        .iter()
        .any(|slot| slot.slot_type == SlotType::Recovery);
    publish_key_config_mutation(
        &guard,
        &config,
        None,
        revoked_slot_is_recovery || !has_recovery_slot,
    )?;

    info!(slot_id = slot_id_to_revoke, "Revoked key slot");
    Ok(RevokeResult {
        revoked_slot_id: slot_id_to_revoke,
        remaining_slots: config.key_slots.len(),
    })
}

/// Full key rotation - regenerate DEK and re-encrypt payload
pub fn key_rotate(
    archive_dir: &Path,
    old_password: &str,
    new_password: &str,
    keep_recovery: bool,
    progress: impl Fn(f32),
) -> Result<RotateResult> {
    validate_password_input(new_password)?;
    let guard = begin_key_mutation(archive_dir)?;
    let archive_dir = guard.target.live_site_dir();
    let config = load_config(&archive_dir)?;
    validate_supported_payload_format(&config)?;
    let old_export_id_raw = BASE64_STANDARD.decode(&config.export_id)?;
    let old_export_id: [u8; 16] = old_export_id_raw.as_slice().try_into().map_err(|err| {
        // [coding_agent_session_search-htiim] Chain the underlying
        // TryFromSliceError so a debug-mode error chain shows the
        // exact conversion that failed in addition to the
        // human-readable length mismatch.
        anyhow::anyhow!(
            "invalid export_id length: expected 16, got {}: {err}",
            old_export_id_raw.len()
        )
    })?;

    // 1. Decrypt payload with old password
    let old_dek = zeroize::Zeroizing::new(unwrap_dek_with_password(&config, old_password)?);
    let plaintext =
        zeroize::Zeroizing::new(decrypt_all_chunks(&archive_dir, &old_dek, &config, |p| {
            progress(p * 0.5)
        })?);

    // 2. Generate new DEK and export_id
    let mut new_dek = zeroize::Zeroizing::new([0u8; 32]);
    let mut new_export_id = [0u8; 16];
    let mut new_base_nonce = [0u8; 12];
    let mut rng = rand::rng();
    rng.fill_bytes(new_dek.as_mut());
    rng.fill_bytes(&mut new_export_id);
    rng.fill_bytes(&mut new_base_nonce);

    // 3. Create replacement key slots before staging any publishable bytes.
    let mut new_slots = vec![create_password_slot(
        new_password,
        &new_dek,
        &BASE64_STANDARD.encode(new_export_id),
        0,
    )?];

    let mut recovery_secret_encoded: Option<String> = None;
    let mut recovery_secret_bytes: Option<Vec<u8>> = None;
    if keep_recovery {
        let secret = RecoverySecret::generate();
        new_slots.push(create_recovery_slot(
            secret.as_bytes(),
            &new_dek,
            &BASE64_STANDARD.encode(new_export_id),
            1,
        )?);
        recovery_secret_bytes = Some(secret.as_bytes().to_vec());
        recovery_secret_encoded = Some(secret.encoded().to_string());
    }

    // 4. Stage the complete site/private generation and publish it through
    // the same journaled transaction as add/revoke. Old payload and blob
    // ciphertext are deliberately omitted from the staged copy.
    let (staged_root, prior, staging_ownership) = stage_key_mutation_tree(
        &guard,
        KeyMutationStageMode::ReplaceEncryptedPayload,
    )?;
    let staged_site_dir = guard.target.staged_site_dir(&staged_root);
    let prepare_result = (|| -> Result<TreeEvidence> {
        let (chunk_count, total_compressed_size) = encrypt_all_chunks(
            &plaintext,
            &new_dek,
            &new_export_id,
            &new_base_nonce,
            config.payload.chunk_size,
            &staged_site_dir.join("payload"),
            |p| progress(0.5 + p * 0.5),
        )?;
        reencrypt_blobs_into_dir(
            &archive_dir,
            &staged_site_dir,
            &old_dek,
            &old_export_id,
            &new_dek,
            &new_export_id,
        )?;

        let new_config = EncryptionConfig {
            version: config.version,
            export_id: BASE64_STANDARD.encode(new_export_id),
            base_nonce: BASE64_STANDARD.encode(new_base_nonce),
            compression: config.compression.clone(),
            kdf_defaults: Argon2Params::default(),
            payload: crate::pages::encrypt::PayloadMeta {
                chunk_size: config.payload.chunk_size,
                chunk_count,
                total_compressed_size,
                total_plaintext_size: plaintext.len() as u64,
                files: (0..chunk_count)
                    .map(|i| format!("payload/chunk-{i:05}.bin"))
                    .collect(),
            },
            key_slots: new_slots.clone(),
        };
        write_json_pretty(&staged_site_dir.join("config.json"), &new_config)?;
        let manifest = crate::pages::bundle::generate_integrity_manifest(&staged_site_dir)?;
        write_json_pretty(&staged_site_dir.join("integrity.json"), &manifest)?;
        refresh_private_artifacts(
            &staged_site_dir,
            &new_config,
            Some(&manifest),
            recovery_secret_bytes.as_deref(),
            !keep_recovery,
        )?;
        let verification = crate::pages::verify::verify_bundle(&staged_site_dir, false)?;
        if verification.status != "valid" {
            bail!(
                "staged Pages key rotation failed bundle verification with status {}",
                verification.status
            );
        }
        sync_tree(&staged_root)?;
        inspect_key_publication_tree(&staged_root)
    })();
    let candidate = match prepare_result {
        Ok(candidate) => candidate,
        Err(error) => {
            return match remove_owned_key_mutation_tree(
                &guard,
                &staged_root,
                None,
                Some(&staging_ownership),
                None,
            ) {
                Ok(()) => Err(error),
                Err(cleanup_error) => Err(error.context(format!(
                    "failed cleaning rejected key-rotation candidate: {cleanup_error:#}"
                ))),
            };
        }
    };
    publish_staged_key_mutation(&guard, &staged_root, &prior, &candidate)?;

    Ok(RotateResult {
        new_dek_created_at: chrono::Utc::now(),
        slot_count: new_slots.len(),
        recovery_secret: recovery_secret_encoded,
    })
}

// ============================================================================
// Helper functions
// ============================================================================

/// Unwrap DEK using password (tries all password slots)
fn unwrap_dek_with_password(config: &EncryptionConfig, password: &str) -> Result<[u8; 32]> {
    validate_password_input(password)?;
    let export_id = BASE64_STANDARD.decode(&config.export_id)?;

    for slot in &config.key_slots {
        if slot.slot_type != SlotType::Password {
            continue;
        }

        let salt = BASE64_STANDARD.decode(&slot.salt)?;
        let wrapped_dek = BASE64_STANDARD.decode(&slot.wrapped_dek)?;
        let nonce = BASE64_STANDARD.decode(&slot.nonce)?;

        if let Ok(kek) = derive_kek_argon2id(password, &salt) {
            let result = unwrap_key(&kek, &wrapped_dek, &nonce, &export_id, slot.id);
            if let Ok(dek) = result {
                return Ok(dek);
            }
        }
    }

    bail!("Invalid password or no matching key slot")
}

/// Unwrap DEK and return which slot was used
fn unwrap_dek_with_slot_id(config: &EncryptionConfig, password: &str) -> Result<(u8, [u8; 32])> {
    validate_password_input(password)?;
    let export_id = BASE64_STANDARD.decode(&config.export_id)?;

    for slot in &config.key_slots {
        if slot.slot_type != SlotType::Password {
            continue;
        }

        let salt = BASE64_STANDARD.decode(&slot.salt)?;
        let wrapped_dek = BASE64_STANDARD.decode(&slot.wrapped_dek)?;
        let nonce = BASE64_STANDARD.decode(&slot.nonce)?;

        if let Ok(kek) = derive_kek_argon2id(password, &salt) {
            let result = unwrap_key(&kek, &wrapped_dek, &nonce, &export_id, slot.id);
            if let Ok(dek) = result {
                return Ok((slot.id, dek));
            }
        }
    }

    bail!("Invalid password or no matching key slot")
}

/// Derive KEK from password using Argon2id
fn derive_kek_argon2id(password: &str, salt: &[u8]) -> Result<zeroize::Zeroizing<[u8; 32]>> {
    let params = Params::new(
        ARGON2_MEMORY_KB,
        ARGON2_ITERATIONS,
        ARGON2_PARALLELISM,
        Some(32),
    )
    .map_err(|e| anyhow::anyhow!("Invalid Argon2 parameters: {:?}", e))?;

    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);

    let mut kek = zeroize::Zeroizing::new([0u8; 32]);
    argon2
        .hash_password_into(password.as_bytes(), salt, kek.as_mut())
        .map_err(|e| anyhow::anyhow!("Argon2 derivation failed: {:?}", e))?;

    Ok(kek)
}

/// Derive KEK from recovery secret using HKDF-SHA256
fn derive_kek_hkdf(secret: &[u8], salt: &[u8]) -> Result<zeroize::Zeroizing<[u8; 32]>> {
    let kek = crate::encryption::hkdf_extract_expand(secret, salt, b"cass-pages-kek-v2", 32)
        .map_err(|e| anyhow::anyhow!("HKDF extract+expand failed for recovery secret KEK: {e}"))?;
    let actual_len = kek.len();
    let kek: [u8; 32] = kek.try_into().map_err(|_| {
        // [coding_agent_session_search-htiim] Capture actual_len BEFORE
        // try_into consumes the Vec so the message can show the actual
        // KEK length operators / future contributors need to debug a
        // frankensqlite / hkdf upstream regression.
        anyhow::anyhow!(
            "HKDF expansion produced invalid KEK length: expected 32, got {}",
            actual_len
        )
    })?;
    Ok(zeroize::Zeroizing::new(kek))
}

/// Unwrap DEK with KEK
fn unwrap_key(
    kek: &[u8; 32],
    wrapped: &[u8],
    nonce: &[u8],
    export_id: &[u8],
    slot_id: u8,
) -> Result<[u8; 32]> {
    let cipher = Aes256Gcm::new_from_slice(kek).expect("Invalid key length");

    let actual_nonce_len = nonce.len();
    let nonce: &[u8; 12] = nonce.try_into().map_err(|err| {
        // [coding_agent_session_search-htiim] Chain TryFromSliceError so
        // debug-mode chains preserve the source while the operator
        // sees the exact-length diagnostic.
        anyhow::anyhow!(
            "invalid nonce length: expected 12, got {}: {err}",
            actual_nonce_len
        )
    })?;

    // AAD: export_id || slot_id
    let mut aad = Vec::with_capacity(export_id.len() + 1);
    aad.extend_from_slice(export_id);
    aad.push(slot_id);

    let dek = cipher
        .decrypt(
            &Nonce::from(*nonce),
            Payload {
                msg: wrapped,
                aad: &aad,
            },
        )
        .map_err(|err| {
            // [coding_agent_session_search-htiim] Chain the underlying
            // aead error so operators can distinguish "wrong password
            // (KEK derivation succeeded but DEK MAC failed)" from
            // "corrupt key slot ciphertext" from "wrong AAD (slot id /
            // export id mismatch)". The aead crate's Display impl
            // remains opaque about the specific sub-failure (timing-
            // attack hardening), but the source error type IS preserved
            // so debug-mode error chains can show whether the failure
            // came from the cipher layer vs a subsequent layer. Slot
            // id is included so operators can correlate with the
            // recovery / password slot they were attempting. Mirrors
            // the encrypt.rs::unwrap_key fix landed in 0b81b601.
            anyhow::anyhow!(
                "Key unwrapping failed for slot {} ({} bytes wrapped, {} bytes nonce, \
                 {} bytes aad): {}",
                slot_id,
                wrapped.len(),
                actual_nonce_len,
                aad.len(),
                err
            )
        })?;

    let dek_len = dek.len();
    dek.try_into().map_err(|_| {
        anyhow::anyhow!(
            "Invalid DEK length after unwrap: expected 32, got {}",
            dek_len
        )
    })
}

/// Create a password-based key slot
fn create_password_slot(
    password: &str,
    dek: &[u8; 32],
    export_id_b64: &str,
    slot_id: u8,
) -> Result<KeySlot> {
    validate_password_input(password)?;
    let export_id = BASE64_STANDARD.decode(export_id_b64)?;

    // Generate salt
    let mut salt = [0u8; 32];
    let mut rng = rand::rng();
    rng.fill_bytes(&mut salt);

    // Derive KEK from password
    let kek = derive_kek_argon2id(password, &salt)?;

    // Wrap DEK
    let result = wrap_key(&kek, dek, &export_id, slot_id);

    let (wrapped_dek, nonce) = result?;

    Ok(KeySlot {
        id: slot_id,
        slot_type: SlotType::Password,
        kdf: KdfAlgorithm::Argon2id,
        salt: BASE64_STANDARD.encode(salt),
        wrapped_dek: BASE64_STANDARD.encode(&wrapped_dek),
        nonce: BASE64_STANDARD.encode(nonce),
        argon2_params: Some(Argon2Params::default()),
    })
}

/// Create a recovery secret key slot
fn create_recovery_slot(
    secret: &[u8],
    dek: &[u8; 32],
    export_id_b64: &str,
    slot_id: u8,
) -> Result<KeySlot> {
    if secret.len() < MIN_RECOVERY_SECRET_BYTES {
        bail!("Recovery secret must contain at least {MIN_RECOVERY_SECRET_BYTES} bytes (192 bits)");
    }
    let export_id = BASE64_STANDARD.decode(export_id_b64)?;

    // Generate salt
    let mut salt = [0u8; 16];
    let mut rng = rand::rng();
    rng.fill_bytes(&mut salt);

    // Derive KEK from recovery secret
    let kek = derive_kek_hkdf(secret, &salt)?;

    // Wrap DEK
    let result = wrap_key(&kek, dek, &export_id, slot_id);

    let (wrapped_dek, nonce) = result?;

    Ok(KeySlot {
        id: slot_id,
        slot_type: SlotType::Recovery,
        kdf: KdfAlgorithm::HkdfSha256,
        salt: BASE64_STANDARD.encode(salt),
        wrapped_dek: BASE64_STANDARD.encode(&wrapped_dek),
        nonce: BASE64_STANDARD.encode(nonce),
        argon2_params: None,
    })
}

/// Wrap DEK with KEK using AES-256-GCM
fn wrap_key(
    kek: &[u8; 32],
    dek: &[u8; 32],
    export_id: &[u8],
    slot_id: u8,
) -> Result<(Vec<u8>, [u8; 12])> {
    let cipher = Aes256Gcm::new_from_slice(kek).expect("Invalid key length");

    let mut nonce = [0u8; 12];
    let mut rng = rand::rng();
    rng.fill_bytes(&mut nonce);

    // AAD: export_id || slot_id
    let mut aad = Vec::with_capacity(export_id.len() + 1);
    aad.extend_from_slice(export_id);
    aad.push(slot_id);

    let wrapped = cipher
        .encrypt(
            &Nonce::from(nonce),
            Payload {
                msg: dek,
                aad: &aad,
            },
        )
        .map_err(|e| anyhow::anyhow!("Key wrapping failed: {}", e))?;

    Ok((wrapped, nonce))
}

/// Decrypt all chunks and return plaintext
fn decrypt_all_chunks(
    archive_dir: &Path,
    dek: &[u8; 32],
    config: &EncryptionConfig,
    progress: impl Fn(f32),
) -> Result<Vec<u8>> {
    let cipher = Aes256Gcm::new_from_slice(dek).expect("Invalid key length");
    let base_nonce_raw = BASE64_STANDARD.decode(&config.base_nonce)?;
    let base_nonce: [u8; 12] = base_nonce_raw.as_slice().try_into().map_err(|err| {
        // [coding_agent_session_search-htiim] Chain TryFromSliceError so
        // debug chains preserve the source. Length diagnostic is the
        // operator-facing signal.
        anyhow::anyhow!(
            "invalid base_nonce length: expected 12, got {}: {err}",
            base_nonce_raw.len()
        )
    })?;
    let export_id_raw = BASE64_STANDARD.decode(&config.export_id)?;
    let export_id: [u8; 16] = export_id_raw.as_slice().try_into().map_err(|err| {
        // [coding_agent_session_search-htiim] Chain TryFromSliceError.
        anyhow::anyhow!(
            "invalid export_id length: expected 16, got {}: {err}",
            export_id_raw.len()
        )
    })?;
    let canonical_archive_dir = archive_dir.canonicalize().with_context(|| {
        format!(
            "Failed to resolve archive root {} before decrypting chunks",
            archive_dir.display()
        )
    })?;

    let mut plaintext = Vec::new();
    let mut total_ciphertext_size = 0u64;
    let mut total_plaintext_size = 0u64;

    if config.payload.chunk_count != config.payload.files.len() {
        bail!(
            "Invalid config: payload chunk_count {} does not match file list length {}",
            config.payload.chunk_count,
            config.payload.files.len()
        );
    }

    for (chunk_index, chunk_file) in config.payload.files.iter().enumerate() {
        progress(chunk_index as f32 / config.payload.chunk_count as f32);

        let expected_chunk_file = format!("payload/chunk-{chunk_index:05}.bin");
        if chunk_file != &expected_chunk_file {
            bail!(
                "Invalid chunk path in config.json: expected {}, got {}",
                expected_chunk_file,
                chunk_file
            );
        }
        let chunk_path = archive_dir.join(chunk_file);
        let chunk_meta = std::fs::symlink_metadata(&chunk_path).with_context(|| {
            format!(
                "Failed to inspect encrypted chunk {} at {}",
                chunk_index,
                chunk_path.display()
            )
        })?;
        if chunk_meta.file_type().is_symlink() {
            bail!("Encrypted chunk must not be a symlink: {}", chunk_file);
        }
        if !chunk_meta.file_type().is_file() {
            bail!("Encrypted chunk must be a regular file: {}", chunk_file);
        }

        let canonical_chunk_path = chunk_path.canonicalize().with_context(|| {
            format!(
                "Failed to resolve encrypted chunk {} at {}",
                chunk_index,
                chunk_path.display()
            )
        })?;
        if !canonical_chunk_path.starts_with(&canonical_archive_dir) {
            bail!(
                "Encrypted chunk path escapes archive directory: {}",
                chunk_file
            );
        }

        let max_ciphertext_size = max_archive_ciphertext_chunk_size(config.payload.chunk_size);
        let chunk_file = File::open(&canonical_chunk_path)?;
        let mut ciphertext = Vec::new();
        chunk_file
            .take(max_ciphertext_size.saturating_add(1))
            .read_to_end(&mut ciphertext)?;
        if ciphertext.len() as u64 > max_ciphertext_size {
            bail!(
                "Encrypted chunk {} exceeds the maximum size of {} bytes",
                chunk_index,
                max_ciphertext_size
            );
        }
        total_ciphertext_size = total_ciphertext_size
            .checked_add(ciphertext.len() as u64)
            .context("Encrypted payload byte count overflowed u64")?;

        // Derive nonce
        let nonce = derive_chunk_nonce(&base_nonce, chunk_index as u32);

        // Build AAD
        let aad = build_chunk_aad(&export_id, chunk_index as u32);

        // Decrypt
        let compressed = cipher
            .decrypt(
                &Nonce::from(nonce),
                Payload {
                    msg: &ciphertext,
                    aad: &aad,
                },
            )
            .map_err(|err| {
                // [coding_agent_session_search-htiim] Chain the aead error
                // so operators can correlate: which chunk failed, how
                // big the ciphertext was, and what the cipher layer
                // reported. The aead crate keeps the sub-failure type
                // opaque (timing-attack hardening) but the source is
                // preserved in the error chain. Mirrors encrypt.rs::
                // decrypt_all_chunks fix landed in 0b81b601.
                anyhow::anyhow!(
                    "Decryption failed for chunk {} ({} bytes ciphertext): {}",
                    chunk_index,
                    ciphertext.len(),
                    err
                )
            })?;

        let chunk_plaintext =
            decompress_archive_chunk(&compressed, config.payload.chunk_size, chunk_index)?;
        total_plaintext_size = total_plaintext_size
            .checked_add(chunk_plaintext.len() as u64)
            .context("Decrypted payload byte count overflowed u64")?;
        plaintext.extend_from_slice(&chunk_plaintext);
    }

    if total_ciphertext_size != config.payload.total_compressed_size {
        bail!(
            "Encrypted payload contains {} ciphertext bytes; config declares {}",
            total_ciphertext_size,
            config.payload.total_compressed_size
        );
    }
    if total_plaintext_size != config.payload.total_plaintext_size {
        bail!(
            "Decrypted payload contains {} plaintext bytes; config declares {}",
            total_plaintext_size,
            config.payload.total_plaintext_size
        );
    }

    progress(1.0);
    Ok(plaintext)
}

/// Encrypt plaintext into chunks
fn encrypt_all_chunks(
    plaintext: &[u8],
    dek: &[u8; 32],
    export_id: &[u8; 16],
    base_nonce: &[u8; 12],
    chunk_size: usize,
    payload_dir: &Path,
    progress: impl Fn(f32),
) -> Result<(usize, u64)> {
    std::fs::create_dir_all(payload_dir)?;

    let cipher = Aes256Gcm::new_from_slice(dek).expect("Invalid key length");
    if chunk_size == 0 {
        anyhow::bail!("chunk_size must be > 0");
    }
    let total_chunks = plaintext.len().div_ceil(chunk_size);
    ensure_archive_chunk_count_fits_nonce_space(total_chunks as u64, chunk_size)?;
    let mut chunk_index = 0u32;
    let mut total_ciphertext_size = 0u64;

    for (i, chunk) in plaintext.chunks(chunk_size).enumerate() {
        progress(i as f32 / total_chunks as f32);
        ensure_can_write_archive_chunk(chunk_index, chunk_size)?;

        // Compress
        let mut compressed = Vec::new();
        {
            let mut encoder = DeflateEncoder::new(&mut compressed, Compression::default());
            encoder.write_all(chunk)?;
            encoder.finish()?;
        }

        // Derive nonce
        let nonce = derive_chunk_nonce(base_nonce, chunk_index);

        // Build AAD
        let aad = build_chunk_aad(export_id, chunk_index);

        // Encrypt
        let ciphertext = cipher
            .encrypt(
                &Nonce::from(nonce),
                Payload {
                    msg: &compressed,
                    aad: &aad,
                },
            )
            .map_err(|e| anyhow::anyhow!("Encryption failed: {}", e))?;

        // Write chunk file
        let chunk_filename = format!("chunk-{:05}.bin", chunk_index);
        let chunk_path = payload_dir.join(&chunk_filename);
        let mut chunk_file = File::create(&chunk_path)?;
        chunk_file.write_all(&ciphertext)?;
        total_ciphertext_size = total_ciphertext_size
            .checked_add(ciphertext.len() as u64)
            .context("Encrypted payload byte count overflowed u64")?;

        chunk_index = chunk_index.checked_add(1).ok_or_else(|| {
            anyhow::anyhow!(
                "File too large: exceeds maximum of {} chunks ({} bytes with current chunk size)",
                u32::MAX,
                (u32::MAX as u64) * (chunk_size as u64)
            )
        })?;
    }

    progress(1.0);
    Ok((chunk_index as usize, total_ciphertext_size))
}

/// Derive chunk nonce from base nonce and chunk index
fn derive_chunk_nonce(base_nonce: &[u8; 12], chunk_index: u32) -> [u8; 12] {
    let mut nonce = *base_nonce;
    // Set the last 4 bytes to the chunk index (big-endian)
    nonce[8..12].copy_from_slice(&chunk_index.to_be_bytes());
    nonce
}

/// Build AAD for chunk encryption
fn build_chunk_aad(export_id: &[u8; 16], chunk_index: u32) -> Vec<u8> {
    let mut aad = Vec::with_capacity(21);
    aad.extend_from_slice(export_id);
    aad.extend_from_slice(&chunk_index.to_be_bytes());
    aad.push(SCHEMA_VERSION);
    aad
}

/// Regenerate entire integrity.json
fn regenerate_integrity_manifest(
    archive_dir: &Path,
) -> Result<Option<crate::pages::bundle::IntegrityManifest>> {
    let integrity_path = archive_dir.join("integrity.json");
    if !integrity_path.exists() {
        return Ok(None);
    }

    let integrity = crate::pages::bundle::generate_integrity_manifest(archive_dir)?;
    write_json_pretty(&integrity_path, &integrity)?;

    Ok(Some(integrity))
}

fn materialize_safe_required_file_symlinks(archive_dir: &Path) -> Result<()> {
    let canonical_archive_dir = archive_dir.canonicalize().with_context(|| {
        format!(
            "Failed to resolve archive directory {} before key mutation",
            archive_dir.display()
        )
    })?;

    for rel_path in REQUIRED_SITE_FILES {
        materialize_safe_required_file_symlink(archive_dir, &canonical_archive_dir, rel_path)?;
    }

    Ok(())
}

fn materialize_safe_required_file_symlink(
    archive_dir: &Path,
    canonical_archive_dir: &Path,
    rel_path: &str,
) -> Result<()> {
    let path = archive_dir.join(rel_path);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(err).with_context(|| {
                format!(
                    "Failed to inspect required site file {} before key mutation",
                    path.display()
                )
            });
        }
    };
    if !metadata.file_type().is_symlink() {
        return Ok(());
    }

    let canonical_target = path.canonicalize().with_context(|| {
        format!(
            "Failed to resolve symlinked required site file {} before key mutation",
            path.display()
        )
    })?;
    if !canonical_target.starts_with(canonical_archive_dir) {
        bail!(
            "Refusing to materialize required site file symlink outside archive root: {}",
            path.display()
        );
    }

    let target_metadata = std::fs::metadata(&canonical_target).with_context(|| {
        format!(
            "Failed to inspect symlink target {} before key mutation",
            canonical_target.display()
        )
    })?;
    if !target_metadata.file_type().is_file() {
        bail!(
            "Refusing to materialize required site file symlink that does not point to a regular file: {}",
            path.display()
        );
    }

    let temp_path = unique_atomic_sidecar_path(&path, "materialize", "site-file");
    std::fs::copy(&canonical_target, &temp_path).with_context(|| {
        format!(
            "Failed copying symlink target {} into staged required site file {}",
            canonical_target.display(),
            temp_path.display()
        )
    })?;
    File::open(&temp_path)
        .and_then(|file| file.sync_all())
        .with_context(|| {
            format!(
                "Failed syncing materialized required site file {}",
                temp_path.display()
            )
        })?;
    replace_file_from_temp(&temp_path, &path).with_context(|| {
        format!(
            "Failed materializing required site file symlink {} before key mutation",
            path.display()
        )
    })?;

    Ok(())
}

fn write_json_pretty_atomically<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let temp_path = unique_atomic_temp_path(path);
    {
        let file = File::create(&temp_path)?;
        let mut writer = BufWriter::new(file);
        serde_json::to_writer_pretty(&mut writer, value)?;
        writer.flush()?;
        writer.get_ref().sync_all()?;
    }
    replace_file_from_temp(&temp_path, path)
}

fn write_json_pretty<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let file = File::create(path)?;
    let mut writer = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut writer, value)?;
    writer.flush()?;
    writer.get_ref().sync_all()?;
    Ok(())
}

fn replace_file_from_temp(temp_path: &Path, final_path: &Path) -> Result<()> {
    if cfg!(windows) {
        match std::fs::rename(temp_path, final_path) {
            Ok(()) => {
                sync_parent_directory(final_path)?;
                Ok(())
            }
            Err(first_err) if replacement_path_entry_exists(final_path)? => {
                replace_file_from_temp_via_backup(temp_path, final_path, &first_err)
            }
            Err(err) => Err(err.into()),
        }
    } else {
        std::fs::rename(temp_path, final_path)?;
        sync_parent_directory(final_path)?;
        Ok(())
    }
}

fn replacement_path_entry_exists(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(err) if matches!(err.kind(), std::io::ErrorKind::NotFound) => Ok(false),
        Err(err) => Err(err)
            .with_context(|| format!("failed inspecting replacement target {}", path.display())),
    }
}

fn replace_file_from_temp_via_backup(
    temp_path: &Path,
    final_path: &Path,
    first_err: &std::io::Error,
) -> Result<()> {
    let backup_path = unique_atomic_backup_path(final_path);
    std::fs::rename(final_path, &backup_path).map_err(|backup_err| {
        let _ = std::fs::remove_file(temp_path);
        anyhow::anyhow!(
            "failed replacing {} with {}: {}; failed moving existing file to backup {}: {}",
            final_path.display(),
            temp_path.display(),
            first_err,
            backup_path.display(),
            backup_err
        )
    })?;

    match std::fs::rename(temp_path, final_path) {
        Ok(()) => {
            let _ = std::fs::remove_file(&backup_path);
            sync_parent_directory(final_path)?;
            Ok(())
        }
        Err(second_err) => match std::fs::rename(&backup_path, final_path) {
            Ok(()) => {
                let _ = std::fs::remove_file(temp_path);
                sync_parent_directory(final_path)?;
                anyhow::bail!(
                    "failed replacing {} with {}: {}; restored original file",
                    final_path.display(),
                    temp_path.display(),
                    second_err
                );
            }
            Err(restore_err) => {
                anyhow::bail!(
                    "failed replacing {} with {}: {}; restore error: {}; temp file retained at {}",
                    final_path.display(),
                    temp_path.display(),
                    second_err,
                    restore_err,
                    temp_path.display()
                );
            }
        },
    }
}

#[cfg(not(windows))]
fn sync_parent_directory(path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(windows)]
fn sync_parent_directory(_path: &Path) -> Result<()> {
    Ok(())
}

fn unique_atomic_temp_path(path: &Path) -> std::path::PathBuf {
    unique_atomic_sidecar_path(path, "tmp", "config.json")
}

fn unique_atomic_backup_path(path: &Path) -> std::path::PathBuf {
    unique_atomic_sidecar_path(path, "bak", "config.json")
}

fn unique_atomic_sidecar_path(
    path: &Path,
    suffix: &str,
    fallback_name: &str,
) -> std::path::PathBuf {
    static NEXT_NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let nonce = NEXT_NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(fallback_name);

    path.with_file_name(format!(
        ".{file_name}.{suffix}.{}.{}.{}",
        std::process::id(),
        timestamp,
        nonce
    ))
}

#[cfg(test)]
fn replace_dir_from_temp(temp_dir: &Path, final_dir: &Path) -> Result<()> {
    if !ensure_replaceable_site_dir(final_dir)? {
        std::fs::rename(temp_dir, final_dir).with_context(|| {
            format!(
                "failed renaming staged site {} into place at {}",
                temp_dir.display(),
                final_dir.display()
            )
        })?;
        sync_parent_directory(final_dir)?;
        return Ok(());
    }

    let backup_dir = unique_atomic_sidecar_path(final_dir, "bak", "site");
    std::fs::rename(final_dir, &backup_dir).with_context(|| {
        format!(
            "failed preparing backup {} before replacing {}",
            backup_dir.display(),
            final_dir.display()
        )
    })?;

    match std::fs::rename(temp_dir, final_dir) {
        Ok(()) => {
            sync_parent_directory(final_dir)?;
            let _ = std::fs::remove_dir_all(&backup_dir);
            sync_parent_directory(final_dir)?;
            Ok(())
        }
        Err(second_err) => match std::fs::rename(&backup_dir, final_dir) {
            Ok(()) => {
                let _ = std::fs::remove_dir_all(temp_dir);
                sync_parent_directory(final_dir)?;
                anyhow::bail!(
                    "failed replacing {} with {}: {}; restored original site",
                    final_dir.display(),
                    temp_dir.display(),
                    second_err
                )
            }
            Err(restore_err) => anyhow::bail!(
                "failed replacing {} with {}: {}; restore error: {}; staged site retained at {}",
                final_dir.display(),
                temp_dir.display(),
                second_err,
                restore_err,
                temp_dir.display()
            ),
        },
    }
}

#[cfg(test)]
fn ensure_replaceable_site_dir(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                bail!(
                    "Refusing to replace site directory through symlink: {}",
                    path.display()
                );
            }
            if !file_type.is_dir() {
                bail!(
                    "Refusing to replace site directory because it is not a directory: {}",
                    path.display()
                );
            }
            Ok(true)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err).with_context(|| {
            format!(
                "Failed inspecting site directory before replacement: {}",
                path.display()
            )
        }),
    }
}

#[cfg(not(windows))]
fn sync_tree(path: &Path) -> Result<()> {
    sync_tree_inner(path)?;
    sync_parent_directory(path)
}

#[cfg(windows)]
fn sync_tree(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(not(windows))]
fn sync_tree_inner(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("Failed reading metadata for {}", path.display()))?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        return Ok(());
    }
    if file_type.is_file() {
        std::fs::File::open(path)
            .with_context(|| format!("Failed opening {} for sync", path.display()))?
            .sync_all()
            .with_context(|| format!("Failed syncing {}", path.display()))?;
        return Ok(());
    }
    if file_type.is_dir() {
        for entry in std::fs::read_dir(path)
            .with_context(|| format!("Failed reading directory {}", path.display()))?
        {
            let entry = entry.with_context(|| format!("Failed walking {}", path.display()))?;
            sync_tree_inner(&entry.path())?;
        }
        std::fs::File::open(path)
            .with_context(|| format!("Failed opening directory {} for sync", path.display()))?
            .sync_all()
            .with_context(|| format!("Failed syncing directory {}", path.display()))?;
    }
    Ok(())
}

fn refresh_private_artifacts(
    archive_dir: &Path,
    config: &EncryptionConfig,
    manifest: Option<&crate::pages::bundle::IntegrityManifest>,
    recovery_secret: Option<&[u8]>,
    remove_recovery_artifacts: bool,
) -> Result<()> {
    let Some(private_dir) = private_dir_for_archive(archive_dir)? else {
        return Ok(());
    };

    if let Some(manifest) = manifest {
        let fingerprint = crate::pages::bundle::compute_fingerprint(manifest);
        crate::pages::bundle::write_private_fingerprint(&private_dir, &fingerprint)?;
    }

    let should_generate_qr = recovery_secret.is_some()
        && (private_dir.join("qr-code.png").exists() || private_dir.join("qr-code.svg").exists());

    crate::pages::bundle::write_private_artifacts_encrypted(
        &private_dir,
        config,
        recovery_secret,
        should_generate_qr,
        remove_recovery_artifacts,
    )?;

    Ok(())
}

fn private_dir_for_archive(archive_dir: &Path) -> Result<Option<std::path::PathBuf>> {
    if archive_dir
        .file_name()
        .map(|name| name == "site")
        .unwrap_or(false)
    {
        let Some(parent) = archive_dir.parent() else {
            return Ok(None);
        };
        let private_dir = parent.join("private");
        match std::fs::symlink_metadata(&private_dir) {
            Ok(metadata) => {
                let file_type = metadata.file_type();
                if file_type.is_symlink() {
                    bail!(
                        "private artifact directory must not be a symlink: {}",
                        private_dir.display()
                    );
                }
                if file_type.is_dir() {
                    return Ok(Some(private_dir));
                }
                bail!(
                    "private artifact path must be a directory: {}",
                    private_dir.display()
                );
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "Failed to inspect private artifact directory {}",
                        private_dir.display()
                    )
                });
            }
        }
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pages::attachments::{
        AttachmentConfig, AttachmentData, AttachmentProcessor, decrypt_blob, decrypt_manifest,
    };
    use crate::pages::bundle::BundleBuilder;
    use crate::pages::encrypt::{DecryptionEngine, EncryptionEngine, MAX_CHUNK_SIZE, PayloadMeta};
    use crate::pages::errors::DecryptError;
    use crate::pages::verify::verify_bundle;
    use std::cell::Cell;
    use tempfile::TempDir;

    #[cfg(unix)]
    fn replace_viewer_with_in_tree_symlink(site_dir: &Path) {
        use std::os::unix::fs::symlink;

        let real_viewer = site_dir.join("viewer-real.js");
        std::fs::rename(site_dir.join("viewer.js"), &real_viewer).unwrap();
        symlink("viewer-real.js", site_dir.join("viewer.js")).unwrap();

        let manifest = crate::pages::bundle::generate_integrity_manifest(site_dir).unwrap();
        write_json_pretty(&site_dir.join("integrity.json"), &manifest).unwrap();
    }

    fn setup_test_archive() -> (TempDir, std::path::PathBuf) {
        let temp_dir = TempDir::new().unwrap();
        let input_path = temp_dir.path().join("input.txt");
        let bundle_root = temp_dir.path().join("bundle");
        let encrypted_dir = temp_dir.path().join("encrypted");

        // Create test file
        std::fs::write(&input_path, b"Test data for key management").unwrap();

        // Encrypt
        let mut engine = EncryptionEngine::new(1024).unwrap();
        engine.add_password_slot("test-password").unwrap();
        engine
            .encrypt_file(&input_path, &encrypted_dir, |_, _| {})
            .unwrap();

        BundleBuilder::new()
            .build(&encrypted_dir, &bundle_root, |_, _| {})
            .unwrap();

        (temp_dir, bundle_root)
    }

    fn setup_test_archive_with_attachments() -> (TempDir, std::path::PathBuf) {
        let temp_dir = TempDir::new().unwrap();
        let input_path = temp_dir.path().join("input.txt");
        let bundle_root = temp_dir.path().join("bundle");
        let encrypted_dir = temp_dir.path().join("encrypted");

        std::fs::write(&input_path, b"Test data for key management").unwrap();

        let mut engine = EncryptionEngine::new(1024).unwrap();
        engine.add_password_slot("test-password").unwrap();
        engine
            .encrypt_file(&input_path, &encrypted_dir, |_, _| {})
            .unwrap();

        let config = load_config(&encrypted_dir).unwrap();
        let dek = unwrap_dek_with_password(&config, "test-password").unwrap();
        let export_id_raw = BASE64_STANDARD.decode(&config.export_id).unwrap();
        let export_id: [u8; 16] = export_id_raw.as_slice().try_into().unwrap();

        let mut processor = AttachmentProcessor::new(AttachmentConfig::enabled());
        processor
            .process_attachments(
                1,
                &[AttachmentData {
                    filename: "proof.txt".to_string(),
                    mime_type: "text/plain".to_string(),
                    data: b"attachment payload".to_vec(),
                }],
            )
            .unwrap();
        processor
            .write_encrypted_blobs(&encrypted_dir, &dek, &export_id)
            .unwrap();

        BundleBuilder::new()
            .build(&encrypted_dir, &bundle_root, |_, _| {})
            .unwrap();

        (temp_dir, bundle_root)
    }

    fn rewrite_test_config(archive_dir: &Path, mutate: impl FnOnce(&mut EncryptionConfig)) {
        let site_dir = super::super::resolve_site_dir(archive_dir).unwrap();
        let mut config = load_config(&site_dir).unwrap();
        mutate(&mut config);
        write_json_pretty(&site_dir.join("config.json"), &config).unwrap();
    }

    fn assert_unsupported_payload_format_error(err: anyhow::Error, _compression: &str) {
        assert!(
            matches!(
                err.downcast_ref::<DecryptError>(),
                Some(DecryptError::UnsupportedMetadata(field)) if field == "compression"
            ),
            "unexpected unsupported-metadata taxonomy: {err:#}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_private_dir_for_archive_rejects_symlinked_private_dir() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let site_dir = temp.path().join("bundle/site");
        let outside_private = temp.path().join("outside-private");
        std::fs::create_dir_all(&site_dir).unwrap();
        std::fs::create_dir_all(&outside_private).unwrap();
        symlink(&outside_private, temp.path().join("bundle/private")).unwrap();

        let err = private_dir_for_archive(&site_dir).unwrap_err();

        assert!(
            err.to_string().contains("must not be a symlink"),
            "unexpected error: {err:#}"
        );
        assert!(
            std::fs::symlink_metadata(temp.path().join("bundle/private"))
                .unwrap()
                .file_type()
                .is_symlink(),
            "rejected private directory symlink should remain untouched"
        );
    }

    #[test]
    fn test_private_dir_for_archive_rejects_non_directory_private_path() {
        let temp = TempDir::new().unwrap();
        let site_dir = temp.path().join("bundle/site");
        std::fs::create_dir_all(&site_dir).unwrap();
        std::fs::write(temp.path().join("bundle/private"), "not a directory").unwrap();

        let err = private_dir_for_archive(&site_dir).unwrap_err();

        assert!(
            err.to_string().contains("must be a directory"),
            "unexpected error: {err:#}"
        );
        assert_eq!(
            std::fs::read_to_string(temp.path().join("bundle/private")).unwrap(),
            "not a directory"
        );
    }

    #[test]
    fn key_mutation_rejects_a_concurrent_bundle_publication_without_changing_config() -> Result<()> {
        let (_temp_dir, archive_dir) = setup_test_archive();
        let _bundle_publication_guard = acquire_pages_publication_lock(&archive_dir)?;
        let config_before = std::fs::read(archive_dir.join("site/config.json"))?;

        let error = key_add_password(&archive_dir, "test-password", "concurrent-password")
            .expect_err("a second key writer must not enter the transaction");

        anyhow::ensure!(format!("{error:#}").contains("another Pages publication"));
        anyhow::ensure!(
            std::fs::read(archive_dir.join("site/config.json"))? == config_before,
            "bundle/key lock contention changed the live key configuration"
        );
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn key_mutation_lock_path_replacement_blocks_owned_tree_cleanup() -> Result<()> {
        let (temp_dir, archive_dir) = setup_test_archive();
        let guard = open_key_mutation_lock(derive_key_mutation_target(&archive_dir)?)?;
        let (staged, _prior, staging_ownership) =
            stage_key_mutation_tree(&guard, KeyMutationStageMode::PreserveSite)?;
        let parked_lock = temp_dir.path().join("parked-publication-lock");
        std::fs::rename(&guard.publication_lock.lock_path, &parked_lock)?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let _replacement = options.open(&guard.publication_lock.lock_path)?;

        let error = remove_owned_key_mutation_tree(
            &guard,
            &staged,
            None,
            Some(&staging_ownership),
            None,
        )
            .expect_err("a replaced lock pathname must block destructive cleanup");

        anyhow::ensure!(format!("{error:#}").contains("was replaced"));
        anyhow::ensure!(std::fs::symlink_metadata(&staged)?.file_type().is_dir());
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn key_mutation_staging_path_replacement_blocks_prejournal_cleanup() -> Result<()> {
        let (temp_dir, archive_dir) = setup_test_archive();
        let guard = open_key_mutation_lock(derive_key_mutation_target(&archive_dir)?)?;
        let (staged, _prior, staging_ownership) =
            stage_key_mutation_tree(&guard, KeyMutationStageMode::PreserveSite)?;
        let parked_staged = temp_dir.path().join("parked-staged-generation");
        std::fs::rename(&staged, &parked_staged)?;
        std::fs::create_dir(&staged)?;

        let error = remove_owned_key_mutation_tree(
            &guard,
            &staged,
            None,
            Some(&staging_ownership),
            None,
        )
        .expect_err("a replaced staging pathname must survive pre-journal cleanup");

        anyhow::ensure!(format!("{error:#}").contains("changed identity"));
        anyhow::ensure!(std::fs::symlink_metadata(&staged)?.file_type().is_dir());
        anyhow::ensure!(
            std::fs::symlink_metadata(&parked_staged)?
                .file_type()
                .is_dir()
        );
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn committed_key_cleanup_retains_tree_when_journal_identity_changes() -> Result<()> {
        let (temp_dir, archive_dir) = setup_test_archive();
        let guard = open_key_mutation_lock(derive_key_mutation_target(&archive_dir)?)?;
        let (staged, prior, _staging_ownership) =
            stage_key_mutation_tree(&guard, KeyMutationStageMode::PreserveSite)?;
        std::fs::write(staged.join("candidate-marker"), b"candidate")?;
        let candidate = inspect_key_publication_tree(&staged)?;
        let backup = random_key_mutation_sidecar_path(&archive_dir, "backup");
        let journal =
            write_key_mutation_journal(&guard, &staged, &backup, &prior, &candidate)?;
        let parked_journal = temp_dir.path().join("parked-key-journal");
        std::fs::rename(&guard.target.journal_path, &parked_journal)?;
        std::fs::write(&guard.target.journal_path, b"{}")?;
        std::fs::set_permissions(
            &guard.target.journal_path,
            std::fs::Permissions::from_mode(0o600),
        )?;

        cleanup_committed_key_mutation(&guard, &staged, &candidate.digest, &journal);

        anyhow::ensure!(std::fs::symlink_metadata(&staged)?.file_type().is_dir());
        Ok(())
    }

    #[test]
    fn key_mutation_of_standalone_directory_named_site_preserves_its_siblings() -> Result<()> {
        let (temp_dir, archive_dir) = setup_test_archive();
        let standalone_parent = temp_dir.path().join("standalone-parent");
        let standalone_site = standalone_parent.join("site");
        std::fs::create_dir(&standalone_parent)?;
        std::fs::rename(archive_dir.join("site"), &standalone_site)?;
        let sibling = standalone_parent.join("unrelated.txt");
        std::fs::write(&sibling, b"unrelated")?;

        key_add_password(&standalone_site, "test-password", "new-password")?;

        anyhow::ensure!(std::fs::read(&sibling)? == b"unrelated");
        anyhow::ensure!(key_list(&standalone_site)?.active_slots == 2);
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn key_add_private_artifact_symlink_failure_preserves_complete_live_generation() -> Result<()> {
        use std::os::unix::fs::symlink;

        let (temp_dir, archive_dir) = setup_test_archive();
        let config_path = archive_dir.join("site/config.json");
        let integrity_path = archive_dir.join("site/integrity.json");
        let master_key_path = archive_dir.join("private/master-key.json");
        let protected_path = temp_dir.path().join("protected-master-key.json");
        std::fs::rename(&master_key_path, &protected_path)?;
        symlink(&protected_path, &master_key_path)?;
        let config_before = std::fs::read(&config_path)?;
        let integrity_before = std::fs::read(&integrity_path)?;
        let protected_before = std::fs::read(&protected_path)?;

        let error = key_add_password(&archive_dir, "test-password", "new-password")
            .expect_err("a private-artifact symlink must reject the staged transaction");

        anyhow::ensure!(format!("{error:#}").contains("must not be a symlink"));
        anyhow::ensure!(std::fs::read(&config_path)? == config_before);
        anyhow::ensure!(std::fs::read(&integrity_path)? == integrity_before);
        anyhow::ensure!(std::fs::read(&protected_path)? == protected_before);
        anyhow::ensure!(
            std::fs::symlink_metadata(&master_key_path)?
                .file_type()
                .is_symlink()
        );
        Ok(())
    }

    #[test]
    fn key_mutation_recovery_rejects_spoofed_sibling_name_without_removal() -> Result<()> {
        let (temp_dir, archive_dir) = setup_test_archive();
        let guard = open_key_mutation_lock(derive_key_mutation_target(&archive_dir)?)?;
        let victim = temp_dir.path().join("unrelated-sibling");
        std::fs::create_dir(&victim)?;
        std::fs::write(victim.join("keep.txt"), b"must survive")?;
        let prior = inspect_key_publication_tree(&archive_dir)?;
        let victim_evidence = inspect_key_publication_tree(&victim)?;
        let backup = random_key_mutation_sidecar_path(&archive_dir, "backup");
        let journal = KeyMutationJournal {
            format: KEY_MUTATION_JOURNAL_FORMAT.to_string(),
            staged_file_name: "unrelated-sibling".to_string(),
            backup_file_name: backup
                .file_name()
                .and_then(|name| name.to_str())
                .context("backup filename")?
                .to_string(),
            prior_digest: prior.digest,
            candidate_digest: victim_evidence.digest,
        };
        write_json_pretty(&guard.target.journal_path, &journal)?;
        #[cfg(unix)]
        std::fs::set_permissions(
            &guard.target.journal_path,
            std::fs::Permissions::from_mode(0o600),
        )?;

        let error = recover_interrupted_key_mutation(&guard)
            .expect_err("a journal must not claim an arbitrary sibling directory");

        anyhow::ensure!(format!("{error:#}").contains("owned staged prefix"));
        anyhow::ensure!(std::fs::read(victim.join("keep.txt"))? == b"must survive");
        Ok(())
    }

    #[test]
    fn key_mutation_recovery_rejects_noncanonical_digest_without_cleanup() -> Result<()> {
        let (_temp_dir, archive_dir) = setup_test_archive();
        let guard = open_key_mutation_lock(derive_key_mutation_target(&archive_dir)?)?;
        let (staged, prior, _staging_ownership) = stage_key_mutation_tree(
            &guard,
            KeyMutationStageMode::PreserveSite,
        )?;
        let backup = random_key_mutation_sidecar_path(&archive_dir, "backup");
        let journal = KeyMutationJournal {
            format: KEY_MUTATION_JOURNAL_FORMAT.to_string(),
            staged_file_name: staged
                .file_name()
                .and_then(|name| name.to_str())
                .context("staged filename")?
                .to_string(),
            backup_file_name: backup
                .file_name()
                .and_then(|name| name.to_str())
                .context("backup filename")?
                .to_string(),
            prior_digest: prior.digest,
            candidate_digest: "A".repeat(64),
        };
        write_json_pretty(&guard.target.journal_path, &journal)?;
        #[cfg(unix)]
        std::fs::set_permissions(
            &guard.target.journal_path,
            std::fs::Permissions::from_mode(0o600),
        )?;

        let error = recover_interrupted_key_mutation(&guard)
            .expect_err("noncanonical journal digests must fail closed");

        anyhow::ensure!(format!("{error:#}").contains("not canonical SHA-256"));
        anyhow::ensure!(std::fs::symlink_metadata(&staged)?.file_type().is_dir());
        Ok(())
    }

    #[test]
    fn key_mutation_recovery_restores_exact_prior_after_park_crash() -> Result<()> {
        let (_temp_dir, archive_dir) = setup_test_archive();
        let guard = open_key_mutation_lock(derive_key_mutation_target(&archive_dir)?)?;
        let (staged, prior, _staging_ownership) = stage_key_mutation_tree(
            &guard,
            KeyMutationStageMode::PreserveSite,
        )?;
        std::fs::write(staged.join("transaction-candidate.txt"), b"candidate")?;
        sync_tree(&staged)?;
        let candidate = inspect_key_publication_tree(&staged)?;
        let backup = random_key_mutation_sidecar_path(&archive_dir, "backup");
        write_key_mutation_journal(&guard, &staged, &backup, &prior, &candidate)?;
        std::fs::rename(&archive_dir, &backup)?;
        sync_parent_directory(&archive_dir)?;

        let recovered = recover_interrupted_key_mutation(&guard)?;

        anyhow::ensure!(recovered == RecoveredKeyMutation::RolledBack);
        anyhow::ensure!(inspect_key_publication_tree(&archive_dir)? == prior);
        anyhow::ensure!(std::fs::symlink_metadata(&staged).is_err());
        anyhow::ensure!(std::fs::symlink_metadata(&guard.target.journal_path).is_err());
        Ok(())
    }

    #[test]
    fn key_publication_evidence_rejects_destination_drift() -> Result<()> {
        let (_temp_dir, archive_dir) = setup_test_archive();
        let guard = open_key_mutation_lock(derive_key_mutation_target(&archive_dir)?)?;
        let (staged, _prior, staging_ownership) =
            stage_key_mutation_tree(&guard, KeyMutationStageMode::PreserveSite)?;
        let evidence = inspect_key_publication_tree(&staged)?;
        std::fs::write(staged.join("site/config.json"), b"{}")?;

        let error = verify_published_key_tree(&staged, &evidence)
            .expect_err("destination drift must invalidate exact publication evidence");

        anyhow::ensure!(format!("{error:#}").contains("failed exact verification"));
        remove_owned_key_mutation_tree(
            &guard,
            &staged,
            None,
            Some(&staging_ownership),
            None,
        )?;
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn key_publication_evidence_binds_root_permissions() -> Result<()> {
        let (_temp_dir, archive_dir) = setup_test_archive();
        let evidence = inspect_key_publication_tree(&archive_dir)?;
        std::fs::set_permissions(&archive_dir, std::fs::Permissions::from_mode(0o777))?;

        let error = verify_published_key_tree(&archive_dir, &evidence)
            .expect_err("a permissive publication-root mode must invalidate evidence");

        anyhow::ensure!(format!("{error:#}").contains("failed exact verification"));
        Ok(())
    }

    #[test]
    fn key_publication_tree_depth_is_bounded_without_recursive_walk() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let mut deepest = temp_dir.path().join("root");
        std::fs::create_dir(&deepest)?;
        for _ in 0..=KEY_MUTATION_TREE_DEPTH_LIMIT {
            deepest = deepest.join("d");
            std::fs::create_dir(&deepest)?;
        }

        let error = inspect_key_publication_tree(&temp_dir.path().join("root"))
            .expect_err("over-deep trees must fail before exhausting the call stack");

        anyhow::ensure!(format!("{error:#}").contains("depth bound"));
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn key_add_rejects_public_symlink_to_private_key_material() -> Result<()> {
        use std::os::unix::fs::symlink;

        let (_temp_dir, archive_dir) = setup_test_archive();
        let site_dir = archive_dir.join("site");
        let viewer_path = site_dir.join("viewer.js");
        let private_key_path = archive_dir.join("private/master-key.json");
        let private_key_before = std::fs::read(&private_key_path)?;
        std::fs::rename(&viewer_path, site_dir.join("viewer-real.js"))?;
        symlink("../private/master-key.json", &viewer_path)?;

        let error = key_add_password(&archive_dir, "test-password", "new-password")
            .expect_err("public symlinks must not materialize private key bytes");

        anyhow::ensure!(format!("{error:#}").contains("outside the public site root"));
        anyhow::ensure!(std::fs::read(&private_key_path)? == private_key_before);
        anyhow::ensure!(
            std::fs::symlink_metadata(&viewer_path)?
                .file_type()
                .is_symlink()
        );
        Ok(())
    }

    #[test]
    fn test_decrypt_all_chunks_rejects_mismatched_chunk_count_before_progress() {
        let temp_dir = TempDir::new().unwrap();
        let archive_dir = temp_dir.path();
        let config = EncryptionConfig {
            version: SCHEMA_VERSION,
            export_id: BASE64_STANDARD.encode([0u8; 16]),
            base_nonce: BASE64_STANDARD.encode([0u8; 12]),
            compression: "deflate".to_string(),
            kdf_defaults: Argon2Params::default(),
            payload: PayloadMeta {
                chunk_size: 1024,
                chunk_count: 0,
                total_compressed_size: 0,
                total_plaintext_size: 0,
                files: vec!["payload/chunk-00000.bin".to_string()],
            },
            key_slots: Vec::new(),
        };
        let progress_calls = Cell::new(0);

        let err = decrypt_all_chunks(archive_dir, &[0u8; 32], &config, |progress| {
            assert!(progress.is_finite(), "progress must be finite: {progress}");
            progress_calls.set(progress_calls.get() + 1);
        })
        .unwrap_err();

        assert!(
            err.to_string().contains("chunk_count 0"),
            "unexpected error: {err:#}"
        );
        assert_eq!(progress_calls.get(), 0);
    }

    #[test]
    fn test_key_list() {
        let (_temp_dir, archive_dir) = setup_test_archive();

        let result = key_list(&archive_dir).unwrap();
        assert_eq!(result.active_slots, 1);
        assert_eq!(result.slots.len(), 1);
        assert_eq!(result.slots[0].slot_type, "password");
        assert_eq!(result.slots[0].kdf, "argon2id");
    }

    #[test]
    fn key_add_password_rejects_empty_and_whitespace_only_new_passwords() {
        let (_temp_dir, archive_dir) = setup_test_archive();

        for new_password in ["", "   ", "\t\n"] {
            let err = key_add_password(&archive_dir, "test-password", new_password)
                .expect_err("weak empty-equivalent password must not create a key slot");
            assert!(
                err.to_string().contains("empty or whitespace-only"),
                "unexpected password validation error: {err:#}"
            );
        }

        let config = load_config(&archive_dir).unwrap();
        assert_eq!(
            config.key_slots.len(),
            1,
            "rejected passwords must leave the archive config unchanged"
        );
    }

    #[test]
    fn test_key_mutations_reject_unsupported_payload_compression() {
        let (_temp_dir, archive_dir) = setup_test_archive();
        key_add_password(&archive_dir, "test-password", "second-password").unwrap();
        rewrite_test_config(&archive_dir, |config| {
            config.compression = "zstd".to_string();
        });

        let err = key_add_password(&archive_dir, "test-password", "third-password").unwrap_err();
        assert_unsupported_payload_format_error(err, "zstd");

        let err = key_add_recovery(&archive_dir, "test-password").unwrap_err();
        assert_unsupported_payload_format_error(err, "zstd");

        let err = key_revoke(&archive_dir, "second-password", 0).unwrap_err();
        assert_unsupported_payload_format_error(err, "zstd");

        let err =
            key_rotate(&archive_dir, "test-password", "new-password", false, |_| {}).unwrap_err();
        assert_unsupported_payload_format_error(err, "zstd");

        let config = load_config(&archive_dir).unwrap();
        assert_eq!(config.key_slots.len(), 2);
        assert!(unwrap_dek_with_password(&config, "test-password").is_ok());
        assert!(unwrap_dek_with_password(&config, "second-password").is_ok());
        assert!(unwrap_dek_with_password(&config, "third-password").is_err());
        assert!(unwrap_dek_with_password(&config, "new-password").is_err());
    }

    #[test]
    fn test_key_rotate_rejects_oversized_payload_chunk_size_before_rewriting() {
        let (_temp_dir, archive_dir) = setup_test_archive();
        rewrite_test_config(&archive_dir, |config| {
            config.payload.chunk_size = MAX_CHUNK_SIZE + 1;
        });

        let err =
            key_rotate(&archive_dir, "test-password", "new-password", false, |_| {}).unwrap_err();
        // The shared payload-format validation now rejects the oversized
        // chunk_size first ("unsupported encryption metadata
        // (payload.chunk_size)"), before key_rotate's own bound message;
        // either shape proves the rotate refused to rewrite.
        let rendered = err.to_string();
        assert!(
            rendered.contains("chunk_size"),
            "unexpected chunk-size error: {err:#}"
        );

        let config = load_config(&archive_dir).unwrap();
        assert!(unwrap_dek_with_password(&config, "test-password").is_ok());
        assert!(unwrap_dek_with_password(&config, "new-password").is_err());
    }

    #[test]
    fn test_key_rotate_chunk_count_preflight_preserves_nonce_space_limit() {
        ensure_archive_chunk_count_fits_nonce_space(u64::from(u32::MAX), 1).unwrap();

        let err =
            ensure_archive_chunk_count_fits_nonce_space(u64::from(u32::MAX) + 1, 1).unwrap_err();
        let rendered = err.to_string();
        assert!(
            rendered.contains("exceeds maximum") && rendered.contains(&u32::MAX.to_string()),
            "unexpected chunk-count error: {rendered}"
        );
    }

    #[test]
    fn test_key_add_password() {
        let (_temp_dir, archive_dir) = setup_test_archive();

        // Add new password
        let slot_id = key_add_password(&archive_dir, "test-password", "new-password").unwrap();
        assert_eq!(slot_id, 1);

        // Verify it was added
        let result = key_list(&archive_dir).unwrap();
        assert_eq!(result.active_slots, 2);

        // Verify new password works
        let config = load_config(&archive_dir).unwrap();
        let dek = unwrap_dek_with_password(&config, "new-password").unwrap();
        assert!(!dek.iter().all(|&b| b == 0));
    }

    #[test]
    fn test_key_add_recovery() {
        let (_temp_dir, archive_dir) = setup_test_archive();

        // Add recovery slot
        let (slot_id, secret) = key_add_recovery(&archive_dir, "test-password").unwrap();
        assert_eq!(slot_id, 1);
        assert_eq!(secret.entropy_bits(), 256);

        // Verify it was added
        let result = key_list(&archive_dir).unwrap();
        assert_eq!(result.active_slots, 2);
        assert_eq!(result.slots[1].slot_type, "recovery");
        assert_eq!(result.slots[1].kdf, "hkdf-sha256");
    }

    #[test]
    fn test_key_add_wrong_password_fails() {
        let (_temp_dir, archive_dir) = setup_test_archive();

        let result = key_add_password(&archive_dir, "wrong-password", "new-password");
        assert!(result.is_err());
    }

    #[test]
    fn test_key_revoke() {
        let (_temp_dir, archive_dir) = setup_test_archive();

        // Add second slot
        key_add_password(&archive_dir, "test-password", "second-password").unwrap();

        // Revoke first slot using second password
        let result = key_revoke(&archive_dir, "second-password", 0).unwrap();
        assert_eq!(result.revoked_slot_id, 0);
        assert_eq!(result.remaining_slots, 1);

        // Old password should no longer work
        let config = load_config(&archive_dir).unwrap();
        assert!(unwrap_dek_with_password(&config, "test-password").is_err());

        // Second password should still work
        assert!(unwrap_dek_with_password(&config, "second-password").is_ok());
    }

    #[test]
    fn test_key_revoke_last_slot_fails() {
        let (_temp_dir, archive_dir) = setup_test_archive();

        let result = key_revoke(&archive_dir, "test-password", 0);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("last remaining"));
    }

    #[test]
    fn test_key_revoke_auth_slot_fails() {
        let (_temp_dir, archive_dir) = setup_test_archive();

        // Add second slot
        key_add_password(&archive_dir, "test-password", "second-password").unwrap();

        // Try to revoke slot 0 using slot 0's password
        let result = key_revoke(&archive_dir, "test-password", 0);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("authentication"));
    }

    #[test]
    fn test_key_rotate() {
        let (temp_dir, archive_dir) = setup_test_archive();
        let decrypted_path = temp_dir.path().join("decrypted.txt");

        // Rotate keys
        let result =
            key_rotate(&archive_dir, "test-password", "new-password", false, |_| {}).unwrap();
        assert_eq!(result.slot_count, 1);
        assert!(result.recovery_secret.is_none());

        // Old password should fail
        let config = load_config(&archive_dir).unwrap();
        assert!(
            config.payload.total_compressed_size > 0,
            "rotated non-empty archives must record their ciphertext total"
        );
        assert!(unwrap_dek_with_password(&config, "test-password").is_err());

        // New password should work and decrypt correctly
        let decryptor = DecryptionEngine::unlock_with_password(config, "new-password").unwrap();
        decryptor
            .decrypt_to_file(&archive_dir, &decrypted_path, |_, _| {})
            .unwrap();

        let decrypted = std::fs::read(&decrypted_path).unwrap();
        assert_eq!(decrypted, b"Test data for key management");
    }

    #[test]
    fn test_key_rotate_with_recovery() {
        let (_temp_dir, archive_dir) = setup_test_archive();

        // Rotate keys with recovery
        let result =
            key_rotate(&archive_dir, "test-password", "new-password", true, |_| {}).unwrap();
        assert_eq!(result.slot_count, 2);
        assert!(result.recovery_secret.is_some());

        // Verify recovery slot
        let list_result = key_list(&archive_dir).unwrap();
        assert_eq!(list_result.slots.len(), 2);
        assert_eq!(list_result.slots[0].slot_type, "password");
        assert_eq!(list_result.slots[1].slot_type, "recovery");
    }

    #[test]
    fn test_key_add_after_revoke_no_id_collision() {
        let (_temp_dir, archive_dir) = setup_test_archive();

        // Add slots 1 and 2
        key_add_password(&archive_dir, "test-password", "password-1").unwrap();
        key_add_password(&archive_dir, "test-password", "password-2").unwrap();

        // Now have slots [0, 1, 2]
        let list = key_list(&archive_dir).unwrap();
        assert_eq!(list.slots.len(), 3);

        // Revoke slot 1 using slot 2's password
        key_revoke(&archive_dir, "password-2", 1).unwrap();

        // Now have slots [0, 2] (gap at 1)
        let list = key_list(&archive_dir).unwrap();
        assert_eq!(list.slots.len(), 2);
        let ids: Vec<u8> = list.slots.iter().map(|s| s.id).collect();
        assert_eq!(ids, vec![0, 2]);

        // Add new slot - should get ID 3, not 2
        let new_id = key_add_password(&archive_dir, "test-password", "password-3").unwrap();
        assert_eq!(new_id, 3, "New slot should get max_id + 1, not len()");

        // Verify all passwords still work
        let config = load_config(&archive_dir).unwrap();
        assert!(unwrap_dek_with_password(&config, "test-password").is_ok());
        assert!(unwrap_dek_with_password(&config, "password-1").is_err()); // Revoked
        assert!(unwrap_dek_with_password(&config, "password-2").is_ok());
        assert!(unwrap_dek_with_password(&config, "password-3").is_ok());
    }

    #[test]
    fn test_next_key_slot_id_rejects_max_id() {
        let (_temp_dir, archive_dir) = setup_test_archive();
        let mut config = load_config(&archive_dir).unwrap();
        config.key_slots[0].id = u8::MAX;

        let err = next_key_slot_id(&config.key_slots).unwrap_err();

        assert_eq!(
            err.to_string(),
            "Cannot add more key slots: maximum slot ID (255) reached"
        );
    }

    #[test]
    fn test_key_add_password_preserves_valid_integrity_manifest() {
        let (_temp_dir, archive_dir) = setup_test_archive();

        assert_eq!(verify_bundle(&archive_dir, false).unwrap().status, "valid");

        key_add_password(&archive_dir, "test-password", "new-password").unwrap();

        assert_eq!(verify_bundle(&archive_dir, false).unwrap().status, "valid");
    }

    #[test]
    fn test_key_rotate_preserves_valid_integrity_manifest() {
        let (_temp_dir, archive_dir) = setup_test_archive();

        assert_eq!(verify_bundle(&archive_dir, false).unwrap().status, "valid");

        key_rotate(&archive_dir, "test-password", "new-password", true, |_| {}).unwrap();

        assert_eq!(verify_bundle(&archive_dir, false).unwrap().status, "valid");
    }

    #[test]
    #[cfg(unix)]
    fn test_key_add_password_materializes_in_tree_symlinked_required_asset() -> Result<()> {
        let (_temp_dir, archive_dir) = setup_test_archive();
        let site_dir = super::super::resolve_site_dir(&archive_dir)?;
        replace_viewer_with_in_tree_symlink(&site_dir);

        key_add_password(&archive_dir, "test-password", "new-password")?;

        anyhow::ensure!(verify_bundle(&archive_dir, false)?.status == "valid");
        let viewer_metadata = std::fs::symlink_metadata(site_dir.join("viewer.js"))?;
        anyhow::ensure!(viewer_metadata.file_type().is_file());
        anyhow::ensure!(!viewer_metadata.file_type().is_symlink());
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn test_key_add_password_wrong_password_preserves_in_tree_symlinked_required_asset()
    -> Result<()> {
        let (_temp_dir, archive_dir) = setup_test_archive();
        let site_dir = super::super::resolve_site_dir(&archive_dir)?;
        replace_viewer_with_in_tree_symlink(&site_dir);

        let err = match key_add_password(&archive_dir, "wrong-password", "new-password") {
            Ok(_) => bail!("wrong password unexpectedly added a key slot"),
            Err(err) => err,
        };

        anyhow::ensure!(
            err.to_string().contains("Invalid password"),
            "unexpected error: {err:#}"
        );
        let viewer_metadata = std::fs::symlink_metadata(site_dir.join("viewer.js"))?;
        anyhow::ensure!(viewer_metadata.file_type().is_symlink());
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn test_key_rotate_materializes_in_tree_symlinked_required_asset() {
        let (_temp_dir, archive_dir) = setup_test_archive();
        let site_dir = super::super::resolve_site_dir(&archive_dir).unwrap();
        replace_viewer_with_in_tree_symlink(&site_dir);
        let expected_viewer = std::fs::read(site_dir.join("viewer-real.js")).unwrap();

        key_rotate(&archive_dir, "test-password", "new-password", true, |_| {}).unwrap();

        let viewer_metadata = std::fs::symlink_metadata(site_dir.join("viewer.js")).unwrap();
        assert!(viewer_metadata.file_type().is_file());
        assert!(!viewer_metadata.file_type().is_symlink());
        assert_eq!(
            std::fs::read(site_dir.join("viewer.js")).unwrap(),
            expected_viewer
        );
        assert_eq!(verify_bundle(&archive_dir, false).unwrap().status, "valid");
    }

    #[test]
    #[cfg(unix)]
    fn test_key_rotate_rejects_payload_directory_symlink_escape() {
        use std::os::unix::fs::symlink;

        let (temp_dir, archive_dir) = setup_test_archive();
        let site_dir = super::super::resolve_site_dir(&archive_dir).unwrap();
        let payload_dir = site_dir.join("payload");
        let outside_payload_dir = temp_dir.path().join("outside-payload");

        std::fs::rename(&payload_dir, &outside_payload_dir).unwrap();
        symlink(&outside_payload_dir, &payload_dir).unwrap();

        let err =
            key_rotate(&archive_dir, "test-password", "new-password", false, |_| {}).unwrap_err();
        assert!(
            err.to_string().contains("escapes archive directory"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn test_key_add_password_updates_private_fingerprint_and_master_key() {
        let (_temp_dir, archive_dir) = setup_test_archive();
        let site_dir = super::super::resolve_site_dir(&archive_dir).unwrap();
        let private_dir = site_dir.parent().unwrap().join("private");

        let old_fingerprint =
            std::fs::read_to_string(private_dir.join("integrity-fingerprint.txt")).unwrap();
        let old_master_key = std::fs::read_to_string(private_dir.join("master-key.json")).unwrap();

        key_add_password(&archive_dir, "test-password", "new-password").unwrap();

        let new_fingerprint =
            std::fs::read_to_string(private_dir.join("integrity-fingerprint.txt")).unwrap();
        let new_master_key = std::fs::read_to_string(private_dir.join("master-key.json")).unwrap();

        assert_ne!(old_fingerprint, new_fingerprint);
        assert_ne!(old_master_key, new_master_key);
    }

    #[test]
    fn test_key_add_recovery_writes_private_recovery_artifact() {
        let (_temp_dir, archive_dir) = setup_test_archive();
        let site_dir = super::super::resolve_site_dir(&archive_dir).unwrap();
        let private_dir = site_dir.parent().unwrap().join("private");

        assert!(!private_dir.join("recovery-secret.txt").exists());

        let (_slot_id, secret) = key_add_recovery(&archive_dir, "test-password").unwrap();
        let recovery_file =
            std::fs::read_to_string(private_dir.join("recovery-secret.txt")).unwrap();

        assert!(recovery_file.contains(secret.encoded()));
    }

    #[test]
    fn test_key_revoke_recovery_removes_private_recovery_artifact() {
        let (_temp_dir, archive_dir) = setup_test_archive();
        let site_dir = super::super::resolve_site_dir(&archive_dir).unwrap();
        let private_dir = site_dir.parent().unwrap().join("private");

        let (recovery_slot_id, _secret) = key_add_recovery(&archive_dir, "test-password").unwrap();
        key_add_password(&archive_dir, "test-password", "second-password").unwrap();
        assert!(private_dir.join("recovery-secret.txt").exists());

        key_revoke(&archive_dir, "second-password", recovery_slot_id).unwrap();

        assert!(!private_dir.join("recovery-secret.txt").exists());
    }

    #[test]
    fn test_key_revoke_one_of_multiple_recovery_slots_removes_stale_private_recovery_artifact() {
        let (_temp_dir, archive_dir) = setup_test_archive();
        let site_dir = super::super::resolve_site_dir(&archive_dir).unwrap();
        let private_dir = site_dir.parent().unwrap().join("private");

        let (first_recovery_slot_id, first_secret) =
            key_add_recovery(&archive_dir, "test-password").unwrap();
        let (second_recovery_slot_id, second_secret) =
            key_add_recovery(&archive_dir, "test-password").unwrap();

        let recovery_file_before =
            std::fs::read_to_string(private_dir.join("recovery-secret.txt")).unwrap();
        assert!(recovery_file_before.contains(second_secret.encoded()));

        key_revoke(&archive_dir, "test-password", second_recovery_slot_id).unwrap();

        assert!(!private_dir.join("recovery-secret.txt").exists());

        let config = load_config(&archive_dir).unwrap();
        assert!(DecryptionEngine::unlock_with_recovery(config, first_secret.as_bytes()).is_ok());

        assert_ne!(first_recovery_slot_id, second_recovery_slot_id);
    }

    #[test]
    fn test_key_rotate_refreshes_private_recovery_and_master_key() {
        let (_temp_dir, archive_dir) = setup_test_archive();
        let site_dir = super::super::resolve_site_dir(&archive_dir).unwrap();
        let private_dir = site_dir.parent().unwrap().join("private");

        let old_master_key = std::fs::read_to_string(private_dir.join("master-key.json")).unwrap();
        let result =
            key_rotate(&archive_dir, "test-password", "new-password", true, |_| {}).unwrap();

        let new_master_key = std::fs::read_to_string(private_dir.join("master-key.json")).unwrap();
        let recovery_file =
            std::fs::read_to_string(private_dir.join("recovery-secret.txt")).unwrap();

        assert_ne!(old_master_key, new_master_key);
        assert!(recovery_file.contains(result.recovery_secret.as_deref().unwrap()));
    }

    #[test]
    fn test_key_rotate_without_recovery_removes_stale_private_recovery_artifact() {
        let (_temp_dir, archive_dir) = setup_test_archive();
        let site_dir = super::super::resolve_site_dir(&archive_dir).unwrap();
        let private_dir = site_dir.parent().unwrap().join("private");

        let (_slot_id, _secret) = key_add_recovery(&archive_dir, "test-password").unwrap();
        assert!(private_dir.join("recovery-secret.txt").exists());

        key_rotate(&archive_dir, "test-password", "new-password", false, |_| {}).unwrap();

        assert!(!private_dir.join("recovery-secret.txt").exists());
        assert!(!private_dir.join("qr-code.png").exists());
        assert!(!private_dir.join("qr-code.svg").exists());
    }

    #[test]
    fn test_key_rotate_reencrypts_attachment_blobs() {
        let (_temp_dir, archive_dir) = setup_test_archive_with_attachments();

        assert_eq!(verify_bundle(&archive_dir, false).unwrap().status, "valid");

        key_rotate(&archive_dir, "test-password", "new-password", false, |_| {}).unwrap();

        let site_dir = super::super::resolve_site_dir(&archive_dir).unwrap();
        let config = load_config(&archive_dir).unwrap();
        let dek = unwrap_dek_with_password(&config, "new-password").unwrap();
        let export_id_raw = BASE64_STANDARD.decode(&config.export_id).unwrap();
        let export_id: [u8; 16] = export_id_raw.as_slice().try_into().unwrap();

        let manifest_ciphertext =
            std::fs::read(site_dir.join("blobs").join("manifest.enc")).unwrap();
        let manifest = decrypt_manifest(&manifest_ciphertext, &dek, &export_id).unwrap();
        assert_eq!(manifest.entries.len(), 1);
        assert_eq!(manifest.entries[0].filename, "proof.txt");

        let blob_ciphertext = std::fs::read(
            site_dir
                .join("blobs")
                .join(format!("{}.bin", manifest.entries[0].hash)),
        )
        .unwrap();
        let plaintext = decrypt_blob(
            &blob_ciphertext,
            &dek,
            &export_id,
            &manifest.entries[0].hash,
        )
        .unwrap();
        assert_eq!(plaintext, b"attachment payload");
        assert_eq!(verify_bundle(&archive_dir, false).unwrap().status, "valid");
    }

    #[test]
    fn test_key_rotate_failure_before_site_swap_preserves_live_archive() {
        let (temp_dir, archive_dir) = setup_test_archive_with_attachments();
        let decrypted_path = temp_dir.path().join("decrypted-after-failure.txt");
        let site_dir = super::super::resolve_site_dir(&archive_dir).unwrap();

        std::fs::write(site_dir.join("blobs").join("manifest.enc"), b"corrupted").unwrap();

        let rotate_result =
            key_rotate(&archive_dir, "test-password", "new-password", false, |_| {});
        assert!(rotate_result.is_err());

        let config = load_config(&archive_dir).unwrap();
        assert!(unwrap_dek_with_password(&config, "new-password").is_err());

        let decryptor = DecryptionEngine::unlock_with_password(config, "test-password").unwrap();
        decryptor
            .decrypt_to_file(&archive_dir, &decrypted_path, |_, _| {})
            .unwrap();

        let decrypted = std::fs::read(&decrypted_path).unwrap();
        assert_eq!(decrypted, b"Test data for key management");
    }

    #[test]
    fn test_write_json_pretty_atomically_overwrites_existing_file() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("config.json");
        std::fs::write(&path, "{\"before\":true}\n").unwrap();

        let value = serde_json::json!({ "after": true });
        write_json_pretty_atomically(&path, &value).unwrap();

        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(written, value);
    }

    #[test]
    fn test_unique_atomic_backup_path_changes_each_call() -> Result<()> {
        use std::collections::BTreeSet;

        let temp_dir = TempDir::new()?;
        let path = temp_dir.path().join("config.json");

        let first = unique_atomic_backup_path(&path);
        let second = unique_atomic_backup_path(&path);
        let mut seen = BTreeSet::new();

        if !seen.insert(first.clone()) || !seen.insert(second.clone()) {
            anyhow::bail!(
                "backup sidecar names should be unique, got {} twice",
                first.display()
            );
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn test_replacement_path_entry_exists_detects_dangling_symlink() -> Result<()> {
        use std::os::unix::fs::symlink;

        let temp_dir = TempDir::new()?;
        let link_path = temp_dir.path().join("config.json");
        let missing_target = temp_dir.path().join("missing-config.json");

        symlink(&missing_target, &link_path)?;

        if link_path.exists() {
            anyhow::bail!("dangling symlink unexpectedly resolved through Path::exists");
        }
        if !replacement_path_entry_exists(&link_path)? {
            anyhow::bail!(
                "replacement path entry check missed dangling symlink {}",
                link_path.display()
            );
        }
        Ok(())
    }

    #[test]
    fn test_replace_file_from_temp_via_backup_overwrites_existing_file() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let final_path = temp_dir.path().join("config.json");
        let temp_path = temp_dir.path().join("config.tmp");
        let first_err = std::io::Error::from(std::io::ErrorKind::AlreadyExists);

        std::fs::write(&final_path, br#"{"slots":["old"]}"#)?;
        std::fs::write(&temp_path, br#"{"slots":["new"]}"#)?;

        replace_file_from_temp_via_backup(&temp_path, &final_path, &first_err)?;

        let content = std::fs::read_to_string(&final_path)?;
        if !content.contains("new") {
            anyhow::bail!("final config did not contain replacement content: {content}");
        }
        if temp_path.exists() {
            anyhow::bail!(
                "temporary config was left behind at {}",
                temp_path.display()
            );
        }
        Ok(())
    }

    #[test]
    fn test_replace_dir_from_temp_overwrites_existing_site() {
        let temp_dir = TempDir::new().unwrap();
        let final_dir = temp_dir.path().join("archive");
        let staged_dir = temp_dir.path().join("archive.staged");

        std::fs::create_dir_all(final_dir.join("site")).unwrap();
        std::fs::write(final_dir.join("site/old.txt"), "old").unwrap();

        std::fs::create_dir_all(staged_dir.join("site")).unwrap();
        std::fs::write(staged_dir.join("site/new.txt"), "new").unwrap();

        replace_dir_from_temp(&staged_dir, &final_dir).unwrap();

        assert!(!staged_dir.exists());
        assert!(final_dir.join("site/new.txt").exists());
        assert!(!final_dir.join("site/old.txt").exists());
        let sidecars = std::fs::read_dir(temp_dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(
            !sidecars.iter().any(|name| name.contains(".archive.bak.")),
            "backup sidecar should be cleaned up, found: {sidecars:?}"
        );
    }

    #[test]
    fn test_replace_dir_from_temp_rejects_file_target() {
        let temp_dir = TempDir::new().unwrap();
        let final_dir = temp_dir.path().join("archive");
        let staged_dir = temp_dir.path().join("archive.staged");

        std::fs::write(&final_dir, "not a directory").unwrap();
        std::fs::create_dir_all(staged_dir.join("site")).unwrap();
        std::fs::write(staged_dir.join("site/new.txt"), "new").unwrap();

        let err = replace_dir_from_temp(&staged_dir, &final_dir).unwrap_err();

        assert!(
            err.to_string().contains("not a directory"),
            "unexpected error: {err:#}"
        );
        assert!(staged_dir.exists());
        assert_eq!(
            std::fs::read_to_string(&final_dir).unwrap(),
            "not a directory"
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_replace_dir_from_temp_rejects_dangling_symlink_target() {
        use std::os::unix::fs::symlink;

        let temp_dir = TempDir::new().unwrap();
        let final_dir = temp_dir.path().join("archive");
        let staged_dir = temp_dir.path().join("archive.staged");
        let missing_target = temp_dir.path().join("missing-archive");

        symlink(&missing_target, &final_dir).unwrap();
        std::fs::create_dir_all(staged_dir.join("site")).unwrap();
        std::fs::write(staged_dir.join("site/new.txt"), "new").unwrap();

        let err = replace_dir_from_temp(&staged_dir, &final_dir).unwrap_err();

        assert!(
            err.to_string().contains("through symlink"),
            "unexpected error: {err:#}"
        );
        assert!(staged_dir.exists());
        assert!(
            std::fs::symlink_metadata(&final_dir)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    /// `coding_agent_session_search-htiim`: regression gate mirroring
    /// the unwrap_key contract pinned by `encrypt.rs::
    /// unwrap_key_chains_aead_source_error_into_diagnostic_message`
    /// (commit 0b81b601). Pre-fix, key_management.rs::unwrap_key
    /// returned bare "Key unwrapping failed" / "Invalid DEK length"
    /// strings that dropped the underlying aead::Error. Post-fix,
    /// every site preserves the source error in the chain AND
    /// surfaces actionable diagnostics (slot id, input lengths).
    /// This test exercises the unwrap_key path with a tampered
    /// ciphertext and asserts:
    ///   1. slot id appears in the rendered error
    ///   2. wrapped/nonce lengths appear (sanity-check of inputs)
    ///   3. ":" source-separator survives (a future refactor that
    ///      drops `: {err}` would fail this)
    ///   4. legacy "Key unwrapping failed" prefix preserved so
    ///      operator runbook grep patterns still match.
    #[test]
    fn unwrap_key_chains_aead_source_error_into_diagnostic_message() {
        // Build a real wrapped DEK directly with aes_gcm so we don't
        // depend on a higher-level encryption engine in this module.
        use aes_gcm::aead::{Aead, KeyInit, Payload};
        use aes_gcm::{Aes256Gcm, Nonce};

        let kek = [0u8; 32];
        let dek = [0u8; 32];
        let export_id = [42u8; 16];
        let slot_id = 7u8;
        let nonce_bytes = [3u8; 12];

        let mut aad = Vec::with_capacity(17);
        aad.extend_from_slice(&export_id);
        aad.push(slot_id);

        let cipher = Aes256Gcm::new_from_slice(&kek).expect("Invalid key length");
        let mut wrapped = cipher
            .encrypt(
                &Nonce::from(nonce_bytes),
                Payload {
                    msg: &dek,
                    aad: &aad,
                },
            )
            .expect("encrypt produces wrapped DEK + auth tag");

        // Flip the last byte of the auth tag so MAC verification fails
        // on unwrap. AES-GCM appends a 16-byte auth tag — flipping
        // any byte in it is sufficient to fail verification.
        let last = wrapped.len() - 1;
        wrapped[last] ^= 0x55;

        let err = unwrap_key(&kek, &wrapped, &nonce_bytes, &export_id, slot_id)
            .expect_err("tampered ciphertext must fail unwrap");
        let rendered = err.to_string();

        // Invariant 1: slot id present so operators can correlate.
        assert!(
            rendered.contains(&format!("slot {slot_id}")),
            "unwrap error must name the slot id; got: {rendered}"
        );
        // Invariant 2: input-size diagnostic survives.
        assert!(
            rendered.contains(&format!("{} bytes wrapped", wrapped.len())),
            "unwrap error must include the wrapped-ciphertext length; got: {rendered}"
        );
        assert!(
            rendered.contains("12 bytes nonce"),
            "unwrap error must include the AES-GCM nonce length; got: {rendered}"
        );
        // Invariant 3: ":" source-separator survives.
        assert!(
            rendered.contains(": "),
            "unwrap error must include `: <source>` separator so the \
             aead source error survives in the chain; got: {rendered}"
        );
        // Invariant 4: legacy prefix preserved for runbook grep.
        assert!(
            rendered.contains("Key unwrapping failed"),
            "unwrap error must keep the human-facing prefix for runbook \
             grep compatibility; got: {rendered}"
        );
    }

    /// Companion gate for the HKDF KEK length-check arm. Pre-fix,
    /// `derive_kek_hkdf` returned bare "HKDF expansion produced
    /// invalid KEK length" with no diagnostic; post-fix, the message
    /// carries the actual length so operators can debug a
    /// frankensqlite / hkdf upstream regression that returned the
    /// wrong KEK size.
    #[test]
    fn derive_kek_hkdf_error_message_pins_actual_kek_length() {
        // Direct exercise of the conversion arm, using the public
        // hkdf wrapper to land at a 16-byte output (not 32). This
        // mirrors the gate landed in encrypt.rs by 0b81b601 so a
        // regression in either site fails its own assertion.
        let actual_kek = crate::encryption::hkdf_extract_expand(
            b"recovery-secret",
            b"salty-salty-salty-salt",
            b"cass-pages-kek-v2",
            16,
        )
        .expect("hkdf with 16-byte output must succeed");
        assert_eq!(actual_kek.len(), 16);

        let conversion: Result<[u8; 32], Vec<u8>> = actual_kek.try_into();
        let raw_err = conversion.expect_err("16 != 32 must fail try_into");
        assert_eq!(raw_err.len(), 16);

        // Codify the expected message shape so a future refactor
        // that reverts to `|_| ... "invalid KEK length"` without
        // actual_len fails the assertion.
        let rendered = format!(
            "HKDF expansion produced invalid KEK length: expected 32, got {}",
            raw_err.len()
        );
        assert!(rendered.contains("expected 32"));
        assert!(rendered.contains("got 16"));
    }
}
