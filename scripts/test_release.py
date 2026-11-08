#!/usr/bin/env python3
"""Negative release-contract tests: all repositories/artifacts are synthetic."""
import contextlib
import copy
import importlib.util
import io
import json
import os
import shutil
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True

spec = importlib.util.spec_from_file_location("rds_release", Path(__file__).with_name("release.py"))
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)
VERSION = "0.1.0"


class ReleaseContract(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="rds-release-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.repo = self.root / "repo"
        self.repo.mkdir()
        self.context = contextlib.chdir(self.repo)
        self.context.__enter__()
        self.addCleanup(self.context.__exit__, None, None, None)
        roots = ('README.md', 'LICENSE', 'VERSION', 'CHANGELOG.md', 'AGENTS.md',
                 'deny.toml', 'rust-toolchain.toml', 'Cargo.toml', 'Cargo.lock',
                 'crates', 'tests', 'examples', 'deploy', 'docs', 'scripts', '.github',
                 'ops', 'packaging', 'vendor')
        for name in roots:
            path = Path(name)
            if path.suffix or name in ("LICENSE", "VERSION"):
                path.write_text("fixture\n")
            else:
                path.mkdir()
                (path / "fixture").write_text("fixture\n")
        settings = Path(__file__).resolve().parent.parent / "packaging/macos.json"
        Path("packaging/macos.json").write_bytes(settings.read_bytes())
        resources = settings.parent / 'notices.json'
        Path('packaging/notices.json').write_bytes(resources.read_bytes())
        design = settings.parent / 'design-system.json'
        Path('packaging/design-system.json').write_bytes(design.read_bytes())
        shutil.copytree(settings.parent.parent / 'vendor/nddev-opennetwork-design-tokens',
                        Path('vendor/nddev-opennetwork-design-tokens'))
        assets = settings.parent.parent / 'crates/rds-desktop/assets'
        shutil.copytree(assets / 'font-licenses', Path('crates/rds-desktop/assets/font-licenses'))
        shutil.copyfile(assets / 'app-icon.icns', Path('crates/rds-desktop/assets/app-icon.icns'))
        Path("scripts/package-macos-apps.py").write_text("synthetic checked native app builder\n")
        Path("VERSION").write_text(VERSION + "\n")
        Path("Cargo.toml").write_text('[workspace]\nmembers=["crates/example"]\n[workspace.package]\nversion="0.1.0"\n')
        Path("crates/example").mkdir()
        Path("crates/example/Cargo.toml").write_text('[package]\nname="example"\nversion="0.1.0"\n')
        Path("Cargo.lock").write_text('version=4\n[[package]]\nname="example"\nversion="0.1.0"\n')
        Path("CHANGELOG.md").write_text('## [0.1.0]\nEngineering preview.\n')
        Path("rust-toolchain.toml").write_text('[toolchain]\nchannel="1.98.1"\n')
        self.git("init", "-q")
        self.git("config", "user.name", "Release Fixture")
        self.git("config", "user.email", "fixture@example.invalid")
        self.commit()
        self.git("-c", "tag.gpgSign=false", "tag", "-a", VERSION, "-m", "fixture")
        self.environment = patch.dict(os.environ, {"GITHUB_REF": "refs/tags/" + VERSION})
        self.environment.start()
        self.addCleanup(self.environment.stop)
        self.commit_oid, self.channel = release.contract(VERSION, tagged=True)

    def git(self, *args):
        return subprocess.check_output(["git", *args], stderr=subprocess.PIPE, text=True).strip()

    def commit(self):
        self.git("add", ".")
        self.git("-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null", "commit", "-qm", "fixture")

    def test_exact_version_and_lock_are_required(self):
        for name, content, error in (
            ("VERSION", "0.1.0", "VERSION mismatch"),
            ("Cargo.lock", '[[package]]\nname="example"\nversion="9.9.9"\n', "lock version mismatch"),
            ("rust-toolchain.toml", '[toolchain]\nchannel="stable"\n', "must be pinned"),
        ):
            old = Path(name).read_bytes()
            Path(name).write_text(content)
            with self.assertRaisesRegex(ValueError, error):
                release.contract(VERSION)
            Path(name).write_bytes(old)

    def test_dirty_source_and_wrong_tag_ref_are_refused(self):
        Path("LICENSE").write_text("changed")
        with self.assertRaisesRegex(ValueError, "dirty tracked"):
            release.contract(VERSION)
        self.git("checkout", "--", "LICENSE")
        with patch.dict(os.environ, {"GITHUB_REF": "refs/heads/main"}):
            with self.assertRaisesRegex(ValueError, "tag ref"):
                release.contract(VERSION, tagged=True)

    def test_lightweight_or_moved_tag_is_refused(self):
        self.git("tag", "-d", VERSION)
        self.git("-c", "tag.gpgSign=false", "tag", VERSION)
        with self.assertRaisesRegex(ValueError, "annotated"):
            release.contract(VERSION, tagged=True)
        self.git("tag", "-d", VERSION)
        self.git("-c", "tag.gpgSign=false", "tag", "-a", VERSION, "-m", "fixture")
        Path("README.md").write_text("successor")
        self.commit()
        with self.assertRaisesRegex(ValueError, "tag/source mismatch"):
            release.contract(VERSION, tagged=True)

    def test_source_includes_ops_and_shared_tests_but_no_untracked_files(self):
        Path("ops/untracked-secret").write_text("not a real secret")
        out = self.root / "source"
        release.source_bundle(VERSION, out, tagged=True)
        with tarfile.open(next(out.glob("*.tar.gz"))) as archive:
            names = archive.getnames()
        self.assertIn(f"nddev-remote-{VERSION}/ops/fixture", names)
        self.assertIn(f"nddev-remote-{VERSION}/tests/fixture", names)
        self.assertIn(f"nddev-remote-{VERSION}/examples/fixture", names)
        self.assertFalse(any("untracked-secret" in n for n in names))

    def test_source_symlink_refused(self):
        Path("ops/link").symlink_to("fixture")
        self.commit()
        with self.assertRaisesRegex(ValueError, "link or gitlink"):
            release.source_archive(VERSION, self.root / "source.tar.gz")

    def test_source_includes_new_tracked_build_roots_without_an_allowlist(self):
        Path('native-build').mkdir()
        Path('native-build/required.h').write_text('synthetic native dependency\n')
        self.commit()
        archive = self.root/'complete-source.tar.gz'
        release.source_archive(VERSION, archive)
        with tarfile.open(archive) as bundle:
            self.assertEqual(bundle.extractfile(f'nddev-remote-{VERSION}/native-build/required.h').read(),
                             b'synthetic native dependency\n')
            self.assertIn(f'nddev-remote-{VERSION}/vendor/fixture', bundle.getnames())


class CIEvidence(unittest.TestCase):
    def setUp(self):
        self.commit = "a" * 40
        self.runs = [dict(path=".github/workflows/" + name, head_sha=self.commit,
                          event="push", head_branch="main", status="completed", conclusion="success")
                     for name in ("ci.yml", "supply-chain.yml", "security.yml", "desktop-runtime.yml")]
        self.analyses = [dict(ref="refs/heads/main", category="/language:" + language,
                             analysis_key=".github/workflows/security.yml:codeql", tool={"name": "CodeQL"},
                             commit_sha=self.commit, error="", warning="")
                        for language in ("rust", "actions")]

    def test_green_workflows_with_open_alerts_are_refused(self):
        release.verify_ci_evidence(self.commit, self.runs, self.analyses, [])
        with self.assertRaisesRegex(ValueError, "unresolved code-scanning"):
            release.verify_ci_evidence(self.commit, self.runs, self.analyses, [{"number": 1}])

    def test_exact_successful_unambiguous_main_workflows_required(self):
        for field, value in (("head_sha", "b" * 40), ("event", "pull_request"),
                             ("head_branch", "topic"), ("status", "in_progress"),
                             ("conclusion", "failure")):
            runs = copy.deepcopy(self.runs)
            runs[0][field] = value
            with self.subTest(field=field), self.assertRaisesRegex(ValueError, "exact-commit workflow"):
                release.verify_ci_evidence(self.commit, runs, self.analyses, [])
        for runs in (self.runs[:-1], self.runs + [self.runs[0]]):
            with self.assertRaisesRegex(ValueError, "exact-commit workflow"):
                release.verify_ci_evidence(self.commit, runs, self.analyses, [])

    def test_missing_foreign_incomplete_and_duplicate_scans_are_refused(self):
        for field, value in (("commit_sha", "b" * 40), ("ref", "refs/pull/1/merge"),
                             ("category", "/language:other"), ("analysis_key", "foreign"),
                             ("tool", {"name": "other"}), ("error", "incomplete extraction"),
                             ("warning", "partial results")):
            analyses = copy.deepcopy(self.analyses)
            analyses[0][field] = value
            with self.subTest(field=field), self.assertRaisesRegex(ValueError, "CodeQL analysis"):
                release.verify_ci_evidence(self.commit, self.runs, analyses, [])
        for analyses in (self.analyses[:-1], self.analyses + [self.analyses[0]]):
            with self.assertRaisesRegex(ValueError, "CodeQL analysis"):
                release.verify_ci_evidence(self.commit, self.runs, analyses, [])

    def test_later_main_scan_cannot_supply_an_older_releases_alert_state(self):
        other = dict(self.analyses[0], commit_sha="b" * 40)
        release.verify_ci_evidence(self.commit, self.runs, self.analyses + [other], [])
        with self.assertRaisesRegex(ValueError, "CodeQL analysis"):
            release.verify_ci_evidence(self.commit, self.runs, [other] + self.analyses, [])

    def test_evidence_command_checks_alerts_on_later_pages(self):
        with tempfile.TemporaryDirectory(prefix="rds-ci-evidence-") as directory:
            paths = [Path(directory) / name for name in ("checks", "analyses", "alerts")]
            paths[0].write_text(json.dumps([{"workflow_runs": self.runs[:1]},
                                           {"workflow_runs": self.runs[1:]}]))
            paths[1].write_text(json.dumps([self.analyses[:1], self.analyses[1:]]))
            paths[2].write_text("[[], []]")
            argv = ["release.py", "evidence", "--version", VERSION,
                    "--checks", str(paths[0]), "--analyses", str(paths[1]), "--alerts", str(paths[2])]
            with patch.object(sys, "argv", argv), \
                    patch.object(release, "contract", return_value=(self.commit, "1.98.1")), \
                    contextlib.redirect_stdout(io.StringIO()):
                release.main()
                paths[2].write_text('[[], [{"number": 101}]]')
                with self.assertRaisesRegex(ValueError, "unresolved code-scanning"):
                    release.main()


if __name__ == "__main__":
    unittest.main()
