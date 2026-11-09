# Asynchronous viewer clipboard access

Part of [#212](https://github.com/NDDev-Technocracy-OpenNetwork/nddev-remote/issues/212).
This candidate connects the existing text-mode viewer to the shared asynchronous
native providers on X11 and macOS. It removes the synchronous AppKit viewer
wrapper and the unavailable non-Mac clipboard wrapper.

The viewer's bounded operation futures wake the existing event loop when ready.
No clipboard polling is attached to ordinary frame ticks. A Copy generation
baseline precedes its input; Paste's read precedes the atomic text/input queue
admission. Subsequent input stays ordered and is bounded; only adjacent absolute
pointer motion coalesces. Canceled/refused paste removes only that gesture's V
transitions, preserving later ordinary V input and modifier releases. Native
jobs retain the immutable original session epoch/purpose until actual cleanup,
including cancellation of the UI waiter. Tab/window sessions share one copy
arbiter; source replacement cancels earlier publication admission.

Unit cases cover cached-metadata-only frame ticks, bounded UI admission, actual
native-job lifetime beyond a canceled waiter, superseded publication, an
unfinished Copy baseline, asynchronous publication receipts and held-input
ordering/refusal. The isolated Linux lane additionally uses a dedicated Xvfb
selection owner and the actual X11 facade; OS-dependent cases run serially.
These fixtures are not receiving-application acceptance.

The combined source at `a6755619f71af7f961fc7c8baf18fe7c2ff9c1c8` passes
250 desktop cases on Linux (12 display-dependent cases excluded from that run),
both real-agent handshake cases, and all five isolated Xvfb clipboard cases.
The Mac combined-role build passes denied Clippy, 217 desktop cases and both
handshake cases. Its subsequent viewer-only build exposed a close-signal feature
gate still limited to hosting. The next cut corrects that gate and explicitly
checks denied viewer-only Clippy as well as the combined role.

The following cut at `be40731fa37a27a159f9a37186f6b2f3b10d0769` passes
the complete Linux native lane, including viewer-only denied Clippy, all desktop
and handshake cases, and the five Xvfb cases. Mac combined-role denied Clippy
passes; the new viewer-only denied lane identifies an unused serving-only
scaling module. Its declaration now follows the actual Linux capture and Mac
hosting consumers, retaining unit-test coverage without suppressing warnings.

A bounded keyless native-input qualification example is retained independently
of product packages. It attaches only to an existing admitted manager session
and sends explicit fixture focus/shortcut chords. It reads neither clipboard
contents nor screen pixels, never enrolls an identity or changes permissions,
and requires owned nonblocking input/output pipes. Its artifacts allow real
application input through the already authorized host without another OS input
permission.

Source checks pass. Final-cut native compilation/tests and physical receiving-
application copy/paste remain pending; those effects cannot be inferred from
unit or Xvfb results. No installed application, signature,
permission, saved connection, device pin or shared build/cache configuration was
changed. The native Wayland viewer, negotiated native-mode manager integration,
target shortcut selection, image/file product flows and independent performance
measurements remain open. A build result cannot substitute for those effects.
