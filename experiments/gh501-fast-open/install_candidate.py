#!/usr/bin/env python3
"""Apply reviewed source edits to local checkouts, not GitHub and not a registry.

No publication, dependency override, commit, reset, or branch switch is performed.
All source transformations and conflicts are checked before any file is written.
Run on disposable worktrees for native qualification; this is an uncompiled
candidate, not a substitute for a published and pinned frankensearch release.
"""
from __future__ import annotations
import argparse
import difflib
import pathlib
import re
import sys

HERE = pathlib.Path(__file__).resolve().parent


def once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise ValueError(f"{label}: expected exactly one source anchor, found {count}")
    return text.replace(old, new, 1)


def native_edits(root: pathlib.Path) -> dict[pathlib.Path, str]:
    crate = root / "crates/frankensearch-quill/src"
    edits = {}
    for source in (HERE / "frankensearch").rglob("*.rs"):
        target = root / source.relative_to(HERE / "frankensearch")
        if target.exists():
            raise ValueError(f"Refusing to overwrite existing candidate file: {target}")
        edits[target] = source.read_text()
    lib = crate / "lib.rs"
    edits[lib] = once(lib.read_text(), "pub mod keeper;", "pub mod keeper;\nmod read_open_receipts;", str(lib))
    keeper = crate / "keeper.rs"
    text = keeper.read_text()
    # These APIs are the admission boundary reviewed against main/PR #59.
    for symbol in ("fn authenticate_segment_witness(", "fn validate_segment_witnesses(",
                   "struct AuthenticatedFileWitness", "fn from_parts(", "fn recovery_retryable("):
        if symbol not in text:
            raise ValueError(f"{keeper}: reviewed admission API absent: {symbol}")
    if "mod receipt_admission;" in text:
        raise ValueError(f"{keeper}: candidate already installed")
    text = once(text, "mod tests {", 'mod tests {\n    #[cfg(target_os = "linux")]\n    include!("keeper/receipt_regressions.rs");', str(keeper) + " native regressions")
    edits[keeper] = text + '\n// Read-only GH501 identity-receipt admission.\nmod receipt_admission;\n'
    config = crate / "config.rs"
    text = config.read_text()
    text = once(text, "    pub quarantine_on_unrepairable: bool,", """    pub quarantine_on_unrepairable: bool,
    /// Optional private (0700), owner-controlled directory for Linux-local
    /// read-only identity receipts. Only the file-prefix hash is cached;
    /// section checks remain active. None is strict and is the default.
    /// Writers, recovery and maintenance do not consult this field.
    /// Metadata-preserving storage faults require a strict open. See
    /// KeeperSnapshot::open_with_local_receipts for the trust boundary.
    pub read_open_receipt_directory: Option<std::path::PathBuf>,""", str(config))
    # The default implementation and the exact-default unit-test literal.
    anchor = "quarantine_on_unrepairable: false,"
    if text.count(anchor) != 2:
        raise ValueError(f"{config}: default/test literal drift; review all QuillConfig literals")
    text = re.sub(r"(?m)^(\s*)quarantine_on_unrepairable: false,$",
                  r"\1quarantine_on_unrepairable: false,\n\1read_open_receipt_directory: None,", text)
    edits[config] = text
    index = crate / "index.rs"
    text = index.read_text()
    text = once(text,
        "let snapshot = spawn_blocking(move || KeeperSnapshot::open(open_directory, schema)).await?;",
        """let receipt_directory = config.read_open_receipt_directory.clone();
        let snapshot = spawn_blocking(move || match receipt_directory {
            Some(cache) => KeeperSnapshot::open_with_local_receipts(open_directory, schema, cache),
            None => KeeperSnapshot::open(open_directory, schema),
        }).await?;""", str(index) + " read-only open")
    text = once(text,
        "let snapshot = spawn_blocking(move || KeeperSnapshot::open(directory, schema)).await?;",
        """let receipt_directory = self.reader.config.read_open_receipt_directory.clone();
        let snapshot = spawn_blocking(move || match receipt_directory {
            Some(cache) => KeeperSnapshot::open_with_local_receipts(directory, schema, cache),
            None => KeeperSnapshot::open(directory, schema),
        }).await?;""", str(index) + " read-only refresh")
    edits[index] = text
    return edits


def cass_edits(root: pathlib.Path) -> dict[pathlib.Path, str]:
    bridge = root / "src/search/quill_bridge.rs"
    text = bridge.read_text()
    if "open_cass_search_reader" in text:
        raise ValueError(f"{bridge}: candidate API already present")
    if "pub fn open_cass_reader(path: &Path) -> Result<QuillSearchIndex>" not in text:
        raise ValueError(f"{bridge}: strict API changed; refusing an unreviewed edit")
    edits = {bridge: text + "\n" + (HERE / "cass/search_reader.rs.inc").read_text()}
    query = root / "src/search/query.rs"
    text = query.read_text()
    # Refuse to silently retarget every call site, some of which may maintain
    # or inspect indexes. Only this named user-search admission body is edited.
    match = re.search(r"(?m)^(?P<indent>[ \t]*)(?:pub(?:\([^\n]*?\))?\s+)?(?:async\s+)?fn open_search_readers\b", text)
    if match is None:
        raise ValueError(f"{query}: search-only admission function not found")
    end = re.search(r"(?m)^" + re.escape(match.group("indent")) + r"\}", text[match.start():])
    if end is None:
        raise ValueError(f"{query}: cannot identify end of search admission function")
    stop = match.start() + end.end()
    body = text[match.start():stop]
    calls = re.findall(r"(?:[A-Za-z_][A-Za-z0-9_]*::)*open_cass_reader\(", body)
    if len(calls) != 1:
        raise ValueError(f"{query}: expected one search admission call, found {len(calls)}")
    body = once(body, calls[0],
                "crate::search::quill_bridge::open_cass_search_reader(", str(query))
    edits[query] = text[:match.start()] + body + text[stop:]
    return edits


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--frankensearch", type=pathlib.Path)
    parser.add_argument("--cass", type=pathlib.Path)
    parser.add_argument("--write", action="store_true", help="write after all edits validate; otherwise show unified diff")
    args = parser.parse_args()
    if not args.frankensearch and not args.cass:
        parser.error("select at least one local checkout")
    edits = {}
    try:
        if args.frankensearch:
            edits.update(native_edits(args.frankensearch.resolve()))
        if args.cass:
            edits.update(cass_edits(args.cass.resolve()))
        for path, new in edits.items():
            old = path.read_text() if path.exists() else ""
            sys.stdout.writelines(difflib.unified_diff(old.splitlines(True), new.splitlines(True),
                                                     fromfile=str(path), tofile=str(path)))
        if args.write:
            for path, new in edits.items():
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(new)
        return 0
    except (OSError, ValueError) as error:
        print(f"Candidate NOT applied: {error}", file=sys.stderr)
        return 1

if __name__ == "__main__":
    raise SystemExit(main())
