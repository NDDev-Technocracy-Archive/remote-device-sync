# Native dependency qualification

The native dependency set moves to Rust 1.99.1, Iroh 1.4.1, Noq/Noq-proto
1.4.0, egui 0.37.0, ashpd 0.14.0/zbus 6.2.1, russh 0.65.1 and libc 0.2.191.
The selected redb line remains 4.4.0. The lock records the complete resolved set;
target-specific upstream requirements retain their necessary independent
Objective-C, windowing, shaping, image and cryptographic dependency families.
No Cargo type family is forcibly substituted across an incompatible upstream
requirement.

Iroh and both Noq packages were refreshed from their exact published archives,
then the reviewed project patches were reapplied. Upstream's pending-four-tuple
deduplication replaces the equivalent old Iroh delta. Path recovery, explicit
readiness, ACK routing and pacing changes retain their reviewed behavior.
Each vendored `RDS-PATCH.md` identifies its upstream source and remaining delta;
the upstream notices and license bytes are retained.

| Published source | Archive SHA-256 |
| --- | --- |
| Iroh 1.4.1 | `e3689cab508af9bcef7816c1201b403595ec6aa20e7d15e1f9683bb4be4a5b17` |
| Noq 1.4.0 | `6249a065adc1edbc1704a95436f38dd60476064dbc3b7c47cf279e02d1590eb5` |
| Noq-proto 1.4.0 | `c01a35be3ed37558c8db69554419f24eff8a63599b54f711bbe3863ec727c47d` |

## Receive-buffer regression

The unchanged 32 MiB desktop receipt regression exposed an upstream assembler
failure: well-utilized contiguous packet allocations remained counted as gaps
after ordinary defragmentation and exhausted the existing chunk bound. The
unchanged published assembler plus the deterministic regression fixture fails.
That baseline snapshot has SHA-256
`9727e3b94525035b85a8a299d9306e52a26568b9b8234b15c51278048f096019`;
its assembler source has SHA-256
`83cdedbc6bbfa968e34f869b110380c7b7ace0b030e5d0f6af97b2923d788249`.

The correction performs one contiguous-run compaction only if the ordinary
zero-copy reclamation still exceeds the existing bound. `MAX_CHUNKS` (1024)
and `COMPACT_THRESHOLD` (2048) are unchanged; true gaps still reject. Ordered
compaction uses the consumed prefix. Unordered compaction preserves unread
lower offsets because its byte count is not an ordered prefix. The two new
cases cover contiguous full allocations behind a missing byte in both modes,
and an unordered high-offset read followed by lower-offset reclamation.
Existing overlap, duplicate-flood and randomized cases remain intact.
The related upstream report is [Quinn #2955](https://github.com/quinn-rs/quinn/issues/2955);
this is a project repair, not a claim that upstream has accepted it.

## Native checks

Runtime candidate: `7628bd0d7ab8dc2634eb62b604cbb655fb636839`.
All six PR workflows passed. The complete
[manual CI run](https://github.com/NDDev-Technocracy-OpenNetwork/nddev-remote/actions/runs/42516733619)
also passed its full Linux and macOS workspace jobs, Wayland portal boundaries,
format/lint/build, and workflow checks.

Physical Ubuntu x64 and M1 checks used the normal development toolchain.
Workspace/selected native all-target denied Clippy, transport/client/sync/SSH,
all 455 Noq-proto cases, desktop/CLI/agent and 19 packaging cases per platform
passed. Desktop results were 226 passed with six existing display cases ignored
on Linux and 194 passed on macOS. The Mac snapshot
`5e91572e6d8d251ef44da77616d3cd815eb152d8ce598250b0e1a8ef5272f313`
precedes only the final Linux portal-fixture API adjustment; final Linux snapshot
`ff97bf79d39a83111716540b2126738105cf90582951efe72121bd2f5940837b`
includes it. The first Linux native command mistakenly requested features of
unselected server packages and was rejected before tests; the corrected command
passed. Earlier migration failures are retained as failures, not waived checks.

The portal migration follows the changed bitflag and D-Bus variant/path APIs.
russh fixtures use the verified public-key wrapper. Workflow toolchain selection
reads the repository toolchain file. egui explicitly enables its packaged
monochrome emoji fallback; existing font licenses were checked against that
release's exact bytes and their provenance was updated.

## Native UI

The exact runtime artifacts from
[desktop-runtime run 42513644110](https://github.com/NDDev-Technocracy-OpenNetwork/nddev-remote/actions/runs/42513644110)
identify CI merge source `6231e5626b0ff18517cb2babe546135ef140c6b3`.
Linux executable SHA-256 is
`ab8ee9a8527a8335e58242610d3d761da54d64dfdb7dc5f8db2a43e7bd0c7ee4`;
the packaged macOS viewer executable is
`188aa1bc594b68b2fdb3b9c82cf6ec1b3853d2b0c10678b38b713de93f77a9b8`.
Package inventories and native signatures were verified before execution.

All four themes passed an owned real X11/Xvfb UI journey at 800×550 and
1400×850: resize, appearance selection, About/notices, explicit private workspace
save and clean exit. Pixel captures were inspected. The same verified Mac app
ran through LaunchServices on physical M1 and M2 in all four themes; generated
workspace bytes stayed unchanged and every app exited cleanly. Captures retain
both the first received frame and its successor, cropped to the exact owned
native window. The installed older Host supplied those captures; this does not
qualify its cold-frame behavior or upgrade that Host. Temporary test connections
were removed and the original connection inventory/selection was preserved.

The UI uses the unchanged canonical design-system source and bundled fonts.
These checks establish native rendering and the exercised controls. They do not
establish hardware encoding, interactive delay distributions, hidden-window
restore, rich clipboard integration or production adoption.
