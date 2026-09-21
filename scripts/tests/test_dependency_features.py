"""Run with python3 -m unittest discover -s scripts/tests -p test_dependency_features.py."""
import importlib.util
from pathlib import Path
import unittest

MODULE = Path(__file__).resolve().parents[1] / "check_dependency_features.py"
SPEC = importlib.util.spec_from_file_location("dependency_features", MODULE)
checker = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(checker)


def source(features='"connectors", "openclaw-sqlite", "copilot-vscdb",'):
    return '''const CONTRACTS: &[DependencyContract] = &[
    DependencyContract {
        dep_table: "dependencies",
        dep_key: "franken-agent-detection",
        expected_features: &[''' + features + '''],
    },
];'''


def manifest(features):
    return {"dependencies": {"franken-agent-detection": {"features": features}}}


class DependencyFeaturesTest(unittest.TestCase):
    def test_matching_features_are_order_independent(self):
        self.assertEqual(checker.check(source(), manifest([
            "copilot-vscdb", "connectors", "openclaw-sqlite",
        ])), [])

    def test_original_build_failure_is_detected_before_cargo(self):
        errors = checker.check(source('"connectors"'), manifest([
            "connectors", "openclaw-sqlite", "copilot-vscdb",
        ]))
        self.assertEqual(len(errors), 1)
        self.assertIn("unexpected=['copilot-vscdb', 'openclaw-sqlite']", errors[0])

    def test_removing_required_transcript_readers_is_not_a_fix(self):
        errors = checker.check(source(), manifest(["connectors"]))
        self.assertEqual(len(errors), 1)
        self.assertIn("missing=['copilot-vscdb', 'openclaw-sqlite']", errors[0])

    def test_malformed_manifest_is_rejected(self):
        for features in (None, "connectors", [12]):
            with self.subTest(features=features):
                self.assertTrue(checker.check(source(), manifest(features)))
        self.assertTrue(checker.check(source(), {}))

    def test_unrecognized_source_cannot_silently_pass(self):
        for text in ("", source() + source(), source().replace("DependencyContract {", "make_contract! {"),
                     source().replace('dep_key: "franken-agent-detection",', ""),
                     source().replace('&["connectors", "openclaw-sqlite", "copilot-vscdb",]', "FEATURES")):
            with self.subTest(source=text):
                with self.assertRaises(ValueError):
                    checker.contracts(text)

    def test_actual_build_table_contains_both_readers_and_rejects_drift(self):
        # Exercise every real static contract without duplicating its feature list.
        root = Path(__file__).resolve().parents[2]
        text = (root / "build.rs").read_text(encoding="utf-8")
        expected = checker.contracts(text)
        features = expected[("dependencies", "franken-agent-detection")]
        self.assertTrue({"openclaw-sqlite", "copilot-vscdb"} <= features)
        resolved = {}
        for (table, key), values in expected.items():
            resolved.setdefault(table, {})[key] = {"features": sorted(values)}
        self.assertEqual(checker.check(text, resolved), [])
        for table, key in expected:
            spec = resolved[table][key]
            spec["features"].append("unreviewed-feature")
            self.assertEqual(len(checker.check(text, resolved)), 1)
            spec["features"].pop()


if __name__ == "__main__":
    unittest.main()
