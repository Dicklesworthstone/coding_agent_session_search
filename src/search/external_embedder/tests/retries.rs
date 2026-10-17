//! Real HTTP retries; no sleep-only or error-string-only success controls.
use super::super::retry::retry_after;
use super::*;
use chrono::TimeZone;
use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
use std::sync::atomic::AtomicUsize;

fn busy(status: u16, retry_after: Option<&'static str>) -> Reply {
    Reply {
        status,
        body: "SECRET transcript and bearer token must never appear in diagnostics".into(),
        delay: Duration::ZERO,
        retry_after,
    }
}

#[test]
fn retries_are_opt_in_bounded_and_do_not_change_vector_identity() {
    let server = Server::new(|call, input| {
        if call >= 2 {
            busy(503, None)
        } else {
            Reply::ok(success(input))
        }
    });
    let default = server.config(&[]);
    assert_eq!(default.max_retries, 0);
    let enabled = server.config(&[("CASS_EXTERNAL_EMBEDDING_MAX_RETRIES", "5")]);
    assert_eq!(default.identity(), enabled.identity());
    let error = server.connect().embed_sync("private text").unwrap_err();
    assert!(error.to_string().contains("external_http_503"));
    assert_eq!(server.count(), 3, "default sends no retry");
    for value in ["6", "-1", "true", "", "999999999999999999999999"] {
        let result = ExternalEmbeddingConfig::from_lookup(|key| match key {
            "CASS_EXTERNAL_EMBEDDINGS" => Some("1".into()),
            "CASS_EXTERNAL_EMBEDDING_URL" => Some(server.url.clone()),
            "CASS_EXTERNAL_EMBEDDING_MODEL" => Some("example/model-v1".into()),
            "CASS_EXTERNAL_EMBEDDING_DIMENSION" => Some("3".into()),
            "CASS_EXTERNAL_EMBEDDING_MAX_RETRIES" => Some(value.into()),
            _ => None,
        });
        assert!(result.is_err(), "{value}");
    }
    assert_eq!(server.count(), 3, "configuration never sends HTTP");
}

#[test]
fn transient_statuses_retry_only_the_failed_sub_batch_and_keep_order() {
    for status in [429, 500, 502, 503, 504] {
        let server = Server::new(move |call, input| {
            if matches!(call, 3 | 4) {
                busy(status, None)
            } else {
                Reply::ok(success(input))
            }
        });
        let provider = ExternalEmbedder::connect(
            server.config(&[
                ("CASS_EXTERNAL_EMBEDDING_MAX_RETRIES", "2"),
                ("CASS_EXTERNAL_EMBEDDING_BATCH_SIZE", "3"),
            ]),
            Arc::new(|| false),
        )
        .unwrap();
        let texts = ["a", "b", "c", "d", "e", "f", "g"];
        assert_eq!(
            provider.embed_batch_sync(&texts).unwrap(),
            texts.map(vector)
        );
        let requests = server.seen.lock().unwrap();
        assert_eq!(requests.len(), 7);
        assert_eq!(requests[2]["input"], json!(["a", "b", "c"]));
        assert_eq!(requests[3]["input"], json!(["d", "e", "f"]));
        assert_eq!(requests[3], requests[4], "retry input must be unchanged");
        assert_eq!(requests[4], requests[5]);
        assert_eq!(requests[6]["input"], json!(["g"]));
    }
}

#[test]
fn exhausted_retries_return_no_partial_vectors_or_later_requests() {
    let server = Server::new(|call, input| {
        if call >= 3 {
            busy(503, None)
        } else {
            Reply::ok(success(input))
        }
    });
    let provider = ExternalEmbedder::connect(
        server.config(&[
            ("CASS_EXTERNAL_EMBEDDING_MAX_RETRIES", "2"),
            ("CASS_EXTERNAL_EMBEDDING_BATCH_SIZE", "3"),
        ]),
        Arc::new(|| false),
    )
    .unwrap();
    let error = provider
        .embed_batch_sync(&["a", "b", "c", "d", "e", "f", "g"])
        .expect_err("no prefix vectors may escape an exhausted logical batch")
        .to_string();
    assert!(error.contains("external_http_503"), "{error}");
    assert!(error.contains("after 3 attempts"), "{error}");
    assert!(!error.contains("SECRET"));
    assert!(!error.contains(&server.url));
    let requests = server.seen.lock().unwrap();
    assert_eq!(requests.len(), 6);
    assert!(
        requests
            .iter()
            .all(|request| request["input"] != json!(["g"]))
    );
}

#[test]
fn permanent_and_malformed_responses_are_never_retried() {
    for mode in 0..6 {
        let server = Server::new(move |call, input| {
            if call < 2 {
                return Reply::ok(success(input));
            }
            if mode < 3 {
                return busy([401, 413, 302][mode], Some("0"));
            }
            let mut body = success(input);
            match mode {
                3 => {
                    body["data"].as_array_mut().unwrap().pop();
                }
                4 => {
                    body["data"][0]["embedding"] = json!([1.0]);
                }
                _ => {
                    body["model"] = json!("wrong-model");
                }
            }
            Reply::ok(body)
        });
        let provider = ExternalEmbedder::connect(
            server.config(&[("CASS_EXTERNAL_EMBEDDING_MAX_RETRIES", "5")]),
            Arc::new(|| false),
        )
        .unwrap();
        assert!(provider.embed_sync("do not retry this").is_err(), "{mode}");
        assert_eq!(server.count(), 3, "{mode}");
    }
}

#[test]
fn retry_after_is_honored_or_refused_never_shortened() {
    let previous = Arc::new(Mutex::new(None::<Instant>));
    let elapsed = Arc::new(Mutex::new(None::<Duration>));
    let (started, measured) = (Arc::clone(&previous), Arc::clone(&elapsed));
    let server = Server::new(move |call, input| {
        if call == 2 {
            *started.lock().unwrap() = Some(Instant::now());
            busy(429, Some("1"))
        } else {
            if call == 3 {
                *measured.lock().unwrap() = Some(started.lock().unwrap().unwrap().elapsed());
            }
            Reply::ok(success(input))
        }
    });
    let provider = ExternalEmbedder::connect(
        server.config(&[("CASS_EXTERNAL_EMBEDDING_MAX_RETRIES", "2")]),
        Arc::new(|| false),
    )
    .unwrap();
    assert_eq!(
        provider.embed_sync("delayed retry").unwrap(),
        vector("delayed retry")
    );
    assert!(elapsed.lock().unwrap().unwrap() >= Duration::from_secs(1));
    assert_eq!(server.count(), 4);
    // Each suggested wait is refused immediately rather than slept or clamped.
    for header in ["10", "18446744073709551615", "SECRET bad header"] {
        let server = Server::new(move |call, input| {
            if call >= 2 {
                busy(429, Some(header))
            } else {
                Reply::ok(success(input))
            }
        });
        let mut provider = ExternalEmbedder::connect(
            server.config(&[("CASS_EXTERNAL_EMBEDDING_MAX_RETRIES", "2")]),
            Arc::new(|| false),
        )
        .unwrap();
        provider.config.timeout = Duration::from_secs(2);
        let error = provider.embed_sync("private").unwrap_err().to_string();
        assert!(error.contains("external_http_429"), "{error}");
        assert!(!error.contains("SECRET"));
        assert_eq!(server.count(), 3);
    }
}

#[test]
fn cancellation_during_retry_backoff_sends_no_retry_and_provider_remains_usable() {
    let waiting = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&waiting);
    let polls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&polls);
    let server_waiting = Arc::clone(&waiting);
    let server = Server::new(move |call, input| {
        if call == 2 {
            server_waiting.store(true, Ordering::SeqCst);
            busy(503, Some("10"))
        } else {
            Reply::ok(success(input))
        }
    });
    let provider = ExternalEmbedder::connect(
        server.config(&[("CASS_EXTERNAL_EMBEDDING_MAX_RETRIES", "2")]),
        Arc::new(move || {
            observed.load(Ordering::SeqCst) && counted.fetch_add(1, Ordering::SeqCst) >= 2
        }),
    )
    .unwrap();
    let started = Instant::now();
    let error = provider
        .embed_sync("cancel during wait")
        .unwrap_err()
        .to_string();
    assert!(error.contains("external_cancelled"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(
        polls.load(Ordering::SeqCst) >= 3,
        "wait must poll, not just the response"
    );
    assert_eq!(server.count(), 3);
    waiting.store(false, Ordering::SeqCst);
    assert_eq!(
        provider.embed_sync("after cancellation").unwrap(),
        vector("after cancellation")
    );
    assert_eq!(server.count(), 4);
}

#[test]
fn retry_after_parser_handles_dates_and_refuses_ambiguous_or_overflowing_headers() {
    let now = chrono::Utc.with_ymd_and_hms(2026, 10, 6, 0, 0, 0).unwrap();
    for (text, expected) in [
        ("0", Duration::ZERO),
        ("120", Duration::from_secs(120)),
        ("Tue, 06 Oct 2026 00:00:02 GMT", Duration::from_secs(2)),
        ("Mon, 05 Oct 2026 23:59:59 GMT", Duration::ZERO),
    ] {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static(text));
        assert_eq!(retry_after(&headers, now), Ok(Some(expected)));
    }
    assert_eq!(retry_after(&HeaderMap::new(), now), Ok(None));
    for text in [
        "",
        "-1",
        "+1",
        "1.5",
        "1, 2",
        "18446744073709551616",
        "SECRET",
    ] {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static(text));
        assert!(retry_after(&headers, now).is_err(), "{text}");
    }
    let mut headers = HeaderMap::new();
    headers.append(RETRY_AFTER, HeaderValue::from_static("1"));
    headers.append(RETRY_AFTER, HeaderValue::from_static("2"));
    assert!(retry_after(&headers, now).is_err());
}

#[test]
fn retries_do_not_reset_the_original_http_deadline() {
    let server = Server::new(|call, input| {
        let mut reply = if call == 2 {
            busy(503, None)
        } else {
            Reply::ok(success(input))
        };
        reply.delay = match call {
            2 => Duration::from_millis(200),
            3 => Duration::from_millis(500),
            _ => Duration::ZERO,
        };
        reply
    });
    let mut provider = ExternalEmbedder::connect(
        server.config(&[("CASS_EXTERNAL_EMBEDDING_MAX_RETRIES", "2")]),
        Arc::new(|| false),
    )
    .unwrap();
    provider.config.timeout = Duration::from_millis(600);
    let started = Instant::now();
    // 200 ms + backoff + 500 ms fits two reset deadlines, but not one budget.
    let error = provider
        .embed_sync("one logical deadline")
        .unwrap_err()
        .to_string();
    assert!(error.contains("external_timeout"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_eq!(
        server.count(),
        4,
        "ambiguous timeout must not trigger another attempt"
    );
}

#[test]
fn preflight_retry_does_not_bypass_repeatability_validation() {
    let server = Server::new(|call, input| {
        if call == 0 {
            return busy(503, None);
        }
        let mut body = success(input);
        if call == 2 {
            for row in body["data"].as_array_mut().unwrap() {
                row["embedding"].as_array_mut().unwrap().rotate_left(1);
            }
        }
        Reply::ok(body)
    });
    let error = ExternalEmbedder::connect(
        server.config(&[("CASS_EXTERNAL_EMBEDDING_MAX_RETRIES", "2")]),
        Arc::new(|| false),
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("external_preflight_repeatability"),
        "{error}"
    );
    assert_eq!(server.count(), 3);
}
