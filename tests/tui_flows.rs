use coding_agent_search::ftui_harness;
use coding_agent_search::model::types::{Conversation, Message, MessageRole, Snippet};
use coding_agent_search::search::query::{MatchType, SearchHit};
use coding_agent_search::ui::app::{AgentPane, CassApp, CassMsg, DetailTab, SearchPass};
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
        "flow: {flow}\nkeys: {keys}\nstate: query={:?} detail_open={} detail_tab={:?} find_query={:?} palette_visible={} theme_dark={} status={:?}\n--- frame ---\n{}",
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
