//! Non-mutating checks for the recovery entrypoint embedded in the CASS binary.
//! Apply/crash behavior remains covered by the existing companion/library suites.

use anyhow::Result;
use coding_agent_search::search::semantic_manifest::{ArtifactRecord, SemanticManifest, TierKind};
use frankensearch::index::{Quantization, VectorIndex};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn seed(data: &Path) -> Result<()> {
    let root = data.join("vector_index");
    fs::create_dir_all(&root)?;
    let relative = "vector_index/.staging-fast-fnv1a-384-11111111.fsvi";
    let live = data.join(relative);
    let mut writer = VectorIndex::create_with_revision(
        &live,
        "fnv1a-384",
        "preview-fixture",
        4,
        Quantization::F16,
    )?;
    writer.write_record("retained-document", &[1.0, 0.0, 0.0, 0.0])?;
    writer.finish()?;
    let mut manifest = SemanticManifest {
        fast_tier: Some(ArtifactRecord {
            tier: TierKind::Fast,
            embedder_id: "fnv1a-384".into(),
            model_revision: "preview-fixture".into(),
            schema_version: 1,
            chunking_version: 1,
            dimension: 4,
            doc_count: 1,
            conversation_count: 1,
            db_fingerprint: "preview-fixture".into(),
            index_path: relative.into(),
            size_bytes: fs::metadata(&live)?.len(),
            started_at_ms: 1,
            completed_at_ms: 1,
            ready: false,
        }),
        ..Default::default()
    };
    manifest.save(data)?;
    fs::write(
        root.join(".staging-quality-minilm-384-22222222.fsvi"),
        b"scratch",
    )?;
    let reuse = root.join(".backfill-reuse-Preview123");
    fs::create_dir(&reuse)?;
    fs::write(reuse.join("candidate.fsvi"), b"copy")?;
    Ok(())
}

fn files(data: &Path) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    let mut result = BTreeMap::new();
    let mut pending = vec![data.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            let relative = path.strip_prefix(data)?;
            if relative == Path::new("index-run.lock")
                || relative == Path::new("semantic-backfill-artifacts.lock")
            {
                continue; // Preview is allowed to create these two lock files.
            }
            if entry.file_type()?.is_dir() {
                pending.push(path);
            } else {
                result.insert(relative.to_path_buf(), fs::read(&path)?);
            }
        }
    }
    Ok(result)
}

fn result(output: Output) -> Result<Value> {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    Ok(serde_json::from_slice(&output.stdout)?)
}

#[test]
fn both_executables_preview_the_same_inventory_and_preserve_owned_bytes() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path().join("archive with spaces");
    seed(&data)?;
    let before = files(&data)?;
    let companion = result(
        Command::new(env!("CARGO_BIN_EXE_cass-semantic-reclaim"))
            .arg("--data-dir")
            .arg(&data)
            .arg("--json")
            .output()?,
    )?;
    let shipped = result(
        Command::new(env!("CARGO_BIN_EXE_cass"))
            .args(["semantic-reclaim", "--data-dir"])
            .arg(&data)
            .args(["--json", "--dry-run"])
            .output()?,
    )?;
    assert_eq!(shipped, companion);
    assert_eq!(shipped["operation"], "preview");
    assert_eq!(shipped["plan"]["reclaimable_files"], 1);
    assert_eq!(shipped["plan"]["reclaimable_directories"], 1);
    assert_eq!(shipped["plan"]["reclaimable_bytes"], 11);
    assert_eq!(files(&data)?, before);
    assert!(!data.join("agent_search.db").exists());
    Ok(())
}

#[test]
fn prefix_options_do_not_select_the_environment_default_archive() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path().join("explicit");
    let unselected = temp.path().join("unselected");
    seed(&data)?;
    let before = files(&data)?;
    let plan = result(
        Command::new(env!("CARGO_BIN_EXE_cass"))
            .env("CASS_DATA_DIR", &unselected)
            .arg("--data-dir")
            .arg(&data)
            .args(["--robot", "semantic-reclaim", "--dry-run"])
            .output()?,
    )?;
    assert_eq!(plan["plan"]["reclaimable_bytes"], 11);
    assert_eq!(files(&data)?, before);
    assert!(!unselected.exists());
    Ok(())
}

#[test]
fn missing_archive_argument_and_unknown_flags_are_usage_errors_without_io() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let absent = temp.path().join("must-not-be-created");
    for extra in [vec![], vec!["--force"], vec!["--apply"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_cass"))
            .env("CASS_DATA_DIR", &absent)
            .args(["semantic-reclaim", "--json"])
            .args(extra)
            .output()?;
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        let error: Value = serde_json::from_slice(&output.stderr)?;
        assert_eq!(error["error"]["kind"], "usage");
        assert!(!absent.exists());
    }
    Ok(())
}

#[test]
fn root_and_subcommand_help_discover_the_recovery_options() -> Result<()> {
    let output = Command::new(env!("CARGO_BIN_EXE_cass"))
        .arg("--help")
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("semantic-reclaim"));
    for args in [["semantic-reclaim", "--help"], ["help", "semantic-reclaim"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_cass"))
            .args(args)
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stderr.is_empty());
        let help = String::from_utf8_lossy(&output.stdout);
        assert!(help.contains("--data-dir") && help.contains("--plan-fingerprint"));
        assert!(help.contains("No database or model is opened"));
    }
    Ok(())
}
