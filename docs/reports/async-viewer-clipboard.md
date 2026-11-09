# Asynchronous viewer clipboard access

Part of [#212](https://github.com/NDDev-Technocracy-OpenNetwork/nddev-remote/issues/212).
The candidate connects the existing text-mode viewer to the shared asynchronous
native providers on X11 and macOS. It removes the synchronous AppKit viewer
wrapper and the unavailable non-Mac wrapper. Native work stays off the UI thread;
ordinary frame ticks observe cached metadata without polling the OS clipboard.

A fresh Copy generation baseline precedes its input. Paste reads before atomic
text/input admission. Subsequent input stays ordered and bounded; only adjacent
absolute pointer motion coalesces. Refused/canceled paste removes only that
gesture's V transitions, preserving later ordinary V input and modifier releases.
Native jobs retain the immutable original session epoch/purpose through actual
cleanup, including cancellation of the UI waiter. Tabs and windows share one copy
arbiter; source replacement cancels earlier publication admission.

Unit cases cover cached-metadata-only ticks, bounded admission, actual native-job
lifetime after a canceled waiter, superseded publication, unfinished Copy
baselines, publication receipts and held-input ordering/refusal. The dedicated
serial Xvfb lane exercises real native selection ownership and the viewer facade.
These fixtures do not establish receiving-application success.

Source `bd6bbf50b0ec16c2a196300d08ffd78b1b0abff0` passes the complete native
Linux and Mac jobs of run 42582293630, including combined-role and viewer-only
denied Clippy. Linux passes 250 desktop cases, both real-agent handshake cases
and all five Xvfb clipboard cases. Mac passes 217 desktop cases and both
handshake cases. Earlier feature-gate failures remain preserved in PR #239.
The qualified CI merge source is `47f4983e3f235059796399112b3e818e911657fd`,
tree `ae28033ae3261c8eb78eb79a64a36dfe3692a069`, identical to the PR source.

The Mac application from artifact 13658885044 passed byte/strict-signature
verification on a physical Mac. Its controlled text paste was sent through the
native viewer to an installed Wayland peer; that peer explicitly returned
`ClipboardError::Unavailable` and the receiving Chrome field stayed empty.
Initial unactivated-window input did not reach the viewer; after explicit app
activation it dispatched and acknowledged eight events. Neither input ACKs nor
video rendering establish application paste success.

This refusal exposed a diagnostic defect: the matching outbound transfer kept
waiting for Ready after its terminal error, causing repeated pending-clipboard
incidents. The candidate now removes only the failed transfer, counts cancellation
once, preserves other pending operations and displays a clipboard notice beside
the connected session. Final-cut native and physical refusal qualification of
this correction remain pending.

A bounded keyless native-input example is retained separately from product
packages. It attaches to an already admitted manager session and sends explicit
fixture chords through the authorized host; it reads neither clipboard contents
nor screen pixels and changes no enrollment or OS permissions.

No installed runtime, signature, permission, saved workspace, device pin or
shared build/cache configuration was changed. Product acceptance still requires
successful receiving-application text flows, the native Wayland viewer,
negotiated native-mode manager integration, target shortcut selection, image/file
flows and independent resource/latency measurements.
