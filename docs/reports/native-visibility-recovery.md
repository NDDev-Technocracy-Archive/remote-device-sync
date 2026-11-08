# Native visibility and recovery

Execution: [#213](https://github.com/NDDev-Technocracy-OpenNetwork/nddev-remote/issues/213),
[#214](https://github.com/NDDev-Technocracy-OpenNetwork/nddev-remote/issues/214).
Observed 2026-11-08. This comparison qualifies suspension and resumption of one
native viewer with a synthetic 640×360 H.264 source over real loopback QUIC.
It does not qualify native capture, hardware codecs, input or the whole desktop.

Each pair used the same source, a visible warm-up, ten seconds hidden, a native
restore, and a four-second observation after submitted frames resumed. Linux
used X11 under GNOME XWayland with native Iconic state checked. The packaged
macOS viewers used LaunchServices and actual AppKit visibility/occlusion state.
A successful hide request reply was not used as a substitute for native state.
Existing installed hosts were neither replaced nor started by the fixture.

| Physical platform | Hidden CPU before → after (% of one core) | Hidden decoded before → after | Hidden encoded discards after | Submitted after restore, before → after |
| --- | ---: | ---: | ---: | ---: |
| Linux x64, Vulkan | 1.800 → 0.300 | 300 → 0 | 300 | 119 / 4.000 s → 120 / 4.001 s |
| Apple M1, Metal | 8.788 → 0.312 | 284 → 0 | 284 | 114 / 3.999 s → 114 / 4.000 s |
| Apple M2, Metal | 14.196 → 0.383 | 257 → 0 | 254 | 103 / 3.999 s → 103 / 4.002 s |

All six measured hidden intervals attributed zero process disk-write bytes.
No diagnostic snapshots or incidents were written. All pairs restored without
reconnecting, stopped discarding encoded frames after resumption, passed the
unchanged 20 submitted frames/second recovery gate and exited cleanly. Protected
host processes retained their PIDs. Mac resumption was observed about two seconds
after activation with a two-second diagnostic sampling cadence; that is not a
precise recovery-latency measurement. Startup/cache writes are outside the hidden
measurement, and these short samples establish neither sustained idle power nor
general RAM/CPU or latency quantiles.

Linux CPU uses process user+system ticks. On Apple Silicon, `proc_pid_rusage`
CPU values use absolute-time ticks: the measured `mach_timebase_info` ratio was
125/3 ns per tick. Each run calibrated that domain against POSIX process CPU
seconds, with ratios 1.000112 and 1.000058. Treating those values as nanoseconds
would understate CPU by about 41.67 times. Measurements ran after this work's
native build/check jobs ended; other workloads on the shared devices were not
assumed absent.

## Build binding

The Linux candidate source snapshot SHA-256 is
`985ecd7b8d737949d1220e94fbc2c28cbc6754b22d430d93d83429c8de8cbecb`.
Its runtime code matches `8d65840e83fb87b59d4b7197ec13bf339855f54a`;
the native release executable SHA-256 is
`7c25e79bb38e99d6a33ad5fe1d7a9d7ae596aa7611b15d934946d16d77320b9c`.
The instrumented baseline executable is
`7c03692e17d746904e9b2a7a65877e850c916fcdcee3d7c7cf627b09a890d1db`.

Both Macs used the same public native producer
[42475420040](https://github.com/NDDev-Technocracy-OpenNetwork/nddev-remote/actions/runs/42475420040).
Its synthetic merge `f09e74019c850d40beadca8860d2e4073820c47e` has the
same tree `7d0e071b8452ef399b497534f3929c75420ade97` as the tested head.
Artifact ZIP SHA-256 is
`035b2671281f1ee03096a368a3b324a1c093247bfdec748cc7f6cab18ae18b44`;
the packaged viewer executable is
`6e46fa2b8b49e0cdd18a6e4f5b8c3ff532b9742002f20afd18c0f6b3dbbb250f`.
The earlier instrumented baseline executable is
`9b914fa10a9b5974c2c4ca7ab0fc5eba7ba8aa0c6b3a58da8d5bcc17aad65032`.
Archive checksums, application file inventories, source binding and strict native
code signatures were checked before launch. PR artifacts are not claimed as
attested releases or as adoption by an installed device.

Warning-denied all-target native Clippy passed on Linux and macOS. Relevant
library lanes passed: CLI 40 on each, desktop Linux 215 with five pre-existing
display-dependent cases ignored, and desktop macOS 184. All six public workflow
runs for the runtime head passed, including the native producer above.

## Failed cohorts remain open

Earlier Linux minimize/restore candidates resumed only 58 frames in 4.001 seconds
while their baseline resumed 120. Acquisition occupied about 2.36 seconds of that
sample, including approximately one-second calls. A resume-time configure did
not fix it and was removed. The current bounded native surface worker keeps such
waits off the event loop; it does not establish the driver/WSI cause. An earlier
passing repeat coincided with compilation and is retained only as recovery
evidence, not as an isolated CPU comparison.

Earlier Mac candidates wrote false incidents while intentionally hidden or when
requesting an ordinary resume keyframe. The current decode-demand health contract
and typed resume control correct those causes without changing fault thresholds.

Separate Linux keyboard cohorts failed on the unchanged baseline before candidate
admission: external key injection did not produce a typed keyboard ACK within the
existing limit. Those failed preconditions, source bindings and receipts remain
retained; this visibility comparison neither skips nor passes their input gate.
The original unlabelled X11 cohort (102 baseline / 49 candidate submissions while
receiving approximately 599 frames) also remains unexplained. Native Wayland,
input responsiveness, resizing/closure and multiple-window resource behavior
remain required independent qualification.
