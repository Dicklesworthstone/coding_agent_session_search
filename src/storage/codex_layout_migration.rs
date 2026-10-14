//! Read-only archive proof followed by an opt-in Codex layout identity receipt.
//! No conversation, message, search document, source file or source configuration
//! is rewritten. The existing indexer's append path remains the only DB writer.

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use super::sqlite::SqliteStorage;
use crate::connectors::codex::CodexConnector;
use crate::connectors::codex::layout_identity::{
    LayoutPair, Receipt, ReceiptLock, Snapshot, read_snapshot, receipt_path,
};
use crate::connectors::{
    Connector, NormalizedConversation, NormalizedMessage, ScanContext, ScanRoot,
};
use crate::franken_sync::compat::{ConnectionExt, RowExt};
use crate::franken_sync::{Connection, params};
use crate::model::types::{Message, MessageRole};
use crate::sources::config::SourcesConfig;

#[derive(Debug)]
pub struct MigrationOptions {
    pub data_dir: PathBuf,
    pub source_id: String,
    pub from: PathBuf,
    pub to: PathBuf,
    pub apply: bool,
}

#[derive(Debug, Serialize)]
pub struct MigrationReport {
    pub applied: bool,
    pub already_registered: bool,
    pub source_id: String,
    pub canonical_conversation_id: i64,
    pub canonical_external_id: String,
    pub retained_messages: usize,
    pub receipt_path: PathBuf,
    pub flat_path: PathBuf,
    pub dated_path: PathBuf,
}

/// The existing archive must exist, so canonicalize resolves exactly the same
/// pathname as indexer/archive_lease.rs. Share its permanent lock inode and std
/// OS lock; never create a separate "migration" lock that an indexer can miss.
fn archive_lease(database: &Path) -> Result<(PathBuf, File)> {
    let database = fs::canonicalize(database).context("open an existing CASS archive first")?;
    let mut name = database.as_os_str().to_os_string();
    name.push(".cass-index-run.lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(PathBuf::from(name))?;
    file.try_lock()
        .context("archive is being indexed; stop the watcher and retry migration")?;
    Ok((database, file))
}

// Keep provenance selection and message reads in one database snapshot, even
// if a non-indexing reader/writer also has the archive open.
struct ArchiveSnapshot<'a>(&'a Connection);

impl<'a> ArchiveSnapshot<'a> {
    fn begin(connection: &'a Connection) -> Result<Self> {
        connection.execute("BEGIN DEFERRED")?;
        Ok(Self(connection))
    }
}

impl Drop for ArchiveSnapshot<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.0.execute("ROLLBACK") {
            tracing::warn!(%error, "failed to release Codex migration read snapshot");
        }
    }
}

fn expanded_root(root: &str) -> Option<PathBuf> {
    let path = if let Some(rest) = root.strip_prefix("~/") {
        dirs::home_dir()?.join(rest)
    } else {
        PathBuf::from(root)
    };
    fs::canonicalize(path).ok()
}

fn verify_ownership(config: &SourcesConfig, source_id: &str, pair: &LayoutPair) -> Result<()> {
    for path in [&pair.flat, &pair.dated] {
        let owners: Vec<_> = config
            .sources
            .iter()
            .filter(|source| {
                !source.is_remote()
                    && source
                        .paths
                        .iter()
                        .filter_map(|root| expanded_root(root))
                        .any(|root| path.starts_with(root))
            })
            .collect();
        ensure!(
            owners.len() == 1 && owners[0].name == source_id,
            "each rollout must belong unambiguously to the same configured named local source"
        );
        owners[0].validate()?;
    }
    Ok(())
}

fn same_role(stored: &MessageRole, parsed: &str) -> bool {
    match stored {
        MessageRole::User => parsed == "user",
        MessageRole::Agent => matches!(parsed, "assistant" | "agent"),
        MessageRole::Tool => parsed == "tool",
        MessageRole::System => parsed == "system",
        MessageRole::Other(role) => role == parsed,
    }
}

fn same_message(stored: &Message, parsed: &NormalizedMessage) -> bool {
    stored.idx == parsed.idx
        && same_role(&stored.role, &parsed.role)
        && stored.author == parsed.author
        && stored.created_at == parsed.created_at
        && stored.content == parsed.content
        && stored.extra_json == parsed.extra
        && stored.snippets.len() == parsed.snippets.len()
        && stored.snippets.iter().zip(&parsed.snippets).all(|(a, b)| {
            a.file_path == b.file_path
                && a.start_line == b.start_line
                && a.end_line == b.end_line
                && a.language == b.language
                && a.snippet_text == b.snippet_text
        })
}

fn covers(stored: &[Message], parsed: &NormalizedConversation) -> bool {
    !stored.is_empty()
        && stored.len() <= parsed.messages.len()
        && stored
            .iter()
            .zip(&parsed.messages)
            .all(|(a, b)| same_message(a, b))
}

fn parse_pair(pair: &LayoutPair) -> Result<Vec<NormalizedConversation>> {
    // Use an isolated scan context, not the live receipt registry: dry-run and
    // archive proof must never advance a receipt as a side effect of parsing.
    let scratch = tempfile::tempdir()?;
    let ctx = ScanContext::with_roots(
        scratch.path().to_path_buf(),
        vec![
            ScanRoot::local(pair.flat.clone()),
            ScanRoot::local(pair.dated.clone()),
        ],
        None,
    );
    let parsed = CodexConnector::new().scan(&ctx)?;
    ensure!(
        parsed.len() == 2,
        "both migration rollouts must produce one complete conversation"
    );
    for path in [&pair.flat, &pair.dated] {
        ensure!(
            parsed
                .iter()
                .filter(|conversation| {
                    conversation.agent_slug == "codex" && &conversation.source_path == path
                })
                .count()
                == 1,
            "Codex migration parsing did not preserve the selected source boundaries"
        );
    }
    Ok(parsed)
}

fn prove_pair(pair: &LayoutPair, budget: u64) -> Result<(Snapshot, Snapshot)> {
    let flat = read_snapshot(&pair.flat, &[], budget, None)?;
    let dated = read_snapshot(&pair.dated, &[flat.checkpoint.bytes], budget, None)?;
    ensure!(
        flat.session_id == pair.session_id
            && dated.session_id == pair.session_id
            && flat.metadata_hash == dated.metadata_hash,
        "conflicting native Codex session metadata; no identity was migrated"
    );
    if dated.checkpoint.bytes >= flat.checkpoint.bytes {
        ensure!(
            dated.prefixes.get(&flat.checkpoint.bytes) == Some(&flat.checkpoint.blake3),
            "conflicting Codex contents; no identity was migrated"
        );
    } else {
        let checked = read_snapshot(&pair.flat, &[dated.checkpoint.bytes], budget, None)?;
        ensure!(
            checked.checkpoint == flat.checkpoint
                && checked.prefixes.get(&dated.checkpoint.bytes) == Some(&dated.checkpoint.blake3),
            "conflicting or changing Codex contents; no identity was migrated"
        );
    }
    Ok((flat, dated))
}

/// Register a proven pair before indexing the second layout. Existing duplicates
/// are an explicit refusal, not an excuse to delete rows and invalidate IDs.
pub fn migrate(options: &MigrationOptions) -> Result<MigrationReport> {
    ensure!(
        !options.source_id.is_empty(),
        "a named local source is required"
    );
    let data_dir = fs::canonicalize(&options.data_dir).context("resolve CASS data directory")?;
    let (database, _archive_lease) = archive_lease(&data_dir.join("agent_search.db"))?;
    let pair = LayoutPair::discover(&options.from, &options.to)?;
    verify_ownership(&SourcesConfig::load()?, &options.source_id, &pair)?;
    let storage = SqliteStorage::open_readonly(&database)?;
    let _snapshot = ArchiveSnapshot::begin(storage.raw())?;
    let kinds = storage.raw().query_map_collect(
        "SELECT kind FROM sources WHERE id = ?1 LIMIT 2",
        params![options.source_id.clone()],
        |row| row.get_typed::<String>(0),
    )?;
    ensure!(
        kinds.len() == 1 && kinds[0] == "local",
        "archive source ownership is missing, ambiguous or non-local"
    );
    // Do not pick a convenient compatible twin from several candidates. Include
    // scoped suffixes so a third directory identity makes the proof ambiguous.
    let candidates = storage.raw().query_map_collect(
        "SELECT c.id, c.external_id, c.source_path FROM conversations c
         JOIN agents a ON a.id = c.agent_id
         WHERE c.source_id = ?1 AND a.slug = 'codex' AND
           (c.external_id = ?2 OR substr(c.external_id, -length(?2)-1) = '/' || ?2
            OR substr(c.external_id, -length(?2)-1) = char(92) || ?2
            OR c.source_path = ?3 OR c.source_path = ?4)
         LIMIT 3",
        params![
            options.source_id.clone(),
            pair.bare_id.clone(),
            pair.flat.to_string_lossy().into_owned(),
            pair.dated.to_string_lossy().into_owned()
        ],
        |row| {
            Ok((
                row.get_typed::<i64>(0)?,
                row.get_typed::<String>(1)?,
                row.get_typed::<String>(2)?,
            ))
        },
    )?;
    ensure!(
        candidates.len() == 1,
        "migration needs exactly one indexed same-source Codex identity; existing duplicates or missing evidence require separate review"
    );
    let (conversation_id, external_id, original_path) = &candidates[0];
    ensure!(
        pair.contains_external_id(external_id),
        "existing Codex identity has an unrelated directory scope"
    );
    let original_path = fs::canonicalize(original_path).context(
        "the indexed original rollout must still be accessible when registering migration",
    )?;
    ensure!(
        original_path == pair.flat || original_path == pair.dated,
        "indexed source path does not match either migration input"
    );
    // Parsing initializes the same configured admission budget as normal CASS.
    let parsed = parse_pair(&pair)?;
    let budget = franken_agent_detection::connectors::codex::codex_rollout_byte_budget();
    let (flat, dated) = prove_pair(&pair, budget)?;
    let stored = storage.fetch_messages(*conversation_id)?;
    ensure!(
        parsed
            .iter()
            .any(|conversation| covers(&stored, conversation)),
        "stored Codex history conflicts with the supplied rollouts; no identity was migrated"
    );
    // A second parse of these bounded inputs closes the proof/parse interval.
    // Compare projection too: raw proof alone cannot certify the DB history.
    let reparsed = parse_pair(&pair)?;
    ensure!(
        serde_json::to_value(&parsed)? == serde_json::to_value(&reparsed)?,
        "Codex migration inputs changed during archive validation"
    );
    let (flat_after, dated_after) = prove_pair(&pair, budget)?;
    ensure!(
        flat.checkpoint == flat_after.checkpoint && dated.checkpoint == dated_after.checkpoint,
        "Codex migration inputs changed during prefix validation"
    );
    let (baseline, head) = if flat.checkpoint.bytes <= dated.checkpoint.bytes {
        (flat.checkpoint.clone(), dated.checkpoint.clone())
    } else {
        (dated.checkpoint.clone(), flat.checkpoint.clone())
    };
    let mut receipt = Receipt {
        version: 1,
        source_id: options.source_id.clone(),
        agent: "codex".into(),
        canonical_conversation_id: *conversation_id,
        canonical_external_id: external_id.clone(),
        pair: pair.clone(),
        metadata_hash: flat.metadata_hash,
        baseline,
        head,
        flat_snapshot: flat.checkpoint,
        dated_snapshot: dated.checkpoint,
    };
    receipt.validate()?;
    let path = receipt_path(&data_dir, &options.source_id, &pair.bare_id);
    let previous = ReceiptLock::load_path(&path)?;
    if let Some(mut existing) = previous.clone() {
        ensure!(
            existing.source_id == receipt.source_id
                && existing.canonical_conversation_id == receipt.canonical_conversation_id
                && existing.canonical_external_id == receipt.canonical_external_id
                && existing.pair == receipt.pair,
            "an existing migration receipt belongs to a different archive identity"
        );
        existing.admit_path(&pair.flat, budget, None)?;
        existing.admit_path(&pair.dated, budget, None)?;
        receipt = existing;
    }
    if options.apply {
        let lock = ReceiptLock::acquire(path.clone())?;
        ensure!(
            lock.load()? == previous,
            "Codex receipt changed concurrently; retry migration"
        );
        if previous.as_ref() != Some(&receipt) {
            lock.write(&receipt)?;
        }
    }
    Ok(MigrationReport {
        applied: options.apply,
        already_registered: previous.is_some(),
        source_id: options.source_id.clone(),
        canonical_conversation_id: *conversation_id,
        canonical_external_id: external_id.clone(),
        retained_messages: stored.len(),
        receipt_path: path,
        flat_path: pair.flat,
        dated_path: pair.dated,
    })
}
