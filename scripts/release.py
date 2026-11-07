#!/usr/bin/env python3
"""Build-time release assembly only; installed RDS programs remain native Rust.

No network access or publication here. GitHub Actions supplies bounded build
artifacts; this script refuses version/source/target drift before publication.
"""
import argparse
import gzip
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tarfile
import tomllib

SOURCE_PATHS = ("README.md", "LICENSE", "VERSION", "CHANGELOG.md", "AGENTS.md",
                "deny.toml", "rust-toolchain.toml", "Cargo.toml", "Cargo.lock",
                "crates", "tests", "examples", "deploy", "docs", "scripts", ".github", "ops", "packaging")


def run(*args):
    return subprocess.check_output(args, text=True, timeout=60).strip()


def require(ok, reason):
    if not ok:
        raise ValueError(reason)


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def contract(version, tagged=False):
    require(re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", version),
            "expected numeric SemVer")
    require(Path("VERSION").read_bytes() == (version + "\n").encode(), "VERSION mismatch")
    manifest = tomllib.loads(Path("Cargo.toml").read_text())
    require(manifest["workspace"]["package"]["version"] == version, "Cargo version mismatch")
    lock = tomllib.loads(Path("Cargo.lock").read_text())
    for member in manifest["workspace"]["members"]:
        name = tomllib.loads((Path(member) / "Cargo.toml").read_text())["package"]["name"]
        matches = [p for p in lock["package"] if p["name"] == name and "source" not in p]
        require(len(matches) == 1 and matches[0]["version"] == version, f"lock version mismatch: {name}")
    channel = tomllib.loads(Path("rust-toolchain.toml").read_text())["toolchain"]["channel"]
    require(re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", channel), "Rust toolchain must be pinned")
    notes(version)
    commit = run("git", "rev-parse", "HEAD")
    require(re.fullmatch(r"[0-9a-f]{40}", commit), "invalid source OID")
    require(not run("git", "status", "--porcelain", "--untracked-files=no"), "dirty tracked source")
    if tagged:
        require(os.environ.get("GITHUB_REF") == f"refs/tags/{version}", "release must run from tag ref")
        require(run("git", "cat-file", "-t", f"refs/tags/{version}") == "tag", "annotated tag required")
        require(run("git", "rev-parse", f"refs/tags/{version}^{{commit}}") == commit, "tag/source mismatch")
    return commit, channel


def notes(version):
    content = Path("CHANGELOG.md").read_text()
    headings = list(re.finditer(r"^## \[([^\]]+)\].*$", content, re.M))
    selected = [i for i, h in enumerate(headings) if h.group(1) == version]
    require(len(selected) == 1, "expected exactly one release heading")
    i = selected[0]
    end = headings[i + 1].start() if i + 1 < len(headings) else len(content)
    result = content[headings[i].end():end].strip()
    require(result and "engineering preview" in result.lower(), "preview release notes required")
    return result + "\n"


def verify_ci_evidence(commit, runs, analyses, alerts):
    """A successful scanner invocation does not mean its findings are resolved."""
    require(all(isinstance(items, list) for items in (runs, analyses, alerts)), "invalid CI evidence")
    for workflow in ("ci.yml", "supply-chain.yml", "security.yml", "desktop-runtime.yml"):
        matches = [r for r in runs if r["path"] == ".github/workflows/" + workflow
                   and r["head_sha"] == commit and r["event"] == "push" and r["head_branch"] == "main"]
        require(len(matches) == 1 and matches[0]["status"] == "completed"
                and matches[0]["conclusion"] == "success",
                "missing/ambiguous/unsuccessful exact-commit workflow: " + workflow)
    for language in ("rust", "actions"):
        matches = [a for a in analyses if a["ref"] == "refs/heads/main"
                   and a["category"] == "/language:" + language
                   and a["analysis_key"] == ".github/workflows/security.yml:codeql"
                   and a["tool"]["name"] == "CodeQL"]
        # GitHub returns newest first. Alerts on main must describe this
        # source, not a later revision that already fixed its vulnerabilities.
        require(sum(a["commit_sha"] == commit for a in matches) == 1
                and matches[0]["commit_sha"] == commit
                and matches[0]["error"] == "" and matches[0]["warning"] == "",
                "missing/ambiguous/incomplete exact-commit CodeQL analysis: " + language)
    require(not alerts, "unresolved code-scanning alerts on main")


def new_output(path):
    path.mkdir(parents=True, exist_ok=False)
    return path


def regular(path):
    require(path.is_file() and not path.is_symlink(), f"expected regular file: {path.name}")


def source_archive(version, path):
    # git archive includes only the chosen tracked paths, including observability.
    listing = subprocess.check_output(["git", "ls-files", "--stage", "--", *SOURCE_PATHS], text=True)
    for line in listing.splitlines():
        require(line.split()[0] in ("100644", "100755"), "source contains a link or gitlink")
    with path.open("xb") as dest, gzip.GzipFile(fileobj=dest, filename="", mode="wb", mtime=0) as gz:
        proc = subprocess.Popen(["git", "archive", "--format=tar", f"--prefix=nddev-remote-{version}/",
                                 "HEAD", "--", *SOURCE_PATHS], stdout=subprocess.PIPE)
        try:
            while data := proc.stdout.read(1024 * 1024):
                gz.write(data)
            require(proc.wait(timeout=60) == 0, "git archive failed")
        finally:
            proc.stdout.close()
            if proc.poll() is None:
                proc.kill()
                proc.wait()


def source_bundle(version, out, tagged=False):
    contract(version, tagged=tagged)
    new_output(out)
    path = out / f"nddev-remote-{version}-source.tar.gz"
    source_archive(version, path)
    # Extract only our generated regular-file archive for the source SBOM scan.
    with tarfile.open(path, "r:gz") as tar:
        tar.extractall(out / "source", filter="data")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("check", "evidence", "source"))
    parser.add_argument("--version", required=True)
    parser.add_argument("--tagged", action="store_true")
    parser.add_argument("--out", type=Path)
    parser.add_argument("--checks", type=Path)
    parser.add_argument("--analyses", type=Path)
    parser.add_argument("--alerts", type=Path)
    args = parser.parse_args()
    if args.command == "check":
        print(json.dumps(dict(zip(("commit", "toolchain"), contract(args.version, args.tagged)))))
    elif args.command == "evidence":
        require(args.checks and args.analyses and args.alerts, "all three CI evidence files required")
        commit, _ = contract(args.version, args.tagged)
        # gh --paginate --slurp preserves every page, including late alerts.
        runs = [r for page in json.loads(args.checks.read_text()) for r in page["workflow_runs"]]
        analyses = [a for page in json.loads(args.analyses.read_text()) for a in page]
        alerts = [a for page in json.loads(args.alerts.read_text()) for a in page]
        verify_ci_evidence(commit, runs, analyses, alerts)
        print(json.dumps({"commit": commit, "ci_evidence": "verified"}))
    else:
        require(args.out is not None, "--out required")
        source_bundle(args.version, args.out, args.tagged)


if __name__ == "__main__":
    main()
