# Native utility dependency qualification

The native dependency set updates four existing utility selections:

| Package | Previous | Qualified candidate |
| --- | --- | --- |
| yoke | 0.8.3 | 0.8.4 |
| yoke-derive | 0.8.3 | 0.8.4 |
| syn | 3.0.6 | 3.0.9 |
| unicode-ident | 1.0.26 | 1.0.27 |

The compatible Syn 2 selection remains for its existing consumers. The normal
resolver generated the lock; package checksums and publication timestamps were
checked against the primary crates.io index. All four selections were older
than three days and not yanked when checked. The earlier yoke-derive 0.8.3 was
yanked; its withdrawal alone is not evidence of a vulnerability or a security
fix. No other package version, feature or patched transport source changes.

## Local transport fixture

Three uni-router lifecycle tests timed out before connecting on a physical
Apple Silicon host. This also reproduced with the previous dependency lock,
so the failed connection was not attributed to these utility updates.
The fixture used wildcard-bound endpoint addresses, making a local lifecycle
check depend on the machine's unrelated network interfaces.

All three cases now share one explicit loopback-bound pair with discovery
disabled. The actual connection, stalled tag, routing and shutdown assertions
and their deadlines are unchanged. The fixture does not skip a platform,
retry a connection or change production routing.

The fixture-only snapshot, SHA-256
`58abd7d388f81f5fbd43337fe3b1d914c86bf8d19fffbd14a748f31fb59fc979`,
passed denied all-target network Clippy and all three cases on that host.
The unchanged dependency baseline failed all three before their assertions;
both failure and successful candidate evidence are retained.

## Qualification

Linux native snapshot SHA-256:
`cdc9ee3f4099c244db9f32ab6584cd9c0201c478615807badbf760f4dab88cd8`.
Standalone server binaries built with both default and owned-relay features,
and denied all-target workspace native Clippy passed. Initial test compilation
failed with filesystem exhaustion; those tests did not run. After removing
quiescent, project-owned compiler outputs, the same frozen source passed all
1,175 nextest cases and doctests. Twenty-two native/display cases were outside
that invocation. No shared cache eviction or toolchain override was used.

The final combined native snapshot, including the local fixture, is SHA-256
`ef15041b9cb87f8dad89952448c57ad3be0807a9879c795dfd0175b2775ebb0d`.
On physical Apple Silicon it passed denied all-target Clippy for the seven
native consumer packages, network/client/sync/SSH tests, all 455 owned protocol
cases, selected desktop/CLI/agent library/binary tests and all 19 packaging
cases. Build-free source checks also passed; the earlier yanked-version warning
is gone. Existing multiple-version warnings are retained.

These checks qualify the concrete dependency consumer. They do not establish
new native clipboard functionality, an installed-host upgrade or application
paste acceptance. Earlier failures are not relabeled as successful checks.

References: [primary crates.io index](https://github.com/rust-lang/crates.io-index),
[Iroh explicit bind addresses](https://docs.rs/iroh/latest/iroh/endpoint/struct.Builder.html#method.bind_addr).
