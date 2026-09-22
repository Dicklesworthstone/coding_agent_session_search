use super::super::protocol::Request;
use super::*;
use coding_agent_search::search::tantivy::TantivyIndex;
use frankensearch::quill::cass::CassDocument;
use std::path::Path;

fn request(value: Value) -> Request {
    serde_json::from_value(value).expect("fixture request")
}

fn refine_request() -> Value {
    json!({
        "op": "refine", "id": 7,
        "query": "Which changes improved latency?", "lexical_query": "performance"
    })
}

fn hit(index: i64) -> Value {
    json!({
        "source_id": if index == 0 { "local" } else { "work-laptop" },
        "source_path": "/absent/shared.sqlite", "conversation_id": index + 41,
        "message_index": 13, "score": (index as f64) + 0.25,
        "title": format!("title {index}"), "snippet": format!("preview {index}"),
        "workspace": "/workspace/δ"
    })
}

fn document(id: i64, text: &str, source: &str) -> CassDocument {
    CassDocument {
        agent: "codex".into(),
        workspace: Some("/workspace/δ".into()),
        workspace_original: None,
        source_path: "/absent/shared.sqlite".into(),
        msg_idx: 12,
        created_at: Some(1_700_000_000_000),
        title: Some("candidate fixture".into()),
        content: text.into(),
        source_id: source.into(),
        origin_kind: if source == "local" { "local" } else { "remote" }.into(),
        origin_host: None,
        conversation_id: Some(id),
    }
}

fn create_index(path: &Path) -> Result<()> {
    let mut writer = TantivyIndex::open_or_create(path)?;
    writer.add_prebuilt_documents_slice(&[
        document(41, "performance latency improved", "local"),
        document(42, "performance investigations continued", "work-laptop"),
    ])?;
    writer.commit()?;
    Ok(())
}

#[test]
fn refinement_requires_explicit_permission_and_valid_budgets_before_admission() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mut session = Session::new(temp.path().join("never-opened-index"));
    let (disabled, _) = session.handle(request(refine_request()));
    assert_eq!(disabled.error.unwrap().kind, "refinement_disabled");
    session.refiner = Refiner::new(Some(temp.path().join("never-opened-model")));
    for (field, value) in [
        ("query", json!(" ")),
        ("query", json!("δ".repeat(protocol::MAX_QUERY_BYTES))),
        ("lexical_query", json!("")),
        ("candidate_limit", json!(0)),
        ("candidate_limit", json!(MAX_CANDIDATES + 1)),
        ("candidate_limit", json!(usize::MAX)),
        ("limit", json!(0)),
        ("limit", json!(21)),
        ("filters", json!({"source_id": " local "})),
        ("filters", json!({"created_from": 2, "created_to": 1})),
    ] {
        let mut value_request = refine_request();
        value_request[field] = value;
        let (reply, stop) = session.handle(request(value_request));
        assert!(!stop);
        assert_eq!(reply.error.unwrap().kind, "invalid_request");
    }
    assert_eq!(session.open_attempts, 0);
    assert_eq!(session.refiner.load_attempts, 0);
    assert_eq!(session.canonical_read_attempts, 0);
    assert_eq!(std::fs::read_dir(temp.path())?.count(), 0);
    Ok(())
}

#[test]
fn refinement_decoder_refuses_ambiguous_operations_paths_and_pagination() {
    for (field, value) in [
        ("offset", json!(1)),
        ("db", json!("/other/archive")),
        ("model", json!("/other/model")),
        ("reranker_model", json!("/other/model")),
        ("candidates", json!([hit(0)])),
        ("mode", json!("semantic")),
        ("candidate_limit", json!(-1)),
        ("limit", json!(1.5)),
    ] {
        let mut value_request = refine_request();
        value_request[field] = value;
        assert!(serde_json::from_value::<Request>(value_request).is_err());
    }
    let Request::Refine {
        candidate_limit,
        limit,
        ..
    } = request(refine_request())
    else {
        panic!("wrong operation");
    };
    assert_eq!(candidate_limit, 20);
    assert_eq!(limit, 5);
    assert!(
        serde_json::from_str::<Request>(
            r#"{"op":"refine","id":1,"query":"a","query":"b","lexical_query":"x"}"#
        )
        .is_err()
    );
}

#[test]
fn ranking_preserves_identity_and_lexical_scores_with_deterministic_ties() -> Result<()> {
    let original = (0..4).map(hit).collect::<Vec<_>>();
    let ranked = rank(original.clone(), vec![-1.0, 0.75, 0.75, 0.0], 3)?;
    assert_eq!(
        ranked
            .iter()
            .map(|hit| hit["lexical_rank"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        [2, 3, 4]
    );
    for (result, original_index) in ranked.iter().zip([1, 2, 3]) {
        for (key, value) in original[original_index].as_object().unwrap() {
            assert_eq!(&result[key], value, "changed existing field {key}");
        }
    }
    let zeros = rank(original[..2].to_vec(), vec![-0.0, 0.0], 2)?;
    assert_eq!(zeros[0]["lexical_rank"], 1);
    assert_eq!(zeros[1]["lexical_rank"], 2);
    Ok(())
}

#[test]
fn ranking_rejects_malformed_score_sets_instead_of_synthesizing_evidence() {
    for scores in [vec![], vec![1.0, 2.0], vec![f32::NAN], vec![f32::INFINITY]] {
        assert!(rank(vec![hit(0)], scores, 1).is_err());
    }
    assert!(rank(vec![Value::Null], vec![1.0], 1).is_err());
    assert!(rank(vec![hit(0)], vec![1.0], 0).is_err());
    assert!(rank(vec![hit(0)], vec![1.0], usize::MAX).is_err());
    assert!(rank(vec![hit(0); 33], vec![1.0; 33], 1).is_err());
}

#[test]
fn model_input_is_only_bounded_preview_text_and_preserves_utf8_and_nul() -> Result<()> {
    let mut candidate = hit(0);
    candidate["title"] = json!("δ-title");
    candidate["snippet"] = json!("visible\0tail😀");
    candidate["content"] = json!("must never be scored as canonical content");
    assert_eq!(preview_text(&candidate)?, "δ-title\nvisible\0tail😀");
    candidate["snippet"] = json!("😀".repeat(MAX_CANDIDATE_BYTES / 4));
    assert!(preview_text(&candidate).is_err());
    candidate["snippet"] = Value::Null;
    assert!(preview_text(&candidate).is_err());
    candidate["title"] = json!("");
    candidate["snippet"] = json!(" ");
    assert!(preview_text(&candidate).is_err());
    Ok(())
}

#[test]
fn inference_boundary_refuses_oversized_inputs_without_loading_model() {
    let mut refiner = Refiner::new(Some(PathBuf::from("never-opened-model")));
    for documents in [
        vec![],
        vec!["x".into(); MAX_CANDIDATES + 1],
        vec!["x".repeat(MAX_CANDIDATE_BYTES + 1)],
        vec!["x".repeat(MAX_CANDIDATE_BYTES); MAX_CANDIDATES],
        vec![" ".into()],
    ] {
        assert!(refiner.score("question", &documents).is_err());
    }
    assert_eq!(refiner.load_attempts, 0);
    assert!(!refiner.loaded());
}

#[test]
fn ordinary_search_and_empty_refinement_reuse_reader_without_model_or_database() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let index = temp.path().join("index");
    create_index(&index)?;
    let db = temp.path().join("archive.db");
    std::fs::write(&db, b"not an archive; must never be opened")?;
    let mut session = Session::new(index);
    session.archive = Some(db.clone());
    session.refiner = Refiner::new(Some(temp.path().join("absent-model")));
    for id in 1..=2 {
        let (reply, _) = session.handle(request(json!({
            "op": "search", "id": id, "query": "performance"
        })));
        assert!(reply.ok, "{reply:?}");
        assert_eq!(reply.result.unwrap()["count"], 2);
    }
    let mut empty = refine_request();
    empty["filters"] = json!({"source_id": "not-present"});
    let (reply, _) = session.handle(request(empty));
    assert!(reply.ok, "{reply:?}");
    let result = reply.result.unwrap();
    assert_eq!(result["count"], 0);
    assert_eq!(result["refinement"]["status"], "no_candidates");
    assert_eq!(result["reader_reused"], true);
    assert!(result["next_offset"].is_null());
    assert!(result["more_lexical_candidates"].is_null());
    assert_eq!(session.successful_opens, 1);
    assert_eq!(session.refiner.load_attempts, 0);
    assert_eq!(session.canonical_read_attempts, 0);
    assert_eq!(std::fs::read(db)?, b"not an archive; must never be opened");
    assert!(!temp.path().join("absent-model").exists());
    Ok(())
}

#[test]
fn missing_model_fails_explicitly_without_breaking_the_retained_lexical_reader() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let index = temp.path().join("index");
    create_index(&index)?;
    let model_dir = temp.path().join("model");
    std::fs::create_dir(&model_dir)?;
    let mut session = Session::new(index);
    session.refiner = Refiner::new(Some(model_dir.clone()));
    for attempt in 1..=2 {
        let (reply, _) = session.handle(request(refine_request()));
        assert!(!reply.ok);
        assert!(
            reply.result.is_none(),
            "do not turn lexical scores into reranking scores"
        );
        assert_eq!(reply.error.unwrap().kind, "refinement_failed");
        assert_eq!(session.refiner.load_attempts, attempt);
        assert!(!session.refiner.loaded());
    }
    let (reply, _) = session.handle(request(json!({
        "op": "search", "id": 8, "query": "performance"
    })));
    assert!(reply.ok);
    assert_eq!(reply.result.unwrap()["count"], 2);
    assert_eq!(session.successful_opens, 1);
    assert_eq!(
        std::fs::read_dir(model_dir)?.count(),
        0,
        "no download or model repair"
    );
    Ok(())
}

#[test]
#[ignore = "requires an explicitly installed native safetensors reranker via CASS_TEST_RERANKER_MODEL"]
fn native_refinement_reuses_real_model_and_reader_without_global_semantic_assets() -> Result<()> {
    let model = PathBuf::from(dotenvy::var("CASS_TEST_RERANKER_MODEL")?);
    let temp = tempfile::tempdir()?;
    let index = temp.path().join("index");
    let mut writer = TantivyIndex::open_or_create(&index)?;
    writer.add_prebuilt_documents_slice(&[
        document(41, "planet Mars is known as the red planet", "local"),
        document(
            42,
            "planet themed cupcakes are served at the bakery",
            "work-laptop",
        ),
    ])?;
    writer.commit()?;
    drop(writer);
    let mut session = Session::new(index);
    session.refiner = Refiner::new(Some(model));
    for reused in [false, true] {
        let (reply, _) = session.handle(request(json!({
            "op": "refine", "id": 1, "query": "Which planet is known as the red planet?",
            "lexical_query": "planet", "candidate_limit": 2, "limit": 2
        })));
        assert!(reply.ok, "{reply:?}");
        let result = reply.result.unwrap();
        assert_eq!(result["refinement"]["model_reused"], reused);
        assert_eq!(result["reader_reused"], reused);
        assert_eq!(result["count"], 2);
        assert_eq!(result["hits"][0]["conversation_id"], 41);
        assert_eq!(result["hits"][0]["message_index"], 13);
        assert_eq!(result["refinement"]["model"]["vector_assets_loaded"], false);
    }
    assert_eq!(session.refiner.successful_loads, 1);
    assert_eq!(session.refiner.calls_completed, 2);
    assert_eq!(session.canonical_read_attempts, 0);
    // Unload drops both native owners and leaves the model capability configured.
    let (reply, stop) = session.handle(Request::Unload { id: 2 });
    assert!(reply.ok && !stop);
    assert!(!session.refiner.loaded());
    assert!(session.client.is_none());
    assert!(session.refiner.enabled());
    // Reload does not eagerly reopen model files.
    let (reply, _) = session.handle(Request::Reload { id: 2 });
    assert!(reply.ok, "{reply:?}");
    assert!(!session.refiner.loaded());
    assert_eq!(session.refiner.load_attempts, 1);
    Ok(())
}

#[test]
fn refinement_obeys_shared_admission_before_loading_and_unload_releases_the_slot() -> Result<()> {
    use super::super::admission::{Pool, Refusal};

    let temp = tempfile::tempdir()?;
    let index = temp.path().join("index");
    create_index(&index)?;
    let pool = Pool::new(temp.path().join("pool"), 1)?;
    let occupied = pool.acquire()?;
    let mut session = Session::new(index);
    session.admission_pool = Some(pool.clone());
    session.refiner = Refiner::new(Some(temp.path().join("absent-model")));

    let (denied, stop) = session.handle(request(refine_request()));
    assert!(!stop);
    assert_eq!(denied.error.unwrap().kind, "admission_busy");
    assert_eq!(session.open_attempts, 0);
    assert_eq!(session.refiner.load_attempts, 0);
    assert_eq!(session.canonical_read_attempts, 0);
    drop(occupied);

    // A failed model load still leaves a useful lexical reader under its lease.
    let (failed_model, _) = session.handle(request(refine_request()));
    assert_eq!(failed_model.error.unwrap().kind, "refinement_failed");
    assert_eq!(session.successful_opens, 1);
    assert_eq!(session.refiner.load_attempts, 1);
    assert!(matches!(pool.acquire(), Err(Refusal::Busy)));
    let (unloaded, stop) = session.handle(Request::Unload { id: 9 });
    assert!(unloaded.ok && !stop);
    assert!(session.client.is_none());
    assert!(!session.refiner.loaded());
    assert!(session.reader_lease.is_none());
    let released = pool.acquire()?;
    drop(released);
    assert_eq!(session.refiner.load_attempts, 1);
    Ok(())
}
