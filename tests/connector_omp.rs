//! First-class Oh My Pi v18 integration gates.

use std::fs;
use std::path::{Path, PathBuf};

use coding_agent_search::connectors::{
    Connector, Origin, Platform, ScanContext, ScanRoot, extract_tokens_for_agent,
    omp::OmpConnector, pi_agent::PiAgentConnector,
};
use serde_json::json;

fn write_omp_session(agent_root: &Path, id: &str, title: &str) -> PathBuf {
    let session_dir = agent_root.join("sessions/-projects-cass");
    fs::create_dir_all(&session_dir).expect("create OMP session directory");
    let path = session_dir.join(format!("2026-08-23T12-00-00_{id}.jsonl"));
    let transcript = [
        json!({"type":"title","title":title}),
        json!({"type":"session","version":3,"id":id,"timestamp":"2026-08-23T12:00:00Z","cwd":"/projects/cass"}),
        json!({"type":"model_change","timestamp":"2026-08-23T12:00:01Z","model":"openrouter/stealth/ox-alpha"}),
        json!({"type":"message","timestamp":"2026-08-23T12:00:02Z","message":{"role":"user","content":"index OMP"}}),
        json!({"type":"message","timestamp":"2026-08-23T12:00:03Z","message":{"role":"assistant","model":"openrouter/stealth/ox-alpha","content":"done"}}),
    ]
    .into_iter()
    .map(|entry| entry.to_string())
    .collect::<Vec<_>>()
    .join("\n");
    fs::write(&path, format!("{transcript}\n")).expect("write OMP session");
    path
}

#[test]
fn omp_v18_profiles_are_first_class_and_not_scanned_by_pi_agent() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("copied-home");
    let default_agent = home.join(".omp/agent");
    let profile_agent = home.join(".omp/profiles/work/agent");
    write_omp_session(&default_agent, "omp-default", "Default OMP session");
    write_omp_session(&profile_agent, "omp-work", "Profile OMP session");

    let ctx = ScanContext::with_roots(
        temp.path().join("cass-state"),
        vec![ScanRoot::local(home.clone())],
        None,
    );
    let mut conversations = OmpConnector::new().scan(&ctx).expect("scan OMP fixtures");
    conversations.sort_by(|left, right| left.external_id.cmp(&right.external_id));

    assert_eq!(conversations.len(), 2);
    for conversation in &conversations {
        assert_eq!(conversation.agent_slug, "omp");
        assert_eq!(conversation.metadata["source"], "omp");
        assert_eq!(conversation.metadata["model_id"], "openrouter/stealth/ox-alpha");
    }
    let profile = conversations
        .iter()
        .find(|conversation| conversation.title.as_deref() == Some("Profile OMP session"))
        .expect("profile conversation");
    assert_eq!(profile.metadata["profile"], "work");

    let pi_conversations = PiAgentConnector::new()
        .scan(&ctx)
        .expect("scan Pi Agent against the same copied home");
    assert!(
        pi_conversations.is_empty(),
        "the dedicated Pi Agent connector must not duplicate OMP sessions"
    );
}

#[test]
fn omp_remote_discovery_preserves_origin_and_platform() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("remote-home");
    write_omp_session(&home.join(".omp/agent"), "omp-remote", "Remote OMP");
    let ctx = ScanContext::with_roots(
        temp.path().join("cass-state"),
        vec![ScanRoot::remote(
            home,
            Origin::remote_with_host("build-host", "build-host.example"),
            Some(Platform::Linux),
        )],
        None,
    );

    let sources = OmpConnector::new()
        .discover_source_files(&ctx)
        .expect("discover remote OMP fixture");
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].provider_slug, "omp");
    assert!(sources[0].origin.is_remote());
    assert_eq!(sources[0].platform, Some(Platform::Linux));
}

#[test]
fn omp_token_extraction_uses_the_pi_family_model_schema() {
    let usage = extract_tokens_for_agent(
        "omp",
        &json!({"message":{"model":"openrouter/stealth/ox-alpha"}}),
        "answer",
        "assistant",
    );
    assert_eq!(usage.model_name.as_deref(), Some("openrouter/stealth/ox-alpha"));
    assert_eq!(usage.provider.as_deref(), Some("openrouter"));
}
