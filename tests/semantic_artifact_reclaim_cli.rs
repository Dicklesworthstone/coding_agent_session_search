//! Exercise the operator command boundary in distinct processes, including a
//! real live FSVI. A deliberately invalid canonical DB must not block GC.
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::Result;
use coding_agent_search::indexer::semantic::{
    EmbeddingInput, SemanticBackfillBatchPlan, SemanticIndexer,
};
use coding_agent_search::search::semantic_manifest::{SemanticManifest, TierKind};
use frankensearch::index::{VectorIndex, wal_path_for};
use serde_json::Value;

fn fixture(data: &Path) -> Result<(PathBuf, PathBuf, PathBuf)> {
    let indexer = SemanticIndexer::new("hash", None)?;
    let mut manifest = SemanticManifest::default();
    let live = indexer
        .run_backfill_batch(
            &[EmbeddingInput::new(1, "live compiler checkpoint")],
            data,
            &mut manifest,
            SemanticBackfillBatchPlan {
                tier: TierKind::Quality,
                db_fingerprint: "content-v1:cli-live".into(),
                model_revision: "hash".into(),
                total_conversations: 1,
                conversations_in_batch: 1,
                last_offset: 1,
                cursor_exhausted: true,
            },
        )?
        .index_path;
    fs::write(
        data.join("agent_search.db"),
        b"not a database; recovery must not open it",
    )?;
    let staging = data.join("vector_index/.staging-quality-minilm-384-deadbeef.fsvi");
    let reuse = data.join("vector_index/.backfill-reuse-Killed123");
    fs::write(&staging, b"old")?;
    fs::write(wal_path_for(&staging), b"WAL")?;
    fs::create_dir(&reuse)?;
    fs::write(reuse.join("candidate.fsvi"), b"copy")?;
    Ok((live, staging, reuse))
}

fn command(data: &Path, extra: &[&str]) -> Result<Output> {
    Ok(Command::new(env!("CARGO_BIN_EXE_cass-semantic-reclaim"))
        .arg("--data-dir")
        .arg(data)
        .arg("--json")
        .args(extra)
        .output()?)
}

fn preview(data: &Path) -> Result<Value> {
    let output = command(data, &[])?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    Ok(serde_json::from_slice(&output.stdout)?)
}

#[test]
fn cli_preview_apply_preserves_live_index_and_never_opens_database() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path().join("archive with spaces");
    let (live, staging, reuse) = fixture(&data)?;
    let live_before = fs::read(&live)?;
    let manifest_before = fs::read(SemanticManifest::path(&data))?;
    let db_before = fs::read(data.join("agent_search.db"))?;
    let plan = preview(&data)?;
    assert_eq!(plan["operation"], "preview");
    assert_eq!(plan["status"], "ready");
    assert_eq!(plan["plan"]["reclaimable_bytes"], 10);
    assert!(staging.is_file() && reuse.is_dir());
    let fingerprint = plan["plan"]["plan_fingerprint"].as_str().unwrap();
    let explicit = command(&data, &["--dry-run"])?;
    assert!(explicit.status.success());
    assert_eq!(plan, serde_json::from_slice::<Value>(&explicit.stdout)?);
    let applied = command(&data, &["--apply", "--plan-fingerprint", fingerprint])?;
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    let report: Value = serde_json::from_slice(&applied.stdout)?;
    assert_eq!(report["operation"], "apply");
    assert_eq!(report["status"], "complete");
    assert_eq!(report["report"]["reclaimed_bytes"], 10);
    assert!(!staging.exists() && !wal_path_for(&staging).exists() && !reuse.exists());
    assert_eq!(live_before, fs::read(&live)?);
    assert_eq!(manifest_before, fs::read(SemanticManifest::path(&data))?);
    assert_eq!(db_before, fs::read(data.join("agent_search.db"))?);
    assert_eq!(VectorIndex::open_read_only(&live)?.record_count(), 1);
    Ok(())
}

#[test]
fn stale_approval_and_live_writer_fail_without_deleting_any_candidate() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let (live, staging, reuse) = fixture(data)?;
    let plan = preview(data)?;
    let fingerprint = plan["plan"]["plan_fingerprint"].as_str().unwrap();
    for name in ["index-run.lock", "semantic-backfill-artifacts.lock"] {
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(data.join(name))?;
        fs2::FileExt::try_lock_exclusive(&lock)?;
        let busy = command(data, &["--apply", "--plan-fingerprint", fingerprint])?;
        assert_eq!(busy.status.code(), Some(1));
        assert!(busy.stdout.is_empty());
        assert!(serde_json::from_slice::<Value>(&busy.stderr)?["error"].is_object());
        assert!(live.is_file() && staging.is_file() && reuse.is_dir());
        drop(lock);
    }
    fs::write(reuse.join("candidate.fsvi"), b"changed copy")?;
    let stale = command(data, &["--apply", "--plan-fingerprint", fingerprint])?;
    assert_eq!(stale.status.code(), Some(1));
    assert!(stale.stdout.is_empty());
    assert!(
        serde_json::from_slice::<Value>(&stale.stderr)?["error"]["message"]
            .as_str()
            .unwrap()
            .contains("plan changed")
    );
    assert!(live.is_file() && staging.is_file() && reuse.is_dir());
    Ok(())
}

#[test]
fn usage_errors_and_help_do_not_create_an_archive() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let absent = temp.path().join("absent");
    for args in [
        &["--apply"][..],
        &["--plan-fingerprint", "invalid"],
        &["--force"],
    ] {
        let output = command(&absent, args)?;
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stderr)?["error"]["kind"],
            "usage"
        );
        assert!(!absent.exists());
    }
    let help = command(&absent, &["--help"])?;
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("--plan-fingerprint"));
    assert!(!absent.exists());
    Ok(())
}

#[test]
fn corrupt_authority_is_reported_not_overwritten_or_ignored() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let (live, staging, reuse) = fixture(data)?;
    fs::write(SemanticManifest::path(data), b"{torn manifest")?;
    let output = command(data, &[])?;
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(
        serde_json::from_slice::<Value>(&output.stderr)?["error"]["message"]
            .as_str()
            .unwrap()
            .contains("invalid")
    );
    assert_eq!(fs::read(SemanticManifest::path(data))?, b"{torn manifest");
    assert!(live.is_file() && staging.is_file() && reuse.is_dir());
    Ok(())
}

#[test]
fn dangling_checkpoint_is_a_blocked_preview_not_a_successful_empty_plan() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let (live, staging, reuse) = fixture(data)?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let mut manifest = SemanticManifest::load(data)?.unwrap();
    let saved = indexer.run_backfill_batch(
        &[EmbeddingInput::new(2, "next resumable checkpoint")],
        data,
        &mut manifest,
        SemanticBackfillBatchPlan {
            tier: TierKind::Quality,
            db_fingerprint: "content-v1:cli-next".into(),
            model_revision: "hash".into(),
            total_conversations: 2,
            conversations_in_batch: 1,
            last_offset: 1,
            cursor_exhausted: false,
        },
    )?;
    // Keep the file as recovery evidence, but remove it from the recorded name.
    let fallback = data.join("vector_index/retained-checkpoint.fsvi");
    fs::rename(saved.index_path, &fallback)?;
    // Automatic startup recovery already retired the fixture's original scratch.
    fs::write(&staging, b"fallback staging")?;
    fs::create_dir(&reuse)?;
    fs::write(reuse.join("candidate.fsvi"), b"fallback reuse")?;
    let output = command(data, &[])?;
    assert_eq!(output.status.code(), Some(3));
    assert!(output.stderr.is_empty());
    let plan: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(plan["status"], "blocked");
    assert_eq!(plan["plan"]["checkpoint_missing"], true);
    let fingerprint = plan["plan"]["plan_fingerprint"].as_str().unwrap();
    let apply = command(data, &["--apply", "--plan-fingerprint", fingerprint])?;
    assert_eq!(apply.status.code(), Some(1));
    assert!(apply.stdout.is_empty());
    assert!(live.is_file() && fallback.is_file() && staging.is_file() && reuse.is_dir());
    Ok(())
}
