#!/usr/bin/env python3
"""Generate a temporary exact-source GH473 retry/error-chain engine probe.

This is NOT the CASS persistence, canonical-ledger, or integrated-index gate.
The full locked CASS tests remain independently required. No repository source,
manifest, lockfile, or archive is modified by this script.
"""
from __future__ import annotations

import argparse
import hashlib
import re
from pathlib import Path


def definition(text: str, name: str) -> str:
    pattern = r"^( *)(?:pub(?:\([^)]*\))? )?fn " + re.escape(name) + r"\b"
    matches = list(re.finditer(pattern, text, re.MULTILINE))
    if len(matches) != 1:
        raise ValueError(f"Missing or ambiguous production definition: {name}")
    match = matches[0]
    end = re.search("^" + match[1] + r"}$", text[match.end():], re.MULTILINE)
    if end is None:
        raise ValueError(f"Unterminated production definition: {name}")
    return text[match.start():match.end() + end.end()] + "\n"


def generate(root: Path, destination: Path) -> None:
    source = (root / "src/indexer/mod.rs").read_text()
    writer = definition(source, "with_ephemeral_writer")
    primary = writer[writer.index("if storage.bulk_single_connection_enabled()"):writer.index("let (writer, reusable)")]
    ephemeral = writer[writer.index("let (writer, reusable)"):]
    assert primary.index("apply_index_writer_busy_timeout(storage)") < primary.index('.execute("UPDATE meta')
    assert ephemeral.index("apply_index_writer_busy_timeout(&writer)") < ephemeral.index('.execute("UPDATE meta')
    error_blocks = re.findall(r"return Err\(anyhow::Error::new\(err\)\.context\(format!\([\s\S]*?\)\)\);", ephemeral)
    if len(error_blocks) != 1 or "ephemeral writer preflight write failed" not in error_blocks[0]:
        raise ValueError("Missing unique typed production preflight error expression")
    sql = re.findall(r'\.execute\("(UPDATE meta[^"\n]+)"\)', ephemeral)
    if len(sql) != 1:
        raise ValueError("Missing unique production preflight UPDATE")
    definitions = "\n".join(definition(source, name) for name in [
        "index_writer_busy_timeout_ms", "transient_franken_error",
        "is_retryable_franken_error", "with_concurrent_retry",
    ])
    # Copy the functions and error expression unchanged. Do not mock storage or
    # claim to exercise its cache, atomic transaction, or ledger lifecycle.
    program = '''#![allow(dead_code, unused_imports)]
mod franken_sync;
mod production {
use anyhow::{Result, anyhow};
use crate::franken_sync::{Connection, FrankenError};
use crate::franken_sync::compat::RowExt;
use rand::prelude::*;
use std::path::Path;
use std::time::{Duration, Instant};
''' + definitions + '''
fn preflight_error(err: FrankenError, context: &str, db_path: &Path) -> Result<()> {
''' + error_blocks[0] + '''
}
const PREFLIGHT_SQL: &str = "''' + sql[0] + '''";
fn attempt(writer: &Connection, path: &Path) -> Result<()> {
    match writer.execute(PREFLIGHT_SQL) {
        Ok(_) => Ok(()),
        Err(err) => preflight_error(err, "GH473 extracted core probe", path),
    }
}
pub fn verify(expected: u64) -> Result<()> {
    assert_eq!(index_writer_busy_timeout_ms(), expected);
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("core.db");
    let holder = Connection::open(path.to_string_lossy().into_owned())?;
    holder.execute("PRAGMA journal_mode = WAL")?;
    holder.execute("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)")?;
    holder.execute("INSERT INTO meta VALUES ('schema_version', '1')")?;
    let writer = Connection::open_existing_schema_only(path.to_string_lossy().into_owned())?;
    writer.execute(&format!("PRAGMA busy_timeout = {}", index_writer_busy_timeout_ms()))?;
    assert_eq!(writer.query_row("PRAGMA busy_timeout")?.get_typed::<i64>(0)?, expected as i64);
    let started = Instant::now();
    holder.execute("BEGIN IMMEDIATE")?;
    let mut attempts = 0;
    with_concurrent_retry(2, || {
        attempts += 1;
        let result = attempt(&writer, &path);
        if attempts == 1 {
            let err = result.as_ref().expect_err("held writer must cause REAL contention");
            assert!(is_retryable_franken_error(err), "typed preflight error lost: {err:#}");
            holder.execute("ROLLBACK")?;
        }
        result
    })?;
    assert_eq!(attempts, 2);
    holder.execute("BEGIN IMMEDIATE")?;
    let mut exhausted_attempts = 0;
    let exhausted = with_concurrent_retry(2, || {
        exhausted_attempts += 1;
        attempt(&writer, &path)
    });
    let err = exhausted.expect_err("exhaustion must propagate the actual engine error");
    assert!(is_retryable_franken_error(&err));
    assert_eq!(exhausted_attempts, 3);
    holder.execute("ROLLBACK")?;
    let mut permanent_attempts = 0;
    let permanent = with_concurrent_retry(2, || {
        permanent_attempts += 1;
        let err = writer.execute("UPDATE missing_gh473_table SET value = 1").unwrap_err();
        preflight_error(err, "GH473 permanent error", &path)
    });
    let err = permanent.expect_err("permanent engine errors must not become success");
    assert!(!is_retryable_franken_error(&err));
    assert_eq!(permanent_attempts, 1);
    assert!(started.elapsed() < Duration::from_secs(5), "short waits regressed");
    writer.close_without_checkpoint()?;
    holder.close()?;
    println!("GH473_CORE_OK policy_ms={expected} released_attempts={attempts} exhausted_attempts={exhausted_attempts} permanent_attempts={permanent_attempts}");
    Ok(())
}
}
fn main() -> anyhow::Result<()> {
    let expected = std::env::args().nth(1).expect("expected timeout argument").parse::<u64>()?;
    production::verify(expected)
}
'''
    manifest_source = (root / "Cargo.toml").read_text()
    dependencies = []
    for name in ["anyhow", "tracing", "asupersync", "frankensqlite", "dotenvy", "tempfile", "rand"]:
        matches = re.findall(r"^" + re.escape(name) + r"\s*=.*$", manifest_source, re.MULTILINE)
        if len(matches) != 1:
            raise ValueError(f"Missing or ambiguous dependency declaration: {name}")
        dependencies.append(matches[0])
    manifest = '[package]\nname = "cass-gh473-core-probe"\nversion = "0.0.0"\nedition = "2024"\n[dependencies]\n'
    manifest += "\n".join(dependencies) + '\n[profile.dev]\ndebug = 0\n'
    destination.mkdir(parents=True, exist_ok=False)
    (destination / "src").mkdir()
    (destination / "Cargo.toml").write_text(manifest)
    (destination / "src/main.rs").write_text(program)
    facade = (root / "src/franken_sync.rs").read_bytes()
    (destination / "src/franken_sync.rs").write_bytes(facade)
    (destination / "rust-toolchain.toml").write_bytes((root / "rust-toolchain.toml").read_bytes())
    for label, contents in [("exact_definitions", definitions.encode()), ("exact_error_expression", error_blocks[0].encode()), ("exact_sync_facade", facade), ("probe", program.encode())]:
        print(label, hashlib.sha256(contents).hexdigest())
    print("SCOPE: engine + exact timeout/retry/error-chain code only; NOT full CASS, cache, atomic ledger, or index validation.")
    print("=== primary-mode activation ===")
    print(definition((root / "src/storage/sqlite.rs").read_text(), "enable_bulk_single_connection"))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("destination", type=Path)
    parser.add_argument("--root", type=Path, default=Path.cwd())
    args = parser.parse_args()
    generate(args.root.resolve(), args.destination.resolve())
