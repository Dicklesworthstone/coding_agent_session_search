"""Orchestrator predicate tests, not native CASS or archive-scale measurements.

Inventories and command receipts here are deliberately small JSON inputs to the
real verification functions. No executable pretends to be CASS.
"""
from __future__ import annotations

import argparse
import copy
import hashlib
import json
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
import forget_archive_qualification as qualification


def row(index: int) -> dict:
    path = f"/synthetic-only/rollout-{index}.jsonl"
    digest = lambda label: hashlib.sha256(f"{label}-{index}".encode()).hexdigest()
    return {"id": index, "source_path": path, "external_id": f"session-{index}",
            "source_id": "local", "message_count": 3 + index,
            "content_bytes": 1024 * (3 + index), "conversation_sha256": digest("metadata"),
            "messages_sha256": digest("messages"), "token_usage_count": 3 + index,
            "token_usage_total_tokens": 100 * (3 + index), "token_usage_sha256": digest("usage")}


def table(rows: list[dict]) -> dict:
    return {"rows": copy.deepcopy(rows), "row_count": len(rows),
            "sha256": hashlib.sha256(json.dumps(rows, sort_keys=True).encode()).hexdigest()}


def snapshot(rows: list[dict], removed: list[dict] | None = None) -> dict:
    messages = sum(item["message_count"] for item in rows)
    usage = sum(item["token_usage_count"] for item in rows)
    tokens = sum(item["token_usage_total_tokens"] for item in rows)
    sources = [{"source_path": item["source_path"], "file_size": 9876 + item["id"],
                "file_mtime_ms": 1_700_000_000_000 + item["id"]}
               for item in removed or []]
    return {"schema_version": 1, "conversations": copy.deepcopy(rows),
            "canonical": {"conversations": len(rows), "messages": messages,
                          "content_bytes": sum(item["content_bytes"] for item in rows),
                          "token_usage_rows": usage, "orphan_messages": 0,
                          "orphan_token_usage_conversations": 0},
            "analytics": {
                "message_metrics": {"exists": True, "row_count": messages},
                **{name: {"exists": True, "message_count_sum": messages}
                   for name in ("daily_stats", "usage_daily", "usage_hourly", "usage_models_daily")},
                "token_usage": {"exists": True, "row_count": usage, "total_tokens_sum": tokens},
                "token_daily_stats": {"exists": True, "row_count": 4,
                                      "message_count_sum": usage, "total_tokens_sum": tokens}},
            "pending": {"effective": False, "sql_marker_present": False,
                        "sql_marker_value": None, "reader_pending": False},
            "tombstones": {"forgotten_sources": table(sources),
                           "forgotten_conversations": table([])}}


class VerificationTests(unittest.TestCase):
    def setUp(self) -> None:
        self.rows = [row(index) for index in range(1, 5)]
        self.before = snapshot(self.rows)
        self.selected = {item["source_path"] for item in self.rows[:2]}
        self.after = snapshot(self.rows[2:], self.rows[:2])

    def verify(self, after: dict | None = None, **kwargs: object) -> dict:
        return qualification.verify_snapshot(self.before, after or self.after,
                                             self.selected, **kwargs)

    def test_exact_deletion_retains_complete_unrelated_inventory(self) -> None:
        evidence = self.verify()
        self.assertEqual(evidence["deleted_conversations"], 2)
        self.assertEqual(evidence["deleted_messages"], 9)
        self.assertEqual(evidence["retained_conversations_hashed"], 2)
        self.assertEqual(evidence["retained_messages_hashed"], 13)
        self.assertEqual(evidence["tombstones"]["forgotten_sources"],
                         self.after["tombstones"]["forgotten_sources"]["sha256"])

    def test_wrong_or_missing_selected_deletion_cannot_pass(self) -> None:
        for after in (snapshot(self.rows[1:], self.rows[:1]),
                      snapshot(self.rows[:3], self.rows[3:]),
                      snapshot(self.rows[3:], self.rows[:3])):
            with self.subTest(after=after["canonical"]), self.assertRaises(RuntimeError):
                self.verify(after)

    def test_partial_interruption_accepts_only_committed_selected_subset(self) -> None:
        committed = snapshot(self.rows[1:], self.rows[:1])
        committed["pending"].update(effective=True, sql_marker_present=True, reader_pending=True)
        result = self.verify(committed, partial=True, require_repaired=False)
        self.assertEqual(result["deleted_conversations"], 1)
        with self.assertRaises(RuntimeError):
            self.verify(committed, partial=True)
        with self.assertRaises(RuntimeError):
            self.verify(snapshot(self.rows[:3], self.rows[3:]), partial=True, require_repaired=False)

    def test_every_retained_digest_identity_and_ledger_field_is_required_unchanged(self) -> None:
        for field in qualification.ROW_FIELDS:
            after = copy.deepcopy(self.after)
            old = after["conversations"][0][field]
            after["conversations"][0][field] = old + 1 if isinstance(old, int) else old + "-changed"
            with self.subTest(field=field), self.assertRaises(RuntimeError):
                self.verify(after)

    def test_wrong_canonical_aggregates_are_rejected(self) -> None:
        for field in ("conversations", "messages", "content_bytes", "token_usage_rows"):
            after = copy.deepcopy(self.after)
            after["canonical"][field] += 1
            with self.subTest(field=field), self.assertRaises(RuntimeError):
                self.verify(after)

    def test_missing_extra_or_wrong_source_tombstones_are_rejected(self) -> None:
        for tombstones in (
                table([]), table([{"source_path": self.rows[0]["source_path"]}]),
                table([{"source_path": item["source_path"]} for item in self.rows]),
                table([{"source_path": "/wrong/path"}])):
            after = copy.deepcopy(self.after)
            after["tombstones"]["forgotten_sources"] = tombstones
            with self.subTest(rows=tombstones["rows"]), self.assertRaises(RuntimeError):
                self.verify(after)

    def test_codex_file_fixture_must_not_acquire_shared_store_tombstones(self) -> None:
        after = copy.deepcopy(self.after)
        after["tombstones"]["forgotten_conversations"] = table([
            {"source_path": self.rows[0]["source_path"], "external_id": "wrong-session",
             "source_id": "wrong-origin"}])
        with self.assertRaisesRegex(RuntimeError, "shared-store"):
            self.verify(after)

    def test_pending_fences_fail_even_with_a_zero_sql_marker_value(self) -> None:
        for pending in (
                {"effective": True, "sql_marker_present": False, "reader_pending": False},
                {"effective": False, "sql_marker_present": True, "sql_marker_value": "0",
                 "reader_pending": False},
                {"effective": False, "sql_marker_present": False, "reader_pending": True}):
            after = copy.deepcopy(self.after)
            after["pending"] = pending
            with self.subTest(pending=pending), self.assertRaises(RuntimeError):
                self.verify(after)

    def test_incomplete_duplicate_and_token_rollup_totals_fail(self) -> None:
        for name, key in (
                ("message_metrics", "row_count"), ("daily_stats", "message_count_sum"),
                ("usage_daily", "message_count_sum"), ("usage_hourly", "message_count_sum"),
                ("usage_models_daily", "message_count_sum"), ("token_usage", "row_count"),
                ("token_daily_stats", "total_tokens_sum"),
                ("token_daily_stats", "message_count_sum")):
            for delta in (-1, 1):
                after = copy.deepcopy(self.after)
                after["analytics"][name][key] += delta
                with self.subTest(table=name, delta=delta), self.assertRaises(RuntimeError):
                    self.verify(after)
        after = copy.deepcopy(self.after)
        after["analytics"]["token_daily_stats"]["row_count"] = 0
        with self.assertRaises(RuntimeError):
            self.verify(after)

    def test_seed_rejects_missing_ambient_and_misparsed_source_records(self) -> None:
        manifest = {"conversations": [
            {"path": item["source_path"], "message_count": item["message_count"],
             "content_bytes": item["content_bytes"]} for item in self.rows]}
        qualification.validate_seed(self.before, manifest)
        for key, value in (("path", "/ambient/source"), ("message_count", 999),
                           ("content_bytes", 0)):
            bad = copy.deepcopy(manifest)
            bad["conversations"][0][key] = value
            with self.subTest(field=key), self.assertRaises(RuntimeError):
                qualification.validate_seed(self.before, bad)

    def test_duplicate_probe_paths_are_not_silently_collapsed(self) -> None:
        after = copy.deepcopy(self.after)
        after["conversations"][1]["source_path"] = after["conversations"][0]["source_path"]
        with self.assertRaisesRegex(RuntimeError, "multiple conversations"):
            self.verify(after)

    def test_seed_requires_complete_per_message_token_ledger(self) -> None:
        manifest = {"conversations": [
            {"path": item["source_path"], "message_count": item["message_count"],
             "content_bytes": item["content_bytes"]} for item in self.rows]}
        incomplete = copy.deepcopy(self.before)
        incomplete["canonical"]["token_usage_rows"] -= 1
        incomplete["analytics"]["token_usage"]["row_count"] -= 1
        incomplete["analytics"]["token_daily_stats"]["message_count_sum"] -= 1
        with self.assertRaises(RuntimeError):
            qualification.validate_seed(incomplete, manifest)


def ready_lexical_status(documents: int = 7) -> dict:
    return {"index": {
        "status": "ready", "exists": True, "fresh": True, "stale": False,
        "partial": False, "rebuilding": False, "stalled": False,
        "engine_incompatible": False, "hollow": False, "empty_with_messages": False,
        "manifest_source": "current", "documents": documents, "live_documents": documents,
        "checkpoint": {"present": True, "completed": True, "db_matches": True,
                       "schema_matches": True, "page_size_matches": True,
                       "page_size_compatible": True},
        "fingerprint": {"matches_current_db_fingerprint": True}},
        "budget": {"timed_out": False}}


class LexicalPublicationTests(unittest.TestCase):
    def test_complete_current_publication_and_compatible_bounded_page_size(self) -> None:
        status = ready_lexical_status()
        qualification.validate_lexical_publication(status, 7)
        status["index"]["checkpoint"]["page_size_matches"] = False
        qualification.validate_lexical_publication(status, 7)

    def test_ready_word_and_matching_checkpoint_cannot_hide_incomplete_publication(self) -> None:
        fields = {"exists": False, "fresh": False, "stale": True, "partial": True,
                  "rebuilding": True, "stalled": True, "engine_incompatible": True,
                  "hollow": True, "empty_with_messages": True, "status": "partial"}
        for field, value in fields.items():
            for wrong in (value, None):
                status = ready_lexical_status()
                status["index"][field] = wrong
                with self.subTest(field=field, value=wrong), self.assertRaises(RuntimeError):
                    qualification.validate_lexical_publication(status, 7)

    def test_old_recorded_count_does_not_substitute_for_exact_live_manifest_count(self) -> None:
        for count in (None, True, False, "7", 7.0, 0, 6, 8):
            status = ready_lexical_status()
            status["index"]["live_documents"] = count
            # The fallback documents count and completed checkpoint still look
            # correct: this is the hollow/unknown live-generation counterexample.
            with self.subTest(count=count), self.assertRaisesRegex(RuntimeError, "live MANIFEST"):
                qualification.validate_lexical_publication(status, 7)

    def test_previous_manifest_or_missing_checkpoint_authority_is_refused(self) -> None:
        for source in (None, "previous_after_missing_current", "previous_after_corrupt_current"):
            status = ready_lexical_status()
            status["index"]["manifest_source"] = source
            with self.subTest(source=source), self.assertRaisesRegex(RuntimeError, "current MANIFEST"):
                qualification.validate_lexical_publication(status, 7)
        for field in ("present", "completed", "db_matches", "schema_matches", "page_size_compatible"):
            for value in (False, None, 1):
                status = ready_lexical_status()
                status["index"]["checkpoint"][field] = value
                with self.subTest(field=field, value=value), self.assertRaises(RuntimeError):
                    qualification.validate_lexical_publication(status, 7)
        for value in (False, None, 1):
            status = ready_lexical_status()
            status["index"]["fingerprint"]["matches_current_db_fingerprint"] = value
            with self.subTest(fingerprint=value), self.assertRaisesRegex(RuntimeError, "fingerprint"):
                qualification.validate_lexical_publication(status, 7)

    def test_optional_status_budget_shedding_never_excuses_missing_core_evidence(self) -> None:
        status = ready_lexical_status()
        status["budget"] = {"timed_out": True, "skipped_sections": ["remote_sync", "doctor_summary"]}
        qualification.validate_lexical_publication(status, 7)
        for field in ("checkpoint", "fingerprint", "live_documents"):
            incomplete = copy.deepcopy(status)
            del incomplete["index"][field]
            with self.subTest(field=field), self.assertRaises(RuntimeError):
                qualification.validate_lexical_publication(incomplete, 7)
        for incomplete in ({}, {"index": None}, {"index": []}):
            with self.subTest(status=incomplete), self.assertRaises(RuntimeError):
                qualification.validate_lexical_publication(incomplete, 7)


class ObservationReceiptTests(unittest.TestCase):
    @staticmethod
    def interrupted() -> dict:
        return {"status": "requested-interruption", "exit_code": -9, "timed_out": False,
                "memory": {"limited": False, "events_delta": None},
                "containment": {"scope": "process-group", "quiescent": True,
                                "observed_at_seconds": 1.0, "cleanup_timed_out": False,
                                "cleanup_timeout_seconds": 5.0, "cgroup_populated": None,
                                "remaining_pids": []},
                "interruption": {"requested_phase": "analytics", "phase_observed": True,
                                 "sent": True, "last_observed_phase": "analytics"}}

    def test_requested_signal_requires_its_own_clean_terminal_status(self) -> None:
        receipt = self.interrupted()
        self.assertTrue(qualification.clean_requested_interruption(receipt, "analytics"))
        self.assertFalse(qualification.successful(receipt))
        self.assertFalse(qualification.clean_requested_interruption(receipt, "canonical-delete"))
        for changes in ({"status": "exited"}, {"status": "observer-error"}, {"exit_code": 0},
                        {"exit_code": -15}, {"timed_out": True}, {"output_limit_exceeded": True},
                        {"output_pipes_remained_open_after_kill": True},
                        {"descendants_terminated_after_root_exit": True}, {"error": "observer failed"},
                        {"memory": {"limited": False, "events_delta": {"oom_kill": 1}}}):
            with self.subTest(changes=changes):
                self.assertFalse(qualification.clean_requested_interruption(
                    {**receipt, **changes}, "analytics"))
        for field in ("sent", "phase_observed", "requested_phase"):
            bad = copy.deepcopy(receipt)
            bad["interruption"].pop(field)
            with self.subTest(missing=field):
                self.assertFalse(qualification.clean_requested_interruption(bad, "analytics"))
        # A clean signal whose phase moved remains eligible for preservation
        # and recovery; run_trial separately marks the missed phase inconclusive.
        receipt["interruption"]["last_observed_phase"] = "lexical-index"
        self.assertTrue(qualification.clean_requested_interruption(receipt, "analytics"))

    def test_missing_failed_or_unobserved_containment_is_never_quiescence(self) -> None:
        receipt = self.interrupted()
        for containment in (None, {}, {**receipt["containment"], "quiescent": False},
                            {**receipt["containment"], "cleanup_timed_out": True},
                            {**receipt["containment"], "remaining_pids": [1234]},
                            {**receipt["containment"], "observed_at_seconds": None},
                            {**receipt["containment"], "observed_at_seconds": float("inf")},
                            {**receipt["containment"], "scope": "unobserved"}):
            with self.subTest(containment=containment):
                bad = {**receipt, "containment": containment}
                self.assertFalse(qualification.confirmed_quiescence(bad))
                self.assertFalse(qualification.clean_requested_interruption(bad, "analytics"))

    def test_capped_scope_requires_recursive_empty_state_and_final_oom_counters(self) -> None:
        receipt = self.interrupted()
        receipt["memory"] = {"limited": True, "events_delta": {"oom_kill": 0},
                             "final_observed_after_quiescence": True}
        receipt["containment"].update(scope="cgroup-v2", cgroup_populated=0)
        self.assertTrue(qualification.clean_requested_interruption(receipt, "analytics"))
        for changes in ({"scope": "process-group"}, {"cgroup_populated": None},
                        {"cgroup_populated": 1}, {"cgroup_populated": False}):
            bad = copy.deepcopy(receipt)
            bad["containment"].update(changes)
            with self.subTest(changes=changes):
                self.assertFalse(qualification.clean_requested_interruption(bad, "analytics"))
        for changes in ({"final_observed_after_quiescence": False}, {"events_delta": None},
                        {"events_delta": {}}, {"events_delta": {"oom_kill": 1}}):
            bad = copy.deepcopy(receipt)
            bad["memory"].update(changes)
            with self.subTest(changes=changes):
                self.assertTrue(qualification.confirmed_quiescence(bad))
                self.assertFalse(qualification.clean_requested_interruption(bad, "analytics"))


class ReceiptAdapter:
    """Feed a stored JSON receipt into a predicate; does not start a process."""
    def __init__(self, receipt: dict):
        self.receipt = receipt
        self.calls: list[tuple[list[str], str]] = []

    def cass(self, argv: list[str], label: str) -> dict:
        self.calls.append((argv, label))
        return self.receipt

    json = qualification.Commands.json


class SearchReceiptTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory(prefix="cass-gh565-receipt-test-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.count = 0

    def receipt(self, code: int = 0, payload: dict | None = None,
                kind: str | None = None, **extra: object) -> dict:
        self.count += 1
        stdout = self.root / f"{self.count}.stdout"
        stderr = self.root / f"{self.count}.stderr"
        stdout.write_text(json.dumps({"hits": []} if payload is None else payload)
                          if code == 0 or payload is not None else "")
        stderr.write_text(json.dumps({"error": {"kind": kind}}) + "\n" if kind else "")
        return {"status": "exited", "exit_code": code, "timed_out": False,
                "stdout_path": str(stdout), "stderr_path": str(stderr),
                "memory": {"limited": False, "events_delta": {"oom_kill": 0}},
                "containment": {"scope": "process-group", "quiescent": True,
                                "observed_at_seconds": 1.0, "cleanup_timed_out": False,
                                "cleanup_timeout_seconds": 5.0, "cgroup_populated": None,
                                "remaining_pids": []}, **extra}

    def query(self, receipt: dict, mode: str = "lexical", pending: bool = False):
        return qualification.search(ReceiptAdapter(receipt), "syntheticmarker", mode,
                                    "predicate-only", pending=pending)

    def test_every_search_surface_including_pending_refusal_disallows_maintenance(self) -> None:
        for mode in qualification.legacy.SURFACES:
            for pending in (False, True):
                receipt = (self.receipt(code=9, kind="forget-repair-pending")
                           if pending else self.receipt())
                adapter = ReceiptAdapter(receipt)
                qualification.search(adapter, "syntheticmarker", mode, "read-only-check", pending=pending)
                with self.subTest(mode=mode, pending=pending):
                    self.assertEqual(len(adapter.calls), 1)
                    argv, label = adapter.calls[0]
                    self.assertEqual(argv[:2], ["search", "syntheticmarker"])
                    self.assertIn("--no-maintenance", argv)
                    self.assertNotIn("--refresh", argv)
                    self.assertNotIn("--daemon", argv)
                    self.assertEqual(label, "read-only-check")

    def test_publication_evidence_uses_status_and_retains_its_receipt(self) -> None:
        payload = ready_lexical_status()
        receipt = self.receipt(payload=payload)
        adapter = ReceiptAdapter(receipt)
        evidence = qualification.lexical_publication_checks(adapter, 7, "after-repair")
        self.assertEqual(adapter.calls, [(["status", "--json"], "after-repair")])
        self.assertEqual(evidence["measurement"], receipt)
        self.assertEqual(evidence["application_report"], payload)
        self.assertEqual(evidence["expected_live_documents"], 7)
        self.assertIs(evidence["included_in_forget_timing"], False)

    def test_status_process_failure_cannot_be_accepted_as_budget_shedding(self) -> None:
        for flags in ({"timed_out": True}, {"exit_code": 1}, {"output_limit_exceeded": True},
                      {"memory": {"events_delta": {"oom_kill": 1}}}):
            adapter = ReceiptAdapter(self.receipt(payload=ready_lexical_status(), **flags))
            with self.subTest(flags=flags), self.assertRaises(RuntimeError):
                qualification.lexical_publication_checks(adapter, 7, "status-process-failed")

    def test_successful_empty_search_is_distinct_from_failed_or_timed_out_search(self) -> None:
        self.assertEqual(self.query(self.receipt()), [])
        for receipt in (
                self.receipt(payload={"hits": [], "budget": {"timed_out": True}}),
                self.receipt(code=9, kind="io"),
                self.receipt(timed_out=True),
                self.receipt(memory={"events_delta": {"oom_kill": 1}}),
                self.receipt(output_limit_exceeded=True),
                self.receipt(descendants_terminated_after_root_exit=True),
                self.receipt(output_pipes_remained_open_after_kill=True)):
            with self.subTest(receipt=receipt), self.assertRaises(RuntimeError):
                self.query(receipt)

    def test_semantic_unavailability_must_have_exact_kind_and_surface(self) -> None:
        refused = self.receipt(code=15, kind="semantic-unavailable")
        self.assertIsNone(self.query(refused, mode="semantic"))
        with self.assertRaises(RuntimeError):
            self.query(refused, mode="lexical")
        with self.assertRaises(RuntimeError):
            self.query(self.receipt(code=15, kind="other-failure"), mode="semantic")

    def test_pending_reader_must_refuse_with_exact_privacy_error(self) -> None:
        self.assertIsNone(self.query(self.receipt(code=9, kind="forget-repair-pending"), pending=True))
        for receipt in (self.receipt(), self.receipt(code=9, kind="missing-index"),
                        self.receipt(code=15, kind="semantic-unavailable")):
            with self.subTest(receipt=receipt), self.assertRaises(RuntimeError):
                self.query(receipt, pending=True)

    def test_tainted_expected_refusal_receipt_is_never_accepted(self) -> None:
        for flags in ({"timed_out": True}, {"output_limit_exceeded": True},
                      {"status": "observation_failed"},
                      {"memory": {"events_delta": {"oom_kill": 1}}},
                      {"descendants_terminated_after_root_exit": True},
                      {"output_pipes_remained_open_after_kill": True}):
            for code, kind, mode, pending in (
                    (9, "forget-repair-pending", "lexical", True),
                    (15, "semantic-unavailable", "semantic", False)):
                receipt = self.receipt(code=code, kind=kind, **flags)
                with self.subTest(flags=flags, kind=kind), self.assertRaises(RuntimeError):
                    self.query(receipt, mode=mode, pending=pending)

    def test_privacy_refusal_cannot_emit_content_before_its_error(self) -> None:
        leaked = {"hits": [{"source_path": "/forgotten/source", "content": "forgotten text"}]}
        for code, kind, mode, pending in (
                (9, "forget-repair-pending", "lexical", True),
                (15, "semantic-unavailable", "semantic", False)):
            receipt = self.receipt(code=code, kind=kind, payload=leaked)
            with self.subTest(kind=kind), self.assertRaisesRegex(RuntimeError, "emitted data"):
                self.query(receipt, mode=mode, pending=pending)


class ScalingTests(unittest.TestCase):
    @staticmethod
    def trial(name: str, rss: int, wall: int, cap: int = 8 * 1024**3,
              status: str = "passed", interruption: str | None = None) -> dict:
        return {"name": name, "case": "single", "memory_max": cap, "status": status,
                "interruption_phase": interruption,
                "measurement": {"peak_rss_kib": rss, "process_wall_ms": wall,
                                "parent_rss_bytes_at_launch": rss * 100,
                                "parent_hwm_bytes_at_launch": rss * 120,
                                "sampled_root_hwm_peak_bytes": rss * 1000,
                                "memory": {"peak_bytes": rss * 1024 + 1000}}}

    def test_failed_and_interrupted_runs_do_not_inflate_successful_scaling(self) -> None:
        failed = self.trial("oom", 99_999_999, 999_999, status="failed")
        interrupted = self.trial("recovery", 99_999_999, 999_999, interruption="analytics")
        profiles = [
            {"profile": "small", "database_bytes": 1000, "seed": {"canonical": {"messages": 100}},
             "trials": [self.trial("a", 100, 1000), self.trial("b", 120, 1200)]},
            {"profile": "representative", "database_bytes": 10000,
             "seed": {"canonical": {"messages": 1000}},
             "trials": [self.trial("c", 220, 2200), failed, interrupted]}]
        summary = qualification.scaling_summary(profiles)
        comparison = summary["comparisons"][0]
        self.assertEqual(comparison["status"], "measured")
        self.assertEqual(comparison["points"][0]["rss_kib"], 110)
        self.assertEqual(comparison["largest_over_smallest"]["rss_kib"], 2)
        self.assertEqual(comparison["largest_over_smallest"]["messages"], 10)
        self.assertEqual(len(summary["failed_or_inconclusive_trials"]), 1)
        self.assertEqual(summary["failed_or_inconclusive_trials"][0]["measurement"],
                         failed["measurement"])

    def test_different_memory_caps_cannot_be_compared_as_one_scaling_curve(self) -> None:
        profiles = [
            {"profile": "small", "database_bytes": 1000, "seed": {"canonical": {"messages": 100}},
             "trials": [self.trial("a", 100, 1000, cap=1024**3)]},
            {"profile": "representative", "database_bytes": 10000,
             "seed": {"canonical": {"messages": 1000}},
             "trials": [self.trial("b", 200, 2000, cap=2 * 1024**3)]}]
        summary = qualification.scaling_summary(profiles)
        self.assertEqual(len(summary["comparisons"]), 2)
        self.assertTrue(all(row["status"] == "insufficient_sizes" for row in summary["comparisons"]))

    def test_startup_floor_uncertainty_retains_raw_peaks_but_withholds_rss_slope(self) -> None:
        for parent_rss in (None, 205000):
            a = self.trial("small", 100, 1000)
            b = self.trial("representative", 200, 2000)
            b["measurement"]["parent_rss_bytes_at_launch"] = parent_rss
            profiles = [
                {"profile": "small", "database_bytes": 1000,
                 "seed": {"canonical": {"messages": 100}}, "trials": [a]},
                {"profile": "representative", "database_bytes": 10000,
                 "seed": {"canonical": {"messages": 1000}}, "trials": [b]}]
            comparison = qualification.scaling_summary(profiles)["comparisons"][0]
            with self.subTest(parent_rss=parent_rss):
                self.assertEqual(comparison["status"], "rss_startup_floor_uncertain")
                self.assertEqual(comparison["points"][1]["rss_kib"], 200)
                self.assertEqual(comparison["largest_over_smallest"]["rss_kib"], 2)
                self.assertNotIn("empirical_rss_log_slope", comparison)


class InvocationContractTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory(prefix="cass-gh565-identity-test-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()

    def test_uncapped_execution_is_explicitly_smoke_only(self) -> None:
        allowed = qualification.parse_args(["--output-dir", str(self.root / "run"),
                                             "--source-revision", "a" * 40,
                                             "--profile", "smoke", "--allow-uncapped"])
        self.assertTrue(allowed.allow_uncapped)
        for flags in ([], ["--allow-uncapped"],
                      ["--profile", "representative", "--allow-uncapped"],
                      ["--profile", "smoke", "--allow-uncapped", "--memory-max", "1GiB"],
                      ["--profile", "smoke", "--allow-uncapped",
                       "--verification-memory-max", "1GiB"]):
            with self.subTest(flags=flags), self.assertRaises(RuntimeError):
                qualification.parse_args(["--output-dir", str(self.root / "unused"),
                                           "--source-revision", "a" * 40, *flags])

    def test_native_build_revision_is_explicit_and_not_the_inspected_baseline(self) -> None:
        plan = qualification.parse_args(["--output-dir", str(self.root / "plan"), "--plan-only"])
        self.assertIsNone(plan.source_revision)
        flags = ["--output-dir", str(self.root / "native"), "--profile", "smoke", "--allow-uncapped"]
        with self.assertRaisesRegex(RuntimeError, "require --source-revision"):
            qualification.parse_args(flags)
        actual = qualification.parse_args([*flags, "--source-revision", "a" * 40])
        self.assertEqual(actual.source_revision, "a" * 40)
        self.assertNotEqual(actual.source_revision, qualification.INSPECTED_APPLICATION_BASELINE)

    def test_memory_limit_values_and_duplicate_effective_caps(self) -> None:
        args = qualification.parse_args([
            "--output-dir", str(self.root / "plan"), "--plan-only",
            "--memory-max", "1GiB", "--memory-max", "2GB",
            "--verification-memory-max", "4GiB"])
        self.assertEqual(args.memory_max, [1024**3, 2_000_000_000])
        self.assertEqual(args.verification_memory_max, 4 * 1024**3)
        with self.assertRaisesRegex(RuntimeError, "duplicate memory"):
            qualification.parse_args(["--output-dir", str(self.root / "duplicate"), "--plan-only",
                                       "--memory-max", "1GiB", "--memory-max", "1024MiB"])

    def test_smoke_default_recovery_and_comparable_archive_workloads(self) -> None:
        smoke = qualification.parse_args(["--output-dir", str(self.root / "smoke"),
                                           "--profile", "smoke", "--plan-only"])
        self.assertEqual(smoke.recovery_phase, ["analytics", "lexical-index"])
        scale = qualification.parse_args(["--output-dir", str(self.root / "scale"), "--plan-only"])
        self.assertEqual(scale.recovery_phase, ["canonical-delete", "analytics", "lexical-index"])
        with self.assertRaisesRegex(RuntimeError, "smoke cannot"):
            qualification.parse_args(["--output-dir", str(self.root / "mixed"), "--plan-only",
                                       "--profile", "smoke", "--profile", "representative"])

    def test_nonfinite_parameters_and_ambiguous_source_revision_are_refused(self) -> None:
        for flags in (["--timeout", "nan"], ["--sample-interval", "inf"],
                      ["--database-size-tolerance", "nan"], ["--repetitions", "0"],
                      ["--source-revision", "7104358"],
                      ["--profile", "smoke", "--profile", "smoke"]):
            with self.subTest(flags=flags), self.assertRaises(RuntimeError):
                qualification.parse_args(["--output-dir", str(self.root / "invalid"),
                                           "--plan-only", *flags])

    def identity_args(self, profile: list[str], receipt: Path | None = None) -> argparse.Namespace:
        # Hash an existing native interpreter as inert bytes. It is NEVER run as
        # CASS; this test checks only the identity/receipt matching predicate.
        binary = Path(sys.executable).resolve()
        return argparse.Namespace(cass=binary, probe=binary,
                                  source_revision=qualification.INSPECTED_APPLICATION_BASELINE,
                                  build_description="identity predicate test only",
                                  build_receipt=receipt, profile=profile)

    @unittest.skipUnless(sys.platform.startswith("linux"), "native identity is Linux-only")
    def test_archive_scale_requires_a_matching_binary_source_receipt(self) -> None:
        with self.assertRaisesRegex(RuntimeError, "build-receipt"):
            qualification.native_identity(self.identity_args(["small", "representative"]))
        smoke = qualification.native_identity(self.identity_args(["smoke"]))
        self.assertEqual(smoke["source_binding"], "operator_asserted")
        identity = {key: smoke[key] for key in ("source_revision", "cass_sha256", "probe_sha256")}
        path = self.root / "build-receipt.json"
        path.write_text(json.dumps(identity))
        result = qualification.native_identity(self.identity_args(["small", "representative"], path))
        self.assertEqual(result["source_binding"], "supplied_build_receipt_matches_hashes")
        for key in identity:
            bad = dict(identity)
            bad[key] = "0" * (40 if key == "source_revision" else 64)
            bad_path = self.root / f"bad-{key}.json"
            bad_path.write_text(json.dumps(bad))
            with self.subTest(field=key), self.assertRaisesRegex(RuntimeError, "mismatch"):
                qualification.native_identity(self.identity_args(["representative"], bad_path))

    def test_existing_destination_and_symlink_parents_are_refused(self) -> None:
        existing = self.root / "existing"
        existing.mkdir()
        sentinel = existing / "precious"
        sentinel.write_bytes(b"preserved")
        with self.assertRaises(FileExistsError):
            qualification.new_root(existing)
        alias = self.root / "alias"
        alias.symlink_to(existing, target_is_directory=True)
        with self.assertRaisesRegex(RuntimeError, "symlink"):
            qualification.new_root(alias / "child")
        self.assertEqual(sentinel.read_bytes(), b"preserved")
        self.assertFalse((existing / "child").exists())


if __name__ == "__main__":
    unittest.main()
