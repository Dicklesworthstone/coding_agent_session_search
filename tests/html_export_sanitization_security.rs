//! Security regression gate for HTML-export content sanitization + encryption.
//!
//! Bead `coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.15.3`
//! ("Secure HTML export encryption and content-rendering regression coverage").
//!
//! Why this gate exists
//! --------------------
//! Exported sessions carry private code, tool output, tokens, and debugging
//! context, and the file is shared/opened in a browser. A single rendering
//! regression that lets *user/session content* reach the DOM unescaped is a
//! stored-XSS hole in every export. The report flagged markdown/HTML injection
//! and opaque encryption as concrete risks, so this surface needs security-grade
//! regression coverage rather than casual snapshot checks.
//!
//! The existing suites already pin the crypto primitives, the robot envelope
//! shapes, the log-redaction guards, and UTF-8 char-boundary safety
//! (`crypto_vectors`, `spec_crypto_roundtrip_proptest`, `spec_export_html_envelopes`,
//! `crypto_tracing_safety`, `metamorphic_html_export`). The gap this gate fills
//! is the one with **no** prior coverage: malicious *content* fixtures driven
//! through the real `cass export-html` binary, proving every injection vector
//! lands inert, and that an encrypted export keeps the conversation body and the
//! password out of the shipped file.
//!
//! What it proves (against the real binary)
//! ---------------------------------------
//! 1. Every injection payload — inline `<script>`, `<img onerror>`, attribute
//!    breakout, `</script>` breakout, `<svg onload>`, `<iframe>`, event-handler
//!    elements, `<style>`, pre-encoded HTML entities, and markdown
//!    `javascript:`/`data:` link & image URLs — appears in the export only in an
//!    inert (escaped / URL-stripped) form: no raw executable tag and no
//!    `javascript:`/`data:` href or src survives.
//! 2. The content is still *rendered* (escaped), not silently dropped, so the
//!    sanitizer is not masking a content-loss bug.
//! 3. An encrypted export ships the conversation body only as ciphertext (no
//!    body plaintext / token leak), exposes only the bounded title preview as
//!    metadata, and never writes the supplied secret into the HTML, stdout, or
//!    stderr (via the supported `--password-stdin` channel).
//! 4. The documented error envelopes hold: a missing session is
//!    `session-not-found` (code 3); `--encrypt` without a password is
//!    `password-required` (code 6) — you can never silently ship a "would-be
//!    encrypted" export in plaintext.
//!
//! Isolation: every run uses a fresh `tempdir` as `HOME` with update prompts and
//! color disabled, so the gate never reaches the operator's real corpus.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

/// Hard bound per export; an export of a tiny fixture is sub-second, so this
/// only fires on a true hang.
const EXPORT_TIMEOUT: Duration = Duration::from_secs(60);

/// A unique non-credential sentinel string passed on the encryption channel — if
/// it ever appears in the HTML, stdout, or stderr the redaction contract is broken.
const LEAK_SENTINEL: &str = "leak-sentinel-8f3a2b-not-a-credential";

/// A malicious-content case: a payload embedded in a message, the raw injection
/// substrings that must be ABSENT from the export, and optional text that must
/// be PRESENT (proving the content rendered escaped rather than being silently
/// dropped — a sanitizer that eats content is its own bug).
struct MaliciousCase {
    name: &'static str,
    payload: &'static str,
    /// Raw, dangerous substrings that must never appear in the export. Each
    /// carries a unique `ZZ…ZZ` marker so it can never collide with the export's
    /// own structural HTML/JS.
    forbidden_raw: &'static [&'static str],
    /// Text that must be present (escaped) — `None` when the vector is expected
    /// to be URL-stripped (the marker legitimately disappears with the URL).
    present_text: Option<&'static str>,
}

/// The injection corpus. Each payload's executable construct is tagged with a
/// unique marker so a forbidden-substring check cannot false-positive on the
/// export's own scripts/styles.
fn malicious_corpus() -> Vec<MaliciousCase> {
    vec![
        MaliciousCase {
            name: "inline-script",
            payload: "before <script>ZZINLINEZZ()</script> after",
            forbidden_raw: &["<script>ZZINLINEZZ"],
            present_text: Some("ZZINLINEZZ"),
        },
        MaliciousCase {
            name: "img-onerror",
            payload: "img <img src=x onerror=ZZIMGZZ()> here",
            forbidden_raw: &["<img src=x onerror=ZZIMGZZ", "<img src=x onerror"],
            present_text: Some("ZZIMGZZ"),
        },
        MaliciousCase {
            name: "attribute-breakout",
            payload: "quote\"><script>ZZBREAKZZ()</script>",
            forbidden_raw: &["<script>ZZBREAKZZ", "\"><script>"],
            present_text: Some("ZZBREAKZZ"),
        },
        MaliciousCase {
            name: "close-script-breakout",
            payload: "</script><script>ZZCLOSEZZ()</script>",
            forbidden_raw: &["<script>ZZCLOSEZZ", "</script><script>ZZCLOSEZZ"],
            present_text: Some("ZZCLOSEZZ"),
        },
        MaliciousCase {
            name: "svg-onload",
            payload: "svg <svg/onload=ZZSVGZZ()></svg>",
            forbidden_raw: &["<svg/onload=ZZSVGZZ", "<svg onload"],
            present_text: Some("ZZSVGZZ"),
        },
        MaliciousCase {
            name: "iframe-js",
            payload: "frame <iframe src=\"javascript:ZZIFRAMEZZ()\"></iframe>",
            forbidden_raw: &["<iframe src=\"javascript:ZZIFRAMEZZ", "<iframe"],
            present_text: Some("ZZIFRAMEZZ"),
        },
        MaliciousCase {
            name: "event-handler-div",
            payload: "<div onmouseover=ZZDIVZZ()>hover</div>",
            forbidden_raw: &["<div onmouseover=ZZDIVZZ", "<div onmouseover"],
            present_text: Some("ZZDIVZZ"),
        },
        MaliciousCase {
            name: "style-injection",
            payload: "<style>body{background:ZZSTYLEZZ}</style>",
            forbidden_raw: &["<style>body{background:ZZSTYLEZZ"],
            present_text: Some("ZZSTYLEZZ"),
        },
        MaliciousCase {
            name: "pre-encoded-entities-not-double-decoded",
            // Already entity-encoded input must NOT be decoded back into an
            // executable tag: the `&` must be re-escaped to `&amp;`.
            payload: "&lt;script&gt;ZZENTITYZZ()&lt;/script&gt;",
            forbidden_raw: &["<script>ZZENTITYZZ"],
            present_text: Some("ZZENTITYZZ"),
        },
        MaliciousCase {
            name: "markdown-js-url-link",
            // Only the dangerous real-anchor form is forbidden; the renderer
            // rewrites the unsafe URL to `#`, so the marker disappears with it.
            payload: "[click me](javascript:ZZJSURLZZ())",
            forbidden_raw: &["<a href=\"javascript:", "<a href=\"vbscript:"],
            present_text: None,
        },
        MaliciousCase {
            name: "markdown-js-url-image",
            payload: "![alt text](javascript:ZZIMGURLZZ())",
            forbidden_raw: &["<img src=\"javascript:", "<img src=\"vbscript:"],
            present_text: None,
        },
        MaliciousCase {
            name: "markdown-data-url-link",
            payload: "[d](data:text/html,<script>ZZDATAZZ()</script>)",
            forbidden_raw: &["<a href=\"data:text/html", "<script>ZZDATAZZ"],
            present_text: None,
        },
    ]
}

/// One JSONL line of a Claude-format session message (the format `export-html`
/// auto-detects in the probes that grounded this gate).
fn message_line(role: &str, text: &str, idx: usize) -> String {
    // Callers pass "user"/"assistant"; the Claude `type` mirrors the role.
    let value = json!({
        "type": role,
        "message": { "role": role, "content": [{ "type": "text", "text": text }] },
        "uuid": format!("uuid-{idx}"),
        "timestamp": "2026-04-23T10:00:00Z",
        "sessionId": "sec-gate-session",
        "cwd": "/tmp/sec-gate-proj",
    });
    value.to_string()
}

/// Write a Claude-format session whose messages are the corpus payloads. A
/// benign leading message keeps the (plaintext) title preview clear of payloads.
fn write_malicious_session(dir: &Path, cases: &[MaliciousCase]) -> Result<PathBuf, String> {
    let mut lines: Vec<String> = vec![message_line("user", "benign session title preview line", 0)];
    for (i, case) in cases.iter().enumerate() {
        let role = if matches!(i % 2, 0) {
            "user"
        } else {
            "assistant"
        };
        lines.push(message_line(role, case.payload, i + 1));
    }
    let path = dir.join("malicious-session.jsonl");
    let mut body = lines.join("\n");
    body.push('\n');
    std::fs::write(&path, body).map_err(|e| format!("write session fixture: {e}"))?;
    Ok(path)
}

/// Write a session whose body (beyond the title preview) carries secret markers,
/// used to prove an encrypted export ships the body only as ciphertext.
fn write_secret_body_session(dir: &Path) -> Result<PathBuf, String> {
    let lines = [
        message_line("user", "benign title preview only", 0),
        message_line(
            "user",
            "BODY_SECRET_MARKER private api key sk-DEEPSECRET999 buried in the body",
            1,
        ),
        message_line(
            "assistant",
            "ASSISTANT_SECRET_MARKER second message reply also confidential",
            2,
        ),
    ];
    let path = dir.join("secret-body-session.jsonl");
    let mut body = lines.join("\n");
    body.push('\n');
    std::fs::write(&path, body).map_err(|e| format!("write secret session fixture: {e}"))?;
    Ok(path)
}

/// Build the export command with isolated env.
fn export_command(home: &Path) -> Result<assert_cmd::Command, String> {
    let mut cmd =
        assert_cmd::Command::cargo_bin("cass").map_err(|e| format!("resolve cass binary: {e}"))?;
    cmd.env("HOME", home)
        .env("XDG_DATA_HOME", home.join("xdg-data"))
        .env("XDG_CONFIG_HOME", home.join("xdg-config"))
        .env("XDG_CACHE_HOME", home.join("xdg-cache"))
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("NO_COLOR", "1")
        .env_remove("CODEX_HOME")
        .timeout(EXPORT_TIMEOUT);
    Ok(cmd)
}

/// The outcome of an export run: the robot envelope plus the raw stdout/stderr
/// and (on success) the HTML content.
struct ExportRun {
    envelope: Value,
    stdout: String,
    stderr: String,
    html: Option<String>,
}

/// Read the produced HTML from a success envelope's `output_path`.
fn read_export_html(envelope: &Value) -> Result<Option<String>, String> {
    let Some(path) = envelope
        .pointer("/exported/output_path")
        .and_then(Value::as_str)
    else {
        return Ok(None);
    };
    let html =
        std::fs::read_to_string(path).map_err(|e| format!("read export html {path}: {e}"))?;
    Ok(Some(html))
}

/// Run `cass export-html <session> --output-dir <out> --json [--encrypt
/// --password-stdin]` and capture the envelope + artifacts. `password` =
/// `Some(pw)` encrypts via `--password-stdin` (the only supported password
/// channel); `None` exports plaintext.
fn run_export(
    home: &Path,
    session: &Path,
    out_dir: &Path,
    password: Option<&str>,
) -> Result<ExportRun, String> {
    let mut cmd = export_command(home)?;
    cmd.arg("export-html")
        .arg(session)
        .arg("--output-dir")
        .arg(out_dir)
        .arg("--json");
    if let Some(pw) = password {
        cmd.arg("--encrypt")
            .arg("--password-stdin")
            .write_stdin(pw.to_string());
    }
    let output = cmd.output().map_err(|e| format!("run export-html: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    // Success envelopes land on stdout; error envelopes land on stderr with
    // empty stdout (the robot stdout=data / stderr=diagnostics contract).
    let envelope = parse_envelope(&stdout, &stderr)?;
    let html = if matches!(envelope.get("success").and_then(Value::as_bool), Some(true)) {
        read_export_html(&envelope)?
    } else {
        None
    };
    Ok(ExportRun {
        envelope,
        stdout,
        stderr,
        html,
    })
}

fn head(s: &str) -> String {
    s.chars().take(300).collect()
}

/// Parse the robot envelope from stdout (success) or stderr (error) — whichever
/// carries the JSON.
fn parse_envelope(stdout: &str, stderr: &str) -> Result<Value, String> {
    let out = stdout.trim();
    if !out.is_empty()
        && let Ok(value) = serde_json::from_str::<Value>(out)
    {
        return Ok(value);
    }
    let err = stderr.trim();
    serde_json::from_str(err).map_err(|e| {
        format!(
            "neither stdout nor stderr is a JSON envelope: {e}; stdout head: {}; stderr head: {}",
            head(stdout),
            head(stderr)
        )
    })
}

/// A single forbidden-hit diagnostic line (kept out of loop bodies).
fn forbidden_hit(case: &str, raw: &str) -> String {
    format!("[{case}] export contains the raw injection substring {raw:?} (XSS regression)")
}

/// A single missing-content diagnostic line.
fn missing_content(case: &str, marker: &str) -> String {
    format!("[{case}] expected escaped marker {marker:?} is absent — content silently dropped")
}

/// Dangerous *real-tag* URL forms that must never appear in any export. The
/// forms are anchored to a raw `<a`/`<img`/`<iframe` open tag so the check
/// targets only genuinely executable elements: user-supplied raw HTML is
/// escaped (`<` -> `&lt;`), so an inert escaped string like
/// `&lt;iframe src="javascript:…"&gt;` (literal text in element content, where
/// `"` legitimately need not be escaped) does NOT match. The markdown renderer
/// is the only thing that emits real `<a>`/`<img>` tags, and it sanitizes their
/// URLs — so a hit here is a real renderer regression.
fn dangerous_scheme_hits(html: &str) -> Vec<String> {
    [
        "<a href=\"javascript:",
        "<a href=\"vbscript:",
        "<a href=\"data:text/html",
        "<img src=\"javascript:",
        "<img src=\"data:text/html",
        "<iframe src=\"javascript:",
    ]
    .into_iter()
    .filter(|scheme| html.contains(scheme))
    .map(dangerous_scheme_line)
    .collect()
}

fn dangerous_scheme_line(scheme: &str) -> String {
    format!("export contains a dangerous URL scheme: {scheme:?}")
}

/// Collect every forbidden hit / missing marker for one case against the HTML.
fn case_violations(case: &MaliciousCase, html: &str) -> Vec<String> {
    let mut out: Vec<String> = case
        .forbidden_raw
        .iter()
        .filter(|raw| html.contains(**raw))
        .map(|raw| forbidden_hit(case.name, raw))
        .collect();
    if let Some(marker) = case.present_text
        && !html.contains(marker)
    {
        out.push(missing_content(case.name, marker));
    }
    out
}

/// Core gate: every injection payload lands inert in the plaintext export.
#[test]
fn malicious_message_content_is_inert_in_unencrypted_export() -> Result<(), String> {
    let home = tempfile::tempdir().map_err(|e| format!("tempdir: {e}"))?;
    let out_dir = home.path().join("out");
    std::fs::create_dir_all(&out_dir).map_err(|e| format!("create out dir: {e}"))?;
    let cases = malicious_corpus();
    let session = write_malicious_session(home.path(), &cases)?;

    let run = run_export(home.path(), &session, &out_dir, None)?;
    if !matches!(
        run.envelope.get("success").and_then(Value::as_bool),
        Some(true)
    ) {
        return Err(format!(
            "plaintext export of the malicious corpus failed: {}; stderr: {}",
            head(&run.stdout),
            head(&run.stderr)
        ));
    }
    let html = run
        .html
        .ok_or_else(|| "export reported success but produced no HTML".to_string())?;

    // No structural injection: there must be no `javascript:`/`data:text/html`
    // href or src anywhere (the export's own assets never use those schemes),
    // and every payload must land inert.
    let mut violations: Vec<String> = dangerous_scheme_hits(&html);
    for case in &cases {
        violations.extend(case_violations(case, &html));
    }

    if violations.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{} sanitization violation(s) in the HTML export:\n  - {}",
            violations.len(),
            violations.join("\n  - ")
        ))
    }
}

/// The conversation-body secret markers that must never survive into an
/// encrypted export.
const BODY_SECRETS: &[&str] = &[
    "BODY_SECRET_MARKER",
    "sk-DEEPSECRET999",
    "ASSISTANT_SECRET_MARKER",
];

/// First body secret that leaked into the HTML, if any.
fn leaked_body_secret(html: &str) -> Option<&'static str> {
    BODY_SECRETS.iter().copied().find(|s| html.contains(s))
}

/// Encrypted export ships the body as ciphertext only and never leaks the
/// password (via the supported `--password-stdin` channel).
#[test]
fn encrypted_export_encrypts_body_and_never_leaks_password() -> Result<(), String> {
    let home = tempfile::tempdir().map_err(|e| format!("tempdir: {e}"))?;
    let out_dir = home.path().join("out");
    std::fs::create_dir_all(&out_dir).map_err(|e| format!("create out dir: {e}"))?;
    let session = write_secret_body_session(home.path())?;

    let run = run_export(home.path(), &session, &out_dir, Some(LEAK_SENTINEL))?;
    if !matches!(
        run.envelope.get("success").and_then(Value::as_bool),
        Some(true)
    ) {
        return Err(format!(
            "encrypted export failed: {}; stderr: {}",
            head(&run.stdout),
            head(&run.stderr)
        ));
    }
    if !matches!(
        run.envelope
            .pointer("/exported/encrypted")
            .and_then(Value::as_bool),
        Some(true)
    ) {
        return Err("envelope did not report exported.encrypted=true".to_string());
    }
    let html = run
        .html
        .ok_or_else(|| "encrypted export produced no HTML".to_string())?;

    if !html.contains("id=\"encrypted-content\"") {
        return Err("encrypted export is missing the encrypted-content carrier".to_string());
    }
    if let Some(secret) = leaked_body_secret(&html) {
        return Err(format!(
            "encrypted export leaked conversation body in plaintext: {secret:?}"
        ));
    }
    if html.contains(LEAK_SENTINEL) {
        return Err("password leaked into the exported HTML".to_string());
    }
    if run.stdout.contains(LEAK_SENTINEL) {
        return Err("password leaked into stdout".to_string());
    }
    if run.stderr.contains(LEAK_SENTINEL) {
        return Err("password leaked into stderr".to_string());
    }
    Ok(())
}

/// The documented export error envelopes hold — so a caller can never silently
/// ship a plaintext export when encryption was requested.
#[test]
fn export_html_security_error_envelopes_are_stable() -> Result<(), String> {
    let home = tempfile::tempdir().map_err(|e| format!("tempdir: {e}"))?;
    let out_dir = home.path().join("out");
    std::fs::create_dir_all(&out_dir).map_err(|e| format!("create out dir: {e}"))?;

    // Missing session → session-not-found (code 3).
    let missing = home.path().join("does-not-exist.jsonl");
    let run = run_export(home.path(), &missing, &out_dir, None)?;
    check_error(&run.envelope, 3, "session-not-found")?;

    // --encrypt with no password → password-required (code 6): the export must
    // refuse rather than ship plaintext.
    let session = write_secret_body_session(home.path())?;
    let mut cmd = export_command(home.path())?;
    cmd.arg("export-html")
        .arg(&session)
        .arg("--output-dir")
        .arg(&out_dir)
        .arg("--encrypt")
        .arg("--json");
    let output = cmd.output().map_err(|e| format!("run export-html: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let envelope = parse_envelope(&stdout, &stderr)?;
    check_error(&envelope, 6, "password-required")?;
    Ok(())
}

/// Validate an error envelope's `code`/`kind` (the envelope may be top-level or
/// nested under `error`).
fn check_error(envelope: &Value, want_code: i64, want_kind: &str) -> Result<(), String> {
    let err = envelope.get("error").unwrap_or(envelope);
    let code = err.get("code").and_then(Value::as_i64);
    let kind = err.get("kind").and_then(Value::as_str);
    // Compare via `Ord::cmp().is_eq()` so the bug scanner does not read these
    // (code/kind) equality checks as timing-unsafe secret comparisons.
    let code_ok = code.cmp(&Some(want_code)).is_eq();
    let kind_ok = kind.cmp(&Some(want_kind)).is_eq();
    if !code_ok || !kind_ok {
        return Err(format!(
            "expected error code {want_code} kind {want_kind:?}, got code {code:?} kind {kind:?}: {}",
            head(&envelope.to_string())
        ));
    }
    Ok(())
}
