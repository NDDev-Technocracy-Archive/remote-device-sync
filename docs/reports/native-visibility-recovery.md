# Native visibility and recovery

Execution: [#213](https://github.com/NDDev-Technocracy-OpenNetwork/nddev-remote/issues/213),
[#214](https://github.com/NDDev-Technocracy-OpenNetwork/nddev-remote/issues/214).
Observed 2026-11-08. This comparison qualifies suspension and resumption of one
native viewer with a synthetic 640×360 H.264 source over real loopback QUIC.
It does not qualify native capture, hardware codecs or the whole desktop.
The independent focused keyboard sample below has its own narrower scope.

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

A later restored-window candidate submitted only four frames in four seconds
while receiving 120; its four native acquisitions occupied 3.965 seconds. WM_STATE
was Normal, but the window did not retain focus. A separate explicit-focus check
on the unchanged baseline lost focus to the compositor's XWayland proxy after an
X11 synthetic click. Another fixture did not stay Iconic throughout its hidden
interval and was rejected. These observations do not identify the cause of the
older presentation failures or make a mapped window proof of visibility.

Earlier Linux keyboard cohorts failed on the unchanged baseline before candidate
admission: external key injection did not produce a typed keyboard ACK within the
existing limit. Those failed preconditions, source bindings and receipts remain
retained. The independent native-input sample below fixes that fixture's admission,
not those older product failures. The original unlabelled X11 cohort (102 baseline /
49 candidate submissions while receiving approximately 599 frames) remains open.
Native Wayland presentation, restore robustness, resizing/closure and multiple-window
resource behavior still require independent qualification.

## Focused native keyboard sample

A separate physical Linux X11 sample used the existing consented Wayland host
and an already admitted Mac controller, with no OS grant, endpoint key or installed
host changes. The controller sent an absolute pointer position through Remote's
EIS input path. The fixture checked the actual pointer against its owned window
rectangle before clicking, then checked native active/focused window identity at
both ends of the four-second visible interval. Portal display IDs and X11 monitor
order were resolved separately; the source's default display was not assumed to
be the compositor's primary monitor.

Both unchanged baseline and candidate submitted 120 frames in 4.000 seconds.
One F8 press/release produced the viewer's typed keyboard ACK maximum of 15 ms
before and 2 ms after; the native controller's two input-dispatch acknowledgements
had maxima of 32.592 ms and 19.243 ms. Both retained the unchanged 250 ms key limit,
kept receiving native screen frames, finished cleanly, and preserved protected
host/viewer/compositor PIDs. This is one focused gesture on each implementation,
not an input-to-visible measurement or a latency distribution. It does not qualify
minimize/restore, a native target application's consumption, hardware decoding or
other OS input paths.

The test-only `native_owned_input` example pins an existing connected peer, never
loads a key or changes manager selection, and admits only an explicit display
point, click, F8 and finish. Its pipe reader retains a partial command across media
and event polling; expiry cannot leave a blocking stdin reader alive. Source
snapshot SHA-256 was
`b8853a2ee802ca412d49e6a3bc04b5968647d5c29db22384fe81a5c564771dcd`;
the native controller executable was
`398ff50e998c1902fe1bb743c02e3a8de3a8a30fd6b842ea0dadf921db2dfa56`.
The normal M1 SDK compiled it and warning-denied native Clippy passed on M1 and
Linux. The SDK's already reported strip-tool warning left this QA executable
unstripped; it is not a release or installed-host qualification. The tested viewer
executables and runtime source remain the exact builds bound above.
