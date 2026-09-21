#!/usr/bin/env python3
"""Check build.rs feature contracts before Cargo compiles its dependency graph.

This is a narrow, dependency-free preflight for explicit features. build.rs
remains authoritative for versions, package names, sources and default features.
Unrecognized Rust contract syntax fails instead of silently skipping a record.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import re
import sys
import tomllib


def contracts(source: str) -> dict[tuple[str, str], frozenset[str]]:
    source = re.sub(r"(?m)^\s*//[^\n]*", "", source)
    arrays = re.findall(
        r"const CONTRACTS:\s*&\[DependencyContract\]\s*=\s*&\[(.*?)\n\];",
        source, re.S,
    )
    if len(arrays) != 1:
        raise ValueError("expected exactly one literal CONTRACTS array in build.rs")
    body = arrays[0]
    pattern = r"\s*DependencyContract\s*\{(.*?)\},?\s*"
    records = re.findall(pattern, body, re.S)
    if not records or re.sub(pattern, "", body, flags=re.S).strip():
        raise ValueError("unrecognized or empty dependency contract array")
    result = {}
    for record in records:
        fields = {}
        for name in ("dep_table", "dep_key"):
            values = re.findall(r"\b" + name + r':\s*"([^"\\]+)"\s*,', record)
            if len(values) != 1:
                raise ValueError(f"missing or ambiguous {name} in dependency contract")
            fields[name] = values[0]
        values = re.findall(r"\bexpected_features:\s*&\[(.*?)\]\s*,", record, re.S)
        if len(values) != 1:
            raise ValueError(f"nonliteral feature list for {fields['dep_key']}")
        features = json.loads("[" + re.sub(r",\s*$", "", values[0]) + "]")
        if any(not isinstance(item, str) or not item for item in features):
            raise ValueError("contract features must be nonempty strings")
        key = (fields["dep_table"], fields["dep_key"])
        if key in result or len(set(features)) != len(features):
            raise ValueError(f"duplicate dependency contract or feature: {key}")
        result[key] = frozenset(features)
    return result


def check(source: str, manifest: dict) -> list[str]:
    errors = []
    for (table, key), expected in contracts(source).items():
        spec = manifest.get(table, {}).get(key)
        if not isinstance(spec, dict):
            errors.append(f"[{table}].{key}: missing dependency table")
            continue
        actual = spec.get("features", [])
        if not isinstance(actual, list) or any(not isinstance(item, str) for item in actual):
            errors.append(f"[{table}].{key}: features must be a string array")
            continue
        if set(actual) != expected:
            errors.append(
                f"[{table}].{key}: missing={sorted(expected - set(actual))}; "
                f"unexpected={sorted(set(actual) - expected)}"
            )
    return errors


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", nargs="?", type=Path, default=Path(__file__).resolve().parents[1])
    args = parser.parse_args()
    try:
        source = (args.root / "build.rs").read_text(encoding="utf-8")
        manifest = tomllib.loads((args.root / "Cargo.toml").read_text(encoding="utf-8"))
        errors = check(source, manifest)
    except (OSError, ValueError, TypeError) as error:
        print(f"dependency feature preflight failed: {error}", file=sys.stderr)
        return 1
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1
    print(f"All {len(contracts(source))} explicit dependency feature contracts match")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
