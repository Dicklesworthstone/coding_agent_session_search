use super::*;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

struct Reply {
    status: u16,
    body: String,
    delay: Duration,
}

impl Reply {
    fn ok(value: Value) -> Self {
        Self { status: 200, body: value.to_string(), delay: Duration::ZERO }
    }
}

struct Server {
    url: String,
    seen: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Server {
    fn new(reply: impl Fn(usize, &Value) -> Reply + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1/embeddings", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (thread_seen, thread_stop) = (Arc::clone(&seen), Arc::clone(&stop));
        let handle = thread::spawn(move || {
            while !thread_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => serve(stream, &thread_seen, &reply),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("fake server accept: {error}"),
                }
            }
        });
        Self { url, seen, stop, handle: Some(handle) }
    }

    fn config(&self, extras: &[(&str, &str)]) -> ExternalEmbeddingConfig {
        let mut values: HashMap<String, String> = [
            ("CASS_EXTERNAL_EMBEDDINGS", "1"),
            ("CASS_EXTERNAL_EMBEDDING_URL", self.url.as_str()),
            ("CASS_EXTERNAL_EMBEDDING_MODEL", "example/model-v1"),
            ("CASS_EXTERNAL_EMBEDDING_DIMENSION", "3"),
        ].into_iter().map(|(k, v)| (k.into(), v.into())).collect();
        values.extend(extras.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        ExternalEmbeddingConfig::from_lookup(|key| values.get(key).cloned()).unwrap().unwrap()
    }

    fn connect(&self) -> ExternalEmbedder {
        ExternalEmbedder::connect(self.config(&[]), Arc::new(|| false)).unwrap()
    }

    fn count(&self) -> usize { self.seen.lock().unwrap().len() }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.handle.take().unwrap().join().unwrap();
    }
}

fn serve(mut stream: TcpStream, seen: &Mutex<Vec<Value>>, reply: &impl Fn(usize, &Value) -> Reply) {
    stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    stream.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut first = String::new();
    reader.read_line(&mut first).unwrap();
    assert!(first.starts_with("POST /v1/embeddings HTTP/1.1"), "{first}");
    let mut length = None;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" { break; }
        if let Some((key, value)) = line.split_once(':') {
            if key.eq_ignore_ascii_case("content-length") { length = Some(value.trim().parse::<usize>().unwrap()); }
        }
    }
    let length = length.expect("content length");
    assert!(length <= MAX_REQUEST_BYTES);
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).unwrap();
    let input: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(input["encoding_format"], "float");
    assert_eq!(input["model"], "example/model-v1");
    let index = {
        let mut requests = seen.lock().unwrap();
        let index = requests.len();
        requests.push(input.clone());
        index
    };
    let response = reply(index, &input);
    thread::sleep(response.delay);
    // Timeout/cancellation tests may have closed the peer already.
    let _ = write!(stream, "HTTP/1.1 {} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", response.status, response.body.len(), response.body);
}

fn vector(text: &str) -> Vec<f32> {
    let mut result = vec![0.0; 3];
    let index = text.bytes().fold(0usize, |sum, byte| sum.wrapping_add(byte as usize)) % 3;
    result[index] = 1.0;
    result
}

fn success(input: &Value) -> Value {
    // Reverse response order: the client must bind vectors to input indices,
    // not assume that the server returns data in request order.
    let data: Vec<Value> = input["input"].as_array().unwrap().iter().enumerate().rev()
        .map(|(index, text)| json!({"index": index, "embedding": vector(text.as_str().unwrap())}))
        .collect();
    json!({"model": "example/model-v1", "data": data})
}

#[test]
fn external_success_reorders_and_bounds_batches() {
    let server = Server::new(|_, input| Reply::ok(success(input)));
    let embedder = ExternalEmbedder::connect(server.config(&[("CASS_EXTERNAL_EMBEDDING_BATCH_SIZE", "3")]), Arc::new(|| false)).unwrap();
    assert_eq!(server.count(), 2, "preflight only");
    let texts = ["alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta"];
    let values = embedder.embed_batch_sync(&texts).unwrap();
    assert_eq!(values, texts.map(vector));
    assert_eq!(server.count(), 5);
    for request in server.seen.lock().unwrap().iter() {
        assert!(request["input"].as_array().unwrap().len() <= 3);
    }
    assert!(is_external_identity(embedder.id()));
    assert_ne!(embedder.id(), "minilm-384");
    // A remote response must never inherit local MiniLM conformance authority.
    assert!(embedder.identity().is_err());
}

#[test]
fn external_disabled_does_not_resolve_configuration_or_contact_server() {
    let server = Server::new(|_, input| Reply::ok(success(input)));
    for consent in [None, Some("0"), Some("false"), Some("")] {
        let mut looked_up = Vec::new();
        let config = ExternalEmbeddingConfig::from_lookup(|key| {
            looked_up.push(key.to_owned());
            match key {
                "CASS_EXTERNAL_EMBEDDINGS" => consent.map(str::to_owned),
                "CASS_EXTERNAL_EMBEDDING_URL" => Some(server.url.clone()),
                _ => panic!("disabled path attempted to read {key}"),
            }
        }).unwrap();
        assert!(config.is_none());
        assert_eq!(looked_up, ["CASS_EXTERNAL_EMBEDDINGS"]);
    }
    thread::sleep(Duration::from_millis(30));
    assert_eq!(server.count(), 0);
}

#[test]
fn external_identity_separates_model_dimension_endpoint_and_revision() {
    let server = Server::new(|_, input| Reply::ok(success(input)));
    let base = server.config(&[]);
    for (key, value) in [
        ("CASS_EXTERNAL_EMBEDDING_MODEL", "minilm-384"),
        ("CASS_EXTERNAL_EMBEDDING_DIMENSION", "384"),
        ("CASS_EXTERNAL_EMBEDDING_REVISION", "2"),
        ("CASS_EXTERNAL_EMBEDDING_URL", "http://127.0.0.1:7/v1/embeddings"),
    ] {
        let other = server.config(&[(key, value)]);
        assert_ne!(base.identity(), other.identity());
        assert!(is_external_identity(&other.identity()));
    }
    assert_eq!(base.identity(), server.config(&[("CASS_EXTERNAL_EMBEDDING_API_KEY", "rotated-secret")]).identity());
    assert!(!format!("{base:?}").contains("example/model-v1"));
    assert!(!is_external_identity("minilm-384"));
    assert!(!is_external_identity("external-v1-3-../../minilm-384"));
}

#[test]
fn external_preflight_rejects_malformed_dimensions() {
    let server = Server::new(|_, input| {
        let mut response = success(input);
        response["data"][0]["embedding"] = json!([1.0, 0.0]);
        Reply::ok(response)
    });
    let error = ExternalEmbedder::connect(server.config(&[]), Arc::new(|| false)).unwrap_err().to_string();
    assert!(error.contains("external_dimension_mismatch"), "{error}");
    assert_eq!(server.count(), 1);
}

#[test]
fn external_preflight_rejects_nonunit_and_zero_vectors() {
    for bad in [json!([2.0, 0.0, 0.0]), json!([0.0, 0.0, 0.0])] {
        let server = Server::new(move |_, input| {
            let mut response = success(input);
            response["data"][0]["embedding"] = bad.clone();
            Reply::ok(response)
        });
        let error = ExternalEmbedder::connect(server.config(&[]), Arc::new(|| false)).unwrap_err().to_string();
        assert!(error.contains("external_normalization"), "{error}");
    }
}

#[test]
fn external_preflight_rejects_nonrepeatable_provider() {
    let server = Server::new(|call, input| {
        let mut response = success(input);
        if call == 1 {
            for row in response["data"].as_array_mut().unwrap() {
                row["embedding"].as_array_mut().unwrap().rotate_left(1);
            }
        }
        Reply::ok(response)
    });
    let error = ExternalEmbedder::connect(server.config(&[]), Arc::new(|| false)).unwrap_err().to_string();
    assert!(error.contains("external_preflight_repeatability"), "{error}");
}

#[test]
fn external_rejects_partial_duplicate_and_wrong_model_responses() {
    for mode in 0..3 {
        let server = Server::new(move |call, input| {
            let mut response = success(input);
            if call >= 2 {
                match mode {
                    0 => { response["data"].as_array_mut().unwrap().pop(); }
                    1 => { response["data"][0]["index"] = json!(0); }
                    _ => { response["model"] = json!("different-model"); }
                }
            }
            Reply::ok(response)
        });
        let error = server.connect().embed_batch_sync(&["first", "second"]).unwrap_err().to_string();
        let expected = ["external_partial_response", "external_response_index", "external_model_mismatch"][mode];
        assert!(error.contains(expected), "{error}");
    }
}

#[test]
fn external_later_failure_returns_no_partial_batch_and_redacts_body() {
    let server = Server::new(|call, input| {
        if call == 3 {
            Reply { status: 503, body: "SECRET SESSION TEXT; secret API key".into(), delay: Duration::ZERO }
        } else { Reply::ok(success(input)) }
    });
    let embedder = ExternalEmbedder::connect(server.config(&[("CASS_EXTERNAL_EMBEDDING_BATCH_SIZE", "3")]), Arc::new(|| false)).unwrap();
    let error = embedder.embed_batch_sync(&["a", "b", "c", "d", "e", "f", "g"]).unwrap_err().to_string();
    assert!(error.contains("external_http_503"), "{error}");
    assert!(!error.contains("SECRET"));
    assert!(!error.contains(&server.url));
    assert_eq!(server.count(), 4, "third document sub-batch must not be sent");
}

#[test]
fn external_cancellation_before_preflight_sends_nothing() {
    let server = Server::new(|_, input| Reply::ok(success(input)));
    let error = ExternalEmbedder::connect(server.config(&[]), Arc::new(|| true)).unwrap_err().to_string();
    assert!(error.contains("external_cancelled"));
    assert_eq!(server.count(), 0);
}

#[test]
fn external_cancellation_after_send_discards_reply_and_stops_batches() {
    let flag = Arc::new(AtomicBool::new(false));
    let server_flag = Arc::clone(&flag);
    let server = Server::new(move |call, input| {
        if call == 2 { server_flag.store(true, Ordering::SeqCst); }
        Reply::ok(success(input))
    });
    let embedder = ExternalEmbedder::connect(server.config(&[("CASS_EXTERNAL_EMBEDDING_BATCH_SIZE", "3")]), Arc::new(move || flag.load(Ordering::SeqCst))).unwrap();
    let error = embedder.embed_batch_sync(&["a", "b", "c", "d"]).unwrap_err().to_string();
    assert!(error.contains("external_cancelled"), "{error}");
    assert_eq!(server.count(), 3);
    assert!(embedder.embed_sync("must stay local").is_err());
    assert_eq!(server.count(), 3);
}

#[test]
fn external_timeout_is_bounded() {
    let server = Server::new(|call, input| {
        let mut response = Reply::ok(success(input));
        if call >= 2 { response.delay = Duration::from_millis(250); }
        response
    });
    let embedder = ExternalEmbedder::connect(server.config(&[("CASS_EXTERNAL_EMBEDDING_TIMEOUT_MS", "100")]), Arc::new(|| false)).unwrap();
    let started = std::time::Instant::now();
    let error = embedder.embed_sync("deadline test").unwrap_err().to_string();
    assert!(error.contains("external_timeout"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn external_byte_budget_accounts_for_json_escaping_and_validates_all_inputs_first() {
    let server = Server::new(|_, input| Reply::ok(success(input)));
    let embedder = ExternalEmbedder::connect(server.config(&[("CASS_EXTERNAL_EMBEDDING_MAX_REQUEST_BYTES", "1024")]), Arc::new(|| false)).unwrap();
    let large = "x".repeat(1024);
    assert!(embedder.embed_batch_sync(&["secret", large.as_str()]).is_err());
    assert!(embedder.embed_batch_sync(&["secret", " "]).is_err());
    assert_eq!(server.count(), 2);
    let escaped = "\"\\\n".repeat(60);
    embedder.embed_batch_sync(&[&escaped, &escaped, &escaped, &escaped]).unwrap();
    for request in server.seen.lock().unwrap().iter().skip(2) {
        assert!(serde_json::to_vec(request).unwrap().len() <= 1024);
    }
    assert_eq!(server.count(), 4);
}

#[test]
fn external_malformed_json_is_sanitized() {
    let server = Server::new(|_, _| Reply { status: 200, body: "{SECRET USER TEXT".into(), delay: Duration::ZERO });
    let error = ExternalEmbedder::connect(server.config(&[]), Arc::new(|| false)).unwrap_err().to_string();
    assert!(error.contains("external_response_json"));
    assert!(!error.contains("SECRET"));
}
