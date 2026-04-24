//! Golden-file test for the error envelope kind taxonomy.
//!
//! Ensures every `kind: "..."` literal in src/lib.rs is:
//!   1. Strictly kebab-case (no underscores)
//!   2. Present in the canonical golden file
//!   3. No stale entries exist in the golden that aren't in source
//!
//! Regenerate:
//!   UPDATE_GOLDENS=1 cargo test --test golden_error_envelope
//!
//! Then review:
//!   git diff tests/golden/robot/error_envelope_kinds.json.golden

use std::collections::BTreeMap;
use std::path::PathBuf;

fn golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join("robot")
        .join("error_envelope_kinds.json.golden")
}

fn lib_rs_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("lib.rs")
}

fn extract_kind_literals() -> BTreeMap<String, Vec<usize>> {
    let source = std::fs::read_to_string(lib_rs_path()).expect("read src/lib.rs");
    let re = regex::Regex::new(r#"kind:\s*"([a-zA-Z][a-zA-Z0-9_-]*)""#).unwrap();

    let mut kinds: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (line_no, line) in source.lines().enumerate() {
        if let Some(cap) = re.captures(line) {
            let kind = cap[1].to_string();
            kinds.entry(kind).or_default().push(line_no + 1);
        }
    }
    kinds
}

fn extract_kind_exit_codes() -> BTreeMap<String, Vec<i32>> {
    let source = std::fs::read_to_string(lib_rs_path()).expect("read src/lib.rs");
    let kind_re = regex::Regex::new(r#"kind:\s*"([a-zA-Z][a-zA-Z0-9_-]*)""#).unwrap();
    let code_re = regex::Regex::new(r"code:\s*(\d+)").unwrap();
    let lines: Vec<&str> = source.lines().collect();

    let mut kind_codes: BTreeMap<String, std::collections::BTreeSet<i32>> = BTreeMap::new();

    for (i, line) in lines.iter().enumerate() {
        if let Some(cap) = kind_re.captures(line) {
            let kind = cap[1].to_string();
            // Look backwards up to 10 lines for code: N
            for candidate in lines.iter().take(i + 1).skip(i.saturating_sub(10)) {
                if let Some(cm) = code_re.captures(candidate) {
                    let code: i32 = cm[1].parse().unwrap();
                    kind_codes.entry(kind.clone()).or_default().insert(code);
                }
            }
        }
    }

    kind_codes
        .into_iter()
        .map(|(k, v)| (k, v.into_iter().collect()))
        .collect()
}

fn build_golden_json(kinds: &BTreeMap<String, Vec<i32>>) -> serde_json::Value {
    let mut kinds_obj = serde_json::Map::new();
    for (kind, codes) in kinds {
        kinds_obj.insert(kind.clone(), serde_json::json!({ "exit_codes": codes }));
    }

    serde_json::json!({
        "_meta": {
            "description": "Canonical error kind taxonomy for cass robot-mode error envelopes",
            "rule": "All err.kind values MUST be kebab-case per AGENTS.md",
            "total_kinds": kinds.len(),
            "regenerate": "UPDATE_GOLDENS=1 cargo test --test golden_error_envelope",
        },
        "kinds": kinds_obj,
    })
}

#[test]
fn error_kinds_are_strictly_kebab_case() {
    let kinds = extract_kind_literals();
    let mut violations = Vec::new();

    for (kind, lines) in &kinds {
        if kind.contains('_') {
            violations.push(format!(
                "  {kind} (lines: {lines:?}) — contains underscore, should be: {}",
                kind.replace('_', "-")
            ));
        }
    }

    assert!(
        violations.is_empty(),
        "Snake_case err.kind values found in src/lib.rs:\n{}\n\n\
         All err.kind values must be kebab-case per AGENTS.md.",
        violations.join("\n")
    );
}

#[test]
fn error_kinds_golden_coverage() {
    let source_kinds = extract_kind_exit_codes();
    let golden = golden_path();

    if std::env::var("UPDATE_GOLDENS").is_ok() {
        let json = build_golden_json(&source_kinds);
        std::fs::create_dir_all(golden.parent().unwrap()).expect("create golden dir");
        std::fs::write(&golden, serde_json::to_string_pretty(&json).unwrap())
            .expect("write golden");
        eprintln!("[GOLDEN] Updated: {}", golden.display());
        return;
    }

    let golden_content = std::fs::read_to_string(&golden).unwrap_or_else(|err| {
        panic!(
            "Golden file missing: {}\n{err}\n\n\
             Run: UPDATE_GOLDENS=1 cargo test --test golden_error_envelope",
            golden.display(),
        )
    });

    let golden_json: serde_json::Value =
        serde_json::from_str(&golden_content).expect("parse golden JSON");
    let golden_kinds = golden_json["kinds"].as_object().expect("kinds object");

    let mut missing_from_golden = Vec::new();
    let mut stale_in_golden = Vec::new();

    for kind in source_kinds.keys() {
        if !golden_kinds.contains_key(kind) {
            missing_from_golden.push(kind.as_str());
        }
    }

    for kind in golden_kinds.keys() {
        if !source_kinds.contains_key(kind) {
            stale_in_golden.push(kind.as_str());
        }
    }

    let mut errors = Vec::new();
    if !missing_from_golden.is_empty() {
        errors.push(format!(
            "Kinds in src/lib.rs but NOT in golden ({}):\n  {}",
            missing_from_golden.len(),
            missing_from_golden.join(", ")
        ));
    }
    if !stale_in_golden.is_empty() {
        errors.push(format!(
            "Kinds in golden but NOT in src/lib.rs ({}):\n  {}",
            stale_in_golden.len(),
            stale_in_golden.join(", ")
        ));
    }

    assert!(
        errors.is_empty(),
        "Error envelope golden drift detected:\n{}\n\n\
         Regenerate: UPDATE_GOLDENS=1 cargo test --test golden_error_envelope",
        errors.join("\n\n")
    );

    // Also verify the counts match
    assert_eq!(
        source_kinds.len(),
        golden_kinds.len(),
        "Kind count mismatch: source={}, golden={}",
        source_kinds.len(),
        golden_kinds.len()
    );
}

#[test]
fn error_kinds_exit_codes_match_golden() {
    let source_kinds = extract_kind_exit_codes();
    let golden = golden_path();

    if std::env::var("UPDATE_GOLDENS").is_ok() {
        return; // handled by error_kinds_golden_coverage
    }

    let golden_content = std::fs::read_to_string(&golden).unwrap_or_else(|err| {
        panic!(
            "Golden file missing: {}\n{err}\n\n\
             Run: UPDATE_GOLDENS=1 cargo test --test golden_error_envelope",
            golden.display(),
        )
    });

    let golden_json: serde_json::Value =
        serde_json::from_str(&golden_content).expect("parse golden JSON");
    let golden_kinds = golden_json["kinds"].as_object().expect("kinds object");

    let mut mismatches = Vec::new();
    for (kind, source_codes) in &source_kinds {
        if let Some(golden_entry) = golden_kinds.get(kind) {
            let golden_codes: Vec<i32> = golden_entry["exit_codes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_i64().unwrap() as i32)
                .collect();

            if *source_codes != golden_codes {
                mismatches.push(format!(
                    "  {kind}: source={source_codes:?} golden={golden_codes:?}"
                ));
            }
        }
    }

    assert!(
        mismatches.is_empty(),
        "Exit code mismatches between source and golden:\n{}\n\n\
         Regenerate: UPDATE_GOLDENS=1 cargo test --test golden_error_envelope",
        mismatches.join("\n")
    );
}
