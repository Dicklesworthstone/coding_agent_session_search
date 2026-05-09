//! Scanner that asserts shell scripts under scripts/ route cargo invocations
//! through rch and avoid the `set -e` + `((VAR++))` arithmetic-abort bash idiom.
//!
//! Per `coding_agent_session_search-tafss`. Subsumes
//! `coding_agent_session_search-iaor8` (closed as superseded).
//!
//! ## Compliance rule 1: rch-wrap all cargo invocations
//!
//! Bare `cargo build|test|bench|clippy|run|check|fmt|update|install` outside
//! of comments/strings is a violation UNLESS one of these holds:
//!   - The line is preceded (in the same logical command) by `rch exec --`.
//!   - The invocation appears inside a `run_cargo()` function body or via
//!     `$RCH_BIN exec --` substitution.
//!   - The line defines a function named `*cargo*` (definition, not call).
//!
//! ## Compliance rule 2: avoid `set -e + ((VAR++))`
//!
//! Bash's `((VAR++))` evaluates to 0 when VAR is initially 0; under `set -e`,
//! the shell aborts. This caused the zlzpk bug. The scanner flags any
//! `((VAR++))` or `((VAR--))` pattern in a script that also has `set -e` (or
//! `set -euo pipefail`) earlier in the file.
//!
//! ## Logging
//!
//! Every match emits `tracing::info!(target: "scripts_rch_compliance", ...)`
//! so failure context lands in the test harness output.

use std::path::{Path, PathBuf};

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn list_shell_scripts(roots: &[&str]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for root in roots {
        let root_path = project_root().join(root);
        if !root_path.is_dir() {
            continue;
        }
        walk_dir(&root_path, &mut out);
    }
    out
}

fn walk_dir(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // Skip hidden dirs (.git, etc.) and known-noise dirs.
        if let Some(name) = path.file_name().and_then(|n| n.to_str())
            && (name.starts_with('.') || name == "target" || name == "node_modules")
        {
            continue;
        }
        if path.is_dir() {
            walk_dir(&path, out);
        } else if path.extension().and_then(|s| s.to_str()) == Some("sh") {
            out.push(path);
        }
    }
}

/// Strip trailing comment from a line (after the first unquoted `#`).
/// This is a heuristic — full bash parsing is out of scope; we mostly need
/// to ignore "# cargo ..." which is overwhelming the dominant FP.
fn strip_trailing_comment(line: &str) -> &str {
    // Single-quoted and double-quoted regions hide #s; we punt on those and
    // accept the FP risk. The conservative approach: split on " # " (space-#)
    // which excludes the one common case where # appears in identifiers.
    if let Some(idx) = line.find(" # ") {
        return &line[..idx];
    }
    if line.trim_start().starts_with('#') {
        return "";
    }
    line
}

/// Join logical bash lines that span multiple physical lines via trailing
/// `\` continuations. Returns a Vec of (first-physical-line-1-indexed,
/// joined-content). The first-physical-line index lets findings point at the
/// start of the logical command in error messages.
fn logical_lines(body: &str) -> Vec<(usize, String)> {
    let mut out: Vec<(usize, String)> = Vec::new();
    let mut current = String::new();
    let mut current_start: Option<usize> = None;
    for (idx, raw) in body.lines().enumerate() {
        let line_no = idx + 1;
        if current_start.is_none() {
            current_start = Some(line_no);
        }
        // A trailing backslash means the next line is a continuation of this
        // command. We drop the backslash and join with a single space so
        // tokens stay separated.
        if let Some(stripped) = raw.strip_suffix('\\') {
            current.push_str(stripped);
            current.push(' ');
            continue;
        }
        current.push_str(raw);
        out.push((
            current_start.unwrap_or(line_no),
            std::mem::take(&mut current),
        ));
        current_start = None;
    }
    if !current.is_empty()
        && let Some(start) = current_start
    {
        out.push((start, current));
    }
    out
}

/// Returns the byte indices of `cargo` matches that lie OUTSIDE any
/// single- or double-quoted span on the line. This catches `echo "cargo
/// install ..."` and `log INFO "rch cargo test"` style false positives. It
/// is intentionally bash-naive: nested quotes, $(...) interpolation, and
/// heredocs are not modeled — these have rare-enough collisions with cargo
/// invocations that we accept the residual risk.
fn cargo_spans_outside_quotes(line: &str) -> Vec<(usize, usize)> {
    let bytes = line.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    while i < bytes.len() {
        let c = bytes[i];
        if !in_single && !in_double && bytes[i..].starts_with(b"cargo")
            // Boundary check before "cargo"
            && (i == 0 || !is_word_byte(bytes[i - 1]))
        {
            // Boundary check after "cargo" — we want the literal word.
            let end = i + 5;
            if end == bytes.len() || !is_word_byte(bytes[end]) {
                spans.push((i, end));
                i = end;
                continue;
            }
        }
        match c {
            b'\\' => {
                // Skip the next byte regardless of state (escaped char).
                i += 2;
                continue;
            }
            b'\'' if !in_double => in_single = !in_single,
            b'"' if !in_single => in_double = !in_double,
            _ => {}
        }
        i += 1;
    }
    spans
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

#[derive(Debug, Clone)]
struct Finding {
    path: PathBuf,
    line: usize,
    snippet: String,
    rule: &'static str,
    hint: &'static str,
}

fn scan_for_bare_cargo(path: &Path) -> Vec<Finding> {
    let body = match std::fs::read_to_string(path) {
        Ok(b) => b,
        Err(_) => return Vec::new(),
    };
    let cargo_subcmd_re =
        regex::Regex::new(r"\bcargo\s+(build|test|bench|clippy|run|check|fmt|update|install)\b")
            .expect("regex compiles");
    let mut findings = Vec::new();
    for (start_line, logical) in logical_lines(&body) {
        let line = strip_trailing_comment(&logical);
        if !cargo_subcmd_re.is_match(line) {
            continue;
        }
        // Filter out cargo occurrences inside quoted strings — they are echo
        // text, log messages, --help text, etc., not real invocations.
        let cargo_positions: Vec<(usize, usize)> = cargo_spans_outside_quotes(line);
        if cargo_positions.is_empty() {
            continue;
        }
        // For each unquoted `cargo` token, confirm the cargo_subcmd_re actually
        // matches starting at that position; if all matches were inside
        // quotes, treat the line as clean.
        let mut has_real_match = false;
        for (start, _end) in &cargo_positions {
            let tail = &line[*start..];
            if cargo_subcmd_re.is_match(tail) {
                has_real_match = true;
                break;
            }
        }
        if !has_real_match {
            continue;
        }
        // Skip lines that ARE the rch-wrapped form. The wrapped form contains
        // `rch exec --` or runs through `run_cargo` / `$RCH_BIN exec --`.
        let lower = line.to_ascii_lowercase();
        if lower.contains("rch exec --")
            || lower.contains("$rch_bin exec --")
            || lower.contains("\"$rch_bin\" exec --")
            || lower.contains("${rch_bin} exec --")
            // Definition site of run_cargo
            || lower.contains("run_cargo()")
            || lower.contains("function run_cargo")
            || lower.contains("run_cargo ()")
            // Call to run_cargo (the helper IS the wrap)
            || lower.contains("run_cargo ")
        {
            continue;
        }
        // Trim a snippet for reporting; truncate excessively long joined
        // logical lines so panics stay legible.
        let mut snippet = logical.trim().to_string();
        if snippet.len() > 200 {
            snippet.truncate(197);
            snippet.push_str("...");
        }
        findings.push(Finding {
            path: path.to_path_buf(),
            line: start_line,
            snippet,
            rule: "bare_cargo_invocation",
            hint: "wrap via `rch exec -- env CARGO_TARGET_DIR=... cargo ...` or `source scripts/lib/run_cargo.sh && run_cargo ...`",
        });
    }
    findings
}

fn scan_for_set_e_arithmetic(path: &Path) -> Vec<Finding> {
    let body = match std::fs::read_to_string(path) {
        Ok(b) => b,
        Err(_) => return Vec::new(),
    };
    // Match `set -<flags>` where <flags> contains an `e`. This catches
    // `-e`, `-eu`, `-eo`, `-ex`, `-eux`, `-euo`, `-euxo`, `-eou`, etc.
    let set_e_re = regex::Regex::new(r"set\s+-[a-z]*e[a-z]*\b").expect("regex compiles");
    let arith_re = regex::Regex::new(r"\(\(\s*\w+(\+\+|--)\s*\)\)").expect("regex compiles");
    let mut set_e_line: Option<usize> = None;
    let mut findings = Vec::new();
    for (idx, raw) in body.lines().enumerate() {
        let line = strip_trailing_comment(raw);
        if set_e_line.is_none() && set_e_re.is_match(line) {
            set_e_line = Some(idx);
        }
        if set_e_line.is_some() && arith_re.is_match(line) {
            findings.push(Finding {
                path: path.to_path_buf(),
                line: idx + 1,
                snippet: raw.trim().to_string(),
                rule: "set_e_arithmetic_abort",
                hint: "use `((VAR += 1))` or `((VAR++)) || true` — `((VAR++))` evaluates to 0 when VAR was 0, which `set -e` treats as failure",
            });
        }
    }
    findings
}

#[test]
fn scripts_rch_compliance_no_bare_cargo() {
    tracing::info!(target: "scripts_rch_compliance", check = "bare_cargo");
    let scripts = list_shell_scripts(&["scripts"]);
    let mut all_findings = Vec::new();
    for s in &scripts {
        let findings = scan_for_bare_cargo(s);
        for f in &findings {
            tracing::info!(
                target: "scripts_rch_compliance",
                file = %f.path.display(),
                line = f.line,
                snippet = %f.snippet,
                rule = f.rule,
                verdict = "violating"
            );
        }
        all_findings.extend(findings);
    }
    if !all_findings.is_empty() {
        let mut msg = format!(
            "scripts_rch_compliance found {} bare-cargo violation(s):\n",
            all_findings.len()
        );
        for f in &all_findings {
            msg.push_str(&format!(
                "  {}:{}\n    snippet: {}\n    fix: {}\n",
                f.path.display(),
                f.line,
                f.snippet,
                f.hint
            ));
        }
        panic!("{msg}");
    }
}

#[test]
fn scripts_rch_compliance_no_set_e_arithmetic() {
    tracing::info!(target: "scripts_rch_compliance", check = "set_e_arithmetic");
    let scripts = list_shell_scripts(&["scripts"]);
    let mut all_findings = Vec::new();
    for s in &scripts {
        let findings = scan_for_set_e_arithmetic(s);
        for f in &findings {
            tracing::info!(
                target: "scripts_rch_compliance",
                file = %f.path.display(),
                line = f.line,
                snippet = %f.snippet,
                rule = f.rule,
                verdict = "violating"
            );
        }
        all_findings.extend(findings);
    }
    if !all_findings.is_empty() {
        let mut msg = format!(
            "scripts_rch_compliance found {} set-e arithmetic abort risk(s):\n",
            all_findings.len()
        );
        for f in &all_findings {
            msg.push_str(&format!(
                "  {}:{}\n    snippet: {}\n    fix: {}\n",
                f.path.display(),
                f.line,
                f.snippet,
                f.hint
            ));
        }
        panic!("{msg}");
    }
}

#[test]
fn scripts_rch_compliance_helper_module_exists() {
    tracing::info!(target: "scripts_rch_compliance", check = "helper_present");
    let helper = project_root()
        .join("scripts")
        .join("lib")
        .join("run_cargo.sh");
    assert!(
        helper.is_file(),
        "scripts/lib/run_cargo.sh (the run_cargo helper) must exist; missing"
    );
    let body = std::fs::read_to_string(&helper).expect("readable");
    assert!(
        body.contains("run_cargo()") && body.contains("rch") && body.contains("CARGO_TARGET_DIR"),
        "scripts/lib/run_cargo.sh must define run_cargo(), reference rch, and use CARGO_TARGET_DIR"
    );
}

// ---------------- Synthetic-fixture tests for scanner correctness ----------------

#[test]
fn scanner_flags_bare_cargo_in_synthetic_fixture() {
    tracing::info!(target: "scripts_rch_compliance", check = "synthetic_violating");
    let tmp = tempdir_for_test("rch_compliance_violating");
    let path = tmp.join("violating.sh");
    std::fs::write(
        &path,
        b"#!/usr/bin/env bash\nset -e\ncargo build  # bare cargo, should violate\n",
    )
    .unwrap();
    let findings = scan_for_bare_cargo(&path);
    assert_eq!(
        findings.len(),
        1,
        "synthetic violating script must yield exactly 1 finding; got {}",
        findings.len()
    );
    assert!(findings[0].snippet.contains("cargo build"));
}

#[test]
fn scanner_does_not_flag_run_cargo_calls() {
    tracing::info!(target: "scripts_rch_compliance", check = "synthetic_clean");
    let tmp = tempdir_for_test("rch_compliance_clean");
    let path = tmp.join("clean.sh");
    std::fs::write(
        &path,
        b"#!/usr/bin/env bash\nset -euo pipefail\nsource ./run_cargo.sh\nrun_cargo build --release\nrch exec -- env CARGO_TARGET_DIR=/tmp cargo test --release\n",
    )
    .unwrap();
    let findings = scan_for_bare_cargo(&path);
    assert!(
        findings.is_empty(),
        "clean script should yield zero findings; got {findings:?}"
    );
}

#[test]
fn scanner_flags_set_e_with_increment() {
    tracing::info!(target: "scripts_rch_compliance", check = "synthetic_set_e_arith");
    let tmp = tempdir_for_test("rch_compliance_set_e");
    let path = tmp.join("violating.sh");
    std::fs::write(
        &path,
        b"#!/usr/bin/env bash\nset -e\nCOUNT=0\n((COUNT++))\necho ok\n",
    )
    .unwrap();
    let findings = scan_for_set_e_arithmetic(&path);
    assert_eq!(findings.len(), 1, "expected 1 set-e arithmetic finding");
    assert!(findings[0].snippet.contains("((COUNT++))"));
}

#[test]
fn scanner_does_not_flag_increment_without_set_e() {
    tracing::info!(target: "scripts_rch_compliance", check = "synthetic_no_set_e");
    let tmp = tempdir_for_test("rch_compliance_no_set_e");
    let path = tmp.join("safe.sh");
    std::fs::write(
        &path,
        b"#!/usr/bin/env bash\n# no set -e here\nCOUNT=0\n((COUNT++))\necho ok\n",
    )
    .unwrap();
    let findings = scan_for_set_e_arithmetic(&path);
    assert!(
        findings.is_empty(),
        "without set -e, the increment is safe; got {findings:?}"
    );
}

#[test]
fn scanner_does_not_flag_safe_increment_form() {
    tracing::info!(target: "scripts_rch_compliance", check = "synthetic_safe_inc");
    let tmp = tempdir_for_test("rch_compliance_safe_inc");
    let path = tmp.join("safe.sh");
    std::fs::write(
        &path,
        b"#!/usr/bin/env bash\nset -e\nCOUNT=0\n((COUNT += 1))\necho ok\n",
    )
    .unwrap();
    let findings = scan_for_set_e_arithmetic(&path);
    assert!(
        findings.is_empty(),
        "((VAR += 1)) is the safe form and must not trigger; got {findings:?}"
    );
}

#[test]
fn scanner_ignores_cargo_in_comments() {
    tracing::info!(target: "scripts_rch_compliance", check = "synthetic_comment_immune");
    let tmp = tempdir_for_test("rch_compliance_comment");
    let path = tmp.join("with_comment.sh");
    std::fs::write(
        &path,
        b"#!/usr/bin/env bash\n# Don't use cargo build directly - wrap via run_cargo.\nrun_cargo build --release\n",
    )
    .unwrap();
    let findings = scan_for_bare_cargo(&path);
    assert!(
        findings.is_empty(),
        "cargo mentioned in a leading-# comment must not trigger; got {findings:?}"
    );
}

#[test]
fn scanner_ignores_cargo_in_double_quoted_strings() {
    tracing::info!(target: "scripts_rch_compliance", check = "synthetic_double_quote_immune");
    let tmp = tempdir_for_test("rch_compliance_dq");
    let path = tmp.join("dq.sh");
    std::fs::write(
        &path,
        b"#!/usr/bin/env bash\nset -e\necho \"  cargo install cargo-llvm-cov\"\nlog INFO \"using rch cargo test\"\n",
    )
    .unwrap();
    let findings = scan_for_bare_cargo(&path);
    assert!(
        findings.is_empty(),
        "cargo inside double-quoted strings must not trigger; got {findings:?}"
    );
}

#[test]
fn scanner_ignores_cargo_in_single_quoted_strings() {
    tracing::info!(target: "scripts_rch_compliance", check = "synthetic_single_quote_immune");
    let tmp = tempdir_for_test("rch_compliance_sq");
    let path = tmp.join("sq.sh");
    std::fs::write(
        &path,
        b"#!/usr/bin/env bash\nset -e\necho 'rch cargo test'\n",
    )
    .unwrap();
    let findings = scan_for_bare_cargo(&path);
    assert!(
        findings.is_empty(),
        "cargo inside single-quoted strings must not trigger; got {findings:?}"
    );
}

#[test]
fn scanner_handles_line_continuations_for_rch() {
    tracing::info!(target: "scripts_rch_compliance", check = "synthetic_continuation");
    let tmp = tempdir_for_test("rch_compliance_cont");
    let path = tmp.join("cont.sh");
    std::fs::write(
        &path,
        b"#!/usr/bin/env bash\nset -euo pipefail\nrch exec -- env CARGO_TARGET_DIR=/tmp \\\n    cargo test --test foo -- --nocapture\n",
    )
    .unwrap();
    let findings = scan_for_bare_cargo(&path);
    assert!(
        findings.is_empty(),
        "cargo following an `rch exec --` continuation line must not trigger; got {findings:?}"
    );
}

#[test]
fn scanner_set_e_regex_catches_eo_pipefail() {
    tracing::info!(target: "scripts_rch_compliance", check = "synthetic_eo_pipefail");
    let tmp = tempdir_for_test("rch_compliance_eo");
    let path = tmp.join("eo.sh");
    // -eo is a real form used in the cass tree (e2e_logging_acceptance_test.sh).
    // Combined with ((VAR++)) it triggers the abort risk.
    std::fs::write(
        &path,
        b"#!/usr/bin/env bash\nset -eo pipefail\nCOUNT=0\n((COUNT++))\necho ok\n",
    )
    .unwrap();
    let findings = scan_for_set_e_arithmetic(&path);
    assert_eq!(
        findings.len(),
        1,
        "scanner must catch ((VAR++)) under `set -eo pipefail`; got {findings:?}"
    );
}

#[test]
fn scanner_set_e_regex_catches_eux_combinations() {
    tracing::info!(target: "scripts_rch_compliance", check = "synthetic_eux");
    let tmp = tempdir_for_test("rch_compliance_eux");
    let path = tmp.join("eux.sh");
    std::fs::write(
        &path,
        b"#!/usr/bin/env bash\nset -eux\nCOUNT=0\n((COUNT++))\n",
    )
    .unwrap();
    let findings = scan_for_set_e_arithmetic(&path);
    assert_eq!(findings.len(), 1, "scanner must catch `set -eux`");
}

#[test]
fn scanner_set_e_regex_does_not_match_uo_only() {
    // `set -uo pipefail` (no `e`) must NOT trigger the gate.
    let tmp = tempdir_for_test("rch_compliance_uo");
    let path = tmp.join("uo.sh");
    std::fs::write(
        &path,
        b"#!/usr/bin/env bash\nset -uo pipefail\nCOUNT=0\n((COUNT++))\n",
    )
    .unwrap();
    let findings = scan_for_set_e_arithmetic(&path);
    assert!(
        findings.is_empty(),
        "without `-e` flag the gate must stay quiet; got {findings:?}"
    );
}

fn tempdir_for_test(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("cass-tafss-{label}-{nanos}"));
    std::fs::create_dir_all(&dir).expect("tempdir create");
    dir
}
