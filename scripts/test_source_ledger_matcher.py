#!/usr/bin/env python3
"""Compile the exact production filesystem matcher against CASS's dependency pins.

This is a native matcher test/benchmark, NOT connector, storage or CLI coverage.
--watch-components also exercises the exact watch source-stability helper and
receipt serialization buffer with the pinned FAD metadata methods. Its declared
provenance/policy adapters do not implement connector parsing or persistence.
Cargo may prune unused features/edges in this disposable graph, but every retained
package identity, checksum and edge must come from the repository lockfile before
any compilation. Native tests then run with --locked. The repository is read-only.
"""
from __future__ import annotations

import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import signal
import subprocess
import tempfile
import time
import tomllib
from typing import Any

PACKAGE = "cass-source-ledger-matcher"
VERSION = "0.0.0"
DIRECT = ("serde_json", "tempfile")


def identity(package: dict[str, Any]) -> tuple[str, str, str]:
    return package["name"], package["version"], package.get("source", "")


def resolve(spec: str, packages: list[dict[str, Any]]) -> dict[str, Any]:
    parts = spec.split()
    matches = [
        package for package in packages
        if package["name"] == parts[0]
        and (len(parts) < 2 or package["version"] == parts[1])
        and (len(parts) < 3 or package.get("source") == parts[2].strip("()"))
    ]
    if len(matches) != 1 or len(parts) > 3:
        raise RuntimeError(f"ambiguous or missing locked dependency: {spec}")
    return matches[0]


def dependency_closure(
    lock: dict[str, Any], names: tuple[str, ...] = DIRECT,
) -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
    packages = lock["package"]
    root = resolve("coding-agent-search", packages)
    direct = [resolve(next(spec for spec in root["dependencies"] if spec.split()[0] == name),
                      packages) for name in names]
    selected = {}
    pending = list(direct)
    while pending:
        package = pending.pop()
        key = identity(package)
        if key in selected:
            continue
        if not key[2].startswith("registry+") or not package.get("checksum"):
            raise RuntimeError(f"matcher dependency is not checksum-pinned: {key}")
        selected[key] = package
        pending.extend(resolve(spec, packages) for spec in package.get("dependencies", []))
    return direct, list(selected.values())


def validate_normalized_lock(
    lock: dict[str, Any], allowed: list[dict[str, Any]], direct: list[dict[str, Any]],
) -> list[dict[str, Any]]:
    """Permit feature pruning, never a new version, source, checksum or edge."""
    packages = lock["package"]
    observed = {identity(package): package for package in packages}
    if len(observed) != len(packages):
        raise RuntimeError("duplicate package identity in matcher lockfile")
    root = resolve(f"{PACKAGE} {VERSION}", packages)
    if root.get("source") or root.get("checksum"):
        raise RuntimeError("matcher root is not the local harness")
    expected = {identity(package): package for package in allowed}
    root_edges = {identity(resolve(spec, packages)) for spec in root.get("dependencies", [])}
    if root_edges != {identity(package) for package in direct}:
        raise RuntimeError("matcher direct dependencies changed")
    retained = []
    for package in packages:
        if package is root:
            continue
        key = identity(package)
        original = expected.get(key)
        if original is None or package.get("checksum") != original.get("checksum"):
            raise RuntimeError(f"dependency pin drift: {key}")
        old_edges = {identity(resolve(spec, allowed)) for spec in original.get("dependencies", [])}
        new_edges = {identity(resolve(spec, packages)) for spec in package.get("dependencies", [])}
        if not new_edges <= old_edges:
            raise RuntimeError(f"dependency edge drift: {key}")
        retained.append(package)
    return retained


def watch_component_sources(
    root: Path, lock: dict[str, Any], checkout: Path, include_scan: bool = False,
) -> dict[str, Any]:
    """Copy tested logic verbatim; identify every type/policy boundary adapter."""
    inputs = {}
    ranges = []
    outputs = {}

    def read(path: Path, label: str) -> str:
        data = path.read_bytes()
        inputs[label] = {"path": str(path), "sha256": hashlib.sha256(data).hexdigest()}
        return data.decode()

    def piece(text: str, label: str, start: str, end: str | None = None) -> str:
        if text.count(start) != 1:
            raise RuntimeError(f"ambiguous extraction boundary: {label}: {start}")
        first = text.index(start)
        last = text.index(end, first) if end else len(text)
        selected = text[first:last]
        ranges.append({
            "input": label, "start_line": text.count("\n", 0, first) + 1,
            "end_line": text.count("\n", 0, last),
            "start_byte": len(text[:first].encode()), "end_byte": len(text[:last].encode()),
            "sha256": hashlib.sha256(selected.encode()).hexdigest(),
        })
        return selected

    fad = resolve("franken-agent-detection", lock["package"])
    revision = fad["source"].rsplit("#", 1)[-1]
    actual = subprocess.check_output(
        ["git", "rev-parse", "HEAD"], cwd=checkout, timeout=30,
    ).decode().strip()
    if actual != revision:
        raise RuntimeError(f"FAD checkout differs from Cargo.lock: {actual} != {revision}")
    scan_path = checkout / "src/connectors/scan.rs"
    scan = read(scan_path, "pinned-fad:src/connectors/scan.rs")
    if subprocess.check_output(["git", "show", f"{revision}:src/connectors/scan.rs"],
                               cwd=checkout, timeout=30) != scan.encode():
        raise RuntimeError("pinned FAD source has local modifications")
    outputs["src/fad.rs"] = (
        "use crate::connectors::{Origin, Platform, ScanRoot};\n"
        "use std::path::PathBuf;\nuse std::time::UNIX_EPOCH;\n"
        + piece(scan, "pinned-fad:src/connectors/scan.rs",
                "/// Connector-declared role for a file", "/// Completion record for one")
    )

    observation_path = root / "src/connectors/source_dependencies/observation.rs"
    observation = read(observation_path, "cass:src/connectors/source_dependencies/observation.rs")
    outputs["src/connectors/source_dependencies/observation.rs"] = observation
    windows = root / "src/connectors/source_dependencies/observation/windows.rs"
    outputs["src/connectors/source_dependencies/observation/windows.rs"] = read(
        windows, "cass:src/connectors/source_dependencies/observation/windows.rs")
    stability_path = root / "src/indexer/watch_source_stability.rs"
    stability = read(stability_path, "cass:src/indexer/watch_source_stability.rs")
    outputs["src/indexer/watch_source_stability.rs"] = stability
    ledger = read(root / "src/indexer/watch_source_ledger.rs", "cass:src/indexer/watch_source_ledger.rs")
    entry_limit = re.search(r"(?m)^const MAX_RECEIPT_ENTRY_BYTES: usize = .+;\n", ledger)
    if entry_limit is None:
        raise RuntimeError("missing production receipt entry byte limit")
    boundary_marker = '\n#[cfg(test)]\n#[path = "watch_source_ledger_tests.rs"]'
    receipt_start = ledger.index("#[derive(Default)]\nstruct ReceiptBuffer")
    receipt_end = boundary_marker if ledger.find(boundary_marker, receipt_start) >= 0 else None
    outputs["src/receipt_buffer.rs"] = (
        piece(ledger, "cass:src/indexer/watch_source_ledger.rs",
              entry_limit.group(), "const MAX_REUSED_PATH_BYTES:")
        + piece(ledger, "cass:src/indexer/watch_source_ledger.rs",
                "#[derive(Default)]\nstruct ReceiptBuffer", receipt_end)
    )
    policy = read(root / "src/connectors/source_dependencies.rs",
                  "cass:src/connectors/source_dependencies.rs")
    outputs["src/policy.rs"] = piece(
        policy, "cass:src/connectors/source_dependencies.rs",
        "/// Whether discovery of a new sibling", "pub(crate) struct ConnectorRegistration",
    )
    indexer = read(root / "src/indexer/mod.rs", "cass:src/indexer/mod.rs")
    observation_helper = piece(
        indexer, "cass:src/indexer/mod.rs", "fn source_file_observation(",
        "fn source_ledger_matches_with_contract(",
    )
    folder_helper = piece(indexer, "cass:src/indexer/mod.rs", "fn source_folder_is_dependency(",
                          "fn source_parent_observation(")
    # These carriers only satisfy the copied FAD constructor's provenance
    # fields. They perform no filesystem I/O and are not parser substitutes.
    outputs["src/lib.rs"] = """#![allow(dead_code)]
mod fad;
pub mod connectors {
    pub use crate::fad::{DiscoveredSourceFile, DiscoveredSourceRole};
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Origin;
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Platform;
    pub struct ScanRoot {
        pub path: std::path::PathBuf,
        pub origin: Origin,
        pub platform: Option<Platform>,
    }
    impl ScanRoot {
        pub fn local(path: std::path::PathBuf) -> Self {
            Self { path, origin: Origin, platform: None }
        }
    }
    pub mod claude_code { pub struct ClaudeCodeConnector; }
    pub mod codex { pub struct CodexConnector; }
    pub mod codebuff { pub struct CodebuffConnector; }
    pub(crate) mod source_dependencies {
        include!("policy.rs");
        pub(crate) mod observation;
        // Adapter dispatches only fixture names to the exact capability
        // declarations above; the application's full factory is not built.
        pub(crate) fn source_dependency_policy(slug: &str) -> SourceDependencyPolicy {
            match slug {
                "claude" | "claude_code" =>
                    <super::claude_code::ClaudeCodeConnector as SourceDependencyCapability>::SOURCE_DEPENDENCIES,
                "codex" =>
                    <super::codex::CodexConnector as SourceDependencyCapability>::SOURCE_DEPENDENCIES,
                "codebuff" =>
                    <super::codebuff::CodebuffConnector as SourceDependencyCapability>::SOURCE_DEPENDENCIES,
                _ => SourceDependencyPolicy::default(),
            }
        }
    }
}
mod indexer {
    use crate::connectors::DiscoveredSourceFile;
    use std::path::Path;
""" + observation_helper + folder_helper + """
    mod watch_source_stability;
    mod watch_source_ledger { include!("receipt_buffer.rs"); }
}
"""
    selected = re.findall(r"(?m)^    fn (gh538_\w+)\(", stability)
    selected.extend(re.findall(r"(?m)^    fn (receipt_\w+)\(", outputs["src/receipt_buffer.rs"]))
    projections = {
        "provenance_carriers": "Minimal Origin, Platform, ScanRoot/local fields for the verbatim FAD constructor",
        "policy_dispatch": "Fixture names map to verbatim CASS capability declarations; full connector registry excluded",
        "receipt_scope": "Verbatim MAX_RECEIPT_ENTRY_BYTES, ReceiptBuffer Write implementation, and both unit tests only",
    }
    exclusions = [
        "Full CASS compilation and application tests",
        "Connector parsing, discovery, notifications and source-hook integration",
        "WatchSourceLedger completion map, admission, memory accounting and receipt/debt transactions",
        "Canonical database, lexical publication, searchability, restart and exactly-once integration",
        "Full connector registry and native macOS qualification",
    ]
    if include_scan:
        def fad_read(relative: str) -> str:
            data = read(checkout / relative, "pinned-fad:" + relative)
            if subprocess.check_output(["git", "show", f"{revision}:{relative}"],
                                       cwd=checkout, timeout=30) != data.encode():
                raise RuntimeError(f"pinned FAD source has local modifications: {relative}")
            return data

        types = fad_read("src/types.rs")
        outputs["src/fad_types.rs"] = piece(
            types, "pinned-fad:src/types.rs", "//! Normalized types",
            '#[cfg(all(test, feature = "connectors"))]',
        )
        fad_connector = fad_read("src/connectors/mod.rs")
        outputs["src/fad.rs"] = (
            "use crate::connectors::{Origin, Platform, ScanRoot};\n"
            "use crate::fad_types::{DetectionResult, NormalizedConversation};\n"
            "use std::path::PathBuf;\nuse std::time::UNIX_EPOCH;\nuse std::sync::Arc;\n"
            + piece(scan, "pinned-fad:src/connectors/scan.rs",
                    '/// A cheap "still alive" tick', "#[cfg(test)]\nmod tests {")
            + piece(fad_connector, "pinned-fad:src/connectors/mod.rs",
                    "pub trait Connector {", "#[cfg(test)]\npub(crate) fn assert_discovery_covers_scan_sources")
        )
        storage = read(root / "src/storage/sqlite.rs", "cass:src/storage/sqlite.rs")
        entry = piece(storage, "cass:src/storage/sqlite.rs", "pub struct SourceIngestLedgerEntry {",
                      "/// Whether a replayed conversation's workspace")
        source_helpers = piece(
            indexer, "cass:src/indexer/mod.rs", "fn source_ledger_key(",
            "/// Keep only the final bounded batch",
        )
        workspace_policy = piece(
            indexer, "cass:src/indexer/mod.rs", "#[derive(Debug, Clone, Default)]\nstruct WorkspaceExclusions",
            "const WORKSPACE_EXCLUSION_SCAN_PREFIX:",
        )
        exclusion_check = piece(
            indexer, "cass:src/indexer/mod.rs", "fn scan_path_exclusions_value_active(",
            "fn active_session_source_skip_observed(",
        )
        # The two SQL methods are absent, not replaced with fake transactions.
        scan_core = piece(
            ledger, "cass:src/indexer/watch_source_ledger.rs", "const MAX_RECEIPT_ENTRIES:",
            "    /// An earlier publication receipt",
        ) + "}\n"
        tail = piece(ledger, "cass:src/indexer/watch_source_ledger.rs",
                     "fn observation_has_watch_publication(", receipt_end)
        outputs["src/indexer/watch_source_ledger.rs"] = (
            "use super::*;\nuse std::cell::{Cell, RefCell};\n"
            "use crate::storage::sqlite::SourceIngestLedgerEntry;\n" + scan_core + tail
            + '\n#[cfg(test)]\n#[path = "watch_source_ledger_tests.rs"]\nmod boundary_tests;\n'
        )
        boundary = read(root / "src/indexer/watch_source_ledger_tests.rs",
                        "cass:src/indexer/watch_source_ledger_tests.rs")
        if boundary.count("use serial_test::serial;\n") != 1 or boundary.count("#[serial]\n") != 2:
            raise RuntimeError("boundary serial-test projection changed; review before qualification")
        outputs["src/indexer/watch_source_ledger_tests.rs"] = boundary.replace(
            "use serial_test::serial;\n", "").replace("#[serial]\n", "")
        selected.extend(re.findall(r"(?m)^fn (gh538_\w+)\(", boundary))
        # Reuse the identical fixture capability declarations/dispatch from
        # the smaller component, but replace provenance carriers with the
        # actual FAD types and expose its exact trait and source hooks.
        base = outputs["src/lib.rs"]
        policies = base[base.index("    pub mod claude_code"):base.index("\nmod indexer {")]
        policies = policies.replace(
            "        pub(crate) mod observation;",
            "        pub(crate) mod observation;\n"
            "        pub(crate) fn can_reuse_with_path_exclusions(_: &str, _: &str) -> bool {\n"
            '            panic!("excluded-source policy is outside this fixture");\n'
            "        }",
        )
        outputs["src/lib.rs"] = """#![allow(dead_code)]
extern crate self as franken_agent_detection;
mod fad;
mod fad_types;
pub use fad::{SourceCompletion, SourceScanHooks};
pub mod connectors {
    pub use crate::fad::*;
    pub use crate::fad_types::*;
    #[derive(Debug, Clone)]
    pub struct ScanRoot {
        pub path: std::path::PathBuf,
        pub origin: Origin,
        pub platform: Option<Platform>,
        pub workspace_rewrites: Vec<PathMapping>,
    }
    impl ScanRoot {
        pub fn local(path: std::path::PathBuf) -> Self {
            Self { path, origin: Origin::local(), platform: None, workspace_rewrites: Vec::new() }
        }
    }
""" + policies + "\npub mod storage { pub mod sqlite {\n" + entry + "\n} }\n" + """
mod indexer {
    use anyhow::{Context, Result};
    use crate::connectors::*;
    use std::collections::{HashMap, BTreeSet};
    use std::fs::{self, OpenOptions};
    use std::path::{Path, PathBuf};
    use std::time::Duration;
""" + source_helpers + workspace_policy + exclusion_check + """
    // These fixtures deliberately pass None progress and no active writers.
    // Calling a progress method is an error, not a simulated successful stop.
    struct IndexingProgress;
    impl IndexingProgress {
        fn stop_requested(&self) -> bool { panic!("progress not included in boundary fixture") }
        fn tick_activity(&self) { panic!("progress not included in boundary fixture") }
        fn check_stop(&self) -> Result<()> { panic!("progress not included in boundary fixture") }
    }
    struct ActiveSessionSourceFilter;
    impl ActiveSessionSourceFilter {
        fn with_recent_write_window_for_test(window: Option<Duration>) -> Self {
            assert!(window.is_none(), "recent-writer policy is outside this fixture");
            Self
        }
        fn take_deferred_sources(&self) -> BTreeSet<PathBuf> { BTreeSet::new() }
    }
    fn should_skip_active_session_source(_: &ActiveSessionSourceFilter, _: SourceKind, _: &Path) -> bool {
        false // Explicit no-active-writer fixture adapter; not an activity probe.
    }
    mod watch_source_stability;
    mod watch_source_ledger;
}
"""
        projections.update({
            "provenance_carriers": "Exact pinned FAD normalized types, Origin, Platform, context, Connector trait and hooks; ScanRoot retains constructor fields needed by fixture/key only",
            "active_filter": "No-active-writer fixture adapter; recent-write option must be None; not a writer-probe test",
            "progress": "Absent-progress fixture adapter; all methods panic if invoked",
            "producer_contract": "Explicit supplemental fixture contract with source reuse enabled; CASS build-contract generation is not exercised",
            "serial_test": "Removed only serial_test import and two serial attributes; executable runs with --test-threads=1",
            "scan_scope": "Verbatim fields and scan method; begin_publication and publish_batch excluded completely; exact SourceIngestLedgerEntry carrier without storage implementation",
        })
        exclusions = [
            "Full CASS compilation and application tests",
            "Native connector parsing, discovery, filesystem notifications; boundary tests use their explicit synthetic connector",
            "Active-writer, exclusion-policy dispatch, cancellation/progress and full registry integration",
            "begin_publication/publish_batch, source receipt and debt persistence, canonical SQL and lexical/search recovery",
            "Aggregate receipt memory limits, replay/restart integration, native macOS qualification",
        ]
    if len(selected) != (12 if include_scan else 10):
        raise RuntimeError(f"watch selection changed; explicitly review its inventory: {selected}")
    return {
        "inputs": inputs, "source_ranges": ranges, "outputs": outputs,
        "required_watch_tests": selected, "fad_revision": revision,
        "projections": projections, "exclusions": exclusions,
    }


def run_watch_components(args: argparse.Namespace, root: Path) -> None:
    if os.name != "posix":
        raise RuntimeError("the selected watch tests include Unix metadata and native-path controls")
    env = os.environ.copy()
    if args.toolchain_bin:
        env["PATH"] = str(args.toolchain_bin.resolve()) + os.pathsep + env.get("PATH", "")
    cargo, rustc = (shutil.which(name, path=env.get("PATH")) for name in ("cargo", "rustc"))
    if not cargo or not rustc:
        raise RuntimeError("cargo and rustc required; use --toolchain-bin for the repository pin")
    env["RUSTUP_TOOLCHAIN"] = tomllib.loads((root / "rust-toolchain.toml").read_text())["toolchain"]["channel"]
    env["RUSTC"] = rustc
    env["CARGO_BUILD_RUSTC"] = rustc
    env["CARGO_BUILD_JOBS"] = "1"
    for variable in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER"):
        env.pop(variable, None)
    lock_bytes = (root / "Cargo.lock").read_bytes()
    lock = tomllib.loads(lock_bytes.decode())
    fad_revision = resolve("franken-agent-detection", lock["package"])["source"].rsplit("#", 1)[-1]
    checkout = args.fad_checkout
    if checkout is None:
        cargo_home = Path(env.get("CARGO_HOME", str(Path.home() / ".cargo"))).resolve()
        matches = list((cargo_home / "git/checkouts").glob(f"franken_agent_detection-*/{fad_revision[:7]}"))
        if len(matches) != 1:
            raise RuntimeError("cannot identify pinned FAD checkout; pass --fad-checkout")
        checkout = matches[0]
    checkout = checkout.resolve()
    now = datetime.datetime.now(datetime.timezone.utc)
    output_root = (args.output_dir or Path(tempfile.mkdtemp(prefix="cass-watch-components-"))).resolve()
    work = output_root / "runs" / (now.strftime("%Y%m%dT%H%M%S.%fZ") + f"-{os.getpid()}")
    work.mkdir(parents=True, exist_ok=False)
    logs = work / "logs"
    logs.mkdir()
    env["CARGO_TARGET_DIR"] = str((args.target_dir or output_root / "target").resolve())
    report = {
        "format": 1, "qualification": "supplemental exact-source components, not full CASS/live watcher/macOS",
        "status": "failed", "started_utc": now.isoformat(), "evidence_directory": str(work),
        "platform": platform.platform(), "repository_lock_sha256": hashlib.sha256(lock_bytes).hexdigest(),
        "runner_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        "commands": [], "environment": {name: env.get(name) for name in (
            "CARGO_HOME", "CARGO_TARGET_DIR", "CARGO_BUILD_JOBS", "RUSTUP_TOOLCHAIN", "RUSTC", "CARGO_BUILD_RUSTC")},
    }

    def run(command: list[str], label: str) -> subprocess.CompletedProcess:
        print(f"watch components: {label}", flush=True)
        started = time.monotonic()
        process = subprocess.Popen(command, cwd=work, env=env, stdout=subprocess.PIPE,
                                   stderr=subprocess.PIPE, start_new_session=True)
        timed_out = False
        drain_timed_out = False
        termination_error = None
        try:
            stdout, stderr = process.communicate(timeout=args.build_timeout)
        except subprocess.TimeoutExpired as initial_timeout:
            timed_out = True
            # This process group was created exclusively for this command;
            # drain a timed-out compiler as well as its Cargo parent.
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass  # The owned process group exited before the signal.
            except OSError as error:
                termination_error = str(error)
            try:
                stdout, stderr = process.communicate(timeout=30)
            except subprocess.TimeoutExpired as drain_timeout:
                drain_timed_out = True
                # Retrying communicate retains its cumulative captured bytes.
                # Preserve those logs even when kernel/pipe state prevents a
                # verified process exit and complete drain after SIGKILL.
                stdout = drain_timeout.output or initial_timeout.output or b""
                stderr = drain_timeout.stderr or initial_timeout.stderr or b""
                for stream in (process.stdout, process.stderr):
                    if stream is not None:
                        stream.close()
                process.poll()
        result = subprocess.CompletedProcess(command, process.returncode, stdout, stderr)
        out_path, err_path = logs / f"{label}.stdout.log", logs / f"{label}.stderr.log"
        out_path.write_bytes(result.stdout)
        err_path.write_bytes(result.stderr)
        report["commands"].append({
            "command": command, "returncode": result.returncode, "timed_out": timed_out,
            "drain_timed_out": drain_timed_out, "termination_error": termination_error,
            "process_exit_and_pipe_drain_verified": not drain_timed_out,
            "wall_seconds": round(time.monotonic() - started, 3),
            "stdout": str(out_path.relative_to(work)), "stderr": str(err_path.relative_to(work)),
            "stdout_sha256": hashlib.sha256(result.stdout).hexdigest(),
            "stderr_sha256": hashlib.sha256(result.stderr).hexdigest(),
        })
        if drain_timed_out:
            raise RuntimeError(
                f"{label} exceeded its deadline and the 30-second termination/drain deadline; "
                f"child cleanup is unverified and partial logs are retained in {logs}"
            )
        if timed_out or result.returncode:
            print(result.stderr.decode(errors="replace"), flush=True)
            raise RuntimeError(
                f"{label} failed with exit {result.returncode}, timed_out={timed_out}; see {work}"
            )
        return result

    try:
        sources = watch_component_sources(root, lock, checkout, args.watch_scan)
        for name, text in sources.pop("outputs").items():
            destination = work / name
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_bytes(text.encode())
        report.update(sources)
        report["source_verified_before"] = True
        direct_names = DIRECT + (("anyhow", "serde", "glob", "dotenvy", "blake3", "dunce")
                                 if args.watch_scan else ())
        direct, allowed = dependency_closure(lock, direct_names)
        selected = {identity(package) for package in allowed}
        tables = ["[[package]]" + block for block in lock_bytes.decode().split("[[package]]")[1:]
                  if identity(tomllib.loads("[[package]]" + block)["package"][0]) in selected]
        dependencies = [f'{p["name"]} = "={p["version"]}"' for p in direct if p["name"] != "serde"]
        if args.watch_scan:
            serde = next(package for package in direct if package["name"] == "serde")
            dependencies.append(f'serde = {{ version = "={serde["version"]}", features = ["derive"] }}')
            env["CASS_SOURCE_INGEST_CONTRACT"] = "supplemental-gh538-source-boundary-fixture"
            env["CASS_SOURCE_INGEST_REUSE"] = "true"
            for name in ("CASS_EXCLUDE_PATHS", "CASS_EXCLUDE_WORKSPACES", "CASS_TEST_ACTIVE_SESSION_SOURCE_PATHS"):
                env.pop(name, None)
            report["environment"].update({name: env.get(name) for name in (
                "CASS_SOURCE_INGEST_CONTRACT", "CASS_SOURCE_INGEST_REUSE", "CASS_EXCLUDE_PATHS",
                "CASS_EXCLUDE_WORKSPACES", "CASS_TEST_ACTIVE_SESSION_SOURCE_PATHS")})
        manifest = "\n".join([
            "[package]", f'name = "{PACKAGE}"', f'version = "{VERSION}"', 'edition = "2024"',
            "[features]", 'default = ["connectors"]', 'connectors = []',
            "[dependencies]", *dependencies,
            "[profile.dev]", "debug = 0", "incremental = false", "[profile.test]",
            "debug = 0", "incremental = false", "",
        ])
        harness_lock = "\n".join([
            f'version = {lock["version"]}', "", "[[package]]", f'name = "{PACKAGE}"',
            f'version = "{VERSION}"', "dependencies = " + json.dumps(list(direct_names)), "", *tables,
        ])
        (work / "Cargo.toml").write_bytes(manifest.encode())
        (work / "Cargo.lock").write_bytes(harness_lock.encode())
        report["cargo"] = run([cargo, "--version", "--verbose"], "cargo-version").stdout.decode().strip()
        report["rustc"] = run([rustc, "--version", "--verbose"], "rustc-version").stdout.decode().strip()
        offline = ["--offline"] if args.offline else []
        run([cargo, "metadata", "--format-version", "1", *offline], "metadata")
        normalized = (work / "Cargo.lock").read_bytes()
        retained = validate_normalized_lock(tomllib.loads(normalized.decode()), allowed, direct)
        report["dependencies"] = [
            {key: package[key] for key in ("name", "version", "source", "checksum")}
            for package in sorted(retained, key=identity)
        ]
        report["harness_lock_sha256"] = hashlib.sha256(normalized).hexdigest()
        report["output_sha256"] = {
            str(path.relative_to(work)): hashlib.sha256(path.read_bytes()).hexdigest()
            for path in sorted(work.rglob("*.rs"))
        }
        report["manifest_sha256"] = hashlib.sha256((work / "Cargo.toml").read_bytes()).hexdigest()
        build = run([cargo, "test", "--locked", "--lib", "--no-run", "--message-format=json", "-j1", *offline],
                    "build")
        artifacts = []
        for line in build.stdout.decode().splitlines():
            message = json.loads(line)
            if (message.get("reason") == "compiler-artifact" and message.get("profile", {}).get("test")
                    and message.get("executable")):
                artifacts.append(message)
        if len(artifacts) != 1:
            raise RuntimeError(f"expected one test executable, found {len(artifacts)}")
        binary = Path(artifacts[0]["executable"])
        report["executable"] = str(binary)
        report["executable_sha256"] = hashlib.sha256(binary.read_bytes()).hexdigest()
        listing = run([str(binary), "--list"], "inventory").stdout.decode()
        names = [line.removesuffix(": test") for line in listing.splitlines() if line.endswith(": test")]
        required = set(report["required_watch_tests"])
        matched = [name for name in names if name.rsplit("::", 1)[-1] in required]
        if len(matched) != len(required) or {name.rsplit("::", 1)[-1] for name in matched} != required:
            raise RuntimeError("compiled inventory omitted or duplicated a required watch test")
        report["compiled_tests"] = names
        tests = run([str(binary), "--nocapture", "--test-threads=1"], "tests")
        text = tests.stdout.decode(errors="replace") + tests.stderr.decode(errors="replace")
        for name in matched:
            if f"test {name} ... ok" not in text:
                raise RuntimeError(f"required watch test did not execute successfully: {name}")
        summary = re.search(r"test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured; (\d+) filtered out", text)
        if summary is None:
            raise RuntimeError("missing complete libtest result")
        report["counts"] = dict(zip(("passed", "failed", "ignored", "measured", "filtered_out"),
                                    map(int, summary.groups())))
        if (report["counts"]["passed"] + report["counts"]["ignored"] != len(names)
                or report["counts"]["failed"] != 0
                or report["counts"]["measured"] != 0
                or report["counts"]["filtered_out"] != 0):
            raise RuntimeError("libtest counts do not match the compiled inventory")
        if args.bench:
            run([str(binary), "gh512_benchmark_4159", "--ignored", "--nocapture"], "benchmark")
        if (work / "Cargo.lock").read_bytes() != normalized:
            raise RuntimeError("component dependency pins changed during qualification")
        for name, digest in report["output_sha256"].items():
            if hashlib.sha256((work / name).read_bytes()).hexdigest() != digest:
                raise RuntimeError(f"generated source changed during qualification: {name}")
        for entry in report["inputs"].values():
            if hashlib.sha256(Path(entry["path"]).read_bytes()).hexdigest() != entry["sha256"]:
                raise RuntimeError(f"input changed during qualification: {entry['path']}")
        if (root / "Cargo.lock").read_bytes() != lock_bytes:
            raise RuntimeError("repository dependency pins changed during qualification")
        if hashlib.sha256(binary.read_bytes()).hexdigest() != report["executable_sha256"]:
            raise RuntimeError("executed test binary changed during qualification")
        report["source_verified_after"] = True
        report["status"] = "supplemental-passed"
    except Exception as error:
        report["error"] = f"{type(error).__name__}: {error}"
        raise
    finally:
        report["finished_utc"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        (work / "report.json").write_text(json.dumps(report, indent=2) + "\n")
        (output_root / "report.json").write_text(json.dumps(report, indent=2) + "\n")
        print(f"{report['status']}: {work / 'report.json'}", flush=True)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bench", action="store_true", help="also run the 4159-source benchmark")
    parser.add_argument("--watch-components", action="store_true",
                        help="also qualify exact watch stability and receipt serialization sources")
    parser.add_argument("--watch-scan", action="store_true",
                        help="include exact scan method and synthetic source-boundary tests (implies --watch-components)")
    parser.add_argument("--fad-checkout", type=Path, help="checkout of the FAD revision pinned by Cargo.lock")
    parser.add_argument("--output-dir", type=Path, help="retain watch component sources, logs, and reports here")
    parser.add_argument("--target-dir", type=Path, help="separate cached watch component build directory")
    parser.add_argument("--toolchain-bin", type=Path, help="directory containing pinned cargo and rustc")
    parser.add_argument("--offline", action="store_true", help="use cached watch component dependencies only")
    parser.add_argument("--build-timeout", type=int, default=300)
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    if args.watch_components or args.watch_scan:
        run_watch_components(args, root)
        return
    source = root / "src/connectors/source_dependencies/observation.rs"
    source_bytes = source.read_bytes()
    lock_path = root / "Cargo.lock"
    lock_bytes = lock_path.read_bytes()
    lock_text = lock_bytes.decode()
    lock = tomllib.loads(lock_text)
    direct, allowed = dependency_closure(lock)
    selected = {identity(package) for package in allowed}
    tables = []
    for block in lock_text.split("[[package]]")[1:]:
        package = tomllib.loads("[[package]]" + block)["package"][0]
        if identity(package) in selected:
            tables.append("[[package]]" + block)
    manifest = "\n".join([
        "[package]", f'name = "{PACKAGE}"', f'version = "{VERSION}"',
        'edition = "2024"', "[lib]", 'path = "observation.rs"', "[dependencies]",
        *(f'{package["name"]} = "={package["version"]}"' for package in direct), "",
    ])
    harness_lock = "\n".join([
        f'version = {lock["version"]}', "", "[[package]]",
        f'name = "{PACKAGE}"', f'version = "{VERSION}"',
        'dependencies = ["serde_json", "tempfile"]', "", *tables,
    ])
    receipt = {
        "source_sha256": hashlib.sha256(source_bytes).hexdigest(),
        "repository_lock_sha256": hashlib.sha256(lock_bytes).hexdigest(),
        "scope": "exact production filesystem matcher only; full integration is separate",
    }
    with tempfile.TemporaryDirectory(prefix="cass-source-ledger-") as directory:
        work = Path(directory)
        (work / "observation.rs").write_bytes(source_bytes)
        # Preserve TOML bytes without Windows text-mode newline translation:
        # checked-out dependency blocks can already contain valid CRLF endings.
        (work / "Cargo.toml").write_bytes(manifest.encode("utf-8"))
        (work / "Cargo.lock").write_bytes(harness_lock.encode("utf-8"))
        env = os.environ.copy()
        toolchain = tomllib.loads((root / "rust-toolchain.toml").read_text())["toolchain"]["channel"]
        env["RUSTUP_TOOLCHAIN"] = toolchain
        for variable in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS"):
            env.pop(variable, None)
        try:
            # Metadata resolves the smaller feature graph without compiling code.
            # The copied CASS graph can contain optional edges not used here; let
            # Cargo normalize only this temporary lock, then audit it before test.
            subprocess.run(
                ["cargo", "metadata", "--format-version", "1", "--manifest-path", str(work / "Cargo.toml")],
                cwd=work, env=env, stdout=subprocess.DEVNULL, check=True,
            )
            normalized = (work / "Cargo.lock").read_bytes()
            retained = validate_normalized_lock(tomllib.loads(normalized.decode()), allowed, direct)
            receipt["harness_lock_sha256"] = hashlib.sha256(normalized).hexdigest()
            receipt["dependencies"] = [
                {key: package[key] for key in ("name", "version", "source", "checksum")}
                for package in sorted(retained, key=identity)
            ]
            receipt["pruned_packages"] = len(allowed) - len(retained)
            print("GH512_MATCHER_SOURCE=" + json.dumps(receipt, sort_keys=True), flush=True)
            command = ["cargo", "test", "--release", "--locked", "--manifest-path", str(work / "Cargo.toml")]
            subprocess.run(command + ["--", "--nocapture"], cwd=work, env=env, check=True)
            if args.bench:
                subprocess.run(command + ["gh512_benchmark_4159", "--", "--ignored", "--nocapture"],
                               cwd=work, env=env, check=True)
            if (work / "Cargo.lock").read_bytes() != normalized:
                raise RuntimeError("matcher dependency pins changed during qualification")
            if (work / "observation.rs").read_bytes() != source_bytes:
                raise RuntimeError("tested matcher changed during qualification")
        finally:
            if source.read_bytes() != source_bytes or lock_path.read_bytes() != lock_bytes:
                raise RuntimeError("repository source or dependency lock changed during qualification")


if __name__ == "__main__":
    main()
