//! Metamorphic test: HTML export from the same source must produce
//! byte-identical output across runs.
//!
//! `coding_agent_session_search-afam7`: src/html_export/ renders
//! Markdown/JSON conversation data into static HTML. The renderer
//! iterates HashMap-backed metadata in places (workspace_original,
//! source_id origin_kind) and uses BTreeMap or sorted Vec elsewhere.
//! A regression that introduces non-deterministic ordering (e.g.,
//! switching a sorted Vec to HashSet iteration) would silently
//! produce different exports for the same on-disk input, breaking
//! content-addressed downstream consumers and confusing operator
//! diff tooling.
//!
//! MR archetype: **Equivalence (Pattern 1)** from the metamorphic
//! skill. T(export) = export(same_session_again). Relation: byte-
//! equal HTML output. The optional transient-field scrub (currently
//! a no-op because the renderer does not embed wall-clock
//! timestamps in the output, only timestamps that come from the
//! session data itself) is wired in case a future change adds one.

use assert_cmd::Command;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn fixture_path(category: &str, name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/html_export")
        .join(category)
        .join(name)
}

#[allow(deprecated)]
fn cass_cmd() -> Command {
    let mut cmd = Command::cargo_bin("cass").unwrap();
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");
    cmd
}

/// Run `cass export-html <session> --output-dir <out_dir> --filename
/// <filename>` and return the absolute path to the produced HTML.
fn export_to(session_path: &Path, out_dir: &Path, filename: &str) -> PathBuf {
    let output = cass_cmd()
        .args([
            "export-html",
            session_path.to_str().expect("utf8 session path"),
            "--output-dir",
            out_dir.to_str().expect("utf8 out dir"),
            "--filename",
            filename,
            "--robot",
        ])
        .output()
        .expect("run cass export-html");
    assert!(
        output.status.success(),
        "cass export-html exited non-zero: status={:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("export-html --robot emits JSON");
    let path = json["exported"]["output_path"]
        .as_str()
        .expect("exported.output_path is a string");
    PathBuf::from(path)
}

/// Strip transient renderer fields that are allowed to vary across
/// runs even though the source is identical. Currently a near no-op
/// because the renderer does not embed wall-clock timestamps in the
/// output (it only echoes timestamps that came from the session
/// data itself, which IS stable). The scrubs below are defensive
/// against a future change that adds e.g. a `<meta
/// name="rendered_at" ...>` tag — pinning the contract now means
/// such a change would either preserve byte-equality OR force the
/// scrub list to grow, both of which are visible signals.
fn scrub_transient(html: &str) -> String {
    let mut scrubbed = html.to_string();
    // ISO timestamps in `rendered_at` / `generated_at` meta tags.
    let rendered_at = regex::Regex::new(
        r#"(?P<key>(rendered|generated)_at)="[^"]*""#,
    )
    .expect("scrub regex compiles");
    scrubbed = rendered_at
        .replace_all(&scrubbed, "$key=\"[SCRUBBED]\"")
        .into_owned();
    scrubbed
}

/// `coding_agent_session_search-afam7`: pin the equivalence MR
/// `export(s) == export(s)` byte-for-byte (modulo the documented
/// transient scrub list). Re-running export against the same source
/// MUST produce identical bytes — non-deterministic iteration order
/// (HashMap iteration leaking into the renderer, unsorted Vec, etc.)
/// would be caught here.
#[test]
fn mr_html_export_byte_idempotent_for_same_source() {
    let session_path = fixture_path("real_sessions", "claude_code_auth_fix.jsonl");
    assert!(
        session_path.exists(),
        "expected fixture session at {}; tests/fixtures/html_export/real_sessions/ \
         must contain claude_code_auth_fix.jsonl for this metamorphic check",
        session_path.display()
    );

    let tmp_a = TempDir::new().expect("tempdir a");
    let tmp_b = TempDir::new().expect("tempdir b");
    let html_a = export_to(&session_path, tmp_a.path(), "first.html");
    let html_b = export_to(&session_path, tmp_b.path(), "second.html");

    let bytes_a = fs::read_to_string(&html_a).expect("read first export");
    let bytes_b = fs::read_to_string(&html_b).expect("read second export");

    let scrubbed_a = scrub_transient(&bytes_a);
    let scrubbed_b = scrub_transient(&bytes_b);

    if scrubbed_a != scrubbed_b {
        // Find the first divergence so the error message points at the
        // exact byte rather than just "they differ".
        let first_diff = scrubbed_a
            .as_bytes()
            .iter()
            .zip(scrubbed_b.as_bytes())
            .position(|(a, b)| a != b);
        let context = first_diff
            .map(|idx| {
                let lo = idx.saturating_sub(40);
                let hi_a = (idx + 40).min(scrubbed_a.len());
                let hi_b = (idx + 40).min(scrubbed_b.len());
                format!(
                    "first divergence at byte {idx}:\n  a: {:?}\n  b: {:?}",
                    &scrubbed_a[lo..hi_a],
                    &scrubbed_b[lo..hi_b]
                )
            })
            .unwrap_or_else(|| {
                format!(
                    "outputs differ in length: a={} bytes, b={} bytes",
                    scrubbed_a.len(),
                    scrubbed_b.len()
                )
            });
        panic!(
            "metamorphic invariant violated: HTML export of the same source produced \
             different bytes across runs. {context}\n\
             Sources: {} vs {}\n\
             This usually indicates non-deterministic iteration order (HashMap, \
             HashSet) leaking into the renderer.",
            html_a.display(),
            html_b.display()
        );
    }
}
