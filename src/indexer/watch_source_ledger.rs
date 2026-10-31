//! Durable source observations for the live watcher.
//!
//! A provider-wide timestamp cannot identify the files left behind by an active
//! writer. Reuse the startup source contract, but certify publication only after
//! every conversation from a stable source has reached a committed lexical
//! batch. Recovery may then skip those sources even while a sibling retains
//! root-wide replay debt.

use super::*;
use crate::franken_sync::compat::TransactionExt;
use crate::storage::sqlite::SourceIngestLedgerEntry;
use std::cell::{Cell, RefCell};

#[cfg(test)]
#[path = "watch_source_ledger_tests.rs"]
mod boundary_tests;

const MAX_RECEIPT_ENTRIES: usize = 65_536;
const MAX_RECEIPT_BYTES: usize = 64 * 1024 * 1024;
const MAX_RECEIPT_ENTRY_BYTES: usize = 256 * 1024;
const MAX_REUSED_PATH_BYTES: usize = 8 * 1024 * 1024;

struct Completion {
    remaining: usize,
    entry: SourceIngestLedgerEntry,
}

#[derive(Default)]
pub(super) struct WatchSourceLedger {
    completions: HashMap<PathBuf, Completion>,
    completion_bytes: usize,
    admitted_keys: BTreeSet<String>,
    invalidate_all_receipts: bool,
    reused_path_bytes: usize,
    pub(super) reused_sources: Vec<PathBuf>,
    pub(super) active_sources_skipped: usize,
    pub(super) unstable_sources: bool,
}

impl WatchSourceLedger {
    /// Discover with no wall-clock cutoff when the connector can identify
    /// sources. This also finds imported/backdated files and restart gaps.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn scan(
        &mut self,
        connector: &dyn Connector,
        connector_name: &str,
        ctx: &ScanContext,
        saved: &HashMap<String, String>,
        allow_reuse: bool,
        replay_canonical: bool,
        active_filter: &ActiveSessionSourceFilter,
        progress: Option<&IndexingProgress>,
        deliver: &mut dyn FnMut(NormalizedConversation) -> Result<bool>,
    ) -> Result<()> {
        if !connector.supports_source_boundaries() {
            return connector
                .scan_with_callback(ctx, &mut |conversation| deliver(conversation).map(|_| ()));
        }
        let mut ctx = ctx.clone();
        ctx.since_ts = None;
        let producer_contract =
            WorkspaceExclusions::from_env()?.bind_contract(env!("CASS_SOURCE_INGEST_CONTRACT"));
        // As in startup, a self-contained provider needs no second inventory
        // merely to prove that it has no sidecars. Unknown policies fail closed.
        let dependencies = if !crate::connectors::source_dependencies::source_dependency_policy(
            connector_name,
        )
        .observes_parent_directory()
        {
            Default::default()
        } else {
            let mut path_bytes = 0usize;
            crate::connectors::source_dependencies::observation::DependencyObservations::capture(
                connector
                    .discover_source_files(&ctx)?
                    .into_iter()
                    .take(MAX_RECEIPT_ENTRIES)
                    .map(|source| source.source_path)
                    .take_while(|path| {
                        path_bytes = path_bytes.saturating_add(path.as_os_str().len());
                        path_bytes <= MAX_REUSED_PATH_BYTES
                    }),
            )
        };
        let primary_before = RefCell::new(None);
        let parent_before = RefCell::new(None);
        let current_source = RefCell::new(None::<DiscoveredSourceFile>);
        let filtered = Cell::new(false);
        let ledger = RefCell::new(self);
        let mut should_scan = |source: &DiscoveredSourceFile| {
            if progress.is_some_and(IndexingProgress::stop_requested) {
                return false;
            }
            // Codex and Claude omit completion when their file changes during
            // parsing. Observe the preceding admission at the next boundary
            // as well, so that omission cannot silently consume its event.
            if let Some(previous) = current_source.borrow_mut().take()
                && super::watch_source_stability::observed_source_changed(
                    &previous,
                    &[],
                    primary_before.borrow().as_ref(),
                    parent_before.borrow().as_ref(),
                    &dependencies.files,
                )
            {
                ledger.borrow_mut().unstable_sources = true;
            }
            if let Some(progress) = progress {
                progress.tick_activity();
            }
            filtered.set(
                scan_path_exclusions_active()
                    && !crate::connectors::source_dependencies::can_reuse_with_path_exclusions(
                        connector_name,
                        &source.provider_slug,
                    ),
            );
            let key = source_ledger_key(source, &ctx);
            let saved_observation = key.as_ref().and_then(|key| saved.get(key));
            let reusable = allow_reuse
                && env!("CASS_SOURCE_INGEST_REUSE") == "true"
                && !filtered.get()
                && saved_observation.is_some_and(|observation| {
                    (!replay_canonical || observation_has_watch_publication(observation))
                        && source_ledger_matches_with_contract(
                            observation,
                            source,
                            &producer_contract,
                        )
                });
            if reusable {
                let mut ledger = ledger.borrow_mut();
                let path_bytes = source.source_path.as_os_str().len();
                if ledger.reused_sources.len() < MAX_RECEIPT_ENTRIES
                    && ledger.reused_path_bytes.saturating_add(path_bytes) <= MAX_REUSED_PATH_BYTES
                {
                    ledger.reused_path_bytes += path_bytes;
                    ledger.reused_sources.push(source.source_path.clone());
                }
                return false;
            }
            if should_skip_active_session_source(
                active_filter,
                source.origin.kind,
                &source.source_path,
            ) {
                ledger.borrow_mut().active_sources_skipped += 1;
                return false;
            }
            if let Some(key) = key {
                let mut ledger = ledger.borrow_mut();
                if !ledger.invalidate_all_receipts {
                    if ledger.admitted_keys.len() < MAX_RECEIPT_ENTRIES {
                        ledger.admitted_keys.insert(key);
                    } else {
                        // An omitted cache entry can still have an old durable
                        // receipt. If the bounded invalidation set fills, clear
                        // the receipt namespace before mutation rather than
                        // allowing an untracked old receipt to certify it.
                        ledger.invalidate_all_receipts = true;
                        ledger.admitted_keys.clear();
                    }
                }
            }
            *primary_before.borrow_mut() = source_file_observation(&source.source_path);
            *parent_before.borrow_mut() = dependencies.parent_for(&source.source_path).cloned();
            *current_source.borrow_mut() = Some(source.clone());
            true
        };
        let mut complete = |completion: &franken_agent_detection::connectors::SourceCompletion| {
            if let Some(progress) = progress {
                progress.check_stop()?;
            }
            if filtered.get() || completion.conversations_emitted == 0 {
                return Ok(());
            }
            let Some(key) = source_ledger_key(&completion.source, &ctx) else {
                return Ok(());
            };
            let Some((primary, dependencies)) = source_observations_after_scan(
                &completion.source,
                &completion.required_sidecars,
                primary_before.borrow().as_ref(),
                parent_before.borrow().as_ref(),
                &dependencies.files,
            ) else {
                if super::watch_source_stability::observed_source_changed(
                    &completion.source,
                    &completion.required_sidecars,
                    primary_before.borrow().as_ref(),
                    parent_before.borrow().as_ref(),
                    &dependencies.files,
                ) {
                    ledger.borrow_mut().unstable_sources = true;
                }
                return Ok(());
            };
            let mut observation = ReceiptBuffer::default();
            if serde_json::to_writer(
                &mut observation,
                &serde_json::json!({
                    "primary": primary,
                    "dependencies": dependencies,
                    "producer_contract": producer_contract,
                    "watch_published": true,
                }),
            )
            .is_err()
            {
                return Ok(());
            }
            let entry = SourceIngestLedgerEntry {
                key,
                observation: String::from_utf8(observation.0)
                    .expect("JSON serialization produces UTF-8"),
            };
            let mut ledger = ledger.borrow_mut();
            let bytes = entry.key.capacity()
                + entry.observation.capacity()
                + completion.source.source_path.as_os_str().len();
            if ledger.completions.len() >= MAX_RECEIPT_ENTRIES
                || ledger.completion_bytes.saturating_add(bytes) > MAX_RECEIPT_BYTES
            {
                // Lack of an optimization receipt never drops source content.
                // The canonical and lexical batches still publish normally.
                return Ok(());
            }
            ledger.completion_bytes += bytes;
            ledger.completions.insert(
                completion.source.source_path.clone(),
                Completion {
                    remaining: completion.conversations_emitted,
                    entry,
                },
            );
            Ok(())
        };
        let result = connector.scan_with_source_boundaries(
            &ctx,
            &mut franken_agent_detection::connectors::SourceScanHooks {
                should_scan_source: Some(&mut should_scan),
                on_source_complete: Some(&mut complete),
            },
            &mut |conversation| {
                if let Some(progress) = progress {
                    progress.check_stop()?;
                }
                if !deliver(conversation)? {
                    filtered.set(true);
                }
                Ok(())
            },
        );
        if let Some(previous) = current_source.borrow_mut().take()
            && super::watch_source_stability::observed_source_changed(
                &previous,
                &[],
                primary_before.borrow().as_ref(),
                parent_before.borrow().as_ref(),
                &dependencies.files,
            )
        {
            ledger.borrow_mut().unstable_sources = true;
        }
        if let Some(progress) = progress {
            progress.check_stop()?;
        }
        result
    }

    /// An earlier publication receipt must not survive another attempted
    /// mutation of that same source. Invalidate before the first transaction;
    /// a crash then leaves replay debt plus an unacknowledged source.
    pub(super) fn begin_publication(
        &self,
        storage: &FrankenStorage,
        debt: &WatchLexicalReplayDebt,
        progress: Option<&IndexingProgress>,
    ) -> Result<()> {
        #[cfg(test)]
        inject_watch_lexical_test_failure(WatchLexicalTestFault::DebtWrite)?;
        let (debt_key, debt_value) = debt.metadata()?;
        persist::with_ephemeral_writer(
            storage,
            false,
            "invalidating watch source receipts",
            |writer| {
                let mut tx = writer.raw().transaction()?;
                tx.execute_compat(
                    "INSERT OR IGNORE INTO meta(key, value) VALUES(?1, ?2)",
                    &[
                        ParamValue::from(debt_key.as_str()),
                        ParamValue::from(debt_value.as_str()),
                    ],
                )?;
                if self.invalidate_all_receipts {
                    if let Some(progress) = progress {
                        progress.check_stop()?;
                    }
                    tx.execute_compat(
                        "DELETE FROM meta WHERE key >= 'source_ingest_v1:' AND key < 'source_ingest_v1;'",
                        &[],
                    )?;
                    if let Some(progress) = progress {
                        progress.tick_activity();
                    }
                } else {
                    for key in &self.admitted_keys {
                        if let Some(progress) = progress {
                            progress.check_stop()?;
                        }
                        tx.execute_compat(
                            "DELETE FROM meta WHERE key = ?1",
                            &[ParamValue::from(key.as_str())],
                        )?;
                        if let Some(progress) = progress {
                            progress.tick_activity();
                        }
                    }
                }
                if let Some(progress) = progress {
                    progress.check_stop()?;
                }
                tx.commit()?;
                Ok(())
            },
        )
    }

    pub(super) fn publish_batch(
        &mut self,
        storage: &FrankenStorage,
        conversations: &[NormalizedConversation],
        completed_debts: &[&WatchLexicalReplayDebt],
        retain_recovery_source: bool,
    ) -> Result<()> {
        let mut ready = Vec::new();
        for conversation in conversations {
            if let Some(completion) = self.completions.get_mut(&conversation.source_path) {
                completion.remaining = completion.remaining.saturating_sub(1);
                if completion.remaining == 0 {
                    ready.push(conversation.source_path.clone());
                }
            }
        }
        if retain_recovery_source {
            // A prior root-wide debt may cover an active or incomplete source
            // whose original identity is unknown. Keep one admitted source
            // unacknowledged until that debt can finish, so the next callback
            // cannot reuse everything and strand recovery without a batch.
            ready.pop();
        }
        if ready.is_empty() && completed_debts.is_empty() {
            return Ok(());
        }
        persist::with_ephemeral_writer(
            storage,
            false,
            "acknowledging published watch sources",
            |writer| {
                let mut tx = writer.raw().transaction()?;
                for path in &ready {
                    let Some(completion) = self.completions.get(path) else {
                        continue;
                    };
                    tx.execute_compat(
                        "INSERT OR REPLACE INTO meta(key, value) VALUES(?1, ?2)",
                        &[
                            ParamValue::from(completion.entry.key.as_str()),
                            ParamValue::from(completion.entry.observation.as_str()),
                        ],
                    )?;
                }
                // The last receipt and its covered debt disappear together.
                // Otherwise a crash between them leaves an all-reused scan
                // with no batch capable of completing the pending recovery.
                for debt in completed_debts {
                    #[cfg(test)]
                    inject_watch_lexical_test_failure(WatchLexicalTestFault::DebtClear)?;
                    let (key, value) = debt.metadata()?;
                    tx.execute_compat(
                        "DELETE FROM meta WHERE key = ?1 AND value = ?2",
                        &[ParamValue::from(key), ParamValue::from(value)],
                    )?;
                }
                tx.commit()?;
                Ok(())
            },
        )?;
        for path in ready {
            self.completions.remove(&path);
        }
        Ok(())
    }
}

fn observation_has_watch_publication(observation: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(observation)
        .ok()
        .is_some_and(|value| value["watch_published"].as_bool() == Some(true))
}

#[derive(Default)]
struct ReceiptBuffer(Vec<u8>);

impl std::io::Write for ReceiptBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let length = self.0.len().saturating_add(bytes.len());
        if length > MAX_RECEIPT_ENTRY_BYTES {
            return Err(std::io::Error::other(
                "watch receipt exceeds its memory budget",
            ));
        }
        if length > self.0.capacity() {
            self.0.reserve_exact(length - self.0.len());
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn receipt_serialization_budget_does_not_retain_an_oversized_payload() {
        let mut buffer = ReceiptBuffer::default();
        buffer
            .write_all(&vec![b'x'; MAX_RECEIPT_ENTRY_BYTES - 1])
            .unwrap();
        buffer.write_all(b"y").unwrap();
        assert!(buffer.write_all(b"z").is_err());
        assert_eq!(buffer.0.len(), MAX_RECEIPT_ENTRY_BYTES);
        assert_eq!(buffer.0.capacity(), MAX_RECEIPT_ENTRY_BYTES);
    }

    #[test]
    fn receipt_budget_rejects_a_single_large_write_before_allocating() {
        let mut buffer = ReceiptBuffer::default();
        assert!(
            buffer
                .write_all(&vec![b'x'; MAX_RECEIPT_ENTRY_BYTES + 1])
                .is_err()
        );
        assert!(buffer.0.is_empty());
        assert_eq!(buffer.0.capacity(), 0);
    }
}
