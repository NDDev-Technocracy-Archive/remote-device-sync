# Native dependency set refresh

Part of [#210](https://github.com/NDDev-Technocracy-OpenNetwork/nddev-remote/issues/210).
The normal Cargo resolver refreshes compatible runtime/build families without
changing direct manifest requirements, native features or vendored transport
sources. Resolution was not substituted for consumer behavior qualification.

The refresh covers crypto utilities, font/text processing, internationalization,
native certificate discovery, HTTP/UUID helpers and the C build helper. ML-KEM
now shares the existing SHA3 family, removing its earlier separate SHA3 line;
its required module-lattice and shake dependencies resolve together. ICU's
families also move together. No manually forced transitive requirement, new
clipboard implementation, disk cache or resident task was introduced.

Compared with the parent lock, 42 published package records are added/replaced
and 41 are removed. Each added record's source/checksum and publication timestamp
was checked against the official Cargo sparse index. Registry identity matching
uses the Cargo ecosystem and canonical package source, including aliases where
another ecosystem owns the same short name. All vendored source/patch bytes
remain unchanged; in particular this does not requalify a fork by upstream
version number alone.

## Qualification state

An earlier frozen subset,
`5f6b08b8f0674f639717fd7f90f7d620941be9e806921ddeac7122de75d3e4db`,
passed default and owned-server builds and denied full-workspace native Clippy
on physical Linux. Its broad test run then exposed the agent stop-registration
race corrected separately in [#236](https://github.com/NDDev-Technocracy-OpenNetwork/nddev-remote/pull/236).
That fail-fast run did not execute the remaining tests and is not counted as a
complete pass.

The same earlier subset passed denied native Clippy on physical Apple Silicon.
Its first transport build exhausted disk space before tests; after removal of
unused private generated build trees, the unchanged source passed the full
net/client/sync/SSH cargo-test set, desktop/CLI/agent cases and the Python script
cases. The independent vendored Noq-proto test target also passed its 455 cases;
its own lock is unchanged and is distinct from the refreshed workspace graph.

Later HTTP/UUID, ML-KEM/module-lattice/shake and build-helper changes require
their final combined CI/native qualification. New physical builds were stopped
after explicit capacity failures; no shared build cache eviction or toolchain
override was used. Source checks and draft CI continue on the source host and
hosted runners respectively. The final selected set is not yet claimed as a
completed native runtime or performance acceptance.

Other inherited API-family/catalog questions are tracked separately. Their
existence is not treated as approval of an old lock or an undocumented exception.
Installed app signatures, OS permissions, device pins and release history are
independent of this source/dependency update.
