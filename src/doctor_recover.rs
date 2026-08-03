//! Corrupt-archive recovery surfaces for `cass doctor` (#285).
//!
//! When the read-only pre-index health gate refuses to index because the
//! canonical `agent_search.db` is corrupt, the operator previously hit a wall:
//! `doctor repair` refuses an unreadable archive, a stock-sqlite `.recover`
//! rebuild is rejected by frankensqlite on readonly open, and the only working
//! path was a hand-rolled JSONL reconstruction from cass's own preserved
//! events. This module turns that working recovery into first-class commands:
//!
//! * [`run_doctor_recover_from_archive`] rebuilds the source JSONL tree from the
//!   canonical archive's preserved `extra_json`/`extra_bin` envelopes so the
//!   data can be re-ingested into a fresh, frankensqlite-native archive — no
//!   `.recover` and no external SQLite tool needed.
//! * [`run_doctor_rebuild_canonical_fts`] inspects exact FTS5 parity, resumes
//!   partial shadows in bounded batches, transactionally creates an absent
//!   shadow, and refuses destructive in-place work on unqueryable artifacts.
//! * [`run_doctor_cleanup_interrupted_artifacts`] quarantines interrupted
//!   `raw_mirror_capture` staging dirs that otherwise block doctor mutation,
//!   without forcing the operator to `rm` inside cass's own data dir.
//!
//! None of these surfaces ever delete canonical rows or source data: recovery
//! is additive (writes reconstructed files), the FTS5 shadow is fully
//! rebuildable from the canonical `messages`, and interrupted artifacts are
//! moved into a quarantine dir rather than deleted.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::storage::sqlite::{
    FrankenStorage, FtsConsistencyRepair, FtsShadowParity, FtsShadowParityStatus,
};
use crate::{CliError, CliResult, RobotFormat, default_data_dir};

/// Page size for streaming conversations during reconstruction. Keeps memory
/// bounded on multi-GB archives (the exact failure surface from #285/#266).
const RECOVER_CONVERSATION_PAGE: i64 = 256;

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn resolve_db_path(data_dir: &Path, db_override: Option<&Path>) -> PathBuf {
    db_override
        .map(Path::to_path_buf)
        .unwrap_or_else(|| data_dir.join("agent_search.db"))
}

fn io_error(message: impl Into<String>, hint: Option<&str>) -> CliError {
    CliError {
        code: 14,
        kind: "io",
        message: message.into(),
        hint: hint.map(str::to_string),
        retryable: true,
    }
}

fn storage_error(message: impl Into<String>, hint: Option<&str>) -> CliError {
    CliError {
        code: 13,
        kind: "storage",
        message: message.into(),
        hint: hint.map(str::to_string),
        retryable: false,
    }
}

/// True when an FTS-repair failure is the frankensqlite FTS5 segment-writer
/// leaf-offset ceiling rather than corruption of the operator's data (GH #369).
///
/// frankensqlite writes exactly one segment leaf per flush and stores each
/// term's byte offset inside that leaf in a `u16`. When a single insert batch's
/// combined terms + doclists encode past 65,535 bytes, `Fts5SegmentLeaf::encode`
/// hard-fails with `segment leaf term offset exceeds u16` (surfaced as
/// `fts5: corrupt %_data record: …`) and the failure-atomic rebuild rolls back
/// without publishing a partial shadow. This is a *content-dependent, sticky*
/// engine limitation — not archive corruption — so it deserves a distinct,
/// reassuring operator diagnostic instead of the generic storage-error wall.
/// Note the `gh362` overlong-*term* tokenizer cap (`FTS5_MAX_TERM_BYTES`) does
/// not address this: the overflow is cumulative across many in-cap terms, not a
/// single oversized token.
fn is_fts5_oversized_leaf_error(err: &anyhow::Error) -> bool {
    // Match the full rendered chain so it is robust to however the fsqlite
    // error was wrapped on the way up (context strings, `{e:#}`, etc.).
    let rendered = format!("{err:#}");
    rendered.contains("segment leaf term offset exceeds u16")
        || rendered.contains("segment leaf rowid offset exceeds u16")
        || rendered.contains("segment leaf footer offset exceeds u16")
        || rendered.contains("segment footer offset exceeds u16")
        || (rendered.contains("corrupt %_data record") && rendered.contains("segment leaf"))
}

/// #368 defect 3: does this error indicate the FTS5 `%_data` shadow structure
/// is corrupt enough to fail an ordinary open during the schema reload (e.g.
/// "structure segment count exceeds FTS5 maximum")? Such an archive can't be
/// opened normally to repair it, but CAN be opened with FTS5 hydration deferred
/// and then rebuilt by dropping + recreating the shadow from canonical. The
/// oversized-*leaf* (gh#369) shape is excluded — that is a content-dependent
/// write-time engine limitation with its own reassuring diagnostic, not a
/// persisted open-blocking corruption.
fn is_fts5_shadow_open_corruption_error(err: &anyhow::Error) -> bool {
    let rendered = format!("{err:#}");
    rendered.contains("corrupt %_data record") && !is_fts5_oversized_leaf_error(err)
}

/// The distinct, non-alarming diagnostic for the GH #369 oversized-leaf case:
/// canonical rows and the Tantivy index are intact and fully serve search; only
/// the optional SQLite-side FTS5 shadow cannot be materialized for this corpus.
fn fts5_oversized_leaf_shadow_error(db_path: &Path) -> CliError {
    CliError {
        code: 13,
        kind: "fts5-oversized-leaf-shadow-unbuildable",
        message: format!(
            "the canonical SQLite FTS5 shadow cannot be built for {} because a single insert \
             batch in this corpus encodes past the frankensqlite FTS5 segment-leaf u16 offset \
             ceiling (one-leaf-per-segment limitation, GH #369) — this is an engine limitation, \
             not corruption of your archive, and the failed rebuild was rolled back without \
             publishing a partial shadow",
            db_path.display()
        ),
        hint: Some(
            "No action is needed and no data was lost: the canonical SQLite tables and the \
             Tantivy lexical index are intact and fully serve search — only the optional \
             SQLite-side `fts_messages` shadow is affected, and `cass doctor check` stays \
             healthy. This is tracked upstream for a multi-leaf FTS5 segment writer; re-run \
             `--rebuild-canonical-fts --yes` once the pinned frankensqlite build ships that fix."
                .to_string(),
        ),
        retryable: false,
    }
}

fn print_json(envelope: &serde_json::Value) -> CliResult<()> {
    let rendered = serde_json::to_string_pretty(envelope).map_err(|e| CliError {
        code: 9,
        kind: "internal",
        message: format!("serialize recovery envelope: {e}"),
        hint: None,
        retryable: false,
    })?;
    println!("{rendered}");
    Ok(())
}

/// One reconstructed session file (or a skip with the reason).
#[derive(Debug)]
struct ReconstructedSession {
    conversation_id: i64,
    external_id: Option<String>,
    relative_or_source_path: String,
    written_path: Option<PathBuf>,
    line_count: usize,
    skipped_reason: Option<String>,
}

/// Compute the on-disk output path for a reconstructed session.
///
/// We deliberately do NOT write back over the original `source_path`: the
/// recovery target is an operator-chosen directory so nothing existing is
/// clobbered. Each session is keyed by its `external_id` when present (stable,
/// collision-free across machines) and otherwise by its conversation id, with
/// the original file name preserved as a `.jsonl` suffix for readability.
fn reconstruction_output_path(
    target_dir: &Path,
    conversation_id: i64,
    external_id: Option<&str>,
    source_path: &Path,
) -> PathBuf {
    let stem = external_id
        .map(sanitize_path_component)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("conversation-{conversation_id}"));
    // Preserve a hint of the original file name without trusting it as a path.
    let original_hint = source_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .map(|s| sanitize_path_component(&s))
        .filter(|s| !s.is_empty());
    let file_name = match original_hint {
        Some(hint) if hint != stem => format!("{stem}__{hint}.jsonl"),
        _ => format!("{stem}.jsonl"),
    };
    target_dir.join(file_name)
}

/// Replace path-unsafe characters so reconstructed file names never escape the
/// recovery dir or collide on case-insensitive filesystems.
fn sanitize_path_component(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches('.')
        .to_string()
}

/// Rebuild the source JSONL tree from the canonical archive's preserved events.
///
/// `target_dir` receives one `.jsonl` file per reconstructable conversation.
/// The canonical archive is opened read-only and never mutated. After this
/// completes the operator can `cass index --full` over `target_dir` to produce
/// a fresh frankensqlite-native archive.
pub fn run_doctor_recover_from_archive(
    data_dir_override: Option<PathBuf>,
    db_override: Option<PathBuf>,
    target_dir: PathBuf,
    structured_format: Option<RobotFormat>,
) -> CliResult<()> {
    let data_dir = data_dir_override.unwrap_or_else(default_data_dir);
    let db_path = resolve_db_path(&data_dir, db_override.as_deref());

    if !db_path.exists() {
        return Err(storage_error(
            format!(
                "canonical archive {} does not exist; nothing to recover from",
                db_path.display()
            ),
            Some(
                "Point --db at the archive, or restore a backup with 'cass doctor backups restore'.",
            ),
        ));
    }

    // Read-only open: recovery must never widen the corruption or take a write
    // lock on a fragile archive.
    let storage = FrankenStorage::open_readonly(&db_path).map_err(|e| {
        storage_error(
            format!(
                "could not open canonical archive {} read-only for recovery: {e:#}",
                db_path.display()
            ),
            Some(
                "If even read-only open fails, the page store itself is unreadable; restore from a \
                 backup ('cass doctor backups list') or a remote mirror.",
            ),
        )
    })?;

    let total = storage
        .total_conversation_count()
        .map_err(|e| storage_error(format!("counting conversations: {e:#}"), None))?;

    std::fs::create_dir_all(&target_dir).map_err(|e| {
        io_error(
            format!(
                "could not create recovery target dir {}: {e}",
                target_dir.display()
            ),
            None,
        )
    })?;

    let mut results: Vec<ReconstructedSession> = Vec::new();
    let mut written = 0usize;
    let mut skipped = 0usize;
    let mut total_lines = 0usize;

    let mut offset: i64 = 0;
    loop {
        let conversations = storage
            .list_conversations(RECOVER_CONVERSATION_PAGE, offset)
            .map_err(|e| {
                storage_error(
                    format!("listing conversations at offset {offset}: {e:#}"),
                    None,
                )
            })?;
        if conversations.is_empty() {
            break;
        }
        let page_len = conversations.len() as i64;

        for conversation in conversations {
            let Some(conversation_id) = conversation.id else {
                continue;
            };
            let source_path_display = conversation.source_path.display().to_string();

            let lines = match storage.reconstruct_source_jsonl_for_conversation(conversation_id) {
                Ok(lines) => lines,
                Err(e) => {
                    skipped += 1;
                    results.push(ReconstructedSession {
                        conversation_id,
                        external_id: conversation.external_id.clone(),
                        relative_or_source_path: source_path_display,
                        written_path: None,
                        line_count: 0,
                        skipped_reason: Some(format!("reconstruct failed: {e:#}")),
                    });
                    continue;
                }
            };

            if lines.is_empty() {
                skipped += 1;
                results.push(ReconstructedSession {
                    conversation_id,
                    external_id: conversation.external_id.clone(),
                    relative_or_source_path: source_path_display,
                    written_path: None,
                    line_count: 0,
                    skipped_reason: Some(
                        "no preserved source events (extra_json/extra_bin) to reconstruct"
                            .to_string(),
                    ),
                });
                continue;
            }

            let out_path = reconstruction_output_path(
                &target_dir,
                conversation_id,
                conversation.external_id.as_deref(),
                &conversation.source_path,
            );

            let mut body = lines.join("\n");
            body.push('\n');
            std::fs::write(&out_path, body.as_bytes()).map_err(|e| {
                io_error(
                    format!(
                        "writing reconstructed session to {}: {e}",
                        out_path.display()
                    ),
                    None,
                )
            })?;

            written += 1;
            total_lines += lines.len();
            results.push(ReconstructedSession {
                conversation_id,
                external_id: conversation.external_id.clone(),
                relative_or_source_path: source_path_display,
                written_path: Some(out_path),
                line_count: lines.len(),
                skipped_reason: None,
            });
        }

        offset += page_len;
        if page_len < RECOVER_CONVERSATION_PAGE {
            break;
        }
    }

    let envelope = serde_json::json!({
        "schema_version": 1,
        "doctor_contract_version": 1,
        "kind": "recover_from_archive",
        "db_path": db_path.display().to_string(),
        "target_dir": target_dir.display().to_string(),
        "conversations_total": total,
        "sessions_written": written,
        "sessions_skipped": skipped,
        "lines_written": total_lines,
        "sessions": results
            .iter()
            .map(|r| serde_json::json!({
                "conversation_id": r.conversation_id,
                "external_id": r.external_id,
                "source_path": r.relative_or_source_path,
                "written_path": r.written_path.as_ref().map(|p| p.display().to_string()),
                "line_count": r.line_count,
                "skipped_reason": r.skipped_reason,
            }))
            .collect::<Vec<_>>(),
        "next_action": format!(
            "Re-ingest the recovered tree with: cass index --full --data-dir <fresh-data-dir> (point the source scan at {})",
            target_dir.display()
        ),
        "note": "Reconstructed verbatim from the canonical archive's preserved extra_json/extra_bin envelopes. The corrupt archive was opened read-only and never mutated; no stock-sqlite .recover was required.",
    });

    if structured_format.is_some() {
        print_json(&envelope)?;
    } else {
        println!(
            "Recovered {written} session(s) ({total_lines} lines) into {}",
            target_dir.display()
        );
        if skipped > 0 {
            println!("  {skipped} conversation(s) had no preserved events and were skipped.");
        }
        println!(
            "Next: re-ingest with 'cass index --full' over {} into a fresh data dir.",
            target_dir.display()
        );
    }
    Ok(())
}

fn fts_parity_json(parity: &FtsShadowParity) -> serde_json::Value {
    serde_json::json!({
        "status": parity.status.as_str(),
        "canonical_messages": parity.canonical_messages,
        "indexable_messages": parity.indexable_messages,
        "indexed_messages": parity.indexed_messages,
        "detail": parity.detail,
    })
}

fn planned_fts_repair(parity: &FtsShadowParity) -> &'static str {
    match parity.status {
        FtsShadowParityStatus::Absent => "failure_atomic_recreate",
        FtsShadowParityStatus::Healthy => "verify_and_record_generation",
        FtsShadowParityStatus::Partial => "resumable_incremental_catch_up",
        FtsShadowParityStatus::Excess | FtsShadowParityStatus::Divergent => {
            "refuse_unsafe_destructive_rebuild"
        }
        FtsShadowParityStatus::Unqueryable => "refuse_unqueryable_preserve_bundle",
    }
}

fn fts_repair_is_applicable(parity: &FtsShadowParity) -> bool {
    match parity.status {
        FtsShadowParityStatus::Absent
        | FtsShadowParityStatus::Healthy
        | FtsShadowParityStatus::Partial => true,
        FtsShadowParityStatus::Excess
        | FtsShadowParityStatus::Divergent
        | FtsShadowParityStatus::Unqueryable => false,
    }
}

fn fts_rebuild_dry_run_envelope(db_path: &Path, parity: &FtsShadowParity) -> serde_json::Value {
    let applicable = fts_repair_is_applicable(parity);
    serde_json::json!({
        "schema_version": 1,
        "doctor_contract_version": 1,
        "kind": "rebuild_canonical_fts_dry_run",
        "dry_run": true,
        "db_path": db_path.display().to_string(),
        "parity": fts_parity_json(parity),
        "planned_action": planned_fts_repair(parity),
        "would_mutate": applicable,
        "canonical_rows_modified": false,
        "apply_command": applicable.then_some("cass doctor --rebuild-canonical-fts --yes --json"),
        "note": "Read-only inspection only; --yes never overrides --dry-run.",
    })
}

/// Verify and safely repair the canonical FTS5 shadow tables in place.
///
/// Queryable partial shadows are retained and caught up in bounded, resumable
/// batches. An absent shadow is created in a transaction so interruption
/// cannot publish a partial table. Unqueryable or divergent artifacts are
/// preserved for bundle-level recovery rather than destroyed in place. Exact
/// canonical/indexable/FTS parity is required before success.
pub fn run_doctor_rebuild_canonical_fts(
    data_dir_override: Option<PathBuf>,
    db_override: Option<PathBuf>,
    dry_run: bool,
    yes: bool,
    structured_format: Option<RobotFormat>,
) -> CliResult<()> {
    let data_dir = data_dir_override.unwrap_or_else(default_data_dir);
    let db_path = resolve_db_path(&data_dir, db_override.as_deref());

    if !dry_run && !yes {
        return Err(CliError {
            code: 4,
            kind: "refused-unsafe",
            message: "`cass doctor --rebuild-canonical-fts` mutates the canonical archive's derived FTS5 shadow and requires `--yes`".to_string(),
            hint: Some(
                "Inspect first with `--rebuild-canonical-fts --dry-run --json`, then re-run with `--rebuild-canonical-fts --yes` only when the plan is applicable. Queryable partial shadows are caught up in place and absent shadows are created failure-atomically; unqueryable/divergent artifacts are preserved for bundle-level recovery. Canonical rows are never modified.".to_string(),
            ),
            retryable: false,
        });
    }

    if !db_path.exists() {
        return Err(storage_error(
            format!("canonical archive {} does not exist", db_path.display()),
            Some("Recover the source tree with 'cass doctor --recover-from-archive <DIR>' first."),
        ));
    }

    let storage_open = if dry_run {
        FrankenStorage::open_readonly(&db_path)
    } else {
        FrankenStorage::open_existing_schema_only_for_fts_repair(&db_path)
    };
    let storage = match storage_open {
        Ok(storage) => storage,
        // #368 defect 3: the FTS5 shadow structure is corrupt enough that the
        // archive cannot be opened normally (the schema reload decodes the
        // corrupt %_data). Open with FTS5 hydration DEFERRED and rebuild the
        // shadow by dropping + recreating it from canonical rows — the shadow is
        // fully derived and canonical rows are never touched.
        Err(open_err) if is_fts5_shadow_open_corruption_error(&open_err) => {
            if dry_run {
                // A dry-run must stay read-only and non-locking: report the
                // planned repair straight from the open error, WITHOUT opening
                // the archive writable or taking the doctor mutation lock that
                // `open_deferred_fts5_for_repair` acquires.
                let envelope = serde_json::json!({
                    "surface": "doctor_rebuild_canonical_fts_dry_run",
                    "status": "shadow_structure_corrupt",
                    "planned_action": "drop_recreate_rebuild_from_canonical",
                    "detail": format!("{open_err:#}"),
                });
                if structured_format.is_some() {
                    print_json(&envelope)?;
                } else {
                    println!(
                        "Canonical FTS5 dry-run: status=shadow_structure_corrupt, planned_action=drop_recreate_rebuild_from_canonical; re-run with --yes to drop, recreate, and rebuild the corrupt shadow from canonical"
                    );
                }
                return Ok(());
            }
            let deferred = FrankenStorage::open_deferred_fts5_for_repair(&db_path).map_err(|e| {
                storage_error(
                    format!(
                        "opening canonical archive {} with deferred FTS5 validation for corrupt-shadow repair: {e:#}",
                        db_path.display()
                    ),
                    Some("Preserve the archive bundle and run 'cass doctor check --json'."),
                )
            })?;
            let inserted = deferred
                .rebuild_fts_shadow_via_drop_recreate()
                .map_err(|e| {
                    storage_error(
                        format!("rebuilding corrupt canonical FTS5 shadow via drop+recreate: {e:#}"),
                        Some("Preserve the archive bundle and run 'cass doctor check --json'."),
                    )
                })?;
            let envelope = serde_json::json!({
                "surface": "doctor_rebuild_canonical_fts",
                "status": "rebuilt_from_corrupt_shadow",
                "method": "drop_recreate_rebuild_from_canonical",
                "inserted_messages": inserted,
            });
            if structured_format.is_some() {
                print_json(&envelope)?;
            } else {
                println!(
                    "Canonical FTS5 shadow was structurally corrupt; dropped, recreated, and rebuilt {inserted} message(s) from canonical rows."
                );
            }
            return Ok(());
        }
        Err(open_err) => {
            return Err(storage_error(
                format!(
                    "could not open canonical archive {} for FTS5 inspection: {open_err:#}",
                    db_path.display()
                ),
                Some(
                    "If the archive cannot be opened at all, the canonical rows are unreadable — use \
                     'cass doctor --recover-from-archive <DIR>' to rebuild the source tree instead.",
                ),
            ));
        }
    };
    let before = storage.inspect_search_fallback_fts_parity().map_err(|e| {
        storage_error(
            format!("inspecting canonical/FTS5 row parity: {e:#}"),
            Some(
                "Preserve the canonical archive bundle and run 'cass doctor check --json' before retrying.",
            ),
        )
    })?;

    if dry_run {
        let envelope = fts_rebuild_dry_run_envelope(&db_path, &before);
        if structured_format.is_some() {
            print_json(&envelope)?;
        } else {
            println!(
                "Canonical FTS5 dry-run: status={}, planned_action={}, canonical={}, indexable={}, indexed={:?}",
                before.status.as_str(),
                planned_fts_repair(&before),
                before.canonical_messages,
                before.indexable_messages,
                before.indexed_messages
            );
        }
        return Ok(());
    }

    let repair = storage
        .ensure_search_fallback_fts_consistency()
        .map_err(|e| {
            if is_fts5_oversized_leaf_error(&e) {
                // GH #369: a known, content-dependent engine limitation — not
                // archive corruption. Surface a distinct, reassuring diagnostic
                // instead of the generic storage wall so operators do not treat
                // a working (Tantivy-served) search as broken.
                fts5_oversized_leaf_shadow_error(&db_path)
            } else {
                storage_error(
                    format!("safely repairing canonical FTS5 shadow tables: {e:#}"),
                    Some(
                        "Preserve the complete database bundle. Re-run the dry-run to inspect exact current parity before any retry.",
                    ),
                )
            }
        })?;
    let after = storage.inspect_search_fallback_fts_parity().map_err(|e| {
        storage_error(
            format!("validating canonical/FTS5 parity after repair: {e:#}"),
            Some("Repair is not complete until exact parity validation succeeds."),
        )
    })?;
    if after.status != FtsShadowParityStatus::Healthy {
        return Err(storage_error(
            format!(
                "canonical FTS5 repair did not reach exact parity: status={}, indexable={}, indexed={:?}",
                after.status.as_str(),
                after.indexable_messages,
                after.indexed_messages
            ),
            Some("Re-run the dry-run; do not treat this repair as complete."),
        ));
    }
    let (repair_kind, inserted_rows) = match repair {
        FtsConsistencyRepair::AlreadyHealthy { .. } => ("already_healthy", 0),
        FtsConsistencyRepair::IncrementalCatchUp { inserted_rows, .. } => {
            ("resumable_incremental_catch_up", inserted_rows)
        }
        FtsConsistencyRepair::Rebuilt { inserted_rows } => {
            ("failure_atomic_recreate", inserted_rows)
        }
    };

    let envelope = serde_json::json!({
        "schema_version": 1,
        "doctor_contract_version": 1,
        "kind": "rebuild_canonical_fts",
        "db_path": db_path.display().to_string(),
        "repair_kind": repair_kind,
        "inserted_rows": inserted_rows,
        "parity_before": fts_parity_json(&before),
        "parity_after": fts_parity_json(&after),
        "mutated_asset_class": "canonical_fts5_shadow",
        "canonical_rows_modified": false,
        "note": "Queryable shadows are preserved and caught up resumably; recreation is transactionally published only after exact parity validation. Canonical rows are never modified.",
    });

    if structured_format.is_some() {
        print_json(&envelope)?;
    } else {
        println!(
            "Canonical FTS5 repair complete ({repair_kind}, {inserted_rows} rows inserted, {} rows indexed) in {}",
            after.indexable_messages,
            db_path.display()
        );
    }
    Ok(())
}

/// Quarantine interrupted `raw_mirror_capture` staging artifacts.
///
/// Empty/partial `raw-mirror/v1/tmp/capture.*` staging dirs from killed index
/// runs otherwise block doctor mutation behind "interrupted doctor artifact(s)
/// require inspection", forcing a manual `rm` inside cass's own data dir. This
/// moves them into `<data_dir>/doctor/quarantine/interrupted-artifacts/`
/// (renamed, never deleted — cass never deletes; the operator owns final
/// reclamation), clearing the gate.
pub fn run_doctor_cleanup_interrupted_artifacts(
    data_dir_override: Option<PathBuf>,
    yes: bool,
    structured_format: Option<RobotFormat>,
) -> CliResult<()> {
    let data_dir = data_dir_override.unwrap_or_else(default_data_dir);
    let tmp_root = data_dir.join("raw-mirror").join("v1").join("tmp");

    let quarantine_root = data_dir
        .join("doctor")
        .join("quarantine")
        .join("interrupted-artifacts");

    // Enumerate the interrupted capture staging entries (top-level children of
    // the raw-mirror tmp dir). These are the `capture.*` dirs the doctor gate
    // flags as needs-inspection.
    let mut candidates: Vec<PathBuf> = Vec::new();
    if tmp_root.exists() {
        let entries = std::fs::read_dir(&tmp_root).map_err(|e| {
            io_error(
                format!(
                    "reading interrupted-capture staging dir {}: {e}",
                    tmp_root.display()
                ),
                None,
            )
        })?;
        for entry in entries {
            let entry = entry.map_err(|e| {
                io_error(format!("enumerating interrupted-capture entry: {e}"), None)
            })?;
            candidates.push(entry.path());
        }
    }
    candidates.sort();

    if candidates.is_empty() {
        let envelope = serde_json::json!({
            "schema_version": 1,
            "doctor_contract_version": 1,
            "kind": "cleanup_interrupted_artifacts",
            "data_dir": data_dir.display().to_string(),
            "tmp_root": tmp_root.display().to_string(),
            "quarantined_count": 0,
            "quarantined": [],
            "note": "No interrupted raw_mirror_capture artifacts found; doctor mutation is not blocked by this class.",
        });
        if structured_format.is_some() {
            print_json(&envelope)?;
        } else {
            println!("No interrupted raw_mirror_capture artifacts found.");
        }
        return Ok(());
    }

    if !yes {
        return Err(CliError {
            code: 4,
            kind: "refused-unsafe",
            message: format!(
                "found {} interrupted raw_mirror_capture artifact(s); `--cleanup-interrupted-artifacts` requires `--yes` to quarantine them",
                candidates.len()
            ),
            hint: Some(format!(
                "Inspect them under {} first, then re-run with `--cleanup-interrupted-artifacts --yes`. They are renamed into a quarantine dir, never deleted.",
                tmp_root.display()
            )),
            retryable: false,
        });
    }

    std::fs::create_dir_all(&quarantine_root).map_err(|e| {
        io_error(
            format!("creating quarantine dir {}: {e}", quarantine_root.display()),
            None,
        )
    })?;

    let mut quarantined: Vec<String> = Vec::new();
    for src in &candidates {
        let name = src
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| format!("artifact-{}", now_unix_ms()));
        let dst = quarantine_root.join(&name);
        let final_dst = if dst.exists() {
            quarantine_root.join(format!("{name}.{}", now_unix_ms()))
        } else {
            dst
        };
        std::fs::rename(src, &final_dst).map_err(|e| {
            io_error(
                format!(
                    "quarantining interrupted artifact {} → {}: {e}",
                    src.display(),
                    final_dst.display()
                ),
                Some("The cleanup halted at this artifact; inspect it manually."),
            )
        })?;
        quarantined.push(final_dst.display().to_string());
    }

    let envelope = serde_json::json!({
        "schema_version": 1,
        "doctor_contract_version": 1,
        "kind": "cleanup_interrupted_artifacts",
        "data_dir": data_dir.display().to_string(),
        "tmp_root": tmp_root.display().to_string(),
        "quarantine_root": quarantine_root.display().to_string(),
        "quarantined_count": quarantined.len(),
        "quarantined": quarantined,
        "note": "Interrupted raw_mirror_capture artifacts were renamed into quarantine; cass never deletes. This clears the 'interrupted doctor artifact(s) require inspection' mutation gate.",
    });

    if structured_format.is_some() {
        print_json(&envelope)?;
    } else {
        println!(
            "Quarantined {} interrupted raw_mirror_capture artifact(s) into {}",
            quarantined.len(),
            quarantine_root.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use frankensqlite::compat::{ConnectionExt as _, ParamValue, RowExt as _};

    fn write_message(storage: &FrankenStorage, conversation_id: i64, idx: i64, raw_line: &str) {
        // Store the verbatim line via the historical-raw-json sentinel wrapper
        // (the exact shape franken_message_insert_payload writes for raw lines).
        let wrapper = serde_json::json!({ "__cass_historical_raw_json__": raw_line });
        let extra = serde_json::to_string(&wrapper).unwrap();
        storage
            .raw()
            .execute_compat(
                "INSERT INTO messages(conversation_id, idx, role, author, created_at, content, extra_json, extra_bin) \
                 VALUES(?1, ?2, 'user', NULL, ?3, ?4, ?5, NULL)",
                &[
                    ParamValue::from(conversation_id),
                    ParamValue::from(idx),
                    ParamValue::from(1000_i64 + idx),
                    ParamValue::from(format!("content {idx}")),
                    ParamValue::from(extra),
                ] as &[ParamValue],
            )
            .expect("insert message");
    }

    fn seed_agent(storage: &FrankenStorage) -> i64 {
        // conversations.agent_id is NOT NULL REFERENCES agents(id) after
        // migrations, so a conversation row needs a real agent first.
        storage
            .raw()
            .execute_compat(
                "INSERT INTO agents(slug, name, version, kind, created_at, updated_at) \
                 VALUES('claude', 'Claude Code', NULL, 'cli', 1000, 1000)",
                &[] as &[ParamValue],
            )
            .expect("insert agent");
        storage
            .raw()
            .query_row_map("SELECT last_insert_rowid()", &[] as &[ParamValue], |row| {
                row.get_typed::<i64>(0)
            })
            .expect("agent rowid")
    }

    fn seed_conversation(
        storage: &FrankenStorage,
        agent_id: i64,
        external_id: &str,
        source_path: &str,
    ) -> i64 {
        storage
            .raw()
            .execute_compat(
                "INSERT INTO conversations(agent_id, external_id, title, source_path, started_at) \
                 VALUES(?1, ?2, ?3, ?4, 1000)",
                &[
                    ParamValue::from(agent_id),
                    ParamValue::from(external_id),
                    ParamValue::from(format!("title {external_id}")),
                    ParamValue::from(source_path),
                ] as &[ParamValue],
            )
            .expect("insert conversation");
        storage
            .raw()
            .query_row_map("SELECT last_insert_rowid()", &[] as &[ParamValue], |row| {
                row.get_typed::<i64>(0)
            })
            .expect("rowid")
    }

    #[test]
    fn sanitize_path_component_strips_separators_and_traversal() {
        // Path separators collapse to '_', so the result is always a single
        // flat filename component (interior dots are harmless once no '/'
        // remains).
        assert_eq!(sanitize_path_component("a/b/../c"), "a_b_.._c");
        assert!(!sanitize_path_component("a/b/../c").contains('/'));
        assert_eq!(sanitize_path_component("normal-id_1.2"), "normal-id_1.2");
        assert_eq!(sanitize_path_component(""), "");
        // Leading/trailing dots are trimmed so we never emit "." or "..".
        assert_eq!(sanitize_path_component(".."), "");
        assert_eq!(sanitize_path_component("."), "");
    }

    #[test]
    fn reconstruction_output_path_stays_inside_target_dir() {
        let target = Path::new("/tmp/recover");
        let out = reconstruction_output_path(
            target,
            7,
            Some("sess-abc"),
            Path::new("/home/u/.claude/projects/foo/bar.jsonl"),
        );
        assert!(out.starts_with(target));
        let name = out.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("sess-abc"));
        assert!(name.ends_with(".jsonl"));
        // A malicious external_id can never escape the recovery dir.
        let evil =
            reconstruction_output_path(target, 7, Some("../../etc/passwd"), Path::new("x.jsonl"));
        assert!(evil.starts_with(target));
        assert_eq!(evil.parent().unwrap(), target);
    }

    #[test]
    fn recover_from_archive_reconstructs_verbatim_lines() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("agent_search.db");
        let target = tmp.path().join("recovered");
        {
            let storage = FrankenStorage::open(&db_path).expect("open db");
            let agent_id = seed_agent(&storage);
            let cid = seed_conversation(&storage, agent_id, "sess-1", "/orig/a.jsonl");
            write_message(
                &storage,
                cid,
                0,
                r#"{"type":"user","uuid":"u1","text":"hi"}"#,
            );
            write_message(
                &storage,
                cid,
                1,
                r#"{"type":"assistant","uuid":"a1","text":"yo"}"#,
            );
        }

        run_doctor_recover_from_archive(
            Some(tmp.path().to_path_buf()),
            Some(db_path.clone()),
            target.clone(),
            Some(RobotFormat::Json),
        )
        .expect("recover");

        // One .jsonl file with the two verbatim lines, in order.
        let out_file = std::fs::read_dir(&target)
            .expect("read recovered dir")
            .filter_map(Result::ok)
            .map(|e| e.path())
            .find(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
            .expect("a reconstructed jsonl file");
        let body = std::fs::read_to_string(&out_file).expect("read reconstructed file");
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], r#"{"type":"user","uuid":"u1","text":"hi"}"#);
        assert_eq!(lines[1], r#"{"type":"assistant","uuid":"a1","text":"yo"}"#);
    }

    #[test]
    fn cleanup_interrupted_artifacts_quarantines_without_delete() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let data_dir = tmp.path().to_path_buf();
        let tmp_root = data_dir.join("raw-mirror").join("v1").join("tmp");
        std::fs::create_dir_all(tmp_root.join("capture.dead1")).expect("mk capture dir");
        std::fs::create_dir_all(tmp_root.join("capture.dead2")).expect("mk capture dir");

        // Without --yes the command refuses (and does not move anything).
        let refused = run_doctor_cleanup_interrupted_artifacts(
            Some(data_dir.clone()),
            false,
            Some(RobotFormat::Json),
        );
        assert!(refused.is_err());
        assert!(tmp_root.join("capture.dead1").exists());

        // With --yes the artifacts are quarantined (moved, not deleted).
        run_doctor_cleanup_interrupted_artifacts(
            Some(data_dir.clone()),
            true,
            Some(RobotFormat::Json),
        )
        .expect("cleanup");
        assert!(!tmp_root.join("capture.dead1").exists());
        assert!(!tmp_root.join("capture.dead2").exists());
        let quarantine = data_dir
            .join("doctor")
            .join("quarantine")
            .join("interrupted-artifacts");
        assert!(quarantine.join("capture.dead1").exists());
        assert!(quarantine.join("capture.dead2").exists());
    }

    #[test]
    fn rebuild_canonical_fts_refuses_without_yes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("agent_search.db");
        {
            let _storage = FrankenStorage::open(&db_path).expect("open db");
        }
        let refused = run_doctor_rebuild_canonical_fts(
            Some(tmp.path().to_path_buf()),
            Some(db_path),
            false,
            false,
            Some(RobotFormat::Json),
        );
        assert!(refused.is_err());
    }

    #[test]
    fn rebuild_canonical_fts_dry_run_with_yes_is_read_only() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("agent_search.db");
        let storage = FrankenStorage::open(&db_path).expect("open db");
        let schema_rows_before: i64 = storage
            .raw()
            .query_row_map(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'fts_messages'",
                &[] as &[ParamValue],
                |row| row.get_typed(0),
            )
            .expect("count FTS schema before dry-run");
        let marker_rows_before: i64 = storage
            .raw()
            .query_row_map(
                "SELECT COUNT(*) FROM meta WHERE key = 'fts_frankensqlite_rebuild_generation'",
                &[] as &[ParamValue],
                |row| row.get_typed(0),
            )
            .expect("count FTS generation markers before dry-run");
        drop(storage);
        let db_bytes_before = std::fs::read(&db_path).expect("snapshot database before dry-run");

        run_doctor_rebuild_canonical_fts(
            Some(tmp.path().to_path_buf()),
            Some(db_path.clone()),
            true,
            true,
            Some(RobotFormat::Json),
        )
        .expect("dry-run with --yes must remain read-only");
        let db_bytes_after = std::fs::read(&db_path).expect("snapshot database after dry-run");
        assert_eq!(
            db_bytes_after, db_bytes_before,
            "dry-run with --yes must not alter any canonical database bytes"
        );

        let storage = FrankenStorage::open_readonly(&db_path).expect("reopen read-only");
        let schema_rows_after: i64 = storage
            .raw()
            .query_row_map(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'fts_messages'",
                &[] as &[ParamValue],
                |row| row.get_typed(0),
            )
            .expect("count FTS schema after dry-run");
        let marker_rows_after: i64 = storage
            .raw()
            .query_row_map(
                "SELECT COUNT(*) FROM meta WHERE key = 'fts_frankensqlite_rebuild_generation'",
                &[] as &[ParamValue],
                |row| row.get_typed(0),
            )
            .expect("count FTS generation markers after dry-run");
        assert_eq!(schema_rows_after, schema_rows_before);
        assert_eq!(marker_rows_after, marker_rows_before);
    }

    #[test]
    fn rebuild_canonical_fts_repairs_absent_shadow_without_canonical_row_changes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("agent_search.db");
        let conversation_id = {
            let storage = FrankenStorage::open(&db_path).expect("open db");
            let agent_id = seed_agent(&storage);
            let conversation_id =
                seed_conversation(&storage, agent_id, "fts-repair", "/orig/fts.jsonl");
            write_message(
                &storage,
                conversation_id,
                0,
                r#"{"type":"user","uuid":"fts-1","text":"canonical sentinel"}"#,
            );
            assert_eq!(
                storage
                    .inspect_search_fallback_fts_parity()
                    .expect("inspect absent FTS")
                    .status,
                FtsShadowParityStatus::Absent
            );
            conversation_id
        };

        run_doctor_rebuild_canonical_fts(
            Some(tmp.path().to_path_buf()),
            Some(db_path.clone()),
            false,
            true,
            Some(RobotFormat::Json),
        )
        .expect("repair absent FTS through schema-only writer");

        let readonly = FrankenStorage::open_readonly(&db_path).expect("reopen read-only");
        let parity = readonly
            .inspect_search_fallback_fts_parity()
            .expect("inspect repaired FTS");
        assert_eq!(parity.status, FtsShadowParityStatus::Healthy);
        assert_eq!(parity.canonical_messages, 1);
        assert_eq!(parity.indexable_messages, 1);
        assert_eq!(parity.indexed_messages, Some(1));
        let canonical: (i64, i64, String, String) = readonly
            .raw()
            .query_row_map(
                "SELECT id, conversation_id, content, extra_json FROM messages",
                &[] as &[ParamValue],
                |row| {
                    Ok((
                        row.get_typed(0)?,
                        row.get_typed(1)?,
                        row.get_typed(2)?,
                        row.get_typed(3)?,
                    ))
                },
            )
            .expect("read canonical sentinel after repair");
        assert_eq!(canonical.0, 1);
        assert_eq!(canonical.1, conversation_id);
        assert_eq!(canonical.2, "content 0");
        assert!(canonical.3.contains("canonical sentinel"));
    }

    #[test]
    fn divergent_fts_dry_run_contract_refuses_mutation() {
        let parity = FtsShadowParity {
            status: FtsShadowParityStatus::Divergent,
            canonical_messages: 2,
            indexable_messages: 2,
            indexed_messages: Some(2),
            detail: Some("equal counts conceal rowid divergence".to_string()),
        };
        let envelope = fts_rebuild_dry_run_envelope(Path::new("/tmp/divergent.db"), &parity);
        assert_eq!(
            envelope["planned_action"],
            "refuse_unsafe_destructive_rebuild"
        );
        assert_eq!(envelope["would_mutate"], false);
        assert_eq!(envelope["apply_command"], serde_json::Value::Null);
        assert_eq!(envelope["parity"]["status"], "divergent");
    }

    #[test]
    fn unqueryable_fts_dry_run_preserves_bundle_instead_of_advertising_apply() {
        let parity = FtsShadowParity {
            status: FtsShadowParityStatus::Unqueryable,
            canonical_messages: 2,
            indexable_messages: 2,
            indexed_messages: None,
            detail: Some("counting fts_messages_docsize failed".to_string()),
        };
        let envelope = fts_rebuild_dry_run_envelope(Path::new("/tmp/unqueryable.db"), &parity);
        assert_eq!(
            envelope["planned_action"],
            "refuse_unqueryable_preserve_bundle"
        );
        assert_eq!(envelope["would_mutate"], false);
        assert_eq!(envelope["apply_command"], serde_json::Value::Null);
    }

    /// GH #362: a single whitespace-delimited token beyond the FTS5 u16
    /// leaf-offset space (91,548 bytes observed in a real Codex rollout) used
    /// to fail every canonical FTS repair path with "fts5: corrupt %_data
    /// record: segment leaf term offset exceeds u16" — including this exact
    /// `--rebuild-canonical-fts` recovery. With the pinned frankensqlite
    /// hotfix family (branch `fts5-overlong-hotfix-cass362`, which carries
    /// the overlong-term skip cap) the overlong term is skipped at the
    /// tokenizer, the rebuild completes, and neighboring terms in the same
    /// message stay indexed.
    #[test]
    fn rebuild_canonical_fts_survives_overlong_term_in_corpus() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("agent_search.db");
        let giant = "a".repeat(91_548);
        {
            let storage = FrankenStorage::open(&db_path).expect("open db");
            let agent_id = seed_agent(&storage);
            let conversation_id =
                seed_conversation(&storage, agent_id, "overlong", "/orig/overlong.jsonl");
            // The giant token must land in `messages.content` — that is the
            // column the FTS rebuild streams through the tokenizer. (The
            // `write_message` helper stores a placeholder content, which
            // would never exercise the overlong path.)
            for (idx, content) in [
                (0_i64, format!("before {giant} needle")),
                (1_i64, "ordinary reply".to_string()),
            ] {
                storage
                    .raw()
                    .execute_compat(
                        "INSERT INTO messages(conversation_id, idx, role, author, created_at, content, extra_json, extra_bin) \
                         VALUES(?1, ?2, 'user', NULL, ?3, ?4, NULL, NULL)",
                        &[
                            ParamValue::from(conversation_id),
                            ParamValue::from(idx),
                            ParamValue::from(1000_i64 + idx),
                            ParamValue::from(content),
                        ] as &[ParamValue],
                    )
                    .expect("insert message with overlong content");
            }
        }

        run_doctor_rebuild_canonical_fts(
            Some(tmp.path().to_path_buf()),
            Some(db_path.clone()),
            false,
            true,
            Some(RobotFormat::Json),
        )
        .expect("rebuild must survive an overlong term in the corpus (GH #362)");

        let readonly = FrankenStorage::open_readonly(&db_path).expect("reopen read-only");
        let parity = readonly
            .inspect_search_fallback_fts_parity()
            .expect("inspect rebuilt FTS");
        assert_eq!(parity.status, FtsShadowParityStatus::Healthy);
        assert_eq!(parity.canonical_messages, 2);
        assert_eq!(parity.indexed_messages, Some(2));
    }

    /// GH #369: the cumulative oversized-leaf failure (many in-cap terms in one
    /// batch, not a single overlong token) must be recognized so the operator
    /// gets a reassuring "search still works via Tantivy" diagnostic rather than
    /// the generic storage wall. This mirrors the exact wrapped chain the
    /// failure-atomic rebuild produces (`sqlite.rs` `.context(...)`), with the
    /// fsqlite root string preserved.
    #[test]
    fn oversized_leaf_error_is_classified_and_gets_reassuring_diagnostic() {
        let wrapped = anyhow::anyhow!(
            "inserting 4000 rows into fts_messages during streaming FTS maintenance: \
             fts5: corrupt %_data record: segment leaf term offset exceeds u16"
        )
        .context("failure-atomic FTS rebuild rolled back without publishing a partial shadow");
        assert!(
            is_fts5_oversized_leaf_error(&wrapped),
            "the real wrapped chain must be recognized as the GH #369 oversized-leaf case"
        );

        // Each sibling leaf/footer overflow signature is also covered.
        for signature in [
            "segment leaf rowid offset exceeds u16",
            "segment leaf footer offset exceeds u16",
            "segment footer offset exceeds u16",
        ] {
            assert!(
                is_fts5_oversized_leaf_error(&anyhow::anyhow!(signature.to_string())),
                "signature must be recognized: {signature}"
            );
        }

        // Unrelated storage failures must NOT be misclassified — they still get
        // the generic storage wall + bundle-preservation hint.
        for unrelated in [
            "database is locked",
            "no such table: fts_messages",
            "disk I/O error while reading page 42",
            "segment terms must be strictly increasing",
        ] {
            assert!(
                !is_fts5_oversized_leaf_error(&anyhow::anyhow!(unrelated.to_string())),
                "unrelated error must not be misclassified: {unrelated}"
            );
        }

        let diagnostic = fts5_oversized_leaf_shadow_error(Path::new("/tmp/agent_search.db"));
        assert_eq!(diagnostic.kind, "fts5-oversized-leaf-shadow-unbuildable");
        assert!(!diagnostic.retryable);
        assert!(
            diagnostic
                .message
                .contains("not corruption of your archive"),
            "message must reassure the operator their data is intact"
        );
        let hint = diagnostic
            .hint
            .expect("oversized-leaf diagnostic carries a hint");
        assert!(
            hint.contains("Tantivy") && hint.contains("No action is needed"),
            "hint must state search still works and no action is needed"
        );
    }
}
