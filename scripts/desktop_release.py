#!/usr/bin/env python3
"""Promote the exact checked main desktop packages; never rebuild at publication."""
import argparse
import hashlib
import json
from pathlib import Path
import plistlib
import shutil
import tarfile
import tempfile

import release
import design_system

REPOSITORY = 'NDDev-Technocracy-OpenNetwork/nddev-remote'
BINARIES = {'rds', 'rds-agent', 'nddev-remote'}
FEATURES = {
    'x86_64-unknown-linux-gnu': sorted(['rds-cli/desktop', 'rds-cli/transport-noq',
                                      'rds-agent/portal', 'rds-agent/transport-noq']),
    'aarch64-apple-darwin': sorted(['rds-cli/desktop', 'rds-cli/transport-noq',
                                   'rds-agent/desktop', 'rds-agent/transport-noq']),
}
require = release.require
digest = release.digest


def inventory(directory, expected):
    require(directory.is_dir() and not directory.is_symlink(), 'invalid artifact directory')
    files = list(directory.iterdir())
    require({p.name for p in files} == expected, 'desktop artifact inventory mismatch')
    for path in files:
        release.regular(path)
        require(path.stat().st_size <= 300 * 1024 * 1024, 'oversized desktop artifact')


def checksum(directory, sums, name):
    require((directory / sums).stat().st_size <= 1024, 'oversized desktop checksum')
    require((directory / sums).read_text() == digest(directory / name) + '  ' + name + '\n',
            'desktop archive checksum mismatch')


def members(tar, expected):
    entries = tar.getmembers()
    require(len(entries) == len(expected) and {m.name for m in entries} == expected,
            'desktop archive member mismatch')
    require(all(m.isfile() and 0 < m.size <= 256 * 1024 * 1024 for m in entries),
            'unsafe desktop archive member')


def json_member(tar, name):
    require(tar.getmember(name).size <= 128 * 1024, 'oversized desktop metadata')
    return json.load(tar.extractfile(name))


def verify_runtime(directory, target, version, commit, channel, runtime_run):
    name = f'nddev-remote-{target}-{commit}.tar.gz'
    extra = ({f'nddev-remote-macos-apps-{commit}.tar.gz', 'apps-build-info.json', 'apps-SHA256SUMS'}
             if target == 'aarch64-apple-darwin' else set())
    inventory(directory, {name, 'build-info.json', 'SHA256SUMS'} | extra)
    checksum(directory, 'SHA256SUMS', name)
    require((directory / 'build-info.json').stat().st_size <= 128 * 1024, 'oversized desktop metadata')
    info = json.loads((directory / 'build-info.json').read_text())
    require(info.get('design_system') == design_system.verify(Path.cwd()),
            'desktop design-system pin differs from the selected source')
    for key, value in dict(schema_version=1, source=commit, version=version, target=target,
                           features=FEATURES[target]).items():
        require(info.get(key) == value, 'desktop build identity mismatch: ' + key)
    require(set(info.get('binaries', {})) == BINARIES, 'desktop binary inventory mismatch')
    notices = json.loads(Path('packaging/notices.json').read_text())
    require(info.get('notices') == {name: digest(Path(path)) for name, path in notices.items()},
            'desktop notice inventory mismatch')
    compiler = info.get('rustc', '')
    require(compiler.startswith('rustc ' + channel + ' ') and '\nhost: ' + target + '\n' in compiler,
            'desktop native compiler mismatch')
    build = info.get('build', {})
    for key, value in {'GITHUB_REPOSITORY': REPOSITORY, 'GITHUB_WORKFLOW': 'desktop-runtime',
                       'GITHUB_REF': 'refs/heads/main', 'GITHUB_RUN_ID': str(runtime_run)}.items():
        require(build.get(key) == value, 'desktop producer mismatch: ' + key)
    with tarfile.open(directory / name, 'r:gz') as tar:
        members(tar, BINARIES | set(notices) | {'build-info.json'})
        require(json_member(tar, 'build-info.json') == info, 'desktop metadata copies differ')
        for binary in BINARIES:
            require(tar.getmember(binary).mode == 0o755, 'desktop executable mode mismatch')
            with tar.extractfile(binary) as stream:
                require(hashlib.file_digest(stream, 'sha256').hexdigest() == info['binaries'][binary],
                        'desktop executable digest mismatch')
        for notice in notices:
            require(tar.getmember(notice).mode == 0o644, 'desktop notice mode mismatch')
            with tar.extractfile(notice) as stream:
                require(hashlib.file_digest(stream, 'sha256').hexdigest() == info['notices'][notice],
                        'desktop notice digest mismatch')
    return name, info


def verify_apps(directory, runtime, commit, version):
    name = f'nddev-remote-macos-apps-{commit}.tar.gz'
    checksum(directory, 'apps-SHA256SUMS', name)
    settings = json.loads(Path('packaging/macos.json').read_text())
    notices = json.loads(Path('packaging/notices.json').read_text())
    resources = {settings['icon']['name']: settings['icon']['source']}
    resources.update({'Notices/' + name: path for name, path in notices.items()})
    require((directory / 'apps-build-info.json').stat().st_size <= 128 * 1024, 'oversized macOS metadata')
    info = json.loads((directory / 'apps-build-info.json').read_text())
    for key, value in dict(schema_version=1, runtime=runtime, signing='ad-hoc', notarized=False,
                           builder_sha256=digest(Path('scripts/package-macos-apps.py')),
                           settings_sha256=digest(Path('packaging/macos.json'))).items():
        require(info.get(key) == value, 'macOS application identity mismatch: ' + key)
    expected, applications = set(), {}
    for app in settings['applications']:
        prefix = app['name'] + '.app/Contents/'
        expected |= {prefix + suffix for suffix in ('Info.plist', 'Resources/build-info.json',
                                                   '_CodeSignature/CodeResources', 'MacOS/' + app['executable'])}
        expected |= {prefix + 'Resources/' + name for name in resources}
        applications[app['name']] = app
    require(set(info.get('files', {})) == expected, 'macOS application file inventory mismatch')
    records = info.get('applications', [])
    require(len(records) == len(applications) and {r['name'] for r in records} == set(applications),
            'macOS application inventory mismatch')
    with tarfile.open(directory / name, 'r:gz') as tar:
        members(tar, expected | {'apps-build-info.json'})
        require(json_member(tar, 'apps-build-info.json') == info, 'macOS metadata copies differ')
        for filename, selected in info['files'].items():
            with tar.extractfile(filename) as stream:
                require(hashlib.file_digest(stream, 'sha256').hexdigest() == selected,
                        'macOS application file digest mismatch')
        for record in records:
            app = applications[record['name']]
            prefix = app['name'] + '.app/Contents/'
            require(record['identifier'] == app['identifier'], 'macOS bundle identity mismatch')
            require(record['executable_sha256'] == info['files'][prefix + 'MacOS/' + app['executable']],
                    'macOS signed executable receipt mismatch')
            require(tar.getmember(prefix + 'MacOS/' + app['executable']).mode == 0o755,
                    'macOS executable mode mismatch')
            require(json_member(tar, prefix + 'Resources/build-info.json') == runtime,
                    'macOS embedded runtime mismatch')
            for resource, source_path in resources.items():
                require(info['files'][prefix + 'Resources/' + resource] == digest(Path(source_path)),
                        'macOS source resource mismatch')
            require(tar.getmember(prefix + 'Info.plist').size <= 64 * 1024, 'oversized application plist')
            plist = plistlib.loads(tar.extractfile(prefix + 'Info.plist').read())
            for key, value in dict(CFBundleIdentifier=app['identifier'], CFBundleName=app['name'],
                                   CFBundleDisplayName=app['name'], NSHighResolutionCapable=True,
                                   CFBundleExecutable=app['executable'], CFBundleVersion=version,
                                   CFBundleIconFile=settings['icon']['name'],
                                   CFBundleShortVersionString=version, CFBundlePackageType='APPL',
                                   LSMinimumSystemVersion=settings['minimum_os'], LSUIElement=app['background']).items():
                require(plist.get(key) == value, 'macOS application plist mismatch: ' + key)
    return name


def finalize(version, incoming, source, out, runtime_run, tagged=False):
    commit, channel = release.contract(version, tagged=tagged)
    require(incoming.is_dir() and not incoming.is_symlink(), 'invalid desktop input directory')
    require({p.name for p in incoming.iterdir()} == {'desktop-' + t for t in FEATURES},
            'desktop target inventory mismatch')
    selected, runtimes = {}, {}
    for target in FEATURES:
        directory = incoming / ('desktop-' + target)
        name, info = verify_runtime(directory, target, version, commit, channel, runtime_run)
        selected[name] = directory / name
        runtimes[target] = info
    mac = incoming / 'desktop-aarch64-apple-darwin'
    name = verify_apps(mac, runtimes['aarch64-apple-darwin'], commit, version)
    selected[name] = mac / name
    source_name = f'nddev-remote-{version}-source.tar.gz'
    inventory(source, {source_name, 'source-sbom.spdx.json'})
    with tempfile.TemporaryDirectory(prefix='nddev-source-verify-') as temporary:
        expected = Path(temporary) / 'source.tar.gz'
        release.source_archive(version, expected)
        require(digest(source / source_name) == digest(expected), 'source archive mismatch')
    sbom = json.loads((source / 'source-sbom.spdx.json').read_text())
    require(str(sbom.get('spdxVersion', '')).startswith('SPDX-2.') and sbom.get('packages'),
            'empty or invalid source SBOM')
    selected[source_name] = source / source_name
    selected['source-sbom.spdx.json'] = source / 'source-sbom.spdx.json'
    release.new_output(out)
    for name, path in selected.items():
        with path.open('rb') as reader, (out / name).open('xb') as writer:
            shutil.copyfileobj(reader, writer)
    (out / 'release-notes.md').write_text(release.notes(version))
    assets = {p.name: {'sha256': digest(p), 'size': p.stat().st_size} for p in sorted(out.iterdir())}
    manifest = dict(schema_version=1, product='nddev-remote', version=version, source_commit=commit,
                    source_tag_object=release.run('git', 'rev-parse', 'refs/tags/' + version) if tagged else None,
                    prerelease=True, runtime_run=str(runtime_run), runtimes=runtimes,
                    macos_signing='ad-hoc', macos_notarized=False, assets=assets)
    (out / 'release-manifest.json').write_text(json.dumps(manifest, indent=2, sort_keys=True) + '\n')
    (out / 'SHA256SUMS').write_text(''.join(digest(p) + '  ' + p.name + '\n' for p in sorted(out.iterdir())))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--version', required=True)
    parser.add_argument('--incoming', type=Path, required=True)
    parser.add_argument('--source', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--runtime-run', required=True)
    parser.add_argument('--tagged', action='store_true')
    args = parser.parse_args()
    finalize(args.version, args.incoming, args.source, args.out, args.runtime_run, args.tagged)
