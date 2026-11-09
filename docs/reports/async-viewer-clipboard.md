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

Source checks pass. Native compilation, unit/Xvfb cases and physical application
copy/paste are pending for this candidate. No installed application, signature,
permission, saved connection, device pin or shared build/cache configuration was
changed. The native Wayland viewer, negotiated native-mode manager integration,
target shortcut selection, image/file product flows and independent performance
measurements remain open. A build result cannot substitute for those effects.
