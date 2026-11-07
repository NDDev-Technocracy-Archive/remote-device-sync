#!/usr/bin/env python3
"""Promotion accepts exact native artifacts and rejects substituted producers."""
import copy
import hashlib
import io
import json
from pathlib import Path
import plistlib
import tarfile
import unittest

import desktop_release as desktop
import test_release as fixture


def encoded(value):
    return (json.dumps(value, sort_keys=True) + '\n').encode()


class DesktopPromotion(unittest.TestCase):
    def setUp(self):
        base = fixture.ReleaseContract(methodName='runTest')
        base.setUp()
        self.addCleanup(base.doCleanups)
        self.root, self.commit = base.root, base.commit_oid
        self.incoming, self.source = self.root/'incoming', self.root/'source'
        self.incoming.mkdir()
        self.runtime_run = '42'
        notices = json.loads(Path('packaging/notices.json').read_text())
        self.runtimes = {}
        self.payloads = {}
        for target in desktop.FEATURES:
            directory = self.incoming/('desktop-'+target)
            directory.mkdir()
            binaries = {name: ('synthetic '+target+' '+name).encode() for name in desktop.BINARIES}
            info = dict(schema_version=1, source=self.commit, version=fixture.VERSION, target=target,
                        features=desktop.FEATURES[target],
                        rustc='rustc 1.98.1 (fixture)\nhost: '+target+'\nrelease: 1.98.1',
                        binaries={name: hashlib.sha256(value).hexdigest() for name,value in binaries.items()},
                        notices={name:desktop.digest(Path(path)) for name,path in notices.items()},
                        build=dict(GITHUB_REPOSITORY=desktop.REPOSITORY, GITHUB_WORKFLOW='desktop-runtime',
                                   GITHUB_REF='refs/heads/main', GITHUB_RUN_ID=self.runtime_run))
            self.runtimes[target] = info
            payload = {name:(value,0o755) for name,value in binaries.items()}
            payload.update({name:(Path(path).read_bytes(),0o644) for name,path in notices.items()})
            payload['build-info.json'] = (encoded(info),0o644)
            self.payloads[target] = payload
            self.write_runtime(target)
        self.mac = self.incoming/'desktop-aarch64-apple-darwin'
        runtime = self.runtimes['aarch64-apple-darwin']
        settings = json.loads(Path('packaging/macos.json').read_text())
        self.apps, records = {}, []
        for app in settings['applications']:
            prefix = app['name']+'.app/Contents/'
            plist = dict(CFBundleIdentifier=app['identifier'], CFBundleName=app['name'],
                         CFBundleDisplayName=app['name'], NSHighResolutionCapable=True,
                         CFBundleIconFile=settings['icon']['name'],
                         CFBundleExecutable=app['executable'], CFBundleVersion=fixture.VERSION,
                         CFBundleShortVersionString=fixture.VERSION, CFBundlePackageType='APPL',
                         LSMinimumSystemVersion=settings['minimum_os'], LSUIElement=app['background'])
            binary = ('synthetic signed '+app['executable']).encode()
            self.apps[prefix+'Info.plist'] = (plistlib.dumps(plist),0o644)
            self.apps[prefix+'Resources/build-info.json'] = (encoded(runtime),0o644)
            self.apps[prefix+'_CodeSignature/CodeResources'] = (b'synthetic signature resources',0o644)
            self.apps[prefix+'MacOS/'+app['executable']] = (binary,0o755)
            resources = {settings['icon']['name']:settings['icon']['source']}
            resources.update({'Notices/'+name:path for name,path in notices.items()})
            self.apps.update({prefix+'Resources/'+name:(Path(path).read_bytes(),0o644)
                              for name,path in resources.items()})
            records.append(dict(name=app['name'],identifier=app['identifier'],
                                executable_sha256=hashlib.sha256(binary).hexdigest()))
        self.apps_info = dict(schema_version=1, runtime=runtime, signing='ad-hoc', notarized=False,
                              builder_sha256=desktop.digest(Path('scripts/package-macos-apps.py')),
                              settings_sha256=desktop.digest(Path('packaging/macos.json')),
                              applications=records)
        self.write_apps()
        fixture.release.source_bundle(fixture.VERSION,self.source,tagged=True)
        # The source scan directory is not an artifact.
        import shutil
        shutil.rmtree(self.source/'source')
        (self.source/'source-sbom.spdx.json').write_bytes(encoded(dict(spdxVersion='SPDX-2.3',packages=[{'name':'fixture'}])))

    def archive(self, directory, name, payload, metadata, sums):
        path = directory/name
        with tarfile.open(path,'w:gz') as bundle:
            for filename,(data,mode) in sorted(payload.items()):
                member = tarfile.TarInfo(filename)
                member.size, member.mode = len(data), mode
                bundle.addfile(member,io.BytesIO(data))
        (directory/sums).write_text(desktop.digest(path)+'  '+name+'\n')
        (directory/metadata[0]).write_bytes(encoded(metadata[1]))

    def write_runtime(self,target):
        info = self.runtimes[target]
        self.payloads[target]['build-info.json'] = (encoded(info),0o644)
        self.archive(self.incoming/('desktop-'+target),
                     f'nddev-remote-{target}-{self.commit}.tar.gz',self.payloads[target],
                     ('build-info.json',info),'SHA256SUMS')

    def write_apps(self):
        self.apps_info['files'] = {name:hashlib.sha256(value[0]).hexdigest() for name,value in self.apps.items()}
        payload = dict(self.apps)
        payload['apps-build-info.json'] = (encoded(self.apps_info),0o644)
        self.archive(self.mac,f'nddev-remote-macos-apps-{self.commit}.tar.gz',payload,
                     ('apps-build-info.json',self.apps_info),'apps-SHA256SUMS')

    def promote(self):
        out = self.root/'dist'
        desktop.finalize(fixture.VERSION,self.incoming,self.source,out,self.runtime_run,tagged=True)
        return out

    def test_promoted_archives_are_byte_identical_with_complete_inventory(self):
        originals = {p.name:p.read_bytes() for p in self.incoming.glob('*/*.tar.gz')}
        out = self.promote()
        self.assertEqual(len(list(out.iterdir())),8)
        for name,value in originals.items():
            self.assertEqual((out/name).read_bytes(),value)
        info = json.loads((out/'release-manifest.json').read_text())
        self.assertEqual(info['source_commit'],self.commit)
        self.assertEqual(info['runtime_run'],'42')
        self.assertFalse(info['macos_notarized'])
        self.assertEqual(set(info['assets']),set(p.name for p in out.iterdir())-{'release-manifest.json','SHA256SUMS'})
        self.assertEqual(len((out/'SHA256SUMS').read_text().splitlines()),7)

    def test_foreign_producer_ref_run_and_native_build_identity_are_refused(self):
        target = 'x86_64-unknown-linux-gnu'
        original = copy.deepcopy(self.runtimes[target])
        for key,value in [('GITHUB_REPOSITORY','other/project'),('GITHUB_WORKFLOW','release'),
                          ('GITHUB_REF','refs/pull/2/merge'),('GITHUB_RUN_ID','43')]:
            self.runtimes[target] = copy.deepcopy(original)
            self.runtimes[target]['build'][key] = value
            self.write_runtime(target)
            with self.subTest(key=key),self.assertRaisesRegex(ValueError,'desktop producer mismatch'):
                self.promote()
        for key,value in [('source','a'*40),('features',[]),('version','9.0.0'),
                          ('rustc','rustc 1.98.1 (cross)\nhost: aarch64-apple-darwin\n')]:
            self.runtimes[target] = copy.deepcopy(original)
            self.runtimes[target][key] = value
            self.write_runtime(target)
            with self.subTest(key=key),self.assertRaisesRegex(ValueError,'desktop (build identity|native compiler) mismatch'):
                self.promote()

    def test_missing_extra_and_linked_artifacts_are_refused(self):
        path = self.mac/'build-info.json'
        value = path.read_bytes()
        path.unlink()
        with self.assertRaisesRegex(ValueError,'inventory mismatch'): self.promote()
        path.write_bytes(value)
        extra = self.mac/'extra'
        extra.write_text('unexpected')
        with self.assertRaisesRegex(ValueError,'inventory mismatch'): self.promote()
        extra.unlink()
        saved = self.root/'saved-info'
        path.rename(saved); path.symlink_to(saved)
        with self.assertRaisesRegex(ValueError,'regular file'): self.promote()

    def test_tampered_binary_and_extra_archive_member_are_refused(self):
        target = 'x86_64-unknown-linux-gnu'
        self.payloads[target]['rds'] = (b'substituted executable',0o755)
        self.write_runtime(target)
        with self.assertRaisesRegex(ValueError,'executable digest mismatch'): self.promote()
        self.payloads[target]['extra'] = (b'extra',0o644)
        self.write_runtime(target)
        with self.assertRaisesRegex(ValueError,'archive member mismatch'): self.promote()

    def test_native_notices_and_mac_resource_bytes_are_bound_to_source(self):
        target = 'x86_64-unknown-linux-gnu'
        self.payloads[target]['LICENSE'] = (b'substituted notice',0o644)
        self.write_runtime(target)
        with self.assertRaisesRegex(ValueError,'notice digest mismatch'): self.promote()
        self.payloads[target]['LICENSE'] = (Path('LICENSE').read_bytes(),0o644)
        self.write_runtime(target)
        filename = 'NDDev Remote.app/Contents/Resources/NDDevRemote.icns'
        self.apps[filename] = (b'substituted icon',0o644)
        self.write_apps()
        with self.assertRaisesRegex(ValueError,'source resource mismatch'): self.promote()

    def test_mac_bundle_identity_embedded_source_and_builder_are_bound(self):
        original = copy.deepcopy(self.apps)
        for key,value in [('CFBundleIdentifier','other.app'),('CFBundleVersion','9.0.0'),('LSUIElement',True)]:
            self.apps = copy.deepcopy(original)
            name = 'NDDev Remote.app/Contents/Info.plist'
            plist = plistlib.loads(self.apps[name][0]); plist[key] = value
            self.apps[name] = (plistlib.dumps(plist),0o644)
            self.write_apps()
            with self.subTest(key=key),self.assertRaisesRegex(ValueError,'application plist mismatch'):
                self.promote()
        self.apps = copy.deepcopy(original)
        name = 'NDDev Remote.app/Contents/Resources/build-info.json'
        self.apps[name] = (encoded({'source':'foreign'}),0o644)
        self.write_apps()
        with self.assertRaisesRegex(ValueError,'embedded runtime mismatch'): self.promote()
        self.apps = original; self.apps_info['builder_sha256'] = 'a'*64; self.write_apps()
        with self.assertRaisesRegex(ValueError,'identity mismatch: builder'): self.promote()

    def test_tampered_source_or_empty_sbom_are_refused(self):
        path = self.source/f'nddev-remote-{fixture.VERSION}-source.tar.gz'
        original = path.read_bytes(); path.write_bytes(b'substituted source')
        with self.assertRaisesRegex(ValueError,'source archive mismatch'): self.promote()
        path.write_bytes(original)
        (self.source/'source-sbom.spdx.json').write_text('{"spdxVersion":"SPDX-2.3","packages":[]}')
        with self.assertRaisesRegex(ValueError,'source SBOM'): self.promote()


if __name__ == '__main__':
    unittest.main()
