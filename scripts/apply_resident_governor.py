#!/usr/bin/env python3
"""Integrate resident admission without rewriting a concurrently edited monolith.

Each replacement has an exact precondition. Already-integrated input is verified
byte-for-byte at the call-site level; partial or conflicting changes fail closed.
The workflow runs this against its immutable checkout and only fast-forwards main
AFTER Rust tests/checks pass. No branch, force push, dependency edit or deletion.
"""
from pathlib import Path
import hashlib

PATH = Path('src/indexer/responsiveness.rs')
EDITS = [
    ('use std::collections::VecDeque;\n', 'mod resident;\n\nuse std::collections::VecDeque;\n'),
    ('    fn run(&self) {\n        loop {\n            self.step_once();',
     '    fn run(&self) {\n        loop {\n            resident::sample(&self.cfg);\n            self.step_once();'),
    ('    g.current_capacity.load(Ordering::Relaxed)\n',
     '    g.current_capacity\n        .load(Ordering::Relaxed)\n        .min(resident::capacity_pct())\n'),
    ('    scale_inflight_byte_limit(desired_bytes, current_capacity_pct(), &g.cfg)\n',
     '    resident::limit_inflight_bytes(scale_inflight_byte_limit(\n        desired_bytes,\n        current_capacity_pct(),\n        &g.cfg,\n    ))\n'),
    ('    pub calibration: Option<CalibrationTelemetry>,\n',
     '    pub calibration: Option<CalibrationTelemetry>,\n    /// Live resident-memory observation, absent before the sampler has run.\n    #[serde(skip_serializing_if = "Option::is_none")]\n    pub memory: Option<resident::MemoryTelemetry>,\n'),
    ('            recent_decisions: recent,\n            calibration,\n',
     '            recent_decisions: recent,\n            calibration,\n            memory: resident::telemetry(),\n'),
    ('        recent_decisions: Vec::new(),\n        calibration,\n',
     '        recent_decisions: Vec::new(),\n        calibration,\n        memory: resident::telemetry(),\n'),
]


def integrate(text: str) -> str:
    result = text
    for old, new in EDITS:
        if new in result:
            if result.count(new) != 1:
                raise ValueError(f'Non-unique integrated anchor: {new!r}')
            continue
        if result.count(old) != 1:
            raise ValueError(f'Expected exactly one unmodified anchor: {old!r}')
        result = result.replace(old, new, 1)
    return result


def main() -> None:
    before = PATH.read_bytes()
    after = integrate(before.decode('utf-8')).encode('utf-8')
    assert integrate(after.decode('utf-8')).encode('utf-8') == after, 'Integration must be idempotent'
    print(f'input_sha256={hashlib.sha256(before).hexdigest()}')
    print(f'output_sha256={hashlib.sha256(after).hexdigest()}')
    if before != after:
        PATH.write_bytes(after)
    print(f'changed={before != after}')


if __name__ == '__main__':
    main()
