use super::*;
use std::io::Cursor;
use std::path::PathBuf;

fn rpc(value: Value) -> RpcRequest {
    serde_json::from_value(value).unwrap()
}

fn initialize(adapter: &mut Adapter, session: &mut Session, version: &str) -> Value {
    let result = adapter.handle(session, rpc(json!({
        "jsonrpc": "2.0", "id": "initialize", "method": "initialize",
        "params": {"protocolVersion": version, "capabilities": {},
                   "clientInfo": {"name": "fixture", "version": "1"}}
    }))).unwrap();
    assert!(adapter.handle(session, rpc(json!({
        "jsonrpc": "2.0", "method": "notifications/initialized"
    }))).is_none());
    result
}

fn call(id: Value, name: &str, arguments: Value) -> RpcRequest {
    rpc(json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
        "params": {"name": name, "arguments": arguments}}))
}

#[test]
fn mcp_negotiates_only_supported_handshake_versions_without_opening_index() {
    for (requested, expected) in [
        ("2025-06-18", "2025-06-18"),
        ("2025-11-25", "2025-11-25"),
        ("2026-07-28", "2025-11-25"),
        ("future-version", "2025-11-25"),
    ] {
        let mut adapter = Adapter::default();
        let mut session = Session::new(PathBuf::from("never-opened"));
        // Modern SDKs may probe before negotiating a legacy handshake.
        let probe = adapter.handle(&mut session, rpc(json!({
            "jsonrpc": "2.0", "id": 1, "method": "server/discover",
            "params": {"_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {}}}
        }))).unwrap();
        assert_eq!(probe["error"]["code"], -32601);
        let result = initialize(&mut adapter, &mut session, requested);
        assert_eq!(result["result"]["protocolVersion"], expected);
        assert_eq!(result["result"]["capabilities"], json!({"tools": {"listChanged": false}}));
        assert_eq!(session.open_attempts, 0);
        let repeated = initialize(&mut adapter, &mut session, requested);
        assert_eq!(repeated["error"]["code"], -32600);
    }
}

#[test]
fn mcp_requires_readiness_and_never_executes_or_answers_tool_notifications() {
    let mut adapter = Adapter::default();
    let mut session = Session::new(PathBuf::from("never-opened"));
    let early = adapter.handle(&mut session, call(1.into(), "cass_reload", json!({}))).unwrap();
    assert_eq!(early["error"]["code"], -32600);
    let ping = adapter.handle(&mut session, rpc(json!({
        "jsonrpc": "2.0", "id": -3, "method": "ping"
    }))).unwrap();
    assert_eq!(ping["id"], -3);
    assert_eq!(ping["result"], json!({}));
    initialize(&mut adapter, &mut session, CURRENT_VERSION);
    for method in ["tools/call", "notifications/cancelled", "unknown_notification"] {
        assert!(adapter.handle(&mut session, rpc(json!({
            "jsonrpc": "2.0", "method": method,
            "params": {"name": "cass_reload", "arguments": {}}
        }))).is_none());
    }
    assert_eq!(session.open_attempts, 0);
    assert_eq!(adapter.calls, 0);
}

#[test]
fn mcp_correlates_exact_scalar_ids_and_separates_protocol_and_tool_errors() {
    let mut adapter = Adapter::default();
    let mut session = Session::new(PathBuf::from("never-opened"));
    initialize(&mut adapter, &mut session, CURRENT_VERSION);
    for id in [json!("δ-😀"), json!(-12), json!(u64::MAX)] {
        let result = adapter.handle(&mut session, call(id.clone(), "cass_status", json!({}))).unwrap();
        assert_eq!(result["id"], id);
        assert_eq!(result["result"]["isError"], false);
        let text: Value = serde_json::from_str(result["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(text, result["result"]["structuredContent"]);
    }
    for id in [Value::Null, json!(true), json!(1.5), json!({}), json!([])] {
        assert!(serde_json::from_value::<RpcRequest>(json!({
            "jsonrpc": "2.0", "id": id, "method": "ping"
        })).is_err());
    }
    let unknown = adapter.handle(&mut session, call(4.into(), "cass_index", json!({}))).unwrap();
    assert_eq!(unknown["error"]["code"], -32602);
    let invalid = adapter.handle(&mut session, call(5.into(), "cass_search", json!({"query": "x", "limit": 0}))).unwrap();
    assert_eq!(invalid["id"], 5);
    assert_eq!(invalid["result"]["isError"], true);
    assert_eq!(invalid["result"]["structuredContent"]["error"]["kind"], "invalid_request");
    assert_eq!(session.open_attempts, 0);
}

#[test]
fn mcp_tool_catalog_is_stable_bounded_and_has_no_mutating_archive_tools() {
    let catalog = tools();
    assert_eq!(catalog, tools());
    assert_eq!(catalog.iter().map(|tool| tool["name"].as_str().unwrap()).collect::<Vec<_>>(),
        ["cass_search", "cass_status", "cass_reload"]);
    for tool in &catalog {
        assert_eq!(tool["annotations"]["readOnlyHint"], true);
        assert_eq!(tool["annotations"]["openWorldHint"], false);
        assert_eq!(tool["inputSchema"]["additionalProperties"], false);
    }
    let properties = &catalog[0]["inputSchema"]["properties"];
    assert_eq!(properties["limit"]["maximum"], protocol::MAX_LIMIT);
    assert_eq!(properties["query"]["maxLength"], protocol::MAX_QUERY_BYTES);
    assert_eq!(properties["filters"]["properties"]["agents"]["maxItems"], protocol::MAX_FILTERS);
    let mut adapter = Adapter::default();
    let mut session = Session::new(PathBuf::from("never-opened"));
    initialize(&mut adapter, &mut session, CURRENT_VERSION);
    for args in [json!({"id": 4}), json!({"op": "reload"}), json!({"index": "/other"})] {
        let result = adapter.handle(&mut session, call(2.into(), "cass_search", args)).unwrap();
        assert_eq!(result["error"]["code"], -32602);
    }
    assert_eq!(session.open_attempts, 0);
}

#[test]
fn mcp_rate_limit_is_nonblocking_and_releases_no_additional_index_work() {
    let mut adapter = Adapter::default();
    let mut session = Session::new(PathBuf::from("never-opened"));
    initialize(&mut adapter, &mut session, CURRENT_VERSION);
    for id in 0..MAX_TOOL_CALLS_PER_MINUTE {
        let result = adapter.handle(&mut session, call(id.into(), "cass_status", json!({}))).unwrap();
        assert_eq!(result["result"]["isError"], false);
    }
    let limited = adapter.handle(&mut session, call(121.into(), "cass_reload", json!({}))).unwrap();
    assert_eq!(limited["result"]["structuredContent"]["error"]["kind"], "rate_limited");
    assert_eq!(session.open_attempts, 0);
    adapter.window_started = Instant::now() - Duration::from_secs(61);
    let allowed = adapter.handle(&mut session, call(122.into(), "cass_status", json!({}))).unwrap();
    assert_eq!(allowed["result"]["isError"], false);
    assert_eq!(adapter.calls, 1);
}

#[test]
fn mcp_framing_preserves_ids_and_recovers_from_malformed_messages() -> io::Result<()> {
    let mut session = Session::new(PathBuf::from("never-opened"));
    let mut output = Vec::new();
    serve_io(&mut session, &mut Cursor::new(
        b"{\n{\"jsonrpc\":\"2.0\",\"id\":null,\"method\":\"ping\"}\n{\"jsonrpc\":\"2.0\",\"id\":\"valid\",\"method\":\"ping\"}\n{\"jsonrpc\":\"2.0\",\"method\":\"ignored\"}\n"
    ), &mut output)?;
    let replies: Vec<Value> = std::str::from_utf8(&output).unwrap().lines()
        .map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(replies.len(), 3);
    assert_eq!(replies[0]["error"]["code"], -32700);
    assert_eq!(replies[1]["error"]["code"], -32600);
    assert_eq!(replies[2]["id"], "valid");
    assert!(serde_json::from_str::<RpcRequest>(r#"{"jsonrpc":"2.0","id":1,"id":2,"method":"ping"}"#).is_err());
    assert_eq!(session.open_attempts, 0);
    Ok(())
}

#[test]
fn mcp_wire_budget_never_emits_partial_data_or_a_cass_protocol_envelope() -> io::Result<()> {
    let mut output = Vec::new();
    let large = success(json!("correlate"), json!({"text": "\u{0001}".repeat(protocol::MAX_RESPONSE_BYTES)}));
    write_response(&mut output, &large)?;
    let response: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(response["id"], "correlate");
    assert_eq!(response["jsonrpc"], "2.0");
    assert_eq!(response["error"]["code"], -32603);
    assert!(response.get("result").is_none());
    output.clear();
    let mut session = Session::new(PathBuf::from("never-opened"));
    serve_io(&mut session, &mut Cursor::new(vec![b' '; protocol::MAX_REQUEST_BYTES + 1]), &mut output)?;
    let response: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(response["error"]["code"], -32600);
    assert_eq!(response["jsonrpc"], "2.0");
    assert_eq!(session.open_attempts, 0);
    Ok(())
}

#[test]
fn mcp_calls_use_the_same_real_reader_and_explicit_reload_boundary() -> anyhow::Result<()> {
    use coding_agent_search::search::tantivy::TantivyIndex;
    use frankensearch::quill::cass::CassDocument;

    let temp = tempfile::tempdir()?;
    let path = temp.path().join("index");
    let document = |id| CassDocument {
        agent: "codex".into(), workspace: Some("/work".into()), workspace_original: None,
        source_path: "/history/session.jsonl".into(), msg_idx: 12,
        created_at: Some(1_700_000_000_000), title: Some("fixture".into()),
        content: "mcpretainedneedle".into(), source_id: "local".into(),
        origin_kind: "local".into(), origin_host: None, conversation_id: Some(id),
    };
    let mut writer = TantivyIndex::open_or_create(&path)?;
    writer.add_prebuilt_documents_slice(&[document(42)])?;
    writer.commit()?;
    std::fs::write(temp.path().join("agent_search.db"), b"never a database")?;
    let mut session = Session::new(path);
    let mut adapter = Adapter::default();
    initialize(&mut adapter, &mut session, CURRENT_VERSION);
    let first = adapter.handle(&mut session, call("first".into(), "cass_search", json!({"query": "mcpretainedneedle"}))).unwrap();
    assert_eq!(first["result"]["isError"], false, "{first}");
    assert_eq!(first["result"]["structuredContent"]["count"], 1);
    writer.add_prebuilt_documents_slice(&[document(43)])?;
    writer.commit()?;
    let retained = adapter.handle(&mut session, call("second".into(), "cass_search", json!({"query": "mcpretainedneedle"}))).unwrap();
    assert_eq!(retained["result"]["structuredContent"]["count"], 1);
    assert_eq!(retained["result"]["structuredContent"]["reader_reused"], true);
    assert_eq!(retained["result"]["structuredContent"]["hits"][0]["conversation_id"], 42);
    assert_eq!(retained["result"]["structuredContent"]["hits"][0]["message_index"], 13);
    let reload = adapter.handle(&mut session, call(3.into(), "cass_reload", json!({}))).unwrap();
    assert_eq!(reload["result"]["isError"], false, "{reload}");
    let fresh = adapter.handle(&mut session, call(4.into(), "cass_search", json!({"query": "mcpretainedneedle"}))).unwrap();
    assert_eq!(fresh["result"]["structuredContent"]["count"], 2);
    assert_eq!(session.successful_opens, 2);
    assert_eq!(std::fs::read(temp.path().join("agent_search.db"))?, b"never a database");
    Ok(())
}
