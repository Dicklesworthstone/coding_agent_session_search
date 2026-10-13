#!/usr/bin/env python3
"""Apply the narrowly scoped GH521 query fix without replacing the CLI monolith.

This is a one-time, idempotent migration for API-only editing environments. It
refuses partial or unfamiliar query shapes and preserves all filters, mappings,
and output code. No database is opened or modified by this script.
"""

from pathlib import Path
import argparse

START = "fn run_timeline("
END = "\n/// Handle sources subcommands"
COUNT_OLD = "COUNT(m.id) as message_count"
COUNT_NEW = (
    "(SELECT COUNT(*) FROM messages m WHERE m.conversation_id = c.id) as message_count"
)
JOIN = "         LEFT JOIN messages m ON m.conversation_id = c.id\n"
ORDER_OLD = 'sql.push_str(" GROUP BY c.id ORDER BY c.started_at DESC");'
ORDER_NEW = 'sql.push_str(" ORDER BY c.started_at DESC");'
COMMENT = (
    "    // GH521: count only the selected conversation through the messages index.\n"
    "    // Joining messages here makes the aggregate materialize archive-wide\n"
    "    // message rows (including payloads) even for a narrow time window. Keep\n"
    "    // session-start filtering and whole-session message counts unchanged.\n"
)


def patched_source(source: str) -> str:
    if source.count(START) != 1:
        raise ValueError("expected exactly one run_timeline function")
    start = source.index(START)
    end = source.index(END, start)
    body = source[start:end]
    old = (body.count(COUNT_OLD), body.count(JOIN), body.count(ORDER_OLD))
    new = (body.count(COUNT_NEW), body.count(ORDER_NEW))
    if old == (0, 0, 0) and new == (1, 1):
        return source
    if old != (1, 1, 1) or new != (0, 0):
        raise ValueError(f"unfamiliar/partially patched timeline query: old={old}, new={new}")
    anchor = "    let mut sql = String::from(\n"
    if body.count(anchor) != 1:
        raise ValueError("expected exactly one timeline query builder")
    body = body.replace(anchor, COMMENT + anchor, 1)
    body = body.replace(COUNT_OLD, COUNT_NEW, 1)
    body = body.replace(JOIN, "", 1)
    body = body.replace(ORDER_OLD, ORDER_NEW, 1)
    return source[:start] + body + source[end:]


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("path", nargs="?", type=Path, default=Path("src/lib.rs"))
    parser.add_argument("--check", action="store_true", help="require the fix to be applied")
    args = parser.parse_args()
    original = args.path.read_text(encoding="utf-8")
    updated = patched_source(original)
    if args.check:
        if updated != original:
            parser.exit(1, "GH521 timeline fix has not been applied\n")
    elif updated != original:
        args.path.write_text(updated, encoding="utf-8")
        print(f"Applied GH521 indexed-count query fix to {args.path}")
    else:
        print("GH521 timeline fix already present; left source unchanged")


if __name__ == "__main__":
    main()
