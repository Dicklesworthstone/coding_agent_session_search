//! Golden regression test for semantic prep memo trace output.
//!
//! Bead `[ibuuh.34-golden]`: freeze the trace-level memo audit/window
//! contract so future field drift on semantic prep hit/miss logs fails
//! loudly instead of silently changing downstream observability.
//!
//! Regenerate with:
//! `UPDATE_GOLDENS=1 cargo test --test golden_memo_trace`

use anyhow::{Context as _, Result, anyhow, bail};
use coding_agent_search::indexer::semantic::{EmbeddingInput, SemanticIndexer};
use serde_json::{Map, Value, json};
use serial_test::serial;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::Registry;
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

mod util;

#[derive(Clone, Default)]
struct TraceEventCollector {
    events: Arc<Mutex<Vec<Value>>>,
}

impl TraceEventCollector {
    fn snapshot(&self) -> Vec<Value> {
        self.events.lock().expect("trace collector lock").clone()
    }
}

impl<S> Layer<S> for TraceEventCollector
where
    S: Subscriber,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = JsonVisitor::default();
        event.record(&mut visitor);

        let message = visitor
            .fields
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !message.starts_with("semantic prep memo cache") {
            return;
        }

        let mut record = Map::new();
        record.insert(
            "level".to_string(),
            Value::String(event.metadata().level().as_str().to_string()),
        );
        record.extend(visitor.fields);
        self.events
            .lock()
            .expect("trace collector lock")
            .push(Value::Object(record));
    }
}

#[derive(Default)]
struct JsonVisitor {
    fields: Map<String, Value>,
}

impl Visit for JsonVisitor {
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields
            .insert(field.name().to_string(), Value::Bool(value));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields
            .insert(field.name().to_string(), Value::Number(value.into()));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields
            .insert(field.name().to_string(), Value::Number(value.into()));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        let number = serde_json::Number::from_f64(value)
            .expect("trace field should not serialize NaN or infinity");
        self.fields
            .insert(field.name().to_string(), Value::Number(number));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields
            .insert(field.name().to_string(), Value::String(value.to_string()));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.fields.insert(
            field.name().to_string(),
            Value::String(format!("{value:?}")),
        );
    }
}

fn sort_json(value: Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.into_iter().map(sort_json).collect()),
        Value::Object(map) => {
            let mut sorted = std::collections::BTreeMap::new();
            for (key, value) in map {
                sorted.insert(key, sort_json(value));
            }
            Value::Object(sorted.into_iter().collect())
        }
        other => other,
    }
}

fn assert_golden(name: &str, actual: &str) -> Result<()> {
    let golden_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join(name);

    if std::env::var("UPDATE_GOLDENS").is_ok() {
        let parent = golden_path.parent().ok_or_else(|| {
            anyhow!(
                "golden path should have a parent: {}",
                golden_path.display()
            )
        })?;
        std::fs::create_dir_all(parent).context("create golden parent")?;
        std::fs::write(&golden_path, actual).context("write golden")?;
        eprintln!("[GOLDEN] Updated: {}", golden_path.display());
        return Ok(());
    }

    let expected = std::fs::read_to_string(&golden_path).map_err(|err| {
        anyhow!(
            "Golden file missing or unreadable: {}\n{err}\n\n\
             Run with UPDATE_GOLDENS=1 to create it, then review and commit:\n\
             \tUPDATE_GOLDENS=1 cargo test --test golden_memo_trace\n\
             \tgit diff tests/golden/\n\
             \tgit add tests/golden/",
            golden_path.display(),
        )
    });
    let expected = expected?;

    if actual != expected {
        let actual_path = golden_path.with_extension("actual");
        std::fs::write(&actual_path, actual).context("write .actual file")?;
        bail!(
            "GOLDEN MISMATCH: {name}\n\n\
             Expected: {}\n\
             Actual:   {}\n\n\
             diff the two files, then either fix the code or regenerate with:\n\
             \tUPDATE_GOLDENS=1 cargo test --test golden_memo_trace",
            golden_path.display(),
            actual_path.display(),
        );
    }

    Ok(())
}

fn capture_memo_trace_json() -> Result<String> {
    let collector = TraceEventCollector::default();
    let subscriber = Registry::default().with(collector.clone());

    let _parallel_guard = util::EnvGuard::set("CASS_SEMANTIC_PREP_PARALLEL", "0");
    let _capacity_guard = util::EnvGuard::set("CASS_SEMANTIC_PREP_MEMO_CAPACITY", "4");

    let indexer = SemanticIndexer::new("hash", None)
        .context("hash indexer")?
        .with_batch_size(1)
        .context("batch size")?;
    let inputs = vec![
        EmbeddingInput::new(1, "alpha repeat"),
        EmbeddingInput::new(2, "alpha repeat"),
        EmbeddingInput::new(3, ""),
        EmbeddingInput::new(4, "beta unique"),
        EmbeddingInput::new(5, "alpha repeat"),
    ];

    tracing::subscriber::with_default(subscriber, || {
        let embedded = indexer.embed_messages(&inputs).context("embed inputs")?;
        assert_eq!(embedded.len(), 4, "empty canonical should be skipped");
        Ok::<(), anyhow::Error>(())
    })?;

    let canonical = json!({
        "events": collector.snapshot(),
    });
    serde_json::to_string_pretty(&sort_json(canonical)).context("pretty-print memo trace JSON")
}

#[test]
#[serial]
fn semantic_prep_memo_trace_matches_golden() -> Result<()> {
    let actual = capture_memo_trace_json()?;
    assert_golden("log/memo_trace.json.golden", &actual)
}
