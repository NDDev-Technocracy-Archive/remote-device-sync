# Native desktop qualification — 0.1.1

The engineering preview at source
`d5c8e466c64db62c405cea53f1775a6bbdd69360` was exercised with its published
Linux x86_64 executables and signed macOS arm64 applications. The applications
were not rebuilt or re-signed during installation. These observations qualify
the exercised screen paths; they do not expand the planned backend matrix.

## Capture, control and presentation

Each interactive run used an owned application with a rendered 64-cell counter,
one focus click, 12 physical A presses and 12 Backspaces. Actual application
counts and the final empty field were checked separately from transport ACKs.
The viewer correlated every response with the corresponding submitted frame.
All rows below have 24 keyboard responses, no correlation loss, timeout,
modified controlled key, dropped input or reconnect during the measured run.

| Controlled source → receiver | Native path exercised | Keyboard median / p95 |
| --- | --- | --- |
| Linux X11 → Linux | Xvfb/XTEST, GTK, private X11 software-GPU viewer | 49.6 / 61.0 ms |
| Ubuntu Wayland → Linux | GNOME portal/PipeWire/EIS, GTK, private software-GPU viewer | 65.8 / 109.7 ms |
| Ubuntu Wayland, 30 FPS source cap → Linux | Same path with the native source cap | 114.7 / 150.5 ms |
| macOS arm64 host, run A → Linux | ScreenCaptureKit/CGEvent, browser field, private software-GPU viewer | 102.7 / 115.0 ms |
| macOS arm64 host, run B → Linux | ScreenCaptureKit/CGEvent, browser field, private software-GPU viewer | 108.0 / 128.2 ms |
| Physical Ubuntu Wayland → macOS arm64 | Portal/EIS, real GTK field, native Metal viewer | 55.3 / 72.5 ms |

The output height was 1080 with the source aspect ratio preserved. These are
input-to-submitted-frame measurements, not optical scanout, network RTT or a
hardware-encode benchmark. The small controlled samples do not promise these
percentiles for different hardware, routes or workloads. X11's virtual display
and software rendering do not qualify a physical composited X11 desktop.

The GTK target used its supported per-process Cairo renderer after a Vulkan
surface publication failure was isolated in the fixture. No global driver,
renderer or product override was introduced. The browser fixture's geometry
was refreshed after activation/scrolling. Attempts without a visible marker or
confirmed focus click stopped before keyboard input and are not latency rows.

## Recovery and packaging

Both native macOS clients presented a remote Linux desktop over a WAN route.
One current native Metal window survived its local manager restart without a
new viewer process. A native Linux window likewise recovered after separate
local-manager and remote-agent restarts.

A 25-second direct-UDP interruption retained frame delivery without a viewer
reconnect. A separate 25-second interruption of the admitted desktop paths
stalled delivery, then the same native Linux window automatically recovered.
Connected state plus five additional submitted frames was observed 5.505 seconds
after path restoration; this is not a time-to-first-frame measurement.
The receipt does not establish which multipath route carried any individual
frame. Normal path selection remained enabled.

The release contains native hosting and viewing executables, signed Mac apps,
tracked source/vendor files, notices, checksums and provenance. Its Mac apps are
ad-hoc signed and not notarized. Normal local Screen Recording and Accessibility
grants were applied to the installed Host signatures before host qualification.
A successful build or an enabled Desktop service without display inventory
was never counted as capture/control acceptance.

No physical power cycle, FileVault unlock, physical Wi-Fi handover, monitor
hotplug, permission-withdrawal journey or arbitrary compositor/hardware matrix
is established by this report. Native startup jobs and job-restart recovery
retain their own scope; OS login and consent remain native prerequisites.
