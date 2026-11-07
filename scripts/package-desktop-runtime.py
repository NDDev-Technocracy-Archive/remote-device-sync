#!/usr/bin/env python3
"""Package existing desktop binaries; compilation and native acceptance are separate."""
import argparse
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import platform
import re
import stat
import subprocess
import tarfile


def command(*args):
    return subprocess.check_output(args, text=True).strip()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary-dir', required=True, type=Path)
    parser.add_argument('--target', required=True,
                        choices=['x86_64-unknown-linux-gnu', 'aarch64-apple-darwin'])
    parser.add_argument('--features', required=True)
    parser.add_argument('--out', required=True, type=Path)
    args = parser.parse_args()
    expected = {'x86_64-unknown-linux-gnu': ('Linux', 'x86_64'),
                'aarch64-apple-darwin': ('Darwin', 'arm64')}
    if (platform.system(), platform.machine()) != expected[args.target]:
        raise SystemExit('package on the selected native executor')
    source = command('git', 'rev-parse', 'HEAD')
    if not re.fullmatch(r'[0-9a-f]{40}', source):
        raise SystemExit('unsupported source identity')
    if command('git', 'status', '--porcelain', '--untracked-files=no'):
        raise SystemExit('tracked source changed during the build')
    features = sorted(set(args.features.split(',')))
    if not features or any(not re.fullmatch(r'[a-z0-9_-]+/[a-z0-9_-]+', f) for f in features):
        raise SystemExit('invalid selected feature list')
    if 'rds-cli/desktop' not in features:
        raise SystemExit('the desktop runtime requires the native viewer feature')
    if not any(f in features for f in ['rds-agent/desktop', 'rds-agent/portal']):
        raise SystemExit('the desktop runtime requires its agent feature')
    payload = {}
    for name in ['rds', 'rds-agent', 'nddev-remote']:
        path = args.binary_dir / name
        info = path.lstat()
        if (not stat.S_ISREG(info.st_mode) or not os.access(path, os.X_OK)
                or not 0 < info.st_size <= 256 * 1024 * 1024):
            raise SystemExit('missing or unsafe native executable: ' + name)
        payload[name] = path.read_bytes()
    root = Path(__file__).resolve().parent.parent
    notices = json.loads((root / 'packaging/notices.json').read_text())
    for name, source_path in notices.items():
        path = root / source_path
        info = path.lstat()
        if (not stat.S_ISREG(info.st_mode) or not 0 < info.st_size <= 128 * 1024
                or Path(name).is_absolute() or '..' in Path(name).parts or name in payload):
            raise SystemExit('invalid package notice: ' + name)
        payload[name] = path.read_bytes()
    manifest = {
        'schema_version': 1,
        'source': source,
        'version': Path('VERSION').read_text().strip(),
        'target': args.target,
        'features': features,
        'rustc': command('rustc', '--version', '--verbose'),
        'binaries': {name: hashlib.sha256(payload[name]).hexdigest()
                     for name in ['rds', 'rds-agent', 'nddev-remote']},
        'notices': {name: hashlib.sha256(payload[name]).hexdigest() for name in notices},
        'build': {key: os.environ[key] for key in ['GITHUB_REPOSITORY', 'GITHUB_WORKFLOW',
                                                 'GITHUB_RUN_ID', 'GITHUB_RUN_ATTEMPT',
                                                 'GITHUB_REF'] if key in os.environ},
    }
    metadata = (json.dumps(manifest, indent=2, sort_keys=True) + '\n').encode()
    payload['build-info.json'] = metadata
    args.out.mkdir(mode=0o700, parents=True, exist_ok=False)
    archive = args.out / ('nddev-remote-' + args.target + '-' + source + '.tar.gz')
    with archive.open('xb') as raw:
        with gzip.GzipFile(fileobj=raw, mode='wb', filename='', mtime=0) as compressed:
            with tarfile.open(fileobj=compressed, mode='w') as bundle:
                for name, data in sorted(payload.items()):
                    info = tarfile.TarInfo(name)
                    info.size = len(data)
                    info.mode = 0o755 if name in manifest['binaries'] else 0o644
                    bundle.addfile(info, io.BytesIO(data))
    (args.out / 'build-info.json').write_bytes(metadata)
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    (args.out / 'SHA256SUMS').write_text(digest + '  ' + archive.name + '\n')
    print(json.dumps({'source': source, 'target': args.target, 'archive': archive.name,
                      'sha256': digest, 'binaries': manifest['binaries']}))


if __name__ == '__main__':
    main()
