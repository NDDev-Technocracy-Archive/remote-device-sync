# Explicit native desktop handshake

Part of [#212](https://github.com/NDDev-Technocracy-OpenNetwork/nddev-remote/issues/212).
This is an opt-in engine API. Production manager/GUI integration and native
receiving-application paste remain open; no installed host was upgraded.

DesktopV6 carries explicit native clipboard capabilities on a fresh desktop ID.
Its hello appends tag 14, and its acknowledgement appends tag 6; earlier greeting,
acknowledgement, frame and text formats retain their bytes. Explicit native
unavailability still accepts the desktop, but never permits a legacy text path.

The viewer claims the exact native route before greeting I/O. It validates
local capability reduction and policy before sending, then prepares the peer
using the same inbox after the acknowledgement. A failed/canceled handshake
closes its native lease. The agent leases only an already prepared backend under
the original full-control grant/service lifetime. View-only and unavailable
backends cannot open an ambient clipboard. Serving validates both desktop ID
and actual responder direction. Native peer lifetime and drain belong to the
desktop; native metadata failure does not become a video/input failure.

Frozen source
`13684ffda6024dfc4eba27bdb58be2aef447dd2075cf3a5182ce4cbda7e1da52`
passed core wire cases and denied full-workspace native Clippy on physical
Linux. The native feature set included CLI desktop, agent portal, owned relay
and the Noq benchmark consumer. Its actual integration build then failed before
test execution with filesystem exhaustion, including an explicit ENOSPC metadata
write and a simultaneous linker SIGBUS. This is not a successful behavior run
or an established linker/protocol regression. Source and assertions were not
changed to mask the failure.

The added integration test uses a real agent, authenticated connections and
actual tagged streams over explicit loopback routes. Its clipboard provider
and screen producer are deliberately in-memory/synthetic. It is intended to
verify native capability negotiation, a requested Unicode body, explicit
unavailability, legacy-worker exclusion and view-only factory/input exclusion
on Iroh and Noq. It establishes no OS clipboard or application-paste acceptance.
The native Linux/macOS CI lanes explicitly run this feature-gated integration
target; those results are required before review readiness.

The manager's bulk native IPC/proxy, explicit focus/gesture ownership, paste
publication-before-input barriers, remote-to-remote clipboard forwarding and
native file receivers remain separate acceptance work. The existing Native
clipboard component's text/PNG restriction is preserved. AppKit named-board
probes do not substitute for those remaining interactions.
