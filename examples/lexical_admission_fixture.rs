//! Generate a deterministic disposable lexical fixture without a canonical DB.
//! Usage: cargo run --release --example lexical_admission_fixture -- DIR [DOCS]
use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use coding_agent_search::search::quill_bridge::QuillCassIndex;
use frankensearch::quill::cass::CassDocument;

fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let path = PathBuf::from(
        args.next()
            .context("usage: lexical_admission_fixture DIR [DOCS]")?,
    );
    let docs: u64 = args
        .next()
        .map(|v| v.to_string_lossy().parse())
        .transpose()?
        .unwrap_or(4096);
    ensure!(
        args.next().is_none() && (32..=1_000_000).contains(&docs),
        "DOCS must be 32..1000000"
    );
    // Refuse existing destinations, including symlinks; never open a user archive.
    std::fs::create_dir(&path).context("fixture destination must not exist")?;
    let mut writer = QuillCassIndex::open_or_create(&path)?;
    for start in (0..docs).step_by(256) {
        let batch: Vec<_> = (start..docs.min(start + 256))
            .map(|msg_idx| {
                let content = (0..128)
                    .map(|word| format!("token{} ", (word * 37 + msg_idx) % 65521))
                    .collect::<String>();
                CassDocument {
                    agent: "codex".into(),
                    workspace: Some("/fixture".into()),
                    workspace_original: None,
                    source_path: "/fixture/session.jsonl".into(),
                    msg_idx,
                    created_at: Some(1_700_000_000),
                    title: Some(format!("admission fixture {msg_idx}")),
                    content: format!("performance indexing integrity {content}"),
                    source_id: "local".into(),
                    origin_kind: "local".into(),
                    origin_host: None,
                    conversation_id: Some(1),
                }
            })
            .collect();
        writer.add_cass_documents(&batch)?;
        if (start / 256 + 1) % 4 == 0 {
            writer.commit()?;
        }
    }
    writer.commit()?;
    drop(writer);
    let manifest = frankensearch::quill::load_manifest_pair(&path)?;
    println!(
        "{}",
        serde_json::json!({"path": path, "docs": docs,
        "live_segments": manifest.manifest.segments.len()})
    );
    Ok(())
}
