# Consented native clipboard and observation cancellation

The portal provider reads and publishes bounded UTF-8 text and PNG through the
already consented RemoteDesktop session. It subscribes before Start and only
activates when clipboard access is granted. Its per-session factory performs no
native I/O, opens no additional consent and cannot fall back to X11.

Owner signals update type metadata without reading contents. Requested bodies
use owned nonblocking descriptors and immutable bytes, without a staging file.
Safe Tokio pipe/socket registration replaces direct unsafe reactor registration.
Actual FIFO checks cover multichunk content, directional rejection and a native
peer write proving that descriptor closure precedes original-context cleanup.
The process body allowance survives physical-owner replacement while any old
operation retains credit. Descriptor cleanup precedes purpose, body-credit and
drain acknowledgement. The former periodic cleanup tick is removed; inactive
owners wait for actual signals/jobs. This is a structural idle-work reduction,
not a measured whole-application CPU claim.

Policy schema 3 declares the portal FD chunk size separately from wire and X11
chunks. Missing/unknown fields and unsupported schemas fail validation. The
profile remains compiled and does not load environment/filesystem defaults.

## Lost text observation

A control-input select can cancel `next_change` while a native read is already
admitted. Advancing the observed generation before that read finishes and then
discarding its waiter loses the copy: the next call waits for another clipboard
change. The actual private-D-Bus/descriptor regression fails against baseline
snapshot `41237f9407546b33b4ca01a04454948442bb5c5911832581a9716adfda7195c5`.
It resumes the observation after a deliberately blocked SelectionRead reply,
with no extra owner notification or second read; the old adapter times out.

One shared text-wire adapter now retains its current metadata/read future
across that outer cancellation. macOS and the portal use the same adapter;
their duplicate implementations are removed. Close seals admission and drops
the waiter, while the provider still drains any actual native job under its
original purpose. A closed provider retains its lease binding until the handle
is dropped; each per-operation purpose and credit must already drain earlier.

## Qualification

Linux runtime snapshot SHA-256:
`28091c7a63bf0d5671115b8a3d253210a919f1bfbe58fb34bbb56c380076e443`.
On physical Ubuntu x64, scoped denied all-target Clippy passed, the unchanged
regression above passed, and all 232 desktop library cases passed with 10 native
display cases excluded by that invocation. The preceding complete native
candidate also passed workspace denied Clippy, desktop/CLI/agent tests and 19
packaging cases. Earlier compilation and fixture failures remain failures; the
incorrect fixture expectation about dropping a still-held lease binding was
corrected to match the existing owner contract, without weakening per-operation
drain assertions.

A second real-descriptor regression completes an observation but leaves its
result undelivered, then publishes a newer explicit value. Snapshot
`702c56c2be3d6fc6dd5930156465e7eb696f61fa42a2eb1174bd3671af602c0f`
fails by returning the older observation afterward. The adapter now discards
that superseded waiter and records the successfully published generation.
Actual already-admitted native work still drains independently. The AppKit
change observer also refreshes a differing cached snapshot on its native owner;
a cache left asleep before current/publication cannot disclose an older change.

The private D-Bus case exercises actual messages and transferred kernel FDs:
early directed metadata, no eager body read, multichunk UTF-8/PNG, stale
publication refusal, an independent native selection requestor, exact published
bytes and successful write acknowledgement. Blocked/canceled and queued reads
retain their original contexts and body capacity until actual completion, and
closing a lease prevents its queued read from starting. The shared text adapter
also publishes and observes through this same provider.

Apple Silicon snapshot SHA-256:
`7eac521114f9499dbe7c6958bfce0f2e9b9e52731edf9ce112a52482a1261b2d`.
Selected native denied Clippy/library/binary/packaging checks passed for the
shared adapter. The final macOS snapshot passed denied all-target Clippy and
all 199 desktop library cases. The added AppKit cases publish through the text
adapter, change only its private named pasteboard, block the serialized native
edge, cancel the outer observation and receive that same copy after resuming.
The read gate is entered after asynchronous metadata refresh and counts the
actual provider read exactly once; an earlier probe assumed that metadata had
already completed after one poll and failed on the second host. Another case
verifies that an idle metadata cache cannot disclose a superseded snapshot. The exact debug
probe, SHA-256
`9bf9f0f31aca7a880e5a4df859cb49047d30cb3794ac5e99a09f7bd00133c7a8`,
passed strict native signature verification and all 17 cases on physical M1 and
M2. It used the ordinary development toolchain. These are debug functional
results; an exact optimized CI probe remains the release-qualification step.
Both hosts retained the `sandbox_extension_consume` permission warning; sandbox
and operating-system consent acceptance are not inferred from these probes.

## Acceptance boundary

The portal checks observed generations, but SetSelection offers no atomic
cross-client compare-and-set. Its reply proves native publication, not another
application's consumption. The native capabilities contain text and PNG, with
Control as the Linux shortcut; file formats remain absent.

No installed host, device pin, permission or shared development-cache change is
part of this increment. Native negotiation/manager/viewer integration, Wayland
viewer clipboard access, file promises/URIs and actual desktop application
pastes across the device matrix remain unfinished. A private-bus case does not
claim an exercised GNOME desktop paste or a clipboard-only viewer consent flow.

Protocol reference: [XDG Clipboard portal](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.Clipboard.html).
