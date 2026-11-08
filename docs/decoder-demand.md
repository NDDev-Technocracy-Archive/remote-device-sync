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
`video_resume_requests` records these intentional restarts separately from
fault repairs. Diagnostic video-loss detection follows `decode_required`:
hidden windows require encoded progress instead of writing a false decode-stall
incident to disk. Control and transport faults remain observable while hidden.
The queued resume has its own local input type, so dispatch cannot count it
again as a repair. Decode remains suspended until this admission is complete;
the decoder observes the request and keeps its normal repair interval while
awaiting the keyframe. Intervening deltas still cannot produce a visible image.

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

The physical X11 comparison has now isolated a narrower result: during ten
seconds of native minimization, software decoding stopped (300 discarded,
zero decoded frames), no process disk writes were attributed, and CPU fell
from 1.80% to 0.20% of one core. Its recovery gate failed: only 58 frames were
submitted during a 4.001-second post-restore sample, while the unchanged
baseline submitted 120. Acquisition accounted for 2.36 seconds of that sample,
including approximately one-second native calls. Reconfiguring the surface
on resume did not resolve this and has been removed. This is failed acceptance
evidence, not an accepted performance improvement or an explanation of the
older unlabelled X11 failures. The native recovery and historical cohorts stay
open; no threshold or diagnostic fault check has been waived.
