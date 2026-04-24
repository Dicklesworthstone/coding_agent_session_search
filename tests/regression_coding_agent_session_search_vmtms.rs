//! Regression + metamorphic/property-style coverage for
//! src/ui/time_parser.rs::parse_time_input.
//!
//! The original regression (bead vmtms) pinned that adversarial
//! i64::MAX relative-time inputs never panic. Bead 7qtn5 (this file's
//! later tests) extends that to a property-style contract set:
//!
//! - total function: parse_time_input(ANY &str) never panics, only
//!   returns Option<i64>;
//! - empty / whitespace-only input is None;
//! - case-insensitive keywords (NOW/today/YESTERDAY);
//! - trim-invariance (leading/trailing whitespace doesn't change result);
//! - negative-duration monotonicity for "N days ago" shapes;
//! - equivalent unit spellings ("7d" == "7 days ago" within a tight
//!   tolerance that covers the ~wall-clock drift between two calls).

use coding_agent_search::ui::time_parser::parse_time_input;

#[test]
fn oversized_relative_time_filters_are_rejected_without_panicking() {
    let overflowing_inputs = [
        "9223372036854775807d",
        "-9223372036854775807d",
        "9223372036854775807 days ago",
        "9223372036854775807d ago",
    ];

    for input in overflowing_inputs {
        assert_eq!(parse_time_input(input), None, "input: {input}");
    }
}

/// Property: parse_time_input is TOTAL — every &str input must return
/// Some(i64) or None, never panic. This is the contract the error-
/// handling-in-filters path relies on.
#[test]
fn parse_time_input_is_total_for_adversarial_inputs() {
    let adversarial: &[&str] = &[
        // empty / pure whitespace
        "",
        " ",
        "\t",
        "\n",
        "   \t\n   ",
        // partial unit strings
        "d",
        "-",
        "-d",
        "1",
        "1 ",
        "1  days",
        "1 days", // valid? likely None without "ago"
        "days ago",
        "ago",
        "  ago  ",
        "-1",
        "-1 ",
        "- 1d",
        // unknown units
        "1fortnight",
        "7 centuries ago",
        "42 jiffies",
        // numeric boundaries
        "0",
        "-0",
        "00000",
        "9999999999999999999999999999", // way past i64::MAX
        // trailing/embedded garbage
        "7d extra",
        "7d\0",
        "\0now",
        // non-ASCII / unicode
        "７ｄ", // fullwidth digits/letters (ASCII-only parser should return None)
        "🔥",   // emoji
        "yesterday🕑",
        "−1d", // unicode minus (NOT hyphen-minus)
        // long strings (no panic)
        &"a".repeat(10_000),
        &"-1d".repeat(1_000),
    ];

    for input in adversarial {
        // The real assertion: this call site doesn't panic. The
        // Option is returned unexamined — we're not asserting on the
        // VALUE here, only on crash-resistance.
        let _ = parse_time_input(input);
    }
}

/// Property: empty / whitespace-only input returns None.
#[test]
fn parse_time_input_empty_and_whitespace_returns_none() {
    for empty in ["", " ", "\t", "\n", "\r\n", "   ", "   \t\n   "] {
        assert_eq!(
            parse_time_input(empty),
            None,
            "empty/whitespace input must be None; got Some for {empty:?}"
        );
    }
}

/// Metamorphic: trim-invariance. parse_time_input("  now  ") must
/// equal parse_time_input("now") (modulo wall-clock drift). "now" is
/// the cleanest keyword for this test because the code path returns
/// directly from Utc::now().timestamp_millis() with no midnight
/// rounding.
#[test]
fn parse_time_input_trims_leading_and_trailing_whitespace() {
    // Both calls happen within milliseconds of each other; allow a
    // tight tolerance to cover system-clock drift while still
    // catching a regression that mis-parsed the padded form.
    let tolerance_ms: i64 = 1_000;

    let pairs: &[(&str, &str)] = &[
        ("now", "  now  "),
        ("now", "\tnow\n"),
        ("today", "   today   "),
        ("yesterday", "\nyesterday\n"),
        ("7d", "  7d  "),
        ("-3h", "   -3h  "),
    ];
    for (bare, padded) in pairs {
        let b = parse_time_input(bare).unwrap_or_else(|| {
            panic!("bare {bare:?} must parse (precondition to trim-invariance test)")
        });
        let p = parse_time_input(padded)
            .unwrap_or_else(|| panic!("padded {padded:?} must parse — trim-invariance regression"));
        assert!(
            (b - p).abs() <= tolerance_ms,
            "trim-invariance violated: bare={bare:?}->{b}, padded={padded:?}->{p}, \
             diff={}ms exceeds tolerance {tolerance_ms}ms",
            (b - p).abs()
        );
    }
}

/// Metamorphic: keyword case-insensitivity. parse_time_input lowercases
/// input before keyword matching, so "NOW", "Now", "nOw" must all
/// parse the same as "now".
#[test]
fn parse_time_input_keywords_are_case_insensitive() {
    let tolerance_ms: i64 = 1_000;
    for (canonical, variants) in [
        ("now", &["NOW", "Now", "nOw", "nOW"][..]),
        ("today", &["TODAY", "Today", "tOdAy"][..]),
        ("yesterday", &["YESTERDAY", "Yesterday", "YeStErDaY"][..]),
    ] {
        let c = parse_time_input(canonical)
            .unwrap_or_else(|| panic!("canonical keyword {canonical:?} must parse"));
        for variant in variants {
            let v = parse_time_input(variant).unwrap_or_else(|| {
                panic!(
                    "case-variant keyword {variant:?} must parse — \
                        case-insensitivity regression"
                )
            });
            assert!(
                (c - v).abs() <= tolerance_ms,
                "case-insensitivity violated: {canonical:?}->{c}, {variant:?}->{v}, \
                 diff={}ms",
                (c - v).abs()
            );
        }
    }
}

/// Metamorphic: negative-duration monotonicity. For M > N > 0,
/// parse_time_input("Md") <= parse_time_input("Nd") — going further
/// back in time only produces EARLIER (smaller) timestamps.
#[test]
fn parse_time_input_negative_durations_are_monotonic() {
    let ns: [i64; 6] = [1, 2, 7, 30, 365, 1000];
    let mut parsed: Vec<(i64, i64)> = Vec::new();
    for n in ns {
        let s = format!("{n}d");
        let t = parse_time_input(&s)
            .unwrap_or_else(|| panic!("{s} must parse — precondition to monotonicity"));
        parsed.push((n, t));
    }
    for window in parsed.windows(2) {
        let (small_n, small_ts) = window[0];
        let (large_n, large_ts) = window[1];
        assert!(
            large_ts <= small_ts,
            "monotonicity violated: {large_n}d ({large_ts}) > {small_n}d ({small_ts}); \
             larger N must go further back in time (smaller timestamp)"
        );
    }
}

/// Metamorphic: equivalent unit spellings. parse_time_input("7d"),
/// "7 days ago", and "7d ago" must all map to approximately the same
/// timestamp. The three code paths are separate branches in the
/// parser, so this test pins the cross-branch consistency invariant
/// the docstring advertises.
#[test]
fn parse_time_input_equivalent_unit_spellings_agree() {
    // Both calls happen within milliseconds. Tolerance is loose
    // enough to survive wall-clock drift but tight enough to catch
    // a parser that accidentally applied the wrong unit multiplier.
    let tolerance_ms: i64 = 60 * 1_000;

    let equivalent_groups: &[&[&str]] = &[
        &["7d", "7 days ago", "7d ago"],
        &["24h", "24 hours ago", "24h ago", "1d"],
        &["60m", "60 minutes ago", "60m ago", "1h"],
        &["2w", "2 weeks ago", "14d"],
    ];
    for group in equivalent_groups {
        let parsed: Vec<(String, i64)> = group
            .iter()
            .map(|s| {
                let t = parse_time_input(s).unwrap_or_else(|| {
                    panic!("{s:?} must parse — precondition to equivalence test")
                });
                ((*s).to_string(), t)
            })
            .collect();
        let first = &parsed[0];
        for other in &parsed[1..] {
            assert!(
                (first.1 - other.1).abs() <= tolerance_ms,
                "unit-spelling equivalence violated: {:?}->{} vs {:?}->{}, \
                 diff={}ms exceeds tolerance {tolerance_ms}ms",
                first.0,
                first.1,
                other.0,
                other.1,
                (first.1 - other.1).abs()
            );
        }
    }
}

/// Property: the numeric-fallback heuristic. Seconds (< 10^11) are
/// upscaled to milliseconds; values >= 10^11 are treated as ms.
/// Boundary-probe the heuristic so a refactor that changes the cutoff
/// trips this test instead of silently misinterpreting operator
/// filter values.
#[test]
fn parse_time_input_numeric_heuristic_cutoff_holds() {
    // Clearly-seconds values (< 10^11) get multiplied by 1000.
    let seconds_cases: &[(&str, i64)] = &[
        ("0", 0),
        ("1", 1_000),
        ("1700000000", 1_700_000_000_000),
        // Just below the cutoff.
        ("99999999999", 99_999_999_999_000),
    ];
    for (input, expected) in seconds_cases {
        assert_eq!(
            parse_time_input(input),
            Some(*expected),
            "{input} must be treated as seconds and upscaled to {expected} ms"
        );
    }

    // At/above cutoff, already-ms.
    let ms_cases: &[(&str, i64)] = &[
        ("100000000000", 100_000_000_000),
        ("1700000000000", 1_700_000_000_000),
    ];
    for (input, expected) in ms_cases {
        assert_eq!(
            parse_time_input(input),
            Some(*expected),
            "{input} must be treated as already-ms and returned unchanged"
        );
    }
}
