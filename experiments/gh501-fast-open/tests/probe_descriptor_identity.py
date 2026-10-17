"""Actual Linux filesystem probe; NOT Rust/Quill validation or a benchmark."""
from pathlib import Path
import json
import os
import tempfile
import time


def identity(fd):
    s = os.fstat(fd)
    return dict(dev=s.st_dev, ino=s.st_ino, size=s.st_size, mtime_ns=s.st_mtime_ns, ctime_ns=s.st_ctime_ns)


def main():
    findings = {}
    with tempfile.TemporaryDirectory(prefix='gh501-os-probe-') as directory:
        root = Path(directory); path = root/'segment'; path.write_bytes(b'a'*256)
        with path.open('r+b', buffering=0) as file:
            before = identity(file.fileno()); times = os.fstat(file.fileno())
            time.sleep(0.02); os.pwrite(file.fileno(), b'b', 17)
            changed = identity(file.fileno())
            assert before != changed and before['size'] == changed['size']
            findings['same_length_rewrite_changes_identity'] = True
            os.utime(file.fileno(), ns=(times.st_atime_ns, times.st_mtime_ns))
            restored = identity(file.fileno())
            assert before['mtime_ns'] == restored['mtime_ns'] and before['ctime_ns'] != restored['ctime_ns']
            findings['restoring_mtime_does_not_restore_ctime'] = True
            os.ftruncate(file.fileno(), 128)
            assert identity(file.fileno())['size'] == 128
            findings['truncation_changes_length'] = True
            replacement = root/'replacement'; replacement.write_bytes(b'a'*256)
            os.replace(replacement, path)
            with path.open('rb') as new:
                assert identity(new.fileno())['ino'] != before['ino']
                assert identity(file.fileno())['ino'] == before['ino']
            findings['replacement_uses_new_inode_retained_descriptor_keeps_old_inode'] = True
    print(json.dumps({'kind':'filesystem_assumption_probe', 'native_quill_tests_executed':False, 'findings':findings}, indent=2))

if __name__ == '__main__': main()
