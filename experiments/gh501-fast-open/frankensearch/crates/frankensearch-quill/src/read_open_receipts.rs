//! Optional, Linux-local reuse of a successful *file-prefix* verification.
//!
//! This is not a replacement for cryptographic verification of current bytes.
//! It relies on immutable published files and trustworthy kernel metadata. In
//! particular, media corruption and writes through an already dirty writable
//! mapping can leave that metadata unchanged. Use the default strict open for
//! those threat models. Section checks are deliberately NOT bypassed.
//!
//! The cache directory must already exist, belong to the effective user, and
//! have mode 0700. Files are opened relative to its retained directory handle;
//! neither symlinks nor a path switch can redirect a read/write after admission.
//! A SHA-256 checksum detects damaged proof state, not forgery by the owner.
//! The owner is already trusted to publish the MANIFEST and segment witnesses.
//!
//! No index files are written. Missing/unusable/corrupt receipts mean a full
//! check. Only a successful full check may mint a new receipt. Hits preserve
//! the original verification time rather than extending their own lifetime.

use std::collections::BTreeMap;
use std::fs::File;
use std::io;
#[cfg(target_os = "linux")]
use std::io::{Read, Write};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

const MAGIC: &[u8; 8] = b"FSQORC02";
const MAX_BYTES: usize = 1 << 20;
const RECORD_BYTES: usize = 12 * 8;
const HEADER_BYTES: usize = 8 + 32 + 4;
const CHECKSUM_BYTES: usize = 32;
const BOOK: &str = "read-open-receipts-v2";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Policy {
    pub max_age: Duration,
    pub minimum_file_age: Duration,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            max_age: Duration::from_secs(3600),
            minimum_file_age: Duration::from_secs(60),
        }
    }
}

/// Bind proof to the schema and the actual MANIFEST file witness, not a path.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct Binding {
    pub schema_id: u64,
    pub segment_id: u64,
    pub file_len: u64,
    pub file_xxh3: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Identity {
    dev: u64,
    ino: u64,
    len: u64,
    mtime_s: i64,
    mtime_ns: i64,
    ctime_s: i64,
    ctime_ns: i64,
}

impl Identity {
    /// fstat the descriptor passed by SegmentReader::open_published_checked.
    /// Do not replace this with path-based metadata.
    #[cfg(target_os = "linux")]
    pub(crate) fn of_file(file: &File) -> Option<Self> {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata().ok()?;
        if !metadata.is_file() {
            return None;
        }
        let identity = Self {
            dev: metadata.dev(), ino: metadata.ino(), len: metadata.len(),
            mtime_s: metadata.mtime(), mtime_ns: metadata.mtime_nsec(),
            ctime_s: metadata.ctime(), ctime_ns: metadata.ctime_nsec(),
        };
        identity.valid().then_some(identity)
    }

    #[cfg(not(target_os = "linux"))]
    pub(crate) fn of_file(_file: &File) -> Option<Self> { None }

    fn valid(self) -> bool {
        (0..1_000_000_000).contains(&self.mtime_ns)
            && (0..1_000_000_000).contains(&self.ctime_ns)
    }

    fn old_enough(self, now: u64, minimum: Duration) -> bool {
        let mtime = i128::from(self.mtime_s) * 1_000_000_000 + i128::from(self.mtime_ns);
        let ctime = i128::from(self.ctime_s) * 1_000_000_000 + i128::from(self.ctime_ns);
        let elapsed = i128::from(now) * 1_000_000_000 - mtime.max(ctime);
        i128::try_from(minimum.as_nanos()).is_ok_and(|minimum| elapsed >= minimum)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Receipt {
    binding: Binding,
    identity: Identity,
    verified_s: u64,
}

pub(crate) struct ReceiptBook {
    directory: Option<File>,
    producer: [u8; 32],
    loaded: BTreeMap<Binding, Receipt>,
    kept: BTreeMap<Binding, Receipt>,
    policy: Policy,
    now: Option<u64>,
}

impl ReceiptBook {
    pub(crate) fn load(directory: &Path, policy: Policy) -> Self {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs());
        Self::load_at(directory, policy, now)
    }

    fn load_at(directory: &Path, policy: Policy, now: Option<u64>) -> Self {
        let producer = producer_binding();
        let directory = producer.and_then(|_| open_private_directory(directory).ok());
        let producer = producer.unwrap_or([0; 32]);
        let loaded = directory.as_ref()
            .and_then(|dir| read_book(dir).ok())
            .and_then(|bytes| decode(&bytes, producer))
            .unwrap_or_default();
        Self { directory, producer, loaded, kept: BTreeMap::new(), policy, now }
    }

    pub(crate) fn enabled(&self) -> bool {
        self.directory.is_some() && self.now.is_some()
    }

    /// An eligible receipt must still be checked against the same descriptor
    /// after the cheap MANIFEST validation, before its witness is consumed.
    pub(crate) fn admitted(&self, binding: Binding, identity: Identity) -> Option<Receipt> {
        if !self.enabled() || identity.len != binding.file_len { return None; }
        let now = self.now?;
        let receipt = self.loaded.get(&binding)?;
        let age = now.checked_sub(receipt.verified_s)?; // future-dated proof is a miss
        (receipt.identity == identity && age <= self.policy.max_age.as_secs()
            && identity.old_enough(now, self.policy.minimum_file_age))
            .then(|| receipt.clone())
    }

    pub(crate) fn verified(
        &self, binding: Binding, before: Identity, after: Option<Identity>,
    ) -> Option<Receipt> {
        if !self.enabled() || Some(before) != after || before.len != binding.file_len {
            return None;
        }
        let now = self.now?;
        before.old_enough(now, self.policy.minimum_file_age).then_some(Receipt {
            binding, identity: before, verified_s: now,
        })
    }

    pub(crate) fn keep(&mut self, receipt: Receipt) {
        // Only receipts for this successful snapshot survive publication.
        self.kept.insert(receipt.binding, receipt);
    }

    pub(crate) fn persist(self) {
        if self.kept == self.loaded { return; }
        let Some(directory) = self.directory else { return; };
        let Some(bytes) = encode(&self.kept, self.producer) else { return; };
        // Concurrent writers can lose cache entries, never grant new trust.
        let _ = write_book(&directory, &bytes);
    }
}

/// Only Linux local filesystems with kernel-managed inode/change metadata.
/// Unknown, network, FUSE and overlay filesystems take the strict path.
#[cfg(target_os = "linux")]
pub(crate) fn supported(file: &File) -> bool {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    rustix::fs::fstatfs(file).is_ok_and(|stat| matches!(stat.f_type as u32,
        0xef53 | 0x5846_5342 | 0x9123_683e | 0xf2f5_2010 | 0x0102_1994))
}
#[cfg(not(target_os = "linux"))]
pub(crate) fn supported(_file: &File) -> bool { false }

/// Reboot invalidation prevents accepting a recycled dev/inode identity after
/// remount. A producer/version change also invalidates the complete book.
#[cfg(target_os = "linux")]
fn producer_binding() -> Option<[u8; 32]> {
    let mut bytes = Vec::new();
    File::open("/proc/sys/kernel/random/boot_id").ok()?.take(65).read_to_end(&mut bytes).ok()?;
    let boot = std::str::from_utf8(&bytes).ok()?.trim();
    if boot.len() != 36 || !boot.bytes().all(|c| c == b'-' || c.is_ascii_hexdigit()) {
        return None;
    }
    let mut hash = Sha256::new();
    hash.update(b"quill-read-open-receipts-v2\0");
    hash.update(env!("CARGO_PKG_VERSION").as_bytes());
    hash.update(b"\0");
    hash.update(boot.as_bytes());
    hash.update(crate::keeper::CURRENT_ENGINE_VERSION.to_le_bytes());
    hash.update(crate::segment::FSLX_FORMAT_VERSION.to_le_bytes());
    Some(hash.finalize().into())
}
#[cfg(not(target_os = "linux"))]
fn producer_binding() -> Option<[u8; 32]> { None }

fn encode(receipts: &BTreeMap<Binding, Receipt>, producer: [u8; 32]) -> Option<Vec<u8>> {
    let size = receipts.len().checked_mul(RECORD_BYTES)?
        .checked_add(HEADER_BYTES + CHECKSUM_BYTES)?;
    if size > MAX_BYTES { return None; }
    let mut bytes = Vec::with_capacity(size);
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&producer);
    bytes.extend_from_slice(&u32::try_from(receipts.len()).ok()?.to_le_bytes());
    for receipt in receipts.values() {
        let b = receipt.binding;
        let i = receipt.identity;
        for value in [b.schema_id, b.segment_id, b.file_len, b.file_xxh3,
            i.dev, i.ino, i.len,
            u64::from_le_bytes(i.mtime_s.to_le_bytes()),
            u64::from_le_bytes(i.mtime_ns.to_le_bytes()),
            u64::from_le_bytes(i.ctime_s.to_le_bytes()),
            u64::from_le_bytes(i.ctime_ns.to_le_bytes()), receipt.verified_s] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
    }
    let checksum = Sha256::digest(&bytes);
    bytes.extend_from_slice(&checksum);
    Some(bytes)
}

fn decode(bytes: &[u8], producer: [u8; 32]) -> Option<BTreeMap<Binding, Receipt>> {
    if !(HEADER_BYTES + CHECKSUM_BYTES..=MAX_BYTES).contains(&bytes.len()) { return None; }
    let (body, checksum) = bytes.split_at(bytes.len() - CHECKSUM_BYTES);
    if &Sha256::digest(body)[..] != checksum || &body[..8] != MAGIC
        || body[8..40] != producer { return None; }
    let count = usize::try_from(u32::from_le_bytes(body[40..44].try_into().ok()?)).ok()?;
    if count.checked_mul(RECORD_BYTES)?.checked_add(HEADER_BYTES)? != body.len() {
        return None;
    }
    let mut receipts = BTreeMap::new();
    for record in body[HEADER_BYTES..].chunks_exact(RECORD_BYTES) {
        let mut f = [0_u64; 12];
        for (out, word) in f.iter_mut().zip(record.chunks_exact(8)) {
            *out = u64::from_le_bytes(word.try_into().ok()?);
        }
        let binding = Binding { schema_id: f[0], segment_id: f[1], file_len: f[2], file_xxh3: f[3] };
        let identity = Identity { dev: f[4], ino: f[5], len: f[6],
            mtime_s: i64::from_le_bytes(f[7].to_le_bytes()),
            mtime_ns: i64::from_le_bytes(f[8].to_le_bytes()),
            ctime_s: i64::from_le_bytes(f[9].to_le_bytes()),
            ctime_ns: i64::from_le_bytes(f[10].to_le_bytes()) };
        if !identity.valid() || identity.len != binding.file_len { return None; }
        let receipt = Receipt { binding, identity, verified_s: f[11] };
        if receipts.insert(binding, receipt).is_some() { return None; }
    }
    Some(receipts)
}

#[cfg(target_os = "linux")]
fn private(metadata: &std::fs::Metadata, directory: bool) -> bool {
    use std::os::unix::fs::MetadataExt;
    metadata.uid() == rustix::process::geteuid().as_raw()
        && if directory {
            metadata.is_dir() && metadata.mode() & 0o777 == 0o700
        } else {
            metadata.is_file() && metadata.mode() & 0o777 == 0o600 && metadata.nlink() == 1
        }
}

#[cfg(target_os = "linux")]
fn open_private_directory(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let flags = rustix::fs::OFlags::DIRECTORY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC;
    let directory = std::fs::OpenOptions::new().read(true)
        .custom_flags(i32::try_from(flags.bits()).map_err(io::Error::other)?).open(path)?;
    if !private(&directory.metadata()?, true) {
        return Err(io::Error::other("receipt directory must be owned by this user and mode 0700"));
    }
    Ok(directory)
}
#[cfg(not(target_os = "linux"))]
fn open_private_directory(_path: &Path) -> io::Result<File> {
    Err(io::Error::other("receipt reuse requires a supported Linux filesystem"))
}

#[cfg(target_os = "linux")]
fn read_book(directory: &File) -> io::Result<Vec<u8>> {
    use rustix::fs::{Mode, OFlags, openat};
    let file = File::from(openat(directory, BOOK,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC, Mode::empty())?);
    let before = Identity::of_file(&file);
    let metadata = file.metadata()?;
    if !private(&metadata, false) || metadata.len() > u64::try_from(MAX_BYTES).unwrap_or(u64::MAX) {
        return Err(io::Error::other("untrusted or oversized receipt book"));
    }
    let mut bytes = Vec::new();
    (&file).take(u64::try_from(MAX_BYTES + 1).unwrap_or(u64::MAX)).read_to_end(&mut bytes)?;
    if bytes.len() > MAX_BYTES || before.is_none() || before != Identity::of_file(&file) {
        return Err(io::Error::other("receipt book changed during read"));
    }
    Ok(bytes)
}
#[cfg(not(target_os = "linux"))]
fn read_book(_directory: &File) -> io::Result<Vec<u8>> { Err(io::Error::other("unsupported")) }

#[cfg(target_os = "linux")]
fn write_book(directory: &File, bytes: &[u8]) -> io::Result<()> {
    use rustix::fs::{AtFlags, Mode, OFlags, openat, renameat, unlinkat};
    use std::sync::atomic::{AtomicU64, Ordering};
    if !private(&directory.metadata()?, true) { return Err(io::Error::other("receipt directory changed")); }
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let temp = format!(".read-open-{}-{nonce}-{}", std::process::id(), SEQUENCE.fetch_add(1, Ordering::Relaxed));
    let mut file = File::from(openat(directory, temp.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR)?);
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        renameat(directory, temp.as_str(), directory, BOOK)?;
        Ok(())
    })();
    if result.is_err() { let _ = unlinkat(directory, temp.as_str(), AtFlags::empty()); }
    result
}
#[cfg(not(target_os = "linux"))]
fn write_book(_directory: &File, _bytes: &[u8]) -> io::Result<()> { Err(io::Error::other("unsupported")) }

#[cfg(test)]
mod tests;
