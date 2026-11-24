//! Give the pinned Claude parser one immutable complete-record view, while
//! every host callback retains the original source and conversation identity.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use franken_agent_detection::connectors::{SourceCompletion, SourceScanHooks};

use crate::connectors::codex::stable_prefix::{
    self, PrefixDeliveryGuard, StablePrefixProvider, StablePrefixReceipt, StablePrefixSnapshot,
};
use crate::connectors::{
    Connector, DiscoveredSourceFile, DiscoveredSourceRole, NormalizedConversation, ScanContext,
    ScanRoot,
};

const PROVIDER: StablePrefixProvider = StablePrefixProvider::ClaudeCode;
const SOURCE_LIMIT_ENV: &str = "CASS_CLAUDE_MAX_SOURCE_BYTES";
const DEFAULT_SOURCE_BYTES: u64 = 100 * 1024 * 1024;
const MAX_CONFIGURED_SOURCE_BYTES: u64 = 1024 * 1024 * 1024;

fn invalid_source_budget() -> anyhow::Error {
    anyhow::anyhow!(
        "{SOURCE_LIMIT_ENV} must be a decimal byte count from 1 through {MAX_CONFIGURED_SOURCE_BYTES}; unset it for the 100 MiB default"
    )
}

fn parse_source_read_budget(value: &str) -> Result<u64> {
    let value = value.trim();
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid_source_budget());
    }
    let bytes = value.parse::<u64>().map_err(|_| invalid_source_budget())?;
    if !(1..=MAX_CONFIGURED_SOURCE_BYTES).contains(&bytes) {
        return Err(invalid_source_budget());
    }
    Ok(bytes)
}

pub(crate) fn source_read_budget() -> Result<u64> {
    match dotenvy::var(SOURCE_LIMIT_ENV) {
        Ok(value) => parse_source_read_budget(&value),
        Err(dotenvy::Error::EnvVar(std::env::VarError::NotPresent)) => Ok(DEFAULT_SOURCE_BYTES),
        // Invalid/non-Unicode configuration is never echoed or silently reset.
        Err(_) => Err(invalid_source_budget()),
    }
}

pub(crate) fn can_attempt_active_jsonl_source(path: &Path) -> bool {
    source_read_budget().is_ok_and(|limit| stable_prefix::can_attempt_for(PROVIDER, path, limit))
}

pub(crate) fn guarded_active_jsonl_source(path: &Path) -> bool {
    stable_prefix::guarded_prefix_delivery_for(PROVIDER, path)
}

fn source_path_key(path: &Path) -> PathBuf {
    // Match the pinned parser's dedupe policy: canonicalize only symlink
    // leaves. Resolving every path would merge distinct configured roots.
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
        }
        _ => path.to_path_buf(),
    }
}

fn scoped_context(ctx: &ScanContext, source: &DiscoveredSourceFile, path: &Path) -> ScanContext {
    let root = ctx
        .scan_roots
        .iter()
        .find(|root| root.path == source.scan_root && root.origin == source.origin)
        .map(|root| root.with_path(path.to_path_buf()))
        .unwrap_or_else(|| {
            ScanRoot::remote(path.to_path_buf(), source.origin.clone(), source.platform)
        });
    ScanContext {
        data_dir: ctx.data_dir.clone(),
        scan_roots: vec![root],
        // Discovery already applied the source cutoff. A private view's mtime
        // is not a second source-freshness filter.
        since_ts: None,
        progress_tick: ctx.progress_tick.clone(),
    }
}

struct SourceIdentity {
    external_id: Option<String>,
    parent_session_id: Option<String>,
    sidechain: bool,
}

impl SourceIdentity {
    fn observe(source: &DiscoveredSourceFile) -> Self {
        // FAD uses a directory root verbatim, but an explicit-file root is
        // relative to its nearest projects ancestor (or its parent). Preserve
        // this distinction so targeted watches update the existing session.
        let base = if source.scan_root == source.source_path || source.scan_root.is_file() {
            source
                .source_path
                .ancestors()
                .find(|ancestor| {
                    ancestor.is_dir() && ancestor.file_name().is_some_and(|name| name == "projects")
                })
                .or_else(|| source.source_path.parent())
        } else {
            Some(source.scan_root.as_path())
        };
        let external_id = base
            .and_then(|base| source.source_path.strip_prefix(base).ok())
            .and_then(Path::to_str)
            .or_else(|| {
                source
                    .source_path
                    .file_name()
                    .and_then(|name| name.to_str())
            })
            .map(str::to_owned);
        let mut parent_session_id = None;
        let mut sidechain = false;
        let mut previous = None;
        for component in source.source_path.components() {
            let component = component.as_os_str().to_str();
            if component == Some("subagents") {
                sidechain = true;
                parent_session_id = previous.map(str::to_owned);
                break;
            }
            previous = component;
        }
        Self {
            external_id,
            parent_session_id,
            sidechain,
        }
    }

    fn restore(&self, source: &DiscoveredSourceFile, conversation: &mut NormalizedConversation) {
        conversation.source_path.clone_from(&source.source_path);
        // Desktop sidecars use the recorded CLI/session ID, not a path ID.
        if conversation.metadata["source"] != "claude_code_desktop_sidecar" {
            conversation.external_id.clone_from(&self.external_id);
        }
        conversation.metadata["sidechain"] = self.sidechain.into();
        conversation.metadata["parentSessionId"] = self.parent_session_id.clone().into();
    }
}

fn attach_receipt(
    conversation: &mut NormalizedConversation,
    receipt: &StablePrefixReceipt,
) -> Result<()> {
    let metadata = conversation
        .metadata
        .as_object_mut()
        .context("Claude parser metadata is not an object")?;
    let cass = metadata
        .entry("cass")
        .or_insert_with(|| serde_json::json!({}));
    let cass = cass
        .as_object_mut()
        .context("Claude parser CASS metadata is not an object")?;
    cass.insert(
        PROVIDER.metadata_key().to_owned(),
        serde_json::to_value(receipt)?,
    );
    Ok(())
}

fn scan_strict_source(
    inner: &dyn Connector,
    ctx: &ScanContext,
    source: &DiscoveredSourceFile,
    identity: &SourceIdentity,
    hooks: &mut SourceScanHooks<'_>,
    on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    consumer_failed: &Cell<bool>,
) -> Result<()> {
    let parser_ctx = scoped_context(ctx, source, &source.source_path);
    let mut before = |observed: &DiscoveredSourceFile| observed.source_path == source.source_path;
    let mut complete = |done: &SourceCompletion| {
        anyhow::ensure!(
            done.source.source_path == source.source_path && done.required_sidecars.is_empty(),
            "Claude parser completed an unexpected source"
        );
        hooks.on_source_complete.as_mut().map_or(Ok(()), |sink| {
            sink(&SourceCompletion {
                source: source.clone(),
                required_sidecars: Vec::new(),
                conversations_emitted: done.conversations_emitted,
            })
            .inspect_err(|_| consumer_failed.set(true))
        })
    };
    inner.scan_with_source_boundaries(
        &parser_ctx,
        &mut SourceScanHooks {
            should_scan_source: Some(&mut before),
            on_source_complete: Some(&mut complete),
        },
        &mut |mut conversation| {
            anyhow::ensure!(
                conversation.agent_slug == PROVIDER.slug()
                    && conversation.source_path == source.source_path,
                "Claude parser delivered an unexpected source"
            );
            identity.restore(source, &mut conversation);
            on_conversation(conversation).inspect_err(|_| consumer_failed.set(true))
        },
    )
}

struct PrefixProjection<'a> {
    identity: &'a SourceIdentity,
    snapshot: &'a StablePrefixSnapshot,
    previous: Option<&'a StablePrefixReceipt>,
}

fn scan_prefix_source(
    inner: &dyn Connector,
    ctx: &ScanContext,
    source: &DiscoveredSourceFile,
    hooks: &mut SourceScanHooks<'_>,
    on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    consumer_failed: &Cell<bool>,
    projection: PrefixProjection<'_>,
) -> Result<()> {
    let PrefixProjection {
        identity,
        snapshot,
        previous,
    } = projection;
    let parser_ctx = scoped_context(ctx, source, snapshot.path());
    let seen = Cell::new(false);
    let completed = Cell::new(false);
    let emitted = Cell::new(0usize);
    let receipt = RefCell::new(None::<StablePrefixReceipt>);
    let mut before = |observed: &DiscoveredSourceFile| {
        let matches =
            observed.source_path == snapshot.path() && observed.provider_slug == PROVIDER.slug();
        seen.set(seen.get() || matches);
        matches
    };
    let mut complete = |done: &SourceCompletion| {
        anyhow::ensure!(
            done.source.source_path == snapshot.path()
                && done.required_sidecars.is_empty()
                && done.conversations_emitted == emitted.get(),
            "Claude prefix parser completed an unexpected source"
        );
        completed.set(true);
        Ok(())
    };
    inner.scan_with_source_boundaries(
        &parser_ctx,
        &mut SourceScanHooks {
            should_scan_source: Some(&mut before),
            on_source_complete: Some(&mut complete),
        },
        &mut |mut conversation| {
            anyhow::ensure!(
                conversation.agent_slug == PROVIDER.slug()
                    && conversation.source_path == snapshot.path()
                    && emitted.get() == 0,
                "Claude prefix parser delivered an unexpected source"
            );
            snapshot.validate(ctx.progress_tick.as_deref())?;
            identity.restore(source, &mut conversation);
            let evidence = snapshot.receipt_for(Some(&conversation), previous)?;
            attach_receipt(&mut conversation, &evidence)?;
            {
                let _delivery = PrefixDeliveryGuard::enter(evidence.clone());
                on_conversation(conversation).inspect_err(|_| consumer_failed.set(true))?;
            }
            snapshot.validate(ctx.progress_tick.as_deref())?;
            *receipt.borrow_mut() = Some(evidence);
            emitted.set(1);
            Ok(())
        },
    )?;
    anyhow::ensure!(
        seen.get() && (completed.get() || emitted.get() == 0),
        "Claude immutable parser view was not completely consumed"
    );
    let Some(receipt) = receipt.into_inner() else {
        snapshot.validate(ctx.progress_tick.as_deref())?;
        anyhow::ensure!(
            previous.is_none_or(|receipt| receipt.message_count == 0),
            "Claude source no longer projects its previously published messages"
        );
        let evidence = snapshot.receipt_for(None, None)?;
        anyhow::ensure!(
            evidence.committed_bytes == evidence.admitted_bytes,
            "Claude source has an unfinished tail and no complete messages"
        );
        // Preserve FAD's no-message contract: no invented source completion.
        return Ok(());
    };
    let receipt = snapshot.finish_receipt(receipt)?;
    let mut completed_source = source.clone();
    if receipt.partial {
        // Generic consumers cannot mistake this callback for whole-file EOF.
        completed_source.size_bytes = None;
        completed_source.modified_at_ms = None;
    }
    let _completion = PrefixDeliveryGuard::enter_completion(receipt);
    hooks.on_source_complete.as_mut().map_or(Ok(()), |sink| {
        sink(&SourceCompletion {
            source: completed_source,
            required_sidecars: Vec::new(),
            conversations_emitted: emitted.get(),
        })
        .inspect_err(|_| consumer_failed.set(true))
    })
}

pub(super) fn scan(
    inner: &dyn Connector,
    ctx: &ScanContext,
    hooks: &mut SourceScanHooks<'_>,
    on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
) -> Result<()> {
    scan_with_limit(inner, ctx, hooks, on_conversation, source_read_budget()?)
}

fn scan_with_limit(
    inner: &dyn Connector,
    ctx: &ScanContext,
    hooks: &mut SourceScanHooks<'_>,
    on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    limit: u64,
) -> Result<()> {
    let sources = inner.discover_source_files(ctx)?;
    let mut seen = HashSet::new();
    let mut failures = crate::connectors::codex::SourceAdmissionFailures::default();
    for source in sources {
        if !seen.insert(source_path_key(&source.source_path)) {
            continue;
        }
        if let Some(tick) = ctx.progress_tick.as_deref() {
            tick();
        }
        stable_prefix::clear_previous_prefix_receipt();
        let source = source.with_fs_metadata();
        if !hooks
            .should_scan_source
            .as_mut()
            .is_none_or(|predicate| predicate(&source))
        {
            stable_prefix::clear_previous_prefix_receipt();
            continue;
        }
        let consumer_failed = Cell::new(false);
        let result = (|| {
            let previous =
                stable_prefix::take_previous_prefix_receipt_for(PROVIDER, &source.source_path)?;
            let identity = SourceIdentity::observe(&source);
            let metadata = std::fs::symlink_metadata(&source.source_path)?;
            if !metadata.is_file() && !metadata.file_type().is_symlink() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "Claude source changed after discovery",
                )
                .into());
            }
            let eligible = source.provider_slug == PROVIDER.slug()
                && source.role == DiscoveredSourceRole::PrimarySessionLog
                && stable_prefix::eligible_for(PROVIDER, &source.source_path);
            let within_budget = metadata.len() <= limit;
            if eligible && !within_budget && previous.is_some() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::FileTooLarge,
                    "Claude prefix source exceeds its admitted byte budget; raise CASS_CLAUDE_MAX_SOURCE_BYTES within the 1 GiB ceiling",
                ).into());
            }
            let snapshot = if eligible && within_budget {
                StablePrefixSnapshot::capture_for(
                    PROVIDER,
                    &source.source_path,
                    limit,
                    previous.as_ref(),
                    ctx.progress_tick.as_deref(),
                )?
            } else {
                anyhow::ensure!(
                    previous.is_none(),
                    "Claude source requires its prior prefix admission; reconciliation is required"
                );
                None
            };
            if let Some(snapshot) = snapshot {
                scan_prefix_source(
                    inner,
                    ctx,
                    &source,
                    hooks,
                    on_conversation,
                    &consumer_failed,
                    PrefixProjection {
                        identity: &identity,
                        snapshot: &snapshot,
                        previous: previous.as_ref(),
                    },
                )
            } else {
                scan_strict_source(
                    inner,
                    ctx,
                    &source,
                    &identity,
                    hooks,
                    on_conversation,
                    &consumer_failed,
                )
            }
        })();
        if let Err(error) = result {
            if consumer_failed.get() {
                return Err(error);
            }
            failures.record(&source, error);
        }
    }
    failures.finish(Ok(()))
}

#[cfg(all(test, unix))]
mod tests;
