#!/usr/bin/env python3
"""Bundle hash-verified native executables without compiling or enrolling devices."""
import argparse
import gzip
import hashlib
import io
import json
from pathlib import Path
import platform
import plistlib
import re
import shutil
import stat
import subprocess
import tarfile
import tempfile


def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary-dir', required=True, type=Path)
    parser.add_argument('--build-info', required=True, type=Path)
    parser.add_argument('--out', required=True, type=Path)
    parser.add_argument('--sign-identity', default='-',
                        help='existing local signing certificate SHA-1; default: ad hoc engineering build')
    args = parser.parse_args()
    if (platform.system(), platform.machine()) != ('Darwin', 'arm64'):
        raise SystemExit('bundle on a native macOS arm64 executor')
    if args.sign_identity != '-' and not re.fullmatch(r'[A-Fa-f0-9]{40}', args.sign_identity):
        raise SystemExit('select an existing signing identity by certificate digest')
    base = json.loads(args.build_info.read_text())
    source = base['source']
    if (base['schema_version'] != 1 or base['target'] != 'aarch64-apple-darwin'
            or not re.fullmatch(r'[0-9a-f]{40}', source)
            or set(base['binaries']) != {'rds', 'rds-agent', 'nddev-remote'}):
        raise SystemExit('unsupported input build identity')
    version = re.fullmatch(r'(\d+\.\d+\.\d+)(?:[-+][A-Za-z0-9.+-]+)?', base['version'])
    if not version:
        raise SystemExit('unsupported application version')
    for name, selected in base['binaries'].items():
        path = args.binary_dir / name
        info = path.lstat()
        if (not stat.S_ISREG(info.st_mode) or not info.st_mode & 0o111
                or not 0 < info.st_size <= 256 * 1024 * 1024 or digest(path) != selected):
            raise SystemExit('input executable does not match build metadata: ' + name)
        dependencies = subprocess.check_output(['otool', '-L', str(path)], text=True)
        for line in dependencies.splitlines()[1:]:
            dependency = line.strip().split(' (', 1)[0]
            if not dependency.startswith(('/System/Library/', '/usr/lib/')):
                raise SystemExit('runtime depends on an unbundled library: ' + dependency)
    root = Path(__file__).resolve().parent.parent
    settings = json.loads((root / 'packaging/macos.json').read_text())
    notices = json.loads((root / 'packaging/notices.json').read_text())
    if base.get('notices') != {name: digest(root / path) for name, path in notices.items()}:
        raise SystemExit('native package notices differ from the selected source')
    args.out.mkdir(parents=True, exist_ok=True, mode=0o700)
    filename = 'nddev-remote-macos-apps-' + source + '.tar.gz'
    archive = args.out / filename
    if archive.exists() or (args.out / 'apps-build-info.json').exists():
        raise SystemExit('retain the existing package; select a fresh output directory')
    with tempfile.TemporaryDirectory(prefix='nddev-remote-apps-') as temporary:
        staging = Path(temporary)
        applications = []
        for app in settings['applications']:
            bundle = staging / (app['name'] + '.app')
            contents = bundle / 'Contents'
            (contents / 'MacOS').mkdir(parents=True)
            (contents / 'Resources').mkdir()
            executable = contents / 'MacOS' / app['executable']
            shutil.copyfile(args.binary_dir / app['executable'], executable)
            executable.chmod(0o755)
            info = {
                'CFBundleIdentifier': app['identifier'], 'CFBundleName': app['name'],
                'CFBundleDisplayName': app['name'], 'CFBundleExecutable': app['executable'],
                'CFBundleIconFile': settings['icon']['name'],
                'CFBundleVersion': version[1], 'CFBundleShortVersionString': version[1],
                'CFBundlePackageType': 'APPL', 'LSMinimumSystemVersion': settings['minimum_os'],
                'LSUIElement': app['background'], 'NSHighResolutionCapable': True,
            }
            (contents / 'Info.plist').write_bytes(plistlib.dumps(info, sort_keys=True))
            (contents / 'Resources' / 'build-info.json').write_text(
                json.dumps(base, indent=2, sort_keys=True) + '\n')
            resources = {settings['icon']['name']: settings['icon']['source']}
            resources.update({'Notices/' + name: path for name, path in notices.items()})
            for name, source_path in resources.items():
                source_file = root / source_path
                if (source_file.is_symlink() or not source_file.is_file()
                        or Path(name).is_absolute() or '..' in Path(name).parts):
                    raise SystemExit('invalid application resource')
                destination = contents / 'Resources' / name
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(source_file, destination)
            subprocess.run(['codesign', '--force', '--sign', args.sign_identity,
                            '--options', 'runtime', '--timestamp=none', str(bundle)], check=True)
            subprocess.run(['codesign', '--verify', '--deep', '--strict', str(bundle)], check=True)
            subprocess.run([str(executable), '--help'], stdout=subprocess.DEVNULL, check=True)
            applications.append({'name': app['name'], 'identifier': app['identifier'],
                                 'executable_sha256': digest(executable)})
        files = {str(p.relative_to(staging)): digest(p)
                 for p in sorted(staging.rglob('*')) if p.is_file()}
        receipt = {
            'schema_version': 1, 'runtime': base, 'applications': applications, 'files': files,
            'builder_sha256': digest(Path(__file__).resolve()),
            'settings_sha256': digest(root / 'packaging/macos.json'),
            'signing': 'ad-hoc' if args.sign_identity == '-' else 'local-certificate',
            'notarized': False,
        }
        metadata = (json.dumps(receipt, indent=2, sort_keys=True) + '\n').encode()
        (staging / 'apps-build-info.json').write_bytes(metadata)
        with archive.open('xb') as raw:
            with gzip.GzipFile(fileobj=raw, mode='wb', filename='', mtime=0) as compressed:
                with tarfile.open(fileobj=compressed, mode='w') as package:
                    for path in sorted(staging.rglob('*')):
                        if not path.is_file():
                            continue
                        data = path.read_bytes()
                        info = tarfile.TarInfo(str(path.relative_to(staging)))
                        info.size = len(data)
                        info.mode = 0o755 if 'MacOS' in path.parts else 0o644
                        package.addfile(info, io.BytesIO(data))
        (args.out / 'apps-build-info.json').write_bytes(metadata)
    (args.out / 'apps-SHA256SUMS').write_text(digest(archive) + '  ' + filename + '\n')
    print(json.dumps({'source': source, 'archive': filename, 'sha256': digest(archive),
                      'signing': receipt['signing'], 'notarized': False}))


if __name__ == '__main__':
    main()
