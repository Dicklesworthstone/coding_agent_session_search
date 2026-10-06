#!/usr/bin/env python3
"""Measure fresh-process and retained-reader lexical admission independently.

By default this records one baseline, NOT an alleged fast-open comparison.
To compare implementations, supply a separately built --candidate-cass binary.
No verification-policy environment switch is assumed or silently synthesized.
Uses one index-only `cass serve` search per fresh process, not the complete
ordinary `cass search` command. Requires Linux /proc for per-request counters.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import resource
import selectors
import statistics
import subprocess
import sys
import tempfile
import time
from typing import Any


def digest(path: Path) -> str:
    with path.open('rb') as source:
        return hashlib.file_digest(source, 'sha256').hexdigest()


def child_cpu() -> float:
    usage = resource.getrusage(resource.RUSAGE_CHILDREN)
    return usage.ru_utime + usage.ru_stime


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
    def __init__(self, binary: Path, index: Path, timeout: float):
        self.stderr = tempfile.TemporaryFile()
        self.cpu_before = child_cpu()
        self.started = time.perf_counter()
        try:
            self.proc = subprocess.Popen(
                [str(binary), 'serve', '--stdio', '--index', str(index),
                 '--request-timeout-ms', str(max(1, min(300_000, int(timeout * 1000))))],
                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.stderr,
            )
        except BaseException:
            self.stderr.close()
            raise
        self.timeout = timeout
        self.sequence = 0
        self.pending = bytearray()
        self.closed = False
        self.selector = selectors.DefaultSelector()
        assert self.proc.stdout is not None
        self.selector.register(self.proc.stdout, selectors.EVENT_READ)
        try:
            ready, _ = self.request('status')
            if ready.get('ok') is not True:
                raise RuntimeError(f'service status failed: {ready}')
        except BaseException:
            self.close(check=False)
            raise

    def request(self, op: str, **fields: Any) -> tuple[dict[str, Any], dict[str, Any]]:
        self.sequence += 1
        before = counters(self.proc.pid)
        started = time.perf_counter()
        assert self.proc.stdin is not None and self.proc.stdout is not None
        frame = {'op': op, 'id': self.sequence, **fields}
        encoded = json.dumps(frame).encode() + b'\n'
        if len(encoded) > 65_537:
            raise ValueError('request exceeds service frame limit')
        self.proc.stdin.write(encoded)
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
        if not isinstance(response, dict) or response.get('id') != self.sequence:
            raise RuntimeError(f'response ID mismatch: {response}')
        measurement = {key: after[key] - before[key]
                       for key in ('cpu_s', 'rchar', 'read_bytes')}
        measurement.update(wall_s=elapsed, max_rss_kib=after['max_rss_kib'])
        return response, measurement

    def close(self, *, check: bool = True) -> dict[str, float]:
        if self.closed:
            raise RuntimeError('service already closed')
        self.closed = True
        timed_out = False
        try:
            try:
                if self.proc.stdin is not None:
                    self.proc.stdin.close()
            except BrokenPipeError:
                pass
            try:
                self.proc.wait(timeout=self.timeout)
            except subprocess.TimeoutExpired:
                timed_out = True
                self.proc.kill()
                self.proc.wait()
            measured = {'process_wall_s': time.perf_counter() - self.started,
                        'process_cpu_s': child_cpu() - self.cpu_before}
            if check and (timed_out or self.proc.returncode != 0):
                self.stderr.seek(0)
                raise RuntimeError(f'unclean service shutdown: exit={self.proc.returncode}, '
                                   f'timeout={timed_out}, stderr={self.stderr.read()[-8000:]!r}')
            return measured
        finally:
            if self.proc.stdout is not None:
                self.proc.stdout.close()
            self.selector.close()
            self.stderr.close()


def search(service: Service, query: str, limit: int) -> tuple[list[Any], dict[str, Any]]:
    response, measurement = service.request('search', query=query, limit=limit)
    if response.get('ok') is not True:
        raise RuntimeError(f'query {query!r} failed: {response}')
    result = response['result']
    if not isinstance(result.get('hits'), list) or type(result.get('reader_reused')) is not bool:
        raise RuntimeError(f'invalid search response: {response}')
    measurement.update(reader_reused=result['reader_reused'],
                       setup_ms=result['setup_ms'], search_ms=result['search_ms'])
    return result['hits'], measurement


def retained(binary: Path, index: Path, args: argparse.Namespace,
             queries: list[str], reference: dict[str, list[Any]]) -> list[dict[str, Any]]:
    service = Service(binary, index, args.timeout)
    rows = []
    try:
        sequence = [('first_admission', queries[0], False)]
        sequence += [('retained_query', q, True) for _ in range(args.rounds) for q in queries]
        for phase, query, reused in sequence:
            hits, measurement = search(service, query, args.limit)
            if hits != reference[query] or measurement['reader_reused'] is not reused:
                raise AssertionError(f'retained-reader contract failed during {phase}')
            rows.append(dict(measurement, phase=phase, query=query))
        for op, phase, reused in (('unload', 'admission_after_unload', False),
                                  ('reload', 'query_after_reload', True)):
            response, measurement = service.request(op)
            if response.get('ok') is not True:
                raise AssertionError(response)
            rows.append(dict(measurement, phase=op))
            hits, measurement = search(service, queries[0], args.limit)
            if hits != reference[queries[0]] or measurement['reader_reused'] is not reused:
                raise AssertionError(f'reader contract failed during {phase}')
            rows.append(dict(measurement, phase=phase, query=queries[0]))
    except BaseException:
        service.close(check=False)
        raise
    service.close()
    return rows


def run(args: argparse.Namespace) -> dict[str, Any]:
    index = args.index.resolve(strict=True)
    if not (index / 'MANIFEST').is_file():
        raise ValueError('index must name a published generation containing MANIFEST')
    binaries = {'baseline': args.cass.resolve(strict=True)}
    if args.candidate_cass is not None:
        binaries['candidate'] = args.candidate_cass.resolve(strict=True)
    binary_hashes = {arm: digest(path) for arm, path in binaries.items()}
    if len(binary_hashes) == 2 and len(set(binary_hashes.values())) != 2:
        raise ValueError('baseline and candidate binaries are identical; no comparison exists')
    manifest_hash = digest(index / 'MANIFEST')
    queries = args.query or ['performance', 'indexing']
    report: dict[str, Any] = {
        'schema_version': 2, 'index': str(index), 'manifest_sha256': manifest_hash,
        'mode': 'comparison' if len(binaries) == 2 else 'baseline_only',
        'binaries': {arm: {'path': str(path), 'sha256': binary_hashes[arm]}
                     for arm, path in binaries.items()},
        'workload': 'fresh cass serve process with one index-only lexical search; not cass search CLI',
        'cache_policy': 'warm OS page cache; no flush; each arm primed once per query',
        'verification_policy': 'selected binaries control policy; harness does not set an override',
        'counter_contract': {
            'cpu_s': 'per-request /proc CPU delta; quantized to kernel clock ticks',
            'process_cpu_s': 'reaped-child user+system CPU including startup/status/search/teardown',
            'rchar': 'read() bytes, NOT bytes hashed; mmap section verification excluded',
            'read_bytes': 'kernel storage I/O bytes, NOT verification work',
            'max_rss_kib': 'process lifetime high-water RSS, NOT per-request allocation',
            'process_wall_s': 'spawn through clean EOF exit, including harness status exchange',
        },
        'all_segment_files_bytes': sum(p.stat().st_size for p in index.glob('seg-*.fslx')),
        'fresh': [], 'retained': [],
    }
    reference: dict[str, list[Any]] = {}
    for query in queries:
        for binary in binaries.values():
            service = Service(binary, index, args.timeout)
            try:
                hits, _ = search(service, query, args.limit)
                if query in reference and hits != reference[query]:
                    raise AssertionError(f'priming hits differ for {query!r}')
                reference[query] = hits
            except BaseException:
                service.close(check=False)
                raise
            service.close()
    if not any(reference.values()):
        raise ValueError('all queries returned zero hits; use representative matching queries')
    for round_id in range(args.rounds):
        for query in queries:
            arms = list(binaries.items())
            if round_id % 2:
                arms.reverse()
            for arm, binary in arms:
                service = Service(binary, index, args.timeout)
                try:
                    hits, measurement = search(service, query, args.limit)
                    if hits != reference[query]:
                        raise AssertionError(f'hit/score/order mismatch: arm={arm}, {query!r}')
                    if measurement['reader_reused'] is not False:
                        raise AssertionError('fresh process unexpectedly reused an admitted reader')
                except BaseException:
                    service.close(check=False)
                    raise
                measurement.update(service.close())
                report['fresh'].append(dict(measurement, round=round_id, query=query, arm=arm))
    for arm, binary in binaries.items():
        report['retained'].extend(dict(row, arm=arm) for row in
                                  retained(binary, index, args, queries, reference))
    if digest(index / 'MANIFEST') != manifest_hash:
        raise RuntimeError('index manifest changed during qualification; discard measurements')
    if any(digest(path) != binary_hashes[arm] for arm, path in binaries.items()):
        raise RuntimeError('a binary changed during qualification; discard measurements')
    report['fresh_medians'] = {
        arm: {key: statistics.median(row[key] for row in report['fresh'] if row['arm'] == arm)
              for key in ('process_wall_s', 'process_cpu_s', 'wall_s', 'cpu_s',
                          'rchar', 'read_bytes', 'max_rss_kib')}
        for arm in binaries
    }
    report['hits_equal'] = True
    return report


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--cass', type=Path, required=True, help='baseline binary')
    parser.add_argument('--candidate-cass', type=Path, help='optional distinct candidate binary')
    parser.add_argument('--index', type=Path, required=True)
    parser.add_argument('--query', action='append', help='repeat for each representative query')
    parser.add_argument('--rounds', type=int, default=7)
    parser.add_argument('--limit', type=int, default=10)
    parser.add_argument('--timeout', type=float, default=120)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    if not sys.platform.startswith('linux'):
        parser.error('Linux /proc is required for per-request resource counters')
    if args.rounds < 1 or not 1 <= args.limit <= 100 or not 0 < args.timeout <= 300:
        parser.error('rounds >= 1, limit 1..100, timeout (0, 300] required')
    if args.query and any(not q.strip() or len(q.encode()) > 4096 for q in args.query):
        parser.error('queries must be nonempty and at most 4096 UTF-8 bytes')
    if args.output.exists():
        parser.error('output already exists; evidence will not be overwritten')
    report = run(args)
    with args.output.open('x', encoding='utf-8') as out:
        json.dump(report, out, indent=2, allow_nan=False)
        out.write('\n')
    print(json.dumps(report['fresh_medians'], indent=2))


if __name__ == '__main__':
    main()
