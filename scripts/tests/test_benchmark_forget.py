"""Check the forget benchmark's evidence validation, not CASS performance.

These tests use temporary manifest bytes only. They never run a CASS binary,
substitute a simulated timing, or qualify the Rust deletion implementation.
"""
from __future__ import annotations

import importlib.util
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "benchmark_forget.py"
spec = importlib.util.spec_from_file_location("forget_benchmark", SCRIPT)
assert spec is not None and spec.loader is not None
bench = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bench)


class PublicationEvidenceTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory(prefix="cass-forget-evidence-")
        self.addCleanup(temporary.cleanup)
        self.data = Path(temporary.name)
        self.manifest = self.data / "index/v9-quill/MANIFEST"
        self.manifest.parent.mkdir(parents=True)
        self.prior = b"prior synthetic lexical generation\n"
        self.current = b"replacement synthetic lexical generation\n"
        self.manifest.write_bytes(self.prior)

    def retain(self, name: str, content: bytes) -> Path:
        directory = self.data / "index/.lexical-publish-backups" / name
        directory.mkdir(parents=True)
        (directory / "MANIFEST").write_bytes(content)
        return directory

    def test_unchanged_generation_with_existing_backups_is_a_noop(self) -> None:
        self.retain("older-generation", b"an earlier generation\n")
        before = bench.publication_witness(self.data)
        after = bench.publication_witness(self.data)

        self.assertEqual(bench.observed_publications(self.data, before, after), [])

    def test_one_retained_prior_generation_proves_one_publication(self) -> None:
        self.retain("older-generation", b"an earlier generation\n")
        before = bench.publication_witness(self.data)
        self.retain("new-backup", self.prior)
        self.manifest.write_bytes(self.current)
        after = bench.publication_witness(self.data)

        self.assertEqual(
            bench.observed_publications(self.data, before, after), ["new-backup"]
        )

    def test_changed_manifest_without_backup_is_not_a_noop(self) -> None:
        before = bench.publication_witness(self.data)
        self.manifest.write_bytes(self.current)
        after = bench.publication_witness(self.data)

        with self.assertRaisesRegex(RuntimeError, "changed without a retained publication"):
            bench.observed_publications(self.data, before, after)

    def test_pruned_evidence_is_refused_even_when_a_new_backup_exists(self) -> None:
        older = self.retain("older-generation", b"an earlier generation\n")
        before = bench.publication_witness(self.data)
        older.rename(self.data / "outside-publication-inventory")
        self.retain("new-backup", self.prior)
        self.manifest.write_bytes(self.current)
        after = bench.publication_witness(self.data)

        with self.assertRaisesRegex(RuntimeError, "publication evidence was pruned"):
            bench.observed_publications(self.data, before, after)

    def test_backup_of_the_wrong_prior_generation_is_refused(self) -> None:
        before = bench.publication_witness(self.data)
        self.retain("new-backup", b"an unrelated generation\n")
        self.manifest.write_bytes(self.current)
        after = bench.publication_witness(self.data)

        with self.assertRaisesRegex(RuntimeError, "does not retain the observed prior"):
            bench.observed_publications(self.data, before, after)

    def test_empty_backup_directory_is_not_publication_evidence(self) -> None:
        before = bench.publication_witness(self.data)
        (self.data / "index/.lexical-publish-backups/new-backup").mkdir(parents=True)
        self.manifest.write_bytes(self.current)
        after = bench.publication_witness(self.data)

        with self.assertRaisesRegex(RuntimeError, "does not retain the observed prior"):
            bench.observed_publications(self.data, before, after)

    def test_adding_a_backup_without_replacing_the_live_generation_is_refused(self) -> None:
        before = bench.publication_witness(self.data)
        self.retain("new-backup", self.prior)
        after = bench.publication_witness(self.data)

        with self.assertRaisesRegex(RuntimeError, "did not replace live MANIFEST"):
            bench.observed_publications(self.data, before, after)

    def test_two_new_backups_cannot_be_reported_as_one_publication(self) -> None:
        before = bench.publication_witness(self.data)
        self.retain("first-backup", self.prior)
        self.retain("second-backup", b"an intermediate generation\n")
        self.manifest.write_bytes(self.current)
        after = bench.publication_witness(self.data)

        with self.assertRaisesRegex(RuntimeError, "one forget invocation published 2"):
            bench.observed_publications(self.data, before, after)

    def test_ambiguous_live_generation_is_refused(self) -> None:
        second = self.data / "index/another-live-index/MANIFEST"
        second.parent.mkdir()
        second.write_bytes(self.current)

        with self.assertRaisesRegex(RuntimeError, "expected one live lexical generation"):
            bench.publication_witness(self.data)


class ExactSelectorTests(unittest.TestCase):
    def test_glob_encoding_preserves_literal_metacharacters_and_whitespace(self) -> None:
        for path, encoded in (
            ("/logs/plain name.jsonl", "/logs/plain name.jsonl"),
            (" /logs/session[1]*?.jsonl ", " /logs/session[[]1[]][*][?].jsonl "),
            ("/logs/café/[]?.jsonl", "/logs/café/[[][]][?].jsonl"),
        ):
            with self.subTest(path=path):
                self.assertEqual(bench.exact_glob(path), encoded)

    def test_serial_and_batch_encoding_preserve_the_same_exact_queue(self) -> None:
        literal = " /logs/session[1]*?.jsonl "
        escaped = " /logs/session[[]1[]][*][?].jsonl "
        queue = [
            {"path": literal, "kind": "path"},
            {"path": "/logs/absent.jsonl", "kind": "path"},
            {"path": literal, "kind": "path"},
            {"path": literal, "kind": "glob"},
        ]
        self.assertEqual(bench.selectors_for(queue, batched=False), [
            "--source-glob", escaped,
            "--source-glob", "/logs/absent.jsonl",
            "--source-glob", escaped,
            "--source-glob", escaped,
        ])
        self.assertEqual(bench.selectors_for(queue, batched=True), [
            "--source-path", literal,
            "--source-path", "/logs/absent.jsonl",
            "--source-path", literal,
            "--source-glob", escaped,
        ])


if __name__ == "__main__":
    unittest.main()
