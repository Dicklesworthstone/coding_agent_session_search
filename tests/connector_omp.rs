//! First-class Oh My Pi v18 integration gates.

use std::fs;
use std::path::{Path, PathBuf};

use coding_agent_search::connectors::{
    Connector, Origin, Platform, ScanContext, ScanRoot, extract_tokens_for_agent,
    get_connector_factories, omp::OmpConnector,
};
use coding_agent_search::sources::sync::path_to_safe_dirname;
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

fn write_omp_subagent(main_session: &Path, id: &str) -> PathBuf {
    let subagent_dir = main_session.with_extension("");
    fs::create_dir_all(&subagent_dir).expect("create OMP sub-agent directory");
    let path = subagent_dir.join("Researcher.jsonl");
    let transcript = [
        json!({"type":"session","version":3,"id":id,"timestamp":"2026-08-23T12:01:00Z","cwd":"/projects/cass"}),
        json!({"type":"model_change","timestamp":"2026-08-23T12:01:01Z","model":"openrouter/stealth/ox-alpha"}),
        json!({"type":"message","timestamp":"2026-08-23T12:01:02Z","message":{"role":"user","content":"OMP sub-agent task"}}),
        json!({"type":"message","timestamp":"2026-08-23T12:01:03Z","message":{"role":"assistant","model":"openrouter/stealth/ox-alpha","content":"sub-agent done"}}),
    ]
    .into_iter()
    .map(|entry| entry.to_string())
    .collect::<Vec<_>>()
    .join("\n");
    fs::write(&path, format!("{transcript}\n")).expect("write OMP sub-agent session");
    path
}

fn runtime_connector(name: &str) -> Box<dyn Connector + Send> {
    get_connector_factories()
        .into_iter()
        .find_map(|(slug, factory)| (slug == name).then_some(factory))
        .unwrap_or_else(|| panic!("missing runtime connector factory for {name}"))()
}

#[test]
fn omp_v18_profiles_are_first_class_and_not_scanned_by_pi_agent() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("copied-home");
    let default_agent = home.join(".omp/agent");
    let profile_agent = home.join(".omp/profiles/work/agent");
    let default_session = write_omp_session(&default_agent, "omp-default", "Default OMP session");
    write_omp_subagent(&default_session, "omp-default-researcher");
    write_omp_session(&profile_agent, "omp-work", "Profile OMP session");

    let ctx = ScanContext::with_roots(
        temp.path().join("cass-state"),
        vec![ScanRoot::local(home.clone())],
        None,
    );
    let mut conversations = runtime_connector("omp")
        .scan(&ctx)
        .expect("scan OMP fixtures through the production registry");
    conversations.sort_by(|left, right| left.external_id.cmp(&right.external_id));

    assert_eq!(conversations.len(), 3);
    for conversation in &conversations {
        assert_eq!(conversation.agent_slug, "omp");
        assert_eq!(conversation.metadata["source"], "omp");
        assert_eq!(
            conversation.metadata["model_id"],
            "openrouter/stealth/ox-alpha"
        );
    }
    let profile = conversations
        .iter()
        .find(|conversation| conversation.title.as_deref() == Some("Profile OMP session"))
        .expect("profile conversation");
    assert_eq!(profile.metadata["profile"], "work");
    assert!(
        conversations.iter().any(|conversation| {
            conversation.source_path.ends_with("Researcher.jsonl")
                && conversation
                    .messages
                    .iter()
                    .any(|message| message.content == "OMP sub-agent task")
        }),
        "OMP sub-agent transcripts must remain independently searchable"
    );

    let pi_conversations = runtime_connector("pi_agent")
        .scan(&ctx)
        .expect("scan Pi Agent through the production registry against the same copied home");
    assert!(
        pi_conversations.is_empty(),
        "the dedicated Pi Agent connector must not duplicate OMP sessions"
    );
}

#[test]
fn omp_direct_profile_root_preserves_profile_metadata() {
    let temp = tempfile::tempdir().expect("tempdir");
    let profile_agent = temp.path().join(".omp/profiles/review/agent");
    write_omp_session(&profile_agent, "omp-review", "Profile root OMP session");
    let ctx = ScanContext::with_roots(
        temp.path().join("cass-state"),
        vec![ScanRoot::local(profile_agent.join("sessions"))],
        None,
    );

    let conversations = runtime_connector("omp")
        .scan(&ctx)
        .expect("scan a direct OMP profile root through the production registry");

    assert_eq!(conversations.len(), 1);
    assert_eq!(conversations[0].agent_slug, "omp");
    assert_eq!(conversations[0].metadata["profile"], "review");
}

#[test]
fn direct_pi_sessions_root_is_never_parsed_as_omp() {
    let temp = tempfile::tempdir().expect("tempdir");
    let pi_agent = temp.path().join(".pi/agent");
    write_omp_session(&pi_agent, "pi-only", "Pi-only session");
    let ctx = ScanContext::with_roots(
        temp.path().join("cass-state"),
        vec![ScanRoot::local(pi_agent.join("sessions"))],
        None,
    );

    let pi_conversations = runtime_connector("pi_agent")
        .scan(&ctx)
        .expect("scan direct Pi sessions root");
    let omp_conversations = runtime_connector("omp")
        .scan(&ctx)
        .expect("apply OMP ownership boundary to direct Pi sessions root");

    assert_eq!(pi_conversations.len(), 1);
    assert_eq!(pi_conversations[0].agent_slug, "pi_agent");
    assert!(
        omp_conversations.is_empty(),
        "a basename of `sessions` alone must not make a canonical Pi store OMP"
    );
}

#[test]
fn explicit_xdg_omp_root_is_never_parsed_as_pi_agent() {
    let temp = tempfile::tempdir().expect("tempdir");
    let xdg_app = temp.path().join(".local/share/omp");
    write_omp_session(&xdg_app, "omp-xdg", "XDG OMP session");
    let ctx = ScanContext::with_roots(
        temp.path().join("cass-state"),
        vec![ScanRoot::local(xdg_app)],
        None,
    );

    let omp_conversations = runtime_connector("omp")
        .scan(&ctx)
        .expect("scan explicit OMP XDG root");
    let pi_conversations = runtime_connector("pi_agent")
        .scan(&ctx)
        .expect("apply Pi ownership boundary to OMP XDG root");

    assert_eq!(omp_conversations.len(), 1);
    assert_eq!(omp_conversations[0].agent_slug, "omp");
    assert!(
        pi_conversations.is_empty(),
        "the shared pi-family wire format must not let Pi duplicate an XDG OMP session"
    );
}

#[test]
fn sanitized_remote_omp_roots_keep_provider_identity() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mirror = temp.path().join("cass/remotes/build-host/mirror");
    fs::create_dir_all(&mirror).expect("create production-shaped mirror root");

    let cases = [
        (
            "~/.omp/agent/sessions",
            false,
            "omp-sanitized-default-tilde",
            "Sanitized default tilde mirror",
        ),
        (
            "/home/dev/.omp/agent/sessions",
            false,
            "omp-sanitized-default-absolute",
            "Sanitized default absolute mirror",
        ),
        (
            "~/.local/share/omp",
            true,
            "omp-sanitized-xdg-tilde",
            "Sanitized XDG tilde mirror",
        ),
        (
            "/home/dev/.local/share/omp",
            true,
            "omp-sanitized-xdg-absolute",
            "Sanitized XDG absolute mirror",
        ),
    ];

    for (remote_path, includes_leaf_dir, id, title) in cases {
        let root = mirror.join(path_to_safe_dirname(remote_path));
        let store_root = if includes_leaf_dir {
            root.join("omp")
        } else {
            root.clone()
        };
        write_omp_session(&store_root, id, title);
        let ctx = ScanContext::with_roots(
            temp.path().join("cass-state"),
            vec![ScanRoot::remote(
                root,
                Origin::remote_with_host("build-host", "build-host.example"),
                Some(Platform::Linux),
            )],
            None,
        );
        let omp_conversations = runtime_connector("omp")
            .scan(&ctx)
            .expect("scan sanitized OMP mirror root");
        let pi_conversations = runtime_connector("pi_agent")
            .scan(&ctx)
            .expect("apply Pi boundary to sanitized OMP mirror root");

        assert_eq!(omp_conversations.len(), 1);
        assert_eq!(omp_conversations[0].title.as_deref(), Some(title));
        assert!(pi_conversations.is_empty());
    }

    let absolute_safe_name = path_to_safe_dirname("/home/dev/.omp/agent/sessions");
    let non_mirror_root = temp.path().join("ordinary-cache").join(absolute_safe_name);
    write_omp_session(
        &non_mirror_root,
        "not-a-mirror",
        "Non-mirror sanitized lookalike",
    );
    let non_mirror_ctx = ScanContext::with_roots(
        temp.path().join("cass-state"),
        vec![ScanRoot::local(non_mirror_root)],
        None,
    );
    assert!(
        runtime_connector("omp")
            .scan(&non_mirror_ctx)
            .expect("apply OMP ownership boundary to non-mirror lookalike")
            .is_empty(),
        "an embedded sanitized marker outside remotes/<source>/mirror must not claim OMP ownership"
    );
}

#[test]
fn broad_root_partitions_pi_and_omp_sessions_once_each() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("copied-home");
    write_omp_session(
        &home.join(".pi/agent"),
        "pi-in-broad-root",
        "Broad-root Pi session",
    );
    write_omp_session(
        &home.join(".omp/agent"),
        "omp-in-broad-root",
        "Broad-root OMP session",
    );
    let ctx = ScanContext::with_roots(
        temp.path().join("cass-state"),
        vec![ScanRoot::local(home)],
        None,
    );

    let pi_conversations = runtime_connector("pi_agent")
        .scan(&ctx)
        .expect("scan Pi from broad copied home");
    let omp_conversations = runtime_connector("omp")
        .scan(&ctx)
        .expect("scan OMP from broad copied home");

    assert_eq!(pi_conversations.len(), 1);
    assert_eq!(pi_conversations[0].agent_slug, "pi_agent");
    assert_eq!(omp_conversations.len(), 1);
    assert_eq!(omp_conversations[0].agent_slug, "omp");
    assert_ne!(
        pi_conversations[0].source_path,
        omp_conversations[0].source_path
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
    assert_eq!(
        usage.model_name.as_deref(),
        Some("openrouter/stealth/ox-alpha")
    );
    assert_eq!(usage.provider.as_deref(), Some("openrouter"));
}
