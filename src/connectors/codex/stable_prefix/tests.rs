use super::*;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicUsize, Ordering};

fn fixture(bytes: &[u8]) -> Result<(tempfile::TempDir, PathBuf)> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("rollout-prefix.jsonl");
    fs::write(&path, bytes)?;
    Ok((directory, path))
}

fn snapshot(path: &Path) -> Result<StablePrefixSnapshot> {
    StablePrefixSnapshot::capture(path, 1024 * 1024, None, None)?
        .context("fixture must have a complete prefix")
}

fn conversation(path: &Path, contents: &[&str]) -> NormalizedConversation {
    NormalizedConversation {
        agent_slug: "codex".into(),
        external_id: Some("original-session".into()),
        title: None,
        workspace: None,
        source_path: path.to_path_buf(),
        started_at: None,
        ended_at: None,
        metadata: serde_json::json!({}),
        messages: contents
            .iter()
            .enumerate()
            .map(|(index, text)| NormalizedMessage {
                idx: index as i64,
                role: "assistant".into(),
                author: None,
                created_at: Some(index as i64),
                content: (*text).into(),
                extra: serde_json::json!({}),
                snippets: Vec::new(),
                invocations: Vec::new(),
            })
            .collect(),
    }
}

#[test]
fn exact_boundary_and_digest_cover_only_newline_terminated_records() -> Result<()> {
    for tail in [
        b"{\"cut\":".as_slice(),
        b"{\"valid\":true}".as_slice(),
        b"\xf0\x9f".as_slice(),
    ] {
        let prefix = b"{\"first\":true}\r\n{\"second\":true}\n";
        let bytes = [prefix.as_slice(), tail].concat();
        let (_directory, path) = fixture(&bytes)?;
        let captured = snapshot(&path)?;
        assert_eq!(fs::read(captured.path())?, prefix);
        assert_eq!(captured.receipt.admitted_bytes, bytes.len() as u64);
        assert_eq!(captured.receipt.committed_bytes, prefix.len() as u64);
        assert_eq!(
            captured.receipt.blake3,
            blake3::hash(prefix).to_hex().to_string()
        );
        assert!(captured.receipt_for(None, None)?.partial);
        assert_eq!(fs::read(&path)?, bytes);
    }
    Ok(())
}

#[test]
fn boundary_hash_survives_buffer_edges_and_a_large_unfinished_tail() -> Result<()> {
    for boundary in [
        COPY_BUFFER_BYTES - 1,
        COPY_BUFFER_BYTES,
        COPY_BUFFER_BYTES + 1,
    ] {
        let mut bytes = vec![b'a'; boundary - 1];
        bytes.push(b'\n');
        let prefix = bytes.clone();
        bytes.extend(vec![b'z'; COPY_BUFFER_BYTES * 2 + 7]);
        let (_directory, path) = fixture(&bytes)?;
        let captured = snapshot(&path)?;
        assert_eq!(captured.receipt.committed_bytes, boundary as u64);
        assert_eq!(fs::read(captured.path())?, prefix);
        assert_eq!(
            captured.receipt.blake3,
            blake3::hash(&prefix).to_hex().to_string()
        );
    }
    Ok(())
}

#[test]
fn continuous_appends_cannot_extend_the_admitted_read() -> Result<()> {
    let bytes = b"{\"original\":true}\n".repeat(12_000);
    let (_directory, path) = fixture(&bytes)?;
    let appended = AtomicUsize::new(0);
    let tick = || {
        let sequence = appended.fetch_add(1, Ordering::Relaxed);
        writeln!(
            OpenOptions::new().append(true).open(&path).unwrap(),
            "{{\"later\":{sequence}}}"
        )
        .unwrap();
    };
    let captured =
        StablePrefixSnapshot::capture(&path, 1024 * 1024, None, Some(&tick))?.context("prefix")?;
    assert!(appended.load(Ordering::Relaxed) > 1);
    assert!(fs::metadata(&path)?.len() > bytes.len() as u64);
    assert_eq!(captured.receipt.admitted_bytes, bytes.len() as u64);
    assert_eq!(fs::read(captured.path())?, bytes);
    captured.validate(Some(&tick))?;
    assert!(captured.receipt_for(None, None)?.partial);
    Ok(())
}

#[test]
fn edits_replacement_and_truncation_never_pass_as_appends() -> Result<()> {
    for mutation in [
        "same-size",
        "append-and-rewrite",
        "truncate",
        "regrow",
        "replace",
        "symlink",
    ] {
        let original = b"{\"original\":true}\n";
        let (_directory, path) = fixture(original)?;
        let captured = snapshot(&path)?;
        let mtime = fs::metadata(&path)?.modified()?;
        match mutation {
            "same-size" | "append-and-rewrite" | "regrow" => {
                if mutation == "regrow" {
                    OpenOptions::new().write(true).open(&path)?.set_len(0)?;
                }
                fs::write(&path, b"{\"modified\":true}\n")?;
                if mutation == "append-and-rewrite" {
                    OpenOptions::new()
                        .append(true)
                        .open(&path)?
                        .write_all(b"{\"later\":true}\n")?;
                }
            }
            "truncate" => OpenOptions::new().write(true).open(&path)?.set_len(1)?,
            "replace" | "symlink" => {
                let retained = path.with_extension("retained");
                fs::rename(&path, &retained)?;
                if mutation == "replace" {
                    fs::write(&path, original)?;
                } else {
                    std::os::unix::fs::symlink(retained, &path)?;
                }
            }
            _ => unreachable!(),
        }
        File::open(&path)?.set_modified(mtime)?;
        assert!(captured.validate(None).is_err(), "{mutation}");
        assert!(captured.receipt.validate_source().is_err(), "{mutation}");
    }
    Ok(())
}

#[test]
fn advisory_lock_revokes_prefix_admission_and_delivery() -> Result<()> {
    let (_directory, path) = fixture(b"{\"original\":true}\n")?;
    let captured = snapshot(&path)?;
    let _delivery = PrefixDeliveryGuard::enter(captured.receipt.clone());
    assert!(guarded_prefix_delivery(&path));
    let lock = File::open(&path)?;
    lock.try_lock()?;
    assert!(!can_attempt(&path, 1024));
    assert!(!guarded_prefix_delivery(&path));
    assert!(captured.validate(None).is_err());
    lock.unlock()?;
    assert!(guarded_prefix_delivery(&path));
    Ok(())
}

#[test]
fn capture_is_private_and_reconstruction_keeps_the_original_boundary() -> Result<()> {
    let bytes = b"{\"first\":true}\n{\"tail\":";
    let (_directory, path) = fixture(bytes)?;
    let captured = snapshot(&path)?;
    assert_eq!(
        fs::metadata(captured.path())?.permissions().mode() & 0o777,
        0o400
    );
    assert_eq!(
        fs::metadata(captured.path().parent().unwrap())?
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    OpenOptions::new()
        .append(true)
        .open(&path)?
        .write_all(b"true}\n")?;
    let reconstructed = captured.receipt.capture_view()?;
    assert_eq!(fs::read(reconstructed.path())?, b"{\"first\":true}\n");
    assert_eq!(fs::read(captured.path())?, fs::read(reconstructed.path())?);
    Ok(())
}

#[test]
fn cross_scan_overlap_and_message_projection_must_both_extend() -> Result<()> {
    let (_directory, path) = fixture(b"{\"first\":true}\n")?;
    let first = snapshot(&path)?;
    let prior = first.receipt_for(Some(&conversation(&path, &["first"])), None)?;
    OpenOptions::new()
        .append(true)
        .open(&path)?
        .write_all(b"{\"second\":true}\n")?;
    let second = StablePrefixSnapshot::capture(&path, 1024, Some(&prior), None)?.unwrap();
    let next = second.receipt_for(
        Some(&conversation(&path, &["first", "second"])),
        Some(&prior),
    )?;
    assert!(next.committed_bytes > prior.committed_bytes);
    assert_eq!(next.message_count, 2);
    for contents in [&["second", "first"][..], &["revised"][..], &[][..]] {
        assert!(
            second
                .receipt_for(Some(&conversation(&path, contents)), Some(&prior))
                .is_err()
        );
    }
    fs::write(&path, b"{\"other\":true}\n{\"second\":true}\n")?;
    assert!(StablePrefixSnapshot::capture(&path, 1024, Some(&prior), None).is_err());
    Ok(())
}

#[test]
fn publication_invalidation_retains_overlap_without_delivery_authority() -> Result<()> {
    let (_directory, path) = fixture(b"{\"first\":true}\n")?;
    let captured = snapshot(&path)?;
    let receipt = captured.receipt_for(Some(&conversation(&path, &["first"])), None)?;
    let saved = serde_json::json!({
        "publication_invalidated": true, "watch_published": false,
        "codex_prefix": receipt, "producer_contract": "fixture",
    })
    .to_string();
    set_previous_prefix_receipt(&path, Some(&saved))?;
    assert!(current_prefix_receipt(&path).is_none());
    assert!(!guarded_prefix_delivery(&path));
    assert_eq!(take_previous_prefix_receipt(&path)?, Some(receipt));
    assert!(take_previous_prefix_receipt(&path)?.is_none());
    fs::write(&path, b"{\"other\":true}\n")?;
    assert!(set_previous_prefix_receipt(&path, Some(&saved)).is_err());
    Ok(())
}

#[test]
fn receipt_scope_is_thread_bound_and_completion_does_not_admit_active_delivery() -> Result<()> {
    let (_directory, path) = fixture(b"{\"first\":true}\n")?;
    let captured = snapshot(&path)?;
    {
        let _guard = PrefixDeliveryGuard::enter(captured.receipt.clone());
        assert!(guarded_prefix_delivery(&path));
        let foreign_thread = path.clone();
        assert!(
            !std::thread::spawn(move || guarded_prefix_delivery(&foreign_thread))
                .join()
                .unwrap()
        );
        {
            let _complete = PrefixDeliveryGuard::enter_completion(captured.receipt.clone());
            assert!(current_prefix_receipt(&path).is_some());
            assert!(!guarded_prefix_delivery(&path));
        }
        assert!(guarded_prefix_delivery(&path));
    }
    assert!(current_prefix_receipt(&path).is_none());
    Ok(())
}

#[test]
fn unsupported_formats_empty_input_and_budget_edges_keep_strict_semantics() -> Result<()> {
    let (directory, path) = fixture(b"{\"first\":true}\n")?;
    let bytes = fs::read(&path)?;
    assert!(can_attempt(&path, bytes.len() as u64));
    assert!(!can_attempt(&path, bytes.len() as u64 - 1));
    assert!(StablePrefixSnapshot::capture(&path, bytes.len() as u64 - 1, None, None).is_err());
    for name in ["session.jsonl", "rollout-old.json", "rollout-old.jsonl.zst"] {
        let other = directory.path().join(name);
        fs::write(&other, &bytes)?;
        assert!(!eligible(&other));
        assert!(!can_attempt(&other, 1024));
    }
    fs::write(&path, b"")?;
    assert!(StablePrefixSnapshot::capture(&path, 1024, None, None)?.is_none());
    fs::write(&path, b"{\"valid\":true}")?;
    assert!(!can_attempt(&path, 1024));
    let error = match StablePrefixSnapshot::capture(&path, 1024, None, None) {
        Err(error) => error,
        Ok(_) => anyhow::bail!("an active first record without LF must retain retry debt"),
    };
    assert_eq!(
        error.downcast_ref::<io::Error>().map(io::Error::kind),
        Some(io::ErrorKind::UnexpectedEof),
    );
    Ok(())
}

#[test]
fn previously_live_abandoned_tail_returns_to_existing_inactive_eof_policy() -> Result<()> {
    let bytes = b"{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":\"first\"}}\n{\"cut\":";
    let (directory, path) = fixture(bytes)?;
    let first = snapshot(&path)?;
    let mut parsed = conversation(&path, &["first"]);
    parsed.messages[0].role = "user".into();
    parsed.messages[0].created_at = None;
    let prior = first.receipt_for(Some(&parsed), None)?;
    assert!(prior.partial);
    File::open(&path)?
        .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(7200))?;
    let recovered = StablePrefixSnapshot::capture(&path, 1024, Some(&prior), None)?.unwrap();
    assert_eq!(fs::read(recovered.path())?, bytes);
    let receipt = recovered.receipt_for(Some(&parsed), Some(&prior))?;
    assert!(receipt.inactive_eof);
    assert!(!receipt.partial);
    assert_eq!(receipt.committed_bytes, bytes.len() as u64);

    // Exercise the real pinned parser and CASS enrichment on that private EOF
    // view; both receive the original inactivity decision via preserved mtime.
    let ctx = super::super::ScanContext::with_roots(
        directory.path().join("data"),
        vec![super::super::ScanRoot::local(path.clone())],
        None,
    );
    let saved = serde_json::json!({"codex_prefix": prior}).to_string();
    let mut before = |source: &super::super::DiscoveredSourceFile| {
        set_previous_prefix_receipt(&source.source_path, Some(&saved)).unwrap();
        true
    };
    let mut conversations = Vec::new();
    let mut completed = 0;
    let mut complete = |done: &franken_agent_detection::SourceCompletion| {
        let receipt = current_prefix_receipt(&done.source.source_path).unwrap();
        assert!(!receipt.partial);
        assert!(receipt.inactive_eof);
        completed += 1;
        Ok(())
    };
    super::super::Connector::scan_with_source_boundaries(
        &super::super::CodexConnector::new(),
        &ctx,
        &mut franken_agent_detection::SourceScanHooks {
            should_scan_source: Some(&mut before),
            on_source_complete: Some(&mut complete),
        },
        &mut |conversation| {
            conversations.push(conversation);
            Ok(())
        },
    )?;
    assert_eq!(completed, 1);
    assert_eq!(conversations.len(), 1);
    assert_eq!(conversations[0].messages[0].content, "first");
    assert_eq!(fs::read(&path)?, bytes);
    Ok(())
}

#[test]
fn header_only_and_blank_prefixes_do_not_fabricate_completions() -> Result<()> {
    for bytes in [
        b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"header-only\"}}\n".as_slice(),
        b"\n\n".as_slice(),
    ] {
        let (directory, path) = fixture(bytes)?;
        let ctx = super::super::ScanContext::with_roots(
            directory.path().join("data"),
            vec![super::super::ScanRoot::local(path)],
            None,
        );
        let mut complete = |_: &franken_agent_detection::SourceCompletion| {
            panic!("zero-output source must not invent completion authority");
        };
        super::super::Connector::scan_with_source_boundaries(
            &super::super::CodexConnector::new(),
            &ctx,
            &mut franken_agent_detection::SourceScanHooks {
                should_scan_source: None,
                on_source_complete: Some(&mut complete),
            },
            &mut |_| panic!("zero-output source must not invent a conversation"),
        )?;
    }
    Ok(())
}

#[test]
fn retained_descriptor_receipt_allows_appends_and_rejects_object_substitution() -> Result<()> {
    let original = b"{\"original\":true}\n";
    let (directory, path) = fixture(original)?;
    let captured = snapshot(&path)?;
    let receipt = captured.receipt_for(None, None)?;
    let mut opened = File::open(&path)?;
    receipt.validate_opened_source(&opened)?;
    assert_eq!(opened.stream_position()?, receipt.committed_bytes);

    OpenOptions::new()
        .append(true)
        .open(&path)?
        .write_all(b"{\"later\":true}\n")?;
    receipt.validate_opened_source(&opened)?;
    opened.rewind()?;
    let mut admitted = Vec::new();
    (&mut opened)
        .take(receipt.committed_bytes)
        .read_to_end(&mut admitted)?;
    assert_eq!(admitted, original);

    let foreign = directory.path().join("rollout-foreign.jsonl");
    fs::write(&foreign, fs::read(&path)?)?;
    assert!(
        receipt.validate_opened_source(&File::open(&foreign)?).is_err(),
        "identical bytes in another opened object are not the admitted descriptor"
    );
    let retained = path.with_extension("retained");
    fs::rename(&path, &retained)?;
    fs::rename(&foreign, &path)?;
    assert!(
        receipt.validate_opened_source(&opened).is_err(),
        "the retained object cannot certify a replacement at its original name"
    );

    fs::rename(&path, directory.path().join("replacement-retained"))?;
    fs::rename(&retained, &path)?;
    receipt.validate_opened_source(&opened)?;
    let modified = fs::metadata(&path)?.modified()?;
    let mut writer = OpenOptions::new().write(true).open(&path)?;
    writer.write_all(b"{\"modified\":true}\n")?;
    writer.set_modified(modified)?;
    assert!(
        receipt.validate_opened_source(&opened).is_err(),
        "restoring mtime cannot hide a rewrite of the admitted bytes"
    );
    Ok(())
}
