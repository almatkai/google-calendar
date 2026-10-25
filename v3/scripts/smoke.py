#!/usr/bin/env python3
"""Extract, verify complete ZIP inventory and run with no source/toolchain PATH."""
import argparse
import hashlib
import json
from pathlib import Path, PurePosixPath
import stat
import tempfile
import zipfile
from protocol_test import run

parser = argparse.ArgumentParser()
parser.add_argument('--archive', type=Path, required=True)
parser.add_argument('--target', choices=['aarch64-apple-darwin', 'x86_64-pc-windows-msvc'], required=True)
args = parser.parse_args()
with tempfile.TemporaryDirectory(prefix='calendar-extracted-') as d:
    root = Path(d)
    with zipfile.ZipFile(args.archive) as z:
        manifest = json.loads(z.read('extension.json'))
        assert len(z.namelist()) == len(set(z.namelist()))
        assert set(z.namelist()) == set(manifest['files']) | {'extension.json'}
        for entry in z.infolist():
            path = PurePosixPath(entry.filename)
            assert not path.is_absolute() and all(p not in ('.', '..') and ':' not in p and '\\' not in p for p in path.parts)
            assert not stat.S_ISLNK(entry.external_attr >> 16)
            data = z.read(entry)
            if entry.filename != 'extension.json':
                assert hashlib.sha256(data).hexdigest() == manifest['files'][entry.filename]
            target = root.joinpath(*path.parts)
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(data)
    binary = root / 'bin' / ('tezbar-google-calendar.exe' if 'windows' in args.target else 'tezbar-google-calendar')
    binary.chmod(0o755)
    run(binary)
print(json.dumps({'passed': True, 'target': args.target, 'inventory': True, 'noToolchainPATH': True}))
