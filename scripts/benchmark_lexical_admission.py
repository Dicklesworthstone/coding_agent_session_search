#!/usr/bin/env python3
"""Interleave fresh-process admission arms; report retained-reader phases separately.

Uses the actual cass serve protocol with one search per fresh process, matching
GH#501's index-only measurements without opening/mutating the canonical DB.
Requires Linux for per-request /proc CPU/I/O/RSS counters. rchar is NOT a hash
counter: it includes protocol/metadata reads and excludes mmap section checks.
No cache flushes, archive edits, automatic maintenance, or timing thresholds.
"""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import selectors
import statistics
import subprocess
import sys
import tempfile
import time
from typing import Any

STRICT_ENV = "CASS_LEXICAL_VERIFY_EVERY_OPEN"


def counters(pid: int) -> dict[str, float | int]:
    root = Path('/proc') / str(pid)
    stat = (root / 'stat').read_text().rsplit(')', 1)[1].split()
    ticks = os.sysconf('SC_CLK_TCK')
    io = dict(line.split(':', 1) for line in (root / 'io').read_text().splitlines())
    status = dict(line.split(':', 1) for line in (root / 'status').read_text().splitlines())
    return {
        'cpu_s': (int(stat[11]) + int(stat[12])) / ticks,
        'rchar': int(io['rchar']),
        'read_bytes': int(io['read_bytes']),
        'max_rss_kib': int(status['VmHWM'].split()[0]),
    }


class Service:
    def __init__(self, binary: Path, index: Path, strict: bool, timeout: float):
        env = dict(os.environ)
        # A value of 0 must select the candidate arm even if the caller's .env
        # contains the strict override; this is set before starting the child.
        env[STRICT_ENV] = '1' if strict else '0'
        self.stderr = tempfile.TemporaryFile()
        self.started = time.perf_counter()
        self.proc = subprocess.Popen(
            [str(binary), 'serve', '--stdio', '--index', str(index),
             '--request-timeout-ms', str(min(300_000, int(timeout * 1000)))],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.stderr, env=env,
        )
        self.timeout = timeout
        self.sequence = 0
        self.pending = bytearray()
        self.selector = selectors.DefaultSelector()
        assert self.proc.stdout is not None
        self.selector.register(self.proc.stdout, selectors.EVENT_READ)
        try:
            ready, _ = self.request('status')
            if ready.get('ok') is not True:
                raise RuntimeError(f'service status failed: {ready}')
        except BaseException:
            self.close()
            raise

    def request(self, op: str, **fields: Any) -> tuple[dict[str, Any], dict[str, Any]]:
        self.sequence += 1
        before = counters(self.proc.pid)
        started = time.perf_counter()
        assert self.proc.stdin is not None and self.proc.stdout is not None
        frame = {'op': op, 'id': self.sequence, **fields}
        self.proc.stdin.write(json.dumps(frame).encode() + b'\n')
        self.proc.stdin.flush()
        deadline = started + self.timeout
        while b'\n' not in self.pending:
            remaining = deadline - time.perf_counter()
            if remaining <= 0 or not self.selector.select(remaining):
                raise TimeoutError(f'{op} exceeded {self.timeout}s')
            chunk = os.read(self.proc.stdout.fileno(), 65536)
            if not chunk:
                self.stderr.seek(0)
                raise RuntimeError(f'service exited during {op}: {self.stderr.read()[-8000:]!r}')
            self.pending.extend(chunk)
            if len(self.pending) > 1_048_576:
                raise RuntimeError('service response exceeds its 1 MiB contract')
        line, _, rest = self.pending.partition(b'\n')
        self.pending = bytearray(rest)
        elapsed = time.perf_counter() - started
        after = counters(self.proc.pid)
        response = json.loads(line)
        if response.get('id') != self.sequence:
            raise RuntimeError(f'response ID mismatch: {response}')
        measurement = {key: after[key] - before[key]
                       for key in ('cpu_s', 'rchar', 'read_bytes')}
        measurement.update(wall_s=elapsed, max_rss_kib=after['max_rss_kib'])
        return response, measurement

    def close(self) -> float:
        # EOF is part of the public shutdown contract; include process teardown
        # in the fresh-process wall measurement without a shutdown response race.
        try:
            if self.proc.stdin is not None:
                self.proc.stdin.close()
            self.proc.wait(timeout=self.timeout)
        except (subprocess.TimeoutExpired, BrokenPipeError):
            self.proc.kill()
            self.proc.wait()
        finally:
            if self.proc.stdout is not None:
                self.proc.stdout.close()
            self.selector.close()
            self.stderr.close()
        return time.perf_counter() - self.started


def search(service: Service, query: str, limit: int) -> tuple[list[Any], dict[str, Any]]:
    response, measurement = service.request('search', query=query, limit=limit)
    if response.get('ok') is not True:
        raise RuntimeError(f'query {query!r} failed: {response}')
    result = response['result']
    measurement.update(reader_reused=result['reader_reused'],
                       setup_ms=result['setup_ms'], search_ms=result['search_ms'])
    return result['hits'], measurement


def run(args: argparse.Namespace) -> dict[str, Any]:
    binary, index = args.cass.resolve(strict=True), args.index.resolve(strict=True)
    if not (index / 'MANIFEST').is_file():
        raise ValueError('index must name a published generation containing MANIFEST')
    queries = args.query or ['performance', 'indexing']
    report: dict[str, Any] = {
        'schema_version': 1, 'binary': str(binary), 'index': str(index),
        'workload': 'fresh cass serve process with one index-only lexical search',
        'cache_policy': 'warm OS page cache; no flush; candidate primed once per query',
        'counter_contract': 'rchar counts read() bytes, NOT bytes hashed; mmap checks excluded',
        'segment_files_bytes': sum(p.stat().st_size for p in index.glob('seg-*.fslx')),
        'fresh': [], 'retained': [],
    }
    reference: dict[str, list[Any]] = {}
    # Prime both arms without mixing these observations into steady-state timing.
    for query in queries:
        for strict in (True, False):
            service = Service(binary, index, strict, args.timeout)
            try:
                hits, _ = search(service, query, args.limit)
                if query in reference and hits != reference[query]:
                    raise AssertionError(f'priming hits differ for {query!r}')
                reference[query] = hits
            finally:
                service.close()
    if not any(reference.values()):
        raise ValueError('all queries returned zero hits; use representative matching queries')
    for round_id in range(args.rounds):
        for query in queries:
            # Alternate ordering to avoid assigning cache/thermal drift to one arm.
            for strict in ((True, False) if round_id % 2 == 0 else (False, True)):
                service = Service(binary, index, strict, args.timeout)
                try:
                    hits, measurement = search(service, query, args.limit)
                    if hits != reference[query]:
                        raise AssertionError(f'hit/score/order mismatch: strict={strict}, {query!r}')
                    if measurement['reader_reused'] is not False:
                        raise AssertionError('fresh process unexpectedly reused an admitted reader')
                finally:
                    process_wall = service.close()
                report['fresh'].append(dict(measurement, process_wall_s=process_wall,
                    round=round_id, query=query, arm='strict' if strict else 'candidate'))
    # Strict initial admission makes retained reuse independent of cross-process proofs.
    service = Service(binary, index, True, args.timeout)
    try:
        sequence = [('first_admission', queries[0], False)]
        sequence += [('retained_query', q, True) for _ in range(args.rounds) for q in queries]
        for phase, query, reused in sequence:
            hits, measurement = search(service, query, args.limit)
            if hits != reference[query] or measurement['reader_reused'] is not reused:
                raise AssertionError(f'retained-reader contract failed during {phase}')
            report['retained'].append(dict(measurement, phase=phase, query=query))
        response, measurement = service.request('unload')
        if response.get('ok') is not True:
            raise AssertionError(response)
        report['retained'].append(dict(measurement, phase='unload'))
        hits, measurement = search(service, queries[0], args.limit)
        if hits != reference[queries[0]] or measurement['reader_reused'] is not False:
            raise AssertionError('admission after unload must open a new reader')
        report['retained'].append(dict(measurement, phase='admission_after_unload'))
        response, measurement = service.request('reload')
        if response.get('ok') is not True:
            raise AssertionError(response)
        report['retained'].append(dict(measurement, phase='reload_admission'))
        hits, measurement = search(service, queries[0], args.limit)
        if hits != reference[queries[0]] or measurement['reader_reused'] is not True:
            raise AssertionError('query after reload should reuse its newly admitted reader')
        report['retained'].append(dict(measurement, phase='query_after_reload'))
    finally:
        service.close()
    report['fresh_medians'] = {
        arm: {key: statistics.median(row[key] for row in report['fresh'] if row['arm'] == arm)
              for key in ('process_wall_s', 'wall_s', 'cpu_s', 'rchar', 'read_bytes', 'max_rss_kib')}
        for arm in ('strict', 'candidate')
    }
    report['hits_equal'] = True
    return report


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--cass', type=Path, required=True)
    parser.add_argument('--index', type=Path, required=True)
    parser.add_argument('--query', action='append', help='repeat for each representative query')
    parser.add_argument('--rounds', type=int, default=7)
    parser.add_argument('--limit', type=int, default=10)
    parser.add_argument('--timeout', type=float, default=120)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    if not sys.platform.startswith('linux'):
        parser.error('Linux /proc is required for honest per-request resource counters')
    if args.rounds < 1 or not 1 <= args.limit <= 100 or not 0 < args.timeout <= 300:
        parser.error('rounds >= 1, limit 1..100, timeout (0, 300] required')
    report = run(args)
    # Do not overwrite another qualification's evidence.
    with args.output.open('x', encoding='utf-8') as out:
        json.dump(report, out, indent=2)
        out.write('\n')
    print(json.dumps(report['fresh_medians'], indent=2))


if __name__ == '__main__':
    main()
