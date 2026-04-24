//! Fuzz corpus replay golden test.
//!
//! Replays every seed in fuzz/corpus/fuzz_cli_argv/ through the same
//! structured-argv → parse_cli pipeline as the fuzz target, asserting
//! none panic. This is a cargo-test-driven regression guard: if any
//! corpus seed causes a panic, this test catches it without needing
//! cargo-fuzz installed.
//!
//! The test also snapshots the corpus size so additions/removals show
//! as an explicit golden diff.
//!
//! Regenerate:
//!   UPDATE_GOLDENS=1 cargo test --test golden_fuzz_corpus

use arbitrary::{Arbitrary, Unstructured};
use coding_agent_search::parse_cli;
use std::path::PathBuf;

const SUBCOMMANDS: &[&str] = &[
    "search", "find", "query", "q", "stats", "ls", "list", "index", "diag", "status", "doctor",
    "daemon", "analytics", "robot-docs", "introspect", "api-version", "models", "detect", "pages",
    "bakeoff", "setup",
];

const LONG_FLAGS: &[&str] = &[
    "robot",
    "json",
    "limit",
    "offset",
    "agent",
    "workspace",
    "fields",
    "max-tokens",
    "request-id",
    "cursor",
    "since",
    "until",
    "days",
    "today",
    "yesterday",
    "week",
    "full",
    "watch",
    "data-dir",
    "verbose",
    "quiet",
    "color",
    "progress",
    "wrap",
    "nowrap",
    "db",
    "trace-file",
    "robot-format",
    "robot-meta",
    "mode",
    "approximate",
];

const MAX_ARGV_LEN: usize = 32;
const MAX_STRING_BYTES: usize = 256;

#[derive(Arbitrary, Debug)]
enum DashStyle {
    None,
    Single,
    Double,
}

#[derive(Arbitrary, Debug)]
enum CasePerturbation {
    Lower,
    Upper,
    Mixed,
}

#[derive(Arbitrary, Debug)]
struct FlagToken {
    flag_index: u8,
    dash_style: DashStyle,
    case: CasePerturbation,
    value_style: ValueStyle,
    value: String,
}

#[derive(Arbitrary, Debug)]
enum ValueStyle {
    None,
    Inline,
    SeparateSlot,
}

#[derive(Arbitrary, Debug)]
enum ArgKind {
    Subcommand(u8),
    Flag(FlagToken),
    Positional(String),
}

#[derive(Arbitrary, Debug)]
struct ArgvInput {
    args: Vec<ArgKind>,
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

fn mangle_case(flag: &str, case: &CasePerturbation) -> String {
    match case {
        CasePerturbation::Lower => flag.to_ascii_lowercase(),
        CasePerturbation::Upper => flag.to_ascii_uppercase(),
        CasePerturbation::Mixed => flag
            .chars()
            .enumerate()
            .map(|(i, c)| {
                if i % 2 == 0 {
                    c.to_ascii_uppercase()
                } else {
                    c.to_ascii_lowercase()
                }
            })
            .collect(),
    }
}

fn prefix(dash: &DashStyle) -> &'static str {
    match dash {
        DashStyle::None => "",
        DashStyle::Single => "-",
        DashStyle::Double => "--",
    }
}

fn build_argv(input: ArgvInput) -> Vec<String> {
    let mut argv: Vec<String> = Vec::with_capacity(MAX_ARGV_LEN + 1);
    argv.push("cass".to_string());

    for arg in input.args.into_iter().take(MAX_ARGV_LEN) {
        match arg {
            ArgKind::Subcommand(idx) => {
                let name = SUBCOMMANDS[(idx as usize) % SUBCOMMANDS.len()];
                argv.push(name.to_string());
            }
            ArgKind::Flag(tok) => {
                let flag = LONG_FLAGS[(tok.flag_index as usize) % LONG_FLAGS.len()];
                let cased = mangle_case(flag, &tok.case);
                let prefix_str = prefix(&tok.dash_style);
                let full_flag = format!("{prefix_str}{cased}");

                let value = bounded_string(tok.value, MAX_STRING_BYTES);
                match tok.value_style {
                    ValueStyle::None => argv.push(full_flag),
                    ValueStyle::Inline => argv.push(format!("{full_flag}={value}")),
                    ValueStyle::SeparateSlot => {
                        argv.push(full_flag);
                        argv.push(value);
                    }
                }
            }
            ArgKind::Positional(s) => {
                argv.push(bounded_string(s, MAX_STRING_BYTES));
            }
        }
    }

    argv
}

fn contains_help_or_version(argv: &[String]) -> bool {
    argv.iter().any(|a| {
        let t = a.to_ascii_lowercase();
        matches!(
            t.as_str(),
            "--help" | "-h" | "help" | "--version" | "-v" | "-V"
        ) || t.contains("help")
            || t.contains("version")
    })
}

fn corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fuzz")
        .join("corpus")
        .join("fuzz_cli_argv")
}

fn golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join("fuzz_corpus_stats.json.golden")
}

#[test]
fn fuzz_corpus_replay_no_panics() {
    let dir = corpus_dir();
    if !dir.exists() {
        eprintln!("Fuzz corpus dir not found: {}", dir.display());
        return;
    }

    let mut replayed = 0usize;
    let mut skipped_help = 0usize;
    let mut skipped_deserialize = 0usize;
    let mut parse_ok = 0usize;
    let mut parse_err = 0usize;

    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .expect("read corpus dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|ft| ft.is_file()).unwrap_or(false))
        .collect();
    entries.sort_by_key(|e| e.file_name());

    for entry in &entries {
        let data = match std::fs::read(entry.path()) {
            Ok(d) => d,
            Err(_) => continue,
        };

        let mut u = Unstructured::new(&data);
        let input: ArgvInput = match ArgvInput::arbitrary(&mut u) {
            Ok(i) => i,
            Err(_) => {
                skipped_deserialize += 1;
                continue;
            }
        };

        let argv = build_argv(input);
        if argv.len() > MAX_ARGV_LEN + 1 {
            skipped_deserialize += 1;
            continue;
        }
        if contains_help_or_version(&argv) {
            skipped_help += 1;
            continue;
        }

        replayed += 1;
        match parse_cli(argv) {
            Ok(_) => parse_ok += 1,
            Err(_) => parse_err += 1,
        }
    }

    assert!(
        replayed > 0,
        "No corpus seeds were successfully replayed from {}",
        dir.display()
    );

    let snapshot = serde_json::json!({
        "corpus_dir": "fuzz/corpus/fuzz_cli_argv",
        "total_seeds": entries.len(),
        "replayed": replayed,
        "skipped_help_version": skipped_help,
        "skipped_deserialize": skipped_deserialize,
        "parse_ok": parse_ok,
        "parse_err": parse_err,
    });

    let golden = golden_path();
    if std::env::var("UPDATE_GOLDENS").is_ok() {
        std::fs::create_dir_all(golden.parent().unwrap()).expect("create golden dir");
        std::fs::write(&golden, serde_json::to_string_pretty(&snapshot).unwrap())
            .expect("write golden");
        eprintln!("[GOLDEN] Updated: {}", golden.display());
        return;
    }

    if let Ok(expected) = std::fs::read_to_string(&golden) {
        let expected_json: serde_json::Value =
            serde_json::from_str(&expected).expect("parse golden");

        if expected_json["total_seeds"] != snapshot["total_seeds"] {
            panic!(
                "Fuzz corpus size changed: expected {} seeds, got {}.\n\
                 Regenerate: UPDATE_GOLDENS=1 cargo test --test golden_fuzz_corpus",
                expected_json["total_seeds"], snapshot["total_seeds"]
            );
        }
    }

    eprintln!(
        "Fuzz corpus replay: {replayed} replayed, {parse_ok} ok, {parse_err} err, \
         {skipped_help} help-skipped, {skipped_deserialize} deserialize-skipped \
         (of {} seeds)",
        entries.len()
    );
}

#[test]
fn fuzz_corpus_seed_count_golden() {
    let dir = corpus_dir();
    if !dir.exists() {
        eprintln!("Fuzz corpus dir not found: {}", dir.display());
        return;
    }

    let count = std::fs::read_dir(&dir)
        .expect("read corpus dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|ft| ft.is_file()).unwrap_or(false))
        .count();

    let golden = golden_path();
    if std::env::var("UPDATE_GOLDENS").is_ok() {
        return; // handled by fuzz_corpus_replay_no_panics
    }

    if let Ok(expected) = std::fs::read_to_string(&golden) {
        let expected_json: serde_json::Value =
            serde_json::from_str(&expected).expect("parse golden");
        let expected_count = expected_json["total_seeds"].as_u64().unwrap_or(0) as usize;
        assert_eq!(
            count, expected_count,
            "Fuzz corpus seed count changed: expected {expected_count}, got {count}.\n\
             If intentional, run: UPDATE_GOLDENS=1 cargo test --test golden_fuzz_corpus"
        );
    }
}
