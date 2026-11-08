# Native renderer attribution

Execution: [#214](https://github.com/NDDev-Technocracy-OpenNetwork/nddev-remote/issues/214).
Observed 2026-11-08. This increment makes native waits attributable; the prior
X11 failure remains open because the unchanged baseline also ran normally in
the new comparison.

The original coarse `acquiring surface` label covered the complete draw. Native
configuration, acquisition, upload, command recording, submit, present and
cleanup now publish distinct in-memory phases before the operation starts.
The same-UID observation endpoint remains readable during a blocked native call.
Counters and timing storage have constant size; no per-frame file or log is
introduced. Native skipped/error attempts retain their measured spans.

The physical Linux comparison used the original 0.1.3 viewer and an instrumented
local release build with Rust 1.98.1, `rds-cli/desktop,rds-cli/transport-noq`.
Both viewed the same isolated 640×360 synthetic H.264 source through real QUIC
and native X11 presentation under GNOME XWayland. Each sample lasted 20 seconds.
It qualifies this narrow presentation/observation journey, not native capture,
physical input response, file transfer or hardware codec behavior.

| Observation | Baseline | Instrumented |
| --- | ---: | ---: |
| Submitted frames in the interval | 599 | 599 |
| Full snapshot file replacements | 10 | 0 |
| Attributed process disk write bytes | 65,536 | 0 |
| Incident files | 0 | 0 |

The instrumented native renderer reported Vulkan, a discrete GPU, Fifo,
`Bgra8UnormSrgb` and desired maximum frame latency 1. Maximum returned phase
spans in the retained sample were acquisition 71 µs, upload 437 µs, command
recording 12,565 µs, submission 137 µs and presentation 1,986 µs. These maxima
include startup and observation overhead; they are not quantiles or GPU timings.
The original 100-submitted-frame threshold was retained as observation data.
This investigation makes no acceptance-waiver or latency-improvement claim.

The failed earlier cohort received roughly 599 frames while submitting only
102 baseline / 49 candidate frames. The new baseline also submitted 599 frames,
so the new result establishes neither a correction nor a driver explanation.
The old receipts and threshold remain unchanged. Subsequent reproduction must
bind the actual renderer/presentation identity and in-flight phase age, plus
window/epoch, workload and resource conditions.

Native fixture jobs closed cleanly and their private IPC roots were removed
with owner/inode checks. Existing production processes retained their PIDs.
Linux library and warning-denied all-target checks passed for the instrumented
draw path; final-head public Linux/macOS lanes remain required.

The exact public PR producer also ran on physical Apple Silicon. The visible
M2 viewer reported Metal/integrated GPU/Fifo/BGRA8sRGB/latency 1 and submitted
573 frames in 20 seconds, with zero attributed process writes and no diagnostic
file writes/incidents. Acquisition/upload/commands/submission/presentation
maxima were respectively 22,880/1,552/4,200/925/852 µs. Its fixture closed cleanly
and the installed host retained its PID.

On M1, direct-background and ordinary packaged-app launches both remained
occluded. The retained observation showed 506 received frames, zero GPU uploads
and submissions, 517 acquisition attempts/skips (maximum 644 µs), and no later
draw stages. AppKit reported an inactive, visible but fully occluded window;
the local observation endpoint remained healthy. This qualifies attribution of
skipped native draws, not visible presentation on M1. No acquisition stall or
rendering correction is inferred from it. Owned fixture processes closed and
the installed host retained its PID. Neither Mac result closes the earlier
unexplained X11 cohort or establishes hardware-codec/input-to-photon acceptance.
