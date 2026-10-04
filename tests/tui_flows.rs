use coding_agent_search::ftui_harness;
use coding_agent_search::model::types::{Conversation, Message, MessageRole, Snippet};
use coding_agent_search::search::query::{MatchType, SearchHit};
use coding_agent_search::ui::app::{
    AgentPane, AppSurface, CassApp, CassMsg, DetailTab, SearchPass, SwarmCockpitSnapshot,
    SwarmCockpitState,
};
use coding_agent_search::ui::data::ConversationView;
use coding_agent_search::ui::ftui_adapter::{Event, KeyCode, KeyEvent, Model, Modifiers};
use coding_agent_search::ui::style_system::UiThemePreset;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

fn tui_flow_guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn pin_dark_theme(app: &mut CassApp) {
    app.theme_preset = UiThemePreset::TokyoNight;
    app.theme_dark = true;
    app.style_options.preset = UiThemePreset::TokyoNight;
    app.style_options.dark_mode = true;
}

fn extract_msgs(cmd: ftui::Cmd<CassMsg>) -> Vec<CassMsg> {
    match cmd {
        ftui::Cmd::Msg(msg) => vec![msg],
        ftui::Cmd::Batch(cmds) | ftui::Cmd::Sequence(cmds) => {
            cmds.into_iter().flat_map(extract_msgs).collect()
        }
        _ => Vec::new(),
    }
}

fn drain_cmd_messages(app: &mut CassApp, cmd: ftui::Cmd<CassMsg>) {
    let mut pending = extract_msgs(cmd);
    while let Some(msg) = pending.pop() {
        let next = app.update(msg);
        pending.extend(extract_msgs(next));
    }
}

fn key(app: &mut CassApp, code: KeyCode, modifiers: Modifiers) {
    let event = Event::Key(KeyEvent {
        code,
        modifiers,
        kind: ftui::KeyEventKind::Press,
    });
    let msg = CassMsg::from(event);
    let cmd = app.update(msg);
    drain_cmd_messages(app, cmd);
}

fn type_text(app: &mut CassApp, text: &str) {
    for ch in text.chars() {
        key(app, KeyCode::Char(ch), Modifiers::NONE);
    }
}

fn complete_search(app: &mut CassApp, hits: Vec<SearchHit>) {
    let cmd = app.update(CassMsg::SearchCompleted {
        generation: app.search_generation,
        pass: SearchPass::Upgrade,
        requested_limit: 10,
        hits,
        elapsed_ms: 7,
        suggestions: Vec::new(),
        wildcard_fallback: false,
        append: false,
    });
    drain_cmd_messages(app, cmd);
}

fn render_app_text(app: &CassApp, width: u16, height: u16) -> String {
    let mut pool = ftui::GraphemePool::new();
    let mut frame = ftui::Frame::new(width, height, &mut pool);
    frame.set_degradation(ftui::render::budget::DegradationLevel::Full);
    app.view(&mut frame);
    ftui_harness::buffer_to_text(&frame.buffer)
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}

fn flow_snapshot(app: &CassApp, flow: &str, keys: &str) -> String {
    let find_query = app
        .detail_find
        .as_ref()
        .map(|find| find.query.as_str())
        .unwrap_or("<none>");
    format!(
        "FLOW: {flow}\n-----\nKEYS: {keys}\nSTATE: query={:?} detail_open={} detail_tab={:?} find_query={:?} palette_visible={} theme_dark={} status={:?}\nFINAL_FRAME:\n{}",
        app.query,
        app.show_detail_modal,
        app.detail_tab,
        find_query,
        app.command_palette.is_visible(),
        app.theme_dark,
        app.status,
        render_app_text(app, 100, 28)
    )
}

fn search_hit(
    title: &str,
    source_path: &str,
    line_number: usize,
    content: &str,
    snippet: &str,
) -> SearchHit {
    SearchHit {
        title: title.to_string(),
        snippet: snippet.to_string(),
        content: content.to_string(),
        content_hash: 10_000 + line_number as u64,
        score: 0.97,
        agent: "claude_code".to_string(),
        source_path: source_path.to_string(),
        workspace: "/workspace/cass".to_string(),
        workspace_original: None,
        created_at: None,
        line_number: Some(line_number),
        match_type: MatchType::Exact,
        source_id: "local".to_string(),
        origin_kind: "local".to_string(),
        origin_host: None,
        conversation_id: Some(42),
    }
}

fn message(idx: i64, role: MessageRole, content: &str, snippets: Vec<Snippet>) -> Message {
    Message {
        id: Some(idx + 1),
        idx,
        role,
        author: None,
        created_at: None,
        content: content.to_string(),
        extra_json: serde_json::json!({}),
        snippets,
    }
}

fn code_snippet(path: &str, text: &str) -> Snippet {
    Snippet {
        id: Some(1),
        file_path: Some(PathBuf::from(path)),
        start_line: Some(10),
        end_line: Some(18),
        language: Some("rust".to_string()),
        snippet_text: Some(text.to_string()),
    }
}

fn conversation_view(title: &str, source_path: &str, messages: Vec<Message>) -> ConversationView {
    ConversationView {
        convo: Conversation {
            id: Some(42),
            agent_slug: "claude_code".to_string(),
            workspace: Some(PathBuf::from("/workspace/cass")),
            external_id: Some(format!("{title}-fixture")),
            title: Some(title.to_string()),
            source_path: PathBuf::from(source_path),
            started_at: None,
            ended_at: None,
            approx_tokens: Some(2048),
            metadata_json: serde_json::json!({}),
            messages: messages.clone(),
            source_id: "local".to_string(),
            origin_host: None,
        },
        messages,
        workspace: None,
    }
}

fn install_single_result(app: &mut CassApp, hit: SearchHit, view: ConversationView) {
    app.cached_detail = Some((hit.source_path.clone(), view));
    complete_search(app, vec![hit.clone()]);
    app.panes = vec![AgentPane {
        agent: hit.agent.clone(),
        total_count: 1,
        hits: vec![hit],
        selected: 0,
    }];
    app.active_pane = 0;
}

fn install_swarm_payload(app: &mut CassApp, payload: serde_json::Value) {
    app.surface = AppSurface::Swarm;
    app.swarm_cockpit =
        SwarmCockpitState::from_snapshot(SwarmCockpitSnapshot::from_status_payload(&payload));
}

#[test]
fn search_to_detail_snippets_tab() {
    let _guard = tui_flow_guard();
    let mut app = CassApp::default();
    pin_dark_theme(&mut app);
    let source_path = "/fixtures/tui_flows/authentication.jsonl";
    let user_text = "Authentication requests fail when the bearer token expires.";
    let snippet_text =
        "fn authenticate(token: &str) -> Result<User> {\n    verify_bearer(token)\n}";
    let hit = search_hit(
        "Authentication failure triage",
        source_path,
        1,
        user_text,
        "Authentication requests fail when the bearer token expires.",
    );
    let view = conversation_view(
        "Authentication failure triage",
        source_path,
        vec![
            message(
                0,
                MessageRole::User,
                user_text,
                vec![code_snippet("src/auth.rs", snippet_text)],
            ),
            message(
                1,
                MessageRole::Agent,
                "Refresh the token before retrying the protected endpoint.",
                Vec::new(),
            ),
        ],
    );

    type_text(&mut app, "authentication");
    install_single_result(&mut app, hit, view);
    key(&mut app, KeyCode::Enter, Modifiers::NONE);
    key(&mut app, KeyCode::Tab, Modifiers::NONE);

    assert_eq!(app.detail_tab, DetailTab::Snippets);
    insta::assert_snapshot!(
        "search_to_detail_snippets_tab",
        flow_snapshot(
            &app,
            "search_to_detail_snippets_tab",
            "authentication <SearchCompleted:1 hit> <Enter> <Tab>"
        )
    );
}

#[test]
fn search_open_find_in_detail() {
    let _guard = tui_flow_guard();
    let mut app = CassApp::default();
    pin_dark_theme(&mut app);
    let source_path = "/fixtures/tui_flows/login.jsonl";
    let user_text = "login fails after redirect with a visible error banner";
    let agent_text =
        "The error is raised after OAuth callback validation. Retry login after clearing state.";
    let hit = search_hit(
        "Login error investigation",
        source_path,
        1,
        user_text,
        "login fails after redirect with a visible error banner",
    );
    let view = conversation_view(
        "Login error investigation",
        source_path,
        vec![
            message(0, MessageRole::User, user_text, Vec::new()),
            message(1, MessageRole::Agent, agent_text, Vec::new()),
            message(
                2,
                MessageRole::Tool,
                "tail app.log -> error: oauth_state_mismatch",
                Vec::new(),
            ),
        ],
    );

    type_text(&mut app, "login");
    install_single_result(&mut app, hit, view);
    key(&mut app, KeyCode::Enter, Modifiers::NONE);
    key(&mut app, KeyCode::Char('/'), Modifiers::NONE);
    type_text(&mut app, "error");
    let _ = render_app_text(&app, 100, 28);
    key(&mut app, KeyCode::Enter, Modifiers::NONE);

    assert_eq!(
        app.detail_find.as_ref().map(|find| find.query.as_str()),
        Some("error")
    );
    insta::assert_snapshot!(
        "search_open_find_in_detail",
        flow_snapshot(
            &app,
            "search_open_find_in_detail",
            "login <SearchCompleted:1 hit> <Enter> / error <Enter>"
        )
    );
}

#[test]
fn keystroke_driven_command_palette() {
    let _guard = tui_flow_guard();
    let mut app = CassApp::default();
    pin_dark_theme(&mut app);

    key(&mut app, KeyCode::Char('p'), Modifiers::CTRL);
    type_text(&mut app, "theme");
    key(&mut app, KeyCode::Enter, Modifiers::NONE);

    assert!(!app.command_palette.is_visible());
    assert!(!app.theme_dark);
    insta::assert_snapshot!(
        "keystroke_driven_command_palette",
        flow_snapshot(
            &app,
            "keystroke_driven_command_palette",
            "<Ctrl-P> theme <Enter>"
        )
    );
}

#[test]
fn swarm_surface_empty_snapshot_is_idle_and_read_only() {
    let _guard = tui_flow_guard();
    let mut app = CassApp::default();
    pin_dark_theme(&mut app);
    app.surface = AppSurface::Swarm;

    let text = render_app_text(&app, 100, 24);

    assert!(text.contains("No swarm snapshot cached"));
    assert!(text.contains("read-only surface"));
    assert!(!text.contains("force-release"));
    assert!(!text.contains("rm -rf"));
    assert!(!text.contains("delete"));
}

#[test]
fn swarm_surface_renders_active_swarm_counts_from_cached_payload() {
    let _guard = tui_flow_guard();
    let mut app = CassApp::default();
    pin_dark_theme(&mut app);
    install_swarm_payload(
        &mut app,
        serde_json::json!({
            "status": "ok",
            "summary": {
                "ready_count": 2,
                "in_progress_count": 1,
                "blocked_count": 0,
                "active_agent_count": 3,
                "active_reservation_count": 1,
                "stale_candidate_count": 0,
                "stale_state_counts": {"active": 1, "recently_quiet": 0, "likely_stale": 0, "conflicting_evidence": 0, "manual_review_required": 0},
                "proof_gap_count": 0,
                "build_pressure": "none",
                "recommended_action": "claim-ready-bead"
            },
            "evidence": {"proof_gaps": [], "redaction_applied": false},
            "privacy": {"redaction_applied": false},
            "providers": []
        }),
    );

    let text = render_app_text(&app, 110, 24);

    assert!(text.contains("ready:2"));
    assert!(text.contains("agents:3"));
    assert!(text.contains("reservations:1"));
    assert!(text.contains("Queue"));
    assert!(text.contains("in-progress 1"));
    assert!(text.contains("Safety"));
}

#[test]
fn swarm_surface_renders_stale_advisory_states() {
    let _guard = tui_flow_guard();
    let mut app = CassApp::default();
    pin_dark_theme(&mut app);
    install_swarm_payload(
        &mut app,
        serde_json::json!({
            "status": "ok",
            "summary": {
                "ready_count": 0,
                "in_progress_count": 4,
                "blocked_count": 0,
                "active_agent_count": 0,
                "active_reservation_count": 0,
                "stale_candidate_count": 1,
                "stale_state_counts": {"active": 0, "recently_quiet": 1, "likely_stale": 1, "conflicting_evidence": 1, "manual_review_required": 1},
                "proof_gap_count": 0,
                "build_pressure": "none",
                "recommended_action": "inspect-stale"
            },
            "evidence": {"proof_gaps": [], "redaction_applied": false},
            "privacy": {"redaction_applied": false},
            "providers": []
        }),
    );

    let text = render_app_text(&app, 120, 24);

    assert!(text.contains("stale:1"));
    assert!(text.contains("quiet 1"));
    assert!(text.contains("likely 1"));
    assert!(text.contains("conflict 1"));
    assert!(text.contains("manual 1"));
}

#[test]
fn swarm_surface_renders_build_pressure_and_proof_gaps() {
    let _guard = tui_flow_guard();
    let mut app = CassApp::default();
    pin_dark_theme(&mut app);
    install_swarm_payload(
        &mut app,
        serde_json::json!({
            "status": "partial",
            "summary": {
                "ready_count": 1,
                "in_progress_count": 0,
                "blocked_count": 0,
                "active_agent_count": 1,
                "active_reservation_count": 0,
                "stale_candidate_count": 0,
                "stale_state_counts": {"active": 0, "recently_quiet": 0, "likely_stale": 0, "conflicting_evidence": 0, "manual_review_required": 0},
                "proof_gap_count": 1,
                "build_pressure": "high",
                "recommended_action": "reduce-build-pressure"
            },
            "evidence": {"proof_gaps": [{"kind": "missing-rch-proof"}], "redaction_applied": true},
            "privacy": {"redaction_applied": true},
            "providers": [{"warning": "agent-mail unavailable"}]
        }),
    );

    let text = render_app_text(&app, 120, 24);

    assert!(text.contains("build:high"));
    assert!(text.contains("gaps:1"));
    assert!(text.contains("missing-rch-proof"));
    assert!(text.contains("agent-mail unavailable"));
    assert!(text.contains("redaction applied"));
}

#[test]
fn swarm_entered_msg_seeds_snapshot_from_live_partial_aggregator() {
    let _guard = tui_flow_guard();
    let mut app = CassApp::default();
    pin_dark_theme(&mut app);
    assert!(
        app.swarm_cockpit.snapshot.is_none(),
        "fresh app must start with no cached swarm snapshot"
    );

    let cmd = app.update(CassMsg::SwarmEntered);
    drain_cmd_messages(&mut app, cmd);

    assert_eq!(app.surface, AppSurface::Swarm);
    let snapshot = app
        .swarm_cockpit
        .snapshot
        .as_ref()
        .expect("SwarmEntered must seed the cockpit via render_swarm_status_live_partial");
    // The placeholder marks every provider unread, so every count is unknown:
    // `?`, never a fabricated zero. Entering also starts exactly one
    // background live read (the task itself is not run here).
    assert_eq!(snapshot.status, "partial");
    assert_eq!(snapshot.ready_count, None);
    assert!(app.swarm_cockpit.loading, "entering starts the live read");
    assert!(!app.swarm_cockpit.live);
    let text = render_app_text(&app, 110, 24);
    assert!(text.contains("ready:?"), "{text}");
    assert!(!text.contains("ready:0"), "{text}");
    assert!(text.contains("reading live swarm providers"), "{text}");
    assert!(text.contains("Safety"));

    // Re-entering must NOT re-seed or start a second read (the surface is
    // read-only and idempotent — repeated taps of Alt+W shouldn't churn state).
    let original = app.swarm_cockpit.clone();
    let cmd = app.update(CassMsg::SwarmEntered);
    drain_cmd_messages(&mut app, cmd);
    assert_eq!(app.swarm_cockpit, original, "re-entering must not re-seed");
}

#[test]
fn swarm_live_read_replaces_the_placeholder_and_r_reads_again() {
    let _guard = tui_flow_guard();
    let mut app = CassApp::default();
    pin_dark_theme(&mut app);
    let cmd = app.update(CassMsg::SwarmEntered);
    drain_cmd_messages(&mut app, cmd);
    assert!(app.swarm_cockpit.loading);

    // The background task's result: Beads read, Agent Mail not (null counts).
    let cmd = app.update(CassMsg::SwarmLiveLoaded(Box::new(serde_json::json!({
        "status": "partial",
        "summary": {
            "ready_count": 3,
            "in_progress_count": 2,
            "blocked_count": 1,
            "active_agent_count": null,
            "active_reservation_count": null,
            "stale_candidate_count": null,
            "stale_state_counts": null,
            "proof_gap_count": null,
            "build_pressure": null,
            "recommended_action": "recheck-live-coordination"
        },
        "evidence": {"proof_gaps": []},
        "providers": [{"warning": "agent-mail unavailable"}],
        "_meta": {"elapsed_ms": 42}
    }))));
    drain_cmd_messages(&mut app, cmd);
    assert!(!app.swarm_cockpit.loading);
    assert!(app.swarm_cockpit.live);
    let text = render_app_text(&app, 140, 24);
    assert!(text.contains("Live Operations Snapshot"), "{text}");
    assert!(text.contains("ready:3"), "{text}");
    assert!(text.contains("in-progress 2"), "{text}");
    assert!(text.contains("blocked 1"), "{text}");
    // Unread providers stay unknown.
    assert!(text.contains("agents:?"), "{text}");
    assert!(text.contains("reservations:?"), "{text}");
    assert!(text.contains("Stale      unknown"), "{text}");
    assert!(!text.contains("agents:0"), "{text}");
    assert!(text.contains("live providers read in 42 ms"), "{text}");

    // `r` starts another read; other typing neither reads nor edits the
    // hidden search query.
    type_text(&mut app, "x");
    assert!(!app.swarm_cockpit.loading);
    assert!(
        app.query.is_empty(),
        "typing on the swarm surface must not search"
    );
    type_text(&mut app, "r");
    assert!(app.swarm_cockpit.loading, "r requests a live read");
    let while_loading = app.swarm_cockpit.clone();
    type_text(&mut app, "r");
    assert_eq!(
        app.swarm_cockpit, while_loading,
        "a second r while a read is in flight starts nothing"
    );

    // The request schedules the read as a background task, never inline;
    // while one is in flight a request schedules nothing. (The task is not run
    // here: its observers re-launch the current executable, which in this
    // test is the test binary.)
    let cmd = app.update(CassMsg::SwarmLiveLoaded(Box::new(serde_json::json!({
        "status": "ok", "summary": {}, "_meta": {}
    }))));
    drain_cmd_messages(&mut app, cmd);
    assert!(matches!(
        app.update(CassMsg::SwarmLiveRefreshRequested),
        ftui::Cmd::Task(..)
    ));
    assert!(matches!(
        app.update(CassMsg::SwarmLiveRefreshRequested),
        ftui::Cmd::None
    ));
}

// ── Swarm cockpit: empty queue + evidence-gap state coverage ──────────────
// Closes coding_agent_session_search-oh96l.6 acceptance criteria for the
// five required states (empty queue, active swarm, stale advisory, build
// pressure, evidence gap). Active/stale/build are pinned above; the two
// tests below add empty-queue and evidence-gap. The safety tests at the
// bottom of this section assert no raw session content leaks and no
// destructive verbs appear when rendering against any checked-in golden.

#[test]
fn swarm_surface_renders_empty_queue_with_no_ready_action() {
    let _guard = tui_flow_guard();
    let mut app = CassApp::default();
    pin_dark_theme(&mut app);
    install_swarm_payload(
        &mut app,
        serde_json::json!({
            "status": "ok",
            "summary": {
                "ready_count": 0,
                "in_progress_count": 0,
                "blocked_count": 0,
                "active_agent_count": 0,
                "active_reservation_count": 0,
                "stale_candidate_count": 0,
                "stale_state_counts": {"active": 0, "recently_quiet": 0, "likely_stale": 0, "conflicting_evidence": 0, "manual_review_required": 0},
                "proof_gap_count": 0,
                "build_pressure": "none",
                "recommended_action": "no-ready-work"
            },
            "evidence": {"proof_gaps": [], "redaction_applied": false},
            "privacy": {"redaction_applied": false},
            "providers": []
        }),
    );

    let text = render_app_text(&app, 110, 24);

    assert!(
        text.contains("ready:0"),
        "empty-queue header must show zero ready beads"
    );
    assert!(text.contains("agents:0"));
    assert!(text.contains("reservations:0"));
    assert!(text.contains("in-progress 0"));
    assert!(text.contains("blocked 0"));
    assert!(
        text.contains("no-ready-work"),
        "footer/header must surface the no-ready-work recommendation"
    );
    assert!(
        text.contains("Safety"),
        "safety line must remain present in every state"
    );
}

#[test]
fn swarm_surface_renders_evidence_gap_summary_state() {
    let _guard = tui_flow_guard();
    let mut app = CassApp::default();
    pin_dark_theme(&mut app);
    install_swarm_payload(
        &mut app,
        serde_json::json!({
            "status": "ok",
            "summary": {
                "ready_count": 2,
                "in_progress_count": 1,
                "blocked_count": 0,
                "active_agent_count": 1,
                "active_reservation_count": 0,
                "stale_candidate_count": 0,
                "stale_state_counts": {"active": 1, "recently_quiet": 0, "likely_stale": 0, "conflicting_evidence": 0, "manual_review_required": 0},
                "proof_gap_count": 3,
                "build_pressure": "none",
                "recommended_action": "close-evidence-gaps"
            },
            "evidence": {
                "proof_gaps": [
                    {"kind": "missing-rch-proof"},
                    {"kind": "stale-baseline"},
                    {"kind": "missing-commit-link"}
                ],
                "redaction_applied": false
            },
            "privacy": {"redaction_applied": false},
            "providers": []
        }),
    );

    let text = render_app_text(&app, 120, 24);

    assert!(
        text.contains("gaps:3"),
        "evidence-gap header must report the gap count"
    );
    assert!(
        text.contains("missing-rch-proof"),
        "first evidence-gap kind must appear"
    );
    assert!(
        text.contains("stale-baseline"),
        "second evidence-gap kind must appear"
    );
    assert!(
        text.contains("close-evidence-gaps"),
        "recommended action must surface the gap remediation"
    );
    assert!(text.contains("Safety"), "safety line must remain present");
}

// Build a swarm payload from a checked-in golden so the safety tests run
// against the exact JSON shape `render_swarm_status_payload` produces. The
// goldens live under `tests/golden/swarm_status/` and are regenerated via
// `UPDATE_GOLDENS=1 ... cargo test --test swarm_status_contract`.
fn load_swarm_golden(scenario: &str) -> serde_json::Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden/swarm_status")
        .join(format!("{scenario}.json.golden"));
    let bytes = std::fs::read(&path)
        .unwrap_or_else(|err| panic!("missing swarm golden {}: {err}", path.display()));
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|err| panic!("invalid swarm golden {}: {err}", path.display()))
}

#[test]
fn swarm_surface_renders_real_fixture_goldens_without_leaking_raw_session_content() {
    let _guard = tui_flow_guard();
    // Markers that would indicate the cockpit had pulled raw session content
    // into its render path. None of these should ever appear in cockpit
    // output: the cockpit must only show counts, kinds, recommendations,
    // and provider warnings — never message bodies, file paths, or
    // workspace paths from the underlying fixture data.
    let session_content_leak_markers: &[&str] = &[
        "/Users/",
        "/home/",
        ".jsonl",
        "session_id",
        "external_id",
        "tool_use",
        "fn authenticate",
        "BEGIN PRIVATE KEY",
        "Authorization: Bearer",
        "secret",
        "password",
    ];
    for scenario in [
        "healthy",
        "busy",
        "stale_advisory",
        "build_pressure",
        "no_ready_work",
        "privacy_guardrails",
        "reservation_conflict",
        "unrelated_reservation",
    ] {
        let mut app = CassApp::default();
        pin_dark_theme(&mut app);
        let payload = load_swarm_golden(scenario);
        install_swarm_payload(&mut app, payload);
        let text = render_app_text(&app, 140, 30);
        for marker in session_content_leak_markers {
            assert!(
                !text.contains(marker),
                "scenario {scenario}: cockpit render leaked raw session marker {marker:?}; \
                 the cockpit must only render counts and gap kinds. \
                 Offending render:\n{text}"
            );
        }
    }
}

#[test]
fn swarm_surface_render_never_emits_destructive_or_recovery_verbs() {
    let _guard = tui_flow_guard();
    // The cockpit is a READ-ONLY surface. Even when a fixture reports a
    // recommended action, the cockpit's rendered text must never present
    // language that suggests it has performed (or could perform from this
    // surface) a destructive or recovery operation. Operators must use
    // dedicated commands (`cass doctor cleanup`, `br close`, `release_*`)
    // for those — never trust a render to imply it.
    let destructive_markers: &[&str] = &[
        "rm -rf",
        "DROP TABLE",
        "DELETE FROM",
        "force_release",
        "force-release",
        " destroy ",
        " purge ",
        " wipe ",
        " deletes ",
        " deleted ",
        " removing ",
        " removed ",
    ];
    for scenario in [
        "healthy",
        "busy",
        "stale_advisory",
        "build_pressure",
        "no_ready_work",
        "privacy_guardrails",
        "reservation_conflict",
        "unrelated_reservation",
    ] {
        let mut app = CassApp::default();
        pin_dark_theme(&mut app);
        let payload = load_swarm_golden(scenario);
        install_swarm_payload(&mut app, payload);
        let text = render_app_text(&app, 140, 30);
        // Match case-insensitively against the lowercased render so that a
        // future marker like "Destroy" doesn't slip through.
        let haystack = text.to_lowercase();
        for marker in destructive_markers {
            assert!(
                !haystack.contains(&marker.to_lowercase()),
                "scenario {scenario}: cockpit render contained destructive/recovery verb \
                 {marker:?}. The cockpit is read-only — it must never imply mutation. \
                 Offending render:\n{text}"
            );
        }
        // Spot-check the static safety line is still there so a future
        // refactor can't quietly drop it.
        assert!(
            text.contains("Safety"),
            "scenario {scenario}: safety reassurance line missing"
        );
    }
}
