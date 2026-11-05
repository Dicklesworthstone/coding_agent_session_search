#!/usr/bin/env python3
"""Checks for public relevance accounting and bounded fusion, without a model."""
import argparse
import importlib.util
import math
import os
import signal
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock


SPEC = importlib.util.spec_from_file_location(
    'evaluate_title_retrieval', Path(__file__).parents[1] / 'evaluate_title_retrieval.py')
evaluation = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(evaluation)


class RelevanceAccounting(unittest.TestCase):
    def test_multi_relevant_document_metrics_and_cutoff(self):
        ranking = ['irrelevant', 'a', 'b']
        measured = evaluation.metrics(ranking, {'a': 2, 'b': 1, 'missing': 1})
        self.assertEqual(measured['hit1'], 0)
        self.assertEqual(measured['hit5'], 1)
        self.assertEqual(measured['mrr10'], .5)
        self.assertAlmostEqual(measured['recall10'], 2/3)
        self.assertGreater(measured['ndcg10'], 0)
        self.assertLess(measured['ndcg10'], 1)
        self.assertEqual(evaluation.metrics(['a'], {'b': 1})['mrr10'], 0)
        outside = evaluation.metrics([f'no-{i}' for i in range(10)]+['a'], {'a': 1})
        self.assertEqual(outside['hit10'], 0)
        self.assertEqual(outside['mrr10'], 0)
        self.assertEqual(outside['recall10'], 0)
        self.assertEqual(evaluation.metrics(['a', 'a'], {'a': 1})['recall10'], 1)

    def test_gained_and_lost_queries_are_both_preserved(self):
        records = [
            {'query_id':'gained','metrics':{'body':{'hit1':0},'candidate':{'hit1':1}}},
            {'query_id':'lost','metrics':{'body':{'hit1':1},'candidate':{'hit1':0}}},
            {'query_id':'same','metrics':{'body':{'hit1':1},'candidate':{'hit1':1}}},
        ]
        self.assertEqual(evaluation.gains_losses(records, 'candidate'), {
            'gained_hit1_query_ids':['gained'], 'lost_hit1_query_ids':['lost']})

    def test_reproduction_fixture_is_unchanged(self):
        args = argparse.Namespace(dataset='fixture')
        docs, queries, qrels, source = evaluation.dataset(args, Path('.'))
        self.assertEqual(len(docs), 12)
        self.assertEqual(len(queries), 12)
        self.assertEqual({d['text'] for d in docs.values()}, {evaluation.BODY})
        self.assertEqual(len({d['title'] for d in docs.values()}), 12)
        self.assertTrue(source['artificial'])
        self.assertTrue(all(len(relevant)==1 for relevant in qrels.values()))

    def test_negative_control_keeps_answer_in_body_and_changes_only_titles(self):
        args = argparse.Namespace(dataset='adversarial')
        docs, queries, qrels, source = evaluation.dataset(args, Path('.'))
        self.assertEqual(sum(d['title']=='New conversation' for d in docs.values()), 6)
        for qid, query in queries.items():
            doc_id = next(iter(qrels[qid]))
            self.assertIn(query, docs[doc_id]['text'])
        self.assertTrue(source['artificial'])

    def test_policy_weights_do_not_use_relevance_judgments(self):
        body = [('answer', .9), ('decoy', .2)]
        title = [('decoy', .99), ('answer', .1)]
        self.assertEqual(evaluation.fuse(body,title,'weighted',.25,10)[0][0], 'answer')
        self.assertEqual(evaluation.fuse(body,title,'weighted',1,10)[0][0], 'decoy')
        self.assertEqual(evaluation.fuse(body,title,'weighted',0,10)[0][0], 'answer')

    def test_duplicate_passages_do_not_create_extra_rrf_votes(self):
        body = [('many',.9)]*10+[('answer',.8)]
        title = [('answer',.99),('many',.1)]
        duplicated = evaluation.fuse(body,title,'rrf',.5,100)
        deduplicated = evaluation.fuse([body[0],body[-1]],title,'rrf',.5,100)
        self.assertEqual(duplicated, deduplicated)
        self.assertEqual(len(duplicated), 2)

    def test_deterministic_ties_and_candidate_bound(self):
        output = evaluation.fuse([('b',.8),('a',.8),('d',.8)],[('c',.5)],'weighted',.25,2)
        self.assertEqual([doc for doc,_ in output], ['a','b','c'])
        self.assertLessEqual(len(output), 4)

    def test_exact_score_ties_do_not_create_artificial_rrf_evidence(self):
        body = [('arbitrary-first', .1), ('relevant', .1)]
        title = [('relevant', .9), ('arbitrary-first', .2)]
        self.assertEqual(evaluation.fuse(body,title,'rrf-ordinal',.25,10)[0][0], 'arbitrary-first')
        self.assertEqual(evaluation.fuse(body,title,'rrf',.25,10)[0][0], 'relevant')

    def test_negative_cosines_and_zero_weight_ablation_keep_body_order(self):
        body = [('a', -.8), ('b', -.2)]
        title = [('title-only', .99)]
        for policy in ('weighted', 'rrf', 'max'):
            output = evaluation.fuse(body,title,policy,0,10)
            self.assertEqual([doc for doc,_ in output], ['b','a'])
        self.assertLess(evaluation.fuse(body,[],'weighted',.25,10)[0][1], 0)

    def test_max_compares_only_present_active_fields_even_when_negative(self):
        body = [('body-only', -.8)]
        title = [('title-only', -.9)]
        ranked = evaluation.fuse(body,title,'max',.5,10)
        self.assertEqual(ranked, [('body-only', -.4), ('title-only', -.45)])
        self.assertEqual(
            evaluation.fuse(body,title,'max',1,10), [('title-only', -.9)])

    def test_invalid_parameters_are_not_silently_tuned(self):
        for weight in (math.nan, math.inf, -.1, 1.1):
            with self.assertRaises(ValueError):
                evaluation.fuse([],[],'weighted',weight,10)
        with self.assertRaises(ValueError):
            evaluation.fuse([('a',math.nan)],[],'weighted',.25,10)
        with self.assertRaises(ValueError):
            evaluation.fuse([('a',math.inf)],[],'reporter-max',.25,10)


class NativeEvidenceAndFailureReceipts(unittest.TestCase):
    def test_native_reporter_max_is_equal_weight_and_other_policies_stay_fixed(self):
        self.assertEqual(evaluation.native_policy_requests(.25), (
            ('native-max-equal-weight', 'max', .5),
            ('native-weighted', 'weighted', .25),
            ('native-rrf', 'rrf', .25),
        ))
        changed_weight = evaluation.native_policy_requests(.75)
        self.assertEqual(changed_weight[0], ('native-max-equal-weight', 'max', .5))
        self.assertEqual(changed_weight[1][2], .75)
        self.assertEqual(changed_weight[2][2], .75)

    def native_meta(self):
        return {'title_semantic': {
            'policy': {'enabled': True, 'fusion': 'weighted', 'title_weight': .25,
                       'body_candidates': 1000, 'title_candidates': 1000,
                       'total_candidates': 2000},
            'archive_mutation_seq': 12, 'title_mutation_seq': 12,
            'indexed_titles': 12, 'eligible_titles': 12,
            'title_conversations': 12, 'body_conversations': 12,
            'stale_titles_excluded': 0, 'candidate_window_limited': False,
        }}

    def test_old_binary_ignoring_env_cannot_pass_native_evaluation(self):
        for meta in ({}, {'title_semantic': None}, {'search_mode': 'semantic'}):
            with self.assertRaisesRegex(RuntimeError, 'metadata absent'):
                evaluation.verify_native_title_metadata(meta, 'weighted', .25, 1000)

    def test_native_request_must_prove_effective_policy_and_loaded_sidecar(self):
        valid = self.native_meta()
        self.assertEqual(evaluation.verify_native_title_metadata(valid,'weighted',.25,1000),
                         valid['title_semantic'])
        mutations = [
            ('policy', 'enabled', False), ('policy', 'fusion', 'max'),
            ('policy', 'title_weight', .5), ('policy', 'body_candidates', 500),
            ('policy', 'title_candidates', 500), ('policy', 'total_candidates', 1000),
            (None, 'indexed_titles', 0), (None, 'title_conversations', 0),
            (None, 'title_mutation_seq', None), (None, 'stale_titles_excluded', 1),
        ]
        for section, key, value in mutations:
            changed = self.native_meta()
            target = changed['title_semantic']
            if section:target = target[section]
            target[key] = value
            with self.subTest(key=key), self.assertRaises(RuntimeError):
                evaluation.verify_native_title_metadata(changed,'weighted',.25,1000)

    def test_failed_child_persists_resource_receipt_before_raising(self):
        # Keep the failed-command receipt for inspection; no test-owned cleanup.
        base = Path(tempfile.mkdtemp(prefix='cass-title-failure-'))
        evaluation.PROCESS_MEASUREMENTS.clear()
        receipt = evaluation.Receipt(base/'results.json', 'fixture')
        with mock.patch.object(evaluation, 'ACTIVE_RECEIPT', receipt), \
             self.assertRaisesRegex(RuntimeError, 'exited 7'):
            evaluation.run(sys.executable, ['-c', 'raise SystemExit(7)'],
                           os.environ.copy(), base/'logs/index')
        saved = evaluation.parse_json((base/'results.json').read_text(), 'failed-child receipt')
        self.assertEqual(saved['status'], 'failed')
        self.assertEqual(saved['last_process']['returncode'], 7)
        self.assertFalse(saved['last_process']['timed_out'])
        self.assertGreater(saved['last_process']['peak_rss_bytes'], 0)
        self.assertEqual(saved['process_measurements'][0], saved['last_process'])
        self.assertEqual(saved['completed_query_passes'], 0)

    def test_timeout_reaps_owned_child_and_records_failure(self):
        base = Path(tempfile.mkdtemp(prefix='cass-title-timeout-'))
        evaluation.PROCESS_MEASUREMENTS.clear()
        receipt = evaluation.Receipt(base/'results.json', 'fixture')
        with mock.patch.object(evaluation, 'ACTIVE_RECEIPT', receipt), \
             self.assertRaisesRegex(RuntimeError, 'exited -9'):
            evaluation.run(sys.executable, ['-c', 'import time; time.sleep(60)'],
                           os.environ.copy(), base/'logs/index', timeout=.001)
        saved = evaluation.parse_json((base/'results.json').read_text(), 'timeout receipt')
        self.assertEqual(saved['status'], 'failed')
        self.assertEqual(saved['last_process']['returncode'], -signal.SIGKILL)
        self.assertTrue(saved['last_process']['timed_out'])
        self.assertFalse(saved['last_process']['interrupted'])
        with self.assertRaises(ChildProcessError):
            os.waitpid(saved['last_process']['pid'], os.WNOHANG)

    def test_interruption_reaps_owned_child_before_reraising(self):
        base = Path(tempfile.mkdtemp(prefix='cass-title-interrupt-'))
        evaluation.PROCESS_MEASUREMENTS.clear()
        receipt = evaluation.Receipt(base/'results.json', 'fixture')
        with mock.patch.object(evaluation, 'ACTIVE_RECEIPT', receipt), \
             mock.patch.object(evaluation.time, 'sleep', side_effect=KeyboardInterrupt), \
             self.assertRaises(KeyboardInterrupt):
            evaluation.run(sys.executable, ['-c', 'import time; time.sleep(60)'],
                           os.environ.copy(), base/'logs/index')
        saved = evaluation.parse_json((base/'results.json').read_text(), 'interruption receipt')
        self.assertEqual(saved['status'], 'failed')
        self.assertEqual(saved['error']['type'], 'KeyboardInterrupt')
        self.assertEqual(saved['last_process']['returncode'], -signal.SIGKILL)
        self.assertTrue(saved['last_process']['interrupted'])
        self.assertFalse(saved['last_process']['timed_out'])
        with self.assertRaises(ChildProcessError):
            os.waitpid(saved['last_process']['pid'], os.WNOHANG)

    def test_invalid_json_identifies_source_without_echoing_contents(self):
        for payload in ('private-content{', b'\xff'):
            with self.subTest(payload=repr(payload)), \
                 self.assertRaisesRegex(RuntimeError, '^Invalid JSON in search output$'):
                evaluation.parse_json(payload, 'search output')
        self.assertEqual(evaluation.parse_json(b'{"valid": true}', 'search output'), {'valid': True})

    def test_nonprocess_error_cannot_leave_a_success_receipt(self):
        base = Path(tempfile.mkdtemp(prefix='cass-title-missing-binary-'))
        output = base/'results.json'
        argv = ['evaluate_title_retrieval.py', '--cass', 'missing-cass', '--output', str(output)]
        with mock.patch.object(sys, 'argv', argv), \
             mock.patch.object(evaluation, 'isolation_guard'), \
             mock.patch.object(evaluation.shutil, 'which', return_value=None), \
             self.assertRaisesRegex(RuntimeError, 'Put the CASS binary'):
            evaluation.main()
        self.assertEqual(evaluation.parse_json(output.read_text(), 'missing-binary receipt')['status'], 'failed')


if __name__ == '__main__':
    unittest.main()
