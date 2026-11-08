#!/usr/bin/env python3
"""Verify the unchanged released design package before packaging or promotion."""
import hashlib
import json
from pathlib import Path, PurePosixPath
import re
import stat


def verify(root):
    pin = json.loads((root / 'packaging/design-system.json').read_text())
    if (pin.get('schema_version') != 2
            or pin.get('repository') != 'NDDev-Technocracy-OpenNetwork/nddev-opennetwork-design-system'
            or not re.fullmatch(r'v\d+\.\d+\.\d+(?:-[a-z0-9.]+)?', pin.get('tag', ''))
            or any(not re.fullmatch(r'[0-9a-f]{40}', pin.get(key, '')) for key in ('commit', 'git_tree'))
            or pin.get('subdirectory') != 'packages/rust'
            or pin.get('directory') != 'vendor/nddev-opennetwork-design-tokens'):
        raise ValueError('invalid released design-system pin')
    directory = root / pin['directory']
    if directory.is_symlink() or not directory.is_dir():
        raise ValueError('design package is not a local source directory')
    files = {}
    for path in sorted(directory.rglob('*')):
        mode = path.lstat().st_mode
        if stat.S_ISLNK(mode) or not (stat.S_ISREG(mode) or stat.S_ISDIR(mode)):
            raise ValueError('design package contains a nonregular source')
        if stat.S_ISREG(mode):
            with path.open('rb') as stream:
                files[path.relative_to(directory).as_posix()] = hashlib.file_digest(stream, 'sha256').hexdigest()
    entries = pin.get('files', [])
    if (not isinstance(entries, list) or not 1 <= len(entries) <= 4096
            or any(not isinstance(entry, dict) or set(entry) != {'path', 'sha256'}
                   or not isinstance(entry['path'], str)
                   or not isinstance(entry['sha256'], str)
                   or not re.fullmatch(r'[0-9a-f]{64}', entry['sha256']) for entry in entries)):
        raise ValueError('invalid design package file records')
    expected = {entry['path']: entry['sha256'] for entry in entries}
    if (len(expected) != len(entries) or files != expected
            or any(PurePosixPath(name).is_absolute() or '..' in PurePosixPath(name).parts for name in expected)):
        raise ValueError('released design package inventory or bytes changed')
    digest = hashlib.sha256()
    for name, value in sorted(files.items()):
        digest.update(name.encode() + b'\0' + bytes.fromhex(value))
    if digest.hexdigest() != pin.get('directory_sha256'):
        raise ValueError('design package directory digest changed')
    return {key: pin[key] for key in ('repository', 'tag', 'commit', 'subdirectory', 'git_tree', 'directory_sha256')}


if __name__ == '__main__':
    print(json.dumps(verify(Path(__file__).resolve().parent.parent), sort_keys=True))
