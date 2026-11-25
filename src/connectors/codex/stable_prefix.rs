//! A bounded immutable parser view of an append-oriented session transcript.
//!
//! This is content-verified evidence, not a filesystem transaction. We retain
//! the original descriptor, check the named object's identity, and rehash the
//! admitted bytes before and after delivery. No claim covers a future write.
//! Non-Unix platforms keep the existing strict snapshot path until an equally
//! strong file-object identity is available.

use std::cell::RefCell;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::{NormalizedConversation, NormalizedMessage};

mod tail;

const COPY_BUFFER_BYTES: usize = 64 * 1024;
const RECEIPT_VERSION: u32 = 1;

/// The parser is part of the admission proof. A newline in an arbitrary file
/// never authorizes another connector to bypass its active-writer policy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StablePrefixProvider {
    #[default]
    Codex,
    ClaudeCode,
    PiAgent,
    Omp,
}

impl StablePrefixProvider {
    pub(crate) const ALL: [Self; 4] = [Self::Codex, Self::ClaudeCode, Self::PiAgent, Self::Omp];

    pub(crate) fn from_slug(slug: &str) -> Option<Self> {
        match slug {
            "codex" => Some(Self::Codex),
            "claude_code" => Some(Self::ClaudeCode),
            "pi_agent" => Some(Self::PiAgent),
            "omp" => Some(Self::Omp),
            _ => None,
        }
    }

    pub(crate) const fn slug(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::ClaudeCode => "claude_code",
            Self::PiAgent => "pi_agent",
            Self::Omp => "omp",
        }
    }

    pub(crate) const fn observation_key(self) -> &'static str {
        match self {
            Self::Codex => "codex_prefix",
            Self::ClaudeCode => "claude_prefix",
            Self::PiAgent => "pi_prefix",
            Self::Omp => "omp_prefix",
        }
    }

    pub(crate) const fn metadata_key(self) -> &'static str {
        match self {
            Self::Codex => "codex_stable_prefix",
            Self::ClaudeCode => "claude_stable_prefix",
            Self::PiAgent => "pi_stable_prefix",
            Self::Omp => "omp_stable_prefix",
        }
    }

    pub(crate) const fn requirement_key(self) -> &'static str {
        match self {
            Self::Codex => "codex_prefix_required",
            Self::ClaudeCode => "claude_prefix_required",
            Self::PiAgent => "pi_prefix_required",
            Self::Omp => "omp_prefix_required",
        }
    }

    fn is_codex(&self) -> bool {
        *self == Self::Codex
    }

    fn plain_jsonl(self, path: &Path) -> bool {
        cfg!(unix)
            && path.to_str().is_some()
            && path
                .extension()
                .is_some_and(|extension| extension == "jsonl")
            && match self {
                Self::Codex => path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("rollout-")),
                Self::ClaudeCode => !path.components().any(|component| {
                    matches!(
                        component.as_os_str().to_str(),
                        Some("claude-code-sessions" | "local-agent-mode-sessions")
                    )
                }),
                Self::PiAgent | Self::Omp => true,
            }
    }

    fn read_budget(self, path: &Path) -> Result<u64> {
        match self {
            Self::Codex => super::source_read_budget(path),
            Self::ClaudeCode => crate::connectors::claude_code::source_read_budget(),
            // The pinned pi_wire parser uses this same 100 MiB read cap.
            Self::PiAgent | Self::Omp => Ok(PI_FAMILY_MAX_SOURCE_BYTES),
        }
    }
}

pub(crate) const PI_FAMILY_MAX_SOURCE_BYTES: u64 = 100 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StablePrefixReceipt {
    pub(crate) schema_version: u32,
    // Legacy Codex receipts remain byte-compatible. Every other parser must
    // identify itself explicitly; moving its receipt under another key fails.
    #[serde(default, skip_serializing_if = "StablePrefixProvider::is_codex")]
    pub(crate) provider: StablePrefixProvider,
    pub(crate) source_path: PathBuf,
    pub(crate) device: u64,
    pub(crate) inode: u64,
    pub(crate) admitted_bytes: u64,
    pub(crate) committed_bytes: u64,
    pub(crate) blake3: String,
    pub(crate) partial: bool,
    pub(crate) message_count: usize,
    pub(crate) messages_blake3: String,
    /// Existing whole-file EOF/recovery policy, admitted only while inactive.
    /// This never grants permission to read a live unterminated record.
    #[serde(default)]
    pub(crate) inactive_eof: bool,
}

fn named_receipt(
    object: &serde_json::Map<String, serde_json::Value>,
    key: impl Fn(StablePrefixProvider) -> &'static str,
) -> Result<Option<StablePrefixReceipt>> {
    let mut found = None;
    for provider in StablePrefixProvider::ALL {
        let Some(value) = object.get(key(provider)) else {
            continue;
        };
        anyhow::ensure!(found.is_none(), "conflicting parser prefix evidence");
        let receipt: StablePrefixReceipt =
            serde_json::from_value(value.clone()).context("invalid session prefix evidence")?;
        anyhow::ensure!(
            receipt.provider == provider,
            "prefix evidence is stored under another parser's key"
        );
        found = Some(receipt);
    }
    Ok(found)
}

/// Decode durable evidence without granting source or publication authority.
/// A caller still compares the actual provider/path and revalidates the bytes.
pub(crate) fn observation_receipt(
    observation: &serde_json::Value,
) -> Result<Option<StablePrefixReceipt>> {
    let object = observation
        .as_object()
        .context("previous source observation must be a JSON object")?;
    named_receipt(object, StablePrefixProvider::observation_key)
}

/// Conversation metadata uses a separate namespace from the source ledger.
/// Neither malformed evidence nor a different provider marker means "absent".
pub(crate) fn metadata_receipt(
    metadata: &serde_json::Value,
) -> Result<Option<StablePrefixReceipt>> {
    if metadata.is_null() {
        return Ok(None);
    }
    let object = metadata
        .as_object()
        .context("session metadata must be a JSON object")?;
    let Some(cass) = object.get("cass").filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let cass = cass
        .as_object()
        .context("CASS session metadata must be a JSON object")?;
    named_receipt(cass, StablePrefixProvider::metadata_key)
}

fn changed(message: &'static str) -> anyhow::Error {
    io::Error::new(io::ErrorKind::Interrupted, message).into()
}

fn digest_is_valid(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Eligibility is deliberately narrower than the general Codex parser.
pub(crate) fn eligible(path: &Path) -> bool {
    eligible_for(StablePrefixProvider::Codex, path)
}

pub(crate) fn eligible_for(provider: StablePrefixProvider, path: &Path) -> bool {
    provider.plain_jsonl(path)
        && fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file())
}

fn open_source(provider: StablePrefixProvider, path: &Path) -> Result<File> {
    anyhow::ensure!(
        provider.plain_jsonl(path),
        "stable-prefix admission requires a supported plain session transcript"
    );
    if !fs::symlink_metadata(path)?.is_file() {
        return Err(changed(
            "Codex prefix source no longer names a regular file",
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).context("open admitted Codex prefix")?;
    anyhow::ensure!(
        file.metadata()?.is_file(),
        "Codex prefix source is not a regular file"
    );
    Ok(file)
}

fn identity(metadata: &Metadata) -> Result<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok((metadata.dev(), metadata.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "stable Codex object identity is unavailable on this platform",
        )
        .into())
    }
}

fn require_unlocked(file: &File) -> Result<()> {
    file.try_lock()
        .context("Codex prefix source is advisory-locked")?;
    file.unlock().context("release Codex prefix admission lock")
}

fn hash_prefix(file: &File, bytes: u64, tick: Option<&(dyn Fn() + Send + Sync)>) -> Result<String> {
    let mut source = file;
    source.rewind()?;
    let mut remaining = bytes;
    let mut buffer = [0_u8; COPY_BUFFER_BYTES];
    let mut hasher = blake3::Hasher::new();
    while remaining > 0 {
        let count = usize::try_from(remaining.min(buffer.len() as u64))?;
        source
            .read_exact(&mut buffer[..count])
            .context("Codex prefix was truncated during verification")?;
        hasher.update(&buffer[..count]);
        remaining -= count as u64;
        if let Some(tick) = tick {
            tick();
        }
    }
    Ok(hasher.finalize().to_hex().to_string())
}

impl StablePrefixReceipt {
    fn validate_shape(&self) -> Result<()> {
        anyhow::ensure!(
            self.schema_version == RECEIPT_VERSION
                && self.committed_bytes > 0
                && self.committed_bytes <= self.admitted_bytes
                && self.admitted_bytes <= self.provider.read_budget(&self.source_path)?
                && (!self.inactive_eof
                    || (!self.partial && self.committed_bytes == self.admitted_bytes))
                && digest_is_valid(&self.blake3)
                && digest_is_valid(&self.messages_blake3),
            "invalid Codex stable-prefix receipt"
        );
        Ok(())
    }

    fn validate_identity(&self, file: &File) -> Result<()> {
        let opened = file.metadata()?;
        let named =
            fs::symlink_metadata(&self.source_path).context("recheck named Codex prefix source")?;
        if !opened.is_file()
            || !named.is_file()
            || opened.len() < self.admitted_bytes
            || named.len() < self.admitted_bytes
            || identity(&opened)? != (self.device, self.inode)
            || identity(&named)? != (self.device, self.inode)
        {
            return Err(changed("Codex prefix source was truncated or replaced"));
        }
        require_unlocked(file)
    }

    fn validate_opened(&self, file: &File, tick: Option<&(dyn Fn() + Send + Sync)>) -> Result<()> {
        self.validate_shape()?;
        self.validate_identity(file)?;
        if hash_prefix(file, self.committed_bytes, tick)? != self.blake3 {
            return Err(changed("Codex admitted prefix was modified in place"));
        }
        let mut source = file;
        source.seek(SeekFrom::Start(self.committed_bytes - 1))?;
        let mut last = [0_u8; 1];
        source.read_exact(&mut last)?;
        anyhow::ensure!(
            self.inactive_eof || last == *b"\n",
            "Codex prefix boundary is not a complete record"
        );
        self.validate_identity(file)
    }

    /// Validate the retained source descriptor used by a bounded raw capture.
    /// This reuses the byte/object proof and grants no parser-delivery or EOF
    /// authority. Validation moves the descriptor cursor; readers must rewind
    /// before consuming their independently bounded range.
    pub(crate) fn validate_opened_source(&self, file: &File) -> Result<()> {
        self.validate_opened(file, None)
    }

    /// Revalidate persisted evidence before delayed raw capture or reuse.
    pub(crate) fn validate_source(&self) -> Result<()> {
        self.validate_shape()?;
        let file = open_source(self.provider, &self.source_path)?;
        self.validate_opened(&file, None)
    }

    /// Recreate only the previously admitted bytes. Never consume a later EOF.
    #[cfg(test)]
    pub(crate) fn capture_view(&self) -> Result<StablePrefixSnapshot> {
        self.validate_shape()?;
        let file = open_source(self.provider, &self.source_path)?;
        self.validate_opened(&file, None)?;
        let before = file.metadata()?;
        let copied = copy_view(
            &self.source_path,
            &file,
            self.committed_bytes,
            None,
            self.inactive_eof,
            false,
        )?;
        anyhow::ensure!(
            copied.bytes == self.committed_bytes && copied.digest == self.blake3,
            "Codex prefix capture changed during copying"
        );
        self.validate_opened(&file, None)?;
        Ok(StablePrefixSnapshot {
            file,
            before,
            _directory: copied.directory,
            path: copied.path,
            receipt: self.clone(),
        })
    }
}

pub(crate) struct StablePrefixSnapshot {
    file: File,
    before: Metadata,
    _directory: tempfile::TempDir,
    path: PathBuf,
    receipt: StablePrefixReceipt,
}

struct CopiedPrefix {
    directory: tempfile::TempDir,
    path: PathBuf,
    bytes: u64,
    digest: String,
    deferred_tail: bool,
}

/// Copy at most the admission length, remember the last LF, and discard only
/// the private view's incomplete suffix. Hash state at LF avoids retaining a
/// potentially huge final line in memory. The source is opened read-only.
fn copy_view(
    original: &Path,
    file: &File,
    admitted: u64,
    tick: Option<&(dyn Fn() + Send + Sync)>,
    inactive_eof: bool,
    defer_prompt_tail: bool,
) -> Result<CopiedPrefix> {
    let directory = tempfile::Builder::new()
        .prefix("cass-codex-prefix-")
        .tempdir()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // tempfile honors the process umask for directories. Seal the empty
        // directory before creating or copying any private transcript bytes.
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))?;
    }
    let path = directory.path().join(
        original
            .file_name()
            .context("Codex source has no basename")?,
    );
    let mut options = OpenOptions::new();
    options.create_new(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut view = options.open(&path)?;
    let mut source = file;
    source.rewind()?;
    let mut remaining = admitted;
    let mut copied = 0_u64;
    let mut boundary = 0_u64;
    let mut hasher = blake3::Hasher::new();
    let mut digest = hasher.finalize();
    let mut preceding_boundary = 0_u64;
    let mut preceding_digest = digest;
    let mut buffer = [0_u8; COPY_BUFFER_BYTES];
    while remaining > 0 {
        let count = usize::try_from(remaining.min(buffer.len() as u64))?;
        source
            .read_exact(&mut buffer[..count])
            .context("Codex source was truncated while capturing its prefix")?;
        view.write_all(&buffer[..count])?;
        if let Some(last_lf) = buffer[..count].iter().rposition(|byte| *byte == b'\n') {
            let start = if let Some(previous_lf) =
                buffer[..last_lf].iter().rposition(|byte| *byte == b'\n')
            {
                let previous_end = previous_lf + 1;
                hasher.update(&buffer[..previous_end]);
                preceding_boundary = copied + previous_end as u64;
                preceding_digest = hasher.finalize();
                previous_end
            } else {
                preceding_boundary = boundary;
                preceding_digest = digest;
                0
            };
            let end = last_lf + 1;
            hasher.update(&buffer[start..end]);
            digest = hasher.finalize();
            boundary = copied + end as u64;
            hasher.update(&buffer[end..count]);
        } else {
            hasher.update(&buffer[..count]);
        }
        copied += count as u64;
        remaining -= count as u64;
        if let Some(tick) = tick {
            tick();
        }
    }
    if inactive_eof {
        boundary = admitted;
        digest = hasher.finalize();
        // The pinned parser applies the same host inactivity predicate to
        // this private EOF view. The original source is independently held
        // to strict snapshot/inactivity checks before and after delivery.
    }
    let deferred_tail = defer_prompt_tail
        && boundary > 0
        && tail::needs_next_record(&view, preceding_boundary, boundary)?;
    if deferred_tail {
        boundary = preceding_boundary;
        digest = preceding_digest;
    }
    view.set_len(boundary)?;
    view.flush()?;
    if inactive_eof {
        view.set_modified(file.metadata()?.modified()?)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        view.set_permissions(fs::Permissions::from_mode(0o400))?;
    }
    drop(view);
    Ok(CopiedPrefix {
        directory,
        path,
        bytes: boundary,
        digest: digest.to_hex().to_string(),
        deferred_tail,
    })
}

impl StablePrefixSnapshot {
    /// None leaves empty or unsupported inputs on the strict path. A nonempty
    /// active JSONL source must establish a record boundary before delivery.
    /// Existing inactive EOF/recovery policy is applied to a complete private
    /// view, while retaining any previously published prefix's overlap proof.
    pub(crate) fn capture(
        source: &Path,
        limit: u64,
        previous: Option<&StablePrefixReceipt>,
        tick: Option<&(dyn Fn() + Send + Sync)>,
    ) -> Result<Option<Self>> {
        Self::capture_for(StablePrefixProvider::Codex, source, limit, previous, tick)
    }

    pub(crate) fn capture_for(
        provider: StablePrefixProvider,
        source: &Path,
        limit: u64,
        previous: Option<&StablePrefixReceipt>,
        tick: Option<&(dyn Fn() + Send + Sync)>,
    ) -> Result<Option<Self>> {
        if !eligible_for(provider, source) {
            anyhow::ensure!(
                previous.is_none(),
                "Codex prefix source no longer has a supported identity"
            );
            return Ok(None);
        }
        let file = open_source(provider, source)?;
        let before = file.metadata()?;
        if before.len() > limit {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "Codex prefix source exceeds its admitted byte budget",
            )
            .into());
        }
        require_unlocked(&file)?;
        if let Some(previous) = previous {
            anyhow::ensure!(
                previous.provider == provider,
                "prior prefix belongs to another parser"
            );
            if before.len() < previous.admitted_bytes {
                return Err(changed(
                    "Codex admission length moved behind its previous receipt",
                ));
            }
            anyhow::ensure!(
                previous.source_path == source,
                "Codex prior prefix belongs to another source"
            );
            previous.validate_opened(&file, tick)?;
        }
        if before.len() == 0 {
            return Ok(None);
        }
        let mut input = &file;
        input.seek(SeekFrom::Start(before.len() - 1))?;
        let mut last = [0_u8; 1];
        input.read_exact(&mut last)?;
        let inactive = crate::indexer::source_can_recover_incomplete_tail(source);
        let inactive_eof = last != *b"\n" && inactive;
        let copied = copy_view(
            source,
            &file,
            before.len(),
            tick,
            inactive_eof,
            provider == StablePrefixProvider::Codex && !inactive,
        )?;
        let boundary = copied.bytes;
        if previous.is_some_and(|receipt| boundary < receipt.committed_bytes) {
            return Err(changed("Codex admitted prefix boundary moved backwards"));
        }
        if boundary == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                if copied.deferred_tail {
                    "Codex trailing record awaits its neighbor or source inactivity"
                } else {
                    "Codex active source has no newline-committed record; retry this source"
                },
            )
            .into());
        }
        let (device, inode) = identity(&before)?;
        let receipt = StablePrefixReceipt {
            schema_version: RECEIPT_VERSION,
            provider,
            source_path: source.to_path_buf(),
            device,
            inode,
            admitted_bytes: before.len(),
            committed_bytes: boundary,
            blake3: copied.digest,
            partial: !inactive_eof,
            message_count: 0,
            messages_blake3: blake3::hash(b"").to_hex().to_string(),
            inactive_eof,
        };
        let snapshot = Self {
            file,
            before,
            _directory: copied.directory,
            path: copied.path,
            receipt,
        };
        snapshot.validate(tick)?;
        Ok(Some(snapshot))
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn validate(&self, tick: Option<&(dyn Fn() + Send + Sync)>) -> Result<()> {
        self.receipt.validate_opened(&self.file, tick)?;
        if self.receipt.inactive_eof {
            let after = self.file.metadata()?;
            if after.len() != self.before.len()
                || after.modified()? != self.before.modified()?
                || !crate::indexer::source_can_recover_incomplete_tail(&self.receipt.source_path)
            {
                return Err(changed(
                    "Codex inactive EOF source became active or changed",
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn receipt_for(
        &self,
        conversation: Option<&NormalizedConversation>,
        previous: Option<&StablePrefixReceipt>,
    ) -> Result<StablePrefixReceipt> {
        self.validate(None)?;
        let mut receipt = self.receipt.clone();
        anyhow::ensure!(
            previous.is_none_or(|previous| {
                previous.provider == receipt.provider && previous.source_path == receipt.source_path
            }),
            "prior projection belongs to another parser or source"
        );
        if let Some(conversation) = conversation {
            anyhow::ensure!(
                conversation.agent_slug == receipt.provider.slug()
                    && conversation.source_path == receipt.source_path,
                "prefix projection belongs to another parser or source"
            );
            if let Some(previous) = previous
                && (conversation.messages.len() < previous.message_count
                    || messages_digest(&conversation.messages[..previous.message_count])?
                        != previous.messages_blake3)
            {
                return Err(changed(
                    "Codex message projection changed before its published boundary; source reconciliation is required",
                ));
            }
            receipt.message_count = conversation.messages.len();
            receipt.messages_blake3 = messages_digest(&conversation.messages)?;
        }
        // "Full" requires both strict snapshot stability and the existing
        // inactivity policy. An idle held descriptor cannot certify future EOF.
        let after = self.file.metadata()?;
        receipt.partial = receipt.committed_bytes != receipt.admitted_bytes
            || after.len() != self.before.len()
            || after.modified()? != self.before.modified()?
            || !crate::indexer::source_can_recover_incomplete_tail(&receipt.source_path);
        Ok(receipt)
    }

    pub(crate) fn finish_receipt(
        &self,
        mut receipt: StablePrefixReceipt,
    ) -> Result<StablePrefixReceipt> {
        let now = self.receipt_for(None, None)?;
        // Never upgrade a previously partial delivery to full in the same scan.
        receipt.partial |= now.partial;
        Ok(receipt)
    }
}

/// Hash the canonical indexed identity/content, without duplicating rollout
/// bodies. Raw extras can be compacted at a size threshold; they are not stable
/// indexed-message identities and cannot authorize append-only storage.
pub(crate) fn messages_digest(messages: &[NormalizedMessage]) -> Result<String> {
    struct HashWriter(blake3::Hasher);
    impl Write for HashWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut writer = HashWriter(blake3::Hasher::new());
    for message in messages {
        serde_json::to_writer(
            &mut writer,
            &(
                message.idx,
                &message.role,
                &message.author,
                message.created_at,
                &message.content,
            ),
        )?;
        writer.write_all(b"\0")?;
    }
    Ok(writer.0.finalize().to_hex().to_string())
}

thread_local! {
    static CURRENT: RefCell<Option<(StablePrefixReceipt, bool)>> = const { RefCell::new(None) };
    static PREVIOUS: RefCell<Option<StablePrefixReceipt>> = const { RefCell::new(None) };
    static PARSER_VIEW: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// Enable occurrence ordering only while the pinned parser consumes this
/// privately captured view. This grants no source-delivery or EOF authority.
pub(crate) struct PrefixParserViewGuard<'a> {
    previous: Option<PathBuf>,
    _scope: std::marker::PhantomData<(&'a StablePrefixSnapshot, std::rc::Rc<()>)>,
}

impl<'a> PrefixParserViewGuard<'a> {
    pub(crate) fn enter(snapshot: &'a StablePrefixSnapshot) -> Self {
        let previous = PARSER_VIEW.with(|current| current.replace(Some(snapshot.path.clone())));
        Self {
            previous,
            _scope: std::marker::PhantomData,
        }
    }
}

impl Drop for PrefixParserViewGuard<'_> {
    fn drop(&mut self) {
        PARSER_VIEW.with(|current| *current.borrow_mut() = self.previous.take());
    }
}

pub(crate) fn guarded_parser_view(path: &Path) -> bool {
    PARSER_VIEW.with(|current| current.borrow().as_deref() == Some(path))
}

/// A pathname or JSON metadata alone cannot grant active-source admission.
pub(crate) struct PrefixDeliveryGuard {
    previous: Option<(StablePrefixReceipt, bool)>,
    _thread_bound: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl PrefixDeliveryGuard {
    pub(crate) fn enter(receipt: StablePrefixReceipt) -> Self {
        Self::enter_phase(receipt, true)
    }

    pub(crate) fn enter_completion(receipt: StablePrefixReceipt) -> Self {
        Self::enter_phase(receipt, false)
    }

    fn enter_phase(receipt: StablePrefixReceipt, delivery: bool) -> Self {
        let previous = CURRENT.with(|current| current.replace(Some((receipt, delivery))));
        Self {
            previous,
            _thread_bound: std::marker::PhantomData,
        }
    }
}

impl Drop for PrefixDeliveryGuard {
    fn drop(&mut self) {
        CURRENT.with(|current| *current.borrow_mut() = self.previous.take());
    }
}

pub(crate) fn current_prefix_receipt(path: &Path) -> Option<StablePrefixReceipt> {
    CURRENT.with(|current| {
        current
            .borrow()
            .as_ref()
            .filter(|(receipt, _)| receipt.source_path == path)
            .map(|(receipt, _)| receipt.clone())
    })
}

pub(crate) fn guarded_prefix_delivery(path: &Path) -> bool {
    guarded_prefix_delivery_for(StablePrefixProvider::Codex, path)
}

pub(crate) fn guarded_prefix_delivery_for(provider: StablePrefixProvider, path: &Path) -> bool {
    CURRENT.with(|current| {
        current
            .borrow()
            .as_ref()
            .is_some_and(|(receipt, delivery)| {
                *delivery
                    && receipt.provider == provider
                    && receipt.source_path == path
                    && receipt.validate_source().is_ok()
            })
    })
}

/// The host supplies the last durable observation before admission. Invalidated
/// publication receipts retain overlap evidence; they grant no reuse authority.
#[cfg(test)]
pub(crate) fn set_previous_prefix_receipt(path: &Path, observation: Option<&str>) -> Result<()> {
    set_previous_prefix_receipt_for(StablePrefixProvider::Codex, path, observation)
}

pub(crate) fn set_previous_prefix_receipt_for(
    provider: StablePrefixProvider,
    path: &Path,
    observation: Option<&str>,
) -> Result<()> {
    PREVIOUS.with(|previous| *previous.borrow_mut() = None);
    let Some(observation) = observation else {
        return Ok(());
    };
    let value: serde_json::Value = serde_json::from_str(observation)?;
    let Some(receipt) = observation_receipt(&value)? else {
        return Ok(());
    };
    anyhow::ensure!(
        receipt.provider == provider && receipt.source_path == path,
        "previous prefix receipt belongs to another parser or source"
    );
    // Admission may take the legacy strict path (for example after a pathname
    // becomes a symlink). Such a transition must not bypass a prior prefix's
    // object/content proof merely because the new format is ineligible.
    receipt.validate_source()?;
    PREVIOUS.with(|previous| *previous.borrow_mut() = Some(receipt));
    Ok(())
}

pub(crate) fn clear_previous_prefix_receipt() {
    PREVIOUS.with(|previous| *previous.borrow_mut() = None);
}

pub(crate) fn take_previous_prefix_receipt(path: &Path) -> Result<Option<StablePrefixReceipt>> {
    take_previous_prefix_receipt_for(StablePrefixProvider::Codex, path)
}

pub(crate) fn take_previous_prefix_receipt_for(
    provider: StablePrefixProvider,
    path: &Path,
) -> Result<Option<StablePrefixReceipt>> {
    let previous = PREVIOUS.with(|previous| previous.borrow_mut().take());
    anyhow::ensure!(
        previous
            .as_ref()
            .is_none_or(|receipt| receipt.provider == provider && receipt.source_path == path),
        "previous prefix admission belongs to another parser or source"
    );
    Ok(previous)
}

/// Preflight is bounded and grants no delivery or publication authority.
pub(crate) fn can_attempt(path: &Path, limit: u64) -> bool {
    can_attempt_for(StablePrefixProvider::Codex, path, limit)
}

pub(crate) fn can_attempt_for(provider: StablePrefixProvider, path: &Path, limit: u64) -> bool {
    let attempt = || -> Result<bool> {
        if !eligible_for(provider, path) {
            return Ok(false);
        }
        let file = open_source(provider, path)?;
        let metadata = file.metadata()?;
        if metadata.len() == 0 || metadata.len() > limit {
            return Ok(false);
        }
        require_unlocked(&file)?;
        let mut reader = (&file).take(metadata.len());
        let mut buffer = [0_u8; COPY_BUFFER_BYTES];
        loop {
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                return Ok(false);
            }
            if buffer[..count].contains(&b'\n') {
                return Ok(true);
            }
        }
    };
    attempt().unwrap_or(false)
}

#[cfg(all(test, unix))]
mod tests;

#[cfg(all(test, unix))]
mod projection_tests;

#[cfg(all(test, unix))]
mod zero_boundary_tests;

#[cfg(test)]
mod receipt_shape_tests;

#[cfg(all(test, unix))]
mod provider_tests;
