# Public CI preparation

The default Linux workspace check calls the public
`NDDev-Technocracy-OpenNetwork/nddev-ci` reusable check at
`9d59594f4582fd6c8c3439d8a3c4e8e25639ad43`. It prepares Rust 1.99.1 with
rustfmt and Clippy, runs `just setup`, then runs the same build-free checks
and Nextest/doc-test commands exposed by the project. Pull requests also
receive the library's commit, secret, workflow and dependency merge stops.

`packaging/ci-linux-tools.json` declares the public native tool assets used by
that check and the all-feature supply-chain lane: cargo-deny 0.20.2,
cargo-nextest 0.9.151, cargo-shear 1.14.0, actionlint 1.7.12 and zizmor 1.32.0.
Each archive has an exact producer repository, asset ID, size and SHA-256.
Each extracted executable has its own path, size and SHA-256. Archive and
member bytes were inspected against the official release metadata without
executing the downloaded tools on the source host. The shared public action
validates those identities on a disposable hosted runner before adding them
to PATH. A mismatched or missing asset stops preparation.

The optional-feature Clippy/test lanes, vendored transport tests, isolated
X11 keyboard cases, native Linux/macOS artifacts, Wayland portal cases,
observability integration and release gates remain. The default Linux
workspace Clippy/test work moves to the shared check instead of running a
second copy in the optional-feature jobs. The retired private tool/cache
actions are removed from this project's workflows. Hosted jobs do not read
contour cache configuration or change a device's managed SDK.

Cargo-deny remains responsible for advisories, licenses, bans and sources;
the duplicate cargo-audit command is removed. The public library supplies
Betterleaks and its remaining merge stops. Existing product lockfiles,
transport patches, permissions and installed services are unchanged.

This migration preserves the current `main` validation and release trigger
contract. A push receives the full shared check; PRs receive its short check.
It does not establish a new deployment, retag a release, alter device pins or
complete the separate promotion/controller integration. Python packaging
automation and the remaining source-file checks still require their own
migration. Local source checks are not executable runner qualification;
the pull request records the exact-source CI outcome.
