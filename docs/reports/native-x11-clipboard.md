# Native X11 clipboard owner

The prepared X11 provider reads and publishes UTF-8 text and static PNG on a
native worker. It reuses the existing ICCCM selection implementation, including
TARGETS, TIMESTAMP and INCR, rather than introducing a second protocol edge.
XFixes wakes generation watchers. Snapshot discovery requests targets only;
representation bytes are read after explicit body admission.

The physical owner, session lease and each queued/running native call retain
their original purpose independently. Canceling an async waiter cannot release
its body allowance or native owner while the selection call still runs. Closing
a lease seals admission and drains its actual operations. Physical close also
releases the X connection and windows before process admission becomes available.
This factory opens only the explicitly selected X backend; it cannot select a
Wayland fallback or open screen-capture consent.

Policy schema 2 declares separate X11 transfer, chunk, event-batch and deadline
settings in the common compiled profile. Missing or unknown settings fail
validation. Buffered native events are drained before polling. Idle workers
sleep on the X and job descriptors; active work uses its nearest real deadline.
Published/owned PNG bytes are shared without re-encoding or a staging file.

## Qualification

The runtime snapshot SHA-256 is
`d4d04085f21452797cff0f84fbbafabadf2be227e468f3c1e66bb06d2db52444`.
Through the normal development toolchain on physical Ubuntu x64, it passed
denied all-target desktop Clippy with viewer/X11 features, 199 display-free
cases (10 native/display cases excluded by that invocation), and seven explicit
native X11 cases on an owned Xvfb server.

The native cases include large UTF-8 and incompressible PNG bodies across INCR,
an independent requestor reading the published PNG, generation changes refusing
stale publication, metadata discovery without an eager body read, and buffered
native events that arrive before a reply. A blocked real selection reply proves
that cancellation retains its body allowance and original purpose, and that
close waits until the native reply finishes. File capabilities are absent.
Legacy text interoperability and temporary-requestor cleanup also pass.

Over a 20.002-second idle interval, the measured process/thread voluntary
context-switch delta was five. This is a bounded idle-worker observation, not a
whole-application CPU or interactive-latency claim.

The same source passed the selected transport/client/sync/SSH/desktop/CLI/agent
denied all-target Clippy command, native desktop/CLI/agent library and binary
tests, and 19 packaging checks on physical Apple Silicon. An attempted optimized
Mac probe build stopped at a development-cache server startup error before
producing a binary; it is retained as a failed attempt and supplies no probe
acceptance.

This increment does not advertise negotiated native clipboard mode. The shared
session/manager/UI flow, Wayland provider, native file publication and actual
cross-device application pastes still require qualification. Installed hosts,
device identities and OS permissions are unchanged.

## Combined dependency qualification

Runtime source `f0d0671f4691fc550b5b69c7724dab8f39cf669f` includes the current
transport/native dependency set. Its snapshot SHA-256 is
`8046c62fa11ccad22f1bb1cb6c7c2bdf9319944d6de63a246996422ed8fcb168`.
Full Linux workspace all-target denied Clippy passed. The viewer/X11 library
passed 202 display-free cases (10 native/display cases excluded there), then all
seven explicit native X11 cases passed. The idle observation remained five
voluntary switches over 20.002 seconds. Selected Apple Silicon all-target
denied Clippy, native desktop/CLI/agent tests and 19 packaging checks also passed.

Both local optimized Mac build attempts stopped at the development cache's
server startup error before producing an artifact, including the accepted
toolchain source. Instead, the existing
[native CI producer](https://github.com/NDDev-Technocracy-OpenNetwork/nddev-remote/actions/runs/42527075635)
supplied probe artifact `13636139389`. Its checkout source is
`859a04f7dbde68f408c3328ab0d68c5833607bca`, whose tree matches the runtime source
above. ZIP SHA-256 is
`40c8046983db9d9d234433b6feaf29dcdea5b4484c7cc7733e55e9ca9b2e57ef`;
probe executable SHA-256 is
`3ce6948aa4f9fcf70e33ec4367ac876749b8006a2640c1fc6dc5f6bce9ef41c0`.
The verified exact executable passed strict native signature verification and
all 14 private-pasteboard cases on physical M1 and M2. Both retained the
`sandbox_extension_consume` permission warning; these probes do not qualify a
sandboxed application or operating-system consent.
