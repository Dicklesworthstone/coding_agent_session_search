#!/usr/bin/env python3
"""Apply reviewed GH523 call-site edits to the exact inspected monolith.

No Git commands or network operations. Default is a read-only preview. --apply
changes only src/lib.rs and installs the supplied CLI regression test. This
exists because the authoring connector cannot replace a multi-megabyte file.
"""
from __future__ import annotations

import argparse
import difflib
import hashlib
from pathlib import Path

BASE_LIB_SHA = "5c022a957b5d40c21a2e2baee0e7f67368a5efcb"
MODULE = "crate::search::lexical_quarantine_diagnostics"
EDITS = [
    (
        "struct DiagQuarantineReport {\n    summary: DiagQuarantineSummary,\n",
        "struct DiagQuarantineReport {\n    summary: DiagQuarantineSummary,\n"
        '    #[serde(skip_serializing_if = "Option::is_none")]\n'
        f"    lexical_rebuild_quarantine: Option<{MODULE}::Report>,\n",
    ),
    (
        "    let quarantine_report =\n"
        "        quarantine.then(|| collect_diag_quarantine_report(&data_dir, &index_path));\n",
        "    let quarantine_report = quarantine.then(|| {\n"
        "        let mut report = collect_diag_quarantine_report(&data_dir, &index_path);\n"
        "        report.lexical_rebuild_quarantine =\n"
        f"            Some({MODULE}::collect(&data_dir));\n"
        "        report\n    });\n",
    ),
    (
        '        println!("Quarantine:");\n',
        '        println!("Quarantine:");\n'
        "        if let Some(rebuilds) = &report.lexical_rebuild_quarantine {\n"
        "            for line in rebuilds.human_lines() {\n"
        '                println!("  {line}");\n'
        "            }\n        }\n",
    ),
    (
        "    for entry in walkdir::WalkDir::new(manifest_root).follow_links(false) {\n",
        "    for entry in walkdir::WalkDir::new(manifest_root)\n"
        "        .follow_links(false)\n        .follow_root_links(false)\n        .into_iter()\n"
        f"        .filter_entry({MODULE}::generation_walk_entry)\n    {{\n",
    ),
    (
        "    if let Some(fmt) = structured_format {\n"
        "        let quarantine_report = if command_surface == doctor::DoctorCommandSurface::Check {\n"
        "            collect_diag_quarantine_report_without_cleanup_plan(&data_dir, &index_path)\n"
        "        } else {\n"
        "            collect_diag_quarantine_report(&data_dir, &index_path)\n"
        "        };\n",
        f"    let rebuild_quarantine = {MODULE}::collect(&data_dir);\n"
        "    if let Some(fmt) = structured_format {\n"
        "        let mut quarantine_report =\n"
        "            if command_surface == doctor::DoctorCommandSurface::Check {\n"
        "                collect_diag_quarantine_report_without_cleanup_plan(&data_dir, &index_path)\n"
        "            } else {\n"
        "                collect_diag_quarantine_report(&data_dir, &index_path)\n"
        "            };\n"
        "        quarantine_report.lexical_rebuild_quarantine = Some(rebuild_quarantine);\n",
    ),
    (
        '        print_doctor_human_line(wrap, "CASS Doctor".bold().to_string());\n'
        "        println!();\n",
        '        print_doctor_human_line(wrap, "CASS Doctor".bold().to_string());\n'
        "        println!();\n"
        "        if !rebuild_quarantine.complete\n"
        "            || rebuild_quarantine\n                .inventory\n                .as_ref()\n"
        "                .is_some_and(|inventory| !inventory.artifacts.is_empty())\n"
        "        {\n            for line in rebuild_quarantine.human_lines() {\n"
        "                print_doctor_human_line(wrap, line);\n"
        "            }\n            println!();\n        }\n",
    ),
]


def blob_sha(content: bytes) -> str:
    return hashlib.sha1(b"blob " + str(len(content)).encode() + b"\0" + content).hexdigest()


def patch_text(text: str) -> str:
    # Validate all anchors before any replacement. Never partially patch drift.
    for number, (old, new) in enumerate(EDITS, 1):
        if text.count(old) != 1 or new in text:
            raise ValueError(f"edit {number}: expected exactly one unmodified anchor")
    for old, new in EDITS:
        text = text.replace(old, new, 1)
    return text


def already_applied(text: str) -> bool:
    return all(text.count(new) == 1 for _, new in EDITS)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--apply", action="store_true")
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    lib = root / "src/lib.rs"
    template = root / "tests/cli_diag_lexical_rebuild.rs.in"
    test = root / "tests/cli_diag_lexical_rebuild.rs"
    for path in (lib, template, test):
        if path.is_symlink():
            raise SystemExit(f"refusing symlink: {path}")
    original = lib.read_bytes()
    source = original.decode("utf-8")
    cases = template.read_bytes()
    if already_applied(source):
        if not test.is_file() or test.read_bytes() != cases:
            raise SystemExit("call sites already changed but CLI tests differ; inspect manually")
        print("GH523 call sites and CLI test are already applied; no writes")
        return
    actual = blob_sha(original)
    if actual != BASE_LIB_SHA:
        raise SystemExit(f"src/lib.rs changed: expected {BASE_LIB_SHA}, got {actual}; no writes")
    if test.exists():
        raise SystemExit("CLI test target already exists; no files overwritten")
    module = root / "src/search/lexical_quarantine_diagnostics.rs"
    facade = (root / "src/search/mod.rs").read_text()
    if not module.is_file() or "pub mod lexical_quarantine_diagnostics;" not in facade:
        raise SystemExit("reviewed diagnostic module is not registered; no writes")
    updated = patch_text(source)
    print("".join(difflib.unified_diff(source.splitlines(True), updated.splitlines(True),
                                    fromfile="a/src/lib.rs", tofile="b/src/lib.rs")), end="")
    print(f"Verified source blob {actual}; six uniquely anchored edits; CLI regression test ready")
    if args.apply:
        # Recheck after preparation. No user data or unrelated repository file is touched.
        if lib.read_bytes() != original:
            raise SystemExit("source changed during preparation; no writes")
        with test.open("xb") as output:
            output.write(cases)
        lib.write_bytes(updated.encode("utf-8"))
        print("Applied src/lib.rs and installed tests/cli_diag_lexical_rebuild.rs")


if __name__ == "__main__":
    main()
