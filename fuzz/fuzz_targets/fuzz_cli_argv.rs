//! Fuzz target for the robot-mode CLI argv parser.
//!
//! Exercises the full argv-parse pipeline — normalize_args (single-dash
//! long flags, case normalization, subcommand aliases, flag-as-subcommand,
//! global flag hoisting), clap's `try_parse_from`, and the heuristic
//! parse recovery fallback.
//!
//! `parse_cli` must never panic on adversarial argv. It should return
//! either Ok(ParsedCli) or Err(CliError) for every input.
//!
//! The harness builds structure-aware argv from a bounded vocabulary —
//! subcommand names, known long flags, arbitrary flag values, and a few
//! positional args — rather than feeding raw bytes. This keeps the
//! corpus representative of real CLI abuse (typos, wrong case, flag
//! hoisting, alias recovery) while covering the normalization +
//! heuristic-recovery code paths that random bytes would rarely reach.

#![no_main]

use arbitrary::Arbitrary;
use coding_agent_search::parse_cli;
use libfuzzer_sys::fuzz_target;

// A small, bounded vocabulary of real subcommand names, including
// aliases recognized by normalize_args.
const SUBCOMMANDS: &[&str] = &[
    "search", "find", "query", "q", "stats", "ls", "list", "index", "diag", "status",
    "doctor", "daemon", "analytics", "robot-docs", "introspect", "api-version", "models",
    "detect", "pages", "bakeoff", "setup",
];

// Known long flags (from normalize_args' KNOWN_LONG_FLAGS). The fuzzer
// picks an index into this table and then mangles the case/dash prefix.
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
    // Whether to include a value in `--flag=value` style, or as the
    // next argv slot via `--flag value`, or leave it flag-only.
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

const MAX_ARGV_LEN: usize = 32;
const MAX_STRING_BYTES: usize = 256;

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
    // clap's DisplayHelp / DisplayVersion branches call `err.exit()`
    // inside parse_cli, which would terminate the fuzzer process.
    // Filter these tokens out so the fuzzer keeps running.
    argv.iter().any(|a| {
        let t = a.to_ascii_lowercase();
        matches!(
            t.as_str(),
            "--help" | "-h" | "help" | "--version" | "-v" | "--Help" | "-V"
        ) || t.contains("help")
            || t.contains("version")
    })
}

fuzz_target!(|input: ArgvInput| {
    let argv = build_argv(input);
    if argv.len() > MAX_ARGV_LEN + 1 {
        return;
    }
    if contains_help_or_version(&argv) {
        return;
    }
    // parse_cli must never panic. It returns Ok(ParsedCli) or
    // Err(CliError); both outcomes are fine, crashes are not.
    let _ = parse_cli(argv);
});
