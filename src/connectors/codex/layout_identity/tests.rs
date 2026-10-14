use super::*;
use crate::connectors::{DiscoveredSourceRole, Origin, ScanRoot, SourceKind};
use serde_json::json;
use std::io::Write;

const ID: &str = "776931f0-b259-49f6-9808-771bbfb92bbe";
const BUDGET: u64 = 1024 * 1024;

fn record(text: &str) -> String {
    format!("{}\n", json!({"type":"response_item", "payload":{
        "type":"message", "role":"user", "content":[{"type":"input_text", "text":text}]
    }}))
}

fn body(id: &str) -> String {
    format!("{}\n{}", json!({"type":"session_meta", "payload":{
        "id":id, "timestamp":"2026-10-13T00:00:00Z", "cwd":"/tmp/codex-layout-repro"
    }}), record("hello from codex"))
}

fn fixture() -> Result<(tempfile::TempDir, LayoutPair, Receipt)> {
    let root = tempfile::tempdir()?;
    let sessions = root.path().join("source/sessions");
    fs::create_dir_all(sessions.join("2026/10/13"))?;
    let name = format!("rollout-2026-10-13T00-00-00-{ID}.jsonl");
    let flat = sessions.join(&name);
    let dated = sessions.join("2026/10/13").join(&name);
    fs::write(&flat, body(ID))?;
    fs::write(&dated, body(ID))?;
    let pair = LayoutPair::discover(&flat, &dated)?;
    let proof = read_snapshot(&flat, &[], BUDGET, None)?;
    let receipt = Receipt {
        version: 1,
        source_id: "full-history-plan".into(),
        agent: "codex".into(),
        canonical_conversation_id: 17,
        canonical_external_id: pair.bare_id.clone(),
        pair: pair.clone(),
        metadata_hash: proof.metadata_hash,
        baseline: proof.checkpoint.clone(),
        head: proof.checkpoint.clone(),
        flat_snapshot: proof.checkpoint.clone(),
        dated_snapshot: proof.checkpoint,
    };
    Ok((root, pair, receipt))
}

fn append(path: &Path, text: &str) -> Result<()> {
    OpenOptions::new().append(true).open(path)?.write_all(record(text).as_bytes())?;
    Ok(())
}

#[test]
fn gh524_both_layout_orders_preserve_the_selected_identity_and_accept_appends() -> Result<()> {
    for dated_first in [false, true] {
        let (_root, pair, mut receipt) = fixture()?;
        receipt.canonical_external_id = if dated_first { pair.dated_id.clone() } else { pair.bare_id.clone() };
        let original_id = receipt.canonical_external_id.clone();
        let (first, second) = if dated_first { (&pair.dated, &pair.flat) } else { (&pair.flat, &pair.dated) };
        receipt.admit_path(first, BUDGET, None)?;
        receipt.admit_path(second, BUDGET, None)?;
        append(second, "new searchable message")?;
        receipt.admit_path(second, BUDGET, None)?;
        receipt.admit_path(first, BUDGET, None)?;
        let stable = receipt.clone();
        for _ in 0..3 {
            receipt.admit_path(second, BUDGET, None)?;
            receipt.admit_path(first, BUDGET, None)?;
        }
        assert_eq!(receipt, stable);
        assert_eq!(receipt.canonical_conversation_id, 17);
        assert_eq!(receipt.canonical_external_id, original_id);
    }
    Ok(())
}

#[test]
fn gh524_conflicting_equal_length_and_appended_branches_are_refused() -> Result<()> {
    for appended in [false, true] {
        let (_root, pair, mut receipt) = fixture()?;
        if appended {
            append(&pair.flat, "branch A")?;
            receipt.admit_path(&pair.flat, BUDGET, None)?;
            append(&pair.dated, "branch B")?;
        } else {
            fs::write(&pair.dated, body(ID).replace("hello from codex", "jello from codex"))?;
        }
        let before = receipt.clone();
        assert!(receipt.admit_path(&pair.dated, BUDGET, None).is_err());
        assert_eq!(receipt, before);
    }
    Ok(())
}

#[test]
fn gh524_metadata_conflicts_and_concatenated_sessions_are_refused() -> Result<()> {
    let (_root, pair, mut receipt) = fixture()?;
    fs::write(&pair.dated, body("876931f0-b259-49f6-9808-771bbfb92bbe"))?;
    assert!(receipt.admit_path(&pair.dated, BUDGET, None).is_err());
    fs::write(&pair.dated, format!("{}{}", body(ID), body("876931f0-b259-49f6-9808-771bbfb92bbe")))?;
    assert!(read_snapshot(&pair.dated, &[], BUDGET, None).is_err());
    Ok(())
}

#[test]
fn gh524_shorter_catching_up_peer_requires_full_prefix_evidence() -> Result<()> {
    let (_root, pair, mut receipt) = fixture()?;
    append(&pair.dated, "second")?;
    append(&pair.dated, "third")?;
    receipt.admit_path(&pair.dated, BUDGET, None)?;
    append(&pair.flat, "second")?;
    receipt.admit_path(&pair.flat, BUDGET, None)?;
    let good = receipt.clone();
    fs::write(&pair.flat, format!("{}{}", body(ID), record("bad!!!")))?;
    assert!(receipt.admit_path(&pair.flat, BUDGET, None).is_err());
    assert_eq!(receipt, good);
    Ok(())
}

#[test]
fn gh524_removed_legacy_copy_does_not_prevent_active_appends() -> Result<()> {
    let (root, pair, mut receipt) = fixture()?;
    fs::rename(&pair.flat, root.path().join("retained-original.jsonl"))?;
    append(&pair.dated, "after relocation")?;
    receipt.admit_path(&pair.dated, BUDGET, None)?;
    assert_eq!(receipt.canonical_external_id, pair.bare_id);
    Ok(())
}

#[test]
fn gh524_invalid_layouts_do_not_become_aliases() -> Result<()> {
    let (root, pair, _) = fixture()?;
    let nested = root.path().join("source/sessions/another-project");
    fs::create_dir_all(&nested)?;
    let nested = nested.join(pair.flat.file_name().unwrap());
    fs::write(&nested, body(ID))?;
    assert!(LayoutPair::discover(&pair.flat, &nested).is_err());
    let other = root.path().join("other/sessions");
    fs::create_dir_all(&other)?;
    let other = other.join(pair.flat.file_name().unwrap());
    fs::write(&other, body(ID))?;
    let relocated = LayoutPair::discover(&pair.dated, &other)?;
    assert_eq!(relocated.flat, fs::canonicalize(other)?);
    assert_eq!(relocated.dated, pair.dated);
    assert!(LayoutPair::discover(&pair.flat, &pair.flat).is_err());
    Ok(())
}

#[test]
fn gh524_proofs_are_bounded_and_require_complete_jsonl() -> Result<()> {
    let (_root, pair, _) = fixture()?;
    assert!(read_snapshot(&pair.flat, &[], 1, None).is_err());
    let bytes = fs::read(&pair.flat)?;
    fs::write(&pair.flat, &bytes[..bytes.len() - 1])?;
    assert!(read_snapshot(&pair.flat, &[], BUDGET, None).is_err());
    fs::write(&pair.flat, record("no metadata"))?;
    assert!(read_snapshot(&pair.flat, &[], BUDGET, None).is_err());
    Ok(())
}

#[test]
fn gh524_receipts_are_source_scoped_atomic_and_round_trip() -> Result<()> {
    let (root, pair, receipt) = fixture()?;
    let data = root.path().join("data");
    let path = receipt_path(&data, &receipt.source_id, &pair.bare_id);
    assert_ne!(path, receipt_path(&data, "different-owner", &pair.bare_id));
    assert!(ReceiptLock::load_path(&path)?.is_none());
    let lock = ReceiptLock::acquire(path.clone())?;
    lock.write(&receipt)?;
    assert_eq!(lock.load()?, Some(receipt));
    assert!(ReceiptLock::acquire(path.clone()).is_err());
    drop(lock);
    fs::write(&path, b"{broken")?;
    assert!(ReceiptLock::load_path(&path).is_err());
    Ok(())
}

fn delivery_fixture(pair: &LayoutPair) -> (ScanRoot, NormalizedConversation) {
    let origin = Origin { source_id: "full-history-plan".into(), kind: SourceKind::Local, host: None };
    let root = ScanRoot::remote(pair.flat.parent().unwrap().to_owned(), origin, None);
    let conversation = NormalizedConversation {
        agent_slug: "codex".into(), external_id: Some(pair.dated_id.clone()),
        title: None, workspace: None, source_path: pair.dated.clone(),
        started_at: None, ended_at: None, metadata: json!({}), messages: Vec::new(),
    };
    (root, conversation)
}

#[test]
fn gh524_delivery_uses_the_canonical_id_and_keeps_write_ahead_proof_on_failure() -> Result<()> {
    let (temp, pair, receipt) = fixture()?;
    let data = temp.path().join("data");
    let path = receipt_path(&data, &receipt.source_id, &pair.bare_id);
    ReceiptLock::acquire(path.clone())?.write(&receipt)?;
    append(&pair.dated, "consumer may fail after committing")?;
    let (root, conversation) = delivery_fixture(&pair);
    let ctx = ScanContext::with_roots(data, vec![root.clone()], None);
    let source = DiscoveredSourceFile::new("codex", &root, pair.dated.clone(), DiscoveredSourceRole::PrimarySessionLog, true).with_fs_metadata();
    let mut seen = None;
    assert!(deliver(&ctx, &source, conversation.clone(), &mut |conversation| {
        seen = conversation.external_id;
        anyhow::bail!("synthetic consumer failure")
    }).is_err());
    assert_eq!(seen.as_deref(), Some(pair.bare_id.as_str()));
    let saved = ReceiptLock::load_path(&path)?.unwrap();
    assert!(saved.head.bytes > receipt.head.bytes);
    deliver(&ctx, &source, conversation, &mut |_| Ok(()))?;
    assert_eq!(ReceiptLock::load_path(&path)?.unwrap(), saved);
    Ok(())
}

#[test]
fn watch_once_rollout_as_data_dir_is_delivered_unchanged() -> Result<()> {
    // `cass index --watch-once <rollout>` scans with the rollout file itself as
    // data_dir; a registry lookup under it failed with ENOTDIR.
    let (_temp, pair, _receipt) = fixture()?;
    let (root, conversation) = delivery_fixture(&pair);
    let ctx = ScanContext::with_roots(pair.dated.clone(), vec![root.clone()], None);
    let source = DiscoveredSourceFile::new(
        "codex",
        &root,
        pair.dated.clone(),
        DiscoveredSourceRole::PrimarySessionLog,
        true,
    )
    .with_fs_metadata();
    let mut delivered = Vec::new();
    deliver(&ctx, &source, conversation.clone(), &mut |conversation| {
        delivered.push(conversation.external_id);
        Ok(())
    })?;
    assert_eq!(delivered, vec![conversation.external_id]);
    Ok(())
}

#[test]
fn gh524_other_agents_and_sources_keep_their_original_ids() -> Result<()> {
    let (temp, pair, receipt) = fixture()?;
    let data = temp.path().join("data");
    ReceiptLock::acquire(receipt_path(&data, &receipt.source_id, &pair.bare_id))?.write(&receipt)?;
    let (mut root, mut conversation) = delivery_fixture(&pair);
    for other_agent in [true, false] {
        if other_agent { conversation.agent_slug = "claude".into(); }
        else { conversation.agent_slug = "codex".into(); root.origin.source_id = "different-owner".into(); }
        let ctx = ScanContext::with_roots(data.clone(), vec![root.clone()], None);
        let source = DiscoveredSourceFile::new("codex", &root, pair.dated.clone(), DiscoveredSourceRole::PrimarySessionLog, true).with_fs_metadata();
        let expected = conversation.external_id.clone();
        deliver(&ctx, &source, conversation.clone(), &mut |conversation| {
            assert_eq!(conversation.external_id, expected);
            Ok(())
        })?;
    }
    Ok(())
}
