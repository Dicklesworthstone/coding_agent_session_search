"""Protocol-harness tests; fake services are NOT CASS performance evidence."""
from __future__ import annotations

import argparse
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    'admission_benchmark', Path(__file__).with_name('benchmark_lexical_admission.py'))
assert spec is not None and spec.loader is not None
bench = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bench)

FAKE = '''#!{python}
import json, sys, time
from pathlib import Path
mode = {mode!r}
loaded = False
for line in sys.stdin:
    req = json.loads(line)
    op = req['op']
    if mode == 'timeout' and op == 'status':
        time.sleep(10)
    if mode == 'eof' and op == 'status':
        sys.exit(3)
    result = {{}}
    if op == 'search':
        result = {{'hits': [{{'score': 2.0 if mode == 'mismatch' else 1.0,
                             'source_id': 'local', 'message_index': 1}}],
                  'reader_reused': loaded, 'setup_ms': 0, 'search_ms': 0}}
        loaded = True
        if mode == 'empty':
            result['hits'] = []
        if mode == 'false_reuse':
            result['reader_reused'] = True
    elif op == 'unload':
        loaded = False
    elif op == 'reload':
        loaded = True
    ident = req['id'] + 1 if mode == 'wrong_id' else req['id']
    response = {{'schema_version': 1, 'id': ident, 'ok': True, 'result': result}}
    if mode == 'request_error' and op == 'search':
        response = {{'id': ident, 'ok': False, 'error': {{'message': 'refused'}}}}
    if mode == 'oversized' and op == 'status':
        print('x' * 1048577, flush=True)
    elif mode == 'malformed' and op == 'status':
        print('not json', flush=True)
    else:
        print(json.dumps(response), flush=True)
if mode == 'bad_exit':
    sys.exit(7)
'''


@unittest.skipUnless(sys.platform.startswith('linux'), 'requires Linux /proc counters')
class HarnessTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.index = self.root / 'index'
        self.index.mkdir()
        (self.index / 'MANIFEST').write_text('fixture marker, not a real index')
        self.binary = self.service('baseline', 'ok')

    def service(self, name: str, mode: str) -> Path:
        path = self.root / name
        path.write_text(FAKE.format(python=sys.executable, mode=mode))
        path.chmod(0o755)
        return path

    def args(self, **changes: object) -> argparse.Namespace:
        values = dict(cass=self.binary, candidate_cass=None, index=self.index,
                      query=['performance'], rounds=2, limit=10, timeout=2)
        values.update(changes)
        return argparse.Namespace(**values)

    def test_baseline_never_claims_a_candidate(self) -> None:
        report = bench.run(self.args())
        self.assertEqual(report['mode'], 'baseline_only')
        self.assertEqual(set(report['fresh_medians']), {'baseline'})
        self.assertEqual(len(report['fresh']), 2)
        self.assertTrue(report['hits_equal'])
        self.assertTrue(all(row['process_cpu_s'] >= 0 for row in report['fresh']))
        self.assertEqual([r['phase'] for r in report['retained']], [
            'first_admission', 'retained_query', 'retained_query', 'unload',
            'admission_after_unload', 'reload', 'query_after_reload'])

    def test_comparison_alternates_and_measures_retention_for_both(self) -> None:
        candidate = self.service('candidate', 'also_ok')
        report = bench.run(self.args(candidate_cass=candidate))
        self.assertEqual(report['mode'], 'comparison')
        self.assertEqual([r['arm'] for r in report['fresh']],
                         ['baseline', 'candidate', 'candidate', 'baseline'])
        self.assertEqual({r['arm'] for r in report['retained']}, {'baseline', 'candidate'})
        self.assertNotEqual(report['binaries']['baseline']['sha256'],
                            report['binaries']['candidate']['sha256'])

    def test_identical_binaries_cannot_masquerade_as_comparison(self) -> None:
        with self.assertRaisesRegex(ValueError, 'identical'):
            bench.run(self.args(candidate_cass=self.binary))

    def test_candidate_score_mismatch_refused(self) -> None:
        with self.assertRaisesRegex(AssertionError, 'hits differ'):
            bench.run(self.args(candidate_cass=self.service('candidate', 'mismatch')))

    def test_all_empty_workload_refused(self) -> None:
        with self.assertRaisesRegex(ValueError, 'zero hits'):
            bench.run(self.args(cass=self.service('empty', 'empty')))

    def test_reused_fresh_reader_refused(self) -> None:
        with self.assertRaisesRegex(AssertionError, 'fresh process'):
            bench.run(self.args(cass=self.service('reused', 'false_reuse')))

    def test_bad_exit_does_not_produce_evidence(self) -> None:
        with self.assertRaisesRegex(RuntimeError, 'unclean service shutdown'):
            bench.run(self.args(cass=self.service('bad', 'bad_exit')))

    def test_startup_transport_errors(self) -> None:
        for mode, error in [('wrong_id', RuntimeError), ('eof', RuntimeError),
                            ('malformed', json.JSONDecodeError), ('oversized', RuntimeError)]:
            with self.subTest(mode=mode), self.assertRaises(error):
                bench.Service(self.service(mode, mode), self.index, 2)

    def test_timeout_kills_and_reaps_child(self) -> None:
        children = []
        original = bench.subprocess.Popen

        def spawn(*args: object, **kwargs: object):
            child = original(*args, **kwargs)
            children.append(child)
            return child

        with patch.object(bench.subprocess, 'Popen', side_effect=spawn):
            with self.assertRaises(TimeoutError):
                bench.Service(self.service('slow', 'timeout'), self.index, 0.1)
        self.assertEqual(len(children), 1)
        self.assertIsNotNone(children[0].returncode)
        self.assertFalse(Path(f'/proc/{children[0].pid}').exists())

    def test_request_error_refused(self) -> None:
        with self.assertRaisesRegex(RuntimeError, 'failed'):
            bench.run(self.args(cass=self.service('error', 'request_error')))


if __name__ == '__main__':
    unittest.main()
