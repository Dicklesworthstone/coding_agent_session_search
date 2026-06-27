//! INV-cass-29 — `cass introspect --json` envelope completeness.
//!
//! Extends INV-cass-28's structural-integrity coverage of
//! `introspect.commands[]` to the other top-level catalogs that agents
//! consume from the same envelope:
//!
//!   - `global_flags[]` — the global-options catalog (db, color,
//!     data-dir, etc.) every subcommand inherits;
//!   - `response_schemas` — the JSON-schema catalog agents pull when
//!     building strongly-typed response parsers;
//!   - `api_version` + `contract_version` — the version coordination
//!     points that should match the dedicated `cass api-version --json`
//!     subcommand's emission.
//!
//! Three invariants:
//!
//!   1. Every `global_flags[]` entry has the required keys
//!      (`name`, `description`, `arg_type`, `required`) with non-empty
//!      name + description. The global-flag catalog mirrors the per-
//!      command argument catalog INV-cass-28 already guards.
//!   2. `response_schemas` is a non-empty JSON object and every value
//!      is itself a JSON object. Agents pulling per-subcommand schemas
//!      key into this map; a regression that returned a string or null
//!      value for any key would silently break schema-driven parsing.
//!   3. Introspect's `api_version` and `contract_version` exactly
//!      equal the dedicated `cass api-version --json` subcommand's
//!      emission. INV-cass-15 already locks the api-version subcommand's
//!      values against `Cargo.toml`; this cross-check ensures
//!      introspect is not silently drifting from that source-of-truth.

use std::cmp::Ordering;
use std::error::Error;

use assert_cmd::Command;
use serde_json::Value;

type TestResult = Result<(), Box<dyn Error>>;

fn test_error(message: impl Into<String>) -> Box<dyn Error> {
    std::io::Error::other(message.into()).into()
}

fn ensure(condition: bool, message: impl Into<String>) -> TestResult {
    if condition {
        Ok(())
    } else {
        Err(test_error(message))
    }
}

fn run_cass_json(args: &[&str]) -> Result<Value, Box<dyn Error>> {
    let output = Command::cargo_bin("cass")?
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .args(["--color=never"])
        .args(args)
        .output()?;
    let code = output
        .status
        .code()
        .ok_or_else(|| test_error("cass killed by signal"))?;
    if !matches!(code.cmp(&0), Ordering::Equal) {
        return Err(test_error(format!(
            "cass exited {code}; stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn require_flag_key(idx: usize, key: &str, entry: &Value) -> TestResult {
    ensure(
        entry.get(key).is_some(),
        format!("global_flags[{idx}] missing required key `{key}`: {entry}"),
    )
}

fn check_global_flag(idx: usize, entry: &Value) -> TestResult {
    for key in ["name", "description", "arg_type", "required"] {
        require_flag_key(idx, key, entry)?;
    }
    let name = entry
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let description = entry
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default();
    ensure(
        !name.is_empty(),
        format!("global_flags[{idx}].name is empty: {entry}"),
    )?;
    ensure(
        !description.trim().is_empty(),
        format!(
            "global_flags[{idx}={name:?}].description is empty — agents pinning help-text \
             generation on introspect would silently emit blank text for this flag"
        ),
    )?;
    Ok(())
}

fn check_schema_value_is_object(key: &str, value: &Value) -> TestResult {
    ensure(
        value.is_object(),
        format!(
            "response_schemas[{key:?}] must be a JSON object (schema); got: {value} \
             (type: {})",
            value_type(value)
        ),
    )
}

fn value_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[test]
fn global_flags_entries_have_required_keys_with_nonempty_strings() -> TestResult {
    let envelope = run_cass_json(&["introspect", "--json"])?;
    let flags = envelope
        .get("global_flags")
        .and_then(Value::as_array)
        .ok_or_else(|| test_error("introspect.global_flags missing or not an array"))?;
    ensure(
        !matches!(flags.len().cmp(&3), Ordering::Less),
        format!(
            "introspect.global_flags should have >= 3 entries; got {} — likely a \
             regression in catalog emission entirely",
            flags.len()
        ),
    )?;
    for (idx, entry) in flags.iter().enumerate() {
        check_global_flag(idx, entry)?;
    }
    Ok(())
}

#[test]
fn response_schemas_is_nonempty_object_with_object_values() -> TestResult {
    let envelope = run_cass_json(&["introspect", "--json"])?;
    let schemas = envelope
        .get("response_schemas")
        .and_then(Value::as_object)
        .ok_or_else(|| test_error("introspect.response_schemas missing or not an object"))?;
    ensure(
        !matches!(schemas.len().cmp(&3), Ordering::Less),
        format!(
            "introspect.response_schemas should have >= 3 entries; got {}",
            schemas.len()
        ),
    )?;
    for (key, value) in schemas {
        check_schema_value_is_object(key, value)?;
    }
    Ok(())
}

#[test]
fn introspect_version_fields_match_api_version_subcommand() -> TestResult {
    let introspect = run_cass_json(&["introspect", "--json"])?;
    let api_version = run_cass_json(&["api-version", "--json"])?;

    let introspect_api = introspect
        .get("api_version")
        .ok_or_else(|| test_error("introspect missing api_version field"))?;
    let subcommand_api = api_version
        .get("api_version")
        .ok_or_else(|| test_error("api-version subcommand missing api_version field"))?;
    let i_api_int = introspect_api.as_i64().ok_or_else(|| {
        test_error(format!(
            "introspect.api_version must be an integer; got: {introspect_api}"
        ))
    })?;
    let s_api_int = subcommand_api.as_i64().ok_or_else(|| {
        test_error(format!(
            "api-version.api_version must be an integer; got: {subcommand_api}"
        ))
    })?;
    ensure(
        matches!(i_api_int.cmp(&s_api_int), Ordering::Equal),
        format!(
            "introspect.api_version ({i_api_int}) does not match \
             api-version.api_version ({s_api_int}) — introspect is silently drifting \
             from the dedicated api-version subcommand's source-of-truth"
        ),
    )?;

    let introspect_contract = introspect
        .get("contract_version")
        .and_then(Value::as_str)
        .ok_or_else(|| test_error("introspect missing string contract_version"))?;
    let subcommand_contract = api_version
        .get("contract_version")
        .and_then(Value::as_str)
        .ok_or_else(|| test_error("api-version subcommand missing string contract_version"))?;
    ensure(
        introspect_contract == subcommand_contract,
        format!(
            "introspect.contract_version ({introspect_contract:?}) does not match \
             api-version.contract_version ({subcommand_contract:?})"
        ),
    )?;
    Ok(())
}
