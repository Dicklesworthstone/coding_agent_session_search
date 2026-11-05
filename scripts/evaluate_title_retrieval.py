#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
# Copyright (c) 2026 CASS public benchmark contributors
"""Evaluate title/body fusion with the actual CASS native MiniLM backend.

Adapted from smartkiwi's MIT-licensed public reproduction in CASS issue #540:
https://github.com/Dicklesworthstone/coding_agent_session_search/issues/540

Python 3.10+, Linux/macOS. Reproduce two independent stock CASS channels,
then compare fixed fusion policies WITHOUT fitting test judgments.
--native-title also exercises the candidate's real opt-in title retrieval.
The two-index prototype's costs are NOT the production title sidecar's costs.
Use --output outside --work if the latter is on an ephemeral filesystem.
Corpus downloads are not redistributed. Raw local CLI logs stay in --work.

MIT License (the reproduction code; dataset licenses are separate):
Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:
The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.
THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
"""
import argparse
import csv
import hashlib
import io
import json
import math
import os
import platform
import shutil
import signal
import statistics
import subprocess
import time
import urllib.request
import zipfile
from datetime import datetime, timezone
from pathlib import Path

URL = 'https://public.ukp.informatik.tu-darmstadt.de/thakur/BEIR/datasets/scifact.zip'
SHA256 = '536e14446a0ba56ed1398ab1055f39fe852686ecad24a6306c80c490fa8e0165'
USER_AGENT = 'coding_agent_session_search-agent (+https://github.com/Dicklesworthstone/coding_agent_session_search)'
PROCESS_MEASUREMENTS = []
ACTIVE_RECEIPT = None
MODEL_REVISION = 'c9745ed1d9f207416be6d2e6f8de32d1f16199bf'
POLICIES = ('reporter-max', 'max', 'weighted', 'rrf-ordinal', 'rrf')
BODY = 'I checked the configuration, applied the change, restarted the service, and confirmed that the operation now succeeds.'
TOPICS = [
 ('Configure Git commit signing with SSH keys', 'How can I authenticate the authorship of Git commits using an SSH identity?'),
 ('Rotate a PostgreSQL database password', 'How do I replace the credential used to log into a Postgres database?'),
 ('Resolve a Docker container port conflict', 'What should I do when another container already occupies a required TCP port?'),
 ('Repair an expired TLS server certificate', 'How do I restore HTTPS after the web server certificate expires?'),
 ('Create an isolated Python virtual environment', 'How can I separate Python package dependencies between projects?'),
 ('Configure npm access to a private package registry', 'How do I authenticate npm to download packages from an internal registry?'),
 ('Reset a Kafka consumer group offset', 'How do I replay old Kafka records for a consumer group?'),
 ('Cross compile a Rust executable for ARM Linux', 'How do I build a Rust binary for an ARM Linux target from another architecture?'),
 ('Increase the Kubernetes deployment replica count', 'How do I run more copies of an application in a Kubernetes deployment?'),
 ('Inspect disk usage of Docker images', 'How can I find which Docker images consume the most storage?'),
 ('Configure a corporate HTTP proxy for pip', 'How do I install Python packages when network access must use an HTTP proxy?'),
 ('Restore a deleted Git branch from the reflog', 'How can I recover a Git branch that was accidentally removed?'),
]


def write_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2, allow_nan=False)+'\n', encoding='utf-8')


def parse_json(payload, source):
    """Fail with the source of invalid evidence, without including its contents."""
    try:
        return json.loads(payload)
    except (json.JSONDecodeError, UnicodeDecodeError) as error:
        raise RuntimeError(f'Invalid JSON in {source}') from error


class Receipt:
    """Checkpoint command evidence even when indexing or a query fails.

    Full candidate arrays are written on completion/failure, not after every
    subprocess. This keeps checkpoint cost bounded during a full corpus run.
    A killed evaluator leaves an explicitly incomplete, never green receipt.
    """

    def __init__(self, path, dataset_name):
        self.path = path
        self.records = []
        self.data = {
            'schema': 2, 'status': 'running', 'requested_dataset': dataset_name,
            'started_at_utc': datetime.now(timezone.utc).isoformat(),
            'phase': 'initialization', 'process_measurements': PROCESS_MEASUREMENTS,
        }
        self.checkpoint()

    def checkpoint(self, include_records=False, **updates):
        self.data.update(updates)
        value = {**self.data, 'completed_query_passes': len(self.records)}
        if include_records:
            value['records'] = self.records
        temporary = self.path.with_name(self.path.name+'.checkpoint')
        write_json(temporary, value)
        temporary.replace(self.path)

    def fail(self, error):
        self.checkpoint(include_records=True, status='failed',
                        error={'type': type(error).__name__, 'message': str(error)},
                        finished_at_utc=datetime.now(timezone.utc).isoformat())


def env_for(base):
    env = {k:v for k,v in os.environ.items()
           if not k.startswith(('CASS_', 'CODING_AGENT_SEARCH_')) and k != 'CHATGPT_ENCRYPTION_KEY'}
    env.update(CASS_DATA_DIR=str(base/'data'), XDG_CONFIG_HOME=str(base/'config'),
               CASS_AUTO_REFRESH='0', CASS_RAW_MIRROR='0',
               CASS_INDEX_STALL_ABORT_SECS='0', CODING_AGENT_SEARCH_NO_UPDATE_PROMPT='1')
    return env


def isolation_guard():
    # Other connectors are disabled. This is the only enabled connector's
    # native macOS source and its possible key files in the tested release.
    user_root = Path.home()
    roots = [user_root/'.config/cass/chatgpt_key.bin', user_root/'.cass/chatgpt_key.bin']
    if platform.system() == 'Darwin':
        roots += [user_root/'Library/Application Support/com.openai.chat',
                  user_root/'Library/Application Support/cass/chatgpt_key.bin']
    if any(p.exists() for p in roots):
        raise SystemExit('Native ChatGPT data/key location exists. Run in a clean Linux environment/container; do not move or expose your history.')


def run(cass, args, env, log, timeout=1800):
    start = time.perf_counter()
    log.parent.mkdir(parents=True, exist_ok=True)
    expired = False
    interruption = None
    with log.with_suffix('.stdout').open('wb') as out, log.with_suffix('.stderr').open('wb') as err:
        with subprocess.Popen([cass, *args], env=env, stdout=out, stderr=err,
                              stdin=subprocess.DEVNULL, start_new_session=True) as child:
            try:
                while True:
                    pid, status, usage = os.wait4(child.pid, os.WNOHANG)
                    if pid:
                        child.returncode = os.waitstatus_to_exitcode(status)
                        break
                    if time.perf_counter()-start > timeout:
                        expired = True
                        break
                    time.sleep(0.01)
            except BaseException as error:
                interruption = error
            # All polling failures were captured above. Reap before either
            # raising them or leaving the Popen context, which otherwise waits
            # without killing a still-running indexing process.
            if child.returncode is None:
                try:
                    os.killpg(child.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass  # The process may have exited between poll/kill.
                _, status, usage = os.wait4(child.pid, 0)
                child.returncode = os.waitstatus_to_exitcode(status)
    seconds = time.perf_counter()-start
    measurement = {
        'operation': args[0], 'log': '/'.join(log.parts[-3:]), 'seconds': seconds,
        'peak_rss_bytes': usage.ru_maxrss * (1 if platform.system() == 'Darwin' else 1024),
        'cpu_seconds': usage.ru_utime+usage.ru_stime, 'returncode': child.returncode,
        'pid': child.pid, 'timed_out': expired, 'interrupted': interruption is not None,
    }
    PROCESS_MEASUREMENTS.append(measurement)
    if ACTIVE_RECEIPT is not None:
        ACTIVE_RECEIPT.checkpoint(last_process=measurement)
    if interruption is not None:
        if ACTIVE_RECEIPT is not None:
            ACTIVE_RECEIPT.fail(interruption)
        raise interruption
    if expired or child.returncode:
        error = RuntimeError(f'CASS exited {child.returncode}; inspect local log {log.name}')
        if ACTIVE_RECEIPT is not None:
            ACTIVE_RECEIPT.fail(error)
        raise error
    return log.with_suffix('.stdout').read_text(), seconds


def dataset(args, work):
    if args.dataset in ('fixture', 'adversarial'):
        docs = {f'fixture-{i:02}': {'title':t, 'text':BODY} for i,(t,q) in enumerate(TOPICS)}
        queries = {f'fixture-q-{i:02}':q for i,(t,q) in enumerate(TOPICS)}
        qrels = {f'fixture-q-{i:02}':{f'fixture-{i:02}':1} for i in range(len(TOPICS))}
        if args.dataset == 'adversarial':
            for i, (title, query) in enumerate(TOPICS):
                docs[f'fixture-{i:02}'] = {
                    'title': 'New conversation' if i % 2 == 0 else TOPICS[(i+2) % len(TOPICS)][0],
                    'text': title+'. '+query+' '+BODY,
                }
            return docs, queries, qrels, {'name': 'generic-and-misleading-titles', 'artificial': True,
                'description': 'Bodies contain the answer topic. Six titles are identical/generic; six name a different document topic. Fixed synthetic negative control, not independent relevance evidence.'}
        return docs, queries, qrels, {'name':'synthetic-title-context', 'artificial':True,
            'description':'Intentionally identical bodies; only titles distinguish topics. This is a mechanism test, not a quality benchmark.'}
    archive = Path(args.archive).resolve() if args.archive else work/'scifact.zip'
    if not archive.exists():
        print('Downloading public SciFact archive...', flush=True)
        request = urllib.request.Request(URL, headers={'User-Agent': USER_AGENT})
        with urllib.request.urlopen(request, timeout=300) as response, archive.open('wb') as target:
            shutil.copyfileobj(response, target)
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    if digest != SHA256:raise RuntimeError('SciFact archive checksum differs from pinned public release')
    with zipfile.ZipFile(archive) as z:
        corpus = [parse_json(line, f'SciFact corpus line {i}')
                  for i, line in enumerate(z.read('scifact/corpus.jsonl').splitlines(), 1)]
        query_rows = [parse_json(line, f'SciFact queries line {i}')
                      for i, line in enumerate(z.read('scifact/queries.jsonl').splitlines(), 1)]
        docs = {r['_id']:r for r in corpus}
        all_queries = {r['_id']:r['text'] for r in query_rows}
        qrels = {}
        for r in csv.DictReader(io.StringIO(z.read('scifact/qrels/test.tsv').decode()), delimiter='\t'):
            if int(r['score']) > 0:qrels.setdefault(r['query-id'], {})[r['corpus-id']] = int(r['score'])
    ids = sorted(qrels, key=lambda q:hashlib.sha256(('cass-title-repro-v1:'+q).encode()).hexdigest())
    if args.query_limit:ids = ids[:args.query_limit]
    queries = {q:all_queries[q] for q in ids}
    qrels = {q:qrels[q] for q in ids}
    if len(docs) != 5183 or not all(set(v) <= set(docs) for v in qrels.values()):
        raise RuntimeError('Unexpected SciFact corpus coverage or judgments')
    return docs, queries, qrels, {'name':'BEIR SciFact', 'url':URL, 'archive_sha256':digest,
        'split':'test', 'full_corpus_documents':5183, 'all_test_queries':300,
        'selection':'All test queries, or deterministic hash-ordered query prefix; corpus is never reduced.',
        'attribution':'Wadden et al., Fact or Fiction: Verifying Scientific Claims (EMNLP 2020); BEIR conversion by Thakur et al. (2021).',
        'licenses':'Claims/annotations CC BY 4.0; corpus abstracts ODC-By 1.0. See https://github.com/allenai/scifact/blob/master/LICENSE.md'}


def build(cass, base, docs, channel, connectors, model_dir):
    env = env_for(base)
    inputs = base/'inputs/conversations-public'
    inputs.mkdir(parents=True, exist_ok=True)
    for doc_id,doc in docs.items():
        # IDs come only from the public dataset or the built-in fixture.
        if not doc_id.replace('-','').isalnum():raise ValueError('Unsafe public document ID')
        content = doc['text'] if channel=='body' else doc['title']
        write_json(inputs/(doc_id+'.json'), {'id':doc_id, 'title':doc['title'],
                   'messages':[{'role':'user', 'content':content, 'timestamp':'2020-01-01T00:00:00Z'}]})
    config = base/'config/cass';config.mkdir(parents=True, exist_ok=True)
    config.joinpath('sources.toml').write_text('disabled_agents = '+json.dumps([c for c in connectors if c!='chatgpt'])+
        '\n\n[[sources]]\nname = "public-repro"\ntype = "local"\npaths = '+json.dumps([str(inputs.parent)])+'\n')
    data = base/'data';data.mkdir(exist_ok=True)
    models = data/'models'
    if model_dir and not models.exists():models.symlink_to(model_dir.resolve(), target_is_directory=True)
    if not models.exists():run(cass,['models','install','--yes','--robot-format','json'],env,base/'logs/model-install')
    stdout, seconds = run(cass,['index','--full','--semantic','--embedder','minilm','--json'],
                            env,base/'logs/index',timeout=7200)
    result = parse_json(stdout, f'{channel} indexing output')
    if result['conversations'] != len(docs) or result['messages'] != len(docs):
        raise RuntimeError('Unexpected source coverage; stop before evaluating')
    manifest = parse_json((data/'vector_index/semantic_manifest.json').read_text(),
                          f'{channel} semantic manifest')
    tier = manifest.get('quality_tier') or {}
    if not tier.get('ready') or tier.get('conversation_count') != len(docs):
        raise RuntimeError('Semantic tier is not ready with complete conversation coverage')
    identity = {k:tier[k] for k in ['embedder_id','model_revision','dimension','doc_count','conversation_count']}
    identity['vector_index_bytes'] = directory_bytes(data/'vector_index')
    identity['data_bytes_excluding_models'] = directory_bytes(data, exclude='models')
    return env, seconds, identity, models


def directory_bytes(root, exclude=None):
    total = 0
    for parent, dirs, files in os.walk(root, followlinks=False):
        dirs[:] = [d for d in dirs if d != exclude and not (Path(parent)/d).is_symlink()]
        total += sum((Path(parent)/f).stat().st_size for f in files
                     if not (Path(parent)/f).is_symlink())
    return total


def verify_native_title_metadata(meta, policy, weight, candidates):
    """Require request-local proof, rather than trusting accepted env vars."""
    title = meta.get('title_semantic')
    if not isinstance(title, dict):
        raise RuntimeError('Native title metadata absent; cannot prove this binary used titles')
    effective = title.get('policy', {})
    if effective.get('enabled') is not True or effective.get('fusion') != policy:
        raise RuntimeError('Native title policy differs from the requested policy')
    actual_weight = effective.get('title_weight')
    if not isinstance(actual_weight, (int, float)) or not math.isclose(
            actual_weight, weight, rel_tol=0, abs_tol=1e-7):
        raise RuntimeError('Native title weight differs from the requested weight')
    expected_budgets = {'body_candidates': candidates, 'title_candidates': candidates,
                        'total_candidates': min(2*candidates, 2000)}
    if any(effective.get(k) != v for k, v in expected_budgets.items()):
        raise RuntimeError('Native title candidate budgets differ from the requested budgets')
    if weight > 0:
        if not isinstance(title.get('title_mutation_seq'), int):
            raise RuntimeError('Native title sidecar has no verified archive revision')
        if title.get('indexed_titles', 0) <= 0 or title.get('title_conversations', 0) <= 0:
            raise RuntimeError('No title evidence was retrieved; refusing an inactive-title benchmark')
        if title.get('stale_titles_excluded', 0):
            raise RuntimeError('Fresh benchmark unexpectedly excluded stale title records')
    return title


def native_policy_requests(weight):
    # Equal-weight max preserves the reporter's unweighted max ordering.
    # The native candidate budget still differs from the reporter prototype.
    return (
        ('native-max-equal-weight', 'max', .5),
        ('native-weighted', 'weighted', weight),
        ('native-rrf', 'rrf', weight),
    )


def search(cass, query, env, log, limit, native_policy=None, weight=None):
    stdout, seconds = run(cass,['search',query,'--mode','semantic','--robot','--robot-meta',
        '--no-maintenance','--no-daemon','--limit',str(limit),
        '--fields','source_path,score,conversation_id','--timeout','120000'],env,log,timeout=180)
    payload = parse_json(stdout, 'search output')
    meta = payload.get('_meta',{})
    if meta.get('search_mode') != 'semantic':
        raise RuntimeError('Refusing a lexical fallback')
    if meta.get('timed_out'):
        raise RuntimeError('Refusing a timed-out query')
    if any(meta.get(k) for k in ['semantic_fallback_reason', 'fallback_reason', 'fallback_tier']):
        raise RuntimeError('Refusing a fallback')
    if native_policy is not None:
        verify_native_title_metadata(meta, native_policy, weight, limit)
    elif (meta.get('title_semantic') or {}).get('policy', {}).get('enabled'):
        raise RuntimeError('Body baseline unexpectedly enabled title retrieval')
    hits = [(Path(h['source_path']).stem,float(h['score'])) for h in payload['hits']]
    if any(not math.isfinite(score) for _, score in hits):
        raise RuntimeError('Nonfinite score in semantic result')
    return hits, seconds, meta


def distinct(hits):
    seen=set();out=[]
    for doc,score in hits:
        if doc not in seen:seen.add(doc);out.append(doc)
    return out[:10]


def fuse(body, title, policy, weight, candidates, rrf_k=60):
    """Fixed prototype policies; IDs break all ties deterministically.

    reporter-max preserves #540's pre-dedup budget exactly. Other policies
    deduplicate each channel by best score BEFORE fusion, cap each channel,
    then bound the union to twice that limit, like the native policy. Weighted
    sums use zero for missing fields; max compares present active fields only.
    Present cosine scores retain their sign. `rrf`
    uses competition ranks for exact score ties. `rrf-ordinal` retains the
    original experimental policy so its regressions remain inspectable.
    This Python calculation is not evidence that a native binary used titles.
    """
    if not math.isfinite(weight) or not 0 <= weight <= 1 or candidates < 1 or rrf_k <= 0:
        raise ValueError('Invalid fusion parameters')
    if policy not in POLICIES:
        raise ValueError(f'Unknown policy {policy}')
    if any(not math.isfinite(score) for _, score in body+title):
        raise ValueError('Nonfinite score')
    if policy == 'reporter-max':
        return sorted(body+title, key=lambda h:(-h[1],h[0]))[:candidates]
    scores = []
    for hits in (body if weight < 1 else [], title if weight > 0 else []):
        field = {}
        for doc, score in hits:
            field[doc] = max(score, field.get(doc, -math.inf))
        scores.append(dict(sorted(field.items(), key=lambda h:(-h[1], h[0]))[:candidates]))
    ranks = []
    for field in scores:
        ranked = {}
        last_score, rank = None, 0
        for position, (doc, score) in enumerate(field.items(), 1):
            if policy == 'rrf-ordinal' or score != last_score:
                rank = position
            ranked[doc], last_score = rank, score
        ranks.append(ranked)
    output = []
    for doc in scores[0].keys() | scores[1].keys():
        b, t = scores[0].get(doc, 0), scores[1].get(doc, 0)
        if policy == 'weighted':value = (1-weight)*b+weight*t
        elif policy == 'max':
            present = []
            if doc in scores[0]:present.append((1-weight)*b)
            if doc in scores[1]:present.append(weight*t)
            value = max(present)
        else:
            value = ((1-weight)/(rrf_k+ranks[0][doc]) if doc in ranks[0] else 0)
            value += weight/(rrf_k+ranks[1][doc]) if doc in ranks[1] else 0
        output.append((doc,value))
    return sorted(output,key=lambda h:(-h[1],h[0]))[:min(2*candidates, 2000)]


def gains_losses(records, channel):
    gained, lost = [], []
    for record in records:
        before = record['metrics']['body']['hit1']
        after = record['metrics'][channel]['hit1']
        if after and not before:gained.append(record['query_id'])
        if before and not after:lost.append(record['query_id'])
    return {'gained_hit1_query_ids':gained, 'lost_hit1_query_ids':lost}


def distribution(values):
    values = sorted(values)
    return {'count':len(values), 'median':statistics.median(values),
            'p95':values[max(0,math.ceil(.95*len(values))-1)], 'max':max(values)}


def metrics(ranking, judgments):
    ranking = list(dict.fromkeys(ranking))[:10]
    rank = next((i for i,d in enumerate(ranking,1) if d in judgments), None)
    dcg = sum((2**judgments.get(d,0)-1)/math.log2(i+1) for i,d in enumerate(ranking[:10],1))
    ideal = sum((2**g-1)/math.log2(i+1) for i,g in enumerate(sorted(judgments.values(),reverse=True)[:10],1))
    return {'hit1':int(rank==1), 'hit5':int(rank is not None and rank<=5),
        'hit10':int(rank is not None), 'mrr10':1/rank if rank else 0,
        'recall10':sum(d in judgments for d in ranking)/len(judgments), 'ndcg10':dcg/ideal if ideal else 0}


def summaries_for(records):
    summaries = []
    for pass_no in sorted({record['pass'] for record in records}):
        selected = [r for r in records if r['pass'] == pass_no]
        for channel in selected[0]['metrics']:
            summaries.append({'pass': pass_no, 'channel': channel, 'queries': len(selected),
                **gains_losses(selected, channel),
                **{name: sum(r['metrics'][channel][name] for r in selected)/len(selected)
                   for name in ['hit1', 'hit5', 'hit10', 'mrr10', 'recall10', 'ndcg10']}})
    return summaries


def replay_scores(args, receipt):
    """Recompute policy arithmetic only; never label replay as a native run."""
    source_bytes = args.replay_scores.read_bytes()
    source = parse_json(source_bytes, 'saved score receipt')
    if source.get('status', 'completed') != 'completed':
        raise RuntimeError('Cannot replay an incomplete evaluation as a completed run')
    docs, queries, qrels, provenance = dataset(args, args.work)
    if source['dataset']['name'] != provenance['name']:
        raise RuntimeError('Replay dataset differs from the recorded dataset')
    if source['documents'] != len(docs) or source['queries'] != len(queries):
        raise RuntimeError('Replay coverage differs from the recorded dataset')
    if source['candidate_limit'] != args.candidates:
        raise RuntimeError('Replay must use the recorded candidate budget')
    for identity in (source['body_index'], source['title_index']):
        if identity['model_revision'] != MODEL_REVISION or identity['dimension'] != 384:
            raise RuntimeError('Replay does not contain the pinned same-model evidence')
    seen = set()
    for old in source['records']:
        qid, pass_no = old['query_id'], old['pass']
        if qid not in queries or (pass_no, qid) in seen:
            raise RuntimeError('Replay has an unknown or duplicate query')
        seen.add((pass_no, qid))
        body, title = old['body_scores'], old['title_scores']
        if any(doc not in docs for doc, _ in body+title):
            raise RuntimeError('Replay includes an unknown document')
        channels = {'body': body, 'title': title}
        for policy in POLICIES:
            channels[policy] = fuse(body, title, policy, args.title_weight, args.candidates)
        rankings = {name: distinct(hits) for name, hits in channels.items()}
        receipt.records.append({'pass': pass_no, 'query_id': qid,
            'metrics': {name: metrics(ids, qrels[qid]) for name, ids in rankings.items()},
            'top10_ids': rankings, 'body_scores': body, 'title_scores': title})
    expected = {(p, q) for p in range(1, source['passes']+1) for q in queries}
    if seen != expected:
        raise RuntimeError('Replay is missing query/pass results')
    return {
        'kind': 'saved-score-replay', 'dataset': provenance, 'documents': len(docs),
        'queries': len(queries), 'passes': source['passes'],
        'source_receipt_sha256': hashlib.sha256(source_bytes).hexdigest(),
        'source_binary_sha256': source['binary_sha256'],
        'source_revision': source.get('source_revision'),
        'body_index': source['body_index'], 'title_index': source['title_index'],
        'candidate_limit': args.candidates, 'title_weight': args.title_weight, 'rrf_k': 60,
        'summaries': summaries_for(receipt.records),
        'notes': ['Replays saved actual MiniLM scores; does not run CASS or measure native performance.',
                  'rrf-ordinal preserves the prior experimental failure; rrf uses exact-score competition ranks.',
                  'Signed weighted cosine and tie-aware RRF are computed in Python float arithmetic.',
                  'No weight was fitted to these judgments; title weight remains 0.25 by default.'],
    }


def evaluate(args, receipt):
    isolation_guard()
    cass=shutil.which(args.cass)
    if not cass:raise RuntimeError('Put the CASS binary to evaluate on PATH or pass --cass')
    work=args.work.resolve();work.mkdir(parents=True,exist_ok=True)
    probe_env=env_for(work/'probe')
    version,_=run(cass,['--version'],probe_env,work/'probe/logs/version')
    version=version.strip()
    caps_text,_=run(cass,['capabilities','--json'],probe_env,work/'probe/logs/capabilities')
    caps=parse_json(caps_text, 'CASS capabilities output')
    docs,queries,qrels,provenance=dataset(args,work)
    start=time.perf_counter();base=work/args.dataset
    print(json.dumps({'dataset':args.dataset,'documents':len(docs),'queries':len(queries)}),flush=True)
    receipt.checkpoint(phase='body-index', software=version, dataset=provenance,
        documents=len(docs), queries=len(queries), passes=args.passes,
        binary_sha256=hashlib.sha256(Path(cass).read_bytes()).hexdigest(),
        source_revision=args.source_revision, candidate_limit=args.candidates,
        title_weight=args.title_weight, rrf_k=60)
    body_env,body_seconds,body_id,models=build(cass,base/'body',docs,'body',caps['connectors'],args.models_dir)
    if body_id['model_revision'] != MODEL_REVISION or body_id['dimension'] != 384:
        raise RuntimeError('Refusing a different model/revision/dimension for this same-model experiment')
    receipt.checkpoint(phase='title-index', body_index=body_id, body_build_seconds=body_seconds)
    title_env,title_seconds,title_id,_=build(cass,base/'title',docs,'title',caps['connectors'],models)
    if any(body_id[key] != title_id[key] for key in ['embedder_id','model_revision','dimension']):
        raise RuntimeError('Body and title model identities differ')
    receipt.checkpoint(title_index=title_id, title_build_seconds=title_seconds)
    native_index = None
    if args.native_title:
        receipt.checkpoint(phase='native-title-index')
        native_env = {**body_env, 'CASS_SEMANTIC_TITLES':'1'}
        _, seconds = run(cass,['index','--semantic','--embedder','minilm','--json'],native_env,
                         base/'logs/native-title-index',timeout=7200)
        native_index = {'incremental_index_seconds':seconds,
            'vector_index_bytes_after':directory_bytes(base/'body/data/vector_index'),
            'data_bytes_after_excluding_models':directory_bytes(base/'body/data',exclude='models')}
    receipt.checkpoint(phase='queries', native_title_index=native_index)
    records=receipt.records;query_start=time.perf_counter()
    for pass_no in range(1,args.passes+1):
        for qid,query in queries.items():
            channels={};times={};native_metadata={}
            # Alternate channel order to reduce ordering bias.
            order=['body','title'] if int.from_bytes(qid.encode(),'little')%2 else ['title','body']
            for channel in order:
                channels[channel],times[channel],_=search(cass,query,body_env if channel=='body' else title_env,
                    base/'logs'/f'p{pass_no}-{channel}-q{qid}',args.candidates)
                if any(doc not in docs for doc,score in channels[channel]):
                    raise RuntimeError('Search returned a document outside the isolated corpus')
            fusion_times = {}
            for policy in POLICIES:
                t0=time.perf_counter()
                channels[policy]=fuse(channels['body'],channels['title'],policy,args.title_weight,args.candidates)
                fusion_times[policy]=time.perf_counter()-t0
            if args.native_title:
                for name, policy, policy_weight in native_policy_requests(args.title_weight):
                    native_env={**body_env, 'CASS_SEMANTIC_TITLES':'1', 'CASS_TITLE_FUSION':policy,
                                'CASS_TITLE_WEIGHT':str(policy_weight),
                                'CASS_TITLE_BODY_CANDIDATES':str(args.candidates),
                                'CASS_TITLE_CANDIDATES':str(args.candidates),
                                'CASS_TITLE_TOTAL_CANDIDATES':str(min(2*args.candidates,2000))}
                    channels[name],times[name],meta=search(cass,query,native_env,
                        base/'logs'/f'p{pass_no}-{name}-q{qid}',args.candidates,
                        native_policy=policy,weight=policy_weight)
                    native_metadata[name]=meta['title_semantic']
                    if any(doc not in docs for doc,score in channels[name]):
                        raise RuntimeError('Native search returned a document outside the isolated corpus')
            rankings={k:distinct(v) for k,v in channels.items()}
            record={'pass':pass_no,'query_id':qid,'metrics':{k:metrics(v,qrels[qid]) for k,v in rankings.items()},
                    'top10_ids':rankings,'seconds':times,'fusion_seconds':fusion_times,
                    'body_scores':channels['body'],'title_scores':channels['title'],
                    'native_metadata':native_metadata}
            records.append(record)
            print(json.dumps({'pass':pass_no,'query_id':qid,'hit1':{k:v['hit1'] for k,v in record['metrics'].items()}}),flush=True)
    summaries=summaries_for(records)
    return {'kind':'native-cli-channel-experiment','software':version,
        'platform':{'os':platform.system(),'arch':platform.machine()},
        'dataset':provenance,'documents':len(docs),'queries':len(queries),'passes':args.passes,
        'candidate_limit':args.candidates,'body_index':body_id,'title_index':title_id,
        'binary_sha256':hashlib.sha256(Path(cass).read_bytes()).hexdigest(),
        'source_revision':args.source_revision,
        'method':'Same-model channel experiment: reporter max (budget before dedup), weighted signed-cosine sum and max, ordinal and tie-aware weighted RRF k=60 (budget after dedup). No test-label weight fitting.',
        'title_weight':args.title_weight, 'rrf_k':60, 'native_title_index':native_index,
        'native_policy_weights':{name:weight for name,_,weight in native_policy_requests(args.title_weight)}
            if args.native_title else None,
        'process_measurements':PROCESS_MEASUREMENTS,
        'latency_seconds':{c:distribution([r['seconds'][c] for r in records]) for c in records[0]['seconds']},
        'fusion_wall_seconds':{c:distribution([r['fusion_seconds'][c] for r in records]) for c in records[0]['fusion_seconds']},
        'timing_seconds':{'body_build':body_seconds,'title_build':title_seconds,
                          'queries':time.perf_counter()-query_start,'total':time.perf_counter()-start},
        'summaries':summaries,
        'notes':['Public paper records are adapted to generic conversation JSON; these are not real agent sessions.',
                 'Fixture is intentionally artificial. SciFact queries and relevance judgments are supplied by the dataset.',
                 'Raw logs remain local and may contain local paths; this summary deliberately omits them.',
                 'Prototype timings measure two independent stock indexes, not production title-sidecar overhead.']}


def main():
    global ACTIVE_RECEIPT
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--dataset',choices=['fixture','adversarial','scifact'],default='fixture')
    p.add_argument('--cass',default='cass')
    p.add_argument('--work',type=Path,default=Path('cass-public-work'))
    p.add_argument('--archive',help='Previously downloaded canonical scifact.zip')
    p.add_argument('--models-dir',type=Path,help='Reuse model files only; no history/index directories')
    p.add_argument('--query-limit',type=int,default=0)
    p.add_argument('--passes',type=int,default=1)
    p.add_argument('--candidates',type=int,default=1000)
    p.add_argument('--output',type=Path,help='Durable result path; default --work/DATASET/public-results.json')
    p.add_argument('--title-weight',type=float,default=.25,
        help='Fixed prototype/weighted/RRF weight; default .25. Native max comparison always uses .5.')
    p.add_argument('--native-title',action='store_true',help='Also index and query the candidate native title sidecar')
    p.add_argument('--source-revision',help='Exact revision of the tested binary, required with --native-title')
    p.add_argument('--replay-scores',type=Path,help='Recompute policies on a saved result without running CASS')
    args=p.parse_args()
    if args.query_limit<0 or args.passes<1 or not 10<=args.candidates<=1000:p.error('Invalid count')
    if not math.isfinite(args.title_weight) or not 0<=args.title_weight<=1:p.error('Invalid title weight')
    if args.native_title and not args.source_revision:p.error('--native-title requires --source-revision')
    if args.native_title and args.replay_scores:p.error('--native-title cannot be combined with --replay-scores')
    if os.name!='posix':p.error('Per-process wait4 resource accounting requires POSIX')
    output=args.output or args.work/args.dataset/'public-results.json'
    if args.replay_scores and output.resolve()==args.replay_scores.resolve():
        p.error('Replay output must preserve the original receipt')
    PROCESS_MEASUREMENTS.clear()
    ACTIVE_RECEIPT=Receipt(output,args.dataset)
    try:
        result=replay_scores(args,ACTIVE_RECEIPT) if args.replay_scores else evaluate(args,ACTIVE_RECEIPT)
        ACTIVE_RECEIPT.checkpoint(include_records=True,status='completed',phase='complete',
            finished_at_utc=datetime.now(timezone.utc).isoformat(),**result)
        print(json.dumps({'result':str(output),'summaries':result['summaries']},indent=2))
    except BaseException as error:
        ACTIVE_RECEIPT.fail(error)
        raise
    finally:
        ACTIVE_RECEIPT=None


if __name__=='__main__':main()

