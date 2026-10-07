//! Opt-in transient-status retries. A retry never acquires a new endpoint,
//! resets a checkpoint, or extends the logical request's original deadline.
use super::*;
use chrono::{DateTime, Utc};
use reqwest::blocking::Response;
use reqwest::header::{HeaderMap, RETRY_AFTER};

impl ExternalEmbedder {
    pub(super) fn send_with_retry(&self, body: Vec<u8>) -> EmbedderResult<Response> {
        let started = Instant::now();
        let mut retries = 0;
        loop {
            self.check_cancelled()?;
            let remaining = self.config.timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Err(self.failure(
                    "external_timeout: request/retry deadline exceeded; checkpoint retained",
                ));
            }
            let mut request = self
                .client
                .post(self.config.endpoint.clone())
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .timeout(remaining)
                .body(body.clone());
            if let Some(key) = &self.config.api_key {
                request = request.bearer_auth(key);
            }
            let sent = request.send();
            self.check_cancelled()?;
            // A lost connection/timeout can be ambiguous about server execution.
            // Do not retry those, nor any successfully received malformed vector.
            let response = sent.map_err(|error| {
                self.failure(if error.is_timeout() {
                    "external_timeout: request deadline exceeded; checkpoint retained, check server load or timeout configuration"
                } else {
                    "external_transport: request failed; check server reachability and TLS; checkpoint retained"
                })
            })?;
            let status = response.status().as_u16();
            if self.config.max_retries == 0 || !matches!(status, 429 | 500 | 502 | 503 | 504) {
                return Ok(response);
            }
            if retries >= self.config.max_retries {
                return Err(self.failure(format!(
                    "external_http_{status}: retry limit reached after {} attempts; checkpoint retained",
                    retries + 1,
                )));
            }
            let suggested = retry_after(response.headers(), Utc::now()).map_err(|()| {
                self.failure(format!(
                    "external_http_{status}: invalid or ambiguous Retry-After; not retried; checkpoint retained",
                ))
            })?;
            // At most five retries: 100, 200, 400, 800, 1600 ms without a
            // larger server-directed delay. Zero never means a busy retry loop.
            let backoff = Duration::from_millis(100 * (1_u64 << retries.min(4)));
            let delay = suggested.unwrap_or_default().max(backoff);
            let remaining = self.config.timeout.saturating_sub(started.elapsed());
            if delay >= remaining {
                return Err(self.failure(format!(
                    "external_http_{status}: retry delay exceeds remaining request deadline; checkpoint retained",
                )));
            }
            // Do not read, log, retain, or parse an error body. It may echo text
            // or credentials. Release this response before a cooperative wait.
            drop(response);
            let wait_started = Instant::now();
            loop {
                self.check_cancelled()?;
                let remaining = delay.saturating_sub(wait_started.elapsed());
                if remaining.is_zero() {
                    break;
                }
                std::thread::sleep(remaining.min(ADMISSION_POLL_INTERVAL));
            }
            retries += 1;
        }
    }
}

/// Delay-seconds and the standard IMF-fixdate form. Invalid/unsupported date
/// formats are refused rather than ignoring a server's minimum wait. Parsing
/// is bounded and never includes the untrusted header in an error message.
pub(super) fn retry_after(headers: &HeaderMap, now: DateTime<Utc>) -> Result<Option<Duration>, ()> {
    let all = headers.get_all(RETRY_AFTER);
    let mut values = all.iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(());
    }
    let value = value.to_str().map_err(|_| ())?.trim();
    if value.is_empty() || value.len() > 128 {
        return Err(());
    }
    if value.bytes().all(|byte| byte.is_ascii_digit()) {
        return value
            .parse::<u64>()
            .map(|seconds| Some(Duration::from_secs(seconds)))
            .map_err(|_| ());
    }
    let when = DateTime::parse_from_rfc2822(value).map_err(|_| ())?;
    Ok(Some(
        when.signed_duration_since(now).to_std().unwrap_or_default(),
    ))
}
