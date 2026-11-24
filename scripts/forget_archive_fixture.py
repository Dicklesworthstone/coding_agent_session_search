"""Deterministic, disposable GH565 archive sources for the real Codex connector.

This module creates JSONL sources, never a canonical database. CASS must ingest
them through its normal indexer. Counts and UTF-8 content bytes are exact; the
conversation/body distributions are explicit synthetic assumptions, because the
reporter supplied aggregate counts, not a histogram or total message-text bytes.
The default 8 GiB of message text is an adjustable starting point, NOT a claim
that the resulting canonical database is 11.3 GB. Measure that after indexing.

Generation retains O(conversations) descriptors and one message at a time. It
does not assemble an entire rollout, message list, or archive in memory. No files
are overwritten or removed, and only a newly created or empty home is accepted.
"""
from __future__ import annotations

import hashlib
import json
from collections import Counter
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any, Iterator

SINGLE_MARKER = "cassforgetqualsingle"
BATCH_MARKER = "cassforgetqualbatch"
KEEP_MARKER = "cassforgetqualkeep"
MARKERS = {"single": SINGLE_MARKER, "batch": BATCH_MARKER, "keep": KEEP_MARKER}
MIN_MESSAGE_BYTES = 192
SINGLE_BYTES_PER_MESSAGE = 618
FIXTURE_VERSION = 1
BASE_TIME = datetime(2025, 11, 1, 10, tzinfo=timezone.utc)
MODELS = ("gpt-4.1", "gpt-5", "o3")

# These are inputs to make_plan, not promises about application performance.
SMOKE = {"conversations": 40, "messages": 600, "content_bytes": 600_000,
         "batch_conversations": 8, "batch_messages": 80,
         "batch_content_bytes": 50_000, "single_messages": 4}
# Scale the retained cohort to 10%, leaving the deletion workload unchanged.
# Scaling total counts instead would nearly double retained conversation sizes.
SMALL = {"conversations": 3_095, "messages": 214_759,
         "content_bytes": 6_702_472 + (8 * 1024**3 - 6_702_472) // 10,
         "batch_conversations": 1_064, "batch_messages": 10_839,
         "batch_content_bytes": 6_700_000, "single_messages": 4}
REPRESENTATIVE = {"conversations": 21_363, "messages": 2_050_000,
                  "content_bytes": 8 * 1024**3,
                  "batch_conversations": 1_064, "batch_messages": 10_839,
                  "batch_content_bytes": 6_700_000, "single_messages": 4}

# Body sizes are not uniform. Most are short, with medium and long tool-like
# outputs. A seed-derived rotation avoids placing all long rows at the same idx.
MESSAGE_WEIGHTS = (1,) * 240 + (8,) * 12 + (64,) * 3 + (512,)
WEIGHT_SUM = sum(MESSAGE_WEIGHTS)
VOCABULARY = tuple(f"symbol{index:04x}" for index in range(8192))
TOPICS = ("parser", "index", "commit", "cursor", "storage", "recovery", "query", "schema",
          "compile", "render", "stream", "transaction", "timeout", "checksum", "tests", "cache")


def _require(condition: bool, message: str) -> None:
    if not condition:
        raise ValueError(message)


def _json_sha256(value: Any) -> str:
    result = hashlib.sha256()
    encoder = json.JSONEncoder(sort_keys=True, separators=(",", ":"), ensure_ascii=False)
    for fragment in encoder.iterencode(value):
        result.update(fragment.encode("utf-8"))
    return result.hexdigest()


def _number(seed: int, kind: str, index: int) -> int:
    value = f"gh565-v{FIXTURE_VERSION}:{seed}:{kind}:{index}".encode("ascii")
    return int.from_bytes(hashlib.blake2b(value, digest_size=8).digest(), "big")


def _allocate(total: int, weights: list[int], minima: list[int]) -> list[int]:
    """Allocate an exact integer total by largest remainders, above hard minima."""
    _require(bool(weights) and len(weights) == len(minima), "invalid allocation dimensions")
    _require(all(weight > 0 for weight in weights), "allocation weights must be positive")
    remainder = total - sum(minima)
    _require(remainder >= 0, "requested total cannot satisfy the minimum row sizes")
    denominator = sum(weights)
    portions = [divmod(remainder * weight, denominator) for weight in weights]
    result = [minimum + portion[0] for minimum, portion in zip(minima, portions)]
    left = total - sum(result)
    order = sorted(range(len(weights)), key=lambda index: (-portions[index][1], index))
    for index in order[:left]:
        result[index] += 1
    return result


def _conversation_weights(count: int, seed: int) -> list[int]:
    """A declared long tail, stratified so scaled archives retain rare big sessions."""
    result = [0] * count
    order = sorted(range(count), key=lambda index: (_number(seed, "rank", index), index))
    for rank, index in enumerate(order):
        quantile = rank * 1000 // count
        if quantile < 550:
            lower, upper = 1, 5
        elif quantile < 800:
            lower, upper = 6, 25
        elif quantile < 950:
            lower, upper = 26, 120
        elif quantile < 995:
            lower, upper = 121, 600
        else:
            lower, upper = 601, 2500
        result[index] = lower + _number(seed, "weight", index) % (upper - lower + 1)
    return result


def _quantiles(values: list[int]) -> dict[str, int]:
    ordered = sorted(values)
    return {"min": ordered[0],
            **{name: ordered[(len(ordered) - 1) * fraction // 1000]
               for name, fraction in (("p50", 500), ("p90", 900), ("p99", 990), ("p999", 999))},
            "max": ordered[-1]}


def _histogram(values: list[int], bounds: tuple[int, ...]) -> dict[str, int]:
    histogram: Counter[str] = Counter()
    for value in values:
        previous = 0
        for bound in bounds:
            if value <= bound:
                histogram[f"{previous + 1}-{bound}"] += 1
                break
            previous = bound
        else:
            histogram[f">{bounds[-1]}"] += 1
    return dict(histogram)


def make_plan(conversations: int = 21_363, messages: int = 2_050_000,
              content_bytes: int = 8 * 1024**3, seed: int = 565,
              batch_conversations: int = 1_064, batch_messages: int = 10_839,
              batch_content_bytes: int = 6_700_000, single_messages: int = 4,
              single_content_bytes: int | None = None,
              oversized_message_bytes: int = 0) -> dict[str, Any]:
    """Plan exact cohorts without generating message text or opening any archive.

    Smaller scales keep the same deletion workloads where they fit. The optional
    oversized row is an explicit additional stress assumption; it replaces the
    last message of the largest suitable retained conversation without changing
    archive totals. It is disabled in all standard profiles.
    """
    parameters = {"conversations": conversations, "messages": messages,
                  "content_bytes": content_bytes, "seed": seed,
                  "batch_conversations": batch_conversations,
                  "batch_messages": batch_messages,
                  "batch_content_bytes": batch_content_bytes,
                  "single_messages": single_messages,
                  "single_content_bytes": (single_messages * SINGLE_BYTES_PER_MESSAGE
                                           if single_content_bytes is None else single_content_bytes),
                  "oversized_message_bytes": oversized_message_bytes}
    _require(all(type(value) is int for value in parameters.values()), "all fixture inputs must be integers")
    _require(0 <= seed < 2**64, "seed must be an unsigned 64-bit integer")
    _require(batch_conversations >= 1 and single_messages >= 2,
             "require at least one batch conversation and two single-case messages")
    keep_count = conversations - batch_conversations - 1
    _require(keep_count >= 1, "archive must contain the single, batch, and retained cohorts")
    _require(batch_messages >= batch_conversations * 2,
             "batch messages must provide at least two per conversation")
    keep_messages = messages - batch_messages - single_messages
    _require(keep_messages >= keep_count * 2,
             "total messages must leave at least two per retained conversation")
    _require(oversized_message_bytes == 0 or oversized_message_bytes >= MIN_MESSAGE_BYTES,
             "oversized message must be zero or at least the minimum message size")
    single_bytes = parameters["single_content_bytes"]
    _require(single_bytes >= single_messages * MIN_MESSAGE_BYTES,
             "single content bytes do not fit the required message prefixes")
    _require(batch_content_bytes >= batch_messages * MIN_MESSAGE_BYTES,
             "batch content bytes do not fit the required message prefixes")
    keep_bytes = content_bytes - batch_content_bytes - single_bytes
    _require(keep_bytes >= keep_messages * MIN_MESSAGE_BYTES,
             "total content bytes do not fit all requested messages")

    batch_counts = _allocate(batch_messages,
                             [1 + (_number(seed, "batch", index) % 13) ** 2
                              for index in range(batch_conversations)],
                             [2] * batch_conversations)
    keep_counts = _allocate(keep_messages, _conversation_weights(keep_count, seed), [2] * keep_count)
    cohorts = {"single": ([single_messages], single_bytes),
               "batch": (batch_counts, batch_content_bytes),
               "keep": (keep_counts, keep_bytes)}
    rows: list[dict[str, Any]] = []
    cohort_summaries: dict[str, Any] = {}
    for cohort, (counts, text_bytes) in cohorts.items():
        byte_counts = _allocate(text_bytes,
                                [count * (8 + _number(seed, f"{cohort}-bytes", index) % 9)
                                 for index, count in enumerate(counts)],
                                [count * MIN_MESSAGE_BYTES for count in counts])
        for count, byte_count in zip(counts, byte_counts):
            ordinal = len(rows)
            session = f"gh565-{seed:016x}-{ordinal:08d}"
            rows.append({"ordinal": ordinal, "cohort": cohort,
                         "session_id": session, "message_count": count,
                         "content_bytes": byte_count,
                         "expected_usage_events": count // 2,
                         "witness": f"cassforgetwitness{ordinal:08d}",
                         "marker": MARKERS[cohort],
                         "workspace": f"/work/cass-gh565/project-{_number(seed, 'workspace', ordinal) % 32:02d}",
                         "model": MODELS[_number(seed, "model", ordinal) % len(MODELS)],
                         "start_offset_seconds": (_number(seed, "day", ordinal) % 365) * 86400
                                                 + _number(seed, "clock", ordinal) % 3600,
                         "relative_path": f".codex/sessions/gh565-fixture/{cohort}/rollout-{session}.jsonl"})
        cohort_summaries[cohort] = {"conversation_count": len(counts),
                                   "message_count": sum(counts), "content_bytes": text_bytes,
                                   "expected_usage_events": sum(count // 2 for count in counts),
                                   "marker": MARKERS[cohort]}
    if oversized_message_bytes:
        eligible = [row for row in rows if row["cohort"] == "keep"
                    and row["content_bytes"] >= oversized_message_bytes
                    + (row["message_count"] - 1) * MIN_MESSAGE_BYTES]
        _require(bool(eligible), "oversized message does not fit any retained conversation")
        oversized = max(eligible, key=lambda row: (row["content_bytes"], row["ordinal"]))
        oversized["oversized_message_index"] = oversized["message_count"] - 1
        oversized["oversized_message_bytes"] = oversized_message_bytes

    plan = {"fixture_version": FIXTURE_VERSION, "parameters": parameters,
            "totals": {"conversation_count": conversations, "message_count": messages,
                       "content_bytes": content_bytes,
                       "expected_usage_events": sum(row["expected_usage_events"] for row in rows)},
            "cohorts": cohort_summaries, "conversations": rows,
            "distribution": {
                "assumption": "synthetic long tail, not the reporter's unprovided histogram",
                "conversation_count_weights": "55% 1..5; 25% 6..25; 15% 26..120; 4.5% 121..600; 0.5% 601..2500; exact-total apportionment above two messages",
                "message_size_weights": "per 256 positions: 240 x1, 12 x8, 3 x64, 1 x512; seed-rotated per conversation; exact-total allocation above 192 UTF-8 bytes",
                "message_text_volume": "configurable input assumption; canonical database bytes must be measured after real indexing",
                "conversation_messages": _quantiles([row["message_count"] for row in rows]),
                "conversation_content_bytes": _quantiles([row["content_bytes"] for row in rows]),
                "conversation_histogram": _histogram([row["message_count"] for row in rows],
                                                     (4, 16, 64, 256, 1024, 4096)),
                "date_span_days": 365, "workspace_count_max": 32,
                "models": list(MODELS), "provider": "codex"}}
    plan["plan_sha256"] = _json_sha256(plan)
    return plan


def message_sizes(row: dict[str, Any], seed: int) -> Iterator[int]:
    """Allocate each conversation's exact content budget with O(1) state."""
    count = row["message_count"]
    offset = _number(seed, "body-offset", row["ordinal"]) % len(MESSAGE_WEIGHTS)
    full, tail = divmod(count, len(MESSAGE_WEIGHTS))
    weight_sum = full * WEIGHT_SUM + sum(MESSAGE_WEIGHTS[(offset + index) % 256] for index in range(tail))
    fixed_index = row.get("oversized_message_index", -1)
    fixed_size = row.get("oversized_message_bytes", MIN_MESSAGE_BYTES)
    if fixed_index >= 0:
        weight_sum -= MESSAGE_WEIGHTS[(offset + fixed_index) % 256]
    remaining = row["content_bytes"] - count * MIN_MESSAGE_BYTES
    if fixed_index >= 0:
        remaining -= fixed_size - MIN_MESSAGE_BYTES
    consumed_weight = 0
    assigned_bytes = 0
    for index in range(count):
        if index == fixed_index:
            yield fixed_size
            continue
        consumed_weight += MESSAGE_WEIGHTS[(offset + index) % 256]
        next_bytes = remaining * consumed_weight // weight_sum
        yield MIN_MESSAGE_BYTES + next_bytes - assigned_bytes
        assigned_bytes = next_bytes


def message_text(row: dict[str, Any], index: int, size: int, seed: int) -> str:
    """Generate varied deterministic text with an exact UTF-8 byte length."""
    witness = f"{row['witness']} " if index == 0 else ""
    prefix = (f"{row['marker']} {witness}conversation{row['ordinal']:08d} "
              f"message{index:08d} UTF8 café λ. ")
    remaining = size - len(prefix.encode("utf-8"))
    _require(remaining >= 0, "message budget is smaller than its verification prefix")
    state = _number(seed, f"body-{row['ordinal']}", index)
    words = [TOPICS[state % len(TOPICS)]]
    for _ in range(32):
        state = (state * 6364136223846793005 + 1442695040888963407) & (2**64 - 1)
        words.append(VOCABULARY[(state >> 32) % len(VOCABULARY)])
    block = " ".join(words) + "\n"
    # All body bytes are ASCII; slicing cannot split a multibyte character.
    body = (block * (remaining // len(block))) + block[:remaining % len(block)]
    # Avoid normalization-by-trimming changing the declared content-byte count.
    if body:
        body = body[:-1] + "."
    return prefix + body


def _validate_destination(home: Path) -> Path:
    _require(home.is_absolute(), "fixture home must be absolute")
    _require(".." not in home.parts, "fixture home must not contain parent traversals")
    for component in (home, *home.parents):
        _require(not component.is_symlink(), f"symlink in fixture destination: {component}")
    if home.exists():
        _require(home.is_dir() and not any(home.iterdir()), "fixture home must be new or empty")
    return home


def generate_sources(home: Path, plan: dict[str, Any]) -> dict[str, Any]:
    """Write fresh sources and return their inventory; never index or delete data."""
    home = _validate_destination(Path(home))
    _require(plan.get("fixture_version") == FIXTURE_VERSION, "unsupported fixture plan version")
    _require(plan.get("plan_sha256") == _json_sha256({key: value for key, value in plan.items()
                                                    if key != "plan_sha256"}),
             "fixture plan changed after it was hashed")
    rows = plan["conversations"]
    for row in rows:
        relative = Path(row["relative_path"])
        _require(not relative.is_absolute() and ".." not in relative.parts,
                 "fixture source escaped its private home")
        _require(relative.parts[:3] == (".codex", "sessions", "gh565-fixture"),
                 "fixture source is outside the owned source subtree")
    home.mkdir(parents=True, exist_ok=True)
    source_directory = home / ".codex/sessions/gh565-fixture"
    source_directory.mkdir(parents=True, exist_ok=False)
    for cohort in MARKERS:
        (source_directory / cohort).mkdir()

    seed = plan["parameters"]["seed"]
    inventory = []
    tree_hash = hashlib.sha256()
    actual_message_bytes = 0
    actual_messages = 0
    actual_usage = 0
    max_message_bytes = 0
    message_bins: Counter[str] = Counter()
    for row in rows:
        path = home / row["relative_path"]
        source_hash = hashlib.sha256()
        content_hash = hashlib.sha256()
        conversation_bytes = 0
        start = BASE_TIME + timedelta(seconds=row["start_offset_seconds"])
        with path.open("xb") as stream:
            def emit(value: dict[str, Any]) -> None:
                raw = (json.dumps(value, ensure_ascii=False, separators=(",", ":")) + "\n").encode("utf-8")
                stream.write(raw)
                source_hash.update(raw)

            stamp = start.isoformat(timespec="milliseconds").replace("+00:00", "Z")
            emit({"timestamp": stamp, "type": "session_meta",
                  "payload": {"id": row["session_id"], "cwd": row["workspace"], "cli_version": "0.42.0"}})
            emit({"timestamp": stamp, "type": "turn_context",
                  "payload": {"cwd": row["workspace"], "model": row["model"]}})
            for index, size in enumerate(message_sizes(row, seed)):
                role = "user" if index % 2 == 0 else "assistant"
                stamp = (start + timedelta(seconds=(index + 1) * 7)).isoformat(timespec="milliseconds").replace("+00:00", "Z")
                text = message_text(row, index, size, seed)
                encoded = text.encode("utf-8")
                _require(len(encoded) == size, "generated message content-byte contract failed")
                content_hash.update(size.to_bytes(8, "big"))
                content_hash.update(encoded)
                emit({"timestamp": stamp, "type": "response_item",
                      "payload": {"type": "message", "role": role,
                                  "content": [{"type": "input_text" if role == "user" else "text",
                                               "text": text}]}})
                if role == "assistant":
                    emit({"timestamp": stamp, "type": "event_msg",
                          "payload": {"type": "token_count", "input_tokens": 100 + (index % 97),
                                      "output_tokens": max(1, size // 4)}})
                    actual_usage += 1
                conversation_bytes += size
                actual_messages += 1
                max_message_bytes = max(max_message_bytes, size)
                previous_bound = 0
                for bound in (1024, 8192, 65536, 1024**2, 8 * 1024**2):
                    if size <= bound:
                        message_bins[f"{previous_bound + 1}-{bound}"] += 1
                        break
                    previous_bound = bound
                else:
                    message_bins[">8388608"] += 1
            source_bytes = stream.tell()
        _require(conversation_bytes == row["content_bytes"], "conversation content-byte contract failed")
        actual_message_bytes += conversation_bytes
        file_hash = source_hash.hexdigest()
        tree_hash.update(f"{row['relative_path']}\0{file_hash}\n".encode("utf-8"))
        inventory.append({**row, "path": str(path), "source_bytes": source_bytes,
                          "source_sha256": file_hash, "content_sha256": content_hash.hexdigest()})
    _require(actual_messages == plan["totals"]["message_count"], "generated message total mismatch")
    _require(actual_message_bytes == plan["totals"]["content_bytes"], "generated content total mismatch")
    _require(actual_usage == plan["totals"]["expected_usage_events"], "generated usage-event total mismatch")
    cohorts = {name: {**summary, "source_directory": str(source_directory / name),
                       "paths": [row["path"] for row in inventory if row["cohort"] == name]}
               for name, summary in plan["cohorts"].items()}
    return {"fixture_version": FIXTURE_VERSION, "plan_sha256": plan["plan_sha256"],
            "parameters": plan["parameters"], "totals": plan["totals"],
            "cohorts": cohorts, "conversations": inventory,
            "distribution": {**plan["distribution"], "actual_max_message_bytes": max_message_bytes,
                             "message_size_histogram": dict(message_bins)},
            "source_tree_sha256": tree_hash.hexdigest(),
            "source_tree_hash_method": "SHA256 of relative_path, NUL, file_SHA256, newline in conversation ordinal order",
            "content_hash_method": "SHA256 of big-endian u64 UTF-8 length then content bytes, in message order",
            "source_directory": str(source_directory),
            "source_bytes": sum(row["source_bytes"] for row in inventory),
            "max_source_bytes": max(row["source_bytes"] for row in inventory),
            "single_paths": cohorts["single"]["paths"], "batch_paths": cohorts["batch"]["paths"],
            "kept_paths": cohorts["keep"]["paths"]}
