use super::*;
use serde_json::json;

#[test]
fn receipt_provider_binding_preserves_legacy_codex_and_refuses_cross_parser_markers() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("rollout-shared-name.jsonl");
    fs::write(&path, b"{\"record\":true}\n")?;
    let mut receipts = Vec::new();
    for provider in [
        StablePrefixProvider::Codex,
        StablePrefixProvider::ClaudeCode,
    ] {
        let snapshot = StablePrefixSnapshot::capture_for(provider, &path, 1024, None, None)?
            .context("complete record")?;
        let receipt = snapshot.receipt_for(None, None)?;
        let encoded = serde_json::to_value(&receipt)?;
        match provider {
            StablePrefixProvider::Codex => {
                assert!(!encoded.as_object().unwrap().contains_key("provider"))
            }
            StablePrefixProvider::ClaudeCode => assert_eq!(encoded["provider"], "claude_code"),
        }
        let observation = json!({provider.observation_key(): encoded.clone()});
        assert_eq!(observation_receipt(&observation)?, Some(receipt.clone()));
        assert_eq!(
            metadata_receipt(&json!({"cass": {provider.metadata_key(): encoded}}))?,
            Some(receipt.clone())
        );
        let other = match provider {
            StablePrefixProvider::Codex => StablePrefixProvider::ClaudeCode,
            StablePrefixProvider::ClaudeCode => StablePrefixProvider::Codex,
        };
        assert!(
            set_previous_prefix_receipt_for(other, &path, Some(&observation.to_string())).is_err()
        );
        assert!(take_previous_prefix_receipt_for(other, &path)?.is_none());
        let _delivery = PrefixDeliveryGuard::enter(receipt.clone());
        assert!(guarded_prefix_delivery_for(provider, &path));
        assert!(!guarded_prefix_delivery_for(other, &path));
        receipts.push(receipt);
    }
    assert!(observation_receipt(&json!({"codex_prefix":receipts[1]})).is_err());
    assert!(observation_receipt(&json!({"claude_prefix":receipts[0]})).is_err());
    assert!(
        observation_receipt(&json!({"codex_prefix":receipts[0],"claude_prefix":receipts[1]}))
            .is_err()
    );
    assert!(metadata_receipt(&json!({"cass":{"codex_stable_prefix":receipts[1]}})).is_err());
    assert!(
        metadata_receipt(
            &json!({"cass":{"codex_stable_prefix":receipts[0],"claude_stable_prefix":receipts[1]}})
        )
        .is_err()
    );
    let mut unknown = serde_json::to_value(&receipts[1])?;
    unknown["provider"] = json!("other_jsonl");
    assert!(observation_receipt(&json!({"claude_prefix":unknown})).is_err());
    assert!(metadata_receipt(&json!({"cass":"corrupt"})).is_err());
    assert!(observation_receipt(&json!({"claude_prefix":null})).is_err());
    Ok(())
}

#[test]
fn claude_admission_does_not_expand_codex_or_sidecar_eligibility() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("session.jsonl");
    fs::write(&path, b"{\"type\":\"user\"}\n")?;
    assert!(can_attempt_for(
        StablePrefixProvider::ClaudeCode,
        &path,
        1024
    ));
    assert!(!can_attempt(&path, 1024));
    let sidecar = directory
        .path()
        .join("claude-code-sessions")
        .join("session.jsonl");
    fs::create_dir_all(sidecar.parent().unwrap())?;
    fs::write(&sidecar, b"{\"type\":\"user\"}\n")?;
    assert!(!can_attempt_for(
        StablePrefixProvider::ClaudeCode,
        &sidecar,
        1024
    ));
    let rewritten = directory.path().join("session.json");
    fs::write(&rewritten, b"{\"messages\":[]}\n")?;
    assert!(!can_attempt_for(
        StablePrefixProvider::ClaudeCode,
        &rewritten,
        1024
    ));
    assert_eq!(
        StablePrefixProvider::from_slug("claude_code"),
        Some(StablePrefixProvider::ClaudeCode)
    );
    for provider in ["claude", "pi_agent", "omp", "opencode", "unknown"] {
        assert!(StablePrefixProvider::from_slug(provider).is_none());
    }
    Ok(())
}
