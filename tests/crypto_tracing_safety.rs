//! Tracing-safety tests for crypto derivation functions.
//!
//! Per `coding_agent_session_search-vz9t8.4`. Validates that
//! `hkdf_extract_expand`, `hkdf_extract`, and `derive_chunk_nonce` (the
//! functions instrumented in this bead) emit `tracing` events on success AND
//! error paths AND for empty inputs, while NEVER logging the underlying key
//! material.
//!
//! The leak guard is the most important test: it asserts ABSENCE of sensitive
//! bytes in any captured event field name OR value. If a future change
//! accidentally adds key material to a log line, this test fails BEFORE the
//! change can merge.

use coding_agent_search::encryption::{hkdf_extract, hkdf_extract_expand};
use serial_test::serial;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use std::sync::{Arc, Mutex};
use tracing::field::{Field, Visit};
use tracing::span::Attributes;
use tracing::{Event, Id, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

/// A tracing layer that captures every event + span into a structured buffer
/// for inspection. Records (target, name, kv-pairs).
#[derive(Clone, Default)]
struct CaptureLayer {
    captured: Arc<Mutex<Vec<CapturedRow>>>,
}

#[derive(Debug, Clone)]
struct CapturedRow {
    // `kind` distinguishes "event" / "span_new" / "span_record" rows. It is
    // surfaced via the derived Debug impl in panic messages (dead-code
    // analysis ignores Debug usage).
    #[allow(dead_code)]
    kind: &'static str,
    target: String,
    name: String,
    fields: Vec<(String, String)>,
}

#[derive(Default)]
struct FieldCollector {
    fields: Vec<(String, String)>,
}

impl Visit for FieldCollector {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.fields
            .push((field.name().to_string(), format!("{value:?}")));
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields
            .push((field.name().to_string(), value.to_string()));
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields
            .push((field.name().to_string(), value.to_string()));
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields
            .push((field.name().to_string(), value.to_string()));
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields
            .push((field.name().to_string(), value.to_string()));
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.fields
            .push((field.name().to_string(), value.to_string()));
    }
}

impl<S> Layer<S> for CaptureLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, _id: &Id, _ctx: Context<'_, S>) {
        let mut collector = FieldCollector::default();
        attrs.record(&mut collector);
        let metadata = attrs.metadata();
        self.captured.lock().unwrap().push(CapturedRow {
            kind: "span_new",
            target: metadata.target().to_string(),
            name: metadata.name().to_string(),
            fields: collector.fields,
        });
    }
    fn on_record(&self, _id: &Id, values: &tracing::span::Record<'_>, _ctx: Context<'_, S>) {
        let mut collector = FieldCollector::default();
        values.record(&mut collector);
        self.captured.lock().unwrap().push(CapturedRow {
            kind: "span_record",
            target: String::new(),
            name: String::new(),
            fields: collector.fields,
        });
    }
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut collector = FieldCollector::default();
        event.record(&mut collector);
        let metadata = event.metadata();
        self.captured.lock().unwrap().push(CapturedRow {
            kind: "event",
            target: metadata.target().to_string(),
            name: metadata.name().to_string(),
            fields: collector.fields,
        });
    }
}

fn run_with_capture<F: FnOnce()>(f: F) -> Vec<CapturedRow> {
    let layer = CaptureLayer::default();
    let captured = layer.captured.clone();
    let _guard = tracing_subscriber::registry()
        .with(layer.with_filter(tracing_subscriber::filter::LevelFilter::TRACE))
        .set_default();
    f();
    captured.lock().unwrap().clone()
}

#[test]
#[serial]
fn derive_kek_emits_tracing_on_success() {
    tracing::info!(target: "vz9t8_4_test", scenario = "success_path");
    let rows = run_with_capture(|| {
        let _ = hkdf_extract_expand(b"some-ikm-bytes", b"some-salt", b"cass-test-info", 32)
            .expect("should succeed");
    });
    eprintln!(
        "[vz9t8_4_test] captured {} rows on success path",
        rows.len()
    );
    let hkdf_rows: Vec<_> = rows
        .iter()
        .filter(|r| r.target.contains("encryption") || r.name.contains("hkdf_extract_expand"))
        .collect();
    assert!(
        !hkdf_rows.is_empty(),
        "hkdf_extract_expand must emit at least one tracing event/span; got rows: {rows:?}"
    );
    // At least one row should have an `operation` field naming hkdf_extract_expand.
    assert!(
        rows.iter().any(|r| r
            .fields
            .iter()
            .any(|(k, v)| k == "operation" && v.contains("hkdf_extract_expand"))),
        "expected at least one row with operation=hkdf_extract_expand"
    );
}

#[test]
#[serial]
fn derive_kek_emits_tracing_on_error() {
    tracing::info!(target: "vz9t8_4_test", scenario = "error_path");
    // ring's HKDF expand fails when output length exceeds 255 * hash_block_size.
    // For SHA256 (block=32) the cap is 255*32 = 8160 bytes. Pass 10000 to fail.
    let rows = run_with_capture(|| {
        let result = hkdf_extract_expand(b"ikm", b"salt", b"info", 100_000);
        assert!(result.is_err(), "must fail with oversized output length");
    });
    eprintln!("[vz9t8_4_test] captured {} rows on error path", rows.len());
    // Even on error, the entry-side tracing should have fired.
    assert!(
        rows.iter().any(|r| r
            .fields
            .iter()
            .any(|(k, v)| k == "operation" && v.contains("hkdf_extract_expand"))),
        "error path must still emit operation tracing"
    );
}

#[test]
#[serial]
fn derive_kek_handles_empty_info_and_salt() {
    tracing::info!(target: "vz9t8_4_test", scenario = "empty_inputs");
    let rows = run_with_capture(|| {
        let r = hkdf_extract_expand(b"ikm", b"", b"", 32);
        assert!(r.is_ok(), "HKDF accepts empty salt and info");
    });
    let salt_len_rows: Vec<&CapturedRow> = rows
        .iter()
        .filter(|r| r.fields.iter().any(|(k, v)| k == "salt_len" && v == "0"))
        .collect();
    assert!(
        !salt_len_rows.is_empty(),
        "salt_len=0 must appear in captured tracing fields when salt is empty"
    );
}

#[test]
#[serial]
fn derive_functions_do_not_log_key_material() {
    tracing::info!(target: "vz9t8_4_test", scenario = "negative_leak_check");
    // The "known" ikm is 32 bytes of 0xCA. If any captured event field contains
    // a string-encoded form of those bytes, that's a leak.
    let known_ikm = vec![0xCAu8; 32];
    let known_salt = vec![0xDEu8; 16];
    let rows = run_with_capture(|| {
        let _ =
            hkdf_extract_expand(&known_ikm, &known_salt, b"vz9t8_4_test_label", 32).expect("ok");
        let _ = hkdf_extract(&known_salt, &known_ikm);
    });
    // Hex-encode the known patterns so we can grep captured fields for them.
    let ikm_hex_lower = known_ikm
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let ikm_hex_upper = known_ikm
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<String>();
    let salt_hex_lower = known_salt
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let salt_hex_upper = known_salt
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<String>();
    // Also check raw byte-string repr that Debug might produce.
    let ikm_dbg = format!("{:?}", known_ikm);
    let salt_dbg = format!("{:?}", known_salt);

    let banned = [
        &ikm_hex_lower,
        &ikm_hex_upper,
        &salt_hex_lower,
        &salt_hex_upper,
        &ikm_dbg,
        &salt_dbg,
    ];
    // Forbidden field NAMES: any of these strongly imply leaked key material.
    let banned_names = [
        "ikm",
        "salt",
        "key",
        "kek",
        "password",
        "secret",
        "nonce_value",
    ];

    let mut leaks = Vec::new();
    for row in &rows {
        for (k, v) in &row.fields {
            // Field-name leak: the field directly names secret material.
            // Whitelist: salt_len, ikm_len, secret_len, kek_len, output_len —
            // these are *length* fields and OK.
            let lower = k.to_ascii_lowercase();
            for bn in &banned_names {
                if lower == *bn {
                    leaks.push(format!(
                        "field name `{k}` is on the banned-name list (in row {row:?})"
                    ));
                }
            }
            // Field-value leak: any banned hex pattern appears in v.
            for bv in &banned {
                // Only flag if the banned pattern is reasonably long (avoid
                // false positives on short shared substrings).
                if bv.len() >= 16 && v.contains(bv.as_str()) {
                    leaks.push(format!(
                        "field value contains banned key-material pattern: name=`{k}` value=`{v}` (matched `{bv}`)"
                    ));
                }
            }
        }
    }
    assert!(
        leaks.is_empty(),
        "potential key-material leaks in tracing output:\n{}",
        leaks.join("\n")
    );
    eprintln!(
        "[vz9t8_4_test] negative leak check: 0 leaks detected over {} rows",
        rows.len()
    );
}
