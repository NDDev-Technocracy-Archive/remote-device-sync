# Native decode demand

Execution: [#213](https://github.com/NDDev-Technocracy-OpenNetwork/nddev-remote/issues/213).
An invisible native window previously continued software decode and pixel
conversion, even while the surface could submit none of those images.

The native viewer now admits decode only for a presenting, open window which
the OS has not reported as hidden/occluded. AppKit's sampled visibility,
miniaturization, active-Space and occlusion flags supplement window events.
Portable visibility/minimization getters retain their optional values; X11
minimization is not inferred from an occlusion event that may never arrive.
Known-hidden windows also skip native surface acquisition and UI painting.
`hidden_draws_skipped` distinguishes this admission from a native API skip.
Focus alone does not suppress a visible window. Missing native visibility is
unknown and continues decode; this does not claim that every Linux compositor
reports full occlusion.

Discarding a bounded encoded delivery updates sequence ownership and invalidates
the reference chain without starting native decoder work or repeated repair
requests. The next visible image requires a fresh keyframe. Reappearance queues
one ordinary IDR request through the owning session's bounded control path;
reconnect clears the suspended epoch. Pending frame timing and visual probes
cannot report an invisible interval as submission delay.

Managed and direct native viewers share this admission/decode path. The direct
viewer consumes the existing encoded tap and uses the same independently
progressing control pump, so decode cannot hold keyboard, heartbeat or closure.
Immutable encoded buffers move into decode without an additional full payload
copy. Headless consumers continue their explicit decode workload.

Visible video health still requires decoded progress. While decode is
intentionally suspended, bounded encoded delivery proves transport progress;
heartbeats alone cannot hide a video failure. `encoded_frames_discarded` is
separate from decoded `frames_received` and actual `frames_submitted`.

The retained decoder state and last CPU image remain bounded and owned by the
session. This increment suppresses local decode work; it does not suspend the
remote capture/encoder, change quality/cadence or establish hardware-codec
acceptance. Native visibility/recovery and measured resource evidence remain
required before accepting the optimization across the supported OS matrix.

The real H.264 case decodes an initial image, discards a delta, refuses a later
delta as a fresh picture, then verifies changed pixels after the requested IDR.
Warning-denied all-target checks and library tests passed on Linux and macOS.
Native resource comparison is pending.
