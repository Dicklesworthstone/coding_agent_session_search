"""Fixture contracts only: these tests do not execute or qualify CASS."""
from __future__ import annotations

import copy
import hashlib
import importlib.util
import itertools
import json
import tempfile
import tracemalloc
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "forget_archive_fixture.py"
spec = importlib.util.spec_from_file_location("forget_archive_fixture", SCRIPT)
assert spec is not None and spec.loader is not None
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class PlanTests(unittest.TestCase):
    def test_representative_counts_and_fixed_deletion_workloads_are_exact(self) -> None:
        plan = fixture.make_plan(**fixture.REPRESENTATIVE)
        rows = plan["conversations"]
        self.assertEqual(len(rows), 21_363)
        self.assertEqual(sum(row["message_count"] for row in rows), 2_050_000)
        self.assertEqual(sum(row["content_bytes"] for row in rows), 8 * 1024**3)
        for cohort, conversations, messages, content_bytes in (
                ("single", 1, 4, 2_472),
                ("batch", 1_064, 10_839, 6_700_000),
                ("keep", 20_298, 2_039_157, 8 * 1024**3 - 6_702_472)):
            selected = [row for row in rows if row["cohort"] == cohort]
            self.assertEqual(len(selected), conversations)
            self.assertEqual(sum(row["message_count"] for row in selected), messages)
            self.assertEqual(sum(row["content_bytes"] for row in selected), content_bytes)
        distribution = plan["distribution"]["conversation_messages"]
        self.assertGreater(distribution["p99"], distribution["p50"] * 10)
        self.assertGreater(plan["distribution"]["conversation_content_bytes"]["max"], 8 * 1024**2)
        self.assertEqual(len({row["witness"] for row in rows}), len(rows))
        self.assertEqual(len({row["relative_path"] for row in rows}), len(rows))

    def test_smaller_archive_preserves_fixed_deletion_workload_and_history_shape(self) -> None:
        small = fixture.make_plan(**fixture.SMALL)
        full = fixture.make_plan(**fixture.REPRESENTATIVE)
        for name in ("single", "batch"):
            self.assertEqual(small["cohorts"][name], full["cohorts"][name])
        a, b = small["cohorts"]["keep"], full["cohorts"]["keep"]
        self.assertAlmostEqual(a["conversation_count"] / b["conversation_count"], 0.1, places=3)
        self.assertAlmostEqual(a["message_count"] / b["message_count"], 0.1, places=5)
        self.assertAlmostEqual(a["content_bytes"] / b["content_bytes"], 0.1, places=8)
        self.assertAlmostEqual(a["message_count"] / a["conversation_count"],
                               b["message_count"] / b["conversation_count"], delta=0.1)

    def test_seed_controls_distribution_but_preserves_requested_totals(self) -> None:
        first = fixture.make_plan(**fixture.SMOKE, seed=565)
        repeat = fixture.make_plan(**fixture.SMOKE, seed=565)
        changed = fixture.make_plan(**fixture.SMOKE, seed=566)
        self.assertEqual(first, repeat)
        self.assertNotEqual(first["plan_sha256"], changed["plan_sha256"])
        for key in ("conversation_count", "message_count", "content_bytes"):
            self.assertEqual(first["totals"][key], changed["totals"][key])
        self.assertNotEqual([row["message_count"] for row in first["conversations"]],
                            [row["message_count"] for row in changed["conversations"]])

    def test_message_apportionment_is_exact_for_every_smoke_conversation(self) -> None:
        plan = fixture.make_plan(**fixture.SMOKE)
        for row in plan["conversations"]:
            sizes = list(fixture.message_sizes(row, 565))
            self.assertEqual(len(sizes), row["message_count"])
            self.assertEqual(sum(sizes), row["content_bytes"])
            self.assertGreaterEqual(min(sizes), fixture.MIN_MESSAGE_BYTES)

    def test_explicit_oversized_row_preserves_counts_and_content_budget(self) -> None:
        parameters = {"conversations": 3, "messages": 20, "content_bytes": 10 * 1024**2,
                      "batch_conversations": 1, "batch_messages": 4,
                      "batch_content_bytes": 2_472, "single_messages": 4,
                      "oversized_message_bytes": 9 * 1024**2}
        plan = fixture.make_plan(**parameters)
        kept = next(row for row in plan["conversations"] if row["cohort"] == "keep")
        sizes = list(fixture.message_sizes(kept, 565))
        self.assertEqual(sizes[kept["oversized_message_index"]], 9 * 1024**2)
        self.assertEqual(sum(sizes), kept["content_bytes"])
        self.assertEqual(len(sizes), 12)
        self.assertEqual(sum(row["content_bytes"] for row in plan["conversations"]), 10 * 1024**2)

    def test_size_iterator_does_not_allocate_per_message_inventory(self) -> None:
        # This measures Python allocation for producing just the first 32 sizes,
        # not RSS or memory behavior of CASS. A prebuilt ten-million-row list
        # would violate this guard before any large archive is generated.
        row = {"ordinal": 123, "message_count": 10_000_000, "content_bytes": 40_000_000_000}
        tracemalloc.start()
        try:
            sizes = list(itertools.islice(fixture.message_sizes(row, 565), 32))
            _, peak = tracemalloc.get_traced_memory()
        finally:
            tracemalloc.stop()
        self.assertEqual(len(sizes), 32)
        self.assertLess(peak, 512 * 1024)

    def test_impossible_profiles_are_rejected(self) -> None:
        for overrides in (
                {"conversations": 9}, {"batch_conversations": 0},
                {"single_messages": 1}, {"batch_messages": 8},
                {"messages": 90}, {"content_bytes": 1},
                {"batch_content_bytes": 10}, {"single_content_bytes": 10},
                {"seed": -1}, {"seed": 2**64}, {"conversations": 40.5},
                {"conversations": True}, {"oversized_message_bytes": -1},
                {"oversized_message_bytes": 100_000_000}):
            with self.subTest(overrides=overrides), self.assertRaises(ValueError):
                fixture.make_plan(**{**fixture.SMOKE, **overrides})


class SourceTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory(prefix="cass-gh565-source-test-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.plan = fixture.make_plan(**fixture.SMOKE)

    def test_jsonl_independently_matches_manifest_content_counts_usage_and_hashes(self) -> None:
        manifest = fixture.generate_sources(self.root / "home", self.plan)
        totals = {"messages": 0, "content_bytes": 0, "usage_events": 0}
        source_tree = hashlib.sha256()
        for row in manifest["conversations"]:
            messages, content_bytes, usage_events = 0, 0, 0
            content_hash, source_hash = hashlib.sha256(), hashlib.sha256()
            context_model = None
            last_role = None
            with Path(row["path"]).open("rb") as stream:
                for raw in stream:
                    source_hash.update(raw)
                    record = json.loads(raw)
                    kind, payload = record["type"], record["payload"]
                    if kind == "turn_context":
                        context_model = payload["model"]
                    elif kind == "response_item":
                        text = payload["content"][0]["text"]
                        encoded = text.encode("utf-8")
                        content_hash.update(len(encoded).to_bytes(8, "big"))
                        content_hash.update(encoded)
                        content_bytes += len(encoded)
                        self.assertIn(row["marker"], text)
                        self.assertEqual(row["witness"] in text, messages == 0)
                        self.assertIn("café λ", text)
                        self.assertEqual(text, text.strip())
                        last_role = payload["role"]
                        self.assertEqual(last_role, "user" if messages % 2 == 0 else "assistant")
                        messages += 1
                    elif kind == "event_msg":
                        self.assertEqual(payload["type"], "token_count")
                        self.assertEqual(last_role, "assistant")
                        self.assertGreater(payload["input_tokens"], 0)
                        self.assertGreater(payload["output_tokens"], 0)
                        usage_events += 1
                    else:
                        self.assertEqual(kind, "session_meta")
            self.assertEqual(context_model, row["model"])
            self.assertEqual(messages, row["message_count"])
            self.assertEqual(content_bytes, row["content_bytes"])
            self.assertEqual(usage_events, row["expected_usage_events"])
            self.assertEqual(source_hash.hexdigest(), row["source_sha256"])
            self.assertEqual(content_hash.hexdigest(), row["content_sha256"])
            source_tree.update(f"{row['relative_path']}\0{source_hash.hexdigest()}\n".encode("utf-8"))
            totals["messages"] += messages
            totals["content_bytes"] += content_bytes
            totals["usage_events"] += usage_events
        self.assertEqual(totals["messages"], 600)
        self.assertEqual(totals["content_bytes"], 600_000)
        self.assertEqual(totals["usage_events"], manifest["totals"]["expected_usage_events"])
        self.assertGreater(totals["usage_events"], 0)
        self.assertEqual(source_tree.hexdigest(), manifest["source_tree_sha256"])
        self.assertEqual(len(manifest["single_paths"]), 1)
        self.assertEqual(len(manifest["batch_paths"]), 8)
        self.assertEqual(len(manifest["kept_paths"]), 31)
        self.assertEqual(sum(manifest["distribution"]["message_size_histogram"].values()), 600)

    def test_different_destinations_generate_identical_source_bytes(self) -> None:
        left = fixture.generate_sources(self.root / "a", self.plan)
        right = fixture.generate_sources(self.root / "b", self.plan)
        self.assertEqual(left["source_tree_sha256"], right["source_tree_sha256"])
        self.assertEqual(left["plan_sha256"], right["plan_sha256"])
        self.assertEqual([row["source_sha256"] for row in left["conversations"]],
                         [row["source_sha256"] for row in right["conversations"]])

    def test_generation_never_overwrites_or_accepts_a_nonempty_home(self) -> None:
        home = self.root / "existing"
        home.mkdir()
        sentinel = home / "precious"
        sentinel.write_bytes(b"untouched\n")
        with self.assertRaisesRegex(ValueError, "new or empty"):
            fixture.generate_sources(home, self.plan)
        self.assertEqual(sentinel.read_bytes(), b"untouched\n")
        self.assertEqual(list(home.iterdir()), [sentinel])

    def test_symlink_parent_is_rejected_before_writes(self) -> None:
        target = self.root / "target"
        target.mkdir()
        alias = self.root / "alias"
        alias.symlink_to(target, target_is_directory=True)
        with self.assertRaisesRegex(ValueError, "symlink"):
            fixture.generate_sources(alias / "home", self.plan)
        self.assertEqual(list(target.iterdir()), [])

    def test_changed_plan_is_rejected_before_creating_destination(self) -> None:
        changed = copy.deepcopy(self.plan)
        changed["totals"]["message_count"] += 1
        home = self.root / "changed"
        with self.assertRaisesRegex(ValueError, "changed after it was hashed"):
            fixture.generate_sources(home, changed)
        self.assertFalse(home.exists())

    def test_rehashed_path_escape_is_still_rejected(self) -> None:
        changed = copy.deepcopy(self.plan)
        changed["conversations"][0]["relative_path"] = "../outside.jsonl"
        changed["plan_sha256"] = fixture._json_sha256(
            {key: value for key, value in changed.items() if key != "plan_sha256"})
        home = self.root / "escaped"
        with self.assertRaisesRegex(ValueError, "escaped"):
            fixture.generate_sources(home, changed)
        self.assertFalse(home.exists())
        self.assertFalse((self.root / "outside.jsonl").exists())


if __name__ == "__main__":
    unittest.main()
