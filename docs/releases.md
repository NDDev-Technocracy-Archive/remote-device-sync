# Engineering preview releases

NDDev Remote `0.1.1` packages the standalone remote-screen product. The native
client is `nddev-remote`, with `rds` for automation and `rds-agent` for the resident
endpoint. NDS accounts, synchronization, telemetry and build caches are independent
of screen connections. Current acceptance is recorded separately from build
support in the [capability matrix](capability-matrix.md).

## Packages

A release contains eight assets. `<source>` is the exact 40-character source
commit in `release-manifest.json`.

| Asset | Contents |
| --- | --- |
| `nddev-remote-x86_64-unknown-linux-gnu-<source>.tar.gz` | Three desktop-enabled executables and `build-info.json`; native Ubuntu 24.04 x86_64 build, X11 and consented Wayland host |
| `nddev-remote-aarch64-apple-darwin-<source>.tar.gz` | The same three executables and metadata; native macOS arm64 build, ScreenCaptureKit/CGEvent host |
| `nddev-remote-macos-apps-<source>.tar.gz` | `NDDev Remote.app`, `NDDev Remote Host.app` and signed-file receipts |
| `nddev-remote-0.1.1-source.tar.gz` | Tracked source, lockfile, packaging, tests, documentation, deployment and observability configuration |
| `source-sbom.spdx.json` | SPDX inventory of the source archive, not an OS or complete binary SBOM |
| `release-notes.md` | Compatibility and readiness notes |
| `release-manifest.json` | Exact source/tag object, native producer, binary hashes and asset inventory |
| `SHA256SUMS` | Digests of the other seven assets |

The Linux and macOS executable archives enable the native viewer and both
transport implementations. Linux adds the portal agent; macOS adds the native
host. The repository pins Rust and dependencies. Python, Syft and GitHub CLI are
packaging/verification tools, not installed runtime requirements.

Public Mac applications use ad hoc signatures. They are not Developer ID signed
or notarized installers. The bundle's minimum OS declaration is macOS 13;
older-OS acceptance requires native testing. Linux uses the build host's GNU
libc; older distributions, Windows and other architectures are unqualified.
Deterministic packaging does not claim bit-for-bit Rust rebuilds across hosts.

## Verify and install

Download into a new empty directory. GitHub CLI is needed for verification only.

```sh
version=0.1.1
repo=NDDev-Technocracy-OpenNetwork/nddev-remote
target=x86_64-unknown-linux-gnu
# Apple Silicon: target=aarch64-apple-darwin
gh release download "$version" --repo "$repo"
gh attestation verify SHA256SUMS --repo "$repo" \
  --signer-workflow "$repo/.github/workflows/release.yml" \
  --source-ref "refs/tags/$version" --deny-self-hosted-runners
sha256sum -c SHA256SUMS
# macOS: shasum -a 256 -c SHA256SUMS
source=$(python3 -c 'import json; print(json.load(open("release-manifest.json"))["source_commit"])')
archive="nddev-remote-$target-$source.tar.gz"
gh attestation verify "$archive" --repo "$repo" \
  --signer-workflow "$repo/.github/workflows/release.yml" \
  --source-ref "refs/tags/$version" --source-digest "$source" --deny-self-hosted-runners
tar -xzf "$archive"
./rds --version
./nddev-remote --help
```

Verify the Mac application archive the same way before extracting it. Move both
applications to the chosen Applications directory. Run the host's explicit local
setup action and grant Screen Recording and Accessibility in System Settings:

```sh
"NDDev Remote Host.app/Contents/MacOS/rds-agent" --request-desktop-permission
"NDDev Remote Host.app/Contents/MacOS/rds-agent" --desktop-permission-status
```

Consent belongs to the installed application and device. A changed ad hoc code
signature may require new local consent; a package never copies or bypasses a
TCC grant. The manager remains available while the native owner waits for grants.
On Wayland, use the [portal's local consent flow](wayland-portal.md). X11 uses its
native session authorization. An SSH login alone grants neither screen access.

Upgrade the CLI, viewer and agent together. Keep each endpoint's keys, authorized
peers and native consent under its existing owner. A deployment pins the archive
digest and source in its own repository before installing; public packages
contain no private endpoint/profile facts. A running service does not change
merely because a release is published. Device-specific rollback must retain its
accepted predecessor without resetting identity, keyrings or the desktop session.

## Publication contract

`desktop-runtime.yml` builds and tests the three desktop executables natively on
Linux and macOS. Its successful `main` push attests the exact executable and Mac
application archives. PR artifacts remain test inputs, without main acceptance
provenance. `release.yml` checks the promotion contract and generates the exact
source archive/SBOM on PRs and `main`, with no publication authority.

An annotated signed numeric tag may publish only when all of these hold:

1. The tag matches `VERSION`, workspace/lockfile versions, changelog and clean
   checked-out source already on `main`.
2. That exact main source has successful CI, security, supply-chain and desktop
   runtime workflows, complete Rust/Actions CodeQL analyses and no open
   code-scanning findings. All API pages are checked.
3. Native packages come from that one successful main desktop run. Their main
   attestations, source/toolchain/features/producer, exact archive members,
   executable and signed-app hashes, bundle identities and embedded receipts
   verify. Source reconstruction and a nonempty SPDX inventory verify too.
4. Publication promotes the exact checked package bytes without recompiling or
   signing again. The release environment grants publication rights only to the
   tag path. Authentication/network errors cannot mean an absent release.
5. All eight assets are created together as an engineering prerelease with
   `latest=false`. Published assets are never overwritten. Downloaded inventory,
   checksums and attestations are verified after publication.

Failures are fixed forward before tagging; tags never move. Inspect existing
release state before retrying a failed upload. Source SPDX provenance applies
only to the source archive; no SLSA level or Apple distribution-signature claim
is made. The assembly helper has no network or publication authority.

## Native acceptance and history

A green build, permission bit, Info response or input acknowledgement does not
prove interactive desktop behavior. Qualification uses actual native pixels,
input into an owned application, measured response and recovery after network
and agent changes. Synthetic capture and Xvfb presentation retain their narrower
scope. No result proves physical input-to-photon latency without that measurement.
See [product acceptance](product.md), [platforms](platforms.md),
[desktop packages](desktop-runtime.md) and [local sessions](local-sessions.md).

The original `0.1.0` release remains unchanged in GitHub history. Its four-program
SSH/relay/directory packages excluded desktop features and used historical
`remote-device-sync-*` asset names. Those assets and their original provenance
are not relabelled as the new desktop product or used as its installer.
