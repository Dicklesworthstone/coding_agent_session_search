//! Explicitly consented, OpenAI-compatible external embeddings (gl29d / GH #481).
//!
//! Merely configuring a URL never sends text. Callers must select `external` AND
//! set `CASS_EXTERNAL_EMBEDDINGS=1`. A successfully constructed provider has
//! passed fixed-input dimension, unit-norm and repeatability checks. It is never
//! a local MiniLM provider, including when the server runs MiniLM on localhost.

use std::fmt;
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use url::{Host, Url};

use super::embedder::{Embedder, EmbedderError, EmbedderResult};

pub const EXTERNAL_EMBEDDER: &str = "external";
pub const EXTERNAL_VECTOR_SPACE_REVISION: &str = "external-openai-v1:unit-norm:passages-v2";
const MAX_DIMENSION: usize = 16_384;
const MAX_BATCH_SIZE: usize = 128;
const MAX_REQUEST_BYTES: usize = 4 * 1024 * 1024;
const MAX_RESPONSE_BYTES: u64 = 64 * 1024 * 1024;
const NORM_TOLERANCE: f64 = 1e-3;
const REPEATABILITY_TOLERANCE: f32 = 1e-5;
const PROBES: &[&str] = &[
    "cass external embedding probe: the quick brown fox",
    "fn add(left: i32, right: i32) -> i32 { left + right }",
    "Semantic search: café, 日本語, and a different sentence.",
];

/// A caller-owned cancellation check. It must be cheap and must not block.
/// Cancellation is checked before/after every request; an in-flight blocking
/// request is bounded by the configured timeout, not abandoned on a new thread.
pub type CancelCheck = Arc<dyn Fn() -> bool + Send + Sync>;

#[derive(Clone)]
pub struct ExternalEmbeddingConfig {
    endpoint: Url,
    model: String,
    dimension: usize,
    revision: String,
    batch_size: usize,
    max_request_bytes: usize,
    timeout: Duration,
    api_key: Option<String>,
}

impl fmt::Debug for ExternalEmbeddingConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExternalEmbeddingConfig")
            .field("identity", &self.identity())
            .field("dimension", &self.dimension)
            .field("batch_size", &self.batch_size)
            .field("max_request_bytes", &self.max_request_bytes)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

fn unavailable(reason: impl Into<String>) -> EmbedderError {
    EmbedderError::EmbedderUnavailable {
        model: EXTERNAL_EMBEDDER.to_owned(),
        reason: reason.into(),
    }
}

impl ExternalEmbeddingConfig {
    /// Resolve consent before even looking up URL/model/credentials. This does
    /// not construct an HTTP client, probe a server, or load a local model.
    pub fn from_env() -> EmbedderResult<Option<Self>> {
        Self::from_lookup(|key| dotenvy::var(key).ok())
    }

    /// Inject configuration without mutating process-global environment in tests.
    pub fn from_lookup(
        mut lookup: impl FnMut(&str) -> Option<String>,
    ) -> EmbedderResult<Option<Self>> {
        match lookup("CASS_EXTERNAL_EMBEDDINGS").as_deref() {
            None | Some("") | Some("0") | Some("false") => return Ok(None),
            Some("1") | Some("true") => {}
            _ => {
                return Err(unavailable(
                    "external_config: CASS_EXTERNAL_EMBEDDINGS must be 0, false, 1, or true",
                ));
            }
        }
        let mut required = |key: &str| {
            lookup(key)
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| unavailable(format!("external_config: {key} is required")))
        };
        let raw_url = required("CASS_EXTERNAL_EMBEDDING_URL")?;
        let model = required("CASS_EXTERNAL_EMBEDDING_MODEL")?;
        let raw_dimension = required("CASS_EXTERNAL_EMBEDDING_DIMENSION")?;
        let endpoint = Url::parse(&raw_url).map_err(|_| {
            unavailable("external_config: CASS_EXTERNAL_EMBEDDING_URL is not a valid URL")
        })?;
        let loopback = match endpoint.host() {
            Some(Host::Ipv4(ip)) => ip.is_loopback(),
            Some(Host::Ipv6(ip)) => ip.is_loopback(),
            Some(Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
            None => false,
        };
        if endpoint.host().is_none()
            || !(endpoint.scheme() == "https" || (endpoint.scheme() == "http" && loopback))
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
            || !endpoint.path().ends_with("/v1/embeddings")
        {
            return Err(unavailable(
                "external_config: URL must end in /v1/embeddings, use HTTPS (HTTP only on loopback), and contain no credentials, query, or fragment",
            ));
        }
        if model.len() > 512 || model.chars().any(char::is_control) {
            return Err(unavailable(
                "external_config: model must be at most 512 bytes without control characters",
            ));
        }
        fn number(raw: &str, key: &str, min: usize, max: usize) -> EmbedderResult<usize> {
            raw.parse::<usize>()
                .ok()
                .filter(|n| (min..=max).contains(n))
                .ok_or_else(|| {
                    unavailable(format!(
                        "external_config: {key} must be an integer in {min}..={max}"
                    ))
                })
        }
        let dimension = number(
            &raw_dimension,
            "CASS_EXTERNAL_EMBEDDING_DIMENSION",
            1,
            MAX_DIMENSION,
        )?;
        let batch_size = number(
            &lookup("CASS_EXTERNAL_EMBEDDING_BATCH_SIZE").unwrap_or_else(|| "64".into()),
            "CASS_EXTERNAL_EMBEDDING_BATCH_SIZE",
            1,
            MAX_BATCH_SIZE,
        )?;
        let max_request_bytes = number(
            &lookup("CASS_EXTERNAL_EMBEDDING_MAX_REQUEST_BYTES").unwrap_or_else(|| "262144".into()),
            "CASS_EXTERNAL_EMBEDDING_MAX_REQUEST_BYTES",
            1024,
            MAX_REQUEST_BYTES,
        )?;
        let timeout_ms = number(
            &lookup("CASS_EXTERNAL_EMBEDDING_TIMEOUT_MS").unwrap_or_else(|| "30000".into()),
            "CASS_EXTERNAL_EMBEDDING_TIMEOUT_MS",
            1,
            120_000,
        )?;
        let revision = lookup("CASS_EXTERNAL_EMBEDDING_REVISION").unwrap_or_else(|| "1".into());
        if revision.trim().is_empty()
            || revision.len() > 512
            || revision.chars().any(char::is_control)
        {
            return Err(unavailable(
                "external_config: revision must be nonempty, at most 512 bytes, and contain no control characters",
            ));
        }
        let api_key = lookup("CASS_EXTERNAL_EMBEDDING_API_KEY").filter(|key| !key.is_empty());
        if let Some(key) = &api_key {
            reqwest::header::HeaderValue::from_str(&format!("Bearer {key}")).map_err(|_| {
                unavailable("external_config: API key is not a valid authorization header")
            })?;
        }
        Ok(Some(Self {
            endpoint,
            model,
            dimension,
            revision,
            batch_size,
            max_request_bytes,
            timeout: Duration::from_millis(timeout_ms as u64),
            api_key,
        }))
    }

    /// Filesystem-safe, provider/model/dimension/revision-bound namespace. The
    /// model string is never sanitized into an ambiguous filename or leaked in
    /// diagnostics. Credentials are deliberately excluded so key rotation resumes.
    pub fn identity(&self) -> String {
        let mut hasher = blake3::Hasher::new();
        for part in [
            self.endpoint.as_str(),
            self.model.as_str(),
            self.revision.as_str(),
        ] {
            hasher.update(&(part.len() as u64).to_le_bytes());
            hasher.update(part.as_bytes());
        }
        format!(
            "external-v1-{}-{}",
            self.dimension,
            hasher.finalize().to_hex()
        )
    }

    pub fn dimension(&self) -> usize {
        self.dimension
    }
}

/// Only recognize canonical identities, never arbitrary path-like strings.
pub fn is_external_identity(id: &str) -> bool {
    let Some((dimension, digest)) = id
        .strip_prefix("external-v1-")
        .and_then(|rest| rest.split_once('-'))
    else {
        return false;
    };
    dimension
        .parse::<usize>()
        .is_ok_and(|d| (1..=MAX_DIMENSION).contains(&d) && d.to_string() == dimension)
        && digest.len() == 64
        && digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub struct ExternalEmbedder {
    config: ExternalEmbeddingConfig,
    id: String,
    client: Client,
    cancelled: CancelCheck,
    // At most one request in flight for this provider, including concurrent
    // query callers. No detached retry workers and no unbounded request fanout.
    request_gate: Mutex<()>,
}

impl fmt::Debug for ExternalEmbedder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExternalEmbedder")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

#[derive(Serialize)]
struct EmbeddingRequest<'a> {
    model: &'a str,
    input: &'a [&'a str],
    encoding_format: &'static str,
}

#[derive(Deserialize)]
struct EmbeddingResponse {
    model: String,
    data: Vec<EmbeddingRow>,
}

#[derive(Deserialize)]
struct EmbeddingRow {
    index: usize,
    embedding: Vec<f32>,
}

impl ExternalEmbedder {
    pub fn from_env() -> EmbedderResult<Self> {
        Self::from_env_with_cancel(Arc::new(|| false))
    }

    pub fn from_env_with_cancel(cancelled: CancelCheck) -> EmbedderResult<Self> {
        let config = ExternalEmbeddingConfig::from_env()?.ok_or_else(|| unavailable(
            "external_disabled: select external and set CASS_EXTERNAL_EMBEDDINGS=1 to consent to sending text outside the process"
        ))?;
        Self::connect(config, cancelled)
    }

    /// Only returns after the known-input checks pass. Probe requests contain
    /// fixed public strings, never archive contents or a user's search query.
    pub fn connect(
        config: ExternalEmbeddingConfig,
        cancelled: CancelCheck,
    ) -> EmbedderResult<Self> {
        if cancelled() {
            return Err(unavailable("external_cancelled: before preflight"));
        }
        crate::ensure_rustls_crypto_provider();
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(config.timeout.min(Duration::from_secs(10)))
            .timeout(config.timeout)
            .build()
            .map_err(|_| unavailable("external_config: could not create HTTP client"))?;
        let provider = Self {
            id: config.identity(),
            config,
            client,
            cancelled,
            request_gate: Mutex::new(()),
        };
        let first = provider.embed_batch_sync(PROBES)?;
        let second = provider.embed_batch_sync(PROBES)?;
        for (a, b) in first.iter().zip(&second) {
            if a.iter()
                .zip(b)
                .any(|(x, y)| (x - y).abs() > REPEATABILITY_TOLERANCE)
            {
                return Err(provider.failure("external_preflight_repeatability: fixed probes changed between requests; verify deterministic inference and model revision"));
            }
        }
        Ok(provider)
    }

    fn failure(&self, reason: impl Into<String>) -> EmbedderError {
        EmbedderError::EmbeddingFailed {
            model: self.id.clone(),
            source: Box::new(std::io::Error::other(reason.into())),
        }
    }

    fn check_cancelled(&self) -> EmbedderResult<()> {
        if (self.cancelled)() {
            return Err(self.failure("external_cancelled: no further requests sent; resume from the last durable checkpoint"));
        }
        Ok(())
    }

    fn body(&self, texts: &[&str]) -> EmbedderResult<Vec<u8>> {
        serde_json::to_vec(&EmbeddingRequest {
            model: &self.config.model,
            input: texts,
            encoding_format: "float",
        })
        .map_err(|_| self.failure("external_request: could not encode request"))
    }

    fn request(&self, body: Vec<u8>, count: usize) -> EmbedderResult<Vec<Vec<f32>>> {
        self.check_cancelled()?;
        let mut request = self
            .client
            .post(self.config.endpoint.clone())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body);
        if let Some(key) = &self.config.api_key {
            request = request.bearer_auth(key);
        }
        let sent = request.send();
        self.check_cancelled()?;
        let response = sent.map_err(|error| self.failure(if error.is_timeout() {
            "external_timeout: request deadline exceeded; checkpoint retained, check server load or timeout configuration"
        } else {
            "external_transport: request failed; check server reachability and TLS; checkpoint retained"
        }))?;
        if !response.status().is_success() {
            // Never include the response body, URL, headers, or reqwest error:
            // servers/proxies may echo session text and credentials there.
            let status = response.status().as_u16();
            let advice = match status {
                401 | 403 => "check CASS_EXTERNAL_EMBEDDING_API_KEY and server permissions",
                404 => "check the /v1/embeddings URL and model configuration",
                413 => "reduce the request byte limit or batch size",
                429 | 500..=599 => "server busy/unavailable; retry from the retained checkpoint",
                300..=399 => "redirect refused; configure the intended endpoint directly",
                _ => "check server configuration; checkpoint retained",
            };
            return Err(self.failure(format!("external_http_{status}: {advice}")));
        }
        if response
            .content_length()
            .is_some_and(|len| len > MAX_RESPONSE_BYTES)
        {
            return Err(self.failure("external_response_too_large: response exceeds 64 MiB"));
        }
        let mut bytes = Vec::new();
        let read = response
            .take(MAX_RESPONSE_BYTES + 1)
            .read_to_end(&mut bytes);
        self.check_cancelled()?;
        read.map_err(|_| self.failure("external_response_read: truncated response or request deadline exceeded; checkpoint retained"))?;
        if bytes.len() as u64 > MAX_RESPONSE_BYTES {
            return Err(self.failure("external_response_too_large: response exceeds 64 MiB"));
        }
        let response: EmbeddingResponse = serde_json::from_slice(&bytes)
            .map_err(|_| self.failure("external_response_json: expected model and indexed float-vector data; malformed or truncated JSON"))?;
        if response.model != self.config.model {
            return Err(self.failure("external_model_mismatch: response model differs from CASS_EXTERNAL_EMBEDDING_MODEL"));
        }
        if response.data.len() != count {
            return Err(self.failure(format!("external_partial_response: received {} vectors for {count} inputs; entire batch rejected", response.data.len())));
        }
        let mut ordered = vec![None; count];
        for row in response.data {
            if row.index >= count || ordered[row.index].is_some() {
                return Err(self.failure("external_response_index: duplicate or out-of-range input index; entire batch rejected"));
            }
            if row.embedding.len() != self.config.dimension {
                return Err(self.failure(format!("external_dimension_mismatch: input {} has {} dimensions, expected {}; entire batch rejected", row.index, row.embedding.len(), self.config.dimension)));
            }
            let norm_squared: f64 = row
                .embedding
                .iter()
                .map(|&value| f64::from(value).powi(2))
                .sum();
            if row.embedding.iter().any(|value| !value.is_finite())
                || !norm_squared.is_finite()
                || (norm_squared.sqrt() - 1.0).abs() > NORM_TOLERANCE
            {
                return Err(self.failure(format!("external_normalization: input {} is not finite and unit-normalized (tolerance {NORM_TOLERANCE}); entire batch rejected", row.index)));
            }
            ordered[row.index] = Some(row.embedding);
        }
        ordered
            .into_iter()
            .map(|row| {
                row.ok_or_else(|| self.failure("external_partial_response: missing input index"))
            })
            .collect()
    }
}

impl Embedder for ExternalEmbedder {
    fn id(&self) -> &str {
        &self.id
    }
    fn dimension(&self) -> usize {
        self.config.dimension
    }
    fn is_semantic(&self) -> bool {
        true
    }
    fn category(&self) -> frankensearch::ModelCategory {
        frankensearch::ModelCategory::TransformerEmbedder
    }

    fn embed_sync(&self, text: &str) -> EmbedderResult<Vec<f32>> {
        self.embed_batch_sync(&[text])?
            .pop()
            .ok_or_else(|| self.failure("external_partial_response: no vector for query"))
    }

    fn embed_batch_sync(&self, texts: &[&str]) -> EmbedderResult<Vec<Vec<f32>>> {
        self.check_cancelled()?;
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let _guard = self
            .request_gate
            .lock()
            .map_err(|_| self.failure("external_internal: request gate poisoned"))?;
        self.check_cancelled()?;
        let overhead = self.body(&[])?.len();
        // Validate all single inputs before disclosing any of this call's text.
        for (index, text) in texts.iter().enumerate() {
            if text.trim().is_empty() {
                return Err(self.failure(format!("external_input_empty: input {index}")));
            }
            let encoded = serde_json::to_string(text)
                .map_err(|_| self.failure("external_request: could not encode input"))?;
            if overhead + encoded.len() > self.config.max_request_bytes {
                return Err(self.failure(format!("external_input_too_large: input {index} exceeds request byte limit; no text from this call sent")));
            }
        }
        let mut result = Vec::with_capacity(texts.len());
        let mut start = 0;
        while start < texts.len() {
            self.check_cancelled()?;
            let mut end = start;
            let mut bytes = overhead;
            while end < texts.len() && end - start < self.config.batch_size {
                let encoded = serde_json::to_string(texts[end])
                    .map_err(|_| self.failure("external_request: could not encode input"))?;
                let next = bytes + encoded.len() + usize::from(end > start);
                if next > self.config.max_request_bytes {
                    break;
                }
                bytes = next;
                end += 1;
            }
            let body = self.body(&texts[start..end])?;
            if body.len() > self.config.max_request_bytes || end == start {
                return Err(self
                    .failure("external_request_too_large: request exceeds configured byte limit"));
            }
            result.extend(self.request(body, end - start)?);
            start = end;
        }
        self.check_cancelled()?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests;
