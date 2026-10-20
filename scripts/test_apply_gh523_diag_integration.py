"""Patch-transport tests only; these do not compile or qualify the Rust code."""
import hashlib
import unittest
from apply_gh523_diag_integration import EDITS, already_applied, blob_sha, patch_text


class GuardedEdits(unittest.TestCase):
    def fixture(self):
        # Exact inspected source anchors, with unrelated surrounding bytes.
        return "// prefix sentinel\n" + "\n// unrelated sentinel\n".join(old for old, _ in EDITS) + "// suffix sentinel\n"

    def test_all_six_edits_preserve_unrelated_bytes(self):
        original = self.fixture()
        changed = patch_text(original)
        self.assertTrue(already_applied(changed))
        self.assertEqual(changed.count("// unrelated sentinel"), len(EDITS) - 1)
        self.assertTrue(changed.startswith("// prefix sentinel\n"))
        self.assertTrue(changed.endswith("// suffix sentinel\n"))
        restored = changed
        for old, new in reversed(EDITS):
            restored = restored.replace(new, old, 1)
        self.assertEqual(restored, original)

    def test_duplicate_anchors_fail_closed(self):
        for old, _ in EDITS:
            with self.subTest(anchor=old[:60]):
                with self.assertRaises(ValueError):
                    patch_text(self.fixture() + old)

    def test_missing_or_changed_anchors_fail_closed(self):
        for old, _ in EDITS:
            with self.subTest(anchor=old[:60]):
                with self.assertRaises(ValueError):
                    patch_text(self.fixture().replace(old, "// concurrently changed\n", 1))

    def test_partial_application_is_not_an_idempotent_success(self):
        for old, new in EDITS:
            partial = self.fixture().replace(old, new, 1)
            self.assertFalse(already_applied(partial))
            with self.assertRaises(ValueError):
                patch_text(partial)

    def test_patch_is_not_applied_twice(self):
        changed = patch_text(self.fixture())
        with self.assertRaises(ValueError):
            patch_text(changed)

    def test_git_blob_identity_matches_known_empty_blob(self):
        self.assertEqual(blob_sha(b""), "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391")
        self.assertNotEqual(blob_sha(b"line\n"), blob_sha(b"line\r\n"))


if __name__ == "__main__":
    unittest.main()
