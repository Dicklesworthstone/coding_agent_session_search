#!/usr/bin/env python3
"""Extract explicitly selected CASS components without replacing their behavior."""

import argparse
import hashlib
import json
import re
import subprocess
import tomllib
from pathlib import Path


HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]
BASELINE = "a93afc1e85f069f606a64eea4ce810a30a0a3bc8"
NEGATIVE_TEST = (
    "raw_mirror::store::tests::"
    "gh538_dangling_manifest_symlink_is_refused_before_staging"
)
NEGATIVE_ASSERTION = "refusing a symlink must not leak an unowned manifest staging file"


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def require(condition, message):
    if not condition:
        raise ValueError(message)


def package_identity(package):
    return tuple(package.get(key) for key in ("name", "version", "source", "checksum"))


class Extraction:
    def __init__(self, repo, component):
        self.repo = repo
        self.component = component
        self.inputs = {}
        self.ranges = []
        self.outputs = {}
        self.projections = {}
        self.tests = []

    def source(self, path, baseline=False):
        revision = BASELINE if baseline else "working-tree"
        if baseline:
            data = subprocess.check_output(
                ["git", "show", f"{BASELINE}:{path}"], cwd=self.repo
            )
        else:
            data = (self.repo / path).read_bytes()
        text = data.decode("utf-8")
        self.inputs[f"{revision}:{path}"] = sha256(data)
        return text

    def piece(self, path, text, start, end, label, baseline=False):
        selected = text[start:end]
        self.ranges.append({
            "input": path,
            "revision": BASELINE if baseline else "working-tree",
            "label": label,
            "start_byte": len(text[:start].encode()),
            "end_byte": len(text[:end].encode()),
            "start_line": text.count("\n", 0, start) + 1,
            "end_line": text.count("\n", 0, end),
            "selected_sha256": sha256(selected.encode()),
        })
        return selected

    def between(self, path, text, start, end, label):
        a = text.index(start)
        b = text.index(end, a)
        return self.piece(path, text, a, b, label)

    def test(self, path, text, name):
        marker = "    #[test]\n    fn " + name + "("
        a = text.index(marker)
        b = text.index("\n    }\n", a) + len("\n    }\n")
        self.tests.append(name)
        return self.piece(path, text, a, b, "verbatim test: " + name)

    def dependencies(self, template, package_name, dependencies, features=None):
        lock_path = HERE / "locks" / (template + ".Cargo.lock")
        lock_bytes = lock_path.read_bytes()
        # Only the extracted crate identity changes for the pinned baseline.
        if self.component == "mirror-baseline":
            old = b'name = "cass-538-mirror-exact-candidate"'
            require(lock_bytes.count(old) == 1, "unexpected mirror lock root")
            lock_bytes = lock_bytes.replace(old, b'name = "cass-538-mirror-exact-baseline"')
        main_bytes = (self.repo / "Cargo.lock").read_bytes()
        main = tomllib.loads(main_bytes.decode())["package"]
        main_identities = {package_identity(p) for p in main}
        subset = tomllib.loads(lock_bytes.decode())["package"]
        roots = [p for p in subset if p["name"] == package_name]
        require(len(roots) == 1 and "source" not in roots[0], "unexpected harness root")
        for package in subset:
            if package is not roots[0]:
                require(package_identity(package) in main_identities, (
                    "supplemental lock is no longer a subset of Cargo.lock: "
                    + repr(package_identity(package))
                ))
        versions = {}
        for name in dependencies:
            matches = [p["version"] for p in subset if p["name"] == name]
            require(len(matches) == 1, f"ambiguous direct dependency: {name}")
            versions[name] = matches[0]
        manifest = (
            f'[package]\nname = "{package_name}"\nversion = "0.0.0"\n'
            'edition = "2024"\n\n[dependencies]\n'
        )
        for name in dependencies:
            selected = (features or {}).get(name)
            if selected:
                manifest += (
                    f'{name} = {{ version = "={versions[name]}", '
                    f'features = {json.dumps(selected)} }}\n'
                )
            else:
                manifest += f'{name} = "={versions[name]}"\n'
        manifest += (
            '\n[profile.dev]\ndebug = 0\nincremental = false\n'
            '\n[profile.test]\ndebug = 0\nincremental = false\n'
        )
        self.outputs["Cargo.toml"] = manifest
        self.outputs["Cargo.lock"] = lock_bytes.decode()
        self.inputs["working-tree:Cargo.lock"] = sha256(main_bytes)
        self.inputs[f"support:locks/{template}.Cargo.lock"] = sha256(lock_path.read_bytes())
        return versions

    def finish(self, scope, exclusions, dependency_versions):
        # Support files are part of provenance, including the extraction logic.
        self.inputs["support:extract.py"] = sha256(Path(__file__).read_bytes())
        provenance = {
            "format": 1,
            "component": self.component,
            "baseline_revision": BASELINE,
            "scope": scope,
            "exclusions": exclusions,
            "source_sha256": self.inputs,
            "source_ranges": self.ranges,
            "projections": self.projections,
            "selected_tests": self.tests,
            "dependency_versions": dependency_versions,
            "output_sha256": {
                path: sha256(text.encode()) for path, text in self.outputs.items()
            },
        }
        self.outputs["sources.json"] = json.dumps(provenance, indent=2) + "\n"
        return self.outputs


def mirror(repo, baseline=False):
    mode = "baseline" if baseline else "candidate"
    extraction = Extraction(repo, "mirror-" + mode)
    path = "src/raw_mirror/store.rs"
    source = extraction.source(path, baseline)
    tests_source = extraction.source(path)
    tests_start = source.index("#[cfg(test)]\nmod tests {")
    cut1 = source.index("pub fn prune(")
    cut1_end = source.index("fn apply_prune_entries(", cut1)
    cut2 = source.index("fn pinned_prune_manifest_ids(", cut1_end)
    cut2_end = source.index("fn collect_raw_mirror_physical_blobs(", cut2)
    production = "".join(
        extraction.piece(path, source, a, b, "production", baseline)
        for a, b in [(0, cut1), (cut1_end, cut2), (cut2_end, tests_start)]
    )
    helper_start = tests_source.index("#[cfg(test)]\nmod tests {")
    gh447 = tests_source.index("    #[test]\n    fn gh447_sensitive_containers", helper_start)
    tests = extraction.piece(path, tests_source, helper_start, gh447, "GH538 tests and helpers")
    extraction.tests.extend(re.findall(
        r"    #\[test\]\n(?:    #\[[^\n]+\]\n)*    fn (\w+)\(", tests
    ))
    extraction.outputs["src/store.rs"] = production + tests + "}\n"

    path = "src/raw_mirror/policy.rs"
    source = extraction.source(path, baseline)
    tests_start = source.index("#[cfg(test)]\nmod tests {")
    default_start = source.index("pub(crate) fn capture_default(", tests_start)
    retention_start = source.index("mod retention {", default_start)
    policy = extraction.piece(path, source, 0, tests_start, "production", baseline)
    policy += extraction.piece(path, source, default_start, retention_start, "default capture", baseline)
    policy += '\n#[cfg(test)]\nmod tests { include!("policy_tests.rs"); }\n'
    extraction.outputs["src/policy.rs"] = policy

    path = "src/raw_mirror/policy_tests.rs"
    source = extraction.source(path)
    helpers_end = source.index("fn run_usage_peer(")
    tests = extraction.piece(path, source, 0, helpers_end, "fixture helpers")
    wanted = {
        "frequent_appends_reuse_chunks_and_preserve_every_published_snapshot",
        "partial_staging_is_removed_when_the_next_chunk_cannot_fit",
        "an_unchanged_capture_at_the_cap_is_not_an_outage",
        "gh538_unchanged_watch_captures_do_not_mutate_the_mirror",
        "gh538_large_mirror_near_cap_reuses_inventory_across_watch_cycles",
        "gh538_replacement_shrink_invalidates_a_cached_lower_bound",
        "gh538_inventory_rejects_excessive_depth_without_unbounded_descriptors",
        "gh538_cancelled_inventory_is_not_cached_and_does_not_hold_global_mutex",
        "refused_append_leaves_the_last_recovery_copy_intact",
        "a_path_alias_cannot_reuse_another_inventory_of_the_same_store",
        "direct_manifest_link_growth_counts_before_the_next_admission",
        "malformed_or_exhausted_usage_generations_preserve_existing_snapshots",
        "a_symlinked_usage_generation_is_refused_and_preserves_its_target",
    }
    starts = list(re.finditer(r"(?m)^#\[test\]\n", source))
    found = set()
    for i, start in enumerate(starts):
        stop = starts[i + 1].start() if i + 1 < len(starts) else len(source)
        name = re.search(r"(?m)^fn (\w+)\(", source[start.start():stop]).group(1)
        if name in wanted:
            tests += extraction.piece(path, source, start.start(), stop, name)
            extraction.tests.append(name)
            found.add(name)
    require(found == wanted, f"missing selected mirror tests: {wanted - found}")
    extraction.outputs["src/policy_tests.rs"] = tests
    extraction.outputs["src/lib.rs"] = (
        '#![allow(dead_code)]\npub mod raw_mirror { pub mod store {\n'
        'include!("store.rs");\npub(super) mod policy { include!("policy.rs"); }\n} }\n'
    )
    versions = extraction.dependencies(
        "mirror", "cass-538-mirror-exact-" + mode,
        ["anyhow", "serde", "serde_json", "blake3", "fs2", "dotenvy", "libc",
         "tracing", "tracing-subscriber", "tempfile", "walkdir"],
        {"serde": ["derive"], "tracing-subscriber": ["fmt", "json"]},
    )
    return extraction.finish(
        "Verbatim production raw-mirror filesystem and capture/admission policy; "
        "candidate tests, with no filesystem or database substitutes",
        ["SQL prune entry point, pinned manifest/tag SQL queries, automatic retention",
         "Unselected unit tests and subprocess policy tests",
         "Indexer, provider scanning, native watcher, canonical SQL, lexical search/replay",
         "Original macOS kernel stall and reporter archive qualification"],
        versions,
    )


def watchdog(repo):
    extraction = Extraction(repo, "watchdog")
    path = "src/lib.rs"
    source = extraction.source(path)
    core = extraction.between(
        path, source, "fn collect_stall_diagnostics(data_dir: &Path)",
        "fn abort_after_index_stall_if_requested(",
        "exact diagnostic collection, single-flight probe, watchdog and abort envelope",
    )
    exit_helper = extraction.between(
        path, source, "fn exit_without_this_threads_tls_destructors(",
        "#[cfg(test)]\nmod stall_diagnostics_tests", "production platform exit helper",
    )
    tests = (
        "#[cfg(test)]\nmod stall_diagnostics_tests {\n"
        "use super::collect_stall_diagnostics;\nuse tempfile::TempDir;\n"
    )
    a = source.index("#[cfg(test)]\nmod stall_diagnostics_tests")
    b = source.index("fn run_lexical_reconcile_cli(", a)
    for name in re.findall(r"    #\[test\]\n    fn (\w+)\(", source[a:b]):
        if name != "issue_342_index_help_documents_semantic_stall_controls":
            tests += extraction.test(path, source, name)
    tests += "}\n"
    extraction.outputs["src/lib.rs"] = (
        '#![allow(dead_code)]\n#![recursion_limit = "256"]\n'
        "use std::path::{Path, PathBuf};\nuse std::time::Duration;\n"
        "mod indexer;\nmod raw_mirror;\n" + core + exit_helper + tests
        + "\n#[cfg(test)]\nmod fifo_tests;\n"
    )

    path = "src/indexer/mod.rs"
    source = extraction.source(path)
    events = extraction.between(
        path, source, "#[derive(Debug, Clone)]\npub enum ReindexCommand",
        "// =============================================================================\n// Stale Detection",
        "watch control event types",
    )
    carrier = extraction.between(
        path, source, "pub(crate) const INDEX_PHASE_PREPARING:",
        "/// A connector rejected a source", "progress schema before documented projection",
    )
    removed_field = "    storage_cancellation: crate::franken_sync::CancellationHandle,\n"
    require(carrier.count(removed_field) == 1, "progress cancellation field moved")
    carrier = carrier.replace(removed_field, "")
    methods = extraction.between(
        path, source, "impl IndexingProgress {", "fn check_legacy_omp_analytics_stop(",
        "progress methods before documented SQL cancellation relay omission",
    )
    removed_call = "        self.storage_cancellation.cancel();\n"
    require(methods.count(removed_call) == 1, "progress SQL cancellation relay moved")
    methods = methods.replace(removed_call, "")
    extraction.projections = {
        "removed_progress_field": removed_field,
        "removed_request_stop_line": removed_call,
        "IndexingStats": "Only the two fields consumed by snapshot_json, copied verbatim",
    }
    helpers = extraction.between(
        path, source, "fn staged_field_or_null(", "#[derive(Clone)]", "JSON snapshot helpers",
    )
    fields = []
    for name in ["quarantined_conversations", "lexical_update_deferred"]:
        a = source.index("    pub " + name + ":", source.index("pub struct IndexingStats"))
        b = source.index("\n", a) + 1
        fields.append(extraction.piece(path, source, a, b, "IndexingStats field: " + name))
    stats = "#[derive(Debug, Default)]\npub struct IndexingStats {\n" + "".join(fields) + "}\n"
    idle = extraction.between(
        path, source, "enum WatchLoopEvent {", "fn is_database_watch_root(",
        "exact channel selection, idle wait, cancellation checks and idle RAII",
    )
    idle_tests = "#[cfg(test)]\nmod tests {\nuse super::*;\n"
    for name in ["watch_idle_wait_honors_cancellation_without_activity",
                 "watch_idle_wait_preserves_deadline_without_activity",
                 "gh538_watch_idle_marker_clears_before_event_or_disconnect"]:
        idle_tests += extraction.test(path, source, name)
    idle_tests += "}\n"
    extraction.outputs["src/indexer.rs"] = (
        "use anyhow::Result;\nuse crossbeam_channel::{Receiver, bounded, never};\n"
        "use std::path::PathBuf;\nuse std::sync::{Arc, Mutex};\n"
        "use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};\n"
        "use std::time::{Duration, Instant};\n"
        + events + stats + carrier + methods + helpers + idle + idle_tests
    )
    path = "src/raw_mirror/store.rs"
    source = extraction.source(path)
    raw_io = extraction.between(
        path, source, "#[derive(Clone)]\nstruct RawMirrorIoObserver",
        "#[derive(Debug)]\nstruct RawMirrorFileLockGuard",
        "exact raw-mirror observer, operation tracker, snapshot and cancellation wrappers",
    )
    extraction.outputs["src/raw_mirror.rs"] = (
        "use serde_json::{Value, json};\nuse std::cell::RefCell;\n"
        "use std::collections::BTreeMap;\nuse std::fs;\nuse std::path::{Path, PathBuf};\n"
        "use std::sync::{Arc, Mutex, OnceLock};\n"
        "use std::sync::atomic::{AtomicU64, Ordering};\nuse std::time::Instant;\n"
        + raw_io + '\ninclude!("../fifo_io_support.rs");\n'
    )
    for support, output in [("fifo_io_support.rs", "fifo_io_support.rs"),
                            ("fifo_tests.rs", "src/fifo_tests.rs")]:
        data = (HERE / "support" / support).read_bytes()
        extraction.inputs["support:" + support] = sha256(data)
        extraction.outputs[output] = data.decode()
    extraction.tests.append("fifo_tests::real_fifo_diagnostics_cannot_hide_watchdog_abort")
    versions = extraction.dependencies(
        "watchdog", "cass-538-watchdog-exact",
        ["anyhow", "serde_json", "chrono", "dotenvy", "tracing", "tempfile",
         "crossbeam-channel", "libc"],
    )
    return extraction.finish(
        "Exact watchdog, diagnostics, raw I/O observer, idle wait and selected production "
        "tests, plus separately identified real FIFO subprocess probes",
        ["Full cass executable and native filesystem watcher",
         "Provider scanning, ingestion, canonical SQL, lexical publication, replay debts/search",
         "Progress SQL cancellation field/relay; atomic stop behavior remains exact",
         "IndexingStats fields unused by the progress snapshot",
         "abort_after_index_stall_if_requested and real bounded SQL WAL checkpoint; FIFO "
         "probe uses exact envelope/exit helper after the exact watchdog abort decision",
         "CLI help test, unrelated application tests, native macOS qualification"],
        versions,
    )


def extract(repo, output_dir, verify=False):
    result = {}
    for component, outputs in [
        ("mirror-candidate", mirror(repo)),
        ("mirror-baseline", mirror(repo, baseline=True)),
        ("watchdog", watchdog(repo)),
    ]:
        destination = output_dir / component
        for name, text in outputs.items():
            path = destination / name
            data = text.encode()
            if verify:
                require(path.read_bytes() == data, f"source drift: {path}")
            else:
                path.parent.mkdir(parents=True, exist_ok=True)
                if not path.exists() or path.read_bytes() != data:
                    path.write_bytes(data)
        result[component] = json.loads(outputs["sources.json"])
    return result


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output-dir", required=True, type=Path)
    parser.add_argument("--repo", type=Path, default=REPO)
    parser.add_argument("--verify", action="store_true")
    args = parser.parse_args()
    results = extract(args.repo.resolve(), args.output_dir.resolve(), args.verify)
    for name, result in results.items():
        print(f"{'verified' if args.verify else 'extracted'} {name}: "
              f"{len(result['source_ranges'])} ranges, {len(result['selected_tests'])} tests")
