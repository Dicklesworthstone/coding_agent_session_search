//! Opt-in, source-owned identities for a proven flat/native Codex rollout pair.
//!
//! The migration command proves the existing archive before creating a receipt.
//! Scanning never guesses an identity from a basename or UUID alone. Receipts
//! retain a bounded raw-prefix high-water mark, written BEFORE delivery, so a
//! later divergent append cannot silently enter the canonical conversation.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{DiscoveredSourceFile, NormalizedConversation, ScanContext};

pub const REGISTRY_DIRECTORY: &str = "codex-layout-identities-v1";
const MAX_RECEIPT_BYTES: u64 = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Checkpoint {
    pub bytes: u64,
    pub blake3: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayoutPair {
    pub flat: PathBuf,
    pub dated: PathBuf,
    pub bare_id: String,
    pub dated_id: String,
    pub session_id: String,
}

fn valid_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if [8, 13, 18, 23].contains(&index) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

fn filename_identity(path: &Path) -> Result<(String, String, [String; 3])> {
    let stem = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(".jsonl"))
        .context("Codex layout migration requires plain .jsonl rollout files")?;
    ensure!(stem.is_ascii(), "non-ASCII Codex rollout filename");
    let rest = stem
        .strip_prefix("rollout-")
        .context("not a Codex rollout")?;
    let date = rest.get(..10).context("missing rollout date")?;
    ensure!(
        rest.as_bytes().get(10) == Some(&b'T'),
        "missing rollout timestamp"
    );
    let parts: Vec<_> = date.split('-').collect();
    ensure!(
        parts.len() == 3 && parts[0].len() == 4 && parts[1].len() == 2 && parts[2].len() == 2,
        "invalid rollout date"
    );
    let year: u32 = parts[0].parse().context("invalid rollout year")?;
    let month: usize = parts[1].parse().context("invalid rollout month")?;
    let day: u32 = parts[2].parse().context("invalid rollout day")?;
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    ensure!(
        year > 0 && (1..=12).contains(&month) && day > 0 && day <= days[month - 1],
        "invalid rollout calendar date"
    );
    let id = stem
        .get(stem.len().saturating_sub(36)..)
        .context("missing session UUID")?;
    ensure!(
        valid_uuid(id),
        "rollout filename must end in a session UUID"
    );
    ensure!(
        stem.as_bytes().get(stem.len().saturating_sub(37)) == Some(&b'-'),
        "missing session UUID separator"
    );
    Ok((
        stem.to_owned(),
        id.to_owned(),
        [
            parts[0].to_owned(),
            parts[1].to_owned(),
            parts[2].to_owned(),
        ],
    ))
}

impl LayoutPair {
    /// Both files must exist while registering a migration; neither is moved.
    pub fn discover(first: &Path, second: &Path) -> Result<Self> {
        let first = fs::canonicalize(first).context("resolve first Codex rollout")?;
        let second = fs::canonicalize(second).context("resolve second Codex rollout")?;
        let a = Self::from_member(&first)?;
        let b = Self::from_member(&second)?;
        ensure!(
            first != second,
            "migration requires two distinct rollout paths"
        );
        ensure!(
            a.bare_id == b.bare_id,
            "rollout names and native session IDs differ"
        );
        let (flat, dated) = if first == a.flat && second == b.dated {
            (first, second)
        } else if first == a.dated && second == b.flat {
            (second, first)
        } else {
            anyhow::bail!("migration requires one flat and one native dated rollout");
        };
        // Snapshot and live roots may differ. Source ownership is proved by
        // the migration command and checked again at every delivery.
        Ok(Self { flat, dated, ..a })
    }

    fn from_member(path: &Path) -> Result<Self> {
        ensure!(path.is_absolute(), "receipt paths must be absolute");
        let (bare_id, session_id, date) = filename_identity(path)?;
        let filename = path.file_name().context("missing rollout filename")?;
        let parent = path.parent().context("missing sessions directory")?;
        let root = if parent.file_name().is_some_and(|name| name == "sessions") {
            parent
        } else {
            parent
                .ancestors()
                .nth(3)
                .context("missing native sessions directory")?
        };
        ensure!(
            root.file_name().is_some_and(|name| name == "sessions"),
            "not a sessions directory"
        );
        let flat = root.join(filename);
        let dated = root
            .join(&date[0])
            .join(&date[1])
            .join(&date[2])
            .join(filename);
        ensure!(
            path == flat || path == dated,
            "non-native directory scope is not a layout alias"
        );
        let dated_id = format!("{}/{}/{}/{}", date[0], date[1], date[2], bare_id);
        Ok(Self {
            flat,
            dated,
            bare_id,
            dated_id,
            session_id,
        })
    }

    pub fn contains_external_id(&self, id: &str) -> bool {
        id == self.bare_id || id == self.dated_id
    }

    fn member(&self, path: &Path) -> Result<bool> {
        let path = fs::canonicalize(path).context("resolve migrated Codex source")?;
        ensure!(
            path == self.flat || path == self.dated,
            "ambiguous Codex layout identity: same source/basename outside the approved pair"
        );
        Ok(path == self.flat)
    }
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub checkpoint: Checkpoint,
    pub prefixes: BTreeMap<u64, String>,
    pub session_id: String,
    pub metadata_hash: String,
}

fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let sorted: BTreeMap<_, _> = object
                .iter()
                .map(|(key, value)| (key, canonical_json(value)))
                .collect();
            Value::Object(
                sorted
                    .into_iter()
                    .map(|(key, value)| (key.clone(), value))
                    .collect(),
            )
        }
        Value::Array(array) => Value::Array(array.iter().map(canonical_json).collect()),
        value => value.clone(),
    }
}

fn same_snapshot(before: &fs::Metadata, after: &fs::Metadata) -> Result<bool> {
    let same =
        after.is_file() && before.len() == after.len() && before.modified()? == after.modified()?;
    #[cfg(unix)]
    let same = {
        use std::os::unix::fs::MetadataExt;
        same && before.dev() == after.dev() && before.ino() == after.ino()
    };
    Ok(same)
}

/// Hash one complete, bounded snapshot and inspect every session_meta record.
/// `prefixes` is bounded by the caller (at most two offsets in production).
/// Bodies are neither retained in the receipt nor included in diagnostics.
pub fn read_snapshot(
    path: &Path,
    prefixes: &[u64],
    byte_budget: u64,
    tick: Option<&(dyn Fn() + Send + Sync)>,
) -> Result<Snapshot> {
    ensure!(prefixes.len() <= 2, "too many Codex proof checkpoints");
    let file = File::open(path).context("open Codex identity proof source")?;
    let before = file.metadata()?;
    ensure!(
        before.is_file(),
        "Codex identity proof source is not a regular file"
    );
    ensure!(
        before.len() > 0 && before.len() <= byte_budget,
        "Codex identity proof exceeds the rollout byte budget or is empty"
    );
    let mut reader = BufReader::new((&file).take(before.len()));
    let mut line = Vec::new();
    let mut bytes = 0_u64;
    let mut hasher = blake3::Hasher::new();
    let mut prefix_hashes = BTreeMap::new();
    let mut metadata: Option<Value> = None;
    let mut records = 0_u64;
    loop {
        line.clear();
        let count = reader.read_until(b'\n', &mut line)?;
        if count == 0 {
            break;
        }
        ensure!(
            line.last() == Some(&b'\n'),
            "unfinished Codex identity proof source; retry after the writer finishes"
        );
        let end = bytes
            .checked_add(u64::try_from(count)?)
            .context("Codex proof length overflow")?;
        for &offset in prefixes {
            if offset >= bytes && offset <= end && !prefix_hashes.contains_key(&offset) {
                let mut prefix = hasher.clone();
                prefix.update(&line[..usize::try_from(offset - bytes)?]);
                prefix_hashes.insert(offset, prefix.finalize().to_hex().to_string());
            }
        }
        hasher.update(&line);
        bytes = end;
        let text = std::str::from_utf8(&line).context("Codex identity proof is not UTF-8")?;
        let text = text.trim_start_matches('\u{feff}').trim();
        if text.is_empty() {
            continue;
        }
        let record: Value =
            serde_json::from_str(text).context("invalid JSON in Codex identity proof")?;
        if record.get("type").and_then(Value::as_str) == Some("session_meta") {
            let record = canonical_json(&record);
            if let Some(previous) = &metadata {
                ensure!(
                    previous == &record,
                    "conflicting session metadata in Codex rollout"
                );
            } else {
                ensure!(
                    records == 0,
                    "Codex session metadata must precede conversation records"
                );
                metadata = Some(record);
            }
        }
        records += 1;
        if records.is_multiple_of(1024)
            && let Some(tick) = tick
        {
            tick();
        }
    }
    ensure!(
        bytes == before.len(),
        "Codex identity proof source was truncated"
    );
    ensure!(
        same_snapshot(&before, &file.metadata()?)? && same_snapshot(&before, &fs::metadata(path)?)?,
        "Codex identity proof source changed; retry this source"
    );
    let metadata = metadata.context("missing native Codex session metadata")?;
    let session_id = metadata
        .pointer("/payload/id")
        .and_then(Value::as_str)
        .context("missing native Codex session ID")?;
    ensure!(
        valid_uuid(session_id),
        "native Codex session ID is not a UUID"
    );
    let metadata_hash = blake3::hash(&serde_json::to_vec(&metadata)?)
        .to_hex()
        .to_string();
    Ok(Snapshot {
        checkpoint: Checkpoint {
            bytes,
            blake3: hasher.finalize().to_hex().to_string(),
        },
        prefixes: prefix_hashes,
        session_id: session_id.to_owned(),
        metadata_hash,
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub version: u32,
    pub source_id: String,
    pub agent: String,
    pub canonical_conversation_id: i64,
    pub canonical_external_id: String,
    pub pair: LayoutPair,
    pub metadata_hash: String,
    pub baseline: Checkpoint,
    pub head: Checkpoint,
    pub flat_snapshot: Checkpoint,
    pub dated_snapshot: Checkpoint,
}

fn valid_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}

impl Receipt {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 1 && self.agent == "codex",
            "unsupported Codex identity receipt"
        );
        ensure!(
            !self.source_id.is_empty() && self.canonical_conversation_id > 0,
            "invalid Codex receipt ownership"
        );
        let flat = LayoutPair::from_member(&self.pair.flat)?;
        let dated = LayoutPair::from_member(&self.pair.dated)?;
        ensure!(
            flat.flat == self.pair.flat
                && dated.dated == self.pair.dated
                && flat.bare_id == dated.bare_id
                && flat.bare_id == self.pair.bare_id
                && flat.dated_id == self.pair.dated_id
                && flat.session_id == self.pair.session_id,
            "invalid Codex layout pair in receipt"
        );
        ensure!(
            self.pair.contains_external_id(&self.canonical_external_id),
            "receipt cannot change directory scopes outside its pair"
        );
        ensure!(
            valid_hash(&self.metadata_hash),
            "invalid Codex metadata proof"
        );
        for checkpoint in [
            &self.baseline,
            &self.head,
            &self.flat_snapshot,
            &self.dated_snapshot,
        ] {
            ensure!(
                checkpoint.bytes > 0 && valid_hash(&checkpoint.blake3),
                "invalid Codex prefix proof"
            );
            ensure!(
                checkpoint.bytes <= self.head.bytes,
                "Codex receipt would weaken its high-water proof"
            );
            if checkpoint.bytes == self.head.bytes {
                ensure!(
                    checkpoint.blake3 == self.head.blake3,
                    "conflicting equal-length Codex prefix proofs"
                );
            }
        }
        Ok(())
    }

    /// Adopt another snapshot only after proving its entire overlapping prefix.
    /// Unknown shorter snapshots require a still-accessible, proven longer peer.
    pub fn admit_path(
        &mut self,
        path: &Path,
        byte_budget: u64,
        tick: Option<&(dyn Fn() + Send + Sync)>,
    ) -> Result<()> {
        self.validate()?;
        let flat = self.pair.member(path)?;
        let snapshot = read_snapshot(path, &[self.head.bytes], byte_budget, tick)?;
        ensure!(
            snapshot.session_id == self.pair.session_id
                && snapshot.metadata_hash == self.metadata_hash,
            "conflicting Codex session identity; migration refused"
        );
        if snapshot.checkpoint.bytes >= self.head.bytes {
            ensure!(
                snapshot.prefixes.get(&self.head.bytes) == Some(&self.head.blake3),
                "conflicting Codex rollout prefix; migration refused"
            );
        } else if ![&self.baseline, &self.flat_snapshot, &self.dated_snapshot]
            .contains(&&snapshot.checkpoint)
        {
            let peer = if flat {
                &self.pair.dated
            } else {
                &self.pair.flat
            };
            let proof = read_snapshot(
                peer,
                &[self.head.bytes, snapshot.checkpoint.bytes],
                byte_budget,
                tick,
            )
            .context("cannot prove a shorter Codex snapshot without its longer peer")?;
            ensure!(
                proof.session_id == self.pair.session_id
                    && proof.metadata_hash == self.metadata_hash
                    && proof.prefixes.get(&self.head.bytes) == Some(&self.head.blake3)
                    && proof.prefixes.get(&snapshot.checkpoint.bytes)
                        == Some(&snapshot.checkpoint.blake3),
                "conflicting or unverifiable shorter Codex rollout; migration refused"
            );
        }
        if snapshot.checkpoint.bytes > self.head.bytes {
            self.head = snapshot.checkpoint.clone();
        }
        if flat {
            self.flat_snapshot = snapshot.checkpoint;
        } else {
            self.dated_snapshot = snapshot.checkpoint;
        }
        self.validate()
    }
}

/// Source and basename locate a receipt; exact paths and metadata then guard it.
/// Including the source in a length-delimited digest prevents cross-source aliases.
pub fn receipt_path(data_dir: &Path, source_id: &str, bare_id: &str) -> PathBuf {
    let mut hash = blake3::Hasher::new_derive_key("cass codex layout receipt key v1");
    for part in [source_id, bare_id] {
        hash.update(&(part.len() as u64).to_le_bytes());
        hash.update(part.as_bytes());
    }
    data_dir
        .join(REGISTRY_DIRECTORY)
        .join(format!("{}.json", hash.finalize().to_hex()))
}

pub struct ReceiptLock {
    path: PathBuf,
    _file: File,
}

impl ReceiptLock {
    pub fn acquire(path: PathBuf) -> Result<Self> {
        let parent = path.parent().context("missing Codex receipt directory")?;
        fs::create_dir_all(parent)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path.with_extension("lock"))?;
        file.try_lock().context(
            "Codex identity migration is already in use; retry after the other indexer finishes",
        )?;
        Ok(Self { path, _file: file })
    }

    pub fn load_path(path: &Path) -> Result<Option<Receipt>> {
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("open Codex identity receipt"),
        };
        ensure!(
            file.metadata()?.len() <= MAX_RECEIPT_BYTES,
            "oversized Codex identity receipt"
        );
        let mut bytes = Vec::new();
        file.take(MAX_RECEIPT_BYTES + 1).read_to_end(&mut bytes)?;
        ensure!(
            u64::try_from(bytes.len())? <= MAX_RECEIPT_BYTES,
            "Codex identity receipt changed while reading"
        );
        let receipt: Receipt = serde_json::from_slice(&bytes)
            .context("invalid Codex identity receipt; no alias was applied")?;
        receipt.validate()?;
        Ok(Some(receipt))
    }

    pub fn load(&self) -> Result<Option<Receipt>> {
        Self::load_path(&self.path)
    }

    pub fn write(&self, receipt: &Receipt) -> Result<()> {
        receipt.validate()?;
        let mut bytes = serde_json::to_vec_pretty(receipt)?;
        bytes.push(b'\n');
        ensure!(
            u64::try_from(bytes.len())? <= MAX_RECEIPT_BYTES,
            "Codex identity receipt exceeds its size bound"
        );
        let parent = self.path.parent().context("missing receipt directory")?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary.write_all(&bytes)?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(&self.path)
            .context("publish Codex identity receipt")?;
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Ok(())
    }
}

pub(super) fn deliver(
    archive_data_dir: &Path,
    ctx: &ScanContext,
    source: &DiscoveredSourceFile,
    mut conversation: NormalizedConversation,
    sink: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
) -> Result<()> {
    // The receiving archive owns the receipts; ScanContext's data_dir may
    // instead name a configured source directory or a watch-once rollout.
    // Unbound standalone file scans cannot contain a receipt registry.
    if conversation.agent_slug != "codex"
        || !archive_data_dir.is_dir()
        || !archive_data_dir.join(REGISTRY_DIRECTORY).try_exists()?
    {
        return sink(conversation);
    }
    let Some(name) = source
        .source_path
        .file_name()
        .and_then(|name| name.to_str())
    else {
        return sink(conversation);
    };
    let bare_id = name.strip_suffix(".zst").unwrap_or(name);
    let bare_id = bare_id.strip_suffix(".jsonl").unwrap_or(bare_id);
    let path = receipt_path(archive_data_dir, &source.origin.source_id, bare_id);
    if !path.try_exists()? {
        return sink(conversation);
    }
    let lock = ReceiptLock::acquire(path)?;
    let mut receipt = lock
        .load()?
        .context("Codex identity receipt disappeared; retry instead of changing identity")?;
    ensure!(
        source.origin.source_id == receipt.source_id
            && source.origin.kind == franken_agent_detection::SourceKind::Local,
        "Codex identity receipt has different source ownership"
    );
    ensure!(
        conversation
            .external_id
            .as_deref()
            .is_some_and(|id| receipt.pair.contains_external_id(id)),
        "Codex directory scope differs from the approved migration"
    );
    ensure!(
        fs::canonicalize(&conversation.source_path)? == fs::canonicalize(&source.source_path)?,
        "Codex conversation path differs from its admitted source"
    );
    ensure!(
        !source.fs_metadata_changed(),
        "Codex layout source changed since discovery; retry this source"
    );
    let previous = receipt.clone();
    receipt.admit_path(
        &source.source_path,
        franken_agent_detection::connectors::codex::codex_rollout_byte_budget(),
        ctx.progress_tick.as_deref(),
    )?;
    ensure!(
        !source.fs_metadata_changed(),
        "Codex layout source changed during identity proof; retry this source"
    );
    if receipt != previous {
        // Write-ahead evidence: retaining a stronger proof after a consumer
        // failure is safe and retryable. Writing it after the callback leaves
        // a crash window in which a divergent branch could be merged later.
        lock.write(&receipt)?;
    }
    conversation.external_id = Some(receipt.canonical_external_id.clone());
    sink(conversation)
}

#[cfg(test)]
mod tests;
