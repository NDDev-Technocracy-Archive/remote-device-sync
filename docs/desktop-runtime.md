# Desktop runtime packages

The `desktop-runtime` workflow builds Linux x64 and macOS arm64 from one source
commit, the repository's Rust toolchain and the committed dependency lock.
Both include the native viewer, agent and the existing optional transport.
The Linux agent also includes the consented Wayland portal backend.

These packages are separate from the numeric engineering-preview release assets,
whose original feature and publication contracts stay in `release.yml`.
The workflow emits `desktop-<target>` artifacts with an archive, `build-info.json`
and `SHA256SUMS`. The archive contains exactly `rds`, `rds-agent`, `nddev-remote`
and the same build metadata. Metadata records the source commit, target, features,
compiler identity, binary hashes and the native GitHub build identity.
Successful `main` push builds attest the archive in a separate job that downloads
verified artifacts and never executes build inputs. Native build jobs have only
read access. The attestation uses GitHub's build provenance;
pull-request artifacts have no acceptance attestation.

Verify the downloaded artifact digest and its archive checksum, then verify the
archive's attestation against this repository, the `desktop-runtime.yml` workflow
and its exact source commit. Device owners retain accepted archives in their
own immutable runtime inventory and pin the digest before installation. Actions
artifact retention alone is not a permanent deployment store.

An entrypoint smoke check and desktop-enabled build do not enroll an endpoint,
restore screen permissions or qualify actual media/input. Native host/client
acceptance is required before replacing an active device's runtime. macOS viewing
is distinct from macOS capture/input: the prepared host backend requires its
own native permission and screen/input qualification. No key, consent state or private device configuration belongs in
these generic packages.

The macOS artifact also includes a separate application archive and its own
checksum/receipt. It contains `NDDev Remote.app` (viewer) and `NDDev Remote Host.app`
(agent), with stable bundle identities from `packaging/macos.json`. The bundle
builder verifies the input binary hashes, rejects non-system unbundled libraries,
signs each app ad hoc on public native CI, verifies signatures and checks entrypoints.
The separate application archive receives the same main-only provenance contract.

Ad hoc packages are engineering inputs, not notarized distribution. The native
bundle tool can use an existing local signing certificate selected explicitly by
digest; private signing material never enters public CI. Its receipt retains the
original runtime metadata and the final signed file hashes. Local certificate
signing is a separate transformation; it does not pretend that changed signature
bytes are the original CI archive. Retain both receipts in the accepting device's
runtime inventory. The host application's explicit local permission action is
`Contents/MacOS/rds-agent --request-desktop-permission`. OS consent belongs to that
installed application/device and is not included in a package.
