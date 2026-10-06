"""Only source-transformation tests. Fixtures are not native engine execution."""
import importlib.util
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

MODULE = Path(__file__).resolve().parents[1] / 'install_candidate.py'
spec = importlib.util.spec_from_file_location('install_candidate', MODULE)
installer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(installer)

class InstallerTests(unittest.TestCase):
    def make_cass(self, root, *, qualified=False):
        directory = root / 'src/search'; directory.mkdir(parents=True)
        (directory / 'quill_bridge.rs').write_text('pub fn open_cass_reader(path: &Path) -> Result<QuillSearchIndex> { todo!() }\n')
        call = 'quill_bridge::open_cass_reader(path)' if qualified else 'open_cass_reader(path)'
        (directory / 'query.rs').write_text(
            'fn open_search_readers(path: &Path) {\n    let reader = '+call+';\n}\n'
            'fn maintenance(path: &Path) {\n    open_cass_reader(path);\n}\n')
        return directory

    def test_unqualified_call_is_changed_only_in_search_function(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d); directory = self.make_cass(root)
            edits = installer.cass_edits(root)
            self.assertIn('crate::search::quill_bridge::open_cass_search_reader(path)', edits[directory/'query.rs'])
            self.assertIn('fn maintenance(path: &Path) {\n    open_cass_reader(path);', edits[directory/'query.rs'])
            self.assertNotIn('open_cass_search_reader', (directory/'query.rs').read_text())

    def test_qualified_call_does_not_duplicate_namespace(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d); directory = self.make_cass(root, qualified=True)
            changed = installer.cass_edits(root)[directory/'query.rs']
            self.assertNotIn('quill_bridge::crate::', changed)
            self.assertIn('crate::search::quill_bridge::open_cass_search_reader(path)', changed)

    def test_source_drift_is_refused_without_writing(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d); directory = self.make_cass(root)
            path = directory/'query.rs'; path.write_text(path.read_text().replace('open_search_readers','renamed'))
            before = (directory/'quill_bridge.rs').read_bytes()
            with self.assertRaises(ValueError): installer.cass_edits(root)
            self.assertEqual(before, (directory/'quill_bridge.rs').read_bytes())

    def test_multiple_calls_in_search_body_are_refused(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d); directory = self.make_cass(root)
            path = directory/'query.rs'; path.write_text(path.read_text().replace('let reader =', 'open_cass_reader(path);\n    let reader ='))
            with self.assertRaises(ValueError): installer.cass_edits(root)

    def test_reapplication_is_refused(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d); self.make_cass(root)
            for path, new in installer.cass_edits(root).items(): path.write_text(new)
            with self.assertRaises(ValueError): installer.cass_edits(root)

    def test_cli_failure_does_not_write_even_with_write_flag(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d); directory = self.make_cass(root)
            query = directory/'query.rs'; query.write_text('fn unknown() {}\n')
            old = (directory/'quill_bridge.rs').read_bytes()
            result = subprocess.run([sys.executable, str(MODULE), '--cass', str(root), '--write'], capture_output=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(old, (directory/'quill_bridge.rs').read_bytes())

    def test_once_rejects_ambiguous_or_absent_anchor(self):
        for source in ('none','target target'):
            with self.assertRaises(ValueError): installer.once(source, 'target', 'replacement', 'fixture')
        self.assertEqual('new', installer.once('old','old','new','fixture'))

if __name__ == '__main__': unittest.main()
