"""Harness-only recovery control-flow and receipt tests, never CASS proof.

The command adapter supplies small recorded-shape JSON inputs in-process. No
executable pretends to be CASS, no SQLite database is opened, and no application
performance is measured. GNU cp exercises actual independent evidence copies.
All test artifacts are retained; the suite does not clean up its directories.
"""
from __future__ import annotations

import argparse
import copy
import hashlib
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
import forget_archive_qualification as qualification

ARTIFACTS = Path(tempfile.mkdtemp(prefix="cass-forget-recovery-tests-"))


def digest(value: object) -> str:
    return hashlib.sha256(json.dumps(value, sort_keys=True).encode()).hexdigest()


def track_b_receipt(rows: list[dict]) -> dict:
    """Literal schema input, not a simulated native rollup computation."""
    expected = digest(["track-b-oracle", rows])
    return {"schema_version": 1, "method": "canonical-ledger-conversations-v1",
            "key_columns": ["day_id", "agent_slug", "source_id", "model_family"],
            "verified_columns": ["api_call_count", "user_message_count", "assistant_message_count",
                "tool_message_count", "total_input_tokens", "total_output_tokens",
                "total_cache_read_tokens", "total_cache_creation_tokens", "total_thinking_tokens",
                "grand_total_tokens", "total_content_chars", "total_tool_calls", "session_count"],
            "ignored_columns": ["estimated_cost_usd", "last_updated"],
            "expected_rows": 5, "actual_rows": 5,
            "expected_sha256": expected, "actual_sha256": expected,
            "matches": True, "mismatch_count": 0, "mismatch_examples": [],
            "limits": {"max_buckets": 65536, "max_dimension_utf8_bytes": 256, "page_rows": 64}}


def wrong_sessions(snapshot: dict) -> None:
    snapshot["track_b_oracle"].update(matches=False, mismatch_count=1,
        actual_sha256=digest("all session counters lost"),
        mismatch_examples=[{"kind": "different",
            "key": {"day_id": 2500, "agent_slug": "all", "source_id": "all", "model_family": "all"},
            "differing_fields": ["session_count"]}])


def inventory(rows: list[dict], removed: list[dict] = (), *, pending: bool = False) -> dict:
    messages = sum(row["message_count"] for row in rows)
    tokens = sum(row["token_usage_total_tokens"] for row in rows)
    tombstones = [{"source_path": row["source_path"], "size_bytes": 1234,
                   "mtime_ms": 1700000000000, "forgotten_at_ms": 1700000001000}
                  for row in removed]
    return {"schema_version": 2, "conversations": copy.deepcopy(rows),
            "canonical": {"conversations": len(rows), "messages": messages,
                          "content_bytes": sum(row["content_bytes"] for row in rows),
                          "token_usage_rows": messages, "orphan_messages": 0,
                          "orphan_token_usage_conversations": 0},
            "pending": {"effective": pending, "sql_marker_present": pending,
                        "reader_pending": pending},
            "analytics": {"message_metrics": {"exists": True, "row_count": messages},
                **{name: {"exists": True, "message_count_sum": messages}
                   for name in ("daily_stats", "usage_daily", "usage_hourly", "usage_models_daily")},
                "token_usage": {"exists": True, "row_count": messages, "total_tokens_sum": tokens},
                "token_daily_stats": {"exists": True, "row_count": 5,
                                      "total_tokens_sum": tokens, "message_count_sum": messages}},
            "track_b_oracle": track_b_receipt(rows),
            "canonical_catalogs": {name: {"row_count": count, "sha256": digest([name, "unchanged"])}
                                   for name, count in (("agents", 1), ("workspaces", 4), ("sources", 1))},
            "canonical_catalog_limits": {"max_rows_per_table": 4096,
                                         "max_value_bytes_per_table": 8388608},
            "tombstones": {"forgotten_sources": {"rows": tombstones, "sha256": digest(tombstones)},
                           "forgotten_conversations": {"rows": [], "sha256": digest([])}}}


def lexical_publication(messages: int) -> dict:
    """Recorded-shape status input for driver ordering/count predicates only."""
    return {"index": {
        "status": "ready", "exists": True, "fresh": True, "stale": False,
        "partial": False, "rebuilding": False, "stalled": False,
        "engine_incompatible": False, "hollow": False, "empty_with_messages": False,
        "manifest_source": "current", "documents": messages, "live_documents": messages,
        "checkpoint": {"present": True, "completed": True, "db_matches": True,
                       "schema_matches": True, "page_size_matches": False,
                       "page_size_compatible": True},
        "fingerprint": {"matches_current_db_fingerprint": True}},
        "budget": {"timed_out": False}}


class ReceiptCommands(qualification.Commands):
    """Feed receipts to the real driver predicates without launching a binary."""
    def __init__(self, *args, scenario: dict, **kwargs):
        super().__init__(*args, **kwargs)
        self.scenario = scenario
        self.calls: list[dict] = []
        self.admitted = False
        self.interrupted = False
        self.recovered = False
        self.saved_copy_seen_before_probe = False

    def _execute(self, binary: Path, argv: list[str], label: str, **kwargs) -> dict:
        # Keep Commands.invoke's actual quiescence state tracking and admission
        # guard. Only the leaf process execution is replaced by receipt input.
        self.calls.append({"label": label, "argv": list(argv), **kwargs})
        status, code, payload, error = "exited", 0, {}, ""
        extra = {}
        if label == "admit-restored-seed":
            self.admitted = True
            payload = {"harness_only": True, "operation": "copied-seed-index-admission"}
        elif label in ("restored-seed-lexical-publication", "after-repair-lexical-publication",
                       "after-rescan-lexical-publication"):
            baseline = "seed" if label == "restored-seed-lexical-publication" else "after"
            payload = self.scenario.get("lexical_publications", {}).get(
                label, lexical_publication(self.scenario[baseline]["canonical"]["messages"]))
        elif label == "restored-seed":
            if not self.admitted:
                raise AssertionError("strict probe ran before copied-seed writer admission")
            payload = self.scenario.get("restored", self.scenario["seed"])
        elif label == "preview":
            payload = {"conversations_matched": 2, "messages_matched": 4}
        elif label == "forget":
            payload = {"applied": True, "conversations_deleted": 2, "messages_matched": 4,
                       "derived_rebuilt": True, "rebuild_profile": {"name": "bounded"}}
            extra["phases"] = [{"phase": phase} for phase in qualification.PHASES]
        elif label == "interrupted-forget":
            self.interrupted = True
            for relative in self.scenario["bundle_files"]:
                (self.data / relative).write_bytes(b"interrupted:" + relative.encode())
            status, code = "requested-interruption", -9
            extra["interruption"] = {"sent": True, "phase_observed": True,
                                     "requested_phase": kwargs["interrupt_phase"],
                                     "last_observed_phase": kwargs["interrupt_phase"]}
        elif label == "interrupted-canonical":
            saved = self.root / "interrupted-archive" / "agent_search.db"
            self.saved_copy_seen_before_probe = saved.read_bytes() == b"interrupted:agent_search.db"
            if not self.saved_copy_seen_before_probe:
                raise AssertionError("interrupted evidence was not preserved before the strict probe")
            if self.scenario.get("probe_unavailable", True):
                code, error = 1, "Error: BusyRecovery (harness input, not a native result)\n"
            else:
                payload = self.scenario["interrupted_snapshot"]
        elif label == "recovery":
            self.recovered = True
            for relative in self.scenario["bundle_files"]:
                (self.data / relative).write_bytes(b"recovered:" + relative.encode())
            code = self.scenario.get("recovery_exit", 0)
            if code:
                error = "harness input: recovery failed\n"
        elif label in ("after-repair", "reopened-after-noop", "after-full-rescan"):
            payload = self.scenario.get("snapshots", {}).get(label, self.scenario["after"])
        elif label == "same-selector-noop":
            payload = {"conversations_deleted": 0, "derived_rebuilt": False}
        elif label == "full-rescan":
            payload = {"harness_only": True}
        elif label == "target-positive-control":
            payload = {"hits": [{"source_path": self.scenario["rows"][0]["source_path"],
                                  "content": "cassforgetqualbatch"}]}
        elif label.startswith("search-"):
            if self.interrupted and not self.recovered:
                if self.scenario.get("privacy_leak"):
                    payload = {"hits": [{"source_path": self.scenario["rows"][0]["source_path"],
                                          "content": "cassforgetqualbatch"}]}
                else:
                    code, error = 9, json.dumps({"error": {"kind": "forget-repair-pending"}}) + "\n"
            else:
                payload = {"hits": []}
        elif label.startswith("kept-witness-"):
            row = next(row for row in self.scenario["manifest"]["conversations"]
                       if row.get("witness") == argv[1])
            payload = {"hits": [{"source_path": row["path"], "content": row["witness"]}]}
        else:
            raise AssertionError(f"unexpected receipt request {label}")
        stem = self.logs / f"{self.sequence:04d}-{label}"
        stdout, stderr = Path(f"{stem}.stdout"), Path(f"{stem}.stderr")
        stdout.write_text(json.dumps(payload) if code == 0 else "", encoding="utf-8")
        stderr.write_text(error, encoding="utf-8")
        receipt = {"status": status, "exit_code": code, "timed_out": False,
                   "stdout_path": str(stdout), "stderr_path": str(stderr),
                   "receipt_path": f"{stem}.receipt.json", "harness_only": True,
                   "memory": {"limited": False, "events_delta": {"oom_kill": 0}},
                   "containment": {"scope": "process-group", "quiescent": True,
                                   "observed_at_seconds": 1.0, "cleanup_timed_out": False,
                                   "cleanup_timeout_seconds": 5.0, "cgroup_populated": None,
                                   "remaining_pids": []}, **extra}
        receipt.update(copy.deepcopy(self.scenario.get("receipt_overrides", {}).get(label, {})))
        qualification.save(Path(receipt["receipt_path"]), receipt)
        if label in self.scenario.get("process_error_labels", ()):
            raise qualification.process.ProcessSetupError("harness input: observer failed", receipt)
        return receipt


class RecoveryFlowTests(unittest.TestCase):
    def setUp(self) -> None:
        self.root = ARTIFACTS / self.id().split(".")[-1]
        self.root.mkdir()

    def scenario(self, name: str = "case") -> tuple[Path, Path, dict]:
        profile = self.root / name
        home, seed = profile / "home", profile / "seed-data"
        home.mkdir(parents=True)
        (seed / "index").mkdir(parents=True)
        bundle = ("agent_search.db", "agent_search.db-wal", "agent_search.db-shm", "index/forget-state.json")
        for relative in bundle:
            (seed / relative).write_bytes(b"seed:" + relative.encode())
        qualification.save(seed / ".gh565-synthetic-fixture.json", qualification.SENTINEL)
        rows = [{"id": index, "source_path": str(home / f"rollout-{index}.jsonl"),
                 "external_id": f"session-{index}", "source_id": "local",
                 "message_count": 2, "content_bytes": 100 + index,
                 "conversation_sha256": digest(["conversation", index]),
                 "messages_sha256": digest(["messages", index]),
                 "token_usage_count": 2, "token_usage_total_tokens": 50 + index,
                 "token_usage_sha256": digest(["tokens", index])} for index in range(1, 5)]
        manifest = {"cohorts": {"batch": {"paths": [row["source_path"] for row in rows[:2]],
                                          "source_directory": str(home), "marker": "cassforgetqualbatch"}},
                    "conversations": [{"path": row["source_path"],
                                       "cohort": "batch" if index < 2 else "keep",
                                       "witness": f"keptwitness{index}"}
                                      for index, row in enumerate(rows)]}
        return profile, home, {"seed": inventory(rows), "rows": rows,
                               "after": inventory(rows[2:], rows[:2]), "manifest": manifest,
                               "bundle_files": bundle}

    def run_scenario(self, profile: Path, home: Path, scenario: dict,
                     phase: str | None = "analytics") -> tuple[dict, ReceiptCommands]:
        args = argparse.Namespace(verification_memory_max=None, memory_max=[],
                                  fts_shadow_max_messages=None, quick_check=False,
                                  canonical_interrupt_delay=0.01,
                                  cass=Path("/not-executed/cass"), probe=Path("/not-executed/probe"))
        adapters = []
        def adapter(*args, **kwargs):
            commands = ReceiptCommands(*args, scenario=scenario, **kwargs)
            adapters.append(commands)
            return commands
        with mock.patch.object(qualification, "Commands", side_effect=adapter):
            result = qualification.run_trial(profile, home, scenario["manifest"], scenario["seed"],
                                             args, None, "batch", "trial", phase)
        return result, adapters[0]

    def test_tainted_interruption_stops_before_copy_probe_or_recovery(self) -> None:
        variants = ({"timed_out": True}, {"output_limit_exceeded": True},
                    {"output_pipes_remained_open_after_kill": True},
                    {"descendants_terminated_after_root_exit": True},
                    {"status": "observer-error"}, {"status": "exited"},
                    {"error": "observer failed"}, {"exit_code": -15},
                    {"memory": {"limited": False, "events_delta": {"oom_kill": 1}}},
                    {"interruption": {"sent": True, "phase_observed": False,
                                      "requested_phase": "analytics"}},
                    {"interruption": {"sent": True, "phase_observed": True,
                                      "requested_phase": "lexical-index"}})
        for index, changes in enumerate(variants):
            with self.subTest(changes=changes):
                profile, home, scenario = self.scenario(str(index))
                scenario["receipt_overrides"] = {"interrupted-forget": changes}
                with mock.patch.object(qualification, "preserve_interrupted_archive",
                                       wraps=qualification.preserve_interrupted_archive) as preserve:
                    result, adapter = self.run_scenario(profile, home, scenario)
                self.assertEqual(result["status"], "failed")
                self.assertIn("scheduled SIGKILL lacks a clean observation", result["error"])
                self.assertEqual(adapter.calls[-1]["label"], "interrupted-forget")
                preserve.assert_not_called()
                self.assertNotIn("interrupted_archive", result)
                self.assertNotIn("pre_recovery_probe", result)
                self.assertNotIn("recovery", result)
                self.assertFalse(adapter.recovered)

    def test_unconfirmed_interruption_containment_keeps_live_archive_and_blocks_reuse(self) -> None:
        variants = (None, {}, {"scope": "process-group", "quiescent": False},
                    {"scope": "process-group", "quiescent": True, "cleanup_timed_out": True,
                     "observed_at_seconds": 1.0, "remaining_pids": []},
                    {"scope": "process-group", "quiescent": True, "cleanup_timed_out": False,
                     "observed_at_seconds": 1.0, "remaining_pids": [1234]})
        for index, containment in enumerate(variants):
            with self.subTest(containment=containment):
                profile, home, scenario = self.scenario(str(index))
                scenario["receipt_overrides"] = {"interrupted-forget": {"containment": containment}}
                with mock.patch.object(qualification, "preserve_interrupted_archive",
                                       wraps=qualification.preserve_interrupted_archive) as preserve:
                    result, adapter = self.run_scenario(profile, home, scenario)
                self.assertEqual(result["status"], "failed")
                preserve.assert_not_called()
                self.assertEqual(adapter.calls[-1]["label"], "interrupted-forget")
                self.assertFalse(adapter.archive_quiescent)
                self.assertEqual(result["unparked_archive"]["path"], str(profile / "live-data"))
                self.assertTrue((profile / "live-data").is_dir())
                self.assertFalse((profile / "trials/trial/archive").exists())
                calls = len(adapter.calls)
                with self.assertRaisesRegex(RuntimeError, "archive reuse blocked"):
                    adapter.cass(["index", "--full"], "unsafe-followup")
                self.assertEqual(len(adapter.calls), calls)
                with self.assertRaisesRegex(RuntimeError, "earlier trial was not parked"):
                    qualification.run_trial(profile, home, scenario["manifest"], scenario["seed"],
                                            adapter.args, None, "batch", "next-trial", "analytics")

    def test_unquiescent_strict_probe_cannot_start_recovery_even_on_process_error(self) -> None:
        for raises_error in (False, True):
            with self.subTest(raises_error=raises_error):
                profile, home, scenario = self.scenario(str(raises_error))
                scenario["receipt_overrides"] = {"interrupted-canonical": {"containment": None}}
                if raises_error:
                    scenario["process_error_labels"] = ["interrupted-canonical"]
                result, adapter = self.run_scenario(profile, home, scenario)
                self.assertEqual(result["status"], "failed")
                self.assertIn("quiescence; recovery blocked", result["error"])
                self.assertEqual(result["pre_recovery_probe"]["status"], "unavailable")
                self.assertTrue(Path(result["interrupted_archive"]["path"]).is_dir())
                self.assertEqual(adapter.calls[-1]["label"], "interrupted-canonical")
                self.assertNotIn("recovery", result)
                self.assertNotIn("inconclusive", result)
                self.assertFalse(adapter.recovered)
                self.assertFalse(adapter.archive_quiescent)
                self.assertTrue((profile / "live-data").is_dir())
                self.assertFalse((profile / "trials/trial/archive").exists())

    def test_unquiescent_setup_forget_and_rescan_errors_are_never_parked(self) -> None:
        for label in ("admit-restored-seed", "forget", "full-rescan"):
            with self.subTest(label=label):
                profile, home, scenario = self.scenario(label)
                scenario["receipt_overrides"] = {label: {"containment": None, "status": "observer-error"}}
                scenario["process_error_labels"] = [label]
                result, adapter = self.run_scenario(profile, home, scenario, None)
                self.assertEqual(result["status"], "failed")
                self.assertEqual(adapter.calls[-1]["label"], label)
                self.assertFalse(adapter.archive_quiescent)
                self.assertTrue((profile / "live-data").is_dir())
                self.assertFalse((profile / "trials/trial/archive").exists())

    def test_unavailable_probe_retains_evidence_and_recovers_without_claiming_pass(self) -> None:
        for phase in ("canonical-delete", "analytics", "lexical-index"):
            with self.subTest(phase=phase):
                profile, home, scenario = self.scenario(phase)
                result, adapter = self.run_scenario(profile, home, scenario, phase)
                self.assertEqual(result["status"], "inconclusive")
                self.assertFalse(result["partial_delete_commit_proved"])
                self.assertIn("snapshot unavailable", result["inconclusive"])
                self.assertTrue(adapter.saved_copy_seen_before_probe)
                killed = next(call for call in adapter.calls if call["label"] == "interrupted-forget")
                recovery = next(call for call in adapter.calls if call["label"] == "recovery")
                self.assertEqual(recovery["argv"], killed["argv"])
                self.assertTrue(recovery["measured"])
                self.assertEqual(result["recovery"]["route"], "same-selector-forget")
                self.assertEqual(result["canonical_checks"]["deleted_conversations"], 2)
                self.assertIn("rescan_canonical_checks", result)
                self.assertIn("search_checks", result)
                self.assertIn("rescan_search_checks", result)
                for key, count in (("restored_seed_lexical_publication", 8),
                                   ("lexical_publication", 4), ("rescan_lexical_publication", 4)):
                    publication = result[key]
                    self.assertEqual(publication["expected_live_documents"], count)
                    self.assertEqual(publication["application_report"]["index"]["live_documents"], count)
                    self.assertIs(publication["included_in_forget_timing"], False)
                failed_probe = result["pre_recovery_probe"]["measurement"]
                self.assertEqual(failed_probe["exit_code"], 1)
                self.assertEqual(qualification.read_json(Path(failed_probe["receipt_path"])), failed_probe)
                self.assertIn("BusyRecovery", Path(failed_probe["stderr_path"]).read_text())
                trial = profile / "trials/trial"
                self.assertEqual(qualification.read_json(trial / "trial.json")["status"], "inconclusive")
                for relative in scenario["bundle_files"]:
                    saved, recovered = trial / "interrupted-archive" / relative, trial / "archive" / relative
                    self.assertEqual(saved.read_bytes(), b"interrupted:" + relative.encode())
                    self.assertEqual(recovered.read_bytes(), b"recovered:" + relative.encode())
                    self.assertFalse(os.path.samefile(saved, recovered))
                # An interrupted trial with missing evidence contributes no
                # successful scaling point or native qualification claim.
                scaling = qualification.scaling_summary([{"profile": "representative", "trials": [result]}])
                self.assertEqual(scaling["comparisons"], [])

    def test_copied_seed_admission_precedes_strict_probe_and_is_outside_measurement(self) -> None:
        profile, home, scenario = self.scenario()
        result, adapter = self.run_scenario(profile, home, scenario)
        admission, publication, probe = adapter.calls[:3]
        self.assertEqual(admission["label"], "admit-restored-seed")
        self.assertEqual(publication["label"], "restored-seed-lexical-publication")
        self.assertEqual(publication["argv"], ["status", "--json"])
        self.assertEqual(probe["label"], "restored-seed")
        self.assertEqual(admission["argv"],
                         ["index", "--rebuild-profile", "bounded", "--json", "--no-progress-events"])
        self.assertIs(admission["measured"], False)
        receipt = result["restored_seed_admission"]
        self.assertIs(receipt["included_in_forget_timing"], False)
        self.assertEqual(receipt["application_report"]["operation"], "copied-seed-index-admission")
        self.assertEqual(qualification.read_json(Path(receipt["measurement"]["receipt_path"])),
                         receipt["measurement"])

    def test_post_admission_canonical_and_derived_drift_abort_before_forget(self) -> None:
        for field in ("total", "digest", "analytics", "tombstones", "catalogs"):
            with self.subTest(drift=field):
                profile, home, scenario = self.scenario(field)
                restored = copy.deepcopy(scenario["seed"])
                if field == "total":
                    restored["canonical"]["content_bytes"] += 1
                elif field == "digest":
                    restored["conversations"][2]["messages_sha256"] = digest("admission changed history")
                elif field == "analytics":
                    restored["analytics"]["message_metrics"]["row_count"] = 0
                elif field == "catalogs":
                    restored["canonical_catalogs"]["workspaces"]["sha256"] = digest("changed workspace path")
                else:
                    restored["tombstones"]["forgotten_sources"] = {
                        "rows": [{"source_path": scenario["rows"][0]["source_path"]}],
                        "sha256": digest("unexpected restored seed exclusion")}
                scenario["restored"] = restored
                result, adapter = self.run_scenario(profile, home, scenario)
                self.assertEqual(result["status"], "failed")
                self.assertIn("copied seed", result["error"])
                self.assertEqual([call["label"] for call in adapter.calls],
                                 ["admit-restored-seed", "restored-seed-lexical-publication", "restored-seed"])
                self.assertNotIn("measurement", result)
                self.assertIn("restored_seed_admission", result)

    def test_copied_seed_equal_totals_still_require_complete_track_b_oracle(self) -> None:
        profile, home, scenario = self.scenario()
        restored = copy.deepcopy(scenario["seed"])
        wrong_sessions(restored)
        self.assertEqual(restored["analytics"], scenario["seed"]["analytics"])
        scenario["restored"] = restored
        result, adapter = self.run_scenario(profile, home, scenario)
        self.assertEqual(result["status"], "failed")
        self.assertIn("Track B", result["error"])
        self.assertEqual([call["label"] for call in adapter.calls],
                         ["admit-restored-seed", "restored-seed-lexical-publication", "restored-seed"])
        self.assertNotIn("measurement", result)

    def test_completed_repair_oracle_mismatch_stops_before_search(self) -> None:
        for phase in (None, "analytics", "lexical-index"):
            with self.subTest(phase=phase):
                profile, home, scenario = self.scenario(str(phase))
                scenario.update(probe_unavailable=False, interrupted_snapshot=inventory(
                    scenario["rows"][2:], scenario["rows"][:2], pending=True))
                wrong_sessions(scenario["after"])
                result, adapter = self.run_scenario(profile, home, scenario, phase)
                self.assertEqual(result["status"], "failed")
                self.assertIn("Track B", result["error"])
                labels = [call["label"] for call in adapter.calls]
                mutation = "forget" if phase is None else "recovery"
                self.assertTrue(adapter.calls[labels.index(mutation)]["measured"])
                self.assertEqual(labels[labels.index(mutation) + 1:],
                                 ["after-repair-lexical-publication", "after-repair"])
                self.assertNotIn("search_checks", result)
                self.assertNotIn("canonical_checks", result)
                if phase is not None:
                    self.assertTrue(adapter.recovered)
                    self.assertEqual(result["recovery"]["measurement"]["exit_code"], 0)

    def test_pending_derived_mismatch_does_not_block_verified_recovery(self) -> None:
        for phase in ("canonical-delete", "analytics", "lexical-index"):
            with self.subTest(phase=phase):
                profile, home, scenario = self.scenario(phase)
                committed = 2 if phase == "lexical-index" else 1
                interrupted = inventory(scenario["rows"][committed:],
                                        scenario["rows"][:committed], pending=True)
                wrong_sessions(interrupted)
                scenario.update(probe_unavailable=False, interrupted_snapshot=interrupted)
                result, adapter = self.run_scenario(profile, home, scenario, phase)
                self.assertEqual(result["status"], "passed")
                self.assertEqual(result["pre_recovery_probe"]["status"], "verified")
                self.assertEqual(result["interrupted_state"]["deleted_conversations"], committed)
                self.assertEqual(result["recovery"]["route"],
                                 "index-full" if phase == "lexical-index" else "same-selector-forget")
                self.assertTrue(adapter.recovered)

    def test_missing_pending_oracle_is_failure_not_unavailable_probe(self) -> None:
        profile, home, scenario = self.scenario()
        interrupted = inventory(scenario["rows"][1:], scenario["rows"][:1], pending=True)
        del interrupted["track_b_oracle"]
        scenario.update(probe_unavailable=False, interrupted_snapshot=interrupted)
        result, adapter = self.run_scenario(profile, home, scenario)
        self.assertEqual(result["status"], "failed")
        self.assertIn("independent Track B oracle", result["error"])
        self.assertEqual(adapter.calls[-1]["label"], "interrupted-canonical")
        self.assertFalse(adapter.recovered)
        self.assertNotIn("inconclusive", result)
        self.assertNotIn("pre_recovery_probe", result)
        self.assertTrue(Path(result["interrupted_archive"]["path"]).is_dir())

    def test_catalog_drift_is_rejected_at_pending_repaired_noop_and_rescan_checks(self) -> None:
        for label in ("interrupted-canonical", "after-repair", "reopened-after-noop", "after-full-rescan"):
            with self.subTest(label=label):
                profile, home, scenario = self.scenario(label)
                if label == "interrupted-canonical":
                    changed = inventory(scenario["rows"][1:], scenario["rows"][:1], pending=True)
                    scenario.update(probe_unavailable=False, interrupted_snapshot=changed)
                else:
                    changed = copy.deepcopy(scenario["after"])
                    scenario["snapshots"] = {label: changed}
                changed["canonical_catalogs"]["workspaces"]["sha256"] = digest("changed workspace metadata")
                result, adapter = self.run_scenario(profile, home, scenario)
                self.assertEqual(result["status"], "failed")
                self.assertIn("catalog metadata", result["error"])
                self.assertEqual(adapter.calls[-1]["label"], label)
                if label == "interrupted-canonical":
                    self.assertFalse(adapter.recovered)
                if label == "after-full-rescan":
                    self.assertIn("canonical_checks", result)
                    self.assertNotIn("rescan_canonical_checks", result)
                    self.assertNotIn("rescan_search_checks", result)

    def test_incomplete_seed_publication_aborts_before_any_probe_or_search(self) -> None:
        profile, home, scenario = self.scenario()
        incomplete = lexical_publication(8)
        incomplete["index"]["partial"] = True
        scenario["lexical_publications"] = {"restored-seed-lexical-publication": incomplete}
        result, adapter = self.run_scenario(profile, home, scenario)
        self.assertEqual(result["status"], "failed")
        self.assertIn("partial", result["error"])
        self.assertEqual([call["label"] for call in adapter.calls],
                         ["admit-restored-seed", "restored-seed-lexical-publication"])
        self.assertNotIn("measurement", result)

    def test_incomplete_repair_publication_stops_before_followup_scan_or_search(self) -> None:
        for phase in (None, "analytics", "lexical-index"):
            with self.subTest(phase=phase):
                profile, home, scenario = self.scenario(str(phase))
                scenario.update(probe_unavailable=False, interrupted_snapshot=inventory(
                    scenario["rows"][2:], scenario["rows"][:2], pending=True))
                incomplete = lexical_publication(4)
                incomplete["index"]["live_documents"] = 3
                scenario["lexical_publications"] = {"after-repair-lexical-publication": incomplete}
                result, adapter = self.run_scenario(profile, home, scenario, phase)
                self.assertEqual(result["status"], "failed")
                self.assertIn("live MANIFEST", result["error"])
                labels = [call["label"] for call in adapter.calls]
                mutation = "forget" if phase is None else "recovery"
                command = adapter.calls[labels.index(mutation)]
                self.assertTrue(command["measured"])
                self.assertEqual(labels[labels.index(mutation) + 1:],
                                 ["after-repair-lexical-publication"])
                self.assertNotIn("canonical_checks", result)
                self.assertNotIn("search_checks", result)
                if phase is not None:
                    self.assertTrue(adapter.recovered)
                    expected_route = "index-full" if phase == "lexical-index" else "same-selector-forget"
                    self.assertEqual(result["recovery"]["route"], expected_route)
                    self.assertEqual(result["recovery"]["measurement"]["exit_code"], 0)

    def test_incomplete_rescan_publication_stops_before_rescan_scan_or_search(self) -> None:
        profile, home, scenario = self.scenario()
        incomplete = lexical_publication(4)
        incomplete["index"]["checkpoint"]["completed"] = False
        scenario["lexical_publications"] = {"after-rescan-lexical-publication": incomplete}
        result, adapter = self.run_scenario(profile, home, scenario, None)
        self.assertEqual(result["status"], "failed")
        self.assertIn("checkpoint completed", result["error"])
        labels = [call["label"] for call in adapter.calls]
        self.assertEqual(labels[labels.index("full-rescan") + 1:],
                         ["after-rescan-lexical-publication"])
        self.assertIn("canonical_checks", result)
        self.assertIn("search_checks", result)
        self.assertNotIn("rescan_canonical_checks", result)
        self.assertNotIn("rescan_search_checks", result)

    def test_recovery_failure_stays_failed_and_keeps_both_receipts(self) -> None:
        profile, home, scenario = self.scenario()
        scenario["recovery_exit"] = 9
        result, _ = self.run_scenario(profile, home, scenario)
        self.assertEqual(result["status"], "failed")
        self.assertEqual(result["pre_recovery_probe"]["status"], "unavailable")
        self.assertEqual(result["recovery"]["measurement"]["exit_code"], 9)
        self.assertTrue(Path(result["interrupted_archive"]["path"]).is_dir())

    def test_final_canonical_failure_is_not_downgraded_to_inconclusive(self) -> None:
        profile, home, scenario = self.scenario()
        scenario["after"]["conversations"][0]["messages_sha256"] = digest("lost unrelated history")
        result, adapter = self.run_scenario(profile, home, scenario)
        self.assertTrue(adapter.recovered)
        self.assertEqual(result["status"], "failed")
        self.assertIn("canonical identity/content/ledger changed", result["error"])

    def test_parsed_orphan_failure_is_not_a_missing_snapshot(self) -> None:
        profile, home, scenario = self.scenario()
        interrupted = inventory(scenario["rows"][1:], scenario["rows"][:1], pending=True)
        interrupted["canonical"]["orphan_messages"] = 1
        scenario.update(probe_unavailable=False, interrupted_snapshot=interrupted)
        result, adapter = self.run_scenario(profile, home, scenario)
        self.assertEqual(result["status"], "failed")
        self.assertIn("orphan canonical messages", result["error"])
        self.assertNotIn("inconclusive", result)
        self.assertFalse(adapter.recovered)

    def test_proven_missing_fence_and_reader_leak_remain_failures(self) -> None:
        for leak in (False, True):
            with self.subTest(reader_leak=leak):
                profile, home, scenario = self.scenario(str(leak))
                scenario.update(probe_unavailable=False, privacy_leak=leak,
                    interrupted_snapshot=inventory(scenario["rows"][1:], scenario["rows"][:1], pending=leak))
                result, adapter = self.run_scenario(profile, home, scenario)
                self.assertEqual(result["status"], "failed")
                self.assertNotIn("inconclusive", result)
                self.assertFalse(adapter.recovered)
                self.assertTrue(Path(result["interrupted_archive"]["path"]).is_dir())

    def test_proven_lexical_interruption_preserves_index_recovery_route(self) -> None:
        profile, home, scenario = self.scenario()
        scenario.update(probe_unavailable=False,
                        interrupted_snapshot=inventory(scenario["rows"][2:], scenario["rows"][:2], pending=True))
        result, adapter = self.run_scenario(profile, home, scenario, "lexical-index")
        self.assertEqual(result["status"], "passed")
        self.assertEqual(result["recovery"]["route"], "index-full")
        recovery = next(call for call in adapter.calls if call["label"] == "recovery")
        self.assertEqual(recovery["argv"][:2], ["index", "--full"])
        self.assertTrue(recovery["measured"])


class EvidenceCopyTests(unittest.TestCase):
    def setUp(self) -> None:
        self.root = ARTIFACTS / self.id().split(".")[-1]
        self.source, self.trial = self.root / "live-data", self.root / "trial"
        self.source.mkdir(parents=True)
        self.trial.mkdir()
        (self.source / "a").mkdir()
        (self.source / "a/x").write_bytes(b"nested bytes")
        (self.source / "a.txt").write_bytes(b"sibling bytes")

    def test_nested_inventory_is_copied_with_independent_inodes_and_content_receipt(self) -> None:
        receipt = qualification.preserve_interrupted_archive(self.source, self.trial)
        self.assertEqual(receipt["status"], "preserved")
        self.assertEqual([entry["path"] for entry in receipt["files"]], ["a.txt", "a/x"])
        self.assertEqual(qualification.read_json(Path(receipt["receipt_path"])), receipt)
        destination = Path(receipt["path"])
        for entry in receipt["files"]:
            self.assertEqual(qualification.sha256(destination / entry["path"]), entry["sha256"])
            self.assertFalse(os.path.samefile(self.source / entry["path"], destination / entry["path"]))
        (self.source / "a/x").write_bytes(b"later recovery mutation")
        self.assertEqual((destination / "a/x").read_bytes(), b"nested bytes")

    def test_insufficient_disk_refuses_before_copy(self) -> None:
        with mock.patch.object(qualification.shutil, "disk_usage", return_value=SimpleNamespace(free=0)), \
                self.assertRaisesRegex(RuntimeError, "insufficient disk"):
            qualification.preserve_interrupted_archive(self.source, self.trial)
        self.assertFalse((self.trial / "interrupted-archive").exists())

    def test_accidental_hardlinks_are_rejected_and_diagnostics_are_retained(self) -> None:
        def hardlinked_copy(argv, **kwargs):
            destination = Path(argv[-1])
            (destination / "a").mkdir(parents=True)
            for relative in ("a.txt", "a/x"):
                os.link(self.source / relative, destination / relative)
            return subprocess.CompletedProcess(argv, 0)
        with mock.patch.object(qualification.subprocess, "run", side_effect=hardlinked_copy), \
                self.assertRaisesRegex(RuntimeError, "hardlinked"):
            qualification.preserve_interrupted_archive(self.source, self.trial)
        receipt = qualification.read_json(self.trial / "interrupted-copy.json")
        self.assertEqual(receipt["status"], "failed")
        self.assertTrue((self.trial / "interrupted-archive/a/x").is_file())


if __name__ == "__main__":
    print(f"Harness-only recovery test artifacts retained at {ARTIFACTS}", flush=True)
    unittest.main()
