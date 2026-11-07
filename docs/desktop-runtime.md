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
is distinct from macOS capture/input: the planned native serving backends remain
unimplemented. No key, consent state or private device configuration belongs in
these generic packages.
