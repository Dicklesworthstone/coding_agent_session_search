//! Fuzz target for ingestion-time secret redaction.
//!
//! Exercises the public text/JSON redaction paths plus the memoized JSON walker
//! used by indexing. Inputs are bounded so regex and JSON recursion bugs surface
//! without turning the harness into an OOM test.

#![no_main]

use arbitrary::{Arbitrary, Result as ArbitraryResult, Unstructured};
use coding_agent_search::indexer::redact_secrets::{
    fuzz_redact_json_with_memoizing_redactor, redact_json, redact_text,
};
use libfuzzer_sys::fuzz_target;
use serde_json::{Map, Number, Value};

const MAX_BYTES: usize = 64 * 1024;
const MAX_STRING_BYTES: usize = 16 * 1024;
const MAX_ARRAY_ITEMS: usize = 32;
const MAX_OBJECT_ITEMS: usize = 24;

#[derive(Debug)]
struct JsonPair {
    key: String,
    value: JsonScalar,
}

#[derive(Debug)]
enum JsonScalar {
    Null,
    Bool(bool),
    Signed(i64),
    Unsigned(u64),
    Float(f64),
    String(String),
}

#[derive(Debug)]
struct RedactInput {
    raw_bytes: Vec<u8>,
    text: String,
    object_pairs: Vec<JsonPair>,
    array_items: Vec<JsonScalar>,
    memo_capacity: u16,
}

impl<'a> Arbitrary<'a> for JsonScalar {
    fn arbitrary(u: &mut Unstructured<'a>) -> ArbitraryResult<Self> {
        Ok(match u.int_in_range(0..=5u8)? {
            0 => JsonScalar::Null,
            1 => JsonScalar::Bool(bool::arbitrary(u)?),
            2 => JsonScalar::Signed(i64::arbitrary(u)?),
            3 => JsonScalar::Unsigned(u64::arbitrary(u)?),
            4 => JsonScalar::Float(f64::arbitrary(u)?),
            _ => JsonScalar::String(arbitrary_bounded_string(u, MAX_STRING_BYTES)?),
        })
    }
}

impl<'a> Arbitrary<'a> for RedactInput {
    fn arbitrary(u: &mut Unstructured<'a>) -> ArbitraryResult<Self> {
        let raw_bytes = arbitrary_bounded_bytes(u, MAX_BYTES)?;
        let text = arbitrary_bounded_string(u, MAX_STRING_BYTES)?;

        let object_len = u.int_in_range(0..=MAX_OBJECT_ITEMS)?;
        let mut object_pairs = Vec::with_capacity(object_len);
        for _ in 0..object_len {
            object_pairs.push(JsonPair {
                key: arbitrary_bounded_string(u, MAX_STRING_BYTES)?,
                value: JsonScalar::arbitrary(u)?,
            });
        }

        let array_len = u.int_in_range(0..=MAX_ARRAY_ITEMS)?;
        let mut array_items = Vec::with_capacity(array_len);
        for _ in 0..array_len {
            array_items.push(JsonScalar::arbitrary(u)?);
        }

        Ok(RedactInput {
            raw_bytes,
            text,
            object_pairs,
            array_items,
            memo_capacity: u16::arbitrary(u)?,
        })
    }
}

fn arbitrary_bounded_bytes(u: &mut Unstructured<'_>, max_bytes: usize) -> ArbitraryResult<Vec<u8>> {
    let len = u.int_in_range(0..=max_bytes.min(u.len()))?;
    Ok(u.bytes(len)?.to_vec())
}

fn arbitrary_bounded_string(u: &mut Unstructured<'_>, max_bytes: usize) -> ArbitraryResult<String> {
    let bytes = arbitrary_bounded_bytes(u, max_bytes)?;
    Ok(bounded_string(
        String::from_utf8_lossy(&bytes).into_owned(),
        max_bytes,
    ))
}

fn bounded_string(mut value: String, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value
}

fn bounded_bytes(bytes: &[u8]) -> &[u8] {
    if bytes.len() > MAX_BYTES {
        &bytes[..MAX_BYTES]
    } else {
        bytes
    }
}

fn scalar_to_value(scalar: JsonScalar) -> Value {
    match scalar {
        JsonScalar::Null => Value::Null,
        JsonScalar::Bool(value) => Value::Bool(value),
        JsonScalar::Signed(value) => Value::Number(Number::from(value)),
        JsonScalar::Unsigned(value) => Value::Number(Number::from(value)),
        JsonScalar::Float(value) => Number::from_f64(value)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        JsonScalar::String(value) => Value::String(bounded_string(value, MAX_STRING_BYTES)),
    }
}

fn structured_value(input: RedactInput) -> Value {
    let mut object = Map::new();
    for pair in input.object_pairs.into_iter().take(MAX_OBJECT_ITEMS) {
        object.insert(
            bounded_string(pair.key, MAX_STRING_BYTES),
            scalar_to_value(pair.value),
        );
    }
    let array = input
        .array_items
        .into_iter()
        .take(MAX_ARRAY_ITEMS)
        .map(scalar_to_value)
        .collect();
    object.insert("array".to_string(), Value::Array(array));
    object.insert(
        "text".to_string(),
        Value::String(bounded_string(input.text, MAX_STRING_BYTES)),
    );
    Value::Object(object)
}

fn exercise_json(value: &Value, capacity: usize) {
    let uncached = redact_json(value);
    let memoized = fuzz_redact_json_with_memoizing_redactor(value, capacity);
    assert_eq!(
        uncached, memoized,
        "memoized redaction must match the direct redaction path"
    );
    let _ = serde_json::to_vec(&uncached);
}

fuzz_target!(|input: RedactInput| {
    let raw = bounded_bytes(&input.raw_bytes);
    let capacity = usize::from(input.memo_capacity).clamp(1, 1024);

    if let Ok(text) = std::str::from_utf8(raw) {
        let _ = redact_text(text);
        exercise_json(&Value::String(text.to_string()), capacity);
    } else {
        let lossy = String::from_utf8_lossy(raw);
        let _ = redact_text(&lossy);
        exercise_json(&Value::String(lossy.into_owned()), capacity);
    }

    if let Ok(parsed) = serde_json::from_slice::<Value>(raw) {
        exercise_json(&parsed, capacity);
    }

    let value = structured_value(input);
    exercise_json(&value, capacity);
});
