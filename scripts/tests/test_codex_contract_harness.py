#!/usr/bin/env python3
"""Test harness generation/admission, NOT Rust compilation or Cargo resolution.

Run: python3 -m unittest discover -s scripts/tests -p test_codex_contract_harness.py
Cargo process boundaries are controlled; real TOML parsing, source staging and
pre-test refusal execute. CASS_CODEX_HARNESS can select the unmodified baseline.
"""
from contextlib import redirect_stdout
import copy
import importlib.util
import io
import json
import os
from pathlib import Path
import tempfile
import tomllib
import unittest
from unittest.mock import patch

HARNESS = Path(os.environ.get("CASS_CODEX_HARNESS", Path(__file__).resolve().parents[1] / "test_codex_contract.py"))
FAD = "franken-agent-detection"
REV = "6b337ba434b72c65c6c5a748b7ec47885f840ac0"
GIT = "https://github.com/Dicklesworthstone/franken_agent_detection"
SOURCE = f"git+{GIT}?rev={REV}#{REV}"


def load_harness():
    spec = importlib.util.spec_from_file_location("codex_contract_under_test", HARNESS)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class HarnessTests(unittest.TestCase):
    def setUp(self):
        self.harness = load_harness()
        # Keep test artifacts for inspection; never touch a real checkout.
        self.root = Path(tempfile.mkdtemp(prefix="cass-harness-proof-")) / "repo with spaces 🔑"
        for directory in ("scripts", "tests", "src/connectors/codex"):
            (self.root / directory).mkdir(parents=True, exist_ok=True)
        dependencies = ["anyhow", "blake3", "chrono", "dirs", "dotenvy", "serde_json", "tempfile", "thiserror", "tracing"]
        manifest = '[package]\nname="fixture"\nversion="0.0.0"\n[dependencies]\n'
        manifest += ''.join(f'{name}="1"\n' for name in dependencies)
        manifest += f'{FAD}={{version="=0.3.10",git="{GIT}",rev="{REV}",features=["connectors","cursor","codex-zstd"]}}\n'
        manifest += 'serde={version="1",default-features=false,features=["std","derive"]}\n'
        manifest += '[dev-dependencies]\nzstd={version="0.13",default-features=false}\n'
        (self.root / "Cargo.toml").write_text(manifest, encoding="utf-8")
        (self.root / "Cargo.lock").write_text(f'version=4\n[[package]]\nname="{FAD}"\nversion="0.3.10"\nsource="{SOURCE}"\n', encoding="utf-8")
        for test in ("connector_codex_exclusions", "codex_source_containment"):
            (self.root / f"tests/{test}.rs").write_text('// unchanged integration fixture\n', encoding="utf-8")
        (self.root / "src/connectors/codex.rs").write_text('mod nested;\n', encoding="utf-8")
        (self.root / "src/connectors/codex/nested.rs").write_text('// byte-identical source 🔑\n', encoding="utf-8")
        self.before = {path: path.read_bytes() for path in self.root.rglob('*') if path.is_file()}
        self.calls = []
        self.generated = None
        self.library = None

    def run_harness(self, checkout=None, metadata_override=None):
        def capture(command, **kwargs):
            self.calls.append(command)
            if "--manifest-path" in command:
                project = Path(command[command.index("--manifest-path") + 1])
                self.generated = tomllib.loads(project.read_text(encoding="utf-8"))
                self.library = (project.parent / "lib.rs").read_text(encoding="utf-8")
                for original in (self.root / "src/connectors").rglob("*.rs"):
                    self.assertEqual((project.parent / original.relative_to(self.root / "src/connectors")).read_bytes(), original.read_bytes())
            if len(command) > 1 and command[1] == "metadata":
                if metadata_override is not None:
                    return json.dumps(metadata_override)
                spec = self.generated["dependencies"][FAD]
                source = SOURCE if "git" in spec else (None if "path" in spec else "registry+https://github.com/rust-lang/crates.io-index")
                return json.dumps({"packages": [{"name": FAD, "version": "0.3.10", "source": source,
                    "manifest_path": str(Path(spec.get("path", "/cargo/registry/fad")) / "Cargo.toml")} ]})
            return None
        arguments = [str(HARNESS)] + (["--fad-checkout", str(checkout)] if checkout else [])
        with patch.object(self.harness, "__file__", str(self.root / "scripts/test_codex_contract.py")), \
             patch("sys.argv", arguments), patch.object(self.harness.shutil, "which", lambda name: name), \
             patch.object(self.harness.subprocess, "run", capture), \
             patch.object(self.harness.subprocess, "check_output", capture), redirect_stdout(io.StringIO()):
            self.harness.main()
        for path, expected in self.before.items():
            self.assertEqual(path.read_bytes(), expected, f"input mutated: {path}")

    def test_default_generation_keeps_git_revision_instead_of_registry_substitution(self):
        self.run_harness()
        selected = self.generated["dependencies"][FAD]
        self.assertEqual(selected.get("git"), GIT)
        self.assertEqual(selected.get("rev"), REV)
        self.assertEqual(selected["features"], ["connectors", "codex-zstd"])
        self.assertFalse(selected["default-features"])

    def test_non_fad_features_and_zstd_default_feature_optout_survive(self):
        self.run_harness()
        selected = self.generated["dependencies"]
        self.assertEqual(selected["zstd"], {"version": "0.13", "default-features": False})
        self.assertEqual(selected["serde"], {"version": "1", "default-features": False, "features": ["std", "derive"]})

    def test_staged_facade_exposes_current_codex_identity_test_imports(self):
        self.run_harness()
        for identity in ("DiscoveredSourceRole", "Origin", "SourceKind"):
            self.assertIn(identity, self.library.split("pub mod connectors")[0])
            self.assertIn(identity, self.library.split("pub mod connectors")[1])

    def test_metadata_precedes_locked_native_test_and_clippy_commands(self):
        self.run_harness()
        self.assertEqual([command[1] for command in self.calls[:3]], ["metadata", "test", "clippy"])
        for command in self.calls[1:3]:
            self.assertIn("--locked", command)
        self.assertIn("--check", self.calls[-1])

    def test_wrong_registry_same_version_refuses_before_tests(self):
        metadata = {"packages": [{"name": FAD, "version": "0.3.10", "source": "registry+wrong", "manifest_path": "/wrong/Cargo.toml"}]}
        with self.assertRaisesRegex(RuntimeError, "lockfile"):
            self.run_harness(metadata_override=metadata)
        self.assertEqual([command[1] for command in self.calls], ["metadata"])

    def test_explicit_local_checkout_has_only_path_source(self):
        checkout = self.root / "upstream 🔑"
        checkout.mkdir()
        (checkout / "Cargo.toml").write_text(f'[package]\nname="{FAD}"\nversion="0.3.10"\n')
        self.run_harness(checkout=checkout)
        selected = self.generated["dependencies"][FAD]
        self.assertEqual(selected["path"], str(checkout))
        self.assertEqual(selected["version"], "=0.3.10")
        self.assertTrue({"git", "rev", "tag", "branch", "registry"}.isdisjoint(selected))

    def test_full_crate_import_guard_is_not_weakened(self):
        target = self.root / "tests/connector_codex_exclusions.rs"
        target.write_text('use assert_cmd::Command;\n')
        with self.assertRaisesRegex(SystemExit, "full-crate APIs"):
            self.run_harness()
        self.assertEqual(self.calls, [])

    def test_resolution_refuses_missing_duplicate_changed_revision_and_version(self):
        entry = {"name": FAD, "version": "0.3.10", "source": SOURCE, "manifest_path": "/cargo/fad/Cargo.toml"}
        for selected in ([], [entry, entry], [dict(entry, version="0.3.11")], [dict(entry, source=SOURCE[:-1] + "1")]):
            with self.subTest(selected=selected), self.assertRaises(RuntimeError):
                self.run_harness(metadata_override={"packages": selected})
        self.assertFalse(any(command[1] in ("test", "clippy") for command in self.calls))

    def test_local_override_rejects_a_different_or_registry_checkout(self):
        root = self.root / "upstream"
        root.mkdir()
        (root / "Cargo.toml").write_text(f'[package]\nname="{FAD}"\nversion="0.3.10"\n')
        for source, path in ((None, "/wrong/Cargo.toml"), (SOURCE, str(root / "Cargo.toml"))):
            with self.subTest(source=source), self.assertRaises(RuntimeError):
                self.run_harness(checkout=root, metadata_override={"packages": [{"name": FAD,
                    "version": "0.3.10", "source": source, "manifest_path": path}]})
        self.assertFalse(any(command[1] in ("test", "clippy") for command in self.calls))

    def test_dependency_tables_preserve_source_flags_and_unicode_paths_without_mutation(self):
        for spec in ({"version": "1", "registry": "internal", "default-features": False},
                     {"git": GIT, "tag": "release-🔑", "package": "renamed", "features": ["std"]},
                     {"path": "src/connectors", "version": "1"}):
            before = copy.deepcopy(spec)
            text = self.harness.dependency_for_harness("example", spec, self.root, None, "")
            expected = dict(spec)
            if "path" in expected:
                expected["path"] = str((self.root / expected["path"]).resolve())
            self.assertEqual(tomllib.loads(text)["example"], expected)
            self.assertEqual(spec, before)


if __name__ == "__main__":
    unittest.main()
