use super::*;
use coding_agent_search::search::tantivy::TantivyIndex;
use frankensearch::quill::cass::CassDocument;
use std::io::Cursor;

fn request(value: Value) -> Request {
    serde_json::from_value(value).unwrap()
}

fn search(session: &mut Session, id: u64, query: &str) -> Value {
    let (reply, stopped) = session.handle(request(json!({"op":"search", "id":id, "query":query})));
    assert!(!stopped);
    assert!(reply.ok, "{reply:?}");
    reply.result.unwrap()
}

fn document(id: i64, text: &str, source: &str) -> CassDocument {
    CassDocument {
        agent: "codex".into(),
        workspace: Some("/workspace/δ".into()),
        workspace_original: None,
        source_path: "/history/same.jsonl".into(),
        msg_idx: 7,
        created_at: Some(1_700_000_000_000),
        title: Some("search fixture".into()),
        content: text.into(),
        source_id: source.into(),
        origin_kind: if source == "local" { "local" } else { "remote" }.into(),
        origin_host: None,
        conversation_id: Some(id),
    }
}

fn index(path: &Path, docs: &[CassDocument]) -> Result<TantivyIndex> {
    let mut writer = TantivyIndex::open_or_create(path)?;
    writer.add_prebuilt_documents_slice(docs)?;
    writer.commit()?;
    Ok(writer)
}

fn tree(path: &Path) -> Result<Vec<(PathBuf, Vec<u8>)>> {
    let mut snapshot = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            snapshot.extend(tree(&entry.path())?);
        } else {
            snapshot.push((entry.path(), std::fs::read(entry.path())?));
        }
    }
    snapshot.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(snapshot)
}

#[test]
fn invalid_requests_and_status_do_not_open_or_create_an_index() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("missing");
    let mut session = Session::new(path.clone());
    assert_eq!(session.status()["open_attempts"], 0);
    for value in [
        json!({"op":"search", "id":1, "query":"x", "limit":0}),
        json!({"op":"search", "id":2, "query":"x", "limit":101}),
        json!({"op":"search", "id":3, "query":" ", "limit":1}),
        json!({"op":"search", "id":4, "query":"x", "offset":usize::MAX}),
        json!({"op":"search", "id":5, "query":"x", "filters":{"created_from":2, "created_to":1}}),
        json!({"op":"search", "id":6, "query":"x", "filters":{"agents":[""]}}),
        json!({"op":"search", "id":7, "query":"x", "filters":{"source_id":" local "}}),
    ] {
        let (reply, stop) = session.handle(request(value));
        assert!(!reply.ok);
        assert!(!stop);
        assert_eq!(reply.error.unwrap().kind, "invalid_request");
    }
    assert_eq!(session.status()["open_attempts"], 0);
    assert!(!path.exists());
    assert!(tree(temp.path())?.is_empty());
    Ok(())
}

#[test]
fn protocol_rejects_unsupported_work_instead_of_silently_downgrading() {
    for value in [
        json!({"op":"search", "id":1, "query":"x", "mode":"semantic"}),
        json!({"op":"search", "id":1, "query":"x", "filters":{"session_paths":["/secret"]}}),
        json!({"op":"reload", "id":1, "index":"/another/archive"}),
        json!({"op":"status", "id":null}),
        json!({"op":"search", "id":1, "query":"x", "limit":-1}),
        json!({"op":"index", "id":1}),
    ] {
        assert!(
            serde_json::from_value::<Request>(value.clone()).is_err(),
            "{value}"
        );
    }
    assert!(
        serde_json::from_str::<Request>(
            r#"{"op":"search","id":1,"query":"x","limit":1,"limit":2}"#
        )
        .is_err()
    );
}

#[test]
fn framed_exchange_is_lazy_recovers_after_bad_json_and_stops_on_shutdown() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mut session = Session::new(temp.path().join("absent"));
    let mut input = Cursor::new(b"not json\n{\"op\":\"status\",\"id\":2}\r\n{\"op\":\"shutdown\",\"id\":3}\n{\"op\":\"search\",\"id\":4,\"query\":\"never\"}\n");
    let mut output = Vec::new();
    serve_io(&mut session, &mut input, &mut output)?;
    let replies: Vec<Value> = std::str::from_utf8(&output)?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    assert_eq!(replies.len(), 3);
    assert_eq!(replies[0]["ok"], false);
    assert!(replies[0]["id"].is_null());
    assert_eq!(replies[1]["result"]["successful_opens"], 0);
    assert_eq!(replies[2]["result"]["shutdown"], true);
    assert_eq!(session.open_attempts, 0);
    Ok(())
}

#[test]
fn frame_and_response_limits_never_publish_partial_successes() -> Result<()> {
    let mut exact = Cursor::new([vec![b' '; protocol::MAX_REQUEST_BYTES], b"\n".to_vec()].concat());
    assert!(
        matches!(protocol::read_frame(&mut exact)?, Frame::Line(line) if line.len() == protocol::MAX_REQUEST_BYTES)
    );
    let oversized = vec![b'x'; protocol::MAX_REQUEST_BYTES + 1];
    let mut session = Session::new(PathBuf::from("never-opened"));
    let mut output = Vec::new();
    serve_io(&mut session, &mut Cursor::new(oversized), &mut output)?;
    let reply: Value = serde_json::from_slice(&output)?;
    assert_eq!(reply["error"]["kind"], "request_too_large");
    assert_eq!(session.open_attempts, 0);

    output.clear();
    // Escaping also counts toward the wire limit, not only the string length.
    let huge = Reply::success(
        42,
        json!({"path":"\u{0001}".repeat(protocol::MAX_RESPONSE_BYTES / 4)}),
    );
    protocol::write_reply(&mut output, &huge)?;
    assert!(output.len() < 1024);
    let reply: Value = serde_json::from_slice(&output)?;
    assert_eq!(reply["id"], 42);
    assert_eq!(reply["ok"], false);
    assert_eq!(reply["error"]["kind"], "response_too_large");
    assert!(reply.get("result").is_none());
    Ok(())
}

#[test]
fn selected_archive_is_explicit_and_stdio_is_opt_in() {
    for argv in [
        vec!["cass", "serve", "--stdio"],
        vec!["cass", "serve", "--index", "x"],
        vec![
            "cass",
            "serve",
            "--stdio",
            "--index",
            "x",
            "--data-dir",
            "y",
        ],
    ] {
        assert!(ServiceCli::try_parse_from(argv).is_err());
    }
    assert!(ServiceCli::try_parse_from(["cass", "serve", "--stdio", "--index", "x"]).is_ok());
    assert!(ServiceCli::try_parse_from(["cass", "serve", "--stdio", "--data-dir", "y"]).is_ok());
}

#[test]
fn repeated_queries_reuse_one_real_reader_and_preserve_canonical_followup_identity() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("index");
    let writer = index(
        &path,
        &[
            document(41, "needle original", "local"),
            document(42, "needle remote", "work-laptop"),
        ],
    )?;
    drop(writer);
    // A corrupt adjacent database must be irrelevant to this index-only path.
    std::fs::write(temp.path().join("agent_search.db"), b"not a database")?;
    let before = tree(temp.path())?;
    let mut session = Session::new(path.clone());
    let first = search(&mut session, 1, "needle");
    let second = search(&mut session, 2, "needle");
    assert_eq!(first["reader_reused"], false);
    assert_eq!(second["reader_reused"], true);
    assert_eq!(second["snapshot"]["successful_opens"], 1);
    assert_eq!(second["snapshot"]["queries_completed"], 2);
    assert_eq!(first["hits"], second["hits"]);
    assert_eq!(second["count"], 2);
    assert!(second["hits"].as_array().unwrap().iter().all(|hit| {
        hit["message_index"] == 8
            && hit["conversation_id"].is_i64()
            && hit.get("content").is_none()
            && hit["source_path"] == "/history/same.jsonl"
    }));
    let (scoped, _) = session.handle(request(json!({
        "op":"search", "id":3, "query":"needle", "filters":{"source_id":"work-laptop"}
    })));
    assert!(scoped.ok, "{scoped:?}");
    let scoped = scoped.result.unwrap();
    assert_eq!(scoped["count"], 1);
    assert_eq!(scoped["hits"][0]["source_id"], "work-laptop");
    assert_eq!(scoped["hits"][0]["conversation_id"], 42);
    assert_eq!(tree(temp.path())?, before);
    Ok(())
}

#[test]
fn publication_is_invisible_until_reload_and_new_snapshot_replaces_old() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("index");
    let mut writer = index(&path, &[document(41, "needle original", "local")])?;
    let mut session = Session::new(path);
    assert_eq!(search(&mut session, 1, "needle")["count"], 1);
    writer.add_prebuilt_documents_slice(&[document(42, "needle appended", "local")])?;
    writer.commit()?;
    assert_eq!(search(&mut session, 2, "needle")["count"], 1);
    assert_eq!(session.successful_opens, 1);
    let (reload, stop) = session.handle(Request::Reload { id: 3 });
    assert!(reload.ok, "{reload:?}");
    assert!(!stop);
    assert_eq!(session.successful_opens, 2);
    let fresh = search(&mut session, 4, "needle");
    assert_eq!(fresh["count"], 2);
    assert_eq!(fresh["snapshot"]["reader_epoch"], 2);
    assert_eq!(fresh["snapshot"]["freshness"], "not_checked");
    Ok(())
}

#[cfg(unix)]
#[test]
fn failed_reload_releases_the_old_reader_and_recovery_never_serves_it_as_current() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("index");
    let writer = index(&path, &[document(41, "needle original", "local")])?;
    drop(writer);
    let mut session = Session::new(path.clone());
    search(&mut session, 1, "needle");
    let moved = temp.path().join("retained-index");
    std::fs::rename(&path, &moved)?;
    let (reply, _) = session.handle(Request::Reload { id: 2 });
    assert!(!reply.ok);
    assert_eq!(session.status()["loaded"], false);
    assert!(session.status()["reader_epoch"].is_null());
    assert!(
        !path.exists(),
        "read-only reload must not recreate an index"
    );
    std::fs::rename(moved, &path)?;
    let (reply, _) = session.handle(Request::Reload { id: 3 });
    assert!(reply.ok, "{reply:?}");
    assert_eq!(
        search(&mut session, 4, "needle")["snapshot"]["reader_epoch"],
        2
    );
    Ok(())
}

#[test]
fn pagination_never_claims_exhaustion_without_proof() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("index");
    let _writer = index(
        &path,
        &(1..=4)
            .map(|id| document(id, "needle searchable context", "local"))
            .collect::<Vec<_>>(),
    )?;
    let mut session = Session::new(path);
    let (reply, _) = session.handle(request(
        json!({"op":"search", "id":1, "query":"needle", "limit":2}),
    ));
    assert!(reply.ok, "{reply:?}");
    let page = reply.result.unwrap();
    assert_eq!(page["count"], 2);
    assert_eq!(page["has_more"], true);
    assert_eq!(page["next_offset"], 2);
    let absent = search(&mut session, 2, "xyznotpresent");
    assert_eq!(absent["count"], 0);
    assert!(absent["has_more"].is_null());
    assert!(absent["next_offset"].is_null());
    Ok(())
}

#[test]
fn previews_are_truncated_on_unicode_boundaries() {
    assert_eq!(prefix("αβ😀δ", 3), "αβ😀");
    assert_eq!(prefix("αβ😀δ", 0), "");
    assert_eq!(prefix("αβ😀δ", 99), "αβ😀δ");
}

#[test]
fn page_window_boundary_does_not_emit_an_unusable_continuation() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("index");
    let docs = (1..=1030)
        .map(|id| document(id, "needle searchable context", "local"))
        .collect::<Vec<_>>();
    let writer = index(&path, &docs)?;
    drop(writer);
    let mut session = Session::new(path);
    let (reply, _) = session.handle(request(json!({
        "op": "search", "id": 1, "query": "needle", "limit": 10, "offset": 1010
    })));
    assert!(reply.ok, "{reply:?}");
    let page = reply.result.unwrap();
    assert_eq!(page["count"], 10);
    assert_eq!(page["has_more"], true);
    assert_eq!(page["page_window_exhausted"], true);
    assert!(page["next_offset"].is_null());
    Ok(())
}

#[test]
fn admission_is_lazy_for_status_invalid_queries_and_unload() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let pool = temp.path().join("pool");
    let mut session = Session::new(temp.path().join("absent"));
    session.admission_pool = Some(admission::Pool::new(pool.clone(), 1)?);
    assert!(session.handle(Request::Status { id: 1 }).0.ok);
    assert!(session.handle(Request::Unload { id: 2 }).0.ok);
    let (invalid, _) = session.handle(request(json!({"op":"search", "id":3, "query":" "})));
    assert_eq!(invalid.error.unwrap().kind, "invalid_request");
    assert_eq!(session.open_attempts, 0);
    assert!(!pool.exists());
    Ok(())
}

#[test]
fn one_pool_preserves_reader_reuse_and_allows_handoff_after_unload() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("index");
    drop(index(&path, &[document(42, "admissionneedle", "local")])?);
    let before = tree(&path)?;
    let pool = admission::Pool::new(temp.path().join("pool"), 1)?;
    let mut first = Session::new(path.clone());
    let mut second = Session::new(path.clone());
    first.admission_pool = Some(pool.clone());
    second.admission_pool = Some(pool.clone());
    assert_eq!(search(&mut first, 1, "admissionneedle")["count"], 1);
    assert_eq!(
        search(&mut first, 2, "admissionneedle")["reader_reused"],
        true
    );
    let (refused, _) = second.handle(request(
        json!({"op":"search", "id":3, "query":"admissionneedle"}),
    ));
    assert_eq!(refused.error.unwrap().kind, "admission_busy");
    assert_eq!(second.open_attempts, 0);
    let (unloaded, stop) = first.handle(Request::Unload { id: 4 });
    assert!(!stop);
    assert_eq!(
        unloaded.result.unwrap()["reader_admission"]["lease_held"],
        false
    );
    assert_eq!(search(&mut second, 5, "admissionneedle")["count"], 1);
    assert!(matches!(pool.acquire(), Err(admission::Refusal::Busy)));
    assert!(second.handle(Request::Shutdown { id: 6 }).1);
    let reopened = search(&mut first, 7, "admissionneedle");
    assert_eq!(reopened["snapshot"]["reader_epoch"], 2);
    assert_eq!(reopened["hits"][0]["conversation_id"], 42);
    first.unload();
    assert_eq!(tree(&path)?, before);
    Ok(())
}

#[test]
fn failed_index_admission_releases_the_reserved_slot() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let pool = admission::Pool::new(temp.path().join("pool"), 1)?;
    let mut session = Session::new(temp.path().join("absent"));
    session.admission_pool = Some(pool.clone());
    let (failed, _) = session.handle(Request::Reload { id: 1 });
    assert!(!failed.ok);
    assert_eq!(failed.error.unwrap().kind, "index_unavailable");
    assert_eq!(session.open_attempts, 1);
    assert!(
        !session.status()["reader_admission"]["lease_held"]
            .as_bool()
            .unwrap()
    );
    drop(pool.acquire()?);
    assert!(!session.index.exists());
    Ok(())
}
